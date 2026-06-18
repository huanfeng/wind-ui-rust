//! 导航原语：`NavRow`（带 chevron 的钻入行）+ `CollapsibleHeader`（可折叠分组头）。
//!
//! 两者是"设置/导航界面"的常见构件：NavRow 是点击钻入子页的行（右侧 `>`），
//! CollapsibleHeader 是点击展开/收起子区的分组标题（右侧三角随状态翻转）。
//! 二者共享 [`NavTheme`](crate::theme::NavTheme)（文字/悬停底/箭头色）。
//!
//! 设计取舍：侧栏中"选中态高亮"的导航项请直接复用 [`Element::list`](crate::ui::Element::list)
//! （ListRow 已有选中高亮 + 左强调条 + 图标）；NavRow 专注"动作/钻入"语义（无持久选中态），
//! Collapsible 专注"展开/收起"，三者组合即可拼出侧栏分组与内容区钻入，互不重复。

use std::cell::Cell;
use std::rc::Rc;

use crate::core::{ClickFn, EventCtx, Widget};
use crate::event::{CursorShape, Event, Key, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;

/// 行高（逻辑 px），NavRow 与 CollapsibleHeader 共用。
pub const NAV_ROW_H: i32 = 40;
const PAD_X: i32 = 12;
/// 右侧箭头区宽度。
const CHEVRON_W: i32 = 20;

/// 可点击控件三态（与 Link/Button 同构：press 态支持拖出取消）。
#[derive(PartialEq, Eq, Clone, Copy)]
enum State {
    Normal,
    Hover,
    Press,
}

/// 在 `(cx, cy)` 处画一个朝右的箭头 `>`（NavRow 钻入 / 折叠收起态）。
fn chevron_right(canvas: &mut dyn Canvas, cx: f32, cy: f32, color: crate::geometry::Color) {
    let p = Paint::fill(color);
    canvas.draw_line(cx - 2.0, cy - 4.0, cx + 2.0, cy, 1.6, &p);
    canvas.draw_line(cx + 2.0, cy, cx - 2.0, cy + 4.0, 1.6, &p);
}

/// 在 `(cx, cy)` 处画一个朝下的箭头 `v`（折叠展开态，与 Dropdown 一致）。
fn chevron_down(canvas: &mut dyn Canvas, cx: f32, cy: f32, color: crate::geometry::Color) {
    let p = Paint::fill(color);
    canvas.draw_line(cx - 4.0, cy - 2.0, cx, cy + 3.0, 1.6, &p);
    canvas.draw_line(cx, cy + 3.0, cx + 4.0, cy - 2.0, 1.6, &p);
}

// ---------------- NavRow ----------------

/// 导航行：左标签 + 右侧 `>`，悬停高亮，点击/回车触发回调（钻入子页）。
/// 无持久选中态——选中高亮的导航请用 `Element::list`。
pub struct NavRow {
    label: String,
    state: State,
    on_click: Option<ClickFn>,
}

impl NavRow {
    pub fn new(label: String) -> Self {
        Self { label, state: State::Normal, on_click: None }
    }
    fn activate(&mut self, ctx: &mut EventCtx) {
        if let Some(cb) = self.on_click.as_mut() {
            cb(ctx);
        }
    }
}

impl Widget for NavRow {
    fn measure(&self, avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(avail.w.max(0), NAV_ROW_H)
    }

    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, enabled: bool, canvas: &mut dyn Canvas, style: &Style) {
        let th = crate::theme::current();
        let (pal, nav) = (&th.palette, &th.nav);
        let (x, y, w, h) = (bounds.x as f32, bounds.y as f32, bounds.w as f32, bounds.h as f32);
        // 悬停/按下底色（禁用不画）。
        if enabled && self.state != State::Normal {
            canvas.fill_rect(x, y, w, h, &Paint::fill(nav.hover_bg(pal)));
        }
        let text_color = if enabled { nav.text(pal) } else { pal.text_disabled };
        let chevron = if enabled { nav.chevron(pal) } else { pal.text_disabled };
        // 标签（左，留出右侧箭头区）。
        let tr = Rect::new(bounds.x + PAD_X, bounds.y, bounds.w - 2 * PAD_X - CHEVRON_W, bounds.h);
        canvas.draw_text(&self.label, tr, text_color, Align::Start, style.font_family.as_deref(), style.font_size);
        // 右侧钻入箭头。
        let cx = bounds.x as f32 + bounds.w as f32 - PAD_X as f32 - CHEVRON_W as f32 / 2.0;
        let cy = bounds.y as f32 + bounds.h as f32 / 2.0;
        chevron_right(canvas, cx, cy, chevron);
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    if self.state == State::Normal {
                        self.state = State::Hover;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.state != State::Press {
                        self.state = State::Normal;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Down => {
                    self.state = State::Press;
                    ctx.capture();
                    ctx.request_focus();
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Up => {
                    let was_press = self.state == State::Press;
                    let inside = ctx.bounds().contains(p.pos);
                    self.state = if inside { State::Hover } else { State::Normal };
                    ctx.release_capture();
                    ctx.mark_dirty();
                    if was_press && inside {
                        self.activate(ctx);
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed && (k.key == Key::Enter || k.key == Key::Space) => {
                self.activate(ctx);
                ctx.mark_dirty();
                true
            }
            _ => false,
        }
    }

    fn focusable(&self) -> bool {
        true
    }
    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }
    fn take_click(&mut self, f: ClickFn) {
        self.on_click = Some(f);
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

// ---------------- CollapsibleHeader ----------------

/// 可折叠分组头：左标题 + 右侧三角（展开 `v` / 收起 `>`），点击/回车切换 `expanded`。
/// 配套 body 由 `Element::collapsible` 用 `visible_when(expanded)` 显隐。
///
/// 不同于 NavRow：折叠头用 `hover: bool` 而非三态、按下不 `capture()`——toggle
/// 语义无需"拖出取消"，也无独立按压色，故不引入 Press 态。
pub struct CollapsibleHeader {
    title: String,
    expanded: Rc<Cell<bool>>,
    hover: bool,
}

impl CollapsibleHeader {
    pub fn new(title: String, expanded: Rc<Cell<bool>>) -> Self {
        Self { title, expanded, hover: false }
    }
    fn toggle(&self, ctx: &mut EventCtx) {
        self.expanded.set(!self.expanded.get());
        ctx.mark_dirty();
    }
}

impl Widget for CollapsibleHeader {
    fn measure(&self, avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(avail.w.max(0), NAV_ROW_H)
    }

    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, enabled: bool, canvas: &mut dyn Canvas, style: &Style) {
        let th = crate::theme::current();
        let (pal, nav) = (&th.palette, &th.nav);
        let (x, y, w, h) = (bounds.x as f32, bounds.y as f32, bounds.w as f32, bounds.h as f32);
        if enabled && self.hover {
            canvas.fill_rect(x, y, w, h, &Paint::fill(nav.hover_bg(pal)));
        }
        let text_color = if enabled { nav.text(pal) } else { pal.text_disabled };
        let chevron = if enabled { nav.chevron(pal) } else { pal.text_disabled };
        let tr = Rect::new(bounds.x + PAD_X, bounds.y, bounds.w - 2 * PAD_X - CHEVRON_W, bounds.h);
        canvas.draw_text(&self.title, tr, text_color, Align::Start, style.font_family.as_deref(), style.font_size);
        let cx = bounds.x as f32 + bounds.w as f32 - PAD_X as f32 - CHEVRON_W as f32 / 2.0;
        let cy = bounds.y as f32 + bounds.h as f32 / 2.0;
        if self.expanded.get() {
            chevron_down(canvas, cx, cy, chevron);
        } else {
            chevron_right(canvas, cx, cy, chevron);
        }
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    self.hover = true;
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Leave => {
                    self.hover = false;
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Down => {
                    ctx.request_focus();
                    true
                }
                PointerKind::Up => {
                    if ctx.bounds().contains(p.pos) {
                        self.toggle(ctx);
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed && (k.key == Key::Enter || k.key == Key::Space) => {
                self.toggle(ctx);
                true
            }
            _ => false,
        }
    }

    fn focusable(&self) -> bool {
        true
    }
    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{NodeId, Tree};
    use crate::event::{KeyEvent, MouseButton, PointerEvent};
    use crate::geometry::Point;
    use crate::ui::Element;

    fn build(el: Element) -> (Tree, NodeId) {
        let mut tree = Tree::new();
        let root = el.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        (tree, root)
    }

    fn click(tree: &mut Tree, at: Point) {
        let mut hover = None;
        let mut capture = None;
        tree.dispatch_pointer(PointerEvent::single(PointerKind::Down, at, MouseButton::Left), &mut hover, &mut capture);
        tree.dispatch_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left), &mut hover, &mut capture);
    }

    #[test]
    fn nav_row_click_fires_callback_and_hand_cursor() {
        let hit = Rc::new(Cell::new(0));
        let h2 = hit.clone();
        let (mut tree, root) =
            build(Element::nav_row("双拼方案设定").width(180).height(40).on_click(move |_| h2.set(h2.get() + 1)));
        click(&mut tree, Point::new(40, 20));
        assert_eq!(hit.get(), 1, "点击导航行应触发回调");
        assert_eq!(tree.cursor_at(root), CursorShape::Hand, "导航行应报告手型光标");
    }

    #[test]
    fn collapsible_header_toggles_expanded() {
        // 走公开构建器：header 在顶部 0..40，点击切换 expanded。
        let expanded = Rc::new(Cell::new(false));
        let (mut tree, _root) = build(
            Element::collapsible("属性设置", expanded.clone(), Element::label("子项").height(30)).width(180),
        );
        click(&mut tree, Point::new(40, 20));
        assert!(expanded.get(), "首次点击 header 应展开");
        click(&mut tree, Point::new(40, 20));
        assert!(!expanded.get(), "再次点击应收起");
    }

    #[test]
    fn collapsible_body_hidden_when_collapsed() {
        // collapsible 构建器：body 用 visible_when(expanded) 显隐。收起时 body 不可见。
        let expanded = Rc::new(Cell::new(false));
        let (tree, root) = build(
            Element::collapsible(
                "分组",
                expanded.clone(),
                Element::label("子项").width_match().height(30),
            )
            .width(180),
        );
        // 根是 col，含 header + body 两个子节点；收起时 body 节点不可见。
        let kids = tree.get(root).unwrap().children.clone();
        assert_eq!(kids.len(), 2, "collapsible 应有 header + body 两个子节点");
        let body_visible = |t: &Tree| t.get(kids[1]).unwrap().effective_visible();
        assert!(!body_visible(&tree), "收起时 body 应不可见");
        expanded.set(true);
        assert!(body_visible(&tree), "展开后 body 应可见");
    }

    #[test]
    fn nav_key_enter_activates() {
        let hit = Rc::new(Cell::new(0));
        let h2 = hit.clone();
        let (mut tree, root) =
            build(Element::nav_row("钻入").width(180).height(40).on_click(move |_| h2.set(h2.get() + 1)));
        tree.dispatch_key(KeyEvent { key: Key::Enter, pressed: true, shift: false, ctrl: false }, Some(root));
        assert_eq!(hit.get(), 1, "回车应激活导航行");
    }
}
