//! 客户端装饰（CSD）的几何：表面上一点归标题栏、内容区还是缩放边；尺寸怎么扣标题栏。纯逻辑，
//! 有单测。
//!
//! # 取舍
//!
//! - **一张表面，标题栏在上**：标题栏与内容画进同一块 `wl_shm` 缓冲（上 `bar` 行是标题栏，下面是
//!   内容），不另开子表面。内容区的坐标在平台层统一减掉标题栏高度——应用看到的尺寸、指针、输入法
//!   光标、拖入落点都按内容区计，感知不到标题栏。子表面方案（libdecor 的做法）能把阴影 / 不可见
//!   缩放边放到窗口外，但要多管一组表面的同步与缓冲；我们不画阴影，用不上。
//! - **缩放边内缩**：GNOME 的 GTK 应用把缩放边做在窗口外（透明的阴影区里）；我们没有阴影区，
//!   缩放边落在窗口内侧 `border` 像素（与无边框窗口同一套 `host::edge_direction`），只在那里
//!   不是可交互控件时才接管——与无边框窗口一致。最大化时没有缩放边。
//! - **不画圆角与阴影**：标题栏是方的、贴满窗口宽，最大化 / 平铺时也就无需去掉什么。

use crate::geometry::{Point, Rect};

use super::super::host;

/// 表面上一点（物理像素，表面坐标）落在哪。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Region {
    /// 标题栏内，坐标已换成标题栏宿主的坐标。
    Bar(Point),
    /// 内容区内，坐标已减去标题栏高度。
    Content(Point),
}

/// 一扇窗口的装饰几何（物理像素）。`bar == 0` = 没有客户端标题栏（服务端装饰或无边框）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Frame {
    /// 宽度（标题栏与内容同宽）。
    pub width: i32,
    /// 内容区高度。
    pub content_h: i32,
    /// 标题栏高度。
    pub bar: i32,
}

impl Frame {
    /// 整个表面（= 缓冲）的高度。
    pub fn height(&self) -> i32 {
        self.content_h + self.bar
    }

    /// 表面坐标 → 所在区域与区域内坐标。
    pub fn locate(&self, p: Point) -> Region {
        if p.y < self.bar {
            Region::Bar(p)
        } else {
            Region::Content(Point::new(p.x, p.y - self.bar))
        }
    }

    /// 缩放边方向（`host::edge_direction` 的编号）：`edges == false`（不可缩放、最大化）时没有。
    /// 命中的还要由调用方确认那里不是可交互控件，才真的接管（同无边框窗口）。
    pub fn edge(&self, p: Point, border: i32, edges: bool) -> Option<u32> {
        if !edges {
            return None;
        }
        host::edge_direction(p, self.width, self.height(), border)
    }
}

/// 合成器给的窗口尺寸（逻辑，含标题栏）→ 内容区高度（逻辑）。0 = 「客户端自定」，原样传出去
/// 交给调用方回退；给的比标题栏还矮时内容至少留 1。
pub(super) fn content_height(configured: i32, bar: i32) -> i32 {
    if configured <= 0 {
        0
    } else {
        (configured - bar).max(1)
    }
}

/// 只换了装饰模式（标题栏从有到无或反过来）、窗口整体尺寸不变时的新内容高度（逻辑）。
pub(super) fn rebar(content: i32, old_bar: i32, new_bar: i32) -> i32 {
    (content + old_bar - new_bar).max(1)
}

/// 内容区的尺寸约束（逻辑）→ 告诉合成器的窗口约束：高度加上标题栏。
pub(super) fn with_bar((w, h): (i32, i32), bar: i32) -> (i32, i32) {
    (w, if h > 0 { h + bar } else { h })
}

/// 内容区里的矩形（宿主报的脏区）→ 缓冲坐标：下移标题栏。
pub(super) fn content_to_buffer(r: Rect, bar: i32) -> Rect {
    Rect::new(r.x, r.y + bar, r.w, r.h)
}

/// 缓冲坐标 → 内容区坐标（[`content_to_buffer`] 的逆）。
pub(super) fn buffer_to_content(r: Rect, bar: i32) -> Rect {
    Rect::new(r.x, r.y - bar, r.w, r.h)
}

/// 宿主报的输入法光标（内容区物理像素：x, y_top, 行高）→ `set_cursor_rectangle` 的矩形（表面
/// 逻辑坐标）：除以缩放，再下移标题栏的逻辑高。
pub(super) fn ime_rect(
    caret: (i32, i32, i32),
    factor: f64,
    bar_logical: i32,
) -> (i32, i32, i32, i32) {
    let lg = |v: i32| (v as f64 / factor).round() as i32;
    (
        lg(caret.0),
        lg(caret.1) + bar_logical,
        1,
        lg(caret.2).max(1),
    )
}

/// 拖入落点（表面逻辑坐标，带小数）→ 宿主要的内容区物理像素：乘缩放（与指针同一取整），再扣
/// 标题栏的物理高。
pub(super) fn drop_point(pos: (f64, f64), factor: f64, bar_phys: i32) -> Point {
    let ph = |v: f64| (v * factor).round() as i32;
    Point::new(ph(pos.0), ph(pos.1) - bar_phys)
}

/// 这一轮要不要出帧（也是事件循环要不要立即醒）：窗口可见，且内容或标题栏有一处待重画。
/// 标题栏单独算：它的悬停 / 按下只置标题栏脏，不置窗口脏。
pub(super) fn frame_due(visible: bool, content_dirty: bool, bar_dirty: bool) -> bool {
    visible && (content_dirty || bar_dirty)
}

/// 一轮动画排程的结果（见 [`anim_tick`]）。
#[derive(Debug, PartialEq, Eq)]
pub(super) struct AnimTick {
    /// 内容到了续帧时刻，重画。
    pub content: bool,
    /// 标题栏到了，重画（只重画标题栏）。
    pub bar: bool,
    /// 还没到的那一路最早还差多久。
    pub wait: Option<std::time::Duration>,
}

/// 内容与标题栏的动画各按各的截止时间走：`content` / `bar` 是（距该路上一帧多久，该路的帧
/// 间隔），`None` = 那一路没在动。标题栏按钮的淡入淡出按满帧走，但不能把内容拖着一起：内容
/// 报的是光标闪烁那种 500ms 截止，跟着满帧重画就成了每帧整窗重画。
pub(super) fn anim_tick(
    content: Option<(std::time::Duration, std::time::Duration)>,
    bar: Option<(std::time::Duration, std::time::Duration)>,
) -> AnimTick {
    let mut wait: Option<std::time::Duration> = None;
    let mut step = |track: Option<(std::time::Duration, std::time::Duration)>| match track {
        Some((elapsed, due)) if elapsed >= due => true,
        Some((elapsed, due)) => {
            let left = due - elapsed;
            wait = Some(wait.map_or(left, |w| w.min(left)));
            false
        }
        None => false,
    };
    let content = step(content);
    let bar = step(bar);
    AnimTick { content, bar, wait }
}

/// 标题栏的物理高度：由「整窗高 − 内容高」得出（`to_physical` 是逻辑 → 物理的取整），而不是
/// 单独取整——分数缩放下两次独立取整之和可能比整窗多 1 像素，缓冲就与 viewport 目标（逻辑整窗
/// 高）差一行，合成器会把整窗重采样发糊。整数缩放下两种算法结果相同。
pub(super) fn bar_physical(
    content_logical: i32,
    bar_logical: i32,
    to_physical: impl Fn(i32) -> i32,
) -> i32 {
    if bar_logical <= 0 {
        return 0;
    }
    to_physical(content_logical + bar_logical) - to_physical(content_logical)
}

/// 有客户端标题栏时的最小尺寸：内容区的下限加上标题栏；没设下限也至少留出标题栏 + 1 行
/// 内容（否则合成器能把窗口压得比标题栏还矮，内容高度只能兜底成 1，提交的表面就比合成器
/// 配置的尺寸高）。`bar == 0` 原样返回。
pub(super) fn min_with_bar(min: Option<(i32, i32)>, bar: i32) -> Option<(i32, i32)> {
    if bar == 0 {
        return min;
    }
    let (w, h) = min.unwrap_or((0, 0));
    Some((w, h.max(1) + bar))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bar: i32) -> Frame {
        Frame {
            width: 600,
            content_h: 400,
            bar,
        }
    }

    #[test]
    fn points_above_the_bar_line_go_to_the_titlebar_and_below_are_shifted_into_content() {
        let f = frame(33);
        assert_eq!(f.locate(Point::new(10, 0)), Region::Bar(Point::new(10, 0)));
        assert_eq!(
            f.locate(Point::new(10, 32)),
            Region::Bar(Point::new(10, 32))
        );
        assert_eq!(
            f.locate(Point::new(10, 33)),
            Region::Content(Point::new(10, 0)),
            "标题栏下第一行是内容的第 0 行"
        );
        assert_eq!(f.height(), 433);
    }

    #[test]
    fn without_a_bar_everything_is_content_at_the_same_coordinates() {
        let f = frame(0);
        assert_eq!(
            f.locate(Point::new(5, 0)),
            Region::Content(Point::new(5, 0))
        );
        assert_eq!(f.height(), 400);
    }

    #[test]
    fn resize_edges_cover_the_whole_frame_including_the_titlebar_top() {
        let f = frame(33);
        let b = 6;
        assert_eq!(
            f.edge(Point::new(300, 2), b, true),
            Some(1),
            "标题栏顶边 = 上边"
        );
        assert_eq!(f.edge(Point::new(1, 1), b, true), Some(0), "左上角");
        assert_eq!(
            f.edge(Point::new(599, 432), b, true),
            Some(4),
            "右下角在内容区底"
        );
        assert_eq!(f.edge(Point::new(0, 200), b, true), Some(7));
        assert_eq!(f.edge(Point::new(300, 200), b, true), None);
        assert_eq!(
            f.edge(Point::new(300, 427), b, true),
            Some(5),
            "底边按整个表面高算"
        );
        assert_eq!(f.edge(Point::new(300, 426), b, true), None);
    }

    #[test]
    fn maximized_or_fixed_size_windows_have_no_resize_edges() {
        let f = frame(33);
        assert_eq!(f.edge(Point::new(0, 0), 6, false), None);
    }

    #[test]
    fn fractional_scale_bar_and_border_are_physical_and_the_split_stays_exact() {
        // 1.5 倍：标题栏 33 逻辑 → 50 物理（round），缩放边 6 → 9。
        let f = Frame {
            width: 900,
            content_h: 600,
            bar: 50,
        };
        assert!(matches!(f.locate(Point::new(0, 49)), Region::Bar(_)));
        assert_eq!(
            f.locate(Point::new(0, 50)),
            Region::Content(Point::new(0, 0))
        );
        assert_eq!(f.edge(Point::new(450, 8), 9, true), Some(1));
        assert_eq!(f.edge(Point::new(450, 9), 9, true), None);
    }

    #[test]
    fn bar_height_is_derived_so_the_whole_buffer_matches_the_viewport_target() {
        let f = 1.5;
        let phys = |v: i32| ((v as f64 * f).round() as i32).max(1);
        for content in 300..700 {
            let bar = bar_physical(content, 33, phys);
            assert_eq!(
                phys(content) + bar,
                phys(content + 33),
                "内容 {content}：缓冲高 = 整窗逻辑高换算，一像素不差"
            );
        }
        // 独立取整会出错的那种：601 × 1.5 = 901.5 → 902，33 × 1.5 = 49.5 → 50，和 952 ≠ 951。
        assert_eq!(bar_physical(601, 33, phys), 49);
        let int2 = |v: i32| v * 2;
        assert_eq!(bar_physical(400, 33, int2), 66, "整数缩放：就是 33 × 2");
        assert_eq!(bar_physical(400, 0, phys), 0);
    }

    #[test]
    fn content_rects_and_points_shift_by_the_bar() {
        let r = Rect::new(5, 0, 10, 20);
        assert_eq!(content_to_buffer(r, 50), Rect::new(5, 50, 10, 20));
        assert_eq!(buffer_to_content(content_to_buffer(r, 50), 50), r);
        assert_eq!(content_to_buffer(r, 0), r, "没有标题栏：原样");
    }

    #[test]
    fn ime_rectangle_is_logical_and_below_the_bar() {
        assert_eq!(ime_rect((20, 63, 17), 1.0, 33), (20, 96, 1, 17));
        assert_eq!(ime_rect((20, 63, 17), 1.0, 0), (20, 63, 1, 17));
        // 1.5 倍：物理 (30, 94.5→95, 25.5→26) → 逻辑 (20, 63, 17)，再加 33。
        assert_eq!(ime_rect((30, 95, 26), 1.5, 33), (20, 96, 1, 17));
    }

    #[test]
    fn drop_point_is_physical_and_content_relative() {
        assert_eq!(drop_point((100.0, 140.0), 1.0, 33), Point::new(100, 107));
        assert_eq!(drop_point((100.0, 140.0), 1.5, 50), Point::new(150, 160));
        assert_eq!(drop_point((100.0, 140.0), 1.0, 0), Point::new(100, 140));
    }

    #[test]
    fn titlebar_animation_does_not_drag_the_content_deadline_along() {
        use std::time::Duration as D;
        let ms = D::from_millis;
        // 标题栏悬停淡入（满帧，4ms 间隔）期间，内容只是光标闪烁（500ms 后才变）。
        let t = anim_tick(Some((ms(16), ms(500))), Some((ms(16), ms(4))));
        assert_eq!(
            t,
            AnimTick {
                content: false,
                bar: true,
                wait: Some(ms(484))
            },
            "只画标题栏，内容等它自己的截止"
        );
        let t = anim_tick(Some((ms(500), ms(500))), Some((ms(1), ms(4))));
        assert!(t.content && !t.bar);
        assert_eq!(t.wait, Some(ms(3)));
        assert_eq!(
            anim_tick(None, None),
            AnimTick {
                content: false,
                bar: false,
                wait: None
            }
        );
    }

    #[test]
    fn a_dirty_titlebar_alone_is_enough_to_paint_and_wake() {
        assert!(frame_due(true, false, true), "只有标题栏悬停变了也要出帧");
        assert!(frame_due(true, true, false));
        assert!(!frame_due(true, false, false));
        assert!(!frame_due(false, true, true), "不可见不出帧");
    }

    #[test]
    fn switching_decoration_mode_keeps_the_window_size() {
        assert_eq!(rebar(400, 0, 33), 367, "开标题栏：内容让出 33");
        assert_eq!(rebar(367, 33, 0), 400, "关标题栏：内容收回");
        assert_eq!(rebar(400, 33, 33), 400);
        assert_eq!(rebar(10, 0, 33), 1, "至少 1");
    }

    #[test]
    fn configured_size_includes_the_bar() {
        assert_eq!(content_height(433, 33), 400);
        assert_eq!(content_height(0, 33), 0, "0 = 客户端自定，交调用方回退");
        assert_eq!(content_height(20, 33), 1, "比标题栏还矮：内容至少 1");
        assert_eq!(with_bar((300, 200), 33), (300, 233));
        assert_eq!(with_bar((300, 0), 33), (300, 0), "0 = 不限，不加");
        assert_eq!(
            min_with_bar(None, 33),
            Some((0, 34)),
            "没设下限也至少留出标题栏"
        );
        assert_eq!(min_with_bar(Some((200, 0)), 33), Some((200, 34)));
        assert_eq!(min_with_bar(Some((200, 100)), 33), Some((200, 133)));
        assert_eq!(min_with_bar(None, 0), None, "没有标题栏：原样");
    }
}
