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

use crate::signal::Signal;

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

/// 托盘回调想做的事。**纯意图，不含任何 OS 调用**。
///
/// 存在的理由见 `TrayCtx`：回调在持有 `WindowState` 借用期间运行，此时碰 OS 就是 UB。
/// 回调只能写下这个值，由借用释放后的 `run_tray_action` 代为执行。
pub(crate) enum TrayAction {
    Show,
    Hide,
    Quit,
    Notify { title: String, body: String },
}

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

/// 托盘回调上下文：声明要对窗口做什么（不暴露裸 hwnd）。
///
/// **这里的方法只记录意图，不调用任何 OS API**——回调运行时 `wnd_proc` 正持有
/// `&mut WindowState`，而 `ShowWindow` / `DestroyWindow` / `TrackPopupMenu` 都会同步
/// 派发消息重入 `wnd_proc`，届时再取一次 `&mut WindowState` 即形成别名 UB（铁律 6，
/// 无 RefCell 故不会 panic，只会静默出错）。真正的执行发生在借用释放之后。
///
/// 意图按调用顺序累积成队列，逐条执行——故一个回调内 `notify` 后再 `show_window`
/// 两者都生效，与「立即执行」的直觉一致（macOS 侧本就是立即执行，语义由此对齐）。
pub struct TrayCtx {
    actions: Vec<TrayAction>,
}

impl TrayCtx {
    /// 显示并前置窗口（托盘最常见动作）。
    pub fn show_window(&mut self) {
        self.actions.push(TrayAction::Show);
    }
    /// 隐藏窗口（最小化到托盘）。
    pub fn hide_window(&mut self) {
        self.actions.push(TrayAction::Hide);
    }
    /// 退出应用（销毁窗口 → 清理托盘）。
    pub fn quit(&mut self) {
        self.actions.push(TrayAction::Quit);
    }
    /// 弹出气泡通知（标题 + 正文）。
    pub fn notify(&mut self, title: &str, body: &str) {
        self.actions.push(TrayAction::Notify {
            title: title.to_string(),
            body: body.to_string(),
        });
    }
}

type TrayFn = Box<dyn FnMut(&mut TrayCtx)>;

enum ItemKind {
    Action {
        label: String,
        /// 勾选态绑定（None=从不打勾）；菜单弹出时读当前值。
        checked: Option<Signal<bool>>,
        /// 禁用态绑定（None=始终可用）；菜单弹出时读当前值，false 则灰显且不可点。
        enabled: Option<Signal<bool>>,
        cb: TrayFn,
    },
    Separator,
}

/// 托盘右键菜单项：普通项 / 勾选项 / 分隔线。
pub struct TrayMenuItem {
    kind: ItemKind,
}

impl TrayMenuItem {
    /// 普通项：点击触发回调。
    pub fn item(label: impl Into<String>, cb: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        Self {
            kind: ItemKind::Action {
                label: label.into(),
                checked: None,
                enabled: None,
                cb: Box::new(cb),
            },
        }
    }
    /// 勾选项：`checked` 绑定状态，菜单弹出时按当前值显示对勾；点击触发回调
    /// （回调内自行翻转 `checked` 即可，框架不自动改）。
    ///
    /// `Signal<bool>` 是 `!Send` 的（存储线程局部），故整个 `Tray` 也是 `!Send`——
    /// 托盘菜单在 UI 线程构建、勾选态也在 UI 线程的菜单弹出路径上读取，
    /// 把构建好的 `Tray` 搬到别的线程会在编译期就被拦下。
    pub fn check(
        label: impl Into<String>,
        checked: Signal<bool>,
        cb: impl FnMut(&mut TrayCtx) + 'static,
    ) -> Self {
        Self {
            kind: ItemKind::Action {
                label: label.into(),
                checked: Some(checked),
                enabled: None,
                cb: Box::new(cb),
            },
        }
    }
    /// 绑定禁用态：`flag` 为 false 时该项灰显且不可点（菜单弹出时读当前值）。
    /// 对分隔线无效。永久禁用可传 `signal(false)`。
    pub fn enabled(mut self, flag: Signal<bool>) -> Self {
        if let ItemKind::Action { enabled, .. } = &mut self.kind {
            *enabled = Some(flag);
        }
        self
    }
    /// 分隔线。
    pub fn separator() -> Self {
        Self {
            kind: ItemKind::Separator,
        }
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

/// 跑一个回调，取回它声明的意图队列。
fn invoke(cb: Option<&mut TrayFn>) -> Vec<TrayAction> {
    let Some(cb) = cb else { return Vec::new() };
    let mut ctx = TrayCtx {
        actions: Vec::new(),
    };
    cb(&mut ctx);
    ctx.actions
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
        if id < 1 || id > self.tray.items.len() {
            return Vec::new();
        }
        let ItemKind::Action { cb, .. } = &mut self.tray.items[id - 1].kind else {
            return Vec::new();
        };
        let mut ctx = TrayCtx {
            actions: Vec::new(),
        };
        cb(&mut ctx);
        ctx.actions
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::signal;

    /// 编译期护栏：`Tray` 必须保持 `!Send`。
    ///
    /// 勾选态与禁用态都绑 `Signal<bool>`，而信号的存储是**线程局部**的——句柄搬到别的
    /// 线程再读，读到的是那个线程的槽位表。`!Send` 让「构建 Tray 的线程」与「弹菜单读
    /// 勾选态的线程」必然是同一个：`Tray` 只能原地交给 `App::tray`，`App` 因此也 `!Send`，
    /// `App::run` 在同一线程消费它并在那里建窗口，而 Win32 保证窗口消息只由建它的
    /// 线程派发（`build_menu` / `run_item` 都在 `wnd_proc` 里）。
    ///
    /// 一旦 `Tray` 变成 `Send`，下面两条 impl 同时适用，方法解析出歧义，编译失败。
    const _: fn() = || {
        trait AmbiguousIfSend<A> {
            fn tag() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        struct Invalid;
        impl<T: ?Sized + Send> AmbiguousIfSend<Invalid> for T {}
        let _ = <Tray as AmbiguousIfSend<_>>::tag;
    };

    /// 勾选态是**弹出时现读**而非构建时快照：构建完菜单项后翻转信号，
    /// 下一次弹出就该显示新状态（这正是 `check` 收信号而非 `bool` 的全部理由）。
    #[test]
    fn check_binds_the_signal_instead_of_snapshotting_its_value() {
        let on = signal(false);
        let it = TrayMenuItem::check("启用通知", on, |_| {});
        let ItemKind::Action { checked, .. } = &it.kind else {
            unreachable!("check() 建的就是 Action 项");
        };
        assert_eq!(checked.map(|c| c.get()), Some(false));
        on.set(true);
        assert_eq!(checked.map(|c| c.get()), Some(true));
    }

    /// 普通项不带勾选绑定（`None` = 从不打勾）；分隔线根本没有这组字段。
    #[test]
    fn item_and_separator_carry_no_check_binding() {
        let it = TrayMenuItem::item("显示窗口", |_| {});
        let ItemKind::Action { checked, .. } = &it.kind else {
            unreachable!()
        };
        assert!(checked.is_none());
        assert!(matches!(
            TrayMenuItem::separator().kind,
            ItemKind::Separator
        ));
    }
}
