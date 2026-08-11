//! Win32 窗口、消息循环与 GDI 呈现。
//!
//! 渲染全在 CPU：单份 tiny-skia `Pixmap`（RGBA 预乘）作后备缓冲；呈现时原地
//! R/B 交换为 BGRA 后 `SetDIBitsToDevice` 直接拷屏。空闲时阻塞在 `GetMessageW`，零 CPU。

pub mod clipboard;
#[cfg(feature = "d2d")]
mod d2d;
pub mod hotkey;
pub mod tray;

pub use tray::{Tray, TrayCtx, TrayMenuItem};

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::size_of;
use std::path::PathBuf;

use tiny_skia::Pixmap;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetDC, GetDeviceCaps, InvalidateRect, ReleaseDC, ScreenToClient,
    SetDIBitsToDevice, UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DEFAULT_CHARSET,
    DIB_RGB_COLORS, LOGFONTW, PAINTSTRUCT, VREFRESH,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{MARGINS, WM_MOUSELEAVE};
use windows::Win32::UI::HiDpi::{
    AdjustWindowRectExForDpi, GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi,
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::Ime::{
    ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionFontW,
    ImmSetCompositionWindow, CANDIDATEFORM, CFS_CANDIDATEPOS, CFS_POINT, COMPOSITIONFORM,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, GetKeyState, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT,
    VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Input::Touch::{
    CloseTouchInputHandle, GetTouchInputInfo, RegisterTouchWindow, HTOUCHINPUT,
    REGISTER_TOUCH_WINDOW_FLAGS, TOUCHEVENTF_DOWN, TOUCHEVENTF_MOVE, TOUCHEVENTF_UP, TOUCHINPUT,
};
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, DragQueryPoint, ShellExecuteW, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetMessageExtraInfo, GetMessageTime, GetMessageW, GetSystemMetrics, GetWindowLongPtrW,
    GetWindowRect, IsIconic, IsZoomed, LoadCursorW, LoadIconW, MsgWaitForMultipleObjectsEx,
    PeekMessageW, PostMessageW, PostQuitMessage, RegisterClassExW, SetCursor, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage,
    CREATESTRUCTW, CW_USEDEFAULT, GWLP_USERDATA, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION,
    HTCLIENT, HTLEFT, HTRIGHT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IDC_ARROW, IDC_HAND, IDC_IBEAM,
    MINMAXINFO, MSG, MWMO_INPUTAVAILABLE, NCCALCSIZE_PARAMS, PM_REMOVE, QS_ALLINPUT,
    SIZE_MINIMIZED, SM_CXDOUBLECLK, SM_CXFRAME, SM_CXPADDEDBORDER, SM_CXSCREEN, SM_CYDOUBLECLK,
    SM_CYFRAME, SM_CYSCREEN, SPI_GETCLIENTAREAANIMATION, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOW,
    SW_SHOWNORMAL, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_DROPFILES,
    WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_HOTKEY, WM_IME_COMPOSITION,
    WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCALCSIZE, WM_NCCREATE, WM_NCHITTEST, WM_NCMOUSEMOVE,
    WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZE, WM_TIMER, WM_TOUCH,
    WNDCLASSEXW, WS_MAXIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_THICKFRAME,
};
// 只用于 d2d 后端选择（RDP 远程会话下强制软渲染），随该 feature 一起门控。
#[cfg(feature = "d2d")]
use windows::Win32::UI::WindowsAndMessaging::SM_REMOTESESSION;

use super::{AppHandler, WindowConfig};
use crate::event::{CursorShape, Key, KeyEvent, MouseButton, PointerEvent, PointerKind, WindowOp};
use crate::geometry::{Color, Point, Size};

thread_local! {
    /// wnd_proc 入口处写入当前 HWND；PickDialog::pick_* 读取以注入父窗口。
    static ACTIVE_HWND: Cell<isize> = const { Cell::new(0) };
}

/// 供 platform::inject_parent 读取当前活跃窗口句柄（单线程，消息循环内保证有效）。
pub(super) fn active_hwnd() -> isize {
    ACTIVE_HWND.with(|h| h.get())
}

/// 查询系统"显示动画"设置（无障碍/省电）。查询失败默认开。
unsafe fn os_animations_enabled() -> bool {
    let mut on = windows::core::BOOL(1);
    let ok = SystemParametersInfoW(
        SPI_GETCLIENTAREAANIMATION,
        0,
        Some(&mut on as *mut _ as *mut core::ffi::c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok();
    if ok {
        on.as_bool()
    } else {
        true
    }
}

/// 运行应用：截屏模式离屏渲染存盘；否则创建窗口进入消息循环（阻塞至退出）。
pub(crate) fn run(
    cfg: WindowConfig,
    mut handler: Box<dyn AppHandler>,
    waker: Option<std::sync::Arc<crate::sync::WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
) {
    // 全局动画开关：显式配置优先；否则截屏路径恒开（保证终态稳定）、窗口路径随系统设置。
    let os_default = if cfg.screenshot.is_some() {
        true
    } else {
        unsafe { os_animations_enabled() }
    };
    crate::anim::set_enabled(cfg.animations.unwrap_or(os_default));
    if let Some(path) = cfg.screenshot.clone() {
        // 离屏渲染走平台无关的共享实现（与 macOS 后端共用）。
        super::run_offscreen(&cfg, &mut handler, &path);
        return;
    }
    // 单实例：检测；二次实例把 argv 转发给首实例后直接返回、不建窗口。
    // 若转发失败（首实例正退出中/僵死、消息泵已停），回退为正常启动新窗口，
    // 避免被一个无响应的首实例永久挡在门外。
    if let Some(si) = &single {
        if !crate::single_instance::acquire(&si.app_id) {
            let argv: Vec<String> = std::env::args().collect();
            if crate::single_instance::forward(&si.app_id, &argv) {
                return;
            }
        }
    }
    unsafe { run_windowed(cfg, handler, waker, single) };
}

// ── 渲染后端接缝 ────────────────────────────────────────────────────────────
// `WinRenderBackend` 把"如何把一帧呈现到 HWND"的策略封装到独立对象后面，
// 让 `WindowState` 与具体后端（Skia/CPU、未来的 Direct2D）解耦。
// 两个方法均为 `unsafe`：内部直接调用 Win32 GDI API。

trait WinRenderBackend {
    /// 客户区尺寸变化时预先调整缓冲（可选；paint 内部的 ensure 同样处理）。
    /// 当前路径仅用 `paint` 内的 `ensure` 懒建缓冲；此方法为后续 D2D 后端预留。
    #[allow(dead_code)]
    fn resize(&mut self, w: i32, h: i32);
    /// 渲染并呈现一帧：内部清屏 → 构造 target → handler.render → present。
    /// 0×0 客户区仍配对 BeginPaint/EndPaint 但不绘制。
    ///
    /// 返回 `true` 表示后端已不可用、需由 `WindowState` 降级替换为软后端
    /// （D2D 设备丢失且连续重建失败时）。软后端恒返回 `false`。
    unsafe fn paint(&mut self, hwnd: HWND, bg: Color, handler: &mut dyn AppHandler) -> bool;
}

/// CPU 软件渲染后端：tiny-skia `Pixmap` 作后备缓冲，`SetDIBitsToDevice` 呈现。
struct SkiaBackend {
    pixmap: Option<Pixmap>,
    buf_w: i32,
    buf_h: i32,
}

impl SkiaBackend {
    fn new() -> Self {
        Self {
            pixmap: None,
            buf_w: 0,
            buf_h: 0,
        }
    }

    /// 确保后备缓冲匹配目标尺寸；尺寸变化时重建。
    fn ensure(&mut self, w: i32, h: i32) {
        let w = w.max(1);
        let h = h.max(1);
        if self.buf_w == w && self.buf_h == h && self.pixmap.is_some() {
            return;
        }
        self.pixmap = Some(Pixmap::new(w as u32, h as u32).expect("分配 pixmap 失败"));
        self.buf_w = w;
        self.buf_h = h;
    }
}

impl WinRenderBackend for SkiaBackend {
    fn resize(&mut self, w: i32, h: i32) {
        self.ensure(w, h);
    }

    unsafe fn paint(&mut self, hwnd: HWND, bg: Color, handler: &mut dyn AppHandler) -> bool {
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        // 最小化时客户区为 0×0：仍需配对 BeginPaint/EndPaint 校验区域，但不绘制。
        if w <= 0 || h <= 0 {
            let mut ps = PAINTSTRUCT::default();
            let _ = BeginPaint(hwnd, &mut ps);
            let _ = EndPaint(hwnd, &ps);
            return false;
        }
        self.ensure(w, h);

        let size = Size::new(self.buf_w, self.buf_h);
        let pixmap = self.pixmap.as_mut().unwrap();
        pixmap.fill(to_skia_color(bg));
        // target 借用 self.pixmap，限定在块内：块结束借用即释放，再重取引用做后续处理。
        {
            let mut tgt = crate::render::PixmapTarget { pixmap };
            handler.render(&mut tgt, size);
        }
        let pixmap = self.pixmap.as_mut().unwrap();
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
        false // 软后端永不失效
    }
}

// ────────────────────────────────────────────────────────────────────────────

/// 窗口端运行时状态，指针挂在 HWND 的 GWLP_USERDATA 上。
struct WindowState {
    handler: Box<dyn AppHandler>,
    bg: Color,
    /// 当前是否已对窗口调用 OS SetCapture（与 handler 逻辑捕获态同步）。
    capturing: bool,
    /// 渲染后端：封装"如何把一帧呈现到 HWND"的全部逻辑。
    /// 当前为 CPU Skia 路径；后续可替换为 Direct2D 后端而无需改动 WindowState。
    backend: Box<dyn WinRenderBackend>,
    /// 连续点击跟踪（用于双击/三击判定）。
    last_click: ClickTracker,
    /// 触摸拖动滚动状态机（触摸提升为鼠标消息后据此区分点击/滑动）。
    touch: Touch,
    /// 系统托盘状态（None=无托盘）。drop 时自动清理图标。
    tray: Option<tray::TrayState>,
    /// 全局热键状态（None=无热键）。drop 时自动注销。
    hotkeys: Option<hotkey::HotkeyState>,
    /// 无标题栏窗口：wnd_proc 据此处理 WM_NCCALCSIZE / WM_NCHITTEST。
    frameless: bool,
    /// 是否已向系统申请鼠标离开通知（TrackMouseEvent）。离开后系统清此标志需重新申请。
    mouse_tracked: bool,
    /// WM_CHAR 暂存的高代理项：补充平面字符（emoji 等）分两条 WM_CHAR 发来 UTF-16 代理对。
    pending_surrogate: Option<u16>,
    /// 窗口最小客户区尺寸（逻辑 dp，0=不限制）。WM_GETMINMAXINFO 据此换算物理像素下限。
    min_w: i32,
    min_h: i32,
    /// 是否处于交互式拖拽移动/缩放的模态循环内（WM_ENTERSIZEMOVE..WM_EXITSIZEMOVE）。
    /// 据此在 WM_SIZE 里分流：拖拽中走异步重绘（免 vsync 节流拖累手感），
    /// 非拖拽的最大化/还原走同步重绘（避免 DWM 动画采样到旧尺寸缓冲被拉伸变形）。
    in_size_move: bool,
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
    fn bump(
        &mut self,
        button: i32,
        x: i32,
        y: i32,
        now_ms: u32,
        dbl_ms: u32,
        dx: i32,
        dy: i32,
    ) -> u8 {
        let continued = self.count > 0
            && self.button == button
            && now_ms.wrapping_sub(self.time_ms) <= dbl_ms
            && (x - self.x).abs() <= dx
            && (y - self.y).abs() <= dy;
        let count = if continued {
            (self.count + 1).min(3)
        } else {
            1
        };
        *self = ClickTracker {
            time_ms: now_ms,
            x,
            y,
            button,
            count,
        };
        count
    }
}

impl WindowState {
    fn new(handler: Box<dyn AppHandler>, bg: Color) -> Self {
        Self {
            handler,
            bg,
            capturing: false,
            backend: Box::new(SkiaBackend::new()),
            last_click: ClickTracker::default(),
            touch: Touch::default(),
            tray: None,
            hotkeys: None,
            frameless: false,
            mouse_tracked: false,
            pending_surrogate: None,
            min_w: 0,
            min_h: 0,
            in_size_move: false,
        }
    }

    /// 渲染并呈现到窗口。后端报告失效（D2D 设备丢失且连续重建失败）时降级为软后端。
    unsafe fn paint(&mut self, hwnd: HWND) {
        let bg = self.bg;
        let downgrade = self.backend.paint(hwnd, bg, self.handler.as_mut());
        if downgrade {
            // 替换为软后端并请求重绘：下一帧用 Skia 呈现，进程不崩、内容继续渲染。
            self.backend = Box::new(SkiaBackend::new());
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
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

use super::to_skia_color;

const CLASS_NAME: PCWSTR = w!("WindUiWindowClass");

/// 跨线程唤醒消息（WM_APP+2；WM_APP+1 已用于托盘）。
const WM_APP_WAKE: u32 = WM_APP + 2;

/// 跨线程唤醒句柄：仅持 HWND 数值，PostMessage 线程安全。
struct Win32Wake {
    hwnd: isize,
}
unsafe impl Send for Win32Wake {}
impl crate::sync::RawWakeSignal for Win32Wake {
    fn signal(&self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_APP_WAKE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

unsafe fn run_windowed(
    mut cfg: WindowConfig,
    handler: Box<dyn AppHandler>,
    waker: Option<std::sync::Arc<crate::sync::WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
) {
    let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

    let hmodule = GetModuleHandleW(None).expect("GetModuleHandleW 失败");
    let hinst = HINSTANCE(hmodule.0);
    let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

    // MAKEINTRESOURCE(1)=IDI_APPLICATION：整数 1 当资源序号传入（低 64K 表示序号而非字符串指针）。
    // 这不是“悬垂指针”，故抑制 clippy；其自动建议 ptr::dangling() 会把序号改成 u16 对齐值(2)，是语义错误。
    #[allow(clippy::manual_dangling_ptr)]
    let hicon = LoadIconW(Some(hinst), PCWSTR(1usize as *const u16)).unwrap_or_default();
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
    let mut state = Box::new(WindowState::new(handler, cfg.bg));
    state.min_w = cfg.min_width;
    state.min_h = cfg.min_height;
    let state_ptr = Box::into_raw(state);

    let title: Vec<u16> = cfg.title.encode_utf16().chain(std::iter::once(0)).collect();

    // cfg 宽高为逻辑 dp（期望客户区）。按系统 DPI 反算窗口外框物理尺寸，
    // 使客户区 = cfg × scale，避免标题栏/边框吃掉内容空间导致超出。
    let sys_dpi = {
        let d = GetDpiForSystem();
        if d == 0 {
            96
        } else {
            d
        }
    };
    let init_scale = sys_dpi as f32 / 96.0;
    let (phys_w, phys_h) = frame_size_for_client(cfg.width, cfg.height, init_scale, sys_dpi);

    let win_style = if cfg.resizable {
        WS_OVERLAPPEDWINDOW
    } else {
        // 固定大小：保留标题栏、系统菜单、最小化按钮，去掉拉伸边框和最大化按钮
        WINDOW_STYLE(WS_OVERLAPPEDWINDOW.0 & !(WS_THICKFRAME.0 | WS_MAXIMIZEBOX.0))
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
        Some(hinst),
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
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            w,
            h,
            SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE,
        );
    }
    if let Some(s) = state_from(hwnd) {
        s.handler.set_scale(scale);
    }

    // GPU 后端选择（窗口级显式 opt-in）：`cfg.accelerated` 为真（或调试环境变量
    // WINDUI_D2D=1 强制）时尝试用 Direct2D 后端替换软后端。try_create 需要已就绪的
    // HWND 与客户区尺寸，故在窗口创建并完成尺寸校正后切换。
    // 强制软渲染条件：RDP 远程会话（flip-model swapchain 在远程桌面不可用）。
    // 离屏截图走 run_offscreen，根本不到此处，恒软。设备创建失败保持软后端（绝不 panic）。
    #[cfg(feature = "d2d")]
    {
        let env_force = std::env::var("WINDUI_D2D").is_ok_and(|v| v != "0" && !v.is_empty());
        let is_remote = GetSystemMetrics(SM_REMOTESESSION) != 0;
        if (cfg.accelerated || env_force) && !is_remote {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
            match d2d::try_create(hwnd, cw, ch) {
                Some(b) => {
                    if let Some(s) = state_from(hwnd) {
                        s.backend = Box::new(b);
                    }
                }
                None => eprintln!("[windui] D2D 设备创建失败，回退软渲染（Skia）"),
            }
        }
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

    // 全局热键（若配置）：窗口创建后注册，状态存入 WindowState（drop 时自动注销）。
    // 注册失败不阻止启动——热键是全局独占资源，被占用是常态而非异常。
    if !cfg.hotkeys.is_empty() {
        let hs = hotkey::HotkeyState::register(hwnd, std::mem::take(&mut cfg.hotkeys));
        if let Some(s) = state_from(hwnd) {
            s.hotkeys = Some(hs);
        }
    }

    // 系统托盘图标（若配置）：窗口创建后安装，状态存入 WindowState（drop 时清理）。
    if let Some(t) = cfg.tray.take() {
        if let Some(ts) = tray::install(hwnd, t) {
            if let Some(s) = state_from(hwnd) {
                s.tray = Some(ts);
            }
        }
    }

    // 跨线程唤醒：绑定平台句柄（hwnd 数值 + PostMessage），此前积压的 wake 会立即补发。
    if let Some(w) = &waker {
        w.bind(Box::new(Win32Wake {
            hwnd: hwnd.0 as isize,
        }));
    }

    // 单实例首实例：建 message-only 窗口接收二次实例 argv（UI 线程切页 + 激活主窗口）。
    if let Some(si) = single {
        crate::single_instance::install_listener(&si.app_id, hwnd.0 as isize, si.on_second);
    }

    // 注册周期定时器（on_interval）：timer id 从 1 起，靠 WM_TIMER 派发。
    if let Some(s) = state_from(hwnd) {
        for (i, d) in s.handler.intervals().iter().enumerate() {
            let ms = (d.as_millis() as u32).max(1);
            let _ = SetTimer(Some(hwnd), i + 1, ms, None);
        }
    }

    // 无边框窗口：标记状态，扩展 DWM 边框保留窗口投影，并触发非客户区重算
    // （SWP_FRAMECHANGED → WM_NCCALCSIZE 让客户区铺满整窗）。
    if cfg.frameless {
        if let Some(s) = state_from(hwnd) {
            s.frameless = true;
        }
        let margins = MARGINS {
            cxLeftWidth: 0,
            cxRightWidth: 0,
            cyTopHeight: 1,
            cyBottomHeight: 0,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
        // 圆角：显式声明，与 Win11 系统其余窗口一致。
        //
        // 不依赖 DWM 默认策略：本窗口保留着 WS_OVERLAPPEDWINDOW 样式位（非客户区是靠
        // WM_NCCALCSIZE 消掉的，不是换成 WS_POPUP），这类窗口在 Win11 上**通常**默认
        // 就是圆角——但自定义 NCCALCSIZE 之后该默认是否仍成立并无明确保证，显式声明
        // 比赌默认行为可靠。
        //
        // 无版本判断也是刻意的：该属性是 Win11（build 22000+）才有的，旧系统上 DWM
        // 不认识这个属性号，返回 E_INVALIDARG——我们在此丢弃它，正好得到想要的降级
        // （Win10 本就没有圆角窗口一说）。属性号 33 在 Win10 上无任何合法属性占用，
        // 不存在误设成别的属性的风险，故无需 GetVersionEx 分支。
        let pref = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const c_void,
            // 用 size_of_val 而非写死类型名：尺寸与指针由同一个绑定推导，
            // 日后有人改 `pref` 的类型时不会留下静默失配的尺寸参数。
            size_of_val(&pref) as u32,
        );
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
        );
    }

    // 启动即隐藏：常驻托盘类应用不该在启动时闪一下窗口。此处**不调用 ShowWindow**，
    // 窗口保持初始的不可见态，等托盘点击或全局热键送来 WindowOp::Show。
    if !cfg.start_hidden {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
    }

    run_message_loop(hwnd);

    // 消息循环结束后立即显式释放 GPU 共享设备链（D3D11/DXGI/D2D/DWrite COM 对象）。
    // 推迟到线程析构才 Release 会触发 GPU 命令队列排空 + DWrite 字体缓存全局清理，
    // 实测延迟可达 3–4 秒；此处提前释放可规避该问题。
    #[cfg(feature = "d2d")]
    d2d::release_shared_device();
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
            && state_from(hwnd)
                .map(|s| s.handler.wants_animation())
                .unwrap_or(false);
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
                let _ = InvalidateRect(Some(hwnd), None, false);
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
    let hdc = GetDC(Some(hwnd));
    let hz = if hdc.is_invalid() {
        0
    } else {
        let v = GetDeviceCaps(Some(hdc), VREFRESH);
        let _ = ReleaseDC(Some(hwnd), hdc);
        v
    };
    // VREFRESH 返回 0 或 1 表示"硬件默认"（未知）→ 视为 60；否则跟随显示器刷新率
    // （高刷屏吃到 120/144Hz，局部重绘已让每帧足够廉价）。上限 240 兜底异常驱动值。
    let fps = if hz <= 1 { 60 } else { hz.min(240) };
    (1000 / fps.max(1)) as u128
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 记录当前 HWND，供 PickDialog 在事件回调中读取并注入为父窗口句柄。
    ACTIVE_HWND.with(|h| h.set(hwnd.0 as isize));
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
            // 帧路径也消费热键操作：pump/interval/信号驱动的改绑（非事件路径）
            // 经 request_repaint 唤帧后在此落地，不必等下一次用户事件。
            apply_hotkey_ops(hwnd);
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            // 限制窗口最小尺寸：把逻辑 dp 下限按当前 DPI 换算为物理像素写入 ptMinTrackSize。
            // 无边框窗口外框≈客户区，直接用 client×scale；带框窗口经 AdjustWindowRect 计入边框。
            if let Some(state) = state_from(hwnd) {
                if state.min_w > 0 || state.min_h > 0 {
                    let dpi = GetDpiForWindow(hwnd).max(96);
                    let scale = dpi as f32 / 96.0;
                    let (pw, ph) = if state.frameless {
                        (
                            (state.min_w as f32 * scale).round() as i32,
                            (state.min_h as f32 * scale).round() as i32,
                        )
                    } else {
                        frame_size_for_client(state.min_w, state.min_h, scale, dpi)
                    };
                    let mmi = lparam.0 as *mut MINMAXINFO;
                    if !mmi.is_null() {
                        if pw > 0 {
                            (*mmi).ptMinTrackSize.x = pw;
                        }
                        if ph > 0 {
                            (*mmi).ptMinTrackSize.y = ph;
                        }
                    }
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_SIZE => {
            // 最小化（客户区 0×0）：无可见内容，跳过 resize/重绘，避免 1×1 无效缓冲。
            if wparam.0 as u32 == SIZE_MINIMIZED {
                return LRESULT(0);
            }
            // 客户区变化：通知后端调整缓冲（D2D 需 ResizeBuffers；Skia 为懒建无副作用），
            // 再重绘。lParam 低/高字为新客户区宽/高（物理像素）。
            let w = (lparam.0 & 0xffff) as i32;
            let h = ((lparam.0 >> 16) & 0xffff) as i32;
            if let Some(state) = state_from(hwnd) {
                state.backend.resize(w, h);
                if state.in_size_move {
                    // 拖拽缩放中：异步重绘，避免每次 WM_SIZE 都同步等 vsync 拖累拖拽手感。
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else {
                    // 最大化/还原等一次性尺寸变化：ResizeBuffers 后同步出一帧，保证 DWM 动画
                    // 无论何时采样后备缓冲都是新尺寸的正确内容，不会拉伸旧内容成变形左上角。
                    // paint 内部会 ValidateRect 整个客户区，不会再触发多余 WM_PAINT。
                    state.paint(hwnd);
                }
            }
            LRESULT(0)
        }
        // 进入/退出交互式拖拽移动/缩放模态循环：标记状态，供 WM_SIZE 分流同步/异步重绘。
        WM_ENTERSIZEMOVE => {
            if let Some(state) = state_from(hwnd) {
                state.in_size_move = true;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_EXITSIZEMOVE => {
            if let Some(state) = state_from(hwnd) {
                state.in_size_move = false;
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // 无边框：非客户区计算 → 客户区铺满整窗（去系统标题栏/边框）。
        // 最大化时用默认（含任务栏避让、正确插入边框），非最大化返回 0 即整窗。
        WM_NCCALCSIZE if wparam.0 != 0 && is_frameless(hwnd) => handle_nccalcsize(hwnd, lparam),
        // 无边框：自定义命中——边缘做缩放，拖动区做 HTCAPTION，其余 HTCLIENT。
        WM_NCHITTEST if is_frameless(hwnd) => handle_nchittest(hwnd, lparam),
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
            // 申请离开通知：鼠标移出客户区（含移入标题栏等非客户区）时收到 WM_MOUSELEAVE，
            // 以便补发 Leave、清除滞留的悬停态（如标题栏按钮）。
            track_mouse_leave(hwnd);
            handle_pointer(hwnd, PointerKind::Move, MouseButton::Left, lparam);
            LRESULT(0)
        }
        // 鼠标移入非客户区（无边框窗口的标题栏拖动区/缩放边框）：系统改发 NCMOUSEMOVE
        // 而非 MOUSEMOVE。按真实位置补发一个 Move（NCMOUSEMOVE 的 lParam 是屏幕坐标）：
        // 落在拖动区→命中非按钮→清除残留悬停（修最小化按钮卡 hover）；落在按钮顶部
        // 的 HTTOP 缩放条上→仍命中该按钮→保留高亮（不误清）。
        WM_NCMOUSEMOVE => {
            handle_nc_mouse_move(hwnd, lparam);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // 鼠标离开客户区（移到非客户区或移出窗口）：清除悬停态。
        WM_MOUSELEAVE => {
            if let Some(s) = state_from(hwnd) {
                s.mouse_tracked = false;
            }
            clear_hover(hwnd);
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
        // 周期定时器回调：timer id = interval 索引 + 1。
        WM_TIMER => {
            let id = wparam.0;
            let need = state_from(hwnd)
                .map(|s| s.handler.on_interval_fired(id.saturating_sub(1)))
                .unwrap_or(false);
            if need {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        // 跨线程唤醒：触发一帧（render 前会排空消息通道）。
        WM_APP_WAKE => {
            let _ = InvalidateRect(Some(hwnd), None, false);
            LRESULT(0)
        }
        // 托盘回调消息：左键/双击触发回调，右键弹原生菜单。
        // 全局热键：系统投递到本窗口队列（事件驱动，不轮询，故不破坏空闲零 CPU）。
        //
        // 严格两段式（铁律 6）：第一段借 state 跑回调、取出意图；借用在语句结束时释放。
        // 第二段才碰 OS——`ShowWindow`/`SetForegroundWindow` 会同步派发 WM_SHOWWINDOW /
        // WM_ACTIVATE 回本函数，届时会再 `state_from` 一次。若此刻第一段的借用还活着，
        // 就是两个 `&mut WindowState` 并存的 UB（无 RefCell，不会 panic，只会静默出错）。
        WM_HOTKEY => {
            let op = state_from(hwnd)
                .and_then(|s| s.hotkeys.as_mut())
                .and_then(|hs| hs.dispatch(wparam.0));
            run_window_op(hwnd, op);
            LRESULT(0)
        }
        tray::WM_TRAYICON => {
            on_tray_message(hwnd, lparam);
            LRESULT(0)
        }
        // 输入法开始合成：通知焦点控件进入组合态（自绘光标隐藏，让系统组合浮层
        // 自带的、随组合进度前进的光标成为唯一可见光标），再定位候选窗。
        WM_IME_STARTCOMPOSITION => {
            let repaint = state_from(hwnd)
                .map(|s| s.handler.set_ime_composing(true))
                .unwrap_or(false);
            if repaint {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            handle_ime_position(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // 合成中：把候选窗重新定位到焦点控件的光标处，再交默认处理。重复定位到
        // 同一点是幂等的；兼顾"候选窗在合成中才出现"的输入法。
        WM_IME_COMPOSITION => {
            handle_ime_position(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // 输入法结束合成（提交上屏或取消）：通知焦点控件退出组合态，恢复自绘光标。
        WM_IME_ENDCOMPOSITION => {
            let repaint = state_from(hwnd)
                .map(|s| s.handler.set_ime_composing(false))
                .unwrap_or(false);
            if repaint {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_CLOSE => {
            // 先询问应用层：对话框关闭 / 未保存拦截等。
            let (allow, repaint) = {
                let Some(state) = state_from(hwnd) else {
                    return LRESULT(0);
                };
                let allow = state.handler.on_close_request();
                // 若取消关闭但对话框已关，需重绘。
                let repaint = !allow;
                (allow, repaint)
            };
            if repaint {
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            if allow {
                let _ = DestroyWindow(hwnd);
            } else {
                // 取消关闭时排一次待处理窗口操作：hide_on_close 正是在 on_close_request
                // 里返回 false 并留下 WindowOp::Hide。不排的话点关闭按钮会既不关也不隐，
                // 看起来像卡死。
                //
                // 两段式：上面的 state 借用已在取出 (allow, repaint) 的块结束时释放。
                apply_window_op(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // 先发退出消息让消息循环立即响应，再释放资源（避免阻塞退出感知）。
            // TrayState::drop 会调 Shell_NotifyIconW(NIM_DELETE)，需在进程退出前执行，
            // 因此不能 leak，仍须显式 drop——但顺序调整后用户感知到的关闭延迟消失。
            PostQuitMessage(0);
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
            if !ptr.is_null() {
                // **先清零指针再 drop**，顺序是承重的：模态循环（`TrackPopupMenu`）
                // 期间窗口可能被销毁，循环结束后 `on_tray_message` 还要再
                // `state_from` 一次。清零在前，那次调用才会拿到 None 而不是解引用
                // 已释放的 WindowState。
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
            }
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
        let _ = SetCursor(Some(cur));
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
            paths.push(PathBuf::from(String::from_utf16_lossy(
                &buf[..got as usize],
            )));
        }
    }
    DragFinish(hdrop);
    if paths.is_empty() {
        return;
    }
    let repaint = {
        let Some(state) = state_from(hwnd) else {
            return;
        };
        let _guard = super::EventDispatchGuard::enter();
        state.handler.on_drop_files(Point::new(pt.x, pt.y), paths)
    };
    if repaint {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    apply_dialog_request(hwnd);
    if state_from(hwnd)
        .map(|s| s.handler.wants_close())
        .unwrap_or(false)
    {
        let _ = DestroyWindow(hwnd);
    }
}

/// 该窗口是否为无边框（自定义标题栏）模式。
unsafe fn is_frameless(hwnd: HWND) -> bool {
    state_from(hwnd).map(|s| s.frameless).unwrap_or(false)
}

/// 无边框窗口非客户区计算：客户区铺满整窗（去系统标题栏/边框）。
/// 最大化时窗口会超出工作区一个边框厚度——按 DPI 内缩客户区，避免内容溢出屏幕/盖任务栏，
/// 但**不重新插入标题栏**（这正是此前最大化露出系统标题栏的根因：当时误调了 DefWindowProc）。
unsafe fn handle_nccalcsize(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    if IsZoomed(hwnd).as_bool() {
        let params = &mut *(lparam.0 as *mut NCCALCSIZE_PARAMS);
        let dpi = GetDpiForWindow(hwnd).max(96);
        let cx = GetSystemMetricsForDpi(SM_CXFRAME, dpi)
            + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
        let cy = GetSystemMetricsForDpi(SM_CYFRAME, dpi)
            + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi);
        params.rgrc[0].left += cx;
        params.rgrc[0].right -= cx;
        params.rgrc[0].top += cy;
        params.rgrc[0].bottom -= cy;
    }
    // 非最大化：rgrc[0] 不动 → 客户区 = 整窗。
    LRESULT(0)
}

/// 无边框窗口缩放边框宽度（逻辑像素）。
///
/// 这一圈会在 `WM_NCHITTEST` 阶段就把指针事件截走，**永远进不到客户区**——任何贴着窗口
/// 边缘绘制的可点元素都会被它吞掉。滚动条正是踩过这个坑的受害者，现由
/// `core::scrollbar::WINDOW_EDGE_INSET`（略大于此值）整体内缩避让；两者须一同调整。
const RESIZE_BORDER_LOGICAL: i32 = 8;

/// 无边框窗口自定义命中：窗口边缘 N px 内返回缩放命中；否则查拖动区
/// （HTCAPTION）或普通客户区（HTCLIENT）。
unsafe fn handle_nchittest(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // 屏幕坐标 → 客户区物理像素。
    let sx = (lparam.0 & 0xffff) as i16 as i32;
    let sy = ((lparam.0 >> 16) & 0xffff) as i16 as i32;
    let mut pt = POINT { x: sx, y: sy };
    let _ = ScreenToClient(hwnd, &mut pt);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let (w, h) = (rc.right, rc.bottom);
    // 交互控件（窗口按钮等）优先判为客户区：使整个按钮都收普通鼠标移动、hover 稳定，
    // 不被顶部缩放条夺走——优先级高于缩放边框与拖动区。
    let interactive = state_from(hwnd)
        .map(|s| s.handler.interactive_at(Point::new(pt.x, pt.y)))
        .unwrap_or(false);
    if interactive {
        return LRESULT(HTCLIENT as isize);
    }
    // 缩放边框宽度（物理像素，按 DPI 放大；逻辑上恒为 RESIZE_BORDER_LOGICAL）。
    let dpi = GetDpiForWindow(hwnd).max(96);
    let m = ((RESIZE_BORDER_LOGICAL as f32 * dpi as f32 / 96.0) as i32).max(4);
    let (left, right) = (pt.x < m, pt.x >= w - m);
    let (top, bottom) = (pt.y < m, pt.y >= h - m);
    let ht: i32 = if top && left {
        HTTOPLEFT as i32
    } else if top && right {
        HTTOPRIGHT as i32
    } else if bottom && left {
        HTBOTTOMLEFT as i32
    } else if bottom && right {
        HTBOTTOMRIGHT as i32
    } else if left {
        HTLEFT as i32
    } else if right {
        HTRIGHT as i32
    } else if top {
        HTTOP as i32
    } else if bottom {
        HTBOTTOM as i32
    } else {
        // 非边缘：问宿主该点是否拖动区。
        let drag = state_from(hwnd)
            .map(|s| s.handler.window_drag_at(Point::new(pt.x, pt.y)))
            .unwrap_or(false);
        if drag {
            HTCAPTION as i32
        } else {
            HTCLIENT as i32
        }
    };
    LRESULT(ht as isize)
}

/// 事件分发后执行待处理的窗口操作（自定义标题栏按钮、`EventCtx::hide_window` 等）。
///
/// 两段式：`state_from` 的借用在取出 op 的那条语句结束时即释放，随后 `run_window_op`
/// 里的 OS 调用才可能重入 `wnd_proc`（铁律 6）。
unsafe fn apply_window_op(hwnd: HWND) {
    let op = state_from(hwnd).and_then(|s| s.handler.take_window_op());
    run_window_op(hwnd, op);
    // 运行期热键操作与窗口操作同点消费（HotkeyHandle 排队 → 此处落地）。
    // Register/UnregisterHotKey 不向本窗口同步派发消息，可在借用内直接执行。
    apply_hotkey_ops(hwnd);
}

/// 消费运行期热键操作队列（改绑/启停），落地到 `HotkeyState`。
/// 先经 handler 取队列（借 handler 字段），再对 hotkeys 字段执行——同一
/// `WindowState` 的两个字段序贯借用，无别名。
unsafe fn apply_hotkey_ops(hwnd: HWND) {
    let Some(state) = state_from(hwnd) else {
        return;
    };
    let ops = state.handler.take_hotkey_ops();
    if ops.is_empty() {
        return;
    }
    if let Some(hk) = state.hotkeys.as_mut() {
        for (id, op) in ops {
            hk.apply(id, op);
        }
    }
}

/// 执行一个窗口操作。**调用方须已释放 `WindowState` 借用**——此处的 OS 调用会同步
/// 重入 `wnd_proc`（`ShowWindow` 派发 WM_SHOWWINDOW、`SetForegroundWindow` 派发
/// WM_ACTIVATE），届时会再次 `state_from`。
///
/// 事件路径（`apply_window_op`）与全局热键路径（`WM_HOTKEY`）共用本函数：op 的来源
/// 不同，执行语义必须一致。
unsafe fn run_window_op(hwnd: HWND, op: Option<WindowOp>) {
    match op {
        Some(WindowOp::Minimize) => {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
        Some(WindowOp::ToggleMaximize) => {
            let cmd = if IsZoomed(hwnd).as_bool() {
                SW_RESTORE
            } else {
                SW_MAXIMIZE
            };
            let _ = ShowWindow(hwnd, cmd);
        }
        Some(WindowOp::Show) => show_and_activate(hwnd),
        Some(WindowOp::Hide) => {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
        None => {}
    }
}

/// 托盘消息处理。**严格分段，每段之间必须释放 `WindowState` 借用**（铁律 6）。
///
/// 托盘是重入风险最高的路径：右键菜单的 `TrackPopupMenu` 自带模态消息循环，菜单
/// 从弹出到用户点选之间的每一次鼠标移动都会重入 `wnd_proc`。
///
/// 分段按「这个 OS 调用会不会重入」切，两条路径互斥（非先后关系）：
/// - 点击路径：「取意图」（持借用）→「执行意图」（无借用）。
/// - 菜单路径：「建菜单」（持借用，不重入）→「弹菜单」（**必须无借用**，模态重入）
///   →「跑选中项」（持借用，只写意图）→「执行意图」（无借用）。
///
/// 动作分类由自由函数 `tray::classify` 完成，不碰 state——右键路径因此全程只在
/// 「建菜单」「跑选中项」两处取借用。
unsafe fn on_tray_message(hwnd: HWND, lparam: LPARAM) {
    match tray::classify(lparam) {
        tray::TrayEvent::Click(kind) => {
            // 取意图：借 state 跑回调；借用随本语句结束而释放。
            let actions = state_from(hwnd)
                .and_then(|s| s.tray.as_mut())
                .map(|ts| tray::run_click(ts, kind))
                .unwrap_or_default();
            // 执行意图：已无借用。
            run_tray_actions(hwnd, actions);
        }
        tray::TrayEvent::RightClick => {
            // 建菜单：借用内完成（CreatePopupMenu/AppendMenuW 均不重入）。
            let Some(menu) = state_from(hwnd)
                .and_then(|s| s.tray.as_ref())
                .and_then(|ts| ts.build_menu())
            else {
                return;
            };
            // 弹菜单：**无借用**。菜单存续期间 wnd_proc 会被反复重入。
            let id = tray::track_menu(hwnd, menu);
            if id == 0 {
                return; // 用户取消
            }
            // 跑选中项：重借取意图，借用随语句释放。
            //
            // 若窗口在弹菜单的模态循环里被销毁，`WM_DESTROY` 已先清零 GWLP_USERDATA
            // 才 drop `WindowState`（见该分支），故此处 `state_from` 返回 None 而非
            // 解引用已释放内存——这个顺序是本分段设计的前提。
            let actions = state_from(hwnd)
                .and_then(|s| s.tray.as_mut())
                .map(|ts| ts.run_item(id))
                .unwrap_or_default();
            // 执行意图：已无借用。
            run_tray_actions(hwnd, actions);
        }
        tray::TrayEvent::Other => {}
    }
}

/// 按声明顺序执行托盘回调的意图队列。**调用方须已释放 `WindowState` 借用**——
/// Show/Hide/Quit 都会同步重入 `wnd_proc`。
///
/// 逐条执行且每条之间不持有借用，故「先 notify 再 show_window」这类组合成立。
unsafe fn run_tray_actions(hwnd: HWND, actions: Vec<tray::TrayAction>) {
    for action in actions {
        match action {
            // 显隐复用窗口操作通道：托盘与热键、事件路径的显隐语义必须一致
            // （例如 Show 需处理「窗口当前是最小化」的情形）。
            tray::TrayAction::Show => run_window_op(hwnd, Some(WindowOp::Show)),
            tray::TrayAction::Hide => run_window_op(hwnd, Some(WindowOp::Hide)),
            // 不走 WindowOp：托盘「退出」是应用的唯一真实出口，**刻意绕过
            // `hide_on_close`**（否则开了关闭转隐藏的应用将永远退不掉）。
            //
            // `break` 丢弃 quit 之后的意图，是刻意的三重收口：窗口已销毁，后续意图
            // 本就无从生效（HWND 失效、`state_from` 取不到 state）；显式截断让这个
            // 事实可读，而非依赖两个不相干的兜底；也堵住「HWND 被系统回收后
            // `state_from` 取到另一个窗口的 state」这一理论缺口。macOS 侧
            // `NSApp::terminate` 本就不返回，两平台由此在构造上一致。
            tray::TrayAction::Quit => {
                let _ = DestroyWindow(hwnd);
                break;
            }
            // 先取出投递目标释放借用，再调 Shell_NotifyIconW（它会跨线程发消息）。
            tray::TrayAction::Notify { title, body } => {
                let target = state_from(hwnd)
                    .and_then(|s| s.tray.as_ref())
                    .map(|ts| ts.notify_target());
                if let Some((h, uid)) = target {
                    tray::notify(h, uid, &title, &body);
                }
            }
        }
    }
}

/// 显示并前置窗口：取消最小化 + 置前。
///
/// `SetForegroundWindow` 受系统前台激活权限限制——后台进程默认无权抢前台，调用会
/// 静默失败（窗口只在任务栏闪烁）。但**全局热键是系统认可的激活来源**：处理
/// `WM_HOTKEY` 期间本线程持有前台激活权，故经热键唤起时此处成立。
/// 托盘点击同理（用户交互授予）。
pub(crate) fn show_and_activate(hwnd: HWND) {
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

/// 事件分发后执行待处理的原生文件对话框请求。此时 OS 鼠标捕获已在
/// `dispatch_pointer_event`/`dispatch_key_event` 里同步完毕，才轮到这个可能长时间
/// 阻塞、自带消息泵的调用——避免对话框存续期间本窗口仍持有 `SetCapture` 与其抢
/// 鼠标输入（见 `DialogRequest` 文档）。
///
/// 两段式：`state_from` 的借用在取出请求的那条语句结束后即释放，`req.run()` 触发的
/// 重入（对话框消息泵期间本窗口 WM_PAINT/WM_TIMER 等会重新进入 wnd_proc）不会与之
/// 产生 `&mut` 别名。
unsafe fn apply_dialog_request(hwnd: HWND) {
    let req = state_from(hwnd).and_then(|s| s.handler.take_dialog_request());
    let Some(req) = req else { return };
    req.run();
    // 延续回调多半间接改了 Signal 状态而不经过脏区系统，保守整窗重绘。
    let _ = InvalidateRect(Some(hwnd), None, false);
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
unsafe fn frame_size_for_client(
    logical_w: i32,
    logical_h: i32,
    scale: f32,
    dpi: u32,
) -> (i32, i32) {
    let cw = (logical_w as f32 * scale).round() as i32;
    let ch = (logical_h as f32 * scale).round() as i32;
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: cw,
        bottom: ch,
    };
    let _ = AdjustWindowRectExForDpi(
        &mut rc,
        WS_OVERLAPPEDWINDOW,
        false,
        WINDOW_EX_STYLE::default(),
        dpi,
    );
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
        state_from(hwnd)
            .map(|s| s.last_click.bump(btn, x, y, now, dbl, dx, dy))
            .unwrap_or(1)
    } else {
        1
    };
    dispatch_pointer_event(
        hwnd,
        PointerEvent {
            kind,
            pos: Point::new(x, y),
            button,
            click_count,
        },
    );
}

/// 向系统申请鼠标离开通知（含非客户区），离开时收到 WM_MOUSELEAVE / WM_NCMOUSELEAVE。
/// 申请是一次性的，系统在投递离开消息后即注销，故离开后需重新申请（由下次 Move 触发）。
unsafe fn track_mouse_leave(hwnd: HWND) {
    let Some(state) = state_from(hwnd) else {
        return;
    };
    if state.mouse_tracked {
        return;
    }
    state.mouse_tracked = true;
    // 只追踪"离开客户区"（→ WM_MOUSELEAVE）。切勿加 TME_NONCLIENT：光标本在客户区时
    // 它会让系统立刻投递 WM_NCMOUSELEAVE，把刚设置的 hover 瞬间清掉（表现为完全没高亮）。
    let mut tme = TRACKMOUSEEVENT {
        cbSize: core::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    let _ = TrackMouseEvent(&mut tme);
}

/// 清除悬停态：派发一个落在所有节点之外的 Move（命中 None → 原 hover 控件收到 Leave）。
/// 用于鼠标离开窗口（WM_MOUSELEAVE / WM_NCMOUSELEAVE）——此时无有意义的位置可用。
unsafe fn clear_hover(hwnd: HWND) {
    dispatch_pointer_event(
        hwnd,
        PointerEvent::single(PointerKind::Move, Point::new(-1, -1), MouseButton::Left),
    );
}

/// 非客户区鼠标移动（WM_NCMOUSEMOVE，lParam 为**屏幕坐标**）：转客户坐标后按真实位置补发 Move。
/// 让 hover 随实际命中走——拖动区会清掉按钮残留高亮，而按钮顶部 HTTOP 缩放条仍命中按钮保留高亮。
unsafe fn handle_nc_mouse_move(hwnd: HWND, lparam: LPARAM) {
    let mut pt = POINT {
        x: (lparam.0 & 0xffff) as i16 as i32,
        y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
    };
    let _ = ScreenToClient(hwnd, &mut pt);
    dispatch_pointer_event(
        hwnd,
        PointerEvent::single(PointerKind::Move, Point::new(pt.x, pt.y), MouseButton::Left),
    );
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
        PointerEvent::single(
            PointerKind::Wheel(delta),
            Point::new(pt.x, pt.y),
            MouseButton::Left,
        ),
    );
}

/// 指针事件分发的公共两段式实现（事件分发 + OS 捕获同步 + 关闭）。
unsafe fn dispatch_pointer_event(hwnd: HWND, ev: PointerEvent) {
    let (repaint, active, was_capturing, close) = {
        let Some(state) = state_from(hwnd) else {
            return;
        };
        // 风险窗口：on_pointer 回调栈内 OS 捕获尚未同步，见 EventDispatchGuard 文档。
        let _guard = super::EventDispatchGuard::enter();
        let repaint = state.handler.on_pointer(ev);
        (
            repaint,
            state.handler.capture_active(),
            state.capturing,
            state.handler.wants_close(),
        )
    };
    if repaint {
        let _ = InvalidateRect(Some(hwnd), None, false);
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
    // 自定义标题栏按钮请求的窗口操作（最小化/最大化）；在可能的关窗之前执行。
    apply_window_op(hwnd);
    // 原生文件对话框请求：此时 OS 捕获已在上面同步完毕，才轮到这个阻塞调用。
    apply_dialog_request(hwnd);
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
    let _ = InvalidateRect(Some(hwnd), None, false);
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
    let mut pt = POINT {
        x: ti.x / 100,
        y: ti.y / 100,
    };
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
                    PointerEvent::single(
                        PointerKind::Down,
                        Point::new(start.0, start.1),
                        MouseButton::Left,
                    ),
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
        let Some(state) = state_from(hwnd) else {
            return;
        };
        state.handler.on_pan(pos, dy)
    };
    if repaint {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 触摸松手：按释放速度启动惯性滑动。启动后触发首帧，其余由动画循环按帧推进。
unsafe fn dispatch_fling(hwnd: HWND, pos: Point, vy: f32) {
    let started = {
        let Some(state) = state_from(hwnd) else {
            return;
        };
        state.handler.start_fling(pos, vy)
    };
    if started {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

/// 打断进行中的惯性滑动（新触摸按下时调用）。
unsafe fn cancel_fling(hwnd: HWND) {
    let repaint = {
        let Some(state) = state_from(hwnd) else {
            return;
        };
        state.handler.cancel_fling()
    };
    if repaint {
        let _ = InvalidateRect(Some(hwnd), None, false);
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
    // 合成串字体：按 caret 物理高度设字高（h 已含 DPI scale），使 IME 内联绘制的
    // 合成串与我们自绘、已缩放的上屏文字大小一致。不设则 IME 用默认未缩放字体，
    // 高 DPI 下合成串明显偏小（上屏后正常）。lfFaceName 显式指定为与正文渲染同族的
    // "Microsoft YaHei UI"（见 text::dwrite::DEFAULT_FAMILY），否则留空时系统常回退到
    // 陈旧的 SimSun/宋体，与我们自绘文字观感不一致。
    let mut lf = LOGFONTW {
        lfHeight: h,
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    for (dst, src) in lf
        .lfFaceName
        .iter_mut()
        .zip("Microsoft YaHei UI".encode_utf16())
    {
        *dst = src;
    }
    let _ = ImmSetCompositionFontW(himc, &lf);
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
        let Some(state) = state_from(hwnd) else {
            return;
        };
        if !state.capturing {
            return;
        }
        state.capturing = false;
        state.handler.on_capture_lost()
    };
    if repaint {
        let _ = InvalidateRect(Some(hwnd), None, false);
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
    let ev = KeyEvent {
        key,
        pressed: true,
        shift,
        ctrl,
    };
    dispatch_key_event(hwnd, ev);
}

/// 把 WM_CHAR 的 UTF-16 码元累积成完整 `char`。
///
/// 补充平面字符（emoji 等，码点 > U+FFFF）由系统分两条 WM_CHAR 发来 UTF-16
/// 代理对：高代理项（`0xD800..=0xDBFF`）先到，暂存于 `pending`；低代理项
/// （`0xDC00..=0xDFFF`）到达后与之合成。BMP 码元直接成 `char`。孤立或非法的
/// 代理序列被丢弃并清空 `pending`，返回 `None`。
fn accumulate_char(pending: &mut Option<u16>, unit: u16) -> Option<char> {
    if (0xD800..=0xDBFF).contains(&unit) {
        *pending = Some(unit); // 高代理项暂存（覆盖任何旧的悬挂高代理项）
        return None;
    }
    if (0xDC00..=0xDFFF).contains(&unit) {
        // 低代理项：须有配对高代理项，否则为孤立项丢弃。
        let hi = pending.take()?;
        let cp = 0x10000 + (((hi as u32 - 0xD800) << 10) | (unit as u32 - 0xDC00));
        return char::from_u32(cp);
    }
    *pending = None; // BMP 码元：清掉任何悬挂高代理项（异常序列）
    char::from_u32(unit as u32)
}

/// WM_CHAR：已翻译的字符（含 IME/CJK 输入与 emoji 代理对）。控制字符跳过。
unsafe fn handle_char(hwnd: HWND, wparam: WPARAM) {
    let unit = wparam.0 as u16;
    // 先在独立借用作用域内累积代理对并释放 state 借用，再分发——避免与
    // dispatch_key_event 内部的 state_from 形成 &mut 别名（见其两段式说明）。
    let c = {
        let Some(state) = state_from(hwnd) else {
            return;
        };
        accumulate_char(&mut state.pending_surrogate, unit)
    };
    let Some(c) = c else { return };
    if c.is_control() {
        return;
    }
    let ev = KeyEvent {
        key: Key::Char(c),
        pressed: true,
        shift: false,
        ctrl: false,
    };
    dispatch_key_event(hwnd, ev);
}

/// 分发键盘事件（两段式：先借 state 取意图，释放后再调可能重入的 DestroyWindow）。
unsafe fn dispatch_key_event(hwnd: HWND, ev: KeyEvent) {
    let (repaint, close) = {
        let Some(state) = state_from(hwnd) else {
            return;
        };
        let _guard = super::EventDispatchGuard::enter();
        (state.handler.on_key(ev), state.handler.wants_close())
    };
    if repaint {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
    apply_window_op(hwnd);
    apply_dialog_request(hwnd);
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
    use super::{accumulate_char, ClickTracker};

    #[test]
    fn bmp_char_passes_through() {
        let mut pend = None;
        assert_eq!(accumulate_char(&mut pend, b'A' as u16), Some('A'));
        assert_eq!(
            accumulate_char(&mut pend, 0x4E16),
            Some('世'),
            "BMP 中文字符"
        );
        assert_eq!(pend, None, "BMP 字符不留挂起状态");
    }

    #[test]
    fn surrogate_pair_combines_to_emoji() {
        // 😀 U+1F600 = UTF-16 代理对 D83D DE00
        let mut pend = None;
        assert_eq!(accumulate_char(&mut pend, 0xD83D), None, "高代理项先暂存");
        assert_eq!(pend, Some(0xD83D));
        assert_eq!(
            accumulate_char(&mut pend, 0xDE00),
            Some('😀'),
            "低代理项合成 emoji"
        );
        assert_eq!(pend, None, "合成后清空挂起");
    }

    #[test]
    fn lone_low_surrogate_is_dropped() {
        let mut pend = None;
        assert_eq!(accumulate_char(&mut pend, 0xDE00), None, "孤立低代理项丢弃");
        assert_eq!(pend, None);
    }

    #[test]
    fn dangling_high_surrogate_recovers_on_bmp() {
        let mut pend = None;
        assert_eq!(accumulate_char(&mut pend, 0xD83D), None);
        // 异常序列：高代理后直接来 BMP —— 丢弃悬挂高代理项，BMP 正常返回。
        assert_eq!(accumulate_char(&mut pend, b'X' as u16), Some('X'));
        assert_eq!(pend, None, "悬挂高代理项被清除");
    }

    #[test]
    fn second_high_surrogate_replaces_first() {
        // 🌈 U+1F308 = D83C DF08
        let mut pend = None;
        assert_eq!(accumulate_char(&mut pend, 0xD83D), None);
        assert_eq!(
            accumulate_char(&mut pend, 0xD83C),
            None,
            "第二个高代理项替换第一个"
        );
        assert_eq!(pend, Some(0xD83C));
        assert_eq!(accumulate_char(&mut pend, 0xDF08), Some('🌈'));
    }

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
        assert_eq!(
            t.bump(1, 10, 10, wrapped, DBL, DX, DY),
            2,
            "跨回绕仍判为双击"
        );
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
