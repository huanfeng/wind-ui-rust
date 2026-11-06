//! 剪贴板图片的格式转换（纯函数，不碰任何系统 API）。
//!
//! 剪贴板里图片有两类常见载体：PNG 字节流（Windows 的 `"PNG"` 注册格式、X11 的
//! `image/png` target）与 Windows 的 DIB（`CF_DIB` / `CF_DIBV5`）。这里只做
//! 「`DecodedImage`（非预乘 RGBA8）↔ 这两类字节」的转换，平台层负责与系统剪贴板交互。
//! PNG 走 tiny-skia 自带编解码（不新增依赖、不做其它格式解码）。

use crate::render::DecodedImage;

/// 允许解码的最大像素数。剪贴板是不可信的跨进程数据，宽高字段可被伪造成天文数字，
/// 先限住再分配（与 `lanmsg-media` 的 `MAX_DECODE_PIXELS` 同量级）。
const MAX_PIXELS: u64 = 64_000_000;

/// RGBA（非预乘）→ PNG 字节。尺寸为 0、像素长度不符或编码失败时返回 `None`。
#[cfg(any(windows, target_os = "linux", test))]
pub(crate) fn rgba_to_png(img: &DecodedImage) -> Option<Vec<u8>> {
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 || img.rgba.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let mut pm = tiny_skia::Pixmap::new(w, h)?;
    // tiny-skia 的像素是预乘的；`encode_png` 内部会再反预乘，往返无损（不透明像素恒等）。
    for (dst, px) in pm.pixels_mut().iter_mut().zip(img.rgba.as_chunks::<4>().0) {
        let c = tiny_skia::ColorU8::from_rgba(px[0], px[1], px[2], px[3]);
        *dst = c.premultiply();
    }
    pm.encode_png().ok()
}

/// PNG 字节 → RGBA（非预乘）。解码失败或像素数超限返回 `None`。
#[cfg(any(windows, target_os = "linux", test))]
pub(crate) fn png_to_rgba(bytes: &[u8]) -> Option<DecodedImage> {
    // PNG 头 IHDR：8 字节签名 + 4 长度 + 4 类型，随后是大端宽、高。解码前先验尺寸。
    if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
    if u64::from(w) * u64::from(h) > MAX_PIXELS {
        return None;
    }
    let pm = tiny_skia::Pixmap::decode_png(bytes).ok()?;
    let mut rgba = Vec::with_capacity((pm.width() as usize) * (pm.height() as usize) * 4);
    for p in pm.pixels() {
        let c = p.demultiply();
        rgba.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Some(DecodedImage {
        width: pm.width(),
        height: pm.height(),
        rgba,
    })
}

/// RGBA → `CF_DIB` 载荷：`BITMAPINFOHEADER`（40 字节）+ 32bpp、BI_RGB、自底向上的 BGRA 像素。
/// alpha 原样写入（不预乘）；不认 alpha 的程序会忽略它。
#[cfg(any(windows, test))]
pub(crate) fn rgba_to_dib(img: &DecodedImage) -> Option<Vec<u8>> {
    let (w, h) = (img.width as usize, img.height as usize);
    if w == 0 || h == 0 || img.rgba.len() != w * h * 4 {
        return None;
    }
    let mut out = Vec::with_capacity(40 + w * h * 4);
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(img.width as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(img.height as i32).to_le_bytes()); // biHeight（正 = 自底向上）
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&((w * h * 4) as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&[0u8; 16]); // 分辨率 ×2、biClrUsed、biClrImportant
    for row in img.rgba.chunks_exact(w * 4).rev() {
        for px in row.as_chunks::<4>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
        }
    }
    Some(out)
}

/// `CF_DIB` / `CF_DIBV5` 载荷 → RGBA。支持 24/32bpp 的 BI_RGB / BI_BITFIELDS（按标准 BGRA
/// 掩码处理）、自底向上与自顶向下；其它（调色板、压缩）返回 `None`。
/// 32bpp 的 BI_RGB 里 alpha 字节常是未定义的 0：整幅 alpha 全 0 时视为不透明。
#[cfg(any(windows, test))]
pub(crate) fn dib_to_rgba(data: &[u8]) -> Option<DecodedImage> {
    let u32_at = |o: usize| -> Option<u32> {
        Some(u32::from_le_bytes(data.get(o..o + 4)?.try_into().ok()?))
    };
    let u16_at = |o: usize| -> Option<u16> {
        Some(u16::from_le_bytes(data.get(o..o + 2)?.try_into().ok()?))
    };
    let header = u32_at(0)? as usize;
    if header < 40 {
        return None;
    }
    let width = u32_at(4)? as i32;
    let height = u32_at(8)? as i32;
    let bpp = u16_at(14)?;
    let compression = u32_at(16)?;
    let clr_used = u32_at(32)? as usize;
    if width <= 0
        || height == 0
        || !(bpp == 24 || bpp == 32)
        || !(compression == 0 || compression == 3)
    {
        return None;
    }
    let (w, h) = (width as usize, height.unsigned_abs() as usize);
    if (w as u64) * (h as u64) > MAX_PIXELS {
        return None;
    }
    // 像素偏移：头 + （旧式 40 字节头的 BI_BITFIELDS 后面跟 12 字节掩码）+ 调色板。
    let mut off = header + clr_used.min(256) * 4;
    if compression == 3 && header == 40 {
        off += 12;
    }
    let bytes_pp = usize::from(bpp / 8);
    let stride = (w * bytes_pp).div_ceil(4) * 4;
    let body = data.get(off..)?;
    if body.len() < stride * h {
        return None;
    }
    let mut rgba = vec![0u8; w * h * 4];
    let bottom_up = height > 0;
    for y in 0..h {
        let src_y = if bottom_up { h - 1 - y } else { y };
        let row = &body[src_y * stride..src_y * stride + w * bytes_pp];
        let dst = &mut rgba[y * w * 4..(y + 1) * w * 4];
        for (d, s) in dst
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(row.chunks_exact(bytes_pp))
        {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = if bytes_pp == 4 { s[3] } else { 255 };
        }
    }
    if bytes_pp == 4 && rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 0) {
        rgba.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .for_each(|p| p[3] = 255);
    }
    Some(DecodedImage {
        width: w as u32,
        height: h as u32,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> DecodedImage {
        // 2×2：红(不透明) 绿(不透明) / 蓝(不透明) 半透明白
        DecodedImage {
            width: 2,
            height: 2,
            rgba: vec![
                255, 0, 0, 255, 0, 255, 0, 255, //
                0, 0, 255, 255, 255, 255, 255, 128,
            ],
        }
    }

    #[test]
    fn png_clipboard_roundtrip_linux_codec() {
        let img = sample();
        let png = rgba_to_png(&img).expect("编码");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let back = png_to_rgba(&png).expect("解码");
        assert_eq!((back.width, back.height), (2, 2));
        // 不透明像素逐字节恒等
        assert_eq!(&back.rgba[..12], &img.rgba[..12]);
        // 半透明像素预乘往返允许 ±1 误差，alpha 必须保持
        assert_eq!(back.rgba[15], 128);
        for i in 12..15 {
            assert!(
                back.rgba[i].abs_diff(255) <= 1,
                "通道 {i} = {}",
                back.rgba[i]
            );
        }
    }

    #[test]
    fn png_rejects_garbage_and_oversized_header() {
        assert!(png_to_rgba(b"not a png").is_none());
        // 伪造 IHDR：宽高各 20000 → 4 亿像素，应在解码前拒绝
        let mut fake = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13];
        fake.extend_from_slice(b"IHDR");
        fake.extend_from_slice(&20000u32.to_be_bytes());
        fake.extend_from_slice(&20000u32.to_be_bytes());
        fake.extend_from_slice(&[8, 6, 0, 0, 0]);
        assert!(png_to_rgba(&fake).is_none());
    }

    #[test]
    fn dib_roundtrip_is_bottom_up_bgra() {
        let img = sample();
        let dib = rgba_to_dib(&img).expect("编码");
        assert_eq!(dib.len(), 40 + 16);
        // 自底向上：第一行像素是原图最后一行（蓝），BGRA 序 → [255,0,0,255]
        assert_eq!(&dib[40..44], &[255, 0, 0, 255]);
        let back = dib_to_rgba(&dib).expect("解码");
        assert_eq!(back.rgba, img.rgba);
    }

    #[test]
    fn dib_zero_alpha_means_opaque() {
        let mut dib = rgba_to_dib(&DecodedImage {
            width: 1,
            height: 1,
            rgba: vec![10, 20, 30, 0],
        })
        .unwrap();
        dib[40 + 3] = 0;
        assert_eq!(dib_to_rgba(&dib).unwrap().rgba, vec![10, 20, 30, 255]);
    }

    #[test]
    fn dib_truncated_or_bogus_is_none() {
        assert!(dib_to_rgba(&[0u8; 10]).is_none());
        let mut dib = rgba_to_dib(&sample()).unwrap();
        dib.truncate(45);
        assert!(dib_to_rgba(&dib).is_none());
    }
}
