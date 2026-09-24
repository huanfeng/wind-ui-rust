//! 与显示协议无关的宿主簿记：X11 与 Wayland 两个后端共用。
//!
//! 只收「换了协议也一字不改」的部分：点击计数、interval 定时器、动画帧配速、
//! 一帧的渲染与脏区计算、`after_event` 要从宿主取走的意图、像素换序。怎么建窗、
//! 怎么上屏、窗口操作落到哪条协议请求上，仍各归各的后端。

use std::time::{Duration, Instant};

use tiny_skia::Pixmap;

use crate::event::{CursorShape, HotkeyOp, WindowOp};
use crate::geometry::{Color, Rect, Size};
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
    fn bgra_swaps_red_and_blue() {
        let mut out = Vec::new();
        rgba_to_bgra(&[1, 2, 3, 4, 5, 6, 7, 8], &mut out);
        assert_eq!(out, [3, 2, 1, 4, 7, 6, 5, 8]);
    }
}
