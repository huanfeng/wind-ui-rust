//! 窗口 surface 的呈现封装（对标 `platform/win32/d2d.rs` 里 swapchain 那一段）。
//!
//! 与 [`OffscreenGpu`](super::offscreen::OffscreenGpu) 的分工：那边渲到自己建的纹理、画完
//! 读回 CPU（测试用）；这边渲到**由窗口系统轮转的** surface 纹理，画完 present 上屏。图元
//! 管线、清屏语义、预乘约定两边完全一样，差别只在纹理从哪来、以及「取不到这一帧」要怎么办。
//!
//! # 平台无关
//!
//! 本文件**不出现任何 `#[cfg(target_os)]`**（模块契约，见 `mod.rs`）：`wgpu::Surface` 由平台
//! 层建好后交进来。建 surface 的调用天生带平台性——macOS 要 `SurfaceTargetUnsafe::
//! CoreAnimationLayer`、Wayland/X11 又是另一套，而且都是 `unsafe`（生命周期得由窗口层担保）。
//! 把那一步留在注入点，这里只收成品，Linux 将来接平台层时本文件零改动。
//!
//! # 取帧失败不是 panic
//!
//! 窗口 surface 会因为 resize、显示器切换、窗口被遮挡、设备丢失而临时或永久取不到纹理。
//! 这类事件在正常使用中一定会发生（拖一下窗口边就可能撞上），故 [`WindowGpu::begin_frame`]
//! 返回 [`FrameError`] 而不是崩：`Skipped` 表示「这帧不画，下帧自然重来」，`Lost` 表示
//! 「已经重配过还是不行」，由调用方决定是提示还是降级——降级策略属于平台层（win32 的 D2D
//! 后端也是这么分工的）。

use std::sync::Arc;

use super::canvas::WgpuTarget;
use super::device::SharedGpu;
use super::prim::PrimRenderer;
use crate::geometry::Color;

/// 取帧失败的三档。分这么细是因为**调用方对三者的正确反应各不相同**：重试、干等、降级。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// 窗口当前不可见（最小化、被完全遮住、不在当前 Space）。
    ///
    /// **不要重试**：不可见期间每次都会是这个结果，重试就是空转。等窗口重新可见时由窗口
    /// 系统的事件唤回一次重绘即可（macOS 上是 `windowDidChangeOcclusionState:`）。
    Occluded,
    /// 本帧没拿到纹理（驱动侧短暂用尽可用 drawable 之类），下一帧多半就好了。
    /// 调用方应**再排一次重绘**——事件驱动的宿主不重排的话，界面会停在上一帧的旧内容上。
    Skipped,
    /// 重配之后仍取不到：surface 或设备已不可用。调用方据此提示或降级软后端。
    Lost,
}

/// 一个窗口的 GPU 呈现目标：surface + 配置 + 跨帧复用的图元管线。
///
/// 多窗口各持一个，设备（[`SharedGpu`]）是进程单例、大家共用——这与 d2d 后端「共享 device、
/// 各持 swapchain」的分法一致。
pub struct WindowGpu {
    gpu: Arc<SharedGpu>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    /// 图元管线。绑死了 [`Self::config`] 的附着格式，故随本对象一起建、一起析构。
    prim: PrimRenderer,
    /// 上一帧拿到的是 `Suboptimal`（尺寸/格式已与 surface 实际状态不符）。
    ///
    /// 不当场重配：`configure` 在还有 `SurfaceTexture` 活着时会 panic，而那张纹理正要拿去
    /// 画这一帧。改为记下来，下一帧开画前补做。
    needs_reconfigure: bool,
}

impl WindowGpu {
    /// 接管一个已建好的窗口 surface。`size` 是**物理像素**尺寸。
    ///
    /// 适配器不支持该 surface、或它只给得出 sRGB 格式时返回 `None`——调用方据此回退软路径。
    /// 后一种情况之所以也算失败，见 [`pick_format`]。
    pub fn new(
        gpu: Arc<SharedGpu>,
        surface: wgpu::Surface<'static>,
        size: (u32, u32),
    ) -> Option<Self> {
        let caps = surface.get_capabilities(gpu.adapter());
        if caps.formats.is_empty() || caps.present_modes.is_empty() {
            return None;
        }
        let format = pick_format(&caps.formats)?;
        // 不透明合成：窗口底色恒不透明（宿主每帧先铺 bg），让窗口系统省掉一次混合。
        // 拿不到 `Opaque` 时退 `Auto`（由后端自行决定，Metal 上即不透明）。
        let alpha_mode = if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::Opaque) {
            wgpu::CompositeAlphaMode::Opaque
        } else {
            wgpu::CompositeAlphaMode::Auto
        };
        let (w, h) = clamp_size(&gpu, size)?;
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            // 颜色空间交后端定（= 历史行为）。本项目的通道值本就是 sRGB 编码字节，
            // 不走宽色域/HDR——那会改变「写进去的字节即最终像素」这条全链约定。
            color_space: wgpu::SurfaceColorSpace::Auto,
            width: w,
            height: h,
            // `Fifo` 是唯一处处都支持的呈现模式，且它就是垂直同步——本项目的帧驱动是
            // 「有动画才排下一帧」，不需要 Mailbox 那种为了压延迟而空转的模式。
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode,
            view_formats: vec![],
        };
        surface.configure(gpu.device(), &config);
        let prim = PrimRenderer::new(&gpu, format);
        Some(Self {
            gpu,
            surface,
            config,
            prim,
            needs_reconfigure: false,
        })
    }

    /// 当前配置的物理像素尺寸。
    pub fn size(&self) -> (u32, u32) {
        (self.config.width, self.config.height)
    }

    /// 选中的呈现格式。
    pub fn format(&self) -> wgpu::TextureFormat {
        self.config.format
    }

    /// 选中的 alpha 合成模式。
    pub fn alpha_mode(&self) -> wgpu::CompositeAlphaMode {
        self.config.alpha_mode
    }

    /// 一行诊断信息（适配器 + 后端 + 格式），供平台层在 `WINDUI_PROF` 下打印。
    pub fn info(&self) -> String {
        let a = self.gpu.adapter().get_info();
        format!(
            "gpu: {} [{:?}/{:?}] surface={:?} alpha={:?} {}x{}",
            a.name,
            a.backend,
            a.device_type,
            self.config.format,
            self.config.alpha_mode,
            self.config.width,
            self.config.height
        )
    }

    /// 尺寸变化时重配 surface。尺寸未变则是空操作（每帧无脑调用即可）。
    pub fn resize(&mut self, size: (u32, u32)) {
        let Some((w, h)) = clamp_size(&self.gpu, size) else {
            return;
        };
        if w == self.config.width && h == self.config.height {
            return;
        }
        self.config.width = w;
        self.config.height = h;
        self.surface.configure(self.gpu.device(), &self.config);
        self.needs_reconfigure = false;
    }

    /// 取本帧纹理并用 `clear` 铺底，返回可供绘制的一帧。
    ///
    /// 调用方须在丢弃 [`Frame`] 之前画完，并调 [`Frame::present`] 上屏；不 present 就丢弃
    /// 等于放弃这一帧（不会泄漏，只是屏幕上没有变化）。
    pub fn begin_frame(&mut self, clear: Color) -> Result<Frame<'_>, FrameError> {
        if self.needs_reconfigure {
            self.surface.configure(self.gpu.device(), &self.config);
            self.needs_reconfigure = false;
        }
        let texture = self.acquire()?;
        let view = texture
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        self.clear(&view, clear);
        Ok(Frame {
            gpu: self.gpu.clone(),
            texture,
            view,
            prim: &mut self.prim,
            size: (self.config.width, self.config.height),
        })
    }

    /// 取一帧纹理，失败时按 wgpu 的建议重配一次再试。
    fn acquire(&mut self) -> Result<wgpu::SurfaceTexture, FrameError> {
        use wgpu::CurrentSurfaceTexture as C;
        match self.surface.get_current_texture() {
            C::Success(t) => Ok(t),
            C::Suboptimal(t) => {
                // 能用，但配置已经跟不上 surface 的实际状态了（典型是刚 resize 完）。
                // 先把这帧画完，重配推到下一帧——见 `needs_reconfigure`。
                self.needs_reconfigure = true;
                Ok(t)
            }
            C::Occluded => Err(FrameError::Occluded),
            C::Timeout => Err(FrameError::Skipped),
            // 需要重配后重试的三种。重配一次仍不成就上报 `Lost`——再退一步（重建 surface
            // 乃至设备）要动窗口层的对象，属于调用方的决定。
            C::Outdated | C::Lost | C::Validation => {
                self.surface.configure(self.gpu.device(), &self.config);
                self.needs_reconfigure = false;
                match self.surface.get_current_texture() {
                    C::Success(t) | C::Suboptimal(t) => Ok(t),
                    C::Occluded => Err(FrameError::Occluded),
                    C::Timeout => Err(FrameError::Skipped),
                    C::Outdated | C::Lost | C::Validation => Err(FrameError::Lost),
                }
            }
        }
    }

    /// 用 `color` 铺底（一个只做 `LoadOp::Clear` 的 pass）。
    ///
    /// 图元 pass 是 `LoadOp::Load`（painter's algorithm），所以底必须先铺——对应软路径每帧
    /// 的 `pixmap.fill(bg)`。surface 纹理是轮转复用的，不清就会看到两三帧之前的内容。
    fn clear(&self, view: &wgpu::TextureView, color: Color) {
        let mut encoder =
            self.gpu
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("windui surface clear"),
                });
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("windui surface clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(clear_color(color)),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        self.gpu.queue().submit([encoder.finish()]);
    }
}

/// 已取到纹理、已铺底的一帧。
///
/// 生命周期借着 [`WindowGpu`]：一帧没画完不能再开下一帧，也不能在这期间 resize
/// （`configure` 撞上活着的 `SurfaceTexture` 会 panic）——借用检查直接把这两件事挡掉。
pub struct Frame<'a> {
    gpu: Arc<SharedGpu>,
    texture: wgpu::SurfaceTexture,
    view: wgpu::TextureView,
    prim: &'a mut PrimRenderer,
    size: (u32, u32),
}

impl Frame<'_> {
    /// 本帧的渲染目标。交给宿主的 `render(&mut dyn RenderTarget, size)`。
    pub fn target(&mut self) -> WgpuTarget<'_> {
        WgpuTarget::new(
            self.gpu.clone(),
            &self.view,
            &mut *self.prim,
            self.size,
            None,
        )
    }

    /// 物理像素尺寸。
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// 上屏。**调用前须先丢弃 [`Self::target`] 返回的目标**——`WgpuCanvas` 是在析构时
    /// 才把攒下的图元提交的（见 `canvas.rs`），提前 present 就会 present 一张空底。
    /// `target()` 借的是 `&mut self`，而本方法按值取 `self`，这条顺序由借用检查保证。
    pub fn present(self) {
        let Self {
            gpu, texture, view, ..
        } = self;
        // 视图先于纹理交还：present 之后这张纹理就回到 surface 的轮转队列里了，
        // 不该再有视图指着它。
        drop(view);
        gpu.queue().present(texture);
    }
}

/// 选呈现格式：**必须是非 sRGB 变体**（优先 `Bgra8Unorm`——Metal/DXGI 的原生序）。
///
/// 理由与离屏目标那边逐字一致（见 `offscreen.rs` 的 `TEXTURE_FORMAT`）：本项目全链存的是
/// **已 sRGB 编码的字节**，`*Srgb` 格式会把写入值当线性量再编码一次，界面整体偏亮，且与软
/// 后端的截图逐像素对不上。管线按这里选出的格式建，混合仍发生在 sRGB 字节空间。
///
/// 一个非 sRGB 格式都拿不到时返回 `None`（调用方回退软路径）：与其偷偷画出一套颜色不对的
/// 界面，不如不画——「颜色整体偏亮」这种症状最难被认成后端选错。
fn pick_format(formats: &[wgpu::TextureFormat]) -> Option<wgpu::TextureFormat> {
    let preferred = [
        wgpu::TextureFormat::Bgra8Unorm,
        wgpu::TextureFormat::Rgba8Unorm,
    ];
    preferred
        .into_iter()
        .find(|f| formats.contains(f))
        .or_else(|| formats.iter().copied().find(|f| !f.is_srgb()))
}

/// 把尺寸收进 [1, 适配器纹理上限]。0 会让 `configure` panic，超上限会让它报验证错误——
/// 窗口最小化（尺寸归零）与超大外接屏都是真实会走到的路径。
fn clamp_size(gpu: &SharedGpu, (w, h): (u32, u32)) -> Option<(u32, u32)> {
    let max = gpu.device().limits().max_texture_dimension_2d;
    if max == 0 {
        return None;
    }
    Some((w.clamp(1, max), h.clamp(1, max)))
}

/// `Color`（非预乘 sRGB 字节）→ 清屏值。与 `offscreen.rs` 的同名函数同一份算法：
/// 先预乘再归一化，`Unorm` 目标下能逐字节还原预期结果。两处各留一份而不是抽公共函数，
/// 是因为它短到抽出去反而要多绕一层——但改动必须同步。
fn clear_color(c: Color) -> wgpu::Color {
    let a = c.a as u32;
    let premul = |v: u8| ((v as u32 * a + 127) / 255) as f64 / 255.0;
    wgpu::Color {
        r: premul(c.r),
        g: premul(c.g),
        b: premul(c.b),
        a: c.a as f64 / 255.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非 sRGB 优先，且 `Bgra8Unorm` 排在 `Rgba8Unorm` 前（与后端原生序一致，省一次转换）。
    #[test]
    fn picks_non_srgb_format() {
        use wgpu::TextureFormat as F;
        assert_eq!(
            pick_format(&[F::Bgra8UnormSrgb, F::Bgra8Unorm]),
            Some(F::Bgra8Unorm)
        );
        assert_eq!(
            pick_format(&[F::Rgba8Unorm, F::Bgra8Unorm]),
            Some(F::Bgra8Unorm)
        );
        assert_eq!(
            pick_format(&[F::Rgba8UnormSrgb, F::Rgba8Unorm]),
            Some(F::Rgba8Unorm)
        );
        // 既非首选也非 sRGB 的格式仍可用（fp16 等）。
        assert_eq!(
            pick_format(&[F::Bgra8UnormSrgb, F::Rgba16Float]),
            Some(F::Rgba16Float)
        );
    }

    /// 只有 sRGB 变体可选时宁可失败——回退软路径也好过画出偏亮的界面。
    #[test]
    fn rejects_srgb_only_surface() {
        use wgpu::TextureFormat as F;
        assert_eq!(pick_format(&[F::Bgra8UnormSrgb, F::Rgba8UnormSrgb]), None);
        assert_eq!(pick_format(&[]), None);
    }

    /// 预乘清屏色：50% 红 → (0.5, 0, 0, 0.5)。与离屏那份必须给出同一结果，
    /// 否则同一个 `bg` 在窗口与截图里会是两个颜色。
    #[test]
    fn clear_color_is_premultiplied() {
        let c = clear_color(Color::rgba(255, 0, 0, 128));
        assert!((c.r - 128.0 / 255.0).abs() < 0.005, "实得 {}", c.r);
        assert_eq!(c.g, 0.0);
        assert_eq!(c.b, 0.0);
        assert!((c.a - 128.0 / 255.0).abs() < 0.005, "实得 {}", c.a);
        let o = clear_color(Color::rgb(255, 255, 255));
        assert_eq!((o.r, o.g, o.b, o.a), (1.0, 1.0, 1.0, 1.0));
    }
}
