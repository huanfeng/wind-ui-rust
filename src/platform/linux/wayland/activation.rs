//! 把已显示的窗口提到前台：`xdg-activation-v1`。
//!
//! Wayland 不许应用自己抢焦点，得拿「激活令牌」换：
//! - **本进程有近期输入**（按下键盘键、按下鼠标键给的 serial；松开、键盘焦点进入、触摸都不算——
//!   mutter 按「按下」那一下的 serial 校验，推断自其源码、GNOME 上待实测）：`get_activation_token` →
//!   `set_serial(serial, seat)` + `set_surface(有焦点的那扇窗)` + `set_app_id` → `commit`，收到
//!   `done(令牌)` 后对目标窗口 `activate(令牌, 表面)`。异步：不等，`done` 到了再激活；那之前目标
//!   窗口关了就丢掉。同一扇窗口已有一个在路上的请求时不再发（连着两次唤出同一扇窗口只要一个令牌）。
//! - **单实例二次启动**：桌面启动器给第二个进程设了 `XDG_ACTIVATION_TOKEN`，它随 argv 转过来
//!   （`single_instance::attach_activation_token`），直接拿来 `activate`，不用自己请求。本进程
//!   自己启动时环境里的那一个，用来激活主窗口。
//! - **既没令牌也没近期输入**：照样请求一个（不带 serial）。合成器多半不给焦点，只把窗口标成
//!   「需要注意」（GNOME 弹「已就绪」通知、任务栏高亮）——协议预期的行为，不是失败。
//!
//! 合成器没有这个协议（很少见）：维持原状（不提到前台），stderr 提示一次。

use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::{
    xdg_activation_token_v1::{self, XdgActivationTokenV1},
    xdg_activation_v1::XdgActivationV1,
};

use super::super::host;
use super::Wl;

/// 记下最近一次输入：只认按下（松开的 serial 合成器不认，而点击回调多在松开时触发，要是让松开
/// 覆盖了按下，申请到的令牌就换不来焦点）；不知道落在哪扇窗口时保留旧值。
pub(super) fn note_input(
    prev: Option<(u32, u32)>,
    serial: u32,
    window: Option<u32>,
    press: bool,
) -> Option<(u32, u32)> {
    match window {
        Some(k) if press => Some((serial, k)),
        _ => prev,
    }
}

/// 唤出窗口的步骤：（先照常显示，再激活）。已显示的直接激活；隐藏着的照常映射（合成器一般给
/// 新映射的窗口焦点），手里有别处给的令牌才再激活一次。
pub(super) fn raise_steps(shown: bool, has_token: bool) -> (bool, bool) {
    if shown {
        (false, true)
    } else {
        (true, has_token)
    }
}

/// 「哪些窗口有令牌请求在路上」的簿记（纯逻辑，有单测）。
#[derive(Default)]
pub(super) struct Pending {
    keys: Vec<u32>,
}

impl Pending {
    /// 想激活 `key`：返回是否要发一个新请求（已有一个在路上就不发，等它）。
    pub fn want(&mut self, key: u32) -> bool {
        if self.keys.contains(&key) {
            return false;
        }
        self.keys.push(key);
        true
    }

    /// `key` 的令牌到了：返回是否还要激活它（请求发出后窗口关了就不要）。
    pub fn done(&mut self, key: u32) -> bool {
        let Some(pos) = self.keys.iter().position(|&k| k == key) else {
            return false;
        };
        self.keys.remove(pos);
        true
    }

    /// 窗口关了或隐藏了：在路上的请求作废。
    pub fn forget(&mut self, key: u32) {
        self.keys.retain(|&k| k != key);
    }
}

pub(super) struct Activation {
    obj: XdgActivationV1,
    pending: Pending,
}

impl Activation {
    pub fn new(obj: XdgActivationV1) -> Self {
        Self {
            obj,
            pending: Pending::default(),
        }
    }
}

impl Dispatch<XdgActivationV1, ()> for Wl {
    fn event(
        _: &mut Self,
        _: &XdgActivationV1,
        _: <XdgActivationV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

/// 令牌请求的 user data：要激活的窗口号。
impl Dispatch<XdgActivationTokenV1, u32> for Wl {
    fn event(
        state: &mut Self,
        token: &XdgActivationTokenV1,
        event: xdg_activation_token_v1::Event,
        key: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_activation_token_v1::Event::Done { token: t } = event {
            token.destroy();
            let wanted = state
                .activation
                .as_mut()
                .is_some_and(|a| a.pending.done(*key));
            if wanted {
                state.activate_with(*key, &t);
            }
        }
    }
}

impl Wl {
    /// 把窗口提到前台。`token`：别处给的激活令牌（单实例二次启动转来的）；没有就自己请求一个。
    pub(super) fn activate(&mut self, key: u32, token: Option<String>) {
        if self.activation.is_none() {
            if !std::mem::replace(&mut self.activation_warned, true) {
                eprintln!("[windui] xdg-activation-v1 不可用，无法把已显示的窗口提到前台");
            }
            return;
        }
        if let Some(t) = token {
            self.activate_with(key, &t);
            return;
        }
        let Some(a) = self.activation.as_mut() else {
            return;
        };
        if !a.pending.want(key) {
            return;
        }
        let req = a.obj.get_activation_token(&self.qh, key);
        // 有近期输入就带上它的 serial 与那扇窗口：合成器凭此认定这是用户操作的结果，才肯把焦点
        // 交出去。没有 serial 也报上有键盘焦点的窗口（KWin 主要看申请方是不是当前活动窗口）。
        let mut from = self.keyboard.as_ref().and_then(|k| k.focus);
        if let (Some((serial, k)), Some(seat)) = (self.last_input, &self.g.seat) {
            req.set_serial(serial, seat);
            from = Some(k);
        }
        if let Some(i) = from.and_then(|k| self.idx(k)) {
            req.set_surface(&self.windows[i].surface);
        }
        req.set_app_id(host::exe_name());
        req.commit();
    }

    fn activate_with(&mut self, key: u32, token: &str) {
        let (Some(a), Some(i)) = (self.activation.as_ref(), self.idx(key)) else {
            return;
        };
        if self.windows[i].role.is_none() {
            return; // 已隐藏：没有可激活的窗口（再显示时会重新申请）
        }
        a.obj.activate(token.to_string(), &self.windows[i].surface);
    }

    /// 窗口关了或隐藏了：在路上的令牌请求作废。
    pub(super) fn activation_window_closed(&mut self, key: u32) {
        if let Some(a) = self.activation.as_mut() {
            a.pending.forget(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_press_on_a_known_window_counts_as_recent_input() {
        assert_eq!(note_input(None, 5, Some(1), true), Some((5, 1)));
        assert_eq!(
            note_input(Some((5, 1)), 6, Some(1), false),
            Some((5, 1)),
            "松开不覆盖按下"
        );
        assert_eq!(
            note_input(Some((5, 1)), 7, None, true),
            Some((5, 1)),
            "不知道落在哪扇窗口：保留旧值"
        );
        assert_eq!(note_input(Some((5, 1)), 8, Some(2), true), Some((8, 2)));
    }

    #[test]
    fn raising_a_shown_window_activates_it_and_a_hidden_one_is_shown_first() {
        assert_eq!(
            raise_steps(true, false),
            (false, true),
            "已显示：申请令牌激活"
        );
        assert_eq!(raise_steps(true, true), (false, true));
        assert_eq!(
            raise_steps(false, true),
            (true, true),
            "隐藏 + 令牌：显示后再激活"
        );
        assert_eq!(
            raise_steps(false, false),
            (true, false),
            "隐藏无令牌：照常显示"
        );
    }

    #[test]
    fn one_request_per_window_until_its_token_arrives() {
        let mut p = Pending::default();
        assert!(p.want(1), "第一次：发请求");
        assert!(!p.want(1), "同一窗口还在等：不再发");
        assert!(p.want(2), "别的窗口照发");
        assert!(p.done(1), "令牌到了：激活");
        assert!(p.want(1), "之后再要激活：重新请求");
    }

    #[test]
    fn a_window_closed_before_its_token_arrives_is_not_activated() {
        let mut p = Pending::default();
        p.want(1);
        p.forget(1);
        assert!(!p.done(1), "窗口已关：令牌丢弃");
        assert!(!p.done(9), "从没请求过的：不激活");
    }
}
