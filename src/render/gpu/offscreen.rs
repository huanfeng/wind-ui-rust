//! 离屏 GPU 渲染目标（对标 `platform/win32/d2d.rs` 的 `offscreen::OffscreenBackend`）。
//!
//! 存在的理由和 d2d 那份一样：**GPU 路径的像素不取回来就无法验证**。窗口 surface 里的像素
//! 读不回来，而离屏截图路径恒走软渲染，于是 GPU 后端的代码天然处在「画得像不像全靠肉眼看」
//! 的状态。渲到纹理再 readback 成 `Pixmap` 之后，既有的截图比对与墨量判据可以原样用在
//! GPU 路径上。
//!
//! P0 只有「清屏 + readback」这条最短闭环：它钉死了设备创建、渲染 pass、纹理拷回、行对齐
//! 去 padding、以及**字节语义（预乘 RGBA 直通）**这几件事。P1 的图元会在同一个纹理上加
//! 自己的 pass，再走同一个 `readback` 做逐像素比对。

use super::canvas::WgpuTarget;
use super::device::SharedGpu;
use super::prim::PrimRenderer;
use crate::geometry::Color;
use std::sync::Arc;
use tiny_skia::Pixmap;

/// 渲染目标格式。选 **`Rgba8Unorm` 而非 `Rgba8UnormSrgb`**：本项目全链（`Color`、
/// `tiny_skia::Pixmap`、D2D 后端）存的都是**已 sRGB 编码的字节**，通道值即最终像素值。
/// `*Srgb` 格式会把写入值当线性量再做一次 OETF 编码（清 0.5 会读回 188 而不是 128），
/// 与软后端逐像素比对时全盘对不上。`Unorm` 下 `写入字节/255 → 读回同一字节`，直通。
const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

/// 纹理拷到缓冲时每行字节数必须 256 对齐（WebGPU 的 `COPY_BYTES_PER_ROW_ALIGNMENT`），
/// 故窄图的缓冲行比实际像素行长，readback 时要逐行去掉尾部 padding。
const ROW_ALIGN: u32 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;

/// 离屏渲染目标：一张 `w×h` 的颜色纹理 + 一份等大的可映射回拷缓冲。
///
/// 两者都建一次、跨帧复用：截图要连渲初始帧/点击帧/悬停帧，每帧重建纹理既慢也测不出
/// 跨帧缓存相关的行为（d2d 侧的同款教训）。
pub struct OffscreenGpu {
    gpu: Arc<SharedGpu>,
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// CPU 可读的回拷缓冲。GPU 不允许直接映射渲染目标纹理，必须先 copy 到缓冲。
    readback: wgpu::Buffer,
    w: u32,
    h: u32,
    /// 对齐后的每行字节数（≥ `w * 4`）。
    row_bytes: u32,
    /// 图元管线与实例缓冲。懒建（清屏/readback 那条闭环用不到它，没必要为纯 P0 的
    /// 调用付一次管线编译），此后跨帧复用——每帧重建管线会把帧时间全吃掉。
    prim: Option<PrimRenderer>,
}

impl OffscreenGpu {
    /// 建一个 `w×h`（物理像素）的离屏目标。无可用 GPU（含软件适配器）时返回 `None`，
    /// 尺寸为 0 或超出适配器纹理上限时同样返回 `None`——绝不 panic。
    pub fn new(w: u32, h: u32) -> Option<Self> {
        if w == 0 || h == 0 {
            return None;
        }
        let gpu = SharedGpu::get()?;
        let max = gpu.device().limits().max_texture_dimension_2d;
        if w > max || h > max {
            return None;
        }
        let row_bytes = align_row_bytes(w);
        let (texture, readback) = {
            let device = gpu.device();
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("windui offscreen color"),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: TEXTURE_FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("windui offscreen readback"),
                size: row_bytes as u64 * h as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            (texture, readback)
        };
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        Some(Self {
            gpu,
            texture,
            view,
            readback,
            w,
            h,
            row_bytes,
            prim: None,
        })
    }

    /// 取本纹理的 [`RenderTarget`](crate::render::RenderTarget)：图元 pass 渲到同一张
    /// 颜色纹理上，画完仍走 [`Self::readback`] 取回，于是 P0 钉下的字节语义（预乘直通、
    /// 行对齐去 padding）原样适用于 P1 的图元。
    ///
    /// 目标是 `LoadOp::Load`——先 [`Self::clear`] 铺背景再 `make_canvas` 画内容，
    /// 与「窗口每帧先清屏后绘制」是同一条路径。
    pub fn target(&mut self) -> WgpuTarget<'_> {
        if self.prim.is_none() {
            self.prim = Some(PrimRenderer::new(&self.gpu, TEXTURE_FORMAT));
        }
        // 分字段借用：gpu/view 只读、prim 可变，互不冲突。
        let prim = self.prim.as_mut().expect("prim renderer 刚建好");
        WgpuTarget::new(self.gpu.clone(), &self.view, prim, (self.w, self.h), None)
    }

    /// 物理像素尺寸 (w, h)。
    pub fn size(&self) -> (u32, u32) {
        (self.w, self.h)
    }

    /// 颜色纹理视图。P1 的图元 pass 挂在同一张纹理上，画完仍走 [`Self::readback`] 取回。
    pub fn view(&self) -> &wgpu::TextureView {
        &self.view
    }

    /// 共享设备句柄（P1 建管线/缓冲用）。
    pub fn gpu(&self) -> &Arc<SharedGpu> {
        &self.gpu
    }

    /// 用 `color` 清屏（一个 render pass）。`color` 是非预乘的 `Color`，写入纹理的是**预乘**
    /// 字节——与 `tiny_skia::Pixmap` 的存储约定一致，两条路径的像素才可直接比对。
    pub fn clear(&mut self, color: Color) {
        let mut encoder =
            self.gpu
                .device()
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("windui offscreen clear"),
                });
        // pass 一建即清屏，不需要画任何东西；作用域结束后 encoder 才可 finish。
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("windui offscreen clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &self.view,
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
                // 不用 multiview（那是 VR 双眼分层渲染的能力）。
                multiview_mask: None,
            });
        }
        self.gpu.queue().submit([encoder.finish()]);
    }

    /// 把纹理取回 CPU，转成预乘 RGBA 的 `Pixmap`（与软后端同格式，可直接送进既有的截图
    /// 比对工具）。映射失败或分配失败时返回 `None`。
    pub fn readback(&self) -> Option<Pixmap> {
        let device = self.gpu.device();
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("windui offscreen readback"),
        });
        encoder.copy_texture_to_buffer(
            self.texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &self.readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.row_bytes),
                    rows_per_image: Some(self.h),
                },
            },
            wgpu::Extent3d {
                width: self.w,
                height: self.h,
                depth_or_array_layers: 1,
            },
        );
        self.gpu.queue().submit([encoder.finish()]);

        let slice = self.readback.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
        });
        // wgpu 没有后台线程：不 poll 就没人推进映射回调，这里会永远等下去。
        device.poll(wgpu::PollType::wait_indefinitely()).ok()?;
        rx.recv().ok()?.ok()?;

        let mut pm = Pixmap::new(self.w, self.h)?;
        {
            let src = slice.get_mapped_range().ok()?;
            let dst = pm.data_mut();
            let row = self.w as usize * 4;
            // 逐行拷贝，跳过 256 对齐带来的行尾 padding。
            for y in 0..self.h as usize {
                let s = y * self.row_bytes as usize;
                dst[y * row..(y + 1) * row].copy_from_slice(&src[s..s + row]);
            }
        }
        // BufferView 必须先析构（上面的作用域）才能 unmap。
        self.readback.unmap();
        Some(pm)
    }
}

/// 每行字节数上取整到 256 的倍数。
fn align_row_bytes(w: u32) -> u32 {
    let unpadded = w * 4;
    unpadded.div_ceil(ROW_ALIGN) * ROW_ALIGN
}

/// `Color`（非预乘 sRGB 字节）→ 清屏值。先做预乘再归一化：`Rgba8Unorm` 下
/// `round(v * 255)` 精确还原字节，故清屏色能逐字节对上预期的预乘结果。
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

    /// 取像素（预乘 RGBA）。
    fn px(pm: &Pixmap, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * pm.width() + x) * 4) as usize;
        let d = pm.data();
        [d[i], d[i + 1], d[i + 2], d[i + 3]]
    }

    /// 没有可用适配器（含软件回退）时跳过而非失败：本 feature 默认关，开着它跑 CI 的环境
    /// 未必有 GPU。跳过必须打印出来——否则报告里「跳过」和「通过」长得一样。
    fn offscreen(w: u32, h: u32) -> Option<OffscreenGpu> {
        let off = OffscreenGpu::new(w, h);
        if off.is_none() {
            println!("跳过：本机没有可用的 wgpu 适配器（含软件回退），GPU 离屏测试未执行");
        }
        off
    }

    /// 端到端最小闭环：设备创建 → 清屏 → 拷回 → 通道序。
    ///
    /// 断言具体通道而不是「有颜色」：RGBA/BGRA 搞反时红会读成蓝，弱断言抓不住
    /// （d2d 离屏测试的同款判据）。
    #[test]
    fn clears_opaque_red_and_reads_back() {
        let Some(mut off) = offscreen(64, 64) else {
            return;
        };
        off.clear(Color::rgb(255, 0, 0));
        let pm = off.readback().expect("离屏帧读回失败");
        assert_eq!(pm.width(), 64);
        assert_eq!(pm.height(), 64);
        for (x, y) in [(0, 0), (63, 0), (0, 63), (63, 63), (32, 32)] {
            assert_eq!(px(&pm, x, y), [255, 0, 0, 255], "({x},{y}) 应为不透明红");
        }
    }

    /// 半透明清屏色：读回的必须是**预乘**字节（50% 红 → 128,0,0,128），不是直通的
    /// 255,0,0,128。预乘约定不一致会让 P1 的半透明叠加整体发白/发黑，且症状要到很后面
    /// 才显形，所以在骨架期就钉住。
    ///
    /// 顺带覆盖行对齐：16px 宽 = 64 字节/行，会被 padding 到 256 —— 去 padding 写错的话
    /// 像素会整体错位。
    #[test]
    fn clears_translucent_color_in_premultiplied_bytes() {
        let Some(mut off) = offscreen(16, 16) else {
            return;
        };
        off.clear(Color::rgba(255, 0, 0, 128));
        let pm = off.readback().expect("离屏帧读回失败");
        for (x, y) in [(0, 0), (15, 15), (8, 8)] {
            let p = px(&pm, x, y);
            let near = |got: u8, want: i32| (got as i32 - want).abs() <= 1;
            assert!(
                near(p[0], 128) && p[1] == 0 && p[2] == 0 && near(p[3], 128),
                "({x},{y}) 应为预乘的 50% 红 [128,0,0,128]，实得 {p:?}"
            );
        }
    }

    /// 非法尺寸不 panic，直接给 `None`（调用方据此回退软路径）。
    #[test]
    fn rejects_zero_size() {
        assert!(OffscreenGpu::new(0, 16).is_none());
        assert!(OffscreenGpu::new(16, 0).is_none());
    }

    #[test]
    fn row_bytes_align_to_256() {
        assert_eq!(align_row_bytes(1), 256);
        assert_eq!(align_row_bytes(64), 256);
        assert_eq!(align_row_bytes(65), 512);
    }
}
