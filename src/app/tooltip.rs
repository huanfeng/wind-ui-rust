//! 悬停提示（tooltip）浮层：延时触发、点击抑制、越界翻转定位。
//!
//! 状态只有三样（指针位置、悬停起始时刻、是否被抑制），绘制时按当前悬停节点
//! 现取文案，故与控件树无耦合。

use crate::core::{NodeId, Tree};
use crate::geometry::{Point, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::text::TextStyle;
use crate::theme::Theme;

/// 悬停提示：触发延时（ms）、字号、内边距、相对指针的偏移。
/// 换行宽度上限由宿主经 `Theme.tooltip.max_width` 配置（见 [`crate::theme::TooltipTheme::max_width`]）。
const TOOLTIP_DELAY_MS: u64 = 500;
const TOOLTIP_FONT: f32 = 13.0;
const TOOLTIP_PAD_X: i32 = 8;
const TOOLTIP_PAD_Y: i32 = 4;
const TOOLTIP_CURSOR_DX: i32 = 12;
const TOOLTIP_CURSOR_DY: i32 = 20;
/// 翻到指针上方时与指针的间隙。比 `TOOLTIP_CURSOR_DY` 小：上方翻转本就是空间
/// 不够时的退路，再留一样大的空隙只会更早触发"两边都放不下"。
const TOOLTIP_FLIP_GAP: i32 = 4;

/// 宿主持有的悬停提示状态。
#[derive(Default)]
pub(super) struct TooltipState {
    /// 最近一次指针位置（逻辑坐标），用于悬停提示浮层定位。
    pub(super) pos: Point,
    /// 当前悬停起始时刻（ms，单调时钟）。悬停节点变化或点击时复位；
    /// 渲染据 `now - since_ms >= TOOLTIP_DELAY_MS` 决定是否弹出提示。
    pub(super) since_ms: u64,
    /// 点击后抑制提示，直到指针再次移动（避免点完控件原地又弹出盖住它）。
    pub(super) suppressed: bool,
}

/// 浮层左上角定位：默认落在指针右下方，放不下则依次翻转 / 贴边。
///
/// 抽成纯函数是为了**可测**：`paint` 要 `Canvas` 与 `Theme`，测试里造不出来，判据
/// 留在里面就只能靠肉眼守。同 `Tree::node_tooltip` 之于 `will_show`。
///
/// `ws` 为窗口尺寸；任一维为 0（尺寸未知）时该维不做收边。
fn place(pos: Point, size: Size, ws: Size) -> Point {
    let (w, h) = (size.w, size.h);
    let mut x = pos.x + TOOLTIP_CURSOR_DX;
    if ws.w > 0 && x + w > ws.w {
        x = (ws.w - w).max(0);
    }
    let mut y = pos.y + TOOLTIP_CURSOR_DY;
    if ws.h > 0 && y + h > ws.h {
        let above = pos.y - h - TOOLTIP_FLIP_GAP;
        y = if above >= 0 {
            above // 下方放不下则翻到指针上方
        } else if pos.y >= ws.h - pos.y {
            // 上下都容不下整个浮层。多行提示放开之前到不了这里——单行浮层比谁都矮，
            // 翻上方总是放得下的；行数一多就成了常态，故必须有这一支。
            //
            // 贴向空间更大的一侧：盖住指针已经躲不掉（两边都比浮层矮），只能让它
            // 尽量少盖——指针底下正是用户在看的那个控件。贴顶。
            0
        } else {
            // 贴底，同理。`max(0)` 兜住"浮层比整个窗口还高"：那时底部被裁，
            // 但起点不会变成负数、把首行推出屏幕外——宁可看不到末尾也要看得到开头。
            (ws.h - h).max(0)
        };
    }
    Point::new(x, y)
}

impl TooltipState {
    /// 当前悬停节点是否会弹出提示（决定本帧算不算"有浮层"，进而能否局部重绘）。
    pub(super) fn will_show(&self, tree: &Tree, hover: Option<NodeId>) -> bool {
        !self.suppressed && hover.and_then(|h| tree.node_tooltip(h)).is_some()
    }

    /// 悬停提示浮层绘制（菜单激活时不显示）：悬停节点带 tooltip 且停留超过延时则弹出；
    /// 未到延时则请求下一帧——鼠标静止后无事件，需靠 anim 续帧推进计时
    /// （与不确定进度条同源）。
    pub(super) fn paint(
        &self,
        canvas: &mut dyn Canvas,
        tree: &Tree,
        hover: Option<NodeId>,
        menu_open: bool,
        theme: &Theme,
        ws: Size,
        now_ms: u64,
    ) {
        if menu_open || self.suppressed {
            return;
        }
        let Some(text) = hover.and_then(|h| tree.node_tooltip(h)) else {
            return;
        };
        if now_ms.saturating_sub(self.since_ms) < TOOLTIP_DELAY_MS {
            crate::anim::request_repaint();
            return;
        }
        let (pal, tt) = (&theme.palette, &theme.tooltip);
        let ts = canvas.measure_text_wrapped(&text, &TextStyle::new(TOOLTIP_FONT), tt.max_width());
        let size = Size::new(ts.w + 2 * TOOLTIP_PAD_X, ts.h + 2 * TOOLTIP_PAD_Y);
        let (w, h) = (size.w, size.h);
        let Point { x, y } = place(self.pos, size, ws);
        let corner = tt.corner(&theme.metrics);
        canvas.fill_round_rect(
            x as f32,
            y as f32,
            w as f32,
            h as f32,
            corner,
            &Paint::fill(tt.bg(pal)),
        );
        let tr = Rect::new(x + TOOLTIP_PAD_X, y, w - 2 * TOOLTIP_PAD_X, h);
        canvas.draw_text(
            &text,
            tr,
            tt.text(pal),
            crate::spec::Align::Start,
            &TextStyle::new(TOOLTIP_FONT),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一块 800×600 的窗口，下面各用例共用。
    const WS: Size = Size::new(800, 600);

    /// 空间充足时落在指针右下方，两个偏移都按常量给。
    #[test]
    fn places_below_right_of_cursor_when_it_fits() {
        let p = place(Point::new(100, 100), Size::new(200, 30), WS);
        assert_eq!(
            p,
            Point::new(100 + TOOLTIP_CURSOR_DX, 100 + TOOLTIP_CURSOR_DY)
        );
    }

    /// 右侧放不下则贴右边缘——横向从来只有这一种收边（宽度有 max_width 封顶）。
    #[test]
    fn clamps_to_right_edge() {
        let p = place(Point::new(700, 100), Size::new(200, 30), WS);
        assert_eq!(p.x, 800 - 200);
    }

    /// 下方放不下、上方放得下 → 翻到指针上方，留 `TOOLTIP_FLIP_GAP` 间隙。
    #[test]
    fn flips_above_cursor_when_below_overflows() {
        let p = place(Point::new(100, 580), Size::new(200, 100), WS);
        assert_eq!(p.y, 580 - 100 - TOOLTIP_FLIP_GAP);
    }

    /// ★ 上下都容不下整个浮层：贴向空间更大的一侧。
    ///
    /// 放开多行之前到不了这个分支——单行浮层比谁都矮，翻上方总是放得下的。行数一多
    /// 就成了常态，而原先那句 `(pos.y - h - 4).max(0)` 在这里会把浮层顶到 y=0 并
    /// **盖住指针**，也就是盖住用户正悬停的那个控件。
    #[test]
    fn sticks_to_the_roomier_side_when_neither_fits() {
        let tall = Size::new(200, 500);

        // 指针在窗口下部（上方 450 > 下方 150）→ 贴顶。
        let p = place(Point::new(100, 450), tall, WS);
        assert_eq!(p.y, 0, "上方空间更大时贴顶");

        // 指针在窗口上部（上方 150 < 下方 450）→ 贴底，而不是硬翻到上方。
        let p = place(Point::new(100, 150), tall, WS);
        assert_eq!(p.y, 600 - 500, "下方空间更大时贴底");
    }

    /// 浮层比整个窗口还高：起点收在 0，宁可裁掉末尾也要看得见开头。
    #[test]
    fn overlong_tooltip_starts_at_top_rather_than_negative() {
        let p = place(Point::new(100, 100), Size::new(200, 900), WS);
        assert_eq!(p.y, 0, "起点不得为负——负数会把首行推出屏幕外");
    }

    /// 窗口尺寸未知（任一维为 0）时该维不收边：拿 0 当边界会把浮层全挤到角上。
    #[test]
    fn unknown_window_size_skips_clamping() {
        let p = place(Point::new(700, 580), Size::new(200, 100), Size::ZERO);
        assert_eq!(
            p,
            Point::new(700 + TOOLTIP_CURSOR_DX, 580 + TOOLTIP_CURSOR_DY),
            "尺寸未知时按原始偏移落点"
        );
    }
}
