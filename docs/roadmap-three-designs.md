# ROADMAP 三项记账能力设计草案

> 来源：下游 2026-11 反馈的三项记账（见 `docs/ROADMAP.md`「功能能力」表）：外部 GPU 纹理接入、
> 滚动联动、查询主窗真实窗口状态。本文是**设计草案**，不是实现——只定 API 形状、所有权与
> 契约、平台改动点、风险与分期。行号锚点基于 0.34.0，实现时以代码现状为准。

---

## 0. 三项通用的设计边界

1. **平台差异收口在 `platform/` 层**（AGENTS.md §3 铁律 3）：类型定义放平台无关处，实现按
   平台 cfg 分发（先例：`native_window_handle()` / `double_click_thresholds()` / `system_locales()`，
   `src/platform/mod.rs:26-59`）。
2. **Signal 是状态唯一原语**：新增可绑定状态一律走 `Signal<T>`（Copy 句柄 + 全局运行时）。
3. **绝不 panic**：后端与平台层的一切失败（设备建不出、COM 失败、同步超时）退化为
   「跳过 + 一行 debug 日志」（`src/platform/win32/d2d.rs` 模块头是范式）。
4. **错得显眼**：拿不到答案返回「一眼看得出的不知道」，不猜、不编造
   （`WindowState::UNKNOWN` 的注释是范式，`src/event.rs:66-84`）；无法返回错误值的绘制路径
   配探测函数让下游能提前分支。
5. **内存纪律**：新后端对象缓存复用、字节水位与活性淘汰（先例：`Image::cache_id` +
   `ImageLiveness`，`src/render/image.rs:405-447`；`IMAGE_CACHE_MAX_BYTES`）；惰性创建，
   不碰视频的应用零新增开销。
6. **事件外裸写信号不重排**：局部帧不 layout；「外部写入 → 画面落地」必须走通
   「notify → 跨窗口脏 → full frame → 补 layout_root」这条链
   （`src/app/mod.rs:3557-3563`），不能只靠 notify。

---

## 1. 设计一：外部 GPU 纹理接入（视频 / 摄像头预览）

### 1.1 现状与缺口

- `src/platform/mod.rs:32-41` `native_window_handle()`：Windows 返回活跃窗口 HWND，其余平台
  恒 `None`。只给句柄不给设备——下游拿不到内部那台 D3D11 设备。
- `src/platform/win32/d2d.rs:253-255` `SharedDevice`（线程共享单例）持有 `d3d_device: ID3D11Device`
  与 DXGI 工厂；`src/platform/win32/d2d.rs:310-332` `create_shared_device()` 建设备只带
  `D3D11_CREATE_DEVICE_BGRA_SUPPORT`——**没有 `VIDEO_SUPPORT`，也没挂 `ID3D10Multithread`**。
- `src/render/mod.rs:704` `Canvas` trait 有 `draw_image`（`src/render/mod.rs:836-843`），
  但 `Image` 是构建期一次性解码的 `Rc<Pixmap>`（`src/render/image.rs:275-283`）——像素
  **永远在 CPU**。没有「绘制外部 GPU 纹理」的口，视频帧只能 GPU→CPU→GPU 往返。
- 三后端既有图片路径（本设计的接缝）：D2D `src/platform/win32/d2d.rs:1496`（按
  `Image::cache_id` 缓存 `ID2D1Bitmap1`，`src/platform/win32/d2d.rs:105-110`）；wgpu
  `src/render/gpu/canvas.rs:743`（`src/render/gpu/tex.rs:5-17` 按 cache_id+尺寸 LRU）；
  软光栅 `src/render/skia.rs:731`（直接混合进 Pixmap）。
- Windows 的 GPU 档是 D2D（默认 feature）；wgpu 后端（`gpu` feature）只用于离屏测试与
  macOS（`src/render/gpu/mod.rs` 模块头）。

### 1.2 设计决策

#### 1.2.1 设备所有权：惰性独立单例「发布设备」

新建一台**与渲染后端解耦的惰性单例 D3D11 设备**（进程内一台，首次谈到才建）：

- 创建标志：`D3D11_CREATE_DEVICE_VIDEO_SUPPORT`；立即上下文挂
  `ID3D10Multithread::SetMultithreadProtected(TRUE)`。跨线程共用同一立即上下文本就是 UB
  （MF 工作线程经 `IMFDXGIDeviceManager` 用设备是标准场景），多线程保护是官方 MF 互操作
  的前提，代价只是一个临界区。渲染链的 D2D 设备**不加**——它只在 UI 线程用，不为本特性
  引入新行为。
- 建不出（无 GPU）返回 `None`，下游据此退回 CPU 路径。
- **否决「直接暴露 D2D `SharedDevice`」**（备选方案）：默认档是软渲染，软件窗根本没有
  D2D 设备链；D2D 设备丢失整链重建（`docs/DESIGN.md` §8，`*self = try_create(...)`），
  交出去的句柄静默失效还得补「设备换代」通知。独立设备与渲染后端零耦合。
- 内存：惰性建，不碰视频的应用零新增；碰了的多一台设备（约几 MB 驱动侧对象）——这是
  「视频能力」的固有成本。

#### 1.2.2 同步契约：keyed mutex 统一口径

交给 windui 的共享纹理**必须带 `D3D11_RESOURCE_MISC_SHARED_KEYED_MUTEX`**（NT 句柄共享），
同步纪律与设备拓扑无关、一律如此：

- **写侧（下游）**：`AcquireSync(0, INFINITE)` → 写帧 → `ReleaseSync(1)`。
- **读侧（windui，绘制时）**：`AcquireSync(1, 8ms)` → 画 → `ReleaseSync(0)`。超时即跳过
  本次绘制（不画旧内容冒充新帧），UI 线程有界等待，绝不死等。
- 选 keyed mutex 而非 fence：与共享纹理同属一次创建参数，下游一行 flag；MF 经
  `IMFDXGIDeviceManager` 的共享表面本就是 keyed-mutex 纪律，下游用 MF 时零额外心智。
- 两种下游拓扑都成立：**同设备**（下游用 windui 暴露的发布设备跑 MF/自管管线——GPU 队列
  FIFO 天然有序，keyed mutex 是额外的保险）；**异设备**（下游自建设备——NT 句柄跨设备
  共享同一份物理内存，windui 在渲染链自己的设备上 `OpenSharedResource1` 打开，打开是
  别名而非拷贝，零拷贝；此时 keyed mutex 是**必要**的同步原语）。

#### 1.2.3 类型与 Canvas 原语

```rust
// src/render/external.rs（新模块，平台无关）
/// 外部共享纹理的身份：下游视频管线产出的帧的句柄 + 尺寸 + 世代。
/// 下游保证：按 keyed-mutex 契约同步（见模块文档）；格式为预乘 BGRA8 共享纹理
/// （NV12 等视频原生格式由下游先转，色彩转换不进 windui 的业务范围）。
/// Clone 廉价；同一纹理画多次零额外打开成本。
pub struct ExternalTexture {
    pub(crate) handle: isize,    // NT 句柄值（Windows）
    pub(crate) width: u32,       // 纹理物理像素尺寸
    pub(crate) height: u32,
    pub(crate) generation: u64,  // 下游重建纹理时递增，防句柄值复用 ABA
}

impl ExternalTexture {
    pub fn from_shared_handle(handle: isize, w: u32, h: u32, generation: u64) -> Self;
}
```

Canvas trait 增方法（带默认实现，不强迫 trait 实现者；fit/radius/opacity 语义与
`draw_image` 完全一致，`src/render/mod.rs:836-843`）：

```rust
/// 在 dst 内绘制一张外部共享纹理（视频 / 摄像头帧）。不支持的后端为无操作 +
/// debug 一行日志（用 `external_texture_support()` 提前分支，别让用户看黑块）。
fn draw_external_texture(
    &mut self,
    tex: &ExternalTexture,
    dst: Rect,
    fit: Fit,
    radius: f32,
    opacity: f32,
) {
    let _ = (tex, dst, fit, radius, opacity);
}
```

配套探测（三平台同形，先例 `native_window_handle()`）：

```rust
// src/platform/mod.rs
/// 发布设备句柄（VIDEO_SUPPORT + 多线程保护），惰性创建，建不出返回 None。
/// 供下游经 IMFDXGIDeviceManager::ResetDevice 接入 MF，或自管管线直接使用。
#[cfg(windows)]
pub fn video_device_handle() -> Option<isize>;

/// 当前平台 × 渲染后端组合画 `ExternalTexture` 会走哪条路。供下游启动时分支。
pub fn external_texture_support() -> ExternalTextureSupport;

pub enum ExternalTextureSupport {
    /// D2D：打开共享纹理 + DrawBitmap，零拷贝。
    Native,
    /// 软光栅：每帧 GPU→CPU 回读，正确但慢（成本见 1.4）。
    Readback,
    /// wgpu（Windows 非 default、macOS 未接）：本版本不支持，绘制为无操作。
    Unsupported,
}
```

下游接法（示意）：

```rust
match platform::external_texture_support() {
    ExternalTextureSupport::Native => { /* 解码→共享纹理→draw_external_texture */ }
    ExternalTextureSupport::Readback => { /* 可走 GPU 纹理；预览场景可接受；也可全 CPU */ }
    ExternalTextureSupport::Unsupported => { /* 全 CPU：解码到内存 → Image::from_rgba */ }
}
```

#### 1.2.4 三后端实现与降级路径

| 后端 | 实现 | 缓存 | 失败行为 |
|------|------|------|----------|
| D2D（Windows GPU 档） | 渲染链设备上 `OpenSharedResource1` → `CreateBitmapFromDxgiSurface` → `DrawBitmap`；Acquire/Release 夹住本次绘制 | 打开结果按 (句柄, 世代) 缓存，容量与字节水位对齐 `image_cache`；**位图每帧重画**（内容每帧变，位图只是别名） | 打不开/超时 → 跳帧 + debug 日志 |
| wgpu | 本版本不支持（默认实现无操作 + debug 日志）；macOS 将来走 IOSurface/MTLSharedTexture wgpu-hal 互操作，API 已预留 | — | 无操作 |
| 软光栅（默认档） | 惰性建一台**回读设备**（BGRA_SUPPORT 足够，无需 VIDEO_SUPPORT——只开/拷/映）→ 打开（按身份缓存）→ staging 拷贝 → Map → BGRA→预乘 RGBA → tiny-skia Pixmap → 按 `draw_image` 的几何语义混合 | 打开按身份缓存；**像素每帧必须重拷**（内容每帧变，不能按身份缓存内容） | 打不开（无 GPU）→ 无操作 + debug 日志 |

软光栅每帧成本：720p BGRA ≈ 2.7MB 拷贝 + 通道交换，30fps 约 80MB/s 内存带宽——预览窗格
可接受；1080p60 约 500MB/s——视频主窗应用应 opt-in GPU 渲染（`App::renderer(Renderer::Auto)`）。

#### 1.2.5 缓存键与验证要求

- `ExternalTexture` 成为后端缓存键后，按 AGENTS.md §1 必跑非默认档：
  `cargo clippy --no-default-features --all-targets`、`cargo clippy --all-targets --features gpu`、
  `cargo test --features gpu --lib`。
- ABA 防御与 `Image::cache_id`（`src/render/image.rs:405-424`）同源：NT 句柄值可能被内核
  复用，`generation` 由下游维护、随纹理重建递增。淘汰策略：条目上限 + 字节水位 +
  「连续 N 帧没画过」的年龄淘汰（句柄没有进程内对象，不能像图片那样用 `Rc` 弱引用探活）。
- 契约测试（AGENTS.md §5，真实路径不 mock）：`ExternalTexture` 世代语义与探测函数各组合
  返回值的单测可跨平台跑；闭环验证走 `examples/external_texture.rs`（下游视角的最小
  生产者：自建设备写 3 槽 keyed-mutex 环，windui 画进滚动容器），`--screenshot` 在
  Windows 测试机比对 D2D 路径；离屏截图路径（`App::screenshot_from_args`，离屏 D2D 走
  `src/platform/mod.rs:241-273` 的 `Offscreen`）能真画共享纹理，验证闭环成立。

### 1.3 平台层改动点

| 平台 | 做什么 |
|------|--------|
| Windows | `d2d.rs` 增惰性单例发布设备（VIDEO_SUPPORT + multithread）；`video_device_handle()` / `external_texture_support()`；`D2DCanvas::draw_external_texture`（打开缓存 + keyed mutex + DrawBitmap）；`SkiaCanvas::draw_external_texture`（回读设备 + staging + Map）；windows crate 增 feature：keyed-mutex 相关 DXGI 项、`Win32_Graphics_Direct3D10`（ID3D10Multithread） |
| macOS | 类型与 trait 方法编译通过；`external_texture_support()` 恒 `Unsupported`；IOSurface/MTLSharedTexture 互操作另起一轮（API 已预留） |
| Linux | 同 macOS：类型编译通过 + 恒 `Unsupported`；将来 VK_KHR_external_memory 互操作另起一轮 |

### 1.4 风险与取舍

| 取舍 | 得 | 失 |
|------|----|----|
| 独立发布设备（否决共用 D2D SharedDevice） | 与渲染后端零耦合（D2D 设备丢失互不牵连）；软渲染默认档下也存在 | 视频应用多一台设备的驱动侧内存（惰性建，不碰为零）；同设备 FIFO 同步优惠没了，同步全靠 keyed mutex |
| keyed mutex 统一契约 | 拓扑无关、MF 兼容、UI 线程有界等待（8ms 跳帧） | 每帧两次内核级 Acquire/Release（微秒级）；下游多一个创建 flag 的义务 |
| 只收预乘 BGRA8 | windui 不进色彩转换业务；绘制语义与 draw_image 完全一致 | NV12 原生格式下游必须先转（MF VideoProcessor / 自管 shader），下游多一步 |
| 软光栅实现真回读 | 三后端都有**正确**降级路径（不是黑块） | 每帧 GPU→CPU 拷贝成本固有且显著；靠探测函数让下游提前分支 |
| generation 防 ABA | 与 `Image::cache_id` 教训同源（句柄值复用） | 下游多维护一个计数器；漏了症状是「画成上一张纹理」，文档写死 |

### 1.5 分期建议

- **一期（可独立先做）**：发布设备 + `video_device_handle()` + `external_texture_support()`；
  D2D 路径 + `Canvas::draw_external_texture`（默认实现无操作）+ `examples/external_texture.rs`
  生产者-消费者闭环。验收 = Windows 真机截图比对。落地即可把 ROADMAP 该行改「已交付（D2D）」。
- **二期**：软光栅回读路径；`ExternalTexture` 进 prelude；缓存水位与年龄淘汰。
- **三期（另起一轮）**：macOS IOSurface / Linux Vulkan 互操作；wgpu 后端支持；NV12 直收
  （下游实测确有需要才做）。

---

## 2. 设计二：滚动联动（声明式）

### 2.1 现状与缺口

- `src/core.rs:831` `Node.scroll_y: i32`：滚动偏移是 **Node 字段**，不是控件内部状态——绑定
  可以做成节点级、控件无关（换掉 `ScrollWidget` 后布局仍是 `Layout::Scroll`，绑定不失效；
  先例见 `src/ui/mod.rs:6361` 附近的说明）。
- `src/core.rs:4392-4415` `Tree::set_scroll_offset` / `set_scroll_y` / `set_scroll_x`：
  **不钳制、不标脏**（文档写明「惯性滑动用，下一帧 arrange 钳制」）。没人出帧时写入静止在
  树里，画面不动。钳制唯一真值点在 `arrange_scroll`（`src/core.rs:2570-2583`，含
  `stick_to_bottom`/头部锚定改写与滚动条预留）。
- 下游现状（ROADMAP 115 行）：每 50ms DFS 内部树找 `Layout::Scroll`、手动跳过浮层内容节点、
  改 `scroll_y`、再 `mark_layout_dirty` + `mark_dirty_all`。三个本质缺口：拿不到稳定身份
  （只能 DFS）、不钳制、标脏是手写两连击。
- 已有的「框架改 scroll_y」先例（本设计的落点参照）：
  - `EventCtx::scroll_by` / `set_scroll`（`src/core.rs:3591-3596`、`3639-3644`）：
    写 `scroll_y` + `mark_layout_dirty`——控件侧的单点入口。
  - `Tree::scroll_into_view`（`src/core.rs:4683`）+ 宿主侧 `apply_reveal_triggers`
    （`src/app/focus.rs:169-176`）：布局后改 scroll_y 并 `needs_relayout` + `needs_full`。
  - `reveal_seen`（`src/core.rs:1119`）的「按信号记已兑现值」规则：版本/值比较防重复兑现
    ——本设计的同步相位直接沿用这套规则。
- 信号系统：`Signal<T>` 是 Copy 句柄（`src/signal.rs:336-375`）；`set` → `notify_changed`
  → 跨窗口脏 + `request_repaint`（`src/signal.rs:241-250`）。**没有按信号的订阅回调**
  （Signal Phase 2 未落地，`src/core.rs:3374` 注释确认）——「外部写信号 → 滚动落地」必须
  经过一条帧路径（第 0 节边界 6）。
- 失效链：`scroll_y` 不进布局签名（`src/core.rs:4143-4174`，bounds 进）；消费外部写入改了
  scroll_y → 子节点 bounds 平移 → 签名变化 → `note_layout_signature`
  （`src/app/mod.rs:3539-3555`）自动升整窗。滚动本就是全视口内容位移，整窗帧合理。
- 惯性滑动：`begin_fling` / `step_fling` / `clear_fling`（`src/app/fling.rs:73-154,191-193`）
  每帧直接写 `set_scroll_offset` / `set_over_scroll`，由 `render` 开头的 `step_fling`
  （`src/app/mod.rs:3719`）驱动；任何打断惯性的路径必经 `clear_fling`（AGENTS.md 高频陷阱）。

### 2.2 设计决策

#### 2.2.1 API 形状：Signal<i32>，节点级双向绑定

`scroll_y` 本就是 i32，不造新位置类型（YAGNI）。形状对齐 `split` 的 ratio 信号先例
（`src/ui/mod.rs:3248`）：容器写信号、信号写容器。

```rust
// src/ui/mod.rs。只能链在 Element::scroll()/list_signal() 上（debug 断言，同 stick_to_bottom）
impl Element {
    /// 把滚动位置双向绑定到信号：
    /// - 容器侧变化（滚轮 / 惯性 / 触摸平移 / 钳制 / stick_to_bottom）→ 写回信号；
    /// - 外部写信号 → 下一次布局的滚动同步相位消费（arrange 钳制后生效）。
    /// 消费与写回都做值/版本比较防抖：无事即不标脏。
    /// 多个容器绑同一个信号 = 等偏移联动（linked_scroll 的 v1 形态）。
    pub fn scroll_position(self, s: Signal<i32>) -> Self;
}
```

#### 2.2.2 同步相位：layout_root 开头两遍扫描

`layout_root` 开头（`dispatch_reactive_updates`（`src/core.rs:1422-1440`）之后、measure 之前）
新增滚动同步相位，驱动一张节点注册表 `Tree::scroll_bound: Vec<NodeId>`（登记/剔除机制对齐
`reactive_nodes`，`src/core.rs:1097-1098`）。每帧两遍：

- **Pass1（容器→信号，写回）**：`n.scroll_y != s.get()` 且**信号版本干净**（版本号等于上次
  见到的值）→ `s.set(n.scroll_y)`；随后记录新版本。「版本干净」区分「容器自己滚了」与
  「外部刚写入尚未消费」：版本变了说明信号是新的、节点是旧的，此时容器**不覆盖**它。
- **Pass2（信号→容器，消费）**：版本变了 → `n.scroll_y = s.get()`（钳制留给本帧 arrange），
  更新已见版本。

为此 `Signal` 增 `pub(crate) fn version() -> u64`（slot 版本号已存在，`src/signal.rs:731`，
只是没暴露）。

一句话判据：**版本脏 → 只消费不写回；版本干净且值不等 → 写回**。

**触发时机**：只在 layout 路径跑。外部写信号 → notify → 跨窗口脏 → full frame →
`prepare_full_frame` 补 `layout_root`（`src/app/mod.rs:3557-3563`）——这条链就是第 0 节
边界 6，缺它外部写入会静止在信号里无人消费。

#### 2.2.3 钳制与标脏的职责划分

- **钳制恒归 arrange**（`src/core.rs:2576-2577`）——唯一真值点不变；同步相位消费时**不钳**，
  宁可下一帧再收敛，也不建立第二处钳制真值。`set_scroll_y` 本身**不加钳制**：惯性撞界
  结算依赖「写超界值 + 下帧 arrange 钳制 + over_scroll 结算」这条链
  （`src/app/fling.rs:96-154`），给它加钳制会破坏撞界回弹。
- **标脏分三路**，各归既有机制，不新造标脏 API：
  1. 外部写信号：`Signal::set` 的 notify → 跨窗口脏 → full frame → 补 layout。
  2. 容器侧滚动（滚轮/滚动条/`ctx.scroll_by`/惯性）：既有 `mark_layout_dirty`
     （`src/core.rs:3591-3596`）。
  3. 同步相位消费后的视觉落地：scroll_y 变化 → 子节点 bounds 平移 → 布局签名变化 →
     自动升整窗——不需要新标脏代码。
- **写回点只有 Pass1**（每帧 layout 前）。钳制/stick_to_bottom 在 arrange 里的改写由下一帧
  Pass1 广播（钳制传播晚 1-2 帧，见风险表）。**不在 arrange 里写信号**——layout 期间写信号
  的 notify 自通知（`request_repaint`）会让滚动期间每帧多一帧空转帧，代价不值（见风险表）。

#### 2.2.4 两遍扫描为什么同帧收敛、且顺序无关

- A 滚到 100、B 绑同一信号，无论 A、B 在注册表里的先后：
  Pass1：值有变化的一方写 s（另一方值相等不写）；写回使版本变脏 → 另一方 Pass1 跳过写回；
  Pass2：另一方消费 s。**同帧收敛**，与扫描顺序无关。
- 外部写 s=200：Pass1 全跳过（版本脏）；Pass2 两个容器都消费 200。同帧。
- 闭环防 ping-pong：A 写回使版本变脏，B 在同一相位内看到脏版本 → 只消费不写回，不会
  A 写→B 写→A 写振荡。
- 钳制传播（外部写 9999、max 1000）：本帧两容器都消费 9999、arrange 各自钳到 1000；
  下一帧 Pass1 广播 1000、再下一帧对齐。晚 1-2 帧，无正确性问题。

#### 2.2.5 linked_scroll（比例 / 偏移联动）

v1 形态：**多个容器绑同一个 `Signal<i32>` = 等偏移联动**（两遍扫描保证同帧收敛）。
比例/偏移联动是 v2：

```rust
impl Element {
    /// 声明联动：本容器跟随 src 信号滚动，位置按 f 映射
    /// （f(src_val, src_max, my_max) → 我的位置）。src_max / my_max 用上一帧布局的
    /// scroll_range（`src/core.rs:4375-4390`，帧间不变）。与 scroll_position 同开时
    /// 以本声明为准。
    pub fn scroll_link(
        self,
        src: Signal<i32>,
        f: impl Fn(i32, i32, i32) -> i32 + 'static,
    ) -> Self;
}
```

v2 才需要解决：闭环检测（A link B、B link A 的 ping-pong 用「本帧已同步集合」兜底）与
src_max 的帧间陈旧度。等偏移联动两者都不需要。

### 2.3 平台层改动点

**无**——滚动是核心层机制，三平台共享。唯一关联事项：触摸惯性走既有 fling/pan 通路，
同步相位只在 layout 前同步值，**不新增任何 `clear_fling` 调用点**，不触碰平台手势状态机。

### 2.4 风险与取舍

| 取舍 | 得 | 失 |
|------|----|----|
| 节点级绑定（Signal<i32> 直绑 Node.scroll_y） | 换控件不失效；与 reactive/reveal 同一套注册表机制 | 将来要位置类型（含 x/y/内容尺寸）时需演进为 ScrollPos——i32 直绑是当前最贴合 Node 字段的形状 |
| 两遍扫描 + 版本判据 | 同帧收敛、顺序无关、天然防 ping-pong | 每帧 layout 前多 O(绑定容器数) 的值/版本比较——绑定容器通常 0-2 个，可忽略 |
| 写回用 `set`（带 notify） | 跨窗口联动正确（另一窗的联动容器经跨窗口脏拿 full frame 补 layout） | 滚动期间每帧多一次空转帧（layout 无 raster）；实测显著则 v1.1 改 `set_quiet` + 同步相位有写动作时宿主补 `needs_full` |
| 钳制传播晚 1-2 帧 | 单一钳制真值点（arrange）不变 | 外部写超界值时联动方晚 1-2 帧对齐；等偏移联动下两容器各自钳制、下一帧对齐，无正确性问题 |
| 惯性中的外部写入赢一帧 | 不动惯性状态机，零新增 `clear_fling` 调用点 | 回弹进行中外部写入的视觉毛刺（回弹继续回 0）；取消惯性不是绑定的职责，文档写明 |
| 不动 `set_scroll_y` 本身 | 惯性撞界结算链不变 | 旧行为（不钳制不标脏）保留是刻意的；新 API 才是出口，文档互相指路 |
| 测试覆盖 Wrap 宽与显式宽 | 符合 AGENTS.md §5「宽度不能只测一种」 | 无——横向滚动绑定的测试矩阵两档宽度各一条用例 |

### 2.5 分期建议

- **一期**：`scroll_position` 绑定 + 同步相位（两遍扫描 + 版本判据）+ `Signal::version()`
  （pub(crate)）+ 契约单测：真实 `dispatch` 滚轮路径写回、外部写信号消费、双容器同信号
  联动同帧收敛、钳制传播、与 `stick_to_bottom` 共存、与惯性共存（外部写入后惯性从新位置
  继续）。触摸惯性真机手感请用户实测（AGENTS.md §5 纪律）。
- **二期**：`scroll_link`（比例/偏移联动 + 闭环兜底）；`list_signal` 行高变化后的绑定语义。
- 与既有滚动扩展点的边界：`stick_to_bottom` / `preserve_on_prepend` / `on_scroll_edge` 是
  容器**自洽行为**，绑定是容器↔应用的双向通道——两者正交、可同开。

---

## 3. 设计三：查询主窗真实窗口状态

### 3.1 现状与缺口

- `src/event.rs:44-64` `WindowState`（maximized/minimized/visible/maximizable/minimizable）；
  `src/event.rs:73-84` `UNKNOWN`（visible=true 是刻意的：能力位全 false 失败模式是「菜单全灰」
  ——一眼看得见；visible 取 false 的失败模式是「热键切换毫无反应」——无声）。
- 注入机制：平台推送（`AppHandler::on_window_state`，`src/platform/mod.rs:1303-1311`，trait
  文档写明**单向推送而非让宿主回查**）→ UiHost 缓存（`src/app/mod.rs:4371-4372`）→
  `UiHost::enter()` 每次分发/绘制前注入线程局部（`src/app/mod.rs:3244-3246`）→
  `event::window_state()` 读（`src/event.rs:148-150`）。
- **「恒为 UNKNOWN」的准确边界**（比 ROADMAP 记账文本更细，实现前必须以此为准）：
  1. **常驻模式**（无主窗）：应用级回调经 `enter_app_scope` 注入 UNKNOWN
     （`src/app/mod.rs:905-912`）——**设计使然**（「残留已销毁窗口的快照比不知道更糟」，
     `src/app/mod.rs:899`）。**有主窗时 `App::on_interval` 并非恒 UNKNOWN**：win32 主窗
     WM_TIMER → `UiHost::on_interval_fired` → `enter()` 注入缓存快照
     （`src/app/mod.rs:4261-4262`；平台侧 `src/platform/win32/mod.rs:2348-2366`）。
  2. **快照维护有真实缺口**：win32 的 push 点只有建窗（`src/platform/win32/mod.rs:1475/1683`）、
     宿主装配（512）与 `WM_SIZE`（`src/platform/win32/mod.rs:2083`，在 SIZE_MINIMIZED 早退
     之前——最小化本身被推到，此处已正确）。**`run_window_op` 的 Show/Hide 分支
     （`src/platform/win32/mod.rs:3190-3216`）调 `ShowWindow` 后不 push，wnd_proc 也没有
     WM_SHOWWINDOW 臂**——`visible` 在显隐转换后**陈旧**：hide_on_minimize 的窗口
     （WM_SIZE 里 push 完 minimized=true + visible=true 之后立即被 SW_HIDE）此后读到的
     visible 恒 true，热键「露着就收起、藏着就唤起」会恒走隐藏分支毫无反应——这正是
     `WindowState::visible` 文档（`src/event.rs:50-58`）声称要服务的场景。
  3. 平台覆盖：macOS 恒不推送（`AppHandler::on_window_state` 默认空实现）；Linux X11/Wayland
     **已推送**（`src/platform/linux/x11.rs:1926-1935`、`src/platform/linux/wayland/mod.rs:963,992`）。
  4. 下游 0.33.0 观察到的「恒 UNKNOWN」最可能是常驻模式（设计使然）或显隐后的陈旧快照。
- 已有查询先例：`window_open()`（`src/event.rs:175-189`）读平台窗口登记表
  （`src/platform/win32/mod.rs:121-192`），任何时机成立——本设计给它加一个邻居。

### 3.2 设计决策：快照为主、同步查询为兜底

**不改 `enter_app_scope` 的 UNKNOWN 注入**（理由仍成立）。三条改动线：

1. **补快照维护缺口**（P0，独立可先做）：win32 wnd_proc 增 `WM_SHOWWINDOW` 臂（前置消息，visible 用 wparam 合成：lparam 0/1/3 档给转换后真值、2/4 档可见性不变保持 live 读，口径见 §3.4 Windows 行）；`query_main_window_state()` 走独立登记的主窗句柄（thread_local `MAIN_HWND`：建窗时写、主窗 WM_DESTROY 清 0、IsWindow 兜底悬空句柄）→ 不经 `app_host()`（热键回调内两个活 `&mut AppHost` 的技术性 UB）
2. **同步查询**：`event.rs` 自由函数 + `platform/mod.rs` 三平台门。Windows/macOS 同步读 OS 真值（win32 的 IsZoomed/IsIconic/IsWindowVisible 不派发消息、macOS 读 NSWindow 属性），任何时机成立；Linux 恒 None（文档写明，加日志是噪音）
3. **WindowState 字段文档修正**：minimized/visible 补快照纪律（仅事件回调/菜单构建/paint 期间有效），指路 `main_window_state()` 仲裁读法

### 3.3 关键代码样例

```rust
// src/event.rs
/// 查询**主窗**此刻的真实窗口状态。平台层同步查询，不走线程局部快照，任何时机成立；
/// UI 线程限定。无主窗（常驻模式）返回 None——「无窗可谈状态」是事实陈述，不是失败。
///
/// 与 `window_state()` 的分工：快照快、事件期内为真值，是首选；本函数是任何时机的
/// 仲裁读法。平台未实现同步查询时恒返回 None（与无主窗的 None 同形，区分靠文档，
/// 不加第二个返回层，也不打日志——查询可能被 on_interval 高频调用，恒定路径上打印是噪音）。
pub fn main_window_state() -> Option<WindowState>;

// src/platform/mod.rs（crate 内部门，三平台同形）
pub(crate) fn query_main_window_state() -> Option<crate::event::WindowState>;
```

### 3.4 平台层改动点

| 平台 | 做什么 |
|------|--------|
| Windows | wnd_proc 增 WM_SHOWWINDOW 臂（wparam 合成口径见 §3.2）→ 推送线收口显隐缺口；query_main_window_state() 走独立登记的主窗句柄（thread_local MAIN_HWND：建窗时写、主窗 WM_DESTROY 清 0、IsWindow 兜底悬空）→ 读数集合与推送同一份（read_window_state），不经 app_host() |
| macOS | P2：windowDidMiniaturize/didMiniaturize/windowDidResize 通知桥到 `on_window_state` 推送（现状「不推送时沿用配置推导快照」是保守但正确，补推送后升级为真实跟踪）；`query_main_window_state()` = NSWindow 属性读（isMiniaturized/isVisible/isZoomed + styleMask），主线程限定 |
| Linux | X11：`_NET_WM_STATE` / WM_STATE round-trip 同步查询（x11rb）；Wayland：无同步查询语义（协议事件驱动），返回最近 configure 快照。推送线已存在，不动 |
| 核心层 | `event.rs` 自由函数 + `platform/mod.rs` 平台门 + `WindowState` 字段文档修正 |

### 3.5 风险与取舍

| 取舍 | 得 | 失 |
|------|----|----|
| UNKNOWN 注入不动 | 快照纪律完整（不会读到已销毁窗口的残值） | 常驻模式 interval 里 `window_state()` 依旧 UNKNOWN——这是设计，出路是 `main_window_state()`（无主窗 = None，诚实） |
| 快照 + 同步查询并存 | 事件期内快照快；任何时机有诚实出口 | 两条读法并存，文档必须写清分工与各自失败模式 |
| WM_SHOWWINDOW push | 一条消息收口显隐缺口，覆盖所有 ShowWindow 来源 | 状态真变才发送——恰好是想要的（无冗余 push），无真实代价 |
| macOS 推送列 P2 | 下游 Windows 优先，收益排序合理 | macOS 自绘标题栏的状态跟随继续靠配置推导快照（保守正确） |
| Wayland 无同步查询 | configure 推送快照就是协议口径的真值 | 「任何时机查询」退化为「最近 configure 的快照」，文档写明 |
| 未实现平台恒 None、不打日志 | 实现零成本；高频调用下恒定路径打印是噪音 | None 有歧义（无主窗 vs 平台没实现）——区分靠文档写明（同 `single_window_open` 口径） |

### 3.6 分期建议

- **P0（独立可先做）**：WM_SHOWWINDOW push 臂（修复 visible 陈旧）+ `main_window_state()`
  （三平台门 + event.rs 自由函数）+ `WindowState` 字段文档修正 + 契约单测（快照与查询的
  一致性主张：推送缺口修复后，事件期内快照 == 查询结果）。真窗口的状态转换只有真窗口能验
  （AGENTS.md §5「只能真机验证」同款纪律），请在 Windows 测试机人工核对显隐/最小化/最大化
  各转换后两个读法的一致性。
- **P2**：macOS `on_window_state` 推送补齐。

---

## 4. 三项总览

三项相互独立、可并行分期：

- **项一**：D2D 路径（一期）→ 软光栅回读（二期）→ macOS/Linux 互操作与 wgpu（三期）。
- **项二**：`scroll_position` 绑定（一期）→ `scroll_link` 比例/偏移联动（二期）。
- **项三**：WM_SHOWWINDOW push + `main_window_state()`（P0）→ macOS 推送补齐（P2）。

三者都不引入新范式：项一是「平台层新原语 + Canvas trait 扩展」（先例：渐变、阴影）；
项二是「Signal 绑定 + Tree 注册表 + layout 前同步相位」（先例：reactive_nodes、reveal_seen、
split 的 ratio）；项三是「平台层自由函数 + 快照纪律修正」（先例：`native_window_handle()`、
`window_open()`）。
