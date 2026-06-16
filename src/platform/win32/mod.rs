//! Win32 窗口、消息循环与 GDI 呈现（Phase 0）。
//!
//! 渲染全在 CPU：tiny-skia `Pixmap`（RGBA 预乘）作后备缓冲；呈现时拷贝到
//! GDI `CreateDIBSection`（BGRA）并 `BitBlt` 到窗口 DC。空闲时阻塞在
//! `GetMessageW`，零 CPU。

use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;

use tiny_skia::Pixmap;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EndPaint,
    GetDC, InvalidateRect, ReleaseDC, SelectObject, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VK_BACK, VK_DOWN, VK_ESCAPE, VK_LEFT, VK_RETURN,
    VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, LoadCursorW, PostQuitMessage, RegisterClassExW, SetWindowLongPtrW,
    ShowWindow, TranslateMessage, CREATESTRUCTW, CW_USEDEFAULT, GWLP_USERDATA, IDC_ARROW, MSG,
    SW_SHOW, WINDOW_EX_STYLE, WM_CAPTURECHANGED, WM_CHAR, WM_DESTROY, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SIZE,
    WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
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
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "windui".into(),
            width: 800,
            height: 600,
            bg: Color::hex(0xF3F3F3),
            screenshot: None,
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
    let size = Size::new(cfg.width.max(1), cfg.height.max(1));
    let mut pixmap = Pixmap::new(size.w as u32, size.h as u32).expect("分配 pixmap 失败");
    pixmap.fill(to_skia_color(cfg.bg));
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
    pixmap: Option<Pixmap>,
    // GDI 呈现资源
    memdc: HDC,
    dib: HBITMAP,
    old_bitmap: HGDIOBJ, // memdc 创建时默认选入的位图，销毁前需换回
    dib_bits: *mut u32,
    dib_w: i32,
    dib_h: i32,
}

impl WindowState {
    fn new(handler: Box<dyn AppHandler>, bg: Color) -> Self {
        Self {
            handler,
            bg,
            capturing: false,
            pixmap: None,
            memdc: HDC::default(),
            dib: HBITMAP::default(),
            old_bitmap: HGDIOBJ::default(),
            dib_bits: std::ptr::null_mut(),
            dib_w: 0,
            dib_h: 0,
        }
    }

    /// 确保后备缓冲与 DIB 尺寸匹配客户区；尺寸变化时重建。
    unsafe fn ensure_buffers(&mut self, w: i32, h: i32) {
        let w = w.max(1);
        let h = h.max(1);
        if self.dib_w == w && self.dib_h == h && self.pixmap.is_some() {
            return;
        }
        self.release_gdi();
        // 新建 top-down 32bpp DIB
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // 负数 = top-down，与 pixmap 行序一致
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let screen = GetDC(None);
        let mut bits: *mut c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(screen, &bmi as *const _, DIB_RGB_COLORS, &mut bits, None, 0)
            .expect("CreateDIBSection 失败");
        let memdc = CreateCompatibleDC(screen);
        let old = SelectObject(memdc, HGDIOBJ(dib.0));
        ReleaseDC(None, screen);

        self.old_bitmap = old;
        self.memdc = memdc;
        self.dib = dib;
        self.dib_bits = bits as *mut u32;
        self.dib_w = w;
        self.dib_h = h;
        self.pixmap = Some(Pixmap::new(w as u32, h as u32).expect("分配 pixmap 失败"));
    }

    unsafe fn release_gdi(&mut self) {
        // 顺序很重要：先把 DIB 从 memdc 换出（恢复默认位图），
        // 再删除 DIB 对象，最后删除 memdc。不能删除仍被 DC 选中的对象。
        if !self.memdc.is_invalid() && !self.old_bitmap.is_invalid() {
            let _ = SelectObject(self.memdc, self.old_bitmap);
            self.old_bitmap = HGDIOBJ::default();
        }
        if !self.dib.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(self.dib.0));
            self.dib = HBITMAP::default();
        }
        if !self.memdc.is_invalid() {
            let _ = DeleteDC(self.memdc);
            self.memdc = HDC::default();
        }
        self.dib_bits = std::ptr::null_mut();
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
        self.ensure_buffers(w, h);

        let size = Size::new(self.dib_w, self.dib_h);
        let pixmap = self.pixmap.as_mut().unwrap();
        pixmap.fill(to_skia_color(self.bg));
        self.handler.render(pixmap, size);

        // pixmap(RGBA 预乘) -> DIB(BGRA)，逐像素交换 R/B
        copy_rgba_to_bgra(pixmap.data(), self.dib_bits, (self.dib_w * self.dib_h) as usize);

        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let _ = BitBlt(
            hdc,
            0,
            0,
            self.dib_w,
            self.dib_h,
            self.memdc,
            0,
            0,
            SRCCOPY,
        );
        let _ = EndPaint(hwnd, &ps);
    }
}

impl Drop for WindowState {
    fn drop(&mut self) {
        unsafe { self.release_gdi() };
    }
}

/// 把 RGBA 字节流逐像素交换 R/B 写入 BGRA 缓冲。
fn copy_rgba_to_bgra(src: &[u8], dst: *mut u32, px_count: usize) {
    debug_assert!(src.len() >= px_count * 4);
    let src32 = src.as_ptr() as *const u32;
    unsafe {
        for i in 0..px_count {
            // src 字节序 [R,G,B,A] => u32 = A<<24|B<<16|G<<8|R
            // 用 read_unaligned 避免对 &[u8] 底层指针做未对齐 u32 解引用（规范 UB）。
            let p = src32.add(i).read_unaligned();
            // 目标 [B,G,R,A] => 交换 byte0 与 byte2
            let swapped = (p & 0xFF00_FF00) | ((p & 0x0000_00FF) << 16) | ((p & 0x00FF_0000) >> 16);
            *dst.add(i) = swapped;
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

    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        CLASS_NAME,
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        cfg.width,
        cfg.height,
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

/// 从 lParam 解出客户区坐标，构造并分发指针事件。
///
/// 两段式：先借 state 分发事件并读取意图，**释放借用后**再调用会同步重入
/// WndProc 的 OS API（SetCapture/ReleaseCapture/DestroyWindow），避免 &mut 别名 UB。
unsafe fn handle_pointer(hwnd: HWND, kind: PointerKind, button: MouseButton, lparam: LPARAM) {
    let x = (lparam.0 & 0xffff) as i16 as i32;
    let y = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
    let ev = PointerEvent { kind, pos: Point::new(x, y), button };
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
    } else if vk == VK_LEFT.0 {
        Key::Left
    } else if vk == VK_RIGHT.0 {
        Key::Right
    } else if vk == VK_UP.0 {
        Key::Up
    } else if vk == VK_DOWN.0 {
        Key::Down
    } else {
        Key::Other(vk as u32)
    };
    let ev = KeyEvent { key, pressed: true, shift };
    dispatch_key_event(hwnd, ev);
}

/// WM_CHAR：已翻译的字符（含 IME/CJK 输入）。控制字符跳过。
unsafe fn handle_char(hwnd: HWND, wparam: WPARAM) {
    let Some(c) = char::from_u32(wparam.0 as u32) else { return };
    if c.is_control() {
        return;
    }
    let ev = KeyEvent { key: Key::Char(c), pressed: true, shift: false };
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
