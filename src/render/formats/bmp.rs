//! BMP 解码（`bmp` feature，自写、零依赖）。
//!
//! 支持：
//! - 信息头：BITMAPCOREHEADER（12 字节，OS/2）与 BITMAPINFOHEADER 及其 V2–V5 扩展（≥40 字节）；
//! - 位深与压缩：BI_RGB 的 1/4/8 位调色板与 16/24/32 位，BI_BITFIELDS / BI_ALPHABITFIELDS
//!   的 16/32 位；
//! - 自下而上（高度为正）与自上而下（高度为负）两种行序。
//!
//! 不支持 RLE4 / RLE8 与内嵌 JPEG / PNG 的压缩方式，遇到时报错，不猜。
//!
//! 32 位 BI_RGB 的第 4 字节按惯例是「保留」，但很多程序在里面写了 alpha。这里的处理与
//! 浏览器一致：只要有一个像素的第 4 字节非零就当作 alpha，全为零则视为不透明——否则
//! 老程序写出的 32 位 BMP 会整张透明。
//!
//! [`decode_dib`] 同时服务 ICO：ICO 的位图条目就是去掉文件头的 DIB，高度记为两倍
//! （颜色数据后面紧跟 1 位的 AND 掩码）。

use super::{check_size, decoded};
use crate::render::image::{DecodedImage, ImageDecoder, ImageError};

pub(crate) struct BmpDecoder;

impl ImageDecoder for BmpDecoder {
    fn probe(&self, bytes: &[u8]) -> bool {
        bytes.len() >= 18 && bytes.starts_with(b"BM")
    }
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, ImageError> {
        let pixels_at = u32_at(bytes, 10)? as usize;
        decode_dib(bytes, 14, Some(pixels_at), false)
    }
    fn name(&self) -> &'static str {
        "bmp"
    }
}

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

fn truncated() -> ImageError {
    ImageError::Decode("BMP 数据截断".into())
}

fn u16_at(b: &[u8], i: usize) -> Result<u16, ImageError> {
    let s = b.get(i..i + 2).ok_or_else(truncated)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

pub(crate) fn u32_at(b: &[u8], i: usize) -> Result<u32, ImageError> {
    let s = b.get(i..i + 4).ok_or_else(truncated)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// 按掩码取一个通道并缩放到 8 位。掩码为 0 表示该通道不存在，返回 0。
fn channel(v: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let bits = (mask >> shift).count_ones();
    // 掩码来自文件，可能不连续（如 0x8000_0001）：x 会超出 bits 位能表示的范围。
    // 用 u64 算并钳到 255，坏文件只会出错色，不会溢出。
    let x = u64::from((v & mask) >> shift);
    if bits >= 8 {
        (x >> (bits - 8)).min(255) as u8
    } else {
        let max = (1u64 << bits) - 1;
        ((x * 255 + max / 2) / max).min(255) as u8
    }
}

/// 解码一段 DIB。
///
/// - `start`：DIB 信息头在 `buf` 里的起点；
/// - `pixels_at`：像素数据的绝对偏移（BMP 文件头里给出）；`None` 表示紧跟在调色板之后（ICO）；
/// - `ico`：按 ICO 条目解释——高度记为两倍，颜色数据后面跟 AND 掩码。
pub(crate) fn decode_dib(
    buf: &[u8],
    start: usize,
    pixels_at: Option<usize>,
    ico: bool,
) -> Result<DecodedImage, ImageError> {
    let hsize = u32_at(buf, start)? as usize;
    // 信息头不可能比整段数据还长；先拦下，后面按它做的偏移相加在 32 位目标上也不会溢出。
    if hsize > buf.len() {
        return Err(truncated());
    }
    let core = hsize == 12;
    let (w, h_raw, bpp, compression, clr_used) = if core {
        (
            i64::from(u16_at(buf, start + 4)?),
            i64::from(u16_at(buf, start + 6)?),
            u16_at(buf, start + 10)?,
            BI_RGB,
            0,
        )
    } else if hsize >= 40 {
        (
            i64::from(u32_at(buf, start + 4)? as i32),
            i64::from(u32_at(buf, start + 8)? as i32),
            u16_at(buf, start + 14)?,
            u32_at(buf, start + 16)?,
            u32_at(buf, start + 32)?,
        )
    } else {
        return Err(ImageError::Decode(format!("BMP 信息头长度 {hsize} 不认识")));
    };
    if w <= 0 || w > i64::from(u32::MAX) {
        return Err(ImageError::Decode(format!("BMP 宽度非法：{w}")));
    }
    let top_down = h_raw < 0;
    let mut h = h_raw.unsigned_abs();
    if ico {
        h /= 2;
    }
    let (w, h) = (w as u32, u32::try_from(h).map_err(|_| truncated())?);
    check_size(w, h)?;

    // 掩码：BI_RGB 用位深的默认值；位域压缩从 V2+ 信息头里读，或紧跟在 40 字节信息头之后。
    let mut extra_masks = 0;
    let masks: [u32; 4] = match compression {
        BI_RGB => match bpp {
            16 => [0x7c00, 0x03e0, 0x001f, 0],
            32 => [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000],
            _ => [0; 4],
        },
        BI_BITFIELDS | BI_ALPHABITFIELDS if bpp == 16 || bpp == 32 => {
            let with_alpha = compression == BI_ALPHABITFIELDS || hsize >= 56;
            let at = if hsize >= 52 {
                start + 40
            } else {
                start + hsize
            };
            if hsize < 52 {
                extra_masks = if compression == BI_ALPHABITFIELDS {
                    16
                } else {
                    12
                };
            }
            [
                u32_at(buf, at)?,
                u32_at(buf, at + 4)?,
                u32_at(buf, at + 8)?,
                if with_alpha { u32_at(buf, at + 12)? } else { 0 },
            ]
        }
        other => {
            return Err(ImageError::Decode(format!(
                "BMP 压缩方式 {other}（位深 {bpp}）不支持"
            )))
        }
    };

    // 调色板（≤8 位）。
    let palette_at = start + hsize + extra_masks;
    let mut palette: Vec<[u8; 3]> = Vec::new();
    let entry = if core { 3 } else { 4 };
    if matches!(bpp, 1 | 4 | 8) {
        let full = 1usize << bpp;
        let n = if clr_used == 0 {
            full
        } else {
            (clr_used as usize).min(full)
        };
        for i in 0..n {
            let at = palette_at + i * entry;
            let c = buf.get(at..at + 3).ok_or_else(truncated)?;
            palette.push([c[2], c[1], c[0]]);
        }
    } else if !matches!(bpp, 16 | 24 | 32) {
        return Err(ImageError::Decode(format!("BMP 位深 {bpp} 不支持")));
    }
    // 16/24/32 位也可以带一个可选调色板（`clr_used` 项，仅作显示优化，不参与解码）。
    // BMP 文件头给了像素偏移，不受影响；ICO 条目没有偏移，要把它跳过去。
    let skipped = if palette.is_empty() {
        (clr_used as usize).saturating_mul(entry)
    } else {
        palette.len() * entry
    };
    let pixels_at = pixels_at.unwrap_or(palette_at.saturating_add(skipped));

    let stride = (u64::from(w) * u64::from(bpp)).div_ceil(32) as usize * 4;
    let data = buf
        .get(pixels_at..)
        .filter(|d| d.len() as u64 >= stride as u64 * u64::from(h))
        .ok_or_else(truncated)?;

    let (wu, hu) = (w as usize, h as usize);
    let mut rgba = vec![0u8; wu * hu * 4];
    let mut any_alpha = false;
    for y in 0..hu {
        let src_row = if top_down { y } else { hu - 1 - y };
        let row = &data[src_row * stride..src_row * stride + stride];
        let out = &mut rgba[y * wu * 4..(y + 1) * wu * 4];
        for x in 0..wu {
            let px: [u8; 4] = match bpp {
                1 | 4 | 8 => {
                    let bits = bpp as usize;
                    let byte = row[x * bits / 8];
                    let shift = 8 - bits - (x * bits % 8);
                    let idx = (byte >> shift) as usize & ((1 << bits) - 1);
                    // 越界索引按黑色处理：坏文件常见，报错会让整张图都看不了。
                    let c = palette.get(idx).copied().unwrap_or([0, 0, 0]);
                    [c[0], c[1], c[2], 255]
                }
                24 => [row[x * 3 + 2], row[x * 3 + 1], row[x * 3], 255],
                16 => {
                    let v = u32::from(u16::from_le_bytes([row[x * 2], row[x * 2 + 1]]));
                    let a = if masks[3] == 0 {
                        255
                    } else {
                        channel(v, masks[3])
                    };
                    [
                        channel(v, masks[0]),
                        channel(v, masks[1]),
                        channel(v, masks[2]),
                        a,
                    ]
                }
                _ => {
                    let s = &row[x * 4..x * 4 + 4];
                    let v = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                    let a = if masks[3] == 0 {
                        255
                    } else {
                        channel(v, masks[3])
                    };
                    if a != 0 {
                        any_alpha = true;
                    }
                    [
                        channel(v, masks[0]),
                        channel(v, masks[1]),
                        channel(v, masks[2]),
                        a,
                    ]
                }
            };
            out[x * 4..x * 4 + 4].copy_from_slice(&px);
        }
    }

    // 32 位且 alpha 全零：老程序把第 4 字节当保留位，按不透明处理（见模块注释）。
    let alpha_usable = bpp == 32 && masks[3] != 0 && any_alpha;
    if bpp == 32 && masks[3] != 0 && !any_alpha {
        for px in rgba.as_chunks_mut::<4>().0 {
            px[3] = 255;
        }
    }

    // ICO：颜色数据后面是 1 位 AND 掩码（1 = 透明）。已有可用 alpha 时以 alpha 为准；
    // 缺掩码的条目（个别生成器省略）照常显示为不透明。
    if ico && !alpha_usable {
        let mask_stride = u64::from(w).div_ceil(32) as usize * 4;
        let mask_at = pixels_at + stride * hu;
        if let Some(mask) = buf.get(mask_at..mask_at + mask_stride * hu) {
            for y in 0..hu {
                let src_row = if top_down { y } else { hu - 1 - y };
                let row = &mask[src_row * mask_stride..(src_row + 1) * mask_stride];
                for x in 0..wu {
                    if row[x / 8] >> (7 - x % 8) & 1 == 1 {
                        rgba[(y * wu + x) * 4 + 3] = 0;
                    }
                }
            }
        }
    }

    decoded(w, h, rgba)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// 拼一个 BITMAPINFOHEADER（40 字节）。
    pub(crate) fn info_header(
        w: i32,
        h: i32,
        bpp: u16,
        compression: u32,
        clr_used: u32,
    ) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&40u32.to_le_bytes());
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&bpp.to_le_bytes());
        v.extend_from_slice(&compression.to_le_bytes());
        v.extend_from_slice(&[0; 12]); // sizeimage, x/y ppm
        v.extend_from_slice(&clr_used.to_le_bytes());
        v.extend_from_slice(&0u32.to_le_bytes());
        v
    }

    /// 文件头 + DIB（信息头、掩码、调色板、像素依次拼好传进来）。
    fn file(dib_before_pixels: &[u8], pixels: &[u8]) -> Vec<u8> {
        let off = 14 + dib_before_pixels.len();
        let mut v = b"BM".to_vec();
        v.extend_from_slice(&((off + pixels.len()) as u32).to_le_bytes());
        v.extend_from_slice(&[0; 4]);
        v.extend_from_slice(&(off as u32).to_le_bytes());
        v.extend_from_slice(dib_before_pixels);
        v.extend_from_slice(pixels);
        v
    }

    fn px(img: &DecodedImage, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * img.width + x) * 4) as usize;
        img.rgba[i..i + 4].try_into().unwrap()
    }

    const RED: [u8; 4] = [255, 0, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];

    #[test]
    fn probe_requires_bm_signature() {
        assert!(BmpDecoder.probe(&file(&info_header(1, 1, 24, 0, 0), &[0; 4])));
        assert!(!BmpDecoder.probe(b"BM"));
        assert!(
            !BmpDecoder.probe(&[0x89, b'P', b'N', b'G', 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        );
    }

    #[test]
    fn bottom_up_24bit_keeps_row_order_and_padding() {
        // 2×2：下行（文件里先存）红、蓝；上行蓝、红。每行 6 字节补到 8。
        let pixels = [
            0, 0, 255, 255, 0, 0, 0, 0, // 下行：红、蓝（BGR）
            255, 0, 0, 0, 0, 255, 0, 0, // 上行：蓝、红
        ];
        let img = BmpDecoder
            .decode(&file(&info_header(2, 2, 24, 0, 0), &pixels))
            .unwrap();
        assert_eq!((img.width, img.height), (2, 2));
        assert_eq!(px(&img, 0, 0), BLUE);
        assert_eq!(px(&img, 1, 0), RED);
        assert_eq!(px(&img, 0, 1), RED);
        assert_eq!(px(&img, 1, 1), BLUE);
    }

    #[test]
    fn from_bytes_recognizes_bmp_via_registry() {
        let f = file(&info_header(3, 2, 24, 0, 0), &[0; 24]);
        let img = crate::render::image::Image::from_bytes(&f).unwrap();
        assert_eq!((img.width(), img.height()), (3, 2));
    }

    #[test]
    fn top_down_negative_height_is_not_flipped() {
        let pixels = [0, 0, 255, 0, 255, 0, 0, 0]; // 上行红；下行蓝（各补 1 字节）
        let img = BmpDecoder
            .decode(&file(&info_header(1, -2, 24, 0, 0), &pixels))
            .unwrap();
        assert_eq!(px(&img, 0, 0), RED);
        assert_eq!(px(&img, 0, 1), BLUE);
    }

    #[test]
    fn one_bit_palette_reads_msb_first() {
        let mut dib = info_header(3, 1, 1, 0, 2);
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]); // 0=红 1=蓝
        let img = BmpDecoder
            .decode(&file(&dib, &[0b0100_0000, 0, 0, 0]))
            .unwrap();
        assert_eq!(px(&img, 0, 0), RED);
        assert_eq!(px(&img, 1, 0), BLUE);
        assert_eq!(px(&img, 2, 0), RED);
    }

    #[test]
    fn four_and_eight_bit_palettes() {
        let mut dib = info_header(2, 1, 4, 0, 2);
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        let img = BmpDecoder.decode(&file(&dib, &[0x10, 0, 0, 0])).unwrap();
        assert_eq!((px(&img, 0, 0), px(&img, 1, 0)), (BLUE, RED));

        let mut dib = info_header(2, 1, 8, 0, 2);
        dib.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
        let img = BmpDecoder.decode(&file(&dib, &[1, 0, 0, 0])).unwrap();
        assert_eq!((px(&img, 0, 0), px(&img, 1, 0)), (BLUE, RED));
    }

    #[test]
    fn rgb555_16bit_scales_to_full_range() {
        // 0x7C00 = 纯红 5 位满值 → 255。
        let img = BmpDecoder
            .decode(&file(&info_header(1, 1, 16, 0, 0), &[0x00, 0x7c, 0, 0]))
            .unwrap();
        assert_eq!(px(&img, 0, 0), RED);
    }

    #[test]
    fn rgb565_bitfields_16bit() {
        let mut dib = info_header(1, 1, 16, BI_BITFIELDS, 0);
        for m in [0xf800u32, 0x07e0, 0x001f] {
            dib.extend_from_slice(&m.to_le_bytes());
        }
        // 0x001F = 纯蓝。
        let img = BmpDecoder.decode(&file(&dib, &[0x1f, 0x00, 0, 0])).unwrap();
        assert_eq!(px(&img, 0, 0), BLUE);
    }

    #[test]
    fn rgb32_with_zero_alpha_everywhere_is_opaque() {
        let img = BmpDecoder
            .decode(&file(&info_header(1, 1, 32, 0, 0), &[0, 0, 255, 0]))
            .unwrap();
        assert_eq!(px(&img, 0, 0), RED);
    }

    #[test]
    fn rgb32_with_some_alpha_keeps_alpha() {
        let img = BmpDecoder
            .decode(&file(
                &info_header(2, 1, 32, 0, 0),
                &[0, 0, 255, 0, 255, 0, 0, 128],
            ))
            .unwrap();
        assert_eq!(px(&img, 0, 0), [255, 0, 0, 0]);
        assert_eq!(px(&img, 1, 0), [0, 0, 255, 128]);
    }

    /// 不连续的位域掩码（恶意文件）：不能溢出 panic。
    #[test]
    fn non_contiguous_bitfield_mask_does_not_overflow() {
        let mut dib = info_header(1, 1, 32, BI_BITFIELDS, 0);
        for m in [0x8000_0001u32, 0x0000_ff00, 0x00ff_0000] {
            dib.extend_from_slice(&m.to_le_bytes());
        }
        assert!(BmpDecoder.decode(&file(&dib, &[0xff; 4])).is_ok());
        assert_eq!(channel(0xffff_ffff, 0x8000_0001), 255);
    }

    /// ICO 里的 24 位 DIB 带了可选调色板（clr_used=2）：像素起点要跳过它。
    #[test]
    fn optional_palette_before_24bit_pixels_is_skipped() {
        let mut dib = info_header(1, 2, 24, 0, 2); // ICO 约定：高度记两倍
        dib.extend_from_slice(&[9; 8]); // 两项调色板（应被跳过）
        dib.extend_from_slice(&[0, 0, 255, 0]); // 1 像素红 + 补齐
        dib.extend_from_slice(&[0; 4]); // AND 掩码
        let img = decode_dib(&dib, 0, None, true).unwrap();
        assert_eq!(px(&img, 0, 0), RED);
    }

    #[test]
    fn header_size_larger_than_data_fails_cleanly() {
        let mut dib = info_header(1, 1, 24, 0, 0);
        dib[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_dib(&dib, 0, None, false).is_err());
    }

    #[test]
    fn rle_compression_is_rejected_not_guessed() {
        let r = BmpDecoder.decode(&file(&info_header(1, 1, 8, 1, 0), &[0; 8]));
        assert!(matches!(r, Err(ImageError::Decode(_))));
    }

    #[test]
    fn truncated_pixels_and_huge_dimensions_fail_cleanly() {
        assert!(BmpDecoder
            .decode(&file(&info_header(4, 4, 24, 0, 0), &[0; 8]))
            .is_err());
        assert!(BmpDecoder
            .decode(&file(&info_header(100_000, 100_000, 24, 0, 0), &[]))
            .is_err());
    }
}
