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
//! - **换输入框**：宿主经 `ime_field` 报焦点控件身份，变了就 `disable` 再 `enable`（协议要求，
//!   同一表面内也算），内容类型按控件的多行 / 密码属性设。

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::text_input::zv3::client::{
    zwp_text_input_manager_v3::ZwpTextInputManagerV3,
    zwp_text_input_v3::{self, ContentHint, ContentPurpose, ZwpTextInputV3},
};

use super::text_input::{plan_update, surrounding, Req, TextInputState, Want};
use super::Wl;
use crate::event::{ImeHints, Key, KeyEvent, Preedit};

/// 对账时向宿主取的（光标，选区，焦点控件）：都没变、期间又没有按键，正文多半也没变。
type Probe = (
    (i32, i32, i32),
    Option<(usize, usize)>,
    Option<crate::event::ImeField>,
);

/// 框架的内容类型 → 协议的提示与用途。密码：用途 password，并提示「敏感、隐藏」（输入法不
/// 联想、不记忆、不显示明文候选）；多行：提示 multiline（回车是换行）。
fn content_type(h: ImeHints) -> (ContentHint, ContentPurpose) {
    if h.password {
        (
            ContentHint::SensitiveData | ContentHint::HiddenText,
            ContentPurpose::Password,
        )
    } else if h.multiline {
        (ContentHint::Multiline, ContentPurpose::Normal)
    } else {
        (ContentHint::None, ContentPurpose::Normal)
    }
}

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
                Req::ContentType(h) => {
                    let (hint, purpose) = content_type(h);
                    self.obj.set_content_type(hint, purpose)
                }
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
                let d = batch.delete;
                if !d.is_empty() {
                    // 选区里的文字保留、只删其前后：Left 把选区收拢到开头、删前面，再右移过选区、
                    // 删后面，光标停在选区之后（`TextInput` 的 Left / Right 在有选区时先收拢选区）。
                    let keys = std::iter::repeat_n(Key::Left, (d.selected > 0) as usize)
                        .chain(std::iter::repeat_n(Key::Backspace, d.before))
                        .chain(std::iter::repeat_n(Key::Right, d.selected))
                        .chain(std::iter::repeat_n(Key::Delete, d.after));
                    for k in keys {
                        self.dispatch_text_key(key, k);
                    }
                }
                let had_commit = batch.commit.is_some();
                let had_preedit = batch.preedit.is_active();
                if let Some(text) = batch.commit {
                    for c in text.chars().filter(|c| !c.is_control()) {
                        self.dispatch_text_key(key, Key::Char(c));
                    }
                }
                if batch.preedit.is_active() {
                    self.dispatch_preedit(key, batch.preedit);
                }
                let changed = !d.is_empty() || had_commit || had_preedit;
                if let Some(ime) = self.ime.as_mut() {
                    ime.applying = false;
                    // 正文变了：重算周围文本。
                    ime.probe = None;
                }
                if changed {
                    // 交给重画后的那次收尾对账：那时光标位置才是新的，周围文本与矩形一次送出
                    // （现在就对账会先按旧光标 commit 一次、重画后再 commit 一次）。
                    if let Some(i) = self.idx(key) {
                        self.windows[i].needs_paint = true;
                    }
                } else {
                    // 空批次：可能只是 serial 刚对上，压着的状态请求要现在发。
                    self.ime_update_now(key);
                }
            }
            _ => {}
        }
    }

    /// 按键改了正文：光标与选区可能都没动（Delete 键），缓存的周围文本不能再用。
    pub(super) fn ime_text_changed(&mut self) {
        if let Some(ime) = self.ime.as_mut() {
            ime.probe = None;
        }
    }

    /// 关窗：若文本焦点在它上面，本地当作 `leave`（合成器对已销毁的表面不一定再发）。
    pub(super) fn ime_window_closed(&mut self, key: u32) {
        if let Some(ime) = self.ime.as_mut().filter(|m| m.focus == Some(key)) {
            ime.state.on_leave();
            ime.focus = None;
            ime.probe = None;
            ime.want = None;
        }
    }

    /// 窗口集合变了（开了 / 关了窗，模态状态可能随之变化）：给文本焦点所在窗口补一次对账。
    pub(super) fn ime_resync_focus(&mut self) {
        if let Some(key) = self.ime.as_ref().and_then(|m| m.focus) {
            self.ime_update_now(key);
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
        let bar = self.bar_logical(i);
        let w = &self.windows[i];
        let field = caret.and(w.handler.ime_field());
        let plan = plan_update(
            ime.want.as_ref().map(|w| w.field),
            caret.map(|_| field.map_or(0, |f| f.id)),
            w.composing,
            w.needs_paint && w.visible(),
        );
        if plan.clear_preedit {
            // 合成中焦点被移到另一个控件（程序改焦点、Tab）或离开文本控件：旧框的合成串先撤掉
            // （宿主清的是合成串所在的那个节点）；输入法那边随下面的重新 enable 一并丢弃。
            // 放在推迟判断之前：否则旧框要多显示一帧合成串。
            if let Some(ime) = self.ime.as_mut() {
                ime.applying = true;
            }
            self.dispatch_preedit(key, Preedit::default());
            let Some(ime) = self.ime.as_mut() else { return };
            ime.applying = false;
            // 清合成串的回调里可能关窗、焦点跟着走了。
            if ime.focus != Some(key) {
                return;
            }
        }
        if plan.defer {
            return;
        }
        let Some(i) = self.idx(key) else { return };
        let w = &self.windows[i];
        let Some(ime) = self.ime.as_ref() else { return };
        // 缓存（光标、选区、焦点控件）没变且期间没有按键：沿用上次算好的 Want，不重算、不复制。
        let fresh = caret.and_then(|c| {
            let sel = w.handler.ime_selection();
            let probe = (c, sel, field);
            if ime.probe == Some(probe) && ime.want.is_some() {
                return None;
            }
            // 物理像素 → 表面逻辑坐标（与 X11 / win32 同样锚在光标上，候选窗贴其底边）；宿主
            // 报的是内容区坐标，矩形要的是表面坐标，下移客户端标题栏的高度。
            let rect = super::csd::ime_rect(c, w.scale.factor, bar);
            let hints = field.map(|f| f.hints).unwrap_or_default();
            // 密码框不发周围文本（宿主本就给空正文，这里连空串也不发）。
            let surrounding = sel
                .filter(|_| !hints.password)
                .map(|s| surrounding(&w.handler.ime_text(), s));
            let want = Want {
                rect,
                surrounding,
                field: field.map_or(0, |f| f.id),
                hints,
            };
            Some((want, probe))
        });
        let Some(ime) = self.ime.as_mut() else { return };
        match (caret, fresh) {
            (None, _) => {
                ime.probe = None;
                ime.want = None;
            }
            (Some(_), Some((want, probe))) => {
                ime.probe = Some(probe);
                ime.want = Some(want);
            }
            (Some(_), None) => {}
        }
        let reqs = ime.state.sync(ime.want.as_ref());
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
