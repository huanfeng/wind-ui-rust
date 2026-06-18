//! Win32 窗口、消息循环与 GDI 呈现。
//!
//! 渲染全在 CPU：单份 tiny-skia `Pixmap`（RGBA 预乘）作后备缓冲；呈现时原地
//! R/B 交换为 BGRA 后 `SetDIBitsToDevice` 直接拷屏。空闲时阻塞在 `GetMessageW`，零 CPU。

pub mod clipboard;
pub mod tray;

pub use tray::{Tray, TrayCtx, TrayMenuItem};

use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;

use tiny_skia::Pixmap;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{FALSE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetDC, GetDeviceCaps, InvalidateRect, ReleaseDC, ScreenToClient,
    SetDIBitsToDevice, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
    PAINTSTRUCT, VREFRESH,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::Ime::{
    ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionWindow, CANDIDATEFORM,
    CFS_CANDIDATEPOS, CFS_POINT, COMPOSITIONFORM,
};
use windows::Win32::UI::Input::Touch::{
    CloseTouchInputHandle, GetTouchInputInfo, RegisterTouchWindow, HTOUCHINPUT,
    REGISTER_TOUCH_WINDOW_FLAGS, TOUCHEVENTF_DOWN, TOUCHEVENTF_MOVE, TOUCHEVENTF_UP, TOUCHINPUT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyState, ReleaseCapture, SetCapture, VK_BACK, VK_CONTROL, VK_DELETE,
    VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB,
    VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageExtraInfo, GetMessageTime, GetMessageW, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowRect, IsIconic, LoadCursorW,
    MsgWaitForMultipleObjectsEx, PeekMessageW, PostQuitMessage, RegisterClassExW, SM_CXDOUBLECLK,
    SM_CXSCREEN, SM_CYDOUBLECLK, SM_CYSCREEN, SetCursor, SetWindowLongPtrW, ShowWindow,
    TranslateMessage, CREATESTRUCTW, CW_USEDEFAULT, GWLP_USERDATA, HTCLIENT, MWMO_INPUTAVAILABLE,
    PM_REMOVE, QS_ALLINPUT, SetWindowPos, IDC_ARROW, IDC_HAND, IDC_IBEAM, MSG, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_SHOW, SW_SHOWNORMAL, LoadIconW, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_CAPTURECHANGED, WM_CHAR, WM_DESTROY, WM_DPICHANGED, WM_IME_COMPOSITION,
    WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_DROPFILES, WM_NCCREATE, WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR,
    WM_SIZE, WM_TOUCH, WNDCLASSEXW, WS_MAXIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_THICKFRAME,
};
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, DragQueryPoint, ShellExecuteW, HDROP,
};

use super::AppHandler;
use crate::event::{CursorShape, Key, KeyEvent, MouseButton, PointerEvent, PointerKind};
use crate::geometry::{Color, Point, Size};

/// 窗口配置。
pub struct WindowConfig {
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub bg: Color,
    /// 窗口居中显示。
    pub centered: bool,
    /// 允许用户调整窗口大小（默认 true）。false 时移除 WS_THICKFRAME 和最大化按钮。
    pub resizable: bool,
    /// 截屏模式：渲染一帧离屏存 PNG 后立即退出，不创建窗口。
    pub screenshot: Option<PathBuf>,
    /// 截屏时的 DPI 缩放（默认 1.0），用于验证高 DPI 渲染。
    pub screenshot_scale: f32,
    /// 截屏前合成一次右键按下（逻辑坐标），用于验证右键菜单等交互视觉。
    pub screenshot_rclick: Option<(i32, i32)>,
    /// 截屏前合成一次左键单击（逻辑坐标，Down+Up），用于验证下拉展开等交互视觉。
    pub screenshot_click: Option<(i32, i32)>,
    /// 系统托盘图标（None=不创建）。窗口创建后安装，窗口销毁时自动清理。
    pub tray: Option<tray::Tray>,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "windui".into(),
            width: 800,
            height: 600,
            bg: Color::hex(0xF3F3F3),
            centered: false,
            resizable: true,
            screenshot: None,
            screenshot_scale: 1.0,
            screenshot_rclick: None,
            screenshot_click: None,
            tray: None,
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
    // 可选：合成一次右键按下（先渲染暖布局，再派发事件，再重绘以捕获菜单）。
    if let Some((lx, ly)) = cfg.screenshot_rclick {
        let pos = Point::new((lx as f32 * s).round() as i32, (ly as f32 * s).round() as i32);
        handler.on_pointer(PointerEvent::single(PointerKind::Down, pos, MouseButton::Right));
        pixmap.fill(to_skia_color(cfg.bg));
        handler.render(&mut pixmap, size);
    }
    // 可选：合成一次左键单击（Down+Up），捕获下拉展开等。
    if let Some((lx, ly)) = cfg.screenshot_click {
        let pos = Point::new((lx as f32 * s).round() as i32, (ly as f32 * s).round() as i32);
        handler.on_pointer(PointerEvent::single(PointerKind::Down, pos, MouseButton::Left));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, pos, MouseButton::Left));
        pixmap.fill(to_skia_color(cfg.bg));
        handler.render(&mut pixmap, size);
    }
    // 有动画时，前进一帧（让动画相位非零）以便不确定进度等可在截图中显现。
    if handler.wants_animation() {
        std::thread::sleep(std::time::Duration::from_millis(300));
        pixmap.fill(to_skia_color(cfg.bg));
        handler.render(&mut pixmap, size);
    }
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
    /// 触摸拖动滚动状态机（触摸提升为鼠标消息后据此区分点击/滑动）。
    touch: Touch,
    /// 系统托盘状态（None=无托盘）。drop 时自动清理图标。
    tray: Option<tray::TrayState>,
}

/// 触摸拖动判定状态。区分"点击"（按下抬起未越阈值）与"滑动滚动"（越阈值后拖动）。
#[derive(Default, Clone, Copy)]
struct Touch {
    down: bool,
    /// 按下起点 + 上一帧位置（客户区物理像素）。
    start: (i32, i32),
    last: (i32, i32),
    /// 是否已越过移动阈值进入滑动滚动。
    scrolling: bool,
    /// 上一次移动的消息时间（ms，`GetMessageTime`）。
    last_t: u32,
    /// 平滑后的 y 速度（**物理像素/ms**），松手时据此启动惯性滑动。
    vy: f32,
}

/// 触摸拖动判定阈值（物理像素）。
const TOUCH_THRESHOLD: i32 = 12;
/// 触摸速度平滑系数（新样本权重）：低通滤噪，又不过度滞后。
const TOUCH_VEL_SMOOTH: f32 = 0.4;

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
            touch: Touch::default(),
            tray: None,
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

unsafe fn run_windowed(mut cfg: WindowConfig, handler: Box<dyn AppHandler>) {
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

    let hmodule = GetModuleHandleW(None).expect("GetModuleHandleW 失败");
    let hinst = HINSTANCE(hmodule.0);
    let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

    let hicon = LoadIconW(hinst, PCWSTR(1usize as *const u16)).unwrap_or_default();
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        lpszClassName: CLASS_NAME,
        hCursor: cursor,
        hIcon: hicon,
        hIconSm: hicon,
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

    let win_style = if cfg.resizable {
        WS_OVERLAPPEDWINDOW
    } else {
        // 固定大小：保留标题栏、系统菜单、最小化按钮，去掉拉伸边框和最大化按钮
        WINDOW_STYLE(
            WS_OVERLAPPEDWINDOW.0
                & !(WS_THICKFRAME.0 | WS_MAXIMIZEBOX.0)
        )
    };

    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        CLASS_NAME,
        PCWSTR(title.as_ptr()),
        win_style,
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

    // 居中窗口
    if cfg.centered {
        let screen_w = GetSystemMetrics(SM_CXSCREEN);
        let screen_h = GetSystemMetrics(SM_CYSCREEN);
        let mut rc = RECT::default();
        let _ = GetWindowRect(hwnd, &mut rc);
        let win_w = rc.right - rc.left;
        let win_h = rc.bottom - rc.top;
        let x = (screen_w - win_w) / 2;
        let y = (screen_h - win_h) / 2;
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOZORDER | SWP_NOSIZE);
    }

    // 注册触摸窗口：触摸以 WM_TOUCH 原始点递送（禁用系统手势；消费后无重复鼠标提升）。
    let _ = RegisterTouchWindow(hwnd, REGISTER_TOUCH_WINDOW_FLAGS(0));

    // 接收文件拖放：拖入文件后以 WM_DROPFILES 递送路径 + 落点。
    DragAcceptFiles(hwnd, true);

    // 系统托盘图标（若配置）：窗口创建后安装，状态存入 WindowState（drop 时清理）。
    if let Some(t) = cfg.tray.take() {
        if let Some(ts) = tray::install(hwnd, t) {
            if let Some(s) = state_from(hwnd) {
                s.tray = Some(ts);
            }
        }
    }

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = UpdateWindow(hwnd);

    run_message_loop(hwnd);
}

/// 消息循环：无动画时阻塞至下一条消息（零 CPU）；有动画时按**帧截止时间**配速——
/// 唤醒后只要距上帧 ≥ FRAME_MS 就重绘一帧，故连续输入下不会超 60fps 空转，
/// 拖动时也不会饿死动画。最小化时强制阻塞避免空转。
///
/// 已知限制：OS 驱动的模态循环（窗口拖拽/缩放、系统菜单跟踪）期间本循环不执行，
/// 动画会暂停至用户释放——单窗口小工具可接受；如需模态期间也动画，需补 WM_TIMER 兜底。
/// 提升系统定时器分辨率到 1ms 的 RAII 守卫。Drop 时 `timeEndPeriod` 归还，
/// 覆盖 panic 展开与所有 return 路径，避免进程级 1ms 分辨率泄漏（影响系统电源）。
struct TimerResolution;
impl TimerResolution {
    fn raise() -> Self {
        unsafe {
            let _ = timeBeginPeriod(1);
        }
        TimerResolution
    }
}
impl Drop for TimerResolution {
    fn drop(&mut self) {
        unsafe {
            let _ = timeEndPeriod(1);
        }
    }
}

unsafe fn run_message_loop(hwnd: HWND) {
    // 动画帧间隔按显示器刷新率取整（默认 60fps 上限，刷新率 <60 时回退到实际值）。
    // 注：仅起始采样一次；跨刷新率不同的显示器移动后不更新（单窗口小工具可接受）。
    let frame_ms = frame_interval_ms(hwnd);
    let mut msg = MSG::default();
    let mut last_frame = std::time::Instant::now();
    // 仅动画期间持有（提升定时器分辨率），空闲时 None 由 Drop 归还，省电。
    let mut hires: Option<TimerResolution> = None;
    loop {
        let animating = !IsIconic(hwnd).as_bool()
            && state_from(hwnd).map(|s| s.handler.wants_animation()).unwrap_or(false);
        if animating {
            // 提升定时器分辨率到 1ms：否则 MsgWait 超时被默认 ~15.6ms tick 向上取整，
            // 16ms 等待常变成 ~31ms → 实测掉到 ~30fps。
            if hires.is_none() {
                hires = Some(TimerResolution::raise());
            }
            // 等待输入，至多到下一帧截止；零句柄，仅作可被输入中断的定时等待。
            let elapsed = last_frame.elapsed().as_millis();
            let wait = frame_ms.saturating_sub(elapsed) as u32;
            MsgWaitForMultipleObjectsEx(None, wait, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
            // 非阻塞排空所有待处理消息。
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return; // hires 的 Drop 归还定时器分辨率
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            // 到达帧截止才推进一帧（与唤醒原因解耦，保证 ≤刷新率且不冻结）。
            if last_frame.elapsed().as_millis() >= frame_ms {
                let _ = InvalidateRect(hwnd, None, false);
                let _ = UpdateWindow(hwnd);
                last_frame = std::time::Instant::now();
            }
        } else {
            // 无动画：归还定时器分辨率，阻塞至下一条消息（零 CPU 空闲）。
            hires = None;
            let r = GetMessageW(&mut msg, None, 0, 0);
            if !r.as_bool() {
                return; // WM_QUIT(0) 或错误(-1)
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            last_frame = std::time::Instant::now(); // 进入动画时从此刻起算首帧
        }
    }
}

/// 动画帧间隔（ms）= 1000 / 目标帧率。目标帧率取窗口所在显示器刷新率，
/// 上限 60（默认）；刷新率 <60（如 50Hz 面板）则回退到实际值；查询失败按 60 处理。
unsafe fn frame_interval_ms(hwnd: HWND) -> u128 {
    let hdc = GetDC(hwnd);
    let hz = if hdc.is_invalid() {
        0
    } else {
        let v = GetDeviceCaps(hdc, VREFRESH);
        let _ = ReleaseDC(hwnd, hdc);
        v
    };
    // VREFRESH 返回 0 或 1 表示"硬件默认"（未知）→ 视为 60。
    let fps = if hz <= 1 { 60 } else { hz.min(60) };
    (1000 / fps.max(1)) as u128
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
        // 客户区光标：按当前悬停控件期望形状设置（链接=手型、文本=I 形）。
        // 仅客户区由我们决定，非客户区（边框/标题栏）交默认处理。
        WM_SETCURSOR => {
            if (lparam.0 & 0xffff) as u32 == HTCLIENT {
                if let Some(state) = state_from(hwnd) {
                    apply_cursor(state.handler.cursor());
                    return LRESULT(1); // TRUE：已处理，阻止默认覆盖为类光标
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
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
        // 原始触摸输入（已 RegisterTouchWindow）：自实现点击/拖动滚动，消费后不交默认（无鼠标提升）。
        WM_TOUCH => {
            handle_touch_input(hwnd, wparam, lparam);
            LRESULT(0)
        }
        // 文件拖放（已 DragAcceptFiles）：取路径 + 落点，路由到落点下的控件。
        WM_DROPFILES => {
            handle_drop_files(hwnd, wparam);
            LRESULT(0)
        }
        // 托盘回调消息：左键/双击触发回调，右键弹原生菜单。
        tray::WM_TRAYICON => {
            if let Some(state) = state_from(hwnd) {
                if let Some(ts) = state.tray.as_mut() {
                    tray::handle_message(ts, lparam);
                }
            }
            LRESULT(0)
        }
        // 输入法开始合成 / 合成中：把候选窗定位到焦点控件的光标处，再交默认处理。
        // 合成期间光标不移动，重复定位到同一点是幂等的；兼顾"候选窗在合成中才出现"
        // 的输入法（仅 STARTCOMPOSITION 可能错过候选窗放置时机）。
        WM_IME_STARTCOMPOSITION | WM_IME_COMPOSITION => {
            handle_ime_position(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
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

/// 按形状加载并设置系统光标（应答 WM_SETCURSOR）。加载失败时静默退回类光标。
unsafe fn apply_cursor(shape: CursorShape) {
    let id = match shape {
        CursorShape::Hand => IDC_HAND,
        CursorShape::Text => IDC_IBEAM,
        CursorShape::Arrow => IDC_ARROW,
    };
    if let Ok(cur) = LoadCursorW(None, id) {
        let _ = SetCursor(cur);
    }
}

/// 处理 WM_DROPFILES：解出拖入的文件路径与落点（客户区物理像素），交宿主路由。
unsafe fn handle_drop_files(hwnd: HWND, wparam: WPARAM) {
    let hdrop = HDROP(wparam.0 as *mut c_void);
    // 落点（客户区物理像素）。
    let mut pt = POINT::default();
    let _ = DragQueryPoint(hdrop, &mut pt);
    // ifile=0xFFFFFFFF + 空缓冲 → 返回文件总数。
    let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
    let mut paths = Vec::with_capacity(count as usize);
    for i in 0..count {
        // 空缓冲先查所需长度（字符数，不含 NUL），再按长度取内容。
        let len = DragQueryFileW(hdrop, i, None);
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u16; len as usize + 1];
        let got = DragQueryFileW(hdrop, i, Some(&mut buf));
        if got > 0 {
            paths.push(PathBuf::from(String::from_utf16_lossy(&buf[..got as usize])));
        }
    }
    DragFinish(hdrop);
    if paths.is_empty() {
        return;
    }
    let repaint = {
        let Some(state) = state_from(hwnd) else { return };
        state.handler.on_drop_files(Point::new(pt.x, pt.y), paths)
    };
    if repaint {
        let _ = InvalidateRect(hwnd, None, false);
    }
    if state_from(hwnd).map(|s| s.handler.wants_close()).unwrap_or(false) {
        let _ = DestroyWindow(hwnd);
    }
}

/// 用系统默认程序打开 URL/路径（`ShellExecuteW` 的 "open" 动词）。fire-and-forget，忽略结果。
pub fn open_url(url: &str) {
    let verb = w!("open");
    let target: Vec<u16> = url.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            verb,
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
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
    // 触摸提升的鼠标消息：忽略（触摸已由 WM_TOUCH 完整处理，避免点击双重触发）。
    if is_touch_event() {
        return;
    }
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

/// 当前消息是否来自触摸/笔（被提升为鼠标消息时附加信息带 0xFF515700 签名）。
/// 用于在鼠标路径忽略触摸提升的重复消息——触摸统一由 WM_TOUCH 处理。
unsafe fn is_touch_event() -> bool {
    const SIGNATURE: usize = 0xFF51_5700;
    const MASK: usize = 0xFFFF_FF00;
    (GetMessageExtraInfo().0 as usize & MASK) == SIGNATURE
}

/// 解码 WM_TOUCH 原始触摸点，对主接触点跑触摸状态机。坐标为屏幕 1/100 像素。
/// 调用方消费后返回 0（不交 DefWindowProc，故不会再有重复的鼠标提升消息）。
unsafe fn handle_touch_input(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    let count = wparam.0 & 0xffff;
    if count == 0 {
        return;
    }
    let hti = HTOUCHINPUT(lparam.0 as *mut c_void);
    // 最多取 8 指；单指滚动只用主接触点。
    let mut inputs = [TOUCHINPUT::default(); 8];
    let n = count.min(inputs.len());
    let ok = GetTouchInputInfo(hti, &mut inputs[..n], size_of::<TOUCHINPUT>() as i32).is_ok();
    let _ = CloseTouchInputHandle(hti);
    if !ok {
        return;
    }
    // 主接触点（首个）。屏幕 1/100 像素 → 客户区物理像素。
    let ti = inputs[0];
    let mut pt = POINT { x: ti.x / 100, y: ti.y / 100 };
    let _ = ScreenToClient(hwnd, &mut pt);
    let kind = if ti.dwFlags.0 & TOUCHEVENTF_DOWN.0 != 0 {
        PointerKind::Down
    } else if ti.dwFlags.0 & TOUCHEVENTF_UP.0 != 0 {
        PointerKind::Up
    } else if ti.dwFlags.0 & TOUCHEVENTF_MOVE.0 != 0 {
        PointerKind::Move
    } else {
        return;
    };
    // 当前触摸消息时间（与移动采样同源），用于估算释放速度。
    let t = GetMessageTime() as u32;
    handle_touch(hwnd, kind, pt.x, pt.y, t);
}

/// 触摸状态机：按下抬起未越阈值=点击（合成正常派发）；越阈值后拖动=滚动手指下的容器；
/// 松手带速度=惯性滑动。两段式：每次先借 state 读/写触摸态，释放后再调可能重入的分发。
unsafe fn handle_touch(hwnd: HWND, kind: PointerKind, x: i32, y: i32, t: u32) {
    match kind {
        PointerKind::Down => {
            // 新触摸按下：打断进行中的惯性滑动（停住动量）。
            cancel_fling(hwnd);
            if let Some(s) = state_from(hwnd) {
                s.touch = Touch {
                    down: true,
                    start: (x, y),
                    last: (x, y),
                    last_t: t,
                    ..Touch::default()
                };
            }
        }
        PointerKind::Move => {
            let (down, start, last, last_t, scrolling, vy) = match state_from(hwnd) {
                Some(s) => (
                    s.touch.down,
                    s.touch.start,
                    s.touch.last,
                    s.touch.last_t,
                    s.touch.scrolling,
                    s.touch.vy,
                ),
                None => return,
            };
            if !down {
                return;
            }
            let dy = y - last.1;
            // 估算瞬时速度并低通平滑（dt=0 的重复样本跳过，避免除零）。
            let dt = t.wrapping_sub(last_t) as i32;
            let vy = if dt > 0 {
                let inst = dy as f32 / dt as f32;
                vy * (1.0 - TOUCH_VEL_SMOOTH) + inst * TOUCH_VEL_SMOOTH
            } else {
                vy
            };
            let past = scrolling
                || (x - start.0).abs() >= TOUCH_THRESHOLD
                || (y - start.1).abs() >= TOUCH_THRESHOLD;
            if let Some(s) = state_from(hwnd) {
                s.touch.last = (x, y);
                s.touch.last_t = t;
                s.touch.vy = vy;
                if past {
                    s.touch.scrolling = true;
                }
            }
            if past {
                dispatch_pan(hwnd, Point::new(x, y), dy);
            }
        }
        PointerKind::Up => {
            let (down, start, scrolling, vy) = match state_from(hwnd) {
                Some(s) => (s.touch.down, s.touch.start, s.touch.scrolling, s.touch.vy),
                None => return,
            };
            if let Some(s) = state_from(hwnd) {
                s.touch.down = false;
                s.touch.scrolling = false;
            }
            if down && scrolling {
                // 拖动滚动后松手：按释放速度启动惯性滑动（速度过低时宿主会忽略）。
                dispatch_fling(hwnd, Point::new(x, y), vy);
            } else if down {
                // 未进入滚动 → 视为点击：在起点合成按下，抬起处合成抬起，走正常派发。
                dispatch_pointer_event(
                    hwnd,
                    PointerEvent::single(PointerKind::Down, Point::new(start.0, start.1), MouseButton::Left),
                );
                dispatch_pointer_event(
                    hwnd,
                    PointerEvent::single(PointerKind::Up, Point::new(x, y), MouseButton::Left),
                );
            }
        }
        _ => {}
    }
}

/// 触摸滚动：把 dy 注入手指下的滚动容器（两段式：借用读取后释放再 InvalidateRect）。
unsafe fn dispatch_pan(hwnd: HWND, pos: Point, dy: i32) {
    let repaint = {
        let Some(state) = state_from(hwnd) else { return };
        state.handler.on_pan(pos, dy)
    };
    if repaint {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// 触摸松手：按释放速度启动惯性滑动。启动后触发首帧，其余由动画循环按帧推进。
unsafe fn dispatch_fling(hwnd: HWND, pos: Point, vy: f32) {
    let started = {
        let Some(state) = state_from(hwnd) else { return };
        state.handler.start_fling(pos, vy)
    };
    if started {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// 打断进行中的惯性滑动（新触摸按下时调用）。
unsafe fn cancel_fling(hwnd: HWND) {
    let repaint = {
        let Some(state) = state_from(hwnd) else { return };
        state.handler.cancel_fling()
    };
    if repaint {
        let _ = InvalidateRect(hwnd, None, false);
    }
}

/// 把输入法合成窗 + 候选窗定位到焦点文本控件的光标处。
/// 光标位置由 handler 提供（物理像素、相对客户区），无文本焦点时不动作。
unsafe fn handle_ime_position(hwnd: HWND) {
    let caret = match state_from(hwnd) {
        Some(s) => s.handler.ime_caret(),
        None => return,
    };
    let Some((x, y, h)) = caret else { return };
    let himc = ImmGetContext(hwnd);
    if himc.0.is_null() {
        return; // 无输入法上下文
    }
    // 合成串定位在光标处。
    let cf = COMPOSITIONFORM {
        dwStyle: CFS_POINT,
        ptCurrentPos: POINT { x, y },
        rcArea: RECT::default(),
    };
    let _ = ImmSetCompositionWindow(himc, &cf);
    // 候选窗放在光标行下方，避免遮住输入处。
    let cand = CANDIDATEFORM {
        dwIndex: 0,
        dwStyle: CFS_CANDIDATEPOS,
        ptCurrentPos: POINT { x, y: y + h },
        rcArea: RECT::default(),
    };
    let _ = ImmSetCandidateWindow(himc, &cand);
    let _ = ImmReleaseContext(hwnd, himc);
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
