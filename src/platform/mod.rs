//! 平台抽象层。目前仅 Windows（`win32`）。
//!
//! 模块名用 `win32` 而非 `windows`，以免与外部 `windows` crate 冲突。

pub mod win32;

pub use win32::{run, WindowConfig};
