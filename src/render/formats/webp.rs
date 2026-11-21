//! WebP 解码（`webp` feature，image-webp）。有损 + 无损；动图只取首帧。

use super::{check_size, decoded, rgb_to_rgba};
use crate::render::image::{DecodedImage, ImageDecoder, ImageError};

pub(crate) struct WebpDecoder;

impl ImageDecoder for WebpDecoder {
    fn probe(&self, bytes: &[u8]) -> bool {
        bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP"
    }
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, ImageError> {
        let err = |e: image_webp::DecodingError| ImageError::Decode(format!("WebP：{e}"));
        let mut dec = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).map_err(err)?;
        let (w, h) = dec.dimensions();
        check_size(w, h)?;
        let len = dec
            .output_buffer_size()
            .ok_or_else(|| ImageError::Decode("WebP 输出尺寸溢出".into()))?;
        let mut buf = vec![0u8; len];
        // 动图时 read_image 给的是首帧。
        dec.read_image(&mut buf).map_err(err)?;
        let rgba = if dec.has_alpha() {
            buf
        } else {
            rgb_to_rgba(&buf)
        };
        decoded(w, h, rgba)
    }
    fn name(&self) -> &'static str {
        "webp"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 无损，4×2：左两列红不透明，右两列蓝、alpha 128。
    const LOSSLESS: &[u8] = include_bytes!("testdata/red_blue_4x2_lossless.webp");
    /// 有损（无 alpha），16×8：左红右蓝。
    const LOSSY: &[u8] = include_bytes!("testdata/red_blue_16x8_lossy.webp");

    fn px(img: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width + x) * 4) as usize;
        img.rgba[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn probe_needs_riff_and_webp_tags() {
        assert!(WebpDecoder.probe(LOSSLESS));
        assert!(!WebpDecoder.probe(b"RIFF\0\0\0\0WAVE"));
    }

    #[test]
    fn lossless_keeps_exact_pixels_and_alpha() {
        let img = WebpDecoder.decode(LOSSLESS).unwrap();
        assert_eq!((img.width, img.height), (4, 2));
        assert_eq!(px(&img, 0, 0), [255, 0, 0, 255]);
        assert_eq!(px(&img, 3, 1), [0, 0, 255, 128]);
    }

    #[test]
    fn lossy_without_alpha_is_opaque() {
        let img = WebpDecoder.decode(LOSSY).unwrap();
        assert_eq!((img.width, img.height), (16, 8));
        let (l, r) = (px(&img, 2, 4), px(&img, 13, 4));
        assert!(l[0] > 200 && l[2] < 60 && l[3] == 255, "{l:?}");
        assert!(r[2] > 200 && r[0] < 60 && r[3] == 255, "{r:?}");
    }

    #[test]
    fn truncated_webp_fails_without_panic() {
        assert!(WebpDecoder.decode(&LOSSLESS[..16]).is_err());
    }
}
