//! tiny-skia 后端：把 `Canvas` 图元光栅化到 `Pixmap`（RGBA 预乘）。

use tiny_skia::{FillRule, LineCap, Paint as SkPaint, Pixmap, Stroke, Transform};

use super::{rounded_rect_path, Canvas, Paint};
use crate::geometry::Color;

/// 直接绘制到借入的 `Pixmap`。坐标为绝对窗口坐标。
pub struct SkiaCanvas<'a> {
    pixmap: &'a mut Pixmap,
}

impl<'a> SkiaCanvas<'a> {
    pub fn new(pixmap: &'a mut Pixmap) -> Self {
        Self { pixmap }
    }

    fn sk_paint(&self, p: &Paint) -> SkPaint<'static> {
        let mut sp = SkPaint::default();
        sp.set_color(to_sk_color(p.color));
        sp.anti_alias = p.anti_alias;
        sp
    }
}

impl Canvas for SkiaCanvas<'_> {
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, paint: &Paint) {
        if let Some(rect) = tiny_skia::Rect::from_xywh(x, y, w, h) {
            let sp = self.sk_paint(paint);
            self.pixmap.fill_rect(rect, &sp, Transform::identity(), None);
        }
    }

    fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, paint: &Paint) {
        if let Some(path) = rounded_rect_path(x, y, w, h, radius) {
            let sp = self.sk_paint(paint);
            self.pixmap
                .fill_path(&path, &sp, FillRule::Winding, Transform::identity(), None);
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
        // 描边沿路径中线，向内缩半个线宽避免越界裁掉外缘。
        // 线宽不超过盒子短边的一半，避免路径塌陷/反转。
        let width = width.min(w / 2.0).min(h / 2.0).max(0.0);
        let half = width / 2.0;
        if let Some(path) =
            rounded_rect_path(x + half, y + half, w - width, h - width, (radius - half).max(0.0))
        {
            let sp = self.sk_paint(paint);
            let stroke = Stroke { width, ..Default::default() };
            self.pixmap
                .stroke_path(&path, &sp, &stroke, Transform::identity(), None);
        }
    }

    fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, paint: &Paint) {
        use tiny_skia::PathBuilder;
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        if let Some(path) = pb.finish() {
            let sp = self.sk_paint(paint);
            let stroke = Stroke { width, line_cap: LineCap::Butt, ..Default::default() };
            self.pixmap
                .stroke_path(&path, &sp, &stroke, Transform::identity(), None);
        }
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, paint: &Paint) {
        use tiny_skia::PathBuilder;
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            let sp = self.sk_paint(paint);
            self.pixmap
                .fill_path(&path, &sp, FillRule::Winding, Transform::identity(), None);
        }
    }
}

fn to_sk_color(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
}
