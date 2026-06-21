# 开发指南

面向**在本仓库内开发** windui 的人：环境、项目结构、构建/验证流程、如何加控件、平台缝合层。
写应用的 API 用法见 [`API_GUIDE.md`](API_GUIDE.md)；架构取舍见 [`DESIGN.md`](DESIGN.md)；
约定与陷阱速查见 [`../AGENTS.md`](../AGENTS.md)。

## 1. 环境

| 平台 | 前置 |
|------|------|
| Windows | Rust stable + MSVC 工具链 |
| macOS | Rust stable + Xcode Command Line Tools |

```bash
cargo run --release --example fullshowcase   # 验证环境
```

仅 Windows 与 macOS 受支持；其他目标编译期会 `compile_error!`（见 `src/platform/mod.rs`）。

## 2. 项目结构

```
src/
  lib.rs            crate 根 + prelude（对外导出面）
  app.rs            App 构建器 + UiHost（实现 platform::AppHandler，驱动渲染与事件）
  core.rs           arena Tree/Node · Measure/Arrange/Paint 三阶段 · 事件分发 · ClipboardProvider
  event.rs          事件类型（PointerEvent/KeyEvent/CursorShape/WindowOp/Menu…）
  geometry.rs       Point/Size/Rect/Color/Insets
  spec.rs           Align/Axis/Dimension
  style.rs          Style（背景/边框/圆角/内边距…）
  theme.rs          主题系统（palette + metrics 两层 + 控件 Option 覆盖，TOML 可配）
  anim.rs           动画补间引擎（Easing/Transition/全局开关）
  render/
    mod.rs          Canvas trait（平台无关绘制接口）
    skia.rs         tiny-skia 后端
    image.rs        图片原语 + 可扩展解码框架（PNG/SVG）
  text/
    mod.rs          TextEngine trait + PlatformTextEngine 类型别名（cfg 分发）
    dwrite.rs       Windows：DirectWrite
    coretext.rs     macOS：Core Text
  platform/
    mod.rs          AppHandler trait · WindowConfig（平台中性）· run_offscreen（共享）· cfg 分发
    win32/          Windows 后端：窗口/WndProc/DIB 呈现 + clipboard + tray
    macos/          macOS 后端：NSWindow/NSView/CGImage 呈现 + clipboard + tray + window
  ui/
    mod.rs          Element 链式 Builder（控件对外入口）
    containers.rs inputs.rs link.rs list.rs nav.rs progress.rs
    segmented.rs select.rs stepper.rs image.rs window_buttons.rs
examples/           可运行示例（每个控件/特性一个）
docs/               设计/路线/移植文档
scripts/            截屏脚本等
```

## 3. 构建与验证

提交前必须通过三道闸（CI 在 Windows + macOS 双平台复跑）：

```bash
cargo build --all-targets
cargo test                 # 核心逻辑测试，平台无关，两平台应同样全过
cargo clippy --all-targets # 须零警告
```

运行与截屏：

```bash
cargo run --release --example <name>                       # 运行某示例
cargo run --example <name> -- --screenshot out.png         # 离屏渲染存 PNG
cargo run --example <name> -- --screenshot out.png --scale 1.5   # 验证高 DPI
# 截屏前合成交互（验证菜单/下拉/悬停视觉）：
#   --click X Y / --rclick X Y / --hover X Y（逻辑坐标）
```

`--screenshot` 走平台无关的 `platform::run_offscreen`，无需开窗，适合自动化视觉回归。

> 格式：不强制 `cargo fmt`——图形 API 刻意用宽行。保持与周边一致即可。

## 4. 加一个新控件

1. 在 `src/ui/` 新建模块（或并入相近文件），实现控件的内容/绘制/事件。
2. 在 `src/ui/mod.rs` 给 `Element` 加链式构造方法（对外 API 入口）。
3. **去硬编码**：颜色/尺寸走 `theme.rs` 的 palette/metrics，必要时加控件级 Option 覆盖（见
   [主题系统约定](../AGENTS.md)）。新增控件须接入主题。
4. **禁用态**：核心级 `Node.enabled` 已统一下沉，复用即可，勿各控件自造。
5. 写单元测试（`#[cfg(test)]`，纯逻辑/布局，平台无关）。
6. 新增一个 `examples/<name>.rs`，并在 `fullshowcase` 的「控件」标签页挂上展示。
7. 跑三道闸 + 截图验证视觉。

## 5. 平台缝合层

控件/布局/事件/渲染全部平台无关。每个平台只实现两条缝：

- **窗口 + 事件循环** → 实现 `platform::AppHandler` 的消费侧：建窗、blit `Pixmap` 到屏、
  把 OS 输入翻译成 `PointerEvent`/`KeyEvent`、DPI、光标、IME、触摸、文件拖放、无边框命中。
- **文字引擎** → 实现 `text::TextEngine`（measure/draw），按 `scale` 物理化排版。

`cfg` 分发只集中在两处薄文件：`src/platform/mod.rs` 与 `src/text/mod.rs`（声明编译哪个后端 +
re-export）。实现代码各自独立成文件，**实现文件内零内联平台 cfg**。新增平台只在这两处加分支，
并补一个后端目录 + 一个文字引擎文件。逐项缝面映射见 [`MACOS_PORTING.md`](MACOS_PORTING.md)。

坐标约定（缝两侧必须一致）：控件树用**逻辑坐标**；`AppHandler` 回调收发的是**物理像素、相对客户区左上角**；
绘制层按 `scale` 放大。文字测量与绘制走同一物理字号路径（hinting 非线性，禁止线性外推）。

## 6. 提交

遵循 Conventional Commits，每个提交 DCO 签署（`git commit -s`）。详见 [`../CONTRIBUTING.md`](../CONTRIBUTING.md)。
