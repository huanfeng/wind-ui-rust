# Changelog

本项目遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/)。

## [Unreleased]

### Added
- macOS 平台后端（Cocoa/AppKit 窗口 + Core Text 文字 + NSPasteboard 剪贴板 + NSStatusItem 托盘）。
- 跨平台缝合层：渲染/控件/事件平台无关，平台仅实现「窗口+事件循环」与「文字引擎」两条缝。
- 开源配套：双许可（MIT OR Apache-2.0）、DCO、贡献指南、开发指南、issue/PR 模板、CI、发布工作流。

### Changed
- 依赖按 target 门控：`windows` 仅 Windows、`objc2` 系列仅 macOS。
- README 改为跨平台说明（中文主 + 英文副）。
- 依赖更新：`toml` 0.8 → 1.1；CI actions（checkout v7、action-gh-release v3）。
- **windows-rs 0.58 → 0.62 迁移**：`implement` 宏改由 `windows-core` 提供；可空句柄参数
  语义化为 `Option<T>`；`BOOL` 迁至 `windows::core`；COM 实现入参 `Option<&T>` → `Ref<'_, T>`。

## [0.1.0]

首个版本（Windows）。命令式 Builder API、retained 模式、DPI 感知、DirectWrite 文字、
完整控件集（布局/文本/按钮/表单/容器/列表/图片/导航）、系统托盘、无边框窗口、触摸滚动、自动截屏。

[Unreleased]: https://github.com/huanfeng/wind-ui-rust/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/huanfeng/wind-ui-rust/releases/tag/v0.1.0
