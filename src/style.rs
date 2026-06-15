//! 节点视觉样式。

use crate::geometry::Color;
use crate::spec::Align;

/// 背景/边框/文字等视觉属性。核心层统一绘制背景与边框，widget 绘制内容。
#[derive(Debug, Clone)]
pub struct Style {
    /// 背景填充色（None = 透明）。
    pub bg: Option<Color>,
    /// 边框（颜色, 线宽 px）。
    pub border: Option<(Color, i32)>,
    /// 圆角半径 px。
    pub corner_radius: f32,
    /// 前景/文字色。
    pub fg: Color,
    /// 字号 px。
    pub font_size: f32,
    /// 字体族（None = 系统默认）。
    pub font_family: Option<String>,
    /// 文字水平对齐。
    pub text_align: Align,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            bg: None,
            border: None,
            corner_radius: 0.0,
            fg: Color::hex(0x1A1A1A),
            font_size: 14.0,
            font_family: None,
            text_align: Align::Start,
        }
    }
}
