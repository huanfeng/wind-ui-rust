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
//! # 三条管线与叠放次序
//!
//! 几何（`prim.rs`）、文字（`text.rs`）、图片（`tex.rs`）各有一条管线，各攒各的批。
//! 任一批入批前先把另两批画掉（[`WgpuCanvas::before_prim`] 及 `draw_text`/`draw_image`
//! 开头那两行），于是**屏幕上的叠放次序恒等于 `Canvas` 的调用次序**。
//! `push_layer`/`pop_layer` 则在切换绘制目标前后各 flush 一次（[`WgpuCanvas::flush_all`]）。
//!
//! # 分期空缺
//!
//! - `measure_text` / `measure_text_wrapped` / `text_line_metrics`：**委托 `make_canvas`
//!   传入的 `engine`**。这不是桩——软后端同样是委托它，排版与度量本就属于平台文字栈，
//!   GPU 后端（P2）只接管「光栅出来的像素怎么上屏」那一段。
//! - `draw_text`：已实现（P2），但引擎不提供 `GlyphSource` 时仍是空操作 + 一次提示
//!   （Windows 的 DirectWrite 引擎走 D2D 后端，不实现它）。

use std::sync::Arc;

use super::device::SharedGpu;
use super::layer::LayerTexture;
use super::prim::{PrimBatch, PrimRenderer, MAX_BATCH};
use super::tex::{image_item, place_image, ImageItem, MAX_IMAGE_BATCH};
use super::text::{glyph_item, text_item, RunKey, TextItem, MAX_TEXT_BATCH};
use crate::geometry::{Color, Rect, Size};
use crate::render::image::{Fit, Image};
use crate::render::{Canvas, Paint, RenderTarget};
use crate::spec::Align;
use crate::text::{block_offset_y, LineMetrics, RunRequest, TextEngine, TextStyle};

/// 一帧的 GPU 渲染目标：一张颜色纹理视图 + 跨帧复用的管线与缓冲。
pub struct WgpuTarget<'t> {
    gpu: Arc<SharedGpu>,
    view: &'t wgpu::TextureView,
    renderer: &'t mut PrimRenderer,
    /// 目标物理尺寸（像素）。
    size: (u32, u32),
    /// 本帧只允许落笔的物理矩形（`x, y, w, h`），`None` = 整窗。见 [`WgpuCanvas::scissor`]。
    scissor: Option<[u32; 4]>,
    /// 本目标画的是**常驻**色纹理（窗口后备缓冲），故上一帧的内容还在，可以只重画一块。
    /// 离屏目标恒 `false`：它每次都是新的一张。
    partial: bool,
    /// 宿主没宣告重绘范围时的兜底铺底色。`None` = 不兜底（离屏目标由调用方自己清）。
    ///
    /// 存在的理由是向后兼容：不经宿主重绘决策的调用方（离屏截图、测试）从来只调
    /// `make_canvas`，而窗口目标必须保证「开画前底是干净的」——把兜底放在这里，
    /// 老路径的行为与此前的「开帧即清屏」逐字相同。
    fallback_bg: Option<Color>,
    /// 是否已经宣告过本帧的重绘范围（[`RenderTarget::begin_damage`]）。
    damage_set: bool,
}

impl<'t> WgpuTarget<'t> {
    pub(super) fn new(
        gpu: Arc<SharedGpu>,
        view: &'t wgpu::TextureView,
        renderer: &'t mut PrimRenderer,
        size: (u32, u32),
        partial: bool,
        fallback_bg: Option<Color>,
    ) -> Self {
        Self {
            gpu,
            view,
            renderer,
            size,
            scissor: None,
            partial,
            fallback_bg,
            damage_set: false,
        }
    }

    /// 把整张目标铺成 `color`（一个只做 `LoadOp::Clear` 的 pass）。
    ///
    /// 对应软后端全窗帧的 `pixmap.fill(bg)`：图元 pass 一律 `LoadOp::Load`
    /// （painter's algorithm），底必须先铺好。
    fn clear_all(&self, color: Color) {
        let mut encoder =
            self.gpu
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("windui target clear"),
                });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("windui target clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color(color)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.gpu.queue().submit([encoder.finish()]);
    }
}

/// `Color`（非预乘 sRGB 字节）→ 清屏值：先预乘再归一化。
///
/// `*Unorm` 目标下 `round(v * 255)` 精确还原字节，故清屏色能逐字节对上预期的预乘结果
/// ——这是双后端截图比对「内部通道差 = 0」的前提之一（`*Srgb` 格式会再编码一次）。
///
/// 三处清屏（窗口目标、离屏目标、层）**必须**是同一个算法：同一个 `bg` 在窗口里与在
/// 截图里若不同色，最难被认成是清屏色算错了。曾经各留一份并靠注释约定「改动须同步」，
/// 到第三处出现时收口成这一份。
pub(super) fn clear_color(c: Color) -> wgpu::Color {
    let a = c.a as u32;
    let premul = |v: u8| ((v as u32 * a + 127) / 255) as f64 / 255.0;
    wgpu::Color {
        r: premul(c.r),
        g: premul(c.g),
        b: premul(c.b),
        a: c.a as f64 / 255.0,
    }
}

impl RenderTarget for WgpuTarget<'_> {
    fn supports_partial(&mut self) -> bool {
        self.partial
    }

    /// 宣告本帧的重绘范围。整窗时顺带铺底；局部时**不铺**——脏区那一块的底由宿主
    /// 自己画（对应软后端局部帧的 `sub.fill(bg)`），脏区之外则要原样保留上一帧。
    fn begin_damage(&mut self, damage: Option<Rect>, bg: Color) {
        self.damage_set = true;
        match damage.filter(|_| self.partial) {
            None => {
                self.scissor = None;
                self.clear_all(bg);
            }
            Some(r) => self.scissor = Some(clamp_scissor(r, self.size)),
        }
    }

    fn make_canvas<'a>(
        &'a mut self,
        engine: &'a mut dyn TextEngine,
        scale: f32,
    ) -> Box<dyn Canvas + 'a> {
        // 兜底：没经过重绘决策的调用方（离屏截图、测试）此前靠的是「开帧即清屏」。
        if !self.damage_set {
            self.damage_set = true;
            if let Some(bg) = self.fallback_bg {
                self.clear_all(bg);
            }
        }
        Box::new(WgpuCanvas {
            gpu: self.gpu.clone(),
            view: self.view,
            renderer: &mut *self.renderer,
            engine,
            batch: PrimBatch::default(),
            text_batch: Vec::new(),
            image_batch: Vec::new(),
            encoder: None,
            scissor: self.scissor,
            size: self.size,
            scale: scale.max(0.01),
            clips: Vec::new(),
            saves: Vec::new(),
            layers: Vec::new(),
        })
    }
    // `as_pixmap` 用 trait 默认的 None：GPU 的像素读不回宿主。
    //
    // 但**局部重绘不再依赖它**——那条能力现在由 `supports_partial` + `begin_damage`
    // 承担（见本文件下方的 `RenderTarget` 实现与 `surface.rs::BackBuffer`）。`as_pixmap`
    // 退回它本来的语义：软后端局部重绘快路取原始 Pixmap 的那个口子，GPU 与 d2d 都没有。
}

/// 攒图元的 `Canvas`。析构时把本帧攒下的实例一次画完。
pub struct WgpuCanvas<'a> {
    gpu: Arc<SharedGpu>,
    /// **基础**渲染目标（窗口后备缓冲或离屏纹理）。层栈非空时当前目标是栈顶那张层纹理，
    /// 取法统一走 [`current_view`]。
    view: &'a wgpu::TextureView,
    renderer: &'a mut PrimRenderer,
    engine: &'a mut dyn TextEngine,
    batch: PrimBatch,
    /// 待画的文字。与另两批 **互斥非空**：入批前互相 flush，见 [`WgpuCanvas::flush_all`]。
    text_batch: Vec<TextItem>,
    /// 待画的图片（含 `pop_layer` 的合成 quad）。同上，与另两批互斥非空。
    image_batch: Vec<ImageItem>,
    /// 本帧的命令 encoder（懒建）：三条管线的每一批都录进它，**帧末一次提交**。
    ///
    /// 见 [`WgpuCanvas::before_prim`]：交错 flush 的次数与控件数同阶（一帧上百次），
    /// 而每次 `queue.submit` 实测约 90 µs。录进同一个 encoder 后交错次数不变、提交
    /// 次数降到 1——叠放次序仍由录制顺序保证（同一 encoder 内的 pass 按录制顺序执行）。
    encoder: Option<wgpu::CommandEncoder>,
    /// 本帧的裁剪盒（物理整数 `x, y, w, h`），`None` = 整窗。局部重绘帧由调用方设上，
    /// 三条管线的每个 pass 都据它切 scissor——落在脏区外的片元于是连混合都不做。
    ///
    /// 与实例里那个逐像素 `clip` 字段是两回事：那个是 `Canvas::clip_rect` 的语义裁剪
    /// （必须逐像素、且与软后端的 mask 边界逐字对齐），这个是整帧一律生效的呈现窗口。
    scissor: Option<[u32; 4]>,
    size: (u32, u32),
    scale: f32,
    /// 裁剪栈：存**逻辑**矩形，每一层已是各级交集（只会收窄）。
    clips: Vec<Rect>,
    /// `save()` 记下的栈深，`restore()` 据此回弹。
    saves: Vec<usize>,
    /// 离屏层栈（P3）。栈顶即当前绘制目标。
    ///
    /// 元素是 `Option`：层纹理分配失败时压一个 `None` 占位——**栈必须保持平衡**，
    /// 否则后续 `pop_layer` 会把外层的层提前合成掉，画面错得离成因很远。占位期间子树
    /// 直接画到最近的外层目标上（opacity 失效，内容仍在），对标软后端「分配失败退化成
    /// 1×1/0 透明度」那条守卫的同一个意图：宁可少一层效果，不可乱栈。
    layers: Vec<Option<LayerTexture>>,
}

#[cfg(test)]
thread_local! {
    /// 本线程提交过的 command buffer 数。理由同 `layer.rs` 的 `ALLOCS`：「一帧只提交
    /// 一次」是这次改动的**收益本身**，而它不改变任何一个像素——退回「每批一提交」
    /// 只会让帧时间翻几倍，没有计数器就只能等下次量帧率才发现。
    static SUBMITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// 本线程已提交的 command buffer 数（测试判据用）。
#[cfg(test)]
pub(super) fn submit_count() -> u64 {
    SUBMITS.with(|c| c.get())
}

/// 逻辑上的脏区（**物理**像素 `Rect`）→ scissor 的 `[x, y, w, h]`，并收进目标边界。
///
/// 完全落在目标外时返回零宽高：那是「本帧什么都不该画」，比放行整窗安全——脏区算错
/// 的症状是「界面某一块不刷新」，而放行整窗的症状是「局部帧把没重画的区域清成底色」。
fn clamp_scissor(r: Rect, (w, h): (u32, u32)) -> [u32; 4] {
    let x0 = r.x.max(0).min(w as i32) as u32;
    let y0 = r.y.max(0).min(h as i32) as u32;
    let x1 = r.right().max(0).min(w as i32) as u32;
    let y1 = r.bottom().max(0).min(h as i32) as u32;
    [x0, y0, x1.saturating_sub(x0), y1.saturating_sub(y0)]
}

/// 当前绘制目标：最近一个成功分配的层纹理，没有则是基础目标。
///
/// 写成自由函数而不是 `&self` 方法：调用点要同时可变借用 `self.renderer`/`self.batch`，
/// 而 `&self` 方法会把整个 `self` 借出去。分字段借用只在函数体内成立。
fn current_view<'v>(
    layers: &'v [Option<LayerTexture>],
    base: &'v wgpu::TextureView,
) -> &'v wgpu::TextureView {
    layers
        .iter()
        .rev()
        .flatten()
        .next()
        .map(|l| l.view())
        .unwrap_or(base)
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
            self.flush_prims();
        }
        // 渐变表在帧内累积（见 `PrimBatch::clear`），满了必须**真提交**一次再重置：
        // 表里的色标被已录制、尚未提交的实例引用着，不提交就重置会让它们在执行时
        // 读到后来者的颜色。一帧超过 64 组渐变本就罕见，退化成多一次提交即可。
        if self.batch.grads_full() {
            self.flush_prims();
            self.submit();
            self.batch.reset_grads();
        }
    }

    /// 本帧 encoder（懒建）。一帧一个图元都没有的目标不该付一次 encoder 分配。
    fn ensure_encoder(&mut self) {
        if self.encoder.is_none() {
            let enc = self
                .gpu
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("windui frame encoder"),
                });
            self.encoder = Some(enc);
        }
    }

    /// 把已录下的所有 pass 一次提交掉。帧末调一次；渐变表满时也会提前调。
    fn submit(&mut self) {
        if let Some(enc) = self.encoder.take() {
            self.gpu.queue().submit([enc.finish()]);
            #[cfg(test)]
            SUBMITS.with(|c| c.set(c.get() + 1));
        }
    }

    /// 录制本批几何实例（画到**当前**目标：层栈顶或基础目标）。
    fn flush_prims(&mut self) {
        if self.batch.is_empty() {
            return;
        }
        self.ensure_encoder();
        let view = current_view(&self.layers, self.view);
        let scissor = self.scissor;
        let enc = self.encoder.as_mut().expect("ensure_encoder 刚建过");
        self.renderer.flush(
            &self.gpu,
            view,
            self.size,
            self.scale,
            scissor,
            enc,
            &mut self.batch,
        );
    }

    /// 录制本批文字。
    fn flush_text(&mut self) {
        if self.text_batch.is_empty() {
            return;
        }
        self.ensure_encoder();
        let gpu = self.gpu.clone();
        let view = current_view(&self.layers, self.view);
        let (size, scissor) = (self.size, self.scissor);
        let enc = self.encoder.as_mut().expect("ensure_encoder 刚建过");
        self.renderer
            .text(&gpu)
            .flush(&gpu, view, size, scissor, enc, &mut self.text_batch);
    }

    /// 录制本批图片（含层合成 quad）。
    fn flush_images(&mut self) {
        if self.image_batch.is_empty() {
            return;
        }
        self.ensure_encoder();
        let gpu = self.gpu.clone();
        let view = current_view(&self.layers, self.view);
        let (size, scissor) = (self.size, self.scissor);
        let enc = self.encoder.as_mut().expect("ensure_encoder 刚建过");
        self.renderer
            .image(&gpu)
            .flush(&gpu, view, size, scissor, enc, &mut self.image_batch);
    }

    /// 把三批全部画到当前目标。切换绘制目标（push/pop 层）前必须调——攒着的实例属于
    /// **切换前**那个目标，跟着切过去就是画到了错误的层里。
    ///
    /// 三批互斥非空（入批前互相 flush），故先后顺序不影响结果。
    fn flush_all(&mut self) {
        self.flush_prims();
        self.flush_text();
        self.flush_images();
    }

    /// 把一段排版结果按字形入批。返回 `false` 表示 atlas 装不下，调用方退回整段光栅。
    ///
    /// **全成功才入批**：中途失败就整段放弃，否则会画出「一半字形在 atlas 里、另一半
    /// 是整段光栅」的重影。故先把槽位全取到，再一次性 extend。
    fn push_glyphs(
        &mut self,
        gpu: &Arc<SharedGpu>,
        sr: &crate::text::ShapedRun,
        prect: Rect,
        align: Align,
        clip: [f32; 4],
        color: Color,
    ) -> bool {
        if sr.glyphs.is_empty() {
            return false;
        }
        // 块的落点与整段路径同一份算法（水平按 align、垂直按 `block_offset_y` 的契约）。
        let (bwf, bhf) = sr.block;
        let block_x = match align {
            Align::Start | Align::Stretch => prect.x as f32,
            Align::Center => prect.x as f32 + (prect.w as f32 - bwf) / 2.0,
            Align::End => prect.x as f32 + prect.w as f32 - bwf,
        };
        let block_y = prect.y as f32 + block_offset_y(prect.h as f32, bhf);
        // 基线吸附方向见 `draw_text` 里那段注释——两条路径必须用同一个，否则同一个
        // 界面里走 atlas 的标签会比走整段光栅的段落高一像素。
        let base = (block_y + sr.ascent).ceil();
        let ox = block_x.round();

        let bind = self.renderer.text(gpu).atlas(gpu).bind_group();
        let mut items = Vec::with_capacity(sr.glyphs.len());
        {
            // 分字段借用：`engine` 出字形像素，`renderer` 持 atlas，两者是不同字段。
            let src = self
                .engine
                .glyph_source()
                .expect("上面已判定本引擎支持 GlyphSource");
            let atlas = self.renderer.text(gpu).atlas(gpu);
            for g in &sr.glyphs {
                let Some(slot) = atlas.slot(gpu, src, &g.key) else {
                    return false;
                };
                if slot.is_blank() {
                    continue;
                }
                let quad = [
                    ox + g.x as f32 + slot.left as f32,
                    base + (g.dy + slot.top) as f32,
                    slot.w as f32,
                    slot.h as f32,
                ];
                items.push(glyph_item(bind.clone(), quad, slot.uv, clip, color));
            }
        }
        self.text_batch.append(&mut items);
        if self.text_batch.len() >= MAX_TEXT_BATCH {
            self.flush_text();
        }
        true
    }

    /// **几何图元入批前**必须调：把已攒的文字与图片先画掉。
    ///
    /// 这就是 painter's algorithm 在「三条管线」下的全部实现。几何攒批、文字攒批、
    /// 图片攒批，谁要入批就先把另外两边画掉——于是屏幕上的叠放次序恒等于 `Canvas`
    /// 调用次序。反过来（各攒到帧末再画）会让所有文字压在所有几何之上：文字被输入框
    /// 背景盖住、或者反过来浮在滚动区之外，都是这一条错了的症状。
    ///
    /// 交错的**次数**与控件数同阶，这是语义要求，省不掉；能省的是每次交错的代价。
    /// 三批现在都录进同一个帧 encoder（[`WgpuCanvas::encoder`]），一帧只提交一次——
    /// 实例数据各占缓冲的一段（`prim.rs` 的帧内游标）、渐变表帧内累积增量写，于是
    /// 「后写的数据覆盖掉前一批」这条坑不再成立。**不要**为了再省几次 pass 而把三边
    /// 攒到帧末合并，那是在拿正确性换性能。
    fn before_prim(&mut self) {
        self.flush_text();
        self.flush_images();
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
        debug_assert!(
            self.layers.is_empty(),
            "帧末合成层未归零：还剩 {} 层（push_layer/pop_layer 未配对）",
            self.layers.len()
        );
        // 三批互斥非空（入批前互相 flush），故先后顺序不影响结果；三个都调是为了
        // 「最后一笔是文字/图元/图片」三种收尾都能画干净。
        debug_assert!(
            [
                self.batch.is_empty(),
                self.text_batch.is_empty(),
                self.image_batch.is_empty()
            ]
            .iter()
            .filter(|e| !**e)
            .count()
                <= 1,
            "几何/文字/图片不应同时有待画内容——交错 flush 漏了一处"
        );
        // 未配对的层在 release 档下不会触发上面的断言，此时把剩下的层依次合成回去：
        // 少一层 opacity 也比整片内容凭空消失强（内容都画在层纹理里，不合成就全丢了）。
        while !self.layers.is_empty() {
            self.pop_layer();
        }
        self.flush_prims();
        self.flush_text();
        self.flush_images();
        // 本帧唯一一次提交。**必须早于 end_frame**：后者把帧内实例游标归零，而游标
        // 归零意味着下一帧从缓冲头部重写，未提交的批次还指着那些字节。
        self.submit();
        // 帧末回收层纹理池里的超额纹理、归零帧内游标（见 `layer.rs` 与 `prim.rs`）。
        let gpu = self.gpu.clone();
        self.renderer.end_frame(&gpu);
    }
}

impl Canvas for WgpuCanvas<'_> {
    fn dpi_scale(&self) -> f32 {
        self.scale
    }

    /// 本帧真正会落笔的世界范围（逻辑坐标）。全窗帧 `None`（不剔除），局部帧即脏区。
    ///
    /// scissor 省的是**片元**，这里省的是 CPU：绘制遍历据此跳过框外节点的自绘，那些
    /// 图元虽然最终也会被 scissor 丢掉，但构造与排版的开销已经付掉了。光标闪烁这类
    /// 只脏几十像素的动画里这是大头——120 个控件的界面每帧照样提交上百次描边与文字。
    ///
    /// 物理→逻辑向外取整：契约要求返回可见范围的**超集**，报小了会真的丢内容。
    fn cull_rect(&self) -> Option<Rect> {
        let [x, y, w, h] = self.scissor?;
        let s = if self.scale > 0.0 { self.scale } else { 1.0 };
        let x0 = (x as f32 / s).floor() as i32;
        let y0 = (y as f32 / s).floor() as i32;
        let x1 = ((x + w) as f32 / s).ceil() as i32;
        let y1 = ((y + h) as f32 / s).ceil() as i32;
        Some(Rect::new(x0, y0, x1 - x0, y1 - y0).inflate(1))
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, paint: &Paint) {
        self.fill_round_rect(x, y, w, h, 0.0, paint);
    }

    fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, paint: &Paint) {
        self.before_prim();
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
        self.before_prim();
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
        self.before_prim();
        // 端帽是 Butt（skia.rs:262 的 `LineCap::Butt`）：线段两端不外延，
        // 几何上就是一个以线段为中轴的旋转矩形——`sd_segment_box` 直接给这个形状。
        let (clip, m) = (self.clip_phys(), self.aa_margin());
        self.batch
            .push_line(x0, y0, x1, y1, width / 2.0, paint, clip, m);
        self.maybe_flush();
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, paint: &Paint) {
        self.before_prim();
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
        self.before_prim();
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

    /// 图片纹理缓存（`tex.rs`）→ 一条带纹理的四边形。
    ///
    /// **fit 缩放、1:1 吸附、落点取整全部逐行照抄 `SkiaCanvas::draw_image`**
    /// （src/render/skia.rs:364-458），连注释里的理由一起搬——同一张图标在两个后端下
    /// 必须落在同一个像素上、糊或不糊也必须一致。差别只有两处，都是路径差异不是语义差异：
    ///
    /// 1. 软后端要给 `draw_pixmap` 建一张圆角 mask 位图，这里把同一个圆角矩形交给
    ///    片元的 SDF（`image.wgsl`），Cover/None 的溢出同样由它裁掉。
    /// 2. 软后端恒用 `FilterQuality::Bilinear`（1:1 时双线性自然退化为纯 blit），
    ///    这里必须**显式**在 1:1 时切 nearest：GPU 的线性采样即便在 1:1 上也会因浮点
    ///    误差沾到邻纹素，细描边照样被摊糊，前面那次精确光栅就白做了。
    fn draw_image(&mut self, img: &Image, dst: Rect, fit: Fit, radius: f32, opacity: f32) {
        let _g = crate::render::prof::scope(crate::render::prof::IMAGE);
        let opacity = opacity.clamp(0.0, 1.0);
        if opacity <= 0.0 {
            return;
        }
        // 逻辑 dst → 物理像素（与图形/裁剪同源的边界取整）。没有软后端局部帧那个原点
        // 偏移：那边把脏区画进一张脏区大小的子 pixmap（故带偏移），这边直接画在绝对坐标
        // 的常驻纹理上，范围由 scissor 收窄。
        let s = self.scale;
        let pdst = dst.scaled(s);
        if pdst.is_empty() {
            return;
        }
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let clip = self.clip_phys();
        // 剔除：dst 与裁剪矩形无交集就别上传纹理了（对标 `draw_text` 开头那次剔除）。
        let clip_rect = Rect::new(
            clip[0] as i32,
            clip[1] as i32,
            (clip[2] - clip[0]) as i32,
            (clip[3] - clip[1]) as i32,
        );
        if pdst.intersect(&clip_rect).is_empty() {
            return;
        }

        let (pw, ph) = (pdst.w as f32, pdst.h as f32);
        let (px, py) = (pdst.x as f32, pdst.y as f32);
        // fit 缩放 / 近 1:1 吸附 / 居中取整（纯几何，逐行照抄软后端，见 `tex.rs`）。
        let (quad, nearest) = place_image(fit, iw, ih, [px, py, pw, ph], s);
        // 圆角 clamp 与 `rounded_rect_path` 同源（软后端建 mask 路径时 clamp）。
        let pr = (radius * s).min(pw / 2.0).min(ph / 2.0).max(0.0);

        // 图片入批前把几何与文字画掉，保持提交顺序即叠放顺序。
        self.flush_prims();
        self.flush_text();

        let gpu = self.gpu.clone();
        let Some(bound) = self.renderer.image(&gpu).get_or_upload(&gpu, img) else {
            return;
        };
        self.image_batch.push(image_item(
            bound,
            quad,
            [px, py, pw, ph],
            clip,
            pr,
            opacity,
            nearest,
        ));
        if self.image_batch.len() >= MAX_IMAGE_BATCH {
            self.flush_images();
        }
    }

    /// 平台光栅 → run-cache → 一条带纹理的四边形（见 `text.rs`）。
    ///
    /// 放置逻辑**在这里**而不是在 `GlyphSource` 里：水平按 `align`、垂直按
    /// [`block_offset_y`] 的契约（装得下居中、装不下顶对齐），与软后端同一份语义。
    /// 平台侧只负责「把这段字排版光栅成一块覆盖度位图」，位图往哪儿贴与平台无关——
    /// 这条分界也是这些判据能在 Windows 上用 mock 测到的原因。
    ///
    /// # 与软后端的两处已知差异
    ///
    /// 1. **贴整数物理像素**。位图与目标 1:1 nearest 采样，四边形落在半像素上会整体
    ///    错半格；软后端是把字直接合成进 pixmap，水平位置可以是小数。于是 `Center`/
    ///    `End` 对齐时两条路径最多差 0.5 物理像素。
    /// 2. **排版宽度不含 rect 的小数位置**（见 `coretext.rs` 的 `raster_run` 注释）。
    fn draw_text(&mut self, text: &str, rect: Rect, color: Color, align: Align, ts: &TextStyle) {
        let _g = crate::render::prof::scope(crate::render::prof::TEXT);
        if text.is_empty() || rect.is_empty() || color.a == 0 {
            return;
        }
        if self.engine.glyph_source().is_none() {
            notice_once(
                &TEXT_NOTICE,
                "windui: 当前文字引擎不支持 GPU 文字光栅（未实现 GlyphSource），本帧的文字不会被绘制",
            );
            return;
        }
        let s = self.scale;
        let prect = rect.scaled(s);
        let clip = self.clip_phys();
        // 剔除：整块文字都落在裁剪矩形外就别光栅了（对标软后端 `draw_text` 开头那次
        // 与 pixmap 边界的相交判断——滚动列表里绝大多数行都走这条路）。留 4px 余量
        // 兜住字形出挑与整数取整。
        let clip_rect = Rect::new(
            clip[0] as i32,
            clip[1] as i32,
            (clip[2] - clip[0]) as i32,
            (clip[3] - clip[1]) as i32,
        );
        if prect.inflate(4).intersect(&clip_rect).is_empty() {
            return;
        }

        // 文字入批前把已攒的几何与图片画掉，保持提交顺序即叠放顺序。
        self.flush_prims();
        self.flush_images();

        let max_width = rect.w as f32;
        let key = RunKey::new(text, ts, align, max_width, s);
        let gpu = self.gpu.clone();
        let req = RunRequest {
            text,
            style: *ts,
            align,
            max_width,
            scale: s,
        };

        // ---- 先试 glyph atlas ----
        // `WINDUI_GPU_NOATLAS=1` 关掉它退回整段光栅（对照测量用，同 `WINDUI_NOSHADOW`
        // 的先例）。收益量化不该依赖"翻出旧版本再编一遍"——那样它就只会被量一次。
        if atlas_enabled() {
            // 单行文字走这条：字形跨文本共享（同一个界面里 `控件 0xx` 那 160 条标签只有
            // 十几个不同字形），动态文本逐字变化也几乎全命中——而整段粒度下它每帧都 miss。
            // 折行段落交不出字形序列（`coretext.rs::shape_run`），落到下面那条整段光栅。
            let shaped = match self.renderer.text(&gpu).shaped(&key) {
                Some(v) => v,
                None => {
                    let src = self
                        .engine
                        .glyph_source()
                        .expect("上面已判定本引擎支持 GlyphSource");
                    let r = src.shape_run(&req);
                    self.renderer.text(&gpu).insert_shaped(key.clone(), r)
                }
            };
            if let Some(sr) = shaped.as_ref() {
                if self.push_glyphs(&gpu, sr, prect, align, clip, color) {
                    return;
                }
            }
        }

        // ---- 退回整段光栅（run-cache）----
        let tex = match self.renderer.text(&gpu).get(&key) {
            Some(t) => t,
            None => {
                let src = self
                    .engine
                    .glyph_source()
                    .expect("上面已判定本引擎支持 GlyphSource");
                let Some(mask) = src.raster_run(&req) else {
                    return;
                };
                match self.renderer.text(&gpu).upload(&gpu, key, &mask) {
                    Some(t) => t,
                    None => return,
                }
            }
        };

        // ---- 放置（全在物理像素空间）----
        // 用**未取整**的块尺寸：`ceil` 过的整数块高会给垂直居中引入恒为负的半像素
        // 偏置，实测表现为整段文字比软后端稳定高 1px（见 `AlphaMask::block`）。
        let (bwf, bhf) = tex.block;
        let block_x = match align {
            Align::Start | Align::Stretch => prect.x as f32,
            Align::Center => prect.x as f32 + (prect.w as f32 - bwf) / 2.0,
            Align::End => prect.x as f32 + prect.w as f32 - bwf,
        };
        let block_y = prect.y as f32 + block_offset_y(prect.h as f32, bhf);
        let pad = tex.pad as f32;
        // 纵向不能直接 `round(块顶 − pad)`：平台光栅器把基线吸附到整数行，于是位图里的
        // 基线与目标里的基线各自取整，差出来的 1px 随容器高的奇偶翻转——症状是 GPU 模式
        // 下整段文字比软后端高一行，而墨量逐字节相同（正是这个「墨量对得上、位置差一行」
        // 的组合把成因指了出来）。故按两边的基线行对齐来算落点。
        //
        // 吸附方向是 **ceil(块顶 + ascent)**：CG 的 `set_text_position` 收的是「距底」，
        // 它把距底 floor 到整数设备行，而距顶 = 高 − 距底，那一侧于是恰好是 ceil。
        // （此前这里写的是 floor；两边同为 floor 时差值在多数情况下与 ceil 一致，只在
        // 某一侧的小数部分恰为 0 时差 1 像素。`coretext.rs` 的重组判据把真实规则量了
        // 出来：逐行墨量逐值相同，只整体错开一行。）
        //
        // 横向不做同样的事：mask 是整段按相位 0 光栅的一份。故取最近整数，居中/右对齐
        // 时最多与软后端差半个像素。走 atlas 那条路则按相位存多份，没有这个损失。
        let asc = tex.ascent;
        let quad = [
            (block_x - pad).round(),
            (block_y + asc).ceil() - (pad + asc).ceil(),
            tex.width as f32,
            tex.height as f32,
        ];
        self.text_batch.push(text_item(tex, quad, clip, color));
        if self.text_batch.len() >= MAX_TEXT_BATCH {
            self.flush_text();
        }
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

    /// 离屏层：后续绘制重定向到一张与目标同尺寸的透明纹理（见 `layer.rs`）。
    fn push_layer(&mut self, opacity: f32) {
        // 切目标前把攒着的三批画到**当前**目标——它们属于层外，跟着切进层里就等于
        // 被层的 opacity 又调制了一遍。
        self.flush_all();
        self.ensure_encoder();
        let gpu = self.gpu.clone();
        let size = self.size;
        let enc = self.encoder.as_mut().expect("ensure_encoder 刚建过");
        let layer = self.renderer.image(&gpu).acquire_layer(&gpu, size, enc);
        match layer {
            Some(mut l) => {
                l.opacity = opacity.clamp(0.0, 1.0);
                self.layers.push(Some(l));
            }
            None => {
                // 分配失败（显存/尺寸上限）：压占位保持栈平衡，子树直接画到外层。
                notice_once(
                    &LAYER_NOTICE,
                    "windui: gpu 后端离屏层纹理分配失败，本次 push_layer 的 opacity 不生效（子树按不透明绘制）",
                );
                self.layers.push(None);
            }
        }
    }

    fn pop_layer(&mut self) {
        // 守卫防下溢（仿软后端 `pop_layer` 的 `if let Some`）。
        debug_assert!(!self.layers.is_empty(), "pop_layer 多于 push_layer");
        if self.layers.is_empty() {
            return;
        }
        // ★ 顺序：先把层内攒着的批次画进**层自己**（此刻栈顶仍是它），再出栈。
        //   反过来的话这些图元会画到父目标上，且完全绕过 opacity 调制。
        self.flush_all();
        let Some(top) = self.layers.pop().expect("上面已判过非空") else {
            return; // 占位层：内容本就直接画在外层，无需合成。
        };
        // 合成回父目标：整目标大小的 quad × opacity，圆角 0、裁剪取全目标
        // （层内容自身已被各自的 clip 裁过，这里再裁一次只会重复剪切）。
        let (w, h) = self.size;
        let full = [0.0, 0.0, w as f32, h as f32];
        self.image_batch.push(image_item(
            top.bound(),
            full,
            full,
            [0.0, 0.0, w as f32, h as f32],
            0.0,
            top.opacity,
            // 层与目标 1:1，nearest 免掉一次无谓的重采样。
            true,
        ));
        // 此刻 `current_view` 已回到父目标，合成 quad 正好画在那里。
        self.flush_images();
        // 合成已提交，纹理可以还回池子（wgpu 会保证已提交的命令用完再释放）。
        let gpu = self.gpu.clone();
        self.renderer.image(&gpu).release_layer(top);
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
static LAYER_NOTICE: std::sync::Once = std::sync::Once::new();

/// 进程内只提示一次降级。每帧刷屏没人会看，一次则刚好够把「不是我布局写错了」
/// 这个判断送到眼前。
/// glyph atlas 是否启用（`WINDUI_GPU_NOATLAS=1` 关）。只读一次环境变量。
fn atlas_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| !matches!(std::env::var("WINDUI_GPU_NOATLAS").as_deref(), Ok("1")))
}

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

    /// 离屏目标，无适配器时打印跳过。跳过必须打印——否则报告里「跳过」和「通过」
    /// 长得一模一样。
    fn offscreen(w: u32, h: u32) -> Option<OffscreenGpu> {
        let off = OffscreenGpu::new(w, h);
        if off.is_none() {
            println!("跳过：本机没有可用的 wgpu 适配器（含软件回退），GPU 图元测试未执行");
        }
        off
    }

    /// 在既有离屏目标上渲一帧（缓存跨帧复用的测试要连渲两帧，故不能每次重建目标）。
    fn draw_on(
        off: &mut OffscreenGpu,
        scale: f32,
        bg: Color,
        eng: &mut dyn TextEngine,
        draw: &dyn Fn(&mut dyn Canvas),
    ) -> Option<Pixmap> {
        off.clear(bg);
        {
            let mut target = off.target();
            let mut canvas = target.make_canvas(eng, scale);
            draw(&mut *canvas);
        }
        off.readback()
    }

    /// GPU 路径：同一串调用打到离屏目标再 readback。无适配器时 `None`（打印跳过）。
    fn render_gpu(
        w: u32,
        h: u32,
        scale: f32,
        bg: Color,
        draw: &dyn Fn(&mut dyn Canvas),
    ) -> Option<Pixmap> {
        let mut off = offscreen(w, h)?;
        let mut eng = crate::text::NullTextEngine;
        draw_on(&mut off, scale, bg, &mut eng, draw)
    }

    /// 同上，但指定文字引擎（mock `GlyphSource`）。
    fn render_gpu_text(
        w: u32,
        h: u32,
        scale: f32,
        bg: Color,
        eng: &mut dyn TextEngine,
        draw: &dyn Fn(&mut dyn Canvas),
    ) -> Option<Pixmap> {
        let mut off = offscreen(w, h)?;
        draw_on(&mut off, scale, bg, eng, draw)
    }

    /// 有墨像素（相对背景有偏离）的外接矩形 `(x0, y0, x1, y1)`，半开区间。全空时 `None`。
    ///
    /// 判据用墨迹范围而不是「某个像素等于某色」：文字是抗锯齿的，边缘像素的具体取值
    /// 随光栅器浮动，而「这段字占了哪几列哪几行」是稳定的（也正是放置逻辑要钉住的）。
    fn ink_bounds(pm: &Pixmap, bg: [u8; 4]) -> Option<(u32, u32, u32, u32)> {
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for y in 0..pm.height() {
            for x in 0..pm.width() {
                if px(pm, x, y) != bg {
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x + 1);
                    y1 = y1.max(y + 1);
                }
            }
        }
        (x0 != u32::MAX).then_some((x0, y0, x1, y1))
    }

    /// 「墨量」：全图相对背景的通道偏离总量。比单纯的通道和敏感得多——白底上画白色
    /// 图元、或者只画对一半，通道和几乎不动，墨量却会立刻掉下来
    /// （memory「视觉效果要量化验证」的同一条教训）。
    fn ink(pm: &Pixmap, bg: [u8; 4]) -> f64 {
        pm.data()
            .as_chunks::<4>()
            .0
            .iter()
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
            .as_chunks::<4>()
            .0
            .iter()
            .any(|p| p != &[255u8, 255, 255, 255]);
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

    /// 未实现的图元、以及不支持 `GlyphSource` 的引擎（此处是 `NullTextEngine`）
    /// 不得把已画的东西弄坏，也不得 panic。
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

    // ---- 文字（P2）：放置 / 调色 / 缓存 / 交错顺序 ----
    //
    // 全部用 mock `GlyphSource`（`text.rs` 的 `mock` 模块）。放置、裁剪、缓存、批次
    // 顺序都是平台无关逻辑，本来就该在任何一台机器上测得到；真引擎的那一半（光栅得
    // 像不像）由 macOS 上的墨量比对负责，两层各管一段。

    use super::super::text::mock::{MockGlyphEngine, Pattern};

    const BLACK: Color = Color::rgb(0, 0, 0);
    /// 白底的字节（mock 画的是不透明黑块，判据只看「和白底不一样」）。
    const WHITE_PX: [u8; 4] = [255, 255, 255, 255];

    /// 水平对齐：文本块按 `align` 在 rect 内定位。三种对齐的墨迹左边界应分别落在
    /// rect 左端、正中、右端——这是 `draw_text` 里那段放置逻辑唯一的可观察产物。
    #[test]
    fn text_horizontal_alignment_places_block_in_rect() {
        for (align, want_x0) in [(Align::Start, 0u32), (Align::Center, 45), (Align::End, 90)] {
            // 文本块 10×4（逻辑=物理，scale=1），rect 宽 100。
            let mut eng = MockGlyphEngine::new((10, 4));
            let Some(pm) = render_gpu_text(100, 20, 1.0, WHITE, &mut eng, &|c| {
                c.draw_text(
                    "x",
                    Rect::new(0, 0, 100, 20),
                    BLACK,
                    align,
                    &TextStyle::new(12.0),
                );
            }) else {
                return;
            };
            let b = ink_bounds(&pm, WHITE_PX).expect("应画出文字");
            assert_eq!(
                (b.0, b.2),
                (want_x0, want_x0 + 10),
                "{align:?} 的墨迹横向范围不对，实得 {b:?}"
            );
        }
    }

    /// 垂直定位：装得下时居中（`block_offset_y` 的第一条分支）。
    #[test]
    fn text_is_vertically_centered_when_it_fits() {
        let mut eng = MockGlyphEngine::new((10, 4));
        let Some(pm) = render_gpu_text(60, 60, 1.0, WHITE, &mut eng, &|c| {
            c.draw_text(
                "x",
                Rect::new(0, 10, 60, 20),
                BLACK,
                Align::Start,
                &TextStyle::new(12.0),
            );
        }) else {
            return;
        };
        let b = ink_bounds(&pm, WHITE_PX).expect("应画出文字");
        // rect y=10 高 20，块高 4 → 顶偏移 (20-4)/2 = 8 → y0 = 18。
        assert_eq!((b.1, b.3), (18, 22), "垂直居中位置不对，实得 {b:?}");
    }

    /// 垂直定位：装不下时**顶对齐**而非居中溢出——`block_offset_y` 里那个
    /// `.max(0)` 的 GPU 版（同 `text::text_block_contract::overflowing_text_is_top_aligned`）。
    #[test]
    fn overflowing_text_is_top_aligned() {
        let mut eng = MockGlyphEngine::new((10, 40));
        let Some(pm) = render_gpu_text(60, 120, 1.0, WHITE, &mut eng, &|c| {
            c.draw_text(
                "x",
                Rect::new(0, 20, 60, 16),
                BLACK,
                Align::Start,
                &TextStyle::new(12.0),
            );
        }) else {
            return;
        };
        let b = ink_bounds(&pm, WHITE_PX).expect("应画出文字");
        assert_eq!(b.1, 20, "装不下时应顶对齐到 rect 顶边，实得 y0={}", b.1);
        assert_eq!(b.3, 60, "块高 40 应完整画出（越出 rect 由裁剪收口）");
    }

    /// `pad`（字形出挑余量）不参与定位：位图带 pad 时墨迹落点必须与不带 pad 时一致。
    /// 漏减 pad 的症状是整段文字整体偏移几个像素，且随字号变化——极难认出成因。
    #[test]
    fn overhang_pad_does_not_shift_the_block() {
        let mut plain = MockGlyphEngine::new((10, 4));
        let mut padded = MockGlyphEngine::new((10, 4)).with_pad(3);
        let draw = |c: &mut dyn Canvas| {
            c.draw_text(
                "x",
                Rect::new(0, 0, 100, 20),
                BLACK,
                Align::Center,
                &TextStyle::new(12.0),
            );
        };
        let Some(a) = render_gpu_text(100, 20, 1.0, WHITE, &mut plain, &draw) else {
            return;
        };
        let Some(b) = render_gpu_text(100, 20, 1.0, WHITE, &mut padded, &draw) else {
            return;
        };
        assert_eq!(
            ink_bounds(&a, WHITE_PX),
            ink_bounds(&b, WHITE_PX),
            "带 pad 的位图应贴到同一处（定位按文本块算，不是按位图算）"
        );
    }

    /// 裁剪矩形对文字与几何图元同样生效（同一份 clip 字段、同一条片元判据）。
    #[test]
    fn text_respects_clip_rect() {
        // 块 40×20 在 60 高的 rect 内居中 → 占 y∈[20,40)；裁剪带取它中间的一条。
        let mut eng = MockGlyphEngine::new((40, 20));
        let Some(pm) = render_gpu_text(60, 60, 1.0, WHITE, &mut eng, &|c| {
            c.save();
            c.clip_rect(Rect::new(10, 25, 20, 8));
            c.draw_text(
                "x",
                Rect::new(0, 0, 60, 60),
                BLACK,
                Align::Start,
                &TextStyle::new(12.0),
            );
            c.restore();
        }) else {
            return;
        };
        let b = ink_bounds(&pm, WHITE_PX).expect("裁剪带内应有字");
        assert_eq!(
            b,
            (10, 25, 30, 33),
            "墨迹应恰好被裁成 (10,25,20,8)，实得 {b:?}"
        );
    }

    /// 调色：R8 覆盖度 × 文字颜色，输出预乘。50% 红字盖白底应得粉——判据与 P1 的
    /// `translucent_overlay_matches_soft_backend` 同一条（预乘约定错了会整体发白/发黑）。
    #[test]
    fn text_is_tinted_by_color_with_premultiplied_output() {
        let mut eng = MockGlyphEngine::new((20, 20));
        let Some(pm) = render_gpu_text(20, 20, 1.0, WHITE, &mut eng, &|c| {
            c.draw_text(
                "x",
                Rect::new(0, 0, 20, 20),
                Color::rgba(255, 0, 0, 128),
                Align::Start,
                &TextStyle::new(12.0),
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

    /// 覆盖度是逐纹素采样的（nearest + 1:1）：棋盘图案画出来仍是棋盘，不会被过滤糊平。
    #[test]
    fn coverage_is_sampled_one_to_one() {
        // 棋盘是**整段光栅**那条路的判据（atlas 出的是逐字形位图），故显式关掉字形序列。
        let mut eng = MockGlyphEngine::new((16, 16))
            .with_pattern(Pattern::Checker)
            .without_shaping();
        let Some(pm) = render_gpu_text(16, 16, 1.0, WHITE, &mut eng, &|c| {
            c.draw_text(
                "x",
                Rect::new(0, 0, 16, 16),
                BLACK,
                Align::Start,
                &TextStyle::new(12.0),
            );
        }) else {
            return;
        };
        // 2×2 棋盘：(0,0) 格是墨、(2,0) 格是空。
        assert_eq!(px(&pm, 0, 0), [0, 0, 0, 255], "棋盘的墨格应是纯黑");
        assert_eq!(px(&pm, 2, 0), WHITE_PX, "棋盘的空格应保持白底");
        assert_eq!(px(&pm, 1, 1), [0, 0, 0, 255], "同一格内应一致（无过渡）");
    }

    /// 缓存：同键第二次绘制不再调用光栅器；换 scale 则是新键，必须重新光栅。
    #[test]
    fn same_run_is_rastered_once_and_scale_makes_a_new_key() {
        let Some(mut off) = offscreen(60, 40) else {
            return;
        };
        // 数的是 `raster_run` 的调用次数——只有整段光栅那条路会调它。
        // atlas 那条的同款判据是 `glyph_calls`，见 `a_glyph_is_rastered_once_per_atlas`。
        let mut eng = MockGlyphEngine::new((10, 4)).without_shaping();
        let draw = |c: &mut dyn Canvas| {
            c.draw_text(
                "hello",
                Rect::new(0, 0, 60, 20),
                BLACK,
                Align::Start,
                &TextStyle::new(12.0),
            );
        };
        draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("首帧");
        assert_eq!(eng.calls, 1, "首次绘制应光栅一次");
        draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("次帧");
        assert_eq!(eng.calls, 1, "同键第二次应命中缓存，不再光栅");
        draw_on(&mut off, 2.0, WHITE, &mut eng, &draw).expect("换 scale");
        assert_eq!(eng.calls, 2, "scale 变化天然换键，应重新光栅");
        assert_eq!(eng.last.map(|l| l.0), Some(2.0), "光栅请求应带上新的 scale");
        // 换回旧 scale 仍命中（换键不等于整体失效——旧键还在，只是排在 LRU 里）。
        draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("换回");
        assert_eq!(eng.calls, 2, "换回旧 scale 应仍命中");
    }

    /// 同一段文字换颜色**不**产生新键：颜色在片元里调制，不进纹理也不进键。
    #[test]
    fn color_is_not_part_of_the_cache_key() {
        let Some(mut off) = offscreen(40, 20) else {
            return;
        };
        // 同上：数的是 `raster_run` 的调用次数。颜色不进键这一条两条路径都成立。
        let mut eng = MockGlyphEngine::new((10, 4)).without_shaping();
        for color in [BLACK, Color::rgb(255, 0, 0), Color::rgb(0, 0, 255)] {
            draw_on(&mut off, 1.0, WHITE, &mut eng, &|c| {
                c.draw_text(
                    "hi",
                    Rect::new(0, 0, 40, 20),
                    color,
                    Align::Start,
                    &TextStyle::new(12.0),
                );
            })
            .expect("渲染");
        }
        assert_eq!(eng.calls, 1, "三种颜色应共用同一张覆盖度纹理");
    }

    /// 交错顺序（painter's algorithm）：几何 → 文字 → 几何，后画的压在先画的之上。
    ///
    /// 这是「两条管线」下最容易错的一条：两边各攒到帧末再画，所有文字就会一起浮到
    /// 所有几何之上——输入框的字盖住选中高亮、滚动区的字飘在容器外，都是这个症状。
    #[test]
    fn text_and_geometry_interleave_in_submission_order() {
        let mut eng = MockGlyphEngine::new((40, 40));
        let Some(pm) = render_gpu_text(40, 40, 1.0, WHITE, &mut eng, &|c| {
            // ① 蓝底铺满
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(Color::rgb(0, 0, 255)));
            // ② 红字铺满（应盖住蓝底）
            c.draw_text(
                "x",
                Rect::new(0, 0, 40, 40),
                Color::rgb(255, 0, 0),
                Align::Start,
                &TextStyle::new(12.0),
            );
            // ③ 绿块只盖左上角（应盖住红字）
            c.fill_rect(0.0, 0.0, 20.0, 20.0, &Paint::fill(Color::rgb(0, 255, 0)));
        }) else {
            return;
        };
        assert_eq!(
            px(&pm, 30, 30),
            [255, 0, 0, 255],
            "文字应压在先画的几何之上"
        );
        assert_eq!(
            px(&pm, 10, 10),
            [0, 255, 0, 255],
            "后画的几何应压在文字之上"
        );
    }

    /// 反向：文字 → 几何 → 文字。第二段文字必须压在中间那块几何之上。
    #[test]
    fn geometry_between_two_runs_does_not_reorder_them() {
        // 两段文字的块都是 40×40（mock 尺寸与 rect 无关），故用 rect 的位置把它们错开：
        // "a" 落在左上 40×40，"b" 落在右下 40×40，中间那块几何铺满全图。
        let mut eng = MockGlyphEngine::new((40, 40));
        let Some(pm) = render_gpu_text(80, 80, 1.0, WHITE, &mut eng, &|c| {
            c.draw_text(
                "a",
                Rect::new(0, 0, 40, 40),
                Color::rgb(255, 0, 0),
                Align::Start,
                &TextStyle::new(12.0),
            );
            c.fill_rect(0.0, 0.0, 80.0, 80.0, &Paint::fill(Color::rgb(0, 0, 255)));
            c.draw_text(
                "b",
                Rect::new(40, 40, 40, 40),
                Color::rgb(0, 255, 0),
                Align::Start,
                &TextStyle::new(12.0),
            );
        }) else {
            return;
        };
        assert_eq!(px(&pm, 20, 20), [0, 0, 255, 255], "几何应盖住第一段文字");
        assert_eq!(
            px(&pm, 60, 60),
            [0, 255, 0, 255],
            "第二段文字应压在几何之上"
        );
    }

    /// scale=1.5：逻辑坐标不变，文本块与落点都按物理放大。
    #[test]
    fn text_placement_scales_with_dpi() {
        let mut eng = MockGlyphEngine::new((10, 4));
        let Some(pm) = render_gpu_text(150, 60, 1.5, WHITE, &mut eng, &|c| {
            c.draw_text(
                "x",
                Rect::new(0, 0, 100, 20),
                BLACK,
                Align::Center,
                &TextStyle::new(12.0),
            );
        }) else {
            return;
        };
        let b = ink_bounds(&pm, WHITE_PX).expect("应画出文字");
        // 物理 rect 150 宽、块 15 宽 → x0 = (150-15)/2 = 67.5 → 取整 67 或 68。
        assert!(
            (67..=68).contains(&b.0) && b.2 - b.0 == 15,
            "1.5x 下应居中且块宽 15px，实得 {b:?}"
        );
        // 物理 rect 高 30、块高 6 → y0 = (30-6)/2 = 12。
        assert_eq!((b.1, b.3), (12, 18), "1.5x 下的垂直居中不对，实得 {b:?}");
    }

    /// 完全落在裁剪之外的文字不该走光栅（滚动列表里这是绝大多数行的路径）。
    #[test]
    fn text_outside_the_clip_is_not_rastered() {
        let mut eng = MockGlyphEngine::new((10, 4));
        let Some(_) = render_gpu_text(60, 60, 1.0, WHITE, &mut eng, &|c| {
            c.save();
            c.clip_rect(Rect::new(0, 0, 10, 10));
            c.draw_text(
                "x",
                Rect::new(0, 40, 60, 12),
                BLACK,
                Align::Start,
                &TextStyle::new(12.0),
            );
            c.restore();
        }) else {
            return;
        };
        assert_eq!(eng.calls, 0, "裁剪外的文字应在光栅前就被剔除");
    }

    // ---- 图片（P3）----
    //
    // 测试图全部**程序生成**（`Image::from_rgba`），不依赖任何文件：解码是
    // `render/image.rs` 的事，本文件要测的是「解码好的像素怎么落到目标上」。

    use super::super::layer::alloc_count;
    use super::super::tex::upload_count;

    /// 纯色测试图（非预乘 RGBA）。
    fn solid_image(w: u32, h: u32, c: Color) -> Image {
        Image::from_rgba(w, h, &[c.r, c.g, c.b, c.a].repeat((w * h) as usize))
            .expect("测试图尺寸合法")
    }

    /// 逐像素红/蓝棋盘（非预乘 RGBA）。用来看重采样有没有把纹素网格糊掉。
    fn checker_image(w: u32, h: u32) -> Image {
        let mut v = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let c: [u8; 4] = if (x + y) % 2 == 0 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 255, 255]
                };
                v.extend_from_slice(&c);
            }
        }
        Image::from_rgba(w, h, &v).expect("测试图尺寸合法")
    }

    const IMG_RED: Color = Color::rgb(255, 0, 0);

    /// Fill：铺满 dst，框内是图片色、框外原样。对标软后端
    /// `draw_image_fills_dst_and_respects_bounds`。
    #[test]
    fn draw_image_fill_matches_soft_backend() {
        let img = solid_image(4, 4, IMG_RED);
        assert_matches_soft("image_fill", 100, 100, 1.0, WHITE, &[(22, 22, 36, 36)], {
            let img = img.clone();
            move |c: &mut dyn Canvas| {
                c.draw_image(&img, Rect::new(20, 20, 40, 40), Fit::Fill, 0.0, 1.0);
            }
        });
        let Some(pm) = render_gpu(100, 100, 1.0, WHITE, &|c| {
            c.draw_image(&img, Rect::new(20, 20, 40, 40), Fit::Fill, 0.0, 1.0);
        }) else {
            return;
        };
        assert_eq!(px(&pm, 40, 40), [255, 0, 0, 255], "dst 内应被图片填满");
        assert_eq!(px(&pm, 5, 5), WHITE_PX, "dst 外一个像素都不该动");
        // 边界严丝合缝：dst 是 (20,20,40,40)，墨迹范围必须恰好是它。
        assert_eq!(ink_bounds(&pm, WHITE_PX), Some((20, 20, 60, 60)));
    }

    /// 物理尺寸与源图一致时必须逐像素精确——对标软后端
    /// `draw_image_unit_scale_is_pixel_exact`（那条判据的存在理由是 DPI 感知图标）。
    #[test]
    fn draw_image_unit_scale_is_pixel_exact() {
        let img = solid_image(4, 4, IMG_RED);
        let Some(pm) = render_gpu(20, 20, 1.0, WHITE, &|c| {
            c.draw_image(&img, Rect::new(5, 5, 4, 4), Fit::Contain, 0.0, 1.0);
        }) else {
            return;
        };
        for y in 5..9 {
            for x in 5..9 {
                assert_eq!(
                    px(&pm, x, y),
                    [255, 0, 0, 255],
                    "({x},{y}) 应为纯红（1:1 无插值）"
                );
            }
        }
        assert_eq!(px(&pm, 9, 9), WHITE_PX, "框外不得被插值溢出污染");
    }

    /// 1:1 时纹素网格必须原样保留：逐像素棋盘画出来仍是棋盘。
    ///
    /// 严格说，四边形已经贴在整数物理像素上、纹素与像素 1:1 时，linear 采样落在纹素
    /// 中心，理论上也能得到同样的结果——所以这条真正钉住的是**对齐**（落点取整 +
    /// 尺寸吸附）。「1:1 走 nearest」那一半由 `tex.rs::exact_unit_scale_uses_nearest`
    /// 在 CPU 侧钉死，两条各管一段。
    #[test]
    fn draw_image_at_unit_scale_keeps_the_texel_grid() {
        let img = checker_image(9, 9);
        let Some(pm) = render_gpu(20, 20, 1.0, WHITE, &|c| {
            c.draw_image(&img, Rect::new(2, 3, 9, 9), Fit::Contain, 0.0, 1.0);
        }) else {
            return;
        };
        for y in 0..9u32 {
            for x in 0..9u32 {
                let want = if (x + y) % 2 == 0 {
                    [255, 0, 0, 255]
                } else {
                    [0, 0, 255, 255]
                };
                assert_eq!(
                    px(&pm, 2 + x, 3 + y),
                    want,
                    "棋盘格 ({x},{y}) 被重采样糊掉了"
                );
            }
        }
    }

    /// 近 1:1 的落点：源 8×8 进 9×9 的框，两个后端必须画在同一处（对标软后端
    /// `draw_image_snaps_near_unit_scale`）。
    #[test]
    fn draw_image_near_unit_scale_matches_soft_backend() {
        let img = solid_image(8, 8, IMG_RED);
        assert_matches_soft("image_near_unit", 30, 30, 1.0, WHITE, &[(6, 6, 7, 7)], {
            let img = img.clone();
            move |c: &mut dyn Canvas| {
                c.draw_image(&img, Rect::new(5, 5, 9, 9), Fit::Contain, 0.0, 1.0);
            }
        });
        let Some(pm) = render_gpu(30, 30, 1.0, WHITE, &|c| {
            c.draw_image(&img, Rect::new(5, 5, 9, 9), Fit::Contain, 0.0, 1.0);
        }) else {
            return;
        };
        assert_eq!(px(&pm, 9, 9), [255, 0, 0, 255], "近 1:1 处应是纯图片色");
    }

    /// 大圆角把四角裁掉（角落保持背景色），对标软后端 `draw_image_rounded_clips_corners`。
    #[test]
    fn draw_image_rounded_clips_corners() {
        let img = solid_image(4, 4, IMG_RED);
        assert_matches_soft("image_rounded", 60, 60, 1.0, WHITE, &[(25, 25, 10, 10)], {
            let img = img.clone();
            move |c: &mut dyn Canvas| {
                // dst 40×40、圆角 20（=半边长，近圆）。
                c.draw_image(&img, Rect::new(10, 10, 40, 40), Fit::Fill, 20.0, 1.0);
            }
        });
        let Some(pm) = render_gpu(60, 60, 1.0, WHITE, &|c| {
            c.draw_image(&img, Rect::new(10, 10, 40, 40), Fit::Fill, 20.0, 1.0);
        }) else {
            return;
        };
        assert_eq!(px(&pm, 11, 11), WHITE_PX, "圆角应把角落裁成背景");
        assert_eq!(px(&pm, 30, 30), [255, 0, 0, 255], "中心仍是图片色");
    }

    /// 低不透明度：红图叠白底混出更浅的色（状态调制），对标软后端
    /// `draw_image_opacity_blends_lighter`。
    #[test]
    fn draw_image_opacity_blends_lighter() {
        let img = solid_image(4, 4, IMG_RED);
        assert_matches_soft("image_opacity", 40, 40, 1.0, WHITE, &[(7, 7, 26, 26)], {
            let img = img.clone();
            move |c: &mut dyn Canvas| {
                c.draw_image(&img, Rect::new(5, 5, 30, 30), Fit::Fill, 0.0, 0.4);
            }
        });
        let Some(pm) = render_gpu(40, 40, 1.0, WHITE, &|c| {
            c.draw_image(&img, Rect::new(5, 5, 30, 30), Fit::Fill, 0.0, 0.3);
        }) else {
            return;
        };
        let p = px(&pm, 20, 20);
        assert!(p[0] > 240, "红通道应仍高，实得 {p:?}");
        assert!(p[1] > 150 && p[2] > 150, "低不透明应混入白底（实得 {p:?}）");
    }

    /// Cover：等比铺满并把溢出裁到 dst——墨迹范围必须**恰好**是 dst。
    #[test]
    fn draw_image_cover_is_clipped_to_dst() {
        // 源 4×2（宽高比 2:1）进 20×20 的框：k=max(5,10)=10 → 40×20，横向溢出 ±10。
        let img = solid_image(4, 2, IMG_RED);
        let Some(pm) = render_gpu(40, 40, 1.0, WHITE, &|c| {
            c.draw_image(&img, Rect::new(10, 10, 20, 20), Fit::Cover, 0.0, 1.0);
        }) else {
            return;
        };
        assert_eq!(
            ink_bounds(&pm, WHITE_PX),
            Some((10, 10, 30, 30)),
            "Cover 的溢出必须被 dst 裁掉，一个像素都不能漏到框外"
        );
    }

    /// Contain：等比完整显示、短边留白（letterbox）。
    #[test]
    fn draw_image_contain_leaves_letterbox() {
        // 源 4×2 进 20×20：k=min(5,10)=5 → 20×10，竖直居中 → 上下各留 5。
        let img = solid_image(4, 2, IMG_RED);
        let Some(pm) = render_gpu(40, 40, 1.0, WHITE, &|c| {
            c.draw_image(&img, Rect::new(10, 10, 20, 20), Fit::Contain, 0.0, 1.0);
        }) else {
            return;
        };
        assert_eq!(
            ink_bounds(&pm, WHITE_PX),
            Some((10, 15, 30, 25)),
            "Contain 应等比缩放并在框内居中留白"
        );
    }

    /// 缓存：同一张图第二次绘制不再上传；换一张图才会再传一次。
    #[test]
    fn same_image_is_uploaded_once() {
        let Some(mut off) = offscreen(40, 40) else {
            return;
        };
        let mut eng = crate::text::NullTextEngine;
        let img = solid_image(4, 4, IMG_RED);
        let before = upload_count();
        let draw = |c: &mut dyn Canvas| {
            c.draw_image(&img, Rect::new(0, 0, 40, 40), Fit::Fill, 0.0, 1.0);
        };
        draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("首帧");
        assert_eq!(upload_count() - before, 1, "首次绘制应上传一次");
        draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("次帧");
        assert_eq!(upload_count() - before, 1, "同一张图第二次应命中缓存");
        // 同一帧内画两次也只有一次上传。
        draw_on(&mut off, 1.0, WHITE, &mut eng, &|c| {
            draw(c);
            draw(c);
        })
        .expect("同帧两次");
        assert_eq!(upload_count() - before, 1, "同帧重复绘制同样命中缓存");
        // 换一张图（新的 `Rc`）→ 新键 → 再传一次。
        let other = solid_image(4, 4, Color::rgb(0, 0, 255));
        draw_on(&mut off, 1.0, WHITE, &mut eng, &|c| {
            c.draw_image(&other, Rect::new(0, 0, 40, 40), Fit::Fill, 0.0, 1.0);
        })
        .expect("换图");
        assert_eq!(upload_count() - before, 2, "换一张图应重新上传");
    }

    /// 交错顺序（painter's algorithm）：几何 → 图片 → 几何，后画的压在先画的之上。
    #[test]
    fn image_and_geometry_interleave_in_submission_order() {
        let img = solid_image(2, 2, IMG_RED);
        let Some(pm) = render_gpu(40, 40, 1.0, WHITE, &|c| {
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(Color::rgb(0, 0, 255)));
            c.draw_image(&img, Rect::new(0, 0, 40, 40), Fit::Fill, 0.0, 1.0);
            c.fill_rect(0.0, 0.0, 20.0, 20.0, &Paint::fill(Color::rgb(0, 255, 0)));
        }) else {
            return;
        };
        assert_eq!(px(&pm, 30, 30), [255, 0, 0, 255], "图片应压在先画的几何上");
        assert_eq!(px(&pm, 10, 10), [0, 255, 0, 255], "后画的几何应压在图片上");
    }

    // ---- 离屏层（P3）----

    /// 50% 红块合成到白底 → 粉色。对标软后端 `push_pop_layer_composites_with_opacity`。
    #[test]
    fn push_pop_layer_composites_with_opacity() {
        assert_matches_soft("layer_50", 40, 40, 1.0, WHITE, &[(2, 2, 36, 36)], |c| {
            c.push_layer(0.5);
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(Color::hex(0xFF0000)));
            c.pop_layer();
        });
        // 两后端一致还不够，绝对值也要对：是粉，不是红也不是白。
        let Some(pm) = render_gpu(40, 40, 1.0, WHITE, &|c| {
            c.push_layer(0.5);
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(Color::hex(0xFF0000)));
            c.pop_layer();
        }) else {
            return;
        };
        let p = px(&pm, 20, 20);
        assert!(p[0] > 240, "红通道应高，实得 {p:?}");
        assert!(
            (100..200).contains(&(p[1] as i32)) && (100..200).contains(&(p[2] as i32)),
            "绿/蓝应被白底抬到中段（50% 合成），实得 {p:?}"
        );
        assert_eq!(p[3], 255, "不透明背景上合成后 alpha 应仍为满");
    }

    /// 层的意义在于「**整体**一次 opacity」：层内两块不透明色重叠，重叠处与非重叠处
    /// 必须一模一样。
    ///
    /// 这是层与「把 opacity 摊到每个图元上」的唯一区别，也是唯一会被偷偷做错的地方——
    /// 后者在重叠处会叠成 1-(1-a)² 的更深色，而 UI 里的浮层/卡片子树几乎处处重叠
    /// （背景 + 描边 + 内容），一旦做错整块浮层的观感就厚一层。
    #[test]
    fn layer_composites_its_subtree_as_a_group() {
        let Some(pm) = render_gpu(60, 40, 1.0, WHITE, &|c| {
            c.push_layer(0.5);
            // 两块同色不透明矩形，左块 0..40、右块 20..60 → 中间 20..40 重叠。
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(Color::hex(0xFF0000)));
            c.fill_rect(20.0, 0.0, 40.0, 40.0, &Paint::fill(Color::hex(0xFF0000)));
            c.pop_layer();
        }) else {
            return;
        };
        let single = px(&pm, 8, 20);
        let overlap = px(&pm, 30, 20);
        assert_eq!(
            single, overlap,
            "重叠处与非重叠处必须同色（层是整体调制，不是逐图元调制）"
        );
    }

    /// 嵌套层：两层 0.5 叠起来等效 0.25（层是栈，内层先合成进外层）。
    #[test]
    fn nested_layers_multiply_opacity() {
        let draw = |c: &mut dyn Canvas| {
            c.push_layer(0.5);
            c.push_layer(0.5);
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(Color::hex(0xFF0000)));
            c.pop_layer();
            c.pop_layer();
        };
        assert_matches_soft("layer_nested", 40, 40, 1.0, WHITE, &[(2, 2, 36, 36)], draw);
        let Some(pm) = render_gpu(40, 40, 1.0, WHITE, &draw) else {
            return;
        };
        // 25% 红 over 白：r=255，g/b≈191。
        let p = px(&pm, 20, 20);
        assert!(p[0] > 240, "红通道应仍满，实得 {p:?}");
        let want = 255 - 64; // (1-0.25)*255
        for ch in [p[1], p[2]] {
            assert!(
                (ch as i32 - want).abs() <= 3,
                "嵌套 0.5×0.5 应等效 0.25（期望 g/b≈{want}），实得 {p:?}"
            );
        }
    }

    /// 层内的几何与文字仍按调用顺序叠放，合成回父目标时作为整体调制。
    #[test]
    fn layer_preserves_interleaving_of_text_and_geometry() {
        let mut eng = MockGlyphEngine::new((40, 40));
        let Some(pm) = render_gpu_text(40, 40, 1.0, WHITE, &mut eng, &|c| {
            c.push_layer(0.5);
            // 层内：蓝底 → 红字铺满（压住蓝底）→ 绿块盖左上角（压住红字）。
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(Color::rgb(0, 0, 255)));
            c.draw_text(
                "x",
                Rect::new(0, 0, 40, 40),
                Color::rgb(255, 0, 0),
                Align::Start,
                &TextStyle::new(12.0),
            );
            c.fill_rect(0.0, 0.0, 20.0, 20.0, &Paint::fill(Color::rgb(0, 255, 0)));
            c.pop_layer();
        }) else {
            return;
        };
        // 50% 红 over 白 → (255,128,128)；50% 绿 over 白 → (128,255,128)。
        let text = px(&pm, 30, 30);
        assert!(
            text[0] > 240 && (100..200).contains(&(text[1] as i32)),
            "层内文字应压在层内几何之上并被整体调制，实得 {text:?}"
        );
        let geo = px(&pm, 10, 10);
        assert!(
            geo[1] > 240 && (100..200).contains(&(geo[0] as i32)),
            "层内后画的几何应压在文字之上，实得 {geo:?}"
        );
    }

    /// 层内画图片：图片同样只被层调制一次，且不会被层的清屏抹掉。
    #[test]
    fn layer_composites_images_too() {
        let img = solid_image(2, 2, IMG_RED);
        let Some(pm) = render_gpu(40, 40, 1.0, WHITE, &|c| {
            c.push_layer(0.5);
            c.draw_image(&img, Rect::new(0, 0, 40, 40), Fit::Fill, 0.0, 1.0);
            c.pop_layer();
        }) else {
            return;
        };
        let p = px(&pm, 20, 20);
        assert!(
            p[0] > 240 && (100..200).contains(&(p[1] as i32)),
            "层内图片应按 50% 合成到白底（→ 粉），实得 {p:?}"
        );
    }

    /// 层纹理池：连续两帧各 push/pop 一次，第二帧必须复用第一帧那张纹理。
    ///
    /// 窗口尺寸的层纹理动辄几 MiB，每帧现建现丢在淡入淡出动画里就是每帧几 MiB 的
    /// 分配——这条判据是池存在与否的唯一可观察证据。
    #[test]
    fn layer_texture_is_reused_across_frames() {
        let Some(mut off) = offscreen(40, 40) else {
            return;
        };
        let mut eng = crate::text::NullTextEngine;
        let draw = |c: &mut dyn Canvas| {
            c.push_layer(0.5);
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(RED));
            c.pop_layer();
        };
        let before = alloc_count();
        draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("首帧");
        assert_eq!(alloc_count() - before, 1, "首帧应新建一张层纹理");
        draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("次帧");
        assert_eq!(alloc_count() - before, 1, "次帧应从池里复用，不再新建");
        // 复用的纹理必须被清干净。判据取「50% RED over 白」的解析值：层若残留上一帧
        // 的同色内容，合成两遍会明显更深（每个通道各往 RED 那边多走一半）。
        let pm = draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("第三帧");
        let p = px(&pm, 20, 20);
        let want = |c: u8| (c as f32 * 0.5 + 255.0 * 0.5).round() as i32;
        for (i, ch) in [RED.r, RED.g, RED.b].iter().enumerate() {
            assert!(
                (p[i] as i32 - want(*ch)).abs() <= 2,
                "复用的层纹理未清干净（残留上一帧）：通道 {i} 期望 {}，实得 {p:?}",
                want(*ch)
            );
        }
    }

    // ---- 帧末一次提交（一帧一个 encoder，P5 的地基）----

    /// 同一帧内前后两次 `push_layer` 复用同一张池纹理时，后一层的清屏不得抹掉前一层。
    ///
    /// 这是帧 encoder 引入的新风险：层清屏若自己 `submit`，会插到整帧序列**最前面**
    /// 执行，而池是复用纹理的——`push A → pop A（合成时采样 A）→ push B（池里取回同
    /// 一张）`这条路径下，B 的清屏会赶在「合成 A」之前跑掉，把 A 的像素抹成透明。
    /// 症状是「前一层的内容凭空消失」或「后一层带着前一层的残留一起合成」，而成因
    /// 都在那两层之外。故清屏必须录进同一个 encoder（`layer.rs::LayerTexture::clear`）。
    ///
    /// **形状是判据的一部分**：两层若都不透明且互不重叠，残留恰好被同色覆盖，画面
    /// 完全正确——第一版判据就是这么写的，把清屏改回自己提交仍然全绿。这里让第一层
    /// 铺满、第二层只占右半且都取 50% 不透明：漏清时左半会多叠一次 50% 红，明显更深。
    #[test]
    fn a_reused_layer_in_the_same_frame_keeps_the_earlier_composite() {
        const BLUE: Color = Color::rgb(40, 70, 210);
        assert_matches_soft(
            "layer_reuse_same_frame",
            40,
            40,
            1.0,
            WHITE,
            &[(2, 2, 16, 36), (22, 2, 16, 36)],
            |c| {
                c.push_layer(0.5);
                c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(RED));
                c.pop_layer();
                c.push_layer(0.5);
                c.fill_rect(20.0, 0.0, 20.0, 40.0, &Paint::fill(BLUE));
                c.pop_layer();
            },
        );
    }

    /// 一帧内的渐变组数超过表容量（`GRAD_MAX` = 64）时，第 65 组之后**仍然是渐变**。
    ///
    /// 渐变表配合帧末一次提交改成了帧内累积，表满的处理随之从「退化成纯色」变为
    /// 「提前真提交一次再重置表」——重置前必须提交，否则表里的色标被已录制、尚未
    /// 提交的实例引用着，它们在执行时会读到后来者的颜色。
    ///
    /// 判据取跨过 64 那道坎的最后两行与软后端逐像素比：退纯色的话它们会整块变成
    /// 首标色（黑），内部像素与墨量都对不上。
    #[test]
    fn gradients_beyond_table_capacity_stay_gradients() {
        const COLS: u32 = 10;
        const CELL: u32 = 10;
        const N: u32 = 80; // > GRAD_MAX(64)
        let draw = |c: &mut dyn Canvas| {
            for i in 0..N {
                let (x, y) = ((i % COLS) * CELL, (i / COLS) * CELL);
                let g = Gradient::linear(
                    (0.0, 0.0),
                    (1.0, 0.0),
                    vec![(0.0, Color::rgb(0, 0, 0)), (1.0, WHITE)],
                );
                c.fill_rect(
                    x as f32,
                    y as f32,
                    CELL as f32,
                    CELL as f32,
                    &Paint::gradient(g),
                );
            }
        };
        let interior: Vec<Box2> = (60..N)
            .map(|i| ((i % COLS) * CELL + 2, (i / COLS) * CELL + 2, 6, 6))
            .collect();
        assert_matches_soft(
            "grad_beyond_capacity",
            COLS * CELL,
            (N / COLS) * CELL,
            1.0,
            WHITE,
            &interior,
            draw,
        );
    }

    /// 一帧内的实例总数超过缓冲容量时，帧中途换缓冲不能丢掉先前录的批。
    ///
    /// 帧末一次提交之后，同一个实例缓冲要装下帧内**所有**批次（各批依次往后排）；
    /// 装不下就在帧中途换一张更大的并把游标归零。已录制的 pass 各自持着旧缓冲的
    /// 引用，换掉字段是安全的——这条判据钉的正是「换缓冲那一刻之前录的批还在画」。
    ///
    /// 造多批的手段是 `push_layer`：每次 push/pop 各强制 flush 一次。20 层 × 30 个
    /// 图元 = 600 个实例，超过 `prim.rs` 的 INIT_CAPACITY（512）。
    #[test]
    fn instance_buffer_growth_mid_frame_keeps_every_batch() {
        const LAYERS: i32 = 20;
        const PER: i32 = 30;
        assert_matches_soft(
            "instance_growth",
            60,
            40,
            1.0,
            WHITE,
            &[(10, 8, 20, 24)],
            |c| {
                for i in 0..LAYERS {
                    c.push_layer(1.0);
                    // 同一层内画多个**重叠**的不透明矩形：视觉上等价于画一个，
                    // 但实例数是 PER 倍——这里要的是实例数，不是画面复杂度。
                    for _ in 0..PER {
                        c.fill_rect(4.0 + i as f32, 4.0, 24.0, 24.0, &Paint::fill(RED));
                    }
                    c.pop_layer();
                }
            },
        );
    }

    /// 一帧只提交一次 command buffer，无论帧内交错出多少批。
    ///
    /// 这条钉的是改动的**收益本身**。交错的次数与控件数同阶（一帧上百次），每次
    /// `queue.submit` 在 Metal 上实测约 90 µs——退回「每批一提交」不会画错任何一个
    /// 像素，只会让帧时间翻几倍，没有计数器就只能等到下次量帧率才发现。
    #[test]
    fn a_frame_submits_once_no_matter_how_many_batches() {
        let Some(mut off) = offscreen(40, 40) else {
            return;
        };
        let mut eng = crate::text::NullTextEngine;
        let before = submit_count();
        draw_on(&mut off, 1.0, WHITE, &mut eng, &|c: &mut dyn Canvas| {
            // 10 次 push/pop = 至少 30 批（层清屏、层内几何、合成 quad 各一批）。
            for i in 0..10 {
                c.push_layer(1.0);
                c.fill_rect(i as f32, 0.0, 8.0, 40.0, &Paint::fill(RED));
                c.pop_layer();
            }
        })
        .expect("readback");
        assert_eq!(
            submit_count() - before,
            1,
            "一帧应只提交一次 command buffer（回归成每批一提交了？）"
        );
    }

    // ---- 局部重绘（damage → scissor）----

    /// 局部帧只重画脏区，框外**原样保留上一帧的像素**。
    ///
    /// 判据的锐利之处在于第二帧画的是**整窗**绿：只有 scissor 真的生效，右半才会留着
    /// 第一帧的蓝。少了 scissor 的话整张都会变绿，而画面本身看不出任何"异常"——局部
    /// 重绘错在这个方向上是最难发现的一类，它不报错、不闪、只是多画了。
    #[test]
    fn a_partial_frame_repaints_only_the_damaged_rect() {
        const BLUE: Color = Color::rgb(40, 70, 210);
        const GREEN: Color = Color::rgb(30, 160, 90);
        let Some(mut off) = offscreen(60, 40) else {
            return;
        };
        let mut eng = crate::text::NullTextEngine;
        // 第一帧：整窗，左红右蓝。
        off.clear(WHITE);
        {
            let mut t = off.target();
            t.begin_damage(None, WHITE);
            let mut c = t.make_canvas(&mut eng, 1.0);
            c.fill_rect(0.0, 0.0, 30.0, 40.0, &Paint::fill(RED));
            c.fill_rect(30.0, 0.0, 30.0, 40.0, &Paint::fill(BLUE));
        }
        // 第二帧：脏区只有左半，但**画满整窗**绿。
        {
            let mut t = off.target();
            t.begin_damage(Some(Rect::new(0, 0, 30, 40)), WHITE);
            let mut c = t.make_canvas(&mut eng, 1.0);
            c.fill_rect(0.0, 0.0, 60.0, 40.0, &Paint::fill(GREEN));
        }
        let pm = off.readback().expect("readback");
        let want_g = [GREEN.r, GREEN.g, GREEN.b, 255];
        let want_b = [BLUE.r, BLUE.g, BLUE.b, 255];
        assert_eq!(px(&pm, 15, 20), want_g, "脏区内应已重画成绿");
        assert_eq!(
            px(&pm, 45, 20),
            want_b,
            "脏区外应保留上一帧的蓝（scissor 没生效？）"
        );
        // 边界：脏区右缘的最后一列在内、下一列在外。
        assert_eq!(px(&pm, 29, 20), want_g, "脏区右缘最后一列应在内");
        assert_eq!(px(&pm, 30, 20), want_b, "脏区右缘之外的第一列应在外");
    }

    /// 整窗帧（`begin_damage(None)`）负责铺底；局部帧不铺——脏区那一块的底由宿主自己画。
    ///
    /// 这条钉的是「清屏从开帧挪到宿主宣告之后」这次改动：若整窗帧漏了铺底，上一帧的
    /// 内容会在没有控件覆盖的区域渗出来。
    #[test]
    fn a_full_frame_clears_the_target_but_a_partial_one_does_not() {
        const GREEN: Color = Color::rgb(30, 160, 90);
        let Some(mut off) = offscreen(40, 40) else {
            return;
        };
        let mut eng = crate::text::NullTextEngine;
        off.clear(WHITE);
        {
            let mut t = off.target();
            t.begin_damage(None, WHITE);
            let mut c = t.make_canvas(&mut eng, 1.0);
            c.fill_rect(0.0, 0.0, 40.0, 40.0, &Paint::fill(GREEN));
        }
        // 局部帧：宣告脏区后什么都不画 —— 目标必须原样不动。
        {
            let mut t = off.target();
            t.begin_damage(Some(Rect::new(0, 0, 20, 20)), WHITE);
            let _c = t.make_canvas(&mut eng, 1.0);
        }
        let pm = off.readback().expect("readback");
        assert_eq!(
            px(&pm, 10, 10),
            [GREEN.r, GREEN.g, GREEN.b, 255],
            "局部帧不该铺底（铺了就把上一帧盖掉了）"
        );
        // 整窗帧：宣告后什么都不画 —— 整张应回到底色。
        {
            let mut t = off.target();
            t.begin_damage(None, WHITE);
            let _c = t.make_canvas(&mut eng, 1.0);
        }
        let pm = off.readback().expect("readback");
        assert_eq!(px(&pm, 10, 10), [255, 255, 255, 255], "整窗帧必须铺底");
    }

    /// `cull_rect` 把物理脏区报成**逻辑坐标的超集**——绘制遍历据此跳过框外节点的自绘。
    ///
    /// 这是局部重绘在 CPU 侧的全部收益来源：scissor 省的是片元，而框外图元的构造与
    /// 排版开销在到达 scissor 之前就已经付掉了。报小了会真的丢内容，故一律向外取整。
    #[test]
    fn cull_rect_reports_a_logical_superset_of_the_damage() {
        let Some(mut off) = offscreen(64, 64) else {
            return;
        };
        let mut eng = crate::text::NullTextEngine;
        // scale=2：物理 (20,30)+24x18 → 逻辑 (10,15)+12x9，再各放一像素余量。
        {
            let mut t = off.target();
            t.begin_damage(Some(Rect::new(20, 30, 24, 18)), WHITE);
            let c = t.make_canvas(&mut eng, 2.0);
            assert_eq!(c.cull_rect(), Some(Rect::new(9, 14, 14, 11)));
        }
        // 整窗帧不剔除。
        {
            let mut t = off.target();
            t.begin_damage(None, WHITE);
            let c = t.make_canvas(&mut eng, 2.0);
            assert_eq!(c.cull_rect(), None, "整窗帧不得剔除任何节点");
        }
    }

    // ---- glyph atlas ----

    /// 同一段文字，走 atlas 与走整段光栅**画出来必须逐像素相同**。
    ///
    /// 这是 atlas 的总判据。它的风险从来不在性能而在观感——「文字与系统一致」是这个
    /// 项目的卖点，而拆成单字形最容易丢的是水平亚像素相位与基线取整。真引擎那一半由
    /// `coretext.rs` 的重组判据用墨量钉住（实测差 0.00~0.01%），这一半钉的是**放置**：
    /// mock 把文本块等分成无缝相接的字形，两条路径于是应当给出同一张图。
    #[test]
    fn atlas_and_whole_run_paths_paint_the_same_pixels() {
        let draw = |c: &mut dyn Canvas| {
            c.draw_text(
                "hello",
                Rect::new(3, 2, 40, 16),
                BLACK,
                Align::Start,
                &TextStyle::new(10.0),
            );
        };
        let mut atlas_eng = MockGlyphEngine::new((20, 8));
        let mut whole_eng = MockGlyphEngine::new((20, 8)).without_shaping();
        let Some(a) = render_gpu_text(48, 20, 1.0, WHITE, &mut atlas_eng, &draw) else {
            return;
        };
        let b = render_gpu_text(48, 20, 1.0, WHITE, &mut whole_eng, &draw).expect("整段路径");
        assert!(
            atlas_eng.glyph_calls > 0,
            "本判据要求 atlas 路径真的被走到（glyph_calls 应大于 0）"
        );
        assert_eq!(whole_eng.glyph_calls, 0, "对照组不该走 atlas");
        assert_eq!(a.data(), b.data(), "两条路径画出来必须逐像素相同");
    }

    /// 一个字形在整个 atlas 里只光栅一次，跨文本共享。
    ///
    /// 这是 atlas 相对整段粒度的**根本**收益：整段粒度下「控件 000」和「控件 001」是
    /// 两条独立的纹理，各光栅一遍；字形粒度下它们共用同一批格子。输入框逐字变化更极端
    /// ——整段粒度每帧全 miss，字形粒度只多出新敲的那一个。
    #[test]
    fn a_glyph_is_rastered_once_and_shared_across_runs() {
        let Some(mut off) = offscreen(40, 20) else {
            return;
        };
        // block 宽 20、两个字符 → 每个字形宽 10，两段文字用的是同一个字形键。
        let mut eng = MockGlyphEngine::new((20, 8));
        for t in ["ab", "cd", "ab"] {
            draw_on(&mut off, 1.0, WHITE, &mut eng, &|c: &mut dyn Canvas| {
                c.draw_text(
                    t,
                    Rect::new(0, 0, 40, 20),
                    BLACK,
                    Align::Start,
                    &TextStyle::new(8.0),
                );
            })
            .expect("readback");
        }
        assert_eq!(
            eng.glyph_calls, 1,
            "三段文字共 6 个字符只该光栅出 1 个不同字形，实得 {} 次",
            eng.glyph_calls
        );
        assert_eq!(eng.calls, 0, "走 atlas 就不该再整段光栅");
    }

    /// 一帧里的多条文字压成**一次** draw call。
    ///
    /// 整段粒度下每条 run 各自一张纹理，切绑定就得切 draw；atlas 下整帧共用一组绑定,
    /// 连续的实例合成一次 instanced draw。这条钉的是收益本身——它不改变任何一个像素,
    /// 回归了也只是变慢。
    #[test]
    fn a_frame_of_text_collapses_into_one_draw_call() {
        let Some(mut off) = offscreen(80, 80) else {
            return;
        };
        let mut eng = MockGlyphEngine::new((20, 8));
        let before = super::super::text::draw_count();
        draw_on(&mut off, 1.0, WHITE, &mut eng, &|c: &mut dyn Canvas| {
            for i in 0..8 {
                c.draw_text(
                    "ab",
                    Rect::new(0, i * 10, 40, 10),
                    BLACK,
                    Align::Start,
                    &TextStyle::new(8.0),
                );
            }
        })
        .expect("readback");
        assert_eq!(
            super::super::text::draw_count() - before,
            1,
            "8 条文字（16 个字形）应合成一次 draw call"
        );
    }

    /// 字形塞不进 atlas 时，这一段整体退回整段光栅——**不能**画出一半一半的重影。
    ///
    /// 退回必须是"整段"的：中途失败就放弃已取到的槽位，否则同一段文字会有一部分来自
    /// atlas、另一部分来自整段位图，叠在一起就是重影。
    #[test]
    fn a_glyph_too_big_for_the_atlas_falls_back_to_whole_run_raster() {
        // 单个字形宽 2500 > ATLAS_SIZE(2048)，货架分配必然失败。
        let mut eng = MockGlyphEngine::new((2500, 8));
        let draw = |c: &mut dyn Canvas| {
            c.draw_text(
                "x",
                Rect::new(0, 0, 200, 20),
                BLACK,
                Align::Start,
                &TextStyle::new(8.0),
            );
        };
        let Some(pm) = render_gpu_text(200, 20, 1.0, WHITE, &mut eng, &draw) else {
            return;
        };
        assert_eq!(eng.calls, 1, "应退回整段光栅（raster_run 恰好一次）");
        let bg = bg_bytes(WHITE);
        assert!(
            ink_bounds(&pm, bg).is_some(),
            "退回之后仍要把这段文字画出来，不能什么都不画"
        );
    }

    /// 稳态装不下时 atlas 会**关掉自己**，而不是每帧"重置→全部重光栅→再溢出"。
    ///
    /// 那个循环比根本不用 atlas 还慢（每帧把所有字形重新过一遍平台文字栈），而且悄无声息
    /// ——`notice_once` 早在第一次溢出时就用掉了。判据看的是"还试不试"：关掉之后
    /// `raster_glyph` 不该再被调用，文字则继续由整段光栅画出来。
    #[test]
    fn an_atlas_that_keeps_overflowing_turns_itself_off() {
        let Some(mut off) = offscreen(60, 20) else {
            return;
        };
        // 单个字形宽 2500 > ATLAS_SIZE(2048)，货架分配必然失败 ⇒ 每帧都溢出。
        let mut eng = MockGlyphEngine::new((2500, 8));
        let draw = |c: &mut dyn Canvas| {
            c.draw_text(
                "x",
                Rect::new(0, 0, 60, 20),
                BLACK,
                Align::Start,
                &TextStyle::new(8.0),
            );
        };
        // 前几帧：每帧都试一次 atlas（各光栅一个字形），并退回整段。
        for _ in 0..4 {
            draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("readback");
        }
        let tried = eng.glyph_calls;
        assert!(tried > 0, "前几帧应当真的试过 atlas");
        // 连续溢出到阈值后关掉：此后不再碰 raster_glyph。
        for _ in 0..3 {
            draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("readback");
        }
        assert_eq!(
            eng.glyph_calls, tried,
            "atlas 应已关闭，不该再光栅字形（实得 {} 次，关闭前 {tried} 次）",
            eng.glyph_calls
        );
        // 画面照旧：文字由整段光栅补上。
        let pm = draw_on(&mut off, 1.0, WHITE, &mut eng, &draw).expect("readback");
        assert!(
            ink_bounds(&pm, bg_bytes(WHITE)).is_some(),
            "关掉 atlas 之后仍要把文字画出来"
        );
    }

    /// 预乘清屏色：50% 红 → (0.5, 0, 0, 0.5)。窗口目标、离屏目标、层三处共用这一份，
    /// 算错的症状是「同一个 `bg` 在窗口里与在截图里是两个颜色」——最难被认成清屏色的锅。
    #[test]
    fn clear_color_is_premultiplied() {
        let c = clear_color(Color::rgba(255, 0, 0, 128));
        assert!((c.r - 128.0 / 255.0).abs() < 0.005, "实得 {}", c.r);
        assert_eq!(c.g, 0.0);
        assert_eq!(c.b, 0.0);
        assert!((c.a - 128.0 / 255.0).abs() < 0.005, "实得 {}", c.a);
        let o = clear_color(Color::rgb(255, 255, 255));
        assert_eq!((o.r, o.g, o.b, o.a), (1.0, 1.0, 1.0, 1.0));
    }

    /// `push_layer` 后不 `pop_layer`：帧末断言必须炸。
    ///
    /// 不平衡的层栈会把「本该合成回去的一整棵子树」留在层纹理里，画面上表现为一大块
    /// 内容凭空消失，而成因离现场很远。断言只在 debug 档生效，故本测试也只在 debug 编。
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "帧末合成层未归零")]
    fn unbalanced_push_layer_trips_the_frame_end_assert() {
        let Some(mut off) = offscreen(20, 20) else {
            // 没有适配器时这条判据无从验证。`should_panic` 分不清「跳过」和「没炸」，
            // 故补一个同文案的 panic，并照例打印一行跳过说明。
            println!("跳过：GPU 不可用，帧末层平衡断言未实际验证");
            panic!("帧末合成层未归零（跳过占位）");
        };
        let mut eng = crate::text::NullTextEngine;
        let _ = draw_on(&mut off, 1.0, WHITE, &mut eng, &|c| {
            c.push_layer(0.5);
            c.fill_rect(0.0, 0.0, 20.0, 20.0, &Paint::fill(RED));
        });
    }

    /// 真 Core Text 的**墨量比对**：软后端（直接合成进 pixmap）与 GPU 后端
    /// （光栅成 alpha mask → 纹理 → 调色合成）画同一段字，比墨迹范围与总墨量。
    ///
    /// 只在 macOS 上跑——上面那些 mock 判据管的是「位图往哪儿贴」，这一组管的是
    /// 「贴上去的像不像」，而后者只有真引擎能回答。判据用墨量而不是逐像素：两条
    /// 光栅路径（真背景上直接抗锯齿混合 vs 透明底光栅再合成）的边缘取值必然有差异，
    /// 逐像素比会把两个都对的实现判成不一致；墨量则能抓住「整体偏胖/偏瘦」「位置偏了」
    /// 「漏画了一块」这三类真问题（memory「视觉效果要量化验证」的同一条教训）。
    #[cfg(target_os = "macos")]
    mod coretext_ink {
        use super::*;
        use crate::text::CoreTextEngine;

        /// 墨迹范围各边允许的偏差（物理像素）。
        ///
        /// 实测六个场景**全部为 0**——两条路径的字形位置逐像素重合。留 1px 是给不同
        /// 系统版本/字体版本的度量微调留的余量，不是给实现留的。
        const BOUND_TOL: i64 = 1;
        /// 总墨量的相对差上限。
        ///
        /// 实测：单行英文/中文、折行长文、半透明、2x 缩放五个场景 **0.00%**（逐字节
        /// 相同——mask 与直接合成走的是同一份 Core Text 光栅），只有居中对齐是 0.14%，
        /// 差在水平亚像素相位（CG 的字形水平定位不吸附像素，而 mask 只能按相位 0 光栅
        /// 一份，见 `draw_text` 的注释）。取 2% ≈ 实测最差值的 14 倍余量：够容忍字体
        /// 版本差异，又能在「换了条光栅路径导致字重变了」时立刻报警——最初把 mask 画在
        /// 透明底上时，这个数是 13~17%。
        const INK_TOL: f64 = 0.02;

        /// 白底上的总墨量（各通道相对 255 的偏离之和）。
        fn ink_amount(pm: &Pixmap) -> f64 {
            pm.data()
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| (0..3).map(|i| (255 - p[i]) as f64).sum::<f64>())
                .sum()
        }

        /// 同场景两条路径各画一遍并比对。
        fn compare(name: &str, w: u32, h: u32, scale: f32, draw: &dyn Fn(&mut dyn Canvas)) {
            let mut soft = Pixmap::new(w, h).expect("软后端 pixmap");
            soft.fill(sk_color(WHITE));
            {
                let mut eng = CoreTextEngine::new();
                eng.set_scale(scale);
                let mut c = SkiaCanvas::with_text(&mut soft, &mut eng, scale);
                draw(&mut c);
            }
            let Some(mut off) = offscreen(w, h) else {
                return;
            };
            let mut eng = CoreTextEngine::new();
            eng.set_scale(scale);
            let gpu = draw_on(&mut off, scale, WHITE, &mut eng, draw).expect("GPU 帧读回");

            let (bs, bg) = (
                ink_bounds(&soft, WHITE_PX).expect("软后端应画出字"),
                ink_bounds(&gpu, WHITE_PX).expect("GPU 应画出字"),
            );
            let (is, ig) = (ink_amount(&soft), ink_amount(&gpu));
            let rel = if is > 0.0 { (ig - is).abs() / is } else { 1.0 };
            println!(
                "[{name}] 墨迹 软={bs:?} GPU={bg:?}  墨量 软={is:.0} GPU={ig:.0} 相对差={:.2}%",
                rel * 100.0
            );

            let edges = [
                ("左", bs.0, bg.0),
                ("上（首行基线位置）", bs.1, bg.1),
                ("右", bs.2, bg.2),
                ("下", bs.3, bg.3),
            ];
            for (which, a, b) in edges {
                let d = (a as i64 - b as i64).abs();
                assert!(
                    d <= BOUND_TOL,
                    "[{name}] 墨迹{which}边差 {d}px（软={a} GPU={b}），超过 {BOUND_TOL}px"
                );
            }
            assert!(
                rel <= INK_TOL,
                "[{name}] 墨量相对差 {:.2}% 超过 {:.0}%（软={is:.0} GPU={ig:.0}）",
                rel * 100.0,
                INK_TOL * 100.0
            );
        }

        #[test]
        fn single_line_latin() {
            compare("单行英文", 240, 40, 1.0, &|c| {
                c.draw_text(
                    "Hello windui",
                    Rect::new(10, 8, 220, 24),
                    BLACK,
                    Align::Start,
                    &TextStyle::new(14.0),
                );
            });
        }

        #[test]
        fn single_line_cjk() {
            compare("单行中文", 240, 40, 1.0, &|c| {
                c.draw_text(
                    "中文排版测试",
                    Rect::new(10, 8, 220, 24),
                    BLACK,
                    Align::Start,
                    &TextStyle::new(14.0),
                );
            });
        }

        /// 折行：rect 宽小于整段宽，两条路径必须折在同一处、行数一致。
        #[test]
        fn wrapped_paragraph() {
            compare("折行长文", 200, 140, 1.0, &|c| {
                c.draw_text(
                    "The quick brown fox jumps over the lazy dog near the river bank.",
                    Rect::new(10, 10, 160, 120),
                    BLACK,
                    Align::Start,
                    &TextStyle::new(13.0),
                );
            });
        }

        /// 居中对齐：块的水平落点是 GPU 侧自己算的，这条钉住它与引擎内部定位一致。
        #[test]
        fn centered_line() {
            compare("居中对齐", 240, 40, 1.0, &|c| {
                c.draw_text(
                    "Centered",
                    Rect::new(10, 8, 220, 24),
                    BLACK,
                    Align::Center,
                    &TextStyle::new(14.0),
                );
            });
        }

        /// 半透明文字色：覆盖度 × alpha 的调制是否与直接合成同量级。
        #[test]
        fn translucent_color() {
            compare("半透明字", 240, 40, 1.0, &|c| {
                c.draw_text(
                    "Translucent",
                    Rect::new(10, 8, 220, 24),
                    Color::rgba(0, 0, 0, 128),
                    Align::Start,
                    &TextStyle::new(14.0),
                );
            });
        }

        /// 2x 屏：字号物理化后重新排版（不是把 1x 的位图放大），两条路径同源。
        #[test]
        fn hidpi_scale_2() {
            compare("2x 缩放", 480, 80, 2.0, &|c| {
                c.draw_text(
                    "Retina 文字",
                    Rect::new(10, 8, 220, 24),
                    BLACK,
                    Align::Start,
                    &TextStyle::new(14.0),
                );
            });
        }
    }
}
