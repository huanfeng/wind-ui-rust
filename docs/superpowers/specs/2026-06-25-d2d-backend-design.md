# Direct2D 渲染后端设计

- 日期：2026-06-25
- 状态：待评审
- 作者：huanfeng（与 Claude 讨论）

## 1. 背景与目标

windui 当前渲染全在 CPU：单份 tiny-skia `Pixmap` 作后备缓冲，Win32 端 R/B 交换后 `SetDIBitsToDevice` 拷屏。这套方案轻量、可移植、空闲零 CPU，但**大不透明窗口在交互/界面变化时的重绘成本不可接受**——根因有二：

1. tiny-skia 单线程 CPU 光栅，大窗口填充 + 文字合成 + 阴影模糊都是 O(像素) 串行。
2. 文字字形位图与阴影 box-blur 都在 CPU。

目标：为**大不透明窗口**引入一个 GPU 加速的 Direct2D 后端，在不破坏现有 tiny-skia 轻量路径的前提下，把填充/渐变/阴影/文字光栅迁到 GPU。

### 硬约束（不可妥协）

- **文字必须继续走 DirectWrite**：Windows 上 DirectWrite 的字形 grid-fitting/ClearType 符合用户习惯，且字体缓存由系统进程共享，进程不自持字形位图。任何"自建字形图集"方案（vello/swash 等）被否决——这也是不选 wgpu/vello 的根本原因。Direct2D 与 DirectWrite 同栈，能在 GPU 上原生绘制 DirectWrite layout，完美满足此约束。

## 2. 范围（v1 边界）

经讨论确认的三个决策：

1. **只接管不透明大窗**：v1 用 flip-model DXGI swapchain（`CreateSwapChainForHwnd`），**跳过 DirectComposition**。透明小窗（浮动工具栏、圆角菜单、未来候选窗）继续走 tiny-skia 软渲染。理由：性能告急的是大不透明设置窗；透明小窗绘制廉价、不卡，且 per-pixel 透明需 DComp，复杂度高、收益低。
2. **窗口级显式 opt-in**：建窗时一个标志（如 `WindowConfig.accelerated`）选择后端，**默认 tiny-skia**。明确、可控、不让小工具意外背上 D3D 内存税。
3. **阴影模糊用 D2D 内置 GPU 效果**：`draw_shadow` 在 D2D 后端改用 `ID2D1Effect`（Shadow / GaussianBlur），GPU 做模糊，直接消除现有手写 box-blur 的 CPU 热点。

### 非目标（v1 明确不做）

- DirectComposition / per-pixel 透明窗的 GPU 化。
- macOS GPU 后端（见 §10）。
- 把 tiny-skia 路径删除或降级——它是默认后端与回退后端，长期保留。
- 多线程软光栅（属于"榨干软渲染"方案 A，与本后端正交，另行评估）。
- dirty-rect 增量呈现优化（v1 用全量 Present；见 §11）。

## 3. 架构：两道接缝

GPU 后端与软后端的差异有两块，绘图只是其一。抽象必须画在**两个层级**：

```
        widget 层（完全不感知后端）
                 │
   缝① trait Canvas   —— 逐图元绘制（已存在，两后端各实现一份）
                 │
   缝② trait Surface  —— 取帧/呈现/resize/设备丢失（新增）
          ┌──────┴──────┐
   tiny-skia 后端      D2D 后端
   Pixmap→R/B→         D3D11+DXGI swapchain
   SetDIBitsToDevice   →ID2D1DeviceContext→Present
```

### 缝①：Canvas（已存在）

`render::Canvas` trait 已是即时逐图元接口（`fill_rect`/`fill_round_rect`/`stroke_round_rect`/`draw_line`/`fill_circle`/`draw_shadow`/`draw_image`/`draw_text`/clip/layer）。D2D 的即时 API 与之几乎逐方法同构：

| Canvas 方法 | D2D 对应 |
|---|---|
| `fill_round_rect` | `FillRoundedRectangle` |
| `stroke_round_rect` | `DrawRoundedRectangle`(strokeWidth) |
| `fill_circle` | `FillEllipse` |
| `draw_line` | `DrawLine` |
| `Brush::Gradient` | `ID2D1LinearGradientBrush` / `RadialGradientBrush` |
| `draw_shadow` | `ID2D1Effect`(Shadow / GaussianBlur) |
| `push_layer(opacity)` / `pop_layer` | `PushLayer`(opacity) / `PopLayer` |
| clip | `PushAxisAlignedClip` / `PopAxisAlignedClip` |
| `draw_text` | `ID2D1DeviceContext::DrawTextLayout` |

新增一个 `D2DCanvas` 实现 `Canvas`，与现有 `SkiaCanvas` 并列。widget 层零改动。

### 缝②：Surface / RenderBackend（新增）

当前**没有**这道缝——`platform/win32` 的 `WindowState` 直接持有 `Pixmap`，`paint()` 里 `handler.render(pixmap, size)`，脏区/呈现逻辑是软渲染专属。这是真正要重构的部分。

引入两层抽象：
- `RenderTarget`（跨平台，render 层）：per-frame 渲染目标，宿主用它 + 自身文字引擎构造 `Canvas`。软实现 `PixmapTarget` 包 `Pixmap`。
- `WinRenderBackend`（win32 层）：后端生命周期 begin/end/resize（+设备丢失）。`SkiaBackend` 重构自现有 present；`D2DBackend` 新增。

`AppHandler::render` 签名从 `&mut Pixmap` 改为 `&mut dyn RenderTarget`。

## 4. 文字：复用 DirectWrite（关键）

DirectWrite 引擎（`text/dwrite.rs::DWriteEngine`）天然可拆成两段：

- **布局/测量（后端无关）**：`factory: IDWriteFactory` + `format()` 格式缓存 + `layout()` 建 `IDWriteTextLayout` + `measure()`(GetMetrics)。两后端共用，测量结果必须一致否则布局错乱——DirectWrite 量出来本就一致。
- **字形光栅（后端专属）**：
  - 软后端：现有 `GlyphRenderer`(自实现 `IDWriteTextRenderer`)→`BitmapRenderTarget`→合成进 Pixmap。保持不变。
  - D2D 后端：`ID2D1DeviceContext::DrawTextLayout(origin, layout, brush)`，DirectWrite layout 直接在 GPU 上绘制，ClearType/系统缓存原生保留。

### COM 共享

`IDWriteFactory` 是 device-independent COM 对象、进程共享系统字体缓存、clone（AddRef）廉价。D2D 后端自持一个 `IDWriteFactory` + format 缓存用于 `DrawTextLayout`，与宿主 measure 路径产出一致（同 family/size/weight DirectWrite 量算确定性一致）。字重经线程局部 `crate::text::current_weight()` 两后端同源。

> ClearType 注意：不透明目标可用 `D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE`。v1 只接不透明窗，恰好满足。

## 5. D2D 后端组成

`platform/win32/d2d.rs`（新增），初始化链：

1. `D3D11CreateDevice`（BGRA support flag；优先硬件，失败返回 None → 调用方落软后端）。
2. `IDXGIDevice` ← D3D11 device；`IDXGIFactory2::CreateSwapChainForHwnd`（flip-model，`DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL`，2 缓冲，BGRA8）。
3. `D2D1CreateFactory` → `ID2D1Device`(from IDXGIDevice) → `ID2D1DeviceContext`。
4. 每帧：从 swapchain 取后备缓冲 `IDXGISurface` → `CreateBitmapFromDxgiSurface` 设为 context target → `BeginDraw` → 画 → `EndDraw` → `Present`。

新增 Cargo `d2d` feature；`windows` crate 增补 `Win32_Graphics_Direct2D`、`Win32_Graphics_Direct2D_Common`、`Win32_Graphics_Direct3D`、`Win32_Graphics_Direct3D11`、`Win32_Graphics_Dxgi`、`Win32_Graphics_Dxgi_Common`。

## 6. 画刷与资源缓存

tiny-skia 的 `Paint` 是每调用廉价构造的值；D2D 的 brush 是 COM 对象，每帧每图元新建很浪费，渐变（`GradientStopCollection` + brush）尤其贵。

- 纯色 brush：一个可复用的 `ID2D1SolidColorBrush`，每次 `SetColor`。
- 渐变 brush：按 paint 参数（stops + 端点 + 类型）key 的缓存（HashMap），复用 `ID2D1GradientStopCollection` + brush。
- 阴影/模糊 effect：按 (尺寸, blur, color) key 缓存（与现有 `SHADOW_CACHE` 思路一致）。

所有 device-dependent 资源（brush/bitmap/effect）在设备丢失时连同 device 一起重建。

## 7. 设备丢失（device loss）

GPU 复位/TDR/RDP 切换/换显示器时，`EndDraw`/`Present` 返回 `D2DERR_RECREATE_TARGET` 或 `DXGI_ERROR_DEVICE_REMOVED`。处理：

1. 丢弃 D3D device、swapchain、D2D device/context 及**全部 device-dependent 资源**（brush/effect/bitmap 缓存清空）。
2. 重新走 §5 初始化链。
3. 若重建连续失败（如无 GPU 可用）→ **降级回 tiny-skia 软后端**，保证不崩。

device-independent 资源（DirectWrite factory/layout、D2D factory）无需重建。

## 8. 后端选择

- API：`WindowConfig` 增 `accelerated: bool`（默认 `false`）。建窗时 `true` 且 `d2d` feature 开启且 D3D 设备创建成功 → 用 D2D 后端；否则 tiny-skia。
- 自动回退条件（即使 opt-in 也强制软）：`GetSystemMetrics(SM_REMOTESESSION)` 为真（RDP）、D3D 设备创建失败、截屏/离屏模式（`run_offscreen` 恒走软）。
- Cargo feature `d2d`：倾向**默认编入、运行时默认 `accelerated=false`**——代码在，用不用由窗口配置定。

## 9. 软后端与离屏路径保留

- tiny-skia 路径重构为 `SkiaBackend`，逻辑等价于现有 `WindowState.pixmap` + `paint()`，仅接口归位。
- `run_offscreen`（截图/CI）**恒走软后端**——离屏渲染要确定性像素、无窗口/无 GPU 上下文。
- 因此截图回归测试天然只测软路径；D2D 视觉正确性需另设手段（见 §11）。

## 10. macOS：接受非对称加速

macOS **没有 D2D 等价物**：CoreGraphics/Quartz 2D 基本是 CPU 光栅（QuartzGL 已废弃），CoreText 是原生文字但不带 GPU 2D；真·GPU 2D 要 Metal（Skia/vello）+ CoreText 字形图集——又踩中"自建字形图集"的否决线。

结论：**D2D 是 Windows 专属加速，macOS 近期停在 tiny-skia**（跨平台软后端）。`RenderTarget`/`Canvas` 抽象正好吸收这种非对称：今天软（跨平台）+ D2D（Win）；将来 mac 若要 GPU，再加第三个后端，widget 层不动。

## 11. 风险与未决

- **呈现路径**：v1 用全量 `Present`，**不做** dirty-rect 增量呈现。flip-model 后备缓冲跨帧不保证保留，每帧全量重绘最稳。脏区增量是独立的后续优化项。
- **D2D 视觉与软渲染的一致性**：圆角/AA/渐变插值/文字位置可能与 tiny-skia 有亚像素差异。接受"两后端像素不必逐一致，但观感一致"。
- **D2D 路径的自动化测试**：离屏截图只覆盖软路径。D2D 正确性 v1 暂靠手动目视；可选探索"D2D 渲染到离屏 bitmap 比对"。
- **内存基线**：D3D 栈带来数十 MB 固定占用（多为共享 DLL，私有增量约 10–40MB，依驱动而定）。这正是默认 `accelerated=false`、按窗 opt-in 的理由。
- **opt-level="z" + LTO**：release 配置为体积优化，D2D 大量 COM 调用的性能需实测确认。

## 12. 验收

- 大不透明设置窗 opt-in D2D 后，交互/界面变化时的帧耗时显著低于软路径（用 `WINDUI_BENCH` 对比）。
- 软路径行为零回归（156+ 测试 + 截图回归全过）。
- 设备丢失能恢复或降级，不崩。
- 透明工具栏/菜单仍走软渲染、外观不变。
