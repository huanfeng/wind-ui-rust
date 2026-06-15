//! windui — 轻量 Windows 桌面 GUI 框架。
//!
//! 见 `docs/DESIGN.md` 架构设计与 `docs/ROADMAP.md` 实施路线图。

pub mod app;
pub mod geometry;
pub mod platform;

pub mod prelude {
    pub use crate::app::App;
    pub use crate::geometry::{Color, Insets, Point, Rect, Size};
}
