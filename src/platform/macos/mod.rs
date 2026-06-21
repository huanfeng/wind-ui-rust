//! macOS 平台后端（Cocoa/AppKit）——**缝合骨架**。
//!
//! 对外暴露与 `win32` 同形的 API：`run` / `open_url` / `Tray`·`TrayCtx`·`TrayMenuItem` /
//! `clipboard::MacClipboard`。当前为纯 Rust 占位（`unimplemented!()` / `todo!()`），
//! 不依赖 objc2，保证 macOS 上 `cargo build` 即可通过、拿到全部缝合点。
//!
//! 实现指引（在 macOS 上填入，逐项对照 `platform/win32/mod.rs`）：
//! - 事件循环：`NSApplication::sharedApplication()` + `NSWindow` + 自定义 `NSView`。
//! - 呈现：`NSView::drawRect` 中用 `CGBitmapContext`/`CGImage` 包裹 tiny-skia `Pixmap`
//!   （RGBA8 预乘）blit 到屏（对应 win32 的 `SetDIBitsToDevice`）。注意 CG 坐标 Y 轴向上。
//! - 输入：`NSEvent`（mouseDown/Up/Moved/Dragged/scrollWheel）→ `PointerEvent`；
//!   `keyDown` + `NSTextInputClient` → `KeyEvent` 与输入法合成（对应 win32 的 `WM_CHAR`/IME）。
//! - HiDPI：`NSWindow::backingScaleFactor` → `handler.set_scale`（对应 `GetDpiForWindow`）。
//! - 触摸/惯性：触控板 `scrollWheel`（含 momentum phase）天然提供惯性，可直接喂 `on_pan`，
//!   或仅转发原生滚动、关掉自研 fling（二选一，见 win32 的 WM_TOUCH 对照）。
//! - 无边框：`NSWindow` styleMask 去 `titled`、`movableByWindowBackground`，命中区交
//!   `window_drag_at` / `interactive_at`（对应 win32 `WM_NCHITTEST`）。
//! - 截屏模式：`cfg.screenshot` 路径应复用「离屏渲染一帧存 PNG」逻辑——建议把 win32 的
//!   `run_offscreen` 上移为平台无关的共享函数，两端共用（当前 TODO）。

pub mod clipboard;
pub mod tray;

pub use tray::{Tray, TrayCtx, TrayMenuItem};

use super::{AppHandler, WindowConfig};

/// 运行应用：截屏模式离屏渲染存盘；否则创建窗口进入事件循环。
pub fn run(cfg: WindowConfig, handler: Box<dyn AppHandler>) {
    // 全局动画开关：显式配置优先；否则截屏路径恒开、窗口路径随系统设置（对应 win32::run）。
    let os_default = cfg.screenshot.is_some() || os_animations_enabled();
    crate::anim::set_enabled(cfg.animations.unwrap_or(os_default));

    // TODO(macos): if let Some(path) = &cfg.screenshot { run_offscreen(...); return; }
    //              否则 run_windowed(cfg, handler)。
    let _ = handler;
    unimplemented!("macOS 窗口后端尚未实现（缝合骨架）：见本模块文档与 platform/win32/mod.rs 对照");
}

/// 查询系统“显示动画”偏好（减弱动态效果）。占位：默认开。
fn os_animations_enabled() -> bool {
    // TODO(macos): 读取 NSWorkspace.accessibilityDisplayShouldReduceMotion，取反。
    true
}

/// 用系统默认程序打开 URL/路径（链接点击）。
pub fn open_url(url: &str) {
    // TODO(macos): NSWorkspace::sharedWorkspace().openURL(NSURL::URLWithString(url))。
    let _ = url;
    unimplemented!("macOS open_url 尚未实现（缝合骨架）");
}
