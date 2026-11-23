//! 拖出到外部程序：XDND 协议的**源端**（版本 5，兼容 3、4）。目标端见 `dnd`。
//!
//! 流程：占有 `XdndSelection`、抓住指针与键盘 → 指针移动时从根窗口逐层往下找第一个声明了
//! `XdndAware` 的窗口（经 `XdndProxy` 转发的照转），进出时发 `XdndEnter` / `XdndLeave`、
//! 移动时发 `XdndPosition`，对方回 `XdndStatus` 说接不接受 → 松手时对方接受就发 `XdndDrop`，
//! 对方向 `XdndSelection` 要 `text/uri-list`，我们写进它的属性 → 对方回 `XdndFinished`。
//!
//! ## 为什么不经 `X11` 结构
//!
//! 调用方经 `ctx.defer_blocking` 调用 `drag_files`，延迟闭包在 `X11::after_event` 里执行，
//! 那时整个 `X11` 正被可变借用，回调不了后端。所以 X11 后端启动时把连接登记在线程局部的
//! [`SourceCtx`] 里，拖出时自己跑嵌套事件循环：只处理拖放相关的事件，其余事件放进
//! [`Pending`] 队列，主循环在读新事件之前先取完这个队列。代价是拖动期间本程序的窗口不重绘
//! ——内容是静态的，可以接受（对照 win32：`DoDragDrop` 的消息泵会照常派发 `WM_PAINT`）。
//!
//! 拖到本程序自己的窗口上时不走协议（目标端也在被借用的 `X11` 里），直接排一个
//! [`Pending::LocalDrop`]，由主循环交给那个窗口，与 win32「本窗口也算接收方」一致。
//!
//! ## 动作
//!
//! 源端每次 `XdndPosition` 只能提议一个动作。默认提议复制；`allow_move` 时按住 Shift 提议
//! 移动——GTK / Qt 拖动的惯例。接收方最终执行什么以它回的为准。

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, EventMask, GrabMode, GrabStatus,
    PropMode, SelectionNotifyEvent, SelectionRequestEvent, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{CURRENT_TIME, NONE};

use super::dnd::{self, DndAtoms, XDND_VERSION};
use crate::platform::DragEffect;

/// 松手后等 `XdndFinished` 的上限，从最后一次收到事件起算：对方要数据、我们同步跑
/// `before_drop`（可能弹框很久）都会重新计时，这里只防对方一去不回。
const FINISH_TIMEOUT: Duration = Duration::from_secs(10);

/// 拖出要用到的后端资源，X11 后端启动时登记（Wayland 后端不登记，拖出返回 `None`）。
#[derive(Clone)]
pub(super) struct SourceCtx {
    pub conn: Rc<RustConnection>,
    pub screen: usize,
    pub root: Window,
    pub atoms: DndAtoms,
    /// 逻辑→物理缩放，拖动图像按它出图。
    pub scale: f32,
    pub icon: IconVisual,
}

/// 拖动图像窗口用的视觉。有合成器时是 32 位 ARGB（圆角 + 半透明），否则是根窗口的视觉。
#[derive(Clone, Copy)]
pub(super) struct IconVisual {
    pub depth: u8,
    pub visual: xproto::Visualid,
    /// 0 = 沿用父窗口的（默认视觉）。
    pub colormap: xproto::Colormap,
    /// 与 `depth` 同深度的绘图上下文。
    pub gc: xproto::Gcontext,
    pub argb: bool,
    /// true = 小端（B,G,R,A）。
    pub lsb: bool,
}

/// 嵌套循环没处理、留给主循环的东西。
pub(super) enum Pending {
    /// 与拖放无关的事件，原样交还。
    Event(Event),
    /// 拖动吞掉了左键松开：按捕获丢失收尾，否则上层一直处于按下态，下一次移动又会
    /// 再拖一次（对照 win32 `DoDragDrop` 抢走捕获时的 `WM_CAPTURECHANGED`）。
    CaptureLost(Window),
    /// 放在了本程序自己的窗口上。`pos` 是窗口内物理像素。
    LocalDrop {
        window: Window,
        pos: (i32, i32),
        paths: Vec<PathBuf>,
    },
}

thread_local! {
    static CTX: RefCell<Option<SourceCtx>> = const { RefCell::new(None) };
    static PENDING: RefCell<VecDeque<Pending>> = const { RefCell::new(VecDeque::new()) };
    /// 最近一次左键按下 / 指针移动所在的窗口与时间戳：拖动从这里开始，抓取与占有选区
    /// 都要带一个真实的时间戳（`CURRENT_TIME` 会被一些窗口管理器拒绝）。
    static LAST_POINTER: Cell<Option<(Window, u32)>> = const { Cell::new(None) };
    /// 首次拖动时按光标主题加载，此后一直复用（几个光标 id，不值得每次建删）。
    static CURSORS: Cell<Option<Cursors>> = const { Cell::new(None) };
    /// 合成器此刻在不在（运行中可能退出 / 重开）：不在时 ARGB 窗口的透明像素没人混合，
    /// 拖动图像改画方角不透明。
    static COMPOSITED: Cell<bool> = const { Cell::new(false) };
}

/// X11 后端在启动与合成器出现 / 退出时调用。
pub(super) fn set_composited(on: bool) {
    COMPOSITED.with(|c| c.set(on));
}

pub(super) fn install(ctx: SourceCtx) {
    CTX.with(|c| *c.borrow_mut() = Some(ctx));
}

/// X11 主循环收到按下 / 移动事件时记一笔（只记左键按住的情形）。
pub(super) fn note_pointer(window: Window, time: u32) {
    LAST_POINTER.with(|c| c.set(Some((window, time))));
}

pub(super) fn take_pending() -> Option<Pending> {
    PENDING.with(|q| q.borrow_mut().pop_front())
}

pub(super) fn has_pending() -> bool {
    PENDING.with(|q| !q.borrow().is_empty())
}

fn push_pending(p: Pending) {
    PENDING.with(|q| q.borrow_mut().push_back(p));
}

/// 见 win32 `drag_files_with`。Linux 上 `before_drop` 在接收方第一次来要 `text/uri-list` 时
/// 调用：多数文件管理器在松手后才要，个别程序在悬停时就要（同 macOS）。
pub(super) fn drag_files_with(
    paths: &[impl AsRef<Path>],
    allow_move: bool,
    before_drop: Option<Box<dyn FnOnce() -> bool>>,
) -> DragEffect {
    let Some(ctx) = CTX.with(|c| c.borrow().clone()) else {
        log::warn!("drag_files: 当前后端不支持拖出（仅 X11）");
        return DragEffect::None;
    };
    let Some((window, time)) = LAST_POINTER.with(|c| c.get()) else {
        log::warn!("drag_files 须在鼠标按下或拖动的事件里经 ctx.defer_blocking 调用");
        return DragEffect::None;
    };
    let uris = dnd::uri_list(paths);
    if uris.is_empty() {
        return DragEffect::None;
    }
    let paths: Vec<PathBuf> = paths.iter().map(|p| p.as_ref().to_path_buf()).collect();
    let mut s = Session {
        conn: &ctx.conn,
        ctx: &ctx,
        root: ctx.root,
        a: ctx.atoms,
        source: window,
        time,
        uris,
        paths,
        allow_move,
        before_drop,
        verdict: None,
        target: None,
        accepted: false,
        status_action: 0,
        awaiting_status: false,
        queued_position: None,
        dropped: false,
        last_root: (0, 0),
        shift: false,
        cursors: Cursors::default(),
        grabbed: false,
        icon: None,
        escape: Vec::new(),
    };
    let effect = s.run();
    s.finish();
    // 没抓到指针就没吞掉什么，不补捕获丢失（否则会打断应用正在进行的按住拖动）。
    // 排在队首：松手后放开了抓取，等收尾期间用户在本窗口上的新按下已经排进队列，
    // 捕获丢失必须先于它们处理，不能把那次新按下也当成被打断。
    if s.grabbed {
        PENDING.with(|q| q.borrow_mut().push_front(Pending::CaptureLost(window)));
    }
    effect
}

/// 指针下的接收方。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    /// 声明了 `XdndAware` 的窗口：消息里的「目标窗口」字段填它。
    window: Window,
    /// 消息实际发往的窗口（有 `XdndProxy` 时是代理，否则同 `window`）。
    dest: Window,
    version: u32,
    /// 本程序自己的窗口：不走协议，见模块文档。
    local: bool,
}

#[derive(Clone, Copy, Default)]
struct Cursors {
    copy: xproto::Cursor,
    move_: xproto::Cursor,
    refuse: xproto::Cursor,
}

/// 跟着指针走的拖动图像窗口（override-redirect，不进窗口管理器）。
struct DragIcon {
    window: Window,
    /// 图像左上角相对指针的偏移（物理像素）。
    offset: (i16, i16),
}

enum Step {
    Continue,
    Done(DragEffect),
}

struct Session<'a> {
    conn: &'a RustConnection,
    ctx: &'a SourceCtx,
    root: Window,
    a: DndAtoms,
    /// 拖动源窗口（占有选区、抓取指针的那个）。
    source: Window,
    time: u32,
    uris: Vec<u8>,
    paths: Vec<PathBuf>,
    allow_move: bool,
    before_drop: Option<Box<dyn FnOnce() -> bool>>,
    /// `before_drop` 的结果，`None` = 还没问过。
    verdict: Option<bool>,
    target: Option<Target>,
    accepted: bool,
    status_action: Atom,
    /// 发了 Position、还没收到 Status：协议要求这期间不再发新的 Position。
    awaiting_status: bool,
    queued_position: Option<(i16, i16)>,
    /// 已发 `XdndDrop`，在等 `XdndFinished`。
    dropped: bool,
    last_root: (i16, i16),
    shift: bool,
    cursors: Cursors,
    /// 拿到了指针抓取（松开就会被我们吞掉）。
    grabbed: bool,
    icon: Option<DragIcon>,
    escape: Vec<u8>,
}

impl Session<'_> {
    fn run(&mut self) -> DragEffect {
        let a = self.a;
        // 左键必须还按着：从键盘、定时器触发时 `LAST_POINTER` 是旧的，抓取只校验时间戳、
        // 不看按钮，照样会成功，结果是没按鼠标就进了拖动态。
        let Some(start) = self
            .conn
            .query_pointer(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
        else {
            return DragEffect::None;
        };
        if u16::from(start.mask) & u16::from(xproto::KeyButMask::BUTTON1) == 0 {
            log::warn!("drag_files: 左键没有按着，不发起拖动");
            return DragEffect::None;
        }
        let _ = self
            .conn
            .set_selection_owner(self.source, a.XdndSelection, self.time);
        self.cursors = cursors(self.conn, self.ctx.screen);
        let mask = EventMask::POINTER_MOTION | EventMask::BUTTON_RELEASE;
        let grabbed = self
            .conn
            .grab_pointer(
                false,
                self.source,
                mask,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
                NONE,
                self.cursors.refuse,
                self.time,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|r| r.status == GrabStatus::SUCCESS);
        if !grabbed {
            log::warn!("drag_files: 抓取指针失败（鼠标可能已松开）");
            return DragEffect::None;
        }
        self.grabbed = true;
        // 查按钮与抓取之间松开的话，那次松开没经过抓取、再也等不到：抓到后再查一次。
        let still_down = self
            .conn
            .query_pointer(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some_and(|p| u16::from(p.mask) & u16::from(xproto::KeyButMask::BUTTON1) != 0);
        if !still_down {
            return DragEffect::None;
        }
        // 抓键盘只为 Esc 取消，抓不到也照常拖。
        let _ = self.conn.grab_keyboard(
            false,
            self.source,
            self.time,
            GrabMode::ASYNC,
            GrabMode::ASYNC,
        );
        self.escape = escape_keycodes(self.conn);
        self.icon = self.create_icon(start.root_x, start.root_y);
        // 起点就可能已经在某个接收方上方。
        self.shift = u16::from(start.mask) & u16::from(xproto::KeyButMask::SHIFT) != 0;
        self.moved(start.root_x, start.root_y);

        let fd = self.conn.stream().as_raw_fd();
        let mut dropped_at: Option<Instant> = None;
        loop {
            let _ = self.conn.flush();
            let ev = match self.conn.poll_for_event() {
                Ok(Some(ev)) => Some(ev),
                Ok(None) => None,
                Err(e) => {
                    log::error!("drag_files: X 连接断开：{e}");
                    return DragEffect::None;
                }
            };
            let Some(ev) = ev else {
                if dropped_at.is_some_and(|t| t.elapsed() > FINISH_TIMEOUT) {
                    log::warn!("drag_files: 接收方没有回 XdndFinished，按它最后的回复处理");
                    return self.fallback_effect();
                }
                super::sys::wait_readable(&[fd], Some(Duration::from_millis(100)));
                continue;
            };
            match self.handle(ev) {
                Step::Continue => {}
                Step::Done(effect) => return effect,
            }
            // 在 handle 之后取时间：其中的 before_drop 不计入等待。
            if self.dropped {
                dropped_at = Some(Instant::now());
            }
        }
    }
}

impl Session<'_> {
    fn handle(&mut self, ev: Event) -> Step {
        let a = self.a;
        // 松手后已放开抓取，此后的鼠标键盘事件是用户在本程序窗口上的正常操作，交还主循环。
        if self.dropped
            && matches!(
                ev,
                Event::MotionNotify(_)
                    | Event::ButtonPress(_)
                    | Event::ButtonRelease(_)
                    | Event::KeyPress(_)
                    | Event::KeyRelease(_)
            )
        {
            push_pending(Pending::Event(ev));
            return Step::Continue;
        }
        match ev {
            Event::MotionNotify(e) if !self.dropped => {
                self.time = e.time;
                self.shift = u16::from(e.state) & u16::from(xproto::KeyButMask::SHIFT) != 0;
                self.moved(e.root_x, e.root_y);
            }
            Event::MotionNotify(_) => {}
            Event::ButtonRelease(e) if e.detail == 1 && !self.dropped => {
                self.time = e.time;
                return self.released(e.root_x, e.root_y);
            }
            Event::ButtonRelease(_) | Event::ButtonPress(_) => {}
            Event::KeyPress(e) if self.escape.contains(&e.detail) && !self.dropped => {
                self.leave();
                return Step::Done(DragEffect::None);
            }
            Event::KeyPress(_) | Event::KeyRelease(_) => {}
            Event::ClientMessage(e) if e.type_ == a.XdndStatus => self.on_status(&e),
            Event::ClientMessage(e) if e.type_ == a.XdndFinished => {
                if let Some(effect) = self.on_finished(&e) {
                    return Step::Done(effect);
                }
            }
            Event::SelectionRequest(e) if e.selection == a.XdndSelection => {
                self.on_selection_request(&e)
            }
            Event::SelectionClear(e) if e.selection == a.XdndSelection => {
                // 别人抢走了选区（另一个拖动开始了）：这次拖动已无法完成。
                self.leave();
                return Step::Done(DragEffect::None);
            }
            other => push_pending(Pending::Event(other)),
        }
        Step::Continue
    }

    /// 指针移到了根窗口坐标 (x, y)。
    fn moved(&mut self, x: i16, y: i16) {
        self.last_root = (x, y);
        if let Some(icon) = &self.icon {
            let aux = xproto::ConfigureWindowAux::new()
                .x(i32::from(x) + i32::from(icon.offset.0))
                .y(i32::from(y) + i32::from(icon.offset.1));
            let _ = self.conn.configure_window(icon.window, &aux);
        }
        let found = self.find_target(x, y);
        if found.map(|t| t.window) != self.target.map(|t| t.window) {
            self.leave();
            self.target = found;
            if let Some(t) = found {
                if t.local {
                    self.accepted = true;
                    self.status_action = self.a.XdndActionCopy;
                } else {
                    let flags = t.version << 24;
                    self.send(
                        t,
                        self.a.XdndEnter,
                        [self.source, flags, self.a.TEXT_URI_LIST, 0, 0],
                    );
                }
            }
        }
        match self.target {
            Some(t) if !t.local => {
                if self.awaiting_status {
                    self.queued_position = Some((x, y));
                } else {
                    self.send_position(t, x, y);
                }
            }
            _ => {}
        }
        self.update_cursor();
    }

    fn send_position(&mut self, t: Target, x: i16, y: i16) {
        let packed = (u32::from(x as u16) << 16) | u32::from(y as u16);
        let action = self.proposed_action();
        self.send(
            t,
            self.a.XdndPosition,
            [self.source, 0, packed, self.time, action],
        );
        self.awaiting_status = true;
    }

    fn proposed_action(&self) -> Atom {
        if self.allow_move && self.shift {
            self.a.XdndActionMove
        } else {
            self.a.XdndActionCopy
        }
    }

    fn on_status(&mut self, e: &ClientMessageEvent) {
        let d = e.data.as_data32();
        let Some(t) = self.target else { return };
        if d[0] != t.window || self.dropped {
            return;
        }
        self.accepted = d[1] & 1 != 0;
        self.status_action = if self.accepted { d[4] } else { 0 };
        self.awaiting_status = false;
        if let Some((x, y)) = self.queued_position.take() {
            self.send_position(t, x, y);
        }
        self.update_cursor();
    }

    fn released(&mut self, x: i16, y: i16) -> Step {
        let Some(t) = self.target else {
            return Step::Done(DragEffect::None);
        };
        if !self.accepted {
            self.leave();
            return Step::Done(DragEffect::None);
        }
        if t.local {
            // `before_drop` 可能弹框：先放开抓取，否则整个桌面的鼠标键盘都被我们占着。
            self.ungrab();
            if !self.files_ready() {
                return Step::Done(DragEffect::None);
            }
            let pos = self
                .conn
                .translate_coordinates(self.root, t.window, x, y)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| (i32::from(r.dst_x), i32::from(r.dst_y)))
                .unwrap_or_default();
            push_pending(Pending::LocalDrop {
                window: t.window,
                pos,
                paths: self.paths.clone(),
            });
            return Step::Done(DragEffect::Copy);
        }
        self.send(t, self.a.XdndDrop, [self.source, 0, self.time, 0, 0]);
        self.dropped = true;
        // 松手了：收起拖动图像、放开抓取。之后只等对方要数据与回 `XdndFinished`，这两样
        // 不经抓取也会送到；而 `before_drop` 可能弹框、对方也可能先问「是否覆盖」，
        // 抓着不放的话这些框都收不到鼠标键盘。
        if let Some(icon) = &self.icon {
            let _ = self.conn.unmap_window(icon.window);
        }
        self.ungrab();
        Step::Continue
    }

    /// `XdndFinished`：v5 带「是否成功」与最终动作，更早的版本只说结束了。
    fn on_finished(&mut self, e: &ClientMessageEvent) -> Option<DragEffect> {
        let d = e.data.as_data32();
        let t = self.target?;
        if !self.dropped || d[0] != t.window {
            return None;
        }
        if self.verdict == Some(false) {
            return Some(DragEffect::None);
        }
        if t.version >= 5 {
            if d[1] & 1 == 0 {
                return Some(DragEffect::None);
            }
            let action = if d[2] != 0 { d[2] } else { self.status_action };
            return Some(effect_of(
                action,
                self.a.XdndActionMove,
                self.a.XdndActionLink,
            ));
        }
        Some(self.fallback_effect())
    }

    fn fallback_effect(&self) -> DragEffect {
        if self.verdict == Some(false) || !self.accepted {
            DragEffect::None
        } else {
            effect_of(
                self.status_action,
                self.a.XdndActionMove,
                self.a.XdndActionLink,
            )
        }
    }

    fn on_selection_request(&mut self, req: &SelectionRequestEvent) {
        let a = self.a;
        // 老客户端把 property 填 NONE，按 ICCCM 用 target 当属性名。
        let prop = if req.property == NONE {
            req.target
        } else {
            req.property
        };
        let ok = if req.target == a.TARGETS {
            self.conn
                .change_property32(
                    PropMode::REPLACE,
                    req.requestor,
                    prop,
                    AtomEnum::ATOM,
                    &[a.TARGETS, a.TEXT_URI_LIST],
                )
                .is_ok()
        } else if req.target == a.TEXT_URI_LIST && self.files_ready() {
            self.conn
                .change_property8(
                    PropMode::REPLACE,
                    req.requestor,
                    prop,
                    a.TEXT_URI_LIST,
                    &self.uris,
                )
                .is_ok()
        } else {
            false
        };
        let notify = SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: req.time,
            requestor: req.requestor,
            selection: req.selection,
            target: req.target,
            property: if ok { prop } else { NONE },
        };
        let _ = self
            .conn
            .send_event(false, req.requestor, EventMask::NO_EVENT, notify);
    }

    /// 首次来要路径时调 `before_drop`，之后沿用结果。
    fn files_ready(&mut self) -> bool {
        files_ready(&mut self.verdict, &mut self.before_drop)
    }

    fn leave(&mut self) {
        if let Some(t) = self.target.take() {
            if !t.local {
                self.send(t, self.a.XdndLeave, [self.source, 0, 0, 0, 0]);
            }
        }
        self.accepted = false;
        self.status_action = 0;
        self.awaiting_status = false;
        self.queued_position = None;
    }

    /// 建拖动图像窗口，放在 (x, y) 指针处。图像做成窗口的背景 pixmap：暴露后由服务器
    /// 自己重画，嵌套循环不必处理它的 Expose。出图或建窗失败就不要图像，拖动照常。
    fn create_icon(&self, x: i16, y: i16) -> Option<DragIcon> {
        let v = self.ctx.icon;
        let img = super::drag_image::render(
            &self.paths,
            self.ctx.scale,
            v.argb && COMPOSITED.with(Cell::get),
        )?;
        let (w, h) = (
            u16::try_from(img.width()).ok()?,
            u16::try_from(img.height()).ok()?,
        );
        let conn = self.conn;

        let pixmap = conn.generate_id().ok()?;
        conn.create_pixmap(v.depth, pixmap, self.root, w, h)
            .ok()?
            .check()
            .ok()?;
        let row_bytes = usize::from(w) * 4;
        let max = conn
            .maximum_request_bytes()
            .saturating_sub(64)
            .max(row_bytes);
        let rows_per = (max / row_bytes).max(1);
        let mut buf = Vec::with_capacity(rows_per.min(usize::from(h)) * row_bytes);
        for (i, rows) in img.data().chunks(rows_per * row_bytes).enumerate() {
            buf.clear();
            if v.lsb {
                super::host::rgba_to_bgra(rows, &mut buf);
            } else {
                for px in rows.as_chunks::<4>().0 {
                    buf.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
                }
            }
            let _ = conn.put_image(
                xproto::ImageFormat::Z_PIXMAP,
                pixmap,
                v.gc,
                w,
                (rows.len() / row_bytes) as u16,
                0,
                (i * rows_per) as i16,
                0,
                v.depth,
                &buf,
            );
        }

        // 放在指针右下方：不挡光标，也不落在指针正下方（否则找接收方会先撞上它）。
        let off = (12.0 * self.ctx.scale).round() as i16;
        let window = conn.generate_id().ok()?;
        let mut aux = xproto::CreateWindowAux::new()
            .override_redirect(1)
            .background_pixmap(pixmap)
            .border_pixel(0);
        if v.colormap != NONE {
            aux = aux.colormap(v.colormap);
        }
        let made = conn
            .create_window(
                v.depth,
                window,
                self.root,
                x.saturating_add(off),
                y.saturating_add(off),
                w,
                h,
                0,
                xproto::WindowClass::INPUT_OUTPUT,
                v.visual,
                &aux,
            )
            .ok()
            .and_then(|c| c.check().ok());
        let _ = conn.free_pixmap(pixmap);
        made?;
        // 告诉合成器这是拖动图像：不加阴影、不做开窗动画。
        let atom = |n: &[u8]| {
            conn.intern_atom(false, n)
                .ok()?
                .reply()
                .ok()
                .map(|r| r.atom)
        };
        if let (Some(ty), Some(dnd)) = (
            atom(b"_NET_WM_WINDOW_TYPE"),
            atom(b"_NET_WM_WINDOW_TYPE_DND"),
        ) {
            let _ = conn.change_property32(PropMode::REPLACE, window, ty, AtomEnum::ATOM, &[dnd]);
        }
        let _ = conn.map_window(window);
        Some(DragIcon {
            window,
            offset: (off, off),
        })
    }

    fn update_cursor(&self) {
        let c = if self.accepted && self.status_action == self.a.XdndActionMove {
            self.cursors.move_
        } else if self.accepted {
            self.cursors.copy
        } else {
            self.cursors.refuse
        };
        let _ = self.conn.change_active_pointer_grab(
            c,
            CURRENT_TIME,
            EventMask::POINTER_MOTION | EventMask::BUTTON_RELEASE,
        );
    }

    fn send(&self, t: Target, type_: Atom, data: [u32; 5]) {
        let ev = ClientMessageEvent::new(32, t.window, type_, data);
        let _ = self.conn.send_event(false, t.dest, EventMask::NO_EVENT, ev);
    }

    /// 从根窗口逐层往下，找指针下第一个声明了 `XdndAware` 的窗口。
    fn find_target(&self, x: i16, y: i16) -> Option<Target> {
        let mut w = self.root;
        // 层数上限防环（正常树深不过十来层）。
        for _ in 0..32 {
            if w != self.root {
                if let Some(t) = self.aware(w) {
                    return Some(t);
                }
            }
            let child = self
                .conn
                .translate_coordinates(self.root, w, x, y)
                .ok()?
                .reply()
                .ok()?
                .child;
            // 拖动图像不挡在指针下（有偏移），这里只是兜底：碰上了就当底下没有接收方。
            if child == NONE || self.icon.as_ref().is_some_and(|i| i.window == child) {
                // 桌面程序常把根窗口的拖放代理给自己的桌面窗口。
                return if w == self.root { self.aware(w) } else { None };
            }
            w = child;
        }
        None
    }

    /// `w`（或它的代理）声明的 XDND 版本够用就返回目标。
    fn aware(&self, w: Window) -> Option<Target> {
        let a = self.a;
        let proxy = self
            .conn
            .get_property(false, w, a.XdndProxy, AtomEnum::WINDOW, 0, 1)
            .ok()?
            .reply()
            .ok()
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .filter(|&p| p != NONE);
        let holder = proxy.unwrap_or(w);
        let version = self
            .conn
            .get_property(false, holder, a.XdndAware, AtomEnum::ATOM, 0, 1)
            .ok()?
            .reply()
            .ok()?
            .value32()?
            .next()?;
        if version < 3 {
            return None;
        }
        Some(Target {
            window: w,
            dest: holder,
            version: version.min(XDND_VERSION),
            local: self.is_own(w),
        })
    }

    /// 本连接建的窗口：资源 id 落在本客户端的 id 段里。
    fn is_own(&self, w: Window) -> bool {
        let setup = self.conn.setup();
        w & !setup.resource_id_mask == setup.resource_id_base
    }

    fn ungrab(&self) {
        let _ = self.conn.ungrab_pointer(CURRENT_TIME);
        let _ = self.conn.ungrab_keyboard(CURRENT_TIME);
        let _ = self.conn.flush();
    }

    /// 放开抓取与选区、销毁拖动图像。
    fn finish(&mut self) {
        self.ungrab();
        let _ = self
            .conn
            .set_selection_owner(NONE, self.a.XdndSelection, CURRENT_TIME);
        if let Some(icon) = self.icon.take() {
            let _ = self.conn.destroy_window(icon.window);
        }
        let _ = self.conn.flush();
    }
}

/// `before_drop` 只跑一次，结果沿用。先记成 false 再调：回调里若跑了嵌套事件循环（弹框），
/// 期间对方再来要，不能把还没生成好的路径交出去。
fn files_ready(
    verdict: &mut Option<bool>,
    before_drop: &mut Option<Box<dyn FnOnce() -> bool>>,
) -> bool {
    if let Some(v) = *verdict {
        return v;
    }
    *verdict = Some(false);
    let ok = before_drop.take().is_none_or(|f| f());
    *verdict = Some(ok);
    ok
}

/// 对方执行的动作 → [`DragEffect`]。未知动作（`XdndActionPrivate`、`Ask` 选完之后的值）按复制。
fn effect_of(action: Atom, move_: Atom, link: Atom) -> DragEffect {
    if action == move_ {
        DragEffect::Move
    } else if action == link {
        DragEffect::Link
    } else if action == 0 {
        DragEffect::None
    } else {
        DragEffect::Copy
    }
}

/// 拖动用的光标：先按光标主题找 `dnd-copy` 这类名字（与文件管理器、浏览器拖动时同一套），
/// 主题里没有就落到 `cursor` 字体的 `XC_left_ptr`（68）/ `XC_circle`（24）。
fn cursors(conn: &RustConnection, screen: usize) -> Cursors {
    if let Some(c) = CURSORS.with(|c| c.get()) {
        return c;
    }
    let glyph = |g: u16| -> xproto::Cursor {
        let make = || -> Option<xproto::Cursor> {
            let font = conn.generate_id().ok()?;
            conn.open_font(font, b"cursor").ok()?;
            let c = conn.generate_id().ok()?;
            let made = conn
                .create_glyph_cursor(c, font, font, g, g + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff)
                .is_ok();
            let _ = conn.close_font(font);
            made.then_some(c)
        };
        make().unwrap_or(NONE)
    };
    let handle = x11rb::resource_manager::new_from_default(conn)
        .ok()
        .and_then(|db| {
            x11rb::cursor::Handle::new(conn, screen, &db)
                .ok()?
                .reply()
                .ok()
        });
    let themed = |names: &[&str]| -> xproto::Cursor {
        let Some(h) = &handle else { return NONE };
        names
            .iter()
            .map(|n| h.load_cursor(conn, n).unwrap_or(NONE))
            .find(|&c| c != NONE)
            .unwrap_or(NONE)
    };
    let copy = match themed(&["dnd-copy", "copy"]) {
        NONE => glyph(68),
        c => c,
    };
    let move_ = match themed(&["dnd-move"]) {
        NONE => copy,
        c => c,
    };
    let refuse = match themed(&["dnd-no-drop", "no-drop", "not-allowed"]) {
        NONE => glyph(24),
        c => c,
    };
    let c = Cursors {
        copy,
        move_,
        refuse,
    };
    CURSORS.with(|cell| cell.set(Some(c)));
    c
}

/// 映射到 `Escape` 键符的所有键码。
fn escape_keycodes(conn: &RustConnection) -> Vec<u8> {
    const XK_ESCAPE: u32 = 0xff1b;
    let setup = conn.setup();
    let (min, max) = (setup.min_keycode, setup.max_keycode);
    let Some(map) = conn
        .get_keyboard_mapping(min, max - min + 1)
        .ok()
        .and_then(|c| c.reply().ok())
    else {
        return Vec::new();
    };
    let per = usize::from(map.keysyms_per_keycode.max(1));
    map.keysyms
        .chunks(per)
        .enumerate()
        .filter(|(_, syms)| syms.contains(&XK_ESCAPE))
        .map(|(i, _)| min + i as u8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn before_drop_runs_once_and_verdict_sticks() {
        let calls = Rc::new(Cell::new(0));
        let c = calls.clone();
        let mut cb: Option<Box<dyn FnOnce() -> bool>> = Some(Box::new(move || {
            c.set(c.get() + 1);
            true
        }));
        let mut verdict = None;
        assert!(files_ready(&mut verdict, &mut cb));
        assert!(files_ready(&mut verdict, &mut cb));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn rejected_before_drop_withholds_data() {
        let mut cb: Option<Box<dyn FnOnce() -> bool>> = Some(Box::new(|| false));
        let mut verdict = None;
        assert!(!files_ready(&mut verdict, &mut cb));
        assert!(!files_ready(&mut verdict, &mut cb));
    }

    #[test]
    fn no_callback_means_ready() {
        let mut verdict = None;
        assert!(files_ready(&mut verdict, &mut None));
    }

    #[test]
    fn actions_map_to_effects() {
        let (copy, mv, link) = (10, 11, 12);
        assert_eq!(effect_of(mv, mv, link), DragEffect::Move);
        assert_eq!(effect_of(link, mv, link), DragEffect::Link);
        assert_eq!(effect_of(copy, mv, link), DragEffect::Copy);
        assert_eq!(effect_of(99, mv, link), DragEffect::Copy, "私有动作按复制");
        assert_eq!(effect_of(0, mv, link), DragEffect::None);
    }
}
