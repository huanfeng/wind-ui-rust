//! 分段控制器 SegmentedControl：连体多段单选，选中段高亮。
//!
//! 与 [`RadioButton`](crate::ui::RadioButton) 语义相同（绑定 `Rc<Cell<usize>>` 选中索引），
//! 但视觉为一条连体胶囊：等宽分段、段间分隔线、选中段填充强调色底 + 强调色文字。
//! 适合"二/三选一"的紧凑切换（简体/繁体、半角/全角等输入法常见开关）。
//!
//! 事件契约：点击某段即选中该段；悬停逐段高亮（依赖 `Move` 派发到悬停控件，
//! 见 `core::dispatch_pointer`）；获得焦点后左右方向键在相邻段间移动选中。

use std::cell::Cell;
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;

/// 段内左右内边距（measure 用，决定等宽段的自然宽度）。
const SEG_PAD_X: i32 = 14;

/// 多段单选控件。各段等宽，选中段高亮，段间以分隔线连体。
pub struct SegmentedControl {
    options: Vec<String>,
    selected: Rc<Cell<usize>>,
    /// 当前悬停段下标（仅视觉，不写绑定状态）。
    hover: Option<usize>,
}

impl SegmentedControl {
    pub fn new(options: Vec<String>, selected: Rc<Cell<usize>>) -> Self {
        Self { options, selected, hover: None }
    }

    fn len(&self) -> usize {
        self.options.len()
    }

    /// 夹紧后的有效选中下标（绑定值越界时回退到末段）。
    fn sel(&self) -> usize {
        self.selected.get().min(self.len().saturating_sub(1))
    }

    /// 第 `i` 段的横向范围 `[x0, x1)`。整数边界按 `i*w/n` 计算，保证相邻段
    /// 首尾相接、无缝无叠，且末段右界恰为 `bounds` 右缘（不溢出）。
    fn seg_x(&self, bounds: Rect, i: usize) -> (i32, i32) {
        let n = self.len().max(1) as i32;
        let x0 = bounds.x + (bounds.w * i as i32) / n;
        let x1 = bounds.x + (bounds.w * (i as i32 + 1)) / n;
        (x0, x1)
    }

    /// 命中点 `x` 落在第几段（夹紧到 `[0, n)`）。与 `seg_x` 同一映射的反演。
    fn seg_at(&self, bounds: Rect, x: i32) -> usize {
        let n = self.len();
        if n == 0 || bounds.w <= 0 {
            return 0;
        }
        let rel = (x - bounds.x).clamp(0, bounds.w - 1);
        ((rel * n as i32) / bounds.w).clamp(0, n as i32 - 1) as usize
    }

    fn select(&self, ctx: &mut EventCtx, i: usize) {
        if i < self.len() && self.selected.get() != i {
            self.selected.set(i);
            ctx.mark_dirty();
        }
    }
}

impl Widget for SegmentedControl {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        // 取最宽选项作为统一段宽，乘段数 → 等宽连体。
        let mut tw = 0;
        let mut th = 0;
        for o in &self.options {
            let t = text.measure(o, style.font_family.as_deref(), style.font_size, None);
            tw = tw.max(t.w);
            th = th.max(t.h);
        }
        let seg_w = tw + 2 * SEG_PAD_X;
        let h = th.max(style.font_size as i32) + 14;
        Size::new(seg_w * self.len().max(1) as i32, h)
    }

    fn paint(&self, bounds: Rect, _content: Rect, focused: bool, enabled: bool, canvas: &mut dyn Canvas, style: &Style) {
        let t = crate::theme::current();
        let (pal, sg) = (&t.palette, &t.segment);
        let (x, y, w, h) = (bounds.x as f32, bounds.y as f32, bounds.w as f32, bounds.h as f32);
        let corner = sg.corner(&t.metrics);
        let n = self.len();
        let sel = self.sel();

        // 容器底。
        canvas.fill_round_rect(x, y, w, h, corner, &Paint::fill(sg.bg(pal)));

        // 逐段：选中段填强调底，悬停段填浅底；文字居中。
        let family = style.font_family.as_deref();
        let fsize = style.font_size;
        for i in 0..n {
            let (x0, x1) = self.seg_x(bounds, i);
            if i == sel {
                let bg = if enabled { sg.selected_bg(pal) } else { pal.surface_alt };
                canvas.fill_round_rect(
                    (x0 + 2) as f32,
                    (bounds.y + 2) as f32,
                    (x1 - x0 - 4) as f32,
                    (bounds.h - 4) as f32,
                    (corner - 2.0).max(0.0),
                    &Paint::fill(bg),
                );
            } else if self.hover == Some(i) && enabled {
                canvas.fill_round_rect(
                    (x0 + 2) as f32,
                    (bounds.y + 2) as f32,
                    (x1 - x0 - 4) as f32,
                    (bounds.h - 4) as f32,
                    (corner - 2.0).max(0.0),
                    &Paint::fill(sg.hover_bg(pal)),
                );
            }
            let tc = if i == sel {
                if enabled { sg.selected_text(pal) } else { pal.text_disabled }
            } else if enabled {
                sg.text(pal)
            } else {
                pal.text_disabled
            };
            let seg = Rect::new(x0, bounds.y, x1 - x0, bounds.h);
            canvas.draw_text(&self.options[i], seg, tc, Align::Center, family, fsize);
        }

        // 段间分隔线：仅画两侧都非选中的边界（选中段填充自带视觉边界）。
        let divider = sg.divider(pal);
        for i in 1..n {
            if i == sel || i - 1 == sel {
                continue;
            }
            let (x0, _) = self.seg_x(bounds, i);
            canvas.draw_line(
                x0 as f32,
                (bounds.y + 6) as f32,
                x0 as f32,
                (bounds.y + bounds.h - 6) as f32,
                1.0,
                &Paint::fill(divider),
            );
        }

        // 外边框最后描，盖住分隔线端点与选中底的圆角缝。聚焦时换强调色 + 加粗，
        // 让键盘焦点可见（控件支持左右键切换，需可发现）——对齐 Dropdown 惯例。
        let (border, bw) = if !enabled {
            (pal.track, 1.5)
        } else if focused {
            (sg.border_focus(pal), 1.8)
        } else {
            (sg.border(pal), 1.5)
        };
        canvas.stroke_round_rect(x, y, w, h, corner, bw, &Paint::fill(border));
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Down => {
                    ctx.request_focus();
                    true
                }
                PointerKind::Up => {
                    if ctx.bounds().contains(p.pos) {
                        let i = self.seg_at(ctx.bounds(), p.pos.x);
                        self.select(ctx, i);
                    }
                    true
                }
                PointerKind::Move | PointerKind::Enter => {
                    let i = self.seg_at(ctx.bounds(), p.pos.x);
                    if self.hover != Some(i) {
                        self.hover = Some(i);
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.hover.is_some() {
                        self.hover = None;
                        ctx.mark_dirty();
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed => match k.key {
                Key::Left => {
                    let cur = self.sel();
                    if cur > 0 {
                        self.select(ctx, cur - 1);
                    }
                    true
                }
                Key::Right => {
                    let cur = self.sel();
                    if cur + 1 < self.len() {
                        self.select(ctx, cur + 1);
                    }
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn focusable(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{MouseButton, PointerEvent};
    use crate::geometry::Point;
    use crate::ui::Element;

    /// 在 200×200 窗口布局单个分段控件，返回 (tree, root)。
    fn layout(el: Element) -> Tree {
        let mut tree = Tree::new();
        let root = el.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        tree
    }

    use crate::core::Tree;

    /// 合成一次完整点击（Down→Up）于 `at`。
    fn click(tree: &mut Tree, at: Point) -> crate::core::DispatchResult {
        let mut hover = None;
        let mut capture = None;
        tree.dispatch_pointer(PointerEvent::single(PointerKind::Down, at, MouseButton::Left), &mut hover, &mut capture);
        tree.dispatch_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left), &mut hover, &mut capture)
    }

    #[test]
    fn click_selects_segment_under_pointer() {
        let sel = Rc::new(Cell::new(0usize));
        // 180 宽 3 段 → 每段 60：[0,60) [60,120) [120,180)。根布局落在 (0,0)。
        let mut tree = layout(
            Element::segmented(vec!["简体", "繁体", "其它"], sel.clone()).width(180).height(32),
        );
        click(&mut tree, Point::new(150, 16)); // 第三段
        assert_eq!(sel.get(), 2, "点击第三段应选中索引 2");
        click(&mut tree, Point::new(30, 16)); // 第一段
        assert_eq!(sel.get(), 0, "点击第一段应选中索引 0");
    }

    #[test]
    fn arrow_keys_move_selection() {
        let sel = Rc::new(Cell::new(0usize));
        let mut tree = layout(
            Element::segmented(vec!["A", "B", "C"], sel.clone()).width(180).height(32),
        );
        // 先点击聚焦（Down 请求焦点），再用方向键移动。
        let root = tree.root;
        let mut hover = None;
        let mut capture = None;
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, Point::new(30, 16), MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        let focus = res.focus.or(root);
        let right = crate::event::KeyEvent { key: Key::Right, pressed: true, shift: false, ctrl: false };
        tree.dispatch_key(right, focus);
        assert_eq!(sel.get(), 1, "右键应移到下一段");
        tree.dispatch_key(right, focus);
        assert_eq!(sel.get(), 2);
        let left = crate::event::KeyEvent { key: Key::Left, pressed: true, shift: false, ctrl: false };
        tree.dispatch_key(left, focus);
        assert_eq!(sel.get(), 1, "左键应移回上一段");
    }

    #[test]
    fn seg_at_maps_boundaries() {
        let sc = SegmentedControl::new(vec!["a".into(), "b".into(), "c".into()], Rc::new(Cell::new(0)));
        let b = Rect::new(0, 0, 180, 32);
        assert_eq!(sc.seg_at(b, 0), 0);
        assert_eq!(sc.seg_at(b, 59), 0);
        assert_eq!(sc.seg_at(b, 60), 1);
        assert_eq!(sc.seg_at(b, 179), 2);
        // 越界夹紧。
        assert_eq!(sc.seg_at(b, 999), 2);
        assert_eq!(sc.seg_at(b, -5), 0);
    }
}
