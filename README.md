<p align="center">
  <img src="assets/windui-256.png" width="96" alt="windui">
</p>

# windui

**简体中文** · [English](README.en.md)

[![CI](https://github.com/huanfeng/wind-ui-rust/actions/workflows/ci.yml/badge.svg)](https://github.com/huanfeng/wind-ui-rust/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/windui.svg)](https://crates.io/crates/windui)
[![docs.rs](https://docs.rs/windui/badge.svg)](https://docs.rs/windui)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#许可证)

> 轻量跨平台桌面 GUI 框架 — 用 Rust 构建内存友好的小工具。

`平台原生窗口` · `tiny-skia 矢量渲染` · `平台原生文字排版` · 无运行时 · 无 GC。

<p align="center">
  <img src="docs/images/settings-input.png" width="880" alt="windui 设置窗：自绘标题栏 + 图标侧栏 + 内容区 + 底部操作栏">
</p>

| 平台 | 窗口/呈现 | 文字 |
|------|-----------|------|
| **Windows** | Win32 + GDI（DIB 拷屏） | DirectWrite |
| **macOS** | Cocoa/AppKit + CoreGraphics（CGImage blit） | Core Text |

渲染层（`tiny-skia`）与全部控件/布局/事件逻辑平台无关；每个平台只实现「窗口+事件循环」与「文字引擎」两条缝。

## 为什么

做小工具时，Electron 动辄上百 MB，Go GUI 因 runtime/GC 也要 15–40MB。windui 没有运行时、没有垃圾回收，Windows 上的实测：

| 指标 | 实测值 |
|------|--------|
| 二进制体积（release，LTO+strip） | 最小窗口应用 **0.64 MB**；综合示例（全控件 + SVG）**1.38 MB** |
| 私有内存（PrivateBytes，100% 缩放） | 最小窗口 480×320 **2.7 MB**；关于窗 620×556 **5.5 MB** |
| 同上，200% 缩放 | **4.6 MB** / **14.2 MB** |
| 跨平台直接依赖 | tiny-skia（渲染）· resvg（SVG，默认开，不用则被 LTO 裁掉）· serde + toml（主题）；平台系统绑定按 target 引入 |

> **内存数字离开 DPI 就没有意义**：大头是软件光栅为窗口留的约 2.5 份全屏 RGBA 缓冲，
> 而缓冲按**物理**像素分配——200% 缩放下同一窗口的物理面积是 100% 的 4 倍，内存也跟着翻。
> 上表两行是同一批二进制在两种缩放下的实测，不是两套代码。
>
> 工作集另含 gdi32/dwrite 等**跨进程共享**的系统 DLL 映射（关于窗 100% 下约 22MB），
> 进程真正独占的只有上表的私有内存。
>
> 全部数字由 [`scripts/measure_footprint.ps1`](scripts/measure_footprint.ps1) 实测，跑一遍即可复现。

## 特性

- **命令式 Builder API** — 纯 Rust 链式构建，类型安全、零解析开销。
- **Copy 句柄状态** — 状态是 `Signal<T>`，闭包里 `move` 直接捕获、不用 `clone()` 前戏；`set()` 自动触发重绘。数据变化驱动子树重建（`list_signal`），动态列表不用手写 diff。
- **运行期换主题** — `App::theme_handle()` 拿句柄，回调里 `set(Theme::dark())` 即整树热切换；用 `Role` 表达的颜色（`fg_role`/`bg_role`）自动跟随。
- **一份代码，两个平台** — 控件树、布局、事件、动画、主题全平台无关；切换平台零改动。
- **Retained 模式 + 脏触发** — 空闲不重绘、阻塞在事件循环，零 CPU 占用。
- **高质量文字** — 平台原生排版（DirectWrite / Core Text）+ 灰度抗锯齿，CJK 清晰；Label 自动换行；**彩色 emoji**（含 ZWJ 组合序列、肤色修饰），文本框可输入 emoji。
- **DPI / Retina 感知** — 控件树用逻辑坐标、绘制层统一缩放到物理像素，文字按物理字号渲染（测量与绘制同源），高 DPI（1.5x/2x/Retina）下依然锐利、不偏小。
- **纯净焦点环** — 焦点环仅在键盘 Tab 导航时显示，纯鼠标操作不显示外框。
- **完整控件集** — 布局、文本、按钮、表单输入、容器导航、列表、图片、托盘一应俱全。
- **触摸/触控板** — 平移滚动 + 惯性滑动 + 撞界回弹。
- **可选 GPU 加速（Windows）** — 大窗口可 opt-in Direct2D 后端（`App::accelerated(true)`），几何/渐变/阴影/文字光栅走 GPU；文字仍用 DirectWrite（系统字体缓存、ClearType）。默认软渲染；RDP / 无 GPU / 离屏截图自动回退、绝不 panic。
- **自动截屏** — `--screenshot` 离屏渲染存 PNG（`--scale 1.5` 验证高 DPI），适合自动化回归。

## 界面预览

下列截图全部由离屏渲染自动截取（`--screenshot`，见 [`scripts/readme_shots.ps1`](scripts/readme_shots.ps1)），未作任何后期修饰。
主要示例统一走无边框窗口 + 自绘标题栏——**标题栏本身就是控件树的一部分**，和正文用同一套布局与主题。

<table>
<tr>
<td width="50%"><img src="docs/images/fullshowcase.png" alt="控件总览"></td>
<td width="50%"><img src="docs/images/theming.png" alt="主题与换肤"></td>
</tr>
<tr>
<td><sub>控件总览：七个分页按控件族分组（表单 / 按钮 / 布局 / 文字 / 数据 / 图片 / 关于）</sub></td>
<td><sub>主题：TOML 部分覆盖 + <code>Role</code> 角色着色，运行期整树热切换</sub></td>
</tr>
<tr>
<td><img src="docs/images/settings-dialog.png" alt="模态对话框与可编辑表格"></td>
<td><img src="docs/images/virtual-list.png" alt="虚拟滚动"></td>
</tr>
<tr>
<td><sub>模态对话框：背景遮罩 + 带标题栏的面板 + 点单元格即编辑的表格</sub></td>
<td><sub>虚拟滚动：列表 10 万行 / 表格 1 万行，只构建视口内的行</sub></td>
</tr>
<tr>
<td><img src="docs/images/image.png" alt="图片与矢量"></td>
<td><img src="docs/images/about.png" alt="关于页"></td>
</tr>
<tr>
<td><sub>图片与矢量：PNG/SVG、Fit 模式、圆角裁剪、单色着色</sub></td>
<td><sub>关于页：可点击卡片 + 胶囊徽章 + 描边按钮 + Toast</sub></td>
</tr>
</table>

## 快速开始

```rust
use windui::prelude::*;

fn main() {
    // 状态是 Signal<T>：Copy 句柄，闭包直接捕获，写入自动触发重绘
    let on = signal(true);

    let ui = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .bg(Color::hex(0xF5F6FA))
        .child(Element::label("Hello, windui!").font_size(22.0).width_match())
        .child(Element::checkbox("启用功能", on))
        .child(Element::button("确定").on_click(move |ctx| {
            println!("checkbox = {}", on.get());
            ctx.request_close();
        }));

    App::new("Demo", 360, 240).content(ui).run();
}
```

## 控件

| 类别 | 控件 |
|------|------|
| 布局 | `col` / `row`（LinearLayout，支持 weight）、`stack`（FrameLayout）、`grid`（等宽网格）、`flex_spacer` |
| 文本 | `label`（自动换行）、`label_signal`（绑信号）、`link`（可点击链接）、`rich`（富文本：多样式 span / 折叠段） |
| 按钮 | `button`（hover/press/focus 三态 + 点击/回车/空格激活）、`icon_button`（纯图标） |
| 表单 | `checkbox` / `switch` / `radio`（互斥组）/ `slider`（拖动+键盘）/ `text_input`（CJK 编辑+密码+多行）/ `dropdown` / `check_menu` / `stepper` / `chip` / `tag_field` |
| 反馈 | `progress`（确定/不确定）/ `tooltip`（悬停提示）/ `toast`（居中轻提示）/ `badge`（胶囊徽章） |
| 容器 | `scroll`（滚轮/触摸+裁剪+滚动条）/ `tabs` / `tabs_pill` / `divider` / `dialog`（模态）/ `dialog_panel`（带标题栏）/ `visible_when`（条件可见） |
| 导航 | `segmented`（连体多段单选）/ `nav_row`（钻入行）/ `collapsible` / `accordion`·`accordion_multi`（手风琴） |
| 列表 | `list` / `list_pill`（侧栏样式）/ `list_icons`（单选/滚动/高亮/图标/禁用态）/ `list_signal`（数据驱动动态列表）/ `reorder_list`（拖拽排序） |
| 表格 | `table`（只读）/ `table_custom` / `table_editable` / `table_sortable` / `table_sortable_server`（服务端排序分页）/ `table_selectable`（多选） |
| 图片 | `image` / `image_svg` / `image_view`（PNG/SVG，状态调制/着色/圆角） |
| 系统 | 系统托盘（图标 + 左键/双击 + 原生右键菜单）、全局热键、多窗口（`ctx.open_window`，含单例窗口）、启动即隐藏、关闭转隐藏、无边框窗口（自定义标题栏）、文件拖放、剪贴板 |

控件状态统一绑定 `Signal<T>`（`signal(初值)` 创建的 `Copy` 句柄）：`checkbox`/`switch` 绑
`Signal<bool>`、`dropdown`/`list`/`tabs` 绑 `Signal<usize>`、`text_input` 绑 `Signal<String>`。
`set()` 写入即自动触发重绘，无需手动标脏。用法见 [`docs/API_GUIDE.md`](docs/API_GUIDE.md) §3.2。

## 构建与运行

```bash
cargo run --release --example fullshowcase                  # 运行综合示例窗口
cargo run --release --example ime -- --accelerated          # 启用 Direct2D GPU 后端（Windows）
cargo run --example fullshowcase -- --screenshot out.png    # 离屏渲染存 PNG
cargo test                                                  # 运行单元测试
cargo clippy --all-targets                                  # 静态检查
```

示例按用途分四类：

| 类别 | 示例 |
|------|------|
| **完整应用** | `settings`（设置窗：标题栏 + 图标侧栏 + 内容 + 底部操作栏 + 两个对话框）、`about`（关于页）、`ime_settings`（输入法设置场景）、`light_titlebar`（安装器风格的浅色标题栏） |
| **控件与能力** | `fullshowcase`（控件总览，七个分页）、`theming`（TOML 主题 + 运行期换肤）、`image`（图片/SVG）、`animation`、`emoji`（彩色 emoji）、`caret`（文本光标四风格） |
| **数据展示** | `virtual_list`（虚拟滚动列表 + 表格）、`virtual_table_server`（服务端分页）、`table_pager`（分页操作栏）、`dyn_list`（数据驱动动态列表）、`list`、`dropdown`、`tabs_pill`、`toast`、`progress`、`multiline` |
| **系统集成** | `tray`（系统托盘）、`hotkey`（全局热键 + 启动即隐藏）、`multi_window`（子窗 + 跨窗共享状态）、`file_drop`、`frameless`（自定义标题栏 + 系统菜单）、`background_task`（跨线程更新）、`ime` |

另有 `phase0`–`phase5` 分阶段演示与 `perfprobe` 性能探针，供开发与回归比对使用。

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
