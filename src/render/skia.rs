//! tiny-skia 后端：把 `Canvas` 图元光栅化到 `Pixmap`（RGBA 预乘）。
//!
//! 支持矩形裁剪栈：用 alpha `Mask` 表示当前裁剪区，所有绘制传入栈顶 mask。

use std::cell::RefCell;
use std::collections::HashMap;

use tiny_skia::{
    FillRule, FilterQuality, GradientStop as SkStop, LineCap, LinearGradient, Mask,
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
fn shadows_disabled() -> bool {
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

    /// 逻辑→物理变换：缩放后平移 -offset（物理像素），把世界坐标映射进子 pixmap。
    fn tf(&self) -> Transform {
        Transform::from_scale(self.scale, self.scale).post_translate(
            -self.offset.x as f32 * self.scale,
            -self.offset.y as f32 * self.scale,
        )
    }
}

impl Canvas for SkiaCanvas<'_> {
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, paint: &Paint) {
        self.fill_round_rect(x, y, w, h, 0.0, paint);
    }

    fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, paint: &Paint) {
        let _g = super::prof::scope(super::prof::FILL);
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
        // 3 趟 box-blur ≈ 高斯，实际可见扩散约 1.5×半径；留 2× 余量足够，缩小阴影 pixmap 降低合成开销。
        let margin = (pblur * 2.0).ceil() as i32 + 1;
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
        let (dw, dh) = (iw * sx, ih * sy);
        // 在 dst 框内居中（Cover/None 的溢出由裁剪 mask 收口）。
        let tx = px + (pw - dw) / 2.0;
        let ty = py + (ph - dh) / 2.0;
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
        // 与当前裁剪矩形求交（滚动视口等）；当前裁剪皆为矩形。
        if let Some(c) = self.clips.last() {
            let cr = c
                .rect
                .offset(-self.offset.x, -self.offset.y)
                .scaled(self.scale);
            if cr.is_empty() {
                return;
            }
            if let Some(rect) =
                tiny_skia::Rect::from_xywh(cr.x as f32, cr.y as f32, cr.w as f32, cr.h as f32)
            {
                let mut pb = PathBuilder::new();
                pb.push_rect(rect);
                if let Some(clip_path) = pb.finish() {
                    mask.intersect_path(
                        &clip_path,
                        FillRule::Winding,
                        false,
                        Transform::identity(),
                    );
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
        family: Option<&str>,
        size: f32,
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
            engine.draw(target, text, rect, color, align, family, size, clip);
        }
    }

    fn measure_text(
        &mut self,
        text: &str,
        family: Option<&str>,
        size: f32,
    ) -> crate::geometry::Size {
        // 逻辑入参；引擎内部物理测量后 /scale 回逻辑，与正文绘制度量同源。
        match self.engine.as_deref_mut() {
            Some(engine) => engine.measure(text, family, size, None),
            None => crate::geometry::Size::new(
                (text.chars().count() as f32 * size * 0.6).ceil() as i32,
                size.ceil() as i32,
            ),
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
