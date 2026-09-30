//! 输入法：`zwp_text_input_v3` 协议对象的收发，状态机在 `text_input.rs`。
//!
//! 与 X11（XIM）/ macOS 同一条上层通路：宿主经 `ime_caret` 报光标位置、`ime_text` /
//! `ime_selection` 报上下文，合成串经 `set_ime_preedit` 由 `TextInput` **内联绘制**，
//! 提交的文字按 `Key::Char` 逐字送进去（同 X11 的 `ImeOut::Commit`）。
//!
//! - **何时启用**：文本焦点（`enter`）在某窗口、且宿主报了光标（焦点在可编辑控件上）、且窗口
//!   没被模态子窗挡住 → `enable`；否则 `disable`。每次事件收尾（`after_event`）对账一次，
//!   只有状态真变了才发请求、才 `commit`。
//! - **合成中的按键**：输入法抓着键盘时，合成器不把按键发给我们（Enter 确认候选、Esc 取消都
//!   由输入法消化）；它放行的键照常走本地——与 X11「输入法不要的才转回来」同一效果，不必另外
//!   拦截。
//! - **合成中点击**：先放弃合成（本地清掉，`disable` + `enable` 让输入法也丢掉），命中位置才
//!   按不含合成串的文本算——同 X11 的 `abort_composition`。
//! - **缺口**：框架没有向平台层暴露「多行 / 密码」属性，内容类型一律「普通」，密码框也会弹
//!   输入法（X11 的 XIM 同样如此；密码框不交出正文，`ime_text` 为空）。

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{self, ContentHint, ContentPurpose, ZwpTextInputV3},
};

use super::text_input::{surrounding, Req, TextInputState, Want};
use super::Wl;
use crate::event::{Key, KeyEvent, Preedit};

/// 对账时向宿主取的（光标，选区）：两者都没变，正文多半也没变。
type Probe = ((i32, i32, i32), Option<(usize, usize)>);

/// 每 seat 一个 text-input 对象（本后端只绑启动时那一个 seat）。
pub(super) struct Ime {
    obj: ZwpTextInputV3,
    state: TextInputState,
    /// 文本焦点所在窗口（`enter` 的表面）。
    focus: Option<u32>,
    /// 正在应用一批 `done`：期间的 `after_event` 不对账，应用完统一对账一次。
    applying: bool,
    /// 上次对账用的 (光标, 选区)：没变就沿用上次算好的周围文本，免得每个事件都复制一遍正文。
    probe: Option<Probe>,
    want: Option<Want>,
}

impl Ime {
    pub(super) fn new(
        manager: &ZwpTextInputManagerV3,
        seat: &wayland_client::protocol::wl_seat::WlSeat,
        qh: &QueueHandle<Wl>,
    ) -> Self {
        Self {
            obj: manager.get_text_input(seat, qh, ()),
            state: TextInputState::default(),
            focus: None,
            applying: false,
            probe: None,
            want: None,
        }
    }

    fn send(&self, reqs: Vec<Req>) {
        for r in reqs {
            match r {
                Req::Enable => self.obj.enable(),
                Req::Disable => self.obj.disable(),
                Req::ContentType => self
                    .obj
                    .set_content_type(ContentHint::None, ContentPurpose::Normal),
                Req::Rect(x, y, w, h) => self.obj.set_cursor_rectangle(x, y, w, h),
                Req::Surrounding(t, c, a) => self.obj.set_surrounding_text(t, c, a),
                Req::Commit => self.obj.commit(),
            }
        }
    }
}

impl Dispatch<ZwpTextInputManagerV3, ()> for Wl {
    fn event(
        _: &mut Self,
        _: &ZwpTextInputManagerV3,
        _: <ZwpTextInputManagerV3 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &ZwpTextInputV3,
        event: zwp_text_input_v3::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.on_text_input_event(event);
    }
}

impl Wl {
    fn on_text_input_event(&mut self, event: zwp_text_input_v3::Event) {
        let Some(ime) = self.ime.as_mut() else { return };
        match event {
            zwp_text_input_v3::Event::Enter { surface } => {
                ime.state.on_enter();
                ime.probe = None;
                ime.want = None;
                let key = self
                    .windows
                    .iter()
                    .find(|w| w.surface == surface)
                    .map(|w| w.key);
                ime.focus = key;
                if let Some(key) = key {
                    self.ime_update_now(key);
                }
            }
            zwp_text_input_v3::Event::Leave { .. } => {
                ime.state.on_leave();
                ime.probe = None;
                ime.want = None;
                // 协议：客户端应清掉合成串。
                if let Some(key) = ime.focus.take() {
                    self.dispatch_preedit(key, Preedit::default());
                }
            }
            zwp_text_input_v3::Event::PreeditString {
                text,
                cursor_begin,
                cursor_end,
            } => ime.state.on_preedit(text, cursor_begin, cursor_end),
            zwp_text_input_v3::Event::CommitString { text } => ime.state.on_commit_string(text),
            zwp_text_input_v3::Event::DeleteSurroundingText {
                before_length,
                after_length,
            } => ime.state.on_delete(before_length, after_length),
            zwp_text_input_v3::Event::Done { serial } => {
                let batch = ime.state.on_done(serial);
                let Some(key) = ime.focus else { return };
                ime.applying = true;
                // 顺序见 `text_input.rs`：清旧合成串 → 删周围 → 提交 → 新合成串。
                self.dispatch_preedit(key, Preedit::default());
                let (before, after) = batch.delete;
                for k in std::iter::repeat_n(Key::Backspace, before)
                    .chain(std::iter::repeat_n(Key::Delete, after))
                {
                    self.dispatch_text_key(key, k);
                }
                if let Some(text) = batch.commit {
                    for c in text.chars().filter(|c| !c.is_control()) {
                        self.dispatch_text_key(key, Key::Char(c));
                    }
                }
                if batch.preedit.is_active() {
                    self.dispatch_preedit(key, batch.preedit);
                }
                if let Some(ime) = self.ime.as_mut() {
                    ime.applying = false;
                    // 正文变了：重算周围文本。
                    ime.probe = None;
                }
                self.ime_update_now(key);
            }
            _ => {}
        }
    }

    /// 不经 `after_event`、直接问宿主当前光标后对账。
    fn ime_update_now(&mut self, key: u32) {
        let caret = self
            .idx(key)
            .and_then(|i| self.windows[i].handler.ime_caret());
        self.ime_update(key, caret);
    }

    /// 事件收尾时对账：`caret` 是宿主报的光标（物理像素：x, y_top, 行高）。
    pub(super) fn ime_update(&mut self, key: u32, caret: Option<(i32, i32, i32)>) {
        let Some(ime) = self.ime.as_ref() else { return };
        if ime.applying || ime.focus != Some(key) {
            return;
        }
        let Some(i) = self.idx(key) else { return };
        let caret = caret.filter(|_| self.blocked_by_modal(key).is_none());
        let w = &self.windows[i];
        let mut fresh = None;
        let want = caret.map(|c| {
            let sel = w.handler.ime_selection();
            let probe = (c, sel);
            match ime.want.clone().filter(|_| ime.probe == Some(probe)) {
                Some(cached) => cached,
                None => {
                    // 物理像素 → 表面逻辑坐标（与 X11 / win32 同样锚在光标上，候选窗贴其底边）。
                    let f = w.scale.factor;
                    let lg = |v: i32| (v as f64 / f).round() as i32;
                    let rect = (lg(c.0), lg(c.1), 1, lg(c.2).max(1));
                    let surrounding = sel.map(|s| surrounding(&w.handler.ime_text(), s));
                    let want = Want { rect, surrounding };
                    fresh = Some((probe, want.clone()));
                    want
                }
            }
        });
        let Some(ime) = self.ime.as_mut() else { return };
        match (&want, fresh) {
            (None, _) => {
                ime.probe = None;
                ime.want = None;
            }
            (Some(_), Some((probe, w))) => {
                ime.probe = Some(probe);
                ime.want = Some(w);
            }
            (Some(_), None) => {}
        }
        let reqs = ime.state.sync(want.as_ref());
        ime.send(reqs);
    }

    /// 合成中点击：放弃合成（本地清掉、输入法也丢掉）。
    pub(super) fn ime_abort_composition(&mut self, key: u32) {
        let composing = self.idx(key).is_some_and(|i| self.windows[i].composing);
        if !composing {
            return;
        }
        self.dispatch_preedit(key, Preedit::default());
        if let Some(ime) = self.ime.as_mut().filter(|m| m.focus == Some(key)) {
            if let Some(want) = ime.want.clone() {
                let reqs = ime.state.reset(&want);
                ime.send(reqs);
            }
        }
    }

    pub(super) fn dispatch_preedit(&mut self, key: u32, pe: Preedit) {
        let Some(i) = self.idx(key) else { return };
        let active = pe.is_active();
        // 模态挡着时只放行「清空」（收尾一段已开始的合成），不放行新的合成串（同 X11）。
        if active && self.blocked_by_modal(key).is_some() {
            return;
        }
        let w = &mut self.windows[i];
        if !active && !w.composing {
            return;
        }
        w.composing = active;
        let r = {
            let _g = crate::platform::EventDispatchGuard::enter();
            w.handler.set_ime_preedit(&pe)
        };
        if r {
            w.needs_paint = true;
        }
        self.after_event(key);
    }

    fn dispatch_text_key(&mut self, key: u32, k: Key) {
        if self.blocked_by_modal(key).is_some() {
            return;
        }
        let Some(i) = self.idx(key) else { return };
        let w = &mut self.windows[i];
        let ev = KeyEvent {
            key: k,
            pressed: true,
            shift: false,
            ctrl: false,
            alt: false,
            meta: false,
        };
        let r = {
            let _g = crate::platform::EventDispatchGuard::enter();
            w.handler.on_key(ev)
        };
        if r {
            w.needs_paint = true;
        }
        self.after_event(key);
    }
}
