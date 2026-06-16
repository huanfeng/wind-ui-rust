//! 平台抽象层。目前仅 Windows（`win32`）。
//!
//! 模块名用 `win32` 而非 `windows`，以免与外部 `windows` crate 冲突。

pub mod win32;

pub use win32::{run, WindowConfig};

use tiny_skia::Pixmap;

use crate::event::{KeyEvent, PointerEvent};
use crate::geometry::{Point, Size};

/// 平台驱动的应用逻辑：渲染一帧 + 处理输入。返回 true 表示需要重绘。
pub trait AppHandler {
    fn render(&mut self, pixmap: &mut Pixmap, size: Size);
    fn on_pointer(&mut self, _ev: PointerEvent) -> bool {
        false
    }
    fn on_key(&mut self, _ev: KeyEvent) -> bool {
        false
    }
    /// 是否请求关闭窗口（事件处理后由平台查询）。
    fn wants_close(&self) -> bool {
        false
    }
    /// 当前是否处于指针捕获态。平台据此调用 OS 的 SetCapture/ReleaseCapture，
    /// 保证拖出窗口时仍能收到移动/抬起消息。
    fn capture_active(&self) -> bool {
        false
    }
    /// OS 抢走指针捕获（Alt+Tab 等）时调用，让逻辑捕获方收尾（如复位拖动态）。
    /// 返回 true 表示需要重绘。
    fn on_capture_lost(&mut self) -> bool {
        false
    }
    /// 设置 DPI 缩放因子（DPI/96）。窗口创建后与 WM_DPICHANGED 时由平台调用。
    fn set_scale(&mut self, _scale: f32) {}

    /// 焦点文本控件的光标位置（**物理像素**，相对客户区左上角）+ 高度：`(x, y_top, height)`。
    /// 平台层据此定位输入法候选窗。无文本焦点时返回 None。
    fn ime_caret(&self) -> Option<(i32, i32, i32)> {
        None
    }

    /// 本帧是否有控件请求持续动画。平台层据此在阻塞空闲与按帧驱动之间切换。
    fn wants_animation(&self) -> bool {
        false
    }

    /// 触摸平移手势：在 `pos`（**物理像素**，相对客户区）按 `dy` 物理像素平移，
    /// 滚动手指下的容器。返回 true 表示需要重绘。
    fn on_pan(&mut self, _pos: Point, _dy: i32) -> bool {
        false
    }
}
