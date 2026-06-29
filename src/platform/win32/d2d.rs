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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use windows::core::{Interface, HRESULT, PCWSTR};
use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_COMPOSITE_MODE_SOURCE_OVER,
    D2D1_GRADIENT_STOP, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    CLSID_D2D1Shadow, D2D1CreateFactory, ID2D1Bitmap1, ID2D1Brush, ID2D1Device, ID2D1DeviceContext,
    ID2D1Effect, ID2D1Factory1, ID2D1SolidColorBrush, D2D1_ANTIALIAS_MODE_ALIASED,
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_BITMAP_OPTIONS_CANNOT_DRAW, D2D1_BITMAP_OPTIONS_NONE,
    D2D1_BITMAP_OPTIONS_TARGET, D2D1_BITMAP_PROPERTIES1, D2D1_BUFFER_PRECISION_8BPC_UNORM,
    D2D1_COLOR_INTERPOLATION_MODE_STRAIGHT, D2D1_COLOR_SPACE_SRGB,
    D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT, D2D1_ELLIPSE, D2D1_EXTEND_MODE_CLAMP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_INTERPOLATION_MODE_LINEAR, D2D1_LAYER_OPTIONS1_NONE,
    D2D1_LAYER_PARAMETERS1, D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES, D2D1_PROPERTY_TYPE_FLOAT,
    D2D1_PROPERTY_TYPE_VECTOR4, D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES, D2D1_ROUNDED_RECT,
    D2D1_SHADOW_PROP_BLUR_STANDARD_DEVIATION, D2D1_SHADOW_PROP_COLOR,
    D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, IDWriteTextLayout,
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_ALIGNMENT_TRAILING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    DXGI_FORMAT_UNKNOWN, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISurface, IDXGISwapChain1, DXGI_ERROR_DEVICE_REMOVED,
    DXGI_ERROR_DEVICE_RESET, DXGI_PRESENT, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
    DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows_numerics::{Matrix3x2, Vector2};

use super::{AppHandler, WinRenderBackend};
use crate::geometry::{Color, Size};
use crate::render::{Canvas, Gradient, Paint, RenderTarget};

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
    /// 缓存的后备缓冲 D2D 位图：官方文档要求建一次、仅 resize 重建（每帧 CreateBitmapFromDxgiSurface
    /// 很贵且连续重绘下累积驱动内存）。resize 时置 None，下一帧 bind_target 重建。
    target_bitmap: Option<ID2D1Bitmap1>,
    /// 可复用纯色画刷：每次绘制前 `SetColor` 改色，避免逐图元建/销画刷。
    /// device-dependent 资源（设备丢失时随上下文一并重建——Task 11 处理丢失）。
    /// Task 11 注意：设备丢失重建 context 后必须重建此 brush（绑定旧 context，复用会绘制失败）。
    solid: ID2D1SolidColorBrush,
    /// 渐变画刷缓存：键为 (类型, 量化端点/半径, 量化 stops)，避免每帧重复 CreateGradientBrush。
    /// Task 11 注意：设备丢失重建 context 时必须 `grad_cache.clear()`——缓存的 `ID2D1Brush`
    /// 绑定到旧 context，复用会绘制失败/崩溃（solid brush 同理须重建）。
    grad_cache: HashMap<GradKey, ID2D1Brush>,
    /// DirectWrite 工厂：文字排版/绘制的入口（`CreateTextFormat`/`CreateTextLayout`）。
    /// device-**独立**资源（进程级系统字体缓存），设备丢失**无需**重建——
    /// 不同于 solid/渐变缓存，**别**放进 Task 11 的设备丢失重建清单。
    dwrite_factory: IDWriteFactory,
    /// TextFormat 缓存：键 (family, 逻辑字号 bits, 字重)，与软引擎 `DWriteEngine::format` 同构。
    /// IDWriteTextFormat 亦 device-independent，设备丢失无需清空。对齐不进 key（在 layout 上设）。
    format_cache: HashMap<(String, u32, u16), IDWriteTextFormat>,
    /// 文字 layout 缓存（键 `LayoutKey`）：`IDWriteTextLayout` 是重对象（字形整形/换行排版），
    /// 每帧每文字重建会 churn 驱动内存——官方文档明确要复用。device-independent，设备丢失无需清。
    layout_cache: HashMap<LayoutKey, IDWriteTextLayout>,
    /// 图片位图缓存：键 = `Image::cache_id()`（底层 `Rc<Pixmap>` 指针），值为上传到 GPU 的
    /// `ID2D1Bitmap1`。绝不每帧重建——图片每帧可画几十次，每次 `CreateBitmap` 会 churn 驱动内存
    /// 到几百 M（与 layout/grad 同源的内存累积坑）。device-**dependent**（绑定 context）：
    /// Task 11 设备丢失重建 context 时必须 `image_cache.clear()`（同 solid/grad_cache）。
    image_cache: HashMap<usize, ID2D1Bitmap1>,
    /// 离屏烘焙用的第二个设备上下文：主帧 BeginDraw 期间主 ctx 禁止 SetTarget，故用独立 ctx
    /// 把「模糊后的阴影」一次性渲到离屏位图缓存。device-dependent，设备丢失随 `*self=fresh` 重建。
    bake_ctx: ID2D1DeviceContext,
    /// 阴影 GPU 模糊效果（lazy，建在 bake_ctx 上）：烘焙时复用，仅 cache-miss 跑一次。
    shadow_effect: Option<ID2D1Effect>,
    /// 烘焙后的阴影位图缓存：键 (w,h,radius,blur,color)，值为含模糊外扩 margin 的成品位图。
    /// 命中后主 ctx 每帧仅 `DrawImage` 合成、**不再跑模糊**（重对象算一次的复用纪律，根治每帧
    /// 重模糊累积驱动内存）。device-**dependent**：设备丢失重建时随 `*self=fresh` 清空。
    shadow_cache: HashMap<ShadowKey, ID2D1Bitmap1>,
    /// 本后端所用共享设备链的代际。设备丢失重建时用于守卫 `invalidate_shared_device`
    /// （多窗下只失效自己那一代，不误清他窗已重建的新设备）。
    device_gen: u64,
    /// 设备丢失标志：`EndDraw`/`Present`/`ResizeBuffers` 返回 RECREATE_TARGET /
    /// DEVICE_REMOVED / DEVICE_RESET 时置真，下一帧 `paint` 据此整体重建（丢弃所有 COM
    /// 对象与缓存，重走 try_create）。
    lost: bool,
    /// 连续重建失败次数。超过 `MAX_RECREATE_FAILS` 时 `paint` 返回 true，由 `WindowState`
    /// 降级为软后端（避免设备永久不可用时无限重试空转）。
    recreate_fails: u32,
}

/// 连续重建失败上限：超过即放弃 GPU、降级软渲染。
const MAX_RECREATE_FAILS: u32 = 3;

/// 文字 layout 缓存键：(family, text, 字号 bits, 字重, maxWidth bits, maxHeight bits)。
type LayoutKey = (String, String, u32, u16, u32, u32);

/// 烘焙阴影缓存键：(宽 px, 高 px, 圆角 px, 模糊 bits, 颜色 rgba)，前三项量化逻辑像素整数。
type ShadowKey = (u32, u32, u32, u32, u32);

/// 渐变画刷缓存键。坐标/颜色量化为整数（×1000 / u8）以便 Hash+Eq。
/// 端点已是逻辑像素（由归一化 × 图元包围盒换算），故同尺寸控件可命中复用。
#[derive(Clone, PartialEq, Eq, Hash)]
struct GradKey {
    /// 0 = 线性，1 = 径向。
    kind: u8,
    /// 线性：(start, end)；径向：(center, (radius*1000, 0))。单位 1/1000 逻辑 px。
    a: (i32, i32),
    b: (i32, i32),
    /// 量化色标：(offset×1000, rgba)。
    stops: Vec<(i32, u32)>,
}

/// 线程局部共享的设备链（device 级、与具体窗口无关）。同一 UI（STA）线程的所有 D2D 窗口
/// 共用一份，避免每窗各建一套 D3D/D2D 设备造成 ×N 内存。COM 对象非 `Send`/`Sync`，故用
/// `thread_local!` 而非 `OnceLock`（后者要求 `Sync`）——windui 消息循环单线程，语义正确且 sound。
#[derive(Clone)]
struct SharedDevice {
    /// D3D11 设备（DXGI/D2D 设备链之源）。
    d3d_device: ID3D11Device,
    /// DXGI 工厂：每窗 `CreateSwapChainForHwnd` 的入口。
    dxgi_factory: IDXGIFactory2,
    /// D2D 工厂（device-independent，设备丢失存活；但被本结构整体重建以保链路一致）。
    d2d_factory: ID2D1Factory1,
    /// D2D 设备：每窗 `CreateDeviceContext` 之源。
    d2d_device: ID2D1Device,
    /// DirectWrite 工厂（device-independent，进程共享系统字体缓存）。
    dwrite_factory: IDWriteFactory,
    /// 代际：每次重建递增。设备丢失失效时用于多窗竞态守卫（只清自己那一代，不误清他人新建的）。
    generation: u64,
}

thread_local! {
    /// 当前线程的共享设备链（懒建）。设备丢失时 `invalidate_shared_device` 清空，下次重建。
    static SHARED_DEVICE: RefCell<Option<SharedDevice>> = const { RefCell::new(None) };
    /// 单调代际计数器（每次建共享设备 +1）。
    static DEVICE_GEN: Cell<u64> = const { Cell::new(0) };
}

/// 取/建本线程共享设备链。已存在则克隆其 COM 引用返回（廉价 AddRef）；否则新建并缓存。
unsafe fn shared_device() -> Option<SharedDevice> {
    SHARED_DEVICE.with(|cell| {
        if let Some(d) = cell.borrow().as_ref() {
            return Some(d.clone());
        }
        let d = create_shared_device()?;
        *cell.borrow_mut() = Some(d.clone());
        Some(d)
    })
}

/// 消息循环结束后显式释放共享设备链，避免推迟到线程析构阶段才释放。
/// ID3D11Device 最后一次 Release 需等 GPU 命令队列排空；DWRITE_FACTORY_TYPE_SHARED 最后一次
/// Release 触发系统字体缓存全局清理——两者在线程析构时延迟可达数秒，显式调用可规避。
pub(super) fn release_shared_device() {
    SHARED_DEVICE.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

/// 设备丢失后失效共享设备：**仅当缓存代际 == 调用方代际**才清空。多窗下首个检测到丢失者清并
/// 重建（代际 +1），其余窗随后检测到丢失时代际已不匹配 → 不误清新设备，转而复用之（最终收敛同一代）。
unsafe fn invalidate_shared_device(gen: u64) {
    SHARED_DEVICE.with(|cell| {
        let stale = cell
            .borrow()
            .as_ref()
            .map(|d| d.generation == gen)
            .unwrap_or(false);
        if stale {
            *cell.borrow_mut() = None;
        }
    });
}

/// 新建设备链（D3D11 硬件设备 → DXGI 工厂 → D2D 工厂/设备 → DirectWrite 工厂）。任一环节失败返回 `None`。
unsafe fn create_shared_device() -> Option<SharedDevice> {
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

    // 2. D3D11 → IDXGIDevice → adapter → IDXGIFactory2（每窗建 swapchain 用）。
    let dxgi_device: IDXGIDevice = d3d_device.cast().ok()?;
    let adapter = dxgi_device.GetAdapter().ok()?;
    let dxgi_factory: IDXGIFactory2 = adapter.GetParent().ok()?;

    // 3. D2D 工厂 → 设备（from IDXGIDevice）。
    let d2d_factory: ID2D1Factory1 =
        D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None).ok()?;
    let d2d_device = d2d_factory.CreateDevice(&dxgi_device).ok()?;

    // DirectWrite 工厂（device-independent；进程共享系统字体缓存）。
    let dwrite_factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED).ok()?;

    let generation = DEVICE_GEN.with(|g| {
        let n = g.get() + 1;
        g.set(n);
        n
    });
    Some(SharedDevice {
        d3d_device,
        dxgi_factory,
        d2d_factory,
        d2d_device,
        dwrite_factory,
        generation,
    })
}

/// 尝试建立 GPU 呈现链路。任一环节失败返回 `None`（调用方回退软后端）。
pub(super) fn try_create(hwnd: HWND, w: i32, h: i32) -> Option<D2DBackend> {
    unsafe { try_create_inner(hwnd, w, h) }
}

unsafe fn try_create_inner(hwnd: HWND, w: i32, h: i32) -> Option<D2DBackend> {
    // 设备链（D3D/D2D/DWrite）取自线程共享单例；本函数只建**每窗私有**资源（swapchain/context/画刷）。
    let shared = shared_device()?;

    // flip-model swapchain（绑定本窗 hwnd）。
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
    let swapchain = shared
        .dxgi_factory
        .CreateSwapChainForHwnd(&shared.d3d_device, hwnd, &desc, None, None)
        .ok()?;

    // 每窗主 ctx + 烘焙 ctx（同 device，位图跨上下文共享）。
    let context = shared
        .d2d_device
        .CreateDeviceContext(Default::default())
        .ok()?;
    let bake_ctx = shared
        .d2d_device
        .CreateDeviceContext(Default::default())
        .ok()?;

    // 可复用纯色画刷（device-dependent）：初始色随意，绘制前会被 SetColor 覆盖。
    let solid = context
        .CreateSolidColorBrush(&d2d_color(Color::rgba(0, 0, 0, 255)), None)
        .ok()?;

    // ClearType 文字抗锯齿：不透明渲染目标，一次设置即可（设备丢失重建 context 后须重设）。
    context.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);

    let mut backend = D2DBackend {
        d3d_device: shared.d3d_device.clone(),
        d2d_factory: shared.d2d_factory.clone(),
        d2d_device: shared.d2d_device.clone(),
        swapchain,
        context,
        target_bitmap: None,
        solid,
        grad_cache: HashMap::new(),
        dwrite_factory: shared.dwrite_factory.clone(),
        format_cache: HashMap::new(),
        layout_cache: HashMap::new(),
        image_cache: HashMap::new(),
        bake_ctx,
        shadow_effect: None,
        shadow_cache: HashMap::new(),
        device_gen: shared.generation,
        lost: false,
        recreate_fails: 0,
    };
    // 初次绑定后备缓冲为 target。绑定失败同样回退软后端。
    backend.bind_target().ok()?;
    Some(backend)
}

impl D2DBackend {
    /// 把 swapchain 后备缓冲包成 D2D 位图并设为渲染 target。
    /// **缓存复用**：仅在无缓存（首帧或 resize 后）时 `CreateBitmapFromDxgiSurface`，否则直接
    /// `SetTarget` 复用——官方文档要求建一次、仅 resize 重建，避免每帧重建的开销与驱动内存累积。
    unsafe fn bind_target(&mut self) -> windows::core::Result<()> {
        if self.target_bitmap.is_none() {
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
            self.target_bitmap = Some(bitmap);
        }
        if let Some(bitmap) = &self.target_bitmap {
            self.context.SetTarget(bitmap);
        }
        Ok(())
    }

    /// 设备丢失后整体重建设备链。成功时用全新后端替换 `self`（所有 COM 对象与缓存重置、
    /// `lost=false`、`recreate_fails=0`），返回 `true`；失败时累加 `recreate_fails` 返回 `false`。
    /// 旧 `self`（含旧 COM 对象）在赋值时被 `Drop` 正常释放。
    unsafe fn try_recreate(&mut self, hwnd: HWND) -> bool {
        // 先失效共享设备链（代际守卫：仅清自己那一代）。下一句 try_create_inner → shared_device
        // 发现缓存空 → 重建一套新设备（代际 +1），本窗及其余丢失窗最终都收敛到这套新设备。
        invalidate_shared_device(self.device_gen);
        let mut rc = windows::Win32::Foundation::RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rc);
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        match try_create_inner(hwnd, w, h) {
            Some(fresh) => {
                *self = fresh;
                true
            }
            None => {
                self.recreate_fails += 1;
                false
            }
        }
    }
}

/// 该 HRESULT 是否表示 D2D/DXGI 设备丢失（需重建整条设备链）。
/// `D2DERR_RECREATE_TARGET`（EndDraw）/ `DEVICE_REMOVED` / `DEVICE_RESET`（Present/ResizeBuffers）。
fn is_device_lost(hr: HRESULT) -> bool {
    hr == D2DERR_RECREATE_TARGET || hr == DXGI_ERROR_DEVICE_REMOVED || hr == DXGI_ERROR_DEVICE_RESET
}

impl WinRenderBackend for D2DBackend {
    fn resize(&mut self, w: i32, h: i32) {
        unsafe {
            // 先解绑 target 并释放缓存位图，释放对旧后备缓冲的全部引用，否则 ResizeBuffers 失败。
            self.context.SetTarget(None);
            self.target_bitmap = None;
            let r = self.swapchain.ResizeBuffers(
                0, // 保持缓冲数
                w.max(1) as u32,
                h.max(1) as u32,
                DXGI_FORMAT_UNKNOWN,     // 保持格式
                DXGI_SWAP_CHAIN_FLAG(0), // 无额外标志
            );
            if let Err(e) = r {
                // 设备丢失：标记 lost，下一帧 paint 整体重建。其余错误调试期断言暴露。
                if is_device_lost(e.code()) {
                    self.lost = true;
                    return;
                }
                debug_assert!(false, "ResizeBuffers 失败: {e:?}");
                return;
            }
            // 重新绑定新尺寸的后备缓冲。
            let _ = self.bind_target();
        }
    }

    unsafe fn paint(&mut self, hwnd: HWND, bg: Color, handler: &mut dyn AppHandler) -> bool {
        // 设备丢失：先尝试整体重建。重建成功则用新设备链继续本帧；连续失败超上限则
        // 返回 true，由 WindowState 降级软后端（避免设备永久不可用时无限重试空转）。
        if self.lost && !self.try_recreate(hwnd) {
            return self.recreate_fails >= MAX_RECREATE_FAILS;
        }
        // 重新绑定 target：覆盖首帧之外、resize 之后等情形，幂等且廉价。
        if self.bind_target().is_err() {
            return false;
        }
        // 客户区物理像素尺寸：与 SkiaBackend 一致，作为 handler.render 的 size 传入
        // （make_canvas 内再用 handler 提供的 scale 应用 SetTransform）。
        let mut rc = windows::Win32::Foundation::RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut rc);
        let size = Size::new(rc.right - rc.left, rc.bottom - rc.top);

        self.context.BeginDraw();
        self.context.Clear(Some(&d2d_color(bg)));
        // 单线程 STA：把 ctx/solid/渐变缓存借给本帧 target，由 handler 渲染控件树。
        // target 在 EndDraw 前 drop，释放对 ctx 的借用（EndDraw/Present 仍需 ctx/swapchain）。
        {
            let mut target = D2DTarget {
                ctx: &self.context,
                solid: &self.solid,
                grad_cache: &mut self.grad_cache,
                dwrite_factory: &self.dwrite_factory,
                format_cache: &mut self.format_cache,
                layout_cache: &mut self.layout_cache,
                image_cache: &mut self.image_cache,
                bake_ctx: &self.bake_ctx,
                shadow_effect: &mut self.shadow_effect,
                shadow_cache: &mut self.shadow_cache,
            };
            handler.render(&mut target, size);
            // 复位变换，避免 SetTransform 的 scale 残留到下一帧的 Clear/绑定。
            self.context.SetTransform(&Matrix3x2::identity());
        }
        // EndDraw 的 out 参数（tag1/tag2）此处不关心。返回 D2DERR_RECREATE_TARGET 等设备丢失
        // 错误时标记 lost，下一帧整体重建；本帧呈现已无意义，提前返回（不 Present、不 Validate，
        // 让 WM_PAINT 再投递触发重建帧）。
        if let Err(e) = self.context.EndDraw(None, None) {
            if is_device_lost(e.code()) {
                self.lost = true;
                return false;
            }
        }
        // Present(1, 0)：与垂直同步对齐，呈现一帧。返回 HRESULT（DXGI_STATUS_OCCLUDED 等成功码
        // 也可能返回，不能当错误），仅设备移除/重置时标记 lost。
        let hr = self.swapchain.Present(1, DXGI_PRESENT(0));
        if is_device_lost(hr) {
            self.lost = true;
            return false;
        }
        // DXGI 呈现路径不走 BeginPaint/EndPaint，必须显式验证整个客户区更新区域，
        // 否则 Windows 持续重投 WM_PAINT → 忙循环、单核 100%，破坏空闲零 CPU 设计。
        // 这让 backend.paint 自包含完成"呈现 + 验证更新区域"契约，与 SkiaBackend 对称。
        let _ = ValidateRect(Some(hwnd), None);
        false
    }
}

/// 一帧的 D2D 渲染目标：借用设备上下文 + 可复用画刷 + 渐变缓存。
/// `make_canvas` 应用 DPI scale 后产出 `D2DCanvas`，交给 handler 绘制控件树。
struct D2DTarget<'a> {
    ctx: &'a ID2D1DeviceContext,
    solid: &'a ID2D1SolidColorBrush,
    grad_cache: &'a mut HashMap<GradKey, ID2D1Brush>,
    dwrite_factory: &'a IDWriteFactory,
    format_cache: &'a mut HashMap<(String, u32, u16), IDWriteTextFormat>,
    layout_cache: &'a mut HashMap<LayoutKey, IDWriteTextLayout>,
    image_cache: &'a mut HashMap<usize, ID2D1Bitmap1>,
    bake_ctx: &'a ID2D1DeviceContext,
    shadow_effect: &'a mut Option<ID2D1Effect>,
    shadow_cache: &'a mut HashMap<ShadowKey, ID2D1Bitmap1>,
}

impl RenderTarget for D2DTarget<'_> {
    fn make_canvas<'a>(
        &'a mut self,
        _engine: &'a mut dyn crate::text::TextEngine,
        scale: f32,
    ) -> Box<dyn Canvas + 'a> {
        // 应用 DPI 缩放：控件树用逻辑坐标绘制，D2D 在此按 scale 放大到物理像素。
        // 漏掉会让 DPI≠1 时内容缩到左上角（与软渲染同源的坑，必须保留）。
        // D2D 自带 DirectWrite 文字栈，忽略软后端的 engine。
        unsafe {
            self.ctx.SetTransform(&Matrix3x2::scale(scale, scale));
        }
        Box::new(D2DCanvas {
            ctx: self.ctx,
            solid: self.solid,
            grad_cache: self.grad_cache,
            dwrite_factory: self.dwrite_factory,
            format_cache: self.format_cache,
            layout_cache: self.layout_cache,
            image_cache: self.image_cache,
            bake_ctx: self.bake_ctx,
            shadow_effect: self.shadow_effect,
            shadow_cache: self.shadow_cache,
            saves: Vec::new(),
            pushed_clips: 0,
            pushed_layers: 0,
            scale,
        })
    }
    // as_pixmap 用 trait 默认 None：GPU 无 pixmap，调用方走全窗重绘。
}

/// 把 `Canvas` 图元绘制到 D2D 设备上下文（逻辑坐标；DPI scale 已由 SetTransform 应用）。
///
/// 本任务实现填充/圆角/描边/线/圆 + 纯色/渐变画刷；文字/图片/阴影/裁剪/层为桩
/// （分别由 Task 8/后续/Task 9/Task 7 补）。
struct D2DCanvas<'a> {
    ctx: &'a ID2D1DeviceContext,
    solid: &'a ID2D1SolidColorBrush,
    grad_cache: &'a mut HashMap<GradKey, ID2D1Brush>,
    /// DirectWrite 工厂（借入）：建 format/layout。
    dwrite_factory: &'a IDWriteFactory,
    /// TextFormat 缓存（借入，可变）：按 (family, 逻辑字号 bits, 字重) 复用。
    format_cache: &'a mut HashMap<(String, u32, u16), IDWriteTextFormat>,
    layout_cache: &'a mut HashMap<LayoutKey, IDWriteTextLayout>,
    /// 图片位图缓存（借入，可变）：按 `Image::cache_id()` 复用 GPU 位图，避免每帧 `CreateBitmap`。
    image_cache: &'a mut HashMap<usize, ID2D1Bitmap1>,
    /// 离屏烘焙阴影用的第二个设备上下文（借入）。
    bake_ctx: &'a ID2D1DeviceContext,
    /// 阴影模糊效果（借入，可变 Option，建在 bake_ctx 上）：lazy 建一次后复用。
    shadow_effect: &'a mut Option<ID2D1Effect>,
    /// 烘焙后的阴影成品位图缓存（借入，可变）：按 (w,h,radius,blur,color) 复用，命中免重模糊。
    shadow_cache: &'a mut HashMap<ShadowKey, ID2D1Bitmap1>,
    /// save() 时记录的裁剪栈深度快照；restore() pop 到该深度。每帧空起。
    saves: Vec<u32>,
    /// 当前已 PushAxisAlignedClip 未配对 Pop 的层数（裁剪栈深度）。
    /// EndDraw 前必须归零（LIFO 平衡），否则 EndDraw 失败。
    pushed_clips: u32,
    /// 当前已 PushLayer 未配对 PopLayer 的合成层数。
    /// 同裁剪：EndDraw 前必须归零，否则层不平衡使 EndDraw 静默返回 Err、整帧丢失。
    pushed_layers: u32,
    /// 当前帧 DPI 缩放因子（物理像素 / 逻辑像素）。由 make_canvas 传入，
    /// 供 Canvas::dpi_scale() 返回，使控件层能将 Len::Px 换算为逻辑值。
    scale: f32,
}

impl D2DCanvas<'_> {
    /// 取/建文字 layout（重对象：字形整形/换行排版）。按 family/text/字号/字重/maxW/maxH 缓存复用，
    /// 避免每帧每文字重建（官方文档明确 layout 须复用，否则连续重绘 churn 驱动内存）。
    /// 对齐**不**进 key——在 `draw_text` 里每次绘制时设（`SetTextAlignment` 是元数据，不重排字形）。
    fn text_layout(
        &mut self,
        text: &str,
        family: Option<&str>,
        size: f32,
        maxw: f32,
        maxh: f32,
    ) -> Option<IDWriteTextLayout> {
        let fam = family.unwrap_or(DEFAULT_FAMILY).to_string();
        let weight = crate::text::current_weight();
        let key: LayoutKey = (
            fam,
            text.to_string(),
            size.to_bits(),
            weight,
            maxw.to_bits(),
            maxh.to_bits(),
        );
        if let Some(l) = self.layout_cache.get(&key) {
            return Some(l.clone());
        }
        let format = self.text_format(family, size)?;
        let text_w = wide(text);
        let layout = unsafe {
            self.dwrite_factory
                .CreateTextLayout(&text_w, &format, maxw, maxh)
        }
        .ok()?;
        // 防无界增长（文本输入/计数器等动态文字）：超阈值整体清空重建。
        if self.layout_cache.len() > 512 {
            self.layout_cache.clear();
        }
        self.layout_cache.insert(key, layout.clone());
        Some(layout)
    }

    /// 取/建图片的 GPU 位图（device-dependent 重对象）。按 `Image::cache_id()`（底层
    /// `Rc<Pixmap>` 指针）缓存复用——绝不每帧 `CreateBitmap`，否则图片每帧画几十次会 churn
    /// 驱动内存到几百 M（与 layout/grad 同源的内存累积坑，参见文件头注释）。
    ///
    /// 源数据为 tiny-skia `Pixmap` 的**预乘 RGBA8**：像素布局与 `DXGI_FORMAT_R8G8B8A8_UNORM`
    /// + `PREMULTIPLIED` 一致（无需 R/B 交换，区别于后备缓冲的 BGRA）。
    fn image_bitmap(&mut self, img: &crate::render::image::Image) -> Option<ID2D1Bitmap1> {
        let id = img.cache_id();
        if let Some(b) = self.image_cache.get(&id) {
            return Some(b.clone());
        }
        let pm = img.pixmap(); // 预乘 RGBA8
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 {
            return None;
        }
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_R8G8B8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            // 96 dpi：位图按其像素尺寸即逻辑尺寸取样（DPI 缩放由 context 的 SetTransform 统一施加）。
            dpiX: 96.0,
            dpiY: 96.0,
            // 普通可绘制位图（非 target）：源数据直接上传，可作 DrawBitmap 的源。
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let size = D2D_SIZE_U {
            width: w,
            height: h,
        };
        // pitch = w*4（每行字节数，RGBA8）。ctx 为 ID2D1DeviceContext，此重载收 PROPERTIES1 出 Bitmap1。
        let bitmap = unsafe {
            self.ctx
                .CreateBitmap(size, Some(pm.data().as_ptr() as *const _), w * 4, &props)
        }
        .ok()?;
        // 防无界增长（如大量一次性图片）：超阈值整体清空重建。
        if self.image_cache.len() > 64 {
            self.image_cache.clear();
        }
        self.image_cache.insert(id, bitmap.clone());
        Some(bitmap)
    }

    /// CPU 光栅一张清晰白色圆角矩形并上传为 GPU 位图（用作 Shadow 效果输入）。**不缓存**——
    /// 只在烘焙 cache-miss 时用一次（成品缓存在 `shadow_cache`，命中后免重建掩膜）。
    /// Shadow 效果只用其 **alpha 通道**生成投影、RGB 由效果的 Color 属性决定，故掩膜恒白。
    /// 逻辑分辨率：DPI 物理化由主 ctx 的 SetTransform 统一施加；阴影本就柔和，放大无碍。
    fn build_mask(&self, wpx: u32, hpx: u32, r: f32) -> Option<ID2D1Bitmap1> {
        // tiny-skia 光栅白色圆角矩形（复用跨平台的 rounded_rect_path，与软路径同源几何）。
        let mut pm = tiny_skia::Pixmap::new(wpx, hpx)?;
        if let Some(path) = crate::render::rounded_rect_path(0.0, 0.0, wpx as f32, hpx as f32, r) {
            let mut sp = tiny_skia::Paint::default();
            sp.set_color_rgba8(255, 255, 255, 255);
            sp.anti_alias = true;
            pm.fill_path(
                &path,
                &sp,
                tiny_skia::FillRule::Winding,
                tiny_skia::Transform::identity(),
                None,
            );
        }
        // 上传：预乘 RGBA8 与 R8G8B8A8_UNORM+PREMULTIPLIED 一致（同 image_bitmap，无需 R/B 交换）。
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_R8G8B8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_NONE,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let size = D2D_SIZE_U {
            width: wpx,
            height: hpx,
        };
        unsafe {
            self.bake_ctx
                .CreateBitmap(size, Some(pm.data().as_ptr() as *const _), wpx * 4, &props)
        }
        .ok()
    }

    /// 取/建可复用的 Shadow 效果实例（lazy，建在 bake_ctx 上）。device-dependent，设备丢失随后端重置。
    fn bake_effect(&mut self) -> Option<ID2D1Effect> {
        if let Some(e) = self.shadow_effect.as_ref() {
            return Some(e.clone());
        }
        let e = unsafe { self.bake_ctx.CreateEffect(&CLSID_D2D1Shadow) }.ok()?;
        *self.shadow_effect = Some(e.clone());
        Some(e)
    }

    /// 离屏烘焙一张「模糊后的阴影」成品位图（含四周 margin 外扩）。在 bake_ctx 上独立 BeginDraw/
    /// EndDraw，与主帧互不干扰。仅 cache-miss 调用一次；成品交由 `draw_shadow` 缓存复用。
    fn bake_shadow(
        &mut self,
        wpx: u32,
        hpx: u32,
        r: f32,
        sigma: f32,
        margin: u32,
        color: Color,
    ) -> Option<ID2D1Bitmap1> {
        let mask = self.build_mask(wpx, hpx, r)?;
        let effect = self.bake_effect()?;
        let (ow, oh) = (wpx + 2 * margin, hpx + 2 * margin);
        // 成品目标位图：TARGET（可作渲染目标）且不含 CANNOT_DRAW（仍可作主 ctx 的 DrawImage 源）。
        let props = D2D1_BITMAP_PROPERTIES1 {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            bitmapOptions: D2D1_BITMAP_OPTIONS_TARGET,
            colorContext: std::mem::ManuallyDrop::new(None),
        };
        let target: ID2D1Bitmap1 = unsafe {
            self.bake_ctx.CreateBitmap(
                D2D_SIZE_U {
                    width: ow,
                    height: oh,
                },
                None,
                0,
                &props,
            )
        }
        .ok()?;
        // Shadow 颜色为直通（非预乘）RGBA f32×4；alpha 调制投影整体不透明度。
        let col = [
            color.r as f32 / 255.0,
            color.g as f32 / 255.0,
            color.b as f32 / 255.0,
            color.a as f32 / 255.0,
        ];
        unsafe {
            effect.SetInput(0, &mask, true);
            let _ = effect.SetValue(
                D2D1_SHADOW_PROP_BLUR_STANDARD_DEVIATION.0 as u32,
                D2D1_PROPERTY_TYPE_FLOAT,
                &sigma.to_le_bytes(),
            );
            let col_bytes =
                std::slice::from_raw_parts(col.as_ptr() as *const u8, std::mem::size_of_val(&col));
            let _ = effect.SetValue(
                D2D1_SHADOW_PROP_COLOR.0 as u32,
                D2D1_PROPERTY_TYPE_VECTOR4,
                col_bytes,
            );
            let out = effect.GetOutput().ok()?;
            self.bake_ctx.SetTarget(&target);
            self.bake_ctx.BeginDraw();
            self.bake_ctx.Clear(None); // 透明底
                                       // 掩膜原点 (0,0) → 成品的 (margin,margin)，模糊向四周外扩填满 margin 边。
            let off = vec2(margin as f32, margin as f32);
            self.bake_ctx.DrawImage(
                &out,
                Some(&off as *const _),
                None,
                D2D1_INTERPOLATION_MODE_LINEAR,
                D2D1_COMPOSITE_MODE_SOURCE_OVER,
            );
            let r = self.bake_ctx.EndDraw(None, None);
            self.bake_ctx.SetTarget(None); // 解绑，释放对成品位图的引用
            r.ok()?;
        }
        Some(target)
    }

    /// 取本次填充用的画刷：无渐变 → 复用 solid 并改色；有渐变 → 按包围盒 (x,y,w,h)
    /// 把归一化端点映射到逻辑坐标，建/取缓存的渐变画刷。返回 `ID2D1Brush`（克隆引用计数，廉价）。
    /// 渐变构造失败时退回纯色（与 SkiaCanvas 一致）。
    fn fill_brush(&mut self, paint: &Paint, x: f32, y: f32, w: f32, h: f32) -> ID2D1Brush {
        match paint.gradient.as_ref() {
            Some(g) => match self.gradient_brush(g, x, y, w, h) {
                Some(b) => b,
                None => self.solid_brush(paint.color),
            },
            None => self.solid_brush(paint.color),
        }
    }

    /// 纯色画刷：复用 solid，改色后向上转型为 `ID2D1Brush`。
    ///
    /// 共享可变陷阱：返回值是 `self.solid`（同一底层 COM 对象）的 cast 克隆，`SetColor`
    /// 改的是该共享对象。**返回值不得跨多次 `SetColor` 持有**——同时持有两个 solid_brush
    /// 返回值再绘制会互相覆盖颜色（都指向同一对象，后一次 SetColor 赢）。当前所有图元都是
    /// "取一次→立即绘制"故安全；Task 7+ 扩展（如同一图元填充+描边混用两种纯色）须遵守此约束，
    /// 否则需各自独立 brush（CreateSolidColorBrush）而非复用。
    fn solid_brush(&self, color: Color) -> ID2D1Brush {
        unsafe { self.solid.SetColor(&d2d_color(color)) };
        self.solid
            .cast()
            .expect("ID2D1SolidColorBrush is ID2D1Brush")
    }

    /// 描边/线用纯色画刷（渐变仅作用于填充，描边退化用 paint.color，与 SkiaCanvas 一致）。
    fn stroke_brush(&self, paint: &Paint) -> ID2D1Brush {
        self.solid_brush(paint.color)
    }

    /// 取/建渐变画刷。归一化端点 × 逻辑包围盒 → 逻辑坐标（与 path 同一变换空间，
    /// SetTransform 的 scale 会统一物理化）。stops<2 或构造失败返回 None。
    fn gradient_brush(
        &mut self,
        g: &Gradient,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> Option<ID2D1Brush> {
        let stops = g.stops();
        if stops.len() < 2 {
            return None;
        }
        let q = |v: f32| (v * 1000.0).round() as i32;
        let rgba = |c: Color| {
            ((c.r as u32) << 24) | ((c.g as u32) << 16) | ((c.b as u32) << 8) | c.a as u32
        };
        let stop_keys: Vec<(i32, u32)> = stops
            .iter()
            .map(|s| (q(s.offset.clamp(0.0, 1.0)), rgba(s.color)))
            .collect();
        let key = match g {
            // 线性：key 用**归一化端点**（位置/尺寸无关）→ 同一渐变样式跨控件/跨位置复用一个画刷，
            // 缓存条目从"渐变元素数"降到"渐变样式数"（~十几个），根治每帧重建 thrash（D2D 内存暴涨主因）。
            Gradient::Linear { start, end, .. } => GradKey {
                kind: 0,
                a: (q(start.0), q(start.1)),
                b: (q(end.0), q(end.1)),
                stops: stop_keys,
            },
            // 径向：半径取 min(w,h) 保圆，无法用单一画刷变换做到位置无关，故 key 保留绝对
            // 中心/半径（ime 中径向极少，不构成 thrash）。
            Gradient::Radial { center, radius, .. } => {
                let (cx, cy) = (x + center.0 * w, y + center.1 * h);
                let r = (radius * w.min(h)).max(0.01);
                GradKey {
                    kind: 1,
                    a: (q(cx), q(cy)),
                    b: (q(r), 0),
                    stops: stop_keys,
                }
            }
        };
        let brush = match self.grad_cache.get(&key) {
            Some(b) => b.clone(),
            None => {
                // 位置无关后缓存条目极少；上限仅防径向异常累积。
                if self.grad_cache.len() > 256 {
                    self.grad_cache.clear();
                }
                let b = self.build_gradient_brush(g, x, y, w, h)?;
                self.grad_cache.insert(key, b.clone());
                b
            }
        };
        // 线性画刷在单位空间 [0,1]² 定义，每次绘制用画刷变换映射到当前控件**逻辑**矩形
        // （DPI scale 由 context 的 SetTransform 再统一施加）。径向画刷已按绝对坐标构造，置单位变换。
        match g {
            Gradient::Linear { .. } => unsafe {
                brush.SetTransform(&Matrix3x2 {
                    M11: w,
                    M12: 0.0,
                    M21: 0.0,
                    M22: h,
                    M31: x,
                    M32: y,
                });
            },
            Gradient::Radial { .. } => unsafe {
                brush.SetTransform(&Matrix3x2::identity());
            },
        }
        Some(brush)
    }

    /// 实际构造渐变画刷（CreateGradientStopCollection → Create{Linear,Radial}GradientBrush）。
    fn build_gradient_brush(
        &self,
        g: &Gradient,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> Option<ID2D1Brush> {
        let d2d_stops: Vec<D2D1_GRADIENT_STOP> = g
            .stops()
            .iter()
            .map(|s| D2D1_GRADIENT_STOP {
                position: s.offset.clamp(0.0, 1.0),
                color: d2d_color(s.color),
            })
            .collect();
        unsafe {
            // ID2D1DeviceContext 重载（6 参）：sRGB 色空间 + 8bpc + 直通 alpha 插值，
            // CLAMP 端点（与 SkiaCanvas 的 SpreadMode::Pad 一致）。
            let coll = self
                .ctx
                .CreateGradientStopCollection(
                    &d2d_stops,
                    D2D1_COLOR_SPACE_SRGB,
                    D2D1_COLOR_SPACE_SRGB,
                    D2D1_BUFFER_PRECISION_8BPC_UNORM,
                    D2D1_EXTEND_MODE_CLAMP,
                    D2D1_COLOR_INTERPOLATION_MODE_STRAIGHT,
                )
                .ok()?;
            let brush: ID2D1Brush = match g {
                Gradient::Linear { start, end, .. } => {
                    // 单位空间 [0,1]² 端点；位置/尺寸由调用处的画刷变换施加（位置无关复用）。
                    let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                        startPoint: vec2(start.0, start.1),
                        endPoint: vec2(end.0, end.1),
                    };
                    self.ctx
                        .CreateLinearGradientBrush(&props, None, &coll)
                        .ok()?
                        .cast()
                        .ok()?
                }
                Gradient::Radial { center, radius, .. } => {
                    let c = vec2(x + center.0 * w, y + center.1 * h);
                    let r = (radius * w.min(h)).max(0.01);
                    let props = D2D1_RADIAL_GRADIENT_BRUSH_PROPERTIES {
                        center: c,
                        gradientOriginOffset: vec2(0.0, 0.0),
                        radiusX: r,
                        radiusY: r,
                    };
                    self.ctx
                        .CreateRadialGradientBrush(&props, None, &coll)
                        .ok()?
                        .cast()
                        .ok()?
                }
            };
            Some(brush)
        }
    }

    /// 取/建 `IDWriteTextFormat`（逻辑字号；DPI scale 由 SetTransform 应用，**不**在此 ×scale）。
    /// 缓存键 (family, 逻辑字号 bits, 字重) 与软引擎 `DWriteEngine::format` 同构，保证两后端字体/字重一致。
    /// family 缺省 `DEFAULT_FAMILY`、字重经线程局部 `current_weight()`，locale 固定 "zh-cn"——皆与软路径同源。
    /// 对齐**不**进 key（在 layout 上设），避免污染缓存的 format。
    fn text_format(&mut self, family: Option<&str>, size: f32) -> Option<IDWriteTextFormat> {
        let fam = family.unwrap_or(DEFAULT_FAMILY).to_string();
        let weight = crate::text::current_weight();
        let key = (fam.clone(), size.to_bits(), weight);
        if let Some(f) = self.format_cache.get(&key) {
            return Some(f.clone());
        }
        let dw_weight = if weight == crate::text::WEIGHT_NORMAL {
            DWRITE_FONT_WEIGHT_NORMAL
        } else {
            DWRITE_FONT_WEIGHT(weight as i32)
        };
        let fam_w = wide_nul(&fam);
        let locale = wide_nul("zh-cn");
        let format = unsafe {
            self.dwrite_factory
                .CreateTextFormat(
                    PCWSTR(fam_w.as_ptr()),
                    None,
                    dw_weight,
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size, // 逻辑字号：D2D 变换会放大到物理像素，绝不在此 ×scale
                    PCWSTR(locale.as_ptr()),
                )
                .ok()?
        };
        self.format_cache.insert(key, format.clone());
        Some(format)
    }
}

impl Drop for D2DCanvas<'_> {
    /// 兜底平衡：正常控件树的 save/clip/restore 与 push/pop_layer 本就 LIFO 平衡
    /// （pushed_clips / pushed_layers 归零）。此处防御性清空残留裁剪与层，避免 EndDraw
    /// 因 Push/PopAxisAlignedClip 或 Push/PopLayer 不平衡而（静默）失败、整帧丢失。
    /// 理论上不应触发，触发即上层逻辑漏 restore / pop_layer。
    fn drop(&mut self) {
        debug_assert_eq!(
            self.pushed_clips, 0,
            "EndDraw 前裁剪栈应已平衡（pushed_clips==0），残留说明上层漏 restore"
        );
        while self.pushed_clips > 0 {
            unsafe { self.ctx.PopAxisAlignedClip() };
            self.pushed_clips -= 1;
        }
        debug_assert_eq!(
            self.pushed_layers, 0,
            "EndDraw 前合成层应已平衡（pushed_layers==0），残留说明上层 push_layer/pop_layer 不平衡"
        );
        while self.pushed_layers > 0 {
            unsafe { self.ctx.PopLayer() };
            self.pushed_layers -= 1;
        }
    }
}

impl Canvas for D2DCanvas<'_> {
    fn dpi_scale(&self) -> f32 {
        self.scale
    }

    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, paint: &Paint) {
        let brush = self.fill_brush(paint, x, y, w, h);
        unsafe { self.ctx.FillRectangle(&rect_f(x, y, w, h), &brush) };
    }

    fn fill_round_rect(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, paint: &Paint) {
        let brush = self.fill_brush(paint, x, y, w, h);
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        let rr = D2D1_ROUNDED_RECT {
            rect: rect_f(x, y, w, h),
            radiusX: r,
            radiusY: r,
        };
        unsafe { self.ctx.FillRoundedRectangle(&rr, &brush) };
    }

    fn stroke_round_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        width: f32,
        paint: &Paint,
    ) {
        let width = width.min(w / 2.0).min(h / 2.0).max(0.0);
        let half = width / 2.0;
        // 内缩半个线宽：D2D 描边以路径为中线对称外扩，内缩使描边落在 (x,y,w,h) 框内，
        // 与 SkiaCanvas 的描边几何一致。
        let r = (radius - half).max(0.0);
        let rr = D2D1_ROUNDED_RECT {
            rect: rect_f(x + half, y + half, w - width, h - width),
            radiusX: r,
            radiusY: r,
        };
        let brush = self.stroke_brush(paint);
        unsafe { self.ctx.DrawRoundedRectangle(&rr, &brush, width, None) };
    }

    fn draw_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, paint: &Paint) {
        let brush = self.stroke_brush(paint);
        unsafe {
            self.ctx
                .DrawLine(vec2(x0, y0), vec2(x1, y1), &brush, width, None)
        };
    }

    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, paint: &Paint) {
        // 渐变包围盒为圆的外接正方形（与 SkiaCanvas 一致）。
        let brush = self.fill_brush(paint, cx - r, cy - r, 2.0 * r, 2.0 * r);
        let ellipse = D2D1_ELLIPSE {
            point: vec2(cx, cy),
            radiusX: r,
            radiusY: r,
        };
        unsafe { self.ctx.FillEllipse(&ellipse, &brush) };
    }

    fn draw_shadow(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        blur: f32,
        color: Color,
    ) {
        // 与软路径同源的禁用/退化判定（WINDUI_NOSHADOW、全透明、零尺寸跳过）。
        if color.a == 0 || w <= 0.0 || h <= 0.0 || crate::render::skia::shadows_disabled() {
            return;
        }
        let wpx = w.ceil().max(1.0) as u32;
        let hpx = h.ceil().max(1.0) as u32;
        let r = radius.min(w / 2.0).min(h / 2.0).max(0.0);
        // 标准差：3 趟 box-blur(半径 r) 的方差≈r²+r，故高斯 σ≈blur 与软路径扩散量同量级。
        let sigma = blur.max(0.0);
        // 模糊外扩：高斯 3σ 覆盖 >99%，成品四周各留 margin 像素容纳模糊溢出。
        let margin = (sigma * 3.0).ceil().max(0.0) as u32;
        let rgba = ((color.r as u32) << 24)
            | ((color.g as u32) << 16)
            | ((color.b as u32) << 8)
            | color.a as u32;
        let key: ShadowKey = (wpx, hpx, r.round() as u32, sigma.to_bits(), rgba);
        // 取/烘焙成品（模糊一次、缓存复用）。烘焙失败则跳过本阴影。
        let baked = match self.shadow_cache.get(&key) {
            Some(b) => b.clone(),
            None => {
                let Some(b) = self.bake_shadow(wpx, hpx, r, sigma, margin, color) else {
                    return;
                };
                // 防无界增长（不同尺寸/颜色有限）：超阈值整体清空重建。
                if self.shadow_cache.len() > 128 {
                    self.shadow_cache.clear();
                }
                self.shadow_cache.insert(key, b.clone());
                b
            }
        };
        // 主 ctx 每帧仅合成缓存位图：成品的 (margin,margin) 对应阴影矩形左上，故左上对齐
        // (x-margin, y-margin)，模糊溢出落在边缘（偏移/外扩已由调用方算入 x,y,w,h）。
        let dest = rect_f(
            x - margin as f32,
            y - margin as f32,
            (wpx + 2 * margin) as f32,
            (hpx + 2 * margin) as f32,
        );
        unsafe {
            self.ctx.DrawBitmap(
                &baked,
                Some(&dest),
                1.0,
                D2D1_INTERPOLATION_MODE_LINEAR,
                None,
                None,
            );
        }
    }

    fn draw_image(
        &mut self,
        img: &crate::render::image::Image,
        dst: crate::geometry::Rect,
        fit: crate::render::image::Fit,
        radius: f32,
        opacity: f32,
    ) {
        use crate::render::image::Fit;
        // ★ 全程逻辑坐标：D2D 已 SetTransform(scale)，会把逻辑值放大到物理像素。绝不在此 ×scale
        //   （软路径 SkiaCanvas::draw_image 的 ×scale 是因其直画物理 pixmap、无变换；此处变换统一物理化）。
        let opacity = opacity.clamp(0.0, 1.0);
        if opacity <= 0.0 || dst.is_empty() {
            return;
        }
        let (iw, ih) = (img.width() as f32, img.height() as f32);
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        // 上传/取缓存的 GPU 位图；失败（如尺寸非法/创建失败）直接放弃本图。
        let Some(bitmap) = self.image_bitmap(img) else {
            return;
        };

        let (dw0, dh0) = (dst.w as f32, dst.h as f32);
        // 按 fit 求缩放因子（镜像 SkiaCanvas::draw_image 语义）。None 用 1.0：1 图片像素 = 1 逻辑 dp，
        // DPI 物理化交给 context 的 SetTransform（不像软路径 ×scale，那是因软路径直画物理 pixmap）。
        let (sx, sy) = match fit {
            Fit::Fill => (dw0 / iw, dh0 / ih),
            Fit::Contain => {
                let s = (dw0 / iw).min(dh0 / ih);
                (s, s)
            }
            Fit::Cover => {
                let s = (dw0 / iw).max(dh0 / ih);
                (s, s)
            }
            Fit::None => (1.0, 1.0),
        };
        let (dw, dh) = (iw * sx, ih * sy);
        // 在 dst 框内居中（Cover/None 的溢出由裁剪收口）。
        let tx = dst.x as f32 + (dw0 - dw) / 2.0;
        let ty = dst.y as f32 + (dh0 - dh) / 2.0;
        let dest_rect = D2D_RECT_F {
            left: tx,
            top: ty,
            right: tx + dw,
            bottom: ty + dh,
        };

        // 裁剪到 dst（圆角 radius；Cover/None 溢出由此收口）。
        let r = radius.min(dw0 / 2.0).min(dh0 / 2.0).max(0.0);
        if r <= 0.0 {
            // 矩形裁剪：轴对齐 clip（ALIASED 与软后端整数矩形 mask 边缘一致），廉价。
            let clip = rect_f(dst.x as f32, dst.y as f32, dw0, dh0);
            unsafe {
                self.ctx
                    .PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED);
                self.ctx.DrawBitmap(
                    &bitmap,
                    Some(&dest_rect),
                    opacity,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None, // 整图源
                    None, // 无透视变换
                );
                self.ctx.PopAxisAlignedClip();
            }
        } else {
            // 圆角裁剪：用圆角矩形几何体作 layer 的 geometricMask。圆角图片较少见，
            // 几何体每次创建可接受。TODO：若成热点再按 (dst.w,dst.h,radius) 缓存几何体。
            let rr = D2D1_ROUNDED_RECT {
                rect: rect_f(dst.x as f32, dst.y as f32, dw0, dh0),
                radiusX: r,
                radiusY: r,
            };
            // GetFactory 在 ID2D1Resource（ctx 实现），返回 ID2D1Factory（含 CreateRoundedRectangleGeometry）。
            let geom = unsafe {
                let factory = match self.ctx.GetFactory() {
                    Ok(f) => f,
                    Err(_) => return,
                };
                match factory.CreateRoundedRectangleGeometry(&rr) {
                    Ok(g) => g,
                    Err(_) => return,
                }
            };
            let params = D2D1_LAYER_PARAMETERS1 {
                contentBounds: INFINITE_RECT,
                geometricMask: std::mem::ManuallyDrop::new(Some(geom.into())),
                maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                maskTransform: Matrix3x2::identity(),
                opacity: 1.0,
                opacityBrush: std::mem::ManuallyDrop::new(None),
                layerOptions: D2D1_LAYER_OPTIONS1_NONE,
            };
            unsafe {
                self.ctx.PushLayer(&params, None);
                self.ctx.DrawBitmap(
                    &bitmap,
                    Some(&dest_rect),
                    opacity,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    None, // 整图源
                    None, // 无透视变换
                );
                self.ctx.PopLayer();
            }
        }
    }

    fn draw_text(
        &mut self,
        text: &str,
        rect: crate::geometry::Rect,
        color: Color,
        align: crate::spec::Align,
        family: Option<&str>,
        size: f32,
    ) {
        // ★ 全程逻辑坐标：D2D 已 SetTransform(scale)，会把逻辑值放大到物理像素。
        //   绝不在此 ×scale（软渲染 DWriteEngine::draw 的 ×scale 是因其直画物理 pixmap、无变换）。
        if text.is_empty() || rect.is_empty() {
            return;
        }
        // 横向在 rect.w 内排版/对齐；纵向**不约束**（maxHeight=MAX），由下方按 metrics 手动定位。
        // 纵向不进 layout 的 ParagraphAlignment，避免文本超高时 CENTER 把多行上下对称裁切
        // （用户实测：2 行空间里 3 行被居中、首尾各裁半行）。缓存复用避免每帧重建。
        let Some(layout) = self.text_layout(text, family, size, rect.w as f32, f32::MAX) else {
            return;
        };
        // 水平：与软路径 text_x0 的 Start/Center/End 语义一致（Stretch 同 Start→LEADING）。
        let h_align = match align {
            crate::spec::Align::Start | crate::spec::Align::Stretch => {
                DWRITE_TEXT_ALIGNMENT_LEADING
            }
            crate::spec::Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
            crate::spec::Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
        };
        // 文本块总高（含软/硬换行的全部行），用于纵向定位。
        let mut m = windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_METRICS::default();
        let th = if unsafe { layout.GetMetrics(&mut m) }.is_ok() {
            m.height
        } else {
            0.0
        };
        unsafe {
            let _ = layout.SetTextAlignment(h_align);
            // 顶对齐（纵向位置由 origin.y 控制），配合下方 (h-th).max(0) 实现"装得下居中、装不下顶对齐"。
            let _ = layout.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        }
        // 文字色复用 solid brush（取一次→立即绘制，符合 solid 共享约束）。
        let brush = self.solid_brush(color);
        // 纵向：装得下→垂直居中；装不下→顶对齐（origin 在 rect 顶部，超出部分由上层裁剪收口）。
        // (h-th).max(0)/2 与 SkiaCanvas `oy = y + (h-th).max(0)/2` 完全一致。
        let oy = rect.y as f32 + (rect.h as f32 - th).max(0.0) / 2.0;
        let origin = vec2(rect.x as f32, oy);
        unsafe {
            // ENABLE_COLOR_FONT：让彩色 emoji（如工具栏 😊）正常渲染而非单色轮廓。
            self.ctx.DrawTextLayout(
                origin,
                &layout,
                &brush,
                D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT,
            );
        }
    }

    fn measure_text(
        &mut self,
        text: &str,
        family: Option<&str>,
        size: f32,
    ) -> crate::geometry::Size {
        // 用同一 DirectWrite 工厂建 layout + GetMetrics 返回**逻辑** Size（与软路径一致，
        // 不 ×scale）。失败/空文本回退粗估，保证光标占位与编译。
        if text.is_empty() {
            return crate::geometry::Size::new(0, size.ceil() as i32);
        }
        let fallback = || {
            crate::geometry::Size::new(
                (text.chars().count() as f32 * size * 0.6).ceil() as i32,
                size.ceil() as i32,
            )
        };
        let Some(layout) = self.text_layout(text, family, size, f32::MAX, f32::MAX) else {
            return fallback();
        };
        let mut m = windows::Win32::Graphics::DirectWrite::DWRITE_TEXT_METRICS::default();
        if unsafe { layout.GetMetrics(&mut m) }.is_err() {
            return fallback();
        }
        crate::geometry::Size::new(m.width.ceil() as i32, m.height.ceil() as i32)
    }

    fn push_layer(&mut self, opacity: f32) {
        // 离屏合成层：后续绘制重定向到层，PopLayer 时按 opacity 整体合回父层
        // （子树统一不透明度）。无限 contentBounds 不裁剪层内容（裁剪由 clip 栈负责）。
        let params = D2D1_LAYER_PARAMETERS1 {
            contentBounds: INFINITE_RECT,
            geometricMask: std::mem::ManuallyDrop::new(None),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity: opacity.clamp(0.0, 1.0),
            opacityBrush: std::mem::ManuallyDrop::new(None),
            layerOptions: D2D1_LAYER_OPTIONS1_NONE,
        };
        // layer 传 None：让 D2D 自行分配/复用层资源（device context 重载支持）。
        unsafe { self.ctx.PushLayer(&params, None) };
        self.pushed_layers += 1;
    }

    fn pop_layer(&mut self) {
        // 守卫防溢出（仿 Skia pop_layer 的 if let Some）：仅在有未配对层时 Pop。
        if self.pushed_layers > 0 {
            unsafe { self.ctx.PopLayer() };
            self.pushed_layers -= 1;
        }
    }

    fn save(&mut self) {
        // 记录当前裁剪栈深度，restore() 据此 pop 回此快照。
        self.saves.push(self.pushed_clips);
    }

    fn restore(&mut self) {
        if let Some(target) = self.saves.pop() {
            while self.pushed_clips > target {
                unsafe { self.ctx.PopAxisAlignedClip() };
                self.pushed_clips -= 1;
            }
        }
    }

    fn clip_rect(&mut self, r: crate::geometry::Rect) {
        // 契约：clip_rect 须在 save() 之后（与 restore() 配对），否则裁剪会泄漏。
        debug_assert!(
            !self.saves.is_empty(),
            "clip_rect 必须在 save() 之后调用，以与 restore() 配对"
        );
        // PushAxisAlignedClip 自动与现有 clip 栈求交，无需手算交集。
        // 逻辑坐标：当前 SetTransform(scale) 会对 clip rect 施加缩放（仅缩放不旋转，
        // axis-aligned 仍成立）。ALIASED 与软后端的整数矩形 mask 边缘语义一致。
        let rect = rect_f(r.x as f32, r.y as f32, r.w as f32, r.h as f32);
        unsafe {
            self.ctx
                .PushAxisAlignedClip(&rect, D2D1_ANTIALIAS_MODE_ALIASED)
        };
        self.pushed_clips += 1;
    }
}

/// 合成层的“无限” contentBounds：层内容不被边界裁剪（裁剪交给 clip 栈）。
/// 用足够大的有限值（windows-rs 无 InfiniteRect 辅助常量），±1e7 远超任何窗口尺寸。
const INFINITE_RECT: D2D_RECT_F = D2D_RECT_F {
    left: -1e7,
    top: -1e7,
    right: 1e7,
    bottom: 1e7,
};

/// 中文友好默认字体，与软引擎 `DWriteEngine` 的 `DEFAULT_FAMILY` 同值（两后端字体一致）。
const DEFAULT_FAMILY: &str = "Microsoft YaHei UI";

/// `&str` → UTF-16（不含 NUL），供 `CreateTextLayout`（带长度，不需 NUL）。
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// `&str` → 以 NUL 结尾的 UTF-16，供 `CreateTextFormat`/locale（PCWSTR 需 NUL 终止）。
fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 构造 D2D 矩形（left/top/right/bottom，注意非 x/y/w/h）。
fn rect_f(x: f32, y: f32, w: f32, h: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left: x,
        top: y,
        right: x + w,
        bottom: y + h,
    }
}

/// 逻辑坐标点 → D2D 的 `Vector2`（点/端点统一类型）。
fn vec2(x: f32, y: f32) -> Vector2 {
    Vector2 { X: x, Y: y }
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
