//! 跨平台单实例 + 二次运行传参。
//!
//! 首实例建锁并监听一个本地端点;二次实例把 argv 发给首实例后由调用方退出。
//! Windows = 命名 Mutex + 独立 message-only 窗口(class=app_id) + WM_COPYDATA;
//! macOS/Linux = Unix domain socket。命名与编码逻辑抽为纯函数(下方),便于单测与两端一致。
//!
//! 平台实现见 `win.rs` / `unix.rs`;对外入口 [`acquire`] / [`start_listener`] 在 `app.rs` 集成。

/// 命名 Mutex 名(Windows)。`Local\` 前缀使其会话内唯一。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn mutex_name(app_id: &str) -> String {
    format!(r"Local\{app_id}_si_mutex")
}

/// message-only 窗口 class 名(Windows)。必须含 app_id 以避免与 windui 共享的主窗口 class 撞名。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn class_name(app_id: &str) -> String {
    format!("{app_id}_si_win")
}

/// Unix socket 路径(macOS/Linux):`$TMPDIR`(回退 /tmp)下 `{app_id}_si.sock`。
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn socket_path(app_id: &str) -> std::path::PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR")
        .or_else(|_| std::env::var("TMPDIR"))
        .unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(dir).join(format!("{app_id}_si.sock"))
}

/// argv 编码为字节（`\0` 分隔，UTF-8），供 WM_COPYDATA / socket 传输。
/// 用 NUL 而非 `\n`：NUL 在 Windows/Unix 路径中均非法，无需转义。
pub(crate) fn encode_argv(argv: &[String]) -> Vec<u8> {
    argv.join("\0").into_bytes()
}

/// 字节解码回 argv。空输入 → 空 Vec。
pub(crate) fn decode_argv(bytes: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(bytes);
    if s.is_empty() {
        Vec::new()
    } else {
        s.split('\0').map(|x| x.to_string()).collect()
    }
}

// ── 平台实现分发 ──────────────────────────────────────────────
#[cfg(windows)]
mod win;
#[cfg(not(windows))]
mod unix;

/// 单实例配置:app_id + 二次实例回调。由 `App` 组装,平台 `run` 消费。
pub(crate) struct SingleInstance {
    pub app_id: String,
    pub on_second: Box<dyn FnMut(Vec<String>)>,
}

/// 检测单实例:true=首实例(已持锁),false=已有实例在运行。
pub(crate) fn acquire(app_id: &str) -> bool {
    #[cfg(windows)]
    {
        win::acquire(app_id)
    }
    #[cfg(not(windows))]
    {
        unix::acquire(app_id)
    }
}

/// 二次实例:把 argv 转发给首实例。
pub(crate) fn forward(app_id: &str, argv: &[String]) {
    #[cfg(windows)]
    {
        win::forward(app_id, argv)
    }
    #[cfg(not(windows))]
    {
        unix::forward(app_id, argv)
    }
}

/// 首实例:主窗口就绪后安装监听(收二次实例 argv → on_second + 激活主窗口)。
pub(crate) fn install_listener(
    app_id: &str,
    main_hwnd: isize,
    on_second: Box<dyn FnMut(Vec<String>)>,
) {
    #[cfg(windows)]
    {
        win::install_listener(app_id, main_hwnd, on_second)
    }
    #[cfg(not(windows))]
    {
        unix::install_listener(app_id, main_hwnd, on_second)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn argv_round_trip() {
        let argv = vec![
            "wind_setting.exe".to_string(),
            "--page".to_string(),
            "input".to_string(),
        ];
        let bytes = encode_argv(&argv);
        assert_eq!(decode_argv(&bytes), argv);
    }

    #[test]
    fn decode_empty() {
        assert!(decode_argv(&[]).is_empty());
    }

    #[test]
    fn argv_with_protocol_url() {
        let argv = vec![
            "exe".to_string(),
            "windinput://import/theme?url=https://x/a.yaml".to_string(),
        ];
        assert_eq!(decode_argv(&encode_argv(&argv)), argv);
    }

    #[test]
    fn naming_includes_app_id() {
        assert_eq!(mutex_name("wind_setting_dev"), r"Local\wind_setting_dev_si_mutex");
        assert_eq!(class_name("wind_setting_dev"), "wind_setting_dev_si_win");
        assert!(socket_path("wind_setting_dev").to_string_lossy().ends_with("wind_setting_dev_si.sock"));
    }
}
