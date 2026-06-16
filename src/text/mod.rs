//! 文字引擎抽象。Windows 下由 DirectWrite 实现（`dwrite`）。

pub mod dwrite;

pub use dwrite::DWriteEngine;

use tiny_skia::Pixmap;

use crate::geometry::{Color, Rect, Size};
use crate::spec::Align;

/// 文字测量与绘制接口。测量供布局阶段，绘制供 paint 阶段合成进 pixmap。
pub trait TextEngine {
    /// 单行固有尺寸（不换行）。
    fn measure(&mut self, text: &str, family: Option<&str>, size: f32) -> Size;
    /// 在 `rect` 内按 `align` 水平对齐、垂直居中绘制文字，合成进 `pixmap`。
    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        rect: Rect,
        color: Color,
        align: Align,
        family: Option<&str>,
        size: f32,
    );
}

/// 占位引擎：不渲染，按等宽近似估算尺寸。供无 DirectWrite 的单元测试使用。
pub struct NullTextEngine;

impl TextEngine for NullTextEngine {
    fn measure(&mut self, text: &str, _family: Option<&str>, size: f32) -> Size {
        let w = (text.chars().count() as f32 * size * 0.6).ceil() as i32;
        Size::new(w, size.ceil() as i32)
    }
    fn draw(
        &mut self,
        _pixmap: &mut Pixmap,
        _text: &str,
        _rect: Rect,
        _color: Color,
        _align: Align,
        _family: Option<&str>,
        _size: f32,
    ) {
    }
}
