// windui GPU 图元 shader：**一条管线 + 一次 instanced draw** 覆盖全部几何图元。
//
// 为什么能只用一条管线：`Canvas` 的图元集封闭（无任意 path、无纹理、无变换），
// 圆角矩形/圆/描边/线段/高斯阴影全部有解析 SDF 表达，渐变也能在同一片元里插值。
// 于是整帧没有任何状态切换——没有 scissor 切换（裁剪矩形是实例字段）、没有绑定切换，
// 所有实例可以合成一次 draw call。
//
// 坐标约定：**SDF 求值全程在物理像素空间**。顶点着色器把逻辑几何 × scale（对标 d2d 的
// SetTransform）后以 flat 插值交给片元，片元用 `@builtin(position)`（像素中心 = 整数+0.5）
// 直接比距离，于是抗锯齿过渡宽度恒为 1 个物理像素，与软后端在物理 pixmap 上光栅的
// 结果同源。裁剪矩形是唯一例外：它在 CPU 侧就已经取整成物理整数（软后端的裁剪 mask
// 同样是非抗锯齿的整数矩形），再乘一次 scale 会把取整结果搞坏。
//
// 颜色约定：实例里存的是**非预乘** sRGB 字节除以 255 的值。渐变按非预乘插值（tiny-skia
// 的渐变默认也在非预乘空间插值），片元最后一步才乘 alpha 输出**预乘**颜色，配合
// `ONE / ONE_MINUS_SRC_ALPHA` 的混合状态。全程不做 sRGB↔线性转换：附着是
// `Rgba8Unorm`，混合发生在 sRGB 字节空间，与 tiny-skia 一致——这是逐像素比对的前提。

// ---- 图元类型（与 prim.rs 的 KIND_* 常量一一对应，改动须同步）----
const KIND_RECT: u32 = 0u; // 圆角矩形填充（radius=0 即直角）
const KIND_CIRCLE: u32 = 1u;
const KIND_STROKE: u32 = 2u; // 圆角矩形描边
const KIND_LINE: u32 = 3u; // 线段（Butt 端帽）
const KIND_SHADOW: u32 = 4u; // 高斯模糊圆角矩形投影

// ---- flags 位域 ----
const FLAG_AA: u32 = 1u; // bit0：抗锯齿
const GRAD_MASK: u32 = 6u; // bit1..2：渐变类型
const GRAD_LINEAR: u32 = 2u;
const GRAD_RADIAL: u32 = 4u;

/// 渐变表里每组渐变占的 vec4 数：前 8 个是非预乘 RGBA 色标，后 2 个把 8 个 offset
/// 打包进 x/y/z/w。与 prim.rs 的 `GRAD_STRIDE` 同步。
const GRAD_STRIDE: u32 = 10u;
/// 「本实例无渐变」的哨兵基址。
const GRAD_NONE: u32 = 0xFFFFFFFFu;

const SQRT_1_2: f32 = 0.70710678;
const INV_SQRT_2PI: f32 = 0.3989423;

struct Globals {
    /// 渲染目标物理尺寸（像素）。
    viewport: vec2<f32>,
    /// 逻辑→物理缩放（DPI/96）。
    scale: f32,
    _pad: f32,
};

/// 渐变色标表。用 uniform 数组而不是 storage buffer：设备按
/// `Limits::downlevel_defaults()` 建（GLES 3.0 / D3D11 档），片元阶段的只读 storage
/// buffer 在那一档上不保证可用，uniform 数组则是核心能力。
struct GradTable {
    data: array<vec4<f32>, 640>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var<uniform> grads: GradTable;

/// 每图元一个实例。字段含义随 `info.x`（kind）而变，注释见 prim.rs 的 `Instance`。
struct Inst {
    /// quad 外包框（**逻辑**坐标 x,y,w,h）：由 CPU 侧按图元类型算好，阴影已含模糊外扩。
    @location(0) bbox: vec4<f32>,
    /// 图元矩形（逻辑 x,y,w,h）。circle 为外接正方形，stroke 为**描边中心线**矩形。
    @location(1) rect: vec4<f32>,
    /// 线段端点（逻辑 x0,y0,x1,y1）。
    @location(2) line: vec4<f32>,
    /// 裁剪矩形（**物理整数** x0,y0,x1,y1）。
    @location(3) clip: vec4<f32>,
    /// 非预乘 RGBA（0..1）。有渐变时作为回退色。
    @location(4) color: vec4<f32>,
    /// 渐变几何（逻辑）：linear=(p0.xy, p1.xy)；radial=(center.xy, radius, 0)。
    @location(5) grad: vec4<f32>,
    /// x=圆角/圆半径，y=描边半宽 或 阴影 σ（均为逻辑长度），z/w 保留。
    @location(6) params: vec4<f32>,
    /// x=kind，y=flags，z=渐变基址（GRAD_NONE 表示无），w=色标数。
    @location(7) info: vec4<u32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    /// 以下均为**物理**像素空间（顶点着色器已乘 scale），逐实例常量故 flat 插值。
    @location(0) @interpolate(flat) rect: vec4<f32>,
    @location(1) @interpolate(flat) line: vec4<f32>,
    @location(2) @interpolate(flat) clip: vec4<f32>,
    @location(3) @interpolate(flat) color: vec4<f32>,
    @location(4) @interpolate(flat) grad: vec4<f32>,
    @location(5) @interpolate(flat) params: vec4<f32>,
    @location(6) @interpolate(flat) info: vec4<u32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Inst) -> VsOut {
    // 两个三角形展开外包框；无索引缓冲，六个顶点直接摊开。
    var quad = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let s = globals.scale;
    let bbox = inst.bbox * s;
    let p = bbox.xy + quad[vi] * bbox.zw;

    var out: VsOut;
    // 物理像素 → NDC。y 翻转：窗口坐标向下为正，NDC 向上为正。
    out.pos = vec4<f32>(
        p.x / globals.viewport.x * 2.0 - 1.0,
        1.0 - p.y / globals.viewport.y * 2.0,
        0.0,
        1.0,
    );
    out.rect = inst.rect * s;
    out.line = inst.line * s;
    // clip 已是物理整数，不再乘（见文件头的坐标约定）。
    out.clip = inst.clip;
    out.color = inst.color;
    // grad 四个分量对两种渐变都是长度量（坐标或半径），统一 × scale。
    out.grad = inst.grad * s;
    out.params = inst.params * s;
    out.info = inst.info;
    return out;
}

/// 圆角矩形有向距离场。`b` 为半宽高，`r` 为圆角半径（调用方保证 r ≤ min(b.x,b.y)）。
fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

/// 线段（Butt 端帽）有向距离场：等价于以线段为中轴、宽 `2*half_w`、长 `|b-a|` 的
/// 旋转矩形——端帽不外延，与软后端 `LineCap::Butt` 的描边几何一致（skia.rs:262）。
fn sd_segment_box(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>, half_w: f32) -> f32 {
    let ba = b - a;
    let len = length(ba);
    if len < 1e-6 {
        // 零长线段在软后端同样画不出东西（Butt 端帽没有面积）。
        return 1e9;
    }
    let dir = ba / len;
    let q = p - (a + b) * 0.5;
    // 转到线段局部坐标：x 沿线段方向、y 垂直于线段。
    let local = vec2<f32>(dot(q, dir), dot(q, vec2<f32>(-dir.y, dir.x)));
    let d = abs(local) - vec2<f32>(len * 0.5, half_w);
    return length(max(d, vec2<f32>(0.0))) + min(max(d.x, d.y), 0.0);
}

/// erf 的多项式逼近（Abramowitz & Stegun 7.1.27，最大误差 ~2.5e-4）。
/// 两分量一起算，正好对上下面「积分上限 − 积分下限」的用法。
fn erf_approx(x: vec2<f32>) -> vec2<f32> {
    let s = sign(x);
    let a = abs(x);
    var t = 1.0 + (0.278393 + (0.230389 + 0.078108 * a * a) * a) * a;
    t = t * t; // ^2
    t = t * t; // ^4
    return s - s / t;
}

fn gaussian(x: f32, sigma: f32) -> f32 {
    return exp(-x * x / (2.0 * sigma * sigma)) * INV_SQRT_2PI / sigma;
}

/// 固定 y 上，圆角矩形被高斯在 x 方向模糊后的覆盖（Evan Wallace 的解析近似）：
/// 该行的矩形半宽随圆角收窄，模糊后即一维高斯积分之差。
fn shadow_row(x: f32, y: f32, sigma: f32, corner: f32, half_size: vec2<f32>) -> f32 {
    let delta = min(half_size.y - corner - abs(y), 0.0);
    let curved = half_size.x - corner + sqrt(max(0.0, corner * corner - delta * delta));
    let integral = 0.5 + 0.5 * erf_approx((vec2<f32>(x, x) + vec2<f32>(-curved, curved)) * (SQRT_1_2 / sigma));
    return integral.y - integral.x;
}

/// 高斯模糊圆角矩形的覆盖度：x 方向解析（上式），y 方向在 ±3σ ∩ 矩形范围内数值积分。
/// 比软后端「离屏烘焙 + 3 趟 box-blur」少了一张离屏位图和三趟全图卷积，也就不需要
/// 阴影缓存——每帧现算，且没有 pixmap 边界截断这个失效模式（skia.rs:300 的教训）。
fn shadow_coverage(p: vec2<f32>, rect: vec4<f32>, sigma: f32, corner: f32) -> f32 {
    let half_size = rect.zw * 0.5;
    let center = rect.xy + half_size;
    let q = p - center;
    let low = q.y - half_size.y;
    let high = q.y + half_size.y;
    let start = clamp(-3.0 * sigma, low, high);
    let end = clamp(3.0 * sigma, low, high);
    let dy = (end - start) / 8.0;
    var y = start + dy * 0.5;
    var value = 0.0;
    // 8 个采样点（原始公式用 4）：采样太疏时远场会出现台阶，而软后端那条判据
    // 恰恰要求暗度向外**单调**递减。
    for (var i = 0; i < 8; i = i + 1) {
        value = value + shadow_row(q.x, q.y - y, sigma, corner, half_size) * gaussian(y, sigma) * dy;
        y = y + dy;
    }
    return value;
}

/// 按 offset 表在色标间线性插值。首尾之外钳到端点色 —— 对齐软后端的
/// `SpreadMode::Pad`（skia.rs 的 `sk_shader`）。
fn sample_gradient(base: u32, count: u32, t: f32) -> vec4<f32> {
    let o0 = grads.data[base + 8u];
    let o1 = grads.data[base + 9u];
    var offs = array<f32, 8>(o0.x, o0.y, o0.z, o0.w, o1.x, o1.y, o1.z, o1.w);
    if t <= offs[0] {
        return grads.data[base];
    }
    let last = count - 1u;
    if t >= offs[last] {
        return grads.data[base + last];
    }
    for (var i: u32 = 1u; i < count; i = i + 1u) {
        let a = offs[i - 1u];
        let b = offs[i];
        if t <= b {
            var f = 0.0;
            if b > a {
                f = (t - a) / (b - a);
            }
            return mix(grads.data[base + i - 1u], grads.data[base + i], f);
        }
    }
    return grads.data[base + last];
}

/// 本片元的非预乘基色：纯色，或按归一化参数 t 取的渐变色。
fn base_color(in: VsOut, p: vec2<f32>) -> vec4<f32> {
    let gk = in.info.y & GRAD_MASK;
    if gk == 0u || in.info.z == GRAD_NONE {
        return in.color;
    }
    var t = 0.0;
    if gk == GRAD_LINEAR {
        // 投影到 p0→p1 轴上的归一化位置。
        let d = in.grad.zw - in.grad.xy;
        let dd = dot(d, d);
        if dd > 1e-9 {
            t = dot(p - in.grad.xy, d) / dd;
        }
    } else {
        t = length(p - in.grad.xy) / max(in.grad.z, 1e-6);
    }
    return sample_gradient(in.info.z, in.info.w, t);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let p = in.pos.xy;
    // 裁剪：逐像素比整数物理矩形。clip 的四条边都是整数，像素中心为 n+0.5，故
    // 「中心落在 [x0,x1] 内」恰好选中 [x0, x1) 这些列——与软后端非抗锯齿的
    // 矩形 mask 逐像素同集。用系数而不是 discard：分支在这里没有性能意义，
    // 而 discard 会让后续代码路径多一种要考虑的终止方式。
    let inside = f32(p.x >= in.clip.x && p.x <= in.clip.z && p.y >= in.clip.y && p.y <= in.clip.w);

    let kind = in.info.x;
    var cov = 0.0;
    if kind == KIND_SHADOW {
        cov = clamp(shadow_coverage(p, in.rect, in.params.y, in.params.x), 0.0, 1.0);
    } else {
        var d = 0.0;
        if kind == KIND_CIRCLE {
            d = length(p - (in.rect.xy + in.rect.zw * 0.5)) - in.params.x;
        } else if kind == KIND_STROKE {
            // 描边 = 到中心线的距离落在 ±半宽内。
            let half_size = in.rect.zw * 0.5;
            d = abs(sd_round_box(p - (in.rect.xy + half_size), half_size, in.params.x)) - in.params.y;
        } else if kind == KIND_LINE {
            d = sd_segment_box(p, in.line.xy, in.line.zw, in.params.y);
        } else {
            let half_size = in.rect.zw * 0.5;
            d = sd_round_box(p - (in.rect.xy + half_size), half_size, in.params.x);
        }
        if (in.info.y & FLAG_AA) != 0u {
            // 1 物理像素宽的线性斜坡 ≈ 解析面积覆盖，与 tiny-skia 的 AA 同量级。
            cov = clamp(0.5 - d, 0.0, 1.0);
        } else {
            // 关掉抗锯齿即像素中心采样：边界上非 0 即 1，无过渡带。
            cov = select(0.0, 1.0, d <= 0.0);
        }
    }

    let base = base_color(in, p);
    let a = base.a * cov * inside;
    // 输出预乘颜色，配合 ONE / ONE_MINUS_SRC_ALPHA。
    return vec4<f32>(base.rgb * a, a);
}
