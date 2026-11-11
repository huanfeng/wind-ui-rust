//! 滑动选中高亮：纵向导航栏 / 侧栏里，选中底块从旧行**滑**到新行，而不是旧行淡出、新行淡入。
//!
//! 为什么要容器持有：每行各画各的底，两行之间没有"同一块东西在移动"这回事。高亮必须
//! 归容器所有——容器在子节点之前画它（所以在所有行的文字之下、悬停底之下），位置取
//! "当前选中行"的矩形。
//!
//! 谁是"选中行"由行自己说（[`Widget::slide_highlight`](crate::core::Widget::slide_highlight)），
//! 容器深度优先找第一个答 `Some` 的后代，不依赖下标——分组标题、折叠头混在中间也成立。
//!
//! 只在**选中行换了**时补间；同一行因布局变化（手风琴展开、窗口改尺寸）而挪位置时一律
//! 瞬时跟随，否则高亮会拖在行的后面。位置存**内容坐标**（含滚动偏移），滚动不会让
//! 补间中途的高亮漂走。

use std::cell::Cell;

use crate::anim::Transition;
use crate::core::NodeId;
use crate::geometry::Color;
use crate::theme::Theme;

/// 行对"我是选中行"的回答：高亮该画成什么样。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HighlightSpec {
    /// 相对行矩形左右各内缩多少（逻辑 px）。
    pub inset_x: f32,
    /// 相对行矩形上下各内缩多少（逻辑 px）。
    pub inset_y: f32,
    /// 圆角半径。
    pub corner: f32,
    /// 填充色。
    pub color: Color,
}

/// `[x, y, w, h]`，f32：补间在亚像素上走（GPU 档会再对齐到物理像素）。
pub(crate) type Quad = [f32; 4];

#[derive(Clone, Copy)]
struct State {
    /// 当前高亮所在的行；`None` = 没有选中行（不画）。
    owner: Option<NodeId>,
    from: Quad,
    to: Quad,
    /// 0→1 的进度。
    t: Transition<f32>,
}

/// 容器上的滑动高亮状态（懒分配在 `Node::slide_hl`）。
pub(crate) struct SlideHighlight {
    state: Cell<State>,
}

impl Default for SlideHighlight {
    fn default() -> Self {
        Self::new()
    }
}

impl SlideHighlight {
    pub fn new() -> Self {
        Self {
            state: Cell::new(State {
                owner: None,
                from: [0.0; 4],
                to: [0.0; 4],
                t: Transition::new(1.0).movement(),
            }),
        }
    }

    /// 给定本帧的选中行及其目标矩形（内容坐标），返回此刻应画的矩形；仍在补间时第二项为真。
    ///
    /// 首次出现、换行以外的位置变化、减弱动效档位下，都是瞬时落定。
    pub(crate) fn resolve(
        &self,
        owner: Option<NodeId>,
        target: Quad,
        theme: &Theme,
    ) -> (Quad, bool) {
        let mut s = self.state.get();
        let appeared = s.owner.is_none();
        if owner == s.owner && s.t.is_active() && s.to != target {
            // 同一行、补间途中目标变了（回弹、窗口缩放、FLIP）：只改终点，别把在播的滑动打断到终点。
            s.to = target;
        } else if owner != s.owner || s.to != target {
            let cur = Self::lerp(s.from, s.to, s.t.value());
            if owner.is_some() && !appeared && owner != s.owner {
                s.from = cur;
                s.to = target;
                s.t = Transition::new(0.0).movement();
                s.t.retarget(1.0, theme.anim.normal(), theme.anim.easing());
            } else {
                s.from = target;
                s.to = target;
                s.t.settle(1.0);
            }
            s.owner = owner;
        }
        let q = Self::lerp(s.from, s.to, s.t.value());
        let active = s.t.is_active();
        self.state.set(s);
        (q, active)
    }

    fn lerp(a: Quad, b: Quad, t: f32) -> Quad {
        std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anim::{set_clock_ms, set_motion_level, MotionLevel};

    fn id(i: u32) -> NodeId {
        NodeId::from_raw(i, 0)
    }

    fn setup() -> Theme {
        set_motion_level(MotionLevel::Full);
        set_clock_ms(1_000);
        Theme::default()
    }

    const A: Quad = [0.0, 0.0, 100.0, 30.0];
    const B: Quad = [0.0, 60.0, 100.0, 30.0];

    /// 首次出现直接落在目标上，不从零点滑进来。
    #[test]
    fn first_appearance_snaps() {
        let th = setup();
        let h = SlideHighlight::new();
        let (q, active) = h.resolve(Some(id(1)), A, &th);
        assert_eq!(q, A);
        assert!(!active);
    }

    /// 换行：起点是旧行，中途在两行之间，终点是新行。
    #[test]
    fn changing_owner_slides_between_rows() {
        let th = setup();
        let h = SlideHighlight::new();
        h.resolve(Some(id(1)), A, &th);
        set_clock_ms(2_000);
        let (q0, active) = h.resolve(Some(id(2)), B, &th);
        assert!(active, "换行后应进入补间");
        assert_eq!(q0, A, "起点仍在旧行");
        set_clock_ms(2_000 + th.anim.normal() as u64 / 2);
        let (mid, active) = h.resolve(Some(id(2)), B, &th);
        assert!(active);
        assert!(mid[1] > A[1] && mid[1] < B[1], "中途在两行之间：{mid:?}");
        set_clock_ms(2_000 + th.anim.normal() as u64 + 50);
        let (end, active) = h.resolve(Some(id(2)), B, &th);
        assert_eq!(end, B);
        assert!(!active);
    }

    /// 同一行因布局变化挪了位置：瞬时跟随，不起补间（否则展开手风琴时高亮拖在行后面）。
    #[test]
    fn same_owner_following_layout_does_not_animate() {
        let th = setup();
        let h = SlideHighlight::new();
        h.resolve(Some(id(1)), A, &th);
        set_clock_ms(2_000);
        let (q, active) = h.resolve(Some(id(1)), B, &th);
        assert_eq!(q, B);
        assert!(!active);
    }

    /// 没有选中行时不画；再出现时落定，不从别处飞来。
    #[test]
    fn disappearing_then_reappearing_snaps() {
        let th = setup();
        let h = SlideHighlight::new();
        h.resolve(Some(id(1)), A, &th);
        h.resolve(None, A, &th);
        set_clock_ms(2_000);
        let (q, active) = h.resolve(Some(id(2)), B, &th);
        assert_eq!(q, B);
        assert!(!active);
    }

    /// 减弱动效是位移类：直接到位。
    #[test]
    fn reduced_motion_snaps() {
        let th = setup();
        let h = SlideHighlight::new();
        h.resolve(Some(id(1)), A, &th);
        set_motion_level(MotionLevel::Reduced);
        set_clock_ms(2_000);
        let (q, active) = h.resolve(Some(id(2)), B, &th);
        assert_eq!(q, B);
        assert!(!active);
        set_motion_level(MotionLevel::Full);
    }

    /// 补间途中同一行的目标挪了：不打断，仍在补间且终点随之更新。
    #[test]
    fn target_moving_midflight_keeps_the_slide_going() {
        let th = setup();
        let h = SlideHighlight::new();
        h.resolve(Some(id(1)), A, &th);
        set_clock_ms(2_000);
        h.resolve(Some(id(2)), B, &th);
        set_clock_ms(2_000 + th.anim.normal() as u64 / 2);
        let moved: Quad = [0.0, 80.0, 100.0, 30.0];
        let (q, active) = h.resolve(Some(id(2)), moved, &th);
        assert!(active, "不应被打断");
        assert!(q[1] < moved[1], "仍在途中：{q:?}");
        set_clock_ms(2_000 + th.anim.normal() as u64 + 50);
        let (end, _) = h.resolve(Some(id(2)), moved, &th);
        assert_eq!(end, moved, "终点随目标更新");
    }

    /// 补间中途又换了一行：从**当前画面位置**起步，不跳回旧终点。
    #[test]
    fn retarget_midflight_starts_from_current_position() {
        let th = setup();
        let h = SlideHighlight::new();
        h.resolve(Some(id(1)), A, &th);
        set_clock_ms(2_000);
        h.resolve(Some(id(2)), B, &th);
        set_clock_ms(2_000 + th.anim.normal() as u64 / 2);
        let (mid, _) = h.resolve(Some(id(2)), B, &th);
        let c: Quad = [0.0, 120.0, 100.0, 30.0];
        let (q, active) = h.resolve(Some(id(3)), c, &th);
        assert!(active);
        assert!(
            (q[1] - mid[1]).abs() < 0.5,
            "起点应是当前位置 {mid:?}，实际 {q:?}"
        );
    }

    /// 墨心（非底色像素的纵向重心）与总墨量。底色为白，NullTextEngine 不画字，只剩高亮。
    fn ink(pm: &tiny_skia::Pixmap) -> (f32, u32) {
        let (mut sum_y, mut n) = (0u64, 0u32);
        for y in 0..pm.height() {
            for x in 0..pm.width() {
                let p = pm.pixel(x, y).unwrap();
                if (p.red(), p.green(), p.blue()) != (255, 255, 255) {
                    sum_y += y as u64;
                    n += 1;
                }
            }
        }
        (sum_y as f32 / n.max(1) as f32, n)
    }

    /// ★ 端到端：真实 `list_pill` 经真实 `Tree::paint`。选中从第 0 行改到第 2 行后，高亮的
    /// 重心先在旧行、中途落在两行之间、最终停在新行，且总墨量始终是**一块**高亮的量
    /// （不是旧行淡出 + 新行淡入那种两块叠加）。
    #[test]
    fn list_pill_highlight_slides_between_rows_through_real_paint() {
        use crate::core::Tree;
        use crate::geometry::Size;
        use crate::render::SkiaCanvas;
        use crate::signal::signal;
        use crate::ui::Element;

        let th = setup();
        let sel = signal(0usize);
        let ui = Element::list_pill(vec!["甲", "乙", "丙"], sel).size(200, 200);
        let mut tree = Tree::new();
        let root = ui.build(&mut tree);
        tree.root = Some(root);
        let mut eng = crate::text::NullTextEngine;
        let mut frame = |tree: &mut Tree| {
            tree.layout_root(Size::new(200, 200), &mut eng);
            let mut pm = tiny_skia::Pixmap::new(200, 200).unwrap();
            pm.fill(tiny_skia::Color::WHITE);
            {
                let mut eng2 = crate::text::NullTextEngine;
                let mut cv = SkiaCanvas::with_text(&mut pm, &mut eng2, 1.0);
                tree.paint(&mut cv);
            }
            ink(&pm)
        };

        let (y0, n0) = frame(&mut tree);
        assert!(n0 > 0, "前提：选中行有高亮");
        sel.set(2);
        set_clock_ms(2_000);
        let (y_start, n_start) = frame(&mut tree);
        assert!(
            (y_start - y0).abs() < 1.0,
            "刚切换时仍在旧行：{y0} → {y_start}"
        );
        set_clock_ms(2_000 + th.anim.normal() as u64 / 2);
        let (y_mid, n_mid) = frame(&mut tree);
        set_clock_ms(2_000 + th.anim.normal() as u64 + 100);
        let (y_end, n_end) = frame(&mut tree);
        assert!(y_end > y0 + 20.0, "最终落在第 2 行：{y0} → {y_end}");
        assert!(
            y_mid > y0 + 3.0 && y_mid < y_end - 3.0,
            "中途在两行之间：{y0} < {y_mid} < {y_end}"
        );
        // 一块高亮：墨量不随位置变（圆角在亚像素位置的抗锯齿差异留 15% 余量）。
        for n in [n_start, n_mid, n_end] {
            let r = n as f32 / n0 as f32;
            assert!((0.85..=1.15).contains(&r), "始终是一块高亮：{n0} vs {n}");
        }
    }

    /// 整块 pixmap 的"暗度"总和（255 - 通道值），比墨量更能反映叠层加深。
    fn darkness(pm: &tiny_skia::Pixmap) -> u64 {
        pm.pixels()
            .iter()
            .map(|p| (765 - p.red() as u32 - p.green() as u32 - p.blue() as u32) as u64)
            .sum()
    }

    fn paint_list(tree: &mut crate::core::Tree) -> tiny_skia::Pixmap {
        use crate::geometry::Size;
        let mut eng = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 200), &mut eng);
        let mut pm = tiny_skia::Pixmap::new(200, 200).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let mut eng2 = crate::text::NullTextEngine;
        let mut cv = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng2, 1.0);
        tree.paint(&mut cv);
        drop(cv);
        pm
    }

    /// 禁用的 list_pill 选中行仍要看得出选中：容器与行都不能因禁用而不画。
    #[test]
    fn disabled_list_pill_still_shows_selection() {
        use crate::core::Tree;
        use crate::signal::signal;
        use crate::ui::Element;
        setup();
        let ui = Element::list_pill(vec!["甲", "乙"], signal(0usize))
            .size(200, 200)
            .enabled(false);
        let mut tree = Tree::new();
        let root = ui.build(&mut tree);
        tree.root = Some(root);
        assert!(ink(&paint_list(&mut tree)).1 > 0, "禁用后选中行没有底色");
    }

    /// 鼠标停在选中行上不叠悬停底：与不悬停时逐像素同暗。
    #[test]
    fn hovering_the_selected_row_does_not_darken_it() {
        use crate::core::Tree;
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::signal::signal;
        use crate::ui::Element;
        setup();
        let ui = Element::list_pill(vec!["甲", "乙"], signal(0usize)).size(200, 200);
        let mut tree = Tree::new();
        let root = ui.build(&mut tree);
        tree.root = Some(root);
        let before = darkness(&paint_list(&mut tree));
        let row = tree.get(root).unwrap().children[0];
        let b = tree.abs_bounds(row);
        let c = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, c, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        // 悬停底有淡入补间：先画一帧让补间起跑，再等它跑完。
        set_clock_ms(5_000);
        paint_list(&mut tree);
        set_clock_ms(6_000);
        let after = darkness(&paint_list(&mut tree));
        assert_eq!(before, after, "悬停在选中行上不应加深");
    }
}
