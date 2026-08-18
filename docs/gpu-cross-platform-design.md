# 跨平台 GPU 渲染后端设计（macOS / Linux）

> 状态：**P0~P3 已落地**（2026-08-18，`src/render/gpu/`，feature `gpu`），macOS 窗口
> 可用 GPU 呈现，Canvas 图元集全部实现；P4 收尾同日完成（CI/文档/基准）；P5 按需。
> 对照文档：`docs/DESIGN.md`（总体架构）、`docs/MACOS_PORTING.md`（macOS 平台层）、
> `src/platform/win32/d2d.rs`（Windows GPU 后端先例）。

## 1. 背景与目标

当前渲染后端矩阵：

| 平台 | 软件光栅 | GPU |
| --- | --- | --- |
| Windows | tiny-skia + DirectWrite（光栅进 pixmap） | **D2D/D3D11**（`d2d` feature，自带 DirectWrite 文字栈） |
| macOS | tiny-skia + Core Text（光栅进 pixmap），CGImage 拷屏呈现 | ❌ 无 |
| Linux（未来） | 平台层未实现 | ❌ 无 |

目标：为 macOS（及未来 Linux）提供 GPU 硬件加速渲染，且：

1. **文字仍与系统匹配**——延续本项目的核心卖点：macOS 用 Core Text 光栅字形、
   Linux 将来用 FreeType/fontconfig，GPU 只负责合成，不引入第三方字形光栅器。
2. **一个机制覆盖 macOS + Linux**——不为每个平台各写一套 GPU 图元实现。
3. **Windows 的 D2D 后端保持不动**——已验证、已发布，不推倒重来。
4. 行为语义与现有后端完全一致：`Renderer::Auto/Software/Gpu` 三档、
   失败静默回退软渲染、`as_pixmap() == None` → 全窗重绘、离屏截图测试可跑。

## 2. 现状契约盘点（新后端必须满足什么）

新后端只需实现两个 trait（`src/render/mod.rs`）：

- **`RenderTarget`**：每帧 `make_canvas(engine, scale)`；`as_pixmap()` 默认
  `None`（GPU 后端沿用 D2D 的做法，调用方自动走全窗重绘，damage 快路只属于软后端）。
- **`Canvas`**：封闭图元集——`fill_rect` / `fill_round_rect` / `stroke_round_rect` /
  `draw_line` / `fill_circle` / `draw_shadow` / `draw_image`（含 fit/圆角/opacity）/
  `draw_text` / `measure_text(_wrapped)` / `text_line_metrics` /
  `push_layer(opacity)`+`pop_layer` / `save`+`restore`+`clip_rect`（仅轴对齐矩形）。
  `Paint` 支持纯色与线性/径向渐变。**没有任意 path、没有变换 API**。

其他既有模式（全部有 D2D 先例，直接照搬）：

- 后端选择：`Renderer` 枚举 + 环境变量强制（D2D 是 `WINDUI_D2D`）+ 创建失败回退。
- 共享设备：`d2d::SharedDevice` 进程级单例，消息循环结束显式释放；多窗口共享
  device、各持 surface/swapchain。
- 缓存挂在 backend 上跨帧复用：文字 layout、图片位图（按 `Image::cache_id()`）、
  渐变画刷、阴影烘焙位图。
- 层/裁剪栈必须帧内平衡（D2D 的 `pushed_clips/pushed_layers` 计数是教训沉淀）。
- 离屏截图测试：`d2d::OffscreenBackend` 先例——GPU 渲到纹理再 readback 校验。

## 3. 方案选型

### 候选

| 方案 | 说明 | 结论 |
| --- | --- | --- |
| A. 各平台原生 GPU API | macOS 手写 Metal，Linux 手写 Vulkan/GL | ❌ 违背"一个机制"目标；图元/atlas/层栈逻辑要写两遍并各自维护 |
| B. **wgpu 共享后端 + 自写图元层** | wgpu 抽象 Metal/Vulkan/GL/D3D12；图元用 SDF shader 自绘；文字/图片光栅上传纹理 | ✅ **推荐** |
| C. 现成 GPU 2D 库（vello / skia-safe / femtovg） | 拿来即用的矢量渲染 | ❌ vello 尚在快速演进且为通用 path 设计（我们不需要）；skia-safe 构建链沉重、与"轻量"定位冲突；均无法复用平台文字栈的系统级排版 |

### 推荐 B 的理由

1. **图元集封闭且小**：`Canvas` 没有任意 path。圆角矩形/圆/线/描边/高斯阴影全部
   有解析 SDF（有向距离场）表达，可用**一个实例化 quad shader** 覆盖，渐变也在同一
   shader 内插值。整个图元渲染器预计 <1500 行 Rust + ~200 行 WGSL，不需要 tessellation。
2. **文字架构可保留**：平台文字引擎已经会"光栅到 CPU 位图"（现在的目标是 pixmap），
   改成"光栅到小位图 → 上传纹理 → GPU 合成"即可，排版/度量/字体回退全部复用。
3. **一次覆盖 macOS + Linux**：wgpu 在 macOS 走 Metal、Linux 走 Vulkan（旧机 GL 回退），
   Linux 平台层将来落地时渲染器零改动。远期 Windows 也可切换（见 §9），但不是本设计目标。
4. 纯 Rust 依赖，无 C++ 构建链；有软件适配器（lavapipe/WARP）可在无 GPU 的 CI 跑测试。

代价（接受）：wgpu 依赖树较大（编译时间 +1~2 分钟、二进制 +2~4 MB）——用
feature `gpu` 隔离，默认是否开启在 Phase 4 依据验证结果决定；关掉时零成本。

### 3.1 生态盘点：哪些现成、哪些自研

wgpu 本身是现成基础库（gfx-rs 维护、Firefox WebGPU 底座），地位等同 Windows
侧的 D3D11——不存在"自己实现 wgpu"。分三层看：

- **GPU 抽象层（现成）**：wgpu（首选）；备选 **blade-graphics**（wgpu 原作者
  kvark 的轻量替代，Zed 生产在用，依赖树小但 API 更底层）。图元层设计对两者
  中立（都是 quad + instance buffer），若 P0 实测 wgpu 体积/编译代价不可接受
  可低成本换底。
- **GPU 2D 渲染器（有整装品，均不合身）**：vello（通用 path 渲染 + 自带文字栈
  期望，超出封闭图元集的需求）；lyon（CPU tessellation，解析形状走 SDF 更优）；
  epaint（与 egui 耦合、自带 atlas 文字栈）；skia-safe/femtovg（C++ 构建链 /
  仅 GL）。——图元层是生态空白，但正因图元集封闭所以自研面积很小。
- **零部件（直接采用）**：**etagere**/guillotiere（WebRender 同源的 atlas 矩形
  分配器，P2/P5 文字与图片 atlas 用）；**bytemuck**（实例缓冲字节转换）。
  glyphon+cosmic-text 是现成"wgpu 文字"组合但光栅走 swash（纯 Rust），与
  "文字匹配系统"目标冲突，仅参考其 atlas 结构。

**生产级先例**：Zed 的 GPUI 框架即"SDF quad shader 图元 + 平台 API
（Core Text/DirectWrite）光栅字形进 atlas"，三平台生产运行多年——本路线的
可行性与文字观感均已被验证；GPUI 是完整 UI 框架无法当渲染库复用，故仍需自研
图元层，但其 shader 规模（数百行）佐证了工作量预估。

## 4. 总体架构

```
                    ┌──────────────────────────────────────────┐
                    │  控件层（不变）── Canvas trait 图元调用     │
                    └───────────────┬──────────────────────────┘
        ┌───────────────┬───────────┴────────────┬─────────────────┐
        │ SkiaCanvas    │ D2DCanvas (win32,不变) │ WgpuCanvas (新)  │
        │ tiny-skia CPU │ D2D/DWrite             │ src/render/gpu/  │
        └───────┬───────┘└───────────────────────┘└───────┬─────────┘
                │                                          │
   TextEngine 光栅进 pixmap                    GlyphSource 光栅进纹理缓存
   (dwrite.rs / coretext.rs 现状)              (同一批平台代码，输出改位图)
                                                           │
                                              wgpu Device/Queue（进程级共享）
                                               ├─ macOS: CAMetalLayer surface
                                               ├─ Linux: Vulkan/GL surface（未来）
                                               └─ 离屏纹理 surface（测试）
```

### 4.1 模块划分（新增 `src/render/gpu/`，feature = `gpu`）

| 文件 | 职责 |
| --- | --- |
| `mod.rs` | `WgpuBackend`（对标 `D2DBackend`）：设备/surface 生命周期、resize、present、设备丢失重建、`WgpuTarget: RenderTarget` |
| `device.rs` | 进程级共享 `wgpu::Device/Queue`（对标 `SharedDevice`），显式释放钩子 |
| `prim.rs` | 图元批处理器：实例缓冲收集 → 按 clip/layer/纹理切换分 draw call |
| `shader.wgsl` | 单一 SDF shader：rounded-rect/circle/line/stroke/shadow/渐变 + 纹理采样（文字 alpha mask、图片 RGBA）两个入口 |
| `text.rs` | 文字纹理缓存（Phase 1 整段 run-cache → Phase 2 glyph atlas） |
| `tex.rs` | 图片/SVG 纹理缓存（键 `Image::cache_id()`，对标 D2D `image_cache`）、atlas 分配器 |
| `layer.rs` | `push_layer/pop_layer`：离屏纹理栈 + opacity 合成 pass |
| `offscreen.rs` | 离屏后端（对标 `d2d::OffscreenBackend`）：渲到纹理 → readback 成 Pixmap，供截图测试 |

### 4.2 图元渲染：实例化 quad + SDF

每个图元编码为一个实例（`~64B`：rect、radius、颜色/渐变索引、类型枚举、
描边宽、blur、clip 矩形索引…），顶点着色器展开为覆盖图元外包框（阴影含 blur
外扩）的 quad，片元着色器按类型求 SDF 距离做抗锯齿覆盖：

- `fill_rect` / `fill_round_rect`：rounded-rect SDF（radius=0 退化直角）。
- `fill_circle`：circle SDF。
- `stroke_round_rect` / `draw_line`：`abs(sdf) - width/2`（线段用 segment SDF）。
- `draw_shadow`：高斯模糊圆角矩形有解析近似（Evan Wallace 公式：erf 逼近的
  一维高斯积分 × rounded-rect），**免烘焙免两趟模糊**——比 D2D 的
  bake+cache 方案更简单，shadow_cache 可以不存在。
- 渐变：Linear/Radial 的归一化坐标（相对 rect）直接在片元内插值，
  stops 上传到一个小 storage buffer，实例引用偏移。语义与
  `render/mod.rs` 注释的归一化契约逐字对齐。
- 抗锯齿：SDF 距离 `smoothstep` 半像素过渡，效果等同 MSAA 但零额外采样。
  `Paint::anti_alias=false` 时过渡宽度置 0。

**批处理**：帧内图元顺序即绘制顺序（painter's algorithm，与现状一致）。连续
同状态（同 clip、同 layer、同纹理页）的实例合并为一个 instanced draw；典型
UI 帧预期 <20 个 draw call。

### 4.3 裁剪与层

- `clip_rect`（仅轴对齐矩形，且只会相交收窄）：物理像素对齐时用 scissor；
  非对齐（scale 为分数）时把当前 clip 矩形作为实例参数在 shader 内再裁一刀，
  保证与软后端逐像素一致。`save/restore` 就是 clip 栈快照，帧末断言归零
  （沿用 D2D 的 `pushed_clips` 教训）。
- `push_layer(opacity)`：从纹理池取一张窗口尺寸的离屏纹理，后续图元渲进去；
  `pop_layer` 时以 opacity 作为整体 alpha 合成回父目标。嵌套即纹理栈。
  纹理池按尺寸复用，避免每帧分配（动画期间 push/pop 高频）。

### 4.4 文字：平台光栅 + GPU 合成（分两期）

保持"文字视觉与系统匹配"的关键：**字形像素永远由平台 API 生成**，GPU 只做
搬运和混合。

**Phase 1 —— 整段 run-cache（快速可用）**：
`draw_text` 把 `(text, family, size, weight, 颜色无关, scale, max_width)` 作键，
未命中时调用平台光栅器把整段文字渲成 **alpha mask 小位图**（Core Text 渲到
透明背景的 CGBitmapContext、灰度抗锯齿——macOS 10.14 起系统本来就不做次像素
AA，视觉一致性有保证），上传纹理缓存；命中直接以文字颜色调制采样绘制。
LRU 上限（如 512 条 / 16MB），窗口 scale 变化时整体失效。
`measure_text` / `line_metrics` 完全复用现有 `TextEngine` 排版代码，不动。

**Phase 2 —— glyph atlas（内存与命中率优化，可选）**：
run-cache 对动态文本（输入框逐字变化、数值刷新）命中率差。第二期把粒度降到
字形：平台引擎输出 `(glyph_id, 位置)` 列表 + 单字形位图入 atlas（guillotine
分配器，R8 格式；emoji/彩色字形单独 RGBA atlas 页）。排版结果（glyph run）按
Phase 1 相同的键缓存，字形位图跨文本共享。**先实现 Phase 1 量化验证效果，
Phase 2 视实际内存/性能数据决定是否做**。

对 `TextEngine` 的改动：新增一个窄接口（暂名 `GlyphSource`），
`raster_run(text, ts, scale) -> AlphaBitmap`（Phase 1）；由
`coretext.rs` 内部复用现有排版代码实现，`dwrite.rs` 暂不需要（Windows 不走此后端）。

**已知视觉偏差与验证**：CG 的 font smoothing（加粗渲染）在"透明背景光栅"与
"真背景直接合成"两条路径下参数不同，可能出现字重±细微差异。验收用现有截图
工具做**墨量（ink coverage）对比**（见 memory：视觉效果要量化验证），
允许阈值内差异，超阈值时调 `CGContextSetShouldSmoothFonts` 校准。

### 4.5 图片 / SVG

`draw_image`：按 `cache_id()` 上传 RGBA 纹理（预乘 alpha，与 tiny-skia 同约定），
fit 缩放用采样矩形实现，圆角裁剪 = 同一 SDF shader 的 rounded-rect 遮罩，
opacity 进实例颜色调制。SVG 已在 CPU 侧按目标尺寸光栅（`from_svg_bytes(target)`
出 HiDPI 位图），GPU 侧无特殊处理。缓存淘汰随 `Image` 生命周期（弱引用或
代际清理，对标 D2D image_cache 的做法）。

### 4.6 macOS 平台接线

现状：`window.rs` 走 `drawRect:` + CGImage 拷屏。GPU 路径改为 layer 呈现：

1. 窗口创建时若选中 GPU 后端：`view.setWantsLayer(true)`，`CAMetalLayer`
   （objc2-quartz-core）**挂成 AppKit backing layer 的子层**、`contentsScale`
   跟 `backingScaleFactor`。wgpu 从该 layer 建 surface
   （`SurfaceTargetUnsafe::CoreAnimationLayer`，不必引入 raw-window-handle 窗口抽象）。
   ⚠ 真机结论（2026-08 接线时实测）：不能用 `makeBackingLayer` 让 CAMetalLayer
   顶替 backing layer——视图 layer 一旦不是 AppKit 自建的，AppKit 会把
   `layerContentsRedrawPolicy` 置 `Never` 且再不回调 `drawRect:`/`updateLayer`，
   窗口永远空白；子层方案让两条渲染路径共用同一套 `setNeedsDisplay→updateLayer`
   失效链路，代价只是子层 frame 需每帧对账（`CATransaction` 关隐式动画）。
2. 帧循环不变：仍是现有的"事件 → repaint 标记 → 渲染"路径，只是渲染分支从
   "画 pixmap + setNeedsDisplay" 换成 "WgpuTarget 渲染 + surface present"。
   `drawRect:` 在 GPU 模式下不再承担内容绘制。
3. resize/DPI：`ResizeBuffers` 等价物 = 重配 surface（`surface.configure`）+
   更新 `drawableSize`；沿用现有 resize 通知点。
4. 多窗口：共享 `Device/Queue`，每窗口一个 surface + layer 栈 + 缓存组
   （文字/图片缓存挂共享设备，跨窗口复用）。注意 memory 里记录的
   NSTimer 成环/窗口所有权三坑——GPU 改造不触碰那些机制。
5. 回退：`CAMetalLayer` 或 wgpu adapter 创建失败 → `Renderer::Auto` 静默
   回退现有 CGImage 软路径（stderr 一行），`Renderer::Gpu` 报错终止。
   环境变量 `WINDUI_GPU=1/0` 强制/禁用（对标 `WINDUI_D2D`，多一档显式关，
   用于在有 GPU 的机器上验证回退路径）。
6. 可见性：wgpu 依 `NSWindow.occlusionState` 判窗口可见，不可见直接报
   `Occluded`（绕开 Metal `nextDrawable` 的整秒卡顿）；而窗口刚
   `makeKeyAndOrderFront` 时 visible 位尚未置起，**首帧必然落空**——必须接
   `windowDidChangeOcclusionState:` 在置位时补一次重绘，否则窗口永远空白。
   验证机远程冒烟前要 `caffeinate -u -t N` 唤醒显示器，屏幕休眠时
   occlusionState 恒不可见，GPU 路径一帧不出，极易误判成代码 bug。

### 4.7 Linux 展望

Linux 的缺口在**平台层**（窗口/输入/IME/剪贴板，X11+Wayland），不在渲染器：
本设计落地后，Linux 只需
① 平台窗口层（独立项目级工作量）；
② wgpu surface 从 Wayland/X11 句柄创建（wgpu 原生支持）;
③ `GlyphSource` 的 FreeType+fontconfig（排版可先单行简排，或引 harfbuzz）。
图元/层/裁剪/图片/缓存全部零改动。设计上仅要求：`gpu` 模块内不出现任何
`#[cfg(target_os)]`——平台差异全部收口在 surface 创建与 `GlyphSource` 两个注入点。

## 5. 后端选择与回退（统一语义）

`Renderer` 枚举语义不变，各平台映射：

| 平台 | `Auto`/`Gpu` 尝试 | 回退 |
| --- | --- | --- |
| Windows | D2D（现状不变） | tiny-skia |
| macOS | wgpu/Metal（新） | tiny-skia（现状路径） |
| Linux（未来） | wgpu/Vulkan→GL | tiny-skia |

Windows 上 `gpu` feature 与 `d2d` 共存时 D2D 优先（成熟度）。运行中设备丢失：
沿用 D2D 的"重建 N 次失败 → 降级软后端"框架（`mod.rs:627` 一带的逻辑抽成
跨平台助手，两后端共用）。

## 6. 测试与验证策略

1. **离屏一致性截图**：`offscreen.rs` 渲到纹理 → readback → Pixmap，复用现有
   截图断言工具（`--click/--rclick` 流程）。与软后端基准图做**量化对比**：
   几何图元要求逐像素近似（允许 AA 边缘 ±阈值），文字用墨量/行盒对比而非
   逐像素（两套光栅路径必然有亚像素差异）。选区/命中类控件测试覆盖 Wrap 宽
   （AGENTS.md 既有要求）。
2. **CI**：macOS runner 有真 Metal；Linux job（未来）用 lavapipe 软件 Vulkan。
   无 GPU 环境下 `Renderer::Auto` 回退路径本身也是被测对象。
3. **层/裁剪平衡断言**：帧末 debug_assert 栈深归零（D2D 教训）。
4. **性能画像**：接入现有 `WINDUI_PROF`/`WINDUI_FPS`；验收基准 = ime.rs、
   settings.rs、showcase 滚动帧时间对软后端的比值（release 构建，见 memory：
   测性能必须 --release）。GPU 后端的预期收益点：大窗口全屏重绘、阴影、
   多层 opacity 动画、4K/HiDPI。
5. **内存**：文字/图片缓存加统计口径（条数/字节），`WINDUI_PROF` 输出。

## 7. 分期规划

| 阶段 | 内容 | 验收 | 预估 |
| --- | --- | --- | --- |
| **P0 骨架** | `gpu` feature + wgpu 依赖；`device.rs` 共享设备；macOS CAMetalLayer surface 接线；`WgpuTarget` 清屏+present；`Renderer` 三档与回退接通 | showcase 以 `--renderer gpu` 启动出纯色窗口、resize/HiDPI 正确、`Auto` 在禁 GPU 时回退 | 小 |
| **P1 几何图元** | `prim.rs` + SDF shader：rect/round_rect/stroke/line/circle/渐变/阴影；scissor 裁剪 + save/restore；批处理 | 离屏截图 vs 软后端量化对比通过；无文字的 example 完整渲染 | 中 |
| **P2 文字 run-cache** | `GlyphSource`（coretext 实现）+ `text.rs` 纹理缓存；measure/line_metrics 复用 | ime.rs / settings.rs / showcase 全部 tab 视觉验收；墨量对比达标；输入框打字流畅 | 中 |
| **P3 图片/层** | `tex.rs` 图片纹理缓存 + fit/圆角/opacity；`layer.rs` 离屏层栈；SVG | about.rs（toast/卡片）、图片控件、子树 opacity 动画正确 | 中 |
| **P4 收尾** | `offscreen.rs` 截图测试入 CI；性能画像与基准数据；缓存上限与统计；文档（DESIGN/ROADMAP/MACOS_PORTING 更新）；决定 macOS 默认档 | 全 example 双后端跑通；量化报告；`cargo publish` 干跑（feature 组合矩阵） | 中 |
| **P5 优化（按需）** | glyph atlas（P2 数据说话）；damage→scissor 局部重绘；Linux GlyphSource | 按各自量化目标 | 大，可延后 |

依赖关系：P0→P1→P2→P3→P4 串行；P5 各项独立。每阶段独立可合并、
可发布（feature 默认关，主线不受影响）。

### 实测基准（P4 验收，2026-08-18，验证机 Apple M4 @2x，settings 例子，release）

| 路径 | 首帧（全窗/冷缓存） | 稳态 |
| --- | --- | --- |
| 软光栅 | 30.7ms | **局部重绘** 0.7~1.0ms/帧 |
| GPU（Metal） | 55.9ms（文字缓存全冷） | **全窗重绘** 2.6~2.8ms/帧 |

结论：全窗工作负载（动画、滚动、大面积失效、4K/HiDPI）GPU 约 12×；但静态 UI 的
小面积更新上，软后端的 damage 快路（0.8ms）仍胜过 GPU 全窗（2.6ms）——GPU 后端
`as_pixmap()=None` 恒全窗是当前设计的既定取舍。GPU 稳态的大头是文字：48 条 run
各一次 command buffer 提交（约 90µs/次），P5 的 glyph atlas 把它压回整帧一次提交
后预计 <1.5ms。首帧偏高来自文字 run-cache 冷启动（48 次 Core Text 光栅 + 上传）。

## 8. 风险与对策

| 风险 | 对策 |
| --- | --- |
| 文字字重/灰度与软路径有肉眼差 | P2 验收即做墨量校准；必要时调 font smoothing 参数；保底：macOS 默认档保持 Software，GPU 作为 opt-in 直到达标 |
| wgpu 版本演进快、API 破坏性更新 | 锁定单一版本入 Cargo.toml；`gpu` 模块面积小（<3000 行），升级成本可控 |
| 依赖树膨胀影响"轻量"定位 | feature 默认关（P4 前）；README 标注体积差异；`--no-default-features` 路径始终干净 |
| CAMetalLayer 与现有 drawRect 模式互扰 | GPU/软路径在窗口创建时二选一，不做运行时热切换（设备丢失降级 = 重建窗口内容路径，沿用 D2D 降级框架） |
| 预乘 alpha 约定不一致导致边缘发黑/发白 | 全链统一预乘（tiny-skia、纹理上传、blend state `ONE, ONE_MINUS_SRC_ALPHA`）；P1 就加半透明重叠断言用例 |
| 层纹理池在小内存机器上占用高 | 池上限 + 帧末回收超额；`WINDUI_PROF` 暴露水位 |

## 9. 远期：三后端会不会变两个？

wgpu 后端在 macOS 达标后，Windows 理论上也可走 wgpu（D3D12）+ DWrite 光栅
`GlyphSource`，届时 D2D 后端可退役、图元代码三平台归一。**本设计不做此承诺**，
但 §4 的模块边界（surface 注入 + GlyphSource 注入）保证了这条路是打开的：
`dwrite.rs` 实现 `GlyphSource` 即可接入，无需动图元层。
