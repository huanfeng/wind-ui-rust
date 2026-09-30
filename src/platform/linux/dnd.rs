//! 文件拖入：XDND 协议的**目标端**（版本 5）。
//!
//! 流程：窗口声明 `XdndAware` → 源端发 `XdndEnter` / `XdndPosition`（我们回 `XdndStatus`
//! 表示接受、动作是复制）→ `XdndDrop` 时我们向 `XdndSelection` 要 `text/uri-list` →
//! `SelectionNotify` 到达后读属性、解析成路径交宿主 → 回 `XdndFinished`。
//!
//! 只接受本地文件（`file://`）。拖入的若只有网址或纯文本，Position 阶段就回「不接受」，
//! 源端据此显示禁止光标——与 win32 `WM_DROPFILES` 只收文件的语义一致。

use x11rb::protocol::xproto::{Atom, ClientMessageEvent, Window};

x11rb::atom_manager! {
    pub(super) DndAtoms: DndAtomsCookie {
        XdndAware,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndSelection,
        XdndActionCopy,
        XdndTypeList,
        TEXT_URI_LIST: b"text/uri-list",
    }
}

/// XDND 协议版本（我们实现的最高版本）。
pub(super) const XDND_VERSION: u32 = 5;

/// 一次进行中的拖放（从 Enter 到 Drop/Leave）。
#[derive(Default)]
pub(super) struct DragState {
    pub source: Window,
    pub target: Window,
    /// 源端提供了 `text/uri-list`。
    pub has_uris: bool,
    /// 最近一次 Position 的落点（窗口内物理像素）。
    pub pos: (i32, i32),
    /// 已发出 ConvertSelection、等 SelectionNotify。
    pub awaiting_data: bool,
}

/// `XdndEnter` 声明的类型里有没有 `text/uri-list`。超过 3 种类型时（data.l[1] 最低位为 1）
/// 完整列表在源窗口的 `XdndTypeList` 属性上，由调用方另读后传进 `extra`。
pub(super) fn enter_has_uris(ev: &ClientMessageEvent, uri_list: Atom, extra: &[Atom]) -> bool {
    let d = ev.data.as_data32();
    d[2..5].contains(&uri_list) || extra.contains(&uri_list)
}
