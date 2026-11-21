//! ICO 解码（`ico` feature，自写、零依赖）。
//!
//! ICO 是一个容器：目录里列出若干条目（同一图标的不同尺寸 / 位深），每个条目要么是一张
//! 完整的 PNG（Vista 起 256×256 的大图标通常如此），要么是去掉文件头的 DIB（交给
//! [`super::bmp::decode_dib`]，高度记两倍、带 AND 掩码）。
//!
//! 选哪个条目：经注册表解码（`Image::from_bytes`）时取**最大**的一个，同尺寸取位深高的；
//! 要按显示尺寸挑，用 `Image::from_ico_bytes(bytes, Some(目标边长))`——取不小于目标的
//! 最小条目，都比目标小时取最大的。目录里的尺寸只用来挑选，解码出的真实尺寸以条目
//! 内容为准。

use super::bmp::{decode_dib, u32_at};
use crate::render::image::{DecodedImage, ImageDecoder, ImageError, PngDecoder};

pub(crate) struct IcoDecoder;

impl ImageDecoder for IcoDecoder {
    fn probe(&self, bytes: &[u8]) -> bool {
        // 头 6 字节：保留 0、类型 1（图标）、条目数。`00 00 01 00` 太短太常见，再核对
        // 条目数非零、目录装得下、首个条目的保留字节为 0，降低误认其它格式的概率。
        if bytes.len() < 22 || bytes[..4] != [0, 0, 1, 0] {
            return false;
        }
        let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
        count > 0 && bytes.len() >= 6 + count * 16 && bytes[6 + 3] == 0
    }
    fn decode(&self, bytes: &[u8]) -> Result<DecodedImage, ImageError> {
        decode(bytes, None)
    }
    fn name(&self) -> &'static str {
        "ico"
    }
}

struct Entry {
    /// 目录里声明的边长（0 记作 256）。
    size: u32,
    bpp: u16,
    offset: usize,
    len: usize,
}

fn entries(bytes: &[u8]) -> Result<Vec<Entry>, ImageError> {
    if bytes.len() < 6 || bytes[..4] != [0, 0, 1, 0] {
        return Err(ImageError::Decode("不是 ICO 文件".into()));
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let at = 6 + i * 16;
        let e = bytes
            .get(at..at + 16)
            .ok_or_else(|| ImageError::Decode("ICO 目录截断".into()))?;
        let side = |b: u8| if b == 0 { 256 } else { u32::from(b) };
        out.push(Entry {
            size: side(e[0]).max(side(e[1])),
            bpp: u16::from_le_bytes([e[6], e[7]]),
            len: u32_at(e, 8)? as usize,
            offset: u32_at(e, 12)? as usize,
        });
    }
    if out.is_empty() {
        return Err(ImageError::Decode("ICO 没有任何条目".into()));
    }
    Ok(out)
}

/// 按目标边长挑条目，见模块注释。
fn pick(entries: &[Entry], target: Option<u32>) -> &Entry {
    let largest = entries
        .iter()
        .max_by_key(|e| (e.size, e.bpp))
        .expect("entries 非空");
    let Some(t) = target else { return largest };
    entries
        .iter()
        .filter(|e| e.size >= t)
        .min_by_key(|e| (e.size, std::cmp::Reverse(e.bpp)))
        .unwrap_or(largest)
}

/// 解码 ICO，`target` 见模块注释。
pub(crate) fn decode(bytes: &[u8], target: Option<u32>) -> Result<DecodedImage, ImageError> {
    let list = entries(bytes)?;
    let e = pick(&list, target);
    let data = bytes
        .get(e.offset..e.offset.saturating_add(e.len))
        .ok_or_else(|| ImageError::Decode("ICO 条目超出文件范围".into()))?;
    if PngDecoder.probe(data) {
        PngDecoder.decode(data)
    } else {
        decode_dib(data, 0, None, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::formats::bmp::tests::info_header;

    /// 一个 w×w 的 DIB 条目：32 位、每像素同一颜色（BGRA），带全零 AND 掩码。
    fn dib_entry(w: u32, bgra: [u8; 4], bpp: u16) -> Vec<u8> {
        let mut v = info_header(w as i32, (w * 2) as i32, bpp, 0, 0);
        match bpp {
            32 => {
                for _ in 0..w * w {
                    v.extend_from_slice(&bgra);
                }
            }
            24 => {
                let stride = (w * 3).div_ceil(4) * 4;
                for _ in 0..w {
                    let mut row = Vec::new();
                    for _ in 0..w {
                        row.extend_from_slice(&bgra[..3]);
                    }
                    row.resize(stride as usize, 0);
                    v.extend_from_slice(&row);
                }
            }
            _ => unreachable!(),
        }
        let mask_stride = w.div_ceil(32) * 4;
        v.extend(std::iter::repeat_n(0u8, (mask_stride * w) as usize));
        v
    }

    fn png_entry(w: u32, rgba: [u8; 4]) -> Vec<u8> {
        let mut pm = tiny_skia::Pixmap::new(w, w).unwrap();
        pm.fill(tiny_skia::Color::from_rgba8(
            rgba[0], rgba[1], rgba[2], rgba[3],
        ));
        pm.encode_png().unwrap()
    }

    /// 拼 ICO：每项（目录声明边长, 位深, 条目数据）。
    fn ico(items: &[(u32, u16, Vec<u8>)]) -> Vec<u8> {
        let mut v = vec![0, 0, 1, 0];
        v.extend_from_slice(&(items.len() as u16).to_le_bytes());
        let mut offset = 6 + items.len() * 16;
        for (side, bpp, data) in items {
            let b = if *side >= 256 { 0 } else { *side as u8 };
            v.extend_from_slice(&[b, b, 0, 0]);
            v.extend_from_slice(&1u16.to_le_bytes());
            v.extend_from_slice(&bpp.to_le_bytes());
            v.extend_from_slice(&(data.len() as u32).to_le_bytes());
            v.extend_from_slice(&(offset as u32).to_le_bytes());
            offset += data.len();
        }
        for (_, _, data) in items {
            v.extend_from_slice(data);
        }
        v
    }

    fn first_px(img: &DecodedImage) -> [u8; 4] {
        img.rgba[..4].try_into().unwrap()
    }

    #[test]
    fn probe_checks_header_and_directory() {
        let f = ico(&[(16, 32, dib_entry(16, [0, 0, 255, 255], 32))]);
        assert!(IcoDecoder.probe(&f));
        assert!(!IcoDecoder.probe(&f[..10]));
        let mut cursor = f.clone();
        cursor[2] = 2; // 光标文件（CUR）
        assert!(!IcoDecoder.probe(&cursor));
    }

    #[test]
    fn default_picks_largest_png_entry() {
        let f = ico(&[
            (16, 32, dib_entry(16, [0, 0, 255, 255], 32)),
            (256, 32, png_entry(256, [0, 0, 255, 255])),
            (32, 32, dib_entry(32, [255, 0, 0, 255], 32)),
        ]);
        let img = IcoDecoder.decode(&f).unwrap();
        assert_eq!((img.width, img.height), (256, 256));
        assert_eq!(first_px(&img), [0, 0, 255, 255]);
    }

    #[test]
    fn target_picks_smallest_entry_not_below_target() {
        let f = ico(&[
            (16, 32, dib_entry(16, [0, 0, 255, 255], 32)), // 红
            (32, 32, dib_entry(32, [255, 0, 0, 255], 32)), // 蓝
            (48, 32, dib_entry(48, [0, 255, 0, 255], 32)), // 绿
        ]);
        let img = decode(&f, Some(20)).unwrap();
        assert_eq!((img.width, first_px(&img)), (32, [0, 0, 255, 255]));
        let img = decode(&f, Some(64)).unwrap(); // 都比目标小 → 最大
        assert_eq!(img.width, 48);
        let img = decode(&f, Some(16)).unwrap();
        assert_eq!((img.width, first_px(&img)), (16, [255, 0, 0, 255]));
    }

    #[test]
    fn same_size_prefers_higher_bit_depth() {
        let f = ico(&[
            (16, 24, dib_entry(16, [0, 255, 0, 255], 24)),
            (16, 32, dib_entry(16, [0, 0, 255, 255], 32)),
        ]);
        assert_eq!(first_px(&IcoDecoder.decode(&f).unwrap()), [255, 0, 0, 255]);
        assert_eq!(first_px(&decode(&f, Some(16)).unwrap()), [255, 0, 0, 255]);
    }

    #[test]
    fn and_mask_makes_24bit_pixels_transparent() {
        let mut data = dib_entry(2, [0, 0, 255, 255], 24);
        // AND 掩码两行各 4 字节，在末尾。把底行（文件里先存）首像素置 1。
        let n = data.len();
        data[n - 8] = 0b1000_0000;
        let img = IcoDecoder.decode(&ico(&[(2, 24, data)])).unwrap();
        // 底行在输出里是第 2 行（y=1）。
        assert_eq!(img.rgba[(2 * 4) + 3], 0, "底行首像素应透明");
        assert_eq!(img.rgba[3], 255, "顶行首像素应不透明");
    }

    /// 公共入口：`from_bytes` 经注册表识别 ICO 并取最大条目；`from_ico_bytes` 按目标挑。
    #[test]
    fn public_entry_points() {
        use crate::render::image::Image;
        let f = ico(&[
            (16, 32, dib_entry(16, [0, 0, 255, 255], 32)),
            (32, 32, dib_entry(32, [255, 0, 0, 255], 32)),
        ]);
        assert_eq!(Image::from_bytes(&f).unwrap().width(), 32);
        assert_eq!(Image::from_ico_bytes(&f, Some(16)).unwrap().width(), 16);
    }

    #[test]
    fn entry_outside_file_fails_cleanly() {
        let mut f = ico(&[(16, 32, dib_entry(16, [0, 0, 255, 255], 32))]);
        f.truncate(40);
        assert!(IcoDecoder.decode(&f).is_err());
    }
}
