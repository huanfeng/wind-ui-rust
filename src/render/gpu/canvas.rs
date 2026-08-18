//! `Canvas` / `RenderTarget` 的 wgpu 实现：把图元调用翻译成 SDF 实例（见 `prim.rs`）。
//!
//! 设计意图：**几何语义以 `render/skia.rs` 为准，逐条对着抄**。三个后端里软后端是唯一
//! 一份「行为规格」——描边内缩半个线宽、线段用 Butt 端帽、圆角 clamp 到半边、渐变按
//! rect 归一化映射、阴影读同一个 `WINDUI_NOSHADOW` 开关，这些都不是本文件的自由发挥，
//! 而是照抄。d2d 后端当年也是这么接的（`D2DCanvas::stroke_round_rect` 的注释写明了
//! 「与 SkiaCanvas 的描边几何一致」），本文件延续同一条纪律。
//!
//! 与两个既有后端的差别只在实现路径：软后端逐图元光栅进 pixmap、d2d 逐图元调 D2D API，
//! 而这里**只攒实例不画**，直到 `Canvas` 析构才编一个 pass 一次画完。于是「帧末裁剪栈
//! 必须归零」这条（d2d 的 `pushed_clips` 教训）在这里落在 `Drop` 上。
//!
//! # 分期空缺
//!
//! - `draw_text`（P2）/ `draw_image`（P3）：显式空实现，且进程内提示一次。静默漏画会被
//!   当成布局 bug 排查半天，这一行 stderr 是给未来的自己省时间。
//! - `push_layer` / `pop_layer`（P3）：只维护嵌套计数保证平衡，`opacity` **暂不生效**
//!   （子树会按不透明绘制）。
//! - `measure_text` / `measure_text_wrapped` / `text_line_metrics`：**委托 `make_canvas`
//!   传入的 `engine`**。这不是桩——软后端同样是委托它，排版与度量本就属于平台文字栈，
//!   GPU 后端到 P2 也只接管「光栅」那一段。

use std::sync::Arc;

use super::device::SharedGpu;
use super::prim::{PrimBatch, PrimRenderer, MAX_BATCH};
use crate::geometry::{Color, Rect, Size};
use crate::render::image::{Fit, Image};
use crate::render::{Canvas, Paint, RenderTarget};
use crate::spec::Align;
use crate::text::{LineMetrics, TextEngine, TextStyle};

/// 一帧的 GPU 渲染目标：一张颜色纹理视图 + 跨帧复用的管线与缓冲。
pub struct WgpuTarget<'t> {
    gpu: Arc<SharedGpu>,
    view: &'t wgpu::TextureView,
    renderer: &'t mut PrimRenderer,
    /// 目标物理尺寸（像素）。
    size: (u32, u32),
}

impl<'t> WgpuTarget<'t> {
    pub(super) fn new(
        gpu: Arc<SharedGpu>,
        view: &'t wgpu::TextureView,
        renderer: &'t mut PrimRenderer,
        size: (u32, u32),
    ) -> Self {
        Self {
            gpu,
            view,
            renderer,
            size,
        }
    }
}

impl RenderTarget for WgpuTarget<'_> {
    fn make_canvas<'a>(
        &'a mut self,
        engine: &'a mut dyn TextEngine,
        scale: f32,
    ) -> Box<dyn Canvas + 'a> {
        Box::new(WgpuCanvas {
            gpu: self.gpu.clone(),
            view: self.view,
            renderer: &mut *self.renderer,
            engine,
            batch: PrimBatch::default(),
            size: self.size,
            scale: scale.max(0.01),
            clips: Vec::new(),
            saves: Vec::new(),
            layers: 0,
        })
    }
    // `as_pixmap` 用 trait 默认的 None：GPU 侧像素读不回来，调用方据此走全窗重绘
    // （与 d2d 后端同一档待遇）。
}

/// 攒图元的 `Canvas`。析构时把本帧攒下的实例一次画完。
pub struct WgpuCanvas<'a> {
    gpu: Arc<SharedGpu>,
    view: &'a wgpu::TextureView,
    renderer: &'a mut PrimRenderer,
    engine: &'a mut dyn TextEngine,
    batch: PrimBatch,
    size: (u32, u32),
    scale: f32,
    /// 裁剪栈：存**逻辑**矩形，每一层已是各级交集（只会收窄）。
    clips: Vec<Rect>,
    /// `save()` 记下的栈深，`restore()` 据此回弹。
    saves: Vec<usize>,
    /// `push_layer` 的嵌套计数（P3 前只用来保证平衡）。
    layers: u32,
}

impl WgpuCanvas<'_> {
    /// 当前有效裁剪 → **物理整数**矩形 `[x0,y0,x1,y1]`，并收进目标边界。
    ///
    /// 取整在这里做完（而不是留给 shader）：软后端的裁剪 mask 是
    /// `Rect::scaled` 之后的整数矩形且**不抗锯齿**，先取整再缩放和先缩放再取整在
    /// 非整数 DPI 下会差一个像素列。
    fn clip_phys(&self) -> [f32; 4] {
        let (w, h) = (self.size.0 as i32, self.size.1 as i32);
        let bounds = Rect::new(0, 0, w, h);
        let r = match self.clips.last() {
            Some(c) => c.scaled(self.scale).intersect(&bounds),
            None => bounds,
        };
        [
            r.x as f32,
            r.y as f32,
            (r.x + r.w) as f32,
            (r.y + r.h) as f32,
        ]
    }

    /// 外包框的抗锯齿余量（逻辑长度）。SDF 的过渡带是 1 个物理像素，留 1.5 个足够，
    /// 且外包框大一点不影响画面——框外的片元覆盖度算出来就是 0。
    fn aa_margin(&self) -> f32 {
        1.5 / self.scale
    }

    /// 攒够一批就先画掉，避免长帧把实例数组撑到无界。
    fn maybe_flush(&mut self) {
        if self.batch.len() >= MAX_BATCH {
            self.flush();
        }
    }

    /// 编码并提交本批实例。
    fn flush(&mut self) {
        self.renderer
            .flush(&self.gpu, self.view, self.size, self.scale, &mut self.batch);
    }
}

impl Drop for WgpuCanvas<'_> {
    fn drop(&mut self) {
        // 帧末栈必须归零。d2d 后端的 `pushed_clips/pushed_layers` 计数就是为这条留的：
        // 不平衡的裁剪会泄漏到下一帧，症状是「某个控件莫名其妙被裁掉一半」，且离出问题的
        // 那次 `clip_rect` 已经隔了很远。
        debug_assert!(
            self.clips.is_empty() && self.saves.is_empty(),
            "帧末裁剪栈未归零：clips={} saves={}（save/restore 未配对）",
            self.clips.len(),
            self.saves.len()
        );
        debug_assert_eq!(
            self.layers, 0,
            "帧末合成层未归零（push_layer/pop_layer 未配对）"
        );
        self.flush();
    }
}

impl Canvas for WgpuCanvas<'_> {
    fn dpi_scale(&self) -> f32 {
        self.scale
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, paint: &Paint) {
        self.fill_round_rect(x, y, w, h, 0.0, paint);
    }

    fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, paint: &Paint) {
        let (clip, m) = (self.clip_phys(), self.aa_margin());
        self.batch
            .push_round_rect(x, y, w, h, radius, paint, clip, m);
        self.maybe_flush();
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
        // 以下四步逐行照抄 `SkiaCanvas::stroke_round_rect`（src/render/skia.rs:221-241），
        // 连注释里的理由一起搬——两个后端在同一 DPI 下必须给出同一条描边。
        //
        // ① 对齐物理像素整数坐标：四边乘 scale 取整后还原，使描边中心落在物理半像素上，
        //    两侧各 0.5px 恰好覆盖完整一列像素，消除 125%/150% 等非整数 DPI 的亚像素糊边。
        let s = self.scale;
        let x0 = (x * s).round() / s;
        let y0 = (y * s).round() / s;
        let x1 = ((x + w) * s).round() / s;
        let y1 = ((y + h) * s).round() / s;
        let (x, y, w, h) = (x0, y0, x1 - x0, y1 - y0);
        // ② 线宽 clamp 到半边。
        let width = width.min(w / 2.0).min(h / 2.0).max(0.0);
        let half = width / 2.0;
        // ③ 描边中心线 = 内缩半个线宽的圆角矩形（tiny-skia/D2D 都以路径为中线对称外扩，
        //    内缩后描边正好落在 (x,y,w,h) 框内）。
        let (cw, ch) = (w - width, h - width);
        if cw < 0.0 || ch < 0.0 {
            return;
        }
        // ④ 圆角同步内缩，并 clamp 到中心线自身的半边（`rounded_rect_path` 内部那一条）。
        let cr = (radius - half).max(0.0).min(cw / 2.0).min(ch / 2.0);
        let (clip, m) = (self.clip_phys(), self.aa_margin());
        self.batch
            .push_stroke(x + half, y + half, cw, ch, cr, half, paint, clip, m);
        self.maybe_flush();
    }

    fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, paint: &Paint) {
        // 端帽是 Butt（skia.rs:262 的 `LineCap::Butt`）：线段两端不外延，
        // 几何上就是一个以线段为中轴的旋转矩形——`sd_segment_box` 直接给这个形状。
        let (clip, m) = (self.clip_phys(), self.aa_margin());
        self.batch
            .push_line(x0, y0, x1, y1, width / 2.0, paint, clip, m);
        self.maybe_flush();
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, paint: &Paint) {
        let (clip, m) = (self.clip_phys(), self.aa_margin());
        self.batch.push_circle(cx, cy, r, paint, clip, m);
        self.maybe_flush();
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
        // 禁用/退化判定与软后端同源（skia.rs:289）。`shadows_disabled()` 是 `pub(crate)`
        // 的共享开关，三个后端读同一份，`WINDUI_NOSHADOW` 才有一致的语义。
        if color.a == 0 || w <= 0.0 || h <= 0.0 || crate::render::skia::shadows_disabled() {
            return;
        }
        let s = self.scale;
        let pblur = (blur * s).max(0.0);
        // 软后端的模糊半径是 `pblur.round()`（3 趟 box-blur 的整数半径），这里以它为准换算，
        // 两条路径的扩散范围才对得上。
        let b = pblur.round();
        let pw = w * s;
        let ph = h * s;
        // 圆角 clamp 与 `rounded_rect_path` 同源（软后端在建路径时 clamp，这里提前做，
        // 因为解析阴影公式要求 corner ≤ 半边）。
        let pr = (radius * s).max(0.0).min(pw / 2.0).min(ph / 2.0);
        let (clip, m) = (self.clip_phys(), self.aa_margin());
        if b < 1.0 {
            // blur 退化 → 锐利圆角矩形（`Canvas::draw_shadow` 的契约）。
            self.batch
                .push_round_rect(x, y, w, h, pr / s, &Paint::fill(color), clip, m);
            self.maybe_flush();
            return;
        }
        // 半径 b 的 box-blur 单趟方差 = (b²+b)/3，3 趟叠加得 b²+b，故等效高斯 σ=√(b²+b)
        // （≈b，与 d2d 后端 `sigma = blur*scale` 的取法同量级）。软后端是三趟数值卷积、
        // 这里是解析高斯，两者只在这一个换算系数上握手。
        let sigma_px = (b * b + b).sqrt();
        // 3σ 覆盖高斯 99.7%，quad 再往外一像素兜住取整。外包框给足是关键：给少了
        // 模糊尾部会被 quad 边界直接切断，留下一圈直角硬边——软后端曾按 2×半径留 margin
        // 踩过同一个坑（skia.rs:300）。
        let margin_px = 3.0 * sigma_px + 1.0;
        // 体量保护，与软后端的 8192 上限对齐。
        if pw + 2.0 * margin_px > 8192.0 || ph + 2.0 * margin_px > 8192.0 {
            return;
        }
        self.batch.push_shadow(
            x,
            y,
            w,
            h,
            pr / s,
            // σ 与 margin 是在物理空间算出来的，而实例存逻辑值（顶点着色器统一乘 scale）。
            sigma_px / s,
            margin_px / s,
            color,
            clip,
        );
        self.maybe_flush();
    }

    /// P3 落点：图片纹理缓存（`tex.rs`）。当前为空实现。
    fn draw_image(&mut self, img: &Image, dst: Rect, fit: Fit, radius: f32, opacity: f32) {
        let _ = (img, dst, fit, radius, opacity);
        notice_once(
            &IMAGE_NOTICE,
            "windui: gpu 后端 draw_image 尚未实现（P3），本帧的图片不会被绘制",
        );
    }

    /// P2 落点：平台光栅进纹理缓存（`text.rs`）。当前为空实现。
    fn draw_text(&mut self, text: &str, rect: Rect, color: Color, align: Align, ts: &TextStyle) {
        let _ = (text, rect, color, align, ts);
        notice_once(
            &TEXT_NOTICE,
            "windui: gpu 后端 draw_text 尚未实现（P2），本帧的文字不会被绘制",
        );
    }

    fn measure_text(&mut self, text: &str, ts: &TextStyle) -> Size {
        self.engine.measure(text, ts, None)
    }

    fn measure_text_wrapped(&mut self, text: &str, ts: &TextStyle, max_width: f32) -> Size {
        self.engine.measure(text, ts, Some(max_width))
    }

    fn text_line_metrics(&mut self, text: &str, ts: &TextStyle) -> LineMetrics {
        self.engine.line_metrics(text, ts)
    }

    /// P3 落点：离屏层栈（`layer.rs`）。当前只记嵌套深度，`opacity` **不生效**——
    /// 子树会按不透明绘制。记数是为了 `pop_layer` 的守卫与帧末平衡断言仍然有效。
    fn push_layer(&mut self, opacity: f32) {
        let _ = opacity;
        self.layers += 1;
        notice_once(
            &LAYER_NOTICE,
            "windui: gpu 后端 push_layer 的 opacity 尚未生效（P3），子树按不透明绘制",
        );
    }

    fn pop_layer(&mut self) {
        // 守卫防下溢（仿软后端 `pop_layer` 的 `if let Some`）。
        debug_assert!(self.layers > 0, "pop_layer 多于 push_layer");
        self.layers = self.layers.saturating_sub(1);
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
        // 契约与另两个后端一致：每次 clip_rect 须配一次先行的 save()，否则裁剪会被
        // restore 遗漏而泄漏到兄弟节点。
        debug_assert!(
            !self.saves.is_empty(),
            "clip_rect 必须在 save() 之后调用，以与 restore() 配对"
        );
        // 各级求交（裁剪只会收窄），逻辑空间算——与软后端 `Clip::rect` 同源。
        let eff = match self.clips.last() {
            Some(c) => c.intersect(&r),
            None => r,
        };
        self.clips.push(eff);
    }
}

static TEXT_NOTICE: std::sync::Once = std::sync::Once::new();
static IMAGE_NOTICE: std::sync::Once = std::sync::Once::new();
static LAYER_NOTICE: std::sync::Once = std::sync::Once::new();

/// 进程内只提示一次分期空缺。每帧刷屏没人会看，一次则刚好够把「不是我布局写错了」
/// 这个判断送到眼前。
fn notice_once(once: &std::sync::Once, msg: &str) {
    once.call_once(|| eprintln!("{msg}"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::gpu::offscreen::OffscreenGpu;
    use crate::render::{Gradient, SkiaCanvas};
    use tiny_skia::Pixmap;

    /// 物理像素矩形 (x, y, w, h)，供「内部区域逐像素」判据取样。
    type Box2 = (u32, u32, u32, u32);

    fn sk_color(c: Color) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba8(c.r, c.g, c.b, c.a)
    }

    /// 背景色的**预乘**字节（Pixmap/离屏纹理存的都是预乘）。
    fn bg_bytes(c: Color) -> [u8; 4] {
        let a = c.a as u32;
        let p = |v: u8| ((v as u32 * a + 127) / 255) as u8;
        [p(c.r), p(c.g), p(c.b), c.a]
    }

    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * pm.width() + x) * 4) as usize;
        let d = pm.data();
        [d[i], d[i + 1], d[i + 2], d[i + 3]]
    }

    /// 软后端基准：同一串 Canvas 调用打到 `SkiaCanvas`。
    fn render_soft(
        w: u32,
        h: u32,
        scale: f32,
        bg: Color,
        draw: &dyn Fn(&mut dyn Canvas),
    ) -> Pixmap {
        let mut pm = Pixmap::new(w, h).expect("软后端 pixmap");
        pm.fill(sk_color(bg));
        let mut eng = crate::text::NullTextEngine;
        {
            let mut c = SkiaCanvas::with_text(&mut pm, &mut eng, scale);
            draw(&mut c);
        }
        pm
    }

    /// GPU 路径：同一串调用打到离屏目标再 readback。无适配器时 `None`（打印跳过）。
    fn render_gpu(
        w: u32,
        h: u32,
        scale: f32,
        bg: Color,
        draw: &dyn Fn(&mut dyn Canvas),
    ) -> Option<Pixmap> {
        let mut off = match OffscreenGpu::new(w, h) {
            Some(o) => o,
            None => {
                println!("跳过：本机没有可用的 wgpu 适配器（含软件回退），GPU 图元测试未执行");
                return None;
            }
        };
        off.clear(bg);
        let mut eng = crate::text::NullTextEngine;
        {
            let mut target = off.target();
            let mut canvas = target.make_canvas(&mut eng, scale);
            draw(&mut *canvas);
        }
        off.readback()
    }

    /// 「墨量」：全图相对背景的通道偏离总量。比单纯的通道和敏感得多——白底上画白色
    /// 图元、或者只画对一半，通道和几乎不动，墨量却会立刻掉下来
    /// （memory「视觉效果要量化验证」的同一条教训）。
    fn ink(pm: &Pixmap, bg: [u8; 4]) -> f64 {
        pm.data()
            .chunks_exact(4)
            .map(|p| {
                (0..4)
                    .map(|i| (p[i] as i32 - bg[i] as i32).unsigned_abs() as f64)
                    .sum::<f64>()
            })
            .sum()
    }

    /// 双后端同场景比对。判据分两层：
    /// - **内部区域逐像素**：`interior` 给的是图元收缩 2px 后的内部，通道差 ≤2。
    ///   这一层管「颜色/几何/混合对不对」。
    /// - **AA 边缘带**：全图墨量相对差 ≤3%。两套光栅器的边缘覆盖算法必然有亚像素差异，
    ///   逐像素比会永远红；墨量则能抓住「边缘整体偏胖/偏瘦」和「漏画了一块」。
    fn assert_matches_soft(
        name: &str,
        w: u32,
        h: u32,
        scale: f32,
        bg: Color,
        interior: &[Box2],
        draw: impl Fn(&mut dyn Canvas),
    ) {
        let soft = render_soft(w, h, scale, bg, &draw);
        let Some(gpu) = render_gpu(w, h, scale, bg, &draw) else {
            return;
        };

        let mut worst = (0i32, (0u32, 0u32), [0u8; 4], [0u8; 4]);
        for &(bx, by, bw, bh) in interior {
            assert!(bw > 0 && bh > 0, "[{name}] 内部取样框不能为空");
            for y in by..by + bh {
                for x in bx..bx + bw {
                    let (a, b) = (px(&soft, x, y), px(&gpu, x, y));
                    let d = (0..4)
                        .map(|i| (a[i] as i32 - b[i] as i32).abs())
                        .max()
                        .unwrap_or(0);
                    if d > worst.0 {
                        worst = (d, (x, y), a, b);
                    }
                }
            }
        }
        let bgb = bg_bytes(bg);
        let (is, ig) = (ink(&soft, bgb), ink(&gpu, bgb));
        let rel = if is > 0.0 { (ig - is).abs() / is } else { ig };
        println!(
            "[{name}] 内部最大通道差={} 墨量 软={is:.0} GPU={ig:.0} 相对差={:.4}%",
            worst.0,
            rel * 100.0
        );
        assert!(
            worst.0 <= 2,
            "[{name}] 内部逐像素超差：({},{}) 软={:?} GPU={:?}，最大通道差 {}",
            worst.1 .0,
            worst.1 .1,
            worst.2,
            worst.3,
            worst.0
        );
        assert!(
            is > 0.0,
            "[{name}] 软后端墨量为 0——这条测试什么都没画，判据是空的"
        );
        assert!(
            rel <= 0.03,
            "[{name}] 墨量相对差 {:.2}% 超过 3%（软={is:.0} GPU={ig:.0}）",
            rel * 100.0
        );
    }

    const WHITE: Color = Color::rgb(255, 255, 255);
    const RED: Color = Color::rgb(220, 60, 50);

    #[test]
    fn fill_rect_matches_soft_backend() {
        assert_matches_soft(
            "fill_rect",
            100,
            100,
            1.0,
            WHITE,
            &[(22, 22, 56, 36)],
            |c| {
                c.fill_rect(20.0, 20.0, 60.0, 40.0, &Paint::fill(RED));
            },
        );
    }

    #[test]
    fn fill_round_rect_matches_soft_backend() {
        assert_matches_soft(
            "round_rect",
            100,
            100,
            1.0,
            WHITE,
            &[(34, 34, 32, 22)],
            |c| {
                c.fill_round_rect(20.0, 20.0, 60.0, 50.0, 12.0, &Paint::fill(RED));
            },
        );
    }

    /// 圆角超过半边时两边必须 clamp 到同一个值（→ 胶囊形），否则形状直接不同。
    #[test]
    fn round_rect_radius_clamp_matches_soft_backend() {
        assert_matches_soft(
            "round_rect_clamped",
            80,
            60,
            1.0,
            WHITE,
            &[(36, 22, 8, 26)],
            |c| {
                c.fill_round_rect(20.0, 20.0, 40.0, 30.0, 100.0, &Paint::fill(RED));
            },
        );
    }

    #[test]
    fn fill_circle_matches_soft_backend() {
        assert_matches_soft("circle", 100, 100, 1.0, WHITE, &[(31, 31, 38, 38)], |c| {
            c.fill_circle(50.0, 50.0, 30.0, &Paint::fill(RED));
        });
    }

    /// 描边：内部取样落在 6px 边带中间两行，钉住「描边内缩、外边界贴住 rect」这条语义。
    /// 取样若落在带外，说明描边跑到框外去了（居中而非内缩）。
    #[test]
    fn stroke_round_rect_matches_soft_backend() {
        assert_matches_soft(
            "stroke_rect",
            100,
            80,
            1.0,
            WHITE,
            &[(30, 22, 40, 2), (30, 56, 40, 2)],
            |c| {
                c.stroke_round_rect(20.0, 20.0, 60.0, 40.0, 0.0, 6.0, &Paint::fill(RED));
            },
        );
    }

    /// 斜线：取样落在线段中点附近，钉住 Butt 端帽下的线宽与走向。
    #[test]
    fn draw_line_matches_soft_backend() {
        assert_matches_soft("line", 100, 100, 1.0, WHITE, &[(48, 43, 4, 4)], |c| {
            c.draw_line(20.0, 20.0, 80.0, 70.0, 6.0, &Paint::fill(RED));
        });
    }

    /// 线性渐变（左蓝右红），对标 `fill_round_rect_linear_gradient_left_to_right`。
    /// 内部逐像素比的是整块渐变——插值空间（预乘/非预乘）或映射方向搞错，这里立刻红。
    #[test]
    fn linear_gradient_matches_soft_backend() {
        // 取样带避开四个 8px 圆角：那里是 AA 边缘，归第二层墨量判据管。
        assert_matches_soft(
            "linear_gradient",
            100,
            40,
            1.0,
            WHITE,
            &[(2, 11, 96, 18)],
            |c| {
                let g = Gradient::linear(
                    (0.0, 0.5),
                    (1.0, 0.5),
                    vec![(0.0, Color::hex(0x0000FF)), (1.0, Color::hex(0xFF0000))],
                );
                c.fill_round_rect(0.0, 0.0, 100.0, 40.0, 8.0, &Paint::gradient(g));
            },
        );
    }

    /// 径向渐变（中心亮），对标 `fill_rect_radial_gradient_center_to_edge`。
    #[test]
    fn radial_gradient_matches_soft_backend() {
        assert_matches_soft(
            "radial_gradient",
            60,
            60,
            1.0,
            Color::rgb(0, 0, 0),
            &[(2, 2, 56, 56)],
            |c| {
                let g = Gradient::radial(
                    (0.5, 0.5),
                    1.0,
                    vec![(0.0, Color::hex(0xFFFFFF)), (1.0, Color::hex(0x000000))],
                );
                c.fill_rect(0.0, 0.0, 60.0, 60.0, &Paint::gradient(g));
            },
        );
    }

    /// 薄裁剪带内填充，对标软后端的 `thin_clip_rect_does_not_drop_fill` 回归：
    /// 裁剪算错一个像素，6px 高的进度条会整条消失。
    #[test]
    fn thin_clip_rect_matches_soft_backend() {
        assert_matches_soft("thin_clip", 100, 100, 1.0, WHITE, &[(26, 41, 28, 4)], |c| {
            c.save();
            c.clip_rect(Rect::new(10, 40, 80, 6));
            c.fill_round_rect(
                20.0,
                40.0,
                40.0,
                6.0,
                3.0,
                &Paint::fill(Color::hex(0xFF0000)),
            );
            c.restore();
        });
    }

    /// 半透明叠加：50% 红盖白底应得粉色。预乘约定错了这里会整体发白或发黑。
    #[test]
    fn translucent_overlay_matches_soft_backend() {
        assert_matches_soft("translucent", 60, 60, 1.0, WHITE, &[(2, 2, 56, 56)], |c| {
            c.fill_rect(
                0.0,
                0.0,
                60.0,
                60.0,
                &Paint::fill(Color::rgba(255, 0, 0, 128)),
            );
        });
        // 单独把「是粉不是红也不是白」钉死：上面的逐像素比对只保证两后端一致，
        // 两边一起错的话它是发现不了的。
        let Some(pm) = render_gpu(20, 20, 1.0, WHITE, &|c| {
            c.fill_rect(
                0.0,
                0.0,
                20.0,
                20.0,
                &Paint::fill(Color::rgba(255, 0, 0, 128)),
            );
        }) else {
            return;
        };
        let p = px(&pm, 10, 10);
        assert!(p[0] > 240, "红通道应仍高，实得 {p:?}");
        assert!(
            (100..200).contains(&(p[1] as i32)) && (100..200).contains(&(p[2] as i32)),
            "绿/蓝应被白底抬到中段（50% 合成 → 粉），实得 {p:?}"
        );
        assert_eq!(p[3], 255, "不透明背景上合成后 alpha 应仍为满");
    }

    // ---- scale = 1.5：逻辑坐标不变、物理尺寸 ×1.5 ----

    #[test]
    fn fill_round_rect_matches_soft_backend_at_scale_1_5() {
        assert_matches_soft(
            "round_rect@1.5",
            150,
            150,
            1.5,
            WHITE,
            &[(51, 51, 48, 33)],
            |c| {
                c.fill_round_rect(20.0, 20.0, 60.0, 50.0, 12.0, &Paint::fill(RED));
            },
        );
    }

    #[test]
    fn fill_circle_matches_soft_backend_at_scale_1_5() {
        assert_matches_soft(
            "circle@1.5",
            150,
            150,
            1.5,
            WHITE,
            &[(47, 47, 56, 56)],
            |c| {
                c.fill_circle(50.0, 50.0, 30.0, &Paint::fill(RED));
            },
        );
    }

    #[test]
    fn stroke_round_rect_matches_soft_backend_at_scale_1_5() {
        assert_matches_soft(
            "stroke_rect@1.5",
            150,
            120,
            1.5,
            WHITE,
            &[(45, 33, 60, 3), (45, 84, 60, 3)],
            |c| {
                c.stroke_round_rect(20.0, 20.0, 60.0, 40.0, 0.0, 6.0, &Paint::fill(RED));
            },
        );
    }

    #[test]
    fn linear_gradient_matches_soft_backend_at_scale_1_5() {
        assert_matches_soft(
            "linear_gradient@1.5",
            150,
            60,
            1.5,
            WHITE,
            &[(3, 16, 144, 28)],
            |c| {
                let g = Gradient::linear(
                    (0.0, 0.5),
                    (1.0, 0.5),
                    vec![(0.0, Color::hex(0x0000FF)), (1.0, Color::hex(0xFF0000))],
                );
                c.fill_round_rect(0.0, 0.0, 100.0, 40.0, 8.0, &Paint::gradient(g));
            },
        );
    }

    // ---- 裁剪栈 ----

    /// `save → clip → restore` 之后的绘制不再受裁剪影响。
    #[test]
    fn restore_drops_clip() {
        let Some(pm) = render_gpu(60, 60, 1.0, WHITE, &|c| {
            c.save();
            c.clip_rect(Rect::new(0, 0, 10, 10));
            c.restore();
            c.fill_rect(0.0, 0.0, 60.0, 60.0, &Paint::fill(RED));
        }) else {
            return;
        };
        for (x, y) in [(5, 5), (30, 30), (55, 55)] {
            let p = px(&pm, x, y);
            assert_eq!(
                (p[0], p[1], p[2]),
                (RED.r, RED.g, RED.b),
                "restore 后裁剪应已失效，({x},{y}) 实得 {p:?}"
            );
        }
    }

    /// 嵌套裁剪取交集（只会收窄，不会放宽）。
    #[test]
    fn nested_clips_intersect() {
        let Some(pm) = render_gpu(60, 60, 1.0, WHITE, &|c| {
            c.save();
            c.clip_rect(Rect::new(10, 10, 40, 40));
            c.save();
            c.clip_rect(Rect::new(30, 0, 40, 60)); // 交集 = (30,10,20,40)
            c.fill_rect(0.0, 0.0, 60.0, 60.0, &Paint::fill(RED));
            c.restore();
            c.restore();
        }) else {
            return;
        };
        let inside = px(&pm, 40, 30);
        assert_eq!((inside[0], inside[1], inside[2]), (RED.r, RED.g, RED.b));
        for (x, y) in [(20, 30), (40, 5), (55, 30), (40, 55)] {
            let p = px(&pm, x, y);
            assert_eq!(
                (p[0], p[1], p[2]),
                (255, 255, 255),
                "交集外的 ({x},{y}) 不应被绘制，实得 {p:?}"
            );
        }
    }

    /// 内层 restore 之后回到外层裁剪（而不是回到无裁剪）。
    #[test]
    fn restore_returns_to_outer_clip() {
        let Some(pm) = render_gpu(60, 60, 1.0, WHITE, &|c| {
            c.save();
            c.clip_rect(Rect::new(10, 10, 40, 40));
            c.save();
            c.clip_rect(Rect::new(30, 30, 10, 10));
            c.restore();
            c.fill_rect(0.0, 0.0, 60.0, 60.0, &Paint::fill(RED));
            c.restore();
        }) else {
            return;
        };
        let p = px(&pm, 15, 15);
        assert_eq!(
            (p[0], p[1], p[2]),
            (RED.r, RED.g, RED.b),
            "应回到外层裁剪（含 15,15），实得 {p:?}"
        );
        let out = px(&pm, 5, 5);
        assert_eq!(
            (out[0], out[1], out[2]),
            (255, 255, 255),
            "外层裁剪之外仍不应被绘制，实得 {out:?}"
        );
    }

    // ---- 抗锯齿开关 ----

    /// `anti_alias=false` 时边界上没有过渡带：跨越边缘的一行像素非背景即图元色。
    #[test]
    fn anti_alias_off_has_no_transition_band() {
        // 圆的边界最容易暴露过渡带（矩形边界本就落在整像素上）。
        let Some(pm) = render_gpu(80, 80, 1.0, WHITE, &|c| {
            let mut p = Paint::fill(Color::rgb(0, 0, 0));
            p.anti_alias = false;
            c.fill_circle(40.0, 40.0, 25.5, &p);
        }) else {
            return;
        };
        for x in 0..80u32 {
            let p = px(&pm, x, 40);
            let solid = p == [0, 0, 0, 255];
            let empty = p == [255, 255, 255, 255];
            assert!(
                solid || empty,
                "关闭抗锯齿后不应出现中间色，({x},40) 实得 {p:?}"
            );
        }
        // 反证：开着抗锯齿时同一条扫描线上确实存在中间色，否则上面那条测试是空的。
        let Some(aa) = render_gpu(80, 80, 1.0, WHITE, &|c| {
            c.fill_circle(40.0, 40.0, 25.5, &Paint::fill(Color::rgb(0, 0, 0)));
        }) else {
            return;
        };
        let has_mid = (0..80u32).any(|x| {
            let p = px(&aa, x, 40);
            p != [0, 0, 0, 255] && p != [255, 255, 255, 255]
        });
        assert!(has_mid, "开启抗锯齿时边缘应有过渡像素");
    }

    // ---- 阴影专项（不与软后端逐像素比：模糊算法不同）----

    /// 对标软后端 `draw_shadow_produces_soft_halo`：紧邻边缘变暗、外缘柔化过渡、
    /// 远处保持背景色。
    #[test]
    fn shadow_produces_soft_halo() {
        if crate::render::skia::shadows_disabled() {
            println!("跳过：WINDUI_NOSHADOW 已置位，阴影形状判据不适用");
            return;
        }
        let Some(pm) = render_gpu(120, 120, 1.0, WHITE, &|c| {
            c.draw_shadow(40.0, 40.0, 40.0, 40.0, 8.0, 10.0, Color::rgba(0, 0, 0, 180));
        }) else {
            return;
        };
        let cr = px(&pm, 60, 60)[0];
        assert!(cr < 120, "投影中心应变暗，实得 {cr}");
        let er = px(&pm, 86, 60)[0];
        assert!(er > 130 && er < 252, "外缘应为柔化过渡，实得 {er}");
        let fr = px(&pm, 4, 4)[0];
        assert!(fr > 250, "远角应保持白，实得 {fr}");
    }

    /// 对标软后端 `shadow_fades_out_without_a_hard_cutoff_ring`：外缘必须渐隐到底，
    /// 不能在某一圈上突然截断。GPU 侧对应的失效模式是 quad 外包框留少了模糊外扩，
    /// 症状一模一样——最后一个可见暗度还是个大数。
    #[test]
    fn shadow_fades_out_without_a_hard_cutoff_ring() {
        if crate::render::skia::shadows_disabled() {
            println!("跳过：WINDUI_NOSHADOW 已置位，阴影形状判据不适用");
            return;
        }
        let Some(pm) = render_gpu(400, 400, 1.0, WHITE, &|c| {
            c.draw_shadow(
                150.0,
                150.0,
                100.0,
                100.0,
                10.0,
                18.0,
                Color::rgba(0, 0, 0, 180),
            );
        }) else {
            return;
        };
        let dark: Vec<i32> = (250..399)
            .map(|x| 255 - px(&pm, x, 200)[0] as i32)
            .collect();
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
            "渐隐尾部最后一个可见暗度为 {}（第 {last} 像素处），说明模糊被 quad 外包框切断",
            dark[last]
        );
        assert!(
            dark[0] > 10,
            "紧邻边缘应有可见暗度（否则这条测试没测到东西）"
        );
        assert_eq!(*dark.last().unwrap(), 0, "足够远处应回到纯白");
    }

    /// 阴影的模糊范围要与软后端同量级：解析高斯与 3 趟 box-blur 不会逐像素相同，
    /// 但「可见渐隐走多远」必须对得上，否则同一份主题在两个后端下的浮层观感是两套。
    #[test]
    fn shadow_spread_is_comparable_to_soft_backend() {
        if crate::render::skia::shadows_disabled() {
            println!("跳过：WINDUI_NOSHADOW 已置位");
            return;
        }
        let draw = |c: &mut dyn Canvas| {
            c.draw_shadow(
                150.0,
                150.0,
                100.0,
                100.0,
                10.0,
                18.0,
                Color::rgba(0, 0, 0, 180),
            );
        };
        let soft = render_soft(400, 400, 1.0, WHITE, &draw);
        let Some(gpu) = render_gpu(400, 400, 1.0, WHITE, &draw) else {
            return;
        };
        let reach = |pm: &Pixmap| {
            (250..399)
                .rposition(|x| 255 - px(pm, x as u32, 200)[0] as i32 > 3)
                .unwrap_or(0) as i32
        };
        let (rs, rg) = (reach(&soft), reach(&gpu));
        println!("[shadow] 可见渐隐距离 软={rs}px GPU={rg}px");
        assert!(
            (rs - rg).abs() <= 12,
            "两后端的阴影扩散范围差 {}px（软={rs} GPU={rg}），σ 换算多半错了",
            (rs - rg).abs()
        );
        // 峰值暗度也要同量级（否则可能一边整体淡一半）。
        let peak = |pm: &Pixmap| 255 - px(pm, 200, 200)[0] as i32;
        let (ps, pg) = (peak(&soft), peak(&gpu));
        assert!(
            (ps - pg).abs() <= 20,
            "投影中心暗度差 {}（软={ps} GPU={pg}）",
            (ps - pg).abs()
        );
    }

    /// `WINDUI_NOSHADOW` 开关：三个后端读同一个 `shadows_disabled()`，行为必须一致。
    ///
    /// 不在这里 `set_var`——环境变量是进程全局的，测试并行跑会互相污染，而
    /// `shadows_disabled()` 又用 `OnceLock` 缓存首次读到的值，设了也未必生效。
    /// 改成读当前开关状态、断言 GPU 后端与之相符：未置位时必须画出阴影，置位时
    /// 必须一个像素都不画。两种环境下都是有效判据。
    #[test]
    fn shadow_honors_noshadow_switch() {
        let disabled = crate::render::skia::shadows_disabled();
        let Some(pm) = render_gpu(120, 120, 1.0, WHITE, &|c| {
            c.draw_shadow(40.0, 40.0, 40.0, 40.0, 8.0, 10.0, Color::rgba(0, 0, 0, 180));
        }) else {
            return;
        };
        let painted = pm
            .data()
            .chunks_exact(4)
            .any(|p| p != [255u8, 255, 255, 255]);
        if disabled {
            assert!(!painted, "WINDUI_NOSHADOW 已置位，不应画出任何阴影像素");
        } else {
            assert!(painted, "未禁用阴影时应画出阴影");
        }
    }

    /// `blur<=0` 退化为锐利圆角矩形（`Canvas::draw_shadow` 的契约），与软后端一致。
    #[test]
    fn zero_blur_shadow_is_a_sharp_round_rect() {
        if crate::render::skia::shadows_disabled() {
            println!("跳过：WINDUI_NOSHADOW 已置位");
            return;
        }
        assert_matches_soft(
            "shadow_blur0",
            80,
            80,
            1.0,
            WHITE,
            &[(32, 32, 16, 16)],
            |c| {
                c.draw_shadow(20.0, 20.0, 40.0, 40.0, 8.0, 0.0, Color::rgba(0, 0, 0, 200));
            },
        );
    }

    // ---- 分期空缺的行为 ----

    /// 未实现的图元不得把已画的东西弄坏，也不得 panic。
    #[test]
    fn unimplemented_primitives_are_no_ops() {
        let Some(pm) = render_gpu(40, 40, 1.0, WHITE, &|c| {
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(RED));
            c.draw_text(
                "hi",
                Rect::new(0, 0, 40, 40),
                Color::rgb(0, 0, 0),
                Align::Start,
                &TextStyle::new(14.0),
            );
            c.push_layer(0.5);
            c.pop_layer();
        }) else {
            return;
        };
        assert_eq!(px(&pm, 20, 20), [RED.r, RED.g, RED.b, 255]);
    }

    /// 度量委托给传入的 `engine`（而不是自己瞎猜）：`NullTextEngine` 给什么，
    /// GPU canvas 就得给什么。
    #[test]
    fn text_metrics_are_delegated_to_engine() {
        let Some(mut off) = OffscreenGpu::new(16, 16) else {
            println!("跳过：本机没有可用的 wgpu 适配器");
            return;
        };
        let ts = TextStyle::new(14.0);
        let want = {
            let mut e = crate::text::NullTextEngine;
            (
                e.measure("hello", &ts, None),
                e.measure("hello world", &ts, Some(40.0)),
                e.line_metrics("hello", &ts),
            )
        };
        let mut eng = crate::text::NullTextEngine;
        let mut target = off.target();
        let mut c = target.make_canvas(&mut eng, 1.0);
        assert_eq!(c.measure_text("hello", &ts), want.0);
        assert_eq!(c.measure_text_wrapped("hello world", &ts, 40.0), want.1);
        let lm = c.text_line_metrics("hello", &ts);
        assert_eq!((lm.ascent, lm.descent), (want.2.ascent, want.2.descent));
    }

    /// 单批上限：图元数超过 `MAX_BATCH` 时会中途 flush，叠放顺序不能因此改变
    /// （painter's algorithm）。
    #[test]
    fn oversized_batch_preserves_paint_order() {
        let n = MAX_BATCH + 32;
        let Some(pm) = render_gpu(40, 40, 1.0, WHITE, &|c| {
            for i in 0..n {
                // 前面全是蓝、最后一笔是红：中途 flush 若把顺序打乱，中心就不是红。
                let col = if i + 1 == n {
                    Color::rgb(255, 0, 0)
                } else {
                    Color::rgb(0, 0, 255)
                };
                c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(col));
            }
        }) else {
            return;
        };
        assert_eq!(px(&pm, 20, 20), [255, 0, 0, 255], "最后一笔应压在最上面");
    }
}
