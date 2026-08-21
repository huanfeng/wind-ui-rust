//! 图元实例收集与批渲染（`shader.wgsl` 的 CPU 侧）。
//!
//! 设计意图：**整帧一条管线、一次 instanced draw**。`Canvas` 的图元集封闭（无任意 path、
//! 无纹理、无变换），几何全部有解析 SDF 表达；连裁剪矩形都做成实例字段在片元里逐像素裁，
//! 而不是切 scissor——于是帧内没有任何状态切换，实例攒完一次画掉即可。这比「按 clip 分批」
//! 少一层批次管理，也避开了非整数 DPI 下 scissor 只能整像素、软后端 mask 却按逻辑矩形
//! 取整所带来的边界分歧。
//!
//! 坐标：实例里存的是**逻辑**坐标（裁剪矩形除外，见 [`Instance::clip`]），由顶点着色器
//! 统一乘 `scale` 物理化（对标 d2d 的 `SetTransform`）。这样描边的物理像素对齐、圆角
//! clamp 等几何决策全部在与软后端**同一个坐标空间**里做，逐条对着 `skia.rs` 抄即可。

use std::sync::Arc;

use bytemuck::{Pod, Zeroable};

use super::device::SharedGpu;
use super::tex::ImageRenderer;
use super::text::TextRenderer;
use crate::geometry::Color;
use crate::render::{Gradient, Paint};

// ---- 图元类型（与 shader.wgsl 的 KIND_* 一一对应，改动须同步）----
const KIND_RECT: u32 = 0;
const KIND_CIRCLE: u32 = 1;
const KIND_STROKE: u32 = 2;
const KIND_LINE: u32 = 3;
const KIND_SHADOW: u32 = 4;

// ---- flags 位域（同上）----
const FLAG_AA: u32 = 1;
const GRAD_LINEAR: u32 = 2;
const GRAD_RADIAL: u32 = 4;

/// 每组渐变在表里占的 `vec4` 数：8 个非预乘 RGBA 色标 + 2 个打包 offset。
pub(super) const GRAD_STRIDE: usize = 10;
/// 单组渐变的色标上限。超出部分截断（截断保留渐变观感；退纯色会整块变样）。
pub(super) const GRAD_MAX_STOPS: usize = 8;
/// 一帧内的渐变组数上限。`Limits::downlevel_defaults()` 的
/// `max_uniform_buffer_binding_size` 是 16 KiB，64×10×16 = 10 KiB 留足余量。
pub(super) const GRAD_MAX: usize = 64;
/// 「本实例无渐变」的哨兵基址。
const GRAD_NONE: u32 = u32::MAX;

/// 实例缓冲的初始容量（实例数）。不够时翻倍重建。
const INIT_CAPACITY: usize = 512;
/// 单批实例上限：到顶就先画一批。绘制顺序即提交顺序（painter's algorithm），
/// 中途 flush 不改变叠放次序，只是多一次 draw call。
pub(super) const MAX_BATCH: usize = 16384;

/// 一个图元 = 一个实例（128 B）。字段语义随 `meta[0]`（kind）而变。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub(super) struct Instance {
    /// quad 外包框（逻辑 x,y,w,h）：顶点着色器据此展开两个三角形。
    /// 阴影的模糊外扩、描边与抗锯齿的边缘余量都已由构造方算进来。
    bbox: [f32; 4],
    /// 图元矩形（逻辑 x,y,w,h）。circle 为外接正方形（渐变包围盒也用它，与
    /// `SkiaCanvas::fill_circle` 同源）；stroke 为**描边中心线**矩形。
    rect: [f32; 4],
    /// 线段端点（逻辑 x0,y0,x1,y1）。仅 `KIND_LINE` 使用。
    line: [f32; 4],
    /// 裁剪矩形（**物理整数** x0,y0,x1,y1）。这是唯一不经顶点着色器缩放的字段：
    /// 软后端的裁剪 mask 是 `Rect::scaled` 取整后的非抗锯齿整数矩形，先取整再缩放
    /// 与先缩放再取整不是一回事，要逐像素对上就必须在 CPU 侧定死。
    clip: [f32; 4],
    /// 非预乘 RGBA（0..1）。有渐变时是回退色（对齐 `Paint::gradient` 的首标回退）。
    color: [f32; 4],
    /// 渐变几何（逻辑）：linear=(p0.xy, p1.xy)；radial=(center.xy, radius, 0)。
    grad: [f32; 4],
    /// `[0]`=圆角/圆半径，`[1]`=描边半宽 或 阴影 σ，`[2..]` 保留。均为逻辑长度。
    params: [f32; 4],
    /// `[0]`=kind，`[1]`=flags，`[2]`=渐变基址（`GRAD_NONE` 为无），`[3]`=色标数。
    meta: [u32; 4],
}

impl Instance {
    /// 顶点缓冲布局：8 个 `vec4` 属性，逐实例步进。
    fn layout() -> wgpu::VertexBufferLayout<'static> {
        const ATTRS: [wgpu::VertexAttribute; 8] = wgpu::vertex_attr_array![
            0 => Float32x4, 1 => Float32x4, 2 => Float32x4, 3 => Float32x4,
            4 => Float32x4, 5 => Float32x4, 6 => Float32x4, 7 => Uint32x4,
        ];
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Instance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &ATTRS,
        }
    }
}

/// 顶点/片元共享的每帧常量。
#[repr(C)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
struct Globals {
    viewport: [f32; 2],
    scale: f32,
    _pad: f32,
}

/// 非预乘 sRGB 字节 → 0..1。**不做 gamma 转换**：附着是 `Rgba8Unorm`，字节即最终像素值，
/// 与 `tiny_skia::Pixmap` 同一空间——这是两条路径能逐像素比对的前提。
pub(super) fn color_f32(c: Color) -> [f32; 4] {
    [
        c.r as f32 / 255.0,
        c.g as f32 / 255.0,
        c.b as f32 / 255.0,
        c.a as f32 / 255.0,
    ]
}

/// 一帧（或一批）待画的图元 + 它们引用的渐变表。
#[derive(Default)]
pub(super) struct PrimBatch {
    instances: Vec<Instance>,
    /// 渐变表，按 `GRAD_STRIDE` 一组连续存放。**帧内累积**（`clear` 不动它）。
    grads: Vec<[f32; 4]>,
    /// 已写进 GPU 渐变缓冲的 `vec4` 数（增量写的水位）。
    ///
    /// 存在 batch 上而不是 `PrimRenderer` 上：batch 每帧新建，水位于是自动随帧归零，
    /// 也与 [`Self::reset_grads`] 天然配对——放渲染器上则两处都要各自记得重置，漏一处
    /// 的症状是「此后整帧的渐变全部读到上一批的色标」，而现场看不出任何异常。
    written: usize,
}

impl PrimBatch {
    pub(super) fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    pub(super) fn len(&self) -> usize {
        self.instances.len()
    }

    /// 清掉已画出去的实例。**渐变表不清**：它在帧内累积，见
    /// [`PrimRenderer::flush`] 里「增量写」那段的理由。
    pub(super) fn clear(&mut self) {
        self.instances.clear();
    }

    /// 渐变表是否已满（本帧再登记就会退纯色）。满了要由调用方真提交一次并
    /// [`Self::reset_grads`]——见 `canvas.rs::maybe_flush`。
    pub(super) fn grads_full(&self) -> bool {
        self.grads.len() / GRAD_STRIDE >= GRAD_MAX
    }

    /// 丢弃已累积的渐变表。**只能在引用它的实例都已提交之后调用**，否则那些实例
    /// 会在执行时读到被覆盖的色标。
    pub(super) fn reset_grads(&mut self) {
        self.grads.clear();
        self.written = 0;
    }

    /// 登记一组渐变，返回 `(基址, 色标数, flags 里的渐变类型位)`。
    /// 返回 `None` 表示退纯色：色标不足 2（与 `sk_shader` 同判据）或本帧渐变已满。
    fn push_gradient(&mut self, g: &Gradient, x: f32, y: f32, w: f32, h: f32) -> Option<GradRef> {
        let stops = g.stops();
        if stops.len() < 2 {
            return None;
        }
        if self.grads.len() / GRAD_STRIDE >= GRAD_MAX {
            warn_once(
                &GRAD_OVERFLOW,
                "windui: gpu 后端单帧渐变超过 64 组，超出部分退化为纯色",
            );
            return None;
        }
        if stops.len() > GRAD_MAX_STOPS {
            warn_once(
                &GRAD_TOO_MANY_STOPS,
                "windui: gpu 后端单个渐变的色标超过 8 个，多余色标被截断",
            );
        }
        let base = self.grads.len() as u32;
        let count = stops.len().min(GRAD_MAX_STOPS);
        let mut offs = [0.0f32; GRAD_MAX_STOPS];
        let mut prev = 0.0f32;
        for i in 0..GRAD_MAX_STOPS {
            let s = stops[i.min(count - 1)];
            self.grads.push(color_f32(s.color));
            // clamp 与 `sk_shader` 一致；再取累积最大值保证单调——shader 的插值
            // 循环假定 offset 递增，乱序输入会取到错误的区间。
            prev = s.offset.clamp(0.0, 1.0).max(prev);
            offs[i] = prev;
        }
        self.grads.push([offs[0], offs[1], offs[2], offs[3]]);
        self.grads.push([offs[4], offs[5], offs[6], offs[7]]);

        // 归一化坐标 → 逻辑坐标，语义逐字对齐 `render/mod.rs` 的注释与 `sk_shader`。
        let (kind, geom) = match g {
            Gradient::Linear { start, end, .. } => (
                GRAD_LINEAR,
                [
                    x + start.0 * w,
                    y + start.1 * h,
                    x + end.0 * w,
                    y + end.1 * h,
                ],
            ),
            Gradient::Radial { center, radius, .. } => (
                GRAD_RADIAL,
                [
                    x + center.0 * w,
                    y + center.1 * h,
                    // 半径以短边为基准（保持圆形而非随宽高拉成椭圆），下限同 `sk_shader`。
                    (radius * w.min(h)).max(0.01),
                    0.0,
                ],
            ),
        };
        Some(GradRef {
            base,
            count: count as u32,
            kind,
            geom,
        })
    }

    /// fill 类图元的公共部分：解析 paint 得到颜色/渐变/flags。
    /// `(x,y,w,h)` 是渐变的归一化包围盒（circle 传外接正方形）。
    fn fill_bits(
        &mut self,
        paint: &Paint,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> ([f32; 4], [f32; 4], u32, u32, u32) {
        let mut flags = if paint.anti_alias { FLAG_AA } else { 0 };
        let color = color_f32(paint.color);
        match paint
            .gradient
            .as_ref()
            .and_then(|g| self.push_gradient(g, x, y, w, h))
        {
            Some(gr) => {
                flags |= gr.kind;
                (color, gr.geom, flags, gr.base, gr.count)
            }
            None => (color, [0.0; 4], flags, GRAD_NONE, 0),
        }
    }

    /// 圆角矩形填充（`radius<=0` 即直角）。`clip` 为物理整数裁剪矩形。
    pub(super) fn push_round_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        paint: &Paint,
        clip: [f32; 4],
        aa_margin: f32,
    ) {
        if w <= 0.0 || h <= 0.0 {
            return; // 与 `rounded_rect_path` 返回 None 同步。
        }
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        let (color, grad, flags, base, count) = self.fill_bits(paint, x, y, w, h);
        self.instances.push(Instance {
            bbox: inflate(x, y, w, h, aa_margin),
            rect: [x, y, w, h],
            line: [0.0; 4],
            clip,
            color,
            grad,
            params: [r, 0.0, 0.0, 0.0],
            meta: [KIND_RECT, flags, base, count],
        });
    }

    /// 圆填充。渐变包围盒取外接正方形（与 `SkiaCanvas::fill_circle` 同源）。
    pub(super) fn push_circle(
        &mut self,
        cx: f32,
        cy: f32,
        r: f32,
        paint: &Paint,
        clip: [f32; 4],
        aa_margin: f32,
    ) {
        if r <= 0.0 {
            return; // `PathBuilder::from_circle` 对非正半径返回 None。
        }
        let (x, y, w, h) = (cx - r, cy - r, 2.0 * r, 2.0 * r);
        let (color, grad, flags, base, count) = self.fill_bits(paint, x, y, w, h);
        self.instances.push(Instance {
            bbox: inflate(x, y, w, h, aa_margin),
            rect: [x, y, w, h],
            line: [0.0; 4],
            clip,
            color,
            grad,
            params: [r, 0.0, 0.0, 0.0],
            meta: [KIND_CIRCLE, flags, base, count],
        });
    }

    /// 圆角矩形描边。`rect` 传**描边中心线**矩形、`half` 传半宽（几何由调用方按
    /// `SkiaCanvas::stroke_round_rect` 的内缩语义算好）。描边不吃渐变（软后端同样退纯色）。
    pub(super) fn push_stroke(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        half: f32,
        paint: &Paint,
        clip: [f32; 4],
        aa_margin: f32,
    ) {
        if w < 0.0 || h < 0.0 || half <= 0.0 {
            return;
        }
        let flags = if paint.anti_alias { FLAG_AA } else { 0 };
        self.instances.push(Instance {
            bbox: inflate(x, y, w, h, half + aa_margin),
            rect: [x, y, w, h],
            line: [0.0; 4],
            clip,
            color: color_f32(paint.color),
            grad: [0.0; 4],
            params: [radius, half, 0.0, 0.0],
            meta: [KIND_STROKE, flags, GRAD_NONE, 0],
        });
    }

    /// 线段（Butt 端帽）。`half` 为半线宽。
    pub(super) fn push_line(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        half: f32,
        paint: &Paint,
        clip: [f32; 4],
        aa_margin: f32,
    ) {
        if half <= 0.0 {
            return;
        }
        let flags = if paint.anti_alias { FLAG_AA } else { 0 };
        let (lx, rx) = (x0.min(x1), x0.max(x1));
        let (ty, by) = (y0.min(y1), y0.max(y1));
        let m = half + aa_margin;
        self.instances.push(Instance {
            bbox: [lx - m, ty - m, rx - lx + 2.0 * m, by - ty + 2.0 * m],
            rect: [0.0; 4],
            line: [x0, y0, x1, y1],
            clip,
            color: color_f32(paint.color),
            grad: [0.0; 4],
            params: [0.0, half, 0.0, 0.0],
            meta: [KIND_LINE, flags, GRAD_NONE, 0],
        });
    }

    /// 高斯模糊圆角矩形投影。`sigma` 是**逻辑**长度（顶点着色器会乘 scale 还原成
    /// 物理 σ），`margin` 是模糊的逻辑外扩量。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn push_shadow(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        sigma: f32,
        margin: f32,
        color: Color,
        clip: [f32; 4],
    ) {
        self.instances.push(Instance {
            bbox: inflate(x, y, w, h, margin),
            rect: [x, y, w, h],
            line: [0.0; 4],
            clip,
            color: color_f32(color),
            grad: [0.0; 4],
            params: [radius, sigma, 0.0, 0.0],
            meta: [KIND_SHADOW, FLAG_AA, GRAD_NONE, 0],
        });
    }
}

/// 一组已登记渐变的引用。
struct GradRef {
    base: u32,
    count: u32,
    kind: u32,
    geom: [f32; 4],
}

/// 矩形四边外扩 `m`。
fn inflate(x: f32, y: f32, w: f32, h: f32, m: f32) -> [f32; 4] {
    [x - m, y - m, w + 2.0 * m, h + 2.0 * m]
}

static GRAD_OVERFLOW: std::sync::Once = std::sync::Once::new();
static GRAD_TOO_MANY_STOPS: std::sync::Once = std::sync::Once::new();

/// 进程内只提示一次：这类降级是「画出来还对，只是不够好」，每帧刷屏反而淹没真问题。
fn warn_once(once: &std::sync::Once, msg: &str) {
    once.call_once(|| eprintln!("{msg}"));
}

/// 管线与跨帧复用的 GPU 资源。挂在渲染目标上（而不是进程级单例）：它绑死了附着格式，
/// 而目标才知道自己的格式；跟着目标一起析构也省掉一份「设备释放后管线还活着」的悬空态。
///
/// 文字管线与 run-cache（P2）同样绑死格式、同样要跨帧复用，故一并挂在这里——懒建，
/// 一帧文字都没有的目标（几何测试、纯图形界面）不付那次管线编译。
pub(super) struct PrimRenderer {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    globals: wgpu::Buffer,
    grads: wgpu::Buffer,
    instances: wgpu::Buffer,
    /// 实例缓冲当前能放几个实例。
    capacity: usize,
    /// 本帧已占用的实例槽数（帧内游标）。
    ///
    /// 一帧只提交一次之后，同一个缓冲要装下帧内**所有**批次：几何与文字交错时
    /// 一帧有上百批，各批若都写偏移 0，后写的会在前一批执行前把数据覆盖掉
    /// （`queue.write_buffer` 相对**提交**定序，不是相对录制定序——这正是 P2 记下
    /// 的那条坑）。故各批依次往后排，帧末由 [`Self::end_frame`] 归零。
    used: usize,
    /// 附着格式。文字/图片管线懒建时要用。
    format: wgpu::TextureFormat,
    /// 文字管线 + run-cache（懒建）。
    text: Option<TextRenderer>,
    /// 图片管线 + 纹理缓存 + 层纹理池（懒建，同上）。
    image: Option<ImageRenderer>,
}

impl PrimRenderer {
    pub(super) fn new(gpu: &Arc<SharedGpu>, format: wgpu::TextureFormat) -> Self {
        let device = gpu.device();
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("windui prim shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });
        let globals = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("windui prim globals"),
            size: std::mem::size_of::<Globals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let grads = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("windui prim gradients"),
            size: (GRAD_MAX * GRAD_STRIDE * 16) as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("windui prim bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("windui prim bind group"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: globals.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: grads.as_entire_binding(),
                },
            ],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("windui prim layout"),
            bind_group_layouts: &[Some(&bgl)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("windui prim pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(Instance::layout())],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                // 外包框 quad 的绕向不保证（宽高恒为正，但这里不依赖它），干脆不剔除。
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
                    // 预乘 over：片元已输出预乘颜色。alpha 通道同样按 over 合成，
                    // 半透明背景上叠图元时目标 alpha 才正确累积。
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
            bind_group,
            globals,
            grads,
            instances,
            capacity: INIT_CAPACITY,
            used: 0,
            format,
            text: None,
            image: None,
        }
    }

    /// 帧末收尾（三条管线一起）：归零帧内实例游标、收缩层纹理池。
    ///
    /// **必须在本帧的提交之后调用**——游标归零意味着下一帧会从缓冲头部重新写，而
    /// 未提交的批次还指着那些字节。
    pub(super) fn end_frame(&mut self) {
        self.used = 0;
        if let Some(t) = self.text.as_mut() {
            t.end_frame();
        }
        if let Some(i) = self.image.as_mut() {
            i.end_frame();
        }
    }

    /// 文字渲染器（首次用到时建）。
    pub(super) fn text(&mut self, gpu: &Arc<SharedGpu>) -> &mut TextRenderer {
        let format = self.format;
        self.text
            .get_or_insert_with(|| TextRenderer::new(gpu, format))
    }

    /// 图片渲染器（首次用到时建）。层纹理池也在它里面，理由见 `tex.rs`。
    pub(super) fn image(&mut self, gpu: &Arc<SharedGpu>) -> &mut ImageRenderer {
        let format = self.format;
        self.image
            .get_or_insert_with(|| ImageRenderer::new(gpu, format))
    }

    /// 把 `batch` 里攒的图元编码成一个 render pass 并提交，随后清空 batch。
    /// `LoadOp::Load`：目标已有内容（清屏或上一批）必须保留——painter's algorithm。
    pub(super) fn flush(
        &mut self,
        gpu: &Arc<SharedGpu>,
        view: &wgpu::TextureView,
        size: (u32, u32),
        scale: f32,
        scissor: Option<[u32; 4]>,
        encoder: &mut wgpu::CommandEncoder,
        batch: &mut PrimBatch,
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
                scale,
                _pad: 0.0,
            }),
        );
        // 渐变表**增量写**：它在帧内只增不减，而一帧要提交上百批。每批重写全表是
        // 每帧几 MB 的白搬运；只写新增的那一段则总量恒等于表本身。已写过的前缀不再
        // 触碰，故先录的批次读到的仍是它登记时的那几组色标。
        if batch.grads.len() > batch.written {
            let from = batch.written;
            queue.write_buffer(
                &self.grads,
                (from * 16) as u64,
                bytemuck::cast_slice(&batch.grads[from..]),
            );
            batch.written = batch.grads.len();
        }
        let n = batch.instances.len();
        if self.used + n > self.capacity {
            // 容量不足时翻倍（而不是恰好扩到需要的大小）：滚动/动画帧的图元数会在一个
            // 区间里反复抖动，恰好扩会变成每帧重建缓冲。
            //
            // 帧中途换缓冲是安全的：本帧已录制的 pass 各自持着旧缓冲的引用（wgpu 内部
            // 计数），换掉字段不会让它们悬空。新缓冲从头开始排，故游标一并归零。
            let mut cap = self.capacity.max(1) * 2;
            while cap < n {
                cap *= 2;
            }
            self.instances = new_instance_buffer(device, cap);
            self.capacity = cap;
            self.used = 0;
        }
        let stride = std::mem::size_of::<Instance>() as u64;
        let off = self.used as u64 * stride;
        queue.write_buffer(&self.instances, off, bytemuck::cast_slice(&batch.instances));

        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("windui prim pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
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
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.set_vertex_buffer(0, self.instances.slice(off..off + n as u64 * stride));
            pass.draw(0..6, 0..n as u32);
        }
        self.used += n;
        batch.clear();
    }
}

fn new_instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("windui prim instances"),
        size: (capacity * std::mem::size_of::<Instance>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::Gradient;

    fn clip() -> [f32; 4] {
        [0.0, 0.0, 100.0, 100.0]
    }

    /// 实例布局的字节数必须与 shader 的属性偏移对得上：改了字段忘了改 shader 的话，
    /// 顶点属性会整体错位，症状是「图元位置全乱」而不是编译错误。
    #[test]
    fn instance_is_128_bytes() {
        assert_eq!(std::mem::size_of::<Instance>(), 128);
        assert_eq!(std::mem::align_of::<Instance>(), 4);
    }

    /// kind 编号是 Rust 与 WGSL 之间的口头协议（两边各写一份常量），编号漂移不会
    /// 报错、只会把图元画成另一种形状。
    #[test]
    fn kind_constants_match_shader() {
        assert_eq!(
            (KIND_RECT, KIND_CIRCLE, KIND_STROKE, KIND_LINE, KIND_SHADOW),
            (0, 1, 2, 3, 4)
        );
        let src = include_str!("shader.wgsl");
        for (name, v) in [
            ("KIND_RECT", KIND_RECT),
            ("KIND_CIRCLE", KIND_CIRCLE),
            ("KIND_STROKE", KIND_STROKE),
            ("KIND_LINE", KIND_LINE),
            ("KIND_SHADOW", KIND_SHADOW),
            ("FLAG_AA", FLAG_AA),
            ("GRAD_LINEAR", GRAD_LINEAR),
            ("GRAD_RADIAL", GRAD_RADIAL),
        ] {
            let want = format!("const {name}: u32 = {v}u;");
            assert!(src.contains(&want), "shader.wgsl 缺少或不匹配：{want}");
        }
        assert!(src.contains(&format!("const GRAD_STRIDE: u32 = {GRAD_STRIDE}u;")));
        assert!(src.contains(&format!("array<vec4<f32>, {}>", GRAD_MAX * GRAD_STRIDE)));
    }

    /// 零/负尺寸不产生实例（与 `rounded_rect_path`、`PathBuilder::from_circle`
    /// 返回 None 的行为同步）。
    #[test]
    fn degenerate_geometry_is_skipped() {
        let mut b = PrimBatch::default();
        let p = Paint::fill(Color::rgb(255, 0, 0));
        b.push_round_rect(0.0, 0.0, 0.0, 10.0, 0.0, &p, clip(), 1.0);
        b.push_round_rect(0.0, 0.0, 10.0, -1.0, 0.0, &p, clip(), 1.0);
        b.push_circle(5.0, 5.0, 0.0, &p, clip(), 1.0);
        b.push_line(0.0, 0.0, 10.0, 10.0, 0.0, &p, clip(), 1.0);
        b.push_stroke(0.0, 0.0, 10.0, 10.0, 0.0, 0.0, &p, clip(), 1.0);
        assert!(b.is_empty(), "退化几何不应产生实例");
    }

    /// 圆角半径 clamp 到 min(w/2,h/2)——与 `rounded_rect_path` 同一条 clamp。
    #[test]
    fn radius_is_clamped_to_half_extent() {
        let mut b = PrimBatch::default();
        b.push_round_rect(
            0.0,
            0.0,
            40.0,
            20.0,
            100.0,
            &Paint::fill(Color::rgb(0, 0, 0)),
            clip(),
            1.0,
        );
        assert_eq!(b.instances[0].params[0], 10.0);
    }

    /// 色标不足 2 时退纯色（与 `sk_shader` 的 `sk_stops.len() < 2` 同判据）。
    #[test]
    fn single_stop_gradient_falls_back_to_solid() {
        let mut b = PrimBatch::default();
        let g = Gradient::linear((0.0, 0.0), (1.0, 0.0), vec![(0.0, Color::rgb(255, 0, 0))]);
        b.push_round_rect(0.0, 0.0, 10.0, 10.0, 0.0, &Paint::gradient(g), clip(), 1.0);
        assert_eq!(b.instances[0].meta[2], GRAD_NONE);
        assert!(b.grads.is_empty());
    }

    /// 渐变几何按 rect 映射：归一化 (0,0.5)→(1,0.5) 落在 rect 的左右中点。
    #[test]
    fn linear_gradient_maps_to_rect() {
        let mut b = PrimBatch::default();
        let g = Gradient::linear(
            (0.0, 0.5),
            (1.0, 0.5),
            vec![(0.0, Color::rgb(0, 0, 255)), (1.0, Color::rgb(255, 0, 0))],
        );
        b.push_round_rect(
            20.0,
            30.0,
            100.0,
            40.0,
            0.0,
            &Paint::gradient(g),
            clip(),
            1.0,
        );
        let i = &b.instances[0];
        assert_eq!(i.grad, [20.0, 50.0, 120.0, 50.0]);
        assert_eq!(i.meta[1] & GRAD_LINEAR, GRAD_LINEAR);
        assert_eq!(i.meta[3], 2);
        assert_eq!(b.grads.len(), GRAD_STRIDE);
    }

    /// 径向半径以**短边**为基准（保持圆形），与 `sk_shader` 一致。
    #[test]
    fn radial_gradient_radius_uses_shorter_side() {
        let mut b = PrimBatch::default();
        let g = Gradient::radial(
            (0.5, 0.5),
            1.0,
            vec![(0.0, Color::rgb(255, 255, 255)), (1.0, Color::rgb(0, 0, 0))],
        );
        b.push_round_rect(0.0, 0.0, 100.0, 40.0, 0.0, &Paint::gradient(g), clip(), 1.0);
        let i = &b.instances[0];
        assert_eq!(i.grad[0], 50.0);
        assert_eq!(i.grad[1], 20.0);
        assert_eq!(i.grad[2], 40.0);
        assert_eq!(i.meta[1] & GRAD_RADIAL, GRAD_RADIAL);
    }

    /// 乱序 offset 会让 shader 的插值循环取错区间，登记时须修成单调。
    #[test]
    fn gradient_offsets_are_made_monotonic() {
        let mut b = PrimBatch::default();
        let g = Gradient::linear(
            (0.0, 0.0),
            (1.0, 0.0),
            vec![
                (0.6, Color::rgb(255, 0, 0)),
                (0.2, Color::rgb(0, 255, 0)),
                (1.0, Color::rgb(0, 0, 255)),
            ],
        );
        b.push_round_rect(0.0, 0.0, 10.0, 10.0, 0.0, &Paint::gradient(g), clip(), 1.0);
        let packed = b.grads[GRAD_STRIDE - 2];
        assert!(packed[0] <= packed[1] && packed[1] <= packed[2]);
        assert_eq!(packed[0], 0.6);
        assert_eq!(packed[1], 0.6, "递减的 offset 应被抬平而非留成倒序");
    }

    /// 渐变表满了退纯色而不是画错：uniform 数组容量固定，越界读到的是别人的色标。
    #[test]
    fn gradient_table_overflow_falls_back_to_solid() {
        let mut b = PrimBatch::default();
        let g = Gradient::linear(
            (0.0, 0.0),
            (1.0, 0.0),
            vec![(0.0, Color::rgb(0, 0, 255)), (1.0, Color::rgb(255, 0, 0))],
        );
        for _ in 0..GRAD_MAX + 4 {
            b.push_round_rect(
                0.0,
                0.0,
                10.0,
                10.0,
                0.0,
                &Paint::gradient(g.clone()),
                clip(),
                1.0,
            );
        }
        assert_eq!(b.grads.len(), GRAD_MAX * GRAD_STRIDE);
        assert_eq!(b.instances[GRAD_MAX].meta[2], GRAD_NONE);
        assert!(
            b.grads.len() * 16 <= 16 * 1024,
            "渐变表须放得进 16 KiB uniform"
        );
    }

    /// 阴影 quad 必须把模糊外扩算进外包框——否则模糊尾部被 quad 边界切掉，
    /// 就是软后端 margin 留少了那道直角硬边的 GPU 版（skia.rs:300 的教训）。
    #[test]
    fn shadow_bbox_includes_blur_margin() {
        let mut b = PrimBatch::default();
        b.push_shadow(
            40.0,
            40.0,
            40.0,
            40.0,
            8.0,
            10.0,
            32.0,
            Color::rgba(0, 0, 0, 180),
            clip(),
        );
        let i = &b.instances[0];
        assert_eq!(i.bbox, [8.0, 8.0, 104.0, 104.0]);
        assert_eq!(i.meta[0], KIND_SHADOW);
    }

    /// 线段外包框含半线宽与抗锯齿余量，且对反向端点同样成立。
    #[test]
    fn line_bbox_covers_both_directions() {
        let mut b = PrimBatch::default();
        let p = Paint::fill(Color::rgb(0, 0, 0));
        b.push_line(80.0, 10.0, 20.0, 50.0, 2.0, &p, clip(), 1.0);
        let i = &b.instances[0];
        assert_eq!(i.bbox, [17.0, 7.0, 66.0, 46.0]);
    }

    /// anti_alias=false 不置 AA 位（片元据此走像素中心采样、无过渡带）。
    #[test]
    fn anti_alias_flag_follows_paint() {
        let mut b = PrimBatch::default();
        let mut p = Paint::fill(Color::rgb(0, 0, 0));
        p.anti_alias = false;
        b.push_round_rect(0.0, 0.0, 10.0, 10.0, 0.0, &p, clip(), 1.0);
        p.anti_alias = true;
        b.push_round_rect(0.0, 0.0, 10.0, 10.0, 0.0, &p, clip(), 1.0);
        assert_eq!(b.instances[0].meta[1] & FLAG_AA, 0);
        assert_eq!(b.instances[1].meta[1] & FLAG_AA, FLAG_AA);
    }
}
