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
pub mod spec;
pub mod style;
pub(crate) mod sync;
pub mod text;
pub mod theme;
pub mod ui;

pub mod prelude {
    pub use crate::app::{App, ThemeHandle};
    pub use crate::event::{CursorShape, MenuItem};
    pub use crate::geometry::{Color, Insets, Point, Rect, Size};
    pub use crate::platform::{PickDialog, Tray, TrayCtx, TrayMenuItem};
    pub use crate::render::image::{Fit, Image, ImageError, VisualState};
    pub use crate::render::Gradient;
    pub use crate::spec::{Align, Axis, Dimension};
    pub use crate::style::{Brush, Role, Shadow, Style};
    pub use crate::sync::Sender;
    pub use crate::theme::{Intent, Theme};
    pub use crate::ui::{
        Element, ImageContent, ImageView, Link, Truncate, WindowButton, WindowButtonKind,
    };
}
