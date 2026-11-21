//! JPEG 解码（`jpeg` feature，zune-jpeg）。
//!
//! 解码库只给出按存储顺序排列的像素，不处理 EXIF Orientation。手机照片常把「拍摄时
//! 怎么拿手机」记在这个标签里，而不是真的旋转像素——不处理的话竖拍的照片会横着显示。
//! 这里读出标签（只看 IFD0 的 0x0112，几十行就够，不引 EXIF 库），解码后按它旋转 / 翻转。

use super::{check_size, decoded};
use crate::render::image::{DecodedImage, ImageDecoder, ImageError};
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

pub(crate) struct JpegDecoder;

impl ImageDecoder for JpegDecoder {
    fn probe(&self, bytes: &[u8]) -> bool {
        bytes.starts_with(&[0xff, 0xd8, 0xff])
    }
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, ImageError> {
        let err = |e: zune_jpeg::errors::DecodeErrors| ImageError::Decode(format!("JPEG：{e:?}"));
        let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA);
        let mut dec = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(bytes), opts);
        dec.decode_headers().map_err(err)?;
        let info = dec
            .info()
            .ok_or_else(|| ImageError::Decode("JPEG 头信息缺失".into()))?;
        let (w, h) = (u32::from(info.width), u32::from(info.height));
        check_size(w, h)?;
        let orientation = dec.exif().and_then(|e| exif_orientation(e)).unwrap_or(1);
        let rgba = dec.decode().map_err(err)?;
        let img = decoded(w, h, rgba)?;
        Ok(orient(img, orientation))
    }
    fn name(&self) -> &'static str {
        "jpeg"
    }
}

/// 从 EXIF（自 TIFF 头起）读 IFD0 的 Orientation（0x0112），取值 1–8；读不到返回 `None`。
fn exif_orientation(tiff: &[u8]) -> Option<u16> {
    let le = match tiff.get(..4)? {
        [b'I', b'I', 42, 0] => true,
        [b'M', b'M', 0, 42] => false,
        _ => return None,
    };
    let u16_at = |i: usize| -> Option<u16> {
        let b = tiff.get(i..i + 2)?;
        Some(if le {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        })
    };
    let u32_at = |i: usize| -> Option<u32> {
        let b = tiff.get(i..i + 4)?;
        Some(if le {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    for i in 0..count {
        let at = ifd + 2 + i * 12;
        // 类型 3 = SHORT，值放在 4 字节值域的头两个字节。
        if u16_at(at)? == 0x0112 && u16_at(at + 2)? == 3 {
            let v = u16_at(at + 8)?;
            return (1..=8).contains(&v).then_some(v);
        }
    }
    None
}

/// 按 EXIF Orientation 把像素转到正确朝向。5–8 会交换宽高。
fn orient(img: DecodedImage, orientation: u16) -> DecodedImage {
    if !(2..=8).contains(&orientation) {
        return img;
    }
    let (w, h) = (img.width as usize, img.height as usize);
    let swap = orientation >= 5;
    let (dw, dh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u8; img.rgba.len()];
    for sy in 0..h {
        for sx in 0..w {
            let (dx, dy) = match orientation {
                2 => (w - 1 - sx, sy),
                3 => (w - 1 - sx, h - 1 - sy),
                4 => (sx, h - 1 - sy),
                5 => (sy, sx),
                6 => (h - 1 - sy, sx),
                7 => (h - 1 - sy, w - 1 - sx),
                _ => (sy, w - 1 - sx), // 8
            };
            let s = (sy * w + sx) * 4;
            let d = (dy * dw + dx) * 4;
            out[d..d + 4].copy_from_slice(&img.rgba[s..s + 4]);
        }
    }
    DecodedImage {
        width: dw as u32,
        height: dh as u32,
        rgba: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: &[u8] = include_bytes!("testdata/red_blue_16x8.jpg");
    const ROTATED: &[u8] = include_bytes!("testdata/red_blue_16x8_orient6.jpg");
    const GRAY: &[u8] = include_bytes!("testdata/gray_8x8.jpg");

    fn px(img: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width + x) * 4) as usize;
        img.rgba[i..i + 4].try_into().unwrap()
    }

    /// 有损压缩：只判主色，容差放宽。
    fn is_red(p: [u8; 4]) -> bool {
        p[0] > 200 && p[1] < 60 && p[2] < 60 && p[3] == 255
    }
    fn is_blue(p: [u8; 4]) -> bool {
        p[0] < 60 && p[1] < 60 && p[2] > 200 && p[3] == 255
    }

    #[test]
    fn probe_matches_soi_marker_only() {
        assert!(JpegDecoder.probe(PLAIN));
        assert!(!JpegDecoder.probe(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn decodes_left_red_right_blue() {
        let img = JpegDecoder.decode(PLAIN).unwrap();
        assert_eq!((img.width, img.height), (16, 8));
        assert!(is_red(px(&img, 2, 4)), "{:?}", px(&img, 2, 4));
        assert!(is_blue(px(&img, 13, 4)), "{:?}", px(&img, 13, 4));
    }

    #[test]
    fn exif_orientation_6_rotates_clockwise() {
        // 存储的是 16×8 左红右蓝；Orientation=6 表示显示时顺时针转 90°：8×16、上红下蓝。
        let img = JpegDecoder.decode(ROTATED).unwrap();
        assert_eq!((img.width, img.height), (8, 16));
        assert!(is_red(px(&img, 4, 2)), "{:?}", px(&img, 4, 2));
        assert!(is_blue(px(&img, 4, 13)), "{:?}", px(&img, 4, 13));
    }

    #[test]
    fn grayscale_jpeg_expands_to_rgba() {
        let img = JpegDecoder.decode(GRAY).unwrap();
        assert_eq!((img.width, img.height), (8, 8));
        let p = px(&img, 4, 4);
        assert!(
            p[0].abs_diff(128) < 8 && p[0] == p[1] && p[1] == p[2] && p[3] == 255,
            "{p:?}"
        );
    }

    #[test]
    fn truncated_jpeg_fails_without_panic() {
        assert!(JpegDecoder.decode(&PLAIN[..PLAIN.len() / 3]).is_err());
    }

    /// 2×1 的源图（左 A 右 B），按全部 8 种朝向变换，逐一核对落点。
    #[test]
    fn orient_covers_all_eight_values() {
        const A: [u8; 4] = [1, 0, 0, 255];
        const B: [u8; 4] = [2, 0, 0, 255];
        let src = || DecodedImage {
            width: 2,
            height: 1,
            rgba: [A, B].concat(),
        };
        // (朝向, 期望尺寸, 期望像素按行展开)
        type Case = (u16, (u32, u32), Vec<[u8; 4]>);
        let cases: [Case; 8] = [
            (1, (2, 1), vec![A, B]),
            (2, (2, 1), vec![B, A]),
            (3, (2, 1), vec![B, A]),
            (4, (2, 1), vec![A, B]),
            (5, (1, 2), vec![A, B]),
            (6, (1, 2), vec![A, B]),
            (7, (1, 2), vec![B, A]),
            (8, (1, 2), vec![B, A]),
        ];
        for (o, size, want) in cases {
            let img = orient(src(), o);
            assert_eq!((img.width, img.height), size, "朝向 {o}");
            assert_eq!(img.rgba, want.concat(), "朝向 {o}");
        }
    }

    #[test]
    fn exif_orientation_reads_both_byte_orders() {
        // 最小 TIFF：头 + IFD0 一项 Orientation。
        let le = [
            b'I', b'I', 42, 0, 8, 0, 0, 0, // 头，IFD0 在偏移 8
            1, 0, // 1 项
            0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0,
        ];
        assert_eq!(exif_orientation(&le), Some(6));
        let be = [
            b'M', b'M', 0, 42, 0, 0, 0, 8, //
            0, 1, //
            0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 8, 0, 0,
        ];
        assert_eq!(exif_orientation(&be), Some(8));
        assert_eq!(exif_orientation(b"junk"), None);
    }
}
