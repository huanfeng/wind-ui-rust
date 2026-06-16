//! tiny-skia 后端：把 `Canvas` 图元光栅化到 `Pixmap`（RGBA 预乘）。
//!
//! 支持矩形裁剪栈：用 alpha `Mask` 表示当前裁剪区，所有绘制传入栈顶 mask。

use tiny_skia::{
    FillRule, LineCap, Mask, Paint as SkPaint, PathBuilder, Pixmap, Stroke, Transform,
};

use super::{rounded_rect_path, Canvas, Paint};
use crate::geometry::{Color, Rect};
use crate::spec::Align;
use crate::text::TextEngine;

/// 裁剪层：有效裁剪矩形（各级交集）+ 对应 alpha mask。
struct Clip {
    rect: Rect,
    mask: Mask,
}

/// 直接绘制到借入的 `Pixmap`。
///
/// 控件树用**逻辑坐标**（dp）；本 canvas 通过 `scale` 把逻辑坐标变换为物理像素：
/// 图形走 tiny-skia `Transform::from_scale`，文字按物理字号交 DirectWrite 渲染。
pub struct SkiaCanvas<'a> {
    pixmap: &'a mut Pixmap,
    engine: Option<&'a mut dyn TextEngine>,
    clips: Vec<Clip>,
    /// save() 记录的栈深度，restore() 据此回弹。
    saves: Vec<usize>,
    /// 逻辑→物理缩放因子（DPI / 96）。
    scale: f32,
}

impl<'a> SkiaCanvas<'a> {
    /// 无文字能力（仅图形），scale=1。
    pub fn new(pixmap: &'a mut Pixmap) -> Self {
        Self { pixmap, engine: None, clips: Vec::new(), saves: Vec::new(), scale: 1.0 }
    }

    /// 带文字引擎与 DPI 缩放。
    pub fn with_text(pixmap: &'a mut Pixmap, engine: &'a mut dyn TextEngine, scale: f32) -> Self {
        Self { pixmap, engine: Some(engine), clips: Vec::new(), saves: Vec::new(), scale }
    }

    fn sk_paint(p: &Paint) -> SkPaint<'static> {
        let mut sp = SkPaint::default();
        sp.set_color(to_sk_color(p.color));
        sp.anti_alias = p.anti_alias;
        sp
    }
}

impl Canvas for SkiaCanvas<'_> {
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, paint: &Paint) {
        self.fill_round_rect(x, y, w, h, 0.0, paint);
    }

    fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, paint: &Paint) {
        if let Some(path) = rounded_rect_path(x, y, w, h, radius) {
            let sp = Self::sk_paint(paint);
            let mask = self.clips.last().map(|c| &c.mask);
            self.pixmap.fill_path(&path, &sp, FillRule::Winding, Transform::from_scale(self.scale, self.scale), mask);
        }
    }

    fn stroke_round_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        width: f32,
        paint: &Paint,
    ) {
        let width = width.min(w / 2.0).min(h / 2.0).max(0.0);
        let half = width / 2.0;
        if let Some(path) =
            rounded_rect_path(x + half, y + half, w - width, h - width, (radius - half).max(0.0))
        {
            let sp = Self::sk_paint(paint);
            let stroke = Stroke { width, ..Default::default() };
            let mask = self.clips.last().map(|c| &c.mask);
            self.pixmap.stroke_path(&path, &sp, &stroke, Transform::from_scale(self.scale, self.scale), mask);
        }
    }

    fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, paint: &Paint) {
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        if let Some(path) = pb.finish() {
            let sp = Self::sk_paint(paint);
            let stroke = Stroke { width, line_cap: LineCap::Butt, ..Default::default() };
            let mask = self.clips.last().map(|c| &c.mask);
            self.pixmap.stroke_path(&path, &sp, &stroke, Transform::from_scale(self.scale, self.scale), mask);
        }
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, paint: &Paint) {
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            let sp = Self::sk_paint(paint);
            let mask = self.clips.last().map(|c| &c.mask);
            self.pixmap.fill_path(&path, &sp, FillRule::Winding, Transform::from_scale(self.scale, self.scale), mask);
        }
    }

    fn draw_text(
        &mut self,
        text: &str,
        rect: Rect,
        color: Color,
        align: Align,
        family: Option<&str>,
        size: f32,
    ) {
        // 传逻辑 rect/size/clip；引擎内部持有 scale 自行物理化（与 measure 同源）。
        let clip = self.clips.last().map(|c| c.rect);
        if let Some(engine) = self.engine.as_deref_mut() {
            engine.draw(self.pixmap, text, rect, color, align, family, size, clip);
        }
    }

    fn measure_text(&mut self, text: &str, family: Option<&str>, size: f32) -> crate::geometry::Size {
        // 逻辑入参；引擎内部物理测量后 /scale 回逻辑，与正文绘制度量同源。
        match self.engine.as_deref_mut() {
            Some(engine) => engine.measure(text, family, size, None),
            None => crate::geometry::Size::new(
                (text.chars().count() as f32 * size * 0.6).ceil() as i32,
                size.ceil() as i32,
            ),
        }
    }

    fn save(&mut self) {
        self.saves.push(self.clips.len());
    }

    fn restore(&mut self) {
        if let Some(depth) = self.saves.pop() {
            self.clips.truncate(depth);
        }
    }

    fn clip_rect(&mut self, r: Rect) {
        // 契约：每次 clip_rect 须配一次先行的 save()，使其与 restore() 成对、
        // 仅在当前层之上叠加裁剪。否则裁剪会被 restore 遗漏而泄漏。
        debug_assert!(
            !self.saves.is_empty(),
            "clip_rect 必须在 save() 之后调用，以与 restore() 配对"
        );
        // 与当前裁剪区求交，构造矩形 mask。
        let eff = match self.clips.last() {
            Some(c) => c.rect.intersect(&r),
            None => r,
        };
        let (pw, ph) = (self.pixmap.width(), self.pixmap.height());
        if let Some(mut mask) = Mask::new(pw, ph) {
            // mask 用物理整数矩形（与文字 clip 的 rect.scaled 同源），消除取整分歧。
            let peff = eff.scaled(self.scale);
            if !peff.is_empty() {
                if let Some(rect) = tiny_skia::Rect::from_xywh(
                    peff.x as f32,
                    peff.y as f32,
                    peff.w as f32,
                    peff.h as f32,
                ) {
                    let mut pb = PathBuilder::new();
                    pb.push_rect(rect);
                    if let Some(path) = pb.finish() {
                        mask.fill_path(&path, FillRule::Winding, false, Transform::identity());
                    }
                }
            }
            // clips 存逻辑矩形（intersect 在逻辑空间）。
            self.clips.push(Clip { rect: eff, mask });
        }
    }
}

fn to_sk_color(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(pm: &Pixmap, x: u32, y: u32) -> (u8, u8, u8) {
        let p = pm.pixel(x, y).unwrap();
        (p.red(), p.green(), p.blue())
    }

    /// 在一个薄裁剪矩形内填充，验证裁剪内的像素确实被绘制（复现进度条隐患）。
    #[test]
    fn thin_clip_rect_does_not_drop_fill() {
        let mut pm = Pixmap::new(100, 100).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut pm);
            c.save();
            c.clip_rect(Rect::new(10, 40, 80, 6)); // 薄裁剪带
            c.fill_round_rect(20.0, 40.0, 40.0, 6.0, 3.0, &Paint::fill(Color::hex(0xFF0000)));
            c.restore();
        }
        // 裁剪带中心应被红色填充。
        let (r, g, b) = px(&pm, 35, 43);
        assert!(r > 200 && g < 80 && b < 80, "薄裁剪带内应被填充，实得 ({r},{g},{b})");
    }

    /// 复现进度条精确场景：with_text + 真实几何，薄裁剪带 + 圆角填充。
    #[test]
    fn thin_clip_rect_with_engine_and_offset() {
        let mut pm = Pixmap::new(320, 280).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let mut eng = crate::text::NullTextEngine;
        {
            let mut c = SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            c.save();
            c.clip_rect(Rect::new(22, 42, 276, 6));
            // 进度滑块：x=22+6.37, y=42, w=96.6, h=6, r=3
            c.fill_round_rect(28.37, 42.0, 96.6, 6.0, 3.0, &Paint::fill(Color::hex(0x4C8BF5)));
            c.restore();
        }
        let (r, g, b) = px(&pm, 60, 44);
        assert!(b > 180 && r < 140, "进度滑块应在裁剪带内显现，实得 ({r},{g},{b})");
    }
}
