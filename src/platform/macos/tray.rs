//! macOS 系统托盘（`NSStatusItem`）——**缝合骨架**。
//!
//! 公共构建器 API 与 win32 同形（`Tray` / `TrayMenuItem` / `TrayCtx`）。构建器为纯数据，
//! 已完整实现；安装与 `TrayCtx` 的窗口操作待在 macOS 上接入 AppKit（见各 `todo!()`）。
//!
//! 实现指引：`NSStatusBar::systemStatusBar().statusItemWithLength(...)`，图标用
//! `NSImage`（由 `icon` 的 RGBA 构造），右键菜单用 `NSMenu`/`NSMenuItem`（`checked`→state，
//! `separator`→`separatorItem`），左键/双击经 status item 的 action target 分发。

use std::cell::Cell;
use std::rc::Rc;

type TrayFn = Box<dyn FnMut(&mut TrayCtx)>;

/// 托盘回调上下文：操作窗口与弹通知（不暴露原生句柄）。
///
/// 占位：尚未持有 `NSWindow`/status item 引用；实现时补充字段。
pub struct TrayCtx {
    _priv: (),
}

impl TrayCtx {
    /// 显示并前置窗口（托盘最常见动作）。
    pub fn show_window(&self) {
        // TODO(macos): NSWindow::makeKeyAndOrderFront + NSApp::activate。
        todo!("macOS TrayCtx::show_window");
    }
    /// 隐藏窗口（最小化到托盘）。
    pub fn hide_window(&self) {
        // TODO(macos): NSWindow::orderOut。
        todo!("macOS TrayCtx::hide_window");
    }
    /// 退出应用。
    pub fn quit(&self) {
        // TODO(macos): NSApp::terminate。
        todo!("macOS TrayCtx::quit");
    }
    /// 弹出系统通知（标题 + 正文）。
    pub fn notify(&self, title: &str, body: &str) {
        // TODO(macos): UNUserNotificationCenter / NSUserNotification(旧)。
        let _ = (title, body);
        todo!("macOS TrayCtx::notify");
    }
}

enum ItemKind {
    Action { label: String, checked: Option<Rc<Cell<bool>>>, cb: TrayFn },
    Separator,
}

/// 托盘右键菜单项：普通项 / 勾选项 / 分隔线。
pub struct TrayMenuItem {
    #[allow(dead_code)] // 字段由（待实现的）NSMenu 安装逻辑读取。
    kind: ItemKind,
}

impl TrayMenuItem {
    /// 普通项：点击触发回调。
    pub fn item(label: impl Into<String>, cb: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        Self { kind: ItemKind::Action { label: label.into(), checked: None, cb: Box::new(cb) } }
    }
    /// 勾选项：`checked` 绑定状态，菜单弹出时按当前值显示对勾；点击触发回调
    /// （回调内自行翻转 `checked`，框架不自动改）。
    pub fn check(
        label: impl Into<String>,
        checked: Rc<Cell<bool>>,
        cb: impl FnMut(&mut TrayCtx) + 'static,
    ) -> Self {
        Self {
            kind: ItemKind::Action { label: label.into(), checked: Some(checked), cb: Box::new(cb) },
        }
    }
    /// 分隔线。
    pub fn separator() -> Self {
        Self { kind: ItemKind::Separator }
    }
}

/// 托盘图标构建器。交给 `App::tray(...)`。
#[derive(Default)]
#[allow(dead_code)] // 字段由（待实现的）NSStatusItem 安装逻辑读取。
pub struct Tray {
    tooltip: String,
    icon: Option<(u32, u32, Vec<u8>)>,
    on_left_click: Option<TrayFn>,
    on_double_click: Option<TrayFn>,
    items: Vec<TrayMenuItem>,
}

impl Tray {
    pub fn new() -> Self {
        Self::default()
    }
    /// 鼠标悬停提示。
    pub fn tooltip(mut self, s: impl Into<String>) -> Self {
        self.tooltip = s.into();
        self
    }
    /// 自定义图标：原始非预乘 RGBA8（`rgba.len()==w*h*4`）。未设则用系统默认图标。
    pub fn icon_rgba(mut self, w: u32, h: u32, rgba: &[u8]) -> Self {
        self.icon = Some((w, h, rgba.to_vec()));
        self
    }
    /// 左键单击回调（常见用于显隐窗口）。
    pub fn on_left_click(mut self, f: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        self.on_left_click = Some(Box::new(f));
        self
    }
    /// 左键双击回调。
    pub fn on_double_click(mut self, f: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        self.on_double_click = Some(Box::new(f));
        self
    }
    /// 右键菜单项（普通/勾选/分隔线）。
    pub fn menu(mut self, items: Vec<TrayMenuItem>) -> Self {
        self.items = items;
        self
    }
}
