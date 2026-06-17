//! 图片资源与可插拔解码框架。
//!
//! 分两块：
//! - `Image`：解码后的不可变资源（内部 `Rc<Pixmap>`，构建期一次性解码，paint 期零解码）。
//! - 解码框架：`ImageDecoder` trait + thread-local 注册表。核心只内置 `PngDecoder`
//!   （零新依赖，走 tiny-skia 原生 PNG 解码）；JPEG/WebP 等由使用方 `register_decoder`
//!   注入，核心代码与公共 API 零破坏。
//!
//! 注册表照搬 `theme` 的 thread-local 模式（UI 单线程，免加锁），PNG 预置，
//! 保证 `from_png_bytes` 永远可用。

use std::cell::RefCell;
use std::fmt;
use std::path::Path;
use std::rc::Rc;

use tiny_skia::{ColorU8, IntSize, Pixmap};

use crate::geometry::Size;

/// 加载失败时占位框的默认逻辑尺寸（dp），保证布局不塌陷。
pub const PLACEHOLDER_SIZE: i32 = 48;

/// 图片在控件框内的适配缩放模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// 等比缩放完整显示（可能留白）。
    #[default]
    Contain,
    /// 等比缩放铺满、裁掉溢出。
    Cover,
    /// 非等比拉伸填满。
    Fill,
    /// 原始像素 1:1（仍受 DPI 缩放）。
    None,
}

/// 图片加载/解码错误。
#[derive(Debug)]
pub enum ImageError {
    /// 文件读取失败。
    Io(std::io::Error),
    /// 无任何已注册解码器能识别该字节流。
    UnsupportedFormat,
    /// 解码器内部错误（含底层错误描述）。
    Decode(String),
    /// `from_rgba` 的像素长度与 `width*height*4` 不符。
    InvalidRgba,
}

impl fmt::Display for ImageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImageError::Io(e) => write!(f, "图片读取失败: {e}"),
            ImageError::UnsupportedFormat => write!(f, "无可用解码器识别该图片格式"),
            ImageError::Decode(m) => write!(f, "图片解码失败: {m}"),
            ImageError::InvalidRgba => write!(f, "RGBA 数据长度与 width*height*4 不符"),
        }
    }
}

impl std::error::Error for ImageError {}

impl From<std::io::Error> for ImageError {
    fn from(e: std::io::Error) -> Self {
        ImageError::Io(e)
    }
}

/// 解码产物：非预乘 RGBA8 像素 + 尺寸。第三方解码器（如包 image crate）以此为统一货币，
/// `Image` 内部再转 tiny-skia 的预乘格式。
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// 非预乘 RGBA8，长度须等于 `width*height*4`。
    pub rgba: Vec<u8>,
}

/// 可插拔图片解码器。核心内置 PNG；其它格式由使用方注册。
pub trait ImageDecoder {
    /// 是否能解码该字节流（通常按文件头魔数判断）。
    fn probe(&self, bytes: &[u8]) -> bool;
    /// 解码为非预乘 RGBA8。
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, ImageError>;
    /// 诊断用名称。
    fn name(&self) -> &'static str;
}

/// 内置 PNG 解码器：走 tiny-skia 原生 `decode_png`，再反预乘为 RGBA8 入 `DecodedImage`。
struct PngDecoder;

/// PNG 文件头魔数。
const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

impl ImageDecoder for PngDecoder {
    fn probe(&self, bytes: &[u8]) -> bool {
        bytes.len() >= PNG_MAGIC.len() && &bytes[..PNG_MAGIC.len()] == PNG_MAGIC
    }
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, ImageError> {
        let pm = Pixmap::decode_png(bytes).map_err(|e| ImageError::Decode(format!("{e}")))?;
        let (w, h) = (pm.width(), pm.height());
        let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for p in pm.pixels() {
            let c = p.demultiply();
            rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        Ok(DecodedImage { width: w, height: h, rgba })
    }
    fn name(&self) -> &'static str {
        "png"
    }
}

thread_local! {
    /// 解码器注册表，PNG 预置。后注册者优先级更高（覆盖在前）。
    static DECODERS: RefCell<Vec<Box<dyn ImageDecoder>>> =
        RefCell::new(vec![Box::new(PngDecoder)]);
}

/// 注册一个额外解码器（如包 `image` crate 的 JPEG/WebP 解码器）。
///
/// 后注册者在 `from_bytes` 嗅探时优先匹配，可覆盖内置实现。
pub fn register_decoder(decoder: Box<dyn ImageDecoder>) {
    DECODERS.with(|r| r.borrow_mut().push(decoder));
}

/// 用注册表嗅探并解码：从后往前找首个 `probe` 命中的解码器。
fn decode_with_registry(bytes: &[u8]) -> Result<DecodedImage, ImageError> {
    DECODERS.with(|r| {
        let decoders = r.borrow();
        for d in decoders.iter().rev() {
            if d.probe(bytes) {
                return d.decode(bytes);
            }
        }
        Err(ImageError::UnsupportedFormat)
    })
}

/// 解码后的图片资源。`Rc` 共享，克隆廉价；paint 期只做 blit，不再解码。
#[derive(Clone)]
pub struct Image {
    pixmap: Rc<Pixmap>,
    w: u32,
    h: u32,
}

impl Image {
    /// 从文件读取并解码（按字节嗅探格式，自适配已注册的解码器）。
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ImageError> {
        let bytes = std::fs::read(path)?;
        Self::from_bytes(&bytes)
    }

    /// 从字节流解码：嗅探格式后分发给匹配的解码器。
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ImageError> {
        let decoded = decode_with_registry(bytes)?;
        Self::from_decoded(decoded)
    }

    /// 便捷直通 PNG：跳过注册表嗅探，直接走 tiny-skia 原生解码（无反预乘往返）。
    pub fn from_png_bytes(bytes: &[u8]) -> Result<Self, ImageError> {
        let pm = Pixmap::decode_png(bytes).map_err(|e| ImageError::Decode(format!("{e}")))?;
        Ok(Self::from_pixmap(pm))
    }

    /// 从原始非预乘 RGBA8 构造。`rgba.len()` 须等于 `width*height*4`。
    pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Self, ImageError> {
        if width == 0 || height == 0 {
            return Err(ImageError::InvalidRgba);
        }
        let expect = (width as usize) * (height as usize) * 4;
        if rgba.len() != expect {
            return Err(ImageError::InvalidRgba);
        }
        Self::from_decoded(DecodedImage { width, height, rgba: rgba.to_vec() })
    }

    /// 把非预乘 RGBA8 转为 tiny-skia 预乘 Pixmap。
    fn from_decoded(d: DecodedImage) -> Result<Self, ImageError> {
        let size = IntSize::from_wh(d.width, d.height).ok_or(ImageError::InvalidRgba)?;
        let mut data = Vec::with_capacity(d.rgba.len());
        for px in d.rgba.chunks_exact(4) {
            let c = ColorU8::from_rgba(px[0], px[1], px[2], px[3]).premultiply();
            data.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        let pm = Pixmap::from_vec(data, size).ok_or(ImageError::InvalidRgba)?;
        Ok(Self::from_pixmap(pm))
    }

    fn from_pixmap(pm: Pixmap) -> Self {
        let (w, h) = (pm.width(), pm.height());
        Self { pixmap: Rc::new(pm), w, h }
    }

    /// 固有逻辑尺寸（1 图片像素 = 1 逻辑 dp，再由 DPI 缩放）。
    pub fn size(&self) -> Size {
        Size::new(self.w as i32, self.h as i32)
    }

    /// 像素宽。
    pub fn width(&self) -> u32 {
        self.w
    }
    /// 像素高。
    pub fn height(&self) -> u32 {
        self.h
    }

    /// 后端 Pixmap 引用（供渲染层 blit）。
    pub(crate) fn pixmap(&self) -> &Pixmap {
        &self.pixmap
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造一张 w×h 纯色 PNG 的字节（红色不透明）。
    fn red_png(w: u32, h: u32) -> Vec<u8> {
        let mut pm = Pixmap::new(w, h).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(255, 0, 0, 255));
        pm.encode_png().unwrap()
    }

    #[test]
    fn from_png_bytes_reports_size() {
        let png = red_png(4, 3);
        let img = Image::from_png_bytes(&png).unwrap();
        assert_eq!(img.size(), Size::new(4, 3));
    }

    #[test]
    fn from_bytes_sniffs_png() {
        let png = red_png(2, 2);
        let img = Image::from_bytes(&png).unwrap();
        assert_eq!(img.size(), Size::new(2, 2));
    }

    #[test]
    fn from_bytes_rejects_unknown_format() {
        let junk = [0u8, 1, 2, 3, 4, 5, 6, 7];
        assert!(matches!(Image::from_bytes(&junk), Err(ImageError::UnsupportedFormat)));
    }

    #[test]
    fn from_rgba_validates_length() {
        // 2×2×4 = 16 字节，正确。
        let ok = Image::from_rgba(2, 2, &[0u8; 16]);
        assert!(ok.is_ok());
        // 长度不符。
        assert!(matches!(Image::from_rgba(2, 2, &[0u8; 15]), Err(ImageError::InvalidRgba)));
        // 零尺寸。
        assert!(matches!(Image::from_rgba(0, 2, &[]), Err(ImageError::InvalidRgba)));
    }

    #[test]
    fn from_rgba_preserves_size() {
        let img = Image::from_rgba(5, 7, &[128u8; 5 * 7 * 4]).unwrap();
        assert_eq!(img.size(), Size::new(5, 7));
    }

    #[test]
    fn png_decoder_probe() {
        assert!(PngDecoder.probe(PNG_MAGIC));
        assert!(!PngDecoder.probe(&[0, 1, 2]));
        assert!(!PngDecoder.probe(&[0xff, 0xd8])); // JPEG 头不应命中
    }
}
