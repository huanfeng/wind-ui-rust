//! 渲染抽象层：平台无关的 `Canvas` 绘制接口。
//!
//! 坐标用 f32（绝对窗口坐标）。布局层的 i32 `Rect` 在 paint 时转 f32。

#[cfg(gpu_backend)]
pub mod gpu;
pub mod image;
pub mod prof;
pub mod skia;

pub use image::{DecodedImage, Fit, Image, ImageDecoder, ImageError, VisualState};
pub use skia::SkiaCanvas;

use crate::geometry::{Color, Corners, Rect};
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

/// `draw_text` 剔除用的**逻辑**矩形：字形可能落到的范围的保守上界。
///
/// 不能直接拿调用方的 rect：引擎的纵向契约（见 `TextEngine::draw`）允许 rect 比文本盒
/// 矮——装不下即顶对齐、**向下**溢出，由调用方裁剪收口。按 rect 本身剔除，这类合法
/// 调用在整窗帧上正常，到了局部帧（可绘区域只是脏区那一小块）却与 rect 不相交、整行
/// 不进引擎：光标左右各半个字被底色盖住，不报错、不 panic。
///
/// 溢出只向下，故只把高度抬到「两行字高」——单行文本盒的宽松上界，不去 measure
/// （剔除存在的意义就是省掉排版）。多行文本请给足 rect 高度。两个后端共用这一份，
/// 再各自在物理坐标上留 4px 出挑余量。
pub(crate) fn text_cull_rect(rect: Rect, ts: &crate::text::TextStyle) -> Rect {
    let line = ts.size * ts.line_height.unwrap_or(1.0).max(1.0);
    let h = rect.h.max((line * 2.0).ceil() as i32);
    Rect::new(rect.x, rect.y, rect.w, h)
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

/// [`Canvas::fill_polygon`] 的逐物理行扫描实现（trait 默认实现即此）。抽成自由函数，是让覆盖了
/// `fill_polygon` 的后端在自己的路径失败时还能退回它。
pub(crate) fn fill_polygon_rows<C: Canvas + ?Sized>(
    c: &mut C,
    contours: &[&[(f32, f32)]],
    paint: &Paint,
) {
    let s = c.dpi_scale().max(0.01);
    // 有效边一次收齐：(上端 x, 上端 y, 下端 x, 下端 y, 方向)。水平边不参与求交；端点非有限的
    // 边跳过——NaN 会让环绕数在行尾归不了零，inf 则把下面的行范围撑到天文数字。
    let mut edges: Vec<(f32, f32, f32, f32, i32)> = Vec::new();
    for poly in contours.iter().filter(|p| p.len() >= 3) {
        for (i, &(ax, ay)) in poly.iter().enumerate() {
            let (bx, by) = poly[(i + 1) % poly.len()];
            if ay == by || ![ax, ay, bx, by].iter().all(|v| v.is_finite()) {
                continue;
            }
            edges.push(if ay < by {
                (ax, ay, bx, by, 1)
            } else {
                (bx, by, ax, ay, -1)
            });
        }
    }
    let (mut top, mut bottom) = edges
        .iter()
        .fold((f32::MAX, f32::MIN), |(t, b), e| (t.min(e.1), b.max(e.3)));
    // 行范围钳到画布能落笔的范围：坐标极大时不至于逐行跑上千万次（UI 线程卡死），局部帧
    // 里也不为脏区外的每一行提交一次图元。画布答不上来时按一个任何窗口都够用的范围兜底。
    let (vis_top, vis_bottom) = match c.cull_rect() {
        Some(r) => (r.y as f32, r.bottom() as f32),
        None => (-16384.0, 16384.0),
    };
    top = top.max(vis_top);
    bottom = bottom.min(vis_bottom);
    if top >= bottom {
        return;
    }
    let line = Paint {
        gradient: None,
        ..paint.clone()
    };
    let mut hits: Vec<(f32, i32)> = Vec::new();
    for row in (top * s).floor() as i64..(bottom * s).ceil() as i64 {
        let y = (row as f32 + 0.5) / s;
        hits.clear();
        // 半开区间 [上端, 下端)：顶点恰在行中心时只算一次。
        for &(x0, y0, x1, y1, dir) in &edges {
            if y >= y0 && y < y1 {
                hits.push((x0 + (y - y0) / (y1 - y0) * (x1 - x0), dir));
            }
        }
        hits.sort_by(|a, b| a.0.total_cmp(&b.0));
        let (mut winding, mut start) = (0, 0.0);
        for &(x, dir) in &hits {
            let was = winding;
            winding += dir;
            if was == 0 && winding != 0 {
                start = x;
            } else if was != 0 && winding == 0 && x > start {
                // 线宽放宽 3%：恰好 1 物理像素时，1/scale 的浮点误差会让行缘各缺一丝
                // （175% 下实测内部一行只有 98.8% 覆盖），相邻行重叠这 0.03 像素即可补上。
                // 代价是半透明色的内部像素多叠约 1.5% 的覆盖（alpha 0.5 时差 1～2 个色阶），
                // 各行一致、不成横纹，肉眼不可见。
                c.draw_line(start, y, x, y, 1.03 / s, &line);
            }
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

    /// 四角半径各自独立的圆角矩形填充（CSS `border-radius` 的四值形式）。
    ///
    /// 四角相同时等价于 [`fill_round_rect`](Self::fill_round_rect)（直接转过去，逐像素一致）。
    /// 不同时把轮廓按物理像素密度折线化交给 [`fill_polygon`](Self::fill_polygon)：软件与
    /// D2D 后端那是一次路径光栅化，边缘抗锯齿与 `fill_round_rect` 同一档。GPU 后端覆盖为
    /// 按象限裁剪、各用该角半径画原生圆角矩形（抗锯齿与渐变都与统一半径版一致），只有半径
    /// 越过边长一半时退回折线（`fill_polygon` 在那里是逐行兜底）。半径超出边长时按
    /// [`Corners::fit`] 等比收缩。坐标先对齐物理像素，与统一半径版同源，故同框的填充与
    /// [`stroke_round_rect_corners`](Self::stroke_round_rect_corners) 不会错开半个像素。
    fn fill_round_rect_corners(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        corners: Corners,
        paint: &Paint,
    ) {
        if corners.is_uniform() {
            self.fill_round_rect(x, y, w, h, corners.top_left, paint);
            return;
        }
        fill_round_rect_corners_poly(self, x, y, w, h, corners, paint);
    }

    /// 四角半径各自独立的圆角矩形描边：描边落在 `(x, y, w, h)` 框内，内缘半径 = 外缘半径
    /// − 线宽（同 CSS）。四角相同时等价于 [`stroke_round_rect`](Self::stroke_round_rect)；
    /// 不同时以「外轮廓 + 反向内轮廓」的环带交给 [`fill_polygon`](Self::fill_polygon)，
    /// 精度说明同 [`fill_round_rect_corners`](Self::fill_round_rect_corners)。
    fn stroke_round_rect_corners(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        corners: Corners,
        width: f32,
        paint: &Paint,
    ) {
        if corners.is_uniform() {
            self.stroke_round_rect(x, y, w, h, corners.top_left, width, paint);
            return;
        }
        stroke_round_rect_corners_poly(self, x, y, w, h, corners, width, paint);
    }

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

    /// 多边形填充。`contours` 每项是一个**闭合轮廓**（首尾自动相连，少于 3 个点的忽略），
    /// 逻辑坐标；按**非零环绕**（Winding）规则填充：要挖洞，就让内圈与外圈方向相反——
    /// 圆盘相减、带孔图标、齿轮都是一个调用。凹多边形、自交轮廓照常。
    ///
    /// 软件后端与 Windows D2D 后端覆盖为一次性路径光栅化（边缘双向抗锯齿，支持渐变）。
    /// 默认实现逐物理行扫描：每行取行中心求交，按环绕数取区间，用 1 物理像素宽的
    /// [`draw_line`](Self::draw_line) 画出——左右端点带抗锯齿、行与行首尾相接无隙无叠，但
    /// 上下沿是物理像素台阶，渐变退为 `paint.color` 的纯色（经 `Paint::gradient`
    /// 构造时即首个色标，与描边类图元一致）。正确但不精致，
    /// 是兜底而非占位。
    ///
    /// 扫描按**物理行**而不是逻辑行：逐逻辑行（每行 1 逻辑像素）配 `fill_rect` 时，边界被
    /// 吸附到整数物理像素，125% 下相邻两行之间会留出约 0.25 物理像素的缝，画出来是一道道横纹。
    fn fill_polygon(&mut self, contours: &[&[(f32, f32)]], paint: &Paint) {
        fill_polygon_rows(self, contours, paint);
    }
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
    /// 在 rect 内绘制文字（水平按 align；纵向装得下居中、装不下顶对齐向下溢出，见
    /// `TextEngine::draw` 的纵向契约）。无文字引擎时为空操作。
    ///
    /// 后端会按 rect 剔除不在可绘区域内的文字（局部重绘省排版），剔除时已把 rect 向下
    /// 放宽到约两行字高以容纳单行溢出。**多行文本要给足 rect 高度**：超出这一放宽的
    /// 溢出部分在局部重绘帧里可能整段被剔掉，而整窗帧上看不出来。
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

    /// `text` 单行排版的宽度（逻辑单位，含尾随空白，**不取整**）——自绘排版建字宽表用，
    /// 见 [`TextEngine::advance`](crate::text::TextEngine::advance)。控件在 `paint` 里只拿得到
    /// 画布、够不到文字引擎，故画布上再开一份。
    ///
    /// 量的是**本画布**画字用的那次排版：Windows D2D 后端按逻辑字号排版、由变换承担 DPI，
    /// 软件 / GPU 后端按物理字号排版，两者在字体 hinting 下可差零点几像素。所以建字宽表与
    /// 画字要用同一个画布，不要在布局期用 `TextEngine::advance`、绘制期再换画布量。
    /// `text` 不应含换行。
    ///
    /// 默认由 `measure_text` 的整数宽换算，只给测试画布兜底；三个真实后端都已覆盖。
    fn text_advance(&mut self, text: &str, ts: &crate::text::TextStyle) -> f32 {
        self.measure_text(text, ts).w as f32
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
    /// 「保留上一帧」这件事在两条后端上落点不同：软后端靠平台跨帧持有的这张 `Pixmap`
    /// 本身，GPU 后端靠目标自己的常驻色纹理——窗口 surface 的纹理是**轮转**的（Metal 的
    /// drawable 通常两三张），只画脏区的话其余区域会是两三帧之前的画面。默认按「有没有
    /// Pixmap」判，故 d2d 后端（两者都没有）恒 false。
    ///
    /// # 平台契约（返回 true 即是承诺这两条）
    ///
    /// 1. **缓冲跨帧持久**：不能每帧递一张新的。宿主的局部帧只写脏区，脏区之外直接
    ///    沿用里面已有的像素。
    /// 2. **缓冲重建的那一帧不得出局部帧**。两种做法都合规，选一种即可：
    ///    - 那一帧让本方法返回 `false`（GPU 后端走这条，见 `gpu::surface` 的 `seeded`）；
    ///    - 或在把目标交给 `render()` **之前**调 [`AppHandler::request_full_frame`]
    ///      （两条软后端走这条，见 win32 与 macOS 的 `fresh` 分支）。
    ///
    ///    新缓冲内容不完整，照常出局部帧的话，上屏后除脏区外全是底色。
    ///
    /// 宿主自己也会拦一道（`app/damage.rs` 拿本帧物理尺寸与**上一次渲染**的比），但它
    /// 只看得见「与上一次渲染尺寸不同」的重建。第 2 条仍然不可省，因为有两类重建落在
    /// 宿主的视野之外：
    ///
    /// - **尺寸压根没变的重建**，典型是 macOS 的 `degrade_to_software`——GPU 窗口降级到
    ///   软路径，窗口一点没变，pixmap 却是头一回分配；
    /// - **尺寸变了又变回来、期间一帧都没出的**（两次 `WM_SIZE` 之间没有 `WM_PAINT`）。
    ///
    /// 破坏第 2 条的症状极不显眼：只在「宿主手里恰好有小脏区」（聚焦文本框的光标闪烁
    /// 就够）时发作，静止界面完全正常。
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
    /// 软后端默认空操作：它的铺底与合成走宿主的子 pixmap 路径，不经过本方法。
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

/// 按四个象限各自裁剪、每个象限用该角的半径画一遍**统一半径**图元，拼出四角各异的形状。
///
/// 给 `fill_polygon` 只有逐行兜底的后端（GPU）用：每个象限里画的就是原生的圆角矩形，
/// 抗锯齿与统一半径版同档、渐变坐标按整个矩形算（四块拼起来是同一条渐变）。象限边界取
/// 整数逻辑坐标，两侧经同一个 `Rect::scaled` 物理化，共享边逐像素对齐，半透明色不会在
/// 中线叠出一道深线。
///
/// 某个角的半径越过所在边的一半时象限切不开（那段圆弧会伸进邻角的象限），返回 `false`
/// 由调用方改走折线路径。`draw(canvas, 本象限半径)` 画整个矩形，裁剪负责只留一角。
#[cfg_attr(not(gpu_backend), allow(dead_code))]
pub(crate) fn by_quadrants<C: Canvas + ?Sized>(
    canvas: &mut C,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    corners: Corners,
    mut draw: impl FnMut(&mut C, f32),
) -> bool {
    let k = corners.fit(w, h);
    if k.max() > w.min(h) / 2.0 {
        return false;
    }
    // 外缘多留 2 逻辑像素：裁剪只负责切中线，别切掉边缘的抗锯齿过渡带。
    let (l, t) = (x.floor() as i32 - 2, y.floor() as i32 - 2);
    let (r, b) = ((x + w).ceil() as i32 + 2, (y + h).ceil() as i32 + 2);
    let (mx, my) = ((x + w / 2.0).round() as i32, (y + h / 2.0).round() as i32);
    for (q, radius) in [
        (Rect::new(l, t, mx - l, my - t), k.top_left),
        (Rect::new(mx, t, r - mx, my - t), k.top_right),
        (Rect::new(mx, my, r - mx, b - my), k.bottom_right),
        (Rect::new(l, my, mx - l, b - my), k.bottom_left),
    ] {
        canvas.save();
        canvas.clip_rect(q);
        draw(canvas, radius);
        canvas.restore();
    }
    true
}

/// [`Canvas::fill_round_rect_corners`] 的折线实现（trait 默认实现与后端兜底共用）。
pub(crate) fn fill_round_rect_corners_poly<C: Canvas + ?Sized>(
    canvas: &mut C,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    corners: Corners,
    paint: &Paint,
) {
    let s = canvas.dpi_scale();
    let (x, y, w, h) = align_to_device(x, y, w, h, s);
    if w <= 0.0 || h <= 0.0 {
        return;
    }
    let outer = corners_contour(x, y, w, h, corners.fit(w, h), s);
    canvas.fill_polygon(&[&outer], paint);
}

/// [`Canvas::stroke_round_rect_corners`] 的折线实现（trait 默认实现与后端兜底共用）。
pub(crate) fn stroke_round_rect_corners_poly<C: Canvas + ?Sized>(
    canvas: &mut C,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    corners: Corners,
    width: f32,
    paint: &Paint,
) {
    let s = canvas.dpi_scale();
    let (x, y, w, h) = align_to_device(x, y, w, h, s);
    let width = width.min(w / 2.0).min(h / 2.0);
    if width <= 0.0 {
        return;
    }
    let c = corners.fit(w, h);
    let outer = corners_contour(x, y, w, h, c, s);
    let (iw, ih) = (w - 2.0 * width, h - 2.0 * width);
    if iw <= 0.0 || ih <= 0.0 {
        // 线宽吃满了整个框：没有内缘，就是一块实心。
        canvas.fill_polygon(&[&outer], paint);
        return;
    }
    let mut inner = corners_contour(
        x + width,
        y + width,
        iw,
        ih,
        c.offset(-width).fit(iw, ih),
        s,
    );
    // 内圈反向：非零环绕下抵消成洞，剩下的就是环带。
    inner.reverse();
    canvas.fill_polygon(&[&outer, &inner], paint);
}

/// 四角各自半径的圆角矩形轮廓（逻辑坐标，屏幕坐标系下顺时针）：直边两端点 + 每段圆弧
/// 按物理半径取样的折线。`corners` 须已 [`Corners::fit`]。
///
/// 取样密度按**物理**半径定：弧上每段的弦高（折线与真圆最大偏差）在 0.1 物理像素以内，
/// 肉眼与抗锯齿都分不出它与真圆；半径为 0 的角只出一个顶点（真直角）。
pub(crate) fn corners_contour(
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    corners: Corners,
    scale: f32,
) -> Vec<(f32, f32)> {
    use std::f32::consts::{FRAC_PI_2, PI};
    let mut pts = Vec::with_capacity(32);
    let mut arc = |cx: f32, cy: f32, r: f32, a0: f32| {
        if r <= 0.0 {
            // 直角：顶点就是圆心本身（r = 0 时圆心落在角上）。
            pts.push((cx, cy));
            return;
        }
        // 弦高 r·(1 − cos(θ/2)) ≤ 0.1px ⇒ θ ≤ 2·acos(1 − 0.1/r)。
        let pr = (r * scale).max(1.0);
        let step = 2.0 * (1.0 - (0.1 / pr).min(1.0)).acos();
        let n = ((FRAC_PI_2 / step).ceil() as usize).clamp(2, 64);
        for i in 0..=n {
            let a = a0 + FRAC_PI_2 * i as f32 / n as f32;
            pts.push((cx + r * a.cos(), cy + r * a.sin()));
        }
    };
    let (l, t, r, b) = (x, y, x + w, y + h);
    let c = corners;
    arc(r - c.top_right, t + c.top_right, c.top_right, -FRAC_PI_2);
    arc(r - c.bottom_right, b - c.bottom_right, c.bottom_right, 0.0);
    arc(
        l + c.bottom_left,
        b - c.bottom_left,
        c.bottom_left,
        FRAC_PI_2,
    );
    arc(l + c.top_left, t + c.top_left, c.top_left, PI);
    pts
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
