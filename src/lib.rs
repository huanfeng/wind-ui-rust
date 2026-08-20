//! windui — 轻量跨平台桌面 GUI 框架（Windows：Win32+DirectWrite；macOS：Cocoa+CoreText，开发中）。
//!
//! - 第三方使用指南（API 风格/规范/扩展）：`docs/API_GUIDE.md`
//! - 架构设计：`docs/DESIGN.md`；实施路线图：`docs/ROADMAP.md`

// 图形绘制 API 以标量坐标传参（x,y,w,h,radius,width,paint）是有意设计，放宽该 lint。
#![allow(clippy::too_many_arguments)]

pub mod anim;
pub mod app;
pub mod core;
pub mod event;
pub mod geometry;
pub mod platform;
pub mod render;
pub mod signal;
pub(crate) mod single_instance;
/// 单实例对外面:启动早期的闸门 [`claim_instance`]。模块其余部分是内部实现。
pub use single_instance::{claim_instance, InstanceRole};
pub mod spec;
pub mod style;
pub(crate) mod sync;
pub mod testing;
pub mod text;
pub mod theme;
pub mod ui;

pub mod prelude {
    pub use crate::app::{App, HotkeyHandle, ThemeHandle, Window};
    pub use crate::event::{
        CursorShape, Hotkey, HotkeyCtx, HotkeyOp, Key, MenuItem, Mods, ToastKind,
    };
    pub use crate::geometry::{Color, Insets, Point, Rect, Size};
    pub use crate::platform::{PickDialog, Renderer, Tray, TrayCtx, TrayMenuItem};
    pub use crate::render::image::{Fit, Image, ImageError, VisualState};
    pub use crate::render::{Gradient, PixmapTarget, RenderTarget};
    pub use crate::signal::{signal, Signal};
    pub use crate::spec::{Align, Axis, Dimension};
    pub use crate::style::{Brush, Edges, Role, Shadow, Style};
    pub use crate::sync::Sender;
    pub use crate::theme::{Intent, Len, TableTheme, Theme};
    pub use crate::ui::{
        CaretStyle, CheckMenuItem, CommitMode, DropdownItem, Element, ImageContent, ImageView,
        Link, Para, RichColor, RichDoc, SortKey, SortOrder, SortStyle, SpanStyle, TextContent,
        Truncate, WindowButton, WindowButtonKind,
    };
}
