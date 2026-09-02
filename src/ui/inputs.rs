//! Phase 4 基础输入控件：CheckBox / Switch / RadioButton / Slider / TextInput。
//!
//! 控件通过 `Rc<Cell<T>>` / `Rc<RefCell<String>>` 与外部状态双向绑定：控件改值
//! 即写入共享单元，外部随时读取，无需回调闭包。

use std::cell::{Cell, RefCell};

use crate::anim::{Easing, Lerp, Transition};
use crate::core::{ClickFn, EventCtx, Widget};
use crate::event::{
    CursorShape, Event, Key, KeyEvent, MenuItem, MouseButton, PointerKind, Preedit,
};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;
use crate::theme::Intent;
use crate::ui::caret::{CaretOpts, CaretState, CaretStyle};
use crate::ui::containers::VScrollbar;
use crate::ui::TextContent;

const BOX_SIZE: i32 = 18;
const BOX_SIZE_SMALL: i32 = 14;
const GAP: i32 = 8;
const GAP_SMALL: i32 = 6;

/// CheckBox 尺寸变体。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckBoxSize {
    #[default]
    Normal,
    Small,
}

// ---------------- CheckBox ----------------

/// 文本视觉行布局的缓存键：显示串 + 内框宽 + 字族 + 字号 + 字重 + 行高。
///
/// 凡是会改变排版结果的输入都必须在键里，漏一项就会沿用错误的视觉行——
/// 表现为光标落点与字形对不上，而非明显的渲染错误，很难查。
type LayoutKey = (String, i32, Option<String>, u32, u16, Option<u32>);

pub struct CheckBox {
    label: TextContent,
    state: Signal<bool>,
    /// 勾选填充补间（0=未选、1=选中）：驱动方框底色 white↔accent + 对勾淡入。
    fill: Cell<Transition<f32>>,
    /// 显隐翻转经 `reset_interaction` 复位；复显时首帧靠 `primed` 瞬时落定填充，不回放动画。
    primed: Cell<bool>,
    /// 点击拦截回调（受控模式）。设了它，点击/键盘激活只调回调、不自动翻转 `state`，
    /// 渲染完全跟随 `state` 当前值——app 可在翻转前弹确认、确认后再 `state.set(..)`。
    on_toggle: Option<ClickFn>,
    /// 语义意图色（默认 Primary=主题 accent）。运行时在 paint 解析，故 danger/自定义随主题/实例而变。
    intent: Intent,
    size: CheckBoxSize,
}

impl CheckBox {
    pub fn new(label: impl Into<TextContent>, state: Signal<bool>) -> Self {
        let init = if state.get() { 1.0 } else { 0.0 };
        Self {
            label: label.into(),
            state,
            fill: Cell::new(Transition::new(init)),
            primed: Cell::new(false),
            on_toggle: None,
            intent: Intent::Primary,
            size: CheckBoxSize::Normal,
        }
    }
    /// 设置语义意图色（供 Builder 的 `.intent()/.danger()/.accent()` 调用）。
    pub fn set_intent(&mut self, intent: Intent) {
        self.intent = intent;
    }
    /// 设置尺寸变体（供 Builder 的 `.small()` 调用）。
    pub fn set_size(&mut self, size: CheckBoxSize) {
        self.size = size;
    }
    fn font_size(&self, style: &Style) -> f32 {
        match self.size {
            CheckBoxSize::Normal => style.font_size,
            CheckBoxSize::Small => (style.font_size - 2.0).max(10.0),
        }
    }
    fn toggle(&mut self, ctx: &mut EventCtx) {
        if let Some(cb) = self.on_toggle.as_mut() {
            cb(ctx);
        } else {
            self.state.set(!self.state.get());
        }
        ctx.mark_dirty();
    }
}

impl Widget for CheckBox {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let (bsz, gap) = match self.size {
            CheckBoxSize::Normal => (BOX_SIZE, GAP),
            CheckBoxSize::Small => (BOX_SIZE_SMALL, GAP_SMALL),
        };
        let fsize = self.font_size(style);
        let t = text.measure(
            self.label.resolve().as_ref(),
            &crate::text::TextStyle::of(style).with_size(fsize),
            None,
        );
        Size::new(bsz + gap + t.w, bsz.max(t.h))
    }
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        let (p, tg) = (&th.palette, &th.toggle);
        // intent 解析填充色与对勾色：Primary 走 ToggleTheme（保持换肤）；其余由 intent 派生，
        // 对勾色用 IntentColors.fg 自适应对比（浅底自动转深）。
        let (base_accent, check_fg) = match self.intent {
            Intent::Primary => (tg.accent(p), p.on_accent),
            other => {
                let ic = other.colors(p);
                (ic.bg, ic.fg)
            }
        };
        // 禁用：强调色降为灰轨道、文字用 text_disabled。
        // 文字色：设了 fg_role 时按当前主题解析（运行期换肤跟随，暗色自动转浅），与 label 一致。
        let accent = if enabled { base_accent } else { p.track };
        let text_color = if !enabled {
            p.text_disabled
        } else if style.fg_role.is_some() {
            style.resolved_fg(&th)
        } else {
            style.fg
        };
        let (bsz_i, gap_i, radius, stroke_w, check_stroke) = match self.size {
            CheckBoxSize::Normal => (BOX_SIZE, GAP, 4.0_f32, 1.5_f32, 2.0_f32),
            CheckBoxSize::Small => (BOX_SIZE_SMALL, GAP_SMALL, 3.0_f32, 1.0_f32, 1.5_f32),
        };
        let cy = bounds.y + (bounds.h - bsz_i) / 2;
        let (bx, by) = (bounds.x as f32, cy as f32);
        // 勾选填充补间：据状态改向，amount 驱动底色渐变 + 边框淡出 + 对勾淡入。
        let mut fill = self.fill.get();
        let target = if self.state.get() { 1.0 } else { 0.0 };
        if !self.primed.get() {
            // 首帧/对话框复显：瞬时落定到当前状态，不回放动画。
            fill = Transition::new(target);
            self.primed.set(true);
        } else if fill.target() != target {
            fill.retarget(target, th.anim.fast(), Easing::EaseOut);
        }
        let amount = fill.animate();
        self.fill.set(fill);
        let sz = bsz_i as f32;
        // 底色 white→accent；边框（未选描边）随填充淡出。
        canvas.fill_round_rect(
            bx,
            by,
            sz,
            sz,
            radius,
            &Paint::fill(tg.knob(p).lerp(accent, amount)),
        );
        if amount < 1.0 {
            canvas.stroke_round_rect(
                bx,
                by,
                sz,
                sz,
                radius,
                stroke_w,
                &Paint::fill(tg.track(p).scale_alpha(1.0 - amount)),
            );
        }
        if amount > 0.0 {
            // 启用用 intent 解析的对比色（浅底自动转深）；禁用回退 on_accent。
            let check = if enabled { check_fg } else { p.on_accent };
            // 勾：一条三点折线按 amount 淡入；坐标按方框尺寸等比缩放。
            // 必须走 `draw_polyline` 而不是两次 `draw_line`——后者在 V 底那个拐点
            // 留一块楔形缺口，低 DPI 被抗锯齿糊过去，150% 以上就是肉眼可见的裂口。
            let paint = Paint::fill(check.scale_alpha(amount));
            let s = sz / 18.0; // 18px 为基准尺寸
            canvas.draw_polyline(
                &[
                    (bx + 4.0 * s, by + 9.0 * s),
                    (bx + 8.0 * s, by + 13.0 * s),
                    (bx + 14.0 * s, by + 5.0 * s),
                ],
                check_stroke,
                &paint,
            );
        }
        let text_rect = Rect::new(
            bounds.x + bsz_i + gap_i,
            bounds.y,
            bounds.w - bsz_i - gap_i,
            bounds.h,
        );
        canvas.draw_text(
            self.label.resolve().as_ref(),
            text_rect,
            text_color,
            Align::Start,
            &crate::text::TextStyle::of(style).with_size(self.font_size(style)),
        );
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
    fn take_click(&mut self, f: ClickFn) {
        self.on_toggle = Some(f);
    }
    fn reset_interaction(&mut self) {
        self.primed.set(false); // 下次显示瞬时落定填充，不回放旧的勾选动画
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

// ---------------- Switch ----------------

// 轨道尺寸基准（宽 × 高）；knob 上下各内缩 SWITCH_PAD，直径 = h - 2*PAD。
const SWITCH_W: i32 = 44;
const SWITCH_H: i32 = 24;
const SWITCH_W_SMALL: i32 = 36;
const SWITCH_H_SMALL: i32 = 20;
const SWITCH_PAD: f32 = 3.0;

/// Switch 尺寸变体。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum SwitchSize {
    #[default]
    Normal,
    Small,
}

impl SwitchSize {
    /// 该尺寸的轨道宽高（像素）。
    fn dims(self) -> (i32, i32) {
        match self {
            SwitchSize::Normal => (SWITCH_W, SWITCH_H),
            SwitchSize::Small => (SWITCH_W_SMALL, SWITCH_H_SMALL),
        }
    }
}

pub struct Switch {
    state: Signal<bool>,
    /// 滑块位置补间（0=关、1=开）；同时驱动轨道色 off↔on 渐变。retarget-in-paint。
    pos: Cell<Transition<f32>>,
    /// 显隐翻转经 `reset_interaction` 复位；复显时首帧靠 `primed` 瞬时落定位置，不回放动画。
    primed: Cell<bool>,
    size: SwitchSize,
}

impl Switch {
    pub fn new(state: Signal<bool>) -> Self {
        let init = if state.get() { 1.0 } else { 0.0 };
        Self {
            state,
            pos: Cell::new(Transition::new(init)),
            primed: Cell::new(false),
            size: SwitchSize::Normal,
        }
    }
    /// 设置尺寸变体（供 Builder 的 `.small()` 调用）。
    pub fn set_size(&mut self, size: SwitchSize) {
        self.size = size;
    }
    fn toggle(&self, ctx: &mut EventCtx) {
        self.state.set(!self.state.get());
        ctx.mark_dirty();
    }
}

impl Widget for Switch {
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        let (w, h) = self.size.dims();
        Size::new(w, h)
    }
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
        let (bw, bh) = self.size.dims();
        let h = bh.min(bounds.h);
        let w = bw.min(bounds.w);
        let x = bounds.x as f32;
        let y = (bounds.y + (bounds.h - h) / 2) as f32;
        let on = self.state.get();
        let th = crate::theme::current();
        let (p, tg) = (&th.palette, &th.toggle);
        // 滑块位置补间：据当前状态改向，取动画值（0..1）同时驱动 knob 平移与轨道色渐变。
        let mut pos = self.pos.get();
        let target = if on { 1.0 } else { 0.0 };
        if !self.primed.get() {
            // 首帧/对话框复显：瞬时落定到当前状态，不回放动画。
            pos = Transition::new(target);
            self.primed.set(true);
        } else if pos.target() != target {
            pos.retarget(target, th.anim.normal(), Easing::EaseInOut);
        }
        let amount = pos.animate();
        self.pos.set(pos);
        // 禁用：开态轨道也降为灰，整体弱化；否则按位置在 off↔on 轨道色间插值。
        let track = if !enabled {
            p.track
        } else {
            tg.track(p).lerp(tg.accent(p), amount)
        };
        canvas.fill_round_rect(
            x,
            y,
            w as f32,
            h as f32,
            h as f32 / 2.0,
            &Paint::fill(track),
        );
        let r = (h as f32 - 2.0 * SWITCH_PAD) / 2.0;
        let (off_cx, on_cx) = (x + SWITCH_PAD + r, x + w as f32 - SWITCH_PAD - r);
        let knob_cx = off_cx.lerp(on_cx, amount);
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
    fn reset_interaction(&mut self) {
        self.primed.set(false); // 下次显示瞬时落定位置，不回放旧的开关滑动动画
    }
    fn focusable(&self) -> bool {
        true
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

// ---------------- RadioButton ----------------

pub struct RadioButton {
    label: TextContent,
    group: Signal<usize>,
    index: usize,
    /// 选中补间（0=未选、1=选中）：驱动外环色 + 环厚 + 中心点半径。
    sel: Cell<Transition<f32>>,
    /// 显隐翻转经 `reset_interaction` 复位；复显时首帧靠 `primed` 瞬时落定选中态，不回放动画。
    primed: Cell<bool>,
}

impl RadioButton {
    pub fn new(label: impl Into<TextContent>, group: Signal<usize>, index: usize) -> Self {
        let init = if group.get() == index { 1.0 } else { 0.0 };
        Self {
            label: label.into(),
            group,
            index,
            sel: Cell::new(Transition::new(init)),
            primed: Cell::new(false),
        }
    }
    fn selected(&self) -> bool {
        self.group.get() == self.index
    }
    fn select(&self, ctx: &mut EventCtx) {
        self.group.set(self.index);
        // 共享 group 被同组其他单选按钮读取：选中变化会改其同伴视觉，属非局部 → 整窗失效。
        ctx.mark_dirty_all();
    }
}

impl Widget for RadioButton {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let t = text.measure(
            self.label.resolve().as_ref(),
            &crate::text::TextStyle::of(style),
            None,
        );
        Size::new(BOX_SIZE + GAP + t.w, BOX_SIZE.max(t.h))
    }
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        let (p, tg) = (&th.palette, &th.toggle);
        // 禁用：强调色降为灰、文字用 text_disabled。
        let accent = if enabled { tg.accent(p) } else { p.track };
        let text_color = if !enabled {
            p.text_disabled
        } else {
            style.resolved_fg(&th)
        };
        let cy = bounds.y + bounds.h / 2;
        let cx = bounds.x + BOX_SIZE / 2;
        let outer = BOX_SIZE as f32 / 2.0;
        // 选中补间：amount 驱动外环色(track→accent)、环厚(1.5→5)、中心点半径(0→outer-8)。
        let mut sel = self.sel.get();
        let target = if self.selected() { 1.0 } else { 0.0 };
        if !self.primed.get() {
            // 首帧/对话框复显：瞬时落定到当前选中态，不回放动画。
            sel = Transition::new(target);
            self.primed.set(true);
        } else if sel.target() != target {
            sel.retarget(target, th.anim.fast(), Easing::EaseOut);
        }
        let amount = sel.animate();
        self.sel.set(sel);
        let (cxf, cyf) = (cx as f32, cy as f32);
        canvas.fill_circle(
            cxf,
            cyf,
            outer,
            &Paint::fill(tg.track(p).lerp(accent, amount)),
        );
        canvas.fill_circle(
            cxf,
            cyf,
            outer - 1.5f32.lerp(5.0, amount),
            &Paint::fill(tg.knob(p)),
        );
        if amount > 0.0 {
            canvas.fill_circle(cxf, cyf, (outer - 8.0) * amount, &Paint::fill(accent));
        }
        let text_rect = Rect::new(
            bounds.x + BOX_SIZE + GAP,
            bounds.y,
            bounds.w - BOX_SIZE - GAP,
            bounds.h,
        );
        canvas.draw_text(
            self.label.resolve().as_ref(),
            text_rect,
            text_color,
            Align::Start,
            &crate::text::TextStyle::of(style),
        );
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
    fn reset_interaction(&mut self) {
        self.primed.set(false); // 下次显示瞬时落定选中态，不回放旧的单选动画
    }
    fn focusable(&self) -> bool {
        true
    }
}

// ---------------- Slider ----------------

/// 值标签额外占用的宽度（px），仅 `show_value` 开启时生效。"100%" 约 4 字符。
const VALUE_LABEL_W: i32 = 44;

pub struct Slider {
    value: Signal<f32>, // 0.0..=1.0
    dragging: bool,
    /// 是否在旋钮右侧显示当前值百分比（如 "65%"）。
    pub show_value: bool,
}

impl Slider {
    pub fn new(value: Signal<f32>) -> Self {
        Self {
            value,
            dragging: false,
            show_value: false,
        }
    }

    pub fn set_show_value(&mut self, on: bool) {
        self.show_value = on;
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
    fn measure(&self, _avail: Size, style: &Style, _text: &mut dyn TextEngine) -> Size {
        let extra = if self.show_value { VALUE_LABEL_W } else { 0 };
        // 高度略高于旋钮直径，留出文字空间（show_value 时取字号与旋钮直径的较大值）。
        let h = if self.show_value {
            (2 * KNOB_R).max(style.font_size as i32 + 4)
        } else {
            2 * KNOB_R
        };
        Size::new(120 + extra, h)
    }

    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let v = self.value.get().clamp(0.0, 1.0);
        let th = crate::theme::current();
        let (pal, tg) = (&th.palette, &th.toggle);
        let accent = if enabled { tg.accent(pal) } else { pal.track };

        // show_value 时把轨道限制在左侧，右侧留给标签。
        let track_w = if self.show_value {
            bounds.w - VALUE_LABEL_W
        } else {
            bounds.w
        };
        let cy = bounds.y as f32 + bounds.h as f32 / 2.0;
        let r = KNOB_R as f32;
        let x0 = bounds.x as f32 + r;
        let x1 = bounds.x as f32 + track_w as f32 - r;
        let knob_x = x0 + (x1 - x0) * v;

        // 轨道
        canvas.fill_round_rect(
            x0,
            cy - 2.0,
            (x1 - x0).max(0.0),
            4.0,
            2.0,
            &Paint::fill(tg.track(pal)),
        );
        // 已填充
        canvas.fill_round_rect(
            x0,
            cy - 2.0,
            (knob_x - x0).max(0.0),
            4.0,
            2.0,
            &Paint::fill(accent),
        );
        // 旋钮
        canvas.fill_circle(knob_x, cy, r, &Paint::fill(tg.knob(pal)));
        canvas.fill_circle(knob_x, cy, r - 2.0, &Paint::fill(accent));

        // 值标签
        if self.show_value {
            let label = format!("{:.0}%", v * 100.0);
            let label_rect = Rect::new(bounds.x + track_w, bounds.y, VALUE_LABEL_W, bounds.h);
            let text_color = if enabled { pal.text } else { pal.text_disabled };
            canvas.draw_text(
                &label,
                label_rect,
                text_color,
                crate::spec::Align::Center,
                &crate::text::TextStyle::of(style),
            );
        }
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
                // 仅拖动期间响应，避免悬停即改值（非拖动的 Move 落到 `_ => false`）。
                PointerKind::Move if self.dragging => {
                    self.set_from_pos(ctx, p.pos.x);
                    true
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

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
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

/// TextInput 行为配置。由 Builder 的 `.password()/.multiline()/.wrap()/.leading_icon()` 设置。
#[derive(Clone, Copy)]
pub struct TextConfig {
    /// 多行模式（P4 实现编辑/换行；当前仅占位存储）。
    pub multiline: bool,
    /// 密码模式：显示为掩码圆点、禁止复制/剪切、强制单行。
    pub password: bool,
    /// 多行软换行（仅 multiline 生效）；false 时仅显式 \n 换行。
    pub wrap: bool,
    /// 前置图标字形（如放大镜 🔍）：在左侧留出图标区并绘制，文字/光标/命中相应右移。
    /// 单字形（Copy 友好）；搜索框等用。
    pub leading: Option<char>,
    /// 无边框：不画自身的背景与边框。供**外框由容器统一绘制**的复合控件内嵌
    /// （如 `Element::stepper` 的中部数值框）——两层框叠在一起会画出双线。
    pub frameless: bool,
    /// 单行水平对齐（`Start`/`Center`/`End`）。多行忽略（多行按视觉行左对齐）。
    ///
    /// 只在文本**放得下**时生效：放不下时对齐偏移恒为 0，交回水平滚动接管，
    /// 否则居中会和"滚动跟随光标"打架，长文本编辑时左右横跳。
    pub align: Align,
}

impl Default for TextConfig {
    /// wrap 默认开启，使多行默认软换行；类型自带正确默认避免直接构造踩坑。
    fn default() -> Self {
        Self {
            multiline: false,
            password: false,
            wrap: true,
            leading: None,
            frameless: false,
            align: Align::Start,
        }
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
    /// 重建缓存键：显示串 + 内框宽 + 全部文字属性（字族/字号/字重/行高）。命中则
    /// 跳过整次重建（含每段 O(L²) 累计测量），仅在文本/宽度/字体变化时才重排——
    /// 光标移动/闪烁、悬停等无关重绘不再触发布局。
    ///
    /// 字重与行高必须进键：它们同样改变每行的宽高，漏掉会让加粗/改行距后沿用旧的
    /// 视觉行，光标落点与实际字形对不上。
    key: Option<LayoutKey>,
}

pub struct TextInput {
    text: Signal<String>,
    placeholder: String,
    config: TextConfig,
    cursor: usize,         // 字符索引
    anchor: Option<usize>, // 选区锚点（Some 且 != cursor 时有选区）
    scroll_x: Cell<i32>,   // 水平滚动偏移（逻辑 px），paint 时按光标更新
    scroll_y: Cell<i32>,   // 垂直滚动偏移（逻辑 px，多行用），paint 时按光标更新
    /// 上下移动时保持的目标列像素（粘性 goal column）；水平移动/编辑后清空。
    goal_x: Cell<Option<i32>>,
    layout: RefCell<TextLayout>, // paint 重建的视觉行缓存
    /// 最近一帧绘制的光标局部位置 (x, y_top, height)（节点局部逻辑坐标），供输入法定位。
    caret_local: Cell<Option<(i32, i32, i32)>>,
    /// 插入光标的闪烁相位与平滑移动状态（见 [`crate::ui::caret`]）。
    caret: CaretState,
    dragging: bool,
    scrollbar: VScrollbar,
    /// true 时 paint 将视口滚到光标位置（键盘移动/鼠标点击后设置）；
    /// 滚轮滚动不设置，避免 paint 每帧重置 scroll_y。
    follow_cursor: Cell<bool>,
    /// 鼠标当前悬停在滚动条命中区内（影响光标形状）。
    hover_in_scrollbar: Cell<bool>,
    /// 最近一帧 paint 用的字号快照：供无 style 的命中路径换算前置图标左偏移。
    font_size_hint: Cell<f32>,
    /// 最近一帧 paint 算出的单行对齐偏移（见 [`TextConfig::align`]）。
    ///
    /// 命中测试必须减掉同一个值，否则居中/右对齐时点哪儿光标都落在别处。它依赖
    /// 本帧的测量结果，故只能由 paint 写、事件读——与 `font_size_hint` 同一形状。
    align_off: Cell<i32>,
    /// 输入法组合态（拼音等未上屏）：为 true 时暂不绘制自绘光标条，避免与系统
    /// 组合浮层里跟随组合进度的光标重叠、显得"卡在组合开始前"。
    ///
    /// **只有 win32 会置位**——那边系统 IME 自己画合成串，我们藏光标是在消除双光标。
    /// 需要自绘合成串的平台（macOS、将来的 Linux）走 [`Self::preedit`]。
    composing: Cell<bool>,
    /// 输入法未提交的合成串（见 [`crate::event::Preedit`]）。非空时内联绘制在光标处：
    /// 参与测量与换行、带下划线、光标画在合成串内部。
    ///
    /// **win32 上恒为空**：那边系统 IME 已经在 `ImmSetCompositionWindow` 指定处画了一份，
    /// 再自绘一份就成了双份合成串。
    preedit: RefCell<Preedit>,
    /// 单行模式下 Enter 的出口（见 [`crate::ui::Element::on_submit`]）。
    on_submit: Option<SubmitFn>,
    /// 本控件**未处理**的导航键的出口（见 [`crate::ui::Element::on_nav_key`]）。
    on_nav_key: Option<NavKeyFn>,
    /// 输入准入过滤：对**改动之后的完整正文**表态，`false` 则整次改动被丢弃。
    ///
    /// 收整串而不是单个字符，是因为数字这类格式的合法性本就是全局的——`-` 只能打头、
    /// `.` 只能有一个，逐字符判定说不清这些。同一把尺子同时管住键入与粘贴，
    /// 不会出现"打不进去但能粘进去"。
    filter: Option<FilterFn>,
    /// 「本控件被点了」的通知（见 [`crate::ui::Element::on_click`]）。
    ///
    /// 与 Button 的同名回调**语义不同**：那里 on_click 就是激活本身，这里只是旁路通知，
    /// 输入框该定位光标、该起拖选照旧。宿主用它做的是「点了输入框」这件事的附带反应，
    /// 例如重新展开一个被 Escape 收起的补全/结果浮层——那种状态只有宿主知道，
    /// 而在此之前它连"用户又点了一次输入框"都收不到。
    on_click: Option<ClickFn>,
}

/// 单行 Enter 回调。与 `on_click` 同形（`ctx` 在首位）。
type SubmitFn = Box<dyn FnMut(&mut EventCtx)>;
/// 导航键回调：收本控件未消费的导航键，返回是否消费。
///
/// 收整个 `KeyEvent` 而不只是 `Key`：Tab 必须能区分 Shift——应用把 Tab 用作「接受补全」
/// 时若连 Shift+Tab 一起吞掉，用户就没有任何键盘途径离开这个输入框了。
type NavKeyFn = Box<dyn FnMut(&mut EventCtx, KeyEvent) -> bool>;
/// 输入准入过滤：入参是**改动后的完整正文**，返回是否放行。见 `TextInput::filter`。
type FilterFn = Box<dyn Fn(&str) -> bool>;

impl TextInput {
    pub fn new(text: Signal<String>, placeholder: String) -> Self {
        let cursor = text.with(|t| t.chars().count());
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
            caret: CaretState::new(),
            dragging: false,
            scrollbar: VScrollbar::new(),
            follow_cursor: Cell::new(true),
            hover_in_scrollbar: Cell::new(false),
            font_size_hint: Cell::new(14.0),
            align_off: Cell::new(0),
            composing: Cell::new(false),
            preedit: RefCell::new(Preedit::default()),
            on_submit: None,
            on_nav_key: None,
            filter: None,
            on_click: None,
        }
    }

    /// 可变访问配置（供 Builder 配置）。
    pub fn config_mut(&mut self) -> &mut TextConfig {
        &mut self.config
    }

    /// 设置单行 Enter 回调（供 Builder；下游用 [`crate::ui::Element::on_submit`]）。
    pub fn set_on_submit(&mut self, f: impl FnMut(&mut EventCtx) + 'static) {
        self.on_submit = Some(Box::new(f));
    }

    /// 设置导航键回调（供 Builder；下游用 [`crate::ui::Element::on_nav_key`]）。
    pub fn set_on_nav_key(&mut self, f: impl FnMut(&mut EventCtx, KeyEvent) -> bool + 'static) {
        self.on_nav_key = Some(Box::new(f));
    }

    /// 设置输入准入过滤（见 [`Self::filter`]）。入参是改动后的完整正文。
    pub fn set_filter(&mut self, f: impl Fn(&str) -> bool + 'static) {
        self.filter = Some(Box::new(f));
    }

    /// 「删除选区 + 在光标处插入 `ins`」之后的候选正文——过滤器的入参。
    ///
    /// 无选区时 `selection()` 返回 `None`，退化为在光标处纯插入。
    fn candidate_text(&self, ins: &str) -> String {
        let (s, e) = self.selection().unwrap_or((self.cursor, self.cursor));
        self.text.with(|t| {
            let (bs, be) = (char_to_byte(t, s), char_to_byte(t, e));
            let mut out = String::with_capacity(t.len() + ins.len());
            out.push_str(&t[..bs]);
            out.push_str(ins);
            out.push_str(&t[be..]);
            out
        })
    }

    /// 这次改动放不放行。未设过滤器时恒放行。
    fn allows(&self, ins: &str) -> bool {
        match &self.filter {
            Some(f) => f(&self.candidate_text(ins)),
            None => true,
        }
    }

    /// 触发 `on_submit`；返回是否消费了这次 Enter。
    ///
    /// 未设回调时返回 `false`——如实说明"本控件没处理"。它**不会**因此冒泡（见按键
    /// 分支处的注释），但把消费与否报准仍有意义：宿主据 `consumed` 决定 Escape 的
    /// 关窗兜底走不走，报错会连带影响它。
    fn fire_submit(&mut self, ctx: &mut EventCtx) -> bool {
        match &mut self.on_submit {
            Some(f) => {
                f(ctx);
                true
            }
            None => false,
        }
    }

    /// 触发 `on_nav_key`；返回是否消费——**由回调自己说**。
    ///
    /// 与 [`Self::fire_submit`] 的区别正在这里：Tab 未被消费时宿主会拿它做焦点导航，
    /// 所以「消费与否」必须能逐次表态。回调无脑返回 `true` 会吃掉 Shift+Tab，
    /// 让用户失去离开该控件的唯一键盘途径。
    fn fire_nav_key(&mut self, ctx: &mut EventCtx, ev: KeyEvent) -> bool {
        match &mut self.on_nav_key {
            Some(f) => f(ctx, ev),
            None => false,
        }
    }

    /// 触发 `on_click`。**无返回值**：这次点击归本控件消费是既定的，回调只是旁路通知。
    ///
    /// 与 [`Self::fire_submit`] 的区别在这里——那个要如实报「本控件处理了没有」，
    /// 因为宿主据此决定 Escape 关窗兜底走不走；点击没有这层歧义。
    ///
    /// ⚠ 回调跑在 `Down` 处理器的**中段**，其后本控件还要设 `dragging` 并 `ctx.capture()`。
    /// 而 `EventOutcome.capture` 是单个 `Option`、后写覆盖先写——**回调里请求的捕获
    /// 会被随后那句 `ctx.capture()` 无声抹掉**。这是「链上去不报错也不生效」的同一形状，
    /// 故写在这里备案；不把 `fire_click` 挪到状态更新之后，是因为那会改变它与双击/三击
    /// 分支的先后（选词、选段也该通知，见调用点）。
    fn fire_click(&mut self, ctx: &mut EventCtx) {
        if let Some(f) = self.on_click.as_mut() {
            f(ctx);
        }
    }

    /// 运行期是否多行：密码模式恒为单行（与 Builder 链式顺序无关，杜绝换行进入密码底层文本）。
    fn is_multiline(&self) -> bool {
        self.config.multiline && !self.config.password
    }

    /// 前置图标占用的左侧内宽（图标区 ≈ 字号 + 间距）；无图标为 0。
    /// paint 的 `inner` 与命中的 `local_x` 都按此右移，保证文字/光标/点击一致。
    fn lead_inset(&self, fsize: f32) -> i32 {
        if self.config.leading.is_some() {
            (fsize as i32) + 8
        } else {
            0
        }
    }

    fn char_count(&self) -> usize {
        self.text.with(|t| t.chars().count())
    }

    /// 实际用于显示与测量的字符串：密码模式下逐字符替换为掩码圆点；
    /// 有输入法合成串时把它插在光标处。
    ///
    /// **两种变换的性质不同，别混为一谈**：密码掩码是**等字符数替换**，故索引可直接复用；
    /// 合成串是**插入**，字符数变了，光标与选区索引必须过
    /// [`to_display_index`](Self::to_display_index) 换算。全库只有 paint 消费本方法，
    /// 那 5 处换算点都在 paint 里。
    fn display_string(&self) -> String {
        let base = self.text.with(|t| {
            if self.config.password {
                t.chars().map(|_| PASSWORD_MASK).collect()
            } else {
                t.clone()
            }
        });
        let pe = self.preedit.borrow();
        if !pe.is_active() {
            return base;
        }
        // 合成串插在逻辑光标处。
        //
        // 密码模式下合成串**不**掩码：候选窗就明晃晃浮在屏幕上，把合成串打码既没有
        // 安全收益，又让用户无法确认自己打了什么——原生 NSSecureTextField 同样不掩码。
        let cursor = self.cursor.min(base.chars().count());
        let mut out: String = base.chars().take(cursor).collect();
        out.push_str(&pe.text);
        out.extend(base.chars().skip(cursor));
        out
    }

    /// 逻辑字符索引 → 显示串字符索引。
    ///
    /// 合成串插在 `self.cursor` 处，故其后的索引整体右移合成串的长度。
    /// 无合成时是恒等映射。
    fn to_display_index(&self, i: usize) -> usize {
        let pe = self.preedit.borrow();
        if !pe.is_active() || i <= self.cursor {
            i
        } else {
            i + pe.char_len()
        }
    }

    /// 显示串字符索引 → 逻辑字符索引。
    ///
    /// 落在合成串**内部**的索引一律钳到 `self.cursor`——合成串不属于正文，
    /// 没有对应的逻辑位置。（正常路径下点击会先中止合成，故这一档只在同帧竞态出现。）
    fn to_logical_index(&self, d: usize) -> usize {
        let pe = self.preedit.borrow();
        if !pe.is_active() {
            return d;
        }
        let n = pe.char_len();
        if d <= self.cursor {
            d
        } else if d < self.cursor + n {
            self.cursor
        } else {
            d - n
        }
    }

    /// 合成串在**显示串**中的字符区间 `[start, end)`。无合成时返回 None。
    fn preedit_span(&self) -> Option<(usize, usize)> {
        let pe = self.preedit.borrow();
        if !pe.is_active() {
            return None;
        }
        let s = self.cursor.min(self.char_count());
        Some((s, s + pe.char_len()))
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
            self.text.update(|t| {
                let bs = char_to_byte(t, s);
                let be = char_to_byte(t, e);
                t.replace_range(bs..be, "");
            });
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
        // 预检必须在 `delete_selection` **之前**：那一步已经改了正文，之后再算候选串
        // 就是「删了选区的结果」，过滤器拒绝时选区已经回不来了。
        let mut buf = [0u8; 4];
        if !self.allows(c.encode_utf8(&mut buf)) {
            return;
        }
        self.delete_selection(ctx);
        self.clamp_cursor();
        let cursor = self.cursor;
        self.text.update(|s| {
            let byte = char_to_byte(s, cursor);
            s.insert(byte, c);
        });
        self.cursor += 1;
        self.anchor = None;
        self.goal_x.set(None);
        ctx.mark_dirty();
    }
    fn backspace(&mut self, ctx: &mut EventCtx) {
        self.clamp_cursor();
        if self.cursor == 0 {
            return;
        }
        let cursor = self.cursor;
        self.text.update(|s| {
            let start = char_to_byte(s, cursor - 1);
            let end = char_to_byte(s, cursor);
            s.replace_range(start..end, "");
        });
        self.cursor -= 1;
        self.goal_x.set(None);
        ctx.mark_dirty();
    }
    fn delete_forward(&mut self, ctx: &mut EventCtx) {
        self.clamp_cursor();
        let len = self.char_count();
        if self.cursor >= len {
            return;
        }
        let cursor = self.cursor;
        self.text.update(|s| {
            let start = char_to_byte(s, cursor);
            let end = char_to_byte(s, cursor + 1);
            s.replace_range(start..end, "");
        });
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
        let chars: Vec<char> = self.text.with(|t| t.chars().collect());
        let (s, e) = word_run(&chars, idx);
        self.anchor = Some(s.min(chars.len()));
        self.cursor = e.min(chars.len());
    }
    /// 全选。
    /// 全选正文（Ctrl+A 走的同一条路）。公开是给**内嵌 TextInput 的复合控件**用：
    /// Stepper 用方向键改完值后全选，接着键入就是整体替换，与原生数字框一致。
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.char_count();
    }
    /// 构建右键上下文菜单项。动作经合成 Ctrl+X/C/V/A 回送到本控件，故无需感知"菜单"。
    fn context_menu_items(&self) -> Vec<MenuItem> {
        let has_sel = self.selection().is_some();
        let has_text = self.char_count() > 0;
        let pw = self.config.password;
        let ctrl = |vk: u32| KeyEvent {
            key: Key::Other(vk),
            pressed: true,
            shift: false,
            ctrl: true,
        };
        vec![
            MenuItem::key("剪切", ctrl(0x58), has_sel && !pw), // VK_X
            MenuItem::key("复制", ctrl(0x43), has_sel && !pw), // VK_C
            MenuItem::key("粘贴", ctrl(0x56), true),           // VK_V
            MenuItem::key("全选", ctrl(0x41), has_text),       // VK_A
        ]
    }
    /// 选中 `idx` 所在逻辑行（两 '\n' 之间）。单行文本无 '\n' 即全选。
    fn select_para(&mut self, idx: usize) {
        let chars: Vec<char> = self.text.with(|t| t.chars().collect());
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
    fn rebuild_layout(
        &self,
        canvas: &mut dyn Canvas,
        disp: &str,
        ts: &crate::text::TextStyle,
        inner_w: i32,
    ) {
        let (family, fsize) = (ts.family, ts.size);
        // 缓存命中（文本/宽度/字体均未变）：跳过重建，沿用上次视觉行。
        {
            let lay = self.layout.borrow();
            if let Some((k_disp, k_w, k_fam, k_size, k_weight, k_lh)) = lay.key.as_ref() {
                if k_disp == disp
                    && *k_w == inner_w
                    && k_fam.as_deref() == family
                    && *k_size == fsize.to_bits()
                    && *k_weight == ts.weight
                    && *k_lh == ts.line_height.map(f32::to_bits)
                {
                    return;
                }
            }
        }
        let chars: Vec<char> = disp.chars().collect();
        let n = chars.len();
        let multiline = self.is_multiline();
        let wrap = self.config.wrap && multiline;

        let mut lay = self.layout.borrow_mut();
        lay.key = Some((
            disp.to_string(),
            inner_w,
            family.map(str::to_string),
            fsize.to_bits(),
            ts.weight,
            ts.line_height.map(f32::to_bits),
        ));
        lay.lines.clear();
        lay.line_h = canvas.measure_text("Ay", ts).h.max(fsize as i32);

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
                prefix.push(canvas.measure_text(&acc, ts).w);
            }
            for (ls, le, hard) in wrap_paragraph(&chars, p, q, &prefix, inner_w, wrap) {
                let base = prefix[ls - p];
                let x: Vec<i32> = (ls..=le).map(|k| prefix[k - p] - base).collect();
                lay.lines.push(VisLine {
                    start: ls,
                    end: le,
                    x,
                    hard,
                });
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
        let lead = self.lead_inset(self.font_size_hint.get());
        // 减掉 paint 那一帧实际用的对齐偏移：不减的话居中/右对齐时点击落点整体偏移，
        // 点第一个字符会选到中间去。
        let local_x =
            screen_x - (b.x + TEXT_PAD + lead) + self.scroll_x.get() - self.align_off.get();
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
        // 视觉行里存的是**显示串**索引，而本方法的返回值要拿去赋给 `self.cursor`
        // ——那是**逻辑**索引。有合成串时两者差一个合成串长度，故在此收口换算，
        // 而不是让 5 个调用点各自记得包一层。
        //
        // 正常路径下点到这里时合成已被中止（平台层在 mouseDown 上先收合成），
        // 故这层换算是防竞态的兜底，不是主路径。
        self.to_logical_index(ln.start + best)
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
        let cursor = self.cursor;
        self.text.update(|s| {
            let byte = char_to_byte(s, cursor);
            s.insert(byte, '\n');
        });
        self.cursor += 1;
        self.anchor = None;
        self.goal_x.set(None);
        ctx.mark_dirty();
    }
    /// 当前选区文本（无选区返回 None）。
    fn selected_text(&self) -> Option<String> {
        let (s, e) = self.selection()?;
        self.text.with(|t| {
            let bs = char_to_byte(t, s);
            let be = char_to_byte(t, e);
            Some(t[bs..be].to_string())
        })
    }
    /// 在光标处粘贴（先删选区）。单行控件过滤所有控制字符；多行保留 '\n'
    /// （\r\n / \r 归一为 \n），仍过滤其他控制字符。
    fn paste(&mut self, ctx: &mut EventCtx, s: &str) {
        let clean: String = if self.is_multiline() {
            let normalized = s.replace("\r\n", "\n").replace('\r', "\n");
            normalized
                .chars()
                .filter(|c| *c == '\n' || !c.is_control())
                .collect()
        } else {
            s.chars().filter(|c| !c.is_control()).collect()
        };
        if clean.is_empty() {
            return;
        }
        // 与键入同一把尺子：整串一次性表态，不合格就整次丢弃（不做"逐字符挑能进的"，
        // 那会把 `12abc34` 悄悄变成 `1234`，用户看不出自己粘错了东西）。
        // 同样必须先于 `delete_selection`。
        if !self.allows(&clean) {
            return;
        }
        self.delete_selection(ctx);
        self.clamp_cursor();
        let cursor = self.cursor;
        let added = clean.chars().count();
        self.text.update(|t| {
            let byte = char_to_byte(t, cursor);
            t.insert_str(byte, &clean);
        });
        self.cursor += added;
        self.anchor = None;
        self.goal_x.set(None);
        ctx.mark_dirty();
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
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
            .measure("Ay", &crate::text::TextStyle::of(style), None)
            .h
            .max(style.font_size as i32);
        // 多行默认约 5 行高（用户可 .height() 覆盖）；单行沿用紧凑高度。
        let h = if self.is_multiline() {
            lh * 5 + 2 * TEXT_PAD
        } else {
            (style.font_size as i32) + 16
        };
        Size::new(160, h)
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
        let (pal, inp) = (&th.palette, &th.input);
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        let corner = inp.corner(&th.metrics);
        // 禁用：背景弱化、正文用 text_disabled。
        let bg = if enabled {
            inp.bg(pal)
        } else {
            pal.surface_alt
        };
        let text_color = if enabled {
            style.resolved_fg(&th)
        } else {
            pal.text_disabled
        };
        if !self.config.frameless {
            canvas.fill_round_rect(x, y, w, h, corner, &Paint::fill(bg));
            let border = if focused {
                inp.border_focus(pal)
            } else {
                inp.border(pal)
            };
            let t = crate::theme::current();
            let bw = t.metrics.border_width.to_logical(canvas.dpi_scale());
            canvas.stroke_round_rect(x, y, w, h, corner, bw, &Paint::fill(border));
        }

        // 显示串：密码模式为掩码圆点、有输入法合成串时含合成串；
        // 测量/绘制/光标定位都基于它。
        let disp = self.display_string();
        // 判空必须看**显示串**而不是正文：正文为空但正在输入合成串时，
        // 看正文会走进 placeholder 分支——合成串一个字都画不出来，
        // 用户看到的就是"打了字却只有占位符"。
        let is_empty = disp.is_empty();
        let multiline = self.is_multiline();
        // 单行：仅水平内边距，垂直占满并居中（避免矮控件被垂直裁掉文字）；
        // 多行：四周都留内边距，使多行文本不贴边。
        let vpad = if multiline { TEXT_PAD } else { 0 };
        let lead = self.lead_inset(style.font_size);
        let inner = Rect::new(
            bounds.x + TEXT_PAD + lead,
            bounds.y + vpad,
            bounds.w - 2 * TEXT_PAD - lead,
            bounds.h - 2 * vpad,
        );
        // 文字属性打包传递：字重与行高随 `Style` 自动带上，不必在每个调用点重列。
        let ts = &crate::text::TextStyle::of(style);
        let fsize = ts.size;
        self.font_size_hint.set(fsize);
        // 前置图标（搜索框等）：绘于左侧留白区，垂直居中，弱化色。
        if let Some(g) = self.config.leading {
            let mut buf = [0u8; 4];
            let icon_rect = Rect::new(bounds.x + TEXT_PAD, bounds.y, lead, bounds.h);
            canvas.draw_text(
                g.encode_utf8(&mut buf),
                icon_rect,
                pal.placeholder,
                Align::Center,
                ts,
            );
        }
        let wrap = self.config.wrap && multiline;
        // 光标在**显示串**中的位置。合成期间它落在合成串内部——IME 边打边移动
        // `pe.caret`（如拼音 "zhong" 打到第三个字母），光标要跟着走，否则会钉在
        // 合成串开头，看着像"打字光标不动"。
        let disp_len = disp.chars().count();
        let cursor = match self.preedit_span() {
            Some((ps, _)) => (ps + self.preedit.borrow().caret).min(disp_len),
            None => self.cursor.min(disp_len),
        };

        // 重建视觉行布局缓存。
        self.rebuild_layout(canvas, &disp, &crate::text::TextStyle::of(style), inner.w);
        let lay = self.layout.borrow();
        let line_h = lay.line_h.max(1);
        let (cl, cx_in) = self.caret_line_x(&lay, cursor);

        // 垂直滚动（多行）：仅在 follow_cursor 为 true 时追踪光标（键盘/点击触发）。
        // 滚轮滚动不设 follow_cursor，避免 paint 每帧把视口重置到光标位置。
        let mut sy = self.scroll_y.get();
        if multiline {
            let content_h = lay.lines.len() as i32 * line_h;
            if self.follow_cursor.get() {
                let caret_top = cl as i32 * line_h;
                if caret_top - sy < 0 {
                    sy = caret_top;
                }
                if caret_top + line_h - sy > inner.h {
                    sy = caret_top + line_h - inner.h;
                }
                self.follow_cursor.set(false);
            }
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
        let first_line_y = if multiline {
            inner.y - sy
        } else {
            inner.y + (inner.h - line_h) / 2
        };
        // 单行对齐偏移：只在整行放得下时才给，放不下就交回水平滚动（见 `TextConfig::align`）。
        // 空串时按 placeholder 宽度算，否则占位符会在居中框里贴左边、上屏第一个字瞬间跳到中间。
        let align_off = if multiline || self.config.align == Align::Start {
            0
        } else {
            let content_w = if is_empty {
                canvas.measure_text(&self.placeholder, ts).w
            } else {
                lay.lines
                    .first()
                    .map_or(0, |ln| ln.x.last().copied().unwrap_or(0))
            };
            let slack = inner.w - content_w;
            match self.config.align {
                Align::Center => (slack / 2).max(0),
                Align::End => slack.max(0),
                _ => 0,
            }
        };
        self.align_off.set(align_off);
        let base_x = inner.x - sx + align_off;

        canvas.save();
        canvas.clip_rect(inner);

        // 选区高亮（逐视觉行；跨行处延伸到行尾标示换行/折行被选中）。
        // `selection()` 给的是**逻辑**索引，而下面按视觉行切分用的是**显示串**索引——
        // 有合成串时两者差一个合成串长度，不换算就会整体偏移。
        if let Some((s, e)) = self
            .selection()
            .map(|(s, e)| (self.to_display_index(s), self.to_display_index(e)))
        {
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
                    let x2 = if cont {
                        ln.x.last().copied().unwrap_or(0) + SEL_EOL_EXTRA
                    } else {
                        ln.x[b - ln.start]
                    };
                    // 铺满整个行盒（不内缩）：对齐系统文本控件——`p`/`{` 等下伸部
                    // 完整包住，且多行选中时行与行之间无横向白缝（行距 == line_h）。
                    canvas.fill_rect(
                        (base_x + x1) as f32,
                        ly as f32,
                        (x2 - x1) as f32,
                        line_h as f32,
                        &Paint::fill(inp.selection(pal)),
                    );
                }
            }
        }

        let chars: Vec<char> = disp.chars().collect();
        if is_empty {
            // 与正文同一 base_x：占位符也吃 align_off，居中框里的占位符才真居中。
            //
            // 宽度分两档：`rect.w` 就是排版引擎的折行宽度，多行必须给 `inner.w`，
            // 否则长占位符不再折行、右边被 `clip_rect(inner)` 直接切掉；单行给
            // `NO_WRAP_W`（同正文），免得长占位符自己折到第二行去。
            let pw = if multiline { inner.w } else { NO_WRAP_W };
            let pr = Rect::new(base_x, first_line_y, pw, line_h);
            canvas.draw_text(
                &self.placeholder,
                pr,
                inp.placeholder(pal),
                Align::Start,
                ts,
            );
        } else {
            for (i, ln) in lay.lines.iter().enumerate() {
                let ly = first_line_y + i as i32 * line_h;
                if ly + line_h < inner.y || ly > inner.y + inner.h {
                    continue;
                }
                if ln.end > ln.start {
                    let s: String = chars[ln.start..ln.end].iter().collect();
                    let tr = Rect::new(base_x, ly, NO_WRAP_W, line_h);
                    canvas.draw_text(&s, tr, text_color, Align::Start, ts);
                }
            }
        }

        // 输入法合成串的下划线：告诉用户「这段还没定下来」。逐视觉行画，长合成串
        // 折行后每段各有一条。画在文字之后、光标之前——压在字身下方，不挡字形。
        if let Some((ps, pe_end)) = self.preedit_span() {
            let pe = self.preedit.borrow();
            let ul_color = inp.preedit_underline(pal);
            // 1dp 实线。`fill_rect` 收**逻辑**坐标（canvas 内部再乘 DPI），故这里就是 1.0；
            // 用 dpi_scale 算会画出 scale 倍粗。低于 1x 的缩放下兜底到 1 物理像素，
            // 否则会被舍成 0、整条下划线消失。
            let thin = 1.0f32.max(1.0 / canvas.dpi_scale());
            // 选中分句（日文分节转换中正在转换的那一段）在**显示串**中的绝对范围。
            let sel_abs = pe.sel.map(|(ss, se)| (ps + ss, ps + se));
            for (i, ln) in lay.lines.iter().enumerate() {
                let ly = first_line_y + i as i32 * line_h;
                if ly + line_h < inner.y || ly > inner.y + inner.h {
                    continue;
                }
                let a = ps.clamp(ln.start, ln.end);
                let b = pe_end.clamp(ln.start, ln.end);
                if b <= a {
                    continue;
                }
                // 按选中分句把本行的合成段切成「细—粗—细」最多三段，各自画。
                // 不能按整行判一个粗细：那样只要分句与本行有交集，整行都会加粗，
                // 分节转换就完全看不出当前在转换哪一段了。
                //
                // 加粗而非背景高亮：背景会与选区高亮撞色，且原生 macOS 也是靠粗细区分。
                let mut segs: [(usize, usize, bool); 3] = [(0, 0, false); 3];
                let n_seg = match sel_abs {
                    Some((gs, ge)) => {
                        let (gs, ge) = (gs.clamp(a, b), ge.clamp(a, b));
                        let mut k = 0;
                        for seg in [(a, gs, false), (gs, ge, true), (ge, b, false)] {
                            if seg.1 > seg.0 {
                                segs[k] = seg;
                                k += 1;
                            }
                        }
                        k
                    }
                    None => {
                        segs[0] = (a, b, false);
                        1
                    }
                };
                for &(s0, s1, bold) in segs.iter().take(n_seg) {
                    let x1 = base_x + ln.x[s0 - ln.start];
                    let x2 = base_x + ln.x[s1 - ln.start];
                    let t = if bold { thin * 2.0 } else { thin };
                    // 底边对齐行盒底（向上加粗），三段的基线因此始终齐平。
                    canvas.fill_rect(
                        x1 as f32,
                        (ly + line_h - 1) as f32 - t,
                        (x2 - x1) as f32,
                        t,
                        &Paint::fill(ul_color),
                    );
                }
            }
        }

        let ly = first_line_y + cl as i32 * line_h;
        let cxx = base_x + cx_in;
        // 记录光标局部位置（相对节点左上角）供输入法候选窗定位。
        // 合成期间 `cl`/`cx_in` 已经是**合成串内**光标的位置，故候选窗自动跟着
        // 合成进度往右走，不会钉在合成开始前的原光标处。
        self.caret_local
            .set(Some((cxx - bounds.x, ly - bounds.y, line_h)));
        // 光标绘制分三档：
        // - 有合成串（macOS 等自绘平台）：**要画**，画在合成串内 `pe.caret` 处。
        //   真机三个输入法对比确认过：不画的话合成期间完全没有插入点，反而不如原生。
        //   位置正确与否取决于输入法通过 `setMarkedText:selectedRange:` 报的编辑点，
        //   个别输入法报得不对——那是它的问题，平台层已做越界兜底（见 macos/window.rs）。
        // - `composing`（win32）：**不画**，系统组合浮层自带一个跟随组合进度的光标，
        //   两者并存会显得我们的光标"卡在组合开始前"。
        // - 常规：照常闪烁。
        let has_preedit = self.preedit.borrow().is_active();
        if focused && (has_preedit || !self.composing.get()) {
            // 反色光标：先铺光标条，再裁到光标矩形、用输入框底色把本行文字重画一遍。
            // 于是压在字形笔画上的那一段翻成浅色（等同经典 XOR 插入符的观感），
            // 光标不会沉进深色文字里认不出位置；笔画之外仍是纯粹的光标条。
            //
            // 之所以用「重画一遍再裁剪」而不是 difference 混合：D2D 后端（本库默认）
            // 的 SetPrimitiveBlend 只有 SourceOver/Copy/Min/Add，真反相要么改走
            // ID2D1Effect 离屏合成、要么每帧 GPU 读回，代价都远超此处所值。
            let mut opts = CaretOpts::from_theme(inp);
            if has_preedit {
                // 合成期间恒实心、不滑行：合成串每按一键就重排，光标位置随之跳变，
                // 闪烁与滑行都会和 IME 候选窗的动效互相干扰；原生 macOS 亦是静态光标。
                // 顺带免掉合成期间的每帧续帧。
                opts.style = CaretStyle::Solid;
                opts.smooth_move = false;
            }
            // `frameless` 时本控件没画底，`bg` 那个变量算的是"假如画了会是什么色"
            // ——真正的底是外层容器画的，两者在自定义主题下并不同色。拿它去反色重绘，
            // 光标处会糊上一小段不属于这里的颜色，故 frameless 直接不做反色。
            let invert_over_glyph = !self.config.frameless;
            if let Some((caret, alpha)) =
                // 竖直只内缩 1px：行盒本就含行距，再各缩 2px 会让光标明显短于字身，
                // 视觉上"矮一截"。
                self.caret
                        .paint(canvas, cxx, ly + 1, line_h - 2, inp.cursor(pal), &opts)
            {
                if let Some(ln) = lay
                    .lines
                    .get(cl)
                    .filter(|ln| invert_over_glyph && ln.end > ln.start)
                {
                    let s: String = chars[ln.start..ln.end].iter().collect();
                    canvas.save();
                    canvas.clip_rect(caret);
                    // 与常规绘制同一 rect/ts，只换颜色：同次排版故字形逐像素对齐。
                    // 取本次实际填的底色（非 inp.bg），背景逻辑再变反色也自动跟随。
                    // 按本帧光标 alpha 同步淡化：光标淡下去时反色也跟着退，否则半透明
                    // 的光标底下会浮着一段永远刷白的字。
                    let tr = Rect::new(base_x, ly, NO_WRAP_W, line_h);
                    canvas.draw_text(&s, tr, bg.scale_alpha(alpha), Align::Start, ts);
                    canvas.restore();
                }
            }
        } else {
            // 失焦/组合态：清掉滑行起点与脏区记录，下次出现按"首次"处理，
            // 不会从上一次的旧位置滑过来。
            self.caret.reset();
        }
        canvas.restore();

        // 垂直滚动条（复用 VScrollbar）：画在 restore() 之后，不受文本 clip_rect 限制。
        if multiline {
            let content_h = lay.lines.len() as i32 * line_h;
            self.scrollbar.paint(canvas, bounds, sy, content_h, inner.h);
        }
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Down => {
                    ctx.request_focus();
                    // 多行：滚动条命中优先于文字交互。右键跳过（不拖滚动条）。
                    if self.is_multiline() && p.button != MouseButton::Right {
                        let b = ctx.bounds();
                        let view_h = b.h - 2 * TEXT_PAD;
                        let (content_h, _) = {
                            let lay = self.layout.borrow();
                            let lh = lay.line_h.max(1);
                            (lay.lines.len() as i32 * lh, lh)
                        };
                        if self.scrollbar.on_down(
                            p.pos,
                            b,
                            self.scroll_y.get(),
                            content_h,
                            view_h,
                            ctx,
                        ) {
                            ctx.mark_dirty();
                            return true;
                        }
                    }
                    // 右键不启动拖选：仅聚焦，并在点击落在选区外时移动光标。
                    if p.button == MouseButton::Right {
                        let idx = self.pos_to_index(ctx, p.pos.x, p.pos.y);
                        let in_sel = self.selection().is_some_and(|(s, e)| idx >= s && idx < e);
                        if !in_sel {
                            self.cursor = idx;
                            self.anchor = None;
                        }
                        let items = self.context_menu_items();
                        ctx.show_context_menu(p.pos, items);
                        return true;
                    }
                    // 到这里才算「用户点了这个输入框」，通知宿主。
                    //
                    // 位置有讲究：拖滚动条与右键都已在上面 return 掉了——前者点的是滚动条
                    // 不是文字，后者要弹上下文菜单，再触发宿主的点击反应会打架。而双击/三击
                    // 在**下面**，故选词、选段同样会通知：用户双击输入框时，那个被收起的
                    // 浮层一样该回来。
                    //
                    // ⚠ 只认左键。`wants_right_click()` 为 true 使得**所有**非左键的 Down
                    // 都会投递进来（派发层的判据是 `button != Left`，见 `core.rs` 的
                    // `secondary`），上面那道关只挡了 Right——中键会一路落到这儿。
                    if p.button == MouseButton::Left {
                        self.fire_click(ctx);
                    }
                    // 双击选词 / 三击选段。不进入拖选。
                    match p.click_count {
                        2 => {
                            let idx = self.pos_to_index(ctx, p.pos.x, p.pos.y);
                            self.select_word(idx);
                            self.dragging = false;
                            self.follow_cursor.set(true);
                            ctx.mark_dirty();
                            return true;
                        }
                        n if n >= 3 => {
                            let idx = self.pos_to_index(ctx, p.pos.x, p.pos.y);
                            self.select_para(idx);
                            self.dragging = false;
                            self.follow_cursor.set(true);
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
                    self.follow_cursor.set(true);
                    ctx.capture();
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Move => {
                    // 滚动条拖动优先。
                    if self.scrollbar.dragging {
                        if let Some(new_sy) = self.scrollbar.on_move(p.pos) {
                            self.scroll_y.set(new_sy);
                            ctx.mark_dirty();
                        }
                        return true;
                    }
                    // 文字拖选（含跨视口自动滚动）。
                    if self.dragging {
                        if self.is_multiline() {
                            let b = ctx.bounds();
                            let inner_h = b.h - 2 * TEXT_PAD;
                            let (content_h, line_h) = {
                                let lay = self.layout.borrow();
                                let lh = lay.line_h.max(1);
                                (lay.lines.len() as i32 * lh, lh)
                            };
                            let max_scroll = (content_h - inner_h).max(0);
                            if max_scroll > 0 {
                                let sy = self.scroll_y.get();
                                let top_edge = b.y + TEXT_PAD;
                                let bot_edge = b.y + b.h - TEXT_PAD;
                                if p.pos.y < top_edge && sy > 0 {
                                    // 鼠标在上边界外：按超出距离比例向上滚（最少 1px，最多一行）。
                                    let step = ((top_edge - p.pos.y) / 5).clamp(1, line_h);
                                    self.scroll_y.set((sy - step).max(0));
                                } else if p.pos.y > bot_edge && sy < max_scroll {
                                    // 鼠标在下边界外：向下滚。
                                    let step = ((p.pos.y - bot_edge) / 5).clamp(1, line_h);
                                    self.scroll_y.set((sy + step).min(max_scroll));
                                }
                            }
                        }
                        // scroll_y 已更新，pos_to_index 内部读最新值，天然指向滚入的行。
                        self.cursor = self.pos_to_index(ctx, p.pos.x, p.pos.y);
                        ctx.mark_dirty();
                        return true;
                    }
                    // 普通悬停：更新滚动条命中标志（驱动光标形状切换）。
                    if self.is_multiline() {
                        let b = ctx.bounds();
                        let in_sb = p.pos.x >= b.right() - VScrollbar::HIT_W;
                        if in_sb != self.hover_in_scrollbar.get() {
                            self.hover_in_scrollbar.set(in_sb);
                            // 不需要 mark_dirty：光标形状由平台 WM_SETCURSOR 独立查询，
                            // 无需触发整帧重绘。
                        }
                    }
                    false
                }
                PointerKind::Leave => {
                    if self.hover_in_scrollbar.get() {
                        self.hover_in_scrollbar.set(false);
                    }
                    false
                }
                PointerKind::Up => {
                    // 释放滚动条拖动。
                    if self.scrollbar.on_up(ctx) {
                        ctx.mark_dirty();
                        return true;
                    }
                    // 释放文字拖选。
                    self.dragging = false;
                    ctx.release_capture();
                    if self.anchor == Some(self.cursor) {
                        self.anchor = None;
                    }
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Wheel(delta) if self.is_multiline() => {
                    // 不设 follow_cursor，避免 paint 把视口重置到光标。
                    let b = ctx.bounds();
                    let inner_h = b.h - 2 * TEXT_PAD;
                    let (content_h, line_h) = {
                        let lay = self.layout.borrow();
                        let lh = lay.line_h.max(1);
                        (lay.lines.len() as i32 * lh, lh)
                    };
                    if content_h <= inner_h {
                        return false; // 无溢出 → 冒泡给外层滚动容器
                    }
                    let max_scroll = (content_h - inner_h).max(0);
                    let sy_old = self.scroll_y.get();
                    // 每刻度 120，滚动约 3 行（与外层 ScrollWidget 步长对齐）。
                    let step = (3 * line_h).max(48);
                    let dy = -delta * step / 120;
                    // 已到边界 → 冒泡让外层继续滚动。
                    let at_boundary = (dy < 0 && sy_old == 0) || (dy > 0 && sy_old >= max_scroll);
                    if at_boundary {
                        return false;
                    }
                    self.scroll_y.set((sy_old + dy).clamp(0, max_scroll));
                    ctx.mark_dirty();
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed => {
                // 任何按键都应将视口滚回光标位置（用户开始编辑/导航）。
                self.follow_cursor.set(true);
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
                    // 多行：Enter 插入换行。单行：交给 `on_submit`，未设则不消费。
                    //
                    // **「不消费」不等于「冒泡」**：`Tree::dispatch_key` 只对焦点节点做
                    // 一次 `call_on_event`，没有 `ancestor_chain` 循环（对比同文件的
                    // `dispatch_files`，那个是真冒泡）。所以未消费的按键是**就地消失**，
                    // 不会传给外层容器——想在外层挂 Widget 接 Enter 的写法编译通过、
                    // 逻辑正确、永远不触发。单行 Enter 的唯一出口是 `on_submit`。
                    Key::Enter if self.is_multiline() => {
                        self.insert_newline(ctx);
                        true
                    }
                    // 多行：上下移动到相邻视觉行。单行：交给 `on_nav_key`（候选列表游标
                    // 等）。同样地，未设回调时事件就地消失而非冒泡，理由见上。
                    Key::Up if self.is_multiline() => {
                        self.move_vertical(ctx, false, k.shift);
                        true
                    }
                    Key::Down if self.is_multiline() => {
                        self.move_vertical(ctx, true, k.shift);
                        true
                    }
                    // 单行的 Enter / 上下键：本控件不处理，交给应用声明的出口。
                    // 放在多行守卫之后，故多行模式永远走不到这里（Enter 换行、上下移行
                    // 是编辑器的固有语义，不该被应用截走）。
                    Key::Enter => self.fire_submit(ctx),
                    // 上下键：单行本控件不用，交给应用。
                    //
                    // Tab：**两种模式都转发**，且默认不消费——宿主的焦点导航是兜底，
                    // 只在这里返回 `false` 时才轮到。查询框把 Tab 用作「接受补全」需要
                    // 它，而没声明 `on_nav_key` 的输入框行为完全不变。
                    Key::Up | Key::Down if !self.is_multiline() => self.fire_nav_key(ctx, *k),
                    // 翻页键两种模式都转发：本控件不做翻页（多行的上下移行只按视觉行走），
                    // 交给应用翻候选页 / 翻文档。
                    Key::Tab | Key::PageUp | Key::PageDown => self.fire_nav_key(ctx, *k),
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
                        let target = if self.is_multiline() {
                            self.cur_line_bounds().0
                        } else {
                            0
                        };
                        self.move_to(ctx, target, k.shift);
                        true
                    }
                    Key::End => {
                        let target = if self.is_multiline() {
                            self.cur_line_bounds().1
                        } else {
                            len
                        };
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
    fn take_click(&mut self, f: ClickFn) {
        self.on_click = Some(f);
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
    fn ime_caret(&self) -> Option<(i32, i32, i32)> {
        self.caret_local.get()
    }
    fn set_composing(&mut self, composing: bool) {
        self.composing.set(composing);
    }
    fn set_preedit(&mut self, pe: &Preedit) {
        // 合成串长度变化会改变文本宽度与换行，`rebuild_layout` 的缓存键第一项就是
        // 显示串，故这里只需换掉数据、下一帧 paint 自动重排，无需手动失效布局。
        *self.preedit.borrow_mut() = pe.clone();
        // 合成开始/结束时把视口拉回光标：长合成串可能把光标推出可视区。
        self.follow_cursor.set(true);
    }
    fn selection_range(&self) -> Option<(usize, usize)> {
        // 无选区时返回光标处的空范围——输入法要的是「插入点在哪」，
        // 返回 None 会被理解成「没有文本上下文」。
        Some(self.selection().unwrap_or((self.cursor, self.cursor)))
    }
    fn ime_text(&self) -> Option<String> {
        // 密码框不把内容交给输入法：这段文字会被 IME 读去做联想与重转换，
        // 密码不该流到那里。
        if self.config.password {
            return None;
        }
        Some(self.text.with(|t| t.clone()))
    }
    fn reset_interaction(&mut self) {
        // 复用同一对话框切换编辑目标时（隐藏→再显示），清掉上一条残留的选区/拖选状态，
        // 光标落到（新填充文本的）文末，避免带着旧选区进入下一次编辑。
        self.anchor = None;
        self.cursor = self.char_count();
        self.dragging = false;
        self.goal_x.set(None);
        self.follow_cursor.set(true);
    }
    fn wants_right_click(&self) -> bool {
        true // 右键弹出上下文菜单（剪切/复制/粘贴/全选）
    }
    fn cursor(&self) -> CursorShape {
        // 悬停在滚动条区域或正在拖动滚动条时，显示普通箭头。
        if self.scrollbar.dragging || self.hover_in_scrollbar.get() {
            CursorShape::Arrow
        } else {
            CursorShape::Text
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{word_run, wrap_paragraph, Preedit, TextInput, TextLayout, VisLine};
    use crate::signal::signal;

    fn run(s: &str, idx: usize) -> (usize, usize) {
        let chars: Vec<char> = s.chars().collect();
        word_run(&chars, idx)
    }

    #[test]
    fn reset_interaction_clears_selection() {
        use crate::core::Widget;
        // 复用同一对话框切换编辑目标：隐藏时框架调 reset_interaction 应清掉残留选区。
        let text = signal(String::from("hello"));
        let mut ti = TextInput::new(text, String::new());
        ti.anchor = Some(0);
        ti.cursor = 5;
        assert!(ti.selection().is_some(), "前置：应存在选区");
        ti.reset_interaction();
        assert_eq!(ti.anchor, None, "复位后不应残留选区锚点");
        assert!(ti.selection().is_none(), "复位后选区应清空");
        assert_eq!(ti.cursor, ti.char_count(), "光标应落到文末");
    }

    // ── on_click：点输入框时通知宿主 ────────────────────────────────────────
    //
    // 补这条能力之前 TextInput 没实现 `Widget::take_click`，于是
    // `Element::text_input(..).on_click(..)` 是**静默无效**的——链上去不报错也不生效。
    // 宿主因此收不到"用户又点了一次输入框"，像「Escape 收起结果浮层后再点输入框、
    // 浮层该回来」这类需求就无处落脚。

    /// 建一棵只含单行输入框的树，输入框占满 160×28 的根节点。
    fn input_tree(el: crate::ui::Element) -> crate::core::Tree {
        let mut tree = crate::core::Tree::new();
        let root = el.width(160).height(28).build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(
            crate::geometry::Size::new(200, 60),
            &mut crate::text::NullTextEngine,
        );
        tree
    }

    /// 往输入框正中派发一次按下。`count` 为连击数（2 = 双击）。
    fn press(tree: &mut crate::core::Tree, count: u8, button: crate::event::MouseButton) {
        use crate::event::{PointerEvent, PointerKind};
        let ev = PointerEvent {
            click_count: count,
            ..PointerEvent::single(
                PointerKind::Down,
                crate::geometry::Point::new(20, 14),
                button,
            )
        };
        tree.dispatch_pointer(ev, &mut None, &mut None);
    }

    #[test]
    fn click_notifies_host() {
        use crate::event::MouseButton;
        let hits = signal(0);
        let mut tree = input_tree(
            crate::ui::Element::text_input(signal(String::from("abc")), "")
                .on_click(move |_| hits.set(hits.get() + 1)),
        );
        press(&mut tree, 1, MouseButton::Left);
        assert_eq!(hits.get(), 1, "左键点输入框应通知宿主一次");
    }

    /// 双击（选词）同样通知：用户双击输入框时，那个被收起的浮层一样该回来。
    #[test]
    fn double_click_also_notifies() {
        use crate::event::MouseButton;
        let hits = signal(0);
        let mut tree = input_tree(
            crate::ui::Element::text_input(signal(String::from("hello world")), "")
                .on_click(move |_| hits.set(hits.get() + 1)),
        );
        press(&mut tree, 2, MouseButton::Left);
        assert_eq!(hits.get(), 1, "双击选词也应通知宿主");
    }

    /// 中键**不**通知。
    ///
    /// `wants_right_click()` 为 true 使得**所有**非左键的 Down 都会投递进来（派发层
    /// 的判据是 `button != Left`），而 Down 分支里只挡了 Right——中键曾一路落到
    /// `fire_click`，与 `Element::on_click` 文档承诺的「右键与拖滚动条不触发」不符。
    #[test]
    fn middle_click_does_not_notify() {
        use crate::event::MouseButton;
        let hits = signal(0);
        let mut tree = input_tree(
            crate::ui::Element::text_input(signal(String::from("abc")), "")
                .on_click(move |_| hits.set(hits.get() + 1)),
        );
        press(&mut tree, 1, MouseButton::Middle);
        assert_eq!(hits.get(), 0, "中键不该触发宿主的点击通知");
    }

    /// 拖多行输入框的滚动条**不**通知：点的是滚动条，不是文字。
    ///
    /// 这是触发点选址的理由之一，得有测试守着——否则日后有人把 `fire_click` 往上挪
    /// 一行，不会有任何测试反对。
    ///
    /// 行布局是**手工塞**的：真实 layout 由 `paint` 构建，而那要拉起画布与字体，
    /// 对一条只关心「命中落进哪个分支」的测试来说依赖太重。
    #[test]
    fn scrollbar_hit_does_not_notify() {
        use crate::core::Widget;
        use crate::event::{Event, MouseButton, PointerEvent, PointerKind};
        let hits = signal(0);
        let mut tree = crate::core::Tree::new();
        let root = crate::ui::Element::leaf()
            .width(160)
            .height(60)
            .build(&mut tree);
        tree.root = Some(root);
        // 视口与元素同尺寸：`ctx.bounds()` 取的是根节点布局后的矩形，视口更大时
        // 根会被拉开，右缘就不在 160 上了——滚动条命中区是按 `bounds.right()` 算的。
        tree.layout_root(
            crate::geometry::Size::new(160, 60),
            &mut crate::text::NullTextEngine,
        );

        let mut ti = TextInput::new(signal(String::from("x")), String::new());
        ti.config_mut().multiline = true;
        ti.take_click(Box::new(move |_| hits.set(hits.get() + 1)));
        {
            // 内容高远超视口 ⇒ 滚动条可见、可拖。
            let mut lay = ti.layout.borrow_mut();
            lay.lines = (0..20)
                .map(|i| VisLine {
                    start: i,
                    end: i + 1,
                    x: vec![0, 8],
                    hard: true,
                })
                .collect();
            lay.line_h = 16;
        }
        // 贴右缘落点，进滚动条命中区。
        let at = crate::geometry::Point::new(160 - 2, 30);
        crate::testing::run_with_ctx_in(&mut tree, root, |ctx| {
            ti.on_event(
                ctx,
                &Event::Pointer(PointerEvent::single(
                    PointerKind::Down,
                    at,
                    MouseButton::Left,
                )),
            );
        });
        assert_eq!(hits.get(), 0, "点滚动条不该触发宿主的点击通知");
    }

    /// 右键**不**通知：它要弹上下文菜单，再触发宿主的点击反应会打架。
    #[test]
    fn right_click_does_not_notify() {
        use crate::event::MouseButton;
        let hits = signal(0);
        let mut tree = input_tree(
            crate::ui::Element::text_input(signal(String::from("abc")), "")
                .on_click(move |_| hits.set(hits.get() + 1)),
        );
        press(&mut tree, 1, MouseButton::Right);
        assert_eq!(hits.get(), 0, "右键弹菜单，不应触发宿主的点击通知");
    }

    /// 从树根取出那个 `TextInput` 的 `(cursor, dragging)`。
    fn input_state(tree: &mut crate::core::Tree) -> (usize, bool) {
        let root = tree.root.expect("树应有根");
        let ti = tree
            .get_mut(root)
            .and_then(|n| n.widget.as_any_mut())
            .and_then(|a| a.downcast_mut::<TextInput>())
            .expect("根应当是 TextInput");
        (ti.cursor, ti.dragging)
    }

    /// **回归**：通知是旁路的，输入框自身的行为不能因此改变。
    ///
    /// 装了回调就不定位光标、或不再进入拖选的话，这个「通知」就成了破坏——
    /// 用户点一下，光标不动。
    ///
    /// （这条一度只比较两个**从未收过任何事件**的 `TextInput`：`cursor` 恒等于文末、
    /// `dragging` 恒为 false，两条断言在任何实现下都成立，等于什么都没测。掏空
    /// `take_click` 时它之所以变红，靠的是末尾那句「回调装上了没」的前置断言——
    /// 而那查的是装配，不是行为。现在两棵树各真派发一次点击再比。）
    #[test]
    fn notify_does_not_disturb_the_input_itself() {
        use crate::event::MouseButton;
        let hits = signal(0);
        let mut plain = input_tree(crate::ui::Element::text_input(
            signal(String::from("hello world")),
            "",
        ));
        let mut noisy = input_tree(
            crate::ui::Element::text_input(signal(String::from("hello world")), "")
                .on_click(move |_| hits.set(hits.get() + 1)),
        );
        press(&mut plain, 1, MouseButton::Left);
        press(&mut noisy, 1, MouseButton::Left);
        assert_eq!(hits.get(), 1, "前置：回调确实被调到了");

        let (plain_cursor, plain_dragging) = input_state(&mut plain);
        let (noisy_cursor, noisy_dragging) = input_state(&mut noisy);
        assert_eq!(plain_cursor, noisy_cursor, "装了回调不该改变光标落点");
        assert_eq!(plain_dragging, noisy_dragging, "装了回调不该改变拖选状态");
        assert!(noisy_dragging, "左键按下应进入拖选（这次点击确实被处理了）");
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
        assert_eq!(
            wrap_paragraph(&chars, 0, 3, &pre, 10, false),
            vec![(0, 3, true)]
        );
        // 空段落：占一视觉行。
        assert_eq!(
            wrap_paragraph(&chars, 3, 3, &[0], 50, true),
            vec![(3, 3, true)]
        );
    }

    fn dummy_input() -> TextInput {
        TextInput::new(signal(String::new()), String::new())
    }

    /// 构造带正文、光标位置与合成串的输入框。
    fn input_with(text: &str, cursor: usize, pe: Preedit) -> TextInput {
        use crate::core::Widget;
        let mut ti = TextInput::new(signal(String::from(text)), String::new());
        ti.cursor = cursor;
        ti.set_preedit(&pe);
        ti
    }

    fn preedit(text: &str, caret: usize) -> Preedit {
        Preedit {
            text: String::from(text),
            caret,
            sel: None,
        }
    }

    /// 合成串插在光标处，且**不改动**正文信号本身。
    #[test]
    fn preedit_inserts_at_cursor_in_display_string() {
        let ti = input_with("ab", 1, preedit("xyz", 0));
        assert_eq!(ti.display_string(), "axyzb", "合成串应插在光标处");
        assert_eq!(
            ti.text.with(|t| t.clone()),
            "ab",
            "合成串不得写进正文——它还没上屏"
        );
        // 合成结束后显示串恢复原样。
        let ti2 = input_with("ab", 1, Preedit::default());
        assert_eq!(ti2.display_string(), "ab");
    }

    /// 密码框不给合成串打码：候选窗就浮在屏幕上，打码没有安全收益，
    /// 只会让用户看不清自己打了什么（原生 NSSecureTextField 同此）。
    #[test]
    fn preedit_not_masked_in_password_field() {
        let mut ti = input_with("ab", 2, preedit("pin", 0));
        ti.config.password = true;
        let disp = ti.display_string();
        assert_eq!(disp, "\u{2022}\u{2022}pin", "正文掩码、合成串明文");
    }

    /// 逻辑索引 ↔ 显示索引的往返一致性。
    ///
    /// 这层映射是本功能最容易静默出错的地方：密码掩码是**等字符数替换**故索引可直接
    /// 复用，合成串是**插入**，字符数变了。错了不会崩，只会让光标与选区偏移几个字符。
    #[test]
    fn preedit_index_mapping_roundtrips() {
        let ti = input_with("abcd", 2, preedit("XY", 0));
        // 光标前（含光标位）不移位。
        assert_eq!(ti.to_display_index(0), 0);
        assert_eq!(ti.to_display_index(2), 2);
        // 光标后整体右移合成串长度。
        assert_eq!(ti.to_display_index(3), 5);
        assert_eq!(ti.to_display_index(4), 6);
        // 往返回到原值。
        for i in 0..=4 {
            assert_eq!(ti.to_logical_index(ti.to_display_index(i)), i, "i={i}");
        }
        // 落在合成串内部的显示索引钳到光标处（合成串不属于正文，无对应逻辑位置）。
        assert_eq!(ti.to_logical_index(3), 2);
        assert_eq!(ti.to_logical_index(4), 2);
        // 无合成时是恒等映射。
        let plain = input_with("abcd", 2, Preedit::default());
        for i in 0..=4 {
            assert_eq!(plain.to_display_index(i), i);
            assert_eq!(plain.to_logical_index(i), i);
        }
    }

    /// 合成串的下划线必须真的画出来，且**长度随合成串增长**。
    ///
    /// 只断言"有墨"会被一个画固定小方块的实现蒙混过去，故取 1/2/4 三档长度看**增量**：
    /// 下划线若按合成串宽度画，墨量应是 `C + n·U` 的形式（C 是与长度无关的固定差异），
    /// 于是 `ink4-ink2` 应约为 `ink2-ink1` 的两倍。比增量而不是比绝对值，是为了消掉 C
    /// ——基线帧画了插入光标而合成帧不画（见绘制处注释），这个固定差异会淹没短下划线。
    ///
    /// 注意本路径下 `SkiaCanvas::new` 不带文本引擎，`draw_text` 是空操作、字形不出墨；
    /// 但 `measure_text` 有估算回退，故**布局与下划线几何是真实的**。这个测试覆盖的是
    /// 下划线几何，字形本身的渲染要靠真机验证。
    #[test]
    fn preedit_underline_scales_with_length() {
        with_caret_style(crate::ui::CaretStyle::Solid);
        let base = caret_frame(&input_with("", 0, Preedit::default()), 0);
        let ink = |s: &str| frame_diff(&caret_frame(&input_with("", 0, preedit(s, 0)), 0), &base);
        let (i1, i2, i4) = (ink("a"), ink("ab"), ink("abcd"));
        let (d1, d2) = (i2 as i64 - i1 as i64, i4 as i64 - i2 as i64);
        assert!(
            d1 > 0,
            "下划线应随合成串变长：1 字符 {i1} vs 2 字符 {i2}（增量 {d1}）"
        );
        assert!(
            d2 > d1,
            "多两个字符的增量应大于多一个字符的：+1 字符 {d1} vs +2 字符 {d2}（相等说明下划线宽度不随长度变）"
        );
        crate::theme::set_current(std::rc::Rc::new(crate::theme::Theme::default()));
    }

    /// 合成期间 `ime_caret()` 报告的位置随 `pe.caret` 右移。
    ///
    /// 合成态本身**不画**插入光标（跟随原生，见绘制处注释），所以这个位置现在的唯一
    /// 用途是给输入法定位候选窗（`firstRectForCharacterRange:`）——它必须跟着合成进度
    /// 走，否则候选窗会钉在合成开始前的原光标处，长拼音串下会压住已输入的内容。
    #[test]
    fn caret_follows_preedit_progress() {
        with_caret_style(crate::ui::CaretStyle::Solid);
        use crate::core::Widget;
        let xs: Vec<i32> = (0..=4)
            .map(|c| {
                let ti = input_with("", 0, preedit("abcd", c));
                caret_frame(&ti, 0); // paint 记录 caret_local
                ti.ime_caret().expect("聚焦文本框应有光标位置").0
            })
            .collect();
        assert!(
            xs.windows(2).all(|w| w[1] > w[0]),
            "合成内光标 x 应随合成进度单调右移，实测 {xs:?}"
        );
        crate::theme::set_current(std::rc::Rc::new(crate::theme::Theme::default()));
    }

    /// 正文为空但正在合成时，显示串必须非空——paint 据此决定画正文还是 placeholder。
    ///
    /// 这里锁的是一个真实踩过的坑：判空看的是**正文**，于是"框里没字、正在打拼音"
    /// 这个最常见的场景走进了 placeholder 分支，合成串一个字都画不出来，
    /// 屏幕上只剩占位符——正是用户报的「看不到输入的编码」。
    #[test]
    fn empty_text_with_preedit_is_not_blank() {
        let ti = input_with("", 0, preedit("zhongwen", 8));
        assert!(
            !ti.display_string().is_empty(),
            "正文空但有合成串时，显示串必须非空，否则会被当成空框画 placeholder"
        );
        assert_eq!(ti.display_string(), "zhongwen");
        // 真的空（无合成串）时才算空。
        let blank = input_with("", 0, Preedit::default());
        assert!(blank.display_string().is_empty());
    }

    /// 选中分句只加粗**那一段**，不是整条下划线。
    ///
    /// 按视觉行判一个粗细的实现会让「有交集就整行加粗」，日文分节转换于是完全
    /// 看不出正在转换哪一段。取三档墨量：全细 < 中间一段加粗 < 全粗。
    #[test]
    fn preedit_bold_underline_covers_only_the_selected_clause() {
        with_caret_style(crate::ui::CaretStyle::Solid);
        let base = caret_frame(&input_with("", 0, Preedit::default()), 0);
        let mut pe_none = preedit("abcdef", 0);
        pe_none.sel = None;
        let mut pe_mid = preedit("abcdef", 0);
        pe_mid.sel = Some((2, 4));
        let mut pe_all = preedit("abcdef", 0);
        pe_all.sel = Some((0, 6));
        let ink = |pe: Preedit| frame_diff(&caret_frame(&input_with("", 0, pe), 0), &base);
        let (thin, mid, all) = (ink(pe_none), ink(pe_mid), ink(pe_all));
        assert!(
            thin < mid,
            "加粗一段应比全细多墨：全细 {thin} vs 中段加粗 {mid}"
        );
        assert!(
            mid < all,
            "只加粗中间一段应比整条加粗少墨：中段 {mid} vs 全粗 {all}（相等说明整行都被加粗了）"
        );
        crate::theme::set_current(std::rc::Rc::new(crate::theme::Theme::default()));
    }

    /// 合成串变化必须触发重排：布局缓存键的第一项就是显示串，漏了会让
    /// 合成串变长时沿用旧的视觉行，光标落点与实际字形对不上。
    #[test]
    fn preedit_change_rebuilds_layout() {
        use crate::core::Widget;
        let mut ti = input_with("", 0, preedit("a", 1));
        let a = caret_frame(&ti, 0);
        ti.set_preedit(&preedit("abcd", 4));
        let b = caret_frame(&ti, 0);
        assert!(
            frame_diff(&a, &b) > 0,
            "合成串加长后画面必须跟着变（布局已重排）"
        );
    }

    /// 在给定帧时钟渲染一次聚焦态空输入框，返回像素缓冲。
    fn caret_frame(ti: &TextInput, now_ms: u64) -> Vec<u8> {
        use crate::core::Widget;
        use crate::geometry::Rect;
        use crate::render::SkiaCanvas;
        use crate::style::Style;
        use tiny_skia::Pixmap;
        let b = Rect::new(0, 0, 200, 30);
        crate::anim::set_clock_ms(now_ms);
        let mut pm = Pixmap::new(200, 30).unwrap();
        {
            let mut c = SkiaCanvas::new(&mut pm);
            ti.paint(b, b, true, true, &mut c, &Style::default());
        }
        pm.data().to_vec()
    }

    /// 两帧间的墨量差（逐通道绝对差之和）。
    ///
    /// 全部取样都在**聚焦帧之间**做，不拿失焦帧当基线：失焦会 `CaretState::reset`，
    /// 下一帧遂被判为"首次出现"而重置闪烁相位——那样量到的永远是实心光标。
    fn frame_diff(a: &[u8], b: &[u8]) -> u32 {
        a.iter()
            .zip(b)
            .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
            .sum()
    }

    /// 临时把当前主题换成指定光标风格（thread_local，用完须还原）。
    fn with_caret_style(style: crate::ui::CaretStyle) {
        let mut t = crate::theme::Theme::default();
        t.input.caret_style = Some(style);
        crate::theme::set_current(std::rc::Rc::new(t));
    }

    /// 闪烁必须真的改变画面，且两种风格的**曲线形状**在像素上可区分：
    /// Smooth 的淡变中段严格落在实心与全灭之间，Blink 只有这两个值。
    /// 只验"有没有画出来"是区分不出渐变与方波的。
    #[test]
    fn caret_blink_modulates_actual_pixels() {
        crate::anim::set_enabled(true);
        crate::ui::caret::set_animated(true);
        crate::ui::caret::set_window_active(true);
        crate::ui::caret::set_blink_period_ms(Some(crate::ui::caret::BLINK_HALF_MS as u32));
        let pause = crate::ui::caret::TYPING_PAUSE_MS;

        with_caret_style(crate::ui::CaretStyle::Smooth);
        let ti = dummy_input();
        // 首帧确立光标位置（同时被记成一次交互，相位自此起算）。
        let solid = caret_frame(&ti, 0);
        let dark = caret_frame(&ti, pause + 530 + 65); // 全灭相位
        let mid = caret_frame(&ti, pause + 130 + 200); // 淡变中段
        let ink_solid = frame_diff(&solid, &dark);
        let ink_mid = frame_diff(&mid, &dark);
        assert!(ink_solid > 0, "实心与全灭必须画面不同（光标在闪）");
        assert!(
            ink_mid > 0 && ink_mid < ink_solid,
            "Smooth 的淡变中段应严格介于实心与全灭之间：mid={ink_mid} solid={ink_solid}"
        );

        // 默认风格：方波，采样点只落在"全亮"或"全灭"两个值上。
        with_caret_style(crate::ui::CaretStyle::Blink);
        let ti = dummy_input();
        let lit = caret_frame(&ti, 0);
        let out = caret_frame(&ti, pause + 530 + 65);
        assert!(frame_diff(&lit, &out) > 0, "方波也必须真的改变画面");
        for t in [pause + 100, pause + 300, pause + 520] {
            assert_eq!(
                frame_diff(&caret_frame(&ti, t), &lit),
                0,
                "亮半周期内 {t} 应与全亮帧逐像素相同（方波无中间态）"
            );
        }
        for t in [pause + 600, pause + 900, pause + 1050] {
            assert_eq!(
                frame_diff(&caret_frame(&ti, t), &out),
                0,
                "灭半周期内 {t} 应与全灭帧逐像素相同"
            );
        }
        crate::theme::set_current(std::rc::Rc::new(crate::theme::Theme::default()));
    }

    /// 光标动画关闭（截图路径、系统省电/无障碍设置）时：画面恒定，且**不请求续帧**。
    /// 后者是省电语义——静止界面必须回到零 CPU 的阻塞空闲。
    #[test]
    fn caret_static_when_animation_off_and_asks_no_frames() {
        crate::anim::set_enabled(true);
        crate::ui::caret::set_animated(false);
        let ti = dummy_input();
        let a = caret_frame(&ti, 0);
        let b = caret_frame(&ti, 5_000);
        assert_eq!(frame_diff(&a, &b), 0, "关闭动画后画面应逐像素恒定");
        crate::anim::reset_request();
        caret_frame(&ti, 9_000);
        assert!(
            !crate::anim::animation_requested(),
            "不闪的光标不应请求续帧"
        );
        // 开着动画时则必须续帧，否则闪一半就冻住。
        crate::ui::caret::set_animated(true);
        crate::anim::reset_request();
        caret_frame(&ti, 9_100);
        assert!(crate::anim::animation_requested(), "闪烁光标应请求续帧");
    }

    /// 窗口失活：画面恒定且**不请求续帧**——切到别的应用后，后台窗口不该继续按刷新率
    /// 出帧。这条是省电的实处：光标是最常见的"永续动画"，不掐这一路，任何留着焦点的
    /// 输入框都会让窗口在后台一直烧 CPU。
    #[test]
    fn inactive_window_asks_no_frames() {
        crate::anim::set_enabled(true);
        crate::ui::caret::set_animated(true);
        crate::ui::caret::set_window_active(false);
        let ti = dummy_input();
        let a = caret_frame(&ti, 0);
        let b = caret_frame(&ti, 5_000);
        assert_eq!(frame_diff(&a, &b), 0, "失活窗口画面应逐像素恒定");
        crate::anim::reset_request();
        caret_frame(&ti, 9_000);
        assert!(!crate::anim::animation_requested(), "失活窗口不应请求续帧");
        // 复位：thread_local 会带到同线程的后续测试。
        crate::ui::caret::set_window_active(true);
        crate::anim::reset_request();
        caret_frame(&ti, 9_100);
        assert!(crate::anim::animation_requested(), "重新激活应恢复续帧");
    }

    #[test]
    fn cursor_line_soft_break_affinity() {
        let ti = dummy_input();
        // 两视觉行 [0,3) 软换行 + [3,6)。
        let lay = TextLayout {
            lines: vec![
                VisLine {
                    start: 0,
                    end: 3,
                    x: vec![0, 10, 20, 30],
                    hard: false,
                },
                VisLine {
                    start: 3,
                    end: 6,
                    x: vec![0, 10, 20, 30],
                    hard: true,
                },
            ],
            line_h: 14,
            key: None,
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
                VisLine {
                    start: 0,
                    end: 1,
                    x: vec![0, 10],
                    hard: true,
                },
                VisLine {
                    start: 2,
                    end: 3,
                    x: vec![0, 10],
                    hard: true,
                },
            ],
            line_h: 14,
            key: None,
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

#[cfg(test)]
mod anim_tests {
    use super::Switch;
    use crate::core::Widget;
    use crate::geometry::Rect;
    use crate::render::SkiaCanvas;
    use crate::signal::signal;
    use crate::style::Style;
    use tiny_skia::Pixmap;

    /// 把 Switch 在给定帧时钟下绘制一帧（触发 retarget-in-paint）。
    fn paint_at(sw: &Switch, clock: u64) {
        crate::anim::set_clock_ms(clock);
        let mut pm = Pixmap::new(60, 30).unwrap();
        let mut c = SkiaCanvas::new(&mut pm);
        sw.paint(
            Rect::new(0, 0, 44, 24),
            Rect::new(0, 0, 44, 24),
            false,
            true,
            &mut c,
            &Style::default(),
        );
    }

    #[test]
    fn pos_initializes_to_state_no_first_frame_anim() {
        // 构造期即按当前状态落定，避免首帧从 0 飞到 1 的突兀动画。
        assert_eq!(Switch::new(signal(true)).pos.get().value(), 1.0);
        assert_eq!(Switch::new(signal(false)).pos.get().value(), 0.0);
    }

    #[test]
    fn retargets_and_settles_when_animated() {
        crate::anim::set_enabled(true);
        let state = signal(false);
        let sw = Switch::new(state);
        paint_at(&sw, 0);
        state.set(true); // 外部切换
        paint_at(&sw, 0); // paint 检测目标变化 → 改向 1
        assert_eq!(sw.pos.get().target(), 1.0, "状态变 on 后 pos 目标应为 1");
        assert!(sw.pos.get().is_active(), "动画开启时应在过渡中");
        paint_at(&sw, 5000); // 远超时长 → 落定
        assert_eq!(sw.pos.get().value(), 1.0);
        assert!(!sw.pos.get().is_active());
    }

    #[test]
    fn snaps_when_animation_disabled() {
        crate::anim::set_enabled(false);
        let state = signal(false);
        let sw = Switch::new(state);
        state.set(true);
        paint_at(&sw, 0);
        assert_eq!(sw.pos.get().value(), 1.0, "关闭动画应瞬时到 on");
        assert!(!sw.pos.get().is_active());
        crate::anim::set_enabled(true);
    }

    #[test]
    fn reopen_after_reset_snaps_without_anim() {
        // 回归：对话框第二次弹出仍应无动画。动画开启下，reset_interaction（隐藏时框架调用）
        // 后即便复显时绑定的新状态与残留值不同，也应瞬时落定、不进入过渡。
        crate::anim::set_enabled(true);
        let state = signal(true);
        let mut sw = Switch::new(state);
        paint_at(&sw, 0); // 首帧 primed，落定在 on(1.0)
        assert_eq!(sw.pos.get().value(), 1.0);
        sw.reset_interaction(); // 模拟对话框隐藏
        state.set(false); // 再打开时绑定另一条 off 记录
        paint_at(&sw, 0);
        assert_eq!(sw.pos.get().value(), 0.0, "复显应瞬时落定到新状态");
        assert!(!sw.pos.get().is_active(), "复显不应进入动画过渡");
    }
}
