//! GPU 图片：`Image` → 预乘 RGBA 纹理缓存 → 一条带纹理的四边形（`image.wgsl` 的 CPU 侧）。
//!
//! # 缓存键是图片身份，不是像素
//!
//! 键取 [`Image::cache_id()`]（底层 `Rc<Pixmap>` 指针）+ 像素尺寸，与 d2d 后端的
//! `image_cache` 同一条路子。图片在本项目里是**构建期一次性解码的不可变资源**
//! （`Image` 内部 `Rc<Pixmap>`，克隆廉价且共享同一份像素），于是指针就是身份，不需要
//! 对像素做哈希——一张 4K 图每帧哈希一遍比重新上传还贵。
//!
//! 指针作键有一个已知代价：图片被释放后，新分配的 `Rc` 可能**复用同一地址**，于是命中
//! 一条本属于旧图的缓存。加进尺寸只把碰撞窄到「同尺寸的图恰好复用同一地址」，不能根除。
//! d2d 后端带着同一条风险跑了很久没出过问题（UI 图片的典型生命周期是随窗口常驻），这里
//! 沿用同一条取舍；真要根除得让 `Image` 自带单调递增的 id，那是 `render/image.rs` 的
//! 改动面，不属于本阶段。
//!
//! # 淘汰策略：比 d2d 温和一档
//!
//! d2d 的做法是「超过 64 条整体 `clear()`」——简单，但一旦越过水位就把**正在用的**图
//! 也一起扔了，下一帧全部重传。这里换成与文字 run-cache 同一份 LRU（[`LruCache`]），
//! 保留同样的 64 条水位，另加 64 MiB 的字节预算：条数管住 bind group 的管理开销，字节
//! 管住显存——一张 4096×4096 的图就是 64 MiB，只限条数的话 64 张这种图能吃掉 4 GiB。
//!
//! 剩余的内存风险写在这里：缓存**不随 `Image` 的释放而释放**（持有的是指针值，不是弱
//! 引用），一张已经没人引用的大图会一直占着显存，直到被 LRU 挤出去。上限兜住了总量，
//! 但「大图早就该走了却还占着」这段延迟是真实存在的。

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};

use super::device::SharedGpu;
use super::layer::{LayerPool, LayerTexture};
use super::text::LruCache;
use crate::render::image::Image;

/// 图片纹理缓存的条数上限（对齐 d2d `image_cache` 的同一水位）。
pub(super) const MAX_IMAGE_ENTRIES: usize = 64;
/// 图片纹理缓存的字节上限（RGBA = 4 字节/像素）。
pub(super) const MAX_IMAGE_BYTES: usize = 64 * 1024 * 1024;
/// 单批图片实例上限：到顶就先画一批（同 `prim.rs::MAX_BATCH` 的意思，量级小得多）。
pub(super) const MAX_IMAGE_BATCH: usize = 256;

/// 实例缓冲初始容量（实例数）。
const INIT_CAPACITY: usize = 32;

thread_local! {
    /// 本线程发生过的纹理上传次数。
    ///
    /// 计数放在模块级而不是挂在渲染器上：测试拿不到 `OffscreenGpu` 内部的
    /// `PrimRenderer`（`target()` 借出去的是 `WgpuTarget`），而「同一张图第二次绘制
    /// 不再上传」正是这份缓存存在与否的**唯一可观察证据**。
    ///
    /// 用 thread-local 而不是全局原子：测试是并行跑的，全局计数的增量判据会被同时在
    /// 跑的别的图片测试污染成随机数。UI 本来就是单线程的，每个渲染器也只在自己的线程上
    /// 用，按线程计数与按渲染器计数是同一回事。
    static UPLOADS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// 本线程已发生的图片纹理上传次数（测试判据用）。
#[cfg(test)]
pub(super) fn upload_count() -> u64 {
    UPLOADS.with(|c| c.get())
}

/// 图片纹理缓存的键：图片身份 + 像素尺寸（尺寸只是给指针复用加一道窄门，见模块头）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) struct ImageKey {
    id: usize,
    w: u32,
    h: u32,
}

/// 一张可采样的 RGBA 纹理 + 它的 group(1) 绑定。图片缓存与离屏层共用同一种资源：
/// 层就是「一张与目标同尺寸、可当渲染附着的图片」，合成时走的也是同一条采样路径。
pub(super) struct Bound {
    bind_group: wgpu::BindGroup,
    /// 层要拿它当渲染附着；图片缓存只是持有它保证纹理活着。
    view: wgpu::TextureView,
    // 纹理本身不再被直接引用（bind_group 与 view 持有它），但必须活着。
    _texture: wgpu::Texture,
}

impl Bound {
    /// 渲染附着视图（离屏层用；图片缓存条目不会走这条）。
    pub(super) fn view(&self) -> &wgpu::TextureView {
        &self.view
    }
}

/// 一条待画的图片（或离屏层的合成 quad）：实例数据 + 它引用的纹理绑定。
///
/// 以 `Arc` 持有：入批之后、画完之前，缓存可能被后来的 miss 逐出、层可能已还回池子，
/// 引用计数保证那张纹理活到本批画完。
pub(super) struct ImageItem {
    inst: ImageInstance,
    bound: Arc<Bound>,
}

/// 图片四边形的实例数据（64 B）。坐标是**物理**像素（同文字管线，理由见 `image.wgsl`）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct ImageInstance {
    /// 图片四边形（物理 x,y,w,h）。
    quad: [f32; 4],
    /// 圆角遮罩矩形（物理 x,y,w,h）= dst 框。
    mask: [f32; 4],
    /// 裁剪矩形（物理整数 x0,y0,x1,y1）。
    clip: [f32; 4],
    /// `[0]`=圆角半径（物理），`[1]`=opacity，`[2]`=1 走 nearest / 0 走 linear，`[3]` 保留。
    params: [f32; 4],
}

impl ImageInstance {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        const ATTRS: [wgpu::VertexAttribute; 4] = wgpu::vertex_attr_array![
            0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4,
        ];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<ImageInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &ATTRS,
        }
    }
}

/// 顶点着色器的每帧常量。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Globals {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

/// 图片在 dst 框内的落点：按 `fit` 求缩放、近 1:1 吸附、框内居中并取整到物理像素。
/// 返回（图片四边形 `[x,y,w,h]`，纹素是否与目标像素 1:1）。全程**物理**像素。
///
/// **逐行照抄 `SkiaCanvas::draw_image`（src/render/skia.rs:384-413），连注释里的理由
/// 一起搬**——同一张图标在两个后端下必须落在同一个像素上。抽成纯函数是为了让这几条
/// 语义（fit 的四种取法、吸附阈值、居中取整）能脱离 GPU 直接断言：它们是整块 P3 里
/// 最容易悄悄漂掉、又最难从截图上看出来的部分。
pub(super) fn place_image(
    fit: crate::render::image::Fit,
    iw: f32,
    ih: f32,
    pdst: [f32; 4],
    scale: f32,
) -> ([f32; 4], bool) {
    use crate::render::image::Fit;
    let [px, py, pw, ph] = pdst;
    // 按 fit 求缩放因子（均在物理空间）。
    let (sx, sy) = match fit {
        Fit::Fill => (pw / iw, ph / ih),
        Fit::Contain => {
            let k = (pw / iw).min(ph / ih);
            (k, k)
        }
        Fit::Cover => {
            let k = (pw / iw).max(ph / ih);
            (k, k)
        }
        // 1 图片像素 = 1 逻辑 dp → 物理为 ×scale。
        Fit::None => (scale, scale),
    };
    let (mut dw, mut dh) = (iw * sx, ih * sy);
    // 物理尺寸与源图相差不足 1 像素时吸附为 1:1：DPI 感知的矢量图标经此走上纯 blit
    // 路径，不再被重采样摊糊；`scaled()` 四边各自 round 带来的 ±1 误差也在此吸收
    // （否则细描边会被 0.97 倍这种"几乎 1:1"的缩放糊掉）。
    if (dw - iw).abs() < 1.0 && (dh - ih).abs() < 1.0 {
        dw = iw;
        dh = ih;
    }
    // 吸附后仍严格 1:1 才走 nearest。用相等判断而不是重算比值：上一步把"接近 1:1"
    // 一律改成了"恰好 1:1"，剩下的都是真在缩放。
    let nearest = dw == iw && dh == ih;
    // 在 dst 框内居中（Cover/None 的溢出由圆角遮罩收口）。落点取整到物理像素：
    // 尺寸对上了但平移带半像素，采样同样会把 1:1 的图糊掉。
    let tx = (px + (pw - dw) / 2.0).round();
    let ty = (py + (ph - dh) / 2.0).round();
    ([tx, ty, dw, dh], nearest)
}

/// 组装一条图片实例（供 `canvas.rs` 在算完 fit/吸附之后调用）。
/// `nearest=true` 表示纹素与目标像素 1:1，走精确 blit。
pub(super) fn image_item(
    bound: Arc<Bound>,
    quad: [f32; 4],
    mask: [f32; 4],
    clip: [f32; 4],
    radius: f32,
    opacity: f32,
    nearest: bool,
) -> ImageItem {
    ImageItem {
        inst: ImageInstance {
            quad,
            mask,
            clip,
            params: [radius, opacity, if nearest { 1.0 } else { 0.0 }, 0.0],
        },
        bound,
    }
}

/// 图片管线 + 纹理缓存 + 离屏层纹理池。懒建（一帧图片都没有的目标不付管线编译）。
///
/// 层池挂在这里而不是 `PrimRenderer` 上：层纹理要能被**当图片采样**（合成那一步），
/// 故它的 bind group 必须用本管线的布局与采样器来建。放一起省掉一层「把布局借出去」
/// 的借用体操。
pub(super) struct ImageRenderer {
    pipeline: wgpu::RenderPipeline,
    /// group(1) 的布局：每张图/每张层纹理建自己的 bind group 时要用。
    tex_bgl: wgpu::BindGroupLayout,
    samp_near: wgpu::Sampler,
    samp_lin: wgpu::Sampler,
    globals: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,
    instances: wgpu::Buffer,
    capacity: usize,
    /// 本帧已占用的实例槽数。见 [`super::prim::PrimRenderer::used`]。
    used: usize,
    format: wgpu::TextureFormat,
    cache: LruCache<ImageKey, Bound>,
    pool: LayerPool,
}

impl ImageRenderer {
    pub(super) fn new(gpu: &Arc<SharedGpu>, format: wgpu::TextureFormat) -> Self {
        let device = gpu.device();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("windui image shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("image.wgsl").into()),
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("windui image globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("windui image globals bgl"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let globals_bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("windui image globals bind group"),
            layout: &globals_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        // 一张纹理 + 两个采样器：走 nearest 还是 linear 由实例的 flag 在片元里选
        // （见 `image.wgsl`），于是同一张图在不同缩放下不需要两套绑定。
        let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("windui image tex bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let samp_near = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("windui image sampler nearest"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        // ClampToEdge 对齐 tiny-skia `PixmapPaint` 的 Pad 取边：缩放时最外圈纹素向外
        // 延伸，而不是与透明黑混合（后者会让缩放后的图边缘凭空多一圈暗边）。
        let samp_lin = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("windui image sampler linear"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("windui image layout"),
            bind_group_layouts: &[Some(&globals_bgl), Some(&tex_bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("windui image pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(ImageInstance::layout())],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: None,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    // 预乘 over —— 与几何/文字管线逐字相同的混合状态。
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::One,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let instances = new_instance_buffer(device, INIT_CAPACITY);
        Self {
            pipeline,
            tex_bgl,
            samp_near,
            samp_lin,
            globals,
            globals_bg,
            instances,
            capacity: INIT_CAPACITY,
            used: 0,
            format,
            cache: LruCache::new("图片纹理缓存", MAX_IMAGE_ENTRIES, MAX_IMAGE_BYTES),
            pool: LayerPool::new(),
        }
    }

    /// 取图片纹理：命中缓存直接返回，未命中就上传一张。尺寸退化或超设备上限时 `None`。
    pub(super) fn get_or_upload(
        &mut self,
        gpu: &Arc<SharedGpu>,
        img: &Image,
    ) -> Option<Arc<Bound>> {
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 {
            return None;
        }
        let key = ImageKey {
            id: img.cache_id(),
            w,
            h,
        };
        if let Some(b) = self.cache.get(&key) {
            return Some(b);
        }
        let device = gpu.device();
        let max = device.limits().max_texture_dimension_2d;
        if w > max || h > max {
            return None;
        }
        // `Pixmap` 存的就是**预乘 RGBA8**，与 `Rgba8Unorm` 的通道序一致（区别于 d2d
        // 后备缓冲的 BGRA），直接上传、不做任何转换——这也是两条路径能逐像素比对的前提。
        let data = img.pixmap().data();
        let need = (w as usize) * (h as usize) * 4;
        if data.len() < need {
            return None;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("windui image"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue().write_texture(
            texture.as_image_copy(),
            &data[..need],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                // `write_texture` 不要求 256 对齐（那是 buffer→texture 拷贝的限制）。
                bytes_per_row: Some(w * 4),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        UPLOADS.with(|c| c.set(c.get() + 1));
        let bound = self.bind_texture(device, texture);
        Some(self.cache.insert(key, Arc::new(bound), need))
    }

    /// 建一张纹理的视图与 group(1) 绑定。
    fn bind_texture(&self, device: &wgpu::Device, texture: wgpu::Texture) -> Bound {
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("windui image bind group"),
            layout: &self.tex_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.samp_near),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.samp_lin),
                },
            ],
        });
        Bound {
            bind_group,
            view,
            _texture: texture,
        }
    }

    /// 取一张 `size` 大小的离屏层纹理（池中有就复用），并清成全透明。
    pub(super) fn acquire_layer(
        &mut self,
        gpu: &Arc<SharedGpu>,
        size: (u32, u32),
        encoder: &mut wgpu::CommandEncoder,
    ) -> Option<LayerTexture> {
        let format = self.format;
        let layer = match self.pool.take(size) {
            Some(l) => l,
            None => {
                let texture = LayerPool::create_texture(gpu, size, format)?;
                let bound = self.bind_texture(gpu.device(), texture);
                LayerTexture::new(Arc::new(bound), size)
            }
        };
        // 复用的那张带着上一次的内容，必须清干净（见 `LayerTexture::clear`）。
        layer.clear(encoder);
        self.pool.acquired();
        Some(layer)
    }

    /// 把用完的层纹理还回池子（超过池上限就直接丢掉，见 `layer.rs`）。
    pub(super) fn release_layer(&mut self, layer: LayerTexture) {
        self.pool.put(layer);
    }

    /// 帧末收尾（`WgpuCanvas` 析构时调）：回收池里的超额纹理、归零帧内实例游标。
    pub(super) fn end_frame(&mut self) {
        self.pool.end_frame();
        self.used = 0;
    }

    /// 把 `batch` 里攒下的图片画掉，随后清空。
    ///
    /// 一张图一个 draw call（各自的纹理绑定），但**共用一个 render pass 与一次提交**
    /// ——与文字管线同一条结构。与几何/文字的交错顺序由 `canvas.rs` 保证。
    pub(super) fn flush(
        &mut self,
        gpu: &Arc<SharedGpu>,
        view: &wgpu::TextureView,
        size: (u32, u32),
        scissor: Option<[u32; 4]>,
        encoder: &mut wgpu::CommandEncoder,
        batch: &mut Vec<ImageItem>,
    ) {
        if batch.is_empty() || size.0 == 0 || size.1 == 0 {
            batch.clear();
            return;
        }
        let device = gpu.device();
        let queue = gpu.queue();
        queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&Globals {
                viewport: [size.0 as f32, size.1 as f32],
                _pad: [0.0; 2],
            }),
        );
        let n = batch.len();
        // 帧内游标：一帧只提交一次，各批必须各占一段（理由同 `prim.rs::flush`）。
        if self.used + n > self.capacity {
            let mut cap = self.capacity.max(1) * 2;
            while cap < n {
                cap *= 2;
            }
            self.instances = new_instance_buffer(device, cap);
            self.capacity = cap;
            self.used = 0;
        }
        let stride = std::mem::size_of::<ImageInstance>() as u64;
        let off = self.used as u64 * stride;
        let insts: Vec<ImageInstance> = batch.iter().map(|i| i.inst).collect();
        queue.write_buffer(&self.instances, off, bytemuck::cast_slice(&insts));

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("windui image pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // 目标已有内容（清屏或先前的批次）必须保留 —— painter's algorithm。
                        load: wgpu::LoadOp::Load,
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if let Some(r) = scissor {
                pass.set_scissor_rect(r[0], r[1], r[2], r[3]);
            }
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.globals_bg, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(off..off + n as u64 * stride));
            for (i, item) in batch.iter().enumerate() {
                pass.set_bind_group(1, &item.bound.bind_group, &[]);
                pass.draw(0..6, i as u32..i as u32 + 1);
            }
        }
        self.used += n;
        batch.clear();
    }
}

fn new_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("windui image instances"),
        size: (capacity * std::mem::size_of::<ImageInstance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::image::Fit;

    /// 四种 fit 的缩放取法。判据取「图片四边形的尺寸」而不是缩放因子本身——
    /// 后者是中间量，前者才是画面上看得见的东西。
    #[test]
    fn fit_modes_scale_as_the_soft_backend_does() {
        // 源 4×2（宽高比 2:1），dst 40×40。
        let dst = [0.0, 0.0, 40.0, 40.0];
        // Fill：非等比拉满。
        let (q, _) = place_image(Fit::Fill, 4.0, 2.0, dst, 1.0);
        assert_eq!([q[2], q[3]], [40.0, 40.0]);
        // Contain：等比取**短**边（40/4=10 vs 40/2=20 → 10）→ 40×20，上下留白。
        let (q, _) = place_image(Fit::Contain, 4.0, 2.0, dst, 1.0);
        assert_eq!([q[2], q[3]], [40.0, 20.0]);
        assert_eq!([q[0], q[1]], [0.0, 10.0], "Contain 应在框内居中");
        // Cover：等比取**长**边（20）→ 80×40，左右溢出（由圆角遮罩裁掉）。
        let (q, _) = place_image(Fit::Cover, 4.0, 2.0, dst, 1.0);
        assert_eq!([q[2], q[3]], [80.0, 40.0]);
        assert_eq!([q[0], q[1]], [-20.0, 0.0], "Cover 的溢出应左右对称");
        // None：1 图片像素 = 1 逻辑 dp，物理为 ×scale。
        let (q, _) = place_image(Fit::None, 4.0, 2.0, dst, 2.0);
        assert_eq!([q[2], q[3]], [8.0, 4.0]);
    }

    /// 近 1:1 吸附：物理尺寸与源图差**不足 1 物理像素**时按 1:1 画，并走 nearest。
    ///
    /// 典型落点是非整数 DPI 下的图标：`Fit::None` 要把 8px 的图画成 8.4px，不吸附就是
    /// 一次 1.05 倍的重采样，细描边直接糊掉。阈值取"半像素级的取整误差"，而不是"看着
    /// 差不多"——差满 1 个像素属于**确实要放大**（`< 1.0` 是严格小于，与软后端逐字一致）。
    #[test]
    fn near_unit_scale_snaps_to_one_to_one() {
        // scale=1.05：8×8 → 8.4×8.4，差 0.4 → 吸附回 8×8 并走 nearest。
        let (q, near) = place_image(Fit::None, 8.0, 8.0, [0.0, 0.0, 40.0, 40.0], 1.05);
        assert_eq!([q[2], q[3]], [8.0, 8.0], "应吸附为 1:1");
        assert!(near, "吸附后应走 nearest");
        assert_eq!([q[0], q[1]], [16.0, 16.0], "吸附后仍在框内居中并取整");
        // 差恰好 1 像素：不吸附，按真缩放走（源 8 → 框 9）。
        let (q, near) = place_image(Fit::Contain, 8.0, 8.0, [5.0, 5.0, 9.0, 9.0], 1.0);
        assert_eq!([q[2], q[3]], [9.0, 9.0], "差满 1 像素属于真缩放，不该吸附");
        assert!(!near);
        // 真缩放：走 linear，否则放大后是马赛克。
        let (q, near) = place_image(Fit::Contain, 8.0, 8.0, [0.0, 0.0, 12.0, 12.0], 1.0);
        assert_eq!([q[2], q[3]], [12.0, 12.0]);
        assert!(!near, "真缩放必须走 linear");
    }

    /// 恰好 1:1（无需吸附）同样走 nearest。
    #[test]
    fn exact_unit_scale_uses_nearest() {
        let (q, near) = place_image(Fit::Contain, 4.0, 4.0, [5.0, 5.0, 4.0, 4.0], 1.0);
        assert_eq!(q, [5.0, 5.0, 4.0, 4.0]);
        assert!(near);
    }

    /// 半像素落点必须取整：尺寸对上了但平移带半像素，采样照样把 1:1 的图糊掉。
    #[test]
    fn placement_rounds_to_whole_pixels() {
        let (q, _) = place_image(Fit::Contain, 4.0, 4.0, [0.0, 0.0, 4.0, 5.0], 1.0);
        // 竖直方向 (5-4)/2 = 0.5 → round 到 1（`f32::round` 半数远离零）。
        assert_eq!(q[1], 1.0);
        assert_eq!(q[1].fract(), 0.0, "落点不得留小数");
    }

    /// 实例布局的字节数必须与 shader 的属性偏移对得上（同 `prim.rs`/`text.rs` 的同款判据）。
    #[test]
    fn image_instance_is_64_bytes() {
        assert_eq!(std::mem::size_of::<ImageInstance>(), 64);
        assert_eq!(std::mem::align_of::<ImageInstance>(), 4);
    }

    /// `nearest` 标志进的是 `params.z`——shader 按 `> 0.5` 判，写错位置的症状是
    /// 1:1 的图标被 linear 糊掉，而这在纯色测试图上根本看不出来。
    #[test]
    fn nearest_flag_lands_in_params_z() {
        let inst = ImageInstance {
            quad: [0.0; 4],
            mask: [0.0; 4],
            clip: [0.0; 4],
            params: [4.0, 0.5, 1.0, 0.0],
        };
        assert_eq!(inst.params[2], 1.0);
        let src = include_str!("image.wgsl");
        assert!(
            src.contains("in.params.z > 0.5"),
            "image.wgsl 的采样器选择判据变了，Rust 侧的 flag 位置要跟着改"
        );
        assert!(
            src.contains("in.params.y"),
            "image.wgsl 的 opacity 取自 params.y"
        );
    }

    /// 键含尺寸：同一地址被复用时，尺寸不同至少能挡住一部分误命中（见模块头）。
    #[test]
    fn key_includes_size() {
        let a = ImageKey {
            id: 1,
            w: 10,
            h: 10,
        };
        assert_ne!(
            a,
            ImageKey {
                id: 1,
                w: 10,
                h: 11
            }
        );
        assert_ne!(
            a,
            ImageKey {
                id: 2,
                w: 10,
                h: 10
            }
        );
        assert_eq!(
            a,
            ImageKey {
                id: 1,
                w: 10,
                h: 10
            }
        );
    }
}
