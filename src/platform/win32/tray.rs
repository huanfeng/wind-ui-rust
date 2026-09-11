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

use windows::core::{w, PCWSTR};
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
    LoadIconW, RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu, HICON, HMENU, ICONINFO,
    IDI_APPLICATION, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_APP, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP,
};

/// 托盘回调消息（WM_APP+1）：lParam 低位为鼠标动作（legacy v0 编码）。
pub(crate) const WM_TRAYICON: u32 = WM_APP + 1;

/// Shell 重建托盘区后广播的「请重新登记图标」消息号。
///
/// 托盘图标是**登记在 explorer.exe 的托盘窗口里**的，不是内核持有的资源：explorer
/// 一崩溃或被重启，所有登记随旧 shell 一起蒸发，我们的进程还活着，却再没有任何可见
/// 入口——用户看到的就是「图标没了」。新 shell 起来后会向所有**顶层**窗口广播这条
/// 消息，每个托盘程序必须自己接住并重新 `NIM_ADD`；Win32 不做任何自动恢复。
///
/// 消息号是运行期向系统申请的（同名字符串全系统同号），不是编译期常量，故走
/// `RegisterWindowMessageW`。结果缓存在线程局部：托盘本就是 UI 线程的单例，且
/// 每条消息都要拿它来比对，没必要每次都进一次系统调用。返回 0 表示申请失败，
/// 调用方必须先排除 0 再比对——否则会把所有未处理消息都当成 shell 重启。
pub(crate) fn taskbar_created_msg() -> u32 {
    thread_local! {
        static MSG: u32 = {
            let m = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
            // 注册失败整条恢复路径就此失效，而症状与原 bug 一模一样（图标没了、进程
            // 还在、零线索）。留一行是这种情况下唯一能定位的东西；thread_local 只初始化
            // 一次，不会刷屏。
            if m == 0 {
                eprintln!("[windui] TaskbarCreated 注册失败，shell 重启后托盘图标将无法自动恢复");
            }
            m
        };
    }
    MSG.with(|m| *m)
}

/// 这条消息是不是 shell 重启广播。
///
/// 单独成函数只为一件事：把 `registered != 0` 这个判断钉在测试里。它看着像句多余的
/// 防御，实则是整条路径最脆的一环——`RegisterWindowMessageW` 失败返回 0，而窗口过程
/// 里凡是走到这个判定的都是**未被前面各臂匹配的消息**，其中 `msg == 0` 的恰恰是
/// `WM_NULL`。少了这半句，注册一旦失败，每条 WM_NULL 都会被当成一次 shell 重启，
/// 于是每条 WM_NULL 都去跨线程问一次 shell（`readd`），白白拖慢整个消息循环。
pub(crate) fn is_taskbar_created(msg: u32, registered: u32) -> bool {
    registered != 0 && msg == registered
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

/// 运行期托盘状态（存于 WindowState）；drop 时清理托盘与自建图标。
pub(crate) struct TrayState {
    hwnd: HWND,
    uid: u32,
    hicon: HICON,
    owns_icon: bool,
    tray: Tray,
    /// 当前生效的悬停提示。
    ///
    /// 与 `tray.tooltip`（建表时那份初始值）分开存，是因为 shell 重启后要按**现在**的
    /// 提示重新登记。`TrayHandle::set_tooltip` 改的是 shell 里那份，若这里不同步跟着走，
    /// 图标恢复时提示会悄悄退回启动时的初值——对 wind-dict 这种把当前热键写进提示的
    /// 应用，那就是显示一个早已改掉的快捷键。
    tooltip: String,
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
    let nid = add_nid(hwnd, uid, hicon, &tray.tooltip);
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
        tooltip: tray.tooltip.clone(),
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

    /// 重新登记所需的全部素材。与 [`notify_target`] 同理：取完即可释放借用，
    /// 真正碰 OS 的是自由函数 [`readd`]。
    ///
    /// `hicon` 照旧取用而不重建：图标是**本进程**的 GDI 对象，explorer 死掉动不了它。
    pub(crate) fn readd_target(&self) -> (HWND, u32, HICON, String) {
        (self.hwnd, self.uid, self.hicon, self.tooltip.clone())
    }

    /// 记下已生效的新提示（`set_tooltip` 成功后调用），供 shell 重启时重放。
    pub(crate) fn remember_tooltip(&mut self, tip: String) {
        self.tooltip = tip;
    }
}

/// Shell 重启后重新登记托盘图标。
///
/// **自由函数而非 `&TrayState` 方法，理由同 [`notify`]**：`Shell_NotifyIconW` 跨线程与
/// shell 通信，期间本线程泵入站消息，可能重入窗口过程再借一次 `AppHost`。素材先由
/// [`TrayState::readd_target`] 取走，借用便无处可藏。
///
/// **先 `NIM_MODIFY` 探路，失败才 `NIM_ADD`**，返回图标最终是否登记成功。
///
/// 这条广播**每个顶层窗口各收一次**（`wnd_proc` 是所有窗口共用的），所以本函数会被
/// 连着调用好几次，必须幂等。`NIM_MODIFY` 恰好给出一个无损的存在性判据：图标还在就
/// 用同样的数据就地更新（什么也没变），不在就返回 FALSE，交给 `NIM_ADD`。
///
/// **不能写成「先 DELETE 再 ADD」**：那对第一次广播没问题（新 shell 本就不认识这个
/// (hwnd, uid)，删除只是失败一下），但第二次广播时删掉的是一个**正在正常工作**的图标，
/// 之后能否加回来全押在 `NIM_ADD` 上——而 `NIM_ADD` 恰恰是这条路上最会失败的一步
/// （见 `readd_tray` 的重试）。那等于把「重复广播」从无害变成了可能亲手弄丢图标。
pub(crate) fn readd(hwnd: HWND, uid: u32, hicon: HICON, tip: &str) -> bool {
    unsafe {
        let nid = add_nid(hwnd, uid, hicon, tip);
        if Shell_NotifyIconW(NIM_MODIFY, &nid).as_bool() {
            return true;
        }
        Shell_NotifyIconW(NIM_ADD, &nid).as_bool()
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

/// 换鼠标悬停提示（`NIF_TIP` + `NIM_MODIFY`）。
///
/// **自由函数而非 `&TrayState` 方法，理由同 [`notify`]**：`Shell_NotifyIconW` 会经
/// `SendMessageTimeout` 与 shell 的托盘窗口跨线程通信，期间本线程泵入站消息。签名
/// 只收 hwnd/uid，借用便无处可藏。
pub(crate) fn set_tooltip(hwnd: HWND, uid: u32, tip: &str) {
    unsafe {
        let mut nid = base_nid(hwnd, uid);
        nid.uFlags = NIF_TIP;
        copy_wide(&mut nid.szTip, tip);
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

/// 登记用的完整 NOTIFYICONDATAW（图标 + 回调消息 + 提示）。
///
/// 首次安装与 shell 重启后的重登记共用同一份构造：两者必须逐字段一致，否则恢复出来的
/// 图标会少点什么（最典型是漏了 `uCallbackMessage`——图标看着在，点了没反应）。
fn add_nid(hwnd: HWND, uid: u32, hicon: HICON, tip: &str) -> NOTIFYICONDATAW {
    let mut nid = base_nid(hwnd, uid);
    nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = hicon;
    copy_wide(&mut nid.szTip, tip);
    nid
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
///
/// 托盘图标与窗口图标（`App::icon`）走的是同一条：Win32 里两者都是 HICON，
/// 差别只在设给谁（`Shell_NotifyIconW` 还是 `WM_SETICON`）。
pub(super) unsafe fn hicon_from_rgba(w: i32, h: i32, rgba: &[u8]) -> Option<HICON> {
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
    use super::{add_nid, is_taskbar_created, WM_TRAYICON};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP};
    use windows::Win32::UI::WindowsAndMessaging::HICON;

    /// 登记数据必须三要素齐全。
    ///
    /// 钉的是 `add_nid` 文档自己点名的那个坑：漏了 `uCallbackMessage`，图标看着好好的，
    /// 点了却没反应——首装与 shell 重启后的重登记共用这一份构造，这里错一次是两处错。
    /// 纯数据构造，不碰 OS，故句柄传空指针即可。
    #[test]
    fn 登记数据带齐图标回调与提示() {
        let nid = add_nid(
            HWND(std::ptr::null_mut()),
            1,
            HICON(std::ptr::null_mut()),
            "提示",
        );
        assert_eq!(nid.uFlags, NIF_ICON | NIF_MESSAGE | NIF_TIP);
        assert_eq!(nid.uCallbackMessage, WM_TRAYICON, "漏了它则图标点了没反应");
        assert_eq!(nid.uID, 1);
        let tip: String = nid
            .szTip
            .iter()
            .take_while(|c| **c != 0)
            .map(|c| char::from_u32(*c as u32).unwrap())
            .collect();
        assert_eq!(tip, "提示");
    }

    /// 超长提示按缓冲长度截断，且**始终**以 NUL 收尾。
    ///
    /// `szTip` 是定长数组，写满而不收尾的话 shell 会一直读到越界内容。
    #[test]
    fn 超长提示被截断且以空字符收尾() {
        let long = "提".repeat(500);
        let nid = add_nid(
            HWND(std::ptr::null_mut()),
            1,
            HICON(std::ptr::null_mut()),
            &long,
        );
        let n = nid.szTip.len();
        assert_eq!(nid.szTip[n - 1], 0, "定长缓冲必须以 NUL 收尾");
        assert!(nid.szTip[..n - 1].iter().all(|c| *c != 0), "截断前应写满");
    }

    /// 注册成功时按消息号精确匹配。
    #[test]
    fn 匹配已注册的广播消息号() {
        assert!(is_taskbar_created(0xC123, 0xC123));
        assert!(!is_taskbar_created(0xC124, 0xC123));
    }

    /// 注册失败（返回 0）时**任何**消息都不算 —— 包括 WM_NULL 自己。
    #[test]
    fn 注册失败时一律不匹配() {
        assert!(!is_taskbar_created(0, 0), "WM_NULL 不该被当成 shell 重启");
        assert!(!is_taskbar_created(0xC123, 0));
    }
}
