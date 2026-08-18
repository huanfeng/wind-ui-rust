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
//! | **P0（本模块现有代码）** | 共享设备 + 离屏目标（清屏 / readback 成 `Pixmap`） | `device.rs`、`offscreen.rs` |
//! | P1 | 几何图元：SDF shader + 实例批处理、裁剪与 `save/restore`，接上 `Canvas`/`RenderTarget` | `prim.rs`、`shader.wgsl` |
//! | P2 | 文字：平台光栅进纹理缓存（`GlyphSource`），measure/line_metrics 复用现有排版 | `text.rs` |
//! | P3 | 图片/SVG 纹理缓存与离屏层栈（`push_layer` 的 opacity 合成） | `tex.rs`、`layer.rs` |
//!
//! **P0 刻意不实现 `Canvas` 与 `RenderTarget`**：这两个 trait 一旦实现就必须整套图元都能画，
//! 否则调用方拿到的是一个会静默漏画的后端。骨架期只把「设备能建起来、像素能取回来」这条
//! 最短闭环钉死——它同时也是 P1 图元做逐像素比对的地基。
//!
//! # 平台无关性
//!
//! 本模块内**不出现任何 `#[cfg(target_os)]`**。平台差异只从两个注入点进来：surface 创建
//! （P1，窗口层给句柄）与 `GlyphSource`（P2，平台文字引擎）。这是 Linux 将来只写平台层、
//! 渲染器零改动的前提。

pub mod device;
pub mod offscreen;

pub use device::{release_shared_gpu, SharedGpu};
pub use offscreen::OffscreenGpu;
