//! 渲染抽象层：平台无关的 `Canvas` 绘制接口。
//!
//! 坐标用 f32（绝对窗口坐标）。布局层的 i32 `Rect` 在 paint 时转 f32。

pub mod image;
pub mod prof;
pub mod skia;

pub use image::{DecodedImage, Fit, Image, ImageDecoder, ImageError, VisualState};
pub use skia::SkiaCanvas;

use crate::geometry::{Color, Rect};
use crate::spec::Align;
use crate::text::TextEngine;

/// 渐变色标：位置 `offset`（0..=1）+ 颜色。
#[derive(Debug, Clone, Copy)]
pub struct GradientStop {
    pub offset: f32,
    pub color: Color,
}

/// 渐变填充。所有坐标均为**相对绘制矩形的归一化坐标**（0..1）：
/// (0,0)=左上、(1,1)=右下、(0.5,0.5)=中心；由 SkiaCanvas 在填充时乘以
/// rect 宽高映射到逻辑坐标，故同一渐变可复用于任意尺寸控件。
#[derive(Debug, Clone)]
pub enum Gradient {
    /// 线性渐变：从 `start` 到 `end` 沿直线插值。
    Linear {
        start: (f32, f32),
        end: (f32, f32),
        stops: Vec<GradientStop>,
    },
    /// 径向渐变：以 `center` 为圆心、`radius`（相对 rect 短边的归一化半径）向外插值。
    Radial {
        center: (f32, f32),
        radius: f32,
        stops: Vec<GradientStop>,
    },
}

impl Gradient {
    fn to_stops(stops: Vec<(f32, Color)>) -> Vec<GradientStop> {
        stops
            .into_iter()
            .map(|(offset, color)| GradientStop { offset, color })
            .collect()
    }

    /// 构造线性渐变。`stops` 为 (offset, color) 列表（至少两项，offset 递增）。
    pub fn linear(start: (f32, f32), end: (f32, f32), stops: Vec<(f32, Color)>) -> Self {
        Gradient::Linear {
            start,
            end,
            stops: Self::to_stops(stops),
        }
    }

    /// 构造径向渐变。`radius` 为相对 rect 短边的归一化半径（1.0≈半个短边）。
    pub fn radial(center: (f32, f32), radius: f32, stops: Vec<(f32, Color)>) -> Self {
        Gradient::Radial {
            center,
            radius,
            stops: Self::to_stops(stops),
        }
    }

    /// 色标列表（两个变体共用）。
    pub fn stops(&self) -> &[GradientStop] {
        match self {
            Gradient::Linear { stops, .. } | Gradient::Radial { stops, .. } => stops,
        }
    }
}

/// 绘制参数。
#[derive(Debug, Clone)]
pub struct Paint {
    /// 纯色，或渐变时作为 stroke/降级填充的回退色（取首个 stop）。
    pub color: Color,
    pub anti_alias: bool,
    /// 渐变填充（None = 纯色）。仅 fill 类图元生效；stroke 退化用 `color`。
    pub gradient: Option<Gradient>,
}

impl Paint {
    pub fn fill(color: Color) -> Self {
        Self {
            color,
            anti_alias: true,
            gradient: None,
        }
    }

    /// 渐变填充。`color` 回退为首个 stop（stops 为空时回退透明）。
    pub fn gradient(g: Gradient) -> Self {
        let color = g
            .stops()
            .first()
            .map(|s| s.color)
            .unwrap_or(Color::TRANSPARENT);
        Self {
            color,
            anti_alias: true,
            gradient: Some(g),
        }
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
    /// 绘制圆角矩形投影（drop shadow）：投影矩形 (x,y,w,h)、`radius` 圆角、
    /// `blur` 模糊半径（逻辑 px）、`color`（含 alpha）。绘制在节点背景之下；
    /// 偏移/外扩(spread)由调用方算入 (x,y,w,h)。`blur<=0` 时退化为锐利圆角矩形。
    fn draw_shadow(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, blur: f32, color: Color);
    /// 把图片按 `fit` 缩放绘制到逻辑矩形 `dst`，并始终裁剪到 `dst`（Cover 溢出、
    /// None 超框安全收口）。`radius>0` 时按圆角裁剪（与背景/边框同源圆角）。
    /// `opacity` 为整体不透明度（0..=1，用于禁用置灰等状态调制）。
    fn draw_image(
        &mut self,
        img: &image::Image,
        dst: Rect,
        fit: image::Fit,
        radius: f32,
        opacity: f32,
    );
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
    fn measure_text(
        &mut self,
        text: &str,
        family: Option<&str>,
        size: f32,
    ) -> crate::geometry::Size;

    /// 压入一层离屏合成层：后续绘制重定向到该层；`pop_layer` 时以 `opacity`
    /// 整体合成回父层。用于子树统一不透明度（避免逐节点 alpha 导致的重叠错叠）。
    fn push_layer(&mut self, opacity: f32);
    /// 弹出最近的合成层并按其 opacity 合成回父层。
    fn pop_layer(&mut self);

    /// 保存当前裁剪状态。
    fn save(&mut self);
    /// 恢复到最近一次 save 的裁剪状态。
    fn restore(&mut self);
    /// 将裁剪区与矩形 `r` 求交（后续绘制仅作用于交集内）。
    fn clip_rect(&mut self, r: Rect);
}

/// 后端无关的一帧渲染目标。平台层每帧提供，宿主结合自身文字引擎得到 `Canvas`。
///
/// 软后端把 `Pixmap` 包成 `SkiaCanvas`；GPU 后端自带 DirectWrite 文字栈，忽略 `engine`。
pub trait RenderTarget {
    /// 构造本帧 `Canvas`。`engine` 供软后端委托文字光栅。
    fn make_canvas<'a>(&'a mut self, engine: &'a mut dyn TextEngine) -> Box<dyn Canvas + 'a>;
    /// 软渲染局部重绘快路取原始 Pixmap；GPU 后端默认 None → 调用方强制全窗。
    fn as_pixmap(&mut self) -> Option<&mut tiny_skia::Pixmap> {
        None
    }
}

/// tiny-skia 软后端的渲染目标：借用一份 `Pixmap`。跨平台共用。
pub struct PixmapTarget<'p> {
    pub pixmap: &'p mut tiny_skia::Pixmap,
    pub scale: f32,
}

impl RenderTarget for PixmapTarget<'_> {
    fn make_canvas<'a>(&'a mut self, engine: &'a mut dyn TextEngine) -> Box<dyn Canvas + 'a> {
        Box::new(SkiaCanvas::with_text(&mut *self.pixmap, engine, self.scale))
    }

    fn as_pixmap(&mut self) -> Option<&mut tiny_skia::Pixmap> {
        Some(self.pixmap)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixmap_target_make_canvas_paints() {
        use crate::text::NullTextEngine;
        let mut pixmap = tiny_skia::Pixmap::new(10, 10).unwrap();
        let mut engine = NullTextEngine;
        {
            let mut target = PixmapTarget {
                pixmap: &mut pixmap,
                scale: 1.0,
            };
            let mut canvas = target.make_canvas(&mut engine);
            canvas.fill_rect(0.0, 0.0, 10.0, 10.0, &Paint::fill(Color::rgb(255, 0, 0)));
        }
        // 左上角像素应为红（预乘 RGBA）。
        let px = pixmap.data();
        assert_eq!(&px[0..4], &[255, 0, 0, 255]);
    }
}
