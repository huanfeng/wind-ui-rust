//! 启动通知：桌面启动器（应用菜单、概览、任务栏）给新进程的启动 id。
//!
//! 启动器在环境里放一个 id，等应用「启动完成」的信号；收不到就一直转圈到超时。两种来源：
//! - `XDG_ACTIVATION_TOKEN`：Wayland 的激活令牌（glib 2.76+ 的启动器会设）。
//! - `DESKTOP_STARTUP_ID`：freedesktop startup-notification 的 id。X11 会话只有它；老一些的
//!   GNOME（Ubuntu 22.04，glib 2.72）在 Wayland 会话里也只设它——mutter 照样把它当激活令牌收
//!   （GTK3 的 Wayland 后端就这么回退）。
//!
//! 完成信号：Wayland 下用它 `xdg_activation_v1.activate` 首窗（`wayland::activation`）；X11 下
//! 给首窗设 `_NET_STARTUP_ID` 并向根窗口广播 `remove: ID=…`（[`remove_message`] /
//! [`chunks`]）。用掉后两个变量都从环境里删掉，免得泄漏给子进程。

/// 两个候选里取第一个非空的。
pub(crate) fn pick(first: Option<String>, second: Option<String>) -> Option<String> {
    [first, second]
        .into_iter()
        .flatten()
        .find(|t| !t.is_empty())
}

/// 二次实例要转发的启动 id（只读不删，见 `single_instance::argv_to_forward`）：优先激活令牌。
pub(crate) fn peek() -> Option<String> {
    pick(
        std::env::var("XDG_ACTIVATION_TOKEN").ok(),
        std::env::var("DESKTOP_STARTUP_ID").ok(),
    )
}

/// 取走本进程的启动 id：`wayland` 优先激活令牌，X11 优先 `DESKTOP_STARTUP_ID`（两者都在时多半
/// 是同一个值，各取本协议的那个）。两个变量都删掉——改环境要求此时没有别的线程直接 `getenv`，
/// 与 GTK 打开显示时做同一件事的前提相同。
pub(crate) fn take(wayland: bool) -> Option<String> {
    let token = std::env::var("XDG_ACTIVATION_TOKEN").ok();
    let startup = std::env::var("DESKTOP_STARTUP_ID").ok();
    let picked = prefer(wayland, token.clone(), startup.clone());
    if token.is_some() {
        std::env::remove_var("XDG_ACTIVATION_TOKEN");
    }
    if startup.is_some() {
        std::env::remove_var("DESKTOP_STARTUP_ID");
    }
    picked
}

/// 按后端选：Wayland 优先激活令牌，X11 优先 `DESKTOP_STARTUP_ID`，缺了用另一个。
fn prefer(wayland: bool, token: Option<String>, startup: Option<String>) -> Option<String> {
    if wayland {
        pick(token, startup)
    } else {
        pick(startup, token)
    }
}

/// startup-notification 的 `remove` 消息，含结尾 NUL：`remove: ID=<值>`。值里的空格、双引号、
/// 反斜杠前加反斜杠（与 libstartup-notification、GTK 的写法一致）。
pub(crate) fn remove_message(id: &str) -> Vec<u8> {
    let mut msg = String::from("remove: ID=");
    for c in id.chars() {
        if matches!(c, ' ' | '"' | '\\') {
            msg.push('\\');
        }
        msg.push(c);
    }
    let mut bytes = msg.into_bytes();
    bytes.push(0);
    bytes
}

/// 切成 ClientMessage 的 20 字节片，最后一片不足补零（NUL 已在消息里，故至少带一个零）。
/// 第一片用 `_NET_STARTUP_INFO_BEGIN`，其余用 `_NET_STARTUP_INFO`。
pub(crate) fn chunks(msg: &[u8]) -> Vec<[u8; 20]> {
    msg.chunks(20)
        .map(|c| {
            let mut out = [0u8; 20];
            out[..c.len()].copy_from_slice(c);
            out
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_non_empty_candidate_wins() {
        assert_eq!(pick(Some("a".into()), Some("b".into())), Some("a".into()));
        assert_eq!(pick(None, Some("b".into())), Some("b".into()), "缺了回退");
        assert_eq!(
            pick(Some(String::new()), Some("b".into())),
            Some("b".into()),
            "空串当没有"
        );
        assert_eq!(pick(None, Some(String::new())), None);
    }

    #[test]
    fn each_backend_prefers_its_own_variable_and_falls_back_to_the_other() {
        let t = || Some("token".to_string());
        let d = || Some("startup".to_string());
        assert_eq!(prefer(true, t(), d()), t());
        assert_eq!(prefer(false, t(), d()), d());
        assert_eq!(
            prefer(true, None, d()),
            d(),
            "老 GNOME：Wayland 下只有 DESKTOP_STARTUP_ID"
        );
        assert_eq!(prefer(false, t(), None), t());
    }

    #[test]
    fn spaces_quotes_and_backslashes_are_escaped() {
        let m = remove_message(r#"gnome-shell/windui 多窗口 "x"\y/1_TIME5"#);
        assert_eq!(
            std::str::from_utf8(&m).unwrap(),
            "remove: ID=gnome-shell/windui\\ 多窗口\\ \\\"x\\\"\\\\y/1_TIME5\0"
        );
    }

    #[test]
    fn chunks_are_twenty_bytes_and_the_message_ends_in_a_nul() {
        let m = remove_message("abcdefghi"); // "remove: ID=abcdefghi\0" = 21 字节
        assert_eq!(m.len(), 21);
        let c = chunks(&m);
        assert_eq!(c.len(), 2);
        assert_eq!(&c[0], b"remove: ID=abcdefghi");
        assert_eq!(c[1], [0u8; 20], "只剩结尾 NUL 也要单发一片");
        let joined: Vec<u8> = c.concat();
        assert_eq!(&joined[..m.len()], &m[..]);
        assert_eq!(
            chunks(&remove_message("abcdefgh")).len(),
            1,
            "恰好 20 字节含 NUL：一片"
        );
    }
}
