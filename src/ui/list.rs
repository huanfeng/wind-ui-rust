//! 列表 ListView：可滚动的单选行列表。
//!
//! `Element::list` 复用滚动容器，每项是一个 [`ListRow`]——与 TabButton 同构：
//! 共享 `Rc<Cell<usize>>` 选中索引，点击设置自身索引，选中/悬停高亮。

use std::cell::Cell;

use crate::anim::{Easing, Transition};
use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Color, Rect, Size};
use crate::render::image::VisualState;
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
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
/// 圆角 pill 选中样式的内缩与圆角（逻辑 px）。侧栏导航等用，更贴近现代设计稿。
const PILL_INSET_X: f32 = 6.0;
const PILL_INSET_Y: f32 = 3.0;
const PILL_RADIUS: f32 = 7.0;

pub struct ListRow {
    label: String,
    icon: Option<ImageContent>,
    group: Signal<usize>,
    index: usize,
    hover: bool,
    /// pill 样式：选中/悬停底色画成内缩圆角矩形、且不绘左缘强调条（侧栏导航用）。
    pill: bool,
    /// 行底色"存在量"补间（0..1）：对固定目标色缩 alpha 淡入淡出，避免从透明黑 lerp 过黑。
    bg_amt: Cell<Transition<f32>>,
    /// 记住的底色（选中/悬停色），淡出期沿用以保 RGB 不变。
    bg_color: Cell<Color>,
    /// 选中左缘强调条补间（0..1）：淡入。
    sel: Cell<Transition<f32>>,
}

impl ListRow {
    pub fn new(label: String, group: Signal<usize>, index: usize) -> Self {
        let on = if group.get() == index { 1.0 } else { 0.0 };
        Self {
            label,
            icon: None,
            group,
            index,
            hover: false,
            pill: false,
            bg_amt: Cell::new(Transition::new(on)),
            bg_color: Cell::new(Color::TRANSPARENT),
            sel: Cell::new(Transition::new(on)),
        }
    }
    /// 附带前置图标。
    pub fn with_icon(mut self, icon: ImageContent) -> Self {
        self.icon = Some(icon);
        self
    }
    /// 启用 pill 选中样式：选中/悬停底为内缩圆角矩形、去掉左缘强调条。
    pub fn pill(mut self) -> Self {
        self.pill = true;
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

    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        let (pal, lt) = (&th.palette, &th.list);
        let sel = self.selected();
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        // 底色：选中 > 悬停 > 无。用"存在量"补间对固定目标色缩 alpha 淡入淡出——
        // 记住当前底色，淡出期沿用其 RGB（不从透明黑 lerp，避免中途变暗）。
        let want = if sel {
            Some(lt.selected_bg(pal))
        } else if self.hover {
            Some(lt.hover_bg(pal))
        } else {
            None
        };
        if let Some(c) = want {
            self.bg_color.set(c);
        }
        let mut bg = self.bg_amt.get();
        let bg_target = if want.is_some() { 1.0 } else { 0.0 };
        if bg.target() != bg_target {
            bg.retarget(bg_target, th.anim.fast(), Easing::EaseOut);
        }
        let bg_amt = bg.animate();
        self.bg_amt.set(bg);
        if bg_amt > 0.0 {
            let c = self.bg_color.get().scale_alpha(bg_amt);
            if self.pill {
                // pill：内缩圆角矩形（贴近现代侧栏导航设计稿）。
                canvas.fill_round_rect(
                    x + PILL_INSET_X,
                    y + PILL_INSET_Y,
                    (w - 2.0 * PILL_INSET_X).max(0.0),
                    (h - 2.0 * PILL_INSET_Y).max(0.0),
                    PILL_RADIUS,
                    &Paint::fill(c),
                );
            } else {
                canvas.fill_rect(x, y, w, h, &Paint::fill(c));
            }
        }
        // 选中左缘强调条补间（禁用时不强调）：淡入。pill 样式不绘左条（靠圆角底块表达选中）。
        let mut selt = self.sel.get();
        let sel_target = if sel && enabled && !self.pill {
            1.0
        } else {
            0.0
        };
        if selt.target() != sel_target {
            selt.retarget(sel_target, th.anim.normal(), Easing::EaseOut);
        }
        let sel_amt = selt.animate();
        self.sel.set(selt);
        if sel_amt > 0.0 {
            canvas.fill_rect(x, y, 3.0, h, &Paint::fill(pal.accent.scale_alpha(sel_amt)));
        }
        // 前置图标：方形、垂直居中；文字相应右移。禁用走 Disabled 调制。
        let vstate = if !enabled {
            VisualState::Disabled
        } else {
            self.visual_state()
        };
        // pill 样式文字左移与 pill 内缩对齐（更舒展的内边距，短标题也美观）。
        let pad_x = if self.pill {
            PILL_INSET_X as i32 + 10
        } else {
            PAD_X
        };
        let mut text_x = bounds.x + pad_x;
        if let Some(icon) = &self.icon {
            let side = (bounds.h - 14).max(0);
            let iy = bounds.y + (bounds.h - side) / 2;
            let istyle = Style {
                corner_radius: 0.0,
                ..style.clone()
            };
            icon.paint_into(
                Rect::new(bounds.x + pad_x, iy, side, side),
                canvas,
                &istyle,
                vstate,
            );
            text_x = bounds.x + pad_x + side + ICON_GAP;
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
        canvas.draw_text(
            &self.label,
            tr,
            color,
            Align::Start,
            style.font_family.as_deref(),
            style.font_size,
        );
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
                        // 选中是共享状态：改写后旧选中行（另一节点）也要重绘以清掉其高亮，
                        // 仅 mark_dirty 只失效本行 → 旧行高亮残留（移动鼠标整窗重绘才清）。
                        // 故升整窗（与 RadioButton 同处理）。
                        ctx.mark_dirty_all();
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed && (k.key == Key::Enter || k.key == Key::Space) => {
                self.group.set(self.index);
                ctx.mark_dirty_all();
                true
            }
            _ => false,
        }
    }

    fn focusable(&self) -> bool {
        true
    }
    fn reset_interaction(&mut self) {
        // 清悬停并把底色/左缘补间瞬时落定到当前选中态（隐藏期不回放 hover 淡出）。
        self.hover = false;
        let on = if self.selected() { 1.0 } else { 0.0 };
        self.bg_amt.set(Transition::new(on));
        self.sel
            .set(Transition::new(if self.selected() && !self.pill {
                1.0
            } else {
                0.0
            }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::image::{Fit, Image};
    use crate::render::SkiaCanvas;
    use crate::signal::signal;
    use tiny_skia::Pixmap;

    #[test]
    fn row_with_icon_paints_icon_area() {
        let group = signal(0);
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
        let p = pm
            .pixel((PAD_X + side / 2) as u32, (ROW_H / 2) as u32)
            .unwrap();
        assert!(
            p.red() > 180 && p.green() < 90,
            "行图标应绘制红色，实得 ({},{},{})",
            p.red(),
            p.green(),
            p.blue()
        );
    }

    #[test]
    fn row_visual_state_tracks_selection() {
        let group = signal(1);
        let selected = ListRow::new("A".into(), group, 1);
        assert_eq!(selected.visual_state(), VisualState::Selected);
        let other = ListRow::new("B".into(), group, 2);
        assert_eq!(other.visual_state(), VisualState::Normal);
    }
}
