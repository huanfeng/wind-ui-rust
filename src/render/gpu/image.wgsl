// windui GPU 图片 shader：**预乘 RGBA 纹理 × opacity × 圆角遮罩**，一张图一个四边形实例。
//
// 与文字管线（`text.wgsl`）分开的理由：文字采的是 R8 覆盖度、颜色来自实例，这里采的是
// 完整的预乘 RGBA、实例只给一个 opacity 标量；而且这里要一个**圆角矩形遮罩**（软后端
// 的 `draw_image` 是先建圆角 mask 再 blit），文字那条没有。合成语义两边一致：输出预乘
// 颜色，配 `ONE / ONE_MINUS_SRC_ALPHA`，全程不做 sRGB↔线性转换。
//
// 离屏层的合成（`layer.rs` 的 `pop_layer`）复用同一条管线：一张整目标大小的层纹理、
// 遮罩取整个目标、圆角 0、opacity 取层的 opacity。层纹理与目标同尺寸 1:1，故走 nearest。
//
// 坐标约定同 `text.wgsl`：实例坐标已是**物理像素**，顶点着色器不再乘 scale。图片的
// fit 缩放、1:1 吸附、落点取整全在 CPU 侧算完（逐条对着 `SkiaCanvas::draw_image` 抄），
// shader 只负责「把这块纹理铺到这个四边形上」。
//
// 采样规则：`params.z` 为 1 时用 nearest，否则 linear。**物理尺寸与源图 1:1（含差半
// 像素内的吸附）走 nearest**——软后端在这种情形下的双线性退化为纯 blit，而 GPU 上
// linear 的浮点误差会在图标细描边上留下半像素的糊边。两条 `textureSampleLevel` 显式
// 给 LOD，故允许出现在非一致控制流里（`textureSample` 要求隐式求导，会报 uniformity）。

struct Globals {
    /// 渲染目标物理尺寸（像素）。
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
// 每张图（或每张层纹理）一组绑定。两个采样器共用一份布局，选哪个由实例的 flag 决定。
@group(1) @binding(0) var img: texture_2d<f32>;
@group(1) @binding(1) var samp_near: sampler;
@group(1) @binding(2) var samp_lin: sampler;

struct Inst {
    /// 图片四边形（**物理**像素 x,y,w,h）。纹理在其上 1:1 铺满（uv 从 0 到 1）。
    @location(0) quad: vec4<f32>,
    /// 圆角遮罩矩形（物理 x,y,w,h）= 软后端的 dst 框。Cover/None 的溢出由它裁掉。
    @location(1) mask: vec4<f32>,
    /// 裁剪矩形（物理整数 x0,y0,x1,y1）。与几何/文字管线同一份语义。
    @location(2) clip: vec4<f32>,
    /// x=圆角半径（物理），y=opacity，z=1 走 nearest / 0 走 linear，w 保留。
    @location(3) params: vec4<f32>,
};

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) @interpolate(flat) mask: vec4<f32>,
    @location(2) @interpolate(flat) clip: vec4<f32>,
    @location(3) @interpolate(flat) params: vec4<f32>,
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
    // 物理像素 → NDC，y 翻转（与另两条管线同一条换算）。
    out.pos = vec4<f32>(
        p.x / globals.viewport.x * 2.0 - 1.0,
        1.0 - p.y / globals.viewport.y * 2.0,
        0.0,
        1.0,
    );
    out.uv = q;
    out.mask = inst.mask;
    out.clip = inst.clip;
    out.params = inst.params;
    return out;
}

/// 圆角矩形有向距离场。与 `shader.wgsl` 的同名函数逐字相同（WGSL 没有 include；
/// 两处都改才对得上，这也是「圆角裁角」判据在两条管线上都要跑一遍的原因）。
fn sd_round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r);
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let p = in.pos.xy;
    // 裁剪判据与另两条管线逐字一致（含闭区间的取法）。
    let inside = f32(p.x >= in.clip.x && p.x <= in.clip.z && p.y >= in.clip.y && p.y <= in.clip.w);
    // 圆角遮罩：软后端是 `mask.fill_path(圆角路径, 抗锯齿=true)` 再 blit，这里是同一个
    // 形状的 SDF + 1 物理像素的线性斜坡（与 `shader.wgsl` 的 AA 取法同源）。
    let half_size = in.mask.zw * 0.5;
    let d = sd_round_box(p - (in.mask.xy + half_size), half_size, in.params.x);
    let cov = clamp(0.5 - d, 0.0, 1.0);

    var texel: vec4<f32>;
    if in.params.z > 0.5 {
        texel = textureSampleLevel(img, samp_near, in.uv, 0.0);
    } else {
        texel = textureSampleLevel(img, samp_lin, in.uv, 0.0);
    }
    // 纹素已是预乘的（上传时就与 tiny-skia 同约定），四个通道同乘一个标量仍是预乘。
    return texel * (in.params.y * cov * inside);
}
