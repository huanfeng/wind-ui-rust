//! 数字步进 Stepper：[−] 值 [+]，绑定 `Signal<f64>`，带范围/步长钳制。
//!
//! 自绘单控件：左右各一个按钮区，点击 ∓ 步长（钳制到 [min,max]）；中部显示当前值。
//! 键盘 上/右=增、下/左=减。**点击中部数字区进入编辑模式**，可直接键入数字，
//! Enter 提交（钳制）、Escape 取消；失焦时 paint 检测到 focused=false 自动提交。

use std::cell::{Cell, RefCell};

use crate::anim::{Easing, Transition};
use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;

/// 左右按钮区宽度。
const BTN_W: i32 = 30;

/// 长按首次触发重复前的等待时间（ms）。
const REPEAT_DELAY_MS: u64 = 400;
/// 第一加速阈值：超过此时长后进入中速重复（ms）。
const REPEAT_ACCEL1_MS: u64 = 1000;
/// 第二加速阈值：超过此时长后进入高速重复（ms）。
const REPEAT_ACCEL2_MS: u64 = 2000;
/// 初速重复间隔（ms，elapsed < REPEAT_ACCEL1_MS）。
const REPEAT_INTERVAL_SLOW_MS: u64 = 80;
/// 中速重复间隔（ms，elapsed < REPEAT_ACCEL2_MS）。
const REPEAT_INTERVAL_MID_MS: u64 = 50;
/// 高速重复间隔（ms，elapsed ≥ REPEAT_ACCEL2_MS）。
const REPEAT_INTERVAL_FAST_MS: u64 = 30;

fn repeat_interval_ms(elapsed_ms: u64) -> u64 {
    if elapsed_ms < REPEAT_ACCEL1_MS {
        REPEAT_INTERVAL_SLOW_MS
    } else if elapsed_ms < REPEAT_ACCEL2_MS {
        REPEAT_INTERVAL_MID_MS
    } else {
        REPEAT_INTERVAL_FAST_MS
    }
}

fn hover_amt(cell: &Cell<Transition<f32>>, on: bool) -> f32 {
    let mut tr = cell.get();
    let target = if on { 1.0 } else { 0.0 };
    if tr.target() != target {
        tr.retarget(target, crate::theme::current().anim.fast(), Easing::EaseOut);
    }
    let v = tr.animate();
    cell.set(tr);
    v
}

pub struct Stepper {
    value: Signal<f64>,
    min: f64,
    max: f64,
    step: f64,
    decimals: usize,
    /// 悬停区：-1 无 / 0 减 / 1 加。
    hover: i8,
    hover_l: Cell<Transition<f32>>,
    hover_r: Cell<Transition<f32>>,
    /// 编辑态用 Cell/RefCell，paint(&self) 检测失焦时才能自动提交。
    editing: Cell<bool>,
    edit_buf: RefCell<String>,
    edit_cursor: Cell<usize>,
    /// paint 中记录的光标局部坐标（相对控件左上角），供平台层定位 IME 候选窗。
    caret_local: Cell<Option<(i32, i32, i32)>>,
    /// 长按方向：0=未按 / -1=减 / +1=加。
    press_dir: Cell<i8>,
    /// 按下时的帧时钟（ms），用于计算等待/加速阶段。
    press_start_ms: Cell<u64>,
    /// 上次重复步进的时钟（ms）。
    last_step_ms: Cell<u64>,
}

impl Stepper {
    pub fn new(value: Signal<f64>, min: f64, max: f64, step: f64) -> Self {
        let step = if step.abs() < 1e-12 { 1.0 } else { step.abs() };
        let (min, max) = if min <= max { (min, max) } else { (max, min) };
        let mut decimals = 0;
        let mut s = step;
        while decimals < 6 && (s - s.round()).abs() > 1e-9 {
            s *= 10.0;
            decimals += 1;
        }
        Self {
            value,
            min,
            max,
            step,
            decimals,
            hover: -1,
            hover_l: Cell::new(Transition::new(0.0)),
            hover_r: Cell::new(Transition::new(0.0)),
            editing: Cell::new(false),
            edit_buf: RefCell::new(String::new()),
            edit_cursor: Cell::new(0),
            caret_local: Cell::new(None),
            press_dir: Cell::new(0),
            press_start_ms: Cell::new(0),
            last_step_ms: Cell::new(0),
        }
    }

    fn display(&self) -> String {
        format!("{:.*}", self.decimals, self.value.get())
    }

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

    fn adjust(&self, ctx: &mut EventCtx, steps: f64) {
        let v = (self.value.get() + steps * self.step).clamp(self.min, self.max);
        self.value.set(v);
        ctx.mark_dirty();
    }

    fn start_edit(&self, ctx: &mut EventCtx) {
        let disp = self.display();
        let len = disp.len();
        *self.edit_buf.borrow_mut() = disp;
        self.edit_cursor.set(len);
        self.editing.set(true);
        ctx.mark_dirty();
    }

    /// 提交：解析 edit_buf，合法则钳制写入 value；非法则保留原值。退出编辑态。
    fn commit_edit(&self, ctx: &mut EventCtx) {
        self.do_commit();
        ctx.mark_dirty();
    }

    /// paint 内自动提交（无 ctx）。
    fn do_commit(&self) {
        if let Ok(v) = self.edit_buf.borrow().parse::<f64>() {
            self.value.set(v.clamp(self.min, self.max));
        }
        self.editing.set(false);
        self.caret_local.set(None);
    }

    fn cancel_edit(&self, ctx: &mut EventCtx) {
        self.editing.set(false);
        self.caret_local.set(None);
        ctx.mark_dirty();
    }
}

impl Widget for Stepper {
    fn measure(&self, _avail: Size, style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(120, (style.font_size as i32) + 16)
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
        // 失焦时自动提交（paint 的 focused 参数是感知焦点切换的最早时机）。
        if self.editing.get() && !focused {
            self.do_commit();
        }

        // 长按重复步进（retarget-in-paint 驱动，与 hover 动画同一帧循环）。
        let dir = self.press_dir.get();
        if dir != 0 {
            let now = crate::anim::clock_ms();
            let elapsed = now.saturating_sub(self.press_start_ms.get());
            if elapsed >= REPEAT_DELAY_MS {
                let interval = repeat_interval_ms(elapsed);
                if now.saturating_sub(self.last_step_ms.get()) >= interval {
                    let v = (self.value.get() + dir as f64 * self.step).clamp(self.min, self.max);
                    self.value.set(v);
                    self.last_step_ms.set(now);
                }
            }
            crate::anim::request_repaint();
        }

        let th = crate::theme::current();
        let (pal, st) = (&th.palette, &th.stepper);
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        let corner = th.metrics.corner_md;
        let bg = if enabled { st.bg(pal) } else { pal.surface_alt };
        let value_color = if enabled {
            st.text(pal)
        } else {
            pal.text_disabled
        };

        canvas.fill_round_rect(x, y, w, h, corner, &Paint::fill(bg));
        let border = if focused || self.editing.get() {
            pal.accent
        } else {
            st.border(pal)
        };
        let bw = th.metrics.border_width.to_logical(canvas.dpi_scale());
        canvas.stroke_round_rect(x, y, w, h, corner, bw, &Paint::fill(border));

        let (amt_l, amt_r) = (
            hover_amt(&self.hover_l, enabled && self.hover == 0),
            hover_amt(&self.hover_r, enabled && self.hover == 1),
        );
        if amt_l > 0.0 {
            canvas.fill_round_rect(
                x + 1.0,
                y + 1.0,
                BTN_W as f32 - 2.0,
                h - 2.0,
                corner,
                &Paint::fill(st.button_hover(pal).scale_alpha(amt_l)),
            );
        }
        if amt_r > 0.0 {
            canvas.fill_round_rect(
                x + w - BTN_W as f32 + 1.0,
                y + 1.0,
                BTN_W as f32 - 2.0,
                h - 2.0,
                corner,
                &Paint::fill(st.button_hover(pal).scale_alpha(amt_r)),
            );
        }

        let family = style.font_family.as_deref();
        let fsize = style.font_size;
        let div = Paint::fill(st.border(pal));
        canvas.draw_line(
            (bounds.x + BTN_W) as f32,
            y + 4.0,
            (bounds.x + BTN_W) as f32,
            y + h - 4.0,
            1.0,
            &div,
        );
        canvas.draw_line(
            (bounds.right() - BTN_W) as f32,
            y + 4.0,
            (bounds.right() - BTN_W) as f32,
            y + h - 4.0,
            1.0,
            &div,
        );

        let at_min = self.value.get() <= self.min;
        let at_max = self.value.get() >= self.max;
        let minus_c = if !enabled || at_min {
            pal.text_disabled
        } else {
            st.button(pal)
        };
        let plus_c = if !enabled || at_max {
            pal.text_disabled
        } else {
            st.button(pal)
        };
        let minus_r = Rect::new(bounds.x, bounds.y, BTN_W, bounds.h);
        let plus_r = Rect::new(bounds.right() - BTN_W, bounds.y, BTN_W, bounds.h);
        canvas.draw_text("\u{2212}", minus_r, minus_c, Align::Center, family, fsize);
        canvas.draw_text("+", plus_r, plus_c, Align::Center, family, fsize);

        let mid = Rect::new(
            bounds.x + BTN_W,
            bounds.y,
            (bounds.w - 2 * BTN_W).max(0),
            bounds.h,
        );

        if self.editing.get() {
            let edit_buf = self.edit_buf.borrow();
            // 文字本身不含光标符，避免宽度变化导致居中位移抖动。
            canvas.draw_text(&*edit_buf, mid, value_color, Align::Center, family, fsize);

            // 用 measure_text 算光标 x，然后画竖线（与 TextInput 一致）。
            let full_w = canvas.measure_text(&*edit_buf, family, fsize).w;
            let cursor = self.edit_cursor.get();
            let before_w = canvas.measure_text(&edit_buf[..cursor], family, fsize).w;
            let text_start_x = mid.x + (mid.w - full_w) / 2;
            let cursor_x = text_start_x + before_w;
            canvas.draw_line(
                cursor_x as f32,
                (mid.y + 2) as f32,
                cursor_x as f32,
                (mid.y + mid.h - 2) as f32,
                1.0,
                &Paint::fill(pal.accent),
            );
            // 记录局部坐标供平台层定位 IME 候选窗。
            self.caret_local
                .set(Some((cursor_x - bounds.x, mid.y - bounds.y, mid.h)));
        } else {
            self.caret_local.set(None);
            canvas.draw_text(
                &self.display(),
                mid,
                value_color,
                Align::Center,
                family,
                fsize,
            );
        }
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
                        z @ (0 | 1) => {
                            if self.editing.get() {
                                self.commit_edit(ctx);
                            }
                            let steps = if z == 0 { -1.0 } else { 1.0 };
                            self.adjust(ctx, steps);
                            // 开始长按跟踪。
                            ctx.capture();
                            let dir: i8 = if z == 0 { -1 } else { 1 };
                            self.press_dir.set(dir);
                            let now = crate::anim::clock_ms();
                            self.press_start_ms.set(now);
                            self.last_step_ms.set(now);
                        }
                        _ => {
                            if self.editing.get() {
                                // 编辑中再次点击中部 → 提交
                                self.commit_edit(ctx);
                            } else {
                                self.start_edit(ctx);
                            }
                        }
                    }
                    true
                }
                PointerKind::Up => {
                    if self.press_dir.get() != 0 {
                        self.press_dir.set(0);
                        ctx.release_capture();
                        ctx.mark_dirty();
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed => {
                if self.editing.get() {
                    match k.key {
                        Key::Char(c) => {
                            let can_insert = {
                                let buf = self.edit_buf.borrow();
                                let cursor = self.edit_cursor.get();
                                match c {
                                    '-' => cursor == 0 && !buf.contains('-'),
                                    '.' => !buf.contains('.'),
                                    c if c.is_ascii_digit() => true,
                                    _ => false,
                                }
                            };
                            if can_insert {
                                let cursor = self.edit_cursor.get();
                                self.edit_buf.borrow_mut().insert(cursor, c);
                                self.edit_cursor.set(cursor + 1);
                                ctx.mark_dirty();
                            }
                            true
                        }
                        Key::Backspace => {
                            let cursor = self.edit_cursor.get();
                            if cursor > 0 {
                                self.edit_cursor.set(cursor - 1);
                                self.edit_buf.borrow_mut().remove(cursor - 1);
                                ctx.mark_dirty();
                            }
                            true
                        }
                        Key::Delete => {
                            let cursor = self.edit_cursor.get();
                            if cursor < self.edit_buf.borrow().len() {
                                self.edit_buf.borrow_mut().remove(cursor);
                                ctx.mark_dirty();
                            }
                            true
                        }
                        Key::Left => {
                            let cursor = self.edit_cursor.get();
                            if cursor > 0 {
                                self.edit_cursor.set(cursor - 1);
                                ctx.mark_dirty();
                            }
                            true
                        }
                        Key::Right => {
                            let cursor = self.edit_cursor.get();
                            if cursor < self.edit_buf.borrow().len() {
                                self.edit_cursor.set(cursor + 1);
                                ctx.mark_dirty();
                            }
                            true
                        }
                        Key::Home => {
                            self.edit_cursor.set(0);
                            ctx.mark_dirty();
                            true
                        }
                        Key::End => {
                            let len = self.edit_buf.borrow().len();
                            self.edit_cursor.set(len);
                            ctx.mark_dirty();
                            true
                        }
                        Key::Enter => {
                            self.commit_edit(ctx);
                            true
                        }
                        Key::Escape => {
                            self.cancel_edit(ctx);
                            true
                        }
                        Key::Up => {
                            self.commit_edit(ctx);
                            self.adjust(ctx, 1.0);
                            true
                        }
                        Key::Down => {
                            self.commit_edit(ctx);
                            self.adjust(ctx, -1.0);
                            true
                        }
                        _ => false,
                    }
                } else {
                    match k.key {
                        Key::Down | Key::Left => {
                            self.adjust(ctx, -1.0);
                            true
                        }
                        Key::Up | Key::Right => {
                            self.adjust(ctx, 1.0);
                            true
                        }
                        _ => false,
                    }
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
