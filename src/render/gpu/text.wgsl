// windui GPU 文字 shader：**R8 覆盖度纹理 × 文字颜色**，一条 run 一个四边形实例。
//
// 与几何管线（`shader.wgsl`）分开的理由只有一条：绑定布局不同——这里要一张纹理 + 一个
// 采样器，而几何那条没有任何纹理绑定。合成语义两边完全一样：输出**预乘**颜色，配
// `ONE / ONE_MINUS_SRC_ALPHA`，全程不做 sRGB↔线性转换（附着是 `*Unorm`，混合发生在
// sRGB 字节空间，与 tiny-skia 同源）。
//
// 坐标约定与几何管线**相反**：这里的实例坐标已经是**物理像素**，顶点着色器不再乘 scale。
// 因为字形位图本身是平台引擎按物理字号光栅出来的，它的尺寸只有物理含义；放置也就必须在
// 物理空间算完（含贴整数像素那一步，见 canvas.rs）。逻辑坐标在这条路上没有落脚点。
//
// 采样是 **nearest + 1:1**：位图一个纹素对目标一个物理像素。线性过滤在这里只会把平台
// 光栅好的字形边缘再糊一层，抗锯齿本就已经在覆盖度里了。
//
// 实例带 uv 子矩形，于是同一条管线同时服务两种粒度：整段 run（各自一张纹理、uv 取满）
// 与 glyph atlas（共享一张纹理、uv 取自己那一格）。差别只在绑定的是哪张纹理与 uv 取多大,
// 采样与合成逐字相同。

struct Globals {
    /// 渲染目标物理尺寸（像素）。
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
// 一组绑定 = 一张覆盖度纹理 + 共享采样器。整段 run 各绑各的，atlas 则整帧共用一组
// ——后者于是能把上百条文字合成一次 instanced draw（见 text.rs）。
@group(1) @binding(0) var mask: texture_2d<f32>;
@group(1) @binding(1) var samp: sampler;

struct Inst {
    /// 位图四边形（**物理**像素 x,y,w,h）。已含 `AlphaMask::pad` 的外扩。
    @location(0) quad: vec4<f32>,
    /// 纹理上的子矩形（归一化 u0,v0,u1,v1）。整段 run 取 (0,0,1,1)，atlas 取自己那一格。
    @location(1) uv: vec4<f32>,
    /// 裁剪矩形（物理整数 x0,y0,x1,y1）。与几何管线同一份语义。
    @location(2) clip: vec4<f32>,
    /// 文字颜色，**非预乘** RGBA（0..1）。
    @location(3) color: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) clip: vec4<f32>,
    @location(2) @interpolate(flat) color: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32, inst: Inst) -> VsOut {
    var quad = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let q = quad[vi];
    let p = inst.quad.xy + q * inst.quad.zw;

    var out: VsOut;
    // 物理像素 → NDC，y 翻转（与几何管线同一条换算）。
    out.pos = vec4<f32>(
        p.x / globals.viewport.x * 2.0 - 1.0,
        1.0 - p.y / globals.viewport.y * 2.0,
        0.0,
        1.0,
    );
    // 四边形与它那块位图同尺寸（1:1），故 quad 的四角线性映到 uv 子矩形的四角，
    // 插值出来的采样点恰好命中纹素中心。
    out.uv = mix(inst.uv.xy, inst.uv.zw, q);
    out.clip = inst.clip;
    out.color = inst.color;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let p = in.pos.xy;
    // 裁剪判据与几何管线逐字一致（含闭区间的取法），否则同一个滚动视口会把文字和
    // 背景裁在不同的像素列上。
    let inside = f32(p.x >= in.clip.x && p.x <= in.clip.z && p.y >= in.clip.y && p.y <= in.clip.w);
    let cov = textureSample(mask, samp, in.uv).r;
    let a = in.color.a * cov * inside;
    return vec4<f32>(in.color.rgb * a, a);
}
