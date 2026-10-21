//! 悬停提示（tooltip）浮层：延时触发、点击抑制、越界翻转定位。
//!
//! 状态只有四样（指针位置、悬停起始时刻、是否被抑制、上一帧是否画了），绘制时按
//! 当前悬停节点现取文案，故与控件树无耦合。

use std::cell::Cell;

use crate::core::{NodeId, Tree};
use crate::geometry::{Point, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::text::TextStyle;
use crate::theme::Theme;

/// 悬停提示：字号、内边距、相对指针的偏移。触发延时与换行宽度上限由宿主经
/// `Theme.tooltip.delay_ms` / `max_width` 配置（见 [`crate::theme::TooltipTheme`]）。
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
    /// 渲染据 `now - since_ms >= 主题延时` 决定是否弹出提示。
    pub(super) since_ms: u64,
    /// 点击后抑制提示，直到指针再次移动（避免点完控件原地又弹出盖住它）。
    pub(super) suppressed: bool,
    /// 上一个整窗帧**实际画出了**浮层。`paint` 每次都写它。
    ///
    /// 浮层画在树外、指针旁边，任何控件的脏区都盖不住它；它消失的那一帧（移开、按下
    /// 抑制、菜单弹出）若走局部重绘，脏区外的旧浮层就原样留在画面上。"本帧会不会画"
    /// 回答不了"上一帧画没画"，故单记一笔。`Cell`：`paint` 只拿 `&self`。
    pub(super) shown: Cell<bool>,
}

/// 到点后第 `elapsed_ms` 毫秒的淡入不透明度。动画关闭时恒为 1；随 `WINDUI_ANIM_SCALE` 放慢。
fn fade_in(elapsed_ms: u64, duration_ms: u32) -> f32 {
    if !crate::anim::enabled() || duration_ms == 0 {
        return 1.0;
    }
    let d = duration_ms as f32 * crate::anim::time_scale();
    crate::anim::Easing::DECELERATE.apply(elapsed_ms as f32 / d)
}

/// 浮层左上角定位：默认落在指针右下方，放不下则依次翻转 / 贴边。
///
/// 抽成纯函数是为了**可测**：`paint` 要 `Canvas` 与 `Theme`，测试里造不出来，判据
/// 留在里面就只能靠肉眼守。同 `Tree::node_tooltip` 之于 `is_overlay`。
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
    /// 本帧算不算"有浮层"（有则不能局部重绘）：上一帧画过（本帧要擦掉或重画），
    /// 或悬停节点带提示（本帧可能要画、或要排下延时到点的那一帧）。
    ///
    /// 只看后一半是不够的：移开那一帧悬停节点已经换了，判据为假，于是放行局部重绘，
    /// 而脏区只有控件自己那块——旧浮层两端留成残影。
    ///
    /// 等待延时的那段也必须算进来，尽管那时什么都不画：浮层只在整窗帧里 `paint`，
    /// 而排"到点再来一帧"的正是它——等待期走了局部帧，就没人排那一帧，提示永远弹不
    /// 出来。等待期的代价由截止时间压住（到点前只出一两帧，不按刷新率续帧）。
    pub(super) fn is_overlay(&self, tree: &Tree, hover: Option<NodeId>) -> bool {
        self.shown.get() || (!self.suppressed && hover.and_then(|h| tree.node_tooltip(h)).is_some())
    }

    /// 悬停已满延时。
    fn due(&self, now_ms: u64, delay_ms: u64) -> bool {
        now_ms.saturating_sub(self.since_ms) >= delay_ms
    }

    /// 悬停提示浮层绘制（菜单激活时不显示）：悬停节点带 tooltip 且停留超过延时则弹出；
    /// 未到延时则请求**到点那一刻**的一帧——鼠标静止后无事件，需靠 anim 续帧推进计时，
    /// 但等待期画面不变，按刷新率续帧只是空转（见 [`crate::anim::request_repaint_after`]）。
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
        self.shown.set(false);
        if menu_open || self.suppressed {
            return;
        }
        let Some(text) = hover.and_then(|h| tree.node_tooltip(h)) else {
            return;
        };
        let delay = theme.tooltip.delay_ms();
        if !self.due(now_ms, delay) {
            let left = delay - now_ms.saturating_sub(self.since_ms);
            crate::anim::request_repaint_after(left);
            return;
        }
        self.shown.set(true);
        let tt = &theme.tooltip;
        let ts = canvas.measure_text_wrapped(&text, &TextStyle::new(TOOLTIP_FONT), tt.max_width());
        let size = Size::new(ts.w + 2 * TOOLTIP_PAD_X, ts.h + 2 * TOOLTIP_PAD_Y);
        let (w, h) = (size.w, size.h);
        let Point { x, y } = place(self.pos, size, ws);
        // 到点后淡入（只淡入：指针移开即消失，残影会挡住用户正要看的东西）。按"距到点
        // 时刻"算而不存状态：到点那帧未必恰好落在延时边界上，起点仍是同一刻。
        let alpha = fade_in(
            now_ms.saturating_sub(self.since_ms + delay),
            theme.anim.fast(),
        );
        if alpha < 1.0 {
            let area = Rect::new(x, y, w, h).inflate(2);
            crate::anim::request_repaint_in(area);
            canvas.push_layer_in(alpha, area);
        }
        self.paint_body(canvas, &text, Rect::new(x, y, w, h), theme);
        if alpha < 1.0 {
            canvas.pop_layer();
        }
    }

    fn paint_body(&self, canvas: &mut dyn Canvas, text: &str, r: Rect, theme: &Theme) {
        let (pal, tt) = (&theme.palette, &theme.tooltip);
        let Rect { x, y, w, h } = r;
        let corner = tt.corner(&theme.metrics);
        canvas.fill_round_rect(
            x as f32,
            y as f32,
            w as f32,
            h as f32,
            corner,
            &Paint::fill(tt.bg(pal)),
        );
        if let Some(border) = tt.border {
            canvas.stroke_round_rect(
                x as f32,
                y as f32,
                w as f32,
                h as f32,
                corner,
                tt.border_width().to_logical(canvas.dpi_scale()),
                &Paint::fill(border),
            );
        }
        let tr = Rect::new(x + TOOLTIP_PAD_X, y, w - 2 * TOOLTIP_PAD_X, h);
        canvas.draw_text(
            text,
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

    /// 在 125% 画布上画一次提示，返回 (pixmap, 浮层逻辑矩形)。
    fn paint_tooltip(border: Option<crate::geometry::Color>) -> (tiny_skia::Pixmap, Rect) {
        paint_tooltip_at(border, 10_000)
    }

    /// 同上，指定绘制时刻（悬停起点为 0）。
    fn paint_tooltip_at(
        border: Option<crate::geometry::Color>,
        now_ms: u64,
    ) -> (tiny_skia::Pixmap, Rect) {
        use crate::geometry::Color;
        use crate::render::SkiaCanvas;
        use crate::ui::Element;
        let mut tree = Tree::new();
        let root = Element::leaf().tooltip("提示").build(&mut tree);
        tree.root = Some(root);
        let mut theme = Theme::default();
        theme.tooltip.bg = Some(Color::WHITE);
        theme.tooltip.border = border;
        let st = TooltipState {
            pos: Point::new(20, 20),
            ..TooltipState::default()
        };
        let mut pm = tiny_skia::Pixmap::new(250, 125).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        {
            let mut eng = crate::text::NullTextEngine;
            let mut c = SkiaCanvas::with_text(&mut pm, &mut eng, 1.25);
            st.paint(
                &mut c,
                &tree,
                Some(root),
                false,
                &theme,
                Size::new(200, 100),
                now_ms,
            );
        }
        assert!(st.shown.get(), "前提：满延时应画出");
        let at = Point::new(20 + TOOLTIP_CURSOR_DX, 20 + TOOLTIP_CURSOR_DY);
        (pm, Rect::new(at.x, at.y, 0, 0))
    }

    /// 到点后淡入：刚到点几乎看不见、过一会儿变实、淡入结束与"早就显示着"逐像素一致。
    /// 描边用红色作墨量读数（白底白板上量不出浅灰文字的墨）。
    #[test]
    fn tooltip_fades_in_after_the_delay() {
        crate::anim::set_motion_level(crate::anim::MotionLevel::Full);
        let red = crate::geometry::Color::rgb(255, 0, 0);
        let delay = Theme::default().tooltip.delay_ms();
        let ink = |pm: &tiny_skia::Pixmap| {
            pm.pixels()
                .iter()
                .map(|p| 255 - p.green() as u32)
                .sum::<u32>()
        };
        let at = |dt: u64| ink(&paint_tooltip_at(Some(red), delay + dt).0);
        let (just, mid, done) = (at(0), at(40), at(10_000));
        assert!(done > 0, "前提：显示着的提示有墨");
        assert!(just < done / 10, "刚到点几乎透明：{just} / {done}");
        assert!(
            mid > just && mid < done,
            "淡入中途：{just} < {mid} < {done}"
        );
        assert_eq!(at(1_000), done, "淡入结束后与长期显示一致");
        crate::anim::set_motion_level(crate::anim::MotionLevel::Off);
        assert_eq!(at(0), done, "动画关闭时到点即完整显示");
        crate::anim::set_motion_level(crate::anim::MotionLevel::Full);
    }

    /// 主题给了描边色才描，且默认 1 物理像素：125% 下左边沿一行只染 1 列，不被摊成两列灰。
    #[test]
    fn tooltip_border_follows_theme_at_one_physical_pixel() {
        let red = crate::geometry::Color::rgb(255, 0, 0);
        let (plain, _) = paint_tooltip(None);
        let (pm, at) = paint_tooltip(Some(red));
        // 浮层左边沿附近、竖直取浮层中部（避开圆角）。
        let y = ((at.y as f32 + 10.0) * 1.25) as u32;
        let x0 = (at.x as f32 * 1.25) as u32;
        let reddish = |p: tiny_skia::PremultipliedColorU8| p.red() as i32 > p.green() as i32 + 40;
        let cols: Vec<u32> = (x0.saturating_sub(3)..x0 + 4)
            .filter(|&x| reddish(pm.pixel(x, y).unwrap()))
            .collect();
        assert!(!cols.is_empty(), "设了描边色应画出描边");
        assert_eq!(cols.len(), 1, "1 物理像素描边只应染 1 列，实际 {cols:?}");
        assert!(
            (0..plain.width()).all(|x| !reddish(plain.pixel(x, y).unwrap())),
            "未设描边色时不描"
        );
    }

    /// 延时取主题：800ms 的主题下停 600ms 不弹、并请求恰在到点时刻的一帧；满 800 才弹。
    #[test]
    fn delay_follows_theme() {
        use crate::render::SkiaCanvas;
        use crate::ui::Element;
        let mut tree = Tree::new();
        let root = Element::leaf().tooltip("提示").build(&mut tree);
        tree.root = Some(root);
        let mut theme = Theme::default();
        theme.tooltip.delay_ms = Some(800);
        let st = TooltipState {
            pos: Point::new(20, 20),
            since_ms: 1_000,
            ..TooltipState::default()
        };
        let shown_at = |now: u64| {
            let mut pm = tiny_skia::Pixmap::new(200, 100).unwrap();
            let mut eng = crate::text::NullTextEngine;
            let mut c = SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            crate::anim::reset_request();
            st.paint(
                &mut c,
                &tree,
                Some(root),
                false,
                &theme,
                Size::new(200, 100),
                now,
            );
            st.shown.get()
        };
        assert!(!shown_at(1_600), "600ms < 主题的 800ms，不该弹");
        assert_eq!(
            crate::anim::next_frame_delay_ms(),
            Some(200),
            "未到点时应请求恰在剩余 200ms 后的一帧"
        );
        assert!(shown_at(1_800), "满 800ms 应弹出");
    }

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
