//! GIF 解码（`gif` feature，gif crate）。只取首帧，动图另立项。
//!
//! GIF 的帧可以只覆盖逻辑画布的一部分（`left` / `top` / `width` / `height`），首帧也不例外。
//! 这里按逻辑画布尺寸建一张全透明的图，把首帧贴到它声明的位置上——直接拿帧缓冲当整张图，
//! 尺寸和位置都会错。

use super::{check_size, decoded};
use crate::render::image::{DecodedImage, ImageDecoder, ImageError};

pub(crate) struct GifDecoder;

impl ImageDecoder for GifDecoder {
    fn probe(&self, bytes: &[u8]) -> bool {
        bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
    }
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, ImageError> {
        let err = |e: ::gif::DecodingError| ImageError::Decode(format!("GIF：{e}"));
        let mut opts = ::gif::DecodeOptions::new();
        opts.set_color_output(::gif::ColorOutput::RGBA);
        let mut dec = opts.read_info(bytes).map_err(err)?;
        let (w, h) = (u32::from(dec.width()), u32::from(dec.height()));
        check_size(w, h)?;
        let frame = dec
            .read_next_frame()
            .map_err(err)?
            .ok_or_else(|| ImageError::Decode("GIF 没有任何帧".into()))?;
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        let (fx, fy) = (usize::from(frame.left), usize::from(frame.top));
        let (fw, fh) = (usize::from(frame.width), usize::from(frame.height));
        let (wu, hu) = (w as usize, h as usize);
        for y in 0..fh {
            let dy = fy + y;
            if dy >= hu {
                break;
            }
            // 帧越出画布的部分裁掉（坏文件常见，报错会让整张图看不了）。左边界就在画布外时
            // 整行都不可见；不能照常切片——空切片的起点越界一样会 panic。
            let n = fw.min(wu.saturating_sub(fx));
            if n == 0 {
                break;
            }
            let src = &frame.buffer[y * fw * 4..(y * fw + n) * 4];
            let d = (dy * wu + fx) * 4;
            rgba[d..d + n * 4].copy_from_slice(src);
        }
        decoded(w, h, rgba)
    }
    fn name(&self) -> &'static str {
        "gif"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4×2，第 0 个像素透明，其余：上行红、下行蓝。两帧（第二帧全绿），应只取首帧。
    const ANIMATED: &[u8] = include_bytes!("testdata/red_blue_4x2_anim.gif");
    /// 逻辑画布 4×4，唯一一帧 2×2 位于 (2,2)，纯红。
    const OFFSET: &[u8] = include_bytes!("testdata/offset_frame_4x4.gif");

    fn px(img: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width + x) * 4) as usize;
        img.rgba[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn probe_matches_both_versions() {
        assert!(GifDecoder.probe(ANIMATED));
        assert!(GifDecoder.probe(b"GIF87a...."));
        assert!(!GifDecoder.probe(b"GIF90a"));
    }

    #[test]
    fn first_frame_with_transparency() {
        let img = GifDecoder.decode(ANIMATED).unwrap();
        assert_eq!((img.width, img.height), (4, 2));
        assert_eq!(px(&img, 0, 0)[3], 0, "透明色索引应解成 alpha 0");
        assert_eq!(px(&img, 1, 0), [255, 0, 0, 255]);
        assert_eq!(px(&img, 3, 1), [0, 0, 255, 255]);
    }

    #[test]
    fn frame_is_placed_at_its_offset_on_logical_canvas() {
        let img = GifDecoder.decode(OFFSET).unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        assert_eq!(px(&img, 0, 0)[3], 0, "帧外区域应透明");
        assert_eq!(px(&img, 1, 1)[3], 0);
        assert_eq!(px(&img, 2, 2), [255, 0, 0, 255]);
        assert_eq!(px(&img, 3, 3), [255, 0, 0, 255]);
    }

    /// 帧描述符声明的位置在画布之外（gif crate 默认不校验）：应得到全透明的图而不是 panic。
    #[test]
    fn frame_outside_canvas_is_clipped_not_panicking() {
        let mut bytes = OFFSET.to_vec();
        // 文件头 6 + 逻辑屏幕描述 7 + 全局调色板 6 = 19 处是 0x2C，其后 2 字节是 left。
        assert_eq!(bytes[19], 0x2c);
        bytes[20..22].copy_from_slice(&10u16.to_le_bytes());
        let img = GifDecoder.decode(&bytes).unwrap();
        assert_eq!((img.width, img.height), (4, 4));
        assert!(img.rgba.chunks(4).all(|p| p[3] == 0));
    }

    #[test]
    fn truncated_gif_fails_without_panic() {
        assert!(GifDecoder.decode(&ANIMATED[..20]).is_err());
    }
}
