//! 数字步进 Stepper：[−] 值 [+]，绑定 `Rc<Cell<f64>>`，带范围/步长钳制。
//!
//! 自绘单控件：左右各一个按钮区，点击 ∓ 步长（钳制到 [min,max]）；中部显示当前值。
//! 键盘 上/右=增、下/左=减。

use std::cell::Cell;
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;

/// 左右按钮区宽度。
const BTN_W: i32 = 30;

pub struct Stepper {
    value: Rc<Cell<f64>>,
    min: f64,
    max: f64,
    step: f64,
    decimals: usize,
    /// 悬停区：-1 无 / 0 减 / 1 加。
    hover: i8,
}

impl Stepper {
    pub fn new(value: Rc<Cell<f64>>, min: f64, max: f64, step: f64) -> Self {
        // 退化输入归一：步长取正（0 退化为 1），min/max 顺序纠正（避免 clamp panic）。
        let step = if step.abs() < 1e-12 { 1.0 } else { step.abs() };
        let (min, max) = if min <= max { (min, max) } else { (max, min) };
        // 由步长推断小数位（1→0，0.1→1，0.05→2…）。
        let mut decimals = 0;
        let mut s = step;
        while decimals < 6 && (s - s.round()).abs() > 1e-9 {
            s *= 10.0;
            decimals += 1;
        }
        Self { value, min, max, step, decimals, hover: -1 }
    }

    fn display(&self) -> String {
        format!("{:.*}", self.decimals, self.value.get())
    }
    /// 命中区：屏幕 x（与 ctx.bounds() 同为绝对坐标）→ -1 中部 / 0 左减 / 1 右加。
    fn zone(&self, ctx: &EventCtx, screen_x: i32) -> i8 {
        let b = ctx.bounds();
        if screen_x < b.x + BTN_W {
            0
        } else if screen_x >= b.right() - BTN_W {
            1
        } else {
            -1
        }
    }
    /// 调整 `steps` 个步长并钳制写回。
    fn adjust(&self, ctx: &mut EventCtx, steps: f64) {
        let v = (self.value.get() + steps * self.step).clamp(self.min, self.max);
        self.value.set(v);
        ctx.mark_dirty();
    }
}

impl Widget for Stepper {
    fn measure(&self, _avail: Size, style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(120, (style.font_size as i32) + 16)
    }

    fn paint(&self, bounds: Rect, _content: Rect, focused: bool, enabled: bool, canvas: &mut dyn Canvas, style: &Style) {
        let th = crate::theme::current();
        let (pal, st) = (&th.palette, &th.stepper);
        let (x, y, w, h) = (bounds.x as f32, bounds.y as f32, bounds.w as f32, bounds.h as f32);
        let corner = th.metrics.corner_md;
        // 禁用：背景弱化、值与 +/- 全置灰。
        let bg = if enabled { st.bg(pal) } else { pal.surface_alt };
        let value_color = if enabled { st.text(pal) } else { pal.text_disabled };
        canvas.fill_round_rect(x, y, w, h, corner, &Paint::fill(bg));
        let border = if focused { pal.accent } else { st.border(pal) };
        canvas.stroke_round_rect(x, y, w, h, corner, 1.5, &Paint::fill(border));

        // 按钮区悬停底色（左右对称内缩 1px 避让边框）。
        if self.hover == 0 {
            canvas.fill_round_rect(x + 1.0, y + 1.0, BTN_W as f32 - 2.0, h - 2.0, corner, &Paint::fill(st.button_hover(pal)));
        } else if self.hover == 1 {
            canvas.fill_round_rect(x + w - BTN_W as f32 + 1.0, y + 1.0, BTN_W as f32 - 2.0, h - 2.0, corner, &Paint::fill(st.button_hover(pal)));
        }

        let family = style.font_family.as_deref();
        let fsize = style.font_size;
        // 左右分隔线。
        let div = Paint::fill(st.border(pal));
        canvas.draw_line((bounds.x + BTN_W) as f32, y + 4.0, (bounds.x + BTN_W) as f32, y + h - 4.0, 1.0, &div);
        canvas.draw_line((bounds.right() - BTN_W) as f32, y + 4.0, (bounds.right() - BTN_W) as f32, y + h - 4.0, 1.0, &div);

        // − / + 字形（按钮色，禁用端变灰）。
        let at_min = self.value.get() <= self.min;
        let at_max = self.value.get() >= self.max;
        let minus_c = if !enabled || at_min { pal.text_disabled } else { st.button(pal) };
        let plus_c = if !enabled || at_max { pal.text_disabled } else { st.button(pal) };
        let minus_r = Rect::new(bounds.x, bounds.y, BTN_W, bounds.h);
        let plus_r = Rect::new(bounds.right() - BTN_W, bounds.y, BTN_W, bounds.h);
        canvas.draw_text("\u{2212}", minus_r, minus_c, Align::Center, family, fsize);
        canvas.draw_text("+", plus_r, plus_c, Align::Center, family, fsize);

        // 中部值（窄控件下宽度兜底为非负）。
        let mid = Rect::new(bounds.x + BTN_W, bounds.y, (bounds.w - 2 * BTN_W).max(0), bounds.h);
        canvas.draw_text(&self.display(), mid, value_color, Align::Center, family, fsize);
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter | PointerKind::Move => {
                    let z = self.zone(ctx, p.pos.x);
                    if self.hover != z {
                        self.hover = z;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.hover != -1 {
                        self.hover = -1;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Down => {
                    ctx.request_focus();
                    match self.zone(ctx, p.pos.x) {
                        0 => self.adjust(ctx, -1.0),
                        1 => self.adjust(ctx, 1.0),
                        _ => {}
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed => match k.key {
                Key::Down | Key::Left => {
                    self.adjust(ctx, -1.0);
                    true
                }
                Key::Up | Key::Right => {
                    self.adjust(ctx, 1.0);
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
