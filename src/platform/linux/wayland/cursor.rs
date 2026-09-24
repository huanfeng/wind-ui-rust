//! 指针光标。两条路：
//!
//! - **`cursor-shape-v1`**（sway 1.9+、KDE 6、GNOME 46+）：只报形状名，由合成器按自己的
//!   主题与缩放画，客户端什么都不用加载。
//! - **回退**（GNOME 42 等没有它的合成器）：`wayland-cursor` 读 XCursor 主题文件，自己挂一个
//!   光标表面。主题名 / 尺寸取 `XCURSOR_THEME` / `XCURSOR_SIZE`（默认 `default` / 24），尺寸
//!   乘以窗口缩放（向上取整）后加载，光标表面 `set_buffer_scale` 同一个整数，HiDPI 下不发糊。
//!   主题懒加载、缩放变了才重载；找不到光标图时什么都不挂（记一次日志），不把光标隐藏掉。

use wayland_client::protocol::{wl_pointer, wl_shm, wl_surface};
use wayland_client::Connection;
use wayland_cursor::CursorTheme;
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    self, WpCursorShapeDeviceV1,
};

use crate::event::CursorShape;

/// 回退路径下每种形状依次尝试的 XCursor 名（新式 CSS 名在前，老式 X 名兜底）。
fn theme_names(s: CursorShape) -> &'static [&'static str] {
    match s {
        CursorShape::Arrow => &["default", "left_ptr"],
        CursorShape::Hand => &["pointer", "hand2", "hand1"],
        CursorShape::Text => &["text", "xterm"],
        CursorShape::SizeWE => &["ew-resize", "sb_h_double_arrow", "h_double_arrow"],
        CursorShape::SizeNS => &["ns-resize", "sb_v_double_arrow", "v_double_arrow"],
    }
}

fn protocol_shape(s: CursorShape) -> wp_cursor_shape_device_v1::Shape {
    use wp_cursor_shape_device_v1::Shape;
    match s {
        CursorShape::Arrow => Shape::Default,
        CursorShape::Hand => Shape::Pointer,
        CursorShape::Text => Shape::Text,
        CursorShape::SizeWE => Shape::EwResize,
        CursorShape::SizeNS => Shape::NsResize,
    }
}

/// 光标表面实际用的 buffer_scale：不超过 `want`、且能同时整除图像宽高的最大整数（最差 1）。
///
/// 主题按「名义尺寸 × 缩放」挑最近的一档图，挑到的未必是缩放的整数倍（DMZ 只有 24/32/48/64/96，
/// 3 倍缩放请求 72 会拿到 64）；buffer_scale 不整除缓冲尺寸是协议错误，合成器直接断开连接。
fn divisible_scale(want: i32, (w, h): (u32, u32)) -> i32 {
    (1..=want.max(1))
        .rev()
        .find(|s| w % *s as u32 == 0 && h % *s as u32 == 0)
        .unwrap_or(1)
}

/// 一个 `wl_pointer` 的光标状态。
pub(super) struct Cursor {
    shape_device: Option<WpCursorShapeDeviceV1>,
    theme: Option<(CursorTheme, i32)>,
    surface: Option<wl_surface::WlSurface>,
    /// 上次挂上去的（enter serial，形状，缩放）：相同就不重发。
    applied: Option<(u32, CursorShape, i32)>,
    warned: bool,
}

impl Cursor {
    pub fn new(shape_device: Option<WpCursorShapeDeviceV1>) -> Self {
        Self {
            shape_device,
            theme: None,
            surface: None,
            applied: None,
            warned: false,
        }
    }

    /// 在 `serial`（最近一次 `wl_pointer.enter` 的序号）下把光标设成 `shape`。
    /// `surface_factory` 只在回退路径第一次需要光标表面时调用。
    #[allow(clippy::too_many_arguments)]
    pub fn apply(
        &mut self,
        pointer: &wl_pointer::WlPointer,
        serial: u32,
        shape: CursorShape,
        scale: f64,
        conn: &Connection,
        shm: &wl_shm::WlShm,
        surface_factory: impl FnOnce() -> wl_surface::WlSurface,
    ) {
        let int_scale = (scale.ceil() as i32).max(1);
        if self.applied == Some((serial, shape, int_scale)) {
            return;
        }
        self.applied = Some((serial, shape, int_scale));
        if let Some(dev) = &self.shape_device {
            dev.set_shape(serial, protocol_shape(shape));
            return;
        }
        if self.theme.as_ref().is_none_or(|(_, s)| *s != int_scale) {
            let name = std::env::var("XCURSOR_THEME")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "default".into());
            let base: u32 = std::env::var("XCURSOR_SIZE")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|v| *v > 0)
                .unwrap_or(24);
            match CursorTheme::load_from_name(conn, shm.clone(), &name, base * int_scale as u32) {
                Ok(t) => self.theme = Some((t, int_scale)),
                Err(e) => {
                    log::warn!("加载光标主题 {name} 失败：{e}");
                    return;
                }
            }
        }
        let Some((theme, _)) = self.theme.as_mut() else {
            return;
        };
        // get_cursor 按名加载并缓存，返回借用；先找出能加载的名字，再借一次。
        let Some(name) = theme_names(shape)
            .iter()
            .find(|n| theme.get_cursor(n).is_some())
        else {
            if !std::mem::replace(&mut self.warned, true) {
                log::warn!("光标主题里找不到 {shape:?} 对应的光标图，保留合成器当前光标");
            }
            return;
        };
        let Some(cursor) = theme.get_cursor(name) else {
            return;
        };
        let img = &cursor[0];
        let (hx, hy) = img.hotspot();
        let (iw, ih) = img.dimensions();
        let bs = divisible_scale(int_scale, (iw, ih));
        let surface = self.surface.get_or_insert_with(surface_factory);
        surface.set_buffer_scale(bs);
        surface.attach(Some(img), 0, 0);
        surface.damage_buffer(0, 0, iw as i32, ih as i32);
        surface.commit();
        pointer.set_cursor(serial, Some(surface), hx as i32 / bs, hy as i32 / bs);
    }

    /// 指针离开 / 进入新表面后必须按新 serial 重设。
    pub fn invalidate(&mut self) {
        self.applied = None;
    }

    pub fn destroy(self) {
        if let Some(d) = self.shape_device {
            d.destroy();
        }
        if let Some(s) = self.surface {
            s.destroy();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_buffer_scale_always_divides_the_image() {
        assert_eq!(divisible_scale(2, (48, 48)), 2);
        assert_eq!(
            divisible_scale(3, (64, 64)),
            2,
            "3 倍拿到 64 像素的图：退到 2"
        );
        assert_eq!(divisible_scale(3, (72, 72)), 3);
        assert_eq!(divisible_scale(4, (30, 30)), 3);
        assert_eq!(divisible_scale(2, (25, 24)), 1);
        assert_eq!(divisible_scale(0, (24, 24)), 1);
    }

    #[test]
    fn every_shape_has_css_name_first_and_legacy_fallbacks() {
        for s in [
            CursorShape::Arrow,
            CursorShape::Hand,
            CursorShape::Text,
            CursorShape::SizeWE,
            CursorShape::SizeNS,
        ] {
            assert!(theme_names(s).len() >= 2, "{s:?} 至少一个老式名兜底");
        }
        assert_eq!(
            protocol_shape(CursorShape::Text),
            wp_cursor_shape_device_v1::Shape::Text
        );
    }
}
