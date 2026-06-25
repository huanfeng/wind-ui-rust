//! Direct2D / Direct3D11 / DXGI GPU 呈现后端（v1：仅清屏呈现，不画内容）。
//!
//! 链路：`D3D11CreateDevice`（硬件）→ `IDXGIDevice` → `IDXGIFactory2` →
//! flip-model `CreateSwapChainForHwnd` → `ID2D1Device`/`ID2D1DeviceContext`，
//! 每帧把 swapchain 后备缓冲绑为 D2D target、`Clear(bg)` 后 `Present`。
//!
//! 设计：设备创建任何环节失败 `try_create` 返回 `None`，调用方据此回退软后端，
//! **绝不 panic**。COM 对象（`ID2D1*`/`IDXGI*`/`ID3D11Device`）非 `Send`/`Sync`，
//! 必须在创建它们的 UI（STA）线程上使用——与 `DWriteEngine` 同样的单线程约束，
//! 故仅作普通结构体字段持有，不跨线程共享。

use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext, ID2D1Factory1,
    D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1,
    D2D1_FACTORY_TYPE_SINGLE_THREADED,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::ValidateRect;

use super::{AppHandler, WinRenderBackend};
use crate::geometry::Color;

/// GPU 呈现后端。持有 D3D11/DXGI/D2D 的 COM 对象与 swapchain。
pub(super) struct D2DBackend {
    /// D3D11 设备（保活；DXGI/D2D 设备链均源自它）。
    #[allow(dead_code)]
    d3d_device: ID3D11Device,
    /// D2D 工厂（保活）。
    #[allow(dead_code)]
    d2d_factory: ID2D1Factory1,
    /// D2D 设备（保活；device context 源自它）。
    #[allow(dead_code)]
    d2d_device: ID2D1Device,
    /// flip-model swapchain：Present 与 ResizeBuffers 的目标。
    swapchain: IDXGISwapChain1,
    /// D2D 设备上下文：BeginDraw/Clear/EndDraw 与 SetTarget 的目标。
    context: ID2D1DeviceContext,
}

/// 尝试建立 GPU 呈现链路。任一环节失败返回 `None`（调用方回退软后端）。
pub(super) fn try_create(hwnd: HWND, w: i32, h: i32) -> Option<D2DBackend> {
    unsafe { try_create_inner(hwnd, w, h) }
}

unsafe fn try_create_inner(hwnd: HWND, w: i32, h: i32) -> Option<D2DBackend> {
    // 1. D3D11 硬件设备（BGRA 支持，供 D2D 互操作）。失败即放弃。
    let mut d3d_device: Option<ID3D11Device> = None;
    let mut feature_level = D3D_FEATURE_LEVEL::default();
    D3D11CreateDevice(
        None, // 默认适配器
        D3D_DRIVER_TYPE_HARDWARE,
        Default::default(), // 无软件光栅模块
        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        None, // 默认特性级别集
        D3D11_SDK_VERSION,
        Some(&mut d3d_device),
        Some(&mut feature_level),
        None, // 不需要 immediate context
    )
    .ok()?;
    let d3d_device = d3d_device?;

    // 2. D3D11 → IDXGIDevice → adapter → IDXGIFactory2，建 flip-model swapchain。
    let dxgi_device: IDXGIDevice = d3d_device.cast().ok()?;
    let adapter = dxgi_device.GetAdapter().ok()?;
    let factory: IDXGIFactory2 = adapter.GetParent().ok()?;

    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: w.max(1) as u32,
        Height: h.max(1) as u32,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: false.into(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    let swapchain = factory
        .CreateSwapChainForHwnd(&d3d_device, hwnd, &desc, None, None)
        .ok()?;

    // 3. D2D 工厂 → 设备（from IDXGIDevice）→ 设备上下文。
    let d2d_factory: ID2D1Factory1 =
        D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
    let d2d_device = d2d_factory.CreateDevice(&dxgi_device).ok()?;
    let context = d2d_device.CreateDeviceContext(Default::default()).ok()?;

    let backend = D2DBackend {
        d3d_device,
        d2d_factory,
        d2d_device,
        swapchain,
        context,
    };
    // 初次绑定后备缓冲为 target。绑定失败同样回退软后端。
    backend.bind_target().ok()?;
    Some(backend)
}

impl D2DBackend {
    /// 把 swapchain 当前后备缓冲包成 D2D 位图并设为渲染 target。
    unsafe fn bind_target(&self) -> windows::core::Result<()> {
        let surface: IDXGISurface = self.swapchain.GetBuffer(0)?;
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET | D2D1_BITMAP_OPTIONS_CANNOT_DRAW,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let bitmap: ID2D1Bitmap1 = self
            .context
            .CreateBitmapFromDxgiSurface(&surface, Some(&props))?;
        self.context.SetTarget(&bitmap);
        Ok(())
    }
}

impl WinRenderBackend for D2DBackend {
    fn resize(&mut self, w: i32, h: i32) {
        unsafe {
            // 先解绑 target，释放对旧后备缓冲的全部引用，否则 ResizeBuffers 失败。
            self.context.SetTarget(None);
            let r = self.swapchain.ResizeBuffers(
                0, // 保持缓冲数
                w.max(1) as u32,
                h.max(1) as u32,
                DXGI_FORMAT_UNKNOWN,     // 保持格式
                DXGI_SWAP_CHAIN_FLAG(0), // 无额外标志
            );
            debug_assert!(r.is_ok(), "ResizeBuffers 失败: {r:?}");
            // 重新绑定新尺寸的后备缓冲。
            let _ = self.bind_target();
        }
    }

    unsafe fn paint(&mut self, hwnd: HWND, bg: Color, _handler: &mut dyn AppHandler) {
        // 重新绑定 target：覆盖首帧之外、resize 之后等情形，幂等且廉价。
        if self.bind_target().is_err() {
            return;
        }
        self.context.BeginDraw();
        self.context.Clear(Some(&d2d_color(bg)));
        // EndDraw 的 out 参数（tag1/tag2）此处不关心；返回错误暂仅忽略（设备丢失留后续任务处理）。
        let _ = self.context.EndDraw(None, None);
        // Present(1, 0)：与垂直同步对齐，呈现一帧。
        let _ = self.swapchain.Present(1, DXGI_PRESENT(0));
        // DXGI 呈现路径不走 BeginPaint/EndPaint，必须显式验证整个客户区更新区域，
        // 否则 Windows 持续重投 WM_PAINT → 忙循环、单核 100%，破坏空闲零 CPU 设计。
        // 这让 backend.paint 自包含完成"呈现 + 验证更新区域"契约，与 SkiaBackend 对称。
        let _ = ValidateRect(Some(hwnd), None);
    }
}

/// `Color`（非预乘 sRGB u8）→ D2D `D2D1_COLOR_F`（直通 RGBA / 255）。
fn d2d_color(c: Color) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: c.r as f32 / 255.0,
        g: c.g as f32 / 255.0,
        b: c.b as f32 / 255.0,
        a: c.a as f32 / 255.0,
    }
}
