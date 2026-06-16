//! 动画驱动的可移植抽象。
//!
//! 控件在 paint 中调用 [`request_repaint`] 表示"我在动画中，需要下一帧"；宿主每帧
//! 绘制前 [`reset_request`]，绘制后读 [`animation_requested`] 决定是否继续驱动帧。
//! 平台层据此选择唤醒机制（Win32 用带超时的消息等待，macOS 可用 CADisplayLink），
//! 无动画时回到阻塞空闲（零 CPU）。控件用 [`clock_ms`] 取当前帧单调时钟算动画相位。

use std::cell::Cell;

thread_local! {
    static REQUEST: Cell<bool> = const { Cell::new(false) };
    static CLOCK_MS: Cell<u64> = const { Cell::new(0) };
}

/// 控件请求持续动画（在 paint 内调用）。宿主据此驱动下一帧。
pub fn request_repaint() {
    REQUEST.with(|c| c.set(true));
}

/// 当前帧单调时钟（毫秒）。控件据此计算动画相位（与挂钟无关，仅用差值）。
pub fn clock_ms() -> u64 {
    CLOCK_MS.with(|c| c.get())
}

/// 宿主：每帧绘制前清除动画请求。
pub(crate) fn reset_request() {
    REQUEST.with(|c| c.set(false));
}

/// 宿主/平台：本帧是否有控件请求了动画。
pub(crate) fn animation_requested() -> bool {
    REQUEST.with(|c| c.get())
}

/// 宿主：设置当前帧时钟。
pub(crate) fn set_clock_ms(v: u64) {
    CLOCK_MS.with(|c| c.set(v));
}
