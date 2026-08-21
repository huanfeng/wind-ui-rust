//! 离屏层：`push_layer(opacity)` 的纹理栈与纹理池。
//!
//! # 语义（对齐软后端 `SkiaCanvas::push_layer`/`pop_layer`）
//!
//! `push_layer` 把后续绘制**重定向**到一张与目标同尺寸的全透明纹理，`pop_layer` 把整张
//! 层纹理按 opacity 合成回父目标。关键是「整体」二字：子树内部 A 盖 B 的重叠部分只算
//! 一次 opacity，而不是各自半透明地叠一遍——这正是软后端画到独立 pixmap 再 `draw_pixmap`
//! 的效果，也是 `push_pop_layer_composites_with_opacity` 那条判据（50% 红叠白底成粉，
//! 而不是更浅或更深）真正钉住的东西。嵌套即栈。
//!
//! 合成走的是图片管线（`tex.rs`/`image.wgsl`）：层纹理就是一张与目标 1:1 的图片，
//! 遮罩取整个目标、圆角 0、opacity 取层的 opacity、采样走 nearest（1:1 无重采样）。
//! 层内的像素是**预乘**的，整体乘 opacity 后仍是预乘，配 `ONE/ONE_MINUS_SRC_ALPHA`
//! 混合即得正确结果。
//!
//! # 与「缓冲写入按提交定序」那条教训的关系
//!
//! P2 记下的坑是：`queue.write_buffer` 相对**提交**定序，一个 encoder 里录两个 pass，
//! 后写的实例数据会在前一个 pass 执行前就把缓冲覆盖掉。层重定向没有踩上去，因为它没有
//! 引入「一个 encoder 多个 pass」——每次切目标（push 前、pop 前、合成时）都各自是一次
//! 完整的「write_buffer + 一个 pass + submit」，与 P1/P2 的交错 flush 是同一条结构。
//! 代价同样是每次切层各一次提交；层切换的频率远低于文字条数，不值得为它做批次重设计。
//!
//! # 纹理池
//!
//! 层纹理是窗口尺寸的（1920×1080 就是 8 MiB），动画期间每帧 push/pop 一次，现建现丢会
//! 把分配器和显存都打穿。池按尺寸复用（窗口 resize 后旧尺寸的直接丢弃），并在**帧末**
//! 把池收缩到「本帧实际用到的最大嵌套深度」——于是只在浮层动画期间常驻，动画停了就还
//! 给系统。设计文档 §8 的「池上限 + 帧末回收超额」即此。

use std::sync::Arc;

use super::device::SharedGpu;
use super::tex::Bound;

/// 池里最多留几张层纹理。嵌套深度超过它的部分每次现建现丢——UI 里三层以上的
/// 嵌套 opacity 子树基本不存在，为它常驻显存不划算。
const MAX_POOL: usize = 4;

thread_local! {
    /// 本线程新建层纹理的次数。理由同 `tex.rs` 的上传计数（含为什么是 thread-local）：
    /// 池是否真的在复用，只能从「第二帧没有新建」观察到。
    static ALLOCS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// 本线程已新建的层纹理张数（测试判据用）。
#[cfg(test)]
pub(super) fn alloc_count() -> u64 {
    ALLOCS.with(|c| c.get())
}

/// 一张离屏层纹理。既是渲染附着（层内绘制的目标），也是可采样的图片（合成那一步）。
pub(super) struct LayerTexture {
    bound: Arc<Bound>,
    size: (u32, u32),
    /// 合成回父目标时的整体不透明度（`push_layer` 的入参，已 clamp）。
    pub(super) opacity: f32,
}

impl LayerTexture {
    pub(super) fn new(bound: Arc<Bound>, size: (u32, u32)) -> Self {
        Self {
            bound,
            size,
            opacity: 1.0,
        }
    }

    /// 层内绘制的渲染附着视图。
    pub(super) fn view(&self) -> &wgpu::TextureView {
        self.bound.view()
    }

    /// 合成时要采样的纹理绑定。
    pub(super) fn bound(&self) -> Arc<Bound> {
        self.bound.clone()
    }

    pub(super) fn size(&self) -> (u32, u32) {
        self.size
    }

    /// 清成全透明。层必须从透明开始——它的 alpha 就是子树的覆盖度，残留上一帧的内容
    /// 会在合成时以「上一帧的影子」形式渗出来。
    ///
    /// **录进调用方的帧 encoder，不自己提交**。自己提交会插到帧序列的最前面执行，而
    /// 池是复用纹理的：`push A → pop A（合成时采样 A）→ push B（池里取回同一张）`
    /// 这条路径下，B 的清屏会赶在「合成 A」之前跑掉，把 A 的像素抹成透明——症状是
    /// 前一个层凭空消失，而成因在两层之外。
    pub(super) fn clear(&self, encoder: &mut wgpu::CommandEncoder) {
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("windui layer clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: self.view(),
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
    }
}

/// 层纹理池：按尺寸复用，帧末收缩到本帧用到的最大深度。
pub(super) struct LayerPool {
    free: Vec<LayerTexture>,
    /// 本帧当前的嵌套深度与峰值（帧末据峰值决定留几张）。
    live: usize,
    peak: usize,
}

impl LayerPool {
    pub(super) fn new() -> Self {
        Self {
            free: Vec::new(),
            live: 0,
            peak: 0,
        }
    }

    /// 取一张 `size` 的空闲层纹理。尺寸不符的条目顺手丢掉（窗口 resize 后旧尺寸的
    /// 纹理再也用不上了，留着只是占显存）。
    ///
    /// 不在这里记深度：取空返回 `None` 时调用方还要现建一张，而现建也可能失败，
    /// 只有调用方知道这次 acquire 到底成没成——记账统一由 [`Self::acquired`] 收口。
    pub(super) fn take(&mut self, size: (u32, u32)) -> Option<LayerTexture> {
        self.free.retain(|l| l.size() == size);
        self.free.pop()
    }

    /// 登记一次成功的 acquire（更新本帧嵌套深度的峰值）。
    pub(super) fn acquired(&mut self) {
        self.live += 1;
        self.peak = self.peak.max(self.live);
    }

    /// 还回一张层纹理。池满就直接丢。
    pub(super) fn put(&mut self, layer: LayerTexture) {
        self.live = self.live.saturating_sub(1);
        if self.free.len() < MAX_POOL {
            self.free.push(layer);
        }
    }

    /// 帧末回收：把池收缩到本帧实际用到的最大嵌套深度（且不超过池上限），并重置计数。
    ///
    /// 一帧都没用过层的界面于是不会留着窗口尺寸的纹理——浮层/淡入动画停了显存就还回去，
    /// 这比「留着以防万一」更符合「层纹理很大」这个前提。
    pub(super) fn end_frame(&mut self) {
        let keep = self.peak.min(MAX_POOL);
        self.free.truncate(keep);
        self.live = 0;
        self.peak = 0;
    }

    /// 建一张可当渲染附着、也可被采样的层纹理。尺寸退化或超设备上限时 `None`。
    pub(super) fn create_texture(
        gpu: &SharedGpu,
        size: (u32, u32),
        format: wgpu::TextureFormat,
    ) -> Option<wgpu::Texture> {
        let (w, h) = size;
        let device = gpu.device();
        let max = device.limits().max_texture_dimension_2d;
        if w == 0 || h == 0 || w > max || h > max {
            return None;
        }
        ALLOCS.with(|c| c.set(c.get() + 1));
        Some(device.create_texture(&wgpu::TextureDescriptor {
            label: Some("windui layer"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        }))
    }
}
