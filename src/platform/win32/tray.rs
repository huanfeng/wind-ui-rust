//! 系统托盘图标（Shell_NotifyIcon）：图标 + 提示 + 左键/双击回调 + 原生右键菜单。
//!
//! 右键菜单走原生 `TrackPopupMenu`（真 OS 弹出，显示在托盘旁，窗口外），支持
//! 勾选项（`checked` 绑定 `Rc<Cell<bool>>`，菜单弹出时按当前值显示对勾）与分隔线。
//! 气泡通知经 `TrayCtx::notify`（Shell_NotifyIcon 的 NIF_INFO）。
//!
//! 回调拿到 `TrayCtx`（显隐窗口 / 退出 / 气泡通知）。托盘状态存于 `WindowState`，
//! 窗口销毁时 `TrayState::drop` 自动 `NIM_DELETE` 并释放自建图标。

use std::cell::Cell;
use std::ffi::c_void;
use std::mem::size_of;
use std::rc::Rc;

use windows::core::PCWSTR;
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
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, DestroyWindow,
    GetCursorPos, LoadIconW, SetForegroundWindow, ShowWindow, TrackPopupMenu, HICON, ICONINFO,
    IDI_APPLICATION, MF_CHECKED, MF_SEPARATOR, MF_STRING, SW_HIDE, SW_SHOW, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_APP, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP,
};
use windows::Win32::Foundation::POINT;

/// 托盘回调消息（WM_APP+1）：lParam 低位为鼠标动作（legacy v0 编码）。
pub(crate) const WM_TRAYICON: u32 = WM_APP + 1;

/// 托盘回调上下文：操作窗口与弹气泡（不暴露裸 hwnd）。
pub struct TrayCtx {
    hwnd: HWND,
    uid: u32,
}

impl TrayCtx {
    /// 显示并前置窗口（托盘最常见动作）。
    pub fn show_window(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetForegroundWindow(self.hwnd);
        }
    }
    /// 隐藏窗口（最小化到托盘）。
    pub fn hide_window(&self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }
    /// 退出应用（销毁窗口 → 清理托盘）。
    pub fn quit(&self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
    /// 弹出气泡通知（标题 + 正文）。
    pub fn notify(&self, title: &str, body: &str) {
        unsafe {
            let mut nid = base_nid(self.hwnd, self.uid);
            nid.uFlags = NIF_INFO;
            copy_wide(&mut nid.szInfoTitle, title);
            copy_wide(&mut nid.szInfo, body);
            nid.dwInfoFlags = NIIF_INFO;
            let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
        }
    }
}

type TrayFn = Box<dyn FnMut(&mut TrayCtx)>;

enum ItemKind {
    Action { label: String, checked: Option<Rc<Cell<bool>>>, cb: TrayFn },
    Separator,
}

/// 托盘右键菜单项：普通项 / 勾选项 / 分隔线。
pub struct TrayMenuItem {
    kind: ItemKind,
}

impl TrayMenuItem {
    /// 普通项：点击触发回调。
    pub fn item(label: impl Into<String>, cb: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        Self { kind: ItemKind::Action { label: label.into(), checked: None, cb: Box::new(cb) } }
    }
    /// 勾选项：`checked` 绑定状态，菜单弹出时按当前值显示对勾；点击触发回调
    /// （回调内自行翻转 `checked` 即可，框架不自动改）。
    pub fn check(
        label: impl Into<String>,
        checked: Rc<Cell<bool>>,
        cb: impl FnMut(&mut TrayCtx) + 'static,
    ) -> Self {
        Self {
            kind: ItemKind::Action { label: label.into(), checked: Some(checked), cb: Box::new(cb) },
        }
    }
    /// 分隔线。
    pub fn separator() -> Self {
        Self { kind: ItemKind::Separator }
    }
}

/// 托盘图标构建器。交给 `App::tray(...)`。
#[derive(Default)]
pub struct Tray {
    tooltip: String,
    icon: Option<(u32, u32, Vec<u8>)>,
    on_left_click: Option<TrayFn>,
    on_double_click: Option<TrayFn>,
    items: Vec<TrayMenuItem>,
}

impl Tray {
    pub fn new() -> Self {
        Self::default()
    }
    /// 鼠标悬停提示。
    pub fn tooltip(mut self, s: impl Into<String>) -> Self {
        self.tooltip = s.into();
        self
    }
    /// 自定义图标：原始非预乘 RGBA8（`rgba.len()==w*h*4`）。未设则用系统默认应用图标。
    pub fn icon_rgba(mut self, w: u32, h: u32, rgba: &[u8]) -> Self {
        self.icon = Some((w, h, rgba.to_vec()));
        self
    }
    /// 左键单击回调（常见用于显隐窗口）。
    pub fn on_left_click(mut self, f: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        self.on_left_click = Some(Box::new(f));
        self
    }
    /// 左键双击回调。
    pub fn on_double_click(mut self, f: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        self.on_double_click = Some(Box::new(f));
        self
    }
    /// 右键菜单项（普通/勾选/分隔线）。
    pub fn menu(mut self, items: Vec<TrayMenuItem>) -> Self {
        self.items = items;
        self
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
    Some(TrayState { hwnd, uid, hicon, owns_icon, tray })
}

/// 处理托盘回调消息：左键/双击触发回调，右键弹原生菜单。
pub(crate) fn handle_message(state: &mut TrayState, lparam: LPARAM) {
    match lparam.0 as u32 {
        WM_LBUTTONUP => invoke(state.tray.on_left_click.as_mut(), state.hwnd, state.uid),
        WM_LBUTTONDBLCLK => invoke(state.tray.on_double_click.as_mut(), state.hwnd, state.uid),
        WM_RBUTTONUP => unsafe { show_menu(state) },
        _ => {}
    }
}

fn invoke(cb: Option<&mut TrayFn>, hwnd: HWND, uid: u32) {
    if let Some(cb) = cb {
        let mut ctx = TrayCtx { hwnd, uid };
        cb(&mut ctx);
    }
}

/// 构建并弹出原生右键菜单，调用选中项回调。
unsafe fn show_menu(state: &mut TrayState) {
    let Ok(hmenu) = CreatePopupMenu() else { return };
    for (i, it) in state.tray.items.iter().enumerate() {
        match &it.kind {
            ItemKind::Separator => {
                let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
            }
            ItemKind::Action { label, checked, .. } => {
                let mut flags = MF_STRING;
                if checked.as_ref().is_some_and(|c| c.get()) {
                    flags |= MF_CHECKED;
                }
                let w = wide_nul(label);
                // 命令 id = 序号+1（分隔线不可选，故返回 id 必对应 Action）。
                let _ = AppendMenuW(hmenu, flags, i + 1, PCWSTR(w.as_ptr()));
            }
        }
    }
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // 必须前置窗口，否则菜单点击外部不消失（Win32 经典要求）。
    let _ = SetForegroundWindow(state.hwnd);
    let cmd = TrackPopupMenu(
        hmenu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD,
        pt.x,
        pt.y,
        0,
        state.hwnd,
        None,
    );
    let _ = DestroyMenu(hmenu);
    let id = cmd.0 as usize;
    if id >= 1 && id <= state.tray.items.len() {
        if let ItemKind::Action { cb, .. } = &mut state.tray.items[id - 1].kind {
            let mut ctx = TrayCtx { hwnd: state.hwnd, uid: state.uid };
            cb(&mut ctx);
        }
    }
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
