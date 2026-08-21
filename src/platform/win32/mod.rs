//! Win32 窗口、消息循环与 GDI 呈现。
//!
//! 渲染全在 CPU：单份 tiny-skia `Pixmap`（RGBA 预乘）作后备缓冲；呈现时原地
//! R/B 交换为 BGRA 后 `SetDIBitsToDevice` 直接拷屏。空闲时阻塞在 `GetMessageW`，零 CPU。

pub mod clipboard;
#[cfg(feature = "d2d")]
pub(super) mod d2d;
pub mod hotkey;
pub mod tray;

use std::cell::{Cell, RefCell};
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
    VK_NEXT, VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Input::Touch::{
    CloseTouchInputHandle, GetTouchInputInfo, RegisterTouchWindow, HTOUCHINPUT,
    REGISTER_TOUCH_WINDOW_FLAGS, TOUCHEVENTF_DOWN, TOUCHEVENTF_MOVE, TOUCHEVENTF_UP, TOUCHINPUT,
};
use windows::Win32::UI::Shell::{
    DragAcceptFiles, DragFinish, DragQueryFileW, DragQueryPoint, ShellExecuteW, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCaretBlinkTime,
    GetClientRect, GetMessageExtraInfo, GetMessageTime, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowRect, IsIconic, IsWindow, IsWindowVisible, IsZoomed, LoadCursorW,
    LoadIconW, MsgWaitForMultipleObjectsEx, PeekMessageW, PostMessageW, PostQuitMessage,
    RegisterClassExW, SetCursor, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, SystemParametersInfoW, TranslateMessage, CREATESTRUCTW, CW_USEDEFAULT,
    GWLP_USERDATA, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLIENT, HTLEFT, HTRIGHT,
    HTTOP, HTTOPLEFT, HTTOPRIGHT, HWND_MESSAGE, IDC_ARROW, IDC_HAND, IDC_IBEAM, MINMAXINFO, MSG,
    MWMO_INPUTAVAILABLE, NCCALCSIZE_PARAMS, PM_REMOVE, QS_ALLINPUT, SIZE_MINIMIZED, SM_CXDOUBLECLK,
    SM_CXFRAME, SM_CXPADDEDBORDER, SM_CXSCREEN, SM_CYDOUBLECLK, SM_CYFRAME, SM_CYSCREEN,
    SPI_GETCLIENTAREAANIMATION, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_HIDE, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WA_INACTIVE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE,
    WM_APP, WM_CAPTURECHANGED, WM_CHAR, WM_CLOSE, WM_DESTROY, WM_DPICHANGED, WM_DROPFILES,
    WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_GETMINMAXINFO, WM_HOTKEY, WM_IME_COMPOSITION,
    WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCALCSIZE, WM_NCCREATE, WM_NCHITTEST, WM_NCMOUSEMOVE,
    WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZE, WM_TIMER, WM_TOUCH,
    WNDCLASSEXW, WS_MAXIMIZEBOX, WS_OVERLAPPEDWINDOW, WS_THICKFRAME,
};
// 只用于 d2d 后端选择（RDP 远程会话下强制软渲染），随该 feature 一起门控。
#[cfg(feature = "d2d")]
use windows::Win32::UI::WindowsAndMessaging::SM_REMOTESESSION;

use super::{to_skia_color, AppHandler, NewWindow, Renderer, WindowConfig};
use crate::event::{CursorShape, Key, KeyEvent, MouseButton, PointerEvent, PointerKind, WindowOp};
use crate::geometry::{Color, Point, Rect, Size};

thread_local! {
    /// wnd_proc 入口处写入当前 HWND；PickDialog::pick_* 读取以注入父窗口。
    static ACTIVE_HWND: Cell<isize> = const { Cell::new(0) };
    /// 本线程上仍存活的 windui 窗口，见 [`LiveWindows`]。
    static LIVE_WINDOWS: RefCell<LiveWindows> = const { RefCell::new(LiveWindows::new()) };
}

/// 供 platform::inject_parent 读取当前活跃窗口句柄（单线程，消息循环内保证有效）。
pub(super) fn active_hwnd() -> isize {
    ACTIVE_HWND.with(|h| h.get())
}

/// 本线程上仍存活的 windui 窗口登记表：消息循环据此决定驱动谁的帧、以及何时该退出。
///
/// 存 `isize` 而非 `HWND` 有两个理由：`HWND` 不是 `Send` 也进不了 `const` 初始化，
/// 更要紧的是登记表的增删与退出判定因此**不依赖真实窗口**，可以直接单测——这套逻辑
/// 的错法（少注销一个就永不退出、多注销一个就提前杀掉整个应用）都不是编译期能拦下的。
struct LiveWindows {
    wins: Vec<LiveWindow>,
}

/// 登记表里的一条。
struct LiveWindow {
    id: isize,
    /// 单例键（`Window::single`）。`None` = 普通窗口，永不参与去重。
    single: Option<String>,
}

impl LiveWindows {
    const fn new() -> Self {
        Self { wins: Vec::new() }
    }

    /// 登记一个新窗口。重复登记同一句柄是调用方的错，debug 下拦下。
    fn add(&mut self, id: isize, single: Option<String>) {
        debug_assert!(
            !self.wins.iter().any(|w| w.id == id),
            "窗口重复登记: {id:#x}"
        );
        self.wins.push(LiveWindow { id, single });
    }

    /// 注销一个窗口，返回**注销后登记表是否已空**（即这是不是最后一个窗口）。
    ///
    /// 注销一个从未登记过的句柄返回 `false`：那说明有条销毁路径没走过登记，此时若按
    /// "空了"处理就会在还有窗口活着时退出整个应用。debug 下拦下以便定位。
    fn remove(&mut self, id: isize) -> bool {
        let before = self.wins.len();
        self.wins.retain(|w| w.id != id);
        debug_assert!(before != self.wins.len(), "注销未登记的窗口: {id:#x}");
        before != self.wins.len() && self.wins.is_empty()
    }

    fn ids(&self) -> Vec<isize> {
        self.wins.iter().map(|w| w.id).collect()
    }

    /// 找出带指定单例键的窗口（`Window::single`）。
    ///
    /// 键随窗口注销一并消失，故找到的那个必然还活着——单例判定不需要额外的存活校验，
    /// 也不会因为某条关闭路径绕过了应用层而留下一个"占着键的鬼窗口"。
    fn find_single(&self, key: &str) -> Option<isize> {
        self.wins
            .iter()
            .find(|w| w.single.as_deref() == Some(key))
            .map(|w| w.id)
    }
}

/// 登记一个已创建的窗口。
unsafe fn register_window(hwnd: HWND, single: Option<String>) {
    LIVE_WINDOWS.with(|w| w.borrow_mut().add(hwnd.0 as isize, single));
}

/// 查找带指定单例键的既有窗口。
unsafe fn find_single_window(key: &str) -> Option<HWND> {
    LIVE_WINDOWS
        .with(|w| w.borrow().find_single(key))
        .map(|v| HWND(v as *mut _))
}

/// 注销一个已销毁的窗口，返回它是否是最后一个（调用方据此 `PostQuitMessage`）。
unsafe fn unregister_window(hwnd: HWND) -> bool {
    LIVE_WINDOWS.with(|w| w.borrow_mut().remove(hwnd.0 as isize))
}

/// 当前仍存活的窗口句柄快照。
///
/// 返回**拷贝**而非借用：调用方拿着它去 `InvalidateRect`/`UpdateWindow`，那会同步派发
/// `WM_PAINT` 回到 `wnd_proc`，窗口可能就此被销毁并回头注销自己。持着 `RefCell` 的借用
/// 走进这一步就是运行期 panic。
unsafe fn live_windows() -> Vec<HWND> {
    LIVE_WINDOWS.with(|w| {
        w.borrow()
            .ids()
            .into_iter()
            .map(|v| HWND(v as *mut _))
            .collect()
    })
}

// ── App 级消息宿主 ──────────────────────────────────────────────────────────

/// 托盘、全局热键、跨线程唤醒的宿主状态。指针挂在 message-only 窗口的
/// `GWLP_USERDATA` 上（同 [`WindowState`] 的形状）。
///
/// 这三样都是**应用**的资源而非某个窗口的：托盘图标代表整个程序、全局热键在所有窗口
/// 之外生效、后台线程唤醒的是"这个应用"。此前它们挂在唯一那个窗口上，于是"窗口"与
/// "应用"两个生命周期被迫重合——多窗口下就变成了「关掉哪个窗口托盘图标才消失」这种
/// 没有正确答案的问题。移到独立的 message-only 窗口后，它们活到消息循环结束为止。
struct AppHost {
    /// 系统托盘状态（None=无托盘）。drop 时自动清理图标。
    tray: Option<tray::TrayState>,
    /// 全局热键状态（None=无热键）。drop 时自动注销。
    hotkeys: Option<hotkey::HotkeyState>,
    /// 托盘点击、热键触发所指向的窗口——主窗口，即 `App::run` 建的那个。
    ///
    /// 「唤出窗口」「最小化到托盘」说的都是它；子窗（设置页之类）不是这些操作的对象。
    main: HWND,
    /// 主窗当初选定的渲染后端。子窗沿用它——应用层构造子窗配置时不知道这件事，
    /// 而"主窗跑 GPU、子窗悄悄退回软件"是没人想要的结果。
    renderer: Renderer,
}

const APP_HOST_CLASS: PCWSTR = w!("WindUiAppHostClass");

thread_local! {
    /// App 级消息宿主窗口句柄（0=未创建）。
    static APP_HOST_HWND: Cell<isize> = const { Cell::new(0) };
}

/// App 级消息宿主的窗口句柄。托盘/热键注册与跨线程唤醒都投向它。
fn app_host_hwnd() -> HWND {
    HWND(APP_HOST_HWND.with(|h| h.get()) as *mut _)
}

/// 取 App 级宿主状态的可变引用。
///
/// 与 [`state_from`] 同样的重入约束：返回的借用必须在任何可能重入 `wnd_proc` 的 OS
/// 调用之前结束（铁律 6）。
unsafe fn app_host() -> Option<&'static mut AppHost> {
    let hwnd = app_host_hwnd();
    if hwnd.0.is_null() {
        return None;
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppHost;
    if ptr.is_null() {
        None
    } else {
        Some(&mut *ptr)
    }
}

/// 建 App 级消息宿主：一个 message-only 窗口，承载托盘/热键/唤醒。
///
/// 它**不进** [`LiveWindows`]：那张表回答的是"还有没有可见窗口"，而本窗口从不显示，
/// 把它算进去应用就永远退不掉了。
unsafe fn create_app_host(hinst: HINSTANCE, main: HWND, renderer: Renderer) {
    let wc = WNDCLASSEXW {
        cbSize: size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(app_host_proc),
        hInstance: hinst,
        lpszClassName: APP_HOST_CLASS,
        ..Default::default()
    };
    RegisterClassExW(&wc);
    let host = Box::new(AppHost {
        tray: None,
        hotkeys: None,
        main,
        renderer,
    });
    let host_ptr = Box::into_raw(host);
    match CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        APP_HOST_CLASS,
        PCWSTR::null(),
        WINDOW_STYLE::default(),
        0,
        0,
        0,
        0,
        Some(HWND_MESSAGE),
        None,
        Some(hinst),
        Some(host_ptr as *const c_void),
    ) {
        Ok(h) => APP_HOST_HWND.with(|c| c.set(h.0 as isize)),
        Err(e) => {
            // 建不起来就回收状态：没有宿主窗口，托盘与热键随后都不会安装（见
            // `run_windowed`），应用退化成"只有窗口"仍可正常跑。
            drop(Box::from_raw(host_ptr));
            eprintln!("[windui] App 级消息宿主创建失败，托盘/全局热键将不可用: {e:?}");
        }
    }
}

/// 销毁 App 级消息宿主（消息循环退出后调用），触发托盘图标清理与热键注销。
unsafe fn destroy_app_host() {
    let hwnd = app_host_hwnd();
    if !hwnd.0.is_null() {
        let _ = DestroyWindow(hwnd);
        APP_HOST_HWND.with(|c| c.set(0));
    }
}

/// App 级消息宿主的窗口过程：托盘回调、全局热键、跨线程唤醒。
///
/// 都不碰 `handler`——托盘与热键的回调只需各自的 `TrayState`/`HotkeyState`，产出的是
/// [`WindowOp`] 意图，由 `main` 窗口执行；唤醒则只是让各窗口出一帧。
unsafe extern "system" fn app_host_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let cs = lparam.0 as *const CREATESTRUCTW;
            if !cs.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // 跨线程唤醒：让**所有**窗口出一帧。
        //
        // 广播而非只叫主窗口：`App::channel` 的 pump 挂在各自宿主上，消息落到哪个窗口
        // 的状态里，只有那个宿主自己知道。多叫醒几个窗口的代价是几次被脏区挡掉的重绘，
        // 漏叫一个的代价是那条通道的数据永远不上屏。
        WM_APP_WAKE => {
            for h in live_windows() {
                let _ = InvalidateRect(Some(h), None, false);
            }
            LRESULT(0)
        }
        // 全局热键：系统投递到本窗口队列（事件驱动，不轮询，故不破坏空闲零 CPU）。
        //
        // 严格两段式（铁律 6）：第一段借 host 跑回调、取出意图；借用在语句结束时释放。
        // 第二段才碰 OS——`ShowWindow`/`SetForegroundWindow` 会同步派发 WM_SHOWWINDOW /
        // WM_ACTIVATE 到主窗口的 wnd_proc，那里会再借一次它自己的 state。
        WM_HOTKEY => {
            let (op, main) = match app_host() {
                Some(h) => (
                    h.hotkeys.as_mut().and_then(|hs| hs.dispatch(wparam.0)),
                    h.main,
                ),
                None => (None, HWND(std::ptr::null_mut())),
            };
            if !main.0.is_null() {
                run_window_op(main, op);
            }
            LRESULT(0)
        }
        tray::WM_TRAYICON => {
            on_tray_message(lparam);
            LRESULT(0)
        }
        WM_DESTROY => {
            let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppHost;
            if !ptr.is_null() {
                // 先清零再 drop，同 WindowState：托盘菜单的模态循环里本窗口若被销毁，
                // 循环结束后仍会回到这里 `app_host()`，清零在前那次才拿到 None。
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                drop(Box::from_raw(ptr));
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
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

/// 查询系统**插入符**闪烁半周期（ms）。`None` = 用户关掉了闪烁。
///
/// 与 [`os_animations_enabled`] 是两码事：那个是客户区动画（窗口/控件过渡效果），
/// 关掉它系统自带输入框的插入符照样闪。把光标闪烁挂到那个开关上，用户一开
/// 「最佳性能」我们的光标就成了死杠。
unsafe fn os_caret_blink_half_ms() -> Option<u32> {
    match GetCaretBlinkTime() {
        // INFINITE：用户在辅助功能里关掉了插入符闪烁。
        u32::MAX => None,
        // 0 表示查询失败（MSDN），回退默认值而不是当成"不闪"。
        0 => Some(crate::ui::caret::BLINK_HALF_MS as u32),
        ms => Some(ms),
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
    // 光标闪烁走**插入符**设置，不跟客户区动画走（见 `os_caret_blink_half_ms`）。
    // 应用显式 `animations(false)` 时连它一起关：那是"我要一个完全静止的界面"。
    crate::ui::caret::set_blink_period_ms(match cfg.animations {
        Some(false) => None,
        _ => unsafe { os_caret_blink_half_ms() },
    });
    if let Some(path) = cfg.screenshot.clone() {
        // 离屏渲染走平台无关的共享实现（与 macOS 后端共用）。
        super::run_offscreen(&cfg, &mut handler, &path);
        return;
    }
    // 单实例仲裁（应用若已在 main 里 claim_instance 过，这里直接放行）：二次实例把 argv
    // 转发给首实例后直接返回、不建窗口。
    if let Some(si) = &single {
        if !crate::single_instance::arbitrate(&si.app_id) {
            return;
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
    /// 缓冲刚（重）建，内容尚未画满：这一帧必须整窗上传，不能只送脏区。
    fresh: bool,
}

impl SkiaBackend {
    fn new() -> Self {
        Self {
            pixmap: None,
            fresh: true,
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
        // 新缓冲全 0：在宿主重新画满之前，任何"只上传脏区"都会把没画过的黑底送上屏。
        self.fresh = true;
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
        // 清底交给宿主的全窗路径：局部帧不需要清，而那次 fill 是整窗的。只有**新建**的
        // 缓冲要在这里清一次——它的内容是透明黑，而宿主未必立刻走全窗帧（对照 macOS）。
        if self.fresh {
            pixmap.fill(to_skia_color(bg));
        }
        // target 借用 self.pixmap，限定在块内：块结束借用即释放，再重取引用做后续处理。
        {
            let mut tgt = crate::render::PixmapTarget { pixmap };
            handler.render(&mut tgt, size);
        }
        // 本帧宿主实际重画的**矩形**（None = 整窗）。缓冲刚重建时内容还不完整，按整窗处理。
        //
        // 必须留住矩形而不只是行范围：R/B 交换要按矩形做，按整行会把脏区左右两侧
        // 已是 BGRA 的像素翻第二次（见 `swap_rb_rect`）。
        let drawn = match (self.fresh, handler.last_frame_damage()) {
            (false, Some(d)) => Some(d.intersect(&Rect::new(0, 0, self.buf_w, self.buf_h))),
            _ => None,
        };
        self.fresh = false;
        // 整窗帧时先把整个客户区标失效：`BeginPaint` 返回的 DC 带着 rcPaint 的裁剪，
        // 若失效区只是上一帧那一小块，整窗内容会被裁掉、只更新一条。
        if drawn.is_none() {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        // 上传范围 = 本帧重画的行 ∪ **系统的失效区**。后者不能少：窗口被别的窗口盖住
        // 再暴露时，系统只发一条 WM_PAINT 就认为我们会重传那块；只上传自己的脏区，
        // 暴露出来的部分就永远停在垃圾内容上。pixmap 是持久缓冲，那些行的内容是对的，
        // 直接传即可。
        let (py0, py1) = (
            ps.rcPaint.top.clamp(0, self.buf_h),
            ps.rcPaint.bottom.clamp(0, self.buf_h),
        );
        let (dy0, dy1) = drawn.map_or((0, self.buf_h), |d| (d.y, d.bottom()));
        let (y0, y1) = if py1 > py0 {
            (dy0.min(py0), dy1.max(py1))
        } else {
            (dy0, dy1)
        };
        if y0 >= y1 {
            let _ = EndPaint(hwnd, &ps);
            return false;
        }
        let pixmap = self.pixmap.as_mut().unwrap();
        // RGBA 预乘 → BGRA（GDI 32bpp 字节序）原地交换 R/B。**只翻本帧重画过的那个矩形**：
        // 其余像素早已是上一帧交换过的 BGRA，再翻一次就成了红蓝颠倒。按整行翻是不够的
        // ——脏区左右两侧同样是"已翻过"的（见 `swap_rb_rect`）。
        //
        // 交换之后整张缓冲恒为 BGRA，故下面按行上传 union 范围是安全的。
        let stride = (self.buf_w * 4) as usize;
        match drawn {
            Some(d) => swap_rb_rect(pixmap.data_mut(), self.buf_w, d),
            None => swap_rb_inplace(
                &mut pixmap.data_mut()[dy0 as usize * stride..dy1 as usize * stride],
            ),
        }
        let bits = pixmap.data()[y0 as usize * stride..].as_ptr() as *const c_void;

        // top-down DIB 描述：直接从缓冲拷到设备，无需独立 DIB section。
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: self.buf_w,
                // 负数 = top-down，与 pixmap 行序一致。高度按**本次上传的行数**：
                // bits 已指向首行，DIB 只描述这一段。
                biHeight: -(y1 - y0),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // 只上传这些行：`bits` 已指向首行，DIB 只描述这一段（行连续）。
        let scanlines = SetDIBitsToDevice(
            hdc,
            0,
            y0,
            self.buf_w as u32,
            (y1 - y0) as u32,
            0,
            0,
            0,
            (y1 - y0) as u32,
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
        // 每帧问宿主要底色：运行期换主题时 `self.bg`（创建时抄的那份）不会跟着变。
        let bg = self.handler.bg().unwrap_or(self.bg);
        let downgrade = self.backend.paint(hwnd, bg, self.handler.as_mut());
        if downgrade {
            // 替换为软后端并请求重绘：下一帧用 Skia 呈现，进程不崩、内容继续渲染。
            self.backend = Box::new(SkiaBackend::new());
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

/// 原地把 RGBA 缓冲逐像素交换 R/B（→ BGRA），供 GDI 直接呈现。
/// 只对缓冲里的一个**矩形**做 R/B 交换（按行切片，逐行只翻 `[x, x+w)` 那一段）。
///
/// 局部帧只把脏**矩形**重画成 RGBA，其余像素仍是上一帧交换过的 BGRA。若按整行翻，
/// 脏区左右两侧那些已是 BGRA 的像素会被翻第二次 → 红蓝颠倒。灰/白/黑处 R≈G≈B 看不出来，
/// 只有饱和色显形，故这类错误极易漏网——务必按矩形翻。
fn swap_rb_rect(data: &mut [u8], buf_w: i32, r: Rect) {
    let stride = buf_w as usize * 4;
    for y in r.y..r.bottom() {
        let row = y as usize * stride;
        let (a, b) = (row + r.x as usize * 4, row + r.right() as usize * 4);
        swap_rb_inplace(&mut data[a..b]);
    }
}

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

/// 注册窗口类。重复调用无害（同名类第二次返回 0），主窗与子窗共用同一个类。
unsafe fn register_window_class(hinst: HINSTANCE) {
    let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

    // 打包工具（wind-packer/editpe）注入正式图标时按惯例名 "MAINICON" 写组图标，不会覆盖
    // 数字序号 1（那是 .rc 里 `1 ICON "..."` 编译期烙入的占位图）。优先按名字取，命中的才是
    // 打包后的真实图标；开发态直接 `cargo run`（未过打包工具）时按名字取不到，
    // 回退到数字序号 1 取编译期占位图，行为不变。
    // MAKEINTRESOURCE(1)：整数 1 当资源序号传入（低 64K 表示序号而非字符串指针）。
    // 这不是“悬垂指针”，故抑制 clippy；其自动建议 ptr::dangling() 会把序号改成 u16 对齐值(2)，是语义错误。
    #[allow(clippy::manual_dangling_ptr)]
    let hicon = LoadIconW(Some(hinst), w!("MAINICON"))
        .or_else(|_| LoadIconW(Some(hinst), PCWSTR(1usize as *const u16)))
        .unwrap_or_default();
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
    RegisterClassExW(&wc);
}

/// 建一个窗口并挂上宿主：主窗（`run_windowed`）与子窗（`ctx.open_window`）共用。
///
/// **不含任何应用级设施**（托盘/热键/唤醒/单实例）——那些在 `run_windowed` 里一次性
/// 装到 `AppHost` 上，见该结构的文档。也**不调 `ShowWindow`**：主窗要照顾
/// `start_hidden`、子窗建好即显，由调用方决定。
///
/// `renderer` 单独传而不读 `cfg.renderer`：子窗的配置由应用层构造，那里不知道主窗当初
/// 选了哪个后端，而"主窗跑 GPU、子窗悄悄退回软件"是没人想要的结果。
///
/// 失败返回 `None`（已回收 `WindowState`）。主窗失败是致命的，子窗失败只是少一个窗口。
unsafe fn create_window(
    hinst: HINSTANCE,
    cfg: &WindowConfig,
    handler: Box<dyn AppHandler>,
    // 只在 `d2d` feature 下用于选后端。签名对两档保持一致（调用方不必分 feature 分支），
    // 故仅在关掉那一档时抑制未使用告警——CI 的「Clippy（关闭默认 feature）」正查这个。
    #[cfg_attr(not(feature = "d2d"), allow(unused_variables))] renderer: Renderer,
) -> Option<HWND> {
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
            eprintln!("[windui] CreateWindowExW 失败: {e:?}");
            return None;
        }
    };

    // 登记进活动窗口表：消息循环据此驱动帧，`WM_DESTROY` 据此判断是不是最后一个窗口。
    // 放在 CreateWindowExW **返回之后**而非 WM_NCCREATE 里：创建期间的消息还够不到消息
    // 循环，而创建失败的那条路径上根本没有窗口需要注销。
    register_window(hwnd, cfg.single.clone());

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

    // GPU 后端选择：`cfg.renderer` 想要 GPU（或调试环境变量 WINDUI_D2D=1 强制）时，
    // 尝试用 Direct2D 后端替换软后端。try_create 需要已就绪的 HWND 与客户区尺寸，
    // 故在窗口创建并完成尺寸校正后切换。离屏截图走 run_offscreen，根本不到此处。
    //
    // 两处失败对 `Renderer::Auto` 都退软后端（绝不 panic）、对 `Renderer::Gpu` 都终止：
    //   RDP 远程会话  —— flip-model swapchain 在远程桌面不可用，物理上给不了 GPU；
    //   设备创建失败  —— 无可用适配器。
    // Gpu 之所以终止而非回退，是因为它的用途就是"拿不到 GPU 要告诉我"；静默换一条路
    // 会让基于它做的验证失去意义。
    #[cfg(feature = "d2d")]
    {
        let env_force = std::env::var("WINDUI_D2D").is_ok_and(|v| v != "0" && !v.is_empty());
        let is_remote = GetSystemMetrics(SM_REMOTESESSION) != 0;
        let want = renderer.wants_gpu() || env_force;
        assert!(
            !(renderer.requires_gpu() && is_remote),
            "Renderer::Gpu 要求 GPU 渲染，但当前是 RDP 远程会话——flip-model swapchain \
             在远程桌面不可用。需要自动回退请改用 Renderer::Auto"
        );
        if want && !is_remote {
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let (cw, ch) = (rc.right - rc.left, rc.bottom - rc.top);
            match d2d::try_create(hwnd, cw, ch) {
                Some(b) => {
                    if let Some(s) = state_from(hwnd) {
                        s.backend = Box::new(b);
                    }
                }
                None => {
                    assert!(
                        !renderer.requires_gpu(),
                        "Renderer::Gpu 要求 GPU 渲染，但 D2D 设备创建失败。\
                         需要自动回退请改用 Renderer::Auto"
                    );
                    eprintln!("[windui] D2D 设备创建失败，回退软渲染（Skia）");
                }
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

    Some(hwnd)
}

/// 显示窗口。与创建分开：主窗要照顾 `start_hidden`，子窗建好即显。
unsafe fn show_window(hwnd: HWND) {
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = UpdateWindow(hwnd);
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
    register_window_class(hinst);

    let hwnd = create_window(hinst, &cfg, handler, cfg.renderer).expect("主窗口创建失败");

    // App 级消息宿主：托盘、全局热键、跨线程唤醒的落点。它们的生命周期属于应用而非
    // 某个窗口，故都挂到这个 message-only 窗口上（见 `AppHost`）。
    create_app_host(hinst, hwnd, cfg.renderer);
    let host_hwnd = app_host_hwnd();

    // 全局热键（若配置）：注册到 App 级宿主，状态存入 AppHost（drop 时自动注销）。
    // 注册失败不阻止启动——热键是全局独占资源，被占用是常态而非异常。
    if !cfg.hotkeys.is_empty() {
        let hs = hotkey::HotkeyState::register(host_hwnd, std::mem::take(&mut cfg.hotkeys));
        if let Some(h) = app_host() {
            h.hotkeys = Some(hs);
        }
    }

    // 系统托盘图标（若配置）：回调消息发往 App 级宿主，状态存入 AppHost（drop 时清理）。
    if let Some(t) = cfg.tray.take() {
        if let Some(ts) = tray::install(host_hwnd, t) {
            if let Some(h) = app_host() {
                h.tray = Some(ts);
            }
        }
    }

    // 跨线程唤醒：投向 App 级宿主而非某个窗口——绑在窗口上的话，那个窗口一关，后台
    // 线程的唤醒就静默丢失，通道数据再也不上屏。此前积压的 wake 会在绑定时立即补发。
    if let Some(w) = &waker {
        w.bind(Box::new(Win32Wake {
            hwnd: host_hwnd.0 as isize,
        }));
    }

    // 单实例首实例：建 message-only 窗口接收二次实例 argv（UI 线程切页 + 激活主窗口）。
    if let Some(si) = single {
        crate::single_instance::install_listener(&si.app_id, hwnd.0 as isize, si.on_second);
    }

    // 启动即隐藏：常驻托盘类应用不该在启动时闪一下窗口。此处**不调用 ShowWindow**，
    // 窗口保持初始的不可见态，等托盘点击或全局热键送来 WindowOp::Show。
    if !cfg.start_hidden {
        show_window(hwnd);
    }

    run_message_loop();

    // 销毁 App 级宿主：触发托盘图标 NIM_DELETE 与全局热键注销。放在消息循环之后——
    // 托盘与热键要活过所有窗口（常驻托盘类应用正是"窗口都关了仍在跑"）。
    destroy_app_host();

    // 消息循环结束后立即显式释放 GPU 共享设备链（D3D11/DXGI/D2D/DWrite COM 对象）。
    // 推迟到线程析构才 Release 会触发 GPU 命令队列排空 + DWrite 字体缓存全局清理，
    // 实测延迟可达 3–4 秒；此处提前释放可规避该问题。
    #[cfg(feature = "d2d")]
    d2d::release_shared_device();
}

/// 帧截止的上限（ms）：控件自报的"下次才需要"再长也不超过这个。
///
/// 兜底，不是配速。控件报得过长（或算错）时，界面顶多迟钝 5 秒而不是永远冻住；
/// 代价是最坏情况下每 5 秒一次空唤醒，可以忽略。光标那点周期（≤1.1s）够不着它。
const MAX_FRAME_DELAY_MS: u128 = 5_000;
/// 帧截止小于等于这个值才提升系统定时器分辨率（见 `TimerResolution`）。
/// 约两个默认 tick（15.6ms）：超过这个尺度，等待被 tick 向上取整的误差已不影响观感。
const HIRES_MAX_MS: u128 = 32;

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

/// 消息循环：无动画时阻塞至下一条消息（零 CPU）；有动画时按**帧截止时间**配速——
/// 唤醒后只要距上帧到了截止就重绘一帧，故连续输入下不会超刷新率空转，拖动时也不会
/// 饿死动画。最小化/隐藏时不参与配速，避免空转。
///
/// 截止 = `max(刷新率间隔, 各窗口自报的下次变化时刻)`。前者是上界，后者是下界：控件说
/// "我 530ms 后才变"（方波光标）就真的睡到那时，而不是按刷新率把同一幅画面重画 31 遍。
/// 见 [`AppHandler::next_frame_delay_ms`](crate::platform::AppHandler::next_frame_delay_ms)。
///
/// 已知限制：OS 驱动的模态循环（窗口拖拽/缩放、系统菜单跟踪）期间本循环不执行，
/// 动画会暂停至用户释放——单窗口小工具可接受；如需模态期间也动画，需补 WM_TIMER 兜底。
unsafe fn run_message_loop() {
    let mut msg = MSG::default();
    let mut last_frame = std::time::Instant::now();
    // 仅动画期间持有（提升定时器分辨率），空闲时 None 由 Drop 归还，省电。
    let mut hires: Option<TimerResolution> = None;
    // 动画帧间隔（ms），按当前窗口集合里最快的那块屏采样。缓存到窗口集合变化为止：
    // 每轮都采样要对每个窗口 GetDC + GetDeviceCaps，那是白付的开销。
    let mut frame_ms = frame_interval_ms(&live_windows());
    let mut live_count = LIVE_WINDOWS.with(|w| w.borrow().ids().len());
    loop {
        // 每轮取一次快照：上一轮的 UpdateWindow 可能已经销毁了某个窗口。
        let windows = live_windows();
        if windows.len() != live_count {
            live_count = windows.len();
            frame_ms = frame_interval_ms(&windows);
        }
        // 需要按帧驱动的窗口：最小化**或隐藏**的跳过（画了也看不见，只是空转）。
        //
        // 隐藏这一条尤其要紧：托盘常驻应用「关窗即隐藏」后进程还活着，窗口里若留着一个
        // 聚焦的输入框，光标闪烁会一直请求续帧——于是不可见的窗口按刷新率白烧 CPU，
        // 用户只看得到风扇转。可见性必须在这里问，宿主自己不知道自己是不是被 order 走了。
        let pending: Vec<HWND> = windows
            .into_iter()
            .filter(|&h| {
                IsWindowVisible(h).as_bool()
                    && !IsIconic(h).as_bool()
                    && state_from(h)
                        .map(|s| s.handler.wants_animation())
                        .unwrap_or(false)
            })
            .collect();
        let animating = !pending.is_empty();
        if animating {
            // 本轮的帧截止：不早于刷新率间隔（上界，不超 60/120fps），也不早于所有窗口
            // 自报的"下次画面变化"时刻（下界，见 `AppHandler::next_frame_delay_ms`）。
            //
            // 取各窗口的**最小值**：只要还有一个窗口在连续动画，整条循环就回到满帧配速。
            // 这条循环是共享的，按最慢的那个配速会让别的窗口掉帧。
            let ask = pending
                .iter()
                .filter_map(|&h| state_from(h).map(|s| s.handler.next_frame_delay_ms() as u128))
                .min()
                .unwrap_or(0);
            let due = frame_ms.max(ask.min(MAX_FRAME_DELAY_MS));
            // 提升定时器分辨率到 1ms：否则 MsgWait 超时被默认 ~15.6ms tick 向上取整，
            // 16ms 等待常变成 ~31ms → 实测掉到 ~30fps。
            //
            // 只在真按帧配速时才提：1ms 分辨率是**进程级**的开销（拖低整机空闲功耗），
            // 而截止在几百毫秒开外时（方波光标半周期才翻一次面）多等半个 tick 没人看得出。
            if due <= HIRES_MAX_MS {
                if hires.is_none() {
                    hires = Some(TimerResolution::raise());
                }
            } else {
                hires = None;
            }
            // 等待输入，至多到下一帧截止；零句柄，仅作可被输入中断的定时等待。
            let elapsed = last_frame.elapsed().as_millis();
            let wait = due.saturating_sub(elapsed).min(u32::MAX as u128) as u32;
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
            //
            // 门槛用 `due` 而非 `frame_ms`：被别的消息（鼠标移动、定时器）提前唤醒时，
            // 若还没到画面该变的那一刻就不该白画一帧——那正是"拖着鼠标经过窗口时
            // 光标窗口跟着满帧空转"的来源。
            if last_frame.elapsed().as_millis() >= due {
                for h in pending {
                    // 上一轮 PeekMessage 排空期间该窗口可能已被销毁（点了自己的关闭
                    // 按钮），`pending` 是那之前的快照，故逐个复核仍然有效。
                    if !IsWindow(Some(h)).as_bool() {
                        continue;
                    }
                    // 只失效宿主预计要重画的那块：`BeginPaint` 的 rcPaint 随之收窄，
                    // 上传也就只需那几行（见 `SkiaBackend::paint`）。宿主临时升级整窗时
                    // 由它自己在 paint 里补一次整窗失效，不会漏画。
                    let dmg = state_from(h).and_then(|s| s.handler.pending_damage());
                    match dmg {
                        Some(r) => {
                            let rc = RECT {
                                left: r.x,
                                top: r.y,
                                right: r.right(),
                                bottom: r.bottom(),
                            };
                            let _ = InvalidateRect(Some(h), Some(&rc), false);
                        }
                        None => {
                            let _ = InvalidateRect(Some(h), None, false);
                        }
                    }
                    let _ = UpdateWindow(h);
                }
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

/// 动画帧间隔（ms）：取各窗口所在显示器中**最短**的那个间隔（最高刷新率）。
///
/// 取最短而非平均或最长：这是一条共享的循环，间隔就是所有窗口的公共上限。按慢屏配速会
/// 让快屏上的窗口掉帧，而按快屏配速只是让慢屏窗口多收几次 `InvalidateRect`——那些多余的
/// 帧被脏区系统挡掉，代价远小于掉帧。窗口集合为空时返回默认 60fps 的间隔。
unsafe fn frame_interval_ms(windows: &[HWND]) -> u128 {
    windows
        .iter()
        .map(|&h| window_frame_interval_ms(h))
        .min()
        .unwrap_or(1000 / 60)
}

/// 单个窗口的动画帧间隔（ms）= 1000 / 目标帧率。目标帧率取窗口所在显示器刷新率，
/// 上限 60（默认）；刷新率 <60（如 50Hz 面板）则回退到实际值；查询失败按 60 处理。
unsafe fn window_frame_interval_ms(hwnd: HWND) -> u128 {
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
            // 帧路径也消费意图：`App::channel` 的 `on_message` 与 `on_interval` 的回调都
            // 拿得到 `EventCtx`，它们请求的窗口操作/对话框/关窗与热键改绑都产生在**帧内**
            // （pump 在 render 起始排空，定时器回调靠 InvalidateRect 汇到这一帧），
            // 事件路径的消费点等不到它们——不在此落地就要拖到用户下一次点键盘鼠标，
            // 表现为"后台任务完成了却半天不关窗"。
            //
            // 顺序与指针路径一致：窗口操作（含热键队列）→ 对话框 → 关窗。三者都在
            // `state` 借用之外执行（铁律 6）：run_window_op 与阻塞式对话框都会同步重入本函数。
            apply_window_op(hwnd);
            apply_dialog_request(hwnd);
            if state_from(hwnd)
                .map(|s| s.handler.wants_close())
                .unwrap_or(false)
            {
                let _ = DestroyWindow(hwnd);
            }
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
        // 窗口激活/失活：宿主据此把光标转静态（失活窗口的插入符本就不该闪，也没理由
        // 按刷新率出帧）。仍交默认处理——激活的焦点归属由系统负责，我们只是搭个便车。
        WM_ACTIVATE => {
            handle_activate(hwnd, wparam);
            DefWindowProcW(hwnd, msg, wparam, lparam)
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
                // 拦截器（`App::on_close_request`）现在收 `EventCtx`，"挡下这次关闭 +
                // `ctx.defer_blocking` 弹原生确认框"是它的正规用法——确认框必须等到
                // 事件分发完全返回后才能弹，而这里正是 WM_CLOSE 的返回前一刻。
                // 不在此消费的话，那个闭包要拖到下一次用户事件才跑，看起来像点了没反应。
                apply_dialog_request(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // 最后一个窗口关闭才结束消息循环：多窗口下关掉设置子窗不该把整个应用带走。
            //
            // 先发退出消息让消息循环立即响应，再释放资源（避免阻塞退出感知）。
            // TrayState::drop 会调 Shell_NotifyIconW(NIM_DELETE)，需在进程退出前执行，
            // 因此不能 leak，仍须显式 drop——但顺序调整后用户感知到的关闭延迟消失。
            if unregister_window(hwnd) {
                PostQuitMessage(0);
            }
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
    // 开窗请求同点消费（`ctx.open_window` 排队 → 此处落地）。
    open_pending_windows(hwnd);
    // 跨窗口状态：本次分发若写过信号，让其余窗口也重绘一次。
    broadcast_signal_dirty(hwnd);
}

/// 本次事件分发写过信号时，让**除发起方外**的窗口各失效一次。
///
/// `Signal` 是跨窗口共享状态的唯一原语（`Copy` 句柄，传进子窗即可共享），但事件分发
/// 只会让发起方产生脏区——"在设置窗里改了名字，主窗显示的还是旧的"就是这么来的。
/// 发起方跳过：它自己已经有精确脏区，整窗失效反而把局部重绘的收益抹掉。
///
/// 单窗口下恒为空操作（除自己外没有别的窗口），故这条广播不会影响既有性能画像。
unsafe fn broadcast_signal_dirty(origin: HWND) {
    if !crate::signal::take_cross_window_dirty() {
        return;
    }
    for h in live_windows() {
        if h != origin {
            let _ = InvalidateRect(Some(h), None, false);
        }
    }
}

/// 建出 `ctx.open_window` 排队的子窗口。
///
/// **两段式**（铁律 6）：先取完请求、释放发起方的 `WindowState` 借用，再 `CreateWindowExW`
/// ——建窗会同步派发 WM_NCCREATE/WM_SIZE/WM_PAINT，其中 WM_PAINT 会走到新窗口自己的
/// `state_from`；若此刻发起方的借用还活着，两个 `&mut WindowState` 就并存了。
unsafe fn open_pending_windows(hwnd: HWND) {
    // `is_open` 查的是 `LIVE_WINDOWS`——与 `state_from` 拿的裸指针不是同一份状态，
    // 两者可同时活着。单例判定必须由宿主在**构建内容之前**做，故只能这样传进去。
    let requests = match state_from(hwnd) {
        Some(s) => s
            .handler
            .take_new_windows(&|key| find_single_window(key).is_some()),
        None => return,
    };
    if requests.is_empty() {
        return;
    }
    // 借用已释放（`requests` 是拥有的值）。以下可以安全地重入窗口过程。
    let hmodule = match GetModuleHandleW(None) {
        Ok(m) => m,
        Err(_) => return,
    };
    let hinst = HINSTANCE(hmodule.0);
    let renderer = app_host().map(|h| h.renderer).unwrap_or_default();
    for item in requests {
        match item {
            // 单例（`Window::single`）：把已有的那个激活到前台。找不到说明它在判定与
            // 执行之间被关掉了——正常竞态，什么都不做即可。
            NewWindow::Focus(key) => {
                if let Some(existing) = find_single_window(&key) {
                    show_and_activate(existing);
                }
            }
            // 子窗建不起来只是少一个窗口，不该带走整个应用——`create_window` 已打印原因。
            NewWindow::Create(cfg, handler) => {
                if let Some(child) = create_window(hinst, &cfg, handler, renderer) {
                    show_window(child);
                }
            }
        }
    }
}

/// 消费运行期热键操作队列（改绑/启停），落地到 `HotkeyState`。
///
/// 队列在窗口的 handler 上（`HotkeyHandle` 排进去的），而 `HotkeyState` 在 App 级宿主上，
/// 故要跨两个 state。**先取完队列、释放窗口那份借用，再借宿主**：两份借用不重叠，
/// 与铁律 6 同一个理由——中间隔着的 `Register/UnregisterHotKey` 虽不向本线程同步派发
/// 消息，但让两个 `&mut` 同时活着本身就是别名。
unsafe fn apply_hotkey_ops(hwnd: HWND) {
    let ops = match state_from(hwnd) {
        Some(state) => state.handler.take_hotkey_ops(),
        None => return,
    };
    if ops.is_empty() {
        return;
    }
    if let Some(hk) = app_host().and_then(|h| h.hotkeys.as_mut()) {
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

/// 托盘消息处理。**严格分段，每段之间必须释放 `AppHost` 借用**（铁律 6）。
///
/// 托盘是重入风险最高的路径：右键菜单的 `TrackPopupMenu` 自带模态消息循环，菜单
/// 从弹出到用户点选之间的每一次鼠标移动都会重入窗口过程。
///
/// 分段按「这个 OS 调用会不会重入」切，两条路径互斥（非先后关系）：
/// - 点击路径：「取意图」（持借用）→「执行意图」（无借用）。
/// - 菜单路径：「建菜单」（持借用，不重入）→「弹菜单」（**必须无借用**，模态重入）
///   →「跑选中项」（持借用，只写意图）→「执行意图」（无借用）。
///
/// 动作分类由自由函数 `tray::classify` 完成，不碰 state——右键路径因此全程只在
/// 「建菜单」「跑选中项」两处取借用。
unsafe fn on_tray_message(lparam: LPARAM) {
    // 菜单要弹在主窗口名下：`TrackPopupMenu` 先 `SetForegroundWindow` 才能让菜单在
    // 点击别处时正常消失，而 message-only 窗口不可见、前置它没有意义。
    let Some(main) = app_host().map(|h| h.main) else {
        return;
    };
    match tray::classify(lparam) {
        tray::TrayEvent::Click(kind) => {
            // 取意图：借 host 跑回调；借用随本语句结束而释放。
            let actions = app_host()
                .and_then(|h| h.tray.as_mut())
                .map(|ts| tray::run_click(ts, kind))
                .unwrap_or_default();
            // 执行意图：已无借用。
            run_tray_actions(main, actions);
        }
        tray::TrayEvent::RightClick => {
            // 建菜单：借用内完成（CreatePopupMenu/AppendMenuW 均不重入）。
            let Some(menu) = app_host()
                .and_then(|h| h.tray.as_ref())
                .and_then(|ts| ts.build_menu())
            else {
                return;
            };
            // 弹菜单：**无借用**。菜单存续期间窗口过程会被反复重入。
            let id = tray::track_menu(main, menu);
            if id == 0 {
                return; // 用户取消
            }
            // 跑选中项：重借取意图，借用随语句释放。
            //
            // 若宿主窗口在弹菜单的模态循环里被销毁，其 `WM_DESTROY` 已先清零
            // GWLP_USERDATA 才 drop `AppHost`（见 `app_host_proc`），故此处
            // `app_host()` 返回 None 而非解引用已释放内存——这个顺序是本分段设计的前提。
            let actions = app_host()
                .and_then(|h| h.tray.as_mut())
                .map(|ts| ts.run_item(id))
                .unwrap_or_default();
            // 执行意图：已无借用。
            run_tray_actions(main, actions);
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
            // 销毁**全部**窗口而不只是主窗口：退出说的是整个应用，留下一个设置子窗
            // 会让消息循环继续跑（最后一个窗口关闭才退出，见 `LiveWindows`），
            // 表现为「点了退出，托盘图标没了，程序还在」。
            //
            // `break` 丢弃 quit 之后的意图，是刻意的三重收口：窗口已销毁，后续意图
            // 本就无从生效（HWND 失效、`state_from` 取不到 state）；显式截断让这个
            // 事实可读，而非依赖两个不相干的兜底；也堵住「HWND 被系统回收后
            // `state_from` 取到另一个窗口的 state」这一理论缺口。macOS 侧
            // `NSApp::terminate` 本就不返回，两平台由此在构造上一致。
            tray::TrayAction::Quit => {
                for h in live_windows() {
                    let _ = DestroyWindow(h);
                }
                break;
            }
            // 先取出投递目标释放借用，再调 Shell_NotifyIconW（它会跨线程发消息）。
            tray::TrayAction::Notify { title, body } => {
                let target = app_host()
                    .and_then(|h| h.tray.as_ref())
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
        // 先问再显示：`ShowWindow` 之后 `IsWindowVisible` 恒为真，跃迁就无从判断了。
        let was_hidden = !IsWindowVisible(hwnd).as_bool();
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        } else {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        let _ = SetForegroundWindow(hwnd);
        if was_hidden {
            notify_shown(hwnd);
        }
    }
}

/// 通知宿主"窗口刚被唤起"。
///
/// 放在 `ShowWindow`/`SetForegroundWindow` **之后**：那两个 API 会同步重入 `wnd_proc`
/// （WM_SHOWWINDOW/WM_ACTIVATE），期间若持着 `&mut WindowState` 就是别名 UB（铁律 6）。
/// 借用只活在取 repaint 那一条语句里。
unsafe fn notify_shown(hwnd: HWND) {
    let repaint = state_from(hwnd).is_some_and(|s| s.handler.on_window_shown());
    if repaint {
        let _ = InvalidateRect(Some(hwnd), None, false);
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
/// WM_ACTIVATE：把激活态转给宿主，需要时重绘一帧。
///
/// 严格两段式（铁律 6）：`InvalidateRect` 会同步派发 WM_PAINT，持着 `state` 借用调它
/// 就是重入。
unsafe fn handle_activate(hwnd: HWND, wparam: WPARAM) {
    // 低 16 位：WA_INACTIVE(0) / WA_ACTIVE(1) / WA_CLICKACTIVE(2)。
    let active = (wparam.0 & 0xFFFF) != WA_INACTIVE as usize;
    let repaint = {
        let Some(state) = state_from(hwnd) else {
            return;
        };
        state.handler.on_window_activated(active)
    };
    if repaint {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

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

/// VK 码 → 框架键。具名键逐个翻译，其余原样落进 `Key::Other`（Ctrl+A/C/V/X 等靠它）。
///
/// 独立成函数是为了可测：`hotkey.rs` 的 `vk_of` 是同一套键码的**反向**映射，两张表若走偏，
/// 症状是"注册的是 Home、按下去却当 End 处理"——两处各自看都对，只有对着看才发现。
/// 见 `hotkey.rs` 里钉住互逆关系的那条测试（macOS 侧本就有，Windows 侧此前缺）。
pub(crate) fn map_vk(vk: u16) -> Key {
    if vk == VK_TAB.0 {
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
    } else if vk == VK_PRIOR.0 {
        Key::PageUp
    } else if vk == VK_NEXT.0 {
        Key::PageDown
    } else {
        Key::Other(vk as u32)
    }
}

/// 把 VK 码翻译为框架键并分发。
unsafe fn handle_key(hwnd: HWND, wparam: WPARAM) {
    let vk = wparam.0 as u16;
    let shift = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
    let ev = KeyEvent {
        key: map_vk(vk),
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
mod live_windows_tests {
    use super::{swap_rb_inplace, swap_rb_rect, LiveWindows, Rect};

    /// 单窗口：关掉它就是关掉最后一个，消息循环该退出。
    #[test]
    fn single_window_close_is_last() {
        let mut w = LiveWindows::new();
        w.add(1, None);
        assert!(w.remove(1), "唯一的窗口关掉后应报告「已空」");
    }

    /// 多窗口：只有最后一个才报告「已空」。
    ///
    /// 这是本次改动的全部意义——此前任何窗口的 `WM_DESTROY` 都直接 `PostQuitMessage`，
    /// 关掉设置子窗会把整个应用一起带走。
    #[test]
    fn only_the_last_window_reports_empty() {
        let mut w = LiveWindows::new();
        w.add(1, None);
        w.add(2, None);
        w.add(3, None);
        assert!(!w.remove(2), "还有两个窗口活着，不该退出");
        assert!(!w.remove(1), "还有一个窗口活着，不该退出");
        assert!(w.remove(3), "最后一个关掉才该退出");
    }

    /// 关闭顺序与登记顺序无关：先关最早建的那个同样不该退出。
    #[test]
    fn close_order_does_not_matter() {
        let mut w = LiveWindows::new();
        w.add(10, None);
        w.add(20, None);
        assert!(!w.remove(10));
        assert!(w.remove(20));
    }

    /// 回归：局部帧的 R/B 交换必须按**矩形**做，不能按整行。
    ///
    /// 曾按整行翻：脏区左右两侧那些**已是 BGRA** 的像素会被翻第二次，红蓝颠倒。
    /// 灰/白/黑处 R≈G≈B 看不出来，只有饱和色显形——所以示例界面全绿、用户应用里
    /// 有彩色的那几行出错。这里用一个红色底（R 与 B 差别最大）建模整条序列。
    #[test]
    fn partial_frame_swaps_only_the_damage_rect() {
        const W: i32 = 8;
        const H: i32 = 4;
        let rgba_red = [0xFFu8, 0x00, 0x00, 0xFF]; // R=255 B=0
        let bgra_red = [0x00u8, 0x00, 0xFF, 0xFF]; // 交换后
        let mut buf: Vec<u8> = rgba_red
            .iter()
            .copied()
            .cycle()
            .take((W * H * 4) as usize)
            .collect();

        // 第一帧：整窗。宿主画出 RGBA，平台整窗交换 → 全缓冲变 BGRA。
        swap_rb_inplace(&mut buf);
        assert!(buf.chunks(4).all(|p| p == bgra_red), "整窗帧后应全为 BGRA");

        // 第二帧：局部。宿主只把脏矩形重画成 RGBA（模拟 blit_back_rect_to：逐行只写这几列）。
        let dmg = Rect::new(2, 1, 3, 2);
        let stride = (W * 4) as usize;
        for y in dmg.y..dmg.bottom() {
            for x in dmg.x..dmg.right() {
                let o = y as usize * stride + x as usize * 4;
                buf[o..o + 4].copy_from_slice(&rgba_red);
            }
        }
        swap_rb_rect(&mut buf, W, dmg);

        // 判据：整张缓冲仍恒为 BGRA。脏区左右两侧若被二次交换，这里就会读到 RGBA。
        for y in 0..H {
            for x in 0..W {
                let o = y as usize * stride + x as usize * 4;
                assert_eq!(
                    &buf[o..o + 4],
                    &bgra_red,
                    "({x},{y}) 应为 BGRA；脏区是 {dmg:?}，此处若成 RGBA 即被翻了两次"
                );
            }
        }
    }

    /// 注销一个没登记过的句柄不得报告「已空」。
    ///
    /// release 构建里 `debug_assert` 不生效，这条路径必须自己站得住：若返回 true，
    /// 一次意料之外的重复 `WM_DESTROY` 就会在其他窗口还开着时杀掉整个应用。
    #[test]
    fn removing_unknown_window_never_reports_empty() {
        let mut w = LiveWindows::new();
        w.add(1, None);
        // debug 下 remove 会 debug_assert，故只在 release 语义下验证返回值。
        if !cfg!(debug_assertions) {
            assert!(!w.remove(999), "注销未知句柄不该被当成「最后一个」");
            assert_eq!(w.ids(), vec![1], "登记表不应被未知句柄的注销改动");
        }
    }

    /// 重复注销同一个窗口不得二次报告「已空」。
    #[test]
    fn double_remove_does_not_report_empty_twice() {
        let mut w = LiveWindows::new();
        w.add(1, None);
        w.add(2, None);
        assert!(!w.remove(1));
        assert!(w.remove(2), "第二个关掉时应报告已空");
        if !cfg!(debug_assertions) {
            assert!(!w.remove(2), "已经空了，重复注销不该再报一次");
        }
    }

    /// 单例键（`Window::single`）：同键的第二次请求找得到第一个窗口。
    #[test]
    fn single_key_finds_existing_window() {
        let mut w = LiveWindows::new();
        w.add(1, Some("settings".into()));
        assert_eq!(w.find_single("settings"), Some(1));
    }

    /// 不同键之间互不干扰——「设置」开着不该让「关于」也被挡下。
    #[test]
    fn different_single_keys_do_not_collide() {
        let mut w = LiveWindows::new();
        w.add(1, Some("settings".into()));
        w.add(2, Some("about".into()));
        assert_eq!(w.find_single("settings"), Some(1));
        assert_eq!(w.find_single("about"), Some(2));
        assert_eq!(
            w.find_single("help"),
            None,
            "没登记过的键不该匹配到任何窗口"
        );
    }

    /// 无键的普通窗口**永不**参与去重：它们本就该点几次开几个。
    #[test]
    fn keyless_windows_never_match() {
        let mut w = LiveWindows::new();
        w.add(1, None);
        w.add(2, None);
        assert_eq!(w.find_single("settings"), None);
    }

    /// 键随窗口注销一并释放：关掉设置窗之后必须能再开出来。
    ///
    /// 这正是把单例判定放在登记表、而不是让应用层自己拿 `Signal` 记标记的理由——
    /// 标记会因为绕过 `on_close_request` 的关闭路径（如 `ctx.request_close()`）而漏
    /// 复位，那之后那个窗口就再也开不出来了。
    #[test]
    fn single_key_is_released_when_window_closes() {
        let mut w = LiveWindows::new();
        w.add(1, Some("settings".into()));
        w.add(2, None);
        assert!(!w.remove(1), "还有一个窗口活着");
        assert_eq!(
            w.find_single("settings"),
            None,
            "窗口关掉后键必须释放，否则设置窗再也开不出来"
        );
    }

    /// 同一个键先后用于两个窗口（关掉再开）：找到的是新的那个。
    #[test]
    fn single_key_rebinds_after_reopen() {
        let mut w = LiveWindows::new();
        w.add(1, Some("settings".into()));
        assert!(w.remove(1));
        w.add(7, Some("settings".into()));
        assert_eq!(w.find_single("settings"), Some(7));
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
