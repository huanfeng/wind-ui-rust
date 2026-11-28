//! Win32 全屏：换成 `WS_POPUP` 铺满所在显示器，再切一次还原样式与摆放。
//!
//! 保存的是进入前的 `GWL_STYLE` 与 `WINDOWPLACEMENT`（后者一并记住「此前是否最大化」
//! 与还原矩形），故退出后回到进入前的状态，而不是一律变成普通窗口。
//!
//! 登记表按 HWND 存，放线程局部而非 `WindowState`：`read_window_state` 要在任何时机
//! 同步读「是否全屏」（见 `query_main_window_state`），而 `WindowState` 的借用在分发期间
//! 可能被占着。**表只在函数内短借用，从不跨 OS 调用持有**——`SetWindowPos` 等会同步重入
//! `wnd_proc`，重入里又要读本表（`WM_SIZE` 推状态、`WM_NCCALCSIZE` 判全屏）。

use std::cell::RefCell;
use std::collections::HashMap;

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowLongPtrW, GetWindowPlacement, IsZoomed, SetWindowLongPtrW, SetWindowPlacement,
    SetWindowPos, ShowWindow, GWL_STYLE, HWND_TOP, SWP_FRAMECHANGED, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SW_RESTORE, SW_SHOWMINIMIZED, SW_SHOWNORMAL, WINDOWPLACEMENT, WS_MAXIMIZE,
    WS_MINIMIZE, WS_OVERLAPPEDWINDOW, WS_POPUP,
};

#[derive(Clone, Copy)]
struct Saved {
    style: isize,
    placement: WINDOWPLACEMENT,
}

thread_local! {
    static SAVED: RefCell<HashMap<isize, Saved>> = RefCell::new(HashMap::new());
}

fn key(hwnd: HWND) -> isize {
    hwnd.0 as isize
}

/// 窗口当前是否处于由本模块进入的全屏。
pub(super) fn is_fullscreen(hwnd: HWND) -> bool {
    SAVED.with(|m| m.borrow().contains_key(&key(hwnd)))
}

/// 窗口销毁时清掉登记，避免 HWND 被复用后读到旧状态。
pub(super) fn forget(hwnd: HWND) {
    SAVED.with(|m| {
        m.borrow_mut().remove(&key(hwnd));
    });
}

/// 全屏 / 退出全屏切换。**调用方须已释放 `WindowState` 借用**（OS 调用会重入 `wnd_proc`）。
pub(super) unsafe fn toggle(hwnd: HWND) {
    if is_fullscreen(hwnd) {
        exit(hwnd);
    } else {
        enter(hwnd);
    }
}

/// 退出全屏（未全屏则无操作）：`WindowOp::Restore` 用它。
pub(super) unsafe fn exit_if_fullscreen(hwnd: HWND) -> bool {
    if !is_fullscreen(hwnd) {
        return false;
    }
    exit(hwnd);
    true
}

unsafe fn enter(hwnd: HWND) {
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return;
    }
    // 最大化 / 最小化位属于「当前状态」而非样式，由 placement 的 showCmd 负责还原。
    let style = GetWindowLongPtrW(hwnd, GWL_STYLE) & !((WS_MAXIMIZE.0 | WS_MINIMIZE.0) as isize);
    let mut placement = WINDOWPLACEMENT {
        length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    if GetWindowPlacement(hwnd, &mut placement).is_err() {
        return;
    }
    // 最小化中进入全屏没有意义：退出时回到普通态，而不是又缩回任务栏。
    if placement.showCmd == SW_SHOWMINIMIZED.0 as u32 {
        placement.showCmd = SW_SHOWNORMAL.0 as u32;
    }
    // 先登记再动窗口：下面的 OS 调用会同步重入 `WM_SIZE` / `WM_NCCALCSIZE`，它们要读到
    // 「已全屏」。
    SAVED.with(|m| {
        m.borrow_mut().insert(key(hwnd), Saved { style, placement });
    });
    // 最大化着直接换样式，窗口仍带最大化位，系统按最大化矩形（含边框外溢）摆放。
    // 先还原，再铺满显示器。
    if IsZoomed(hwnd).as_bool() {
        let _ = ShowWindow(hwnd, SW_RESTORE);
    }
    // 无边框窗口也保留着 `WS_OVERLAPPEDWINDOW` 位（非客户区靠 `WM_NCCALCSIZE` 消掉），
    // 同样走这条剥除。
    let fullscreen_style = (GetWindowLongPtrW(hwnd, GWL_STYLE) & !(WS_OVERLAPPEDWINDOW.0 as isize))
        | WS_POPUP.0 as isize;
    SetWindowLongPtrW(hwnd, GWL_STYLE, fullscreen_style);
    let r = info.rcMonitor;
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOP),
        r.left,
        r.top,
        r.right - r.left,
        r.bottom - r.top,
        SWP_FRAMECHANGED,
    );
}

unsafe fn exit(hwnd: HWND) {
    let Some(saved) = SAVED.with(|m| m.borrow_mut().remove(&key(hwnd))) else {
        return;
    };
    SetWindowLongPtrW(hwnd, GWL_STYLE, saved.style);
    // 样式变了须通知系统重算非客户区；`SetWindowPlacement` 随后按保存的 showCmd
    // 还原（最大化过的重新最大化，否则回到还原矩形）。
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        0,
        0,
        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_FRAMECHANGED,
    );
    let _ = SetWindowPlacement(hwnd, &saved.placement);
}
