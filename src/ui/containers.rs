//! 容器/导航控件的内部 widget：滚动滚轮、模态遮罩、标签按钮。

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

/// 滚动条右缘可抓取宽度（与 core::hit_node 的命中区一致）。
const SCROLLBAR_HIT_W: i32 = 10;
/// 滚动条 thumb 最小高（与 core paint 一致）。
const SCROLLBAR_MIN_THUMB: f32 = 24.0;
/// 标签内图标与文字间距。
const TAB_ICON_GAP: i32 = 6;

/// 滚动容器内部 widget：处理滚轮 + 拖动滚动条。
#[derive(Default)]
pub struct ScrollWidget {
    dragging: bool,
    start_y: i32,
    start_scroll: i32,
}

impl Widget for ScrollWidget {
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        let Event::Pointer(p) = ev else { return false };
        match p.kind {
            PointerKind::Wheel(delta) => {
                // Windows 一刻度为 ±120；每刻度滚动 48px（delta>0 向上）。
                ctx.scroll_by(-delta * 48 / 120);
                true
            }
            PointerKind::Down => {
                // 命中到这里且在右缘滚动条区域时启动拖动（hit_node 已优先派发）。
                let b = ctx.bounds();
                let (scroll_y, content_h, view_h) = ctx.scroll_metrics();
                if content_h > view_h && p.pos.x >= b.right() - SCROLLBAR_HIT_W {
                    self.dragging = true;
                    self.start_y = p.pos.y;
                    self.start_scroll = scroll_y;
                    ctx.capture();
                    true
                } else {
                    false
                }
            }
            PointerKind::Move if self.dragging => {
                let (_, content_h, view_h) = ctx.scroll_metrics();
                if view_h > 0 && content_h > view_h {
                    let max_scroll = content_h - view_h;
                    // 按 thumb 实际行程换算，精确反演绘制映射（thumb_h 与 core 同公式）。
                    let thumb_h =
                        (view_h as f32 * view_h as f32 / content_h as f32).max(SCROLLBAR_MIN_THUMB);
                    let travel = (view_h as f32 - thumb_h).max(1.0);
                    let dy = p.pos.y - self.start_y;
                    let delta = (dy as f32 * max_scroll as f32 / travel) as i32;
                    ctx.set_scroll((self.start_scroll + delta).clamp(0, max_scroll));
                }
                true
            }
            PointerKind::Up if self.dragging => {
                self.dragging = false;
                ctx.release_capture();
                true
            }
            _ => false,
        }
    }
}

/// 模态遮罩 widget：吞掉所有指针事件，阻止穿透到下层（命中链先于其下内容）。
pub struct ModalScrim;

impl Widget for ModalScrim {
    fn on_event(&mut self, _ctx: &mut EventCtx, ev: &Event) -> bool {
        // 仅吞指针事件；键盘仍可冒泡（如 Escape 关闭由宿主处理）。
        matches!(ev, Event::Pointer(_))
    }
}

/// 标签按钮：点击切换共享选中索引，选中时高亮 + 底部指示条。可选前置图标。
pub struct TabButton {
    label: String,
    icon: Option<ImageContent>,
    group: Rc<Cell<usize>>,
    index: usize,
    hover: bool,
}

impl TabButton {
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
    /// 标签视觉状态（供图标调制）：选中 > 悬停 > 普通。
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

impl Widget for TabButton {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let t = text.measure(&self.label, style.font_family.as_deref(), style.font_size, None);
        let icon_extra = if self.icon.is_some() { t.h + TAB_ICON_GAP } else { 0 };
        Size::new(t.w + 24 + icon_extra, t.h + 16)
    }
    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        let th = crate::theme::current();
        let (pal, tab) = (&th.palette, &th.tab);
        let sel = self.selected();
        let color = if sel {
            tab.accent(pal)
        } else if self.hover {
            tab.hover(pal)
        } else {
            tab.inactive(pal)
        };
        // 有图标：图标 + 文字作为整体水平居中（图标在左）；否则文字整体居中。
        if let Some(icon) = &self.icon {
            let ts = canvas.measure_text(&self.label, style.font_family.as_deref(), style.font_size);
            let ih = ts.h;
            let total_w = ih + TAB_ICON_GAP + ts.w;
            let sx = bounds.x + ((bounds.w - total_w) / 2).max(0);
            let iy = bounds.y + ((bounds.h - ih) / 2).max(0);
            let istyle = Style { corner_radius: 0.0, ..style.clone() };
            icon.paint_into(Rect::new(sx, iy, ih, ih), canvas, &istyle, self.visual_state());
            let tr = Rect::new(sx + ih + TAB_ICON_GAP, bounds.y, ts.w + 2, bounds.h);
            canvas.draw_text(&self.label, tr, color, Align::Start, style.font_family.as_deref(), style.font_size);
        } else {
            canvas.draw_text(&self.label, bounds, color, Align::Center, style.font_family.as_deref(), style.font_size);
        }
        if sel {
            // 底部指示条
            let y = (bounds.y + bounds.h - 3) as f32;
            canvas.fill_round_rect(
                (bounds.x + 8) as f32,
                y,
                (bounds.w - 16) as f32,
                3.0,
                1.5,
                &Paint::fill(tab.accent(pal)),
            );
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
    use crate::text::NullTextEngine;

    #[test]
    fn tab_icon_widens_measure() {
        let g = Rc::new(Cell::new(0));
        let style = Style::default();
        let mut te = NullTextEngine;
        let w0 = TabButton::new("Home".into(), g.clone(), 0).measure(Size::ZERO, &style, &mut te).w;
        let red = Image::from_rgba(4, 4, &[255u8, 0, 0, 255].repeat(4 * 4)).unwrap();
        let iconed =
            TabButton::new("Home".into(), g, 0).with_icon(ImageContent::new(Some(red)).fit(Fit::Fill));
        let w1 = iconed.measure(Size::ZERO, &style, &mut te).w;
        assert!(w1 > w0, "带图标标签应更宽：w0={w0}, w1={w1}");
    }

    #[test]
    fn tab_visual_state_tracks_selection() {
        let g = Rc::new(Cell::new(2));
        assert_eq!(TabButton::new("A".into(), g.clone(), 2).visual_state(), VisualState::Selected);
        assert_eq!(TabButton::new("B".into(), g, 0).visual_state(), VisualState::Normal);
    }
}
