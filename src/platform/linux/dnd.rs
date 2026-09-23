//! 文件拖入：XDND 协议的**目标端**（版本 5）。
//!
//! 流程：窗口声明 `XdndAware` → 源端发 `XdndEnter` / `XdndPosition`（我们回 `XdndStatus`
//! 表示接受、动作是复制）→ `XdndDrop` 时我们向 `XdndSelection` 要 `text/uri-list` →
//! `SelectionNotify` 到达后读属性、解析成路径交宿主 → 回 `XdndFinished`。
//!
//! 只接受本地文件（`file://`）。拖入的若只有网址或纯文本，Position 阶段就回「不接受」，
//! 源端据此显示禁止光标——与 win32 `WM_DROPFILES` 只收文件的语义一致。

use std::path::PathBuf;

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

/// 解析 `text/uri-list`：跳过注释行，只取 `file://` 且能解码的本地路径。
pub(super) fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(data)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(file_uri_to_path)
        .collect()
}

fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///abs` 或 `file://localhost/abs`；别的主机名是网络路径，不是本机文件。
    let path = if rest.starts_with('/') {
        rest
    } else {
        let (host, p) = rest.split_once('/')?;
        if !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        return percent_decode(&format!("/{p}")).map(PathBuf::from);
    };
    percent_decode(path).map(PathBuf::from)
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            // 截断的转义（`%2` 在末尾）取不到两位十六进制，整条丢弃。
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_yields_decoded_local_paths() {
        let data = b"# comment\r\nfile:///home/u/a%20b.txt\r\nfile://localhost/tmp/%E4%B8%AD.png\r\nhttps://x.org/y\r\nfile://otherhost/z\r\n";
        assert_eq!(
            parse_uri_list(data),
            vec![
                PathBuf::from("/home/u/a b.txt"),
                PathBuf::from("/tmp/中.png")
            ]
        );
    }

    #[test]
    fn malformed_escapes_are_dropped_not_panicking() {
        assert_eq!(
            parse_uri_list(b"file:///a%2\nfile:///b%zz\n"),
            Vec::<PathBuf>::new()
        );
    }
}
