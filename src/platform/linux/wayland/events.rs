//! 各协议对象的事件分发，以及指针 / 键盘事件到宿主的翻译。

use std::os::unix::fs::FileExt;
use std::time::Instant;

use wayland_client::globals::GlobalListContents;
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry,
    wl_seat, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{delegate_noop, Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1, wp_cursor_shape_manager_v1,
};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1, wp_fractional_scale_v1,
};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

use super::super::host::{self, DOUBLE_CLICK_SLOP};
use super::cursor::Cursor;
use super::input::{self, KeyRepeat, WheelFrame};
use super::xkb::Xkb;
use super::{parse_caps, parse_states, Kbd, Output, PendingDrag, Ptr, Wl, RESIZE_BORDER};
use crate::event::{Mods, MouseButton, PointerEvent, PointerKind};
use crate::geometry::Point;

// ── 全局对象 ──────────────────────────────────────────────────────────────

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Wl {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            // 显示器热插拔：新输出绑上，等它报 scale。其余全局对象的增删不跟随。
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == wl_output::WlOutput::interface().name => {
                let obj = registry.bind(name, version.min(4), qh, name);
                state.g.outputs.push(Output {
                    name,
                    obj,
                    scale: 1,
                });
            }
            wl_registry::Event::GlobalRemove { name } => {
                let Some(pos) = state.g.outputs.iter().position(|o| o.name == name) else {
                    return;
                };
                let out = state.g.outputs.remove(pos);
                for i in 0..state.windows.len() {
                    let before = state.windows[i].outputs.len();
                    state.windows[i].outputs.retain(|o| o != &out.obj);
                    if state.windows[i].outputs.len() != before {
                        state.update_scale(i);
                    }
                }
                if out.obj.version() >= 3 {
                    out.obj.release();
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(Wl: wl_compositor::WlCompositor);
delegate_noop!(Wl: wl_shm_pool::WlShmPool);
// `format` 事件：XRGB8888 是协议规定必支持的格式，不必等它。
delegate_noop!(Wl: ignore wl_shm::WlShm);
delegate_noop!(Wl: wp_viewporter::WpViewporter);
delegate_noop!(Wl: wp_viewport::WpViewport);
delegate_noop!(Wl: wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1);
delegate_noop!(Wl: wp_cursor_shape_manager_v1::WpCursorShapeManagerV1);
delegate_noop!(Wl: wp_cursor_shape_device_v1::WpCursorShapeDeviceV1);

impl Dispatch<wl_output::WlOutput, u32> for Wl {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Scale { factor } = event {
            if let Some(o) = state.g.outputs.iter_mut().find(|o| &o.obj == output) {
                o.scale = factor.max(1);
            }
            for i in 0..state.windows.len() {
                if state.windows[i].outputs.contains(output) {
                    state.update_scale(i);
                }
            }
        }
    }
}

/// 表面的 user data 是窗口号；0 是光标表面，不属于任何窗口。
impl Dispatch<wl_surface::WlSurface, u32> for Wl {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(i) = state.idx(*key) else { return };
        let w = &mut state.windows[i];
        match event {
            wl_surface::Event::Enter { output } => {
                // 热拔后才到的 enter 可能带着已删除的输出，不收。
                if state.g.outputs.iter().any(|o| o.obj == output) && !w.outputs.contains(&output) {
                    w.outputs.push(output);
                }
            }
            wl_surface::Event::Leave { output } => w.outputs.retain(|o| o != &output),
            wl_surface::Event::PreferredBufferScale { factor } => w.preferred_int = Some(factor),
            _ => return,
        }
        state.update_scale(i);
    }
}

impl Dispatch<wp_fractional_scale_v1::WpFractionalScaleV1, u32> for Wl {
    fn event(
        state: &mut Self,
        _: &wp_fractional_scale_v1::WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_fractional_scale_v1::Event::PreferredScale { scale } = event {
            if let Some(i) = state.idx(*key) {
                state.windows[i].fractional120 = Some(scale);
                state.update_scale(i);
            }
        }
    }
}

// ── xdg 外壳 ──────────────────────────────────────────────────────────────

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
        let Some(i) = state.idx(*key) else { return };
        if !state.windows[i]
            .role
            .as_ref()
            .is_some_and(|r| &r.top == top)
        {
            return;
        }
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => state.windows[i].pending = Some(((width, height), parse_states(&states))),
            xdg_toplevel::Event::WmCapabilities { capabilities } => {
                let w = &mut state.windows[i];
                w.caps = parse_caps(&capabilities);
                w.handler.on_window_state(w.window_state());
            }
            xdg_toplevel::Event::Close => state.request_close(*key),
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, u32> for Wl {
    fn event(
        state: &mut Self,
        cb: &wl_callback::WlCallback,
        event: wl_callback::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            if let Some(i) = state.idx(*key) {
                let w = &mut state.windows[i];
                // 只认当前等着的那个回调：超时作废的、隐藏前请求的，晚到了也不作数。
                if w.frame_cb.as_ref().is_some_and(|(c, _)| c == cb) {
                    w.frame_cb = None;
                }
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

// ── 输入设备 ──────────────────────────────────────────────────────────────

impl Dispatch<wl_seat::WlSeat, ()> for Wl {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        else {
            return;
        };
        let has_ptr = caps.contains(wl_seat::Capability::Pointer);
        let has_kbd = caps.contains(wl_seat::Capability::Keyboard);
        if has_ptr && state.pointer.is_none() {
            let obj = seat.get_pointer(qh, ());
            let shape = state
                .g
                .cursor_shape
                .as_ref()
                .map(|m| m.get_pointer(&obj, qh, ()));
            state.pointer = Some(Ptr {
                obj,
                cursor: Cursor::new(shape),
                focus: None,
                enter_serial: 0,
                pos: (0.0, 0.0),
                wheel: WheelFrame::default(),
                pressed: 0,
            });
        } else if !has_ptr {
            if let Some(p) = state.pointer.take() {
                p.cursor.destroy();
                if p.obj.version() >= 3 {
                    p.obj.release();
                }
            }
        }
        if has_kbd && state.keyboard.is_none() {
            state.keyboard = Some(Kbd {
                obj: seat.get_keyboard(qh, ()),
                xkb: None,
                focus: None,
                repeat: KeyRepeat::default(),
                warned: false,
            });
        } else if !has_kbd {
            if let Some(k) = state.keyboard.take() {
                if k.obj.version() >= 3 {
                    k.obj.release();
                }
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for Wl {
    fn event(
        state: &mut Self,
        ptr: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.on_pointer_event(ptr, event);
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.on_keyboard_event(event);
    }
}

// ── 指针 ──────────────────────────────────────────────────────────────────

impl Wl {
    fn mods(&self) -> Mods {
        self.keyboard
            .as_ref()
            .and_then(|k| k.xkb.as_ref())
            .map(|x| x.mods())
            .unwrap_or_default()
    }

    /// 指针当前所在窗口的下标与物理坐标。
    fn pointer_target(&self) -> Option<(usize, Point)> {
        let p = self.pointer.as_ref()?;
        let i = self.idx(p.focus?)?;
        let s = self.windows[i].scale;
        Some((
            i,
            Point::new(s.pos_to_physical(p.pos.0), s.pos_to_physical(p.pos.1)),
        ))
    }

    fn on_pointer_event(&mut self, ptr: &wl_pointer::WlPointer, event: wl_pointer::Event) {
        match event {
            wl_pointer::Event::Enter {
                serial,
                surface,
                surface_x,
                surface_y,
            } => {
                let key = self.idx_of_surface(&surface).map(|i| {
                    // 上一次标题栏拖动的残留作废：move / resize 之后合成器不一定发 leave，
                    // 留着的话下一次在内容区的松开会被吞掉。
                    let w = &mut self.windows[i];
                    w.title_drag.reset();
                    w.key
                });
                if let Some(p) = self.pointer.as_mut() {
                    p.focus = key;
                    p.enter_serial = serial;
                    p.pos = (surface_x, surface_y);
                    p.pressed = 0;
                    p.cursor.invalidate();
                }
                self.apply_cursor();
                self.pointer_move();
            }
            wl_pointer::Event::Leave { .. } => {
                let Some(p) = self.pointer.as_mut() else {
                    return;
                };
                let (focus, pressed) = (p.focus.take(), std::mem::take(&mut p.pressed));
                let Some(key) = focus else { return };
                let Some(i) = self.idx(key) else { return };
                let w = &mut self.windows[i];
                w.title_drag.reset();
                // 按着按钮离开 = 隐式抓取被合成器收走了（开始移动 / 缩放窗口、弹出系统菜单等），
                // 配对的松开不会再来：收掉逻辑捕获，同 X11 / win32 的「捕获被抢」。
                if pressed > 0 && std::mem::take(&mut w.capturing) {
                    let r = {
                        let _g = crate::platform::EventDispatchGuard::enter();
                        w.handler.on_capture_lost()
                    };
                    if r {
                        w.needs_paint = true;
                    }
                }
                // 清悬停（与 X11 LeaveNotify 同一口径：发一个窗外坐标的移动）。
                let ev =
                    PointerEvent::single(PointerKind::Move, Point::new(-1, -1), MouseButton::Left);
                self.dispatch_pointer(key, ev);
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                if let Some(p) = self.pointer.as_mut() {
                    p.pos = (surface_x, surface_y);
                }
                if !self.pending_drag_motion() {
                    self.pointer_move();
                }
            }
            wl_pointer::Event::Button {
                serial,
                time,
                button,
                state: bstate,
            } => {
                super::data::note_serial(serial);
                let press = bstate == WEnum::Value(wl_pointer::ButtonState::Pressed);
                if let Some(p) = self.pointer.as_mut() {
                    p.pressed = if press {
                        p.pressed + 1
                    } else {
                        p.pressed.saturating_sub(1)
                    };
                }
                self.on_button(serial, time, button, press);
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                if axis == WEnum::Value(wl_pointer::Axis::VerticalScroll) {
                    if let Some(p) = self.pointer.as_mut() {
                        p.wheel.axis(value);
                    }
                    // v5 以下没有 frame 事件：每个 axis 自成一帧。
                    if ptr.version() < 5 {
                        self.flush_wheel();
                    }
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                if axis == WEnum::Value(wl_pointer::Axis::VerticalScroll) {
                    if let Some(p) = self.pointer.as_mut() {
                        p.wheel.value120(value120);
                    }
                }
            }
            wl_pointer::Event::AxisDiscrete { axis, discrete } => {
                if axis == WEnum::Value(wl_pointer::Axis::VerticalScroll) {
                    if let Some(p) = self.pointer.as_mut() {
                        p.wheel.discrete(discrete);
                    }
                }
            }
            wl_pointer::Event::Frame => self.flush_wheel(),
            _ => {}
        }
    }

    fn flush_wheel(&mut self) {
        let Some(delta) = self.pointer.as_mut().and_then(|p| p.wheel.take()) else {
            return;
        };
        let Some((i, pos)) = self.pointer_target() else {
            return;
        };
        let key = self.windows[i].key;
        let ev = PointerEvent::single_with(
            PointerKind::Wheel(delta),
            pos,
            MouseButton::Left,
            self.mods(),
        );
        self.dispatch_pointer(key, ev);
    }

    fn pointer_move(&mut self) {
        let Some((i, pos)) = self.pointer_target() else {
            return;
        };
        let key = self.windows[i].key;
        let ev = PointerEvent::single_with(PointerKind::Move, pos, MouseButton::Left, self.mods());
        self.dispatch_pointer(key, ev);
    }

    fn on_button(&mut self, serial: u32, time: u32, code: u32, press: bool) {
        let Some((i, pos)) = self.pointer_target() else {
            return;
        };
        let key = self.windows[i].key;
        if let Some(m) = self.blocked_by_modal(key) {
            if press {
                self.show(m);
            }
            return;
        }
        let Some(button) = input::mouse_button(code) else {
            return;
        };
        let left = button == MouseButton::Left;
        if left && self.windows[i].frameless {
            if press {
                // 先作废上一次接管的残留（同 X11 后端的说明）。
                self.windows[i].title_drag.press();
                if let Some(pending) = self.try_frameless_drag(i, serial, time, pos) {
                    let w = &mut self.windows[i];
                    w.title_drag.take_over(pending);
                    // 非客户区按下：收起菜单类浮层（这一下不会作为指针事件下发）。
                    let r = {
                        let _g = crate::platform::EventDispatchGuard::enter();
                        w.handler.on_dismiss_overlays()
                    };
                    if r {
                        w.needs_paint = true;
                    }
                    self.after_event(key);
                    return;
                }
            } else if self.windows[i].title_drag.release() {
                return;
            }
        }
        if press {
            // 合成中点击别处：先放弃合成——命中位置要按不含合成串的文本算（同 X11）。
            self.ime_abort_composition(key);
        }
        let Some(i) = self.idx(key) else { return };
        let w = &mut self.windows[i];
        let slop = (DOUBLE_CLICK_SLOP as f64 * w.scale.factor).round() as i32;
        let click_count = if press {
            w.click.press(time, (pos.x, pos.y), code as u8, slop)
        } else {
            1
        };
        let ev = PointerEvent {
            kind: if press {
                PointerKind::Down
            } else {
                PointerKind::Up
            },
            pos,
            button,
            click_count,
            mods: self.mods(),
        };
        self.dispatch_pointer(key, ev);
    }

    /// 无边框窗口：边缘 → 待定缩放；标题栏拖动区 → 待定移动（双击切最大化）。返回 `Some`
    /// 表示这一下已被接管、不再下发给控件，内含待定拖动（双击切最大化时为 `None`）；真正交给
    /// 合成器要等指针移出阈值（见 `Win::title_drag`）。
    fn try_frameless_drag(
        &mut self,
        i: usize,
        serial: u32,
        time: u32,
        pos: Point,
    ) -> Option<Option<PendingDrag>> {
        let w = &mut self.windows[i];
        let border = (RESIZE_BORDER * w.scale.factor).round() as i32;
        if w.resizable && !w.state.maximized {
            if let Some(dir) = host::edge_direction(pos, w.w, w.h, border) {
                if !w.handler.interactive_at(pos) {
                    return Some(Some(PendingDrag {
                        edge: Some(input::resize_edge(dir)),
                        at: (pos.x, pos.y),
                        serial,
                    }));
                }
            }
        }
        if w.handler.window_drag_at(pos) && !w.handler.interactive_at(pos) {
            let slop = (DOUBLE_CLICK_SLOP as f64 * w.scale.factor).round() as i32;
            let n = w.click.press(time, (pos.x, pos.y), 1, slop);
            if n == 2 {
                let key = w.key;
                if w.resizable {
                    self.apply_window_op(key, crate::event::WindowOp::ToggleMaximize);
                }
                return Some(None);
            }
            return Some(Some(PendingDrag {
                edge: None,
                at: (pos.x, pos.y),
                serial,
            }));
        }
        None
    }

    /// 待定拖动的指针移动：超过阈值就交给合成器。返回 true 表示这次移动已被接管。
    fn pending_drag_motion(&mut self) -> bool {
        let Some((i, pos)) = self.pointer_target() else {
            return false;
        };
        let w = &mut self.windows[i];
        let slop = ((DOUBLE_CLICK_SLOP as f64 * w.scale.factor).round() as i32).max(1);
        let beyond =
            |d: &PendingDrag| (pos.x - d.at.0).abs() > slop || (pos.y - d.at.1).abs() > slop;
        let d = match w.title_drag.motion(beyond) {
            host::DragMotion::Free => return false,
            host::DragMotion::Held => return true,
            host::DragMotion::Start(d) => d,
        };
        let (Some(role), Some(seat)) = (&w.role, &self.g.seat) else {
            return true;
        };
        match d.edge {
            None => role.top._move(seat, d.serial),
            Some(edge) => role.top.resize(seat, d.serial, edge),
        }
        true
    }

    fn dispatch_pointer(&mut self, key: u32, ev: PointerEvent) {
        let Some(i) = self.idx(key) else { return };
        if self.blocked_by_modal(key).is_some() {
            return;
        }
        let w = &mut self.windows[i];
        let r = {
            let _g = crate::platform::EventDispatchGuard::enter();
            w.handler.on_pointer(ev)
        };
        w.capturing = w.handler.capture_active();
        if r {
            w.needs_paint = true;
        }
        self.after_event(key);
    }

    // ── 键盘 ──────────────────────────────────────────────────────────────

    fn on_keyboard_event(&mut self, event: wl_keyboard::Event) {
        let Some(k) = self.keyboard.as_mut() else {
            return;
        };
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                k.repeat.stop();
                k.xkb = None;
                if format != WEnum::Value(wl_keyboard::KeymapFormat::XkbV1) {
                    log::warn!("合成器发来的 keymap 格式不是 XKB v1，键盘不可用");
                    return;
                }
                if !Xkb::available() {
                    // 不做「只有键码没有字符」的降级：没有 keymap 连 Enter / 方向键都认不准，
                    // 半残的键盘比明确不可用更难排查。指针照常。
                    if !std::mem::replace(&mut k.warned, true) {
                        eprintln!(
                            "[windui] 找不到 libxkbcommon.so.0，Wayland 下键盘输入不可用（装 libxkbcommon0）"
                        );
                    }
                    return;
                }
                // v7 起 fd 只允许 MAP_PRIVATE 映射；这里干脆不映射，按文件读出来（memfd /
                // 临时文件都支持 pread），keymap 只有几十 KB。
                let file = std::fs::File::from(fd);
                let mut buf = vec![0u8; size as usize];
                match file.read_exact_at(&mut buf, 0) {
                    Ok(()) => {
                        k.xkb = Xkb::from_text(&buf);
                        if k.xkb.is_none() {
                            log::warn!("合成器发来的 keymap 编译失败，键盘不可用");
                        }
                    }
                    Err(e) => log::warn!("读取 keymap 失败：{e}"),
                }
            }
            wl_keyboard::Event::Enter {
                serial, surface, ..
            } => {
                super::data::note_serial(serial);
                super::data::note_focus(true);
                k.repeat.stop();
                k.focus = self
                    .windows
                    .iter()
                    .find(|w| w.surface == surface)
                    .map(|w| w.key);
            }
            wl_keyboard::Event::Leave { .. } => {
                super::data::note_focus(false);
                k.repeat.stop();
                k.focus = None;
                self.alt_down = false;
            }
            wl_keyboard::Event::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                if let Some(x) = k.xkb.as_mut() {
                    x.update_mask(mods_depressed, mods_latched, mods_locked, group);
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => k.repeat.set_info(rate, delay),
            wl_keyboard::Event::Key {
                serial,
                key,
                state: kstate,
                ..
            } => {
                super::data::note_serial(serial);
                let press = kstate == WEnum::Value(wl_keyboard::KeyState::Pressed);
                // XKB 键码 = evdev 键码 + 8。
                self.on_key_code(key + 8, press, false);
            }
            _ => {}
        }
    }

    /// 一次按键（`repeat` = 客户端按键重复合成的那一下）。
    pub(super) fn on_key_code(&mut self, keycode: u32, press: bool, repeat: bool) {
        let Some(k) = self.keyboard.as_mut() else {
            return;
        };
        let (Some(x), Some(key)) = (k.xkb.as_ref(), k.focus) else {
            return;
        };
        if !repeat {
            if press && x.repeats(keycode) {
                k.repeat.press(keycode, Instant::now());
            } else if !press {
                k.repeat.release(keycode);
            }
        }
        let events = host::translate_key(
            x.keysym(keycode),
            press,
            x.mods(),
            &mut self.alt_down,
            || x.shortcut_code(keycode),
        );
        for ev in events {
            // 逐个判：前一个事件的回调可能刚开了模态子窗（同 X11 的 dispatch_key）。
            if self.blocked_by_modal(key).is_some() {
                return;
            }
            let Some(i) = self.idx(key) else { return };
            let w = &mut self.windows[i];
            let r = {
                let _g = crate::platform::EventDispatchGuard::enter();
                w.handler.on_key(ev)
            };
            if r {
                w.needs_paint = true;
            }
            self.ime_text_changed();
            self.after_event(key);
        }
    }
}
