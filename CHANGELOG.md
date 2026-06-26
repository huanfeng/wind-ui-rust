# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

## [0.4.0] - 2026-06-26

### Added
- **Direct2D GPU 渲染后端（Windows，可选 opt-in）**：大窗口/多控件下软件光栅 paint-bound，新增
  Direct2D 后端把几何/渐变/裁剪/opacity/图片/阴影/文字光栅迁到 GPU。窗口级显式 opt-in
  `App::accelerated(true)`（示例 `--accelerated`），**默认仍软渲染**；与 tiny-skia 软路径并存。
  - 文字坚持走 **DirectWrite**（`DrawTextLayout`，系统字体缓存 + ClearType），与软路径字体/字重一致。
  - 阴影用 `ID2D1Shadow` GPU 高斯模糊，烘焙一次缓存成品避免每帧重模糊。
  - 自动回退软渲染（绝不 panic）：RDP 远程会话、无可用 GPU、设备创建失败、离屏截图。
  - 设备丢失检测 → 整体重建设备链 → 连续失败降级软后端；同 UI 线程多窗口共享设备链（避免 ×N 内存）。
  - 重对象（文字布局/画刷/位图/后备缓冲）全缓存复用，常驻内存从早期 190M 降到 ~70M。
- 渐变画刷（线性/径向）+ `Brush`（Solid/Gradient/Role）主题角色取色体系。
- `Theme::dark` 暗色预设 + `ThemeHandle` 运行期主题热切换（整树跟随刷新）。
- 浮层投影（box-shadow）+ 子树整体不透明度（离屏层合成）。
- 级联右键菜单（图标/分隔/快捷键/子菜单）+ `Element::on_context_menu`。
- `PickDialog`：系统原生文件/目录选择对话框。
- `Signal<T>`：`Copy` 句柄状态原语（运行时 arena 承载），全控件状态从 `Rc<Cell>`/`Rc<RefCell>` 迁入；
  `set` 自动产生局部脏区，新控件免手写 `mark_dirty`。
- 文字字重支持；半透明文字色。
- `App::min_size`：限制窗口最小客户区尺寸。
- 新增 `examples/ime.rs`（复刻中文输入法界面，暗/亮双主题）。

### Changed
- 控件状态原语统一为 `Signal<T>`，取代散落的 `Rc<Cell>`/`Rc<RefCell>`（API 基本不变，状态语义更一致）。
- 渲染接缝重构：`AppHandler::render` 改为面向 `RenderTarget`，软/GPU 两后端同形接入，软路径零回归。

### Performance
- 交互失效系统：hover/拖动/点击/打字走 ~1ms **局部重绘**（结构签名判定局部 vs 整窗），不再每次整窗重绘。
- DirectWrite 测量结果缓存，消除稳定文本每帧重复排版。
- 模糊阴影缓存（位置无关），修复阴影每帧重算导致的卡顿；新增 `WINDUI_PROF` 绘制热点计时。

### Fixed
- 窗口按钮与复选框的文字/悬停色未跟随主题。
- DPI 缩放下 win32 窗口显示异常（全窗重绘 scale 由 handler 提供）。
- 点击切换内容不刷新；标签条内边距、菜单尾随快捷键换行、分段选中反色、菜单高亮溢出等多处 UI 细节。

## [0.3.0] - 2026-06-23

### Added
- 多行 `TextInput`：滚动条、滚轮滚动、跨视口拖选。
- `Label` `max_lines` 行数限制 + Truncate 省略号（End/Start/Middle）。

### Fixed
- `ScrollWidget` 滚轮滚动到边界时冒泡给外层容器。

## [0.2.0] - 2026-06-23

### Added
- 跨线程 UI 更新：`App::channel::<Msg>(on_message) -> Sender<Msg>`（后台 `send` 事件驱动唤醒 UI、`on_message` 在 UI 线程写状态）+ `App::on_interval(dur, cb)` 定时回调。有更新才重绘、空闲零 CPU。
- 语义意图色（Intent）体系：Button / CheckBox 统一 `.intent()` / `.danger()` / `.neutral()` / `.accent(color)`；
  内置 primary/neutral/danger，`Custom(Color)` 为扩展点——单基色自动派生 hover/active + 对比自适应前景。
  Button 默认 Primary（现有代码零改动）；CheckBox 现有 `.danger()`/`.accent()` 收编进同一体系（API 不变）。
- CheckBox 受控点击拦截：`Element::checkbox(..).on_toggle(cb)`——设回调后点击/键盘激活不自动翻转
  绑定 state，交 app 决定是否翻转（可在翻转前弹确认、确认后再置真，渲染跟随 state，零闪烁）。
- `Color::lighten` / `darken` / `pick_fg`（对比自适应前景）颜色派生工具。
- 彩色 emoji 渲染：DirectWrite 字形经 `IDWriteFactory2::TranslateColorGlyphRun`
  拆成 COLR/CPAL 彩色层逐层着色（emoji、ZWJ 组合序列、肤色修饰均正确合成彩色），
  字体无彩色数据时自动回退原单色路径。新增 `examples/emoji.rs` 演示。

### Fixed
- 文本框无法输入 emoji：WM_CHAR 对补充平面字符（码点 > U+FFFF，如 emoji）
  分两条消息发来 UTF-16 代理对，原逻辑对单个代理项解码失败而丢弃。现正确
  暂存高代理项并与低代理项合成为单个 `char`，emoji 及 CJK 扩展区字符可正常输入。

## [0.1.0] - 2026-06-22

首个公开版本（Windows + macOS）。

### Added
- 核心框架：命令式 Builder API、retained 模式、DPI 感知、tiny-skia 渲染。
- 完整控件集（布局/文本/按钮/表单/容器/列表/图片/导航）、系统托盘、无边框窗口、触摸滚动、自动截屏。
- Windows 平台后端（Win32 + GDI + DirectWrite 文字）。
- macOS 平台后端（Cocoa/AppKit 窗口 + Core Text 文字 + NSPasteboard 剪贴板 + NSStatusItem 托盘）。
- 跨平台缝合层：渲染/控件/事件平台无关，平台仅实现「窗口+事件循环」与「文字引擎」两条缝。
- 开源配套：双许可（MIT OR Apache-2.0）、DCO、贡献指南、开发指南、issue/PR 模板、CI、发布工作流。

### Changed
- 依赖按 target 门控：`windows` 仅 Windows、`objc2` 系列仅 macOS。
- README 改为跨平台说明（中文主 + 英文副）。
- 依赖更新：`toml` 0.8 → 1.1；CI actions（checkout v7、action-gh-release v3）。
- **windows-rs 0.58 → 0.62 迁移**：`implement` 宏改由 `windows-core` 提供；可空句柄参数
  语义化为 `Option<T>`；`BOOL` 迁至 `windows::core`；COM 实现入参 `Option<&T>` → `Ref<'_, T>`。

[Unreleased]: https://github.com/huanfeng/wind-ui-rust/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/huanfeng/wind-ui-rust/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/huanfeng/wind-ui-rust/releases/tag/v0.1.0
