//! 输入的纯逻辑部分（可单测）：按键重复计时、滚轮帧聚合、按钮与缩放边的协议编号换算。
//! 协议对象的收发在 `events.rs`。

use std::time::{Duration, Instant};

use crate::event::MouseButton;
use wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge;

/// 客户端自实现的按键重复。
///
/// Wayland 服务端**不发**重复事件，只在 `repeat_info` 里告诉客户端速率（次/秒）与延迟（ms），
/// 由客户端自己计时。计时并进事件循环的 `poll` 超时（[`Self::deadline`]），不另起线程、
/// 不空转。
#[derive(Debug)]
pub(super) struct KeyRepeat {
    /// 每秒次数；0 = 不重复（协议约定）。
    rate: i32,
    delay: Duration,
    /// 正在重复的键（XKB 键码）与下次触发时刻。
    active: Option<(u32, Instant)>,
}

impl Default for KeyRepeat {
    /// 合成器发 `repeat_info` 之前（v4 以下根本不发）的默认值：25 次/秒、600ms，
    /// 与 X 服务器和 GNOME 的出厂值一致。
    fn default() -> Self {
        Self {
            rate: 25,
            delay: Duration::from_millis(600),
            active: None,
        }
    }
}

impl KeyRepeat {
    pub fn set_info(&mut self, rate: i32, delay_ms: i32) {
        self.rate = rate.max(0);
        self.delay = Duration::from_millis(delay_ms.max(0) as u64);
        if self.rate == 0 {
            self.active = None;
        }
    }

    fn interval(&self) -> Duration {
        Duration::from_micros(1_000_000 / self.rate.max(1) as u64)
    }

    /// 按下一个会重复的键：从现在起等 `delay` 后开始重复。新按的键接替旧的（同 X / GTK）。
    pub fn press(&mut self, key: u32, now: Instant) {
        self.active = (self.rate > 0).then_some((key, now + self.delay));
    }

    /// 松开：只有松开的正是在重复的那个键才停（按着 A 再按 B 松 A，B 继续重复）。
    pub fn release(&mut self, key: u32) {
        if self.active.is_some_and(|(k, _)| k == key) {
            self.active = None;
        }
    }

    /// 失焦、换 keymap：无条件停。
    pub fn stop(&mut self) {
        self.active = None;
    }

    /// 到期了就返回要重复的键并排好下一次。一次最多补一下：循环被阻塞很久后恢复时，
    /// 不补发一串（那会让文本框里突然冒出几十个字符）。
    pub fn due(&mut self, now: Instant) -> Option<u32> {
        let (key, at) = self.active?;
        if now < at {
            return None;
        }
        let mut next = at + self.interval();
        if next <= now {
            next = now + self.interval();
        }
        self.active = Some((key, next));
        Some(key)
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.active.map(|(_, at)| at)
    }
}

/// 一个 `wl_pointer.frame` 之内的纵向滚轮量，frame 到了再合成一次 `Wheel`。
///
/// 同一次滚动合成器可能同时发 `axis`（连续量）与 `axis_value120` / `axis_discrete`（格数）：
/// 有格数就只认格数（鼠标滚轮），没有才按连续量换算（触控板两指滚动）。符号与框架相反：
/// Wayland 正值 = 向下，`PointerKind::Wheel` 正值 = 上滚。
#[derive(Debug, Default)]
pub(super) struct WheelFrame {
    value120: Option<i32>,
    continuous: f64,
    /// 连续量换算后不足 1 的零头，留给下一帧（触控板慢慢滑时不丢量）。
    carry: f64,
}

/// 连续滚动量（表面坐标单位）换成 1/120 格：libinput 一格约 15 单位（mutter / weston 报 10–15），
/// 取 10 单位 = 一格，与 GTK / winit 的换算一致。
const CONTINUOUS_PER_NOTCH: f64 = 10.0;

impl WheelFrame {
    pub fn axis(&mut self, value: f64) {
        self.continuous += value;
    }

    /// `axis_value120`（v8+）。
    pub fn value120(&mut self, v: i32) {
        *self.value120.get_or_insert(0) += v;
    }

    /// `axis_discrete`（v5–v7，格数）。v8 起合成器改发 value120，两者不会同时出现。
    pub fn discrete(&mut self, steps: i32) {
        self.value120(steps * 120);
    }

    /// frame 结束：返回框架口径的滚动量（正=上滚），没有滚动为 `None`。
    pub fn take(&mut self) -> Option<i32> {
        let v120 = self.value120.take();
        let cont = std::mem::take(&mut self.continuous);
        let delta = match v120 {
            Some(v) => {
                self.carry = 0.0;
                -v
            }
            None if cont != 0.0 => {
                let base = -cont * 120.0 / CONTINUOUS_PER_NOTCH;
                // 换了方向：上一方向攒的零头作废，不拿来抵消新方向。
                if base * self.carry < 0.0 {
                    self.carry = 0.0;
                }
                let exact = base + self.carry;
                let whole = exact.trunc();
                self.carry = exact - whole;
                whole as i32
            }
            None => return None,
        };
        (delta != 0).then_some(delta)
    }
}

/// Linux evdev 按钮码 → 框架按钮（`BTN_LEFT` = 0x110 起）。侧键等其它按钮不报。
pub(super) fn mouse_button(code: u32) -> Option<MouseButton> {
    match code {
        0x110 => Some(MouseButton::Left),
        0x111 => Some(MouseButton::Right),
        0x112 => Some(MouseButton::Middle),
        _ => None,
    }
}

/// `host::edge_direction` 的方向码（0 = 左上，顺时针到 7 = 左）→ `xdg_toplevel.resize` 的边。
pub(super) fn resize_edge(dir: u32) -> ResizeEdge {
    match dir {
        0 => ResizeEdge::TopLeft,
        1 => ResizeEdge::Top,
        2 => ResizeEdge::TopRight,
        3 => ResizeEdge::Right,
        4 => ResizeEdge::BottomRight,
        5 => ResizeEdge::Bottom,
        6 => ResizeEdge::BottomLeft,
        7 => ResizeEdge::Left,
        _ => ResizeEdge::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn repeat_waits_delay_then_fires_at_rate() {
        let t0 = Instant::now();
        let mut r = KeyRepeat::default();
        r.set_info(20, 400); // 50ms 一次
        r.press(38, t0);
        assert_eq!(r.deadline(), Some(t0 + ms(400)));
        assert_eq!(r.due(t0 + ms(399)), None, "延迟未到");
        assert_eq!(r.due(t0 + ms(400)), Some(38));
        assert_eq!(r.deadline(), Some(t0 + ms(450)));
        assert_eq!(r.due(t0 + ms(449)), None);
        assert_eq!(r.due(t0 + ms(450)), Some(38));
    }

    #[test]
    fn repeat_stops_on_release_of_that_key_and_on_focus_loss() {
        let t0 = Instant::now();
        let mut r = KeyRepeat::default();
        r.press(38, t0);
        r.press(39, t0 + ms(10));
        r.release(38);
        assert!(r.deadline().is_some(), "松开的不是在重复的键：继续");
        r.release(39);
        assert_eq!(r.deadline(), None, "松键即停");
        r.press(40, t0);
        r.stop();
        assert_eq!(r.due(t0 + ms(5000)), None, "失焦即停");
    }

    #[test]
    fn rate_zero_disables_repeat() {
        let t0 = Instant::now();
        let mut r = KeyRepeat::default();
        r.press(38, t0);
        r.set_info(0, 300);
        assert_eq!(r.deadline(), None, "改成不重复：进行中的也停");
        r.press(38, t0);
        assert_eq!(r.due(t0 + ms(10_000)), None);
    }

    #[test]
    fn stalled_loop_fires_once_not_a_burst() {
        let t0 = Instant::now();
        let mut r = KeyRepeat::default();
        r.set_info(25, 600);
        r.press(38, t0);
        let late = t0 + ms(5000);
        assert_eq!(r.due(late), Some(38));
        assert_eq!(r.due(late), None, "只补一下");
        assert_eq!(r.deadline(), Some(late + ms(40)));
    }

    #[test]
    fn wheel_prefers_notches_and_flips_sign() {
        let mut f = WheelFrame::default();
        f.axis(15.0);
        f.value120(120);
        assert_eq!(f.take(), Some(-120), "向下一格 = 框架的 -120");
        f.discrete(-2);
        assert_eq!(f.take(), Some(240));
        assert_eq!(f.take(), None, "空 frame 不报");
    }

    #[test]
    fn wheel_high_res_partial_notch_passes_through() {
        let mut f = WheelFrame::default();
        f.value120(30);
        assert_eq!(f.take(), Some(-30));
    }

    #[test]
    fn continuous_scroll_keeps_the_fraction() {
        let mut f = WheelFrame::default();
        f.axis(0.05); // 0.6 / 120 格
        assert_eq!(f.take(), None, "不足 1 的零头留着");
        f.axis(0.05);
        assert_eq!(f.take(), Some(-1), "攒够了才报");
        f.axis(-10.0);
        assert_eq!(f.take(), Some(120));
    }

    #[test]
    fn buttons_and_edges_map_to_protocol_codes() {
        assert_eq!(mouse_button(0x110), Some(MouseButton::Left));
        assert_eq!(mouse_button(0x111), Some(MouseButton::Right));
        assert_eq!(mouse_button(0x113), None);
        assert_eq!(resize_edge(0), ResizeEdge::TopLeft);
        assert_eq!(resize_edge(3), ResizeEdge::Right);
        assert_eq!(resize_edge(5), ResizeEdge::Bottom);
    }
}
