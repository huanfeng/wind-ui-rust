//! 文件拖入：XDND 协议的**目标端**（版本 5）。源端（拖出）见 `dnd_source`，共用这里的原子与编码。
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
        XdndActionMove,
        XdndActionLink,
        XdndProxy,
        XdndTypeList,
        TARGETS,
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

/// 路径 → `text/uri-list`（RFC 2483：每行一个 `file://` URI，CRLF 结尾）。除 RFC 3986 的
/// 非保留字符与 `/` 外一律百分号转义，按字节进行，非 UTF-8 的文件名也能原样还原。
/// 相对路径没有对应的 URI，跳过。
pub(super) fn uri_list(paths: &[impl AsRef<std::path::Path>]) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut out = Vec::new();
    for p in paths {
        let p = p.as_ref();
        if !p.is_absolute() {
            log::warn!("drag_files: 跳过相对路径 {}", p.display());
            continue;
        }
        out.extend_from_slice(b"file://");
        for &b in p.as_os_str().as_bytes() {
            if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
                out.push(b);
            } else {
                out.extend_from_slice(format!("%{b:02X}").as_bytes());
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_escapes_and_round_trips() {
        let paths = ["/tmp/a b/说明.txt", "/x/100%#?.txt"];
        let bytes = uri_list(&paths);
        let text = String::from_utf8(bytes.clone()).unwrap();
        assert!(text.starts_with("file:///tmp/a%20b/%E8%AF%B4"), "{text}");
        assert!(text.contains("/x/100%25%23%3F.txt\r\n"), "{text}");
        let back = super::super::host::parse_uri_list(&bytes);
        assert_eq!(back, paths.map(std::path::PathBuf::from).to_vec());
    }

    #[test]
    fn uri_list_skips_relative_paths() {
        assert!(uri_list(&["rel/a.txt"]).is_empty());
    }
}
