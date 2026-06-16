//! 列表 ListView：可滚动的单选行列表。
//!
//! `Element::list` 复用滚动容器，每项是一个 [`ListRow`]——与 TabButton 同构：
//! 共享 `Rc<Cell<usize>>` 选中索引，点击设置自身索引，选中/悬停高亮。

use std::cell::Cell;
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;

/// 行高（逻辑 px）。
pub const ROW_H: i32 = 36;
const PAD_X: i32 = 12;

/// 单个列表行：点击设置共享选中索引，选中/悬停高亮。
/// 事件契约与 `containers::TabButton` 同构，两者应保持同步。
pub struct ListRow {
    label: String,
    group: Rc<Cell<usize>>,
    index: usize,
    hover: bool,
}

impl ListRow {
    pub fn new(label: String, group: Rc<Cell<usize>>, index: usize) -> Self {
        Self { label, group, index, hover: false }
    }
    fn selected(&self) -> bool {
        self.group.get() == self.index
    }
}

impl Widget for ListRow {
    fn measure(&self, avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(avail.w.max(0), ROW_H)
    }

    fn paint(&self, bounds: Rect, _content: Rect, focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        let th = crate::theme::current();
        let (pal, lt) = (&th.palette, &th.list);
        let sel = self.selected();
        let (x, y, w, h) = (bounds.x as f32, bounds.y as f32, bounds.w as f32, bounds.h as f32);
        if sel {
            canvas.fill_rect(x, y, w, h, &Paint::fill(lt.selected_bg(pal)));
        } else if self.hover {
            canvas.fill_rect(x, y, w, h, &Paint::fill(lt.hover_bg(pal)));
        }
        // 选中左缘强调条。
        if sel {
            canvas.fill_rect(x, y, 3.0, h, &Paint::fill(pal.accent));
        }
        let color = if sel { lt.selected_text(pal) } else { lt.text(pal) };
        let tr = Rect::new(bounds.x + PAD_X, bounds.y, bounds.w - 2 * PAD_X, bounds.h);
        canvas.draw_text(&self.label, tr, color, Align::Start, style.font_family.as_deref(), style.font_size);
        // 键盘焦点时描边（仅当前焦点行）。
        let _ = focused;
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
                        self.group.set(self.index);
                        ctx.mark_dirty();
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed && (k.key == Key::Enter || k.key == Key::Space) => {
                self.group.set(self.index);
                ctx.mark_dirty();
                true
            }
            _ => false,
        }
    }

    fn focusable(&self) -> bool {
        true
    }
}
