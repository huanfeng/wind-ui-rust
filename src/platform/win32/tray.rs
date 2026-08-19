//! 系统托盘图标（Shell_NotifyIcon）：图标 + 提示 + 左键/双击回调 + 原生右键菜单。
//!
//! 右键菜单走原生 `TrackPopupMenu`（真 OS 弹出，显示在托盘旁，窗口外），支持
//! 勾选项（`checked` 绑定 `Signal<bool>`，菜单弹出时按当前值显示对勾）与分隔线。
//! 气泡通知经 `TrayCtx::notify`（Shell_NotifyIcon 的 NIF_INFO）。
//!
//! 回调拿到 `TrayCtx`（显隐窗口 / 退出 / 气泡通知）。托盘状态存于 `WindowState`，
//! 窗口销毁时 `TrayState::drop` 自动 `NIM_DELETE` 并释放自建图标。

use std::ffi::c_void;
use std::mem::size_of;

pub(crate) use crate::platform::tray::{invoke, ItemKind, Tray, TrayAction};

use windows::core::PCWSTR;
use windows::Win32::Foundation::POINT;
use windows::Win32::Foundation::{HWND, LPARAM, TRUE};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HGDIOBJ,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    LoadIconW, SetForegroundWindow, TrackPopupMenu, HICON, HMENU, ICONINFO, IDI_APPLICATION,
    MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
    WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP,
};

/// 托盘回调消息（WM_APP+1）：lParam 低位为鼠标动作（legacy v0 编码）。
pub(crate) const WM_TRAYICON: u32 = WM_APP + 1;

/// 左键动作。单独成类型是为了让 `run_click` 的 match 天然穷尽——否则它得留一条
/// 「右键不该走到这」的兜底臂，而那种臂一旦被走到就是静默失效（菜单再也弹不出来，
/// 无 panic 无警告），正是本次重构要根除的失败模式。
pub(crate) enum ClickKind {
    Left,
    Double,
}

/// 托盘鼠标动作。分类不需要碰 `WindowState`，故 `classify` 是自由函数——右键路径
/// 因此完全不必取借用（借用窗口越窄越好，这是重入风险最高的路径）。
pub(crate) enum TrayEvent {
    Click(ClickKind),
    RightClick,
    Other,
}

/// 解析托盘回调消息的鼠标动作（lParam 低位，legacy v0 编码）。
pub(crate) fn classify(lparam: LPARAM) -> TrayEvent {
    match lparam.0 as u32 {
        WM_LBUTTONUP => TrayEvent::Click(ClickKind::Left),
        WM_LBUTTONDBLCLK => TrayEvent::Click(ClickKind::Double),
        WM_RBUTTONUP => TrayEvent::RightClick,
        _ => TrayEvent::Other,
    }
}

/// 运行期托盘状态（存于 WindowState）；drop 时清理托盘与自建图标。
pub(crate) struct TrayState {
    hwnd: HWND,
    uid: u32,
    hicon: HICON,
    owns_icon: bool,
    tray: Tray,
}

impl Drop for TrayState {
    fn drop(&mut self) {
        unsafe {
            let nid = base_nid(self.hwnd, self.uid);
            let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
            if self.owns_icon {
                let _ = DestroyIcon(self.hicon);
            }
        }
    }
}

/// 安装托盘图标（NIM_ADD）。失败返回 None。
pub(crate) fn install(hwnd: HWND, tray: Tray) -> Option<TrayState> {
    let (hicon, owns_icon) = match &tray.icon {
        Some((w, h, rgba)) => match unsafe { hicon_from_rgba(*w as i32, *h as i32, rgba) } {
            Some(h) => (h, true),
            None => (default_icon(), false),
        },
        None => (default_icon(), false),
    };
    let uid = 1u32;
    let mut nid = base_nid(hwnd, uid);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = hicon;
    copy_wide(&mut nid.szTip, &tray.tooltip);
    let ok = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) }.as_bool();
    if !ok {
        if owns_icon {
            unsafe {
                let _ = DestroyIcon(hicon);
            }
        }
        return None;
    }
    Some(TrayState {
        hwnd,
        uid,
        hicon,
        owns_icon,
        tray,
    })
}

/// 跑左键/双击回调，取回它声明的意图队列。
///
/// 就地跑回调是安全的——回调只写 `TrayAction`，不碰 OS（见 `TrayCtx`）。
/// 右键不走这里：菜单需要模态弹出，必须在借用之外分段完成，故签名只收
/// `ClickKind`——右键根本传不进来。
pub(crate) fn run_click(state: &mut TrayState, kind: ClickKind) -> Vec<TrayAction> {
    let cb = match kind {
        ClickKind::Left => state.tray.on_left_click.as_mut(),
        ClickKind::Double => state.tray.on_double_click.as_mut(),
    };
    invoke(cb)
}

/// 右键菜单句柄的 RAII 包装：drop 即 `DestroyMenu`。
///
/// 存在的理由：`build_menu` 是安全 fn，若直接交出裸 `HMENU`，日后任何在「建菜单」
/// 与「弹菜单」之间插入可失败步骤的安全代码都会静默泄漏内核对象。包成 RAII 后
/// 泄漏不可表达。
pub(crate) struct PopupMenu(HMENU);

impl Drop for PopupMenu {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

impl TrayState {
    /// 构建右键菜单。只 `CreatePopupMenu` + `AppendMenuW`，两者都不重入
    /// `wnd_proc`，故可在持有 `WindowState` 借用期间安全调用。
    pub(crate) fn build_menu(&self) -> Option<PopupMenu> {
        let hmenu = unsafe { CreatePopupMenu() }.ok()?;
        for (i, it) in self.tray.items.iter().enumerate() {
            match &it.kind {
                ItemKind::Separator => unsafe {
                    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
                },
                ItemKind::Action {
                    label,
                    checked,
                    enabled,
                    ..
                } => {
                    let mut flags = MF_STRING;
                    if checked.is_some_and(|c| c.get()) {
                        flags |= MF_CHECKED;
                    }
                    // 禁用：灰显且不可选（TPM_RETURNCMD 不会返回灰显项 id，故回调天然不触发）。
                    if enabled.is_some_and(|e| !e.get()) {
                        flags |= MF_GRAYED;
                    }
                    let w = wide_nul(label);
                    // 命令 id = 序号+1（分隔线不可选，故返回 id 必对应 Action）。
                    unsafe {
                        let _ = AppendMenuW(hmenu, flags, i + 1, PCWSTR(w.as_ptr()));
                    }
                }
            }
        }
        Some(PopupMenu(hmenu))
    }

    /// 跑菜单项 `id`（`track_menu` 的返回值）对应的回调，取回它声明的意图队列。
    /// 回调只写意图不碰 OS，故可在借用期间安全调用。
    ///
    /// `id` 是 1-based 序号，与 `build_menu` 的 `AppendMenuW(.., i + 1, ..)` 对应；
    /// 分隔线占序号但 id 恒为 0，`TPM_RETURNCMD` 永不返回，故解构失败即视为无意图。
    pub(crate) fn run_item(&mut self, id: usize) -> Vec<TrayAction> {
        let Some(idx) = id.checked_sub(1) else {
            return Vec::new();
        };
        invoke(self.tray.item_callback(idx))
    }

    /// 气泡通知的投递目标。取出后即可释放借用，由自由函数 `notify` 执行。
    pub(crate) fn notify_target(&self) -> (HWND, u32) {
        (self.hwnd, self.uid)
    }
}

/// 弹气泡通知。
///
/// **自由函数而非 `&TrayState` 方法是刻意的**：`Shell_NotifyIconW` 会经
/// `SendMessageTimeout` 与 shell 的托盘窗口跨线程通信，而跨线程发送期间本线程会
/// 泵入站消息。虽然读 `self` 的动作都发生在调用之前（故按 Stacked Borrows 仍成立），
/// 但那让正确性依赖「使用顺序」而非「借用已结构性死亡」——正是本次修复要消除的
/// 那类脆弱性。签名只收 hwnd/uid，借用便无处可藏。
pub(crate) fn notify(hwnd: HWND, uid: u32, title: &str, body: &str) {
    unsafe {
        let mut nid = base_nid(hwnd, uid);
        nid.uFlags = NIF_INFO;
        copy_wide(&mut nid.szInfoTitle, title);
        copy_wide(&mut nid.szInfo, body);
        nid.dwInfoFlags = NIIF_INFO;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    }
}

/// 弹出原生右键菜单，返回选中项的命令 id（0=未选/取消）。按值消费 `menu`，
/// 其 `Drop` 负责 `DestroyMenu`（含提前返回与 panic 路径）。
///
/// **自由函数而非方法是刻意的**：`TrackPopupMenu` 自带模态消息循环，菜单存续期间
/// 用户的每一次鼠标移动、窗口切换都会重入 `wnd_proc`。调用方必须已释放
/// `WindowState` 借用——签名只要 hwnd 不要 `&TrayState`，正是为了让借用无处可藏。
pub(crate) unsafe fn track_menu(hwnd: HWND, menu: PopupMenu) -> usize {
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // 必须前置窗口，否则菜单点击外部不消失（Win32 经典要求）。
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu.0,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        Some(0),
        hwnd,
        None,
    );
    cmd.0 as usize
}

/// 系统默认应用图标（无自定义图标时回退）。
fn default_icon() -> HICON {
    unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or_default()
}

/// 基础 NOTIFYICONDATAW（cbSize + hWnd + uID）。
fn base_nid(hwnd: HWND, uid: u32) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: uid,
        ..Default::default()
    }
}

/// 把 &str 写入定长 UTF-16 缓冲（截断 + NUL 收尾）。
fn copy_wide(dst: &mut [u16], s: &str) {
    let n = dst.len();
    if n == 0 {
        return;
    }
    let mut it = s.encode_utf16();
    for slot in dst.iter_mut().take(n - 1) {
        match it.next() {
            Some(c) => *slot = c,
            None => {
                *slot = 0;
                return;
            }
        }
    }
    dst[n - 1] = 0;
}

/// &str → 以 NUL 结尾的 UTF-16。
fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 从非预乘 RGBA8 造 HICON（32bpp 彩色位图 + 空掩码，透明走 alpha 通道）。
unsafe fn hicon_from_rgba(w: i32, h: i32, rgba: &[u8]) -> Option<HICON> {
    if w <= 0 || h <= 0 || rgba.len() < (w * h * 4) as usize {
        return None;
    }
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let hbm_color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
    if bits.is_null() {
        let _ = DeleteObject(HGDIOBJ(hbm_color.0));
        return None;
    }
    // RGBA → BGRA。
    let px = bits as *mut u8;
    for i in 0..(w * h) as usize {
        let s = i * 4;
        *px.add(s) = rgba[s + 2];
        *px.add(s + 1) = rgba[s + 1];
        *px.add(s + 2) = rgba[s];
        *px.add(s + 3) = rgba[s + 3];
    }
    let hbm_mask = CreateBitmap(w, h, 1, 1, None);
    let ii = ICONINFO {
        fIcon: TRUE,
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };
    let hicon = CreateIconIndirect(&ii).ok();
    let _ = DeleteObject(HGDIOBJ(hbm_color.0));
    let _ = DeleteObject(HGDIOBJ(hbm_mask.0));
    hicon
}
