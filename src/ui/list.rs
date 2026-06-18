//! 列表 ListView：可滚动的单选行列表。
//!
//! `Element::list` 复用滚动容器，每项是一个 [`ListRow`]——与 TabButton 同构：
//! 共享 `Rc<Cell<usize>>` 选中索引，点击设置自身索引，选中/悬停高亮。

use std::cell::Cell;
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::image::VisualState;
use crate::render::{Canvas, Paint};
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;
use crate::ui::ImageContent;

/// 行高（逻辑 px）。
pub const ROW_H: i32 = 36;
const PAD_X: i32 = 12;
/// 行内图标与文字间距。
const ICON_GAP: i32 = 8;

/// 单个列表行：点击设置共享选中索引，选中/悬停高亮。可选前置图标（复用 `ImageContent`）。
/// 事件契约与 `containers::TabButton` 同构，两者应保持同步。
pub struct ListRow {
    label: String,
    icon: Option<ImageContent>,
    group: Rc<Cell<usize>>,
    index: usize,
    hover: bool,
}

impl ListRow {
    pub fn new(label: String, group: Rc<Cell<usize>>, index: usize) -> Self {
        Self { label, icon: None, group, index, hover: false }
    }
    /// 附带前置图标。
    pub fn with_icon(mut self, icon: ImageContent) -> Self {
        self.icon = Some(icon);
        self
    }
    fn selected(&self) -> bool {
        self.group.get() == self.index
    }
    /// 行的视觉状态（供图标调制）：选中 > 悬停 > 普通。
    fn visual_state(&self) -> VisualState {
        if self.selected() {
            VisualState::Selected
        } else if self.hover {
            VisualState::Hover
        } else {
            VisualState::Normal
        }
    }
}

impl Widget for ListRow {
    fn measure(&self, avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(avail.w.max(0), ROW_H)
    }

    fn paint(&self, bounds: Rect, _content: Rect, focused: bool, enabled: bool, canvas: &mut dyn Canvas, style: &Style) {
        let th = crate::theme::current();
        let (pal, lt) = (&th.palette, &th.list);
        let sel = self.selected();
        let (x, y, w, h) = (bounds.x as f32, bounds.y as f32, bounds.w as f32, bounds.h as f32);
        if sel {
            canvas.fill_rect(x, y, w, h, &Paint::fill(lt.selected_bg(pal)));
        } else if self.hover {
            canvas.fill_rect(x, y, w, h, &Paint::fill(lt.hover_bg(pal)));
        }
        // 选中左缘强调条（禁用时不强调）。
        if sel && enabled {
            canvas.fill_rect(x, y, 3.0, h, &Paint::fill(pal.accent));
        }
        // 前置图标：方形、垂直居中；文字相应右移。禁用走 Disabled 调制。
        let vstate = if !enabled { VisualState::Disabled } else { self.visual_state() };
        let mut text_x = bounds.x + PAD_X;
        if let Some(icon) = &self.icon {
            let side = (bounds.h - 14).max(0);
            let iy = bounds.y + (bounds.h - side) / 2;
            let istyle = Style { corner_radius: 0.0, ..style.clone() };
            icon.paint_into(Rect::new(bounds.x + PAD_X, iy, side, side), canvas, &istyle, vstate);
            text_x = bounds.x + PAD_X + side + ICON_GAP;
        }
        let color = if !enabled {
            pal.text_disabled
        } else if sel {
            lt.selected_text(pal)
        } else {
            lt.text(pal)
        };
        let tw = (bounds.right() - PAD_X - text_x).max(0);
        let tr = Rect::new(text_x, bounds.y, tw, bounds.h);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::image::{Fit, Image};
    use crate::render::SkiaCanvas;
    use tiny_skia::Pixmap;

    #[test]
    fn row_with_icon_paints_icon_area() {
        let group = Rc::new(Cell::new(0));
        let red = Image::from_rgba(4, 4, &[255u8, 0, 0, 255].repeat(4 * 4)).unwrap();
        let row = ListRow::new("Inbox".into(), group, 0)
            .with_icon(ImageContent::new(Some(red)).fit(Fit::Fill));
        let mut pm = Pixmap::new(200, ROW_H as u32).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut pm);
            row.paint(
                Rect::new(0, 0, 200, ROW_H),
                Rect::new(0, 0, 200, ROW_H),
                false,
                true,
                &mut c,
                &Style::default(),
            );
        }
        // 图标方块中心（PAD_X 起、垂直居中）应为红色。
        let side = ROW_H - 14;
        let p = pm.pixel((PAD_X + side / 2) as u32, (ROW_H / 2) as u32).unwrap();
        assert!(p.red() > 180 && p.green() < 90, "行图标应绘制红色，实得 ({},{},{})", p.red(), p.green(), p.blue());
    }

    #[test]
    fn row_visual_state_tracks_selection() {
        let group = Rc::new(Cell::new(1));
        let selected = ListRow::new("A".into(), group.clone(), 1);
        assert_eq!(selected.visual_state(), VisualState::Selected);
        let other = ListRow::new("B".into(), group, 2);
        assert_eq!(other.visual_state(), VisualState::Normal);
    }
}
