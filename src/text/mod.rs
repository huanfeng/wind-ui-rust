//! 文字引擎抽象。Windows 下由 DirectWrite 实现（`dwrite`）；macOS 下由 Core Text 实现（`coretext`）。

#[cfg(windows)]
pub mod dwrite;
#[cfg(windows)]
pub use dwrite::DWriteEngine;

#[cfg(target_os = "macos")]
pub mod coretext;
#[cfg(target_os = "macos")]
pub use coretext::CoreTextEngine;

/// 当前平台的具体文字引擎类型。`app` 层用此别名持有引擎，避免 `cfg` 散落到宿主逻辑里。
#[cfg(windows)]
pub type PlatformTextEngine = DWriteEngine;
#[cfg(target_os = "macos")]
pub type PlatformTextEngine = CoreTextEngine;

use tiny_skia::Pixmap;

use crate::geometry::{Color, Rect, Size};
use crate::spec::Align;

/// 默认字重（DirectWrite NORMAL = 400）。
pub const WEIGHT_NORMAL: u16 = 400;

thread_local! {
    /// 当前文字字重（线程局部）：核心层在 measure/paint 前按 `Style.font_weight` 注入，
    /// 引擎构造字体格式时读取，免去逐个 draw/measure 签名携带 weight（仿 anim::set_paint_rect）。
    static WEIGHT: std::cell::Cell<u16> = const { std::cell::Cell::new(WEIGHT_NORMAL) };
}

/// 设置当前文字字重（核心层在测量/绘制文字前调用）。
pub fn set_weight(w: u16) {
    WEIGHT.with(|c| c.set(w));
}

/// 读取当前文字字重（文字引擎构造格式时调用）。
pub fn current_weight() -> u16 {
    WEIGHT.with(|c| c.get())
}

/// 文字测量与绘制接口。测量供布局阶段，绘制供 paint 阶段合成进 pixmap。
///
/// 坐标/字号约定：对外接口均为**逻辑单位**（dp）。引擎内部按 DPI scale 物理化
/// （measure 物理排版后 /scale 回逻辑，draw 物理排版并按 rect×scale 合成），
/// 使测量与绘制走同一物理字号路径——字体 hinting 非线性，绝不可线性外推。
pub trait TextEngine {
    /// 设置 DPI 缩放因子。
    fn set_scale(&mut self, _scale: f32) {}
    /// 文字尺寸。`max_width=None` 单行不换行；`Some(w)` 在宽度 w 内换行并返回多行尺寸。
    fn measure(
        &mut self,
        text: &str,
        family: Option<&str>,
        size: f32,
        max_width: Option<f32>,
    ) -> Size;
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
    fn measure(
        &mut self,
        text: &str,
        _family: Option<&str>,
        size: f32,
        _max_width: Option<f32>,
    ) -> Size {
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
