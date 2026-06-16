//! Phase 4 基础输入控件：CheckBox / Switch / RadioButton / Slider / TextInput。
//!
//! 控件通过 `Rc<Cell<T>>` / `Rc<RefCell<String>>` 与外部状态双向绑定：控件改值
//! 即写入共享单元，外部随时读取，无需回调闭包。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, KeyEvent, MenuItem, MouseButton, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;

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
        let th = crate::theme::current();
        let (p, tg) = (&th.palette, &th.toggle);
        let cy = bounds.y + (bounds.h - BOX_SIZE) / 2;
        let (bx, by) = (bounds.x as f32, cy as f32);
        let on = self.state.get();
        if on {
            canvas.fill_round_rect(bx, by, BOX_SIZE as f32, BOX_SIZE as f32, 4.0, &Paint::fill(tg.accent(p)));
            // 勾：两段线
            let paint = Paint::fill(p.on_accent);
            canvas.draw_line(bx + 4.0, by + 9.0, bx + 8.0, by + 13.0, 2.0, &paint);
            canvas.draw_line(bx + 8.0, by + 13.0, bx + 14.0, by + 5.0, 2.0, &paint);
        } else {
            canvas.fill_round_rect(bx, by, BOX_SIZE as f32, BOX_SIZE as f32, 4.0, &Paint::fill(tg.knob(p)));
            canvas.stroke_round_rect(bx, by, BOX_SIZE as f32, BOX_SIZE as f32, 4.0, 1.5, &Paint::fill(tg.track(p)));
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
        let th = crate::theme::current();
        let (p, tg) = (&th.palette, &th.toggle);
        let track = if on { tg.accent(p) } else { tg.track(p) };
        canvas.fill_round_rect(x, y, w as f32, h as f32, h as f32 / 2.0, &Paint::fill(track));
        let r = (h - 6) as f32 / 2.0;
        let knob_cx = if on { x + w as f32 - 3.0 - r } else { x + 3.0 + r };
        canvas.fill_circle(knob_cx, y + h as f32 / 2.0, r, &Paint::fill(tg.knob(p)));
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
        let th = crate::theme::current();
        let (p, tg) = (&th.palette, &th.toggle);
        let cy = bounds.y + bounds.h / 2;
        let cx = bounds.x + BOX_SIZE / 2;
        let outer = BOX_SIZE as f32 / 2.0;
        if self.selected() {
            canvas.fill_circle(cx as f32, cy as f32, outer, &Paint::fill(tg.accent(p)));
            canvas.fill_circle(cx as f32, cy as f32, outer - 5.0, &Paint::fill(tg.knob(p)));
            canvas.fill_circle(cx as f32, cy as f32, outer - 8.0, &Paint::fill(tg.accent(p)));
        } else {
            canvas.fill_circle(cx as f32, cy as f32, outer, &Paint::fill(tg.track(p)));
            canvas.fill_circle(cx as f32, cy as f32, outer - 1.5, &Paint::fill(tg.knob(p)));
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
        let th = crate::theme::current();
        let (pal, tg) = (&th.palette, &th.toggle);
        canvas.fill_round_rect(x0, cy - 2.0, (x1 - x0).max(0.0), 4.0, 2.0, &Paint::fill(tg.track(pal)));
        // 已填充
        canvas.fill_round_rect(x0, cy - 2.0, (knob_x - x0).max(0.0), 4.0, 2.0, &Paint::fill(tg.accent(pal)));
        // 钮
        canvas.fill_circle(knob_x, cy, r, &Paint::fill(tg.knob(pal)));
        canvas.fill_circle(knob_x, cy, r - 2.0, &Paint::fill(tg.accent(pal)));
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
/// 单行文本绘制用的"足够宽"矩形宽度，依赖 clip_rect 裁剪保证不溢出。
const NO_WRAP_W: i32 = 100_000;
/// 选区跨行时行尾延伸宽度（标示换行/折行被选中）。
const SEL_EOL_EXTRA: i32 = 6;
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

/// 一个视觉行：全文字符区间 [start,end) + 行内每字符左边界 x（相对行首，逻辑 px）。
/// `x` 长度 = end-start+1，`x[0]=0`。`hard` 表示该行以真实 '\n'（或文末）结束，
/// 软换行为 false——用于光标换行归属与选区跨行延伸。
struct VisLine {
    start: usize,
    end: usize,
    x: Vec<i32>,
    hard: bool,
}

/// 视觉行布局缓存。paint 时按显示串 + 内框宽度重建，事件（点击/上下/Home/End）复用。
#[derive(Default)]
struct TextLayout {
    lines: Vec<VisLine>,
    line_h: i32,
}

pub struct TextInput {
    text: Rc<RefCell<String>>,
    placeholder: String,
    config: TextConfig,
    cursor: usize,            // 字符索引
    anchor: Option<usize>,    // 选区锚点（Some 且 != cursor 时有选区）
    scroll_x: Cell<i32>,      // 水平滚动偏移（逻辑 px），paint 时按光标更新
    scroll_y: Cell<i32>,      // 垂直滚动偏移（逻辑 px，多行用），paint 时按光标更新
    /// 上下移动时保持的目标列像素（粘性 goal column）；水平移动/编辑后清空。
    goal_x: Cell<Option<i32>>,
    layout: RefCell<TextLayout>, // paint 重建的视觉行缓存
    /// 最近一帧绘制的光标局部位置 (x, y_top, height)（节点局部逻辑坐标），供输入法定位。
    caret_local: Cell<Option<(i32, i32, i32)>>,
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
            scroll_y: Cell::new(0),
            goal_x: Cell::new(None),
            layout: RefCell::new(TextLayout::default()),
            caret_local: Cell::new(None),
            dragging: false,
        }
    }

    /// 可变访问配置（供 Builder 配置）。
    pub fn config_mut(&mut self) -> &mut TextConfig {
        &mut self.config
    }

    /// 运行期是否多行：密码模式恒为单行（与 Builder 链式顺序无关，杜绝换行进入密码底层文本）。
    fn is_multiline(&self) -> bool {
        self.config.multiline && !self.config.password
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
            self.goal_x.set(None);
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
        self.goal_x.set(None);
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
        self.goal_x.set(None);
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
        self.goal_x.set(None);
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
        self.goal_x.set(None);
        ctx.mark_dirty();
    }
    /// 选中 `idx` 处的词（同类连续段）。
    fn select_word(&mut self, idx: usize) {
        let chars: Vec<char> = self.text.borrow().chars().collect();
        let (s, e) = word_run(&chars, idx);
        self.anchor = Some(s.min(chars.len()));
        self.cursor = e.min(chars.len());
    }
    /// 全选。
    fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.char_count();
    }
    /// 构建右键上下文菜单项。动作经合成 Ctrl+X/C/V/A 回送到本控件，故无需感知"菜单"。
    fn context_menu_items(&self) -> Vec<MenuItem> {
        let has_sel = self.selection().is_some();
        let has_text = self.char_count() > 0;
        let pw = self.config.password;
        let ctrl = |vk: u32| KeyEvent { key: Key::Other(vk), pressed: true, shift: false, ctrl: true };
        vec![
            MenuItem::key("剪切", ctrl(0x58), has_sel && !pw), // VK_X
            MenuItem::key("复制", ctrl(0x43), has_sel && !pw), // VK_C
            MenuItem::key("粘贴", ctrl(0x56), true),           // VK_V
            MenuItem::key("全选", ctrl(0x41), has_text),       // VK_A
        ]
    }
    /// 选中 `idx` 所在逻辑行（两 '\n' 之间）。单行文本无 '\n' 即全选。
    fn select_para(&mut self, idx: usize) {
        let chars: Vec<char> = self.text.borrow().chars().collect();
        let n = chars.len();
        let i = idx.min(n);
        let mut s = i;
        while s > 0 && chars[s - 1] != '\n' {
            s -= 1;
        }
        let mut e = i;
        while e < n && chars[e] != '\n' {
            e += 1;
        }
        self.anchor = Some(s);
        self.cursor = e;
    }
    /// 按显示串与内框宽度重建视觉行布局缓存。paint 调用；点击/上下移动复用其结果。
    fn rebuild_layout(&self, canvas: &mut dyn Canvas, disp: &str, family: Option<&str>, fsize: f32, inner_w: i32) {
        let chars: Vec<char> = disp.chars().collect();
        let n = chars.len();
        let multiline = self.is_multiline();
        let wrap = self.config.wrap && multiline;

        let mut lay = self.layout.borrow_mut();
        lay.lines.clear();
        lay.line_h = canvas.measure_text("Ay", family, fsize).h.max(fsize as i32);

        let mut p = 0usize;
        loop {
            // 段落 [p,q)：多行按 '\n' 切分；单行整体一段。
            let q = if multiline {
                (p..n).find(|&i| chars[i] == '\n').unwrap_or(n)
            } else {
                n
            };
            // 段内前缀宽度（相对段首，累计测量保证 kerning 准确）。
            // TODO(perf): 每段 O(L²) 重测且每帧重建；超长段落可按 (文本版本,宽度,字体) 缓存复用。
            let mut prefix = Vec::with_capacity(q - p + 1);
            prefix.push(0);
            let mut acc = String::new();
            for &ch in &chars[p..q] {
                acc.push(ch);
                prefix.push(canvas.measure_text(&acc, family, fsize).w);
            }
            for (ls, le, hard) in wrap_paragraph(&chars, p, q, &prefix, inner_w, wrap) {
                let base = prefix[ls - p];
                let x: Vec<i32> = (ls..=le).map(|k| prefix[k - p] - base).collect();
                lay.lines.push(VisLine { start: ls, end: le, x, hard });
            }
            if q < n {
                p = q + 1; // 跳过 '\n'；若 p==n（文末换行）下轮产出空尾行后结束
            } else {
                break;
            }
        }
    }

    /// 光标字符索引 → 所在视觉行下标。软换行边界归属下一行（caret 显示在折行后行首）。
    fn cursor_line(&self, lay: &TextLayout, c: usize) -> usize {
        let lines = &lay.lines;
        for (i, ln) in lines.iter().enumerate() {
            if c < ln.end {
                return i;
            }
            if c == ln.end && (ln.hard || i + 1 == lines.len()) {
                return i;
            }
        }
        lines.len().saturating_sub(1)
    }

    /// 光标的 (视觉行下标, 行内 x 逻辑 px)。
    fn caret_line_x(&self, lay: &TextLayout, c: usize) -> (usize, i32) {
        if lay.lines.is_empty() {
            return (0, 0);
        }
        let li = self.cursor_line(lay, c);
        let ln = &lay.lines[li];
        let col = c.saturating_sub(ln.start).min(ln.x.len().saturating_sub(1));
        (li, ln.x.get(col).copied().unwrap_or(0))
    }

    /// 屏幕坐标（逻辑）→ 字符索引：先按 y 定位视觉行，再按 x 取行内最近边界。
    /// 依赖最近一帧 paint 重建的布局；首帧前布局为空时落到索引 0。
    fn pos_to_index(&self, ctx: &EventCtx, screen_x: i32, screen_y: i32) -> usize {
        let lay = self.layout.borrow();
        if lay.lines.is_empty() {
            return 0;
        }
        let b = ctx.bounds();
        let local_x = screen_x - (b.x + TEXT_PAD) + self.scroll_x.get();
        // 垂直按多行内边距换算行号。单行只有一行、下方 clamp 恒为 0，故单行垂直
        // 居中（vpad=0）与此处用 TEXT_PAD 的不一致不影响命中；若将来单行支持多视觉
        // 行，需与 paint 的 first_line_y 同步。
        let local_y = screen_y - (b.y + TEXT_PAD) + self.scroll_y.get();
        let li = if lay.line_h > 0 {
            (local_y / lay.line_h).clamp(0, lay.lines.len() as i32 - 1) as usize
        } else {
            0
        };
        let ln = &lay.lines[li];
        let mut best = 0;
        let mut best_d = i32::MAX;
        for (j, &v) in ln.x.iter().enumerate() {
            let d = (v - local_x).abs();
            if d < best_d {
                best_d = d;
                best = j;
            }
        }
        ln.start + best
    }

    /// 上/下移动光标到相邻视觉行的目标列（粘性 goal_x）。返回是否移动。
    fn move_vertical(&mut self, ctx: &mut EventCtx, down: bool, shift: bool) {
        let lay = self.layout.borrow();
        if lay.lines.is_empty() {
            return;
        }
        let (li, cur_x) = self.caret_line_x(&lay, self.cursor.min(self.char_count()));
        let goal = self.goal_x.get().unwrap_or(cur_x);
        let target_li = if down {
            if li + 1 >= lay.lines.len() {
                drop(lay);
                self.goal_x.set(Some(goal));
                return;
            }
            li + 1
        } else {
            if li == 0 {
                drop(lay);
                self.goal_x.set(Some(goal));
                return;
            }
            li - 1
        };
        // 在目标行内取最接近 goal 的字符边界。
        let ln = &lay.lines[target_li];
        let mut best = 0;
        let mut best_d = i32::MAX;
        for (j, &v) in ln.x.iter().enumerate() {
            let d = (v - goal).abs();
            if d < best_d {
                best_d = d;
                best = j;
            }
        }
        let target = ln.start + best;
        drop(lay);
        if shift {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = target.min(self.char_count());
        self.goal_x.set(Some(goal)); // 保持粘性列
        ctx.mark_dirty();
    }

    /// 当前视觉行的 [start, end) 字符区间（Home/End 用）。
    fn cur_line_bounds(&self) -> (usize, usize) {
        let lay = self.layout.borrow();
        if lay.lines.is_empty() {
            return (0, self.char_count());
        }
        let li = self.cursor_line(&lay, self.cursor.min(self.char_count()));
        let ln = &lay.lines[li];
        (ln.start, ln.end)
    }

    /// 在光标处插入换行（多行模式）。
    fn insert_newline(&mut self, ctx: &mut EventCtx) {
        self.delete_selection(ctx);
        self.clamp_cursor();
        let mut s = self.text.borrow_mut();
        let byte = char_to_byte(&s, self.cursor);
        s.insert(byte, '\n');
        self.cursor += 1;
        drop(s);
        self.anchor = None;
        self.goal_x.set(None);
        ctx.mark_dirty();
    }
    /// 当前选区文本（无选区返回 None）。
    fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        let t = self.text.borrow();
        let bs = char_to_byte(&t, s);
        let be = char_to_byte(&t, e);
        Some(t[bs..be].to_string())
    }
    /// 在光标处粘贴（先删选区）。单行控件过滤所有控制字符；多行保留 '\n'
    /// （\r\n / \r 归一为 \n），仍过滤其他控制字符。
    fn paste(&mut self, ctx: &mut EventCtx, s: &str) {
        self.delete_selection(ctx);
        self.clamp_cursor();
        let clean: String = if self.is_multiline() {
            let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
            normalized.chars().filter(|c| *c == '\n' || !c.is_control()).collect()
        } else {
            s.chars().filter(|c| !c.is_control()).collect()
        };
        if clean.is_empty() {
            return;
        }
        let mut t = self.text.borrow_mut();
        let byte = char_to_byte(&t, self.cursor);
        t.insert_str(byte, &clean);
        drop(t);
        self.cursor += clean.chars().count();
        self.anchor = None;
        self.goal_x.set(None);
        ctx.mark_dirty();
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}

/// 把一个段落 [p,q)（不含换行符）按内框宽度切成视觉行，返回每行的
/// `(start, end, hard)` 字符区间。`prefix[k-p]` 是 chars[p..k] 的累计宽度。
/// `wrap=false` 时整段一行。优先在空格后断行（词换行），否则按字符断（含超宽单字符兜底）。
/// 末行 `hard=true`（段落以真实换行/文末结束）；软换行行 `hard=false`。
fn wrap_paragraph(
    chars: &[char],
    p: usize,
    q: usize,
    prefix: &[i32],
    inner_w: i32,
    wrap: bool,
) -> Vec<(usize, usize, bool)> {
    if p == q {
        return vec![(p, p, true)]; // 空段落（如文末空行）仍占一视觉行
    }
    if !wrap || inner_w <= 0 {
        return vec![(p, q, true)];
    }
    let mut out = Vec::new();
    let mut ls = p;
    while ls < q {
        let base = prefix[ls - p];
        // 在宽度内尽量多放字符（至少 1 个，超宽单字符兜底）。
        let mut e = ls;
        while e < q && prefix[e + 1 - p] - base <= inner_w {
            e += 1;
        }
        if e == ls {
            e = ls + 1;
        }
        // 词换行：若行后仍有内容，在最后一个空格后断开。
        // sp∈[ls,e) ⇒ brk=sp+1∈[ls+1,e]，恒 > ls，保证单调推进、不死循环。
        let mut brk = e;
        if e < q {
            if let Some(sp) = (ls..e).rev().find(|&k| chars[k] == ' ') {
                brk = sp + 1;
            }
        }
        let hard = brk == q;
        out.push((ls, brk, hard));
        ls = brk;
    }
    out
}

/// 字符类别，用于双击选词：把连续同类字符视为一个"词"。
#[derive(PartialEq, Eq, Clone, Copy)]
enum CharClass {
    Word,  // 字母/数字（含 Unicode 字母，如 CJK）
    Space, // 空白
    Other, // 标点/符号
}

fn classify(c: char) -> CharClass {
    if c.is_alphanumeric() {
        CharClass::Word
    } else if c.is_whitespace() {
        CharClass::Space
    } else {
        CharClass::Other
    }
}

/// 返回包含/邻接 `idx` 处字符的同类连续区间 [start, end)（字符索引）。
/// 双击选词用：在字母数字串上选整词，在空白/标点串上选该连续段。
fn word_run(chars: &[char], idx: usize) -> (usize, usize) {
    if chars.is_empty() {
        return (0, 0);
    }
    // idx 是光标间隙；取其右侧字符，末尾时取最后一个字符。
    let i = idx.min(chars.len() - 1);
    let class = classify(chars[i]);
    let mut s = i;
    while s > 0 && classify(chars[s - 1]) == class {
        s -= 1;
    }
    let mut e = i + 1;
    while e < chars.len() && classify(chars[e]) == class {
        e += 1;
    }
    (s, e)
}

impl Widget for TextInput {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let lh = text
            .measure("Ay", style.font_family.as_deref(), style.font_size, None)
            .h
            .max(style.font_size as i32);
        // 多行默认约 5 行高（用户可 .height() 覆盖）；单行沿用紧凑高度。
        let h = if self.is_multiline() { lh * 5 + 2 * TEXT_PAD } else { (style.font_size as i32) + 16 };
        Size::new(160, h)
    }
    fn paint(&self, bounds: Rect, _content: Rect, focused: bool, canvas: &mut dyn Canvas, style: &Style) {
        let th = crate::theme::current();
        let (pal, inp) = (&th.palette, &th.input);
        let (x, y, w, h) = (bounds.x as f32, bounds.y as f32, bounds.w as f32, bounds.h as f32);
        let corner = inp.corner(&th.metrics);
        canvas.fill_round_rect(x, y, w, h, corner, &Paint::fill(inp.bg(pal)));
        let border = if focused { inp.border_focus(pal) } else { inp.border(pal) };
        canvas.stroke_round_rect(x, y, w, h, corner, 1.5, &Paint::fill(border));

        // 显示串：密码模式为掩码圆点；测量/绘制/光标定位都基于它（字符数与真实文本一致）。
        let disp = self.display_string();
        let is_empty = self.text.borrow().is_empty();
        let multiline = self.is_multiline();
        // 单行：仅水平内边距，垂直占满并居中（避免矮控件被垂直裁掉文字）；
        // 多行：四周都留内边距，使多行文本不贴边。
        let vpad = if multiline { TEXT_PAD } else { 0 };
        let inner = Rect::new(bounds.x + TEXT_PAD, bounds.y + vpad, bounds.w - 2 * TEXT_PAD, bounds.h - 2 * vpad);
        let family = style.font_family.as_deref();
        let fsize = style.font_size;
        let wrap = self.config.wrap && multiline;
        let cursor = self.cursor.min(disp.chars().count());

        // 重建视觉行布局缓存。
        self.rebuild_layout(canvas, &disp, family, fsize, inner.w);
        let lay = self.layout.borrow();
        let line_h = lay.line_h.max(1);
        let (cl, cx_in) = self.caret_line_x(&lay, cursor);

        // 垂直滚动（多行）：保证光标行可见并钳制到内容范围。
        let mut sy = self.scroll_y.get();
        if multiline {
            let caret_top = cl as i32 * line_h;
            if caret_top - sy < 0 {
                sy = caret_top;
            }
            if caret_top + line_h - sy > inner.h {
                sy = caret_top + line_h - inner.h;
            }
            let content_h = lay.lines.len() as i32 * line_h;
            sy = sy.clamp(0, (content_h - inner.h).max(0));
        } else {
            sy = 0;
        }
        self.scroll_y.set(sy);

        // 水平滚动：仅非软换行（单行 / 多行不换行）时按光标更新。
        let mut sx = self.scroll_x.get();
        if !wrap {
            if cx_in - sx > inner.w {
                sx = cx_in - inner.w;
            }
            if cx_in - sx < 0 {
                sx = cx_in;
            }
            sx = sx.max(0);
        } else {
            sx = 0;
        }
        self.scroll_x.set(sx);

        // 首行 y：多行从内框顶部减滚动；单行在内框内垂直居中。
        let first_line_y = if multiline { inner.y - sy } else { inner.y + (inner.h - line_h) / 2 };
        let base_x = inner.x - sx;

        canvas.save();
        canvas.clip_rect(inner);

        // 选区高亮（逐视觉行；跨行处延伸到行尾标示换行/折行被选中）。
        if let Some((s, e)) = self.selection() {
            for (i, ln) in lay.lines.iter().enumerate() {
                let ly = first_line_y + i as i32 * line_h;
                if ly + line_h < inner.y || ly > inner.y + inner.h {
                    continue;
                }
                let a = s.clamp(ln.start, ln.end);
                let b = e.clamp(ln.start, ln.end);
                let cont = e > ln.end && s <= ln.end; // 选区越过本行末尾继续到下一行
                if b > a || cont {
                    let x1 = ln.x[a - ln.start];
                    let x2 = if cont { ln.x.last().copied().unwrap_or(0) + SEL_EOL_EXTRA } else { ln.x[b - ln.start] };
                    canvas.fill_rect(
                        (base_x + x1) as f32,
                        (ly + 2) as f32,
                        (x2 - x1) as f32,
                        (line_h - 4) as f32,
                        &Paint::fill(inp.selection(pal)),
                    );
                }
            }
        }

        if is_empty {
            let pr = Rect::new(inner.x, first_line_y, inner.w, line_h);
            canvas.draw_text(&self.placeholder, pr, inp.placeholder(pal), Align::Start, family, fsize);
        } else {
            let chars: Vec<char> = disp.chars().collect();
            for (i, ln) in lay.lines.iter().enumerate() {
                let ly = first_line_y + i as i32 * line_h;
                if ly + line_h < inner.y || ly > inner.y + inner.h {
                    continue;
                }
                if ln.end > ln.start {
                    let s: String = chars[ln.start..ln.end].iter().collect();
                    let tr = Rect::new(base_x, ly, NO_WRAP_W, line_h);
                    canvas.draw_text(&s, tr, style.fg, Align::Start, family, fsize);
                }
            }
        }

        let ly = first_line_y + cl as i32 * line_h;
        let cxx = base_x + cx_in;
        // 记录光标局部位置（相对节点左上角）供输入法候选窗定位。
        self.caret_local.set(Some((cxx - bounds.x, ly - bounds.y, line_h)));
        if focused {
            canvas.draw_line(
                cxx as f32,
                (ly + 2) as f32,
                cxx as f32,
                (ly + line_h - 2) as f32,
                1.0,
                &Paint::fill(inp.cursor(pal)),
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
                        let idx = self.pos_to_index(ctx, p.pos.x, p.pos.y);
                        let in_sel = self.selection().is_some_and(|(s, e)| idx >= s && idx < e);
                        if !in_sel {
                            self.cursor = idx;
                            self.anchor = None;
                        }
                        // 弹出上下文菜单（剪切/复制/粘贴/全选）。
                        let items = self.context_menu_items();
                        ctx.show_context_menu(p.pos, items);
                        return true;
                    }
                    // 双击选词 / 三击选段（单行无 \n 即全选，多行为当前逻辑行）。不进入拖选。
                    match p.click_count {
                        2 => {
                            let idx = self.pos_to_index(ctx, p.pos.x, p.pos.y);
                            self.select_word(idx);
                            self.dragging = false;
                            ctx.mark_dirty();
                            return true;
                        }
                        n if n >= 3 => {
                            let idx = self.pos_to_index(ctx, p.pos.x, p.pos.y);
                            self.select_para(idx);
                            self.dragging = false;
                            ctx.mark_dirty();
                            return true;
                        }
                        _ => {}
                    }
                    let idx = self.pos_to_index(ctx, p.pos.x, p.pos.y);
                    self.cursor = idx;
                    self.anchor = Some(idx);
                    self.dragging = true;
                    self.goal_x.set(None);
                    ctx.capture();
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Move if self.dragging => {
                    self.cursor = self.pos_to_index(ctx, p.pos.x, p.pos.y);
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
                    // 多行：Enter 插入换行。单行不处理（冒泡，留给默认行为）。
                    Key::Enter if self.is_multiline() => {
                        self.insert_newline(ctx);
                        true
                    }
                    // 多行：上下移动到相邻视觉行。单行不消费（冒泡）。
                    Key::Up if self.is_multiline() => {
                        self.move_vertical(ctx, false, k.shift);
                        true
                    }
                    Key::Down if self.is_multiline() => {
                        self.move_vertical(ctx, true, k.shift);
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
                        // 多行：到当前视觉行首；单行：到文本首。
                        let target = if self.is_multiline() { self.cur_line_bounds().0 } else { 0 };
                        self.move_to(ctx, target, k.shift);
                        true
                    }
                    Key::End => {
                        let target = if self.is_multiline() { self.cur_line_bounds().1 } else { len };
                        self.move_to(ctx, target, k.shift);
                        true
                    }
                    // Ctrl+A 全选（VK_A=0x41）
                    Key::Other(0x41) if k.ctrl => {
                        self.select_all();
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
    fn ime_caret(&self) -> Option<(i32, i32, i32)> {
        self.caret_local.get()
    }
}

#[cfg(test)]
mod tests {
    use super::{word_run, wrap_paragraph, TextInput, TextLayout, VisLine};
    use std::cell::RefCell;
    use std::rc::Rc;

    fn run(s: &str, idx: usize) -> (usize, usize) {
        let chars: Vec<char> = s.chars().collect();
        word_run(&chars, idx)
    }

    // 每字符宽 10 的合成前缀，用于纯函数换行测试。
    fn prefix10(len: usize) -> Vec<i32> {
        (0..=len).map(|i| i as i32 * 10).collect()
    }

    #[test]
    fn wrap_paragraph_char_wrap() {
        let chars: Vec<char> = "abcdef".chars().collect();
        let pre = prefix10(6);
        // inner_w=25 → 每行最多 2 字符（宽 20<=25，30>25）。
        let lines = wrap_paragraph(&chars, 0, 6, &pre, 25, true);
        assert_eq!(lines, vec![(0, 2, false), (2, 4, false), (4, 6, true)]);
    }

    #[test]
    fn wrap_paragraph_word_break() {
        let chars: Vec<char> = "ab cd ef".chars().collect();
        let pre = prefix10(8);
        // inner_w=45 → 在空格后断行（词换行）。
        let lines = wrap_paragraph(&chars, 0, 8, &pre, 45, true);
        assert_eq!(lines, vec![(0, 3, false), (3, 6, false), (6, 8, true)]);
    }

    #[test]
    fn wrap_paragraph_nowrap_and_empty() {
        let chars: Vec<char> = "abc".chars().collect();
        let pre = prefix10(3);
        // 不换行：整段一行。
        assert_eq!(wrap_paragraph(&chars, 0, 3, &pre, 10, false), vec![(0, 3, true)]);
        // 空段落：占一视觉行。
        assert_eq!(wrap_paragraph(&chars, 3, 3, &[0], 50, true), vec![(3, 3, true)]);
    }

    fn dummy_input() -> TextInput {
        TextInput::new(Rc::new(RefCell::new(String::new())), String::new())
    }

    #[test]
    fn cursor_line_soft_break_affinity() {
        let ti = dummy_input();
        // 两视觉行 [0,3) 软换行 + [3,6)。
        let lay = TextLayout {
            lines: vec![
                VisLine { start: 0, end: 3, x: vec![0, 10, 20, 30], hard: false },
                VisLine { start: 3, end: 6, x: vec![0, 10, 20, 30], hard: true },
            ],
            line_h: 14,
        };
        assert_eq!(ti.cursor_line(&lay, 0), 0);
        assert_eq!(ti.cursor_line(&lay, 2), 0);
        // 软换行边界 c==3：归属下一行（折行后行首）。
        assert_eq!(ti.cursor_line(&lay, 3), 1);
        assert_eq!(ti.cursor_line(&lay, 6), 1);
    }

    #[test]
    fn cursor_line_hard_break_stays() {
        let ti = dummy_input();
        // 硬换行行 [0,1)（"a\n"）+ [2,3)（"b"）。
        let lay = TextLayout {
            lines: vec![
                VisLine { start: 0, end: 1, x: vec![0, 10], hard: true },
                VisLine { start: 2, end: 3, x: vec![0, 10], hard: true },
            ],
            line_h: 14,
        };
        // c==1 在硬换行末尾：停在本行（光标在 \n 前）。
        assert_eq!(ti.cursor_line(&lay, 1), 0);
        assert_eq!(ti.cursor_line(&lay, 2), 1);
    }

    #[test]
    fn word_run_selects_alnum_word() {
        // "hello world"：在 "hello"(0..5) 内任意位置选中整词。
        assert_eq!(run("hello world", 0), (0, 5));
        assert_eq!(run("hello world", 3), (0, 5));
        // 间隙 5 在 'h'..='o' 末尾右侧是空格，取右侧字符=空格 → 选空白段(5..6)。
        assert_eq!(run("hello world", 5), (5, 6));
        // "world" 内。
        assert_eq!(run("hello world", 8), (6, 11));
    }

    #[test]
    fn word_run_handles_punct_and_cjk() {
        // 标点自成一类：连续 "!!" 作为一段。
        assert_eq!(run("a!!b", 1), (1, 3));
        // CJK 与拉丁同属 Word 类（均 alphanumeric），连续字母数字合并为一个词。
        assert_eq!(run("你好world", 1), (0, 7));
        assert_eq!(run("你好world", 3), (0, 7));
        // 空白分隔则各自成词。
        assert_eq!(run("你好 world", 1), (0, 2));
        assert_eq!(run("你好 world", 4), (3, 8));
    }

    #[test]
    fn word_run_empty_and_end() {
        assert_eq!(run("", 0), (0, 0));
        // idx 超界 → 取最后一个字符所在词。
        assert_eq!(run("ab", 5), (0, 2));
    }
}
