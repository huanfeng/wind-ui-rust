//! 文字引擎抽象。Windows 下由 DirectWrite 实现（`dwrite`）。

pub mod dwrite;

pub use dwrite::DWriteEngine;

use tiny_skia::Pixmap;

use crate::geometry::{Color, Rect, Size};
use crate::spec::Align;

/// 文字测量与绘制接口。测量供布局阶段，绘制供 paint 阶段合成进 pixmap。
///
/// 坐标/字号约定：对外接口均为**逻辑单位**（dp）。引擎内部按 DPI scale 物理化
/// （measure 物理排版后 /scale 回逻辑，draw 物理排版并按 rect×scale 合成），
/// 使测量与绘制走同一物理字号路径——字体 hinting 非线性，绝不可线性外推。
pub trait TextEngine {
    /// 设置 DPI 缩放因子。
    fn set_scale(&mut self, _scale: f32) {}
    /// 文字尺寸。`max_width=None` 单行不换行；`Some(w)` 在宽度 w 内换行并返回多行尺寸。
    fn measure(&mut self, text: &str, family: Option<&str>, size: f32, max_width: Option<f32>) -> Size;
    /// 在 `rect` 内按 `align` 水平对齐、垂直居中绘制文字，合成进 `pixmap`。
    /// `clip` 为可选裁剪矩形（滚动视口等），合成时仅写入该矩形内的像素。
    fn draw(
        &mut self,
        pixmap: &mut Pixmap,
        text: &str,
        rect: Rect,
        color: Color,
        align: Align,
        family: Option<&str>,
        size: f32,
        clip: Option<Rect>,
    );
}

/// 占位引擎：不渲染，按等宽近似估算尺寸。供无 DirectWrite 的单元测试使用。
pub struct NullTextEngine;

impl TextEngine for NullTextEngine {
    fn measure(&mut self, text: &str, _family: Option<&str>, size: f32, _max_width: Option<f32>) -> Size {
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
        _clip: Option<Rect>,
    ) {
    }
}
