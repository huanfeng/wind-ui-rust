//! macOS 文字引擎（Core Text）——**缝合骨架**。
//!
//! 当前为纯 Rust 占位实现：`measure` 用等宽近似（与 `NullTextEngine` 同启发式），
//! `draw` 暂为空操作。这样在 Core Text 实体接入前，macOS 上窗口与布局即可跑通
//! （文字尺寸不精确、不渲染字形，但应用可运行）。
//!
//! 实现指引（在 macOS 上填入，对照 `text/dwrite.rs`）：
//! - `measure`：`CTLine`/`CTFramesetter` 排版后取 typographic bounds；`max_width=Some(w)`
//!   时用 `CTFramesetter` 在宽度内折行，返回多行尺寸。字号需按 `scale` 物理化后再 /scale 回逻辑，
//!   与 `draw` 走同一物理路径（hinting 非线性，禁止线性外推）。
//! - `draw`：用 `CGBitmapContext` 包裹 `pixmap` 的像素缓冲（RGBA8 预乘），`CTLineDraw`/
//!   `CTFrameDraw` 直接绘入；按 `rect`×`scale` 物理化定位，水平按 `align`、垂直居中；
//!   `clip` 命中时用 `CGContextClipToRect` 限制写入区域。
//! - 颜色：`Color`(RGBA) → `CGColor`；注意 Core Graphics 坐标系 Y 轴向上，需翻转。

use tiny_skia::Pixmap;

use super::TextEngine;
use crate::geometry::{Color, Rect, Size};
use crate::spec::Align;

/// Core Text 文字引擎。当前为占位骨架（见模块文档）。
pub struct CoreTextEngine {
    /// DPI 缩放因子（逻辑→物理）。实体实现据此物理化字号与排版。
    scale: f32,
}

impl CoreTextEngine {
    pub fn new() -> Self {
        Self { scale: 1.0 }
    }
}

impl Default for CoreTextEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TextEngine for CoreTextEngine {
    fn set_scale(&mut self, scale: f32) {
        self.scale = scale;
    }

    fn measure(&mut self, text: &str, _family: Option<&str>, size: f32, max_width: Option<f32>) -> Size {
        // TODO(macos): 替换为 Core Text 真实测量（CTLine / CTFramesetter）。
        // 占位：等宽近似，单行；给定 max_width 时按字符宽度粗略折行估高。
        let char_w = size * 0.6;
        let line_h = size.ceil().max(1.0);
        let n = text.chars().count() as f32;
        match max_width {
            Some(w) if w > 0.0 && char_w > 0.0 => {
                let per_line = (w / char_w).floor().max(1.0);
                let lines = (n / per_line).ceil().max(1.0);
                Size::new(w.ceil() as i32, (lines * line_h).ceil() as i32)
            }
            _ => Size::new((n * char_w).ceil() as i32, line_h as i32),
        }
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
        // TODO(macos): 用 CGBitmapContext 包裹 pixmap，CTLineDraw/CTFrameDraw 绘入。
    }
}
