//! 颜色选择器 ColorPicker：色块触发器 + 锚定下拉面板。
//!
//! 面板由四件自绘控件组成——SV 方块（饱和度 × 明度）、色相条、透明度条、预设色块，
//! 外加一个复用 [`TextInput`](crate::ui::TextInput) 的 HEX 输入框。整块面板挂在触发器的
//! [`Element::popup`](crate::ui::Element::popup) 上，由核心负责浮在窗口内容之上、
//! 点外面/按 ESC 收起（见 [`Node::overlay`](crate::core::Node::overlay)）。
//!
//! # 为什么内部存 HSV 而不是每次从 RGB 反算
//!
//! RGB→HSV **不是单射**：饱和度归零时（纯灰）色相信息整个消失，明度归零时（纯黑）
//! 色相与饱和度一起消失。若每帧从绑定的 [`Color`] 反算，把明度拖到底再拖回来，色相
//! 会莫名其妙地跳到红色——用户明明只动了明度。故本控件把 HSVA 作为**权威状态**存着，
//! 只在颜色**从外部**被改动时才回算一次（见 [`PickerState::sync_from_value`]）。

use std::any::Any;

use crate::core::{EventCtx, Widget};
use crate::event::{CursorShape, Event, Key, PointerKind};
use crate::geometry::{Color, Rect, Size};
use crate::render::{Canvas, Gradient, Paint};
use crate::signal::Signal;
use crate::style::Style;
use crate::text::TextEngine;

/// 面板内各条控件的默认几何（逻辑 px）。
pub(crate) const SV_HEIGHT: i32 = 130;
pub(crate) const BAR_HEIGHT: i32 = 14;
pub(crate) const SWATCH_SIZE: i32 = 20;
/// 拖拽手柄半径（SV 方块上的圆环）。
const HANDLE_R: f32 = 6.0;
/// 透明度条棋盘格边长。
const CHECKER: f32 = 6.0;
/// 键盘微调步长。
const KEY_STEP_SV: f32 = 0.02;
const KEY_STEP_HUE: f32 = 3.0;

// ---------------------------------------------------------------- HSVA

/// HSVA 颜色：`h` 色相 0..360，`s` 饱和度、`v` 明度、`a` 不透明度均为 0..=1。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hsva {
    pub h: f32,
    pub s: f32,
    pub v: f32,
    pub a: f32,
}

impl Default for Hsva {
    fn default() -> Self {
        Self {
            h: 0.0,
            s: 0.0,
            v: 0.0,
            a: 1.0,
        }
    }
}

impl Hsva {
    pub fn new(h: f32, s: f32, v: f32, a: f32) -> Self {
        Self {
            h: h.rem_euclid(360.0),
            s: s.clamp(0.0, 1.0),
            v: v.clamp(0.0, 1.0),
            a: a.clamp(0.0, 1.0),
        }
    }

    /// 转 RGBA。
    pub fn to_color(self) -> Color {
        let (h, s, v) = (
            self.h.rem_euclid(360.0),
            self.s.clamp(0.0, 1.0),
            self.v.clamp(0.0, 1.0),
        );
        let c = v * s;
        let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
        let m = v - c;
        let (r, g, b) = match (h / 60.0) as i32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let q = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        Color::rgba(
            q(r),
            q(g),
            q(b),
            (self.a.clamp(0.0, 1.0) * 255.0).round() as u8,
        )
    }

    /// 从 RGBA 反算。**色相在灰与黑上会退化**（返回 0），需要保留原色相时请用
    /// [`Hsva::from_color_keeping`]。
    pub fn from_color(c: Color) -> Self {
        let (r, g, b) = (c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0);
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        let h = if d <= f32::EPSILON {
            0.0
        } else if max == r {
            60.0 * (((g - b) / d) % 6.0)
        } else if max == g {
            60.0 * ((b - r) / d + 2.0)
        } else {
            60.0 * ((r - g) / d + 4.0)
        };
        Self {
            h: h.rem_euclid(360.0),
            s: if max <= f32::EPSILON { 0.0 } else { d / max },
            v: max,
            a: c.a as f32 / 255.0,
        }
    }

    /// 从 RGBA 反算，但在信息退化处沿用 `prev` 的值。
    ///
    /// 这是面板"手感"的关键一步：把明度拖到 0（纯黑）时 RGB 里已经不存在色相与饱和度，
    /// 老老实实反算会得到 (0°, 0)，色相游标于是跳回最左的红色；用户再把明度拖回来，
    /// 拿到的却是红色而不是他刚才选的蓝色。
    pub fn from_color_keeping(c: Color, prev: Hsva) -> Self {
        let mut n = Self::from_color(c);
        if n.v <= f32::EPSILON {
            // 纯黑：色相与饱和度都无从谈起，两个都沿用。
            n.h = prev.h;
            n.s = prev.s;
        } else if n.s <= f32::EPSILON {
            // 纯灰：只有色相退化了。
            n.h = prev.h;
        }
        n
    }

    /// 本色相的纯色（s=v=1，不透明）：SV 方块的底色。
    pub fn hue_color(self) -> Color {
        Hsva::new(self.h, 1.0, 1.0, 1.0).to_color()
    }
}

// ---------------------------------------------------------------- 共享状态

/// 面板内各控件共享的状态句柄（[`Signal`] 是 Copy 句柄，随便克隆进闭包）。
#[derive(Clone, Copy)]
pub(crate) struct PickerState {
    /// 调用方绑定的颜色。
    pub value: Signal<Color>,
    /// 权威 HSVA 状态（见模块文档）。
    pub hsva: Signal<Hsva>,
    /// 最后一次由本控件写进 `value` 的颜色。用来分辨"外部改了颜色"与"我自己刚写的"——
    /// 少了它，每帧回算都会把用户正在拖的色相打回原形。
    pub echo: Signal<Color>,
    /// 是否启用透明度（false 时输出恒为不透明）。
    pub with_alpha: bool,
}

impl PickerState {
    /// 写入新的 HSVA 并同步到绑定颜色。
    pub fn commit(&self, hsva: Hsva) {
        let hsva = if self.with_alpha {
            hsva
        } else {
            Hsva { a: 1.0, ..hsva }
        };
        self.hsva.set(hsva);
        let c = hsva.to_color();
        self.echo.set(c);
        self.value.set(c);
    }

    /// 直接写入一个 RGBA（预设色块、HEX 输入用）。
    pub fn commit_color(&self, c: Color) {
        let c = if self.with_alpha {
            c
        } else {
            Color { a: 255, ..c }
        };
        self.hsva.set(Hsva::from_color_keeping(c, self.hsva.get()));
        self.echo.set(c);
        self.value.set(c);
    }

    /// 若颜色被**外部**改动（不是本控件写的），把 HSVA 回算过来。返回是否真的变了。
    pub fn sync_from_value(&self) -> bool {
        let c = self.value.get();
        if c == self.echo.get() {
            return false;
        }
        self.echo.set(c);
        self.hsva.set(Hsva::from_color_keeping(c, self.hsva.get()));
        true
    }
}

// ---------------------------------------------------------------- 绘制小工具

/// 棋盘格底：透明色的通用画法。
///
/// 只把**深格**裁到左右各内缩 `radius` 的范围内，浅格铺满整块圆角矩形。这样四个圆角
/// 落在浅色上，看起来仍是一格浅棋盘格，而不会有深色方块的直角从圆角里探出来——
/// 画布只有矩形裁剪（[`Canvas::clip_rect`]），没有圆角裁剪可用。
fn paint_checkerboard(canvas: &mut dyn Canvas, r: Rect, radius: f32, light: Color, dark: Color) {
    let (x, y, w, h) = (r.x as f32, r.y as f32, r.w as f32, r.h as f32);
    canvas.fill_round_rect(x, y, w, h, radius, &Paint::fill(light));
    canvas.save();
    canvas.clip_rect(Rect::new(
        r.x + radius.ceil() as i32,
        r.y,
        (r.w - 2 * radius.ceil() as i32).max(0),
        r.h,
    ));
    let cols = (w / CHECKER).ceil() as i32;
    let rows = (h / CHECKER).ceil() as i32;
    for row in 0..rows {
        for col in 0..cols {
            if (row + col) % 2 == 0 {
                continue;
            }
            let cx = x + col as f32 * CHECKER;
            let cy = y + row as f32 * CHECKER;
            let cw = CHECKER.min(x + w - cx);
            let ch = CHECKER.min(y + h - cy);
            canvas.fill_rect(cx, cy, cw, ch, &Paint::fill(dark));
        }
    }
    canvas.restore();
}

/// 条形控件（色相/透明度）上的游标：一枚跨越整条高度的白色圆角滑块 + 深色描边。
fn paint_bar_handle(canvas: &mut dyn Canvas, bar: Rect, t: f32, ring: Color, shade: Color) {
    let w = 6.0;
    let x = bar.x as f32 + (bar.w as f32 - w) * t.clamp(0.0, 1.0);
    let y = bar.y as f32 - 2.0;
    let h = bar.h as f32 + 4.0;
    canvas.fill_round_rect(x, y, w, h, 3.0, &Paint::fill(ring));
    canvas.stroke_round_rect(x, y, w, h, 3.0, 1.0, &Paint::fill(shade));
}

/// 取当前主题。`current()` 返回 `Rc<Theme>`，只是加一次引用计数；调用方按
/// `let th = cp_theme(); let (pal, cp) = (&th.palette, &th.color_picker);` 取用，
/// 与全库其余控件同一形态（此前这里克隆了整个 `Palette`）。
fn cp_theme() -> std::rc::Rc<crate::theme::Theme> {
    crate::theme::current()
}

// ---------------------------------------------------------------- SV 方块

/// 饱和度（横）× 明度（纵）二维取色区。
///
/// 面板内部件，只由 `Element::color_picker*` 组装——它唯一的构造器要一份
/// `PickerState`（面板内共享状态），而那是 crate 私有的，故本类型也保持 crate 私有：
/// 对外露一个连方法都调不了的类型名只是噪声。
pub(crate) struct SvArea {
    st: PickerState,
    dragging: bool,
}

impl SvArea {
    pub(crate) fn new(st: PickerState) -> Self {
        Self {
            st,
            dragging: false,
        }
    }

    fn set_from_pos(&self, ctx: &mut EventCtx, x: i32, y: i32) {
        let b = ctx.bounds();
        let s = if b.w > 1 {
            (x - b.x) as f32 / (b.w - 1) as f32
        } else {
            0.0
        };
        let v = if b.h > 1 {
            1.0 - (y - b.y) as f32 / (b.h - 1) as f32
        } else {
            0.0
        };
        let cur = self.st.hsva.get();
        self.st.commit(Hsva::new(cur.h, s, v, cur.a));
        ctx.mark_dirty();
    }
}

impl Widget for SvArea {
    fn measure(&self, avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(avail.w.max(1), SV_HEIGHT)
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
        let th = cp_theme();
        let (pal, cp) = (&th.palette, &th.color_picker);
        let hsva = self.st.hsva.get();
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        let radius = cp.corner(pal);

        // 三层叠加就是标准的 SV 方块：纯色相打底 → 横向白色渐隐（饱和度）→
        // 纵向黑色渐入（明度）。逐像素算一遍也能出同样的图，但那是每帧几万次
        // HSV→RGB；交给渐变是一次提交的事。
        let base = if enabled { hsva.hue_color() } else { pal.track };
        canvas.fill_round_rect(x, y, w, h, radius, &Paint::fill(base));
        canvas.fill_round_rect(
            x,
            y,
            w,
            h,
            radius,
            &Paint::gradient(Gradient::linear(
                (0.0, 0.0),
                (1.0, 0.0),
                vec![
                    (0.0, Color::rgba(255, 255, 255, 255)),
                    (1.0, Color::rgba(255, 255, 255, 0)),
                ],
            )),
        );
        canvas.fill_round_rect(
            x,
            y,
            w,
            h,
            radius,
            &Paint::gradient(Gradient::linear(
                (0.0, 0.0),
                (0.0, 1.0),
                vec![
                    (0.0, Color::rgba(0, 0, 0, 0)),
                    (1.0, Color::rgba(0, 0, 0, 255)),
                ],
            )),
        );
        canvas.stroke_round_rect(x, y, w, h, radius, 1.0, &Paint::fill(cp.border(pal)));

        if !enabled {
            return;
        }
        // 手柄：双环（外白内深）保证在任何底色上都看得见——单色环压到同色区就消失了。
        let hx = x + (w - 1.0) * hsva.s.clamp(0.0, 1.0);
        let hy = y + (h - 1.0) * (1.0 - hsva.v.clamp(0.0, 1.0));
        canvas.fill_circle(hx, hy, HANDLE_R, &Paint::fill(cp.handle(pal)));
        canvas.fill_circle(hx, hy, HANDLE_R - 1.5, &Paint::fill(hsva.to_color()));
        canvas.stroke_round_rect(
            hx - HANDLE_R,
            hy - HANDLE_R,
            HANDLE_R * 2.0,
            HANDLE_R * 2.0,
            HANDLE_R,
            1.0,
            &Paint::fill(cp.handle_shade(pal)),
        );
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Down => {
                    ctx.request_focus();
                    ctx.capture();
                    self.dragging = true;
                    self.set_from_pos(ctx, p.pos.x, p.pos.y);
                    true
                }
                PointerKind::Move if self.dragging => {
                    self.set_from_pos(ctx, p.pos.x, p.pos.y);
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
                let c = self.st.hsva.get();
                let (ds, dv) = match k.key {
                    Key::Left => (-KEY_STEP_SV, 0.0),
                    Key::Right => (KEY_STEP_SV, 0.0),
                    Key::Up => (0.0, KEY_STEP_SV),
                    Key::Down => (0.0, -KEY_STEP_SV),
                    _ => return false,
                };
                self.st.commit(Hsva::new(c.h, c.s + ds, c.v + dv, c.a));
                ctx.mark_dirty();
                true
            }
            _ => false,
        }
    }

    /// 浮层收起时清掉拖拽态。面板可以在**按住不放**的时候消失（ESC、或调用方把
    /// `open` 置 false），那样 `Up` 永远不会来，`dragging` 就冻在 true 上——下次
    /// 展开后鼠标一划过就当成在拖，颜色被平白改掉。
    fn reset_interaction(&mut self) {
        self.dragging = false;
    }
    fn focusable(&self) -> bool {
        true
    }
    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

// ---------------------------------------------------------------- 色相条

/// 色相长条（0..360°）。
///
/// 面板内部件，只由 `Element::color_picker*` 组装——它唯一的构造器要一份
/// `PickerState`（面板内共享状态），而那是 crate 私有的，故本类型也保持 crate 私有：
/// 对外露一个连方法都调不了的类型名只是噪声。
pub(crate) struct HueBar {
    st: PickerState,
    dragging: bool,
}

impl HueBar {
    pub(crate) fn new(st: PickerState) -> Self {
        Self {
            st,
            dragging: false,
        }
    }

    fn set_from_pos(&self, ctx: &mut EventCtx, x: i32) {
        let b = ctx.bounds();
        let t = if b.w > 1 {
            ((x - b.x) as f32 / (b.w - 1) as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let c = self.st.hsva.get();
        self.st.commit(Hsva::new(t * 360.0, c.s, c.v, c.a));
        ctx.mark_dirty();
    }
}

impl Widget for HueBar {
    fn measure(&self, avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(avail.w.max(1), BAR_HEIGHT)
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
        let th = cp_theme();
        let (pal, cp) = (&th.palette, &th.color_picker);
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        let radius = h / 2.0;
        if !enabled {
            canvas.fill_round_rect(x, y, w, h, radius, &Paint::fill(pal.track));
            return;
        }
        // 六个 60° 顶点 + 收尾的红：色相环本就是分段线性的，七个色标即精确还原，
        // 不需要更细的采样。
        let stops = (0..=6)
            .map(|i| {
                (
                    i as f32 / 6.0,
                    Hsva::new(i as f32 * 60.0, 1.0, 1.0, 1.0).to_color(),
                )
            })
            .collect::<Vec<_>>();
        canvas.fill_round_rect(
            x,
            y,
            w,
            h,
            radius,
            &Paint::gradient(Gradient::linear((0.0, 0.0), (1.0, 0.0), stops)),
        );
        canvas.stroke_round_rect(x, y, w, h, radius, 1.0, &Paint::fill(cp.border(pal)));
        paint_bar_handle(
            canvas,
            bounds,
            self.st.hsva.get().h / 360.0,
            cp.handle(pal),
            cp.handle_shade(pal),
        );
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
                let c = self.st.hsva.get();
                let d = match k.key {
                    Key::Left => -KEY_STEP_HUE,
                    Key::Right => KEY_STEP_HUE,
                    _ => return false,
                };
                self.st.commit(Hsva::new(c.h + d, c.s, c.v, c.a));
                ctx.mark_dirty();
                true
            }
            _ => false,
        }
    }

    /// 浮层收起时清掉拖拽态。面板可以在**按住不放**的时候消失（ESC、或调用方把
    /// `open` 置 false），那样 `Up` 永远不会来，`dragging` 就冻在 true 上——下次
    /// 展开后鼠标一划过就当成在拖，颜色被平白改掉。
    fn reset_interaction(&mut self) {
        self.dragging = false;
    }
    fn focusable(&self) -> bool {
        true
    }
    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

// ---------------------------------------------------------------- 透明度条

/// 透明度长条（0..1），棋盘格底 + 当前色的透明渐变。
///
/// 面板内部件，只由 `Element::color_picker*` 组装——它唯一的构造器要一份
/// `PickerState`（面板内共享状态），而那是 crate 私有的，故本类型也保持 crate 私有：
/// 对外露一个连方法都调不了的类型名只是噪声。
pub(crate) struct AlphaBar {
    st: PickerState,
    dragging: bool,
}

impl AlphaBar {
    pub(crate) fn new(st: PickerState) -> Self {
        Self {
            st,
            dragging: false,
        }
    }

    fn set_from_pos(&self, ctx: &mut EventCtx, x: i32) {
        let b = ctx.bounds();
        let t = if b.w > 1 {
            ((x - b.x) as f32 / (b.w - 1) as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let c = self.st.hsva.get();
        self.st.commit(Hsva::new(c.h, c.s, c.v, t));
        ctx.mark_dirty();
    }
}

impl Widget for AlphaBar {
    fn measure(&self, avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(avail.w.max(1), BAR_HEIGHT)
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
        let th = cp_theme();
        let (pal, cp) = (&th.palette, &th.color_picker);
        let hsva = self.st.hsva.get();
        let radius = bounds.h as f32 / 2.0;
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        if !enabled {
            canvas.fill_round_rect(x, y, w, h, radius, &Paint::fill(pal.track));
            return;
        }
        paint_checkerboard(
            canvas,
            bounds,
            radius,
            cp.checker_light(pal),
            cp.checker_dark(pal),
        );
        let opaque = Hsva { a: 1.0, ..hsva }.to_color();
        canvas.fill_round_rect(
            x,
            y,
            w,
            h,
            radius,
            &Paint::gradient(Gradient::linear(
                (0.0, 0.0),
                (1.0, 0.0),
                vec![(0.0, Color { a: 0, ..opaque }), (1.0, opaque)],
            )),
        );
        canvas.stroke_round_rect(x, y, w, h, radius, 1.0, &Paint::fill(cp.border(pal)));
        paint_bar_handle(canvas, bounds, hsva.a, cp.handle(pal), cp.handle_shade(pal));
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
                let c = self.st.hsva.get();
                let d = match k.key {
                    Key::Left => -0.05,
                    Key::Right => 0.05,
                    _ => return false,
                };
                self.st.commit(Hsva::new(c.h, c.s, c.v, c.a + d));
                ctx.mark_dirty();
                true
            }
            _ => false,
        }
    }

    /// 浮层收起时清掉拖拽态。面板可以在**按住不放**的时候消失（ESC、或调用方把
    /// `open` 置 false），那样 `Up` 永远不会来，`dragging` 就冻在 true 上——下次
    /// 展开后鼠标一划过就当成在拖，颜色被平白改掉。
    fn reset_interaction(&mut self) {
        self.dragging = false;
    }
    fn focusable(&self) -> bool {
        true
    }
    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

// ---------------------------------------------------------------- 预设色块

/// 预设色块：点击即把该色写入绑定值。选中态描一圈强调色。
///
/// 面板内部件，只由 `Element::color_picker*` 组装——它唯一的构造器要一份
/// `PickerState`（面板内共享状态），而那是 crate 私有的，故本类型也保持 crate 私有：
/// 对外露一个连方法都调不了的类型名只是噪声。
pub(crate) struct PresetSwatch {
    st: PickerState,
    color: Color,
    hovered: bool,
}

impl PresetSwatch {
    pub(crate) fn new(st: PickerState, color: Color) -> Self {
        Self {
            st,
            color,
            hovered: false,
        }
    }

    /// 忽略 alpha 比较：预设格给的是**色相**建议，用户已调好的透明度不该被一次
    /// 换色悄悄重置，选中判定自然也只看 RGB。
    fn selected(&self) -> bool {
        let c = self.st.value.get();
        (c.r, c.g, c.b) == (self.color.r, self.color.g, self.color.b)
    }
}

impl Widget for PresetSwatch {
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(SWATCH_SIZE, SWATCH_SIZE)
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
        let th = cp_theme();
        let (pal, cp) = (&th.palette, &th.color_picker);
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        let radius = cp.corner(pal);
        let fill = if enabled { self.color } else { pal.track };
        if fill.a < 255 {
            paint_checkerboard(
                canvas,
                bounds,
                radius,
                cp.checker_light(pal),
                cp.checker_dark(pal),
            );
        }
        canvas.fill_round_rect(x, y, w, h, radius, &Paint::fill(fill));
        let (ring, width) = if self.selected() {
            (pal.accent, 2.0)
        } else if self.hovered && enabled {
            (pal.text_muted, 1.0)
        } else {
            (cp.border(pal), 1.0)
        };
        canvas.stroke_round_rect(x, y, w, h, radius, width, &Paint::fill(ring));
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    self.hovered = true;
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Leave => {
                    self.hovered = false;
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Down => {
                    // 与面板内其余控件一致地接管焦点：本控件 focusable() 为真且响应
                    // Space/Enter，鼠标点过却不拿焦点的话，接下来敲空格会落到别的节点上。
                    ctx.request_focus();
                    // 保留当前 alpha：预设是色相建议，不是"连透明度一起替换"。
                    let a = self.st.value.get().a;
                    self.st.commit_color(Color { a, ..self.color });
                    ctx.mark_dirty();
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed && matches!(k.key, Key::Space | Key::Enter) => {
                let a = self.st.value.get().a;
                self.st.commit_color(Color { a, ..self.color });
                ctx.mark_dirty();
                true
            }
            _ => false,
        }
    }

    /// 浮层收起时清掉悬停态：鼠标停在某个色块上按 ESC 收起，指针不会再经过这里，
    /// `Leave` 也就永远不来——下次展开时那枚色块仍描着 hover 环（见
    /// `Tree::reset_hidden_interactions` 的文档）。
    fn reset_interaction(&mut self) {
        self.hovered = false;
    }

    fn focusable(&self) -> bool {
        true
    }
    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
}

// ---------------------------------------------------------------- 触发器

/// 触发器：一枚当前色的色块（+ 可选 HEX 文本 + 展开箭头），点击开合浮层面板。
///
/// 它同时是整个取色器的**同步中枢**：本节点常驻（面板只在展开时存在），
/// [`Widget::on_update`] 里把外部改动回填进 HSVA、把 HEX 输入框的文本双向对齐。
pub struct ColorTrigger {
    st: PickerState,
    open: Signal<bool>,
    /// HEX 输入框绑定的文本（无输入框时为 None）。
    hex: Option<Signal<String>>,
    /// 最后一次由本控件写进 `hex` 的文本，用来分辨"用户在打字"与"我自己刚写的"。
    hex_echo: Option<Signal<String>>,
    show_text: bool,
    hovered: bool,
}

impl ColorTrigger {
    pub(crate) fn new(
        st: PickerState,
        open: Signal<bool>,
        hex: Option<Signal<String>>,
        hex_echo: Option<Signal<String>>,
        show_text: bool,
    ) -> Self {
        Self {
            st,
            open,
            hex,
            hex_echo,
            show_text,
            hovered: false,
        }
    }

    fn toggle(&self, ctx: &mut EventCtx) {
        self.open.set(!self.open.get());
        // 开合改变的是浮层的显隐，局部脏区盖不住它腾出/占用的那片区域。
        ctx.mark_dirty_all();
    }
}

impl Widget for ColorTrigger {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let h = (style.font_size as i32 + 14).max(28);
        if !self.show_text {
            return Size::new(h + 12, h);
        }
        let w = text
            .measure("#RRGGBBAA", &crate::text::TextStyle::of(style), None)
            .w;
        Size::new(h + 10 + w + 22, h)
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
        let th = cp_theme();
        let (pal, cp) = (&th.palette, &th.color_picker);
        let radius = cp.corner(pal);
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        // 外框：与输入框同一套视觉，让它在表单里跟文本框、下拉框排得齐。
        canvas.fill_round_rect(x, y, w, h, radius, &Paint::fill(cp.bg(pal)));
        let border = if !enabled {
            pal.border
        } else if self.open.get() {
            pal.accent
        } else if self.hovered {
            pal.text_muted
        } else {
            cp.border(pal)
        };
        canvas.stroke_round_rect(x, y, w, h, radius, 1.0, &Paint::fill(border));

        // 色块
        let pad = 5.0;
        let chip = Rect::new(
            bounds.x + pad as i32,
            bounds.y + pad as i32,
            bounds.h - 2 * pad as i32,
            bounds.h - 2 * pad as i32,
        );
        let c = if enabled {
            self.st.value.get()
        } else {
            pal.track
        };
        if c.a < 255 {
            paint_checkerboard(
                canvas,
                chip,
                radius,
                cp.checker_light(pal),
                cp.checker_dark(pal),
            );
        }
        canvas.fill_round_rect(
            chip.x as f32,
            chip.y as f32,
            chip.w as f32,
            chip.h as f32,
            radius,
            &Paint::fill(c),
        );
        canvas.stroke_round_rect(
            chip.x as f32,
            chip.y as f32,
            chip.w as f32,
            chip.h as f32,
            radius,
            1.0,
            &Paint::fill(cp.border(pal)),
        );

        if !self.show_text {
            return;
        }
        let text_color = if enabled { pal.text } else { pal.text_disabled };
        let tx = chip.right() + 8;
        canvas.draw_text(
            &c.to_hex_string(),
            Rect::new(tx, bounds.y, (bounds.right() - 20 - tx).max(0), bounds.h),
            text_color,
            crate::spec::Align::Start,
            &crate::text::TextStyle::of(style),
        );
        // 展开箭头：与 Dropdown 同形，暗示"点这里会出来一层"。
        let cx = bounds.right() as f32 - 12.0;
        let cy = bounds.y as f32 + h / 2.0;
        let chevron = if enabled {
            pal.text_muted
        } else {
            pal.text_disabled
        };
        canvas.draw_polyline(
            &[(cx - 4.0, cy - 2.0), (cx, cy + 2.0), (cx + 4.0, cy - 2.0)],
            1.5,
            &Paint::fill(chevron),
        );
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    self.hovered = true;
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Leave => {
                    self.hovered = false;
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Down => {
                    ctx.request_focus();
                    self.toggle(ctx);
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed && matches!(k.key, Key::Space | Key::Enter) => {
                self.toggle(ctx);
                true
            }
            _ => false,
        }
    }

    /// 每帧对齐三份状态：绑定颜色、HSVA、HEX 文本。
    ///
    /// 三者两两之间都可能被单方面改动，逐对写死会绕成环（改 A 触发写 B、写 B 又触发
    /// 写 A）。这里靠两个 echo 值断环——只有当某一侧与"我上次写出去的值"不同，才认为
    /// 那是**别人**改的，需要传播。
    fn on_update(&mut self, _ctx: &mut EventCtx) {
        // 先看 HEX 输入框：用户正在打字时，它的优先级最高。
        if let (Some(hex), Some(echo)) = (self.hex, self.hex_echo) {
            let text = hex.get();
            if text != echo.get() {
                echo.set(text.clone());
                if let Some(c) = Color::from_hex_str(&text) {
                    self.st.commit_color(c);
                }
                return;
            }
        }
        // 其次看颜色是否被外部改动：改了就把 HSVA 回填过来。
        self.st.sync_from_value();
        // 最后**无条件**把当前颜色补进 HEX 框。
        //
        // 这一步不能挂在「外部改动」那个条件里：拖 SV/色相/透明度条与点预设走的是
        // `commit*`，它们在写 `value` 的同时也写了 `echo`，`sync_from_value` 因此恒
        // 返回 false——HEX 框会一直停在打开面板时的旧值，而触发器上那串 HEX 是每帧
        // 现算的、照常在变，同一个面板里两处 HEX 对不上。
        //
        // 走到这里说明没有待处理的用户输入（打字分支已提前 return），不会跟正在输入
        // 的文本打架；`push_hex` 自带「值没变就不写」的守卫，也不会每帧弄脏。
        self.push_hex();
    }

    /// 触发器自身被隐藏时（整页切走、所在对话框关闭）清掉悬停态，理由同其余控件。
    fn reset_interaction(&mut self) {
        self.hovered = false;
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn Any> {
        Some(self)
    }
    fn focusable(&self) -> bool {
        true
    }
    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }
}

impl ColorTrigger {
    /// HEX 输入框绑定的文本信号（未启用 HEX 框时为 None）。
    ///
    /// 面板里的输入框由 `Element::color_picker_opts` 内部建出，调用方拿不到那个信号；
    /// 需要把 HEX 文本联动到别处（或在测试里断言它）时从触发器节点 downcast 来取。
    pub fn hex_text(&self) -> Option<Signal<String>> {
        self.hex
    }

    /// 把当前颜色写进 HEX 文本框（并记进 echo，免得下一帧被当成用户输入）。
    ///
    /// 两处写入**都**要先比一次：`Signal::set` 不做相等短路（见 `signal.rs` 的
    /// `set`——它无条件 bump 版本并 `notify_changed`），而本方法每帧都会被调用一次，
    /// 不守卫就等于每帧把整窗标脏。
    fn push_hex(&self) {
        if let (Some(hex), Some(echo)) = (self.hex, self.hex_echo) {
            let s = self.st.value.get().to_hex_string();
            if hex.get() != s {
                hex.set(s.clone());
            }
            if echo.get() != s {
                echo.set(s);
            }
        }
    }
}

// ---------------------------------------------------------------- 配置

/// [`Element::color_picker_opts`](crate::ui::Element::color_picker_opts) 的配置。
///
/// 面板的组成在**构建期**就定死（各段是真实节点，不是运行期开关），故用配置结构体
/// 一次交清，而不是像 `.small()` 那样的链式修饰符——后者要 downcast 到具体 widget，
/// 而取色器返回的是"触发器 + 浮层子树"的组合，改不动已经建好的面板。
#[derive(Clone)]
pub struct ColorPickerOpts {
    /// 是否带透明度条。false 时输出恒为不透明色。
    pub alpha: bool,
    /// 是否带 HEX 输入框。
    pub hex: bool,
    /// 预设色块（空 = 不显示该行）。
    pub presets: Vec<Color>,
    /// 面板宽度（逻辑 px）。
    pub panel_width: i32,
    /// 触发器上是否显示 HEX 文本与展开箭头。false 时只有一枚方色块（适合工具栏）。
    pub trigger_text: bool,
    /// 展开信号。`None` = 控件自建一个。传入自己的可以从外部收起面板。
    pub open: Option<Signal<bool>>,
}

impl Default for ColorPickerOpts {
    fn default() -> Self {
        Self {
            alpha: true,
            hex: true,
            presets: default_presets(),
            panel_width: 232,
            trigger_text: true,
            open: None,
        }
    }
}

impl ColorPickerOpts {
    pub fn alpha(mut self, on: bool) -> Self {
        self.alpha = on;
        self
    }
    pub fn hex(mut self, on: bool) -> Self {
        self.hex = on;
        self
    }
    pub fn presets(mut self, colors: Vec<Color>) -> Self {
        self.presets = colors;
        self
    }
    pub fn panel_width(mut self, w: i32) -> Self {
        self.panel_width = w;
        self
    }
    pub fn trigger_text(mut self, on: bool) -> Self {
        self.trigger_text = on;
        self
    }
    pub fn open(mut self, sig: Signal<bool>) -> Self {
        self.open = Some(sig);
        self
    }
}

/// 默认预设色：一排常用色相 + 黑白灰。刻意不取自主题——预设是给用户挑内容色用的，
/// 跟着主题走会让"深色模式下预设全变深"，那不是调色板该有的行为。
pub fn default_presets() -> Vec<Color> {
    [
        0x000000, 0x5F6672, 0xB0B6BD, 0xFFFFFF, 0xE03131, 0xF76707, 0xF5A524, 0x2F9E44, 0x0CA678,
        0x1971C2, 0x6741D9, 0xC2255C,
    ]
    .into_iter()
    .map(Color::hex)
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{NodeId, Tree};
    use crate::event::{MouseButton, PointerEvent};
    use crate::geometry::Point;
    use crate::signal::signal;
    use crate::ui::Element;

    const W: i32 = 400;
    const H: i32 = 500;

    /// 建一棵「整窗 col 里放一个取色器」的树，返回 (tree, 取色器节点)。
    ///
    /// 取色器**不能**直接当根：根节点恒被 `layout_root` 拉伸到整个窗口，触发器于是
    /// 变成 400×500，"点触发器中心"就会落进它自己的浮层面板里——测试会因此测错东西
    /// 而不是失败，比失败更难发现。
    fn picker(value: Signal<Color>, opts: ColorPickerOpts) -> (Tree, NodeId) {
        let mut tree = Tree::new();
        let root = Element::col()
            .width(W)
            .height(H)
            .child(Element::color_picker_opts(value, opts))
            .build(&mut tree);
        tree.root = Some(root);
        relayout(&mut tree);
        let id = tree.get(root).unwrap().children[0];
        (tree, id)
    }

    fn relayout(tree: &mut Tree) {
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(W, H), &mut te);
    }

    fn click(tree: &mut Tree, p: Point) {
        let (mut hover, mut capture) = (None, None);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, p, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, p, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
    }

    /// 在节点内按相对比例 (fx, fy) 取一点并点击。坐标一律经 `abs_bounds` 实算，
    /// 不手写——面板挂在浮层上，手写坐标就绕过了锚定与命中那条链路。
    fn click_in(tree: &mut Tree, id: NodeId, fx: f32, fy: f32) {
        let b = tree.abs_bounds(id);
        assert!(!b.is_empty(), "目标节点尺寸为零，点击无从谈起");
        click(
            tree,
            Point::new(
                b.x + ((b.w - 1) as f32 * fx).round() as i32,
                b.y + ((b.h - 1) as f32 * fy).round() as i32,
            ),
        );
    }

    /// 浮层面板的各段（SV / 色相 / [透明度] / [HEX 行] / [预设网格]）。
    fn parts(tree: &Tree, picker: NodeId) -> Vec<NodeId> {
        let panel = tree.get(picker).unwrap().children[0];
        tree.get(panel).unwrap().children.clone()
    }

    /// HEX 输入框绑定的文本信号（挂在触发器上，见 [`ColorTrigger::hex_text`]）。
    fn hex_signal(tree: &mut Tree, picker: NodeId) -> Signal<String> {
        tree.get_mut(picker)
            .unwrap()
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ColorTrigger>())
            .expect("该节点应是取色器触发器")
            .hex_text()
            .expect("本例启用了 HEX 框")
    }

    // ---- 颜色模型 ----

    /// 颜色往返必须**无损**：`from_color` 再 `to_color` 要一模一样地回来。
    ///
    /// 这才是同步链路真正依赖的不变量。直接比 HSV 数值是不对的——低饱和/低明度下
    /// 8 位 RGB 能表达的色相档位本就很粗（271.5° 与 270° 落在同一组 RGB 上），
    /// 那是量化，不是缺陷。
    #[test]
    fn color_to_hsv_and_back_is_lossless() {
        for r in (0..=255).step_by(17) {
            for g in (0..=255).step_by(51) {
                for b in (0..=255).step_by(85) {
                    let c = Color::rgb(r as u8, g as u8, b as u8);
                    assert_eq!(Hsva::from_color(c).to_color(), c, "往返失真: {c:?}");
                }
            }
        }
    }

    /// 满饱和满明度下色相分辨率最高，此时往返应精确到 1.5° 内。
    #[test]
    fn hue_roundtrip_is_accurate_at_full_saturation() {
        for h in [0.0, 33.0, 60.0, 120.0, 180.0, 240.0, 271.5, 300.0, 359.0] {
            let back = Hsva::from_color(Hsva::new(h, 1.0, 1.0, 1.0).to_color()).h;
            let d = (back - h).abs().min(360.0 - (back - h).abs());
            assert!(d < 1.5, "色相往返失真: {h} -> {back}");
        }
    }

    /// 明度归零时色相必须保留，否则「拖到黑再拖回来」会换一个颜色。
    /// 这是 RGB→HSV 非单射带来的真实故障，不是理论问题。
    #[test]
    fn hue_survives_a_trip_through_black() {
        let blue = Hsva::new(220.0, 0.8, 0.9, 1.0);
        let black = Hsva { v: 0.0, ..blue }.to_color();
        assert_eq!(black, Color::rgba(0, 0, 0, 255), "本例前提：确实退化成纯黑");

        let naive = Hsva::from_color(black);
        assert_eq!(naive.h, 0.0, "直接反算会丢掉色相（对照组）");

        let kept = Hsva::from_color_keeping(black, blue);
        assert_eq!(kept.h, 220.0, "保留式反算必须留住色相");
        assert!((kept.s - 0.8).abs() < 0.001, "纯黑时饱和度也应沿用");
    }

    /// 纯灰只丢色相、不丢明度：饱和度确实是 0，不该沿用旧值。
    #[test]
    fn gray_keeps_hue_but_not_saturation() {
        let prev = Hsva::new(120.0, 0.7, 0.5, 1.0);
        let k = Hsva::from_color_keeping(Color::rgb(128, 128, 128), prev);
        assert_eq!(k.h, 120.0);
        assert_eq!(k.s, 0.0, "纯灰的饱和度就是 0，沿用旧值是错的");
        assert!((k.v - 128.0 / 255.0).abs() < 0.01);
    }

    #[test]
    fn alpha_survives_conversion() {
        let c = Hsva::new(10.0, 1.0, 1.0, 0.5).to_color();
        assert_eq!(c.a, 128);
        assert!((Hsva::from_color(c).a - 0.5).abs() < 0.01);
    }

    // ---- 交互 ----

    #[test]
    fn clicking_the_trigger_toggles_the_popup() {
        let open = signal(false);
        let (mut tree, id) = picker(
            signal(Color::hex(0x4C8BF5)),
            ColorPickerOpts::default().open(open),
        );
        assert!(!open.get());

        click_in(&mut tree, id, 0.5, 0.5);
        assert!(open.get(), "点触发器应展开");

        relayout(&mut tree);
        click_in(&mut tree, id, 0.5, 0.5);
        assert!(
            !open.get(),
            "再点一次应收起——核心的轻量关闭必须放过锚点，否则会关了又开、看着没反应"
        );
    }

    #[test]
    fn dragging_the_sv_area_changes_saturation_and_value_but_not_hue() {
        let value = signal(Color::hex(0x4C8BF5));
        let (mut tree, id) = picker(value, ColorPickerOpts::default().open(signal(true)));
        let seg = parts(&tree, id);
        let hue_before = Hsva::from_color(value.get()).h;

        // 右上角 = 饱和度满、明度满 → 该色相的纯色。
        click_in(&mut tree, seg[0], 1.0, 0.0);
        let got = Hsva::from_color(value.get());
        assert!((got.s - 1.0).abs() < 0.02, "右缘应是满饱和，实得 {}", got.s);
        assert!((got.v - 1.0).abs() < 0.02, "顶缘应是满明度，实得 {}", got.v);
        assert!(
            (got.h - hue_before).abs() < 2.0,
            "SV 方块不得改动色相：{hue_before} -> {}",
            got.h
        );
    }

    #[test]
    fn sv_area_keeps_hue_after_a_trip_through_black() {
        // 端到端复现「拖到黑再拖回来」：只有内部持有 HSVA 才过得去，
        // 每帧从 RGB 反算的实现会在这里把颜色变成红色。
        let value = signal(Color::hex(0x1971C2));
        let (mut tree, id) = picker(value, ColorPickerOpts::default().open(signal(true)));
        let seg = parts(&tree, id);
        let hue0 = Hsva::from_color(value.get()).h;

        click_in(&mut tree, seg[0], 1.0, 1.0); // 底缘：明度 0 → 纯黑
        assert_eq!(value.get(), Color::rgba(0, 0, 0, 255));
        relayout(&mut tree); // 让 on_update 跑一遍，模拟真实帧循环

        click_in(&mut tree, seg[0], 1.0, 0.0); // 回到满明度
        let got = Hsva::from_color(value.get());
        assert!(
            (got.h - hue0).abs() < 2.0,
            "经过纯黑后色相应原样回来：{hue0} -> {}",
            got.h
        );
    }

    #[test]
    fn hue_bar_changes_only_the_hue() {
        let value = signal(Hsva::new(200.0, 0.6, 0.8, 1.0).to_color());
        let (mut tree, id) = picker(value, ColorPickerOpts::default().open(signal(true)));
        let seg = parts(&tree, id);
        let before = Hsva::from_color(value.get());

        click_in(&mut tree, seg[1], 0.5, 0.5); // 正中 = 180°（青）
        let got = Hsva::from_color(value.get());
        assert!(
            (got.h - 180.0).abs() < 6.0,
            "色相条中点应约 180°，实得 {}",
            got.h
        );
        assert!((got.s - before.s).abs() < 0.02, "饱和度不该被色相条改动");
        assert!((got.v - before.v).abs() < 0.02, "明度不该被色相条改动");
    }

    #[test]
    fn alpha_bar_sets_transparency_and_is_absent_when_disabled() {
        let value = signal(Color::hex(0xE03131));
        let (mut tree, id) = picker(value, ColorPickerOpts::default().open(signal(true)));
        let seg = parts(&tree, id);
        click_in(&mut tree, seg[2], 0.0, 0.5); // 最左 = 全透明
        assert_eq!(value.get().a, 0, "透明度条最左应得 alpha=0");

        // 关掉透明度：段数少一条，且无论怎么拖输出恒为不透明。
        let v2 = signal(Color::rgba(0xE0, 0x31, 0x31, 0x40));
        let (mut t2, id2) = picker(
            v2,
            ColorPickerOpts::default().alpha(false).open(signal(true)),
        );
        let seg2 = parts(&t2, id2);
        assert_eq!(
            seg2.len(),
            seg.len() - 1,
            "alpha(false) 应少一段（透明度条）"
        );
        click_in(&mut t2, seg2[0], 1.0, 0.0);
        assert_eq!(v2.get().a, 255, "alpha(false) 时输出必须不透明");
    }

    #[test]
    fn preset_swatch_applies_its_color_but_keeps_current_alpha() {
        let value = signal(Color::rgba(0x00, 0x00, 0x00, 0x80));
        let (mut tree, id) = picker(
            value,
            ColorPickerOpts::default()
                .open(signal(true))
                .hex(false)
                .presets(vec![Color::hex(0x2F9E44)]),
        );
        let grid = *parts(&tree, id).last().unwrap();
        let row = tree.get(grid).unwrap().children[0];
        let sw = tree.get(row).unwrap().children[0];
        click_in(&mut tree, sw, 0.5, 0.5);
        let c = value.get();
        assert_eq!((c.r, c.g, c.b), (0x2F, 0x9E, 0x44));
        assert_eq!(c.a, 0x80, "预设是色相建议，不该顺手重置用户调好的透明度");
    }

    #[test]
    fn typing_a_hex_value_updates_the_color() {
        // HEX 框直接写它绑定的文本信号（TextInput 没有 on_change 回调），故这里也直接写，
        // 与用户敲完键盘后的状态等价；传播发生在下一帧的 on_update。
        let value = signal(Color::hex(0x000000));
        let (mut tree, id) = picker(value, ColorPickerOpts::default().open(signal(true)));
        let text = hex_signal(&mut tree, id);

        text.set("#2F9E44".to_string());
        relayout(&mut tree);
        assert_eq!(value.get(), Color::hex(0x2F9E44));

        // 打到一半的输入不该炸，也不该把颜色改成别的。
        text.set("#2F".to_string());
        relayout(&mut tree);
        assert_eq!(
            value.get(),
            Color::hex(0x2F9E44),
            "解析不出来的半截输入应原样放着，不改色"
        );
    }

    /// 拖面板之后 HEX 框必须跟着变。
    ///
    /// 这条曾经是漏的：`push_hex` 只挂在「外部改动」那条分支上，而拖拽走的
    /// `commit*` 会连 `echo` 一起写，`sync_from_value` 因此恒返回 false。当时所有
    /// 走 `commit*` 的用例都写了 `.hex(false)`，正好把这条路绕开——测试全绿，
    /// 屏幕上却是触发器的 HEX 在变、面板里的 HEX 不动，两处对不上。
    #[test]
    fn dragging_the_panel_updates_the_hex_box() {
        let value = signal(Color::hex(0x000000));
        let (mut tree, id) = picker(value, ColorPickerOpts::default().open(signal(true)));
        let text = hex_signal(&mut tree, id);
        let before = text.get();

        let seg = parts(&tree, id);
        click_in(&mut tree, seg[0], 1.0, 0.0); // SV 右上角：明显换一个颜色
        relayout(&mut tree);
        assert_ne!(text.get(), before, "拖过 SV 方块后 HEX 框不该还是旧值");
        assert_eq!(
            text.get(),
            value.get().to_hex_string(),
            "HEX 框必须与绑定颜色一致"
        );

        click_in(&mut tree, seg[1], 0.5, 0.5); // 色相条中点
        relayout(&mut tree);
        assert_eq!(text.get(), value.get().to_hex_string(), "色相条同理");
    }

    /// 上一条的另一半：没有人动的时候，`on_update` 一个信号都不许写。
    ///
    /// `Signal::set` 不做相等短路（无条件 bump 版本 + `notify_changed`），每帧无谓地
    /// 写一次就等于每帧把整窗标脏——取色器同步三份状态，三处写入任意一处漏了守卫
    /// 都会这样，而**症状是性能而非画面**，跑一遍界面根本看不出来。
    ///
    /// 探针用全局的「写过信号」标志而不是某一个信号的版本号：后者只盯得住它自己那一
    /// 处，换一处漏守卫就测不到了（第一版正是只盯 HEX 文本，漏守 `echo` 时照样绿）。
    #[test]
    fn idle_frames_write_no_signals_at_all() {
        let value = signal(Color::hex(0x4C8BF5));
        let (mut tree, _id) = picker(value, ColorPickerOpts::default().open(signal(true)));
        relayout(&mut tree);
        crate::signal::take_cross_window_dirty(); // 清掉建树/首帧同步留下的标志
        for i in 0..3 {
            relayout(&mut tree);
            assert!(
                !crate::signal::take_cross_window_dirty(),
                "第 {i} 个空闲帧写了信号：on_update 里有一处写入漏了相等守卫"
            );
        }
    }

    #[test]
    fn external_color_change_flows_back_into_the_panel_and_hex_box() {
        let value = signal(Color::hex(0x000000));
        let (mut tree, id) = picker(value, ColorPickerOpts::default().open(signal(true)));

        // 外部（业务代码）直接改绑定值。
        value.set(Color::hex(0x1971C2));
        relayout(&mut tree);
        assert_eq!(
            hex_signal(&mut tree, id).get(),
            "#1971C2",
            "HEX 框应跟随外部改动"
        );

        // 面板状态也要跟上：此时点 SV 方块右上角应得到**这个**色相的纯色。
        let expect = Hsva::from_color(Color::hex(0x1971C2)).h;
        let seg = parts(&tree, id);
        click_in(&mut tree, seg[0], 1.0, 0.0);
        let got = Hsva::from_color(value.get()).h;
        assert!(
            (got - expect).abs() < 2.0,
            "外部改色后 HSVA 应已回填：期望 ~{expect}，实得 {got}"
        );
    }

    /// 同一页上两个取色器：点第二个的触发器时，第一个必须收起、第二个必须展开。
    ///
    /// 两件事发生在**同一次按下**里：核心的轻量关闭收掉 A，紧接着 B 的触发器自己
    /// toggle 开。经破坏性验证，这条守的是"轻量关闭确实在指针按下时跑"——拿掉
    /// `dismiss_overlays_outside` 这一趟即变红。它不守分发顺序，也不守锚点豁免
    /// （那两条各由 `clicking_the_trigger_toggles_the_popup` 与
    /// `clicking_outside_the_panel_closes_it` 覆盖）。
    #[test]
    fn opening_one_picker_closes_the_other() {
        let a_open = signal(true);
        let b_open = signal(false);
        let mut tree = Tree::new();
        let root = Element::col()
            .width(W)
            .height(H)
            // 拉开的距离要大于 A 的面板高度（约 296）：面板会盖住它下方的一切，
            // 距离不够的话 B 的触发器就落在面板里，点到的是面板而不是 B。
            .spacing(340)
            .child(Element::color_picker_opts(
                signal(Color::hex(0x4C8BF5)),
                ColorPickerOpts::default().open(a_open),
            ))
            .child(Element::color_picker_opts(
                signal(Color::hex(0xE03131)),
                ColorPickerOpts::default().open(b_open),
            ))
            .build(&mut tree);
        tree.root = Some(root);
        relayout(&mut tree);

        let a = tree.get(root).unwrap().children[0];
        let b = tree.get(root).unwrap().children[1];
        let a_panel = tree.abs_bounds(tree.get(a).unwrap().children[0]);
        let b_trigger = tree.abs_bounds(b);
        assert!(
            !a_panel.contains(Point::new(
                b_trigger.x + b_trigger.w / 2,
                b_trigger.y + b_trigger.h / 2
            )),
            "本例前提：B 的触发器不在 A 的面板里"
        );

        click_in(&mut tree, b, 0.5, 0.5);
        assert!(!a_open.get(), "点另一个取色器应收起第一个");
        assert!(b_open.get(), "同一次按下里第二个应当展开");
    }

    #[test]
    fn clicking_outside_the_panel_closes_it() {
        let open = signal(true);
        let (mut tree, id) = picker(
            signal(Color::hex(0x4C8BF5)),
            ColorPickerOpts::default().open(open),
        );
        let panel = tree.get(id).unwrap().children[0];
        let pr = tree.abs_bounds(panel);
        let outside = Point::new(pr.right() + 20, pr.bottom() + 20);
        assert!(!pr.contains(outside), "本例前提：落点确实在面板外");
        click(&mut tree, outside);
        assert!(!open.get(), "面板外按下应收起");
    }

    /// 面板必须真的浮在后续内容之上：把取色器排在一列的第一个，展开后面板覆盖住
    /// 下面的兄弟，点在重叠处应命中面板而不是兄弟。这条是 `Element::popup` 的
    /// 存在理由，普通子节点在这里必然失败。
    #[test]
    fn panel_floats_over_the_siblings_below_it() {
        let mut tree = Tree::new();
        let root = Element::col()
            .width(W)
            .height(H)
            .child(Element::color_picker_opts(
                signal(Color::hex(0x4C8BF5)),
                ColorPickerOpts::default().open(signal(true)),
            ))
            .child(Element::leaf().width(300).height(300).bg(Color::WHITE))
            .build(&mut tree);
        tree.root = Some(root);
        relayout(&mut tree);

        let picker_id = tree.get(root).unwrap().children[0];
        let below = tree.get(root).unwrap().children[1];
        let panel_id = tree.get(picker_id).unwrap().children[0];
        let overlap = tree.abs_bounds(panel_id).intersect(&tree.abs_bounds(below));
        assert!(!overlap.is_empty(), "本例前提：面板与下方兄弟确有重叠");

        // 直接问命中，而不是数回调：`Element::leaf()` 上挂 `on_click` 是无效的
        // （EmptyWidget 不接 `take_click`），拿它当探针的话实现坏了也测不出来。
        let hit = tree
            .hit_test(Point::new(
                overlap.x + overlap.w / 2,
                overlap.y + overlap.h / 2,
            ))
            .expect("重叠处应当命中到某个节点");
        assert_ne!(hit, below, "重叠处不该命中被面板盖住的兄弟");
        let mut cur = Some(hit);
        while let Some(c) = cur {
            if c == panel_id {
                return;
            }
            cur = tree.get(c).and_then(|n| n.parent);
        }
        panic!("重叠处的命中应落在浮层面板子树内，实得 {hit:?}");
    }

    /// 预设里的纯黑走的是 `commit_color` + `from_color_keeping` 这条路：选完黑再把
    /// 明度拖回来，色相必须还是原来那个。它与 SV 拖拽那条路（`commit`，HSVA 直接落库）
    /// 是两套代码，各自会坏，故各测一次。
    #[test]
    fn picking_black_from_presets_still_remembers_the_hue() {
        let value = signal(Hsva::new(280.0, 0.9, 0.9, 1.0).to_color());
        let (mut tree, id) = picker(
            value,
            ColorPickerOpts::default()
                .open(signal(true))
                .hex(false)
                .presets(vec![Color::hex(0x000000)]),
        );
        let grid = *parts(&tree, id).last().unwrap();
        let row = tree.get(grid).unwrap().children[0];
        let sw = tree.get(row).unwrap().children[0];
        click_in(&mut tree, sw, 0.5, 0.5);
        assert_eq!(
            value.get(),
            Color::rgba(0, 0, 0, 255),
            "本例前提：确实选到了纯黑"
        );
        relayout(&mut tree);

        let seg = parts(&tree, id);
        click_in(&mut tree, seg[0], 1.0, 0.0); // SV 右上角 = 该色相的纯色
        let got = Hsva::from_color(value.get()).h;
        assert!(
            (got - 280.0).abs() < 2.0,
            "选过纯黑之后色相应仍是 280°，实得 {got}"
        );
    }
}
