# windui

> 轻量 Windows 桌面 GUI 框架 — 用 Rust 构建内存友好的小工具。

`Win32 窗口 + GDI 呈现` · `tiny-skia 矢量图形` · `DirectWrite 文字` · 无运行时 · 无 GC。

## 为什么

做小工具时，Electron 动辄上百 MB，Go GUI 因 runtime/GC 也要 15–40MB。windui 没有运行时、没有垃圾回收，一个综合示例的实测：

| 指标 | 实测值 |
|------|--------|
| 二进制体积（release，LTO+strip） | **0.49 MB** |
| 私有内存（PrivateBytes，520×560 窗口） | **3.65 MB** |
| 依赖 crate 数 | **2**（tiny-skia + windows） |

> 工作集（WorkingSet）约 14MB，其中大部分是 gdi32/dwrite 等**跨进程共享**的系统 DLL 映射；进程真正独占的私有内存仅约 3.6MB。

## 特性

- **命令式 Builder API** — 纯 Rust 链式构建，类型安全、零解析开销。
- **Retained 模式 + 脏触发** — 空闲不重绘、阻塞在消息循环，零 CPU 占用。
- **高质量中文** — DirectWrite 排版 + 灰度抗锯齿，CJK 清晰；Label 自动换行。
- **DPI 感知** — Per-Monitor v2：控件树用逻辑坐标、绘制层统一缩放到物理像素，
  文字按物理字号渲染（测量与绘制同源），高 DPI（1.5x/2x）下依然锐利、不偏小；支持 `WM_DPICHANGED` 动态切换。
- **纯净焦点环** — 焦点环仅在键盘 Tab 导航时显示，纯鼠标操作不显示外框。
- **完整控件集** — 布局、文本、按钮、输入、容器导航一应俱全。
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
| 文本 | `label`（自动换行） |
| 按钮 | `button`（hover/press/focus 三态 + 点击/回车/空格激活） |
| 输入 | `checkbox` / `switch` / `radio`（互斥组）/ `slider`（拖动+键盘）/ `text_input`（CJK 编辑+光标） |
| 容器 | `scroll`（滚轮+裁剪+滚动条）/ `tabs`（标签页）/ `divider` / `dialog`（模态）/ `visible_when`（条件可见） |

输入控件通过 `Rc<Cell<T>>` / `Rc<RefCell<String>>` 与外部状态双向绑定。

## 构建与运行

```bash
cargo run --release --example fullshowcase     # 运行综合示例窗口
cargo run --example fullshowcase -- --screenshot out.png   # 离屏渲染存 PNG
cargo test                                     # 运行单元测试
powershell scripts/screenshots.ps1             # 一键生成所有示例截屏
```

示例：`phase0_window` `phase1_layout` `phase2_text` `phase3_button` `phase4_form` `phase5_containers` `fullshowcase` `ime_settings`（输入法设置形状的主从布局演示）。

## 架构

详见 [`docs/DESIGN.md`](docs/DESIGN.md)（架构设计）与 [`docs/ROADMAP.md`](docs/ROADMAP.md)（实施路线）。

```
应用层  App / UiHost（交互宿主）
控件层  Element Builder · Widget trait · 布局算法
核心层  Arena + Node 树 · Measure/Arrange/Paint 三阶段 · 事件分发
渲染层  Canvas trait → tiny-skia 后端 ｜ TextEngine → DirectWrite
平台层  Win32 窗口 · WndProc · DIB+BitBlt 呈现
```

关键设计：节点存于 **generational arena**（非 `Rc<RefCell>`），`Widget` trait 退化为纯内容、布局递归由 `Tree` 独占 `&mut self` 驱动 —— 从根上规避 Rust 借用冲突。文字用 DirectWrite **白字黑底取覆盖率**技巧合成进 tiny-skia 预乘缓冲。

## 状态

MVP 完成（Phase 0–6）。当前仅 Windows；平台层已留 trait 边界以便未来扩展。

已知限制：单行文本输入无水平滚动；高分辨率滚轮不累积余量；Tab 标签无选中高亮。

## 文档

| 文档 | 面向 |
|------|------|
| [`docs/API_GUIDE.md`](docs/API_GUIDE.md) | 用本库写应用（API 风格、控件、扩展） |
| [`AGENTS.md`](AGENTS.md) | 在仓库内开发（约定、流程、陷阱） |
| [`docs/DESIGN.md`](docs/DESIGN.md) | 架构设计与取舍 |
| [`docs/ROADMAP.md`](docs/ROADMAP.md) | 实施路线与验收 |

## License

MIT
