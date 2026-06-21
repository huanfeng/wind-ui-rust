//! macOS 剪贴板（`NSPasteboard`）。读 `stringForType(NSPasteboardTypeString)`；
//! 写 `clearContents()` + `setString_forType(...)`。对照 `win32/clipboard.rs`（`CF_UNICODETEXT`）。

use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

use crate::core::ClipboardProvider;

/// macOS 剪贴板实现，由 `UiHost` 注入 `Tree`。
pub struct MacClipboard;

impl ClipboardProvider for MacClipboard {
    fn get_text(&self) -> Option<String> {
        let pb = NSPasteboard::generalPasteboard();
        // NSPasteboardTypeString 是 extern static（CFString 常量），取用需 unsafe。
        let ty = unsafe { NSPasteboardTypeString };
        pb.stringForType(ty).map(|s| s.to_string())
    }
    fn set_text(&self, text: &str) {
        let pb = NSPasteboard::generalPasteboard();
        pb.clearContents();
        let ty = unsafe { NSPasteboardTypeString };
        pb.setString_forType(&NSString::from_str(text), ty);
    }
}
