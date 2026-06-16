//! Phase 4 基础输入控件：CheckBox / Switch / RadioButton / Slider / TextInput。
//!
//! 控件通过 `Rc<Cell<T>>` / `Rc<RefCell<String>>` 与外部状态双向绑定：控件改值
//! 即写入共享单元，外部随时读取，无需回调闭包。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Color, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;

const ACCENT: Color = Color { r: 0x4C, g: 0x8B, b: 0xF5, a: 0xFF };
const TRACK_OFF: Color = Color { r: 0xCF, g: 0xD4, b: 0xDC, a: 0xFF };
const BOX_SIZE: i32 = 18;
const GAP: i32 = 8;

// ---------------- CheckBox ----------------

pub struct CheckBox {
    label: String,
    state: Rc<Cell<bool>>,
}

impl CheckBox {
    pub fn new(label: String, state: Rc<Cell<bool>>) -> Self {
        Self { label, state }
    }
    fn toggle(&self, ctx: &mut EventCtx) {
        self.state.set(!self.state.get());
        ctx.mark_dirty();
    }
}

impl Widget for CheckBox {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let t = text.measure(&self.label, style.font_family.as_deref(), style.font_size, None);
        Size::new(BOX_SIZE + GAP + t.w, BOX_SIZE.max(t.h))
    }
    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        let cy = bounds.y + (bounds.h - BOX_SIZE) / 2;
        let (bx, by) = (bounds.x as f32, cy as f32);
        let on = self.state.get();
        if on {
            canvas.fill_round_rect(bx, by, BOX_SIZE as f32, BOX_SIZE as f32, 4.0, &Paint::fill(ACCENT));
            // 勾：两段线
            let p = Paint::fill(Color::WHITE);
            canvas.draw_line(bx + 4.0, by + 9.0, bx + 8.0, by + 13.0, 2.0, &p);
            canvas.draw_line(bx + 8.0, by + 13.0, bx + 14.0, by + 5.0, 2.0, &p);
        } else {
            canvas.fill_round_rect(bx, by, BOX_SIZE as f32, BOX_SIZE as f32, 4.0, &Paint::fill(Color::WHITE));
            canvas.stroke_round_rect(bx, by, BOX_SIZE as f32, BOX_SIZE as f32, 4.0, 1.5, &Paint::fill(TRACK_OFF));
        }
        let text_rect = Rect::new(bounds.x + BOX_SIZE + GAP, bounds.y, bounds.w - BOX_SIZE - GAP, bounds.h);
        canvas.draw_text(&self.label, text_rect, style.fg, Align::Start, style.font_family.as_deref(), style.font_size);
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) if p.kind == PointerKind::Up => {
                if ctx.bounds().contains(p.pos) {
                    self.toggle(ctx);
                }
                true
            }
            Event::Pointer(p) if p.kind == PointerKind::Down => {
                ctx.request_focus();
                true
            }
            Event::Key(k) if k.pressed && (k.key == Key::Space || k.key == Key::Enter) => {
                self.toggle(ctx);
                true
            }
            _ => false,
        }
    }
    fn focusable(&self) -> bool {
        true
    }
}

// ---------------- Switch ----------------

pub struct Switch {
    state: Rc<Cell<bool>>,
}

impl Switch {
    pub fn new(state: Rc<Cell<bool>>) -> Self {
        Self { state }
    }
    fn toggle(&self, ctx: &mut EventCtx) {
        self.state.set(!self.state.get());
        ctx.mark_dirty();
    }
}

impl Widget for Switch {
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(44, 24)
    }
    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, canvas: &mut dyn Canvas, _style: &Style) {
        let h = 24.min(bounds.h);
        let w = 44.min(bounds.w);
        let x = bounds.x as f32;
        let y = (bounds.y + (bounds.h - h) / 2) as f32;
        let on = self.state.get();
        let track = if on { ACCENT } else { TRACK_OFF };
        canvas.fill_round_rect(x, y, w as f32, h as f32, h as f32 / 2.0, &Paint::fill(track));
        let r = (h - 6) as f32 / 2.0;
        let knob_cx = if on { x + w as f32 - 3.0 - r } else { x + 3.0 + r };
        canvas.fill_circle(knob_cx, y + h as f32 / 2.0, r, &Paint::fill(Color::WHITE));
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) if p.kind == PointerKind::Up => {
                if ctx.bounds().contains(p.pos) {
                    self.toggle(ctx);
                }
                true
            }
            Event::Pointer(p) if p.kind == PointerKind::Down => {
                ctx.request_focus();
                true
            }
            Event::Key(k) if k.pressed && (k.key == Key::Space || k.key == Key::Enter) => {
                self.toggle(ctx);
                true
            }
            _ => false,
        }
    }
    fn focusable(&self) -> bool {
        true
    }
}

// ---------------- RadioButton ----------------

pub struct RadioButton {
    label: String,
    group: Rc<Cell<usize>>,
    index: usize,
}

impl RadioButton {
    pub fn new(label: String, group: Rc<Cell<usize>>, index: usize) -> Self {
        Self { label, group, index }
    }
    fn selected(&self) -> bool {
        self.group.get() == self.index
    }
    fn select(&self, ctx: &mut EventCtx) {
        self.group.set(self.index);
        ctx.mark_dirty();
    }
}

impl Widget for RadioButton {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let t = text.measure(&self.label, style.font_family.as_deref(), style.font_size, None);
        Size::new(BOX_SIZE + GAP + t.w, BOX_SIZE.max(t.h))
    }
    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        let cy = bounds.y + bounds.h / 2;
        let cx = bounds.x + BOX_SIZE / 2;
        let outer = BOX_SIZE as f32 / 2.0;
        if self.selected() {
            canvas.fill_circle(cx as f32, cy as f32, outer, &Paint::fill(ACCENT));
            canvas.fill_circle(cx as f32, cy as f32, outer - 5.0, &Paint::fill(Color::WHITE));
            canvas.fill_circle(cx as f32, cy as f32, outer - 8.0, &Paint::fill(ACCENT));
        } else {
            canvas.fill_circle(cx as f32, cy as f32, outer, &Paint::fill(TRACK_OFF));
            canvas.fill_circle(cx as f32, cy as f32, outer - 1.5, &Paint::fill(Color::WHITE));
        }
        let text_rect = Rect::new(bounds.x + BOX_SIZE + GAP, bounds.y, bounds.w - BOX_SIZE - GAP, bounds.h);
        canvas.draw_text(&self.label, text_rect, style.fg, Align::Start, style.font_family.as_deref(), style.font_size);
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) if p.kind == PointerKind::Up => {
                if ctx.bounds().contains(p.pos) {
                    self.select(ctx);
                }
                true
            }
            Event::Pointer(p) if p.kind == PointerKind::Down => {
                ctx.request_focus();
                true
            }
            Event::Key(k) if k.pressed && (k.key == Key::Space || k.key == Key::Enter) => {
                self.select(ctx);
                true
            }
            _ => false,
        }
    }
    fn focusable(&self) -> bool {
        true
    }
}

// ---------------- Slider ----------------

pub struct Slider {
    value: Rc<Cell<f32>>, // 0.0..=1.0
    dragging: bool,
}

impl Slider {
    pub fn new(value: Rc<Cell<f32>>) -> Self {
        Self { value, dragging: false }
    }
    fn set_from_pos(&self, ctx: &mut EventCtx, x: i32) {
        let b = ctx.bounds();
        let r = KNOB_R;
        let usable = (b.w - 2 * r).max(1);
        let v = ((x - b.x - r) as f32 / usable as f32).clamp(0.0, 1.0);
        self.value.set(v);
        ctx.mark_dirty();
    }
}

const KNOB_R: i32 = 9;

impl Widget for Slider {
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(120, 2 * KNOB_R)
    }
    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, canvas: &mut dyn Canvas, _style: &Style) {
        let v = self.value.get().clamp(0.0, 1.0);
        let cy = bounds.y as f32 + bounds.h as f32 / 2.0;
        let r = KNOB_R as f32;
        let x0 = bounds.x as f32 + r;
        let x1 = bounds.x as f32 + bounds.w as f32 - r;
        let knob_x = x0 + (x1 - x0) * v;
        // 轨道
        canvas.fill_round_rect(x0, cy - 2.0, (x1 - x0).max(0.0), 4.0, 2.0, &Paint::fill(TRACK_OFF));
        // 已填充
        canvas.fill_round_rect(x0, cy - 2.0, (knob_x - x0).max(0.0), 4.0, 2.0, &Paint::fill(ACCENT));
        // 钮
        canvas.fill_circle(knob_x, cy, r, &Paint::fill(Color::WHITE));
        canvas.fill_circle(knob_x, cy, r - 2.0, &Paint::fill(ACCENT));
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Down => {
                    ctx.request_focus();
                    ctx.capture();
                    self.dragging = true;
                    self.set_from_pos(ctx, p.pos.x);
                    true
                }
                PointerKind::Move => {
                    // 仅拖动期间响应，避免悬停即改值。
                    if self.dragging {
                        self.set_from_pos(ctx, p.pos.x);
                        true
                    } else {
                        false
                    }
                }
                PointerKind::Up => {
                    self.dragging = false;
                    ctx.release_capture();
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed => {
                let step = 0.05;
                let v = self.value.get();
                match k.key {
                    Key::Left => {
                        self.value.set((v - step).max(0.0));
                        ctx.mark_dirty();
                        true
                    }
                    Key::Right => {
                        self.value.set((v + step).min(1.0));
                        ctx.mark_dirty();
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        }
    }
    fn focusable(&self) -> bool {
        true
    }
}

// ---------------- TextInput ----------------

pub struct TextInput {
    text: Rc<RefCell<String>>,
    placeholder: String,
    cursor: usize, // 字符索引
}

impl TextInput {
    pub fn new(text: Rc<RefCell<String>>, placeholder: String) -> Self {
        let cursor = text.borrow().chars().count();
        Self { text, placeholder, cursor }
    }

    fn clamp_cursor(&mut self) {
        let n = self.text.borrow().chars().count();
        if self.cursor > n {
            self.cursor = n;
        }
    }
    fn insert_char(&mut self, ctx: &mut EventCtx, c: char) {
        if c.is_control() {
            return;
        }
        self.clamp_cursor();
        let mut s = self.text.borrow_mut();
        let byte = char_to_byte(&s, self.cursor);
        s.insert(byte, c);
        self.cursor += 1;
        drop(s);
        ctx.mark_dirty();
    }
    fn backspace(&mut self, ctx: &mut EventCtx) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let mut s = self.text.borrow_mut();
        let start = char_to_byte(&s, self.cursor - 1);
        let end = char_to_byte(&s, self.cursor);
        s.replace_range(start..end, "");
        self.cursor -= 1;
        drop(s);
        ctx.mark_dirty();
    }
    fn move_cursor(&mut self, ctx: &mut EventCtx, delta: isize) {
        let len = self.text.borrow().chars().count();
        let nc = (self.cursor as isize + delta).clamp(0, len as isize) as usize;
        if nc != self.cursor {
            self.cursor = nc;
            ctx.mark_dirty();
        }
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}

impl Widget for TextInput {
    fn measure(&self, _avail: Size, style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(160, (style.font_size as i32) + 16)
    }
    fn paint(&self, bounds: Rect, _content: Rect, focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        let (x, y, w, h) = (bounds.x as f32, bounds.y as f32, bounds.w as f32, bounds.h as f32);
        canvas.fill_round_rect(x, y, w, h, 6.0, &Paint::fill(Color::WHITE));
        // 边框用中性色；聚焦时由核心层的焦点环提示。
        canvas.stroke_round_rect(x, y, w, h, 6.0, 1.5, &Paint::fill(TRACK_OFF));

        let text = self.text.borrow();
        let pad = 10;
        let inner = Rect::new(bounds.x + pad, bounds.y, bounds.w - 2 * pad, bounds.h);
        if text.is_empty() {
            canvas.draw_text(&self.placeholder, inner, Color::hex(0xAAB0B8), Align::Start, style.font_family.as_deref(), style.font_size);
        } else {
            canvas.draw_text(&text, inner, style.fg, Align::Start, style.font_family.as_deref(), style.font_size);
        }
        // 光标：仅在聚焦时绘制，避免多个文本框都显示光标。
        if focused {
            let cursor = self.cursor.min(text.chars().count());
            let prefix: String = text.chars().take(cursor).collect();
            let cw = canvas.measure_text(&prefix, style.font_family.as_deref(), style.font_size).w;
            let cx = (bounds.x + pad + cw) as f32;
            let cyy = bounds.y as f32 + 6.0;
            canvas.draw_line(cx, cyy, cx, bounds.y as f32 + bounds.h as f32 - 6.0, 1.0, &Paint::fill(Color::hex(0x444444)));
        }
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) if p.kind == PointerKind::Down => {
                ctx.request_focus();
                ctx.mark_dirty();
                true
            }
            Event::Key(k) if k.pressed => match k.key {
                Key::Char(c) => {
                    self.insert_char(ctx, c);
                    true
                }
                // 空格经 WM_CHAR 以 Char(' ') 到达，这里不处理 Key::Space，避免双插入。
                Key::Backspace => {
                    self.backspace(ctx);
                    true
                }
                Key::Left => {
                    self.move_cursor(ctx, -1);
                    true
                }
                Key::Right => {
                    self.move_cursor(ctx, 1);
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
