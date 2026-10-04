//! 把已显示的窗口提到前台：`xdg-activation-v1`。
//!
//! Wayland 不许应用自己抢焦点，得拿「激活令牌」换：
//! - **本进程有近期输入**（按键、点击、键盘焦点进入给的 serial）：`get_activation_token` →
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

    /// 窗口关了：在路上的请求作废。
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
                eprintln!("[windui] 合成器不支持 xdg-activation-v1，无法把已显示的窗口提到前台");
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
        // 有近期输入就带上它的 serial 与那扇有焦点的窗口：合成器凭此认定这是用户操作的结果，
        // 才肯把焦点交出去。
        if let (Some((serial, from)), Some(seat)) = (self.last_input, &self.g.seat) {
            req.set_serial(serial, seat);
            if let Some(i) = self.idx(from) {
                req.set_surface(&self.windows[i].surface);
            }
        }
        req.set_app_id(host::exe_name());
        req.commit();
    }

    fn activate_with(&mut self, key: u32, token: &str) {
        let (Some(a), Some(i)) = (self.activation.as_ref(), self.idx(key)) else {
            return;
        };
        a.obj.activate(token.to_string(), &self.windows[i].surface);
    }

    /// 窗口关了：在路上的令牌请求作废。
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
