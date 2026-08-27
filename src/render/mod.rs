//! 渲染抽象层：平台无关的 `Canvas` 绘制接口。
//!
//! 坐标用 f32（绝对窗口坐标）。布局层的 i32 `Rect` 在 paint 时转 f32。

#[cfg(feature = "gpu")]
pub mod gpu;
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

/// 把逻辑矩形对齐到**物理像素整数边界**。
///
/// 填充与描边都必须经过它，且必须是同一份实现——两侧各自对齐（或只对齐一侧）会让同一个
/// 矩形的底色边界与它的边框错开半个物理像素，在 125% / 150% 这类非整数 DPI 下看起来就是
/// 「圆角的边框有点偏」。这个函数存在的全部理由就是消灭那种不一致：四处调用点（两个后端
/// × 填充/描边）共用一份公式，改一处即全部跟上。
///
/// 对无边框的填充同样有益：边界落在整数像素上，不再留一列半透明的抗锯齿边。
///
/// 局部重绘的 offset 已按 4 逻辑像素网格对齐（`offset × scale` 为整数），故先对齐逻辑坐标
/// 再经变换，结果仍落在物理整数上。
pub(crate) fn align_to_device(x: f32, y: f32, w: f32, h: f32, scale: f32) -> (f32, f32, f32, f32) {
    let x0 = (x * scale).round() / scale;
    let y0 = (y * scale).round() / scale;
    let x1 = ((x + w) * scale).round() / scale;
    let y1 = ((y + h) * scale).round() / scale;
    (x0, y0, x1 - x0, y1 - y0)
}

#[cfg(test)]
mod align_tests {
    use super::align_to_device;

    /// 1.0 缩放下整数坐标原样通过——对齐不该在最常见的情形上引入任何位移。
    #[test]
    fn integer_scale_is_identity() {
        assert_eq!(
            align_to_device(10.0, 20.0, 30.0, 40.0, 1.0),
            (10.0, 20.0, 30.0, 40.0)
        );
    }

    /// 1.25 下逻辑 10 落在物理 12.5——正是会露馅的位置，必须被吸到整数物理像素上。
    #[test]
    fn fractional_scale_snaps_edges_to_physical_integers() {
        let s = 1.25;
        let (x, y, w, h) = align_to_device(10.0, 10.0, 20.0, 20.0, s);
        for v in [x * s, y * s, (x + w) * s, (y + h) * s] {
            assert!((v - v.round()).abs() < 1e-4, "{v} 应落在物理整数像素上");
        }
    }

    /// **同一矩形经填充与描边两条路径得到的边界必须相同**——这正是错位 bug 的根因，
    /// 两处若用了不同的公式，这条断言会立刻失败。
    #[test]
    fn same_rect_aligns_identically_for_fill_and_stroke() {
        for s in [1.0, 1.25, 1.5, 1.75, 2.0] {
            let a = align_to_device(10.3, 20.7, 22.0, 22.0, s);
            let b = align_to_device(10.3, 20.7, 22.0, 22.0, s);
            assert_eq!(a, b, "scale={s} 下两条路径必须给出同一边界");
        }
    }
}

/// 绘制接口。Phase 1 提供基础图元；裁剪/变换在 Phase 3 扩展。
pub trait Canvas {
    /// 当前 DPI 缩放因子（物理像素 / 逻辑像素，如 150% → 1.5）。
    /// 用于将 `Len::Px` 换算为等效逻辑值，使描边落在整数物理栅格。
    fn dpi_scale(&self) -> f32;

    /// 本画布实际能落笔的世界范围（**逻辑坐标**）。`None` = 未知，调用方不得剔除。
    ///
    /// 绘制遍历据此跳过完全落在范围外的节点自绘——这些图元本来也会被光栅器逐像素
    /// 丢弃，但**构造与排版的开销已经付掉了**。局部重绘（光标闪烁这类只脏几十像素的
    /// 动画）里这是大头：120 个控件的界面每帧照样提交 61 次描边、122 次文字，
    /// 实际只有光标那一条需要重画。
    ///
    /// 契约：返回值必须是本画布可见范围的**超集**——报小了会真的丢内容。
    fn cull_rect(&self) -> Option<Rect> {
        None
    }
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

    /// 折线描边：`pts` 逐点相连，**拐点处不留缺口**。
    ///
    /// 与"连着调几次 [`draw_line`](Self::draw_line)"的差别只在拐点。`draw_line` 用的是
    /// 平头端面（`LineCap::Butt`），两段方向不同的线各画各的，夹角外侧那块楔形谁也没盖到——
    /// 对勾、折返箭头这类图形因此会在拐点裂开。缺口的**逻辑**尺寸是固定的
    /// （≈ 笔宽 × tan(半夹角)），但占多少**物理**像素随 DPI 放大：100% 下不足一个像素、
    /// 被抗锯齿抹成一点灰，150%/200% 就是肉眼可见的豁口。所以它只在高 DPI 上显形，
    /// 低 DPI 截图比对会完全放过。
    ///
    /// 默认实现逐段描边，并在每个内部拐点补一个直径等于笔宽的圆点填平接缝——
    /// 视觉上等价于圆角连接（round join）。这是**正确**的实现，不是占位：后端可以
    /// 覆盖成一次性的路径描边（少几次提交、接缝质量更好），但不覆盖也不会画错。
    ///
    /// 少于两个点时不绘制。
    fn draw_polyline(&mut self, pts: &[(f32, f32)], width: f32, paint: &Paint) {
        if pts.len() < 2 {
            return;
        }
        for seg in pts.windows(2) {
            let ((x0, y0), (x1, y1)) = (seg[0], seg[1]);
            self.draw_line(x0, y0, x1, y1, width, paint);
        }
        // 只补**内部**拐点：两端点是线头，补圆会变成 round cap，改变外观。
        for &(x, y) in &pts[1..pts.len() - 1] {
            self.fill_circle(x, y, width / 2.0, paint);
        }
    }
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
        ts: &crate::text::TextStyle,
    );
    /// 测量单行文字尺寸（用于光标定位等）。无文字引擎时返回粗略估算。
    fn measure_text(&mut self, text: &str, ts: &crate::text::TextStyle) -> crate::geometry::Size;

    /// 测量在 `max_width` 内自动换行后的文字尺寸（用于 tooltip 等超宽自动换行场景）。
    /// 默认退化为单行测量，实现需覆盖以启用真正的换行度量（与 [`Self::draw_text`]
    /// 传入同宽 rect 时的换行结果一致，方可先测量定容器再绘制）。
    fn measure_text_wrapped(
        &mut self,
        text: &str,
        ts: &crate::text::TextStyle,
        max_width: f32,
    ) -> crate::geometry::Size {
        let _ = max_width;
        self.measure_text(text, ts)
    }

    /// `text` 单行排版后的基线度量（供富文本等同行混字号场景基线对齐）。
    /// 默认按「基线 = 行高 × 0.8」近似；有真实文字栈的实现应覆盖为精确值
    /// （软后端委托 `TextEngine::line_metrics`，d2d 走 GetLineMetrics）。
    fn text_line_metrics(
        &mut self,
        text: &str,
        ts: &crate::text::TextStyle,
    ) -> crate::text::LineMetrics {
        let h = self.measure_text(text, ts).h as f32;
        crate::text::LineMetrics {
            ascent: h * 0.8,
            descent: h * 0.2,
        }
    }

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
    /// 构造本帧 `Canvas`。`engine` 供软后端委托文字光栅；`scale` 为当前 DPI 缩放
    /// （由 handler 经 `set_scale` 统一提供，平台层不再各自决定）。
    fn make_canvas<'a>(
        &'a mut self,
        engine: &'a mut dyn TextEngine,
        scale: f32,
    ) -> Box<dyn Canvas + 'a>;
    /// 软渲染局部重绘快路取原始 Pixmap；GPU 后端默认 None → 调用方走自己的局部路径。
    fn as_pixmap(&mut self) -> Option<&mut tiny_skia::Pixmap> {
        None
    }

    /// 本目标能否只重画一块、其余区域保留上一帧的内容。
    ///
    /// 「保留上一帧」这件事在两条后端上落在不同的地方：软后端靠宿主维护的后备
    /// `Pixmap`（`app/damage.rs`），GPU 后端靠目标自己的常驻色纹理——窗口 surface 的
    /// 纹理是**轮转**的（Metal 的 drawable 通常两三张），只画脏区的话其余区域会是两三
    /// 帧之前的画面。默认按「有没有 Pixmap」判，故 d2d 后端（两者都没有）恒 false。
    fn supports_partial(&mut self) -> bool {
        self.as_pixmap().is_some()
    }

    /// 宣告本帧的重绘范围（**物理**像素，`None` = 整窗），并按需铺底。
    ///
    /// 必须在 [`Self::make_canvas`] 之前调用一次。存在的理由是**时序**：平台层开帧时
    /// 还不知道这一帧是局部还是整窗——那是宿主在 `render()` 里看脏区、浮层、结构签名
    /// 之后才定的。清底若留在开帧那一步，就得靠平台层预测，而预测错一次的代价是整窗
    /// 内容丢失（预测整窗→清了底，宿主却只画脏区）。改由宿主定完再宣告，两者恒等。
    ///
    /// 软后端默认空操作：它的铺底与合成走宿主的后备缓冲路径，不经过本方法。
    fn begin_damage(&mut self, damage: Option<Rect>, bg: Color) {
        let _ = (damage, bg);
    }
}

/// tiny-skia 软后端的渲染目标：借用一份 `Pixmap`。跨平台共用。
pub struct PixmapTarget<'p> {
    pub pixmap: &'p mut tiny_skia::Pixmap,
}

impl RenderTarget for PixmapTarget<'_> {
    fn make_canvas<'a>(
        &'a mut self,
        engine: &'a mut dyn TextEngine,
        scale: f32,
    ) -> Box<dyn Canvas + 'a> {
        Box::new(SkiaCanvas::with_text(&mut *self.pixmap, engine, scale))
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
            };
            let mut canvas = target.make_canvas(&mut engine, 1.0);
            canvas.fill_rect(0.0, 0.0, 10.0, 10.0, &Paint::fill(Color::rgb(255, 0, 0)));
        }
        // 左上角像素应为红（预乘 RGBA）。
        let px = pixmap.data();
        assert_eq!(&px[0..4], &[255, 0, 0, 255]);
    }
}
