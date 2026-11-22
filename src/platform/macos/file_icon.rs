//! 文件图标：`NSWorkspace::iconForFile`。
//!
//! 返回的 `NSImage` 带多个尺寸的表示（16 到 1024）。用 `CGImageForProposedRect` 让 AppKit
//! 按目标尺寸挑最合适的那个，再画进 `size × size` 的 RGBA 位图上下文（预乘），最后经
//! [`to_straight_rgba`] 转成非预乘 RGBA。

use std::ffi::c_void;
use std::path::Path;

use objc2_app_kit::NSWorkspace;
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImageAlphaInfo};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::platform::icon_pixels::{to_straight_rgba, Order};
use crate::render::image::Image;

/// 见 [`crate::platform::file_icon`]。
pub(crate) fn file_icon(path: impl AsRef<Path>, size: u32) -> Option<Image> {
    let path = path.as_ref();
    // iconForFile 对不存在的路径也会给一个通用文档图标；与 Windows 统一为 None。
    if !(1..=crate::platform::MAX_FILE_ICON_SIZE).contains(&size) || !path.exists() {
        return None;
    }
    let ns_path = NSString::from_str(path.to_str()?);
    let icon = NSWorkspace::sharedWorkspace().iconForFile(&ns_path);
    let side = f64::from(size);
    let mut rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side));
    // SAFETY: rect 是有效的栈上 NSRect；不传参考上下文与提示。
    let cg = unsafe { icon.CGImageForProposedRect_context_hints(&mut rect, None, None) }?;

    let n = size as usize;
    let mut px = vec![0u8; n * n * 4];
    let cs = CGColorSpace::new_device_rgb()?;
    {
        // SAFETY: px 按 n×n×4 分配，行跨度 n*4，在上下文存活期间不移动、不释放。
        let ctx = unsafe {
            CGBitmapContextCreate(
                px.as_mut_ptr() as *mut c_void,
                n,
                n,
                8,
                n * 4,
                Some(&cs),
                CGImageAlphaInfo::PremultipliedLast.0,
            )
        }?;
        CGContext::draw_image(
            Some(&ctx),
            NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(side, side)),
            Some(&cg),
        );
    }
    to_straight_rgba(&mut px, Order::Rgba);
    Image::from_rgba(size, size, &px).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FINDER: &str = "/System/Library/CoreServices/Finder.app";

    #[test]
    fn finder_icon_has_visible_pixels() {
        let img = file_icon(FINDER, 64).expect("Finder.app 应能取到图标");
        assert_eq!((img.width(), img.height()), (64, 64));
        let opaque = img
            .pixmap()
            .pixels()
            .iter()
            .filter(|p| p.alpha() > 0)
            .count();
        assert!(
            opaque > 64,
            "图标不应几乎全透明（实得 {opaque} 个不透明像素）"
        );
    }

    #[test]
    fn missing_file_and_bad_size_return_none() {
        assert!(file_icon("/definitely/not/here.txt", 32).is_none());
        assert!(file_icon(FINDER, 0).is_none());
        assert!(file_icon(FINDER, 100_000).is_none());
    }
}
