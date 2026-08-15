# macOS 移植指南

本文给在 macOS 上接续实现的开发者：说明跨平台架构、缝合层如何分发、每个待填实现点对照的
Win32 实现与应使用的 Cocoa/Core 框架 API，以及推荐的分阶段落地顺序。

> 当前状态：**macOS 后端已落地**（objc2 0.6 + 框架 crates 0.3）。窗口/事件循环、Core Text 文字、
> blit 呈现、HiDPI、光标、滚轮、剪贴板、open_url、文件拖放、无边框窗口、系统托盘、输入法
> （`NSTextInputClient`）均已实现；`cargo build`/`cargo test`（122 通过）/`cargo clippy` 在 macOS 上全绿，
> 截屏回归（`--screenshot`）渲染正确。Windows 侧不受影响（仅把平台无关的 `run_offscreen` 上移为共享函数）。
>
> 仍可改进：通知依赖 `NSUserNotification`（未打包 .app 时系统可能不展示）。
>
> **`AppHandler` 缝合面的对齐状况**（哪些回调 win32 调了而 macOS 没调）：
> `set_ime_composing` / `capture_active` / `on_capture_lost` 已接齐（见 §4.1 与 §8）；
> `on_pan` / `start_fling` / `cancel_fling` 在 macOS 上**刻意不接**（惯性由系统给，见 §4.1 末尾）；
> `take_hotkey_ops` 已接（全局热键走 Carbon `RegisterEventHotKey`，见
> `platform/macos/hotkey.rs`），`HotkeyHandle` 的运行期改绑/启停两平台同步落地。
> 至此 `AppHandler` 的缝合面在两平台**已全部对齐**（除刻意不接的惯性三件套）。

---

## 1. 架构分层与跨平台边界

```
┌─────────────────────────────────────────────────────────┐
│  平台无关层（零改动即跨平台）                              │
│  core / ui / event / anim / theme / geometry / spec      │
│  render (tiny-skia, 纯 Rust 像素图渲染)                   │
│  app::UiHost (实现 AppHandler，驱动渲染与事件分发)         │
└─────────────────────────────────────────────────────────┘
        │ 依赖两个 trait + 一组同形 API（缝合线）
        ▼
┌──────────────────────────┬──────────────────────────────┐
│  AppHandler (platform)   │  TextEngine (text)            │
│  = 窗口/事件循环缝         │  = 文字测量/绘制缝             │
└──────────────────────────┴──────────────────────────────┘
        │ cfg 分发                       │ cfg 分发
        ▼                               ▼
   Windows: platform/win32         Windows: text/dwrite (DirectWrite)
   macOS:   platform/macos   ←填    macOS:   text/coretext  ←填
```

关键事实：
- **渲染完全跨平台**。所有绘制由 `tiny-skia` 在 CPU 上画进一份 `Pixmap`（RGBA8 预乘）。
  平台层只负责把这份 `Pixmap` blit 到屏，以及把 OS 输入翻译成框架事件。
- 上层只依赖 `crate::platform::*` 与 `crate::text::*`，**不直接触碰任何具体后端**。
- `cfg` 分发集中在两处：`src/platform/mod.rs` 与 `src/text/mod.rs`。新增平台只在这两处加分支。

---

## 2. 缝合层如何分发（已就绪，无需改动）

### `src/platform/mod.rs`
```rust
#[cfg(windows)]        pub mod win32;   #[cfg(windows)]        pub use win32::{run, open_url, Tray, ...};
#[cfg(target_os="macos")] pub mod macos; #[cfg(target_os="macos")] pub use macos::{run, open_url, Tray, ...};
```
- `WindowConfig` 是**平台中性**结构，定义在此层；其 `tray` 字段类型按 `cfg` 解析到各后端的 `Tray`。
- `Clipboard` 是 `cfg` 别名：Windows→`WinClipboard`，macOS→`MacClipboard`。

### `src/text/mod.rs`
```rust
#[cfg(windows)]        pub type PlatformTextEngine = DWriteEngine;
#[cfg(target_os="macos")] pub type PlatformTextEngine = CoreTextEngine;
```
`app::UiHost` 持有 `engine: PlatformTextEngine`，调用 `::new()`，故各后端引擎须提供同名 `new()`。

---

## 3. 依赖选型（`Cargo.toml` 已留位，注释待启用）

推荐 **objc2 生态**（活跃维护、类型安全、自动内存管理），优于旧的 `cocoa`/`objc`：

| crate | 用途 |
|---|---|
| `objc2` | Objective-C runtime 绑定基础 |
| `objc2-foundation` | `NSString` / `NSURL` / `NSData` / `NSGeometry` 等 |
| `objc2-app-kit` | `NSApplication` / `NSWindow` / `NSView` / `NSEvent` / `NSPasteboard` / `NSStatusItem` / `NSWorkspace` |
| `objc2-core-text` | `CTLine` / `CTFramesetter`（文字排版） |
| `objc2-core-graphics` | `CGContext` / `CGImage`（位图上下文、blit） |

> 版本以届时 crates.io 最新稳定为准，需在 Mac 上 `cargo build` 确认 feature 名（objc2 各子 crate 的
> 类型是按 feature 开关暴露的，缺哪个类型就在 `features` 里补哪个）。

---

## 4. 待实现点 → Win32 对照 → Cocoa API

### 4.1 窗口与事件循环 — `platform/macos/mod.rs::run`
| 职责 | Win32 现实现 | macOS 应使用 |
|---|---|---|
| 事件循环 | `GetMessageW` 阻塞循环 | `NSApplication::run`，或自管 `NSEvent` 取 + `NSRunLoop` |
| 创建窗口 | `CreateWindowExW` | `NSWindow::initWithContentRect_styleMask_...` |
| 自绘视图 | 窗口类 + `WM_PAINT` | 自定义 `NSView` 子类，重写 `drawRect:` |
| **blit 像素图** | `SetDIBitsToDevice`（R/B 原地交换为 BGRA） | `CGBitmapContext` 包裹 `Pixmap` 缓冲 → `CGImage` → `CGContextDrawImage`。⚠️ CG 坐标 Y 轴向上，需翻转 |
| 请求重绘 | `InvalidateRect` | `NSView::setNeedsDisplay` |
| HiDPI | `GetDpiForWindow` / `WM_DPICHANGED` | `NSWindow::backingScaleFactor` → `handler.set_scale`；监听 `windowDidChangeBackingProperties` |
| 鼠标 | `WM_LBUTTONDOWN`/`MOUSEMOVE`/`MOUSEWHEEL` | `mouseDown:`/`mouseDragged:`/`mouseMoved:`/`scrollWheel:` → `PointerEvent` |
| 指针捕获 | `SetCapture`/`ReleaseCapture` + `capture_active()` | ✅ 已接：AppKit 隐式续派发（`mouseDown:` 后的 `mouseDragged:`/`mouseUp:` 恒送同一 view，拖出窗口外照送），故**不做任何 OS 调用**，只镜像 `capture_active()`；`on_capture_lost` 挂在 `windowDidResignKey:`（对照 `WM_CAPTURECHANGED`） |
| 键盘 | `WM_KEYDOWN` | `keyDown:`，特殊键映射到 `Key` |
| 输入法 | `WM_IME_*` + `ImmSetCompositionWindow`（用 `ime_caret()`） | ✅ 已接：`NSTextInputClient`；`firstRectForCharacterRange:` 用 `ime_caret()` 定位候选窗；合成态经 `setMarkedText:`/`unmarkText`/`insertText:`（+ 窗口失活兜底）上报 `set_ime_composing`。⚠️ 语义差见 §8 |
| 光标形状 | `WM_SETCURSOR`（用 `cursor()`） | `NSView::resetCursorRects` 或 `NSCursor::set`，按 `cursor()` 选 arrow/pointingHand/iBeam |
| 文件拖放 | `WM_DROPFILES`（用 `on_drop_files()`） | `NSDraggingDestination`：`draggingEntered:`/`performDragOperation:` |
| 无边框窗口 | `WM_NCCALCSIZE` + `WM_NCHITTEST`（用 `window_drag_at`/`interactive_at`） | styleMask 去 `titled` + 加 `fullSizeContentView`；自管拖动可重写 `mouseDown` 调 `performWindowDragWithEvent:` |
| 窗口操作 | `take_window_op()` → `ShowWindow(SW_MINIMIZE/MAXIMIZE...)` | `NSWindow::miniaturize`/`zoom`/`close` |

**触摸/惯性（已定案，别再移植 win32 那套）**：触控板抬指后 AppKit 会继续投递
`momentumPhase != None` 的 `scrollWheel:` 事件，动量/摩擦/衰减全由系统按用户的触控板设置算好，
用户重新把手指放上触控板时系统自动中止。故 macOS 后端**只转发原生滚动**（走
`PointerKind::Wheel`，与鼠标滚轮同一条路径），`on_pan` / `start_fling` / `cancel_fling`
一律不调用——win32 的自研惯性状态机是为了补 `WM_TOUCH` 只给位置不给动量的缺口，
移植过来会与系统动量叠加成双倍速度，并在松手瞬间因两套摩擦系数不同而跳一下。
`app/fling.rs` 整套状态机在 macOS 上因此是永不激活的死代码，**不加 `#[cfg]` 门控**：
它是平台无关的 `AppHandler` 实现体，门控只会把跨平台的单元测试一并切掉，而 `ScrollState`
的静息成本是一个恒为 `None` 的 `Option`。动量阶段的事件与用户主动滚动不作区分也是有意的：
下游（`ScrollWidget` / 菜单浮层）对滚轮只有"滚多少"这一个语义。

`AppHandler` 的全部回调签名见 `src/platform/mod.rs`，含坐标单位约定（**物理像素，相对客户区左上角**）。

### 4.2 文字引擎 — `text/coretext.rs::CoreTextEngine`
当前为等宽近似占位（app 可跑、文字尺寸不准、不渲染字形）。替换为：
- `measure`：`CTLine`（单行）/ `CTFramesetter`（`max_width=Some(w)` 时按宽折行）排版后取 typographic bounds。
  **字号须按 `scale` 物理化后排版，再 /scale 回逻辑**——与 `draw` 走同一物理路径（hinting 非线性，禁止线性外推）。
- `draw`：`CGBitmapContext` 包裹 `pixmap` 缓冲，`CTLineDraw`/`CTFrameDraw` 绘入；按 `rect`×`scale`
  物理化定位，水平按 `align`、垂直居中；`clip` 命中时 `CGContextClipToRect`。颜色 `Color`→`CGColor`，注意 Y 翻转。

对照实现：`src/text/dwrite.rs`（DirectWrite 版，含 scale 物理化与裁剪合成的完整思路）。

### 4.3 剪贴板 — `platform/macos/clipboard.rs::MacClipboard`
`NSPasteboard::generalPasteboard()`：读 `stringForType(NSPasteboardTypeString)`；
写 `clearContents()` + `setString_forType(...)`。对照 `win32/clipboard.rs`（`CF_UNICODETEXT`）。

### 4.4 托盘 — `platform/macos/tray.rs`
构建器（`Tray`/`TrayMenuItem`）已是纯数据、**无需改**。待实现：
- 安装：`NSStatusBar::systemStatusBar().statusItemWithLength(...)`，图标用 `NSImage`（由 `icon` 的 RGBA 构造），
  右键菜单 `NSMenu`/`NSMenuItem`（`checked`→`state`，`separator`→`separatorItem`）。
- `TrayCtx::{show_window, hide_window, quit, notify}`：分别 `NSWindow::makeKeyAndOrderFront` /
  `orderOut` / `NSApp::terminate` / `UNUserNotificationCenter`。对照 `win32/tray.rs`。
  注意**不是 1:1 对应**：win32 侧这四个方法只把意图推进 `Vec<TrayAction>`，由平台层在
  释放窗口状态借用后执行（铁律 6）；macOS 无此约束，可立即执行。但签名须同为
  `&mut self`，否则同一段回调代码在两个平台上语义不同。

### 4.5 open_url — `platform/macos/mod.rs::open_url`
`NSWorkspace::sharedWorkspace().openURL(NSURL::URLWithString(url))`。对照 win32 `ShellExecuteW`。

---

## 5. 推荐分阶段顺序

按依赖关系，每阶段都可独立验证（`cargo build` + 跑示例）：

1. **窗口 + blit + 基础鼠标/键盘**（`run` 主体）。先让 `examples/fullshowcase` 出窗口、能点击。
   文字此时用占位引擎（位置对、不渲染字形）也能验证布局与交互。
2. **Core Text**（`coretext.rs`）。文字正确渲染，UI 视觉完整。
3. **HiDPI / 光标 / 滚轮惯性**。Retina 清晰、光标随控件变、触控板顺滑。
4. **输入法（NSTextInputClient）**。中文/emoji 输入与候选窗跟随。
5. **剪贴板 / open_url / 文件拖放**。
6. **托盘（NSStatusItem）**。
7. **无边框窗口**（自定义标题栏示例 `light_titlebar` 等）。

### 建议的小重构（截屏离屏路径）
win32 的 `run_offscreen`（渲染一帧存 PNG、不开窗，供自动化截屏验证）是**平台无关逻辑**，
目前困在 `win32/mod.rs`。建议上移为 `platform` 层共享函数，两端 `run` 在 `cfg.screenshot.is_some()`
时共用——这样 macOS 也能直接用 `--screenshot` 做视觉回归（见 `app::App::screenshot_from_args`）。

---

## 6. 验证清单（每阶段收尾）
- `cargo build` / `cargo build --examples`（macOS）
- `cargo test`（核心测试平台无关，macOS 上应同样全过）
- `cargo clippy --all-targets`
- 截屏回归：`cargo run --example fullshowcase -- --screenshot out.png`（待阶段 1+2 完成后可用）
```

## 7. 未验证的 CoreText 改动（技术债登记）

以下改动**在 Windows 上写就，从未在 macOS 编译或运行过**。它们不是遗漏，是明知代价的选择：开发机是 Windows，`coretext.rs` 在此既编译不了也跑不起来，而 AGENTS.md §5 明令「只能真机验证的特性别声称『已验证』」。故在此如实登记，待有 Mac 环境时逐条核实。

| 改动 | 位置 | 需核实什么 |
|---|---|---|
| `TextEngine` 签名改收 `TextStyle` | `src/text/coretext.rs` `measure` / `draw` | 能否编译通过；参数改名后无遗漏引用 |
| 行高（`line_height`） | `attributed()` 新增 `line_h` 参数 | `CTParagraphStyleSpecifier::{MinimumLineHeight, MaximumLineHeight}` 在当前 `objc2-core-text` 版本中是否存在且同名；两者同设时行高是否真的固定 |
| 行高对测量的影响 | `measure()` 单行分支 | 显式行高优先于 `CTLine` 字形度量这一处理是否与实际渲染一致 |
| 段落样式设定数组 | `CTParagraphStyle::new(settings.as_ptr(), settings.len())` | 由固定 1 项改为动态 `Vec` 后，指针与长度的传递是否符合该绑定的约定 |

**风险点**：行高的基线处理两平台是分头实现的——Windows 走 `SetLineSpacing(UNIFORM, line, line * 0.8)`，macOS 走 Min/MaxLineHeight。0.8 这个基线系数只在 DirectWrite 侧验证过；CoreText 侧若表现为文字贴上沿或贴下沿，需要的是补一个基线偏移设定，而不是去调 0.8。

**未做**：`letter_spacing` 与斜体尚未实现（两平台皆是）。它们与行高同属文字引擎特性，日后一并补时应同时补上本表。

## 8. 未验证的窗口层改动（技术债登记）

与 §7 同一处境：**在 Windows 上写就，只跑过 `cargo check --target aarch64-apple-darwin`，
从未在 macOS 上运行**。编译通过不等于行为正确，故逐条登记验法。

| 改动 | 位置 | 真机验法与预期 |
|---|---|---|
| `windowDidResignKey:` → `on_capture_lost` | `platform/macos/window.rs` | 跑 `examples/reorder.rs`，按住一行拖到列表中部**不松手**，Cmd+Tab 切走再切回：该行应已落回合法位置，不再跟随指针 |
| `windowDidResignKey:` → 收掉合成态 | 同上 `abort_composition` | 跑 `examples/ime.rs`，拼音打到一半（候选窗已弹）时 Cmd+Tab 切走再切回：光标应恢复闪烁，候选窗不残留，已输入的拼音不会莫名上屏 |
| 滚轮亚像素残差 | 同上 `on_wheel` | 触控板在长列表上**极慢**地推：应逐点跟手滚动，而不是"轻推没反应、猛推才动" |
| 原生动量滚动 | 同上 `on_wheel` | 触控板两指快滑后抬手：列表应继续滑行并逐渐停下；滑行途中把两指放回触控板应**立即停住**而非加速 |
| `phase()` 在滚轮事件上的取值 | 同上 | 鼠标滚轮的 `phase` 应恒为 `None`（走不到清残差分支）；仅用于清残差，取值即便有出入也只影响一格以内的滚动量 |

**已知未消除的语义差（合成串不可见）**：Windows 的 IME 自己会在
`ImmSetCompositionWindow` 指定处画出合成串及其光标，所以控件在合成态隐藏自绘光标是在
**消除双光标**；macOS 把绘制 marked text 的责任完全交给客户端，而本后端不画——于是合成
期间文本框里既没有合成串、也没有光标。这不是本轮引入的（下发链路一直如此），根治要上层
先有"显示未提交合成串"的 API（`RichText` 的 span 模型可承载）。真机上若观感不可接受，
短期缓解是让 macOS 后端不上报 `composing = true`（保留光标闪烁），代价是与 win32 行为分叉。

**已知未覆盖的收尾路径（两平台同病）**：菜单浮层的滚动条拖拽状态（`app/menu.rs` 的
`scrollbar_drag`）不走 `UiHost::capture`——菜单打开时 `on_pointer` 在进入控件树之前就
被浮层截走了，故 `capture_active()` 恒为 false，捕获丢失时 `on_capture_lost` 直接早退，
拖拽态不会被复位。这是 `app` 层的缺口，win32 上同样存在，不是 macOS 后端能修的。
