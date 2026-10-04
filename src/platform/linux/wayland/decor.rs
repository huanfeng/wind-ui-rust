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
        };
        d.host.handler.set_scale(scale as f32);
        d
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
    pub(super) fn bar_phys(&self, i: usize) -> i32 {
        let w = &self.windows[i];
        match w.deco.as_ref().map(Deco::height) {
            Some(h) if h > 0 => w.scale.to_physical(h),
            _ => 0,
        }
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
            if let Some(s) = w.min_size {
                let (mw, mh) = csd::with_bar(s, bar);
                r.top.set_min_size(mw, mh);
            }
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
        let r = {
            let _g = crate::platform::EventDispatchGuard::enter();
            d.host.handler.on_pointer(ev)
        };
        if r {
            d.needs_paint = true;
        }
        let op = d.host.handler.take_window_op();
        let close = d.host.handler.wants_close();
        if close {
            // 标题栏宿主的「关闭」是粘住的：先换一个新的，再把关闭请求交给窗口（窗口可能拒绝）。
            let maximizable = w.resizable;
            if let Some(h) = w.handler.decoration(&w.title, maximizable) {
                let mut nd = Deco::new(h, w.scale.factor);
                nd.on = true;
                (nd.host.set_active)(w.state.activated);
                w.deco = Some(nd);
            }
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
        let ev = PointerEvent::single(PointerKind::Move, Point::new(-1, -1), MouseButton::Left);
        self.deco_pointer(key, ev);
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

    /// 窗口标题变了 / 激活态变了：同步给标题栏。
    pub(super) fn deco_title(&mut self, i: usize) {
        let w = &mut self.windows[i];
        if let Some(d) = w.deco.as_mut() {
            (d.host.set_title)(&w.title);
            d.needs_paint = true;
            if d.on {
                w.needs_paint = true;
            }
        }
    }

    pub(super) fn deco_active(&mut self, i: usize, active: bool) {
        let w = &mut self.windows[i];
        if let Some(d) = w.deco.as_mut() {
            (d.host.set_active)(active);
            d.needs_paint = true;
            if d.on {
                w.needs_paint = true;
            }
        }
    }
}
