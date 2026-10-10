//! 窗口装饰：`xdg-decoration` 协商，合成器不画时由我们画客户端标题栏（CSD）。几何在 `csd.rs`。
//!
//! - 有 `zxdg_decoration_manager_v1`（sway、KDE、weston）：有边框窗口请求服务端装饰，合成器回
//!   `server_side` 就什么都不画；回 `client_side` 才画。无边框窗口明确请求 `client_side`，免得
//!   给它加边框。
//! - 没有这个协议（Mutter / GNOME）：有边框窗口一律自己画。
//!
//! 标题栏是宿主造的一个小宿主（`AppHandler::decoration`：标题 + 三个窗口按钮，视觉走主题），
//! 画在缓冲的最上面一段；它上面的指针事件交给它，它发出的窗口操作转给窗口。拖动区按下移出
//! 阈值才 `move`（沿用无边框窗口的 `DragGate`），双击切最大化，右键弹合成器的窗口菜单
//! （`show_window_menu`——CSD 的惯例；框架自己的系统菜单是画在窗口里的浮层，塞不进 33 像素高
//! 的标题栏）。

use tiny_skia::Pixmap;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::decoration::zv1::client::{
    zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    zxdg_toplevel_decoration_v1::{self, Mode, ZxdgToplevelDecorationV1},
};

use super::super::host;
use super::csd;
use super::Wl;
use crate::event::{MouseButton, PointerEvent, PointerKind, WindowOp};
use crate::geometry::{Point, Rect};
use crate::platform::Decoration;

/// 一扇窗口的客户端标题栏。
pub(super) struct Deco {
    pub host: Decoration,
    /// 正在用（合成器没给服务端装饰）。不用时整条不画、不占高度，宿主留着以备切回。
    pub on: bool,
    pub pixmap: Option<Pixmap>,
    pub fresh: bool,
    pub needs_paint: bool,
    /// 指针在标题栏上（悬停态归它）。
    pub hover: bool,
    /// 在标题栏的按钮上按下、还没松开：移动与松开都归它（按钮自己捕获）。
    pub captured: bool,
    /// 标题栏上一帧动画的时刻（它与内容各按各的截止时间走）。
    pub last_anim: std::time::Instant,
    /// 右键在拖动区按下、已弹合成器窗口菜单：配对的右键松开吞掉（不经左键的 `DragGate`，
    /// 免得把左键那次的「吞松开」标志冲掉）。
    pub swallow_right_up: bool,
}

impl Deco {
    fn new(host: Decoration, scale: f64) -> Self {
        let mut d = Self {
            host,
            on: false,
            pixmap: None,
            fresh: true,
            needs_paint: true,
            hover: false,
            captured: false,
            swallow_right_up: false,
            last_anim: std::time::Instant::now(),
        };
        d.host.handler.set_scale(scale as f32);
        d
    }

    /// 把一个指针事件交给标题栏宿主：它说要重画就标脏（只脏标题栏，不碰内容）。返回它发出的
    /// 窗口操作与是否请求关闭。
    pub fn pointer(&mut self, ev: PointerEvent) -> (Option<WindowOp>, bool) {
        let r = {
            let _g = crate::platform::EventDispatchGuard::enter();
            self.host.handler.on_pointer(ev)
        };
        if r {
            self.needs_paint = true;
        }
        (
            self.host.handler.take_window_op(),
            self.host.handler.wants_close(),
        )
    }

    /// 标题栏高度（逻辑像素），不用时 0。
    pub fn height(&self) -> i32 {
        if self.on {
            self.host.height
        } else {
            0
        }
    }
}

impl Dispatch<ZxdgDecorationManagerV1, ()> for Wl {
    fn event(
        _: &mut Self,
        _: &ZxdgDecorationManagerV1,
        _: <ZxdgDecorationManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(Wl: wayland_protocols::xdg::dialog::v1::client::xdg_wm_dialog_v1::XdgWmDialogV1);
wayland_client::delegate_noop!(Wl: wayland_protocols::xdg::dialog::v1::client::xdg_dialog_v1::XdgDialogV1);

impl Dispatch<ZxdgToplevelDecorationV1, u32> for Wl {
    fn event(
        state: &mut Self,
        obj: &ZxdgToplevelDecorationV1,
        event: zxdg_toplevel_decoration_v1::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let zxdg_toplevel_decoration_v1::Event::Configure { mode } = event else {
            return;
        };
        let Some(i) = state.idx(*key) else { return };
        let w = &mut state.windows[i];
        // 只认当前角色的装饰对象（隐藏再显示换过一套）。
        if w.role.as_ref().and_then(|r| r.deco.as_ref()) != Some(obj) {
            return;
        }
        // 与 toplevel 的尺寸一样，随下一个 `xdg_surface.configure` 一并生效。
        w.pending_deco = Some(mode == WEnum::Value(Mode::ClientSide));
    }
}

impl Wl {
    /// 标题栏高度（物理像素），不画时 0。
    ///
    /// 由「整窗高 − 内容高」得出而不是单独取整：分数缩放下两次独立取整之和可能比整窗多 1
    /// 像素，缓冲就与 viewport 目标（逻辑整窗高）差一行，合成器会把整窗重采样发糊。
    pub(super) fn bar_phys(&self, i: usize) -> i32 {
        let w = &self.windows[i];
        let h = w.deco.as_ref().map_or(0, Deco::height);
        csd::bar_physical(w.logical.1, h, |v| w.scale.to_physical(v))
    }

    /// 标题栏高度（逻辑像素），不画时 0。
    pub(super) fn bar_logical(&self, i: usize) -> i32 {
        self.windows[i].deco.as_ref().map_or(0, Deco::height)
    }

    pub(super) fn frame(&self, i: usize) -> csd::Frame {
        let w = &self.windows[i];
        csd::Frame {
            width: w.w,
            content_h: w.h,
            bar: self.bar_phys(i),
        }
    }

    /// 新建角色时决定装饰：有协议就协商（结果随 configure 到），没有就有边框窗口自己画。
    pub(super) fn negotiate_decoration(
        &mut self,
        i: usize,
        top: &wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel,
    ) -> Option<ZxdgToplevelDecorationV1> {
        let frameless = self.windows[i].frameless;
        match &self.g.decoration {
            Some(m) => {
                let key = self.windows[i].key;
                let obj = m.get_toplevel_decoration(top, &self.qh, key);
                obj.set_mode(if frameless {
                    Mode::ClientSide
                } else {
                    Mode::ServerSide
                });
                Some(obj)
            }
            None => {
                if !frameless {
                    self.set_csd(i, true);
                }
                None
            }
        }
    }

    /// 打开 / 关掉客户端标题栏。返回是否真的变了（变了要重设尺寸约束、整窗重画）。
    pub(super) fn set_csd(&mut self, i: usize, on: bool) -> bool {
        let on = on && !self.windows[i].frameless;
        let w = &mut self.windows[i];
        if w.deco.as_ref().is_some_and(|d| d.on) == on {
            return false;
        }
        if on && w.deco.is_none() {
            let maximizable = w.resizable;
            match w.handler.decoration(&w.title, maximizable) {
                Some(h) => w.deco = Some(Deco::new(h, w.scale.factor)),
                // 宿主不支持（非 UiHost 的 handler）：不画，窗口照常可用，只是没有标题栏。
                None => return false,
            }
        }
        if let Some(d) = w.deco.as_mut() {
            d.on = on;
            d.needs_paint = true;
            d.hover = false;
            d.captured = false;
            (d.host.set_active)(w.state.activated);
        }
        w.fresh = true;
        w.needs_paint = true;
        // 尺寸约束含标题栏，跟着重设。
        if let Some(r) = &w.role {
            let bar = w.deco.as_ref().map_or(0, Deco::height);
            let (mw, mh) = csd::min_with_bar(w.min_size, bar).unwrap_or((0, 0));
            r.top.set_min_size(mw, mh);
            if let Some(s) = w.max_size {
                let (mw, mh) = csd::with_bar(s, bar);
                r.top.set_max_size(mw, mh);
            }
        }
        true
    }

    /// 出一帧标题栏（需要时）。返回画到的区域（缓冲坐标 = 标题栏坐标）。
    pub(super) fn paint_deco(&mut self, i: usize, content_full: bool) -> Option<Rect> {
        let bar = self.bar_phys(i);
        let w = &mut self.windows[i];
        let width = w.w;
        let bg = w.bg;
        let d = w.deco.as_mut().filter(|d| d.on)?;
        let size_ok = d
            .pixmap
            .as_ref()
            .is_some_and(|p| p.width() as i32 == width && p.height() as i32 == bar);
        // 内容整窗重画多半是换了主题或尺寸，标题栏也跟着画一遍（它读同一份主题）。
        if !(d.needs_paint || !size_ok || content_full) {
            return None;
        }
        d.needs_paint = false;
        if content_full {
            d.fresh = true;
        }
        host::render_frame(
            d.host.handler.as_mut(),
            &mut d.pixmap,
            &mut d.fresh,
            (width, bar),
            bg,
            // 标题栏不描外框、不做圆角（它本身贴着窗口上沿，边缘收尾归内容区）。
            host::RimWant::default(),
            &mut host::Rim::default(),
        )
    }

    /// 把一个指针事件交给标题栏宿主，并落实它发出的窗口操作。
    pub(super) fn deco_pointer(&mut self, key: u32, ev: PointerEvent) {
        let Some(i) = self.idx(key) else { return };
        // 模态子窗开着：标题栏与内容区一样不响应（按下已在 `on_button` 里挡掉，这里挡悬停）。
        if self.blocked_by_modal(key).is_some() {
            return;
        }
        let w = &mut self.windows[i];
        let Some(d) = w.deco.as_mut().filter(|d| d.on) else {
            return;
        };
        let (op, close) = d.pointer(ev);
        if close {
            // 标题栏宿主的「关闭」是粘住的：先换一个新的，再把关闭请求交给窗口（窗口可能拒绝）。
            // 换不出来（理论上不会）就干脆不画标题栏，免得粘住的关闭标志让之后每个事件都请求关闭。
            let maximizable = w.resizable;
            match w.handler.decoration(&w.title, maximizable) {
                Some(h) => {
                    let mut nd = Deco::new(h, w.scale.factor);
                    nd.on = true;
                    (nd.host.set_active)(w.state.activated);
                    w.deco = Some(nd);
                }
                None => w.deco = None,
            }
            w.fresh = true;
            w.needs_paint = true;
        }
        if let Some(op) = op {
            self.apply_window_op(key, op);
        }
        if close {
            self.request_close(key);
        }
        if let Some(i) = self.idx(key) {
            if self.windows[i].deco.as_ref().is_some_and(|d| d.hover) {
                self.apply_cursor();
            }
        }
    }

    /// 指针离开标题栏（移到内容区、离开窗口）：清它的悬停态。
    pub(super) fn deco_leave(&mut self, key: u32) {
        let Some(i) = self.idx(key) else { return };
        let Some(d) = self.windows[i].deco.as_mut() else {
            return;
        };
        if !std::mem::take(&mut d.hover) {
            return;
        }
        // 清悬停不经模态拦截（模态子窗开着时指针移开，悬停态也得清掉），也不落实任何窗口操作。
        let ev = PointerEvent::single(PointerKind::Move, Point::new(-1, -1), MouseButton::Left);
        d.pointer(ev);
    }

    /// 标题栏空白处按下：拖动 / 双击最大化 / 右键窗口菜单。返回 `Some` = 这一下被接管（内含
    /// 待定拖动，双击与右键菜单时为 `None`）。`p` 是表面物理坐标。
    pub(super) fn deco_press(
        &mut self,
        i: usize,
        p: Point,
        button: MouseButton,
        serial: u32,
        time: u32,
    ) -> Option<Option<super::PendingDrag>> {
        let w = &mut self.windows[i];
        let d = w.deco.as_ref().filter(|d| d.on)?;
        if !d.host.handler.window_drag_at(p) || d.host.handler.interactive_at(p) {
            return None;
        }
        match button {
            MouseButton::Right => {
                if let (Some(r), Some(seat)) = (&w.role, &self.g.seat) {
                    let f = w.scale.factor;
                    let lg = |v: i32| (v as f64 / f).round() as i32;
                    r.top.show_window_menu(seat, serial, lg(p.x), lg(p.y));
                }
                Some(None)
            }
            MouseButton::Left => {
                let slop = (host::DOUBLE_CLICK_SLOP as f64 * w.scale.factor).round() as i32;
                if w.click.press(time, (p.x, p.y), 1, slop) == 2 {
                    let key = w.key;
                    if w.resizable {
                        self.apply_window_op(key, WindowOp::ToggleMaximize);
                    }
                    return Some(None);
                }
                Some(Some(super::PendingDrag {
                    edge: None,
                    at: (p.x, p.y),
                    serial,
                }))
            }
            _ => None,
        }
    }

    /// 窗口标题变了 / 激活态变了：同步给标题栏。回调不发重画通知，这里让标题栏整条重画
    /// （含重新布局：标题宽度可能变了）；内容区不动。
    pub(super) fn deco_title(&mut self, i: usize) {
        let w = &mut self.windows[i];
        if let Some(d) = w.deco.as_mut() {
            (d.host.set_title)(&w.title);
            d.needs_paint = true;
            d.fresh = true;
        }
    }

    pub(super) fn deco_active(&mut self, i: usize, active: bool) {
        let w = &mut self.windows[i];
        if let Some(d) = w.deco.as_mut() {
            (d.host.set_active)(active);
            d.needs_paint = true;
            d.fresh = true;
            if !active && std::mem::take(&mut d.captured) {
                // 失活时按着的标题栏按钮：捕获收掉（配对的松开不会再来）。
                d.host.handler.on_capture_lost();
            }
        }
    }

    /// 指针离开窗口时标题栏按钮还按着（合成器收走了抓取）：收掉它的捕获。
    pub(super) fn deco_capture_lost(&mut self, i: usize) {
        if let Some(d) = self.windows[i].deco.as_mut() {
            if std::mem::take(&mut d.captured) {
                d.host.handler.on_capture_lost();
                d.needs_paint = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Size;
    use crate::platform::AppHandler;

    /// 只用来驱动 `Deco` 簿记的最小宿主：`on_pointer` 按给定结果回答「要不要重画」。
    struct Host {
        repaint: bool,
        op: Option<WindowOp>,
    }

    impl AppHandler for Host {
        fn render(&mut self, _: &mut dyn crate::render::RenderTarget, _: Size) {}
        fn on_pointer(&mut self, _: PointerEvent) -> bool {
            self.repaint
        }
        fn take_window_op(&mut self) -> Option<WindowOp> {
            self.op.take()
        }
    }

    fn deco(repaint: bool, op: Option<WindowOp>) -> Deco {
        let mut d = Deco::new(
            Decoration {
                handler: Box::new(Host { repaint, op }),
                height: 33,
                set_title: Box::new(|_| {}),
                set_active: Box::new(|_| {}),
            },
            1.0,
        );
        d.on = true;
        d.needs_paint = false;
        d
    }

    fn mv() -> PointerEvent {
        PointerEvent::single(PointerKind::Move, Point::new(5, 5), MouseButton::Left)
    }

    #[test]
    fn a_titlebar_event_that_needs_a_repaint_marks_only_the_titlebar_dirty() {
        let mut d = deco(true, None);
        assert_eq!(d.pointer(mv()), (None, false));
        assert!(d.needs_paint, "悬停 / 按下要画出来");
        let mut d = deco(false, None);
        d.pointer(mv());
        assert!(!d.needs_paint, "宿主说不用画就不画");
    }

    #[test]
    fn window_operations_from_the_titlebar_are_handed_back() {
        let mut d = deco(false, Some(WindowOp::ToggleMaximize));
        assert_eq!(d.pointer(mv()).0, Some(WindowOp::ToggleMaximize));
        assert_eq!(d.pointer(mv()).0, None, "取走一次就没了");
    }
}
