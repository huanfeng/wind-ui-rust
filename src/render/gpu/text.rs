//! GPU 文字：平台光栅 → R8 覆盖度纹理的 **run-cache** → 一条带纹理的四边形。
//!
//! 分工的那条线：**字形像素永远由平台 API 生成**（`GlyphSource`，macOS 侧是 Core Text），
//! GPU 只做上传与混合。这是「文字与系统一致」这个卖点的落点——自己在 shader 里画字形
//! （SDF 字体、曲线求值）会立刻在字重、hinting、字体回退上与系统不一样，而那种不一样
//! 用户一眼就能看出来，却很难说清差在哪。
//!
//! # 缓存粒度：整段 run（Phase 1）
//!
//! 键是「整段文字 + 全部影响排版的属性」，值是**一整段**光栅出来的纹理。于是一个按钮
//! 标签、一行表格文本各占一条，颜色不进键（颜色在片元里调制，同一段字换色仍复用同一
//! 张纹理）。
//!
//! 代价是动态文本（输入框逐字变化、数值刷新）每帧都不命中，而且每条 run 一张独立纹理、
//! 一个独立 bind group ⇒ 一帧的 draw call 数量与文字条数同阶。glyph atlas（把粒度降到
//! 单字形、跨文本共享、合批成一次 draw）是 **P5** 的事，见
//! `docs/gpu-cross-platform-design.md` §4.4 Phase 2。先把 Phase 1 跑起来量化，再决定
//! 要不要付 atlas 那份复杂度——这是设计文档写死的顺序。
//!
//! # Phase 1 的实测代价（P5 的决策输入）
//!
//! macOS/M4、`examples/settings`（48 条文字/帧）、**debug** 档、`WINDUI_PROF`：
//!
//! | | 首帧（冷缓存） | 稳定帧（全命中） |
//! | --- | --- | --- |
//! | `text` 桶 | 18.8 ms | **4.31 ms** |
//! | 整帧 | 27.1 ms | 9.5 ms |
//!
//! 稳定帧那 4.31 ms **几乎全部不是缓存的开销**：把 `canvas.rs` 里「文字入批前先 flush
//! 几何」那一行临时去掉（会画错叠放顺序，只为量开销）后，同一场景的 `text` 桶掉到
//! **0.08 ms**、整帧 5.6 ms。也就是说缓存查找 + 放置合计约 1.7 µs/条，而交错带来的
//! **每条一次 command buffer 提交**约 90 µs/次。
//!
//! 为什么不能把这些提交合并成一次：几何管线的实例缓冲与渐变表都写在偏移 0
//! （`prim.rs::flush`），而 `queue.write_buffer` 的写入是相对**提交**定序的，不是相对
//! 录制定序——一个 encoder 里录两个 pass，第二批的数据会在第一批执行前就把缓冲覆盖掉。
//! 要合并就得给每批分配独立的缓冲区段（渐变表还需要 256 对齐的 dynamic offset），那是
//! P1 批处理的重新设计，不属于本阶段。
//!
//! 结论摆在这里供 P5 取舍：atlas 的收益不只是显存和命中率，更是把「每条文字一次提交」
//! 压回「整帧一次」。作为对照，同一场景的软后端整帧 211 ms（debug，fill-bound）。
//!
//! # 缓存挂在哪
//!
//! 挂在 [`PrimRenderer`](super::prim::PrimRenderer) 上（= 每个渲染目标一份），而不是
//! 进程级共享。理由是管线绑死了附着格式，而只有目标知道自己的格式；纹理本可以跨目标
//! 共享，但拆成「共享纹理池 + 各自管线」要多一层生命周期管理，在多窗口真成为瓶颈之前
//! 不值得。多窗口场景下每个窗口各一份缓存，预算也是各算各的。

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use bytemuck::{Pod, Zeroable};

use super::device::SharedGpu;
use crate::geometry::Color;
use crate::spec::Align;
use crate::text::{AlphaMask, TextStyle};

/// 缓存条目数上限。
pub(super) const MAX_ENTRIES: usize = 512;
/// 缓存字节数上限（纹理体量，R8 = 1 字节/像素）。
///
/// 条数与字节**双限**：只限条数时，几条大字号的长段落就能吃掉几十 MB；只限字节时，
/// 几千条小标签又会把 HashMap 与 bind group 的管理开销撑起来。两条各自兜住一种极端。
pub(super) const MAX_BYTES: usize = 16 * 1024 * 1024;
/// 单批文字实例上限：到顶就先画一批（同 `prim.rs` 的 `MAX_BATCH`，只是量级小得多）。
pub(super) const MAX_TEXT_BATCH: usize = 1024;

/// 实例缓冲初始容量（实例数）。
const INIT_CAPACITY: usize = 64;

/// run-cache 的键：**一切影响排版与光栅结果的输入**，颜色除外。
///
/// 浮点字段存 `to_bits()` 而不是浮点本身：`f32` 没有 `Eq`/`Hash`。副作用是 `-0.0` 与
/// `0.0` 会成为两条目（值相同、位不同），代价只是多一条缓存，不会画错。
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(super) struct RunKey {
    text: String,
    family: Option<String>,
    size: u32,
    weight: u16,
    /// `line_height` 的位；`None` 用 `u32::MAX` 当哨兵（NaN 的位模式与它不冲突：
    /// 这里的值来自 `Option<f32>`，`None` 根本不产生位）。
    line_height: u32,
    max_width: u32,
    scale: u32,
    /// `Align` 没有 `Hash`（它在 `spec.rs`，不属于本阶段的改动面），存判别值。
    align: u8,
}

impl RunKey {
    pub(super) fn new(
        text: &str,
        ts: &TextStyle,
        align: Align,
        max_width: f32,
        scale: f32,
    ) -> Self {
        Self {
            text: text.to_string(),
            family: ts.family.map(|f| f.to_string()),
            size: ts.size.to_bits(),
            weight: ts.weight,
            line_height: ts.line_height.map(f32::to_bits).unwrap_or(u32::MAX),
            max_width: max_width.to_bits(),
            scale: scale.to_bits(),
            align: match align {
                Align::Start => 0,
                Align::Center => 1,
                Align::End => 2,
                Align::Stretch => 3,
            },
        }
    }
}

/// 一条已上传的文字纹理。以 `Arc` 交出：入批之后、画完之前它可能被后来的 miss 逐出，
/// 引用计数保证那张纹理活到本帧画完。
pub(super) struct RunTexture {
    /// group(1)：覆盖度纹理 + 采样器。
    bind_group: wgpu::BindGroup,
    /// 位图物理尺寸（含 `pad`）。
    pub(super) width: u32,
    pub(super) height: u32,
    /// 位图四周的出挑余量（见 [`AlphaMask::pad`]）。
    pub(super) pad: u32,
    /// 文本块的未取整物理尺寸。定位按它算，**不是**按位图尺寸——见 [`AlphaMask::block`]。
    pub(super) block: (f32, f32),
    /// 首行基线距块顶的物理距离（见 [`AlphaMask::ascent`]）。
    pub(super) ascent: f32,
    // 纹理本身不再被直接引用（bind_group 持有它），但必须活着——bind group 只借视图。
    _texture: wgpu::Texture,
    _view: wgpu::TextureView,
}

/// 一条待画的文字：实例数据 + 它引用的纹理。
pub(super) struct TextItem {
    inst: TextInstance,
    tex: Arc<RunTexture>,
}

/// 文字四边形的实例数据（48 B）。坐标是**物理**像素——与几何管线相反，理由见 `text.wgsl`。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct TextInstance {
    /// 位图四边形（物理 x,y,w,h）。
    quad: [f32; 4],
    /// 裁剪矩形（物理整数 x0,y0,x1,y1）。
    clip: [f32; 4],
    /// 非预乘 RGBA（0..1）。
    color: [f32; 4],
}

impl TextInstance {
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        const ATTRS: [wgpu::VertexAttribute; 3] =
            wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4, 2 => Float32x4];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<TextInstance>() as u64,
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

// ---- LRU 缓存 ----

/// 缓存计数（`WINDUI_PROF` 下打印，也是「第二次绘制不再光栅」这条测试的判据）。
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct CacheStats {
    pub(super) entries: usize,
    pub(super) bytes: usize,
    pub(super) hits: u64,
    pub(super) misses: u64,
    pub(super) evictions: u64,
}

struct Cached<V> {
    value: Arc<V>,
    /// 与 `map` 里的键同一份。`HashMap` 不给「按引用取回键的所有权句柄」的接口，
    /// 而 `get` 命中后要拿它去更新时序表，故在值里也存一份 `Arc`（一次引用计数，
    /// 不是一份拷贝）。
    key: Arc<RunKey>,
    /// 最近一次命中的时刻，与 `order` 里的键一一对应。
    stamp: u64,
    bytes: usize,
}

/// 条数 + 字节双预算的 LRU。
///
/// 用 `HashMap` + 一张 `BTreeMap<stamp, key>` 的时序索引，而不是「HashMap + 每次逐出
/// 线性扫最小值」：逐出在缓存打满后是**每次 miss 都要做**的事，线性扫 512 条会把
/// 光栅省下来的时间又搭进去。`BTreeMap` 的首元素即最久未用。
///
/// 键存 `Arc<RunKey>`（两张表共享同一份）而不是各存一份：`get` 在命中时要往时序表里
/// 重新登记一次键，直接 clone 会连 `String` 一起复制——那是**每帧、每条文字**一次
/// 堆分配，而缓存命中本该是这条路径上最便宜的一步。
pub(super) struct RunCache<V> {
    map: HashMap<Arc<RunKey>, Cached<V>>,
    order: BTreeMap<u64, Arc<RunKey>>,
    tick: u64,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
    /// 上次打印过的高水位（条数 / 字节）。用来把 `WINDUI_PROF` 的输出压成「增长时
    /// 才报一行」——缓存统计每帧打印就是刷屏，而真正有诊断价值的恰恰是增长趋势。
    reported: (usize, usize),
}

impl<V> RunCache<V> {
    pub(super) fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: BTreeMap::new(),
            tick: 0,
            bytes: 0,
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1),
            hits: 0,
            misses: 0,
            evictions: 0,
            reported: (0, 0),
        }
    }

    pub(super) fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.map.len(),
            bytes: self.bytes,
            hits: self.hits,
            misses: self.misses,
            evictions: self.evictions,
        }
    }

    /// 取并「touch」（刷新 LRU 时序）。未命中记 miss。
    pub(super) fn get(&mut self, key: &RunKey) -> Option<Arc<V>> {
        let tick = self.tick;
        let Some(e) = self.map.get_mut(key) else {
            self.misses += 1;
            return None;
        };
        self.order.remove(&e.stamp);
        e.stamp = tick;
        let value = e.value.clone();
        let key = e.key.clone();
        self.order.insert(tick, key);
        self.tick += 1;
        self.hits += 1;
        Some(value)
    }

    /// 放入并按预算逐出。返回放入的值（调用方紧接着就要用它）。
    pub(super) fn insert(&mut self, key: RunKey, value: Arc<V>, bytes: usize) -> Arc<V> {
        let key = Arc::new(key);
        // 同键重入（并发/重算）时先把旧条目的账平掉，否则 bytes 会只增不减。
        if let Some(old) = self.map.remove(&key) {
            self.order.remove(&old.stamp);
            self.bytes -= old.bytes;
        }
        let stamp = self.tick;
        self.tick += 1;
        self.bytes += bytes;
        self.order.insert(stamp, key.clone());
        self.map.insert(
            key.clone(),
            Cached {
                value: value.clone(),
                key,
                stamp,
                bytes,
            },
        );
        self.evict();
        self.report();
        value
    }

    /// 逐出最久未用的，直到两条预算都满足。
    ///
    /// **永远留下至少一条**：单条就超字节预算时（超大字号的长段落），一路逐出会把刚
    /// 放进来的这条也扔掉，而调用方马上就要用它——那是 use-after-evict 的等价物，
    /// 只不过在 Rust 里表现为「画不出来」而不是崩。
    fn evict(&mut self) {
        while self.map.len() > 1
            && (self.map.len() > self.max_entries || self.bytes > self.max_bytes)
        {
            let Some((&stamp, key)) = self.order.iter().next() else {
                break;
            };
            let key = key.clone();
            self.order.remove(&stamp);
            if let Some(old) = self.map.remove(&key) {
                self.bytes -= old.bytes;
            }
            self.evictions += 1;
        }
    }

    /// `WINDUI_PROF` 下按高水位报告一行。
    ///
    /// 不每帧打印：缓存统计的诊断价值在**增长趋势**（是不是有一类文本永远不命中、
    /// 预算是不是设小了），而每帧一行会把 `prof` 本来的耗时拆分淹掉。故只在条数或
    /// 字节翻倍、以及首次发生逐出（= 预算真的开始起作用）时各报一行。
    fn report(&mut self) {
        if !crate::render::prof::enabled() {
            return;
        }
        let s = self.stats();
        let (rn, rb) = self.reported;
        let grew = s.entries >= rn.max(8) * 2 || s.bytes >= rb.max(256 * 1024) * 2;
        let first_evict = s.evictions > 0 && rn == 0 && rb == 0;
        if !(grew || first_evict) {
            return;
        }
        self.reported = (s.entries, s.bytes);
        eprintln!(
            "windui: gpu 文字 run-cache {} 条 / {:.1} MiB（命中 {} 未命中 {} 逐出 {}）",
            s.entries,
            s.bytes as f64 / (1024.0 * 1024.0),
            s.hits,
            s.misses,
            s.evictions
        );
    }
}

// ---- 渲染器 ----

/// 文字管线 + run-cache。懒建（一帧都没有文字的目标不该付一次管线编译）。
pub(super) struct TextRenderer {
    pipeline: wgpu::RenderPipeline,
    /// group(1) 的布局：每条 run 建自己的 bind group 时要用。
    tex_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    globals: wgpu::Buffer,
    globals_bg: wgpu::BindGroup,
    instances: wgpu::Buffer,
    capacity: usize,
    cache: RunCache<RunTexture>,
}

impl TextRenderer {
    pub(super) fn new(gpu: &Arc<SharedGpu>, format: wgpu::TextureFormat) -> Self {
        let device = gpu.device();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("windui text shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("text.wgsl").into()),
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("windui text globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("windui text globals bgl"),
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
            label: Some("windui text globals bind group"),
            layout: &globals_bgl,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });
        let tex_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("windui text tex bgl"),
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
            ],
        });
        // nearest：位图与目标 1:1，线性过滤只会把平台光栅好的边缘再糊一层。
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("windui text sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("windui text layout"),
            bind_group_layouts: &[Some(&globals_bgl), Some(&tex_bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("windui text pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(TextInstance::layout())],
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
                    // 预乘 over —— 与几何管线逐字相同的混合状态。
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
            sampler,
            globals,
            globals_bg,
            instances,
            capacity: INIT_CAPACITY,
            cache: RunCache::new(MAX_ENTRIES, MAX_BYTES),
        }
    }

    /// 查缓存。
    pub(super) fn get(&mut self, key: &RunKey) -> Option<Arc<RunTexture>> {
        self.cache.get(key)
    }

    /// 把一张 alpha mask 传成 R8 纹理并入缓存。尺寸退化或超设备上限时返回 `None`。
    pub(super) fn upload(
        &mut self,
        gpu: &Arc<SharedGpu>,
        key: RunKey,
        mask: &AlphaMask,
    ) -> Option<Arc<RunTexture>> {
        let (w, h) = (mask.width, mask.height);
        if w == 0 || h == 0 || mask.data.len() < (w as usize) * (h as usize) {
            return None;
        }
        let device = gpu.device();
        let max = device.limits().max_texture_dimension_2d;
        if w > max || h > max {
            return None;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("windui text run"),
            size: wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            // R8：覆盖度只需要一个通道。颜色在片元里调制，不进纹理。
            format: wgpu::TextureFormat::R8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        gpu.queue().write_texture(
            texture.as_image_copy(),
            &mask.data[..(w as usize) * (h as usize)],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                // `write_texture` 不要求 256 对齐（那是 buffer→texture 拷贝的限制），
                // 逐行字节数即宽度。
                bytes_per_row: Some(w),
                rows_per_image: Some(h),
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("windui text run bind group"),
            layout: &self.tex_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        let bytes = (w as usize) * (h as usize);
        let entry = Arc::new(RunTexture {
            bind_group,
            width: w,
            height: h,
            pad: mask.pad,
            block: mask.block,
            ascent: mask.ascent,
            _texture: texture,
            _view: view,
        });
        Some(self.cache.insert(key, entry, bytes))
    }

    /// 把 `batch` 里攒下的文字画掉，随后清空。
    ///
    /// 一条 run 一个 draw call（各自的纹理绑定），但**共用一个 render pass 与一次
    /// 提交**——连续的文字之间没有几何要插进来，没必要各开一个 pass。与几何的交错
    /// 顺序由 `canvas.rs` 保证（入批前互相 flush）。
    pub(super) fn flush(
        &mut self,
        gpu: &Arc<SharedGpu>,
        view: &wgpu::TextureView,
        size: (u32, u32),
        batch: &mut Vec<TextItem>,
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
        if batch.len() > self.capacity {
            let mut cap = self.capacity.max(1);
            while cap < batch.len() {
                cap *= 2;
            }
            self.instances = new_instance_buffer(device, cap);
            self.capacity = cap;
        }
        let insts: Vec<TextInstance> = batch.iter().map(|i| i.inst).collect();
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&insts));

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("windui text encoder"),
        });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("windui text pass"),
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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.globals_bg, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(..));
            for (i, item) in batch.iter().enumerate() {
                pass.set_bind_group(1, &item.tex.bind_group, &[]);
                pass.draw(0..6, i as u32..i as u32 + 1);
            }
        }
        queue.submit([encoder.finish()]);
        batch.clear();
    }
}

/// 组装一条文字实例（供 `canvas.rs` 在算完放置后调用）。
pub(super) fn text_item(
    tex: Arc<RunTexture>,
    quad: [f32; 4],
    clip: [f32; 4],
    color: Color,
) -> TextItem {
    TextItem {
        inst: TextInstance {
            quad,
            clip,
            color: super::prim::color_f32(color),
        },
        tex,
    }
}

fn new_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("windui text instances"),
        size: (capacity * std::mem::size_of::<TextInstance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// 测试专用的 `GlyphSource`：光栅确定性图案，不碰平台文字栈。
///
/// 存在的理由是**放置逻辑与平台无关**——对齐、垂直定位、裁剪、缓存、批次顺序全在
/// `canvas.rs`/本文件里，用真引擎测等于把这些判据绑死在 macOS 上（Windows 上连编译
/// 都编不到 Core Text）。真引擎的那一半（光栅得像不像）由 macOS 上的墨量比对负责。
#[cfg(test)]
pub(super) mod mock {
    use crate::geometry::{Rect, Size};
    use crate::spec::Align;
    use crate::text::{AlphaMask, GlyphSource, RunRequest, TextEngine, TextStyle};
    use tiny_skia::Pixmap;

    /// mask 的图案。
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum Pattern {
        /// 文本块内整块 255、pad 环 0。ink 边界恰等于文本块边界，放置断言最锐利。
        Solid,
        /// 2×2 棋盘（块内）。用来确认采样是 1:1 而不是被过滤糊掉。
        Checker,
    }

    /// 可当 `TextEngine` 用的 mock：measure 走等宽近似，光栅出确定性图案。
    pub(crate) struct MockGlyphEngine {
        /// 文本块的**逻辑**尺寸；光栅时乘 scale 得物理尺寸。
        pub(crate) block: (u32, u32),
        /// 出挑余量（物理像素）。
        pub(crate) pad: u32,
        pub(crate) pattern: Pattern,
        /// `raster_run` 的调用次数——「同键第二次不再光栅」这条判据就看它。
        pub(crate) calls: u32,
        /// 最近一次请求的 scale / max_width（键相关断言用）。
        pub(crate) last: Option<(f32, f32)>,
    }

    impl MockGlyphEngine {
        pub(crate) fn new(block: (u32, u32)) -> Self {
            Self {
                block,
                pad: 0,
                pattern: Pattern::Solid,
                calls: 0,
                last: None,
            }
        }
        pub(crate) fn with_pad(mut self, pad: u32) -> Self {
            self.pad = pad;
            self
        }
        pub(crate) fn with_pattern(mut self, p: Pattern) -> Self {
            self.pattern = p;
            self
        }
    }

    impl TextEngine for MockGlyphEngine {
        fn measure(&mut self, _t: &str, ts: &TextStyle, _mw: Option<f32>) -> Size {
            Size::new(self.block.0 as i32, ts.size.ceil() as i32)
        }
        fn draw(
            &mut self,
            _pm: &mut Pixmap,
            _t: &str,
            _r: Rect,
            _c: crate::geometry::Color,
            _a: Align,
            _ts: &TextStyle,
            _clip: Option<Rect>,
        ) {
        }
        fn glyph_source(&mut self) -> Option<&mut dyn GlyphSource> {
            Some(self)
        }
    }

    impl GlyphSource for MockGlyphEngine {
        fn raster_run(&mut self, req: &RunRequest) -> Option<AlphaMask> {
            self.calls += 1;
            self.last = Some((req.scale, req.max_width));
            let s = req.scale.max(0.01);
            let bw = ((self.block.0 as f32 * s).round() as u32).max(1);
            let bh = ((self.block.1 as f32 * s).round() as u32).max(1);
            let pad = self.pad;
            let (w, h) = (bw + 2 * pad, bh + 2 * pad);
            let block = (bw as f32, bh as f32);
            let mut data = vec![0u8; (w * h) as usize];
            for y in 0..bh {
                for x in 0..bw {
                    let v = match self.pattern {
                        Pattern::Solid => 255,
                        Pattern::Checker => {
                            if (x / 2 + y / 2) % 2 == 0 {
                                255
                            } else {
                                0
                            }
                        }
                    };
                    data[((y + pad) * w + (x + pad)) as usize] = v;
                }
            }
            Some(AlphaMask {
                data,
                width: w,
                height: h,
                pad,
                block,
                // mock 没有真字形，取块高的 0.8 当基线（同 `TextEngine::line_metrics`
                // 的默认近似）。
                ascent: block.1 * 0.8,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::TextStyle;

    fn key(text: &str, size: f32, scale: f32) -> RunKey {
        RunKey::new(text, &TextStyle::new(size), Align::Start, 100.0, scale)
    }

    /// 键含全部影响排版的属性：任一项不同即不同键。漏一项的症状是「改了属性画面不变」，
    /// 而且只在缓存命中时显形——本测试是唯一能在改动当场抓住它的地方。
    #[test]
    fn key_covers_every_layout_input() {
        let base = TextStyle {
            family: Some("A"),
            size: 14.0,
            weight: 400,
            line_height: Some(1.5),
        };
        let k = |ts: &TextStyle, align, mw, scale| RunKey::new("hi", ts, align, mw, scale);
        let b = k(&base, Align::Start, 100.0, 1.0);
        assert_eq!(b, k(&base, Align::Start, 100.0, 1.0), "同输入应同键");
        assert_ne!(b, k(&base, Align::Center, 100.0, 1.0), "align 必须进键");
        assert_ne!(b, k(&base, Align::Start, 101.0, 1.0), "max_width 必须进键");
        assert_ne!(b, k(&base, Align::Start, 100.0, 2.0), "scale 必须进键");
        assert_ne!(
            b,
            RunKey::new("ho", &base, Align::Start, 100.0, 1.0),
            "文本必须进键"
        );
        for varied in [
            TextStyle {
                family: Some("B"),
                ..base
            },
            TextStyle { size: 15.0, ..base },
            TextStyle {
                weight: 700,
                ..base
            },
            TextStyle {
                line_height: Some(2.0),
                ..base
            },
            TextStyle {
                line_height: None,
                ..base
            },
        ] {
            assert_ne!(
                b,
                k(&varied, Align::Start, 100.0, 1.0),
                "样式项必须进键：{varied:?}"
            );
        }
    }

    /// scale 变化天然换键（不需要"整体失效"那种粗暴做法），旧键仍可被 LRU 逐出。
    #[test]
    fn scale_change_produces_a_new_key() {
        let mut c: RunCache<u32> = RunCache::new(8, 1 << 20);
        c.insert(key("hi", 14.0, 1.0), Arc::new(1), 16);
        assert!(c.get(&key("hi", 14.0, 2.0)).is_none(), "换 scale 应未命中");
        c.insert(key("hi", 14.0, 2.0), Arc::new(2), 64);
        assert_eq!(c.stats().entries, 2, "两个 scale 各占一条，互不覆盖");
        assert_eq!(*c.get(&key("hi", 14.0, 1.0)).unwrap(), 1);
    }

    /// 条数预算：超限逐出**最久未用**的那条（而不是最早插入的）。
    #[test]
    fn evicts_least_recently_used_by_entry_budget() {
        let mut c: RunCache<u32> = RunCache::new(3, 1 << 20);
        for (i, t) in ["a", "b", "c"].iter().enumerate() {
            c.insert(key(t, 14.0, 1.0), Arc::new(i as u32), 8);
        }
        // touch a：现在最久未用的是 b。
        assert!(c.get(&key("a", 14.0, 1.0)).is_some());
        c.insert(key("d", 14.0, 1.0), Arc::new(3), 8);
        assert_eq!(c.stats().entries, 3);
        assert!(
            c.get(&key("b", 14.0, 1.0)).is_none(),
            "b 最久未用，应被逐出"
        );
        assert!(
            c.get(&key("a", 14.0, 1.0)).is_some(),
            "刚 touch 过的 a 应还在"
        );
        assert!(c.get(&key("d", 14.0, 1.0)).is_some());
    }

    /// 字节预算：条数没超也要按体量逐出，且 `bytes` 统计要跟着掉下来。
    #[test]
    fn evicts_by_byte_budget() {
        let mut c: RunCache<u32> = RunCache::new(100, 1000);
        c.insert(key("a", 14.0, 1.0), Arc::new(0), 600);
        c.insert(key("b", 14.0, 1.0), Arc::new(1), 300);
        assert_eq!(c.stats().bytes, 900);
        c.insert(key("c", 14.0, 1.0), Arc::new(2), 400);
        assert!(c.stats().bytes <= 1000, "超字节预算应逐出到预算内");
        assert!(
            c.get(&key("a", 14.0, 1.0)).is_none(),
            "最久未用的大条应先走"
        );
        assert!(c.get(&key("c", 14.0, 1.0)).is_some());
    }

    /// 单条就超预算时**不能把自己逐掉**：调用方拿到的 Arc 马上就要用。
    #[test]
    fn oversized_single_entry_survives() {
        let mut c: RunCache<u32> = RunCache::new(4, 100);
        let v = c.insert(key("huge", 14.0, 1.0), Arc::new(7), 10_000);
        assert_eq!(*v, 7);
        assert_eq!(c.stats().entries, 1, "唯一一条即便超预算也要留着");
        assert!(c.get(&key("huge", 14.0, 1.0)).is_some());
    }

    /// 同键重入不能把 `bytes` 记两遍（否则跑一会儿预算就被虚高的账逼着狂逐出）。
    #[test]
    fn reinsert_same_key_replaces_accounting() {
        let mut c: RunCache<u32> = RunCache::new(8, 1 << 20);
        c.insert(key("a", 14.0, 1.0), Arc::new(1), 100);
        c.insert(key("a", 14.0, 1.0), Arc::new(2), 250);
        assert_eq!(c.stats().entries, 1);
        assert_eq!(c.stats().bytes, 250, "旧账必须平掉");
        assert_eq!(*c.get(&key("a", 14.0, 1.0)).unwrap(), 2);
    }

    /// 命中/未命中计数——「第二次绘制不再光栅」那条端到端判据靠它兜底。
    #[test]
    fn stats_track_hits_and_misses() {
        let mut c: RunCache<u32> = RunCache::new(8, 1 << 20);
        assert!(c.get(&key("a", 14.0, 1.0)).is_none());
        c.insert(key("a", 14.0, 1.0), Arc::new(1), 8);
        assert!(c.get(&key("a", 14.0, 1.0)).is_some());
        let s = c.stats();
        assert_eq!((s.hits, s.misses), (1, 1));
    }

    /// 实例布局的字节数必须与 shader 的属性偏移对得上（同 `prim.rs` 的同款判据）。
    #[test]
    fn text_instance_is_48_bytes() {
        assert_eq!(std::mem::size_of::<TextInstance>(), 48);
    }

    /// mock 的 mask：ink 恰好落在文本块内、pad 环全 0。放置断言全靠这条性质。
    #[test]
    fn mock_mask_ink_matches_block() {
        use crate::text::{GlyphSource, RunRequest};
        let mut m = mock::MockGlyphEngine::new((10, 4)).with_pad(2);
        let ts = TextStyle::new(12.0);
        let mask = m
            .raster_run(&RunRequest {
                text: "hi",
                style: ts,
                align: Align::Start,
                max_width: 100.0,
                scale: 2.0,
            })
            .expect("mock 应总能光栅");
        assert_eq!(
            (mask.width, mask.height),
            (24, 12),
            "物理块 20×8 + pad 各 2"
        );
        assert_eq!(mask.block, (20.0, 8.0), "块尺寸应是未取整的物理尺寸");
        assert_eq!(mask.pad, 2);
        // pad 环全 0。
        for x in 0..mask.width {
            assert_eq!(mask.data[x as usize], 0, "顶部 pad 行应为空");
        }
        assert_eq!(mask.data[(2 * mask.width + 2) as usize], 255, "块内应有墨");
    }
}
