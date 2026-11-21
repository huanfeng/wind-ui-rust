//! 可选图片格式的解码器：每种格式一个 feature，默认全关（见 `Cargo.toml` 的 `[features]`）。
//!
//! 都实现 [`ImageDecoder`]，由 [`register_builtin`] 在注册表初始化时按 cfg 放进去，
//! 因此 `Image::from_bytes` / `from_file` / `Element::image*` 自动识别，公共 API 不变。
//!
//! | feature | 实现 | 说明 |
//! |---|---|---|
//! | `jpeg` | zune-jpeg | 按 EXIF Orientation 旋转 / 翻转到正确朝向 |
//! | `gif` | gif | 只取首帧 |
//! | `webp` | image-webp | 有损 + 无损；动图只取首帧 |
//! | `bmp` | 自写 | BI_RGB 1/4/8/24/32 位、BI_BITFIELDS 16/32 位；不支持 RLE |
//! | `ico` | 自写 | 条目为 PNG 或 DIB；默认取最大的条目，见 `Image::from_ico_bytes` |

use super::image::ImageDecoder;
#[cfg(any(feature = "jpeg", feature = "gif", feature = "webp", feature = "bmp"))]
use super::image::{DecodedImage, ImageError};

#[cfg(feature = "bmp")]
pub(crate) mod bmp;
#[cfg(feature = "gif")]
mod gif;
#[cfg(feature = "ico")]
pub(crate) mod ico;
#[cfg(feature = "jpeg")]
mod jpeg;
#[cfg(feature = "webp")]
mod webp;

/// 单张图片允许的最大像素数（约 6300×6300），与剪贴板图片的上限一致。
///
/// 解码前先按文件头声明的尺寸检查：手工构造的小文件可以声明巨大的尺寸，不设上限会
/// 让一次解码直接申请上 GB 内存（解压炸弹）。
#[cfg(any(feature = "jpeg", feature = "gif", feature = "webp", feature = "bmp"))]
pub(crate) const MAX_PIXELS: u64 = 40_000_000;

/// 检查声明的尺寸：非零且不超过 [`MAX_PIXELS`]。
#[cfg(any(feature = "jpeg", feature = "gif", feature = "webp", feature = "bmp"))]
pub(crate) fn check_size(width: u32, height: u32) -> Result<(), ImageError> {
    if width == 0 || height == 0 {
        return Err(ImageError::Decode(format!(
            "图片尺寸非法：{width}×{height}"
        )));
    }
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(ImageError::Decode(format!(
            "图片过大：{width}×{height} 超过 {MAX_PIXELS} 像素上限"
        )));
    }
    Ok(())
}

/// RGB8 → 非预乘 RGBA8（alpha 补 255）。
#[cfg(feature = "webp")]
pub(crate) fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for px in rgb.as_chunks::<3>().0 {
        out.extend_from_slice(&[px[0], px[1], px[2], 255]);
    }
    out
}

/// 把已启用的可选格式解码器追加进注册表。
pub(super) fn register_builtin(decoders: &mut Vec<Box<dyn ImageDecoder>>) {
    #[cfg(feature = "jpeg")]
    decoders.push(Box::new(jpeg::JpegDecoder));
    #[cfg(feature = "gif")]
    decoders.push(Box::new(gif::GifDecoder));
    #[cfg(feature = "webp")]
    decoders.push(Box::new(webp::WebpDecoder));
    #[cfg(feature = "bmp")]
    decoders.push(Box::new(bmp::BmpDecoder));
    #[cfg(feature = "ico")]
    decoders.push(Box::new(ico::IcoDecoder));
    let _ = decoders;
}

/// 解码结果的统一构造，顺带核对长度（解码库出错时宁可报错也不交出错位的像素）。
#[cfg(any(feature = "jpeg", feature = "gif", feature = "webp", feature = "bmp"))]
pub(crate) fn decoded(width: u32, height: u32, rgba: Vec<u8>) -> Result<DecodedImage, ImageError> {
    if rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 {
        return Err(ImageError::Decode(format!(
            "解码输出长度 {} 与 {width}×{height} 不符",
            rgba.len()
        )));
    }
    Ok(DecodedImage {
        width,
        height,
        rgba,
    })
}

#[cfg(all(
    test,
    any(feature = "jpeg", feature = "gif", feature = "webp", feature = "bmp")
))]
mod tests {
    use super::*;

    #[test]
    fn check_size_rejects_zero_and_oversized() {
        assert!(check_size(1, 1).is_ok());
        assert!(check_size(0, 10).is_err());
        assert!(check_size(10, 0).is_err());
        assert!(check_size(100_000, 100_000).is_err());
    }

    /// 经公共入口 `Image::from_bytes` 走注册表：已启用的格式都应被识别并解出正确尺寸。
    #[test]
    fn registry_dispatches_enabled_formats() {
        use crate::render::image::Image;
        #[allow(unused_mut)]
        let mut cases: Vec<(&str, &[u8], (u32, u32))> = Vec::new();
        #[cfg(feature = "jpeg")]
        cases.push((
            "jpeg",
            include_bytes!("testdata/red_blue_16x8_orient6.jpg"),
            (8, 16),
        ));
        #[cfg(feature = "gif")]
        cases.push((
            "gif",
            include_bytes!("testdata/red_blue_4x2_anim.gif"),
            (4, 2),
        ));
        #[cfg(feature = "webp")]
        cases.push((
            "webp",
            include_bytes!("testdata/red_blue_4x2_lossless.webp"),
            (4, 2),
        ));
        for (name, bytes, size) in cases {
            let img = Image::from_bytes(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!((img.width(), img.height()), size, "{name}");
        }
    }

    #[test]
    fn decoded_rejects_length_mismatch() {
        assert!(decoded(2, 1, vec![0; 8]).is_ok());
        assert!(decoded(2, 1, vec![0; 7]).is_err());
    }
}
