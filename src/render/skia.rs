//! tiny-skia 后端：把 `Canvas` 图元光栅化到 `Pixmap`（RGBA 预乘）。
//!
//! 支持矩形裁剪栈：用 alpha `Mask` 表示当前裁剪区，所有绘制传入栈顶 mask。

use std::cell::RefCell;
use std::collections::HashMap;

use tiny_skia::{
    FillRule, FilterQuality, GradientStop as SkStop, LineCap, LineJoin, LinearGradient, Mask,
    Paint as SkPaint, PathBuilder, Pixmap, PixmapPaint, Point as SkPoint, RadialGradient, Shader,
    SpreadMode, Stroke, Transform,
};

/// 阴影缓存键：(物理宽, 物理高, 圆角, 模糊半径, 颜色 RGBA)。
type ShadowKey = (i32, i32, i32, i32, u32);

thread_local! {
    /// 模糊后阴影 Pixmap 缓存。阴影几何帧间通常不变，缓存避免每帧重复 box-blur（卡顿主因）。
    static SHADOW_CACHE: RefCell<HashMap<ShadowKey, Pixmap>> = RefCell::new(HashMap::new());
}

/// 是否禁用阴影绘制（环境变量 WINDUI_NOSHADOW；低端机降级或排查阴影开销用）。读一次缓存。
/// `pub(crate)`：D2D 后端的 `draw_shadow` 复用同一开关，保证两后端阴影禁用语义一致。
pub(crate) fn shadows_disabled() -> bool {
    use std::sync::OnceLock;
    static D: OnceLock<bool> = OnceLock::new();
    *D.get_or_init(|| std::env::var("WINDUI_NOSHADOW").is_ok_and(|v| v != "0" && !v.is_empty()))
}

/// 构造一张模糊后的圆角矩形阴影 Pixmap（位置无关，可缓存复用）。
fn build_shadow_pixmap(
    tw: i32,
    th: i32,
    margin: i32,
    pw: f32,
    ph: f32,
    pr: f32,
    pblur: f32,
    color: Color,
) -> Pixmap {
    let mut tmp =
        Pixmap::new(tw as u32, th as u32).unwrap_or_else(|| Pixmap::new(1, 1).expect("1x1 pixmap"));
    if let Some(path) = rounded_rect_path(margin as f32, margin as f32, pw, ph, pr) {
        let mut sp = SkPaint::default();
        sp.set_color(to_sk_color(color));
        sp.anti_alias = true;
        tmp.fill_path(&path, &sp, FillRule::Winding, Transform::identity(), None);
    }
    let r = pblur.round() as usize;
    if r > 0 {
        box_blur(&mut tmp, r);
    }
    tmp
}

use super::image::{Fit, Image};
use super::{rounded_rect_path, Canvas, Paint};
use crate::geometry::{Color, Point, Rect};
use crate::spec::Align;
use crate::text::TextEngine;

/// 裁剪层：有效裁剪矩形（各级交集）+ 对应 alpha mask。
///
/// **不变量：`mask` 恒是 `rect` 对应的轴对齐矩形**（`clip_rect` 是唯一的入栈处，
/// 那里用 `push_rect` 填充、且不开抗锯齿）。`SkiaCanvas::fast_fill_rect` 依赖这一点
/// 直接对 `rect` 求交而**不读 `mask`**。若将来新增 `clip_path` 之类的非矩形裁剪，
/// 必须同时让快路认得出它并退回通用路径——否则那条路会静默画错：不报错、不 panic，
/// 只是裁剪失效，多出来的部分照画。
struct Clip {
    rect: Rect,
    mask: Mask,
}

/// 离屏合成层：与主缓冲同尺寸的透明子缓冲 + 整体不透明度。
struct Layer {
    pixmap: Pixmap,
    opacity: f32,
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
    /// 局部重绘原点（**逻辑坐标**）：pixmap 是脏区大小的子缓冲，其 (0,0) 对应世界 `offset`。
    /// 所有图元绘制时减去此偏移，使世界坐标落入子 pixmap。全窗重绘时为 (0,0)。
    offset: Point,
    /// 离屏合成层栈（子树 opacity 用）：非空时绘制重定向到栈顶层。
    layers: Vec<Layer>,
}

impl<'a> SkiaCanvas<'a> {
    /// 无文字能力（仅图形），scale=1。
    pub fn new(pixmap: &'a mut Pixmap) -> Self {
        Self {
            pixmap,
            engine: None,
            clips: Vec::new(),
            saves: Vec::new(),
            scale: 1.0,
            offset: Point::new(0, 0),
            layers: Vec::new(),
        }
    }

    /// 带文字引擎与 DPI 缩放（全窗重绘，无偏移）。
    pub fn with_text(pixmap: &'a mut Pixmap, engine: &'a mut dyn TextEngine, scale: f32) -> Self {
        Self::with_text_offset(pixmap, engine, scale, Point::new(0, 0))
    }

    /// 局部重绘：`offset`（逻辑坐标）为子 pixmap 在世界中的左上角。
    pub fn with_text_offset(
        pixmap: &'a mut Pixmap,
        engine: &'a mut dyn TextEngine,
        scale: f32,
        offset: Point,
    ) -> Self {
        Self {
            pixmap,
            engine: Some(engine),
            clips: Vec::new(),
            saves: Vec::new(),
            scale,
            offset,
            layers: Vec::new(),
        }
    }

    /// 纯色 paint（stroke/line 用；渐变在 fill 路径单独处理）。
    fn sk_paint(p: &Paint) -> SkPaint<'static> {
        let mut sp = SkPaint::default();
        sp.set_color(to_sk_color(p.color));
        sp.anti_alias = p.anti_alias;
        sp
    }

    /// fill 类 paint：有 gradient 时按 (x,y,w,h) 逻辑矩形构造渐变 shader，
    /// 坐标交由 self.tf() 统一缩放/平移（与 path 同一变换空间）。无 gradient 退纯色。
    fn fill_paint(p: &Paint, x: f32, y: f32, w: f32, h: f32) -> SkPaint<'static> {
        let mut sp = SkPaint::default();
        match p.gradient.as_ref().and_then(|g| sk_shader(g, x, y, w, h)) {
            Some(s) => sp.shader = s,
            None => sp.set_color(to_sk_color(p.color)),
        }
        sp.anti_alias = p.anti_alias;
        sp
    }

    /// 在当前绘制目标（栈顶离屏层，或主缓冲）上填充路径，带栈顶裁剪 mask。
    fn fill_path_on_target(&mut self, path: &tiny_skia::Path, sp: &SkPaint, tf: Transform) {
        let mask = self.clips.last().map(|c| &c.mask);
        match self.layers.last_mut() {
            Some(l) => l.pixmap.fill_path(path, sp, FillRule::Winding, tf, mask),
            None => self.pixmap.fill_path(path, sp, FillRule::Winding, tf, mask),
        };
    }

    /// 当前绘制目标缓冲（栈顶离屏层，或主缓冲）。仅用于无 self.clips/self.engine
    /// 并发借用的场景（draw_image 自带局部 mask）；其余处内联 match 以满足借用拆分。
    fn target_pixmap(&mut self) -> &mut Pixmap {
        match self.layers.last_mut() {
            Some(l) => &mut l.pixmap,
            None => self.pixmap,
        }
    }

    /// 在当前绘制目标上描边路径，带栈顶裁剪 mask。
    fn stroke_path_on_target(
        &mut self,
        path: &tiny_skia::Path,
        sp: &SkPaint,
        stroke: &Stroke,
        tf: Transform,
    ) {
        let mask = self.clips.last().map(|c| &c.mask);
        match self.layers.last_mut() {
            Some(l) => l.pixmap.stroke_path(path, sp, stroke, tf, mask),
            None => self.pixmap.stroke_path(path, sp, stroke, tf, mask),
        };
    }

    /// 纯色轴对齐矩形的快路径：跳过路径光栅化，直接按行写像素。
    ///
    /// tiny-skia 只对**不透明纯色**有 memset 级快路；一旦半透明就落进通用 raster
    /// pipeline（每像素转 f32 过一遍 stage 链）。实测 1920×1080 全屏填充：不透明
    /// 0.22ms，半透明 3.94ms——同样的 source-over 直接在整数域按行做只要 0.36ms。
    /// 而大面积半透明叠层（RoleAlpha 淡底、遮罩层、卡片底）恰是 UI 上最常见的填充，
    /// 故单独特化。不透明一支同样走这里：省掉建路径与扫描线的固定开销。
    ///
    /// 能走快路的前提是**物理边界落在整数像素上**——那时矩形没有抗锯齿边，整行都是
    /// 同一个值，才可以整段写。`align_to_device` 已把逻辑坐标对齐到 1/scale 网格，
    /// 这里再校验一次；对不上就返回 `false`，由调用方落回通用路径。宁可慢，也不能
    /// 画出与通用路径不同的边缘。
    ///
    /// 返回 `true` 表示本次填充已完成（含「被裁剪成空、无需落笔」）。
    ///
    /// `radius` 为逻辑圆角半径，0 即直角。圆角带内逐像素按到圆心的距离求覆盖率做
    /// 抗锯齿，其余部分整行写——真实界面里圆角带只占极小面积（`r=10` 时四角合计约
    /// 400 像素），而通用路径要为这点弧线把**整个**矩形拖进扫描线光栅 + mask 采样：
    /// 实测一张 1884×412 的不透明圆角卡片要 20.2ms（26ns/像素），而全屏纯色填充只要
    /// 0.1ns/像素。这条快路针对的正是这个组合。
    // 用 chunks_exact_mut 而非 as_chunks_mut：后者要 Rust 1.88，而本 crate 已发布到
    // crates.io 且未声明 rust-version，换过去会让老工具链的下游直接编译失败。两者
    // 生成的代码等价。
    #[allow(clippy::chunks_exact_to_as_chunks)]
    #[allow(clippy::too_many_arguments)]
    fn fast_fill_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        anti_alias: bool,
        color: Color,
    ) -> bool {
        let s = self.scale;
        // 非有限坐标必须挡在门外。EPS 那道闸门对它们是**失效**的：w 为无穷时
        // 「差值取整再比较」得到 NaN，而 NaN 与 EPS 的任何比较都是 false——闸门放行，
        // 无穷取整再饱和成 i32::MAX，于是一路写到缓冲右边缘。通用路径这时什么都不画
        // （tiny-skia 的 Rect 构造对非有限值返回 None），两者行为相反：上游一个除零
        // 原本表现为「这块没画出来」，会变成「大片被这个颜色刷掉」。
        // NaN 那一路本就安全：NaN 转 i32 得 0，使 x0 >= x1，不落笔。
        // is_finite 必须排在比较之前：NaN 与任何数比较都是 false，先比大小会把它放行。
        if s <= 0.0
            || !x.is_finite()
            || !y.is_finite()
            || !w.is_finite()
            || !h.is_finite()
            || !radius.is_finite()
            || w <= 0.0
            || h <= 0.0
            || radius < 0.0
        {
            return false;
        }
        let (ox, oy) = (self.offset.x as f32, self.offset.y as f32);
        let fx0 = (x - ox) * s;
        let fy0 = (y - oy) * s;
        let fx1 = (x + w - ox) * s;
        let fy1 = (y + h - oy) * s;
        // 容差取 0.01 物理像素：align_to_device 的浮点往返只会带来远小于此的误差，
        // 而真正的亚像素边界（非 25% 倍数缩放等）会明显超出，从而落回通用路径。
        const EPS: f32 = 0.01;
        if (fx0 - fx0.round()).abs() > EPS
            || (fy0 - fy0.round()).abs() > EPS
            || (fx1 - fx1.round()).abs() > EPS
            || (fy1 - fy1.round()).abs() > EPS
        {
            return false;
        }
        let (mut x0, mut y0) = (fx0.round() as i32, fy0.round() as i32);
        let (mut x1, mut y1) = (fx1.round() as i32, fy1.round() as i32);
        // 圆角几何锚在**未裁剪**的矩形上：裁剪只决定写哪些像素，不改变形状。若拿裁剪后
        // 的边界去算圆心，被滚动容器切到一半的卡片就会在切口处冒出一个本不存在的圆角。
        let (rx0, ry0, rx1, ry1) = (x0, y0, x1, y1);
        let pr = (radius * s)
            .min((rx1 - rx0) as f32 * 0.5)
            .min((ry1 - ry0) as f32 * 0.5)
            .max(0.0);
        // 裁剪求交：`clip_rect` 是唯一的裁剪入口，且只产生轴对齐的矩形 mask，
        // 故 `Clip.rect` 已精确表达裁剪范围——快路无需读 mask。
        if let Some(c) = self.clips.last() {
            let pc = c.rect.offset(-self.offset.x, -self.offset.y).scaled(s);
            x0 = x0.max(pc.x);
            y0 = y0.max(pc.y);
            // 饱和加：Rect::scaled 的 f32→i32 是饱和转换，极端输入下 pc.x 可能已是
            // i32::MAX，裸加会在 debug 档 panic。当前所有 clip_rect 调用方传的都是
            // 有界的控件矩形，够不着这里；但通用路径那侧走 mask，根本没有这个算式。
            x1 = x1.min(pc.x.saturating_add(pc.w));
            y1 = y1.min(pc.y.saturating_add(pc.h));
        }
        let target = self.target_pixmap();
        let (pw, ph) = (target.width() as i32, target.height() as i32);
        x0 = x0.max(0);
        y0 = y0.max(0);
        x1 = x1.min(pw);
        y1 = y1.min(ph);
        if x0 >= x1 || y0 >= y1 || color.a == 0 {
            return true;
        }
        // Pixmap 存**预乘** RGBA8，故源色先预乘再参与合成。
        let a = color.a as u32;
        let src = [
            mul255(color.r as u32, a) as u8,
            mul255(color.g as u32, a) as u8,
            mul255(color.b as u32, a) as u8,
            color.a,
        ];
        let stride = pw as usize * 4;
        let (lo, hi) = (x0 as usize * 4, x1 as usize * 4);
        let data = target.data_mut();
        // 圆角带的四条边界（物理、含 0.5 像素中心偏移的比较基准）。直段行落在
        // [top, bot] 之间，整行同值；圆角行只有左右各 pr 宽需要逐像素。
        let (top, bot) = (ry0 as f32 + pr, ry1 as f32 - pr);
        let (left, right) = (rx0 as f32 + pr, rx1 as f32 - pr);
        for y in y0..y1 {
            let base = y as usize * stride;
            let fy = y as f32 + 0.5;
            let ccy = if fy < top {
                Some(top)
            } else if fy > bot {
                Some(bot)
            } else {
                None
            };
            let Some(ccy) = ccy else {
                // 直段：整行同值，走整段写。
                if color.a == 255 {
                    for p in data[base + lo..base + hi].chunks_exact_mut(4) {
                        p.copy_from_slice(&src);
                    }
                } else {
                    let ia = 255 - a;
                    for p in data[base + lo..base + hi].chunks_exact_mut(4) {
                        p[0] = src[0] + mul255(p[0] as u32, ia) as u8;
                        p[1] = src[1] + mul255(p[1] as u32, ia) as u8;
                        p[2] = src[2] + mul255(p[2] as u32, ia) as u8;
                        p[3] = src[3] + mul255(p[3] as u32, ia) as u8;
                    }
                }
                continue;
            };
            // 圆角行的中段（左右圆心之间）覆盖率恒为 1，和直段一样可以整段写。不拆出来
            // 的话，一张 1884×412、r=10 的卡片会有约 2×10×1864 ≈ 3.7 万个满覆盖像素
            // 白白走逐像素分支 + 四次乘法——而这条快路的全部理由就是常数因子。
            //
            // 满覆盖的条件是 left <= x+0.5 <= right，即 x ∈ [ceil(left-0.5), floor(right-0.5)]。
            let mid0 = (left - 0.5).ceil().max(x0 as f32) as i32;
            let mid1 = ((right - 0.5).floor() + 1.0).min(x1 as f32) as i32;
            if mid1 > mid0 {
                let (mlo, mhi) = (mid0 as usize * 4, mid1 as usize * 4);
                if color.a == 255 {
                    for p in data[base + mlo..base + mhi].chunks_exact_mut(4) {
                        p.copy_from_slice(&src);
                    }
                } else {
                    let ia = 255 - a;
                    for p in data[base + mlo..base + mhi].chunks_exact_mut(4) {
                        p[0] = src[0] + mul255(p[0] as u32, ia) as u8;
                        p[1] = src[1] + mul255(p[1] as u32, ia) as u8;
                        p[2] = src[2] + mul255(p[2] as u32, ia) as u8;
                        p[3] = src[3] + mul255(p[3] as u32, ia) as u8;
                    }
                }
            }
            // 两侧的圆角带：按到所属圆心的距离求覆盖率。
            for x in x0..x1 {
                // 中段已整段写过，跳过。
                if x >= mid0 && x < mid1 {
                    continue;
                }
                let fx = x as f32 + 0.5;
                let ccx = if fx < left {
                    Some(left)
                } else if fx > right {
                    Some(right)
                } else {
                    None
                };
                let cov = match ccx {
                    None => 255,
                    // 关掉抗锯齿时按像素中心是否落在圆内硬判定，与通用路径的非 AA
                    // 光栅同口径。这条语义在 GPU 后端已被
                    // `anti_alias_off_has_no_transition_band` 测成契约（边界上非背景
                    // 即图元色），软件后端不能自作主张给它加一条过渡带。
                    Some(ccx) if !anti_alias => {
                        let (dx, dy) = (fx - ccx, fy - ccy);
                        if dx * dx + dy * dy <= pr * pr {
                            255
                        } else {
                            0
                        }
                    }
                    Some(ccx) => {
                        // 4×4 超采样求覆盖率。先前用 `pr + 0.5 - d` 的线性近似，在弧线
                        // 45° 附近会偏出十几个色阶（实测与通用路径最大差 17）——弧在一个
                        // 像素尺度上并不够直。超采样把误差压回 ±1 量级，代价是每个边缘
                        // 像素 16 次平方比较，而圆角带本身只有四角那点面积。
                        let mut hit = 0u32;
                        for sy in 0..4 {
                            let py = fy - 0.5 + (sy as f32 + 0.5) * 0.25 - ccy;
                            for sx in 0..4 {
                                let px = fx - 0.5 + (sx as f32 + 0.5) * 0.25 - ccx;
                                if px * px + py * py <= pr * pr {
                                    hit += 1;
                                }
                            }
                        }
                        hit * 255 / 16
                    }
                };
                // 有效 alpha = 源 alpha × 覆盖率；为 0 的像素在圆角外，不落笔。
                let sa = if cov == 255 { a } else { mul255(a, cov) };
                if sa == 0 {
                    continue;
                }
                let off = base + x as usize * 4;
                let p = &mut data[off..off + 4];
                if sa == 255 {
                    p.copy_from_slice(&[color.r, color.g, color.b, 255]);
                } else {
                    let ia = 255 - sa;
                    p[0] = mul255(color.r as u32, sa) as u8 + mul255(p[0] as u32, ia) as u8;
                    p[1] = mul255(color.g as u32, sa) as u8 + mul255(p[1] as u32, ia) as u8;
                    p[2] = mul255(color.b as u32, sa) as u8 + mul255(p[2] as u32, ia) as u8;
                    p[3] = sa as u8 + mul255(p[3] as u32, ia) as u8;
                }
            }
        }
        true
    }

    /// 逻辑→物理变换：缩放后平移 -offset（物理像素），把世界坐标映射进子 pixmap。
    fn tf(&self) -> Transform {
        Transform::from_scale(self.scale, self.scale).post_translate(
            -self.offset.x as f32 * self.scale,
            -self.offset.y as f32 * self.scale,
        )
    }
}

impl Canvas for SkiaCanvas<'_> {
    fn dpi_scale(&self) -> f32 {
        self.scale
    }

    /// 子 pixmap 覆盖的世界范围。全窗帧即整窗（等于不剔除），局部帧就是那块脏区。
    ///
    /// 向外各放一像素：物理→逻辑的除法有取整误差，压边的图元不能因为算窄了被丢掉。
    fn cull_rect(&self) -> Option<Rect> {
        let s = if self.scale > 0.0 { self.scale } else { 1.0 };
        let w = (self.pixmap.width() as f32 / s).ceil() as i32;
        let h = (self.pixmap.height() as f32 / s).ceil() as i32;
        Some(Rect::new(self.offset.x, self.offset.y, w, h).inflate(1))
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, paint: &Paint) {
        self.fill_round_rect(x, y, w, h, 0.0, paint);
    }

    fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, paint: &Paint) {
        let _g = super::prof::scope(super::prof::FILL);
        // 对齐物理像素整数坐标，**与 `stroke_round_rect` 用同一套公式**。
        //
        // 缺了这一步的症状：描边那侧早就对齐了（见其注释），填充这侧却按原始亚像素坐标
        // 走，于是 125% / 150% 这类非整数 DPI 下，同一个矩形的底色边界与它的边框错开半个
        // 物理像素——看起来就是「圆角的边框有点偏」。两者必须同源，单独对齐一侧只会把
        // 错位从「都糊」变成「错开」。
        //
        // 对无边框的填充同样是改善：边界落在整数像素上，不再有一列半透明的抗锯齿边。
        let (x, y, w, h) = crate::render::align_to_device(x, y, w, h, self.scale);
        // 纯色（含圆角）走特化快路，见 `fast_fill_rect`。渐变仍交通用路径：每像素都要
        // 算 shader，不是「整行同值」，特化占不到便宜。
        if paint.gradient.is_none()
            && self.fast_fill_rect(x, y, w, h, radius, paint.anti_alias, paint.color)
        {
            return;
        }
        if let Some(path) = rounded_rect_path(x, y, w, h, radius) {
            let sp = Self::fill_paint(paint, x, y, w, h);
            let tf = self.tf();
            self.fill_path_on_target(&path, &sp, tf);
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
        let _g = super::prof::scope(super::prof::STROKE);
        // 对齐物理像素整数坐标（与 D2DCanvas::stroke_round_rect 同源）：矩形四边乘以 scale
        // 取整后还原，使描边中心（边 + half_width）在物理坐标上落在半像素（n+0.5），描边两侧
        // 各 0.5px 恰好覆盖完整一列物理像素，消除非整数 DPI（125%/150%/175% 等）下的亚像素
        // 抗锯齿模糊。tf() 的局部重绘 offset 已按 4 逻辑像素网格对齐（offset×scale 为整数），
        // 故此处对齐逻辑坐标后经变换仍落在物理整数。
        let (x, y, w, h) = crate::render::align_to_device(x, y, w, h, self.scale);

        let width = width.min(w / 2.0).min(h / 2.0).max(0.0);
        let half = width / 2.0;
        if let Some(path) = rounded_rect_path(
            x + half,
            y + half,
            w - width,
            h - width,
            (radius - half).max(0.0),
        ) {
            let sp = Self::sk_paint(paint);
            let stroke = Stroke {
                width,
                ..Default::default()
            };
            let tf = self.tf();
            self.stroke_path_on_target(&path, &sp, &stroke, tf);
        }
    }

    fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, paint: &Paint) {
        let _g = super::prof::scope(super::prof::STROKE);
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        if let Some(path) = pb.finish() {
            let sp = Self::sk_paint(paint);
            let stroke = Stroke {
                width,
                line_cap: LineCap::Butt,
                ..Default::default()
            };
            let tf = self.tf();
            self.stroke_path_on_target(&path, &sp, &stroke, tf);
        }
    }

    fn draw_polyline(&mut self, pts: &[(f32, f32)], width: f32, paint: &Paint) {
        if pts.len() < 2 {
            return;
        }
        let _g = super::prof::scope(super::prof::STROKE);
        let mut pb = PathBuilder::new();
        pb.move_to(pts[0].0, pts[0].1);
        for &(x, y) in &pts[1..] {
            pb.line_to(x, y);
        }
        if let Some(path) = pb.finish() {
            let sp = Self::sk_paint(paint);
            // 端头保持平头（与 `draw_line` 一致，换成圆头会改变既有图形的外观）。
            // 拐点用 **round** 而非 tiny-skia 默认的 miter：默认实现补的那个圆点
            // 恰好就是 round join 的定义域，两边取同一种连接，软件路径与 D2D/wgpu
            // 才画得出同一个形状。笔宽 2px 的对勾上，round 与 miter 的差别不足半像素。
            let stroke = Stroke {
                width,
                line_cap: LineCap::Butt,
                line_join: LineJoin::Round,
                ..Default::default()
            };
            let tf = self.tf();
            self.stroke_path_on_target(&path, &sp, &stroke, tf);
        }
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, paint: &Paint) {
        let _g = super::prof::scope(super::prof::FILL);
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            let sp = Self::fill_paint(paint, cx - r, cy - r, 2.0 * r, 2.0 * r);
            let tf = self.tf();
            self.fill_path_on_target(&path, &sp, tf);
        }
    }

    fn draw_shadow(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        blur: f32,
        color: Color,
    ) {
        let _g = super::prof::scope(super::prof::SHADOW);
        if color.a == 0 || w <= 0.0 || h <= 0.0 || shadows_disabled() {
            return;
        }
        let s = self.scale;
        // 逻辑→物理（含局部重绘 offset）。
        let px = (x - self.offset.x as f32) * s;
        let py = (y - self.offset.y as f32) * s;
        let pw = w * s;
        let ph = h * s;
        let pr = (radius * s).max(0.0);
        let pblur = (blur * s).max(0.0);
        // 3 趟 box-blur（见 `box_blur`）**每趟各扩散 pblur**，总扩散 3×pblur——余量必须给足这么多。
        //
        // ★ 曾按 2× 留余量（注释写着"实际可见扩散约 1.5×半径"），那是把单趟当成了全部：
        //   模糊尾部在 pixmap 边界处被硬截断，阴影最外圈留下一道直角硬边。半径越大越明显，
        //   在远程桌面下尤其扎眼——RDP 强制走本软后端（见 platform/win32/mod.rs 的后端选择，
        //   flip-model swapchain 在远程会话不可用），而有损压缩会把这道本就突兀的边缘糊成块。
        let margin = (pblur * 3.0).ceil() as i32 + 1;
        let tw = (pw.ceil() as i32 + 2 * margin).max(1);
        let th = (ph.ceil() as i32 + 2 * margin).max(1);
        // 体量保护：超大投影直接跳过（避免离屏分配爆炸）。
        if tw > 8192 || th > 8192 {
            return;
        }
        // 取/建缓存的模糊阴影（位置无关）：避免每帧重复 box-blur，且直接从缓存合成（不 clone，
        // 省去每帧 memcpy）。借用拆分：src 借缓存、mask 借 self.clips、目标借 self.layers/pixmap，
        // 分属不同对象/字段，可并存。
        let color_key = ((color.r as u32) << 24)
            | ((color.g as u32) << 16)
            | ((color.b as u32) << 8)
            | color.a as u32;
        let key = (tw, th, pr.round() as i32, pblur.round() as i32, color_key);
        // 合成到主缓冲：左上角对齐投影矩形外扩 margin 处；受当前裁剪 mask 约束（滚动视口）。
        let dx = px.floor() as i32 - margin;
        let dy = py.floor() as i32 - margin;
        // 性能：阴影物理边界完全落在当前裁剪矩形内时，mask 裁不掉任何东西——跳过带 mask 的
        // 慢合成路径（大阴影 + 全窗 mask 的逐像素采样是卡顿主因），仅在跨裁剪边界时才用 mask。
        let shadow_inside = match self.clips.last() {
            Some(c) => {
                let cr = c
                    .rect
                    .offset(-self.offset.x, -self.offset.y)
                    .scaled(self.scale);
                dx >= cr.x && dy >= cr.y && dx + tw <= cr.x + cr.w && dy + th <= cr.y + cr.h
            }
            None => true,
        };
        SHADOW_CACHE.with(|cell| {
            let mut cache = cell.borrow_mut();
            // 防无界增长：不同尺寸有限，超阈值整体清空重建。
            if cache.len() > 128 {
                cache.clear();
            }
            let src = cache
                .entry(key)
                .or_insert_with(|| build_shadow_pixmap(tw, th, margin, pw, ph, pr, pblur, color));
            let mask = if shadow_inside {
                None
            } else {
                self.clips.last().map(|c| &c.mask)
            };
            let pp = PixmapPaint::default();
            match self.layers.last_mut() {
                Some(l) => {
                    l.pixmap
                        .draw_pixmap(dx, dy, src.as_ref(), &pp, Transform::identity(), mask)
                }
                None => {
                    self.pixmap
                        .draw_pixmap(dx, dy, src.as_ref(), &pp, Transform::identity(), mask)
                }
            };
        });
    }

    fn draw_image(&mut self, img: &Image, dst: Rect, fit: Fit, radius: f32, opacity: f32) {
        let _g = super::prof::scope(super::prof::IMAGE);
        let opacity = opacity.clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return;
        }
        // 逻辑 dst → 物理像素（与图形/裁剪同源的边界取整）；局部重绘减 offset 落入子 pixmap。
        let pdst = dst
            .offset(-self.offset.x, -self.offset.y)
            .scaled(self.scale);
        if pdst.is_empty() {
            return;
        }
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let (pw, ph) = (pdst.w as f32, pdst.h as f32);
        let (px, py) = (pdst.x as f32, pdst.y as f32);

        // 按 fit 求缩放因子与绘制原点（均在物理空间）。
        let (sx, sy) = match fit {
            Fit::Fill => (pw / iw, ph / ih),
            Fit::Contain => {
                let s = (pw / iw).min(ph / ih);
                (s, s)
            }
            Fit::Cover => {
                let s = (pw / iw).max(ph / ih);
                (s, s)
            }
            // 1 图片像素 = 1 逻辑 dp → 物理为 ×scale。
            Fit::None => (self.scale, self.scale),
        };
        let (mut sx, mut sy) = (sx, sy);
        let (mut dw, mut dh) = (iw * sx, ih * sy);
        // 物理尺寸与源图相差不足 1 像素时吸附为 1:1：DPI 感知的矢量图标经此走上纯
        // blit 路径，不再被双线性重采样摊糊；`scaled()` 四边各自 round 带来的 ±1
        // 误差也在此吸收（否则细描边会被 0.97 倍这种"几乎 1:1"的缩放糊掉）。
        if (dw - iw).abs() < 1.0 && (dh - ih).abs() < 1.0 {
            sx = 1.0;
            sy = 1.0;
            dw = iw;
            dh = ih;
        }
        // 在 dst 框内居中（Cover/None 的溢出由裁剪 mask 收口）。落点取整到物理像素：
        // 尺寸对上了但平移带半像素，双线性同样会把 1:1 的图糊掉。
        let tx = (px + (pw - dw) / 2.0).round();
        let ty = (py + (ph - dh) / 2.0).round();
        let transform = Transform::from_scale(sx, sy).post_translate(tx, ty);

        // 裁剪 mask：dst 圆角矩形 ∩ 当前裁剪区。radius<=0 时退化为矩形。
        let (mw, mh) = (self.pixmap.width(), self.pixmap.height());
        let Some(mut mask) = Mask::new(mw, mh) else {
            return;
        };
        let pr = (radius * self.scale).min(pw / 2.0).min(ph / 2.0).max(0.0);
        let Some(path) = rounded_rect_path(px, py, pw, ph, pr) else {
            return;
        };
        mask.fill_path(&path, FillRule::Winding, true, Transform::identity());
        // 与当前裁剪矩形求交（滚动视口等）；当前裁剪皆为矩形（见 `Clip` 的不变量）。
        //
        // **不能用 `Mask::intersect_path`**：那是整幅 mask 的操作，成本只与窗口大小有关、
        // 与图片大小无关——1920×1080 下实测 2.2ms/次，占单张图片绘制的 83%，而真正的
        // 像素搬运只要 0.36ms。一个图片 tab 里 17 张小图就是 37ms 白付，窗口拖得越大越卡。
        //
        // 这里利用一个性质把它降到 O(dst)：`fill_path` 之后 mask **只有 dst 那一块非零**，
        // 其余恒为 0，而 0 与任何东西求交还是 0。于是只需在 dst 区域内把裁剪矩形之外的
        // 像素清零；dst 整个落在裁剪区内时（最常见）更是一点都不用动。
        if let Some(c) = self.clips.last() {
            let cr = c
                .rect
                .offset(-self.offset.x, -self.offset.y)
                .scaled(self.scale);
            if cr.is_empty() {
                return;
            }
            let (cx0, cy0) = (cr.x, cr.y);
            let (cx1, cy1) = (cr.x + cr.w, cr.y + cr.h);
            // mask 非零区域 = dst 矩形（圆角只会让它更小），钳进 mask 边界。
            let bx0 = pdst.x.max(0);
            let by0 = pdst.y.max(0);
            let bx1 = (pdst.x + pdst.w).min(mw as i32);
            let by1 = (pdst.y + pdst.h).min(mh as i32);
            let fully_inside = cx0 <= bx0 && cy0 <= by0 && cx1 >= bx1 && cy1 >= by1;
            if !fully_inside && bx1 > bx0 && by1 > by0 {
                let stride = mw as usize;
                let data = mask.data_mut();
                for y in by0..by1 {
                    let row = y as usize * stride;
                    if y < cy0 || y >= cy1 {
                        // 整行都在裁剪区外。
                        data[row + bx0 as usize..row + bx1 as usize].fill(0);
                        continue;
                    }
                    // 行内左右两侧超出裁剪区的部分。
                    let l = cx0.clamp(bx0, bx1);
                    data[row + bx0 as usize..row + l as usize].fill(0);
                    let r = cx1.clamp(bx0, bx1);
                    data[row + r as usize..row + bx1 as usize].fill(0);
                }
            }
        }

        let paint = PixmapPaint {
            opacity,
            quality: FilterQuality::Bilinear,
            ..Default::default()
        };
        let img_ref = img.pixmap();
        self.target_pixmap()
            .draw_pixmap(0, 0, img_ref.as_ref(), &paint, transform, Some(&mask));
    }

    fn draw_text(
        &mut self,
        text: &str,
        rect: Rect,
        color: Color,
        align: Align,
        ts: &crate::text::TextStyle,
    ) {
        let _g = super::prof::scope(super::prof::TEXT);
        // 传逻辑 rect/size/clip；引擎内部持有 scale 自行物理化（与 measure 同源）。
        // 局部重绘时减去 offset（逻辑），使引擎物理化后落入子 pixmap（×scale 与图元同源）。
        let off = self.offset;
        let rect = rect.offset(-off.x, -off.y);
        // 剔除：物理矩形与（子）pixmap 边界无交集则跳过引擎排版（局部重绘省去离屏文字的 COM 开销）。
        let bounds = Rect::new(
            0,
            0,
            self.pixmap.width() as i32,
            self.pixmap.height() as i32,
        );
        if rect.scaled(self.scale).intersect(&bounds).is_empty() {
            return;
        }
        let clip = self.clips.last().map(|c| c.rect.offset(-off.x, -off.y));
        // 绘制目标：栈顶离屏层或主缓冲（与 engine 借用分属不同字段，可并存）。
        let target: &mut Pixmap = match self.layers.last_mut() {
            Some(l) => &mut l.pixmap,
            None => self.pixmap,
        };
        if let Some(engine) = self.engine.as_deref_mut() {
            engine.draw(target, text, rect, color, align, ts, clip);
        }
    }

    fn measure_text(&mut self, text: &str, ts: &crate::text::TextStyle) -> crate::geometry::Size {
        // 逻辑入参；引擎内部物理测量后 /scale 回逻辑，与正文绘制度量同源。
        match self.engine.as_deref_mut() {
            Some(engine) => engine.measure(text, ts, None),
            None => crate::geometry::Size::new(
                (text.chars().count() as f32 * ts.size * 0.6).ceil() as i32,
                ts.line_height_px().unwrap_or(ts.size).ceil() as i32,
            ),
        }
    }

    fn text_line_metrics(
        &mut self,
        text: &str,
        ts: &crate::text::TextStyle,
    ) -> crate::text::LineMetrics {
        // 有引擎时取精确基线；无引擎沿用 trait 默认的 0.8 近似（与 Null 测量一致）。
        match self.engine.as_deref_mut() {
            Some(engine) => engine.line_metrics(text, ts),
            None => {
                let h = ts.line_height_px().unwrap_or(ts.size);
                crate::text::LineMetrics {
                    ascent: h * 0.8,
                    descent: h * 0.2,
                }
            }
        }
    }

    fn measure_text_wrapped(
        &mut self,
        text: &str,
        ts: &crate::text::TextStyle,
        max_width: f32,
    ) -> crate::geometry::Size {
        // 与 measure_text 同源，仅多传 max_width 触发引擎按宽度换行。
        match self.engine.as_deref_mut() {
            Some(engine) => engine.measure(text, ts, Some(max_width)),
            None => {
                // 无引擎的粗略估算：按等宽字符估算每行字数换行，行高按行数累加。
                let per_line = ((max_width / (ts.size * 0.6)).floor() as usize).max(1);
                let chars = text.chars().count().max(1);
                let lines = chars.div_ceil(per_line).max(1);
                let line_h = ts.line_height_px().unwrap_or(ts.size);
                crate::geometry::Size::new(
                    max_width.ceil() as i32,
                    (line_h.ceil() as i32) * lines as i32,
                )
            }
        }
    }

    fn push_layer(&mut self, opacity: f32) {
        let (w, h) = (self.pixmap.width(), self.pixmap.height());
        // 与主缓冲同尺寸的透明层；分配失败时退化为 1×1/0 透明度（不可见但保持栈平衡）。
        let layer = match Pixmap::new(w, h) {
            Some(pm) => Layer {
                pixmap: pm,
                opacity: opacity.clamp(0.0, 1.0),
            },
            None => Layer {
                pixmap: Pixmap::new(1, 1).unwrap(),
                opacity: 0.0,
            },
        };
        self.layers.push(layer);
    }

    fn pop_layer(&mut self) {
        if let Some(layer) = self.layers.pop() {
            let pp = PixmapPaint {
                opacity: layer.opacity,
                ..Default::default()
            };
            let src = layer.pixmap;
            match self.layers.last_mut() {
                Some(parent) => {
                    parent
                        .pixmap
                        .draw_pixmap(0, 0, src.as_ref(), &pp, Transform::identity(), None)
                }
                None => {
                    self.pixmap
                        .draw_pixmap(0, 0, src.as_ref(), &pp, Transform::identity(), None)
                }
            };
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
        let _g = super::prof::scope(super::prof::CLIP);
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
            // 局部重绘时减 offset（逻辑）再物理化，使 mask 落入子 pixmap。
            let peff = eff
                .offset(-self.offset.x, -self.offset.y)
                .scaled(self.scale);
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

/// `x × a / 255`（x、a 均在 0..=255），无除法。
///
/// 全定义域内与真除等价（含两端：`mul255(255,255)==255`、`mul255(x,0)==0`），
/// 供 [`SkiaCanvas::fast_fill_rect`] 的预乘与合成使用。
#[inline(always)]
fn mul255(x: u32, a: u32) -> u32 {
    let t = x * a + 128;
    (t + (t >> 8)) >> 8
}

/// 对预乘 RGBA8 像素做 3 趟可分离 box-blur（≈高斯）。半径 0 时空操作。
/// 用于浮层投影的离屏柔化；预乘空间内逐通道线性平均，足够投影用。
fn box_blur(pm: &mut Pixmap, radius: usize) {
    if radius == 0 {
        return;
    }
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    if w == 0 || h == 0 {
        return;
    }
    for _ in 0..3 {
        let src = pm.data().to_vec();
        blur_h(&src, pm.data_mut(), w, h, radius);
        let src = pm.data().to_vec();
        blur_v(&src, pm.data_mut(), w, h, radius);
    }
}

/// 水平方向 box-blur（滑动窗口运行和，O(w)/行；边缘窗口收窄即边界 clamp 平均）。
fn blur_h(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    for y in 0..h {
        let base = y * w;
        let mut acc = [0u32; 4];
        let mut n = 0u32;
        for xx in 0..=r.min(w - 1) {
            let i = (base + xx) * 4;
            for c in 0..4 {
                acc[c] += src[i + c] as u32;
            }
            n += 1;
        }
        for x in 0..w {
            let o = (base + x) * 4;
            for c in 0..4 {
                dst[o + c] = (acc[c] / n) as u8;
            }
            let add = x + r + 1;
            if add < w {
                let i = (base + add) * 4;
                for c in 0..4 {
                    acc[c] += src[i + c] as u32;
                }
                n += 1;
            }
            if x >= r {
                let i = (base + (x - r)) * 4;
                for c in 0..4 {
                    acc[c] -= src[i + c] as u32;
                }
                n -= 1;
            }
        }
    }
}

/// 垂直方向 box-blur（滑动窗口运行和，O(h)/列）。
fn blur_v(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    for x in 0..w {
        let mut acc = [0u32; 4];
        let mut n = 0u32;
        for yy in 0..=r.min(h - 1) {
            let i = (yy * w + x) * 4;
            for c in 0..4 {
                acc[c] += src[i + c] as u32;
            }
            n += 1;
        }
        for y in 0..h {
            let o = (y * w + x) * 4;
            for c in 0..4 {
                dst[o + c] = (acc[c] / n) as u8;
            }
            let add = y + r + 1;
            if add < h {
                let i = (add * w + x) * 4;
                for c in 0..4 {
                    acc[c] += src[i + c] as u32;
                }
                n += 1;
            }
            if y >= r {
                let i = ((y - r) * w + x) * 4;
                for c in 0..4 {
                    acc[c] -= src[i + c] as u32;
                }
                n -= 1;
            }
        }
    }
}

/// 把归一化渐变映射到逻辑矩形 (x,y,w,h) 并构造 tiny-skia shader。
/// 坐标在逻辑空间构造，物理化交给 fill_path 的 self.tf()（与 path 同源）。
/// stops 不足 2 或构造失败时返回 None（调用方退回纯色）。
fn sk_shader(
    g: &crate::render::Gradient,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Option<Shader<'static>> {
    use crate::render::Gradient;
    let sk_stops: Vec<SkStop> = g
        .stops()
        .iter()
        .map(|s| SkStop::new(s.offset.clamp(0.0, 1.0), to_sk_color(s.color)))
        .collect();
    if sk_stops.len() < 2 {
        return None;
    }
    match g {
        Gradient::Linear { start, end, .. } => {
            let p0 = SkPoint::from_xy(x + start.0 * w, y + start.1 * h);
            let p1 = SkPoint::from_xy(x + end.0 * w, y + end.1 * h);
            LinearGradient::new(p0, p1, sk_stops, SpreadMode::Pad, Transform::identity())
        }
        Gradient::Radial { center, radius, .. } => {
            let c = SkPoint::from_xy(x + center.0 * w, y + center.1 * h);
            // 半径以短边为基准（保持圆形而非随宽高拉成椭圆）。
            let r = (radius * w.min(h)).max(0.01);
            RadialGradient::new(
                c,
                0.0,
                c,
                r,
                sk_stops,
                SpreadMode::Pad,
                Transform::identity(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(pm: &Pixmap, x: u32, y: u32) -> (u8, u8, u8) {
        let p = pm.pixel(x, y).unwrap();
        (p.red(), p.green(), p.blue())
    }

    /// 画一个纯色矩形：`fast=true` 走特化快路，`fast=false` 走通用路径（直接调
    /// `fill_path_on_target`，绕开 `fill_round_rect` 里的快路分支，因此比的是两条
    /// **真实**实现，不是测试里另写的复制品）。
    #[allow(clippy::too_many_arguments)]
    fn paint_rect(
        scale: f32,
        offset: Point,
        clip: Option<Rect>,
        bg: tiny_skia::Color,
        color: Color,
        radius: f32,
        aa: bool,
        fast: bool,
    ) -> Pixmap {
        let mut pm = Pixmap::new(48, 36).unwrap();
        pm.fill(bg);
        let mut eng = crate::text::NullTextEngine;
        let mut c = SkiaCanvas::with_text_offset(&mut pm, &mut eng, scale, offset);
        let mut p = Paint::fill(color);
        p.anti_alias = aa;
        let (x, y, w, h) = (6.0, 4.0, 20.0, 14.0);
        if let Some(r) = clip {
            c.save();
            c.clip_rect(r);
        }
        if fast {
            c.fill_round_rect(x, y, w, h, radius, &p);
        } else {
            let (ax, ay, aw, ah) = crate::render::align_to_device(x, y, w, h, scale);
            let path = rounded_rect_path(ax, ay, aw, ah, radius).expect("矩形路径");
            let sp = SkiaCanvas::fill_paint(&p, ax, ay, aw, ah);
            let tf = c.tf();
            c.fill_path_on_target(&path, &sp, tf);
        }
        if clip.is_some() {
            c.restore();
        }
        drop(c);
        pm
    }

    /// 一个对照用例：名称、缩放、局部帧偏移、裁剪、背景色、填充色、圆角、抗锯齿。
    type Case = (
        &'static str,
        f32,
        Point,
        Option<Rect>,
        tiny_skia::Color,
        Color,
        f32,
        bool,
    );

    /// 快路与通用路径必须画出同一张图。
    ///
    /// 快路绕开了 tiny-skia 的整条 raster pipeline，自己做预乘、合成与圆角覆盖率。
    /// 两者一旦分歧，症状是"某些控件的底色深浅或圆角形状和以前不一样"——不报错、
    /// 不 panic，截图粗看也正常，只有逐像素比对能抓住。
    ///
    /// **判据用覆盖度而非绝对色差**：把每个像素投影到「背景→实心」这根轴上，得到
    /// 0..255 的覆盖度再比。否则判据强度会随 `color.a` 退化——淡底（alpha≈32，正是
    /// 这条快路的头号服务对象）下实心色与背景色本身只差十几个色阶，任何绝对色差阈值
    /// 在那里都变成恒真。
    ///
    /// 用例要覆盖的分支：不透明/半透明/淡底 × 直角/圆角/半径超限 × 有无裁剪 ×
    /// **裁哪一边** × 全窗/局部偏移 × 1x/2x × 抗锯齿开关。其中"裁哪一边"尤其容易
    /// 被写漏：裁剪矩形若一律从原点起，`x0.max(pc.x)` 与 `y0.max(pc.y)` 永远不生效，
    /// 于是把圆角锚点写错在**左上**那一侧的缺陷会全绿通过——而滚动容器把卡片从顶部
    /// 切掉半截，走的正是这条路径。
    #[test]
    fn fast_fill_rect_matches_general_path() {
        let opaque_bg = tiny_skia::Color::from_rgba8(30, 40, 60, 255);
        let alpha_bg = tiny_skia::Color::from_rgba8(30, 40, 60, 90);
        let solid = Color::rgba(200, 60, 40, 255);
        let half = Color::rgba(200, 60, 40, 128);
        let wash = Color::rgba(200, 60, 40, 32); // RoleAlpha 淡底量级
        // 切掉左上：交集起点由裁剪决定，x0.max / y0.max 才真正生效。
        let clip_tl = Some(Rect::new(10, 8, 20, 14));
        let clip_br = Some(Rect::new(0, 0, 16, 12));
        let o = Point::new(0, 0);
        let cases: &[Case] = &[
            ("不透明・直角・无裁剪", 1.0, o, None, opaque_bg, solid, 0.0, true),
            ("半透明・直角・无裁剪", 1.0, o, None, opaque_bg, half, 0.0, true),
            ("半透明・直角・半透明背景（暴露 alpha 合成）", 1.0, o, None, alpha_bg, half, 0.0, true),
            ("半透明・直角・裁右下", 1.0, o, clip_br, opaque_bg, half, 0.0, true),
            ("半透明・直角・裁左上", 1.0, o, clip_tl, opaque_bg, half, 0.0, true),
            ("半透明・直角・局部帧偏移", 1.0, Point::new(4, 3), None, opaque_bg, half, 0.0, true),
            ("半透明・直角・2x", 2.0, o, None, opaque_bg, half, 0.0, true),
            ("半透明・直角・2x + 偏移 + 裁右下", 2.0, Point::new(4, 2), Some(Rect::new(4, 2, 14, 10)), opaque_bg, Color::rgba(200, 60, 40, 200), 0.0, true),
            ("圆角・不透明・无裁剪（最常见的卡片）", 1.0, o, None, opaque_bg, solid, 5.0, true),
            ("圆角・半透明・裁右下", 1.0, o, clip_br, opaque_bg, Color::rgba(200, 60, 40, 160), 5.0, true),
            ("圆角・不透明・裁左上（滚动容器切顶）", 1.0, o, clip_tl, opaque_bg, solid, 5.0, true),
            ("圆角・半透明・裁左上", 1.0, o, clip_tl, opaque_bg, half, 5.0, true),
            ("圆角・2x + 偏移", 2.0, Point::new(4, 2), None, opaque_bg, solid, 4.0, true),
            ("圆角・2x + 偏移 + 裁左上", 2.0, Point::new(4, 2), Some(Rect::new(10, 8, 16, 10)), opaque_bg, solid, 4.0, true),
            ("圆角半径超过半高（钳成胶囊）", 1.0, o, None, opaque_bg, solid, 40.0, true),
            ("淡底・圆角・无裁剪（RoleAlpha 量级）", 1.0, o, None, opaque_bg, wash, 5.0, true),
            ("淡底・圆角・裁左上", 1.0, o, clip_tl, opaque_bg, wash, 5.0, true),
            ("淡底・直角・半透明背景", 1.0, o, None, alpha_bg, wash, 0.0, true),
            ("圆角・关抗锯齿（须无过渡带）", 1.0, o, None, opaque_bg, solid, 5.0, false),
            // 非整数缩放：pr 在物理坐标钳制、通用路径在逻辑坐标钳制，只有这里能验证
            // 两者不分歧。1.5 下逻辑 (6,4,20,14) 的物理边界是 (9,6)-(39,27)，仍是整数。
            ("圆角・1.5x（非整数缩放）", 1.5, o, None, opaque_bg, solid, 4.0, true),
            ("圆角・1.5x + 裁左上", 1.5, o, clip_tl, opaque_bg, half, 4.0, true),
            // 负坐标：局部重绘里"矩形左上角在子 pixmap 之外"是常态，验 x0.max(0)/y0.max(0)。
            ("圆角・偏移到左上角外（负坐标）", 1.0, Point::new(20, 15), None, opaque_bg, solid, 5.0, true),
            ("直角・偏移到左上角外（负坐标）", 1.0, Point::new(20, 15), None, opaque_bg, half, 0.0, true),
        ];

        for (name, scale, offset, clip, bg, color, radius, aa) in cases {
            // 前提校验：用例必须**真的**走到快路。少了这一步，一旦快路因为某次改动
            // 开始一律返回 false，本测试就退化成"通用路径 vs 通用路径"，永远绿着。
            {
                let mut probe = Pixmap::new(48, 36).unwrap();
                let mut eng = crate::text::NullTextEngine;
                let mut c = SkiaCanvas::with_text_offset(&mut probe, &mut eng, *scale, *offset);
                let (ax, ay, aw, ah) = crate::render::align_to_device(6.0, 4.0, 20.0, 14.0, *scale);
                assert!(
                    c.fast_fill_rect(ax, ay, aw, ah, *radius, *aa, *color),
                    "[{name}] 该用例没有走快路，本测试会失去意义"
                );
            }
            let fast = paint_rect(*scale, *offset, *clip, *bg, *color, *radius, *aa, true);
            let slow = paint_rect(*scale, *offset, *clip, *bg, *color, *radius, *aa, false);
            let rgba = |pm: &Pixmap, x: u32, y: u32| {
                let p = pm.pixel(x, y).unwrap();
                [p.red(), p.green(), p.blue(), p.alpha()]
            };
            let maxdiff = |a: [u8; 4], b: [u8; 4]| {
                (0..4).map(|i| (a[i] as i32 - b[i] as i32).abs()).max().unwrap()
            };
            // 背景色与实心色都**独立算出**，不从图上采样：矩形跨越原点时（负坐标用例）
            // (0,0) 根本不是背景，靠采样会得出「背景与实心色相同」的荒谬前提。
            let ba = bg.alpha();
            let q = |v: f32| (v * ba * 255.0).round() as u8;
            let bgpx = [
                q(bg.red()),
                q(bg.green()),
                q(bg.blue()),
                (ba * 255.0).round() as u8,
            ];
            let sa = color.a as f32 / 255.0;
            let mix = |src: u8, dst: u8| (src as f32 * sa + dst as f32 * (1.0 - sa)).round() as u8;
            let fullpx = [
                mix(color.r, bgpx[0]),
                mix(color.g, bgpx[1]),
                mix(color.b, bgpx[2]),
                (color.a as f32 + bgpx[3] as f32 * (1.0 - sa)).round().min(255.0) as u8,
            ];
            // 投影轴取背景与实心差得最开的通道，量化噪声在它上面相对最小。
            let ch = (0..4)
                .max_by_key(|&i| (fullpx[i] as i32 - bgpx[i] as i32).abs())
                .unwrap();
            let span = fullpx[ch] as i32 - bgpx[ch] as i32;
            assert!(
                span.abs() >= 8,
                "[{name}] 用例无效：背景与实心色只差 {span}，判据失去分辨力"
            );
            let cov = |p: [u8; 4]| ((p[ch] as i32 - bgpx[ch] as i32) * 255 / span).clamp(0, 255);
            let mut edge = 0usize;
            for y in 0..fast.height() {
                for x in 0..fast.width() {
                    let (f, sl) = (rgba(&fast, x, y), rgba(&slow, x, y));
                    if *radius == 0.0 {
                        // 直角矩形没有弧线，两条路径应当逐像素一致（±1 留给 f32 合成
                        // 与整数合成的舍入差）。
                        assert!(
                            maxdiff(f, sl) <= 1,
                            "[{name}] ({x}, {y}) 快路 {f:?} 与通用路径 {sl:?} 不一致"
                        );
                        continue;
                    }
                    let (cf, cs) = (cov(f), cov(sl));
                    if cs >= 254 {
                        // 满覆盖区没有抗锯齿，两条路径应当**精确**相同。放宽到覆盖度
                        // 比较会漏掉一整类缺陷：圆角行的中段若与两侧重叠而被画了两次，
                        // 半透明处颜色会变深，但覆盖度双方都钳在 255，看不出来。
                        assert!(
                            maxdiff(f, sl) <= 1,
                            "[{name}] 满覆盖处不一致：({x}, {y}) 快路 {f:?} vs 通用路径 {sl:?}（重复绘制？）"
                        );
                    } else if cs <= 1 {
                        // 空白区**不能**要求精确：两种 AA 对边界外沿一个像素的判定不同，
                        // 快路可能给出极淡的覆盖（4×4 超采样里落进 1 个子样本 = 15/255）
                        // 而通用路径给 0。那是 AA 边界游移，不是画错。
                        assert!(
                            cf <= 128,
                            "[{name}] 形状分歧：({x}, {y}) 通用路径为空白，快路覆盖度已 {cf}"
                        );
                    } else {
                        edge += 1;
                    }
                    // 两种 AA 的量化粒度分别是 255/4 与 255/16，叠加舍入后差一个 1/4
                    // 色阶仍属正常；超过就不是量化能解释的了。
                    assert!(
                        (cf - cs).abs() <= 64,
                        "[{name}] ({x}, {y}) 覆盖度分歧 {cf} vs {cs}（像素 {f:?} vs {sl:?}）"
                    );
                }
            }
            if *radius > 0.0 && *aa {
                assert!(edge > 0, "[{name}] 圆角却没有抗锯齿过渡像素，圆角多半没画出来");
            }
            if !*aa {
                assert_eq!(edge, 0, "[{name}] 关了抗锯齿却仍有 {edge} 个过渡像素");
            }
        }
    }

    /// 离屏层（子树 opacity）里走快路：像素必须落在**层**上，而不是主缓冲。
    ///
    /// `fast_fill_rect` 通过 `target_pixmap()` 做重定向，与通用路径的
    /// `fill_path_on_target` 用的是同一个 `layers.last_mut()` 判据。一旦两者分岔，
    /// 症状是子树 opacity 失效——内容以全不透明画进主缓冲，层合成时又叠一遍。
    #[test]
    fn fast_fill_rect_draws_into_offscreen_layer() {
        let render = |fast: bool| {
            let mut pm = Pixmap::new(48, 36).unwrap();
            pm.fill(tiny_skia::Color::from_rgba8(30, 40, 60, 255));
            let mut eng = crate::text::NullTextEngine;
            {
                let mut c =
                    SkiaCanvas::with_text_offset(&mut pm, &mut eng, 1.0, Point::new(0, 0));
                let p = Paint::fill(Color::rgba(200, 60, 40, 255));
                c.push_layer(0.5);
                if fast {
                    c.fill_round_rect(6.0, 4.0, 20.0, 14.0, 5.0, &p);
                } else {
                    let path = rounded_rect_path(6.0, 4.0, 20.0, 14.0, 5.0).expect("路径");
                    let sp = SkiaCanvas::fill_paint(&p, 6.0, 4.0, 20.0, 14.0);
                    let tf = c.tf();
                    c.fill_path_on_target(&path, &sp, tf);
                }
                c.pop_layer();
            }
            pm
        };
        let (fast, slow) = (render(true), render(false));
        // 层内画的是不透明色，合成时乘 0.5：中心应当是背景与填充色的中点附近，
        // 绝不能是纯填充色（那说明画进了主缓冲、绕过了层的 opacity）。
        let mid = fast.pixel(16, 11).unwrap();
        assert!(
            mid.red() < 200 && mid.red() > 60,
            "层 opacity 未生效：中心像素 red={}（画进主缓冲了？）",
            mid.red()
        );
        for y in 0..fast.height() {
            for x in 0..fast.width() {
                let (a, b) = (fast.pixel(x, y).unwrap(), slow.pixel(x, y).unwrap());
                let d = [
                    a.red() as i32 - b.red() as i32,
                    a.green() as i32 - b.green() as i32,
                    a.blue() as i32 - b.blue() as i32,
                    a.alpha() as i32 - b.alpha() as i32,
                ]
                .into_iter()
                .map(|v| v.abs())
                .max()
                .unwrap();
                assert!(d <= 64, "({x}, {y}) 层内快路与通用路径分歧 {d}");
            }
        }
    }

    /// 快路必须拒绝亚像素边界，把活交回通用路径。
    ///
    /// 快路整行写同一个值，前提是矩形边界压在整数物理像素上、没有抗锯齿边。若边界落在
    /// 半个像素上还硬走快路，边缘就会从"半透明过渡"变成"硬切"——细微到截图不易察觉，
    /// 却是实打实的渲染差异。1.25 倍缩放下 0.5 逻辑像素正好落在 0.625 物理像素处。
    #[test]
    fn fast_fill_rect_declines_subpixel_bounds() {
        let mut pm = Pixmap::new(32, 24).unwrap();
        let mut eng = crate::text::NullTextEngine;
        let mut c = SkiaCanvas::with_text_offset(&mut pm, &mut eng, 1.25, Point::new(0, 0));
        assert!(
            !c.fast_fill_rect(2.5, 2.0, 9.4, 8.0, 0.0, true, Color::rgba(200, 60, 40, 255)),
            "亚像素边界须落回通用路径"
        );
        assert!(
            c.fast_fill_rect(4.0, 4.0, 8.0, 8.0, 0.0, true, Color::rgba(200, 60, 40, 255)),
            "整数物理边界应走快路"
        );
    }

    /// 快路必须拒绝非有限坐标，把活交回通用路径。
    ///
    /// 整数边界那道闸门对无穷是**失效**的：差值取整后得 NaN，而 NaN 与任何数比较都
    /// 是 false，于是闸门放行；无穷取整转 i32 再饱和成 i32::MAX，被钳到缓冲宽度——
    /// 结果是一路写到右边缘。通用路径这时什么都不画（tiny-skia 的 Rect 构造对非有限
    /// 值返回 None）。两者**行为相反**：上游一个除零（weight 除零、measure 返回 inf）
    /// 原本表现为"这块没画出来"，会翻成"大片被这个颜色刷掉"。守的就是这个翻转。
    #[test]
    fn fast_fill_rect_declines_non_finite_bounds() {
        for (label, x, y, w, h, r) in [
            ("w=inf", 5.0, 5.0, f32::INFINITY, 20.0, 0.0),
            ("h=inf", 5.0, 5.0, 20.0, f32::INFINITY, 0.0),
            ("x=inf", f32::INFINITY, 5.0, 20.0, 20.0, 0.0),
            ("y=-inf", 5.0, f32::NEG_INFINITY, 20.0, 20.0, 0.0),
            ("w=NaN", 5.0, 5.0, f32::NAN, 20.0, 0.0),
            ("r=inf", 5.0, 5.0, 20.0, 20.0, f32::INFINITY),
        ] {
            let mut pm = Pixmap::new(40, 30).unwrap();
            pm.fill(tiny_skia::Color::from_rgba8(0, 0, 0, 255));
            let mut eng = crate::text::NullTextEngine;
            {
                let mut c =
                    SkiaCanvas::with_text_offset(&mut pm, &mut eng, 1.0, Point::new(0, 0));
                assert!(
                    !c.fast_fill_rect(x, y, w, h, r, true, Color::rgba(255, 0, 0, 255)),
                    "[{label}] 非有限坐标须落回通用路径"
                );
            }
            let painted = pm.pixels().iter().filter(|p| p.red() > 0).count();
            assert_eq!(painted, 0, "[{label}] 快路拒绝后不应留下任何笔迹，却染了 {painted} 像素");
        }
    }

    /// `mul255` 必须在**整个定义域**上等于四舍五入的真除——预乘与合成全靠它。
    ///
    /// 期望值用浮点独立算出，不从被测实现反推：这类"无除法近似"的经典错法是两端偏差
    /// （`mul255(255,255)` 得 254，于是不透明白色被画成 254 灰），而那恰恰是抽查中段
    /// 最不容易撞上的地方。全域 65536 组合一起验，省得挑样本。
    #[test]
    fn mul255_equals_rounded_true_division() {
        for a in 0..=255u32 {
            for x in 0..=255u32 {
                let want = (f64::from(x * a) / 255.0).round() as u32;
                assert_eq!(mul255(x, a), want, "mul255({x}, {a})");
            }
        }
    }

    /// `cull_rect` 报出本画布覆盖的世界范围，**不能是 `None`**。
    ///
    /// 三个后端（软件 / D2D / GPU）在这一点上必须同口径：调用方把 `None` 读作
    /// "拿不到范围，那就整个画"，而内容远高于视口的节点会因此每帧整表绘制
    /// （见 `core` 里的 `full_frame_culling_skips_rows_far_below_the_viewport`）。
    /// 软件后端一直是对的，补这条是为了它别在将来被改坏——另两个都是这么坏掉的。
    #[test]
    fn cull_rect_reports_the_covered_world_rect() {
        let mut pm = Pixmap::new(120, 80).unwrap();
        // 全窗：整块 pixmap，逻辑坐标（scale=1）
        {
            let c = SkiaCanvas::new(&mut pm);
            let cull = c.cull_rect().expect("软件后端必须报出可见范围");
            assert!(
                cull.x <= 0 && cull.y <= 0 && cull.right() >= 120 && cull.bottom() >= 80,
                "应覆盖整块 pixmap: {cull:?}"
            );
            assert!(cull.h < 10_000, "范围要有界: {cull:?}");
        }
        // 局部帧：子 pixmap + 世界偏移，报的是那块脏区在世界里的位置
        let mut sub = Pixmap::new(40, 24).unwrap();
        let mut eng = crate::text::NullTextEngine;
        let c = SkiaCanvas::with_text_offset(&mut sub, &mut eng, 1.0, Point::new(8, 40));
        let cull = c.cull_rect().expect("局部帧同样要报");
        assert!(
            cull.x <= 8 && cull.y <= 40 && cull.right() >= 48 && cull.bottom() >= 64,
            "应覆盖世界坐标下的脏区 (8,40)+40x24: {cull:?}"
        );
    }

    /// V 形折线的拐点必须实心：`draw_polyline` 要把夹角外侧那块楔形盖住。
    ///
    /// 这条守的是"高 DPI 下对勾裂开"那个缺陷。同一个 V，用两次 `draw_line` 画会在
    /// 尖底留下缺口——测试里连这个**反例一起断言**，否则阈值一松，测试对两种画法
    /// 都是绿的，也就守不住任何东西。
    #[test]
    fn polyline_joint_is_solid_where_two_lines_would_gap() {
        // V 形：左上 → 底 → 右上。夹角约 90°，笔宽 6 —— 放大到能稳定采样的尺度。
        const PTS: [(f32, f32); 3] = [(20.0, 20.0), (50.0, 70.0), (80.0, 20.0)];
        const W: f32 = 6.0;
        // 采样点取尖底正下方：两段线的 Butt 端面都够不到这里，正是缺口所在。
        const SX: u32 = 50;
        const SY: u32 = 72;

        let paint = Paint::fill(Color::hex(0x000000));

        let mut good = Pixmap::new(100, 100).unwrap();
        good.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut good);
            c.draw_polyline(&PTS, W, &paint);
        }

        let mut naive = Pixmap::new(100, 100).unwrap();
        naive.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut naive);
            c.draw_line(PTS[0].0, PTS[0].1, PTS[1].0, PTS[1].1, W, &paint);
            c.draw_line(PTS[1].0, PTS[1].1, PTS[2].0, PTS[2].1, W, &paint);
        }

        let (gr, _, _) = px(&good, SX, SY);
        let (nr, _, _) = px(&naive, SX, SY);
        assert!(gr < 80, "draw_polyline 的拐点应是实心笔色，实得亮度 {gr}");
        assert!(
            nr > 200,
            "反例失效：两次 draw_line 本应在此留白（实得亮度 {nr}）——             采样点或几何被改过，这条测试已不再能抓住拐点缺口"
        );
    }

    /// 折线的两端保持平头（Butt），不因为修拐点而变成圆头——
    /// 圆头会让既有图标（对勾、chevron）的观感整体变化。
    #[test]
    fn polyline_keeps_butt_caps_at_the_ends() {
        let mut pm = Pixmap::new(100, 100).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut pm);
            // 水平两段折线，端点在 x=20 / x=80，笔宽 8。
            c.draw_polyline(
                &[(20.0, 50.0), (50.0, 50.0), (80.0, 50.0)],
                8.0,
                &Paint::fill(Color::hex(0x000000)),
            );
        }
        // Butt 端头在端点处齐平截断：端点外 2px 必须仍是白底。
        // 若退化成 Round cap，这里会被半圆盖住。
        let (r, _, _) = px(&pm, 17, 50);
        assert!(r > 200, "折线端头应平齐截断（Butt），实得亮度 {r}");
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
            c.fill_round_rect(
                20.0,
                40.0,
                40.0,
                6.0,
                3.0,
                &Paint::fill(Color::hex(0xFF0000)),
            );
            c.restore();
        }
        // 裁剪带中心应被红色填充。
        let (r, g, b) = px(&pm, 35, 43);
        assert!(
            r > 200 && g < 80 && b < 80,
            "薄裁剪带内应被填充，实得 ({r},{g},{b})"
        );
    }

    /// draw_image：Fill 模式铺满 dst，框内被图片色填充、框外保持原样。
    #[test]
    fn draw_image_fills_dst_and_respects_bounds() {
        let mut pm = Pixmap::new(100, 100).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        // 4×4 纯红图（非预乘 RGBA）。
        let red = {
            let mut v = Vec::new();
            for _ in 0..16 {
                v.extend_from_slice(&[255, 0, 0, 255]);
            }
            v
        };
        let img = Image::from_rgba(4, 4, &red).unwrap();
        {
            let mut c = SkiaCanvas::new(&mut pm);
            c.draw_image(&img, Rect::new(20, 20, 40, 40), Fit::Fill, 0.0, 1.0);
        }
        // dst 中心应为红。
        let (r, g, b) = px(&pm, 40, 40);
        assert!(
            r > 200 && g < 60 && b < 60,
            "dst 内应被图片填充，实得 ({r},{g},{b})"
        );
        // dst 外应保持白。
        let (r2, g2, b2) = px(&pm, 5, 5);
        assert!(
            r2 > 240 && g2 > 240 && b2 > 240,
            "dst 外不应被绘制，实得 ({r2},{g2},{b2})"
        );
    }

    /// 图片被裁剪矩形切掉的部分必须不落笔——**四条边分别验**。
    ///
    /// 裁剪求交原先用 `Mask::intersect_path`（整幅 mask 操作，1920×1080 下 2.2ms/次，
    /// 占单张图片绘制的 83%）。现改为只在 dst 区域内把裁剪区外的像素清零，四条边是
    /// 四段独立的边界算术。若某一侧算错，症状是图片溢出到裁剪区外——滚动容器里就是
    /// 图片盖住了容器边框或相邻内容。
    ///
    /// 四条边分开写而不是用一个"切掉一角"的用例：裁剪矩形若总是从原点起，左边界与
    /// 上边界的算术永远不生效，那两侧写错也照样全绿。
    #[test]
    fn draw_image_respects_clip_rect_on_every_side() {
        let red = {
            let mut v = Vec::new();
            for _ in 0..16 {
                v.extend_from_slice(&[255, 0, 0, 255]);
            }
            v
        };
        let img = Image::from_rgba(4, 4, &red).unwrap();
        // 图片铺在 (20,20)-(60,60)；每个用例用裁剪矩形切掉它的一侧。
        let dst = Rect::new(20, 20, 40, 40);
        let cases: &[(&str, Rect, Rect)] = &[
            // (名称, 裁剪矩形, 应当保持空白的探测区域——落在图片内但裁剪外)
            ("切左", Rect::new(40, 0, 60, 100), Rect::new(20, 20, 20, 40)),
            ("切上", Rect::new(0, 40, 100, 60), Rect::new(20, 20, 40, 20)),
            ("切右", Rect::new(0, 0, 40, 100), Rect::new(40, 20, 20, 40)),
            ("切下", Rect::new(0, 0, 100, 40), Rect::new(20, 40, 40, 20)),
        ];
        for (name, clip, blank) in cases {
            for &radius in &[0.0f32, 8.0] {
                let mut pm = Pixmap::new(100, 100).unwrap();
                pm.fill(tiny_skia::Color::WHITE);
                {
                    let mut c = SkiaCanvas::new(&mut pm);
                    c.save();
                    c.clip_rect(*clip);
                    c.draw_image(&img, dst, Fit::Fill, radius, 1.0);
                    c.restore();
                }
                // 裁剪区外、但在图片 dst 内的区域必须仍是白的。
                for y in blank.y..blank.y + blank.h {
                    for x in blank.x..blank.x + blank.w {
                        let (r, g, b) = px(&pm, x as u32, y as u32);
                        assert!(
                            r > 240 && g > 240 && b > 240,
                            "[{name} r={radius}] ({x},{y}) 在裁剪区外却被画上了 ({r},{g},{b})"
                        );
                    }
                }
                // 前提校验：裁剪区内确实画上了图，否则本用例什么都没验证。
                let keep = clip.intersect(&dst);
                assert!(!keep.is_empty(), "[{name}] 用例无效：裁剪后不剩任何图片区域");
                let (cx, cy) = (keep.x + keep.w / 2, keep.y + keep.h / 2);
                let (r, g, b) = px(&pm, cx as u32, cy as u32);
                assert!(
                    r > 200 && g < 80 && b < 80,
                    "[{name} r={radius}] 裁剪区内 ({cx},{cy}) 应有图片, 实得 ({r},{g},{b})"
                );
            }
        }
    }

    /// draw_image：物理尺寸与源图一致时须 1:1 blit——边缘不得出现插值灰边。
    /// 这是 DPI 感知矢量图标的落地保证：光栅到物理尺寸后若仍被缩放/半像素平移，
    /// 双线性会把细描边重新摊糊，前面的精确光栅就白做了。
    #[test]
    fn draw_image_unit_scale_is_pixel_exact() {
        let mut pm = Pixmap::new(20, 20).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        // 4×4 纯红源图，dst 恰为 4×4 逻辑（scale=1 → 物理 4×4）。
        let img = Image::from_rgba(4, 4, &[255u8, 0, 0, 255].repeat(4 * 4)).unwrap();
        {
            let mut c = SkiaCanvas::new(&mut pm);
            c.draw_image(&img, Rect::new(5, 5, 4, 4), Fit::Contain, 0.0, 1.0);
        }
        for y in 5..9 {
            for x in 5..9 {
                let (r, g, b) = px(&pm, x, y);
                assert_eq!(
                    (r, g, b),
                    (255, 0, 0),
                    "({x},{y}) 应为纯红（1:1 无插值），实得 ({r},{g},{b})"
                );
            }
        }
        // 框外相邻像素不得被插值溢出污染。
        let (r, g, b) = px(&pm, 9, 9);
        assert_eq!((r, g, b), (255, 255, 255), "dst 外应保持背景白");
    }

    /// 尺寸差不足 1 物理像素时吸附 1:1：非整数 DPI 下 `scaled()` 四边各自 round，
    /// 物理宽可能比理论值差 1，不吸附就会退化成 0.97 倍这种"几乎 1:1"的糊缩放。
    #[test]
    fn draw_image_snaps_near_unit_scale() {
        let mut pm = Pixmap::new(30, 30).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        // 源 8×8，dst 逻辑 9×9（差 1 像素，落在吸附阈值内）→ 保持 8×8 不拉伸。
        let img = Image::from_rgba(8, 8, &[255u8, 0, 0, 255].repeat(8 * 8)).unwrap();
        {
            let mut c = SkiaCanvas::new(&mut pm);
            c.draw_image(&img, Rect::new(5, 5, 9, 9), Fit::Contain, 0.0, 1.0);
        }
        // 吸附后图片 8×8 在 9×9 框内居中（tx 取整），像素应为纯红而非插值中间色。
        let (r, g, b) = px(&pm, 9, 9);
        assert_eq!(
            (r, g, b),
            (255, 0, 0),
            "近 1:1 应吸附为纯 blit，实得 ({r},{g},{b})"
        );
    }

    /// draw_image：大圆角半径把四角裁掉（角落像素保持背景白）。
    #[test]
    fn draw_image_rounded_clips_corners() {
        let mut pm = Pixmap::new(60, 60).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let red = [255u8, 0, 0, 255].repeat(4 * 4);
        let img = Image::from_rgba(4, 4, &red).unwrap();
        {
            let mut c = SkiaCanvas::new(&mut pm);
            // dst 40×40，圆角半径 20（=半边长，近圆）。
            c.draw_image(&img, Rect::new(10, 10, 40, 40), Fit::Fill, 20.0, 1.0);
        }
        // 左上角（dst 角点）应被圆角裁掉 → 仍为白。
        let (r, g, b) = px(&pm, 11, 11);
        assert!(
            r > 240 && g > 240 && b > 240,
            "圆角应裁掉角落，实得 ({r},{g},{b})"
        );
        // 中心仍为红。
        let (rc, gc, bc) = px(&pm, 30, 30);
        assert!(
            rc > 200 && gc < 60 && bc < 60,
            "中心应为图片色，实得 ({rc},{gc},{bc})"
        );
    }

    /// draw_image：低不透明度让红图与白底混出更浅的色（验证状态调制）。
    #[test]
    fn draw_image_opacity_blends_lighter() {
        let mut pm = Pixmap::new(40, 40).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let red = [255u8, 0, 0, 255].repeat(4 * 4);
        let img = Image::from_rgba(4, 4, &red).unwrap();
        {
            let mut c = SkiaCanvas::new(&mut pm);
            c.draw_image(&img, Rect::new(5, 5, 30, 30), Fit::Fill, 0.0, 0.4);
        }
        // 0.4 不透明红 over 白：r 仍高、g/b 被白底抬升（不再接近 0）。
        let (r, g, b) = px(&pm, 20, 20);
        assert!(r > 240, "红通道应仍高，实得 {r}");
        assert!(g > 120 && b > 120, "低不透明应混入白底（g={g}, b={b}）");
    }

    /// 局部重绘正确性：带 offset 的子 pixmap 渲染，应与全窗渲染的对应区域逐像素一致。
    /// 验证图元偏移（图形变换 + 裁剪 mask）的几何正确，杜绝脏区合成错位/残影。
    #[test]
    fn offset_subpixmap_matches_full_region() {
        // 全窗 100×100：白底 + 一个完全落在比较区内的蓝色圆角矩形 + 一层裁剪。
        let draw = |c: &mut SkiaCanvas| {
            c.save();
            c.clip_rect(Rect::new(20, 20, 70, 70));
            c.fill_round_rect(
                40.0,
                40.0,
                22.0,
                18.0,
                5.0,
                &Paint::fill(Color::hex(0x3366CC)),
            );
            c.fill_circle(70.0, 70.0, 8.0, &Paint::fill(Color::hex(0xCC3333)));
            c.restore();
        };
        let mut full = Pixmap::new(100, 100).unwrap();
        full.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut full);
            draw(&mut c);
        }
        // 子 pixmap：脏区 (30,30,40,40)，offset=(30,30)，scale=1。
        let mut sub = Pixmap::new(40, 40).unwrap();
        sub.fill(tiny_skia::Color::WHITE);
        {
            let mut eng = crate::text::NullTextEngine;
            let mut c = SkiaCanvas::with_text_offset(&mut sub, &mut eng, 1.0, Point::new(30, 30));
            draw(&mut c);
        }
        // 逐像素比对 full[30..70, 30..70] 与 sub[0..40, 0..40]。
        for y in 0..40u32 {
            for x in 0..40u32 {
                let f = full.pixel(30 + x, 30 + y).unwrap();
                let s = sub.pixel(x, y).unwrap();
                assert_eq!(
                    (f.red(), f.green(), f.blue(), f.alpha()),
                    (s.red(), s.green(), s.blue(), s.alpha()),
                    "局部重绘像素 ({x},{y}) 应与全窗一致"
                );
            }
        }
    }

    /// 局部重绘在分数缩放（1.5×）下的正确性：当 offset×scale 为整数（脏区对齐到 4px 网格保证）时，
    /// 带 offset 的子 pixmap 渲染应与全窗对应区域逐像素一致（含抗锯齿边缘）。
    #[test]
    fn offset_subpixmap_exact_at_scale_1_5() {
        let s = 1.5;
        // 全窗 180×180 物理（= 120 逻辑 ×1.5）。
        let draw = |c: &mut SkiaCanvas| {
            c.fill_round_rect(
                40.0,
                41.0,
                23.0,
                17.0,
                5.0,
                &Paint::fill(Color::hex(0x3366CC)),
            );
        };
        let mut full = Pixmap::new(180, 180).unwrap();
        full.fill(tiny_skia::Color::WHITE);
        {
            let mut eng = crate::text::NullTextEngine;
            let mut c = SkiaCanvas::with_text(&mut full, &mut eng, s);
            draw(&mut c);
        }
        // 脏区逻辑原点 (12,12)（4 的倍数 → 12×1.5=18 整数），物理 (18,18)，大小 60×60。
        let mut sub = Pixmap::new(60, 60).unwrap();
        sub.fill(tiny_skia::Color::WHITE);
        {
            let mut eng = crate::text::NullTextEngine;
            let mut c = SkiaCanvas::with_text_offset(&mut sub, &mut eng, s, Point::new(12, 12));
            draw(&mut c);
        }
        for y in 0..60u32 {
            for x in 0..60u32 {
                let f = full.pixel(18 + x, 18 + y).unwrap();
                let g = sub.pixel(x, y).unwrap();
                assert_eq!(
                    (f.red(), f.green(), f.blue(), f.alpha()),
                    (g.red(), g.green(), g.blue(), g.alpha()),
                    "1.5× 对齐 offset 下像素 ({x},{y}) 应逐像素一致"
                );
            }
        }
    }

    /// 水平线性渐变：左缘偏蓝、右缘偏红，证属过渡而非纯色。
    #[test]
    fn fill_round_rect_linear_gradient_left_to_right() {
        let mut pm = Pixmap::new(100, 40).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut pm);
            let g = crate::render::Gradient::linear(
                (0.0, 0.5),
                (1.0, 0.5),
                vec![(0.0, Color::hex(0x0000FF)), (1.0, Color::hex(0xFF0000))],
            );
            c.fill_round_rect(
                0.0,
                0.0,
                100.0,
                40.0,
                8.0,
                &crate::render::Paint::gradient(g),
            );
        }
        let (lr, _lg, lb) = px(&pm, 6, 20);
        let (rr, _rg, rb) = px(&pm, 93, 20);
        assert!(lb > lr, "左缘应偏蓝（b>r），实得 r={lr} b={lb}");
        assert!(rr > rb, "右缘应偏红（r>b），实得 r={rr} b={rb}");
    }

    /// 径向渐变：中心亮、边缘暗（证圆心向外过渡）。
    #[test]
    fn fill_rect_radial_gradient_center_to_edge() {
        let mut pm = Pixmap::new(60, 60).unwrap();
        pm.fill(tiny_skia::Color::BLACK);
        {
            let mut c = SkiaCanvas::new(&mut pm);
            let g = crate::render::Gradient::radial(
                (0.5, 0.5),
                1.0,
                vec![(0.0, Color::hex(0xFFFFFF)), (1.0, Color::hex(0x000000))],
            );
            c.fill_rect(0.0, 0.0, 60.0, 60.0, &crate::render::Paint::gradient(g));
        }
        let (cr, _, _) = px(&pm, 30, 30);
        let (er, _, _) = px(&pm, 3, 3);
        assert!(cr > er + 80, "圆心应明显比边角亮，实得 中心={cr} 边角={er}");
    }

    /// 离屏层 opacity：50% 红块合成到白底 → 粉色（r 高、g/b 被抬升）。
    #[test]
    fn push_pop_layer_composites_with_opacity() {
        let mut pm = Pixmap::new(40, 40).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut pm);
            c.push_layer(0.5);
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(Color::hex(0xFF0000)));
            c.pop_layer();
        }
        let (r, g, b) = px(&pm, 20, 20);
        assert!(r > 240, "红通道应高，实得 {r}");
        assert!(
            g > 100 && g < 200,
            "绿应被白底抬到中段（50% 合成），实得 {g}"
        );
        assert!(b > 100 && b < 200, "蓝应被白底抬到中段，实得 {b}");
    }

    /// 投影：矩形外有柔化渐隐（紧邻边缘变暗、远处保持白）。
    #[test]
    fn draw_shadow_produces_soft_halo() {
        let mut pm = Pixmap::new(120, 120).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut pm);
            c.draw_shadow(40.0, 40.0, 40.0, 40.0, 8.0, 10.0, Color::rgba(0, 0, 0, 180));
        }
        // 投影矩形中心应明显变暗。
        let (cr, _, _) = px(&pm, 60, 60);
        assert!(cr < 120, "投影中心应变暗，实得 {cr}");
        // 紧邻矩形外缘（约 6px 外）应处于柔化过渡（介于暗与白之间）。
        let (er, _, _) = px(&pm, 86, 60);
        assert!(er > 130 && er < 252, "外缘应为柔化过渡，实得 {er}");
        // 远角应保持纯白（未被投影波及）。
        let (fr, _, _) = px(&pm, 4, 4);
        assert!(fr > 250, "远角应保持白，实得 {fr}");
    }

    /// 投影外缘必须**渐隐到底**，不能在某一圈上突然截断。
    ///
    /// ★ 回归：阴影 pixmap 的 margin 曾按 `2×半径` 留（以为 3 趟 box-blur 的可见扩散只有
    /// 1.5 倍），而每趟各扩散一个半径、总共 3 倍——尾部被 pixmap 边界切掉，阴影最外圈
    /// 留下一道直角硬边。判据取尾部而非每一步：紧邻矩形处梯度本就陡（从实心到渐隐），
    /// 而截断的特征很具体——渐隐还没走到 0 就没了，"最后一个可见暗度"仍是个大数。
    #[test]
    fn shadow_fades_out_without_a_hard_cutoff_ring() {
        let mut pm = Pixmap::new(400, 400).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        {
            let mut c = SkiaCanvas::new(&mut pm);
            // 半径取得比默认大：截断随半径放大，小半径下未必看得出来。
            c.draw_shadow(
                150.0,
                150.0,
                100.0,
                100.0,
                10.0,
                18.0,
                Color::rgba(0, 0, 0, 180),
            );
        }
        // 从矩形右缘向外逐像素取样，记录暗度（255-r，越大越暗）。
        let dark: Vec<i32> = (250..399).map(|x| 255 - px(&pm, x, 200).0 as i32).collect();
        for i in 1..dark.len() {
            assert!(
                dark[i] <= dark[i - 1] + 1,
                "暗度应向外单调递减，第 {i} 步从 {} 跳到 {}",
                dark[i - 1],
                dark[i]
            );
        }
        let last = dark.iter().rposition(|&d| d > 0).expect("阴影应有可见范围");
        assert!(
            dark[last] <= 2,
            "渐隐尾部最后一个可见暗度为 {}（在第 {last} 像素处），说明模糊被 pixmap 边界切断——\
             渐隐走完的话末端应几乎归零",
            dark[last]
        );
        assert!(
            dark[0] > 10,
            "紧邻边缘应有可见暗度（否则这条测试没测到东西）"
        );
        assert_eq!(*dark.last().unwrap(), 0, "足够远处应回到纯白");
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
            c.fill_round_rect(
                28.37,
                42.0,
                96.6,
                6.0,
                3.0,
                &Paint::fill(Color::hex(0x4C8BF5)),
            );
            c.restore();
        }
        let (r, g, b) = px(&pm, 60, 44);
        assert!(
            b > 180 && r < 140,
            "进度滑块应在裁剪带内显现，实得 ({r},{g},{b})"
        );
    }
}

