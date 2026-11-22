//! 文件图标：`IShellItemImageFactory::GetImage`（只取图标，不取缩略图）。
//!
//! 与资源管理器同源：exe / dll 内嵌的图标、按扩展名关联的类型图标、文件夹图标都走这一个
//! 接口，Shell 负责挑最合适的尺寸并缩放。取到的是 32 位 HBITMAP（预乘 alpha），用
//! `GetDIBits` 读成自上而下的 BGRA，再经 [`to_straight_rgba`] 转成非预乘 RGBA。

use std::os::windows::ffi::OsStrExt;
use std::path::Path;

use windows::core::PCWSTR;
use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_ICONONLY,
};

use crate::platform::icon_pixels::{to_straight_rgba, Order};
use crate::render::image::Image;

/// 见 [`crate::platform::file_icon`]。
pub(crate) fn file_icon(path: impl AsRef<Path>, size: u32) -> Option<Image> {
    let path = path.as_ref();
    if !(1..=crate::platform::MAX_FILE_ICON_SIZE).contains(&size) || !path.exists() {
        return None;
    }
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: Shell 接口要求本线程已进入 COM 公寓。重复初始化返回 S_FALSE、已是别的公寓模型
    // 返回 RPC_E_CHANGED_MODE，两者都不妨碍下面的调用，故忽略结果；不配对 CoUninitialize，
    // 与 OLE 拖放那边一样让公寓活到线程结束。
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    // SAFETY: `wide` 以 NUL 结尾且在调用期间存活。
    let factory: IShellItemImageFactory =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None) }.ok()?;
    let side = size as i32;
    // SAFETY: COM 接口调用；成功时返回的 HBITMAP 归调用方所有，由下面 DeleteObject 释放。
    let hbm = unsafe { factory.GetImage(SIZE { cx: side, cy: side }, SIIGBF_ICONONLY) }.ok()?;
    let img = read_hbitmap(hbm);
    // SAFETY: hbm 是 GetImage 交出的、本函数独占的位图。
    unsafe {
        let _ = DeleteObject(HGDIOBJ(hbm.0));
    }
    img
}

/// HBITMAP → 非预乘 RGBA 的 `Image`。尺寸以位图本身为准（Shell 偶尔给的不是请求的尺寸）。
fn read_hbitmap(hbm: HBITMAP) -> Option<Image> {
    let mut bm = BITMAP::default();
    // SAFETY: bm 是有效的 BITMAP 缓冲，大小如实传入。
    let n = unsafe {
        GetObjectW(
            HGDIOBJ(hbm.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bm as *mut BITMAP as *mut _),
        )
    };
    if n == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    let mut info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            // 负高度 = 自上而下，省掉一次行翻转。
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut px = vec![0u8; w as usize * h as usize * 4];
    // SAFETY: 屏幕兼容 DC 仅用于 GetDIBits 的格式转换；px 按 32 位 × w × h 分配。
    let lines = unsafe {
        let dc = CreateCompatibleDC(None);
        let lines = GetDIBits(
            dc,
            hbm,
            0,
            h as u32,
            Some(px.as_mut_ptr() as *mut _),
            &mut info,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        lines
    };
    if lines != h {
        return None;
    }
    to_straight_rgba(&mut px, Order::Bgra);
    Image::from_rgba(w as u32, h as u32, &px).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn system_exe() -> std::path::PathBuf {
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        Path::new(&root).join("explorer.exe")
    }

    #[test]
    fn explorer_exe_icon_has_visible_pixels() {
        let img = file_icon(system_exe(), 32).expect("explorer.exe 应能取到图标");
        assert!(img.width() > 0 && img.height() > 0);
        let opaque = img
            .pixmap()
            .pixels()
            .iter()
            .filter(|p| p.alpha() > 0)
            .count();
        assert!(opaque > 0, "图标不应整张透明");
    }

    #[test]
    fn missing_file_and_bad_size_return_none() {
        assert!(file_icon("C:\\definitely\\not\\here.exe", 32).is_none());
        assert!(file_icon(system_exe(), 0).is_none());
        assert!(file_icon(system_exe(), 100_000).is_none());
    }
}
