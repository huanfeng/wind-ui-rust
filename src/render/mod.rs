//! 渲染抽象层：平台无关的 `Canvas` 绘制接口。
//!
//! 坐标用 f32（绝对窗口坐标）。布局层的 i32 `Rect` 在 paint 时转 f32。

pub mod skia;

pub use skia::SkiaCanvas;

use crate::geometry::{Color, Rect};
use crate::spec::Align;

/// 绘制参数。
#[derive(Debug, Clone, Copy)]
pub struct Paint {
    pub color: Color,
    pub anti_alias: bool,
}

impl Paint {
    pub fn fill(color: Color) -> Self {
        Self { color, anti_alias: true }
    }
}

/// 绘制接口。Phase 1 提供基础图元；裁剪/变换在 Phase 3 扩展。
pub trait Canvas {
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, paint: &Paint);
    fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, paint: &Paint);
    fn stroke_round_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        width: f32,
        paint: &Paint,
    );
    fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, paint: &Paint);
    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, paint: &Paint);
    /// 在 rect 内绘制文字（水平按 align、垂直居中）。无文字引擎时为空操作。
    fn draw_text(
        &mut self,
        text: &str,
        rect: Rect,
        color: Color,
        align: Align,
        family: Option<&str>,
        size: f32,
    );
    /// 测量单行文字尺寸（用于光标定位等）。无文字引擎时返回粗略估算。
    fn measure_text(&mut self, text: &str, family: Option<&str>, size: f32) -> crate::geometry::Size;

    /// 保存当前裁剪状态。
    fn save(&mut self);
    /// 恢复到最近一次 save 的裁剪状态。
    fn restore(&mut self);
    /// 将裁剪区与矩形 `r` 求交（后续绘制仅作用于交集内）。
    fn clip_rect(&mut self, r: Rect);
}

/// 构造圆角矩形路径（cubic 贝塞尔逼近四角）。radius<=0 退化为直角矩形。
pub(crate) fn rounded_rect_path(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    radius: f32,
) -> Option<tiny_skia::Path> {
    use tiny_skia::PathBuilder;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    let mut pb = PathBuilder::new();
    if r <= 0.0 {
        pb.push_rect(tiny_skia::Rect::from_xywh(x, y, w, h)?);
        return pb.finish();
    }
    let k = 0.552_284_8 * r; // 贝塞尔逼近圆弧的控制点系数
    let (l, t, rt, b) = (x, y, x + w, y + h);
    pb.move_to(l + r, t);
    pb.line_to(rt - r, t);
    pb.cubic_to(rt - r + k, t, rt, t + r - k, rt, t + r);
    pb.line_to(rt, b - r);
    pb.cubic_to(rt, b - r + k, rt - r + k, b, rt - r, b);
    pb.line_to(l + r, b);
    pb.cubic_to(l + r - k, b, l, b - r + k, l, b - r);
    pb.line_to(l, t + r);
    pb.cubic_to(l, t + r - k, l + r - k, t, l + r, t);
    pb.close();
    pb.finish()
}
