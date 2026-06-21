# macOS 移植指南

本文给在 macOS 上接续实现的开发者：说明跨平台架构、缝合层如何分发、每个待填实现点对照的
Win32 实现与应使用的 Cocoa/Core 框架 API，以及推荐的分阶段落地顺序。

> 当前状态：**缝合层重构已完成**。macOS 后端为纯 Rust 骨架（`todo!()`/`unimplemented!()`），
> 不依赖 objc2，`cargo build` 在 macOS 上即可通过。Windows 侧构建/测试/clippy 全绿，不受影响。

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
| 指针捕获 | `SetCapture`/`ReleaseCapture` + `capture_active()` | macOS 拖动期间默认续派发事件给同一 view，通常无需显式捕获；`on_capture_lost` 对应窗口失活 |
| 键盘 | `WM_KEYDOWN` | `keyDown:`，特殊键映射到 `Key` |
| 输入法 | `WM_IME_*` + `ImmSetCompositionWindow`（用 `ime_caret()`） | 实现 `NSTextInputClient`；`firstRectForCharacterRange:` 用 `ime_caret()` 定位候选窗 |
| 光标形状 | `WM_SETCURSOR`（用 `cursor()`） | `NSView::resetCursorRects` 或 `NSCursor::set`，按 `cursor()` 选 arrow/pointingHand/iBeam |
| 文件拖放 | `WM_DROPFILES`（用 `on_drop_files()`） | `NSDraggingDestination`：`draggingEntered:`/`performDragOperation:` |
| 无边框窗口 | `WM_NCCALCSIZE` + `WM_NCHITTEST`（用 `window_drag_at`/`interactive_at`） | styleMask 去 `titled` + 加 `fullSizeContentView`；自管拖动可重写 `mouseDown` 调 `performWindowDragWithEvent:` |
| 窗口操作 | `take_window_op()` → `ShowWindow(SW_MINIMIZE/MAXIMIZE...)` | `NSWindow::miniaturize`/`zoom`/`close` |

触摸/惯性：触控板 `scrollWheel:` 原生带 momentum phase（`NSEvent::momentumPhase`），可直接转 `on_pan`，
并**关掉自研 fling**（`start_fling` 返回 false）；或仅转发原生滚动。二选一，别叠加。

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
