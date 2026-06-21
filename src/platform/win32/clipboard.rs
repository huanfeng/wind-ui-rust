//! Win32 剪贴板读写（CF_UNICODETEXT）。

use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};

use crate::core::ClipboardProvider;

const CF_UNICODETEXT: u32 = 13;

/// Win32 剪贴板实现，由 UiHost 注入 `Tree`。
pub struct WinClipboard;

impl ClipboardProvider for WinClipboard {
    fn get_text(&self) -> Option<String> {
        unsafe { get_text() }
    }
    fn set_text(&self, text: &str) {
        unsafe { set_text(text) };
    }
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
