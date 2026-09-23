//! 输入法：XIM 客户端（经 `xim` crate），对接 fcitx5 / ibus 等的 XIM 前端。
//!
//! 取「合成串回调」风格（`XIMPreeditCallbacks`）：合成串由输入法经 `PreeditDraw` 增量
//! 推给我们，交宿主的 `set_ime_preedit` 由 `TextInput` **内联绘制**——与 macOS 同一条
//! 通路（见 `docs/ime-preedit-design.md`）。候选窗位置经 IC 的 `SpotLocation` 告诉输入法。
//!
//! # 按键流向
//!
//! 有 IC 的窗口收到的按键**先转给输入法**（`ForwardEvent`），输入法不要的再原样转回来
//! （`handle_forward_event`），那时才走本地的键位翻译。异步一来一回，但只在本机进程间，
//! 感觉不到延迟。连不上输入法（没设 `XMODIFIERS`、输入法没启动）时整个模块不存在，
//! 按键直接本地处理。
//!
//! 回调里不碰窗口：`ClientHandler` 的各方法只把结果记进 [`Bridge::out`]，由事件循环在
//! `filter_event` 返回后逐条派给对应窗口——回调运行时 `X11rbClient` 正被可变借用着，
//! 在里面派发事件会让宿主回调与 XIM 状态机交错，时序难以推理。

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use x11rb::protocol::xproto::{KeyPressEvent, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use xim::x11rb::X11rbClient;
use xim::{
    AttributeName, CaretDirection, CaretStyle, Client, ClientCore, ClientError, ClientHandler,
    Feedback, ForwardEventFlag, InputStyle, Point, PreeditDrawStatus, Request,
};

use crate::event::Preedit;

type Xim = X11rbClient<Rc<RustConnection>>;

/// 回调产出，交事件循环派发。
pub(super) enum ImeOut {
    /// 输入法就绪（`Open` + 查过输入风格），可以给窗口建 IC 了。
    Ready,
    /// IC 建好（按建 IC 请求的先后次序对应到窗口）。
    IcCreated(u16),
    Commit(u16, String),
    Preedit(u16, Preedit),
    /// 输入法不处理、原样转回的按键（按下或松开）。
    Forward(KeyPressEvent),
}

#[derive(Default)]
pub(super) struct Bridge {
    im_id: u16,
    locale: String,
    pub out: Vec<ImeOut>,
    /// 每个 IC 当前的合成串（字符）与光标——`PreeditDraw` 是增量替换，必须自己留底。
    preedit: HashMap<u16, (Vec<char>, usize)>,
}

impl Bridge {
    fn emit_preedit(&mut self, ic: u16) {
        let (chars, caret) = self.preedit.get(&ic).cloned().unwrap_or_default();
        let pe = Preedit {
            text: chars.iter().collect(),
            caret: caret.min(chars.len()),
            sel: None,
        };
        self.out.push(ImeOut::Preedit(ic, pe));
    }
}

impl ClientHandler<Xim> for Bridge {
    fn handle_connect(&mut self, client: &mut Xim) -> Result<(), ClientError> {
        client.open(&self.locale)
    }

    fn handle_open(&mut self, client: &mut Xim, input_method_id: u16) -> Result<(), ClientError> {
        self.im_id = input_method_id;
        client.get_im_values(input_method_id, &[AttributeName::QueryInputStyle])
    }

    fn handle_get_im_values(
        &mut self,
        _client: &mut Xim,
        _input_method_id: u16,
        _attributes: xim::AHashMap<AttributeName, Vec<u8>>,
    ) -> Result<(), ClientError> {
        self.out.push(ImeOut::Ready);
        Ok(())
    }

    fn handle_create_ic(
        &mut self,
        _client: &mut Xim,
        _input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.out.push(ImeOut::IcCreated(input_context_id));
        Ok(())
    }

    fn handle_commit(
        &mut self,
        _client: &mut Xim,
        _input_method_id: u16,
        input_context_id: u16,
        text: &str,
    ) -> Result<(), ClientError> {
        self.out
            .push(ImeOut::Commit(input_context_id, text.to_string()));
        Ok(())
    }

    fn handle_forward_event(
        &mut self,
        _client: &mut Xim,
        _input_method_id: u16,
        _input_context_id: u16,
        _flag: ForwardEventFlag,
        xev: KeyPressEvent,
    ) -> Result<(), ClientError> {
        self.out.push(ImeOut::Forward(xev));
        Ok(())
    }

    fn handle_preedit_start(
        &mut self,
        _client: &mut Xim,
        _input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.preedit.insert(input_context_id, (Vec::new(), 0));
        Ok(())
    }

    fn handle_preedit_draw(
        &mut self,
        _client: &mut Xim,
        _input_method_id: u16,
        input_context_id: u16,
        caret: i32,
        chg_first: i32,
        chg_len: i32,
        status: PreeditDrawStatus,
        preedit_string: &str,
        _feedbacks: Vec<Feedback>,
    ) -> Result<(), ClientError> {
        let entry = self.preedit.entry(input_context_id).or_default();
        let insert: Vec<char> = if status.contains(PreeditDrawStatus::NO_STRING) {
            Vec::new()
        } else {
            preedit_string.chars().collect()
        };
        entry.0 = splice(&entry.0, chg_first, chg_len, &insert);
        entry.1 = caret.max(0) as usize;
        self.emit_preedit(input_context_id);
        Ok(())
    }

    fn handle_preedit_caret(
        &mut self,
        _client: &mut Xim,
        _input_method_id: u16,
        input_context_id: u16,
        position: &mut i32,
        direction: CaretDirection,
        _style: CaretStyle,
    ) -> Result<(), ClientError> {
        let entry = self.preedit.entry(input_context_id).or_default();
        let len = entry.0.len();
        entry.1 = match direction {
            CaretDirection::AbsolutePosition => (*position).max(0) as usize,
            CaretDirection::ForwardChar => (entry.1 + 1).min(len),
            CaretDirection::BackwardChar => entry.1.saturating_sub(1),
            CaretDirection::LineStart => 0,
            CaretDirection::LineEnd => len,
            _ => entry.1,
        }
        .min(len);
        *position = entry.1 as i32;
        self.emit_preedit(input_context_id);
        Ok(())
    }

    fn handle_preedit_done(
        &mut self,
        _client: &mut Xim,
        _input_method_id: u16,
        input_context_id: u16,
    ) -> Result<(), ClientError> {
        self.preedit.remove(&input_context_id);
        self.out
            .push(ImeOut::Preedit(input_context_id, Preedit::default()));
        Ok(())
    }
}

/// `PreeditDraw` 的增量语义：把 `[first, first+len)` 这段替换成 `insert`。越界参数按
/// 截断处理——输入法给错了也只影响显示，不能 panic。
fn splice(cur: &[char], first: i32, len: i32, insert: &[char]) -> Vec<char> {
    let first = (first.max(0) as usize).min(cur.len());
    let end = (first + len.max(0) as usize).min(cur.len());
    let mut out = Vec::with_capacity(cur.len() - (end - first) + insert.len());
    out.extend_from_slice(&cur[..first]);
    out.extend_from_slice(insert);
    out.extend_from_slice(&cur[end..]);
    out
}

/// XIM 连接与窗口 ↔ IC 的对应。
pub(super) struct Ime {
    client: Xim,
    pub bridge: Bridge,
    ready: bool,
    ics: HashMap<Window, u16>,
    /// 已发出 `CreateIc`、尚未收到回复的窗口（按发出次序）。
    creating: VecDeque<Window>,
    /// 输入法还没就绪时就建出来的窗口，就绪后补建 IC。
    waiting: Vec<Window>,
    spots: HashMap<Window, (i16, i16)>,
    /// 当前有键盘焦点的窗口。IC 建好时只给它设焦点——后台新开的窗口不该抢输入法焦点。
    focused: Option<Window>,
}

/// `creating` 里占位用的「窗口已销毁」标记：IC 回复到达时直接销毁这个 IC，不登记。
const DEAD: Window = 0;

impl Ime {
    /// 连接 `XMODIFIERS` 指定的输入法。没配置或连不上返回 `None`（按键走本地）。
    pub fn connect(conn: Rc<RustConnection>, screen: usize) -> Option<Self> {
        let client = match X11rbClient::init(conn, screen, None) {
            Ok(c) => c,
            Err(e) => {
                log::info!(
                    "未连接 XIM 输入法（{e}）；XMODIFIERS={:?}",
                    std::env::var("XMODIFIERS").ok()
                );
                return None;
            }
        };
        Some(Self {
            client,
            bridge: Bridge {
                locale: xim_locale(),
                ..Default::default()
            },
            ready: false,
            ics: HashMap::new(),
            creating: VecDeque::new(),
            waiting: Vec::new(),
            spots: HashMap::new(),
            focused: None,
        })
    }

    /// 让 XIM 状态机先看一眼事件。返回 true = 这是 XIM 协议事件，调用方不必再处理。
    pub fn filter(&mut self, ev: &Event) -> bool {
        let mut b = std::mem::take(&mut self.bridge);
        let r = self.client.filter_event(ev, &mut b);
        self.bridge = b;
        match r {
            Ok(consumed) => consumed,
            Err(e) => {
                log::warn!("XIM 协议错误：{e}");
                true
            }
        }
    }

    /// 取走回调产出，并就地处理只关乎 XIM 自身的那几种（就绪、IC 建好）。
    pub fn take_out(&mut self) -> Vec<ImeOut> {
        let out = std::mem::take(&mut self.bridge.out);
        let mut rest = Vec::with_capacity(out.len());
        for o in out {
            match o {
                ImeOut::Ready => {
                    self.ready = true;
                    for w in std::mem::take(&mut self.waiting) {
                        self.create_ic(w);
                    }
                }
                ImeOut::IcCreated(ic) => match self.creating.pop_front() {
                    // 回复到达前窗口已销毁：IC 无主，就地销毁以免泄漏。
                    Some(DEAD) | None => {
                        let _ = self.client.destroy_ic(self.bridge.im_id, ic);
                    }
                    Some(w) => {
                        self.ics.insert(w, ic);
                        // FocusIn 往往早于 IC 回复，那一下没有 IC 可设，在这里补上。
                        if self.focused == Some(w) {
                            let _ = self.client.set_focus(self.bridge.im_id, ic);
                        }
                    }
                },
                other => rest.push(other),
            }
        }
        rest
    }

    pub fn create_ic(&mut self, win: Window) {
        if !self.ready {
            self.waiting.push(win);
            return;
        }
        let attrs = self
            .client
            .build_ic_attributes()
            .push(
                AttributeName::InputStyle,
                InputStyle::PREEDIT_CALLBACKS | InputStyle::STATUS_NOTHING,
            )
            .push(AttributeName::ClientWindow, win)
            .push(AttributeName::FocusWindow, win)
            .nested_list(AttributeName::PreeditAttributes, |b| {
                b.push(AttributeName::SpotLocation, Point { x: 0, y: 0 });
            })
            .build();
        if self.client.create_ic(self.bridge.im_id, attrs).is_ok() {
            self.creating.push_back(win);
        }
    }

    pub fn destroy_ic(&mut self, win: Window) {
        self.waiting.retain(|w| *w != win);
        for w in self.creating.iter_mut() {
            if *w == win {
                *w = DEAD;
            }
        }
        if self.focused == Some(win) {
            self.focused = None;
        }
        self.spots.remove(&win);
        if let Some(ic) = self.ics.remove(&win) {
            let _ = self.client.destroy_ic(self.bridge.im_id, ic);
        }
    }

    pub fn window_of(&self, ic: u16) -> Option<Window> {
        self.ics.iter().find(|(_, v)| **v == ic).map(|(k, _)| *k)
    }

    /// 把按键交给输入法。窗口没有 IC（还没建好）时返回 false，调用方本地处理。
    pub fn forward_key(&mut self, ev: &KeyPressEvent) -> bool {
        let Some(&ic) = self.ics.get(&ev.event) else {
            return false;
        };
        self.client
            .forward_event(self.bridge.im_id, ic, ForwardEventFlag::empty(), ev)
            .is_ok()
    }

    pub fn focus(&mut self, win: Window, on: bool) {
        if on {
            self.focused = Some(win);
        } else if self.focused == Some(win) {
            self.focused = None;
        }
        if let Some(&ic) = self.ics.get(&win) {
            let _ = if on {
                self.client.set_focus(self.bridge.im_id, ic)
            } else {
                self.client.unset_focus(self.bridge.im_id, ic)
            };
        }
    }

    /// 放弃当前合成（点击别处、窗口失活时）。本地合成串由调用方清掉。
    pub fn reset(&mut self, win: Window) {
        if let Some(&ic) = self.ics.get(&win) {
            self.bridge.preedit.remove(&ic);
            let _ = self.client.send_req(Request::ResetIc {
                input_method_id: self.bridge.im_id,
                input_context_id: ic,
            });
        }
    }

    /// 更新候选窗锚点（窗口内物理像素：光标底边）。值没变就不发。
    pub fn set_spot(&mut self, win: Window, x: i32, y: i32) {
        let Some(&ic) = self.ics.get(&win) else {
            return;
        };
        let p = (
            x.clamp(0, i16::MAX as i32) as i16,
            y.clamp(0, i16::MAX as i32) as i16,
        );
        if self.spots.get(&win) == Some(&p) {
            return;
        }
        self.spots.insert(win, p);
        let attrs = self
            .client
            .build_ic_attributes()
            .nested_list(AttributeName::PreeditAttributes, |b| {
                b.push(AttributeName::SpotLocation, Point { x: p.0, y: p.1 });
            })
            .build();
        let _ = self.client.set_ic_values(self.bridge.im_id, ic, attrs);
    }
}

/// 交给 `XIM_OPEN` 的 locale：取 `LC_ALL`/`LC_CTYPE`/`LANG` 去掉编码后缀，缺省 `C`。
fn xim_locale() -> String {
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .filter_map(|k| std::env::var(k).ok())
        .find(|v| !v.is_empty())
        .and_then(|v| v.split('.').next().map(str::to_string))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "C".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Vec<char> {
        v.chars().collect()
    }

    #[test]
    fn preedit_draw_replaces_the_changed_span() {
        assert_eq!(splice(&s(""), 0, 0, &s("ni")), s("ni"));
        assert_eq!(splice(&s("ni"), 0, 2, &s("ni'h")), s("ni'h"));
        assert_eq!(splice(&s("abcd"), 1, 2, &s("X")), s("aXd"));
        assert_eq!(splice(&s("abcd"), 2, 0, &s("X")), s("abXcd"));
    }

    #[test]
    fn out_of_range_draw_is_clamped_not_panicking() {
        assert_eq!(splice(&s("ab"), 5, 3, &s("X")), s("abX"));
        assert_eq!(splice(&s("ab"), -1, 9, &s("")), s(""));
    }
}
