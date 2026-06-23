# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added
- 跨线程 UI 更新：`App::channel::<Msg>(on_message) -> Sender<Msg>`（后台 `send` 事件驱动唤醒 UI、`on_message` 在 UI 线程写状态）+ `App::on_interval(dur, cb)` 定时回调。有更新才重绘、空闲零 CPU。
- 语义意图色（Intent）体系：Button / CheckBox 统一 `.intent()` / `.danger()` / `.neutral()` / `.accent(color)`；
  内置 primary/neutral/danger，`Custom(Color)` 为扩展点——单基色自动派生 hover/active + 对比自适应前景。
  Button 默认 Primary（现有代码零改动）；CheckBox 现有 `.danger()`/`.accent()` 收编进同一体系（API 不变）。
- CheckBox 受控点击拦截：`Element::checkbox(..).on_toggle(cb)`——设回调后点击/键盘激活不自动翻转
  绑定 state，交 app 决定是否翻转（可在翻转前弹确认、确认后再置真，渲染跟随 state，零闪烁）。
- `Color::lighten` / `darken` / `pick_fg`（对比自适应前景）颜色派生工具。

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

[Unreleased]: https://github.com/huanfeng/wind-ui-rust/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/huanfeng/wind-ui-rust/releases/tag/v0.1.0
