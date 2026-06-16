//! windui — 轻量 Windows 桌面 GUI 框架。
//!
//! 见 `docs/DESIGN.md` 架构设计与 `docs/ROADMAP.md` 实施路线图。

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
pub mod text;
pub mod theme;
pub mod ui;

pub mod prelude {
    pub use crate::app::App;
    pub use crate::geometry::{Color, Insets, Point, Rect, Size};
    pub use crate::spec::{Align, Axis, Dimension};
    pub use crate::style::Style;
    pub use crate::theme::Theme;
    pub use crate::ui::Element;
}
