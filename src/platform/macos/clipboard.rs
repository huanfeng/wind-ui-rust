//! macOS 剪贴板（`NSPasteboard`）——**缝合骨架**。
//!
//! 当前为优雅降级占位：读返回 `None`、写为空操作（不 panic，便于早期联调）。
//! 实现指引：`NSPasteboard::generalPasteboard()`，读 `stringForType(NSPasteboardTypeString)`，
//! 写 `clearContents()` + `setString_forType(...)`。

use crate::core::ClipboardProvider;

/// macOS 剪贴板实现，由 `UiHost` 注入 `Tree`。
pub struct MacClipboard;

impl ClipboardProvider for MacClipboard {
    fn get_text(&self) -> Option<String> {
        // TODO(macos): NSPasteboard::generalPasteboard().stringForType(NSPasteboardTypeString)。
        None
    }
    fn set_text(&self, text: &str) {
        // TODO(macos): clearContents() + setString_forType(text, NSPasteboardTypeString)。
        let _ = text;
    }
}
