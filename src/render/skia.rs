//! tiny-skia 后端：把 `Canvas` 图元光栅化到 `Pixmap`（RGBA 预乘）。
//!
//! 支持矩形裁剪栈：用 alpha `Mask` 表示当前裁剪区，所有绘制传入栈顶 mask。

use tiny_skia::{
    FillRule, FilterQuality, LineCap, Mask, Paint as SkPaint, PathBuilder, Pixmap, PixmapPaint,
    Stroke, Transform,
};

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
        }
    }

    fn sk_paint(p: &Paint) -> SkPaint<'static> {
        let mut sp = SkPaint::default();
        sp.set_color(to_sk_color(p.color));
        sp.anti_alias = p.anti_alias;
        sp
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
        if let Some(path) = rounded_rect_path(x, y, w, h, radius) {
            let sp = Self::sk_paint(paint);
            let mask = self.clips.last().map(|c| &c.mask);
            self.pixmap
                .fill_path(&path, &sp, FillRule::Winding, self.tf(), mask);
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
            let mask = self.clips.last().map(|c| &c.mask);
            self.pixmap
                .stroke_path(&path, &sp, &stroke, self.tf(), mask);
        }
    }

    fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, paint: &Paint) {
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
            let mask = self.clips.last().map(|c| &c.mask);
            self.pixmap
                .stroke_path(&path, &sp, &stroke, self.tf(), mask);
        }
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, paint: &Paint) {
        if let Some(path) = PathBuilder::from_circle(cx, cy, r) {
            let sp = Self::sk_paint(paint);
            let mask = self.clips.last().map(|c| &c.mask);
            self.pixmap
                .fill_path(&path, &sp, FillRule::Winding, self.tf(), mask);
        }
    }

    fn draw_image(&mut self, img: &Image, dst: Rect, fit: Fit, radius: f32, opacity: f32) {
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
        self.pixmap
            .draw_pixmap(0, 0, img.pixmap().as_ref(), &paint, transform, Some(&mask));
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
        if let Some(engine) = self.engine.as_deref_mut() {
            engine.draw(self.pixmap, text, rect, color, align, family, size, clip);
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
