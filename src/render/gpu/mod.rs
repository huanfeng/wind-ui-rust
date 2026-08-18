//! 跨平台 GPU 渲染后端（wgpu）：macOS→Metal、Linux→Vulkan/GL、Windows→DX12。
//!
//! 存在的理由是 macOS 与未来的 Linux 目前**只有软件光栅**一条路（Windows 侧已有成熟的
//! D2D 后端，本模块不碰它）。选 wgpu 而不是各平台手写原生 API，是为了「一个机制覆盖
//! 多平台」：`Canvas` 的图元集封闭（无任意 path、无变换），一套 SDF shader 即可覆盖，
//! 而文字仍由平台引擎（Core Text / FreeType）光栅、GPU 只负责合成，保住「文字与系统
//! 一致」这个核心卖点。完整取舍见 `docs/gpu-cross-platform-design.md`。
//!
//! # 分期（当前进度：P0）
//!
//! | 阶段 | 内容 | 落点 |
//! | --- | --- | --- |
//! | ✅ P0 | 共享设备 + 离屏目标（清屏 / readback 成 `Pixmap`） | `device.rs`、`offscreen.rs` |
//! | **✅ P1（当前）** | 几何图元：SDF shader + 实例批处理、裁剪与 `save/restore`，接上 `Canvas`/`RenderTarget` | `prim.rs`、`shader.wgsl`、`canvas.rs` |
//! | P2 | 文字：平台光栅进纹理缓存（`GlyphSource`），measure/line_metrics 复用现有排版 | `text.rs` |
//! | P3 | 图片/SVG 纹理缓存与离屏层栈（`push_layer` 的 opacity 合成） | `tex.rs`、`layer.rs` |
//!
//! P1 把 `Canvas`/`RenderTarget` **整套**实现了，但 `draw_text`（P2）、`draw_image`（P3）、
//! `push_layer` 的 opacity（P3）还是空缺——这三处都在 `canvas.rs` 里显式留了空实现 +
//! 进程内一次性 stderr 提示。这与「P0 刻意不实现两个 trait」并不矛盾：P0 时连一个图元都
//! 画不出来，接上去就是个纯粹会静默漏画的后端；到 P1 几何已经完整，缺的那几项有明确落点、
//! 有提示、有断言（层栈仍保持平衡），拿它渲无文字的界面已经是可验证的行为。
//!
//! # 图元渲染的一句话总结
//!
//! 整帧**一条管线 + 一次 instanced draw**：每个图元一个实例，顶点着色器按外包框展开
//! quad，片元按 kind 求解析 SDF；裁剪矩形是实例字段（逐像素裁，不切 scissor），渐变
//! 色标放 uniform 数组。于是帧内零状态切换，也没有批次划分这件事。详见 `prim.rs`
//! 与 `shader.wgsl` 的模块头。
//!
//! # 平台无关性
//!
//! 本模块内**不出现任何 `#[cfg(target_os)]`**。平台差异只从两个注入点进来：surface 创建
//! （P1，窗口层给句柄）与 `GlyphSource`（P2，平台文字引擎）。这是 Linux 将来只写平台层、
//! 渲染器零改动的前提。

pub mod canvas;
pub mod device;
pub mod offscreen;
mod prim;

pub use canvas::{WgpuCanvas, WgpuTarget};
pub use device::{release_shared_gpu, SharedGpu};
pub use offscreen::OffscreenGpu;
