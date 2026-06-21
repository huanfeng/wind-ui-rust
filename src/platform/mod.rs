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
pub use win32::{open_url, run, Tray, TrayCtx, TrayMenuItem};

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub use macos::clipboard::MacClipboard as Clipboard;
#[cfg(target_os = "macos")]
pub use macos::{open_url, run, Tray, TrayCtx, TrayMenuItem};

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("windui 目前仅支持 Windows 与 macOS 平台");

use std::path::Path;
use std::path::PathBuf;

use tiny_skia::Pixmap;

use crate::event::{CursorShape, KeyEvent, MouseButton, PointerEvent, PointerKind, WindowOp};
use crate::geometry::{Color, Point, Size};

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
    // 可选：合成一次悬停（Move）并等待超过提示延时，捕获 tooltip 等悬停浮层。
    if let Some((lx, ly)) = cfg.screenshot_hover {
        let pos = Point::new((lx as f32 * s).round() as i32, (ly as f32 * s).round() as i32);
        handler.on_pointer(PointerEvent::single(PointerKind::Move, pos, MouseButton::Left));
        // 等待跨过悬停延时（提示延时 500ms + 余量），再渲染让提示显现。
        std::thread::sleep(std::time::Duration::from_millis(650));
        pixmap.fill(to_skia_color(cfg.bg));
        handler.render(&mut pixmap, size);
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
        handler.render(&mut pixmap, size);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    pixmap.save_png(path).expect("保存 PNG 失败");
    eprintln!("[windui] 截屏已保存: {}", path.display());
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
    /// 截屏前合成一次左键单击（逻辑坐标，Down+Up），用于验证下拉展开等交互视觉。
    pub screenshot_click: Option<(i32, i32)>,
    /// 截屏前合成一次悬停（逻辑坐标 Move）并等待超过提示延时，用于验证 tooltip 等悬停视觉。
    pub screenshot_hover: Option<(i32, i32)>,
    /// 系统托盘图标（None=不创建）。窗口创建后安装，窗口销毁时自动清理。
    pub tray: Option<Tray>,
    /// 无标题栏窗口（自定义标题栏）：客户区铺满整窗，保留系统级吸附/阴影/缩放。
    pub frameless: bool,
    /// 动画全局开关：None=随系统“显示动画”设置；Some(b)=强制开/关。
    pub animations: Option<bool>,
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
            screenshot_hover: None,
            tray: None,
            frameless: false,
            animations: None,
        }
    }
}

/// 平台驱动的应用逻辑：渲染一帧 + 处理输入。返回 true 表示需要重绘。
pub trait AppHandler {
    fn render(&mut self, pixmap: &mut Pixmap, size: Size);
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

    /// 本帧是否有控件请求持续动画。平台层据此在阻塞空闲与按帧驱动之间切换。
    fn wants_animation(&self) -> bool {
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
}
