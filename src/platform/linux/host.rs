//! 与显示协议无关的宿主簿记：X11 与 Wayland 两个后端共用。
//!
//! 只收「换了协议也一字不改」的部分：点击计数、interval 定时器、动画帧配速、
//! 一帧的渲染与脏区计算、`after_event` 要从宿主取走的意图、像素换序。怎么建窗、
//! 怎么上屏、窗口操作落到哪条协议请求上，仍各归各的后端。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;

use super::keys;
use crate::event::{CursorShape, HotkeyOp, Key, KeyEvent, Mods, WindowOp};
use crate::geometry::{Color, Point, Rect, Size};
use crate::platform::{to_skia_color, AppHandler, DialogRequest, NewWindow};

/// 动画帧间隔（ms）。软件呈现拿不到可靠的刷新率，按 60Hz。
pub(super) const FRAME_MS: u64 = 16;
/// 控件自报的「下次变化」最长只睡这么久（与 win32 `MAX_FRAME_DELAY_MS` 同值）。
pub(super) const MAX_FRAME_DELAY_MS: u64 = 5000;
/// 双击判定：时限与漂移（逻辑像素，按 scale 换算）。与 `platform::double_click_thresholds` 一致。
pub(super) const DOUBLE_CLICK_MS: u32 = 400;
pub(super) const DOUBLE_CLICK_SLOP: f32 = 4.0;

/// 动画窗口两帧之间至少隔多久：`max(刷新率间隔, 控件自报的下次变化时刻)`。
pub(super) fn anim_frame_interval(ask_ms: u64) -> Duration {
    Duration::from_millis(FRAME_MS.max(ask_ms.min(MAX_FRAME_DELAY_MS)))
}

#[derive(Default)]
pub(super) struct ClickTracker {
    time: u32,
    pos: (i32, i32),
    button: u8,
    count: u8,
}

impl ClickTracker {
    /// 折进 1 / 2 的循环（见 `PointerEvent::click_count`）。
    pub fn press(&mut self, time: u32, pos: (i32, i32), button: u8, slop: i32) -> u8 {
        let near = (pos.0 - self.pos.0).abs() <= slop && (pos.1 - self.pos.1).abs() <= slop;
        let quick = time.wrapping_sub(self.time) <= DOUBLE_CLICK_MS;
        self.count = if near && quick && button == self.button && self.count == 1 {
            2
        } else {
            1
        };
        self.time = time;
        self.pos = pos;
        self.button = button;
        self.count
    }
}

/// 宿主声明的周期定时器（`AppHandler::intervals`），每项是（周期，下次到期）。
pub(super) struct Intervals(Vec<(Duration, Instant)>);

impl Intervals {
    pub fn new(periods: Vec<Duration>, now: Instant) -> Self {
        Self(
            periods
                .into_iter()
                // 下限 10ms（同 win32 `SetTimer` 的最小周期）：周期 0 会让循环超时恒为 0、空转。
                .map(|d| d.max(Duration::from_millis(10)))
                .map(|d| (d, now + d))
                .collect(),
        )
    }

    /// 取出到期项的下标并推进它们的截止；`next` 收拢（推进后的）最早截止。
    pub fn fire_due(&mut self, now: Instant, next: &mut Option<Instant>) -> Vec<usize> {
        let mut fired = Vec::new();
        for (idx, (period, due)) in self.0.iter_mut().enumerate() {
            if now >= *due {
                fired.push(idx);
                // 按周期推进而不是从 now 起算，但落后太多时直接对齐到 now（挂起恢复后不补发一串）。
                *due += *period;
                if *due < now {
                    *due = now + *period;
                }
            }
            *next = Some(next.map_or(*due, |n: Instant| n.min(*due)));
        }
        fired
    }
}

/// 把宿主画进 `pixmap`，返回本帧需要上屏的矩形（物理像素）。
///
/// 尺寸变了就重建缓冲；缓冲刚重建（`fresh`）时宿主本帧必须画整窗（对照 win32 / macOS
/// 的同名分支），否则按宿主报的脏区。尺寸非正返回 `None`。
pub(super) fn render_frame(
    handler: &mut dyn AppHandler,
    pixmap: &mut Option<Pixmap>,
    fresh: &mut bool,
    (w, h): (i32, i32),
    fallback_bg: Color,
) -> Option<Rect> {
    if w <= 0 || h <= 0 {
        return None;
    }
    let rebuilt = match pixmap {
        Some(p) => p.width() as i32 != w || p.height() as i32 != h,
        None => true,
    };
    if rebuilt {
        *pixmap = Pixmap::new(w as u32, h as u32);
        *fresh = true;
    }
    let pm = pixmap.as_mut()?;
    if *fresh {
        handler.request_full_frame();
        pm.fill(to_skia_color(handler.bg().unwrap_or(fallback_bg)));
    }
    {
        let mut tgt = crate::render::PixmapTarget { pixmap: pm };
        handler.render(&mut tgt, Size::new(w, h));
    }
    let full = Rect::new(0, 0, w, h);
    let drawn = match (*fresh, handler.last_frame_damage()) {
        (false, Some(d)) => d.intersect(&full),
        _ => full,
    };
    *fresh = false;
    Some(drawn)
}

/// 事件分发后要从宿主取走的意图，由后端逐项落到各自的协议请求上。
pub(super) struct Requests {
    pub op: Option<WindowOp>,
    pub dialog: Option<DialogRequest>,
    pub close: bool,
    pub title: Option<String>,
    pub hotkey_ops: Vec<(usize, HotkeyOp)>,
    pub new_windows: Vec<NewWindow>,
    pub cursor: CursorShape,
    /// 输入法候选窗锚点（窗口内物理像素：x, y, 行高）。
    pub ime_caret: Option<(i32, i32, i32)>,
}

impl Requests {
    /// `is_open`：该单例键的窗口是否已开着（`Window::single` 去重用）。
    pub fn take(handler: &mut dyn AppHandler, is_open: &dyn Fn(&str) -> bool) -> Self {
        let r = Self {
            op: handler.take_window_op(),
            dialog: handler.take_dialog_request(),
            close: handler.wants_close(),
            title: handler.take_window_title(),
            hotkey_ops: handler.take_hotkey_ops(),
            new_windows: handler.take_new_windows(is_open),
            cursor: handler.cursor(),
            ime_caret: handler.ime_caret(),
        };
        // 托盘操作：Linux 尚无托盘，取走丢弃以免队列无限增长。
        let _ = crate::platform::tray::take_tray_ops();
        r
    }
}

/// tiny-skia 的 RGBA（预乘）一行 → 小端 32 位像素字节（B,G,R,A），即 X 的 LSB 真彩与
/// `wl_shm` 的 ARGB8888 / XRGB8888 共同的内存布局。
pub(super) fn rgba_to_bgra(src: &[u8], out: &mut Vec<u8>) {
    for px in src.as_chunks::<4>().0 {
        out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
}

/// 无边框窗口的边缘命中 → 方向码（0 = 左上，顺时针到 7 = 左），即 `_NET_WM_MOVERESIZE`
/// 的编号；Wayland 后端再换成 `xdg_toplevel.resize_edge`。坐标与尺寸同为物理像素。
pub(super) fn edge_direction(p: Point, w: i32, h: i32, border: i32) -> Option<u32> {
    let left = p.x < border;
    let right = p.x >= w - border;
    let top = p.y < border;
    let bottom = p.y >= h - border;
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => 0,
        (_, true, true, _) => 2,
        (_, true, _, true) => 4,
        (true, _, _, true) => 6,
        (_, _, true, _) => 1,
        (_, true, _, _) => 3,
        (_, _, _, true) => 5,
        (true, _, _, _) => 7,
        _ => return None,
    })
}

/// 无边框窗口「这次左键被标题栏 / 缩放边接管」的簿记（X11 与 Wayland 共用）。
///
/// `D` 是后端交给窗口管理器 / 合成器的拖动描述（方向、起点、serial 等）。拖动**移出阈值
/// 才交出去**（理由见两个后端的 `pending_drag` 说明）；交出去之后，这次按下配对的松开归
/// WM / 合成器，**永远不会送回来**——所以「吞掉配对松开」的标志必须在下一次按下时作废，
/// 否则下一次在内容区点击的松开会被吞掉（按钮按下去弹不起来，关闭按钮点了没反应）。
#[derive(Debug)]
pub(super) struct DragGate<D> {
    pending: Option<D>,
    swallow_up: bool,
}

impl<D> Default for DragGate<D> {
    fn default() -> Self {
        Self {
            pending: None,
            swallow_up: false,
        }
    }
}

/// 指针移动归谁。
#[derive(Debug, PartialEq)]
pub(super) enum DragMotion<D> {
    /// 没有待定拖动：照常下发给控件。
    Free,
    /// 有待定拖动、还没移出阈值：吃掉这次移动。
    Held,
    /// 刚移出阈值：把这个拖动交给 WM / 合成器（本次移动也吃掉）。
    Start(D),
}

impl<D: Copy> DragGate<D> {
    /// 左键按下：先作废上一次接管的残留，再由调用方判定这次要不要接管。
    pub fn press(&mut self) {
        self.pending = None;
        self.swallow_up = false;
    }

    /// 这次按下被接管：配对的松开也不下发。`pending` 是待移出阈值再交出去的拖动
    /// （双击切最大化那一下没有拖动，传 `None`）。
    pub fn take_over(&mut self, pending: Option<D>) {
        self.pending = pending;
        self.swallow_up = true;
    }

    /// 左键松开：返回 true 表示吞掉（配对的按下被接管过）。
    pub fn release(&mut self) -> bool {
        self.pending = None;
        std::mem::take(&mut self.swallow_up)
    }

    /// 指针移动。`beyond(&d)`：是否已移出阈值。
    pub fn motion(&mut self, beyond: impl FnOnce(&D) -> bool) -> DragMotion<D> {
        match self.pending {
            None => DragMotion::Free,
            Some(d) if beyond(&d) => {
                self.pending = None;
                DragMotion::Start(d)
            }
            Some(_) => DragMotion::Held,
        }
    }

    /// 指针离开 / 进入窗口：残留一律作废。（X11 靠 `press` 的复位就够，只有 Wayland 用。）
    #[cfg_attr(not(feature = "wayland"), allow(dead_code))]
    pub fn reset(&mut self) {
        self.press();
    }
}

/// 一次物理按键 → 交给宿主的键盘事件（X11 与 Wayland 共用，保证快捷键 / 单击 Alt / 文本
/// 的口径一致）。
///
/// - `ks`：按当前修饰键与布局解析出的 keysym；`mods`：按下时的修饰键。
/// - `alt_down`：Alt 是否已按着——X 的自动重复会把按住的 Alt 报成一串按下，只认第一下。
/// - `shortcut`：Ctrl / Alt 组合要用的 `Key::Other` 码（基础层 keysym 取码，非拉丁布局回退到
///   同键位上的拉丁字母），只在需要时才算。
///
/// 规则：松开只报 Alt（见 `Key::Alt`）；具名键先报本身，空格与小键盘运算键在无 Ctrl / Alt 时
/// 再补一个字符（与 win32 `WM_KEYDOWN` + `WM_CHAR` 的双发一致）；纯修饰键不报；Ctrl / Alt
/// 组合报 `Key::Other(大写 ASCII)`、不产出文本；其余可打印字符报 `Key::Char`（不带修饰键）。
pub(super) fn translate_key(
    ks: u32,
    press: bool,
    mods: Mods,
    alt_down: &mut bool,
    shortcut: impl FnOnce() -> Option<u32>,
) -> Vec<KeyEvent> {
    let mk = |key| KeyEvent {
        key,
        pressed: true,
        shift: mods.shift,
        ctrl: mods.ctrl,
        alt: mods.alt,
        meta: mods.meta,
    };
    let plain = |key| KeyEvent {
        key,
        pressed: true,
        shift: false,
        ctrl: false,
        alt: false,
        meta: false,
    };
    let mut out = Vec::new();
    if !press {
        if keys::special_key(ks) == Some(Key::Alt) {
            *alt_down = false;
            out.push(KeyEvent {
                pressed: false,
                ..plain(Key::Alt)
            });
        }
        return out;
    }
    if let Some(k) = keys::special_key(ks) {
        if k == Key::Alt {
            if *alt_down {
                return out; // 自动重复
            }
            *alt_down = true;
        }
        out.push(mk(k));
        let emits_char = matches!(
            k,
            Key::Space
                | Key::NumpadAdd
                | Key::NumpadSubtract
                | Key::NumpadMultiply
                | Key::NumpadDivide
        );
        if !emits_char || mods.ctrl || mods.alt {
            return out;
        }
    } else if keys::is_modifier(ks) {
        return out;
    }
    if mods.ctrl || mods.alt {
        if keys::special_key(ks).is_none() {
            if let Some(code) = shortcut() {
                out.push(mk(Key::Other(code)));
            }
        }
        return out;
    }
    if let Some(c) = keys::keysym_char(ks).filter(|c| !c.is_control()) {
        out.push(plain(Key::Char(c)));
    }
    out
}

/// 可执行文件名（不含扩展名），取不到时为 `windui`。X11 的 `WM_CLASS` 与 Wayland 的
/// `app_id` 都用它：桌面据此把窗口归组、匹配 .desktop 文件。
pub(super) fn exe_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "windui".into())
}

/// 跨线程唤醒：写唤醒管道，事件循环醒来后标脏所有窗口（宿主 render 时排空消息通道）。
pub(super) struct LinuxWake;
impl crate::sync::RawWakeSignal for LinuxWake {
    fn signal(&self) {
        super::sys::wake();
    }
}

/// 解析文件拖入的 `text/uri-list`（RFC 2483）：跳过注释行与空行，只取 `file://` 且能解码的
/// 本地路径。X11（XDND）与 Wayland（`wl_data_offer`）两个后端共用。
pub(super) fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(data)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(file_uri_to_path)
        .collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///abs` 或 `file://localhost/abs`；别的主机名是网络路径，不是本机文件。
    let path = if rest.starts_with('/') {
        rest
    } else {
        let (host, p) = rest.split_once('/')?;
        if !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        return percent_decode(&format!("/{p}")).map(PathBuf::from);
    };
    percent_decode(path).map(PathBuf::from)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            // 截断的转义（`%2` 在末尾）取不到两位十六进制，整条丢弃。
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn click_count_folds_into_one_two_cycle() {
        let mut t = ClickTracker::default();
        assert_eq!(t.press(1000, (10, 10), 1, 4), 1);
        assert_eq!(t.press(1100, (11, 10), 1, 4), 2);
        assert_eq!(t.press(1200, (11, 10), 1, 4), 1, "第三下重新起算");
        assert_eq!(t.press(1300, (11, 10), 1, 4), 2);
        assert_eq!(t.press(2000, (11, 10), 1, 4), 1, "超时不算双击");
        assert_eq!(t.press(2100, (40, 10), 1, 4), 1, "漂移过大不算双击");
    }

    #[test]
    fn anim_interval_is_clamped_between_frame_and_max_delay() {
        assert_eq!(anim_frame_interval(0), Duration::from_millis(FRAME_MS));
        assert_eq!(anim_frame_interval(500), Duration::from_millis(500));
        assert_eq!(
            anim_frame_interval(u32::MAX as u64),
            Duration::from_millis(MAX_FRAME_DELAY_MS)
        );
    }

    #[test]
    fn intervals_fire_and_realign_after_long_stall() {
        let t0 = Instant::now();
        let mut iv = Intervals::new(vec![Duration::ZERO, Duration::from_millis(100)], t0);
        let mut next = None;
        assert!(iv.fire_due(t0, &mut next).is_empty());
        assert_eq!(
            next,
            Some(t0 + Duration::from_millis(10)),
            "周期 0 被抬到 10ms"
        );
        // 挂起 1s 后恢复：每项只发一次，截止对齐到 now + 周期而不是补发一串。
        let late = t0 + Duration::from_secs(1);
        let mut next = None;
        assert_eq!(iv.fire_due(late, &mut next), vec![0, 1]);
        assert_eq!(next, Some(late + Duration::from_millis(10)));
    }

    #[test]
    fn drag_handed_off_without_release_does_not_swallow_next_click() {
        let mut g = DragGate::default();
        // 标题栏按下 → 接管，移动未过阈值被吃掉，过阈值交出去。
        g.press();
        g.take_over(Some(7u32));
        assert_eq!(g.motion(|_| false), DragMotion::Held);
        assert_eq!(g.motion(|_| true), DragMotion::Start(7));
        assert_eq!(g.motion(|_| true), DragMotion::Free, "交出去后移动照常下发");
        // 松开归 WM，没送回来。接着在内容区点一下：按下不接管，松开必须照常下发。
        g.press();
        assert!(!g.release(), "上一次接管的残留不能吞掉这次松开");
    }

    #[test]
    fn taken_over_click_swallows_its_own_release() {
        let mut g: DragGate<u32> = DragGate::default();
        g.press();
        g.take_over(Some(1));
        assert!(g.release(), "没拖出去就松开：配对的松开吞掉");
        assert_eq!(g.motion(|_| true), DragMotion::Free, "松开后待定拖动作废");
        // 双击切最大化：接管但没有待定拖动。
        g.press();
        g.take_over(None);
        assert_eq!(g.motion(|_| true), DragMotion::Free);
        assert!(g.release());
        assert!(!g.release(), "只吞一次");
    }

    #[test]
    fn leave_resets_leftovers() {
        let mut g = DragGate::default();
        g.press();
        g.take_over(Some(3u32));
        g.reset();
        assert_eq!(g.motion(|_| true), DragMotion::Free);
        assert!(!g.release());
    }

    #[test]
    fn edges_map_to_ewmh_directions() {
        assert_eq!(edge_direction(Point::new(0, 0), 100, 100, 5), Some(0));
        assert_eq!(edge_direction(Point::new(50, 0), 100, 100, 5), Some(1));
        assert_eq!(edge_direction(Point::new(99, 99), 100, 100, 5), Some(4));
        assert_eq!(edge_direction(Point::new(0, 50), 100, 100, 5), Some(7));
        assert_eq!(edge_direction(Point::new(50, 50), 100, 100, 5), None);
    }

    const XK_A: u32 = 0x61;
    const XK_SPACE: u32 = 0x20;
    const XK_ALT_L: u32 = 0xffe9;
    const XK_SHIFT_L: u32 = 0xffe1;

    fn keys_of(evs: &[KeyEvent]) -> Vec<(Key, bool)> {
        evs.iter().map(|e| (e.key, e.pressed)).collect()
    }

    #[test]
    fn plain_letter_is_text_and_ctrl_letter_is_shortcut() {
        let mut alt = false;
        let evs = translate_key(XK_A, true, Mods::default(), &mut alt, || None);
        assert_eq!(keys_of(&evs), [(Key::Char('a'), true)]);
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        let evs = translate_key(XK_A, true, ctrl, &mut alt, || Some(b'A' as u32));
        assert_eq!(keys_of(&evs), [(Key::Other(b'A' as u32), true)]);
        assert!(evs[0].ctrl);
    }

    #[test]
    fn space_reports_key_then_char_unless_modified() {
        let mut alt = false;
        let evs = translate_key(XK_SPACE, true, Mods::default(), &mut alt, || None);
        assert_eq!(keys_of(&evs), [(Key::Space, true), (Key::Char(' '), true)]);
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        let evs = translate_key(XK_SPACE, true, ctrl, &mut alt, || None);
        assert_eq!(keys_of(&evs), [(Key::Space, true)]);
    }

    #[test]
    fn alt_press_is_reported_once_and_release_resets() {
        let mut alt = false;
        let a = Mods {
            alt: true,
            ..Mods::default()
        };
        assert_eq!(translate_key(XK_ALT_L, true, a, &mut alt, || None).len(), 1);
        assert!(translate_key(XK_ALT_L, true, a, &mut alt, || None).is_empty());
        let up = translate_key(XK_ALT_L, false, a, &mut alt, || None);
        assert_eq!(keys_of(&up), [(Key::Alt, false)]);
        assert!(!alt);
        assert!(translate_key(XK_SHIFT_L, true, Mods::default(), &mut alt, || None).is_empty());
        assert!(translate_key(XK_A, false, Mods::default(), &mut alt, || None).is_empty());
    }

    const XK_KP_ADD: u32 = 0xffab;
    const XK_KP_DIVIDE: u32 = 0xffaf;
    const XK_KP_1: u32 = 0xffb1;
    const XK_DEAD_GRAVE: u32 = 0xfe50;

    #[test]
    fn keypad_operators_emit_key_then_char_but_not_under_ctrl() {
        let mut alt = false;
        let evs = translate_key(XK_KP_ADD, true, Mods::default(), &mut alt, || None);
        assert_eq!(
            keys_of(&evs),
            [(Key::NumpadAdd, true), (Key::Char('+'), true)],
            "小键盘运算键与 win32 一样双发"
        );
        let evs = translate_key(XK_KP_DIVIDE, true, Mods::default(), &mut alt, || None);
        assert_eq!(
            keys_of(&evs),
            [(Key::NumpadDivide, true), (Key::Char('/'), true)]
        );
        let ctrl = Mods {
            ctrl: true,
            ..Mods::default()
        };
        let evs = translate_key(XK_KP_ADD, true, ctrl, &mut alt, || None);
        assert_eq!(
            keys_of(&evs),
            [(Key::NumpadAdd, true)],
            "Ctrl+小键盘不补字符"
        );
        assert!(evs[0].ctrl);
        let evs = translate_key(XK_KP_1, true, Mods::default(), &mut alt, || None);
        assert_eq!(
            keys_of(&evs),
            [(Key::Char('1'), true)],
            "小键盘数字只出字符"
        );
    }

    #[test]
    fn keys_without_text_or_name_produce_nothing() {
        let mut alt = false;
        // 死键（组合用）：既不是具名键也不对应字符。
        let evs = translate_key(XK_DEAD_GRAVE, true, Mods::default(), &mut alt, || None);
        assert!(evs.is_empty());
    }

    #[test]
    fn shortcut_code_is_only_computed_under_ctrl_or_alt() {
        let mut alt = false;
        let evs = translate_key(XK_A, true, Mods::default(), &mut alt, || {
            panic!("没按 Ctrl / Alt 不该求快捷键码")
        });
        assert_eq!(keys_of(&evs), [(Key::Char('a'), true)]);
        let shift = Mods {
            shift: true,
            ..Mods::default()
        };
        let evs = translate_key(0x41, true, shift, &mut alt, || panic!("Shift 不算快捷键"));
        assert_eq!(
            keys_of(&evs),
            [(Key::Char('A'), true)],
            "字符事件不带修饰键"
        );
        assert!(!evs[0].shift);
        let a = Mods {
            alt: true,
            ..Mods::default()
        };
        let evs = translate_key(XK_A, true, a, &mut alt, || None);
        assert!(
            evs.is_empty(),
            "Alt+字母但拿不到快捷键码：什么都不报，也不漏出文本"
        );
    }

    #[test]
    fn bgra_swaps_red_and_blue() {
        let mut out = Vec::new();
        rgba_to_bgra(&[1, 2, 3, 4, 5, 6, 7, 8], &mut out);
        assert_eq!(out, [3, 2, 1, 4, 7, 6, 5, 8]);
    }

    #[test]
    fn uri_list_yields_decoded_local_paths() {
        let data = b"# comment\r\nfile:///home/u/a%20b.txt\r\nfile://localhost/tmp/%E4%B8%AD.png\r\nhttps://x.org/y\r\nfile://otherhost/z\r\n";
        assert_eq!(
            parse_uri_list(data),
            vec![
                PathBuf::from("/home/u/a b.txt"),
                PathBuf::from("/tmp/中.png")
            ]
        );
    }

    #[test]
    fn uri_list_accepts_raw_utf8_blank_lines_and_missing_final_newline() {
        // 有的源端不转义非 ASCII（Nautilus 转义、部分 Qt 应用不转义），两种都要认；
        // 空行、行尾空白、缺最后一个换行都不影响。
        let data = "\r\nfile:///srv/照片 2024/海边.jpg  \n\n#file:///ignored\nfile:///srv/%E6%96%87%E6%A1%A3.txt";
        assert_eq!(
            parse_uri_list(data.as_bytes()),
            vec![
                PathBuf::from("/srv/照片 2024/海边.jpg"),
                PathBuf::from("/srv/文档.txt")
            ]
        );
    }

    #[test]
    fn uri_list_skips_non_file_schemes_and_remote_hosts() {
        let data = b"sftp://h/x\nsmb://h/share/y\ntrash:///z\nfile://127.0.0.1/no\n";
        assert_eq!(
            parse_uri_list(data),
            Vec::<PathBuf>::new(),
            "只有 file:// 且主机为空或 localhost 才算本机文件"
        );
        assert_eq!(
            parse_uri_list(b"file://LocalHost/etc/hosts\n"),
            vec![PathBuf::from("/etc/hosts")],
            "主机名 localhost 不分大小写"
        );
    }

    #[test]
    fn malformed_escapes_are_dropped_not_panicking() {
        assert_eq!(
            parse_uri_list(b"file:///a%2\nfile:///b%zz\nfile:///c%FF\nfile:///d%41\n"),
            vec![PathBuf::from("/dA")],
            "截断 / 非十六进制 / 解出非法 UTF-8 的整条丢弃，其余照收"
        );
    }
}
