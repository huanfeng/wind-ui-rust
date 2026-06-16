//! Phase 4 基础输入控件：CheckBox / Switch / RadioButton / Slider / TextInput。
//!
//! 控件通过 `Rc<Cell<T>>` / `Rc<RefCell<String>>` 与外部状态双向绑定：控件改值
//! 即写入共享单元，外部随时读取，无需回调闭包。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, MouseButton, PointerKind};
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

const TEXT_PAD: i32 = 10;
const SEL_COLOR: Color = Color { r: 0x4C, g: 0x8B, b: 0xF5, a: 0x55 };
/// 单行文本绘制用的"足够宽"矩形宽度，依赖 clip_rect 裁剪保证不溢出。
const NO_WRAP_W: i32 = 100_000;
/// 密码掩码字符（U+2022 BULLET）。
const PASSWORD_MASK: char = '\u{2022}';

/// TextInput 行为配置。由 Builder 的 `.password()/.multiline()/.wrap()` 设置。
#[derive(Clone, Copy)]
pub struct TextConfig {
    /// 多行模式（P4 实现编辑/换行；当前仅占位存储）。
    pub multiline: bool,
    /// 密码模式：显示为掩码圆点、禁止复制/剪切、强制单行。
    pub password: bool,
    /// 多行软换行（仅 multiline 生效）；false 时仅显式 \n 换行。
    pub wrap: bool,
}

impl Default for TextConfig {
    /// wrap 默认开启，使多行默认软换行；类型自带正确默认避免直接构造踩坑。
    fn default() -> Self {
        Self { multiline: false, password: false, wrap: true }
    }
}

pub struct TextInput {
    text: Rc<RefCell<String>>,
    placeholder: String,
    config: TextConfig,
    cursor: usize,            // 字符索引
    anchor: Option<usize>,    // 选区锚点（Some 且 != cursor 时有选区）
    scroll_x: Cell<i32>,      // 水平滚动偏移（逻辑 px），paint 时按光标更新
    char_x: RefCell<Vec<i32>>, // paint 缓存：char_x[i] = display[..i] 宽度（逻辑 px）
    dragging: bool,
}

impl TextInput {
    pub fn new(text: Rc<RefCell<String>>, placeholder: String) -> Self {
        let cursor = text.borrow().chars().count();
        Self {
            text,
            placeholder,
            config: TextConfig::default(),
            cursor,
            anchor: None,
            scroll_x: Cell::new(0),
            char_x: RefCell::new(vec![0]),
            dragging: false,
        }
    }

    /// 可变访问配置（供 Builder 配置）。
    pub fn config_mut(&mut self) -> &mut TextConfig {
        &mut self.config
    }

    fn char_count(&self) -> usize {
        self.text.borrow().chars().count()
    }

    /// 实际用于显示与测量的字符串：密码模式下逐字符替换为掩码圆点，
    /// 字符数与真实文本一一对应，故光标/选区索引可直接复用。
    fn display_string(&self) -> String {
        let t = self.text.borrow();
        if self.config.password {
            t.chars().map(|_| PASSWORD_MASK).collect()
        } else {
            t.clone()
        }
    }
    fn clamp_cursor(&mut self) {
        let n = self.char_count();
        if self.cursor > n {
            self.cursor = n;
        }
    }
    /// 规范化选区为 [start, end)；无选区返回 None。
    /// cursor/anchor 在此夹紧到当前字符数——外部经 Rc<RefCell<String>> 改写文本后
    /// 仍保证选区范围合法，下游 delete/paint 无需各自再夹。
    fn selection(&self) -> Option<(usize, usize)> {
        let n = self.char_count();
        let a = self.anchor?.min(n);
        let c = self.cursor.min(n);
        if a == c {
            None
        } else {
            Some((a.min(c), a.max(c)))
        }
    }
    /// 删除选区文本，返回是否删除了。
    fn delete_selection(&mut self, ctx: &mut EventCtx) -> bool {
        if let Some((s, e)) = self.selection() {
            let mut t = self.text.borrow_mut();
            let bs = char_to_byte(&t, s);
            let be = char_to_byte(&t, e);
            t.replace_range(bs..be, "");
            drop(t);
            self.cursor = s;
            self.anchor = None;
            ctx.mark_dirty();
            true
        } else {
            false
        }
    }
    fn type_char(&mut self, ctx: &mut EventCtx, c: char) {
        if c.is_control() {
            return;
        }
        self.delete_selection(ctx);
        self.clamp_cursor();
        let mut s = self.text.borrow_mut();
        let byte = char_to_byte(&s, self.cursor);
        s.insert(byte, c);
        self.cursor += 1;
        drop(s);
        self.anchor = None;
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
    fn delete_forward(&mut self, ctx: &mut EventCtx) {
        self.clamp_cursor();
        let len = self.char_count();
        if self.cursor >= len {
            return;
        }
        let mut s = self.text.borrow_mut();
        let start = char_to_byte(&s, self.cursor);
        let end = char_to_byte(&s, self.cursor + 1);
        s.replace_range(start..end, "");
        drop(s);
        ctx.mark_dirty();
    }
    /// 移动光标到 target；shift=true 时扩展选区，否则清选区。
    fn move_to(&mut self, ctx: &mut EventCtx, target: usize, shift: bool) {
        if shift {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = target.min(self.char_count());
        ctx.mark_dirty();
    }
    /// 屏幕 x（逻辑坐标）→ 字符索引（用 paint 缓存的 char_x 定位最近边界）。
    /// 前置条件：依赖最近一帧 paint 重建的 char_x 缓存；首帧未绘制前会落到索引 0。
    fn index_at(&self, ctx: &EventCtx, screen_x: i32) -> usize {
        let b = ctx.bounds();
        let local = screen_x - (b.x + TEXT_PAD) + self.scroll_x.get();
        let cx = self.char_x.borrow();
        let mut best = 0;
        let mut best_d = i32::MAX;
        for (i, &v) in cx.iter().enumerate() {
            let d = (v - local).abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        best.min(self.char_count())
    }
    /// 当前选区文本（无选区返回 None）。
    fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        let t = self.text.borrow();
        let bs = char_to_byte(&t, s);
        let be = char_to_byte(&t, e);
        Some(t[bs..be].to_string())
    }
    /// 在光标处粘贴（先删选区）；单行控件过滤换行/控制字符。
    fn paste(&mut self, ctx: &mut EventCtx, s: &str) {
        self.delete_selection(ctx);
        self.clamp_cursor();
        let clean: String = s.chars().filter(|c| !c.is_control()).collect();
        if clean.is_empty() {
            return;
        }
        let mut t = self.text.borrow_mut();
        let byte = char_to_byte(&t, self.cursor);
        t.insert_str(byte, &clean);
        drop(t);
        self.cursor += clean.chars().count();
        self.anchor = None;
        ctx.mark_dirty();
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
        canvas.stroke_round_rect(x, y, w, h, 6.0, 1.5, &Paint::fill(TRACK_OFF));

        // 显示串：密码模式为掩码圆点；测量/绘制/光标定位都基于它（字符数与真实文本一致）。
        let disp = self.display_string();
        let is_empty = self.text.borrow().is_empty();
        let inner = Rect::new(bounds.x + TEXT_PAD, bounds.y, bounds.w - 2 * TEXT_PAD, bounds.h);
        let family = style.font_family.as_deref();
        let fsize = style.font_size;
        let cursor = self.cursor.min(disp.chars().count());

        // 重建每字符边界 x 缓存（逻辑 px）。
        {
            let mut cx = self.char_x.borrow_mut();
            cx.clear();
            cx.push(0);
            let mut acc = String::new();
            for ch in disp.chars() {
                acc.push(ch);
                cx.push(canvas.measure_text(&acc, family, fsize).w);
            }
        }
        let cx = self.char_x.borrow();

        // 更新水平滚动使光标可见。
        let cursor_x = cx.get(cursor).copied().unwrap_or(0);
        let mut sx = self.scroll_x.get();
        if cursor_x - sx > inner.w {
            sx = cursor_x - inner.w;
        }
        if cursor_x - sx < 0 {
            sx = cursor_x;
        }
        sx = sx.max(0);
        self.scroll_x.set(sx);

        // 裁剪到内框，绘制选区 / 文字 / 光标。
        canvas.save();
        canvas.clip_rect(inner);
        let base_x = inner.x - sx;

        if let Some((s, e)) = self.selection() {
            let x1 = base_x + cx.get(s).copied().unwrap_or(0);
            let x2 = base_x + cx.get(e).copied().unwrap_or(0);
            canvas.fill_rect(
                x1 as f32,
                (inner.y + 4) as f32,
                (x2 - x1) as f32,
                (inner.h - 8) as f32,
                &Paint::fill(SEL_COLOR),
            );
        }

        if is_empty {
            canvas.draw_text(&self.placeholder, inner, Color::hex(0xAAB0B8), Align::Start, family, fsize);
        } else {
            // 从 base_x 起绘制整行（足够宽不换行），由 clip 裁到内框。
            let tr = Rect::new(base_x, inner.y, NO_WRAP_W, inner.h);
            canvas.draw_text(&disp, tr, style.fg, Align::Start, family, fsize);
        }

        if focused {
            let cxx = base_x + cursor_x;
            canvas.draw_line(
                cxx as f32,
                bounds.y as f32 + 6.0,
                cxx as f32,
                bounds.y as f32 + bounds.h as f32 - 6.0,
                1.0,
                &Paint::fill(Color::hex(0x444444)),
            );
        }
        canvas.restore();
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Down => {
                    ctx.request_focus();
                    // 右键不启动拖选：仅聚焦，并在点击落在选区外时移动光标
                    // （为 P5 右键菜单预留——菜单针对当前选区/光标操作）。
                    if p.button == MouseButton::Right {
                        let idx = self.index_at(ctx, p.pos.x);
                        let in_sel = self.selection().is_some_and(|(s, e)| idx >= s && idx < e);
                        if !in_sel {
                            self.cursor = idx;
                            self.anchor = None;
                        }
                        ctx.mark_dirty();
                        return true;
                    }
                    let idx = self.index_at(ctx, p.pos.x);
                    self.cursor = idx;
                    self.anchor = Some(idx);
                    self.dragging = true;
                    ctx.capture();
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Move if self.dragging => {
                    self.cursor = self.index_at(ctx, p.pos.x);
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Up => {
                    self.dragging = false;
                    ctx.release_capture();
                    if self.anchor == Some(self.cursor) {
                        self.anchor = None; // 单击未拖动：无选区
                    }
                    ctx.mark_dirty();
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed => {
                let len = self.char_count();
                match k.key {
                    Key::Char(c) if !k.ctrl => {
                        self.type_char(ctx, c);
                        true
                    }
                    Key::Backspace => {
                        if !self.delete_selection(ctx) {
                            self.backspace(ctx);
                        }
                        true
                    }
                    Key::Delete => {
                        if !self.delete_selection(ctx) {
                            self.delete_forward(ctx);
                        }
                        true
                    }
                    Key::Left => {
                        if !k.shift {
                            if let Some((s, _)) = self.selection() {
                                self.cursor = s;
                                self.anchor = None;
                                ctx.mark_dirty();
                                return true;
                            }
                        }
                        self.move_to(ctx, self.cursor.saturating_sub(1), k.shift);
                        true
                    }
                    Key::Right => {
                        if !k.shift {
                            if let Some((_, e)) = self.selection() {
                                self.cursor = e;
                                self.anchor = None;
                                ctx.mark_dirty();
                                return true;
                            }
                        }
                        self.move_to(ctx, (self.cursor + 1).min(len), k.shift);
                        true
                    }
                    Key::Home => {
                        self.move_to(ctx, 0, k.shift);
                        true
                    }
                    Key::End => {
                        self.move_to(ctx, len, k.shift);
                        true
                    }
                    // Ctrl+A 全选（VK_A=0x41）
                    Key::Other(0x41) if k.ctrl => {
                        self.anchor = Some(0);
                        self.cursor = len;
                        ctx.mark_dirty();
                        true
                    }
                    // Ctrl+C 复制（VK_C=0x43）。密码模式禁止复制明文。
                    Key::Other(0x43) if k.ctrl => {
                        if !self.config.password {
                            if let Some(sel) = self.selected_text() {
                                ctx.clipboard_set(&sel);
                            }
                        }
                        true
                    }
                    // Ctrl+X 剪切（VK_X=0x58）。密码模式禁止剪切（不外泄明文）。
                    Key::Other(0x58) if k.ctrl => {
                        if !self.config.password {
                            if let Some(sel) = self.selected_text() {
                                ctx.clipboard_set(&sel);
                                self.delete_selection(ctx);
                            }
                        }
                        true
                    }
                    // Ctrl+V 粘贴（VK_V=0x56）
                    Key::Other(0x56) if k.ctrl => {
                        if let Some(s) = ctx.clipboard_get() {
                            self.paste(ctx, &s);
                        }
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
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}
