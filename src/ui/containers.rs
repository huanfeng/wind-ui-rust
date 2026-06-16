//! 容器/导航控件的内部 widget：滚动滚轮、模态遮罩、标签按钮。

use std::cell::Cell;
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Color, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;

const TAB_ACCENT: Color = Color { r: 0x4C, g: 0x8B, b: 0xF5, a: 0xFF };
const TAB_INACTIVE: Color = Color { r: 0x70, g: 0x76, b: 0x7E, a: 0xFF };
const TAB_HOVER: Color = Color { r: 0x3A, g: 0x40, b: 0x48, a: 0xFF };

/// 滚动容器内部 widget：处理滚轮，调整节点滚动偏移。
pub struct ScrollWidget;

impl Widget for ScrollWidget {
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        if let Event::Pointer(p) = ev {
            if let PointerKind::Wheel(delta) = p.kind {
                // Windows 一刻度为 ±120；每刻度滚动 48px（delta>0 向上）。
                ctx.scroll_by(-delta * 48 / 120);
                return true;
            }
        }
        false
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

/// 标签按钮：点击切换共享选中索引，选中时高亮 + 底部指示条。
pub struct TabButton {
    label: String,
    group: Rc<Cell<usize>>,
    index: usize,
    hover: bool,
}

impl TabButton {
    pub fn new(label: String, group: Rc<Cell<usize>>, index: usize) -> Self {
        Self { label, group, index, hover: false }
    }
    fn selected(&self) -> bool {
        self.group.get() == self.index
    }
}

impl Widget for TabButton {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let t = text.measure(&self.label, style.font_family.as_deref(), style.font_size, None);
        Size::new(t.w + 24, t.h + 16)
    }
    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        let sel = self.selected();
        let color = if sel {
            TAB_ACCENT
        } else if self.hover {
            TAB_HOVER
        } else {
            TAB_INACTIVE
        };
        canvas.draw_text(&self.label, bounds, color, Align::Center, style.font_family.as_deref(), style.font_size);
        if sel {
            // 底部指示条
            let y = (bounds.y + bounds.h - 3) as f32;
            canvas.fill_round_rect(
                (bounds.x + 8) as f32,
                y,
                (bounds.w - 16) as f32,
                3.0,
                1.5,
                &Paint::fill(TAB_ACCENT),
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
