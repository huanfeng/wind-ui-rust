//! 平台抽象层。按目标平台分发到具体后端：Windows→`win32`，macOS→`macos`。
//!
//! 各后端对外暴露同形的 API（`run` / `open_url` / `Clipboard`），由本模块按 `cfg` 统一
//! re-export；上层（`app`/`lib::prelude`）只依赖 `crate::platform::*`，不直接触碰任何具体
//! 后端，从而保持平台无关。
//!
//! 托盘的 `Tray` 三件套**不按 cfg 分发**：声明层收在平台无关的 [`tray`] 模块里，两个后端
//! 只保留执行半边（消费 `TrayAction`）。此前它是两份完整副本，下游的跨平台性只是「两边
//! 方法名恰好一样」的巧合，且 macOS 那份直接调 OS 因而回调不可测——理由详见 [`tray`]。
//!
//! 平台无关的窗口配置 `WindowConfig` 也定义在本层。
//! win32 模块名（而非 `windows`）以免与外部 `windows` crate 冲突。

// 模块名用 `win32` 而非 `windows`，以免与外部 `windows` crate 冲突。
#[cfg(windows)]
pub mod win32;
#[cfg(windows)]
pub use win32::clipboard::WinClipboard as Clipboard;
#[cfg(windows)]
pub use win32::open_url;
#[cfg(windows)]
pub(crate) use win32::run;

#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "macos")]
pub use macos::clipboard::MacClipboard as Clipboard;
#[cfg(target_os = "macos")]
pub use macos::open_url;
#[cfg(target_os = "macos")]
pub(crate) use macos::run;

#[cfg(not(any(windows, target_os = "macos")))]
compile_error!("windui 目前仅支持 Windows 与 macOS 平台");

/// 托盘的平台无关声明层（`Tray` / `TrayMenuItem` / `TrayCtx` / `TrayAction`）。
pub mod tray;
pub use tray::{Tray, TrayAction, TrayCtx, TrayMenuItem};

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

/// 离屏截图的渲染后端：软光栅或 GPU。
///
/// `run_offscreen` 要连渲多帧（初始、右键、点击、悬停、动画收敛、基准），每帧都是
/// 「清底 → 建 target → handler.render」这同一件事。收敛成一个类型，一是消掉六处
/// 重复，二是让 GPU 路径**只需换一个构造**——否则每处都要写一遍 cfg 分支。
enum Offscreen {
    /// tiny-skia 软光栅：像素就地画在自己的 `Pixmap` 上。
    Soft(Pixmap),
    /// Direct2D GPU：画进离屏位图后把像素取回，`last` 持有最近一帧。
    #[cfg(all(windows, feature = "d2d"))]
    Gpu {
        // 装箱：后端带六个缓存表，直接内联会让整个枚举跟着变胖，而软路径那一支
        // 只需要一个 Pixmap。
        backend: Box<crate::platform::win32::d2d::offscreen::OffscreenBackend>,
        last: Pixmap,
    },
}

impl Offscreen {
    /// 按 `renderer` 选后端。
    ///
    /// [`Renderer::Auto`] 下设备建不起来就回退软光栅并告警——截图路径宁可出图也不该
    /// 失败，但必须让人知道出的不是 GPU 的图，否则「GPU 与软渲染一致」这个结论会
    /// 建立在两张软渲染图上。[`Renderer::Gpu`] 则直接终止：它的用途就是"拿不到 GPU
    /// 要告诉我"，静默回退会让这次验证白做。
    fn new(w: u32, h: u32, renderer: Renderer) -> Self {
        #[cfg(all(windows, feature = "d2d"))]
        if renderer.wants_gpu() {
            if let Some(backend) =
                crate::platform::win32::d2d::offscreen::OffscreenBackend::new(w, h)
            {
                return Offscreen::Gpu {
                    backend: Box::new(backend),
                    last: Pixmap::new(w, h).expect("分配 pixmap 失败"),
                };
            }
            assert!(
                !renderer.requires_gpu(),
                "Renderer::Gpu 要求 GPU 截图，但 D2D 离屏设备建不起来（硬件与 WARP 都失败）。\
                 需要自动回退请改用 Renderer::Auto"
            );
            eprintln!("[windui] D2D 离屏设备创建失败，截图回退软渲染");
        }
        // 非 Windows 或未开 d2d feature：`Renderer::Gpu` 无从满足，同样应当报错而非
        // 让调用方以为拿到了 GPU 图。
        assert!(
            !renderer.requires_gpu() || cfg!(all(windows, feature = "d2d")),
            "Renderer::Gpu 在当前平台/编译配置下不可用（需要 Windows 且启用 d2d feature）"
        );
        Offscreen::Soft(Pixmap::new(w, h).expect("分配 pixmap 失败"))
    }

    /// 渲染一帧（清底 + 整树绘制）。
    ///
    /// `fallback_bg` 只在宿主不报底色时兜底：本函数会连渲多帧，而中间合成的交互
    /// （`--click` 回放到换肤按钮上）可能换掉主题——用创建时那份 `cfg.bg` 清屏，
    /// 截出来就是"控件已转暗、底色还停在亮色"的半吊子图。
    fn frame(&mut self, handler: &mut Box<dyn AppHandler>, size: Size, fallback_bg: Color) {
        let bg = handler.bg().unwrap_or(fallback_bg);
        match self {
            Offscreen::Soft(pm) => {
                pm.fill(to_skia_color(bg));
                let mut tgt = crate::render::PixmapTarget { pixmap: pm };
                handler.render(&mut tgt, size);
            }
            #[cfg(all(windows, feature = "d2d"))]
            Offscreen::Gpu { backend, last } => {
                // D2D 的 Clear(bg) 已完成清底，无需另行 fill。
                if let Some(pm) = backend.frame(bg, |t, s| handler.render(t, s)) {
                    *last = pm;
                }
            }
        }
    }

    fn pixmap(&self) -> &Pixmap {
        match self {
            Offscreen::Soft(pm) => pm,
            #[cfg(all(windows, feature = "d2d"))]
            Offscreen::Gpu { last, .. } => last,
        }
    }
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
    handler.set_scale(s);
    // 截图后端随 `renderer` 走：`--screenshot --renderer gpu` 出 GPU 图，使 29 个
    // example 都能做软硬整页比对，而不必为每条差异手写单元测试。
    let mut off = Offscreen::new(pw as u32, ph as u32, cfg.renderer);
    off.frame(handler, size, cfg.bg);
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
        off.frame(handler, size, cfg.bg);
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
        off.frame(handler, size, cfg.bg);
    }
    // 可选：合成一次左键拖拽（Down→Move→Up），捕获划选高亮等拖出才成立的状态。
    // Up 之后再出一帧：宿主对 Up 置 needs_relayout，选区须活过这次重排才算真成立。
    if let Some((x0, y0, x1, y1)) = cfg.screenshot_drag {
        let pt = |lx: i32, ly: i32| {
            Point::new(
                (lx as f32 * s).round() as i32,
                (ly as f32 * s).round() as i32,
            )
        };
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            pt(x0, y0),
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            pt(x1, y1),
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Up,
            pt(x1, y1),
            MouseButton::Left,
        ));
        off.frame(handler, size, cfg.bg);
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
        off.frame(handler, size, cfg.bg);
    }
    // 有动画时推进帧：收敛型（开关/按钮等补间）循环到不再请求动画即停（捕获稳定终态，
    // 不依赖单帧 300ms ≥ 所有时长）；永续型（不确定进度等永远请求动画）由迭代上限兜底，
    // 避免无限循环——末帧相位非零即可在截图显现。
    for _ in 0..4 {
        if !handler.wants_animation() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        off.frame(handler, size, cfg.bg);
    }
    // 性能基准（WINDUI_BENCH=N）：首帧已暖（字体/阴影缓存已建），再渲染 N 帧打印稳态帧耗时。
    if let Ok(spec) = std::env::var("WINDUI_BENCH") {
        let n: u32 = spec.parse().unwrap_or(30);
        let mut total = 0.0f32;
        for i in 0..n {
            let t = std::time::Instant::now();
            off.frame(handler, size, cfg.bg);
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
    off.pixmap().save_png(path).expect("保存 PNG 失败");
    eprintln!("[windui] 截屏已保存: {}", path.display());
    // 合成交互期间开出的子窗（`--click` 点到"打开设置…"之类）各自再出一张。
    // 不做这一步，多窗口界面的视觉回归就只能看到主窗，子窗永远没人验。
    shoot_child_windows(cfg, handler, path);
}

/// 给 `ctx.open_window` 开出的子窗各截一张，文件名在主图基础上加序号
/// （`out.png` → `out-1.png`、`out-2.png`…）。
///
/// 只走一层：子窗**再**开的窗口不再跟进。截图是给视觉回归用的，一层已经覆盖
/// "点开设置页看看长什么样"这个实际需求，递归下去只会让产物文件难以对应。
fn shoot_child_windows(cfg: &WindowConfig, handler: &mut Box<dyn AppHandler>, base: &Path) {
    // 离屏路径没有"已有窗口"，故 `is_open` 恒假；同一串 `--click` 里重复请求的同键窗口
    // 由宿主的批内去重挡下，回报成 `Focus` 后在这里丢弃。**出几张图因此与真跑开几个
    // 窗口一致**——否则截图里出两张、实际只开一个，视觉回归比对的就不是实际界面了。
    let children: Vec<_> = handler
        .take_new_windows(&|_| false)
        .into_iter()
        .filter_map(|w| match w {
            NewWindow::Create(cfg, h) => Some((cfg, h)),
            NewWindow::Focus(_) => None,
        })
        .collect();
    if children.is_empty() {
        return;
    }
    let s = cfg.screenshot_scale.max(0.1);
    for (i, (wcfg, mut whandler)) in children.into_iter().enumerate() {
        let pw = (wcfg.width as f32 * s).round().max(1.0) as i32;
        let ph = (wcfg.height as f32 * s).round().max(1.0) as i32;
        let size = Size::new(pw, ph);
        whandler.set_scale(s);
        // 渲染后端跟主窗一致，与窗口路径下由 `AppHost.renderer` 决定子窗后端同理。
        let mut off = Offscreen::new(pw as u32, ph as u32, cfg.renderer);
        off.frame(&mut whandler, size, wcfg.bg);
        // 同主窗：收敛型动画多推几帧以捕获稳定终态。
        for _ in 0..4 {
            if !whandler.wants_animation() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(300));
            off.frame(&mut whandler, size, wcfg.bg);
        }
        let out = child_screenshot_path(base, i + 1);
        off.pixmap().save_png(&out).expect("保存子窗 PNG 失败");
        eprintln!(
            "[windui] 子窗截屏已保存: {}（标题：{}）",
            out.display(),
            wcfg.title
        );
    }
}

/// `out.png` + 序号 1 → `out-1.png`。无扩展名时补 `png`。
///
/// 用序号而非窗口标题：标题可以带路径分隔符、冒号、换行，做文件名要先净化一遍，
/// 而净化后的名字反而不好和源码里的调用点对上。标题另行打印在日志里。
fn child_screenshot_path(base: &Path, idx: usize) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("screenshot"));
    let ext = base
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_else(|| String::from("png"));
    base.with_file_name(format!("{stem}-{idx}.{ext}"))
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
    /// 截屏前合成一次左键拖拽 `(x0, y0, x1, y1)`（逻辑坐标 Down→Move→Up），用于验证
    /// 划选高亮、拖动排序这类"只有拖出去才成立"的视觉——单击（`screenshot_clicks`）
    /// 到不了这些状态。
    pub screenshot_drag: Option<(i32, i32, i32, i32)>,
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
    /// 动画全局开关：None=随系统“显示动画”设置；Some(b)=强制开/关。
    pub animations: Option<bool>,
    /// 渲染后端选择。默认 [`Renderer::Software`]。
    pub renderer: Renderer,
    /// 窗口最小客户区尺寸（逻辑 dp，0=不限制）。限制后用户无法把窗口缩到操作不到按钮。
    pub min_width: i32,
    pub min_height: i32,
    /// 单例键（见 `crate::event::WindowRequest::single`）。`None` = 不去重。
    ///
    /// 只对 `ctx.open_window` 开出的子窗有意义：主窗本就唯一，`App` 不提供这个设置。
    pub single: Option<String>,
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
            screenshot_drag: None,
            tray: None,
            hotkeys: Vec::new(),
            start_hidden: false,
            frameless: false,
            animations: None,
            renderer: Renderer::default(),
            min_width: 0,
            min_height: 0,
            single: None,
        }
    }
}

/// 渲染后端的选择方式。
///
/// 两条后端并非替代关系。GPU 路径各平台不同但语义一致：Windows 走 Direct2D
/// （ClearType 子像素混合由 D2D 直接完成，是更正统的一条）；macOS 走 wgpu/Metal
/// （`gpu` feature，几何用 SDF shader、文字仍由 Core Text 光栅、GPU 只合成，
/// 见 `render/gpu`）。软光栅则在没有可用 GPU、或内存紧张时兜底。
///
/// ```no_run
/// # use windui::prelude::*;
/// App::new("demo", 800, 600)
///     .renderer(Renderer::Auto)      // GPU 优先，建不起来自动回退
///     .content(Element::label("hi"))
///     .run();
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Renderer {
    /// GPU 优先，设备建不起来时**自动回退**软件光栅。
    ///
    /// 回退是静默的（只在 stderr 留一行说明），适合发布给最终用户——机器上有没有
    /// 可用 GPU 不该由使用方操心。
    Auto,
    /// 强制软件光栅（tiny-skia）。
    ///
    /// 内存敏感场景用这个：GPU 路径要额外持有 swapchain、设备上下文与若干缓存位图。
    /// 当前的默认值——GPU 路径的验证还在补齐中，默认切换会在后续版本进行。
    #[default]
    Software,
    /// 强制 GPU，设备建不起来时**报错终止**而非回退。
    ///
    /// 用于测试与排障：静默回退会让"我在验证 GPU 行为"这件事失去意义——两张软渲染
    /// 的截图看起来当然一致。要的是"拿不到 GPU 就告诉我"，而不是悄悄换一条路。
    Gpu,
}

impl Renderer {
    /// 是否应当尝试 GPU 后端。
    ///
    /// 调用者：Windows + d2d feature（D2D 后端）与 macOS + gpu feature（wgpu 后端）。
    /// 两者都关掉的平台没有可尝试的 GPU 后端，只有 `requires_gpu` 仍需判断
    /// （`Renderer::Gpu` 在那里无从满足，须报错）。
    #[cfg_attr(
        not(any(
            all(windows, feature = "d2d"),
            all(target_os = "macos", feature = "gpu")
        )),
        allow(dead_code)
    )]
    pub(crate) fn wants_gpu(self) -> bool {
        matches!(self, Renderer::Auto | Renderer::Gpu)
    }

    /// GPU 建不起来时是否必须报错（而非回退软件）。
    pub(crate) fn requires_gpu(self) -> bool {
        matches!(self, Renderer::Gpu)
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
    ///
    /// macOS 无需对应的 OS 调用——`mouseDown:` 之后的 `mouseDragged:`/`mouseUp:` 由 AppKit
    /// 隐式续派发给同一 view（拖出窗口外照送），后端只镜像本值以门控 `on_capture_lost`。
    fn capture_active(&self) -> bool {
        false
    }
    /// OS 抢走指针捕获（Alt+Tab 等）时调用，让逻辑捕获方收尾（如复位拖动态）。
    /// 返回 true 表示需要重绘。win32 由 `WM_CAPTURECHANGED` 触发，macOS 由
    /// `windowDidResignKey:` 触发（切走应用/原生模态框接管时抬起事件不再送达）。
    fn on_capture_lost(&mut self) -> bool {
        false
    }
    /// 设置 DPI 缩放因子（DPI/96）。窗口创建后与 WM_DPICHANGED 时由平台调用。
    fn set_scale(&mut self, _scale: f32) {}

    /// 当前清屏色。`None` = 沿用 [`WindowConfig::bg`]（窗口创建时那份）。
    ///
    /// 平台**每帧查询**而不缓存：运行期换主题（[`ThemeHandle::set`](crate::app::ThemeHandle::set)）
    /// 会改变底色，而创建时抄下的那份不会跟着变。此前正是如此——切暗色后控件前景转暗、
    /// 窗口底色仍停在亮色，浅色文字画在浅色底上几乎看不见。
    ///
    /// 构建期的 `App::theme` 那条路不受影响（它当场同步了 `WindowConfig::bg`），本方法
    /// 补的是运行期那条。
    fn bg(&self) -> Option<Color> {
        None
    }

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
    /// 滚动手指下的容器。返回 true 表示需要重绘。**仅 win32 后端调用**（触摸屏拖动滚动）；
    /// macOS 触控板的两指滑动是滚轮事件，走 `PointerKind::Wheel` 而非本方法。
    fn on_pan(&mut self, _pos: Point, _dy: i32) -> bool {
        false
    }

    /// 触摸抬起时按释放速度启动惯性滑动（fling）。`pos` 为**物理像素**（相对客户区）、
    /// `vy` 为手指 y 速度（**物理像素/ms**）。返回 true 表示已启动（平台据此触发首帧）。
    ///
    /// **仅 win32 后端调用**：`WM_TOUCH` 只给位置不给动量，惯性必须自算。macOS 后端
    /// 刻意不调本方法，触控板的动量由系统在 `scrollWheel:` 里续发（见
    /// `platform/macos/window.rs::on_wheel`）——那不是漏实现，别去移植 win32 那套状态机。
    fn start_fling(&mut self, _pos: Point, _vy: f32) -> bool {
        false
    }

    /// 取消进行中的惯性滑动（新触摸按下/点击/滚轮打断时）。返回 true 表示需要重绘。
    /// 同 [`start_fling`](Self::start_fling)，**仅 win32 后端调用**；macOS 的动量由系统
    /// 在用户重新触摸触控板时自行中止，无需框架介入。
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
    ///
    /// 默认实现取 [`crate::app::take_deferred`] 的队列（已废弃的自由函数
    /// `app::defer_blocking` 排入的闭包）——自定义 handler 不覆盖本方法也能让老代码里
    /// 排入的延迟闭包跑起来（覆盖时记得回退到它，见 `UiHost`）。
    /// 走 `ctx.defer_blocking` 的请求不经这条队列，由宿主自己的 `pending_dialog` 交付。
    fn take_dialog_request(&mut self) -> Option<DialogRequest> {
        crate::app::take_deferred()
    }

    /// 取出并清除待创建的子窗口（`EventCtx::open_window` 排入）。平台在事件分发
    /// **完全返回**后调用，与 [`take_window_op`](Self::take_window_op) 同点。
    ///
    /// 交出的是**配置 + 已经建好的宿主**，而不是控件树：宿主的构造需要主题句柄等应用层
    /// 上下文，平台层既拿不到也不该认识它。平台只需按 `WindowConfig` 建窗、把 handler
    /// 挂上去——与 `run` 收到的那一份是同一种东西。
    ///
    /// 返回的 `WindowConfig` 只描述**这一个窗口**：托盘、全局热键、单实例这些应用级配置
    /// 一律为空，平台不得据此重复安装（它们在 `run` 那次已经装好，见 win32 的 `AppHost`）。
    ///
    /// `is_open` 由平台提供，回答"这个单例键（`Window::single`）当前是否已有窗口"。
    /// 判定必须发生在**构建内容之前**，故只能由宿主在这里问平台，而不是平台拿到结果再
    /// 过滤：`WindowRequest::content` 是个闭包，宿主一旦把它跑起来就已经白搭了一整棵
    /// 控件树，闭包里的副作用也跟着执行了一遍。
    fn take_new_windows(&mut self, _is_open: &dyn Fn(&str) -> bool) -> Vec<NewWindow> {
        Vec::new()
    }
}

/// [`AppHandler::take_new_windows`] 的一项：建一个新窗口，或激活已有的同键单例窗口。
pub enum NewWindow {
    /// 建一个新窗口，并把这个 handler 挂上去。
    ///
    /// 配置装箱：`WindowConfig` 三百多字节，而 `Focus` 只有一个 `String`——不装箱的话
    /// 每个 `Focus` 也要按最大变体占位。
    Create(Box<WindowConfig>, Box<dyn AppHandler>),
    /// 已有同键窗口（`Window::single`）：把它激活到前台，本次不建窗。
    ///
    /// 内容闭包**没有被运行**。平台按键去登记表里找那个窗口；找不到就什么都不做
    /// （窗口在判定与执行之间关掉了，属于正常竞态，不是错误）。
    Focus(String),
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
mod renderer_tests {
    use super::*;

    /// 默认必须是软光栅。
    ///
    /// 单独钉住是因为改默认值是一次**行为变更**，会把所有未显式选择的应用一起切到
    /// GPU 上。它该由一次明确的版本决策来做，而不是谁顺手改了 `#[default]` 就生效。
    #[test]
    fn default_is_software() {
        assert_eq!(Renderer::default(), Renderer::Software);
        assert_eq!(WindowConfig::default().renderer, Renderer::Software);
    }

    /// 三个变体在"要不要试 GPU"和"失败能不能回退"两个维度上的取值。
    #[test]
    fn wants_and_requires_gpu_truth_table() {
        assert!(Renderer::Auto.wants_gpu(), "Auto 应尝试 GPU");
        assert!(!Renderer::Auto.requires_gpu(), "Auto 失败应可回退");

        assert!(!Renderer::Software.wants_gpu(), "Software 不应尝试 GPU");
        assert!(!Renderer::Software.requires_gpu());

        assert!(Renderer::Gpu.wants_gpu());
        assert!(
            Renderer::Gpu.requires_gpu(),
            "Gpu 失败必须报错——静默回退会让基于它的验证拿两张软渲染图得出'软硬一致'"
        );
    }
}

#[cfg(test)]
mod child_screenshot_path_tests {
    use super::child_screenshot_path;
    use std::path::PathBuf;

    #[test]
    fn appends_index_before_extension() {
        assert_eq!(
            child_screenshot_path(&PathBuf::from("out.png"), 1),
            PathBuf::from("out-1.png")
        );
        assert_eq!(
            child_screenshot_path(&PathBuf::from("out.png"), 2),
            PathBuf::from("out-2.png")
        );
    }

    /// 目录必须保留：主图与子窗图要落在同一处，视觉回归才好一起比对。
    #[test]
    fn keeps_parent_directory() {
        let got = child_screenshot_path(&PathBuf::from("artifacts/multi.png"), 1);
        assert_eq!(got, PathBuf::from("artifacts/multi-1.png"));
    }

    /// 无扩展名时补 png，而不是产出一个没有后缀的文件。
    #[test]
    fn defaults_extension_to_png() {
        assert_eq!(
            child_screenshot_path(&PathBuf::from("shot"), 3),
            PathBuf::from("shot-3.png")
        );
    }

    /// 文件名里本就带点号时，只在**最后**一个扩展名前插序号。
    #[test]
    fn splits_at_last_extension() {
        assert_eq!(
            child_screenshot_path(&PathBuf::from("v1.2.png"), 1),
            PathBuf::from("v1.2-1.png")
        );
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
