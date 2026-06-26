# windui (Rust) — 架构设计文档

> 轻量 Windows 桌面 GUI 框架。Win32 窗口 DC + GDI 呈现 + tiny-skia 图形 + DirectWrite 文字。
> 目标：做轻量小工具，内存工作集压到 **2–5MB** 量级（对比 Go 版的 15–40MB）。

本设计复用 Go 版 `wind-ui` 被验证过的架构思想（Node 树 + Measure/Arrange/Paint 三阶段 + 脏标记 + Painter/Layout/Handler 策略分离），但用 Rust 惯用法重写，去掉一切运行时与解析开销。

---

## 1. 设计目标与非目标

### 目标
- **极低内存**：无 GC、无运行时；空窗口 + 后备缓冲 < 5MB。
- **空闲零 CPU**：retained 模式，无事件/无脏区时不绘制、不唤醒。
- **高质量中文文本**：DirectWrite 排版 + 灰度抗锯齿。
- **命令式 Builder API**：纯 Rust 代码构建控件树，类型安全、零解析开销。
- **小而清晰**：核心 < 4k 行，MVP 全量 < 6k 行。

### 非目标（至少 MVP 内）
- 跨平台（架构预留 trait 边界，但只实现 Windows）。
- GPU 渲染（CPU 软光栅足够小工具；tiny-skia 即 CPU）。
- 声明式/热加载 UI、动画系统、复杂数据控件（ListView/TreeView 推后）。

---

## 2. 为什么 Rust 版会更轻（根因分析）

Go 版内存偏大**不在架构**，而在 Go runtime：GC 元数据、goroutine 栈（默认 8KB×N）、调度器、运行时反射表，空程序基线就有数 MB，且 GC 会保留峰值堆。

Rust 版消除这些：
| 来源 | Go 版 | Rust 版 |
|------|-------|---------|
| 运行时基线 | 5–15MB | ~0（无 runtime） |
| 节点分配 | GC 堆 + 指针 | Arena `Vec`，一次性扩容 |
| 后备缓冲 | RGBA + 可能多份 | 单份 pixmap，复用 |
| 字体/文本 | gg+freetype 缓存 或 DWrite | DWrite（系统级，进程外共享） |
| 二进制 | Go 静态链接较大 | 裁剪 feature + LTO + strip |

---

## 3. 技术栈与依赖

```toml
[dependencies]
tiny-skia = "0.11"          # CPU 矢量光栅化（路径/填充/描边/抗锯齿/混合）
windows = { version = "0.58", features = [
    "Win32_Foundation",
    "Win32_Graphics_Gdi",                    # DC / DIBSection / BitBlt
    "Win32_Graphics_DirectWrite",            # 文本排版与光栅
    "Win32_UI_WindowsAndMessaging",          # 窗口类 / 消息循环 / WndProc
    "Win32_UI_HiDpi",                        # Per-Monitor DPI v2
    "Win32_UI_Input_KeyboardAndMouse",
    "Win32_System_LibraryLoader",            # GetModuleHandle
    "Win32_System_Com",                      # COM 初始化（DWrite 工厂）
]}

[profile.release]
opt-level = "z"      # 体积优先（小工具不缺这点速度）
lto = true
codegen-units = 1
panic = "abort"      # 去掉 unwind 表，进一步缩体积
strip = true
```

**依赖哲学**：只用 2 个 crate。`windows`（官方 COM 绑定）按 feature 精确裁剪，未用的 API 不进二进制；`tiny-skia` 纯 Rust 无系统依赖。文字不引第三方——直接用系统 DirectWrite，零额外体积与内存。

---

## 4. 总体分层架构

```
┌─────────────────────────────────────────────────────────┐
│  应用层    Application / 主循环 / 窗口管理                  │  app.rs
├─────────────────────────────────────────────────────────┤
│  控件层    Builder API · Widget trait · 布局算法           │  ui/
│            Label Button TextInput CheckBox … Linear/Frame  │
├─────────────────────────────────────────────────────────┤
│  核心层    Arena · Node · 三阶段生命周期 · 脏标记 · 事件分发 │  core
│            MeasureSpec / Dimension / Gravity / Style       │
├─────────────────────────────────────────────────────────┤
│  渲染层    Canvas trait ──► tiny-skia 后端                  │  render/
│            Text trait    ──► DirectWrite 后端              │  text/
├─────────────────────────────────────────────────────────┤
│  平台层    Win32 窗口 · WndProc · DIB+BitBlt 呈现 · DPI     │  platform/windows
└─────────────────────────────────────────────────────────┘
```

模块划分（建议目录）：
```
src/
  lib.rs               公开 API 汇出
  geometry.rs          Point Size Rect Insets Color
  arena.rs             generational arena + NodeId
  node.rs              Node 数据 + 树操作 + 脏标记
  spec.rs              MeasureSpec / Dimension / Gravity / MeasureMode
  style.rs             Style / Theme / 语义色
  event.rs             Event 类型 / 分发 / EventCtx / FocusManager
  app.rs               Application / 主循环入口
  ui/
    mod.rs             Widget trait（measure/paint/event）
    layout/{linear,frame}.rs
    widgets/{label,button,text_input,checkbox,radio,switch,
             slider,dropdown,scrollview,tabs,divider,dialog,panel}.rs
  render/
    mod.rs             Canvas trait + Paint
    skia.rs            tiny-skia 实现
  text/
    mod.rs             TextEngine trait + 测量缓存
    dwrite.rs          DirectWrite COM 实现
  platform/
    mod.rs             PlatformWindow trait + 输入事件类型
    windows/{window,present,dpi}.rs
```

---

## 5. 内存模型：Arena + 代际索引

核心决策：**不使用 `Rc<RefCell<Node>>`**。所有节点存在一个 arena 里，节点间用 `NodeId` 互指。

```rust
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct NodeId { index: u32, generation: u32 }

struct Slot {
    generation: u32,
    data: Option<Node>,     // None = 空槽，可回收复用
}

pub struct Arena {
    slots: Vec<Slot>,
    free: Vec<u32>,         // 空闲槽索引栈
}
```

- **代际**：删除节点时 `generation += 1`，旧 `NodeId` 自然失效（用悬空 id 访问返回 `None`，不会误读新节点）。
- **删除安全**：无需引用计数、无循环引用问题。
- **缓存友好**：节点连续存储，遍历快。
- **树链接**：`Node` 内只存 `parent: Option<NodeId>` 与 `children: Vec<NodeId>`（或首子+兄弟链以省内存，MVP 用 `Vec` 简单）。

```rust
pub struct Node {
    parent: Option<NodeId>,
    children: Vec<NodeId>,
    // 几何（物理像素）
    bounds: Rect,           // 相对父节点
    measured: Size,         // Measure 输出
    padding: Insets,
    margin: Insets,
    // 布局意图
    width: Dimension,       // Px/Match/Wrap/Weight
    height: Dimension,
    gravity: Gravity,
    // 策略对象（命令式 builder 直接塞 widget）
    widget: Box<dyn Widget>,
    style: Style,
    // 脏标记
    flags: NodeFlags,       // DIRTY | LAYOUT_DIRTY | CHILD_DIRTY | VISIBLE | ENABLED
    visibility: Visibility,
}
```

> Go 版把 Painter/Layout/Handler 拆成三个独立策略对象。Rust 版**合并为单一 `Widget` trait**（一个 trait 三个方法），减少 trait object 数量与间接跳转——更省也更直观。容器（布局）也是 Widget，其 `measure/layout_children` 即布局算法。

---

## 6. Node 三阶段生命周期

沿用 Android/Go 版模型，但精简 MeasureSpec：

```rust
pub trait Widget {
    /// 测量自身期望尺寸；容器在此递归测量子节点。
    fn measure(&mut self, ctx: &mut LayoutCtx, node: NodeId, w: MeasureSpec, h: MeasureSpec) -> Size;
    /// 仅容器实现：把已测量的子节点摆到绝对/相对位置（写 child.bounds）。
    fn arrange(&mut self, ctx: &mut LayoutCtx, node: NodeId, bounds: Rect) {}
    /// 绘制自身内容（背景/边框/文字/图形）。子节点由核心递归驱动。
    fn paint(&self, ctx: &PaintCtx, node: NodeId, canvas: &mut dyn Canvas);
    /// 处理命中到本节点的事件，返回是否消费。
    fn on_event(&mut self, ctx: &mut EventCtx, node: NodeId, ev: &Event) -> bool { false }
}
```

`MeasureSpec`（比 Go 版更简）：
```rust
pub enum MeasureMode { Exact, AtMost, Unbounded }
pub struct MeasureSpec { pub mode: MeasureMode, pub size: i32 } // 物理像素
```

**渲染帧管线**（仅在脏时触发）：
```
WM_PAINT / 事件改脏 →
  1. 若 LAYOUT_DIRTY：measure(root, 窗口约束) → arrange(root, 客户区)
  2. 取脏区：dirty_rects（≤8 个）或退化为全屏
  3. 对每个脏矩形：canvas.clip(rect) → 递归 paint（裁剪外早退）
  4. 呈现：pixmap(脏区) ──R/B swap──► DIBSection ──BitBlt──► 窗口 DC
```

---

## 7. 布局系统

### Dimension（尺寸意图）
```rust
pub enum Dimension {
    Px(i32),        // 物理像素（builder 接受逻辑 dp，构建时×scale 落为 Px）
    Match,          // match_parent
    Wrap,           // wrap_content
    Weight(f32),    // LinearLayout 权重
}
```
> DPI 策略：Builder API 对外只谈**逻辑 dp**。`Dimension` 在加入树时按当前 `scale` 折算为物理 `Px` 存储，并保留原始 dp 以便 `WM_DPICHANGED` 时无累积误差地重算（复刻 Go 版 Rescale 思路）。

### Gravity（位图标志）
`START | END | CENTER_H | CENTER_V | CENTER | FILL`，交叉轴对齐与填充。

### LinearLayout（两遍扫描，复刻 Go 版）
1. 第一遍：测量非 weight 子节点，累计主轴尺寸 + 总 weight，交叉轴取 max。
2. 第二遍：剩余空间按 `weight/总weight` 分给 weight 子节点。
3. `arrange`：主轴推进游标，交叉轴按 gravity 对齐；含 margin 与 spacing。

### FrameLayout
所有子节点堆叠同区，容器取最大子尺寸，各子按自身 gravity 定位。用于叠层/居中/overlay。

> MVP 只做 Linear + Frame，足以表达绝大多数小工具界面。Flex/Grid 留作扩展。

---

## 8. 渲染层

### Canvas trait（绘制抽象，平台无关）
```rust
pub trait Canvas {
    fn fill_rect(&mut self, r: Rect, paint: &Paint);
    fn fill_round_rect(&mut self, r: Rect, radius: f32, paint: &Paint);
    fn stroke_rect(&mut self, r: Rect, radius: f32, paint: &Paint);
    fn fill_circle(&mut self, cx: f32, cy: f32, radius: f32, paint: &Paint);
    fn draw_line(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, paint: &Paint);
    fn draw_image(&mut self, img: &ImageRef, dst: Rect);
    fn draw_text(&mut self, layout: &TextLayout, x: f32, y: f32, color: Color);
    // 变换 / 裁剪
    fn save(&mut self); fn restore(&mut self);
    fn translate(&mut self, dx: f32, dy: f32);
    fn clip_rect(&mut self, r: Rect);
}
```

### tiny-skia 后端
- 持有一个 `Pixmap`（RGBA8888 预乘）作为窗口后备缓冲，仅在 resize 时重分配。
- 图形原语直接映射到 tiny-skia 的 `PathBuilder` + `Paint` + `fill_path/stroke_path`。
- 变换/裁剪用 `Transform` + 手动裁剪矩形栈（裁剪外的绘制早退，省光栅）。
- 文字由 `draw_text` 委托给 **Text 层**，把字形位图 alpha-over 合成进 pixmap。

### 像素格式与呈现（关键细节）
- tiny-skia：`RGBA`，预乘。
- GDI `CreateDIBSection`（`BI_RGB`, 32bpp）：内存字节序 `BGRA`。
- 呈现：把 pixmap 脏区拷进 DIB 时做 **R↔B 交换**（SIMD 友好的逐 u32 旋转）。retained 模式下仅脏区参与，空闲零成本。
- DIB + MemDC 跨帧复用，仅窗口尺寸变化时重建。

### GPU 后端（Direct2D，可选 opt-in）

大窗口下软件光栅是 paint-bound（CPU 逐像素填色，几百控件/全窗重绘时掉帧）。为此引入 **Direct2D GPU 后端**，与软路径**并存**、按窗口选择。约束：文字**必须**继续走 DirectWrite（系统字体缓存、ClearType 字形，内存远低于自建字形图集），故选 Direct2D 而非 vello/wgpu。

**两条接缝**（软/GPU 共用）：
- `RenderTarget`（每帧、平台无关）：`make_canvas(engine, scale) -> Box<dyn Canvas>`。软后端把 `Pixmap` 包成 `SkiaCanvas`；GPU 后端自带 DirectWrite 栈、忽略 `engine`，返回 `D2DCanvas`。`as_pixmap()` 默认 `None`——软渲染据此走局部重绘快路，GPU 恒走全窗。
- `WinRenderBackend`（生命周期、win32 私有）：`resize` / `paint(hwnd, bg, handler) -> bool`。`SkiaBackend` 重构自原 present 逻辑（零行为变化）；`D2DBackend` 为 D3D11 + DXGI flip-model swapchain + D2D `DeviceContext`。

**范围（v1）**：仅接管**不透明大窗**（`CreateSwapChainForHwnd` flip-model，**不引入 DirectComposition**）；透明小窗留软渲染。

**后端选择（窗口级显式 opt-in）**：`WindowConfig.accelerated`（默认 `false`）/ `App::accelerated(true)` / 示例 `--accelerated`。即使开启，以下情形**强制软渲染**：RDP 远程会话（`SM_REMOTESESSION`，flip-model 在远程桌面不可用）、离屏截图（`run_offscreen` 走 `Pixmap`）、设备创建失败（`try_create` 返 `None` → 回退，**绝不 panic**）。

**DPI**：D2D 在逻辑坐标绘制，`make_canvas` 时 `SetTransform(scale)` 统一放大到物理像素（不同于软路径直画物理 `Pixmap` 的 ×scale）。

**重对象缓存复用（内存纪律，关键）**：D2D 重对象每帧重建会累积驱动内存（实测可达 190M+）。故全部缓存复用、仅在失效时重建：后备缓冲位图（建一次/仅 resize）、`IDWriteTextLayout`/`IDWriteTextFormat`（按文本/字体键）、图片 GPU 位图（按 `Rc<Pixmap>` 指针键）、渐变画刷（按归一化样式键，位置无关 + 画刷变换）、纯色画刷（复用一个、`SetColor` 改色）。

**文字**：复用 DirectWrite——`D2DCanvas::draw_text` 经 `DrawTextLayout` 用 DirectWrite 栈直接光栅（ClearType + emoji 彩字），measure 与软路径同源、字体/字重一致。

**阴影**：`ID2D1Shadow` GPU 高斯模糊，但**烘焙一次缓存成品**避免每帧重模糊累积内存。流程：CPU（tiny-skia）光栅清晰圆角掩膜 → 第二个 `DeviceContext`（`bake_ctx`）离屏把模糊成品渲到位图（绕开「帧内主 ctx 禁止 `SetTarget`」限制）→ 主 ctx 每帧仅 `DrawBitmap` 合成。成品按 (尺寸/圆角/模糊/颜色) 缓存。

**设备丢失**：`EndDraw`/`Present`/`ResizeBuffers` 返回 `D2DERR_RECREATE_TARGET` / `DXGI_ERROR_DEVICE_REMOVED` / `DEVICE_RESET` 时标记丢失，下一帧整体重建设备链（`*self = try_create(...)`，天然清空全部 device-dependent 缓存）；连续重建失败超上限则 `paint` 返回 `true`，由 `WindowState` 降级为 `SkiaBackend`（进程不崩、内容续渲）。

**内存权衡**：软路径 ime ~19M；D2D ime ~70M（无阴影）、~100M（含暗/亮两主题阴影成品缓存）。换取大窗 GPU 合成的流畅滚动/悬停。v1 用全量 `Present` 不做 dirty-rect 增量（flip-model 后备缓冲跨帧不保证保留），脏区增量为后续优化项。

---

## 9. 文字层（DirectWrite）

### TextEngine trait
```rust
pub trait TextEngine {
    fn layout(&mut self, text: &str, font: &FontDesc, max_width: Option<f32>) -> TextLayout;
    fn measure(&mut self, text: &str, font: &FontDesc) -> Size; // 走缓存
}
pub struct FontDesc { family: String, size: f32, weight: u16, italic: bool }
```

### DirectWrite 实现
- 进程级单例 `IDWriteFactory`（COM `STA`，与 UI 线程绑定）。
- **测量**：`IDWriteTextLayout` → `GetMetrics()`，结果按 `(text, font)` 哈希缓存（小工具文本基本不变，命中率高）。
- **绘制（推荐方案 A：位图合成）**：
  1. `IDWriteGdiInterop::CreateBitmapRenderTarget` 建小块离屏 GDI 位图。
  2. `IDWriteBitmapRenderTarget::DrawGlyphRun` 以 `DWRITE_RENDERING_MODE_NATURAL`（灰度 AA，避免 ClearType 次像素与任意背景合成的麻烦）渲染字形，颜色用文字色。
  3. 读回该位图的 BGRA 像素，按 alpha 覆盖率 **over-blend** 进 tiny-skia pixmap。
- **可选方案 B（未来）**：`IDWriteFontFace::GetGlyphRunOutline` 取轮廓喂给 tiny-skia `PathBuilder`，用于需缩放/旋转的文字。小字号质量不及 A，故非 MVP 默认。

> 缓存策略：`TextLayout`（含每行 glyph run、宽高、基线）按 widget 缓存，文本/字体/宽度变更才重建——绝大多数帧零文本计算。

---

## 10. 平台层（Win32）

### 窗口与消息循环
- `RegisterClassExW`（`CS_HREDRAW|CS_VREDRAW|CS_DBLCLKS`）一次性注册。
- `WndProc`：C ABI `extern "system"` 函数；用 `SetWindowLongPtrW(GWLP_USERDATA)` 把 `*mut WindowState` 绑到 HWND（比全局 map 更快更省）。
- `GetMessageW`/`TranslateMessage`/`DispatchMessageW` 标准阻塞循环——**无消息时线程休眠，零 CPU**。

### 关键消息映射
| 消息 | 处理 |
|------|------|
| `WM_PAINT` | 取脏区 → 跑帧管线 → `present()` → `ValidateRect` |
| `WM_SIZE` | 重建 DIB/pixmap，标 `LAYOUT_DIRTY` + 全屏脏 |
| `WM_DPICHANGED` | 按新 scale `Rescale` 整树，按建议 RECT 重定位窗口 |
| `WM_MOUSEMOVE`/`WM_LBUTTONDOWN`/`UP`/`DBLCLK` | 解析坐标 → 派发 Motion；维护 hover/capture |
| `WM_MOUSEWHEEL` | `ScreenToClient` 换算 → 派发 Scroll |
| `WM_KEYDOWN`/`WM_CHAR` | Tab/Shift+Tab 焦点导航；其余给焦点节点 |
| `WM_DESTROY` | `PostQuitMessage(0)` |

### DPI
启动设 `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`；`GetDpiForWindow` 取 scale；`WM_DPICHANGED` 动态切换。

---

## 11. 事件系统与所有权策略

### 分发（精简为 2 阶段，去掉 Go 版的 Intercept/Dispatch 钩子）
```
1. hit-test：从 root 向下找最深命中节点，记录命中链（只读遍历，得到 Vec<NodeId>）
2. 处理 + 冒泡：从目标向上，依次 on_event，任一消费即停
```
- **指针捕获**：`capture: Option<NodeId>`，拖动期间事件锁定到捕获节点（slider/scrollbar 拖动必需）。
- **Hover 跟踪**：`last_hover`，进入/离开生成 HoverEnter/Exit。
- **焦点**：`FocusManager{ current, order }`，Tab 序遍历可聚焦节点；键盘事件先给焦点节点。

### Rust 所有权难点与解法（重要）
retained 模式下，事件回调既要改 widget 自身状态、又可能要改应用状态、还要请求重绘——这与「arena 已被 &mut 借用」冲突。解法：

- **命中链先算后用**：hit-test 阶段只读借 arena 得到 `Vec<NodeId>`，处理阶段才可变借。
- **`EventCtx` 作受控句柄**：传给 `on_event` 的不是裸 arena，而是 `EventCtx`，提供 `mark_dirty(id)`、`request_focus(id)`、`get_state_mut::<T>(id)`、`set_text(id, ..)` 等安全操作，内部用「取出-执行-放回」打破自借用。
- **应用状态用闭包捕获**：Builder 的 `.on_click(move |ctx| ...)` 闭包可捕获 `Rc<RefCell<AppState>>`（小工具友好），或发出用户消息进 `ctx.commands` 队列由主循环消费（Elm 风格，状态更可控）。MVP 默认支持前者，预留后者。

```rust
pub struct EventCtx<'a> {
    arena: &'a mut Arena,
    pub commands: Vec<Command>,   // request_repaint / focus / close / 用户消息
}
```

---

## 12. Builder API（命令式）

目标：像写普通 Rust 一样搭界面，零解析、类型安全。

```rust
use windui::prelude::*;

fn main() {
    App::new("小工具", 420, 280)
        .content(
            Column::new()                       // = LinearLayout(Vertical)
                .padding(16).spacing(8)
                .child(Label::new("输入名称：").font_size(14.0))
                .child(
                    TextInput::new()
                        .id("name")
                        .placeholder("请输入…")
                        .width(Dimension::Match),
                )
                .child(
                    Row::new().spacing(8)       // = LinearLayout(Horizontal)
                        .child(
                            Button::new("确定")
                                .on_click(|ctx| {
                                    let name = ctx.text_of("name");
                                    println!("hello {name}");
                                }),
                        )
                        .child(Button::new("取消").on_click(|ctx| ctx.close())),
                ),
        )
        .run();
}
```

实现要点：
- 每个 `Xxx::new()` 返回实现 `IntoNode` 的 builder（持有配置 + 回调）。
- `.child(...)` 接受 `impl IntoNode`；`.content(...)`/`.run()` 时统一 `insert` 进 arena，返回 `NodeId`，构建根树。
- `.id("name")` 在一个 `HashMap<&str, NodeId>` 注册，供回调按名取节点。
- 回调统一签名 `FnMut(&mut EventCtx)`，存进对应 widget。

---

## 13. 样式与主题

```rust
pub struct Style {
    pub bg: Option<Color>,
    pub fg: Color,
    pub border: Option<(Color, f32)>,
    pub corner_radius: f32,
    pub font: FontDesc,
}
pub struct Theme { /* 语义色：primary/surface/on_surface/error... */ }
```
- MVP：一个内置浅色主题 + 可整体替换的 `Theme`。
- 不做 XML/样式继承链（Go 版的 StyleRegistry 推后）；命令式 API 下直接在 builder 上覆写即可。

---

## 14. 内存预算（估算）

| 组成 | 估算 |
|------|------|
| 进程基线（无 runtime） | ~0.5–1MB |
| 后备缓冲 pixmap（420×280×4） | ~0.47MB |
| DIB 镜像 | ~0.47MB |
| Arena（数十节点，每节点 ~200B） | < 0.05MB |
| DirectWrite 工厂/缓存 | 系统进程外为主，本进程数百 KB |
| **合计（典型小工具）** | **~2–4MB 工作集** |

随窗口增大主要是缓冲翻倍（线性、可预测）。

---

## 15. 风险与权衡

| 风险 | 影响 | 缓解 |
|------|------|------|
| DirectWrite COM 绑定繁琐 | 工程量集中在 text/dwrite.rs | Phase 1 单独打通并做最小可跑 demo；`windows` crate 已封装 COM 生命周期 |
| ClearType 与透明背景合成 | 文字边缘脏 | 用灰度 AA（NATURAL）而非次像素，over-blend 简单正确 |
| R/B 通道交换开销 | 大窗口每帧拷贝 | 只拷脏区；逐 u32 旋转，编译器自动向量化 |
| Rust 回调 × arena 借用冲突 | API 设计难点 | `EventCtx` 受控句柄 + 命中链先读后写（见 §11） |
| 单线程 STA 限制 | 不能后台线程直接碰 UI | 后台用 channel + `PostMessage` 唤醒 UI 线程（标准做法） |

---

## 附：与 Go 版的取舍对照

| 维度 | Go 版 | Rust 版（本设计） |
|------|-------|------------------|
| 节点内存 | GC + 指针 | Arena + 代际索引 |
| 策略对象 | Painter/Layout/Handler 三分 | 合并为单一 `Widget` trait |
| UI 声明 | XML inflate + 资源引用 | 命令式 Builder（无解析器） |
| 事件分发 | 3 阶段（Dispatch/Intercept/Handle） | 2 阶段（hit-test + 冒泡） |
| 文字 | DirectWrite + FreeType 双后端 | 仅 DirectWrite |
| 图形 | gg（fogleman） | tiny-skia |
| 呈现 | DIB + BitBlt | DIB + BitBlt（相同） |
| 布局 | Linear/Frame/Flex/Grid… | MVP: Linear/Frame |
| 内存 | 15–40MB | 2–5MB（目标） |
