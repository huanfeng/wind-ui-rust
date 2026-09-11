//! 进程级共享 wgpu 设备（对标 `platform/win32/d2d.rs` 的 `SharedDevice`）。
//!
//! 为什么共享：`Instance`/`Adapter`/`Device` 各自对应一份驱动侧对象，多窗口各建一套就是
//! ×N 的显存与句柄占用——这与本项目「极低内存」的核心指标直接冲突。D2D 侧用
//! `thread_local!` 是因为 COM 对象非 `Send`/`Sync`；wgpu 的句柄是 `Send + Sync` 的，故这里
//! 用进程级 `OnceLock`，多窗口（乃至将来的多线程渲染）都能共用同一份。
//!
//! 句柄以 `Arc<SharedGpu>` 交出而非 `&'static`：这与 `SharedDevice` 克隆 COM 引用交给每个
//! 后端的语义一致，也是 [`release_shared_gpu`] 能真正释放的前提——`&'static` 意味着永不回收。

use std::sync::{Arc, OnceLock, RwLock};

/// 共享的 wgpu 设备链。用 [`SharedGpu::get`] 取，克隆 `Arc` 即可分发给多个窗口/离屏目标。
pub struct SharedGpu {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    /// 本设备链的代际：单例每重建一次 +1。
    ///
    /// 存在的唯一理由是**多窗口下的作废竞态**（与 d2d 的 `generation` 同源）：设备丢失时
    /// 每个窗口都会在自己的下一帧发现「画不出来」并要求作废单例，而第一个窗口作废后单例
    /// 立刻被重建成新的一代——其余窗口若照样清一次，清掉的就是别人刚建好的新设备，于是
    /// 整批窗口轮流把对方的设备踢掉，永远收敛不了。带上代际后，[`invalidate_shared_gpu`]
    /// 只作废「我手上这一代」，晚到的请求自然成为空操作，大家最终收敛到同一代。
    generation: u64,
}

/// 单例槽位。`failed` 记住「建不起来」这件事：无 GPU 环境下每次都重新枚举适配器既慢又会
/// 把同一行 stderr 刷屏。
#[derive(Default)]
struct Slot {
    gpu: Option<Arc<SharedGpu>>,
    failed: bool,
    /// 下一次创建要用的代际。只增不减——释放或作废之后重建也拿新号，绝不回收旧号，
    /// 否则「作废我手上那一代」的判据会在号码复用时误伤。
    next_gen: u64,
}

fn slot() -> &'static RwLock<Slot> {
    static SLOT: OnceLock<RwLock<Slot>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(Slot::default()))
}

impl SharedGpu {
    /// 取（必要时懒建）共享设备。**创建失败返回 `None` 而非 panic**：调用方据此回退软后端，
    /// 与 `Renderer::Auto` 的静默回退语义一致；失败原因只在 stderr 留一行，且只留一次。
    pub fn get() -> Option<Arc<SharedGpu>> {
        // 快路：已建好（或已知失败）时只取读锁。
        if let Ok(s) = slot().read() {
            if let Some(gpu) = s.gpu.as_ref() {
                return Some(gpu.clone());
            }
            if s.failed {
                return None;
            }
        }
        let mut s = slot().write().ok()?;
        // 拿到写锁前可能已被别的线程建好，重查一次。
        if let Some(gpu) = s.gpu.as_ref() {
            return Some(gpu.clone());
        }
        if s.failed {
            return None;
        }
        match create(s.next_gen) {
            Some(gpu) => {
                s.next_gen += 1;
                let gpu = Arc::new(gpu);
                s.gpu = Some(gpu.clone());
                Some(gpu)
            }
            None => {
                s.failed = true;
                eprintln!("windui: wgpu 设备创建失败（无可用适配器），GPU 后端不可用");
                None
            }
        }
    }

    /// wgpu 实例。P1 从它建窗口 surface（`create_surface`）。
    pub fn instance(&self) -> &wgpu::Instance {
        &self.instance
    }

    /// 选中的物理适配器。用于查 surface 能力与 `AdapterInfo`（诊断输出）。
    pub fn adapter(&self) -> &wgpu::Adapter {
        &self.adapter
    }

    /// 逻辑设备：一切资源（纹理/缓冲/管线）的创建入口。
    pub fn device(&self) -> &wgpu::Device {
        &self.device
    }

    /// 命令队列：提交 command buffer 与写缓冲。
    pub fn queue(&self) -> &wgpu::Queue {
        &self.queue
    }

    /// 本设备链的代际，见 [`SharedGpu::generation`] 字段说明。传给
    /// [`invalidate_shared_gpu`] 作为「我要作废的是哪一代」的凭据。
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// 显式释放共享设备（对标 `d2d::release_shared_device`）：消息循环结束时调用，把 GPU 侧
/// 资源的回收收口在一个确定的时机，而不是散在各处的 `Drop` 里。
///
/// 只放掉单例持有的那一份引用；若还有窗口/离屏目标持着 `Arc`，真正的释放推迟到最后一份
/// 被丢弃时。释放前等命令队列排空，避免带着未完成的提交析构设备。
pub fn release_shared_gpu() {
    let Ok(mut s) = slot().write() else { return };
    if let Some(gpu) = s.gpu.take() {
        let _ = gpu.device.poll(wgpu::PollType::wait_indefinitely());
    }
    // 释放后允许重新初始化（例如降级后又切回 GPU 档）。
    s.failed = false;
}

/// 作废共享设备：下次 [`SharedGpu::get`] 会重建一套全新的设备链。
///
/// `generation` 是调用方手上那份设备的代际（[`SharedGpu::generation`]）。单例当前的代际
/// 与之不符时**什么也不做**并返回 `false`——说明别人已经重建过了，调用方该做的是取新的
/// 那一份，而不是再清一次（理由见 `SharedGpu::generation` 字段说明）。
///
/// 与 [`release_shared_gpu`] 的分工：那个是「进程要结束了，把 GPU 资源的回收收口在一个
/// 确定时机」，这个是「设备坏了，下一次要新的」——故这里**不**等队列排空（设备已经不
/// 响应时那一等可能永远回不来），也不复位 `failed`（重建失败就该老老实实记住）。
pub fn invalidate_shared_gpu(generation: u64) -> bool {
    let Ok(mut s) = slot().write() else {
        return false;
    };
    match s.gpu.as_ref() {
        Some(g) if g.generation == generation => {
            s.gpu = None;
            // 上一次建成功过，就允许再建——`failed` 只记录「枚举适配器失败」那种硬失败。
            s.failed = false;
            true
        }
        _ => false,
    }
}

/// 建设备链：实例 → 适配器 → 逻辑设备。任一环节失败返回 `None`。
fn create(generation: u64) -> Option<SharedGpu> {
    // `from_env` 让 `WGPU_BACKEND` 等环境变量可以强制后端，便于在同一台机器上分别验证
    // Vulkan/DX12/GL 三条路（与本项目 `WINDUI_D2D`/`WINDUI_PROF` 的诊断风格一致）。
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    // 先要高性能档（有独显时走独显）；拿不到再退软件适配器（lavapipe/WARP）。后者是 CI、
    // 虚拟机与远程桌面上唯一能跑的路——不试它的话，验证会在那些环境里静默跳过，而「跳过」
    // 和「通过」在报告里长得一模一样（d2d 离屏后端的 WARP 回退同理）。
    let adapter = request_adapter(&instance, false).or_else(|| request_adapter(&instance, true))?;
    // downlevel 档 = GLES 3.0 能满足的下限，保证最弱的目标（旧 Linux GL 机器）也建得起来；
    // 只把纹理尺寸上限提到适配器实际能力，否则 2048 的上限连 4K 窗口都放不下。
    let limits = wgpu::Limits::downlevel_defaults().using_resolution(adapter.limits());
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("windui gpu device"),
        // P0 不需要任何可选特性；P1~P3 的图元/文字/图片全在核心能力集内。
        required_features: wgpu::Features::empty(),
        required_limits: limits,
        ..Default::default()
    }))
    .ok()?;
    Some(SharedGpu {
        instance,
        adapter,
        device,
        queue,
        generation,
    })
}

/// 请求适配器。`fallback=true` 时只接受软件实现。wgpu 的请求是 async，本项目无异步运行时，
/// 就地阻塞即可（初始化路径，不在帧循环里）。
fn request_adapter(instance: &wgpu::Instance, fallback: bool) -> Option<wgpu::Adapter> {
    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        force_fallback_adapter: fallback,
        compatible_surface: None,
        ..Default::default()
    }))
    .ok()
}
