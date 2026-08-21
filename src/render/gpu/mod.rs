//! 跨平台 GPU 渲染后端（wgpu）：macOS→Metal、Linux→Vulkan/GL、Windows→DX12。
//!
//! 存在的理由是 macOS 与未来的 Linux 目前**只有软件光栅**一条路（Windows 侧已有成熟的
//! D2D 后端，本模块不碰它）。选 wgpu 而不是各平台手写原生 API，是为了「一个机制覆盖
//! 多平台」：`Canvas` 的图元集封闭（无任意 path、无变换），一套 SDF shader 即可覆盖，
//! 而文字仍由平台引擎（Core Text / FreeType）光栅、GPU 只负责合成，保住「文字与系统
//! 一致」这个核心卖点。完整取舍见 `docs/gpu-cross-platform-design.md`。
//!
//! # 分期（当前进度：P3）
//!
//! | 阶段 | 内容 | 落点 |
//! | --- | --- | --- |
//! | ✅ P0 | 共享设备 + 离屏目标（清屏 / readback 成 `Pixmap`） | `device.rs`、`offscreen.rs` |
//! | ✅ P1 | 几何图元：SDF shader + 实例批处理、裁剪与 `save/restore`，接上 `Canvas`/`RenderTarget` | `prim.rs`、`shader.wgsl`、`canvas.rs` |
//! | ✅ P2 | 文字：平台光栅（`GlyphSource`）→ R8 run-cache → 纹理四边形；measure/line_metrics 仍委托引擎 | `text.rs`、`text.wgsl` |
//! | ✅ P3 | 图片/SVG 纹理缓存（fit/圆角/opacity）与离屏层栈（`push_layer` 的 opacity 合成） | `tex.rs`、`layer.rs`、`image.wgsl` |
//! | **✅ P5（当前）** | 帧末一次提交、damage→scissor 局部重绘、文字 glyph atlas | `canvas.rs`、`surface.rs`、`text.rs` |
//!
//! 到 P3 为止 `Canvas` 的图元集**已全部落地**，没有空实现了。P5 之后 GPU 档在两条主要
//! 场景上都优于软件路径（M4 @2x、160 控件 + 1 输入框、release）：连续动画 CPU 16.2% 对
//! 29.0%，闲置 0.8% 对 1.2%，稳态整窗帧绘制 3.0ms 对 5.8ms。
//!
//! 仍与软后端有意不同的只剩 `as_pixmap` 恒 `None`（GPU 的像素读不回宿主——但**局部重绘
//! 已经不再依赖它**，改由目标自己的常驻色纹理 + `supports_partial` 承担，见 `surface.rs`）
//! 与 `draw_text` 在引擎不提供 `GlyphSource` 时的空操作（Windows 的 DirectWrite 走 D2D
//! 后端，不实现它）。
//!
//! # 图元渲染的一句话总结
//!
//! 几何：整帧**一条管线 + 一次 instanced draw**——每个图元一个实例，顶点着色器按外包框
//! 展开 quad，片元按 kind 求解析 SDF；裁剪矩形是实例字段（逐像素裁，不切 scissor），渐变
//! 色标放 uniform 数组。于是帧内零状态切换。详见 `prim.rs` 与 `shader.wgsl` 的模块头。
//!
//! 文字：另一条管线（要纹理绑定，几何那条没有）。**两种粒度并存**：单行走 glyph atlas
//! （字形跨文本共享，整帧共用一组绑定 → 一次 instanced draw），折行段落退回整段光栅
//! （一条 run 一张 R8 纹理、一个 draw call）。图片是第三条（采完整的预乘 RGBA、外加一个
//! 圆角 SDF 遮罩）。三条管线
//! **按 `Canvas` 调用顺序交替 flush**（谁要入批就先把另两批画掉），叠放次序于是恒等于
//! 录制次序。详见 `text.rs`、`tex.rs` 与 `canvas.rs::before_prim`。
//!
//! 交错的次数与控件数同阶，但**整帧只提交一次** command buffer：三条管线都录进
//! `WgpuCanvas` 持有的同一个 encoder，实例数据各占缓冲的一段（帧内游标），渐变表帧内
//! 累积、增量写。此前是每批一次 `queue.submit`（Metal 上实测约 90 µs/次），一帧上百批
//! 就是十几毫秒——判据见 `canvas.rs` 的 `a_frame_submits_once_no_matter_how_many_batches`。
//!
//! 层：`push_layer` 从纹理池取一张与目标同尺寸的透明纹理并把后续绘制重定向进去，
//! `pop_layer` 把它整张按 opacity 合成回父目标（走图片管线）。嵌套即栈。详见 `layer.rs`。
//!
//! # 平台无关性
//!
//! 本模块内**不出现任何 `#[cfg(target_os)]`**。平台差异只从两个注入点进来：surface 创建
//! （窗口层建好 `wgpu::Surface` 交进 `surface.rs` 的 `WindowGpu`）与 `GlyphSource`
//! （平台文字引擎，P2 已接；macOS 是 Core Text，Linux 将来接 FreeType）。这是 Linux 将来
//! 只写平台层、渲染器零改动的前提。
//!
//! 窗口呈现（`surface.rs`）目前只有 macOS 平台层接了线（`platform/macos/window.rs` 挂
//! CAMetalLayer）；Windows 走的是既有的 D2D 后端，不经过本模块。

pub mod canvas;
pub mod device;
mod layer;
pub mod offscreen;
mod prim;
pub mod surface;
mod tex;
mod text;

pub use canvas::{WgpuCanvas, WgpuTarget};
pub use device::{release_shared_gpu, SharedGpu};
pub use offscreen::OffscreenGpu;
pub use surface::{Frame, FrameError, WindowGpu};
