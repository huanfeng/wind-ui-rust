//! Win32 窗口、消息循环与 GDI 呈现。
//!
//! 渲染全在 CPU：单份 tiny-skia `Pixmap`（RGBA 预乘）作后备缓冲；呈现时原地
//! R/B 交换为 BGRA 后 `SetDIBitsToDevice` 直接拷屏。空闲时阻塞在 `GetMessageW`，零 CPU。

pub mod clipboard;

use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;

use tiny_skia::Pixmap;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{FALSE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, InvalidateRect, ScreenToClient, SetDIBitsToDevice, UpdateWindow,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyState, ReleaseCapture, SetCapture, VK_BACK, VK_CONTROL, VK_DELETE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageTime, GetMessageW, GetSystemMetrics, GetWindowLongPtrW,
    LoadCursorW, PostQuitMessage, RegisterClassExW, SM_CXDOUBLECLK, SM_CYDOUBLECLK,
    SetWindowLongPtrW, ShowWindow, TranslateMessage, CREATESTRUCTW, CW_USEDEFAULT, GWLP_USERDATA,
    SetWindowPos, IDC_ARROW, MSG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SW_SHOW, WINDOW_EX_STYLE,
    WM_CAPTURECHANGED, WM_CHAR, WM_DESTROY, WM_DPICHANGED, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_PAINT, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SIZE, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};

use super::AppHandler;
use crate::event::{Key, KeyEvent, MouseButton, PointerEvent, PointerKind};
use crate::geometry::{Color, Point, Size};

/// 窗口配置。
pub struct WindowConfig {
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub bg: Color,
    /// 截屏模式：渲染一帧离屏存 PNG 后立即退出，不创建窗口。
    pub screenshot: Option<PathBuf>,
    /// 截屏时的 DPI 缩放（默认 1.0），用于验证高 DPI 渲染。
    pub screenshot_scale: f32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "windui".into(),
            width: 800,
            height: 600,
            bg: Color::hex(0xF3F3F3),
            screenshot: None,
            screenshot_scale: 1.0,
        }
    }
}

/// 运行应用：截屏模式离屏渲染存盘；否则创建窗口进入消息循环（阻塞至退出）。
pub fn run(cfg: WindowConfig, mut handler: Box<dyn AppHandler>) {
    if let Some(path) = cfg.screenshot.clone() {
        run_offscreen(&cfg, &mut handler, &path);
        return;
    }
    unsafe { run_windowed(cfg, handler) };
}

/// 离屏渲染一帧并保存 PNG。无需窗口，适合自动化验证。
fn run_offscreen(cfg: &WindowConfig, handler: &mut Box<dyn AppHandler>, path: &PathBuf) {
    // 物理像素 = 逻辑尺寸 × scale，供高 DPI 截屏验证。
    let s = cfg.screenshot_scale.max(0.1);
    let pw = (cfg.width as f32 * s).round().max(1.0) as i32;
    let ph = (cfg.height as f32 * s).round().max(1.0) as i32;
    let size = Size::new(pw, ph);
    let mut pixmap = Pixmap::new(pw as u32, ph as u32).expect("分配 pixmap 失败");
    pixmap.fill(to_skia_color(cfg.bg));
    handler.set_scale(s);
    handler.render(&mut pixmap, size);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    pixmap.save_png(path).expect("保存 PNG 失败");
    eprintln!("[windui] 截屏已保存: {}", path.display());
}

/// 窗口端运行时状态，指针挂在 HWND 的 GWLP_USERDATA 上。
struct WindowState {
    handler: Box<dyn AppHandler>,
    bg: Color,
    /// 当前是否已对窗口调用 OS SetCapture（与 handler 逻辑捕获态同步）。
    capturing: bool,
    /// 单一后备缓冲（tiny-skia 渲染目标）。呈现时原地交换 R/B 为 BGRA 后
    /// 直接 SetDIBitsToDevice 拷屏——省去独立 DIB section，全屏内存减半。
    pixmap: Option<Pixmap>,
    buf_w: i32,
    buf_h: i32,
    /// 连续点击跟踪（用于双击/三击判定）。
    last_click: ClickTracker,
}

/// 连续点击跟踪状态。在平台层把多次快速同位点击折算为 click_count。
#[derive(Default, Clone, Copy)]
struct ClickTracker {
    time_ms: u32,
    x: i32,
    y: i32,
    button: i32,
    count: u8,
}

impl ClickTracker {
    /// 按 Down 事件更新连续点击计数：与上次同按键、在系统双击时限与漂移阈值内则递增
    /// （封顶到 3 支持三击），否则重置为 1。返回本次点击的计数。
    fn bump(&mut self, button: i32, x: i32, y: i32, now_ms: u32, dbl_ms: u32, dx: i32, dy: i32) -> u8 {
        let continued = self.count > 0
            && self.button == button
            && now_ms.wrapping_sub(self.time_ms) <= dbl_ms
            && (x - self.x).abs() <= dx
            && (y - self.y).abs() <= dy;
        let count = if continued { (self.count + 1).min(3) } else { 1 };
        *self = ClickTracker { time_ms: now_ms, x, y, button, count };
        count
    }
}

impl WindowState {
    fn new(handler: Box<dyn AppHandler>, bg: Color) -> Self {
        Self {
            handler,
            bg,
            capturing: false,
            pixmap: None,
            buf_w: 0,
            buf_h: 0,
            last_click: ClickTracker::default(),
        }
    }

    /// 确保后备缓冲匹配客户区；尺寸变化时重建。
    fn ensure_pixmap(&mut self, w: i32, h: i32) {
        let w = w.max(1);
        let h = h.max(1);
        if self.buf_w == w && self.buf_h == h && self.pixmap.is_some() {
            return;
        }
        self.pixmap = Some(Pixmap::new(w as u32, h as u32).expect("分配 pixmap 失败"));
        self.buf_w = w;
        self.buf_h = h;
    }

    /// 渲染并呈现到窗口。
    unsafe fn paint(&mut self, hwnd: HWND) {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        // 最小化时客户区为 0×0：仍需配对 BeginPaint/EndPaint 校验区域，但不绘制。
        if w <= 0 || h <= 0 {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            return;
        }
        self.ensure_pixmap(w, h);

        let size = Size::new(self.buf_w, self.buf_h);
        let pixmap = self.pixmap.as_mut().unwrap();
        pixmap.fill(to_skia_color(self.bg));
        self.handler.render(pixmap, size);
        // RGBA 预乘 → BGRA（GDI 32bpp 字节序）原地交换 R/B。
        swap_rb_inplace(pixmap.data_mut());
        let bits = pixmap.data().as_ptr() as *const c_void;

        // top-down DIB 描述：直接从缓冲拷到设备，无需独立 DIB section。
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.buf_w,
                biHeight: -self.buf_h, // 负数 = top-down，与 pixmap 行序一致
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let scanlines = SetDIBitsToDevice(
            hdc,
            0,
            0,
            self.buf_w as u32,
            self.buf_h as u32,
            0,
            0,
            0,
            self.buf_h as u32,
            bits,
            &bmi,
            DIB_RGB_COLORS,
        );
        debug_assert!(scanlines != 0, "SetDIBitsToDevice 呈现失败");
        let _ = EndPaint(hwnd, &ps);
    }
}

/// 原地把 RGBA 缓冲逐像素交换 R/B（→ BGRA），供 GDI 直接呈现。
fn swap_rb_inplace(data: &mut [u8]) {
    let n = data.len() / 4;
    let p = data.as_mut_ptr() as *mut u32;
    for i in 0..n {
        unsafe {
            // 字节 [R,G,B,A] → [B,G,R,A]：交换 byte0 与 byte2。
            let v = p.add(i).read_unaligned();
            let s = (v & 0xFF00_FF00) | ((v & 0x0000_00FF) << 16) | ((v & 0x00FF_0000) >> 16);
            p.add(i).write_unaligned(s);
        }
    }
}

fn to_skia_color(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

const CLASS_NAME: PCWSTR = w!("WindUiWindowClass");

unsafe fn run_windowed(cfg: WindowConfig, handler: Box<dyn AppHandler>) {
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

    let hmodule = GetModuleHandleW(None).expect("GetModuleHandleW 失败");
    let hinst = HINSTANCE(hmodule.0);
    let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: CLASS_NAME,
        hCursor: cursor,
        ..Default::default()
    };
    let atom = RegisterClassExW(&wc);
    debug_assert!(atom != 0, "RegisterClassExW 失败");

    // 把 WindowState 装箱，指针随 CreateWindow 传入，在 WM_NCCREATE 挂到 HWND。
    let state = Box::new(WindowState::new(handler, cfg.bg));
    let state_ptr = Box::into_raw(state);

    let title: Vec<u16> = cfg.title.encode_utf16().chain(std::iter::once(0)).collect();

    // cfg 宽高为逻辑 dp（期望客户区）。按系统 DPI 反算窗口外框物理尺寸，
    // 使客户区 = cfg × scale，避免标题栏/边框吃掉内容空间导致超出。
    let sys_dpi = {
        let d = GetDpiForSystem();
        if d == 0 { 96 } else { d }
    };
    let init_scale = sys_dpi as f32 / 96.0;
    let (phys_w, phys_h) = frame_size_for_client(cfg.width, cfg.height, init_scale, sys_dpi);

    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        CLASS_NAME,
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        phys_w,
        phys_h,
        None,
        None,
        hinst,
        Some(state_ptr as *const c_void),
    ) {
        Ok(h) => h,
        Err(e) => {
            // 创建失败不会触发 WM_DESTROY，需手动回收已装箱的 WindowState，
            // 避免泄漏（含其 GDI 资源）。成功路径下所有权已转移给 HWND。
            drop(Box::from_raw(state_ptr));
            panic!("CreateWindowExW 失败: {e:?}");
        }
    };

    // 用实际窗口 DPI 设置内容缩放（可能与系统 DPI 不同，如多显示器）。
    let dpi = GetDpiForWindow(hwnd);
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    // 实际 DPI 与系统估算不一致时，按真实 scale 校正窗口物理尺寸（在显示前，无 state 借用）。
    if (scale - init_scale).abs() > 0.01 {
        let (w, h) = frame_size_for_client(cfg.width, cfg.height, scale, dpi);
        let _ = SetWindowPos(hwnd, None, 0, 0, w, h, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE);
    }
    if let Some(s) = state_from(hwnd) {
        s.handler.set_scale(scale);
    }

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = UpdateWindow(hwnd);

    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            // 取出 CreateWindow 传入的 WindowState 指针并挂到 HWND
            let cs = lparam.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                let state_ptr = (*cs).lpCreateParams as isize;
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_PAINT => {
            if let Some(state) = state_from(hwnd) {
                state.paint(hwnd);
            }
            LRESULT(0)
        }
        WM_SIZE => {
            // 客户区变化：请求重绘（paint 内按客户区重建缓冲）。
            let _ = InvalidateRect(hwnd, None, false);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            handle_pointer(hwnd, PointerKind::Move, MouseButton::Left, lparam);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            handle_pointer(hwnd, PointerKind::Down, MouseButton::Left, lparam);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            handle_pointer(hwnd, PointerKind::Up, MouseButton::Left, lparam);
            LRESULT(0)
        }
        WM_RBUTTONDOWN => {
            handle_pointer(hwnd, PointerKind::Down, MouseButton::Right, lparam);
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            handle_pointer(hwnd, PointerKind::Up, MouseButton::Right, lparam);
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            handle_wheel(hwnd, wparam, lparam);
            LRESULT(0)
        }
        WM_KEYDOWN => {
            handle_key(hwnd, wparam);
            LRESULT(0)
        }
        WM_CHAR => {
            handle_char(hwnd, wparam);
            LRESULT(0)
        }
        WM_CAPTURECHANGED => {
            handle_capture_changed(hwnd);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            handle_dpi_changed(hwnd, wparam, lparam);
            LRESULT(0)
        }
        WM_DESTROY => {
            // 回收 WindowState
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
            if !ptr.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 由期望逻辑客户区尺寸 + scale + dpi 反算窗口外框物理尺寸（含标题栏/边框）。
unsafe fn frame_size_for_client(logical_w: i32, logical_h: i32, scale: f32, dpi: u32) -> (i32, i32) {
    let cw = (logical_w as f32 * scale).round() as i32;
    let ch = (logical_h as f32 * scale).round() as i32;
    let mut rc = RECT { left: 0, top: 0, right: cw, bottom: ch };
    let _ = AdjustWindowRectExForDpi(&mut rc, WS_OVERLAPPEDWINDOW, FALSE, WINDOW_EX_STYLE::default(), dpi);
    (rc.right - rc.left, rc.bottom - rc.top)
}

/// 从 lParam 解出客户区坐标，构造并分发指针事件。
///
/// 两段式：先借 state 分发事件并读取意图，**释放借用后**再调用会同步重入
/// WndProc 的 OS API（SetCapture/ReleaseCapture/DestroyWindow），避免 &mut 别名 UB。
unsafe fn handle_pointer(hwnd: HWND, kind: PointerKind, button: MouseButton, lparam: LPARAM) {
    let x = (lparam.0 & 0xffff) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
    // 仅按下时计算连续点击数；其余动作恒为单击。
    let click_count = if matches!(kind, PointerKind::Down) {
        let btn = match button {
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            // Middle 当前不可达：无 WM_MBUTTONDOWN 分发；保留映射以备后续接入。
            MouseButton::Middle => 3,
        };
        let now = GetMessageTime() as u32;
        let dbl = GetDoubleClickTime();
        // SM_CXDOUBLECLK/SM_CYDOUBLECLK 是双击矩形的**全宽/全高**，以首击为中心，
        // 故每侧容差为其一半（与 |x-x0|<=dx 比较）。
        let dx = GetSystemMetrics(SM_CXDOUBLECLK) / 2;
        let dy = GetSystemMetrics(SM_CYDOUBLECLK) / 2;
        state_from(hwnd).map(|s| s.last_click.bump(btn, x, y, now, dbl, dx, dy)).unwrap_or(1)
    } else {
        1
    };
    dispatch_pointer_event(hwnd, PointerEvent { kind, pos: Point::new(x, y), button, click_count });
}

/// WM_MOUSEWHEEL：高位字为滚动量（±120/刻度），lParam 为屏幕坐标需转客户区。
unsafe fn handle_wheel(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    let delta = ((wparam.0 >> 16) & 0xffff) as i16 as i32;
    let mut pt = POINT {
        x: (lparam.0 & 0xffff) as i16 as i32,
        y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
    };
    let _ = ScreenToClient(hwnd, &mut pt);
    dispatch_pointer_event(
        hwnd,
        PointerEvent::single(PointerKind::Wheel(delta), Point::new(pt.x, pt.y), MouseButton::Left),
    );
}

/// 指针事件分发的公共两段式实现（事件分发 + OS 捕获同步 + 关闭）。
unsafe fn dispatch_pointer_event(hwnd: HWND, ev: PointerEvent) {
    let (repaint, active, was_capturing, close) = {
        let Some(state) = state_from(hwnd) else { return };
        let repaint = state.handler.on_pointer(ev);
        (repaint, state.handler.capture_active(), state.capturing, state.handler.wants_close())
    };
    if repaint {
        let _ = InvalidateRect(hwnd, None, false);
    }
    // 同步 OS 指针捕获（此处无 state 借用，重入安全）。
    if active && !was_capturing {
        SetCapture(hwnd);
        if let Some(s) = state_from(hwnd) {
            s.capturing = true;
        }
    } else if !active && was_capturing {
        let _ = ReleaseCapture();
        if let Some(s) = state_from(hwnd) {
            s.capturing = false;
        }
    }
    if close {
        let _ = DestroyWindow(hwnd);
    }
}

/// WM_DPICHANGED：DPI 变化（拖到不同缩放显示器）。按建议矩形调窗口尺寸并更新内容缩放。
unsafe fn handle_dpi_changed(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    let dpi = (wparam.0 & 0xffff) as u32;
    let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
    // lParam 指向系统建议的新窗口矩形，先据此重定位/缩放窗口（无 state 借用，重入安全）。
    let prc = lparam.0 as *const RECT;
    if !prc.is_null() {
        let r = &*prc;
        let _ = SetWindowPos(
            hwnd,
            None,
            r.left,
            r.top,
            r.right - r.left,
            r.bottom - r.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    if let Some(s) = state_from(hwnd) {
        s.handler.set_scale(scale);
    }
    let _ = InvalidateRect(hwnd, None, false);
}

/// OS 抢走指针捕获（如 Alt+Tab、WM_CAPTURECHANGED）：通知 handler 收尾。
unsafe fn handle_capture_changed(hwnd: HWND) {
    let repaint = {
        let Some(state) = state_from(hwnd) else { return };
        if !state.capturing {
            return;
        }
        state.capturing = false;
        state.handler.on_capture_lost()
    };
    if repaint {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// 把 VK 码翻译为框架键并分发。
unsafe fn handle_key(hwnd: HWND, wparam: WPARAM) {
    let vk = wparam.0 as u16;
    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
    let key = if vk == VK_TAB.0 {
        Key::Tab
    } else if vk == VK_RETURN.0 {
        Key::Enter
    } else if vk == VK_ESCAPE.0 {
        Key::Escape
    } else if vk == VK_SPACE.0 {
        Key::Space
    } else if vk == VK_BACK.0 {
        Key::Backspace
    } else if vk == VK_DELETE.0 {
        Key::Delete
    } else if vk == VK_LEFT.0 {
        Key::Left
    } else if vk == VK_RIGHT.0 {
        Key::Right
    } else if vk == VK_UP.0 {
        Key::Up
    } else if vk == VK_DOWN.0 {
        Key::Down
    } else if vk == VK_HOME.0 {
        Key::Home
    } else if vk == VK_END.0 {
        Key::End
    } else {
        Key::Other(vk as u32)
    };
    let ev = KeyEvent { key, pressed: true, shift, ctrl };
    dispatch_key_event(hwnd, ev);
}

/// WM_CHAR：已翻译的字符（含 IME/CJK 输入）。控制字符跳过。
unsafe fn handle_char(hwnd: HWND, wparam: WPARAM) {
    let Some(c) = char::from_u32(wparam.0 as u32) else { return };
    if c.is_control() {
        return;
    }
    let ev = KeyEvent { key: Key::Char(c), pressed: true, shift: false, ctrl: false };
    dispatch_key_event(hwnd, ev);
}

/// 分发键盘事件（两段式：先借 state 取意图，释放后再调可能重入的 DestroyWindow）。
unsafe fn dispatch_key_event(hwnd: HWND, ev: KeyEvent) {
    let (repaint, close) = {
        let Some(state) = state_from(hwnd) else { return };
        (state.handler.on_key(ev), state.handler.wants_close())
    };
    if repaint {
        let _ = InvalidateRect(hwnd, None, false);
    }
    if close {
        let _ = DestroyWindow(hwnd);
    }
}

/// 从 HWND 取回 WindowState 可变引用（生命周期受窗口存续保证）。
///
/// 约束：依赖 WndProc 单线程串行回调，且 handler 内不重入分发本窗口消息。
/// 一旦某 handler 同步 SendMessage 回到本窗口造成重入，返回的 `&mut` 将形成
/// 别名 UB —— 届时须改用 RefCell / 重入计数加固。
unsafe fn state_from<'a>(hwnd: HWND) -> Option<&'a mut WindowState> {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
    if ptr.is_null() {
        None
    } else {
        Some(&mut *ptr)
    }
}

#[cfg(test)]
mod tests {
    use super::ClickTracker;

    // 双击时限 500ms，漂移阈值 ±4px，同左键。
    const DBL: u32 = 500;
    const DX: i32 = 4;
    const DY: i32 = 4;

    #[test]
    fn double_then_triple_then_reset() {
        let mut t = ClickTracker::default();
        assert_eq!(t.bump(1, 10, 10, 1000, DBL, DX, DY), 1, "首击=单击");
        assert_eq!(t.bump(1, 11, 11, 1100, DBL, DX, DY), 2, "时限内同位=双击");
        assert_eq!(t.bump(1, 12, 12, 1200, DBL, DX, DY), 3, "继续=三击");
        assert_eq!(t.bump(1, 12, 12, 1300, DBL, DX, DY), 3, "封顶于三击");
        // 超出时限：重置。
        assert_eq!(t.bump(1, 12, 12, 2000, DBL, DX, DY), 1, "超时重置为单击");
    }

    #[test]
    fn continuation_across_u32_wraparound() {
        // GetMessageTime 是 49.7 天回绕的 ms 计数；wrapping_sub 必须正确处理跨界连击。
        let mut t = ClickTracker::default();
        let near_max = u32::MAX - 100;
        assert_eq!(t.bump(1, 10, 10, near_max, DBL, DX, DY), 1, "首击");
        // 跨过 u32 边界 50ms：near_max + 150 回绕为 49。
        let wrapped = near_max.wrapping_add(150);
        assert_eq!(t.bump(1, 10, 10, wrapped, DBL, DX, DY), 2, "跨回绕仍判为双击");
    }

    #[test]
    fn reset_on_far_move_or_other_button() {
        let mut t = ClickTracker::default();
        assert_eq!(t.bump(1, 10, 10, 1000, DBL, DX, DY), 1);
        // 位移超阈值 → 重新计数。
        assert_eq!(t.bump(1, 30, 10, 1100, DBL, DX, DY), 1, "漂移过大不算连击");
        // 换按键 → 重新计数。
        assert_eq!(t.bump(2, 30, 10, 1150, DBL, DX, DY), 1, "换按键不算连击");
    }
}
