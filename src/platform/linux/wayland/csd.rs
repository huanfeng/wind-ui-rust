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

use crate::geometry::Point;

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

/// 内容区的尺寸约束（逻辑）→ 告诉合成器的窗口约束：高度加上标题栏。
pub(super) fn with_bar((w, h): (i32, i32), bar: i32) -> (i32, i32) {
    (w, if h > 0 { h + bar } else { h })
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
