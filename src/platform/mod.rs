//! 平台抽象层。按目标平台分发到具体后端：Windows→`win32`，macOS→`macos`。
//!
//! 各后端对外暴露同形的 API（`run` / `open_url` / `Tray` 三件套 / `Clipboard`），
//! 由本模块按 `cfg` 统一 re-export；上层（`app`/`lib::prelude`）只依赖 `crate::platform::*`，
//! 不直接触碰任何具体后端，从而保持平台无关。
//!
//! 平台无关的窗口配置 `WindowConfig` 定义在本层（其 `tray` 字段类型按 `cfg` 解析到各后端的 `Tray`）。
//! win32 模块名（而非 `windows`）以免与外部 `windows` crate 冲突。

// 模块名用 `win32` 而非 `windows`，以免与外部 `windows` crate 冲突。
#[cfg(windows)]
pub mod win32;
#[cfg(windows)]
pub use win32::clipboard::WinClipboard as Clipboard;
#[cfg(windows)]
pub(crate) use win32::run;
#[cfg(windows)]
pub use win32::{open_url, Tray, TrayCtx, TrayMenuItem};

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub use macos::clipboard::MacClipboard as Clipboard;
#[cfg(target_os = "macos")]
pub(crate) use macos::run;
#[cfg(target_os = "macos")]
pub use macos::{open_url, Tray, TrayCtx, TrayMenuItem};

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("windui 目前仅支持 Windows 与 macOS 平台");

use std::cell::Cell;
use std::path::Path;
use std::path::PathBuf;

use tiny_skia::Pixmap;

use crate::event::{CursorShape, KeyEvent, MouseButton, PointerEvent, PointerKind, WindowOp};
use crate::geometry::{Color, Point, Size};

thread_local! {
    /// 本线程是否正处于"风险事件分发窗口"内：控件 `on_pointer`/`on_key` 回调正在栈上运行，
    /// OS 鼠标捕获尚未同步（见 win32/macos 后端 `dispatch_pointer`/`dispatch_key` 的两段式
    /// 实现）。`PickDialog` 的阻塞方法据此在 debug 下检测误用。
    static IN_EVENT_DISPATCH: Cell<bool> = const { Cell::new(false) };
}

/// RAII 标记：进入风险事件分发窗口，`Drop` 时自动清除（含回调 panic 时的展开路径）。
/// 各平台后端在调用 `handler.on_pointer`/`on_key` 前后台此持有。
pub(crate) struct EventDispatchGuard(());

impl EventDispatchGuard {
    pub(crate) fn enter() -> Self {
        IN_EVENT_DISPATCH.with(|f| f.set(true));
        Self(())
    }
}

impl Drop for EventDispatchGuard {
    fn drop(&mut self) {
        IN_EVENT_DISPATCH.with(|f| f.set(false));
    }
}

fn in_event_dispatch() -> bool {
    IN_EVENT_DISPATCH.with(|f| f.get())
}

/// `Color`（非预乘 RGBA8）→ tiny-skia 颜色。各后端清屏/填底共用。
pub(crate) fn to_skia_color(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

/// 离屏渲染一帧并保存 PNG——**平台无关**逻辑，Windows 与 macOS 的 `run` 在
/// `cfg.screenshot.is_some()` 时共用。无需窗口，适合自动化视觉回归。
///
/// 与窗口路径走同一渲染管线：按 `screenshot_scale` 物理化尺寸、可选合成
/// 右键/单击/悬停交互、收敛动画推进若干帧以捕获稳定终态。
pub(crate) fn run_offscreen(cfg: &WindowConfig, handler: &mut Box<dyn AppHandler>, path: &Path) {
    // 物理像素 = 逻辑尺寸 × scale，供高 DPI 截屏验证。
    let s = cfg.screenshot_scale.max(0.1);
    let pw = (cfg.width as f32 * s).round().max(1.0) as i32;
    let ph = (cfg.height as f32 * s).round().max(1.0) as i32;
    let size = Size::new(pw, ph);
    let mut pixmap = Pixmap::new(pw as u32, ph as u32).expect("分配 pixmap 失败");
    pixmap.fill(to_skia_color(cfg.bg));
    handler.set_scale(s);
    let mut tgt = crate::render::PixmapTarget {
        pixmap: &mut pixmap,
    };
    handler.render(&mut tgt, size);
    // 可选：合成一次右键按下（先渲染暖布局，再派发事件，再重绘以捕获菜单）。
    if let Some((lx, ly)) = cfg.screenshot_rclick {
        let pos = Point::new(
            (lx as f32 * s).round() as i32,
            (ly as f32 * s).round() as i32,
        );
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            pos,
            MouseButton::Right,
        ));
        pixmap.fill(to_skia_color(cfg.bg));
        let mut tgt = crate::render::PixmapTarget {
            pixmap: &mut pixmap,
        };
        handler.render(&mut tgt, size);
    }
    // 可选：依次合成左键单击（Down+Up），捕获下拉展开等。多个 `--click` 按序回放，
    // 用于验证需要连续点击才能到达的状态（如复选菜单连点多个开关而菜单不关）。
    for &(lx, ly) in &cfg.screenshot_clicks {
        let pos = Point::new(
            (lx as f32 * s).round() as i32,
            (ly as f32 * s).round() as i32,
        );
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            pos,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Up,
            pos,
            MouseButton::Left,
        ));
        pixmap.fill(to_skia_color(cfg.bg));
        let mut tgt = crate::render::PixmapTarget {
            pixmap: &mut pixmap,
        };
        handler.render(&mut tgt, size);
    }
    // 可选：合成一次悬停（Move）并等待超过提示延时，捕获 tooltip 等悬停浮层。
    if let Some((lx, ly)) = cfg.screenshot_hover {
        let pos = Point::new(
            (lx as f32 * s).round() as i32,
            (ly as f32 * s).round() as i32,
        );
        handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            pos,
            MouseButton::Left,
        ));
        // 等待跨过悬停延时（提示延时 500ms + 余量），再渲染让提示显现。
        std::thread::sleep(std::time::Duration::from_millis(650));
        pixmap.fill(to_skia_color(cfg.bg));
        let mut tgt = crate::render::PixmapTarget {
            pixmap: &mut pixmap,
        };
        handler.render(&mut tgt, size);
    }
    // 有动画时推进帧：收敛型（开关/按钮等补间）循环到不再请求动画即停（捕获稳定终态，
    // 不依赖单帧 300ms ≥ 所有时长）；永续型（不确定进度等永远请求动画）由迭代上限兜底，
    // 避免无限循环——末帧相位非零即可在截图显现。
    for _ in 0..4 {
        if !handler.wants_animation() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        pixmap.fill(to_skia_color(cfg.bg));
        let mut tgt = crate::render::PixmapTarget {
            pixmap: &mut pixmap,
        };
        handler.render(&mut tgt, size);
    }
    // 性能基准（WINDUI_BENCH=N）：首帧已暖（字体/阴影缓存已建），再渲染 N 帧打印稳态帧耗时。
    if let Ok(spec) = std::env::var("WINDUI_BENCH") {
        let n: u32 = spec.parse().unwrap_or(30);
        let mut total = 0.0f32;
        for i in 0..n {
            let t = std::time::Instant::now();
            pixmap.fill(to_skia_color(cfg.bg));
            let mut tgt = crate::render::PixmapTarget {
                pixmap: &mut pixmap,
            };
            handler.render(&mut tgt, size);
            let ms = t.elapsed().as_secs_f32() * 1000.0;
            total += ms;
            eprintln!("[windui] bench frame {i}: {ms:.2} ms");
        }
        eprintln!(
            "[windui] bench 平均: {:.2} ms / 帧（{} 帧，全窗重绘）",
            total / n.max(1) as f32,
            n
        );
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    pixmap.save_png(path).expect("保存 PNG 失败");
    eprintln!("[windui] 截屏已保存: {}", path.display());
}

/// 一条全局热键绑定：组合 + 回调。
///
/// 回调拿到的 [`HotkeyCtx`](crate::event::HotkeyCtx) **不持有窗口句柄**，只能声明
/// 窗口操作意图——回调在平台层持有窗口状态借用期间执行，直接调用 OS 窗口 API 会
/// 同步重入消息处理并造成 `&mut` 别名（见 `AGENTS.md` 铁律 6）。
pub struct HotkeyBinding {
    pub hotkey: crate::event::Hotkey,
    pub callback: Box<dyn FnMut(&mut crate::event::HotkeyCtx)>,
}

/// 窗口配置（平台无关）。由 `App` 构建器组装，交各平台后端的 `run` 消费。
pub struct WindowConfig {
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub bg: Color,
    /// 窗口居中显示。
    pub centered: bool,
    /// 允许用户调整窗口大小（默认 true）。
    pub resizable: bool,
    /// 截屏模式：渲染一帧离屏存 PNG 后立即退出，不创建窗口。
    pub screenshot: Option<PathBuf>,
    /// 截屏时的 DPI 缩放（默认 1.0），用于验证高 DPI 渲染。
    pub screenshot_scale: f32,
    /// 截屏前合成一次右键按下（逻辑坐标），用于验证右键菜单等交互视觉。
    pub screenshot_rclick: Option<(i32, i32)>,
    /// 截屏前依次回放的左键单击（逻辑坐标，各合成 Down+Up），用于验证下拉展开等交互视觉。
    /// 多个点按序回放，可捕获需连续点击才能到达的状态（如复选菜单连点多个开关）。
    pub screenshot_clicks: Vec<(i32, i32)>,
    /// 截屏前合成一次悬停（逻辑坐标 Move）并等待超过提示延时，用于验证 tooltip 等悬停视觉。
    pub screenshot_hover: Option<(i32, i32)>,
    /// 系统托盘图标（None=不创建）。窗口创建后安装，窗口销毁时自动清理。
    pub tray: Option<Tray>,
    /// 全局热键绑定（空=不注册）。窗口创建后注册，窗口销毁时自动注销。
    pub hotkeys: Vec<HotkeyBinding>,
    /// 启动即隐藏：窗口创建后不显示，交由托盘或全局热键唤起。
    ///
    /// 无托盘图标也无热键时启用此项，用户将**永远看不到窗口**——故 `App::start_hidden`
    /// 在 debug 期对该组合 panic 提示误用。
    pub start_hidden: bool,
    /// 无标题栏窗口（自定义标题栏）：客户区铺满整窗，保留系统级吸附/阴影/缩放。
    pub frameless: bool,
    /// 窗口置顶（始终浮于其他窗口之上，如系统提示弹框）。
    pub topmost: bool,
    /// 窗口创建完成、**首次显示前**的回调（收到平台句柄数值，win32=HWND、macOS=NSWindow
    /// 指针）。用于定位窗口、调整样式后自行显示，避免"先默认显示再跳位"的闪现。
    /// 与 `start_hidden` 搭配时须在回调内自行显示窗口，否则窗口将保持隐藏。
    pub on_ready: Option<Box<dyn FnMut(isize)>>,
    /// 动画全局开关：None=随系统“显示动画”设置；Some(b)=强制开/关。
    pub animations: Option<bool>,
    /// GPU 加速渲染（Direct2D 后端）opt-in，默认 false（软渲染）。仅不透明大窗有效；
    /// RDP 远程会话与离屏截图恒走软渲染（见 win32 后端选择）。
    pub accelerated: bool,
    /// 窗口最小客户区尺寸（逻辑 dp，0=不限制）。限制后用户无法把窗口缩到操作不到按钮。
    pub min_width: i32,
    pub min_height: i32,
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
            screenshot_clicks: Vec::new(),
            screenshot_hover: None,
            tray: None,
            hotkeys: Vec::new(),
            start_hidden: false,
            frameless: false,
            topmost: false,
            on_ready: None,
            animations: None,
            accelerated: false,
            min_width: 0,
            min_height: 0,
        }
    }
}

/// 平台驱动的应用逻辑：渲染一帧 + 处理输入。返回 true 表示需要重绘。
pub trait AppHandler {
    fn render(&mut self, target: &mut dyn crate::render::RenderTarget, size: Size);
    fn on_pointer(&mut self, _ev: PointerEvent) -> bool {
        false
    }
    fn on_key(&mut self, _ev: KeyEvent) -> bool {
        false
    }
    /// 是否请求关闭窗口（事件处理后由平台查询）。
    fn wants_close(&self) -> bool {
        false
    }
    /// 用户请求关闭窗口（点击 × 按钮或 WM_CLOSE）时调用。
    /// 返回 true 允许关闭，false 取消（如弹出"未保存"提示后需重绘，平台会自行 Invalidate）。
    fn on_close_request(&mut self) -> bool {
        true
    }
    /// 当前是否处于指针捕获态。平台据此调用 OS 的 SetCapture/ReleaseCapture，
    /// 保证拖出窗口时仍能收到移动/抬起消息。
    fn capture_active(&self) -> bool {
        false
    }
    /// OS 抢走指针捕获（Alt+Tab 等）时调用，让逻辑捕获方收尾（如复位拖动态）。
    /// 返回 true 表示需要重绘。
    fn on_capture_lost(&mut self) -> bool {
        false
    }
    /// 设置 DPI 缩放因子（DPI/96）。窗口创建后与 WM_DPICHANGED 时由平台调用。
    fn set_scale(&mut self, _scale: f32) {}

    /// 焦点文本控件的光标位置（**物理像素**，相对客户区左上角）+ 高度：`(x, y_top, height)`。
    /// 平台层据此定位输入法候选窗。无文本焦点时返回 None。
    fn ime_caret(&self) -> Option<(i32, i32, i32)> {
        None
    }

    /// 输入法组合态开始/结束（拼音等未上屏文字合成中）时由平台层调用，转发给
    /// 当前焦点控件（见 `Widget::set_composing`）。返回 true 表示需要重绘。
    fn set_ime_composing(&mut self, _composing: bool) -> bool {
        false
    }

    /// 本帧是否有控件请求持续动画。平台层据此在阻塞空闲与按帧驱动之间切换。
    fn wants_animation(&self) -> bool {
        false
    }

    /// 取走运行期热键操作队列（`HotkeyHandle` 的改绑/启停意图）。平台在意图
    /// 消费点（与窗口操作同点）调用并对 `HotkeyState` 落地。默认无操作。
    fn take_hotkey_ops(&mut self) -> Vec<(usize, crate::event::HotkeyOp)> {
        Vec::new()
    }

    /// 注册的定时器间隔（平台据此 SetTimer/NSTimer）。无则空。
    fn intervals(&self) -> Vec<std::time::Duration> {
        Vec::new()
    }

    /// 第 `idx` 个定时器到点：调对应回调。返回 true 表示需重绘。
    fn on_interval_fired(&mut self, _idx: usize) -> bool {
        false
    }

    /// 当前指针悬停位置期望的光标形状。平台层据此应答 OS 光标查询
    /// （win32 `WM_SETCURSOR`）。默认箭头。
    fn cursor(&self) -> CursorShape {
        CursorShape::Arrow
    }

    /// 触摸平移手势：在 `pos`（**物理像素**，相对客户区）按 `dy` 物理像素平移，
    /// 滚动手指下的容器。返回 true 表示需要重绘。
    fn on_pan(&mut self, _pos: Point, _dy: i32) -> bool {
        false
    }

    /// 触摸抬起时按释放速度启动惯性滑动（fling）。`pos` 为**物理像素**（相对客户区）、
    /// `vy` 为手指 y 速度（**物理像素/ms**）。返回 true 表示已启动（平台据此触发首帧）。
    fn start_fling(&mut self, _pos: Point, _vy: f32) -> bool {
        false
    }

    /// 取消进行中的惯性滑动（新触摸按下/点击/滚轮打断时）。返回 true 表示需要重绘。
    fn cancel_fling(&mut self) -> bool {
        false
    }

    /// 文件拖放到窗口：`pos` 为落点（**物理像素**，相对客户区），`paths` 为文件路径。
    /// 返回 true 表示需要重绘。
    fn on_drop_files(&mut self, _pos: Point, _paths: Vec<std::path::PathBuf>) -> bool {
        false
    }

    /// 无边框窗口命中测试：`pos`（**物理像素**，相对客户区）是否落在窗口拖动区
    /// （自定义标题栏）。平台据此在 `WM_NCHITTEST` 返回 HTCAPTION 实现拖动。
    fn window_drag_at(&self, _pos: Point) -> bool {
        false
    }

    /// 无边框窗口命中测试：`pos`（**物理像素**，相对客户区）是否落在交互控件（窗口按钮等）上。
    /// 平台据此在 `WM_NCHITTEST` 把该点强制判为 HTCLIENT，优先于缩放边框/拖动区。
    fn interactive_at(&self, _pos: Point) -> bool {
        false
    }

    /// 取出并清除待执行的窗口操作（自定义标题栏按钮触发）。平台在事件分发后轮询。
    fn take_window_op(&mut self) -> Option<WindowOp> {
        None
    }

    /// 取出并清除待执行的原生文件对话框请求。平台在事件分发**完全返回**（OS 侧鼠标
    /// 捕获已同步）之后才调用，避免在事件回调栈内重入阻塞式模态对话框。
    fn take_dialog_request(&mut self) -> Option<DialogRequest> {
        None
    }
}

// ── 文件 / 目录选择对话框 ────────────────────────────────────────────────────

/// 在调用 `pick_*` / `save_file` 前，将当前活跃窗口句柄注入 rfd 对话框。
///
/// Windows：读取 wnd_proc 入口处写入的 thread-local HWND，用 `IFileDialog::Show(hwnd)`
/// 把主窗口设为父窗口，确保对话框阻塞主窗口（父窗口被 EnableWindow(FALSE) 禁用直到关闭）。
///
/// macOS：rfd 内部以 `NSOpenPanel.runModal()` 运行，系统保证浮层正确置顶，无需注入。
#[cfg(windows)]
fn inject_parent(d: rfd::FileDialog) -> rfd::FileDialog {
    use raw_window_handle::{
        DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
        RawWindowHandle, Win32WindowHandle, WindowHandle, WindowsDisplayHandle,
    };
    use std::num::NonZeroIsize;

    let hwnd_val = win32::active_hwnd();
    let Some(nz) = NonZeroIsize::new(hwnd_val) else {
        return d;
    };
    struct W(NonZeroIsize);
    impl HasWindowHandle for W {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            Ok(unsafe {
                WindowHandle::borrow_raw(RawWindowHandle::Win32(Win32WindowHandle::new(self.0)))
            })
        }
    }
    impl HasDisplayHandle for W {
        fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
            Ok(unsafe {
                DisplayHandle::borrow_raw(RawDisplayHandle::Windows(WindowsDisplayHandle::new()))
            })
        }
    }
    d.set_parent(&W(nz))
}

#[cfg(target_os = "macos")]
fn inject_parent(d: rfd::FileDialog) -> rfd::FileDialog {
    d
}

/// 系统原生文件 / 目录选择对话框，链式配置后调 `pick_*` / `save_file` 弹出。
///
/// 框架自动将当前窗口注入为对话框父窗口，无需手动传递句柄：
/// - **Windows**：`IFileDialog::Show(hwnd)` — 主窗口在对话框期间被禁用，点击不会穿透
/// - **macOS**：`NSOpenPanel` 以浮层面板形式出现，系统保证 z 序
///
/// # 示例
/// ```no_run
/// use windui::prelude::*;
///
/// // 单文件
/// let file = PickDialog::new().title("打开图片").filter("图片", &["png", "jpg"]).pick_file();
///
/// // 保存
/// let dest = PickDialog::new().title("另存为").file_name("report.pdf").save_file();
/// ```
pub struct PickDialog(rfd::FileDialog);

impl Default for PickDialog {
    fn default() -> Self {
        Self::new()
    }
}

impl PickDialog {
    pub fn new() -> Self {
        Self(rfd::FileDialog::new())
    }

    /// 设置对话框标题栏文字。
    pub fn title(mut self, title: impl AsRef<str>) -> Self {
        self.0 = self.0.set_title(title.as_ref());
        self
    }

    /// 添加文件类型过滤器（`pick_file` / `pick_files` / `save_file` 生效；目录选择忽略）。
    /// 可链式调用多次以添加多个过滤项。
    pub fn filter(mut self, name: impl AsRef<str>, extensions: &[impl AsRef<str>]) -> Self {
        let exts: Vec<&str> = extensions.iter().map(|s| s.as_ref()).collect();
        self.0 = self.0.add_filter(name.as_ref(), &exts);
        self
    }

    /// 设置初始目录。
    pub fn directory(mut self, path: impl AsRef<Path>) -> Self {
        self.0 = self.0.set_directory(path.as_ref());
        self
    }

    /// 预填文件名输入框（`save_file` 场景常用）。
    pub fn file_name(mut self, name: impl AsRef<str>) -> Self {
        self.0 = self.0.set_file_name(name.as_ref());
        self
    }

    fn into_dialog(self) -> rfd::FileDialog {
        debug_assert!(
            !in_event_dispatch(),
            "PickDialog::pick_file()/pick_files()/pick_folder()/pick_folders()/save_file() \
             不能在控件事件回调（on_click/on_event）里直接调用——此时 OS 鼠标捕获尚未同步，\
             会与对话框自身的模态消息泵抢鼠标输入，反复开关几次就会让鼠标彻底失灵。回调里请改用 \
             EventCtx::request_pick_file()/request_pick_files()/request_pick_folder()/\
             request_pick_folders()/request_save_file()，多步流程用 EventCtx::defer_blocking()。"
        );
        inject_parent(self.0)
    }

    /// 打开**单文件**选择对话框；用户取消返回 `None`。
    pub fn pick_file(self) -> Option<PathBuf> {
        self.into_dialog().pick_file()
    }

    /// 打开**多文件**选择对话框；用户取消返回 `None`。
    pub fn pick_files(self) -> Option<Vec<PathBuf>> {
        self.into_dialog().pick_files()
    }

    /// 打开**单目录**选择对话框；用户取消返回 `None`。
    pub fn pick_folder(self) -> Option<PathBuf> {
        self.into_dialog().pick_folder()
    }

    /// 打开**多目录**选择对话框；用户取消返回 `None`。
    pub fn pick_folders(self) -> Option<Vec<PathBuf>> {
        self.into_dialog().pick_folders()
    }

    /// 打开**保存文件**对话框；用户取消返回 `None`。
    pub fn save_file(self) -> Option<PathBuf> {
        self.into_dialog().save_file()
    }
}

/// 由 `EventCtx::request_pick_file` 等方法产生，经 `DispatchResult` 上交宿主。
///
/// **不要**在控件事件回调里直接调用 `PickDialog::pick_file()` 等同步方法——那会在事件
/// 分发的调用栈深处同步进入模态对话框自己的消息泵，而此时本窗口的 OS 鼠标捕获
/// （`SetCapture`）可能还未来得及释放，导致对话框与主窗口抢鼠标输入，多次开关后
/// 会让内部捕获状态与 OS 实际状态错位，表现为鼠标彻底失灵。应改用 `EventCtx` 上的
/// `request_*` 方法：把对话框配置和拿到结果后的延续回调打包成请求，交给宿主在事件
/// 分发彻底返回、OS 输入状态已同步之后再真正弹出。
pub enum DialogRequest {
    PickFile(PickDialog, Box<dyn FnOnce(Option<PathBuf>)>),
    PickFiles(PickDialog, Box<dyn FnOnce(Option<Vec<PathBuf>>)>),
    PickFolder(PickDialog, Box<dyn FnOnce(Option<PathBuf>)>),
    PickFolders(PickDialog, Box<dyn FnOnce(Option<Vec<PathBuf>>)>),
    SaveFile(PickDialog, Box<dyn FnOnce(Option<PathBuf>)>),
    /// 逃生舱：任意一段包含若干阻塞式原生调用的流程（如"选文件→校验→选目录→确认"，
    /// 中间还要穿插 `MessageBoxW` 之类的系统模态框）。当单个 `PickFile`/`SaveFile`
    /// 装不下这种多步序列时用这个——闭包在事件分发完全返回之后运行，此时已不在
    /// 事件回调栈内，闭包内可以放心直接同步调用任意数量的阻塞式原生 API。
    Custom(Box<dyn FnOnce()>),
}

impl DialogRequest {
    /// 真正执行阻塞的原生对话框调用并触发延续回调。调用方须保证此时事件分发已
    /// 完全返回（OS 鼠标捕获等已同步），不会与对话框自身的模态消息泵冲突。
    pub fn run(self) {
        match self {
            DialogRequest::PickFile(d, cb) => cb(d.pick_file()),
            DialogRequest::PickFiles(d, cb) => cb(d.pick_files()),
            DialogRequest::PickFolder(d, cb) => cb(d.pick_folder()),
            DialogRequest::PickFolders(d, cb) => cb(d.pick_folders()),
            DialogRequest::SaveFile(d, cb) => cb(d.save_file()),
            DialogRequest::Custom(f) => f(),
        }
    }
}

#[cfg(test)]
mod dispatch_guard_tests {
    use super::*;

    #[test]
    fn event_dispatch_guard_tracks_state_and_clears_on_drop() {
        assert!(!in_event_dispatch());
        let guard = EventDispatchGuard::enter();
        assert!(in_event_dispatch());
        drop(guard);
        assert!(!in_event_dispatch());
    }

    // debug_assert! 在 release 构建里被剔除——只在 debug_assertions 开启时验证 panic，
    // 避免 release 测试构建真的跑进 into_dialog() 之后的阻塞 rfd 调用。
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "不能在控件事件回调")]
    fn pick_dialog_panics_when_called_inside_event_dispatch() {
        let _guard = EventDispatchGuard::enter();
        // debug_assert! 在 into_dialog() 里先触发 panic，不会真正调用阻塞的 rfd 接口。
        let _ = PickDialog::new().pick_folder();
    }
}
