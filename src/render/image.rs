//! 图片资源与可插拔解码框架。
//!
//! 分两块：
//! - `Image`：解码后的不可变资源（内部 `Rc<Pixmap>`，构建期一次性解码，paint 期零解码）。
//! - 解码框架：`ImageDecoder` trait + thread-local 注册表。核心内置 `PngDecoder`
//!   （零新依赖，走 tiny-skia 原生 PNG 解码）；启用 `svg` feature（默认开）再内置
//!   `SvgDecoder`（resvg 光栅化）；JPEG/WebP 等由使用方 `register_decoder` 注入，
//!   核心代码与公共 API 零破坏。
//!
//! 注册表照搬 `theme` 的 thread-local 模式（UI 单线程，免加锁），PNG 预置，
//! 保证 `from_png_bytes` 永远可用。
//!
//! SVG 是矢量、分辨率无关的：内置解码器按 SVG 的**固有尺寸**一次光栅化（顺着
//! `ImageDecoder` 的定长 RGBA 契约接入，`from_bytes`/`from_file` 自动识别 `.svg`）；
//! 需要 HiDPI 清晰度的调用方改用 [`Image::from_svg_bytes`] 显式指定光栅宽度（如传 2×
//! 逻辑宽度）。resvg 渲染产物经 `DecodedImage` 原始 RGBA「统一货币」转入本项目的
//! tiny-skia，与位图解码同一条入库路径。

use std::cell::RefCell;
use std::fmt;
use std::path::Path;
use std::rc::Rc;

use tiny_skia::{ColorU8, IntSize, Pixmap};

use crate::geometry::{Color, Size};

/// 加载失败时占位框的默认逻辑尺寸（dp），保证布局不塌陷。
pub const PLACEHOLDER_SIZE: i32 = 48;

/// 控件视觉状态。控件把自己的内部状态映射成它，传给图片原语决定调制（透明度/换图）。
/// 与具体控件解耦——原语只认识这套通用词汇。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VisualState {
    #[default]
    Normal,
    Hover,
    Pressed,
    Selected,
    Disabled,
}

impl VisualState {
    /// 该状态下图片的默认不透明度（禁用置灰，其余不变）。可由消费方覆盖。
    pub fn opacity(self) -> f32 {
        match self {
            VisualState::Disabled => 0.38,
            _ => 1.0,
        }
    }
}

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
        Ok(DecodedImage {
            width: w,
            height: h,
            rgba,
        })
    }
    fn name(&self) -> &'static str {
        "png"
    }
}

/// 内置 SVG 解码器（`svg` feature）：用 resvg 解析并按 SVG 固有尺寸光栅化。
///
/// resvg 渲染后端即 tiny-skia（已与本项目对齐到同一版本）。这里仍走 `DecodedImage`
/// 原始 RGBA，与位图解码同一条入库路径，逻辑统一。
#[cfg(feature = "svg")]
struct SvgDecoder;

#[cfg(feature = "svg")]
impl SvgDecoder {
    /// 解析 + 光栅化为非预乘 RGBA8。`target_width=None` 用 SVG 固有尺寸；
    /// `Some(w)` 按该宽度等比缩放光栅（高度随宽高比，至少 1px）。
    fn rasterize(bytes: &[u8], target_width: Option<u32>) -> Result<DecodedImage, ImageError> {
        use resvg::{tiny_skia, usvg};

        #[allow(unused_mut)]
        let mut opt = usvg::Options::default();
        // 启用 svg-text 时挂上系统字体库（thread_local 缓存，避免每次解码重扫字体）。
        #[cfg(feature = "svg-text")]
        {
            thread_local! {
                static FONTDB: std::sync::Arc<usvg::fontdb::Database> = {
                    let mut db = usvg::fontdb::Database::new();
                    db.load_system_fonts();
                    std::sync::Arc::new(db)
                };
            }
            opt.fontdb = FONTDB.with(|db| db.clone());
        }
        let tree = usvg::Tree::from_data(bytes, &opt)
            .map_err(|e| ImageError::Decode(format!("SVG 解析失败: {e}")))?;
        let size = tree.size();
        let (iw, ih) = (size.width(), size.height());
        if iw <= 0.0 || ih <= 0.0 {
            return Err(ImageError::Decode("SVG 固有尺寸非法".into()));
        }
        // 目标像素尺寸：指定宽度则等比缩放，否则取固有尺寸（向上取整保证不丢边）。
        let (tw, th) = match target_width {
            Some(w) if w > 0 => {
                let scale = w as f32 / iw;
                (w, (ih * scale).round().max(1.0) as u32)
            }
            _ => (iw.ceil() as u32, ih.ceil() as u32),
        };
        let mut pm = tiny_skia::Pixmap::new(tw, th)
            .ok_or_else(|| ImageError::Decode("SVG 目标尺寸过大或非法".into()))?;
        let transform = tiny_skia::Transform::from_scale(tw as f32 / iw, th as f32 / ih);
        resvg::render(&tree, transform, &mut pm.as_mut());

        // resvg 产物为预乘 RGBA8，反预乘成 DecodedImage 货币。
        let (w, h) = (pm.width(), pm.height());
        let mut rgba = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for p in pm.pixels() {
            let c = p.demultiply();
            rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        Ok(DecodedImage {
            width: w,
            height: h,
            rgba,
        })
    }
}

#[cfg(feature = "svg")]
impl ImageDecoder for SvgDecoder {
    fn probe(&self, bytes: &[u8]) -> bool {
        // svgz：gzip 包装的 SVG（usvg::Tree::from_data 透明解压）。
        if bytes.starts_with(&[0x1f, 0x8b]) {
            return true;
        }
        // 纯文本 SVG：前 1KB 内出现 `<svg`（容忍 BOM/XML 声明/注释/大小写）。
        let n = bytes.len().min(1024);
        bytes[..n]
            .windows(4)
            .any(|w| w.eq_ignore_ascii_case(b"<svg"))
    }
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, ImageError> {
        Self::rasterize(bytes, None)
    }
    fn name(&self) -> &'static str {
        "svg"
    }
}

thread_local! {
    /// 解码器注册表，PNG 预置（启用 `svg` feature 时追加 SVG）。后注册者优先级更高（覆盖在前）。
    static DECODERS: RefCell<Vec<Box<dyn ImageDecoder>>> = RefCell::new({
        #[allow(unused_mut)]
        let mut v: Vec<Box<dyn ImageDecoder>> = vec![Box::new(PngDecoder)];
        #[cfg(feature = "svg")]
        v.push(Box::new(SvgDecoder));
        v
    });
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

    /// 从 SVG 字节光栅化（`svg` feature）。`target_width=None` 用 SVG 固有尺寸；
    /// `Some(w)` 按该宽度等比光栅——HiDPI 求清晰可传 2× 逻辑宽度。
    ///
    /// `from_bytes`/`from_file` 已能自动识别 `.svg` 并按固有尺寸光栅化；本方法是想
    /// 显式控制光栅分辨率时的出口。含文字的 SVG 因未启用 resvg 文字特性而不渲染文字。
    #[cfg(feature = "svg")]
    pub fn from_svg_bytes(bytes: &[u8], target_width: Option<u32>) -> Result<Self, ImageError> {
        Self::from_decoded(SvgDecoder::rasterize(bytes, target_width)?)
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
        Self::from_decoded(DecodedImage {
            width,
            height,
            rgba: rgba.to_vec(),
        })
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
        Self {
            pixmap: Rc::new(pm),
            w,
            h,
        }
    }

    /// 固有逻辑尺寸（1 图片像素 = 1 逻辑 dp，再由 DPI 缩放）。
    pub fn size(&self) -> Size {
        Size::new(self.w as i32, self.h as i32)
    }

    /// 模板着色副本：rgb 替换为 `color`，alpha 乘以 `color.a`（保形）。
    /// 用于单色图标随主题/状态变色（彩色图请勿用，会丢失原色）。
    pub fn tinted(&self, color: Color) -> Image {
        let size = IntSize::from_wh(self.w, self.h).expect("尺寸已在构造时校验");
        let mut data = Vec::with_capacity((self.w as usize) * (self.h as usize) * 4);
        for p in self.pixmap.pixels() {
            // p 为预乘像素，alpha 即覆盖度；按模板着色重建直色后再预乘。
            let a = ((p.alpha() as u16 * color.a as u16) / 255) as u8;
            let c = ColorU8::from_rgba(color.r, color.g, color.b, a).premultiply();
            data.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        let pm = Pixmap::from_vec(data, size).expect("着色 Pixmap 尺寸匹配");
        Self {
            pixmap: Rc::new(pm),
            w: self.w,
            h: self.h,
        }
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

    /// 稳定缓存键：底层 `Rc<Pixmap>` 指针。同一图片的 `Rc` 克隆共享此 id
    /// （供 D2D 后端按图片身份缓存 device-dependent 位图，避免每帧重建）。
    /// 仅 Windows + `d2d` 后端是消费者；其余平台/配置下无人使用，显式放行 dead_code。
    #[cfg_attr(not(all(windows, feature = "d2d")), allow(dead_code))]
    pub(crate) fn cache_id(&self) -> usize {
        std::rc::Rc::as_ptr(&self.pixmap) as usize
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
        assert!(matches!(
            Image::from_bytes(&junk),
            Err(ImageError::UnsupportedFormat)
        ));
    }

    #[test]
    fn from_rgba_validates_length() {
        // 2×2×4 = 16 字节，正确。
        let ok = Image::from_rgba(2, 2, &[0u8; 16]);
        assert!(ok.is_ok());
        // 长度不符。
        assert!(matches!(
            Image::from_rgba(2, 2, &[0u8; 15]),
            Err(ImageError::InvalidRgba)
        ));
        // 零尺寸。
        assert!(matches!(
            Image::from_rgba(0, 2, &[]),
            Err(ImageError::InvalidRgba)
        ));
    }

    #[test]
    fn from_rgba_preserves_size() {
        let img = Image::from_rgba(5, 7, &[128u8; 5 * 7 * 4]).unwrap();
        assert_eq!(img.size(), Size::new(5, 7));
    }

    #[test]
    fn tinted_recolors_keeping_alpha() {
        // 白色不透明 + 半透明像素混合源。
        let src = Image::from_rgba(2, 1, &[255, 255, 255, 255, 255, 255, 255, 128]).unwrap();
        let red = src.tinted(Color::rgb(255, 0, 0));
        assert_eq!(red.size(), Size::new(2, 1));
        // 取回非预乘验证：第 1 像素红不透明，第 2 像素红半透明。
        let pm = red.pixmap();
        let p0 = pm.pixel(0, 0).unwrap().demultiply();
        assert_eq!(
            (p0.red(), p0.green(), p0.blue(), p0.alpha()),
            (255, 0, 0, 255)
        );
        let p1 = pm.pixel(1, 0).unwrap().demultiply();
        assert_eq!((p1.red(), p1.green(), p1.blue()), (255, 0, 0));
        assert!(p1.alpha() < 200, "半透明 alpha 应保留，实得 {}", p1.alpha());
    }

    #[test]
    fn visual_state_opacity() {
        assert_eq!(VisualState::Disabled.opacity(), 0.38);
        assert_eq!(VisualState::Normal.opacity(), 1.0);
        assert_eq!(VisualState::Hover.opacity(), 1.0);
    }

    #[test]
    fn png_decoder_probe() {
        assert!(PngDecoder.probe(PNG_MAGIC));
        assert!(!PngDecoder.probe(&[0, 1, 2]));
        assert!(!PngDecoder.probe(&[0xff, 0xd8])); // JPEG 头不应命中
    }

    #[cfg(feature = "svg")]
    mod svg {
        use super::*;

        /// 一张 10×6 的红色矩形 SVG。
        const RED_SVG: &[u8] =
            br##"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="6"><rect width="10" height="6" fill="#ff0000"/></svg>"##;

        #[test]
        fn svg_decoder_probe() {
            assert!(SvgDecoder.probe(RED_SVG));
            // 带 XML 声明前缀仍命中。
            assert!(SvgDecoder.probe(br#"<?xml version="1.0"?><svg></svg>"#));
            // 大小写不敏感。
            assert!(SvgDecoder.probe(b"<SVG></SVG>"));
            // svgz（gzip 魔数）命中。
            assert!(SvgDecoder.probe(&[0x1f, 0x8b, 0x08, 0x00]));
            // PNG / 垃圾不命中。
            assert!(!SvgDecoder.probe(PNG_MAGIC));
            assert!(!SvgDecoder.probe(&[0, 1, 2, 3]));
        }

        #[test]
        fn from_svg_bytes_uses_intrinsic_size() {
            let img = Image::from_svg_bytes(RED_SVG, None).unwrap();
            assert_eq!(img.size(), Size::new(10, 6));
        }

        #[test]
        fn from_svg_bytes_target_width_scales_keeping_aspect() {
            // 指定宽度 20 → 高度按 10:6 等比 → 12。
            let img = Image::from_svg_bytes(RED_SVG, Some(20)).unwrap();
            assert_eq!(img.size(), Size::new(20, 12));
        }

        #[test]
        fn from_bytes_sniffs_svg_via_registry() {
            let img = Image::from_bytes(RED_SVG).unwrap();
            assert_eq!(img.size(), Size::new(10, 6));
        }

        #[test]
        fn rasterized_svg_has_red_pixel() {
            // 光栅化后中心像素应为红色（验证确实渲染了内容，而非空白）。
            let img = Image::from_svg_bytes(RED_SVG, Some(20)).unwrap();
            let p = img.pixmap().pixel(10, 6).unwrap().demultiply();
            assert!(
                p.red() > 200 && p.green() < 60 && p.blue() < 60,
                "中心应为红色，实得 ({},{},{})",
                p.red(),
                p.green(),
                p.blue()
            );
        }

        #[test]
        fn invalid_svg_is_decode_error() {
            assert!(matches!(
                Image::from_svg_bytes(b"<svg not valid", None),
                Err(ImageError::Decode(_))
            ));
        }

        /// 文字 SVG 应渲染出字形（依赖系统字体；svg-text feature 才编译）。
        #[cfg(feature = "svg-text")]
        #[test]
        fn renders_svg_text_glyphs() {
            const TEXT_SVG: &[u8] =
                br##"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="40"><text x="4" y="30" font-size="30" font-family="Arial" fill="#000000">Hi</text></svg>"##;
            let img = Image::from_svg_bytes(TEXT_SVG, None).unwrap();
            let opaque = img
                .pixmap()
                .pixels()
                .iter()
                .filter(|p| p.alpha() > 0)
                .count();
            assert!(opaque > 0, "文字 SVG 应光栅出字形像素，实得 {opaque}");
        }
    }
}
