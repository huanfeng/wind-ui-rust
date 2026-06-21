# windui

**简体中文** · [English](README.en.md)

[![CI](https://github.com/huanfeng/wind-ui-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/huanfeng/wind-ui-rust/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/windui.svg)](https://crates.io/crates/windui)
[![docs.rs](https://docs.rs/windui/badge.svg)](https://docs.rs/windui)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)

> 轻量跨平台桌面 GUI 框架 — 用 Rust 构建内存友好的小工具。

`平台原生窗口` · `tiny-skia 矢量渲染` · `平台原生文字排版` · 无运行时 · 无 GC。

| 平台 | 窗口/呈现 | 文字 |
|------|-----------|------|
| **Windows** | Win32 + GDI（DIB 拷屏） | DirectWrite |
| **macOS** | Cocoa/AppKit + CoreGraphics（CGImage blit） | Core Text |

渲染层（`tiny-skia`）与全部控件/布局/事件逻辑平台无关；每个平台只实现「窗口+事件循环」与「文字引擎」两条缝。

## 为什么

做小工具时，Electron 动辄上百 MB，Go GUI 因 runtime/GC 也要 15–40MB。windui 没有运行时、没有垃圾回收，一个综合示例在 Windows 上的实测：

| 指标 | 实测值 |
|------|--------|
| 二进制体积（release，LTO+strip） | **0.49 MB** |
| 私有内存（PrivateBytes，520×560 窗口） | **3.65 MB** |
| 跨平台依赖 crate 数 | **1**（tiny-skia；平台后端各自的系统绑定按 target 引入） |

> 工作集约 14MB，其中大部分是 gdi32/dwrite 等**跨进程共享**的系统 DLL 映射；进程真正独占的私有内存仅约 3.6MB。

## 特性

- **命令式 Builder API** — 纯 Rust 链式构建，类型安全、零解析开销。
- **一份代码，两个平台** — 控件树、布局、事件、动画、主题全平台无关；切换平台零改动。
- **Retained 模式 + 脏触发** — 空闲不重绘、阻塞在事件循环，零 CPU 占用。
- **高质量文字** — 平台原生排版（DirectWrite / Core Text）+ 灰度抗锯齿，CJK 清晰；Label 自动换行。
- **DPI / Retina 感知** — 控件树用逻辑坐标、绘制层统一缩放到物理像素，文字按物理字号渲染（测量与绘制同源），高 DPI（1.5x/2x/Retina）下依然锐利、不偏小。
- **纯净焦点环** — 焦点环仅在键盘 Tab 导航时显示，纯鼠标操作不显示外框。
- **完整控件集** — 布局、文本、按钮、表单输入、容器导航、列表、图片、托盘一应俱全。
- **触摸/触控板** — 平移滚动 + 惯性滑动 + 撞界回弹。
- **自动截屏** — `--screenshot` 离屏渲染存 PNG（`--scale 1.5` 验证高 DPI），适合自动化回归。

## 快速开始

```rust
use std::cell::Cell;
use std::rc::Rc;
use windui::prelude::*;

fn main() {
    let on = Rc::new(Cell::new(true));
    let ui = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .bg(Color::hex(0xF5F6FA))
        .child(Element::label("Hello, windui!").font_size(22.0).height(32).width_match())
        .child(Element::checkbox("启用功能", on.clone()))
        .child(Element::button("确定").on_click(|ctx| {
            println!("clicked");
            ctx.request_close();
        }));

    App::new("Demo", 360, 240).content(ui).run();
}
```

## 控件

| 类别 | 控件 |
|------|------|
| 布局 | `col` / `row`（LinearLayout，支持 weight）、`stack`（FrameLayout） |
| 文本 | `label`（自动换行）、`link`（可点击链接） |
| 按钮 | `button`（hover/press/focus 三态 + 点击/回车/空格激活） |
| 表单 | `checkbox` / `switch` / `radio`（互斥组）/ `slider`（拖动+键盘）/ `text_input`（CJK 编辑+密码+多行）/ `dropdown` / `stepper` |
| 反馈 | `progress`（确定/不确定）/ `tooltip`（悬停提示） |
| 容器 | `scroll`（滚轮/触摸+裁剪+滚动条）/ `tabs` / `divider` / `dialog`（模态）/ `visible_when`（条件可见） |
| 导航 | `segmented`（连体多段单选）/ `nav_row` / `collapsible` / `accordion`（手风琴） |
| 列表 | `list`（单选/滚动/高亮/图标/禁用态） |
| 图片 | `image` / `image_view`（PNG/SVG，状态调制/着色/圆角） |
| 系统 | 系统托盘（图标 + 左键/双击 + 原生右键菜单）、无边框窗口（自定义标题栏）、文件拖放、剪贴板 |

表单控件通过 `Rc<Cell<T>>` / `Rc<RefCell<String>>` 与外部状态双向绑定。

## 构建与运行

```bash
cargo run --release --example fullshowcase                  # 运行综合示例窗口
cargo run --example fullshowcase -- --screenshot out.png    # 离屏渲染存 PNG
cargo test                                                  # 运行单元测试
cargo clippy --all-targets                                  # 静态检查
```

示例一览：`fullshowcase`（综合）、`animation`、`theming`、`image`、`list`、`dropdown`、`progress`、`multiline`、`frameless`、`light_titlebar`、`tray`、`file_drop`、`ime_settings`，以及 `phase0`–`phase5` 分阶段演示。

## 架构

详见 [`docs/DESIGN.md`](docs/DESIGN.md)（架构设计）与 [`docs/ROADMAP.md`](docs/ROADMAP.md)（实施路线）。

```
应用层  App / UiHost（交互宿主，实现 AppHandler）
控件层  Element Builder · Widget trait · 布局算法
核心层  Arena + Node 树 · Measure/Arrange/Paint 三阶段 · 事件分发
渲染层  Canvas trait → tiny-skia 后端（纯 Rust，跨平台）
文字层  TextEngine trait → DirectWrite（Windows）/ Core Text（macOS）
平台层  AppHandler trait → win32（窗口/WndProc/DIB 呈现）/ macos（NSWindow/NSView/CGImage 呈现）
```

关键设计：节点存于 **generational arena**（非 `Rc<RefCell>`），`Widget` trait 退化为纯内容、布局递归由 `Tree` 独占 `&mut self` 驱动 —— 从根上规避 Rust 借用冲突。文字用平台原生引擎在 tiny-skia 预乘缓冲上抗锯齿合成。平台缝合层映射见 [`docs/MACOS_PORTING.md`](docs/MACOS_PORTING.md)。

## 状态

Windows 与 macOS 均已支持。MVP 控件集完成，持续完善中。

## 文档

| 文档 | 面向 |
|------|------|
| [`docs/API_GUIDE.md`](docs/API_GUIDE.md) | 用本库写应用（API 风格、控件、扩展） |
| [`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md) | 在仓库内开发（构建、布局、加控件、平台缝） |
| [`CONTRIBUTING.md`](CONTRIBUTING.md) | 贡献流程与 DCO 签署 |
| [`docs/DESIGN.md`](docs/DESIGN.md) | 架构设计与取舍 |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | 实施路线与验收 |
| [`docs/MACOS_PORTING.md`](docs/MACOS_PORTING.md) | macOS 后端缝合层映射 |
| [`AGENTS.md`](AGENTS.md) | 仓库开发约定（流程、陷阱速查） |

## 许可证

双许可，任选其一：

- Apache License, Version 2.0（[`LICENSE-APACHE`](LICENSE-APACHE)）
- MIT License（[`LICENSE-MIT`](LICENSE-MIT)）

除非另有声明，你有意提交到本仓库的贡献，将按上述双许可授权，无附加条款（见 [`CONTRIBUTING.md`](CONTRIBUTING.md)）。
