//! macOS 平台后端（Cocoa/AppKit + Core Text）。
//!
//! 对外暴露与 `win32` 同形的 API：`run` / `open_url` / `Tray`·`TrayCtx`·`TrayMenuItem` /
//! `clipboard::MacClipboard`。上层只依赖 `crate::platform::*`，不直接触碰本后端。
//!
//! 模块划分：
//! - `window`：`NSApplication` + `NSWindow` + 自定义翻转 `NSView`。渲染走 CPU（tiny-skia
//!   `Pixmap`），`drawRect:` 中经 `CGBitmapContext`→`CGImage`→`CGContextDrawImage` blit；
//!   鼠标/键盘/滚轮/光标/HiDPI/无边框拖动/文件拖放/输入法（`NSTextInputClient`）均在此。
//! - `clipboard`：`NSPasteboard`。
//! - `tray`：`NSStatusItem` 托盘（图标 + 左键/双击 + 原生右键菜单）。
//! - 文字渲染见 `crate::text::coretext`（Core Text）。
//!
//! 逐项对照实现见 `docs/MACOS_PORTING.md` 与 `platform/win32/mod.rs`。

pub mod clipboard;
pub mod tray;
pub mod window;

pub use tray::{Tray, TrayCtx, TrayMenuItem};

use super::{AppHandler, WindowConfig};

/// 运行应用：截屏模式离屏渲染存盘；否则创建窗口进入事件循环。
pub fn run(cfg: WindowConfig, mut handler: Box<dyn AppHandler>) {
    // 全局动画开关：显式配置优先；否则截屏路径恒开（保证终态稳定）、窗口路径随系统设置。
    let os_default = if cfg.screenshot.is_some() { true } else { os_animations_enabled() };
    crate::anim::set_enabled(cfg.animations.unwrap_or(os_default));

    if let Some(path) = cfg.screenshot.clone() {
        // 离屏渲染走平台无关的共享实现（与 win32 后端共用）。
        super::run_offscreen(&cfg, &mut handler, &path);
        return;
    }
    window::run_windowed(cfg, handler);
}

/// 查询系统“显示动画”偏好（减弱动态效果）。占位：默认开。
fn os_animations_enabled() -> bool {
    // TODO(macos): 读取 NSWorkspace.accessibilityDisplayShouldReduceMotion，取反。
    true
}

/// 用系统默认程序打开 URL/路径（链接点击）。对照 win32 `ShellExecuteW`。
pub fn open_url(url: &str) {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSString, NSURL};

    let ns = NSString::from_str(url);
    if let Some(nsurl) = NSURL::URLWithString(&ns) {
        // fire-and-forget，忽略结果。
        let _ = NSWorkspace::sharedWorkspace().openURL(&nsurl);
    }
}
