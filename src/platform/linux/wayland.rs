//! Wayland 窗口与事件循环（wayland-client，纯 Rust 协议实现，不链 libwayland）。
//!
//! 与 X11 后端共用 `host.rs` 的宿主簿记（定时器、帧配速、出帧、事件后意图），这里只管
//! 协议：`xdg_toplevel` 建窗、`wl_shm` 呈现、`wl_surface.frame` 配速。
//!
//! # 当前范围（实施计划 Stage 1）
//!
//! 已有：建窗（标题、app_id、最小 / 最大尺寸、父窗）、合成器给定尺寸时服从、按脏区呈现、
//! 动画按 `frame` 回调配速、关窗、多窗口、最大化 / 最小化请求、激活态上报。
//!
//! 尚无（入口空操作，必要处记日志，不 panic）：指针 / 键盘输入、光标形状、HiDPI 跟随
//! （缩放只认 `WINDUI_SCALE` 的整数部分）、剪贴板、输入法、文件拖入、窗口装饰
//! （无服务端装饰的合成器上窗口没有标题栏）、窗口图标、全局热键、唤起已有窗口。
//! 协议本身不允许的：应用自定窗口坐标（`centered` 无效）、查询是否最小化。
//!
//! # 没有同步重入
//!
//! 与 X11 同理：请求只写进发送缓冲，事件只在我们调 `dispatch_pending` 时回调，铁律 6
//! 天然成立。
//!
//! # 呈现
//!
//! tiny-skia 画进每窗一份 `Pixmap`，上屏时把脏区换成 XRGB8888（小端 B,G,R,X）写进
//! `wl_shm` 缓冲。缓冲以 memfd 为底、用 `pwrite` 写入而**不在本进程映射**——合成器映射
//! 同一个 fd 读取，本进程的私有内存里只有 `Pixmap` 那一份。每窗最多两块缓冲：合成器还
//! 占着（未 `release`）的那块不能写，拿不到空闲缓冲时脏区留着，等 `release` 事件再呈现。
//! 每块缓冲记着「自己上次写入以来别的帧改过哪里」，复用时补写这部分，而不是整窗重写。
//!
//! # 帧配速
//!
//! 有动画的窗口提交时附一个 `wl_surface.frame` 回调，回调到了才出下一帧，间隔仍不短于
//! `max(刷新率间隔, 控件自报的下次变化时刻)`。窗口被遮住 / 最小化时合成器不发回调，动画
//! 自然停下。无动画时不请求回调，阻塞在 `poll`，空闲零 CPU。

use std::fs::File;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::fs::FileExt;
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;
use wayland_client::backend::WaylandError;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use super::host::{self, Intervals, LinuxWake, Requests};
use crate::event::{CursorShape, WindowOp};
use crate::geometry::{Color, Rect};
use crate::platform::{AppHandler, NewWindow, WindowConfig};

/// 上屏转换缓冲的上限（字节）：整窗帧分块转换、分块写入，峰值不随窗口变大。
const UPLOAD_CHUNK: usize = 256 * 1024;
/// 每窗的 `wl_shm` 缓冲数。两块足够：一块在合成器手里，一块给下一帧写。
const MAX_BUFFERS: usize = 2;

/// 动画帧之间至少隔多久（ms）。刷新率由 `frame` 回调配速——回调本就按显示器的节拍来，
/// 再叠一道 `host::FRAME_MS` 会让「回调到了但还差一点点到 16ms」错过一个垂直同步、
/// 实测 60Hz 输出上只剩 40fps。这里只留一个防失控的下限（合成器立即回调时不至于空转），
/// 取 4ms 以容下 240Hz 显示器。
const MIN_FRAME_GAP_MS: u64 = 4;

/// 动画窗口两帧的最小间隔：控件自报的下次变化时刻，夹在 `[MIN_FRAME_GAP_MS, MAX_FRAME_DELAY_MS]`。
fn anim_gap(ask_ms: u64) -> Duration {
    Duration::from_millis(ask_ms.clamp(MIN_FRAME_GAP_MS, host::MAX_FRAME_DELAY_MS))
}

/// 已连上合成器、绑好必需全局对象的会话。
pub(super) struct Session {
    conn: Connection,
    queue: EventQueue<Wl>,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    wm_base: xdg_wm_base::XdgWmBase,
}

/// 连接合成器（`WAYLAND_DISPLAY` / `WAYLAND_SOCKET`）并绑定必需的全局对象。
///
/// 任一步失败返回原因，由调用方决定回退 X11 还是报错退出。
pub(super) fn connect() -> Result<Session, String> {
    let conn = Connection::connect_to_env().map_err(|e| format!("连接合成器失败：{e}"))?;
    let (globals, queue) =
        registry_queue_init::<Wl>(&conn).map_err(|e| format!("读取全局对象失败：{e}"))?;
    let qh = queue.handle();
    // v4 起才有 `wl_surface.damage_buffer`（按缓冲像素报脏区，不必折算 buffer_scale）。
    let compositor = globals
        .bind(&qh, 4..=6, ())
        .map_err(|e| format!("wl_compositor（需要 v4+）：{e}"))?;
    let shm = globals
        .bind(&qh, 1..=1, ())
        .map_err(|e| format!("wl_shm：{e}"))?;
    let wm_base = globals
        .bind(&qh, 1..=5, ())
        .map_err(|e| format!("xdg_wm_base：{e}"))?;
    Ok(Session {
        conn,
        queue,
        compositor,
        shm,
        wm_base,
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
        compositor,
        shm,
        wm_base,
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
    let mut wl = Wl {
        qh: queue.handle(),
        conn,
        compositor,
        shm,
        wm_base,
        scale: detect_scale(),
        windows: Vec::new(),
        next_key: 1,
        main: 0,
        upload_buf: Vec::new(),
        hotkey_warned: false,
    };
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

/// 缩放：Stage 1 只认 `WINDUI_SCALE`，取整数（`wl_surface.set_buffer_scale` 只收整数；
/// 分数缩放要 `fractional-scale-v1` + `viewporter`，属于下一阶段）。
fn detect_scale() -> i32 {
    let s = std::env::var("WINDUI_SCALE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(1.0);
    let n = s.round().clamp(1.0, 4.0) as i32;
    if (s - n as f32).abs() > f32::EPSILON {
        log::warn!("Wayland 后端暂只支持整数缩放，WINDUI_SCALE={s} 按 {n} 处理");
    }
    n
}

// ── 缓冲池：纯簿记（可单测），与协议对象分开 ────────────────────────────────

/// 一块缓冲的簿记。
#[derive(Clone, Debug, PartialEq)]
struct SlotState {
    w: i32,
    h: i32,
    /// 已 attach 给合成器、尚未收到 `release`：不能写。
    busy: bool,
    /// 自己上次写入以来，别的帧改过的区域——复用时要补写。
    debt: Option<Rect>,
}

/// 一次呈现的安排：写哪块缓冲、是否要按新尺寸重建、写哪个矩形。
#[derive(Debug, PartialEq)]
struct SlotPlan {
    index: usize,
    recreate: bool,
    write: Rect,
}

#[derive(Default)]
struct ShmSlots {
    slots: Vec<SlotState>,
}

impl ShmSlots {
    /// 为一帧（尺寸 `w×h`，相对上次呈现改了 `damage`）挑一块缓冲。都被合成器占着时返回
    /// `None`，调用方留着脏区，等 `release` 再来。
    ///
    /// 优先复用同尺寸的空闲块（只写 `damage ∪ debt`）；其次重建尺寸不对的空闲块；
    /// 不足 `MAX_BUFFERS` 块时新建。选中后标忙，其余块把本帧脏区记进欠账。
    fn plan(&mut self, w: i32, h: i32, damage: Rect) -> Option<SlotPlan> {
        let full = Rect::new(0, 0, w, h);
        let free_same = self
            .slots
            .iter()
            .position(|s| !s.busy && s.w == w && s.h == h);
        let plan = if let Some(index) = free_same {
            let debt = self.slots[index].debt;
            let write = debt.map_or(damage, |d| d.union(&damage)).intersect(&full);
            SlotPlan {
                index,
                recreate: false,
                write,
            }
        } else if let Some(index) = self.slots.iter().position(|s| !s.busy) {
            SlotPlan {
                index,
                recreate: true,
                write: full,
            }
        } else if self.slots.len() < MAX_BUFFERS {
            self.slots.push(SlotState {
                w,
                h,
                busy: false,
                debt: None,
            });
            SlotPlan {
                index: self.slots.len() - 1,
                recreate: true,
                write: full,
            }
        } else {
            return None;
        };
        for (i, s) in self.slots.iter_mut().enumerate() {
            if i == plan.index {
                *s = SlotState {
                    w,
                    h,
                    busy: true,
                    debt: None,
                };
            } else {
                s.debt = Some(s.debt.map_or(damage, |d| d.union(&damage)));
            }
        }
        Some(plan)
    }

    fn release(&mut self, index: usize) {
        if let Some(s) = self.slots.get_mut(index) {
            s.busy = false;
        }
    }

    /// `plan` 之后建缓冲或写像素失败：这块的内容不可信，作废到「下次必须按新建处理、
    /// 整块重写」。只 `release` 的话它会被当成已写好，下次只补一小块脏区。
    fn fail(&mut self, index: usize) {
        if let Some(s) = self.slots.get_mut(index) {
            *s = SlotState {
                w: 0,
                h: 0,
                busy: false,
                debt: None,
            };
        }
    }
}

/// 一块缓冲的协议对象。memfd 留着给 `pwrite`；池在建完缓冲后即销毁（缓冲自己持有映射）。
struct ShmBuffer {
    file: File,
    buffer: wl_buffer::WlBuffer,
}

impl ShmBuffer {
    fn destroy(self) {
        self.buffer.destroy();
    }
}

// ── 窗口与宿主 ────────────────────────────────────────────────────────────

/// 窗口的 xdg 角色对象。隐藏时整个销毁、显示时重建：「挂空缓冲取消映射、再做一次无缓冲
/// 提交」按协议也能重新映射，但 weston 13 对第二次首提交不回 configure，窗口就再也出不来；
/// 换一套角色对象在各家合成器上都成立。
struct Role {
    xdg: xdg_surface::XdgSurface,
    top: xdg_toplevel::XdgToplevel,
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
    /// 非最大化时的物理尺寸：合成器还原时给 0×0（「你自己定」），就回到它。
    floating: (i32, i32),
    handler: Box<dyn AppHandler>,
    bg: Color,
    resizable: bool,
    single: Option<String>,
    owner: Option<u32>,
    modal: bool,
    /// 物理像素尺寸。
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
    /// 被我们隐藏（`WindowOp::Hide`、启动即隐藏）。只有从这个状态唤起才发 `on_window_shown`。
    hidden: bool,
    /// 已请求 `frame` 回调、尚未收到 `done`。
    frame_pending: bool,
    last_anim: Instant,
    intervals: Intervals,
    cursor: CursorShape,
    slots: ShmSlots,
    bufs: Vec<Option<ShmBuffer>>,
}

#[derive(Clone, Copy, Default, PartialEq, Debug)]
struct TopState {
    maximized: bool,
    activated: bool,
}

impl Win {
    fn visible(&self) -> bool {
        self.configured && !self.hidden
    }
}

struct Wl {
    qh: QueueHandle<Wl>,
    conn: Connection,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    wm_base: xdg_wm_base::XdgWmBase,
    /// 整数缓冲缩放（物理 = 逻辑 × scale）。
    scale: i32,
    windows: Vec<Win>,
    next_key: u32,
    /// 主窗口：单实例转发的「唤出窗口」指的是它。
    main: u32,
    /// 上屏转换缓冲（复用，容量不超过 `UPLOAD_CHUNK`）。
    upload_buf: Vec<u8>,
    hotkey_warned: bool,
}

impl Wl {
    fn idx(&self, key: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.key == key)
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
        let s = self.scale;
        let surface = self.compositor.create_surface(&self.qh, ());
        if s != 1 {
            surface.set_buffer_scale(s);
        }
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

        handler.set_scale(s as f32);
        let intervals = Intervals::new(handler.intervals(), Instant::now());
        // 缓冲尺寸必须是 buffer_scale 的整数倍（协议要求），所以先夹逻辑尺寸再乘。
        let (pw, ph) = (cfg.width.max(1) * s, cfg.height.max(1) * s);
        self.windows.push(Win {
            key,
            surface,
            role: None,
            title: cfg.title.clone(),
            min_size,
            max_size,
            floating: (pw, ph),
            handler,
            bg: cfg.bg,
            resizable: cfg.resizable,
            single: cfg.single.clone(),
            owner,
            modal: cfg.modal && owner.is_some(),
            w: pw,
            h: ph,
            pixmap: None,
            fresh: true,
            needs_paint: true,
            unpresented: None,
            must_commit: false,
            configured: false,
            pending: None,
            state: TopState::default(),
            hidden: false,
            frame_pending: false,
            last_anim: Instant::now(),
            intervals,
            cursor: CursorShape::Arrow,
            slots: ShmSlots::default(),
            bufs: Vec::new(),
        });
        key
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
        let xdg = self.wm_base.get_xdg_surface(&w.surface, &self.qh, key);
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
        w.role = Some(Role { xdg, top });
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
            w.frame_pending = false;
            w.pending = None;
            // 隐藏期间不留共享内存缓冲（memfd 不计入本进程 RSS，但照样占系统内存）。
            // 合成器可能还持有其中一块：协议允许先销毁，存储由它自己的映射撑到用完。
            for b in w.bufs.drain(..).flatten() {
                b.destroy();
            }
            w.slots = ShmSlots::default();
            // 角色没了，合成器不会再报失活；这里自己补上。
            if std::mem::take(&mut w.state).activated {
                let _g = crate::platform::EventDispatchGuard::enter();
                if w.handler.on_window_activated(false) {
                    w.needs_paint = true;
                }
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
        // handler 随 Win 一起 drop：宿主状态、渲染资源在此归还。
        let w = self.windows.remove(i);
        // 角色 → 表面 → 缓冲：缓冲销毁时已不挂在任何表面上。
        if let Some(role) = w.role {
            role.top.destroy();
            role.xdg.destroy();
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
            WindowOp::Minimize => top.set_minimized(),
            WindowOp::Maximize => top.set_maximized(),
            WindowOp::ToggleMaximize if w.state.maximized => top.unset_maximized(),
            WindowOp::ToggleMaximize => top.set_maximized(),
            WindowOp::Restore => top.unset_maximized(),
            WindowOp::Show | WindowOp::Hide => {}
        }
    }

    // ── configure ─────────────────────────────────────────────────────────

    /// `xdg_surface.configure`：回 ack，把随之而来的 toplevel 尺寸 / 状态一并生效。
    fn on_configure(&mut self, key: u32, xdg: &xdg_surface::XdgSurface, serial: u32) {
        let Some(i) = self.idx(key) else { return };
        let s = self.scale;
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
        let (nw, nh) = if lw > 0 && lh > 0 {
            (lw * s, lh * s)
        } else {
            w.floating
        };
        w.w = nw;
        w.h = nh;
        if !st.maximized {
            w.floating = (nw, nh);
        }
        // 本函数开头已置 `needs_paint`，宿主回调的「要不要重画」不必再看。
        let old = std::mem::replace(&mut w.state, st);
        if old.activated != st.activated {
            let _g = crate::platform::EventDispatchGuard::enter();
            w.handler.on_window_activated(st.activated);
        }
        if old.maximized != st.maximized || first {
            w.handler.on_window_state(crate::event::WindowState {
                maximized: st.maximized,
                // Wayland 不告诉客户端自己是否被最小化。
                minimized: false,
                visible: true,
                maximizable: w.resizable,
                minimizable: true,
            });
        }
        self.after_event(key);
    }

    // ── 事件循环 ──────────────────────────────────────────────────────────

    fn run_loop(&mut self, queue: &mut EventQueue<Wl>) {
        let pipe = super::sys::pipe();
        loop {
            if let Err(e) = queue.dispatch_pending(self) {
                log::error!("Wayland 事件分发失败：{e}");
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
                    log::error!("Wayland 连接断开：{e}");
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
                    log::error!("Wayland 连接断开：{e}");
                    return;
                }
            }
        }
    }

    /// 触发到期的 interval 与动画帧，返回距下一个截止的时长（`None` = 无定时需求）。
    fn tick_timers(&mut self) -> Option<Duration> {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        // 记窗口号而不是下标：回调可能关窗，`Vec::remove` 之后下标整体前移。
        let mut fired: Vec<(u32, usize)> = Vec::new();
        for w in self.windows.iter_mut() {
            let key = w.key;
            fired.extend(
                w.intervals
                    .fire_due(now, &mut next)
                    .into_iter()
                    .map(|idx| (key, idx)),
            );
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

        // 动画：可见、在请求续帧、且上一帧的 frame 回调已回来（没回来就等它，不设超时）。
        for w in &mut self.windows {
            if !w.visible() || w.frame_pending || !w.handler.wants_animation() {
                continue;
            }
            let due = anim_gap(w.handler.next_frame_delay_ms() as u64);
            let elapsed = w.last_anim.elapsed();
            if elapsed >= due {
                w.last_anim = now;
                w.needs_paint = true;
            } else {
                let at = now + (due - elapsed);
                next = Some(next.map_or(at, |n| n.min(at)));
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
        let want_frame = painted && !w.frame_pending && w.handler.wants_animation();
        let damage = match w.unpresented {
            Some(d) if !d.is_empty() => d,
            _ => {
                // 没有新像素，但 ack 过的 configure 要一次提交才生效；动画中的窗口也照样
                // 要 frame 回调——否则这一帧没回调可等，退化成按 `MIN_FRAME_GAP_MS` 空转。
                w.unpresented = None;
                if want_frame {
                    w.surface.frame(&self.qh, w.key);
                    w.frame_pending = true;
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
            match create_buffer(&self.shm, &self.qh, w.key, plan.index, w.w, w.h) {
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
        let r = plan.write;
        w.surface.attach(Some(&buf.buffer), 0, 0);
        w.surface.damage_buffer(r.x, r.y, r.w, r.h);
        if want_frame {
            w.surface.frame(&self.qh, w.key);
            w.frame_pending = true;
        }
        w.surface.commit();
        w.unpresented = None;
        w.must_commit = false;
    }

    /// 事件分发后的收尾：窗口操作、标题、对话框、开窗、关窗、跨窗口脏。
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
        if cursor != w.cursor {
            w.cursor = cursor;
            log::trace!("Wayland 后端尚未接光标形状（{cursor:?}）");
        }
        if let Some(t) = title {
            if let Some(r) = &w.role {
                r.top.set_title(t.clone());
            }
            w.title = t;
        }
        if !hotkey_ops.is_empty() && !std::mem::replace(&mut self.hotkey_warned, true) {
            log::warn!("Wayland 下全局热键不可用，运行期热键增删被忽略");
        }
        if let Some(op) = op {
            self.apply_window_op(key, op);
        }
        if let Some(req) = dialog {
            // 对话框是阻塞的（门户 / zenity 在自己的进程里跑）。先把已排的请求送出去。
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
}

fn create_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<Wl>,
    key: u32,
    slot: usize,
    w: i32,
    h: i32,
) -> std::io::Result<ShmBuffer> {
    let stride = w * 4;
    let size = i32::try_from(stride as i64 * h as i64)
        .map_err(|_| std::io::Error::other(format!("缓冲过大（{w}×{h}）")))?;
    let file = super::sys::memfd(c"windui-shm", size as u64)?;
    let pool = shm.create_pool(file.as_fd(), size, qh, ());
    let buffer = pool.create_buffer(0, w, h, stride, wl_shm::Format::Xrgb8888, qh, (key, slot));
    // 池只是建缓冲的中介：缓冲自己持有映射，销毁池不影响它。
    pool.destroy();
    Ok(ShmBuffer { file, buffer })
}

/// 把 pixmap 的 `r` 区域换成 XRGB8888 写进缓冲文件（同尺寸、行距 = 宽 × 4）。
/// 整行宽的区域按块连续写，否则逐行写。
fn write_pixels(file: &File, pm: &Pixmap, r: Rect, buf: &mut Vec<u8>) -> std::io::Result<()> {
    let r = r.intersect(&Rect::new(0, 0, pm.width() as i32, pm.height() as i32));
    if r.is_empty() {
        return Ok(());
    }
    let stride = pm.width() as usize * 4;
    let data = pm.data();
    let row_bytes = r.w as usize * 4;
    let full_rows = row_bytes == stride;
    let rows_per = if full_rows {
        (UPLOAD_CHUNK / row_bytes).max(1)
    } else {
        1
    };
    let mut y = r.y as usize;
    let bottom = r.bottom() as usize;
    while y < bottom {
        let n = rows_per.min(bottom - y);
        buf.clear();
        for row in y..y + n {
            let off = row * stride + r.x as usize * 4;
            host::rgba_to_bgra(&data[off..off + row_bytes], buf);
        }
        file.write_all_at(buf, (y * stride + r.x as usize * 4) as u64)?;
        y += n;
    }
    Ok(())
}

// ── 协议事件 ──────────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Wl {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 全局对象的增删（显示器热插拔等）Stage 1 不跟随。
    }
}

delegate_noop!(Wl: wl_compositor::WlCompositor);
delegate_noop!(Wl: wl_shm_pool::WlShmPool);
// `format` 事件：XRGB8888 是协议规定必支持的格式，不必等它。
delegate_noop!(Wl: ignore wl_shm::WlShm);
// `enter` / `leave` / `preferred_buffer_scale`：HiDPI 跟随属于下一阶段。
delegate_noop!(Wl: ignore wl_surface::WlSurface);

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for Wl {
    fn event(
        _: &mut Self,
        base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, u32> for Wl {
    fn event(
        state: &mut Self,
        xdg: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.on_configure(*key, xdg, serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, u32> for Wl {
    fn event(
        state: &mut Self,
        top: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // 同 `on_configure`：只认当前角色的事件。
        let current = state
            .idx(*key)
            .and_then(|i| state.windows[i].role.as_ref())
            .is_some_and(|r| &r.top == top);
        if !current {
            return;
        }
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                if let Some(i) = state.idx(*key) {
                    state.windows[i].pending = Some(((width, height), parse_states(&states)));
                }
            }
            xdg_toplevel::Event::Close => state.request_close(*key),
            _ => {}
        }
    }
}

/// `xdg_toplevel.configure` 的状态数组（原生字节序的 u32 列表）。
fn parse_states(raw: &[u8]) -> TopState {
    let mut st = TopState::default();
    for v in raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| u32::from_ne_bytes(*b))
    {
        match xdg_toplevel::State::try_from(v) {
            Ok(xdg_toplevel::State::Maximized) => st.maximized = true,
            Ok(xdg_toplevel::State::Activated) => st.activated = true,
            _ => {}
        }
    }
    st
}

impl Dispatch<wl_callback::WlCallback, u32> for Wl {
    fn event(
        state: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            if let Some(i) = state.idx(*key) {
                state.windows[i].frame_pending = false;
            }
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, (u32, usize)> for Wl {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        &(key, slot): &(u32, usize),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(i) = state.idx(key) {
                let w = &mut state.windows[i];
                // 只认当前占着这个槽的缓冲：隐藏会销毁全部缓冲、再显示按同一槽号新建，
                // 旧缓冲残留的 release 不能把新缓冲错标成空闲（那样会写进合成器还在读的缓冲）。
                let current = w
                    .bufs
                    .get(slot)
                    .and_then(|b| b.as_ref())
                    .is_some_and(|b| &b.buffer == buffer);
                if current {
                    w.slots.release(slot);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn first_frames_create_two_buffers_then_wait_for_release() {
        let mut p = ShmSlots::default();
        let a = p.plan(100, 50, r(0, 0, 100, 50)).unwrap();
        assert_eq!((a.index, a.recreate, a.write), (0, true, r(0, 0, 100, 50)));
        let b = p.plan(100, 50, r(10, 10, 5, 5)).unwrap();
        assert_eq!(
            (b.index, b.recreate),
            (1, true),
            "第一块还在合成器手里，新建第二块"
        );
        assert_eq!(b.write, r(0, 0, 100, 50), "新缓冲整块写");
        assert_eq!(p.plan(100, 50, r(0, 0, 1, 1)), None, "两块都忙：等 release");
    }

    #[test]
    fn reused_buffer_writes_damage_plus_what_it_missed() {
        let mut p = ShmSlots::default();
        p.plan(100, 50, r(0, 0, 100, 50)).unwrap(); // 0
        p.plan(100, 50, r(10, 10, 5, 5)).unwrap(); // 1，0 欠 (10,10,5,5)
        p.release(0);
        let c = p.plan(100, 50, r(40, 20, 2, 2)).unwrap();
        assert_eq!(c.index, 0);
        assert!(!c.recreate);
        assert_eq!(c.write, r(10, 10, 5, 5).union(&r(40, 20, 2, 2)));
        p.release(1);
        let d = p.plan(100, 50, r(0, 0, 1, 1)).unwrap();
        assert_eq!(d.index, 1);
        assert_eq!(
            d.write,
            r(40, 20, 2, 2).union(&r(0, 0, 1, 1)),
            "欠账在自己被写之后清零，只剩之后别的帧的脏区"
        );
    }

    #[test]
    fn resize_recreates_free_buffers_and_keeps_busy_ones() {
        let mut p = ShmSlots::default();
        p.plan(100, 50, r(0, 0, 100, 50)).unwrap(); // 0 忙
        p.plan(100, 50, r(0, 0, 100, 50)).unwrap(); // 1 忙
        p.release(1);
        let e = p.plan(120, 60, r(0, 0, 120, 60)).unwrap();
        assert_eq!((e.index, e.recreate, e.write), (1, true, r(0, 0, 120, 60)));
        p.release(0);
        let f = p.plan(120, 60, r(5, 5, 1, 1)).unwrap();
        assert_eq!(
            (f.index, f.recreate),
            (0, true),
            "旧尺寸的块空出来后按新尺寸重建"
        );
        assert_eq!(f.write, r(0, 0, 120, 60));
    }

    #[test]
    fn failed_slot_is_rebuilt_and_fully_rewritten() {
        let mut p = ShmSlots::default();
        p.plan(100, 50, r(0, 0, 100, 50)).unwrap();
        p.fail(0);
        let h = p.plan(100, 50, r(1, 1, 1, 1)).unwrap();
        assert_eq!((h.index, h.recreate, h.write), (0, true, r(0, 0, 100, 50)));
    }

    #[test]
    fn toplevel_states_are_decoded() {
        let raw: Vec<u8> = [4u32, 1, 3].iter().flat_map(|v| v.to_ne_bytes()).collect();
        assert_eq!(
            parse_states(&raw),
            TopState {
                maximized: true,
                activated: true
            }
        );
        assert_eq!(parse_states(&[]), TopState::default());
    }

    #[test]
    fn pixels_land_in_xrgb_layout_at_the_right_offset() {
        let mut pm = Pixmap::new(4, 3).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(10, 20, 30, 255));
        let file = super::super::sys::memfd(c"windui-test", 4 * 3 * 4).unwrap();
        let mut buf = Vec::new();
        // 非整行：只写 (1,1) 一个像素。
        write_pixels(&file, &pm, r(1, 1, 1, 1), &mut buf).unwrap();
        let mut out = vec![0u8; 48];
        file.read_exact_at(&mut out, 0).unwrap();
        fn px(out: &[u8], x: usize, y: usize) -> &[u8] {
            &out[(y * 4 + x) * 4..(y * 4 + x) * 4 + 4]
        }
        assert_eq!(px(&out, 1, 1), [30, 20, 10, 255]);
        assert_eq!(px(&out, 0, 0), [0, 0, 0, 0], "区域外不写");
        // 整行：第 2 行整行连续写。
        write_pixels(&file, &pm, r(0, 2, 4, 1), &mut buf).unwrap();
        file.read_exact_at(&mut out, 0).unwrap();
        assert_eq!(px(&out, 3, 2), [30, 20, 10, 255]);
        assert_eq!(px(&out, 0, 1), [0, 0, 0, 0]);
    }
}
