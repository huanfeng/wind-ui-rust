//! Wayland 窗口与事件循环（wayland-client，纯 Rust 协议实现，不链 libwayland）。
//!
//! 与 X11 后端共用 `host.rs` 的宿主簿记（定时器、帧配速、出帧、事件后意图、按键翻译、
//! 边缘命中），这里只管协议。文件划分：
//! - `mod.rs`：连接、窗口生命周期、事件循环、出帧与呈现、事件后收尾。
//! - `events.rs`：各协议对象的事件分发（xdg 外壳、输出、表面、指针、键盘、缓冲、回调）。
//! - `shm.rs`：`wl_shm` 缓冲的簿记与写入。`scale.rs`：HiDPI 换算。`input.rs`：按键重复、
//!   滚轮聚合等纯逻辑。`xkb.rs`：libxkbcommon 封装。`cursor.rs`：光标。
//!
//! # 当前范围（实施计划 Stage 1–2）
//!
//! 已有：建窗、按脏区呈现、`frame` 回调配速、多窗口、窗口操作；指针（含高精度滚轮、双击、
//! 捕获）、键盘（xkbcommon、客户端按键重复）、光标（cursor-shape-v1 / XCursor 主题回退）、
//! HiDPI（fractional-scale-v1 + viewporter / 整数 buffer_scale，运行期跟随）、无边框窗口的
//! 拖动 / 边缘缩放 / 双击最大化。
//!
//! 尚无（入口空操作，必要处记日志，不 panic）：剪贴板、输入法、文件拖入、窗口装饰
//! （无服务端装饰的合成器上窗口没有标题栏）、窗口图标、全局热键、唤起已有窗口、横向滚轮
//! （框架没有横向滚动事件，与 X11 / win32 后端一致丢弃）。协议本身不允许的：应用自定窗口
//! 坐标（`centered` 无效）、查询是否最小化。
//!
//! # 没有同步重入
//!
//! 与 X11 同理：请求只写进发送缓冲，事件只在我们调 `dispatch_pending` 时回调，铁律 6
//! 天然成立。
//!
//! # 帧配速
//!
//! 有动画的窗口在本轮真出了帧时附一个 `wl_surface.frame` 回调，回调到了、且到了控件自报的
//! 下次变化时刻才出下一帧。窗口被遮住 / 最小化时合成器不发回调，动画自然停下（有 1 秒兜底，
//! 见 `FRAME_CALLBACK_TIMEOUT`）。无动画时不请求回调，阻塞在 `poll`，空闲零 CPU。

mod cursor;
mod events;
mod input;
mod scale;
mod shm;
mod xkb;

use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;
use wayland_client::backend::WaylandError;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface,
};
use wayland_client::{Connection, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::WpFractionalScaleV1,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use super::host::{self, ClickTracker, Intervals, LinuxWake, Requests};
use crate::event::{CursorShape, WindowOp};
use crate::geometry::{Color, Rect};
use crate::platform::{AppHandler, NewWindow, WindowConfig};
use cursor::Cursor;
use input::{KeyRepeat, WheelFrame};
use scale::Scale;
use shm::{create_buffer, write_pixels, ShmBuffer, ShmSlots};
use xkb::Xkb;

/// 动画帧之间至少隔多久（ms）。刷新率由 `frame` 回调配速——回调本就按显示器的节拍来，
/// 再叠一道 `host::FRAME_MS` 会让「回调到了但还差一点点到 16ms」错过一个垂直同步、
/// 实测 60Hz 输出上只剩 40fps。这里只留一个防失控的下限（合成器立即回调时不至于空转），
/// 取 4ms 以容下 240Hz 显示器。
const MIN_FRAME_GAP_MS: u64 = 4;

/// `frame` 回调迟迟不回（合成器对没有新缓冲的提交不回、或把被遮挡窗口的回调扣住）时，
/// 多久后当它丢了、按普通定时继续出帧。代价是被遮挡的动画窗口每秒醒一次；不设的话，
/// 丢一次回调动画就永远停住。
const FRAME_CALLBACK_TIMEOUT: Duration = Duration::from_secs(1);

/// 无边框窗口的缩放边宽（逻辑像素），与 X11 后端同值。
const RESIZE_BORDER: f64 = 6.0;

/// 动画窗口两帧的最小间隔：控件自报的下次变化时刻，夹在 `[MIN_FRAME_GAP_MS, MAX_FRAME_DELAY_MS]`。
fn anim_gap(ask_ms: u64) -> Duration {
    Duration::from_millis(ask_ms.clamp(MIN_FRAME_GAP_MS, host::MAX_FRAME_DELAY_MS))
}

/// 已连上合成器、绑好全局对象的会话。
pub(super) struct Session {
    conn: Connection,
    queue: EventQueue<Wl>,
    globals: Globals,
}

/// 绑定到的全局对象。前三个必需，其余缺了就走回退路径。
struct Globals {
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    wm_base: xdg_wm_base::XdgWmBase,
    seat: Option<wl_seat::WlSeat>,
    viewporter: Option<WpViewporter>,
    fractional: Option<WpFractionalScaleManagerV1>,
    cursor_shape: Option<WpCursorShapeManagerV1>,
    outputs: Vec<Output>,
}

/// 一块显示器。`name` 是 registry 里的全局编号（热拔时凭它删）。
struct Output {
    name: u32,
    obj: wl_output::WlOutput,
    scale: i32,
}

/// 连接合成器（`WAYLAND_DISPLAY` / `WAYLAND_SOCKET`）并绑定全局对象。
///
/// 必需的（`wl_compositor` v4+、`wl_shm`、`xdg_wm_base`）任一缺失返回原因，由调用方回退 X11。
pub(super) fn connect() -> Result<Session, String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("连接合成器失败：{e}"))?;
    let (globals, queue) =
        registry_queue_init::<Wl>(&conn).map_err(|e| format!("读取全局对象失败：{e}"))?;
    let qh = queue.handle();
    // v4 起才有 `wl_surface.damage_buffer`（按缓冲像素报脏区）；v6 有 preferred_buffer_scale。
    let compositor = globals
        .bind(&qh, 4..=6, ())
        .map_err(|e| format!("wl_compositor（需要 v4+）：{e}"))?;
    let shm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wl_shm：{e}"))?;
    // v5 起有 wm_capabilities。
    let wm_base = globals
        .bind(&qh, 1..=5, ())
        .map_err(|e| format!("xdg_wm_base：{e}"))?;
    // v8 起有 axis_value120（高精度滚轮）；v5 起有 pointer frame。
    let seat = globals.bind(&qh, 1..=8, ()).ok();
    // 诊断开关：`WINDUI_WAYLAND_DISABLE=viewporter,fractional-scale,cursor-shape`（逗号分隔）
    // 假装合成器没有这些协议，在新合成器上也能走一遍回退路径（GNOME 42 就三个都没有）。
    let disabled = std::env::var("WINDUI_WAYLAND_DISABLE").unwrap_or_default();
    let off = |name: &str| disabled.split(',').any(|d| d.trim() == name);
    let viewporter = globals
        .bind(&qh, 1..=1, ())
        .ok()
        .filter(|_| !off("viewporter"));
    let fractional = globals
        .bind(&qh, 1..=1, ())
        .ok()
        .filter(|_| !off("fractional-scale"));
    let cursor_shape = globals
        .bind(&qh, 1..=1, ())
        .ok()
        .filter(|_| !off("cursor-shape"));
    let registry = globals.registry().clone();
    let outputs = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == wl_output::WlOutput::interface().name)
            .map(|g| Output {
                name: g.name,
                obj: registry.bind(g.name, g.version.min(4), &qh, g.name),
                scale: 1,
            })
            .collect()
    });
    Ok(Session {
        conn,
        queue,
        globals: Globals {
            compositor,
            shm,
            wm_base,
            seat,
            viewporter,
            fractional,
            cursor_shape,
            outputs,
        },
    })
}

/// 在已连上的会话上建主窗口并运行，阻塞至最后一个窗口关闭。
pub(super) fn run_windowed(
    session: Session,
    mut cfg: WindowConfig,
    handler: Box<dyn AppHandler>,
    waker: Option<std::sync::Arc<crate::sync::WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
) {
    let Session {
        conn,
        mut queue,
        globals,
    } = session;
    if cfg.tray.is_some() {
        log::warn!("Linux 后端暂不支持系统托盘，托盘配置被忽略");
    }
    if cfg.renderer.requires_gpu() {
        eprintln!("[windui] Renderer::Gpu 在 Linux 后端尚不可用，改用软件渲染");
    }
    if !std::mem::take(&mut cfg.hotkeys).is_empty() {
        log::warn!(
            "Wayland 不允许应用抓取全局按键，全局热键未注册；可在桌面设置里把快捷键绑到 \
             `应用 --参数`，经单实例转发送达（见 docs/LINUX_PORTING.md）"
        );
    }
    let forced_scale = std::env::var("WINDUI_SCALE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0);
    let mut wl = Wl {
        qh: queue.handle(),
        conn,
        g: globals,
        forced_scale,
        windows: Vec::new(),
        next_key: 1,
        main: 0,
        upload_buf: Vec::new(),
        hotkey_warned: false,
        pointer: None,
        keyboard: None,
        alt_down: false,
    };
    // 先把输出的 scale 事件收进来，首帧就能按正确缩放画（少一次重排）。
    let _ = queue.roundtrip(&mut wl);
    let main = wl.create_window(&cfg, handler, None);
    wl.main = main;
    if cfg.start_hidden {
        if let Some(i) = wl.idx(main) {
            wl.windows[i].hidden = true;
        }
    } else {
        wl.show(main);
    }
    if let Some(w) = &waker {
        w.bind(Box::new(LinuxWake));
    }
    if let Some(si) = single {
        crate::single_instance::install_listener(&si.app_id, main as isize, si.on_second);
    }
    wl.run_loop(&mut queue);
}

// ── 窗口 ─────────────────────────────────────────────────────────────────

/// 窗口的 xdg 角色对象。隐藏时整个销毁、显示时重建：「挂空缓冲取消映射、再做一次无缓冲
/// 提交」按协议也能重新映射，但 weston 13 对第二次首提交不回 configure，窗口就再也出不来；
/// 换一套角色对象在各家合成器上都成立。
struct Role {
    xdg: xdg_surface::XdgSurface,
    top: xdg_toplevel::XdgToplevel,
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct TopState {
    maximized: bool,
    activated: bool,
}

/// `xdg_toplevel.wm_capabilities`（v5）：合成器支持哪些窗口操作。没收到时按全支持。
#[derive(Clone, Copy, PartialEq, Debug)]
struct WmCaps {
    maximize: bool,
    minimize: bool,
}

impl Default for WmCaps {
    fn default() -> Self {
        Self {
            maximize: true,
            minimize: true,
        }
    }
}

/// 无边框窗口上按下、尚未移出阈值的待定拖动（见 [`Win::pending_drag`]）。
#[derive(Clone, Copy, Debug)]
struct PendingDrag {
    /// `None` = 移动；`Some(边)` = 缩放。
    edge: Option<xdg_toplevel::ResizeEdge>,
    /// 按下点（物理像素，表面内坐标）。
    at: (i32, i32),
    /// 按下事件的 serial：`move` / `resize` 必须带它，合成器凭此认定这是用户发起的。
    serial: u32,
}

/// 每个窗口的状态。`key` 是本后端自编的窗口号，挂在协议对象的 user data 上认窗。
struct Win {
    key: u32,
    surface: wl_surface::WlSurface,
    /// `None` = 未显示或已隐藏（没有角色，合成器眼里它不是窗口）。
    role: Option<Role>,
    /// 当前标题：重建角色时要重设。
    title: String,
    /// 尺寸约束（逻辑像素），重建角色时重设。
    min_size: Option<(i32, i32)>,
    max_size: Option<(i32, i32)>,
    /// 逻辑尺寸（表面坐标）：configure 给的就是它。
    logical: (i32, i32),
    /// 非最大化时的逻辑尺寸：合成器还原时给 0×0（「你自己定」），就回到它。
    floating: (i32, i32),
    handler: Box<dyn AppHandler>,
    bg: Color,
    resizable: bool,
    frameless: bool,
    single: Option<String>,
    owner: Option<u32>,
    modal: bool,
    // ── 缩放 ──
    scale: Scale,
    /// 分数缩放对象报来的首选缩放（×120）。
    fractional120: Option<u32>,
    /// `wl_surface.preferred_buffer_scale`（v6）。
    preferred_int: Option<i32>,
    /// 表面当前所在的输出（`wl_surface.enter` / `leave`）。
    outputs: Vec<wl_output::WlOutput>,
    fractional: Option<WpFractionalScaleV1>,
    viewport: Option<WpViewport>,
    /// 已经随提交生效的 buffer_scale / viewport 目标：只在挂新尺寸缓冲的那次提交里改，
    /// 免得旧缓冲配上新缩放（尺寸不是整数倍是协议错误）。
    applied_buffer_scale: i32,
    applied_dest: Option<(i32, i32)>,
    /// 物理像素尺寸（= 逻辑尺寸按 `scale` 取整）。
    w: i32,
    h: i32,
    pixmap: Option<Pixmap>,
    /// 缓冲刚重建：宿主本帧必须画整窗。
    fresh: bool,
    needs_paint: bool,
    /// 已画进 pixmap、尚未送上屏的区域（拿不到空闲缓冲时留到 `release` 之后）。
    unpresented: Option<Rect>,
    /// 回了 `ack_configure`、还欠一次 `commit`。
    must_commit: bool,
    /// 当前角色收到过 configure，可以挂缓冲了。
    configured: bool,
    /// `xdg_toplevel.configure` 带来、待 `xdg_surface.configure` 一并生效的（逻辑尺寸，状态）。
    pending: Option<((i32, i32), TopState)>,
    state: TopState,
    caps: WmCaps,
    /// 被我们隐藏（`WindowOp::Hide`、启动即隐藏）。只有从这个状态唤起才发 `on_window_shown`。
    hidden: bool,
    /// 等着的 `frame` 回调及其发出时刻；按对象身份认，旧角色 / 旧提交的回调不作数。
    frame_cb: Option<(wl_callback::WlCallback, Instant)>,
    last_anim: Instant,
    intervals: Intervals,
    cursor: CursorShape,
    slots: ShmSlots,
    bufs: Vec<Option<ShmBuffer>>,
    // ── 指针 ──
    click: ClickTracker,
    capturing: bool,
    /// 无边框窗口：在拖动区 / 缩放边上按下、尚未移动够阈值的待定拖动。
    ///
    /// 与 X11 后端同样「移出阈值才交给合成器」，理由在 Wayland 上换了个形式但依然成立：
    /// `xdg_toplevel.move` 一发，合成器立即接管指针，这次按下配对的松开不再送给我们；只是
    /// 点一下标题栏（没想拖）也会被当成一次移动，而双击标题栏的第二下要靠我们自己的点击
    /// 计数认出来——两下都被合成器吞掉的话就凑不齐。等移出阈值再发，单击 / 双击的按下与
    /// 松开都完整留在客户端。按下的 serial 在按住期间一直有效，晚一点发不影响合成器认可。
    pending_drag: Option<PendingDrag>,
    /// 这一次左键按下被标题栏 / 缩放边接管了，配对的松开也不下发给控件。
    swallow_up: bool,
}

impl Win {
    fn visible(&self) -> bool {
        self.configured && !self.hidden
    }

    fn window_state(&self) -> crate::event::WindowState {
        crate::event::WindowState {
            maximized: self.state.maximized,
            // Wayland 不告诉客户端自己是否被最小化。
            minimized: false,
            visible: !self.hidden,
            maximizable: self.resizable && self.caps.maximize,
            minimizable: self.caps.minimize,
        }
    }
}

/// 指针设备及其跨事件状态。
struct Ptr {
    obj: wl_pointer::WlPointer,
    cursor: Cursor,
    /// 指针所在窗口。
    focus: Option<u32>,
    /// 最近一次 `enter` 的 serial（设光标要用）。
    enter_serial: u32,
    /// 表面坐标（逻辑，带小数）。
    pos: (f64, f64),
    wheel: WheelFrame,
    /// 按着的按钮数（隐式抓取期间指针事件仍送给按下时的表面）。
    pressed: u32,
}

/// 键盘设备及其跨事件状态。
struct Kbd {
    obj: wl_keyboard::WlKeyboard,
    xkb: Option<Xkb>,
    focus: Option<u32>,
    repeat: KeyRepeat,
    /// 没有 libxkbcommon 时只提示一次。
    warned: bool,
}

struct Wl {
    qh: QueueHandle<Wl>,
    conn: Connection,
    g: Globals,
    /// `WINDUI_SCALE`：强制缩放，盖过合成器的建议。
    forced_scale: Option<f64>,
    windows: Vec<Win>,
    next_key: u32,
    /// 主窗口：单实例转发的「唤出窗口」指的是它。
    main: u32,
    /// 上屏转换缓冲（复用，容量有上限，见 `shm.rs`）。
    upload_buf: Vec<u8>,
    hotkey_warned: bool,
    pointer: Option<Ptr>,
    keyboard: Option<Kbd>,
    /// Alt 是否已按着（`host::translate_key` 用）。
    alt_down: bool,
}

impl Wl {
    fn idx(&self, key: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.key == key)
    }

    fn idx_of_surface(&self, s: &wl_surface::WlSurface) -> Option<usize> {
        self.windows.iter().position(|w| &w.surface == s)
    }

    // ── 建窗 / 显隐 / 关窗 ────────────────────────────────────────────────

    fn create_window(
        &mut self,
        cfg: &WindowConfig,
        mut handler: Box<dyn AppHandler>,
        owner: Option<u32>,
    ) -> u32 {
        let key = self.next_key;
        self.next_key += 1;
        let surface = self.g.compositor.create_surface(&self.qh, key);
        // 分数缩放对象一建好合成器就会报首选缩放；没有 viewporter 就算报了也用不上，不建。
        let fractional = self
            .g
            .fractional
            .as_ref()
            .filter(|_| self.g.viewporter.is_some())
            .map(|m| m.get_fractional_scale(&surface, &self.qh, key));
        // 尺寸约束按表面坐标（逻辑像素）给；不可缩放即最小 = 最大 = 当前。
        let (min_size, max_size) = if !cfg.resizable {
            (Some((cfg.width, cfg.height)), Some((cfg.width, cfg.height)))
        } else if cfg.min_width > 0 || cfg.min_height > 0 {
            (Some((cfg.min_width.max(0), cfg.min_height.max(0))), None)
        } else {
            (None, None)
        };
        if cfg.frameless {
            log::debug!("Wayland 后端尚未协商装饰（xdg-decoration），frameless 按合成器默认处理");
        }
        if cfg.icon.is_some() {
            log::debug!("Wayland 后端尚未接窗口图标（xdg-toplevel-icon），图标被忽略");
        }
        // 首帧的缩放只能猜：强制值，否则只有一块屏时用它的 scale，都没有按 1。
        // 进了输出 / 收到首选缩放后按实际值重排（`update_scale`）。
        let guess = scale::pick(scale::Sources {
            forced: self.forced_scale,
            outputs_max: (self.g.outputs.len() == 1).then(|| self.g.outputs[0].scale),
            viewporter: self.g.viewporter.is_some(),
            ..Default::default()
        });
        handler.set_scale(guess.factor as f32);
        let intervals = Intervals::new(handler.intervals(), Instant::now());
        let logical = (cfg.width.max(1), cfg.height.max(1));
        self.windows.push(Win {
            key,
            surface,
            role: None,
            title: cfg.title.clone(),
            min_size,
            max_size,
            logical,
            floating: logical,
            handler,
            bg: cfg.bg,
            resizable: cfg.resizable,
            frameless: cfg.frameless,
            single: cfg.single.clone(),
            owner,
            modal: cfg.modal && owner.is_some(),
            scale: guess,
            fractional120: None,
            preferred_int: None,
            outputs: Vec::new(),
            fractional,
            viewport: None,
            applied_buffer_scale: 1,
            applied_dest: None,
            w: guess.to_physical(logical.0),
            h: guess.to_physical(logical.1),
            pixmap: None,
            fresh: true,
            needs_paint: true,
            unpresented: None,
            must_commit: false,
            configured: false,
            pending: None,
            state: TopState::default(),
            caps: WmCaps::default(),
            hidden: false,
            frame_cb: None,
            last_anim: Instant::now(),
            intervals,
            cursor: CursorShape::Arrow,
            slots: ShmSlots::default(),
            bufs: Vec::new(),
            click: ClickTracker::default(),
            capturing: false,
            pending_drag: None,
            swallow_up: false,
        });
        if let Some(i) = self.idx(key) {
            self.ensure_viewport(i);
        }
        key
    }

    /// viewport 模式需要一个 `wp_viewport`；懒建，建了就留着（切回整数缩放时清目标即可）。
    fn ensure_viewport(&mut self, i: usize) {
        let w = &mut self.windows[i];
        if w.scale.viewport && w.viewport.is_none() {
            if let Some(vp) = &self.g.viewporter {
                w.viewport = Some(vp.get_viewport(&w.surface, &self.qh, ()));
            }
        }
    }

    /// 按窗口当前的缩放来源重算缩放；变了就通知宿主、重算物理尺寸、整窗重画。
    fn update_scale(&mut self, i: usize) {
        let w = &self.windows[i];
        let outputs_max = w
            .outputs
            .iter()
            .filter_map(|o| self.g.outputs.iter().find(|x| &x.obj == o))
            .map(|o| o.scale)
            .max();
        let s = scale::pick(scale::Sources {
            forced: self.forced_scale,
            fractional120: w.fractional120,
            preferred_int: w.preferred_int,
            outputs_max,
            viewporter: self.g.viewporter.is_some(),
        });
        if s == w.scale {
            return;
        }
        let w = &mut self.windows[i];
        log::debug!("窗口 {} 缩放 {:?} → {:?}", w.key, w.scale, s);
        w.scale = s;
        w.handler.set_scale(s.factor as f32);
        w.w = s.to_physical(w.logical.0);
        w.h = s.to_physical(w.logical.1);
        w.needs_paint = true;
        w.fresh = true;
        self.ensure_viewport(i);
        // 光标也要按新缩放取图。
        let key = self.windows[i].key;
        if self.pointer.as_ref().is_some_and(|p| p.focus == Some(key)) {
            if let Some(p) = self.pointer.as_mut() {
                p.cursor.invalidate();
            }
            self.apply_cursor();
        }
    }

    fn show(&mut self, key: u32) {
        let Some(i) = self.idx(key) else { return };
        let parent = self.windows[i]
            .owner
            .and_then(|o| self.idx(o))
            .and_then(|o| self.windows[o].role.as_ref())
            .map(|r| r.top.clone());
        let w = &mut self.windows[i];
        if std::mem::take(&mut w.hidden) && w.handler.on_window_shown() {
            w.needs_paint = true;
        }
        if w.role.is_some() {
            log::debug!("Wayland 下唤起已显示的窗口需要 xdg-activation，尚未实现");
            return;
        }
        let xdg = self.g.wm_base.get_xdg_surface(&w.surface, &self.qh, key);
        let top = xdg.get_toplevel(&self.qh, key);
        top.set_title(w.title.clone());
        top.set_app_id(host::exe_name());
        if let Some(p) = &parent {
            top.set_parent(Some(p));
        }
        if let Some((mw, mh)) = w.min_size {
            top.set_min_size(mw, mh);
        }
        if let Some((mw, mh)) = w.max_size {
            top.set_max_size(mw, mh);
        }
        // 无缓冲的首次提交：合成器据此回第一个 configure，之后才能挂缓冲。
        w.surface.commit();
        // 从属窗口的 parent 指着旧角色（本窗隐藏时已销毁）：换成新的。
        for c in &self.windows {
            if c.owner == Some(key) {
                if let Some(r) = &c.role {
                    r.top.set_parent(Some(&top));
                }
            }
        }
        self.windows[i].role = Some(Role { xdg, top });
    }

    fn hide(&mut self, key: u32) {
        let Some(i) = self.idx(key) else { return };
        let w = &mut self.windows[i];
        w.hidden = true;
        if let Some(role) = w.role.take() {
            // 销毁角色即取消映射。再建角色前表面上不能挂着缓冲（协议错误），这里一并卸下。
            role.top.destroy();
            role.xdg.destroy();
            w.surface.attach(None, 0, 0);
            w.surface.commit();
            w.configured = false;
            w.frame_cb = None;
            w.pending = None;
            w.pending_drag = None;
            // 隐藏期间不留共享内存缓冲（memfd 不计入本进程 RSS，但照样占系统内存）。
            // 合成器可能还持有其中一块：协议允许先销毁，存储由它自己的映射撑到用完。
            for b in w.bufs.drain(..).flatten() {
                b.destroy();
            }
            w.slots = ShmSlots::default();
            // 角色没了，合成器不会再报失活；这里自己补上。
            if std::mem::take(&mut w.state).activated {
                self.deactivated(key);
            }
        }
    }

    fn close_window(&mut self, key: u32) {
        // 从属窗口随主人一起关。
        let owned: Vec<u32> = self
            .windows
            .iter()
            .filter(|w| w.owner == Some(key))
            .map(|w| w.key)
            .collect();
        for o in owned {
            self.close_window(o);
        }
        let Some(i) = self.idx(key) else { return };
        if let Some(p) = self.pointer.as_mut().filter(|p| p.focus == Some(key)) {
            p.focus = None;
        }
        if let Some(k) = self.keyboard.as_mut().filter(|k| k.focus == Some(key)) {
            k.focus = None;
            k.repeat.stop();
        }
        // handler 随 Win 一起 drop：宿主状态、渲染资源在此归还。
        let w = self.windows.remove(i);
        // 角色 → 表面 → 缓冲：缓冲销毁时已不挂在任何表面上。
        if let Some(role) = w.role {
            role.top.destroy();
            role.xdg.destroy();
        }
        if let Some(f) = w.fractional {
            f.destroy();
        }
        if let Some(v) = w.viewport {
            v.destroy();
        }
        w.surface.destroy();
        for b in w.bufs.into_iter().flatten() {
            b.destroy();
        }
    }

    fn request_close(&mut self, key: u32) {
        let Some(i) = self.idx(key) else { return };
        // 模态子窗开着：owner 的关闭不生效（理由同 X11 后端）。
        if self.blocked_by_modal(key).is_some() {
            return;
        }
        let allow = {
            let _g = crate::platform::EventDispatchGuard::enter();
            self.windows[i].handler.on_close_request()
        };
        if allow {
            self.close_window(key);
        } else {
            self.windows[i].needs_paint = true;
            self.after_event(key);
        }
    }

    fn blocked_by_modal(&self, key: u32) -> Option<u32> {
        self.windows
            .iter()
            .find(|w| w.modal && w.owner == Some(key))
            .map(|w| w.key)
    }

    fn apply_window_op(&mut self, key: u32, op: WindowOp) {
        let Some(i) = self.idx(key) else { return };
        let w = &self.windows[i];
        match op {
            WindowOp::Show => return self.show(key),
            WindowOp::Hide => return self.hide(key),
            WindowOp::Restore if w.role.is_none() => return self.show(key),
            _ => {}
        }
        // 以下都要作用在已显示的窗口上；隐藏着的窗口没有角色，请求无从发出，照常忽略。
        let Some(top) = w.role.as_ref().map(|r| &r.top) else {
            return;
        };
        match op {
            WindowOp::Minimize if w.caps.minimize => top.set_minimized(),
            WindowOp::Minimize => log::debug!("合成器不支持最小化（wm_capabilities）"),
            WindowOp::Maximize | WindowOp::ToggleMaximize if !w.caps.maximize => {
                log::debug!("合成器不支持最大化（wm_capabilities）")
            }
            WindowOp::Maximize => top.set_maximized(),
            WindowOp::ToggleMaximize if w.state.maximized => top.unset_maximized(),
            WindowOp::ToggleMaximize => top.set_maximized(),
            WindowOp::Restore => top.unset_maximized(),
            WindowOp::Show | WindowOp::Hide => {}
        }
    }

    /// 窗口失活（`xdg_toplevel` 的 activated 状态消失、或被我们隐藏）：通知宿主，收掉
    /// 这扇窗口上进行中的指针捕获（拖到一半 Alt+Tab 走，松开再也不会来），复位 Alt。
    fn deactivated(&mut self, key: u32) {
        let Some(i) = self.idx(key) else { return };
        self.alt_down = false;
        let w = &mut self.windows[i];
        let _g = crate::platform::EventDispatchGuard::enter();
        let mut r = w.handler.on_window_activated(false);
        if std::mem::take(&mut w.capturing) {
            r |= w.handler.on_capture_lost();
        }
        if r {
            w.needs_paint = true;
        }
    }

    // ── configure ─────────────────────────────────────────────────────────

    /// `xdg_surface.configure`：回 ack，把随之而来的 toplevel 尺寸 / 状态一并生效。
    fn on_configure(&mut self, key: u32, xdg: &xdg_surface::XdgSurface, serial: u32) {
        let Some(i) = self.idx(key) else { return };
        let w = &mut self.windows[i];
        // 只认当前角色：隐藏再显示换过一套角色对象，旧的那套在队列里残留的事件要丢掉，
        // 拿新角色去 ack 旧 serial 是协议错误。
        let Some(role) = w.role.as_ref().filter(|r| &r.xdg == xdg) else {
            return;
        };
        role.xdg.ack_configure(serial);
        let first = !std::mem::replace(&mut w.configured, true);
        w.must_commit = true;
        w.needs_paint = true;
        let Some(((lw, lh), st)) = w.pending.take() else {
            return;
        };
        // 给了尺寸就服从；0 = 合成器不指定，由我们定——回到非最大化时的尺寸。
        let logical = if lw > 0 && lh > 0 {
            (lw, lh)
        } else {
            w.floating
        };
        w.logical = logical;
        w.w = w.scale.to_physical(logical.0);
        w.h = w.scale.to_physical(logical.1);
        if !st.maximized {
            w.floating = logical;
        }
        let old = std::mem::replace(&mut w.state, st);
        if old.maximized != st.maximized || first {
            w.handler.on_window_state(w.window_state());
        }
        if old.activated != st.activated {
            if st.activated {
                let _g = crate::platform::EventDispatchGuard::enter();
                // 本函数开头已置 `needs_paint`，宿主回调的「要不要重画」不必再看。
                w.handler.on_window_activated(true);
            } else {
                self.deactivated(key);
            }
        }
        self.after_event(key);
    }

    // ── 事件循环 ──────────────────────────────────────────────────────────

    fn run_loop(&mut self, queue: &mut EventQueue<Wl>) {
        let pipe = super::sys::pipe();
        loop {
            if let Err(e) = queue.dispatch_pending(self) {
                log::error!("Wayland 事件分发失败：{e}");
                eprintln!("[windui] Wayland 事件分发失败，退出：{e}");
                return;
            }
            // 跨线程唤醒：后台消息、单实例转发的 argv。
            if pipe.is_some_and(|p| p.drain()) {
                if crate::single_instance::run_pending_on_main() && self.idx(self.main).is_some() {
                    self.show(self.main);
                }
                for w in &mut self.windows {
                    w.needs_paint = true;
                }
            }
            let timeout = self.tick_timers();
            self.paint_dirty();
            if self.windows.is_empty() {
                let _ = queue.flush();
                return;
            }
            // 发送缓冲满时请求留在库里：这一轮连带等连接可写，否则合成器若正等着这些请求
            // （等 commit 才回 configure / done），双方会互相干等。
            let mut unsent = false;
            match queue.flush() {
                Ok(()) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    unsent = true;
                }
                Err(e) => {
                    eprintln!("[windui] Wayland 连接断开，退出：{e}");
                    return;
                }
            }
            // 队列里已有未分发的事件（出帧期间读进来的）：不睡，回到开头分发。
            let Some(guard) = queue.prepare_read() else {
                continue;
            };
            let any_dirty = self.windows.iter().any(|w| w.needs_paint && w.visible());
            let timeout = if any_dirty {
                Some(Duration::ZERO)
            } else {
                timeout
            };
            let wl_fd = guard.connection_fd().as_raw_fd();
            let mut fds = vec![wl_fd];
            if let Some(p) = pipe {
                fds.push(p.read.as_raw_fd());
            }
            super::sys::wait_io(&fds, unsent.then_some(wl_fd), timeout);
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => {
                    eprintln!("[windui] Wayland 连接断开，退出：{e}");
                    return;
                }
            }
        }
    }

    /// 触发到期的 interval、按键重复与动画帧，返回距下一个截止的时长（`None` = 无定时需求）。
    fn tick_timers(&mut self) -> Option<Duration> {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        let mut at = |t: Instant| next = Some(next.map_or(t, |n: Instant| n.min(t)));
        // 记窗口号而不是下标：回调可能关窗，`Vec::remove` 之后下标整体前移。
        let mut fired: Vec<(u32, usize)> = Vec::new();
        let mut iv_next = None;
        for w in self.windows.iter_mut() {
            let key = w.key;
            fired.extend(
                w.intervals
                    .fire_due(now, &mut iv_next)
                    .into_iter()
                    .map(|idx| (key, idx)),
            );
        }
        if let Some(t) = iv_next {
            at(t);
        }
        for (key, idx) in fired {
            let Some(wi) = self.idx(key) else { continue };
            let r = {
                let _g = crate::platform::EventDispatchGuard::enter();
                self.windows[wi].handler.on_interval_fired(idx)
            };
            if r {
                self.windows[wi].needs_paint = true;
            }
            self.after_event(key);
        }

        // 按键重复：客户端自己计时（Wayland 服务端不发重复事件）。
        let repeat = self.keyboard.as_mut().and_then(|k| k.repeat.due(now));
        if let Some(kc) = repeat {
            self.on_key_code(kc, true, true);
        }
        if let Some(t) = self.keyboard.as_ref().and_then(|k| k.repeat.deadline()) {
            at(t);
        }

        // 动画：可见、在请求续帧、上一帧的 frame 回调已回来（或超时当丢了），且上一帧已经
        // 送上屏（缓冲都被合成器占着时再画也无处呈现，等 release）。
        for w in &mut self.windows {
            if !w.visible() || !w.handler.wants_animation() {
                continue;
            }
            if let Some((_, since)) = &w.frame_cb {
                if since.elapsed() < FRAME_CALLBACK_TIMEOUT {
                    at(*since + FRAME_CALLBACK_TIMEOUT);
                    continue;
                }
                w.frame_cb = None;
            }
            if w.unpresented.is_some_and(|u| !u.is_empty()) {
                continue;
            }
            let due = anim_gap(w.handler.next_frame_delay_ms() as u64);
            let elapsed = w.last_anim.elapsed();
            if elapsed >= due {
                w.last_anim = now;
                w.needs_paint = true;
            } else {
                at(now + (due - elapsed));
            }
        }
        next.map(|n| n.saturating_duration_since(Instant::now()))
    }

    fn paint_dirty(&mut self) {
        // 按窗口号迭代：`after_event` 可能关窗、开窗，下标会漂。
        let keys: Vec<u32> = self.windows.iter().map(|w| w.key).collect();
        for key in keys {
            let Some(i) = self.idx(key) else { continue };
            let w = &mut self.windows[i];
            if !w.visible() {
                continue;
            }
            let painted = w.needs_paint || w.pixmap.is_none();
            if painted {
                w.needs_paint = false;
                if let Some(d) = host::render_frame(
                    w.handler.as_mut(),
                    &mut w.pixmap,
                    &mut w.fresh,
                    (w.w, w.h),
                    w.bg,
                ) {
                    w.unpresented = Some(w.unpresented.map_or(d, |u| u.union(&d)));
                }
                self.after_event(key);
            }
            if let Some(i) = self.idx(key) {
                self.present(i, painted);
            }
        }
    }

    /// 把 pixmap 里未上屏的部分写进一块空闲缓冲并提交。
    ///
    /// `painted`：本轮刚出过帧。只有这时才为动画请求 frame 回调——每轮循环都会来这里，
    /// 不看它的话，报了较长截止时间（光标闪烁 500ms）的窗口会在每个回调之后立刻再要一个，
    /// 退化成按刷新率空提交。
    fn present(&mut self, i: usize, painted: bool) {
        let w = &mut self.windows[i];
        if !w.visible() {
            return;
        }
        let want_frame = painted && w.frame_cb.is_none() && w.handler.wants_animation();
        let damage = match w.unpresented {
            Some(d) if !d.is_empty() => d,
            _ => {
                // 没有新像素，但 ack 过的 configure 要一次提交才生效；动画中的窗口也照样
                // 要 frame 回调——否则这一帧没回调可等，退化成按 `MIN_FRAME_GAP_MS` 空转。
                w.unpresented = None;
                if want_frame {
                    w.frame_cb = Some((w.surface.frame(&self.qh, w.key), Instant::now()));
                }
                if std::mem::take(&mut w.must_commit) || want_frame {
                    w.surface.commit();
                }
                return;
            }
        };
        let Some(pixmap) = w.pixmap.as_ref() else {
            return;
        };
        if pixmap.width() as i32 != w.w || pixmap.height() as i32 != w.h {
            // 尺寸刚变、还没按新尺寸画：等下一次出帧。
            w.needs_paint = true;
            return;
        }
        let Some(plan) = w.slots.plan(w.w, w.h, damage) else {
            return; // 缓冲都在合成器手里，等 release。
        };
        if plan.recreate || w.bufs.get(plan.index).is_none_or(|b| b.is_none()) {
            if w.bufs.len() <= plan.index {
                w.bufs.resize_with(plan.index + 1, || None);
            }
            if let Some(old) = w.bufs[plan.index].take() {
                old.destroy();
            }
            match create_buffer(&self.g.shm, &self.qh, w.key, plan.index, w.w, w.h) {
                Ok(b) => w.bufs[plan.index] = Some(b),
                Err(e) => {
                    log::error!("创建 wl_shm 缓冲失败（{}×{}）：{e}", w.w, w.h);
                    w.slots.fail(plan.index);
                    return;
                }
            }
        }
        let Some(buf) = w.bufs[plan.index].as_ref() else {
            return;
        };
        if let Err(e) = write_pixels(&buf.file, pixmap, plan.write, &mut self.upload_buf) {
            log::error!("写 wl_shm 缓冲失败：{e}");
            w.slots.fail(plan.index);
            return;
        }
        // 缩放随「挂新尺寸缓冲」的这次提交一起生效（见 `applied_buffer_scale`）。
        let bs = w.scale.buffer_scale();
        if bs != w.applied_buffer_scale {
            w.surface.set_buffer_scale(bs);
            w.applied_buffer_scale = bs;
        }
        if let Some(vp) = &w.viewport {
            let dest = w.scale.viewport.then_some(w.logical);
            if dest != w.applied_dest {
                let (dw, dh) = dest.unwrap_or((-1, -1));
                vp.set_destination(dw, dh);
                w.applied_dest = dest;
            }
        }
        let r = plan.write;
        w.surface.attach(Some(&buf.buffer), 0, 0);
        w.surface.damage_buffer(r.x, r.y, r.w, r.h);
        if want_frame {
            w.frame_cb = Some((w.surface.frame(&self.qh, w.key), Instant::now()));
        }
        w.surface.commit();
        w.unpresented = None;
        w.must_commit = false;
    }

    /// 事件分发后的收尾：窗口操作、标题、光标、对话框、开窗、关窗、跨窗口脏。
    fn after_event(&mut self, key: u32) {
        let Some(i) = self.idx(key) else { return };
        let open: Vec<String> = self
            .windows
            .iter()
            .filter_map(|w| w.single.clone())
            .collect();
        let w = &mut self.windows[i];
        let Requests {
            op,
            dialog,
            close,
            title,
            hotkey_ops,
            new_windows,
            cursor,
            ime_caret: _,
        } = Requests::take(w.handler.as_mut(), &|k| open.iter().any(|o| o == k));
        let cursor_changed = std::mem::replace(&mut w.cursor, cursor) != cursor;
        if let Some(t) = title {
            if let Some(r) = &w.role {
                r.top.set_title(t.clone());
            }
            w.title = t;
        }
        if cursor_changed && self.pointer.as_ref().is_some_and(|p| p.focus == Some(key)) {
            self.apply_cursor();
        }
        if !hotkey_ops.is_empty() && !std::mem::replace(&mut self.hotkey_warned, true) {
            log::warn!("Wayland 下全局热键不可用，运行期热键增删被忽略");
        }
        if let Some(op) = op {
            self.apply_window_op(key, op);
        }
        if let Some(req) = dialog {
            // 对话框是阻塞的（门户 / zenity 在自己的进程里跑）。先把已排的请求送出去。
            // 已知缺口：阻塞期间不分发事件，也就不回 `xdg_wm_base.ping`，对话框开久了合成器
            // 可能把本应用标成「无响应」（X11 后端同样不处理 _NET_WM_PING）。
            let _ = self.conn.flush();
            req.run();
            if let Some(i) = self.idx(key) {
                self.windows[i].needs_paint = true;
            }
        }
        for item in new_windows {
            match item {
                NewWindow::Focus(k) => {
                    if let Some(w) = self
                        .windows
                        .iter()
                        .find(|w| w.single.as_deref() == Some(&k))
                    {
                        let wk = w.key;
                        self.show(wk);
                    }
                }
                NewWindow::Create(cfg, handler) => {
                    let owner = (cfg.owned || cfg.modal).then_some(key);
                    let nk = self.create_window(&cfg, handler, owner);
                    self.show(nk);
                }
            }
        }
        if close {
            self.close_window(key);
        }
        if crate::signal::take_cross_window_dirty() {
            for w in &mut self.windows {
                if w.key != key {
                    w.needs_paint = true;
                }
            }
        }
    }

    /// 把指针所在窗口的光标形状设上去。
    fn apply_cursor(&mut self) {
        let Some(p) = self.pointer.as_mut() else {
            return;
        };
        let Some(i) = p
            .focus
            .and_then(|k| self.windows.iter().position(|w| w.key == k))
        else {
            return;
        };
        let w = &self.windows[i];
        let (compositor, qh) = (&self.g.compositor, &self.qh);
        p.cursor.apply(
            &p.obj,
            p.enter_serial,
            w.cursor,
            w.scale.factor,
            &self.conn,
            &self.g.shm,
            || compositor.create_surface(qh, 0),
        );
    }
}

/// `xdg_toplevel.configure` 的状态数组（原生字节序的 u32 列表）。
fn parse_states(raw: &[u8]) -> TopState {
    let mut st = TopState::default();
    for v in u32_list(raw) {
        match xdg_toplevel::State::try_from(v) {
            Ok(xdg_toplevel::State::Maximized) => st.maximized = true,
            Ok(xdg_toplevel::State::Activated) => st.activated = true,
            _ => {}
        }
    }
    st
}

/// `xdg_toplevel.wm_capabilities` 的能力数组。
fn parse_caps(raw: &[u8]) -> WmCaps {
    let mut c = WmCaps {
        maximize: false,
        minimize: false,
    };
    for v in u32_list(raw) {
        match xdg_toplevel::WmCapabilities::try_from(v) {
            Ok(xdg_toplevel::WmCapabilities::Maximize) => c.maximize = true,
            Ok(xdg_toplevel::WmCapabilities::Minimize) => c.minimize = true,
            _ => {}
        }
    }
    c
}

fn u32_list(raw: &[u8]) -> impl Iterator<Item = u32> + '_ {
    raw.as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_ne_bytes(*b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(v: &[u32]) -> Vec<u8> {
        v.iter().flat_map(|v| v.to_ne_bytes()).collect()
    }

    #[test]
    fn toplevel_states_are_decoded() {
        assert_eq!(
            parse_states(&raw(&[4, 1, 3])),
            TopState {
                maximized: true,
                activated: true
            }
        );
        assert_eq!(parse_states(&[]), TopState::default());
    }

    #[test]
    fn wm_capabilities_are_decoded_and_absence_means_unsupported() {
        let c = parse_caps(&raw(&[1, 2, 4]));
        assert!(c.maximize && c.minimize);
        let none = parse_caps(&[]);
        assert!(!none.maximize && !none.minimize, "收到空列表 = 都不支持");
        assert!(WmCaps::default().maximize, "没收到事件（v5 以下）按全支持");
    }
}
