//! 右下角悬浮窗：隐藏创建 → `on_ready` 定位到工作区右下角 → 显示。
//!
//! 运行：`cargo run --release --example corner`
//! 截屏：`cargo run --example corner -- --screenshot artifacts/corner.png`
//!
//! - `.start_hidden()` 让窗口创建后**保持不可见**，避免"先默认显示再跳位"的闪现。
//! - `.on_ready(position_and_show)` 在窗口创建完成、**首次显示前**回调：此时窗口尚在
//!   暗处，直接把它移到主屏工作区右下角（留 `MARGIN` 边距）再 `ShowWindow`，因此窗口
//!   首次现身就在目标位置，没有"先居中再跳位"。
//! - `.topmost()` 使窗口始终浮于其他窗口之上；`.frameless()` + 自定义标题栏可拖动到
//!   屏幕任意角落；`resizable(false)` + `min_size` 固定窗口尺寸。
//!
//! 平台现状：右下角定位与显示基于 Win32 API（工作区 = 任务栏之外的可用区域），
//! **仅 Windows 完整生效**。macOS 上 `on_ready` 回调为空实现（窗口将保持隐藏，
//! 按 Ctrl+C 结束进程）。

use windui::prelude::*;

const WINDOW_TITLE: &str = "windui — 右下角悬浮窗";
const WINDOW_WIDTH: i32 = 360;
const WINDOW_HEIGHT: i32 = 220;
/// 距屏幕工作区右/下边缘的留白（逻辑像素，按窗口 DPI 缩放）。
const MARGIN: i32 = 24;

/// 窗口创建完成、首次显示前：定位到主屏工作区右下角再显示。
///
/// 参数为平台句柄数值（win32=HWND、macOS=NSWindow 指针）。
fn position_and_show(hwnd: isize) {
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{HWND, RECT};
        use windows::Win32::UI::HiDpi::GetDpiForWindow;
        use windows::Win32::UI::WindowsAndMessaging::{
            GetWindowRect, SetWindowPos, ShowWindow, SystemParametersInfoW, SW_SHOW,
            SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SPI_GETWORKAREA,
        };

        let hwnd = HWND(hwnd as *mut core::ffi::c_void);
        // 工作区 = 任务栏之外的可用区域（物理像素）。
        let mut work = RECT::default();
        let _ = unsafe {
            SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut work as *mut _ as *mut core::ffi::c_void),
                windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        // 以实际窗口矩形（物理像素，已含 DPI 缩放与外框）而非逻辑常量计算落点，
        // 高分屏 / 多显示器下依然准确。
        let mut rc = RECT::default();
        let _ = unsafe { GetWindowRect(hwnd, &mut rc) };
        let win_w = rc.right - rc.left;
        let win_h = rc.bottom - rc.top;
        // 留白按窗口 DPI 缩放，高分屏下保持一致的视觉边距。
        let dpi = unsafe { GetDpiForWindow(hwnd) };
        let scale = if dpi == 0 { 1.0 } else { dpi as f32 / 96.0 };
        let margin = (MARGIN as f32 * scale).round() as i32;
        let x = work.right - win_w - margin;
        let y = work.bottom - win_h - margin;
        // 只移动：不改尺寸与 z 序（置顶由 `.topmost()` 在建窗前完成），不抢焦点。
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                None,
                x,
                y,
                0,
                0,
                SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOSIZE,
            )
        };
        let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    }
    #[cfg(target_os = "macos")]
    {
        // macOS 的 NSWindow 指针定位/显示尚未在示例中实现；窗口保持隐藏，Ctrl+C 结束。
        let _ = hwnd;
    }
}

fn main() {
    // 自定义标题栏：整条可拖（window_drag），方便把悬浮窗拖到任意角落。
    let title_bar = Element::row()
        .width_match()
        .height(32)
        .cross(Align::Center)
        .bg_role(Role::Accent)
        .window_drag()
        .child(
            Element::label("  windui — 右下角悬浮窗")
                .font_size(13.0)
                .fg_role(Role::OnAccent)
                .weight(1.0),
        );

    let body = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .child(
            Element::label("已置顶 · 屏幕工作区右下角")
                .font_size(16.0)
                .fg_role(Role::Text)
                .width_match(),
        )
        .child(
            Element::label("拖动标题栏可移动，窗口始终浮于其他窗口之上。")
                .font_size(13.0)
                .fg_role(Role::TextMuted)
                .width_match(),
        )
        .child(Element::flex_spacer())
        .child(
            Element::row()
                .width_match()
                .child(Element::flex_spacer())
                .child(
                    Element::button("退出")
                        .neutral()
                        .on_click(|ctx| ctx.request_close())
                        .width(88),
                ),
        );

    let ui = Element::col().fill().child(title_bar).child(body);

    // 隐藏创建 → on_ready 定位到工作区右下角 → 显示（无"先居中再跳位"闪现）。
    App::new(WINDOW_TITLE, WINDOW_WIDTH, WINDOW_HEIGHT)
        .frameless()
        .bg(Color::hex(0xFFFFFF))
        .resizable(false)
        .min_size(WINDOW_WIDTH, WINDOW_HEIGHT)
        .topmost()
        .start_hidden()
        .on_ready(position_and_show)
        .screenshot_from_args()
        .content(ui)
        .run();
}
