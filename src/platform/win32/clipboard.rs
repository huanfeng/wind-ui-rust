//! Win32 剪贴板读写（CF_UNICODETEXT；图片：`"PNG"` 注册格式 + `CF_DIBV5` / `CF_DIB`）。

use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

use crate::core::ClipboardProvider;
use crate::platform::clip_image::{
    dib_to_rgba, png_to_rgba, rgba_to_dib, rgba_to_png, MAX_CLIP_BYTES,
};
use crate::render::DecodedImage;

const CF_UNICODETEXT: u32 = 13;
const CF_DIB: u32 = 8;
const CF_DIBV5: u32 = 17;

/// Win32 剪贴板实现，由 UiHost 注入 `Tree`。
pub struct WinClipboard;

impl ClipboardProvider for WinClipboard {
    fn get_text(&self) -> Option<String> {
        unsafe { get_text() }
    }
    fn set_text(&self, text: &str) {
        unsafe { set_text(text) };
    }
    fn get_image(&self) -> Option<DecodedImage> {
        unsafe { get_image() }
    }
    fn set_image(&self, img: &DecodedImage) -> bool {
        unsafe { set_image(img) }
    }
}

/// `"PNG"` 注册格式的 id（进程内恒定，注册失败返回 0）。
unsafe fn png_format() -> u32 {
    RegisterClipboardFormatW(w!("PNG"))
}

/// 读图片：优先 `"PNG"`（保留透明度），其次 `CF_DIBV5`，最后 `CF_DIB`。
/// 前一种格式存在但解码失败时继续试下一种。
unsafe fn get_image() -> Option<DecodedImage> {
    let png = png_format();
    let has_any = (png != 0 && IsClipboardFormatAvailable(png).is_ok())
        || IsClipboardFormatAvailable(CF_DIBV5).is_ok()
        || IsClipboardFormatAvailable(CF_DIB).is_ok();
    if !has_any {
        return None; // 剪贴板上没有图片：不必打开剪贴板
    }
    open_clipboard_retry().then_some(())?;
    let result = (|| {
        if png != 0 {
            if let Some(img) = read_format(png).and_then(|b| png_to_rgba(&b)) {
                return Some(img);
            }
        }
        for fmt in [CF_DIBV5, CF_DIB] {
            if let Some(img) = read_format(fmt).and_then(|b| dib_to_rgba(&b)) {
                return Some(img);
            }
        }
        None
    })();
    let _ = CloseClipboard();
    result
}

/// 打开剪贴板：被别的进程占用时重试 3 次、间隔约 10ms。
unsafe fn open_clipboard_retry() -> bool {
    for i in 0..3 {
        if OpenClipboard(None).is_ok() {
            return true;
        }
        if i < 2 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
    false
}

/// 复制出某格式的整块字节（调用方须已 `OpenClipboard`）。
/// 剪贴板是不可信的跨进程数据：长度取分配块大小，不信任内容自述。
unsafe fn read_format(fmt: u32) -> Option<Vec<u8>> {
    let h = GetClipboardData(fmt).ok()?;
    if h.0.is_null() {
        return None;
    }
    let hg = HGLOBAL(h.0);
    let size = GlobalSize(hg);
    if size > MAX_CLIP_BYTES {
        return None; // 超上限：不锁定、不复制
    }
    let ptr = GlobalLock(hg) as *const u8;
    if ptr.is_null() {
        return None;
    }
    let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
    let _ = GlobalUnlock(hg);
    Some(bytes)
}

/// 写图片：同时放 `"PNG"`（保留透明度）与 `CF_DIB`（兼容只认位图的程序）。
/// `SetClipboardData` 成功后内存归系统；其余路径（失败 / 未交出）一律 `GlobalFree`。
unsafe fn set_image(img: &DecodedImage) -> bool {
    let (Some(png), Some(dib)) = (rgba_to_png(img), rgba_to_dib(img)) else {
        return false;
    };
    let png_fmt = png_format();
    // 先备好两块内存再清剪贴板：分配失败时不丢用户原有内容。
    let Some(h_dib) = alloc_global(&dib) else {
        return false;
    };
    let Some(h_png) = alloc_global(&png) else {
        let _ = GlobalFree(Some(h_dib));
        return false;
    };
    if !open_clipboard_retry() {
        let _ = GlobalFree(Some(h_dib));
        let _ = GlobalFree(Some(h_png));
        return false;
    }
    let _ = EmptyClipboard();
    let ok_dib = SetClipboardData(CF_DIB, Some(HANDLE(h_dib.0))).is_ok();
    if !ok_dib {
        let _ = GlobalFree(Some(h_dib));
    }
    // png 格式注册失败（0）或写入失败时，h_png 仍归我们，须释放。
    if png_fmt == 0 || SetClipboardData(png_fmt, Some(HANDLE(h_png.0))).is_err() {
        let _ = GlobalFree(Some(h_png));
    }
    let _ = CloseClipboard();
    ok_dib
}

/// 分配并填充一块可移动内存；失败时自行释放，不泄漏。
unsafe fn alloc_global(bytes: &[u8]) -> Option<HGLOBAL> {
    let hmem = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
    let ptr = GlobalLock(hmem) as *mut u8;
    if ptr.is_null() {
        let _ = GlobalFree(Some(hmem));
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
    let _ = GlobalUnlock(hmem);
    Some(hmem)
}

unsafe fn get_text() -> Option<String> {
    let _: Option<HWND> = None;
    OpenClipboard(None).ok()?;
    let result = (|| {
        let h = GetClipboardData(CF_UNICODETEXT).ok()?;
        if h.0.is_null() {
            return None;
        }
        let hg = HGLOBAL(h.0);
        let ptr = GlobalLock(hg) as *const u16;
        if ptr.is_null() {
            return None;
        }
        // 剪贴板是不可信的跨进程数据：用分配块大小作上界，防止无 NUL 数据越界读。
        let cap = GlobalSize(hg) / 2; // u16 元素数
        let mut len = 0usize;
        while len < cap && *ptr.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(ptr, len);
        let s = String::from_utf16_lossy(slice);
        let _ = GlobalUnlock(hg);
        Some(s)
    })();
    let _ = CloseClipboard();
    result
}

unsafe fn set_text(text: &str) {
    if OpenClipboard(None).is_err() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * 2;
    if let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes) {
        let ptr = GlobalLock(hmem) as *mut u16;
        if !ptr.is_null() {
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(hmem);
            // 数据就绪后才清空（分配/锁定失败时不丢失用户原剪贴板内容）。
            let _ = EmptyClipboard();
            // 成功后系统接管 hmem 所有权。SetClipboardData 失败为极罕见路径，
            // 此时 hmem 未交系统、会泄漏一次；可忽略。
            let _ = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hmem.0)));
        }
    }
    let _ = CloseClipboard();
}
