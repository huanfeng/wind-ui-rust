//! Linux 平台后端：X11（经 x11rb）与原生 Wayland（经 wayland-client，`wayland` feature）。
//!
//! 运行期选后端，见 [`choose_backend`]。Wayland 后端尚在分阶段落地（见仓库根
//! `IMPLEMENTATION_PLAN.md`），目前只有窗口与呈现。
//!
//! 对外暴露与 `win32` / `macos` 同形的 API：`run` / `open_url` / `clipboard::X11Clipboard` /
//! `drag_files` / `system_prefers_dark` / `system_locales`。上层只依赖 `crate::platform::*`。
//!
//! 模块划分：
//! - `x11`：窗口、事件循环、呈现（`PutImage`）、窗口操作（EWMH）、无边框拖动。
//! - `wayland`：Wayland 窗口、事件循环、`wl_shm` 呈现、`frame` 回调配速。
//! - `host`：与显示协议无关的宿主簿记（点击计数、定时器、帧配速、出帧、事件后意图）。
//! - `ime`：XIM 输入法客户端（合成串回调 → 宿主内联绘制，候选窗跟随光标）。
//! - `keys`：键码 → keysym → 框架键。
//! - `hotkey`：全局热键（根窗口 `GrabKey`）。
//! - `dnd`：文件拖入（XDND 目标端）。
//! - `clipboard`：独立线程专职拥有 / 读取 `CLIPBOARD` 选区。
//! - `sys`：`poll(2)` 与跨线程唤醒管道。
//! - 文字渲染见 `crate::text::linux`。
//!
//! 尚未实现（调用处均为空操作并记日志，API 形状与其它平台一致，下游不必分支）：
//! 系统托盘、文件拖出、零窗口常驻模式、GPU 后端。
//! 现状与路线见 `docs/LINUX_PORTING.md`。

pub mod clipboard;
mod dnd;
mod host;
mod hotkey;
mod ime;
mod keys;
pub(crate) mod sys;
#[cfg(feature = "wayland")]
mod wayland;
mod x11;

use super::{AppHandler, WindowConfig};

/// 运行应用：截屏模式离屏渲染存盘；否则按 [`choose_backend`] 连接合成器 / X 服务器，
/// 创建窗口进入事件循环。
pub(crate) fn run(
    cfg: WindowConfig,
    mut handler: Box<dyn AppHandler>,
    waker: Option<std::sync::Arc<crate::sync::WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
) {
    let os_default = true;
    crate::anim::set_enabled(cfg.animations.unwrap_or(os_default));
    crate::ui::caret::set_blink_period_ms(match cfg.animations {
        Some(false) => None,
        _ => Some(crate::ui::caret::BLINK_HALF_MS as u32),
    });

    if let Some(path) = cfg.screenshot.clone() {
        super::run_offscreen(&cfg, &mut handler, &path);
        return;
    }
    // 与 macOS 同理：常驻模式尚未落地时明确拒绝，而不是退化成开一个窗口。
    if cfg.resident {
        eprintln!(
            "[windui] 常驻模式（App::run_resident）目前仅支持 Windows，Linux 尚未实现，进程退出"
        );
        return;
    }
    if let Some(si) = &single {
        if !crate::single_instance::arbitrate(&si.app_id) {
            return;
        }
    }
    let forced = std::env::var("WINDUI_BACKEND").ok();
    let has_wayland = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
    let (choice, note) = choose_backend(forced.as_deref(), has_wayland, cfg!(feature = "wayland"));
    match note {
        Some(Note::Unknown) => log::warn!(
            "WINDUI_BACKEND={:?} 无法识别（可选 x11 / wayland），按自动选择处理",
            forced.unwrap_or_default()
        ),
        Some(Note::NotCompiled) => eprintln!(
            "[windui] WINDUI_BACKEND=wayland，但本次构建未启用 `wayland` feature，改用 X11"
        ),
        None => {}
    }
    if let Choice::Wayland { fallback } = choice {
        #[cfg(feature = "wayland")]
        match wayland::connect() {
            Ok(session) => return wayland::run_windowed(session, cfg, handler, waker, single),
            Err(e) if fallback => log::warn!("Wayland 不可用（{e}），回退 X11"),
            Err(e) => {
                eprintln!("[windui] WINDUI_BACKEND=wayland，但 Wayland 不可用：{e}");
                return;
            }
        }
        #[cfg(not(feature = "wayland"))]
        let _ = fallback;
    }
    x11::run_windowed(cfg, handler, waker, single);
}

/// 运行期选定的显示后端。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Choice {
    X11,
    /// `fallback`：连不上时能否回退 X11（自动选择时能；`WINDUI_BACKEND=wayland` 强制时不能——
    /// 用户点名要 Wayland，悄悄换成 X11 只会让「为什么还是 XWayland」无从查起）。
    Wayland {
        fallback: bool,
    },
}

/// 选后端时要告诉用户的事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Note {
    /// `WINDUI_BACKEND` 的值认不出，按自动选择处理。
    Unknown,
    /// 点名要 Wayland，但编译时关了 `wayland` feature。
    NotCompiled,
}

/// 选后端：`WINDUI_BACKEND=x11|wayland`（不分大小写）强制；否则会话里有 Wayland
/// （`WAYLAND_DISPLAY` / `WAYLAND_SOCKET`）且编进了 Wayland 后端就走 Wayland、
/// 连不上回退 X11，其余走 X11。
fn choose_backend(
    forced: Option<&str>,
    has_wayland: bool,
    compiled: bool,
) -> (Choice, Option<Note>) {
    let auto = if has_wayland && compiled {
        Choice::Wayland { fallback: true }
    } else {
        Choice::X11
    };
    match forced.map(str::trim).filter(|v| !v.is_empty()) {
        None => (auto, None),
        Some(v) if v.eq_ignore_ascii_case("x11") => (Choice::X11, None),
        Some(v) if v.eq_ignore_ascii_case("wayland") => {
            if compiled {
                (Choice::Wayland { fallback: false }, None)
            } else {
                (Choice::X11, Some(Note::NotCompiled))
            }
        }
        Some(_) => (auto, Some(Note::Unknown)),
    }
}

/// 系统是否偏好暗色外观。
///
/// 依次看：GTK 主题名（`GTK_THEME` 环境变量带 `:dark` 或以 `-dark` 结尾），GNOME 的
/// `color-scheme` 设置（经 `gsettings` 读，桌面没装它就跳过）。都读不到按亮色——理由同
/// win32 侧：那是出厂默认，暗色界面配亮色系统更容易被当成程序坏了。
pub(crate) fn system_prefers_dark() -> bool {
    if let Ok(t) = std::env::var("GTK_THEME") {
        let t = t.to_ascii_lowercase();
        if t.ends_with(":dark") || t.ends_with("-dark") {
            return true;
        }
    }
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "color-scheme"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains("dark"))
}

/// 用户偏好的界面语言（BCP-47，按优先级）。
///
/// `LANGUAGE` 是 GNU gettext 的有序偏好表（`zh_CN:en_US`），优先；其次
/// `LC_ALL` / `LC_MESSAGES` / `LANG` 里的单个 locale。`C` / `POSIX` 视为没有偏好。
pub fn system_locales() -> Vec<String> {
    locales_from(|k| std::env::var(k).ok())
}

fn locales_from(get: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut push = |raw: &str| {
        if let Some(tag) = posix_to_bcp47(raw) {
            if !out.contains(&tag) {
                out.push(tag);
            }
        }
    };
    if let Some(list) = get("LANGUAGE").filter(|v| !v.is_empty()) {
        for item in list.split(':') {
            push(item);
        }
    }
    for k in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(v) = get(k).filter(|v| !v.is_empty()) {
            push(&v);
            break;
        }
    }
    out
}

/// `zh_CN.UTF-8@xxx` → `zh-CN`；`C` / `POSIX` → `None`。
fn posix_to_bcp47(s: &str) -> Option<String> {
    let base = s.split(['.', '@']).next()?.trim();
    if base.is_empty() || base == "C" || base == "POSIX" {
        return None;
    }
    Some(base.replace('_', "-"))
}

/// 用系统默认程序打开 URL/路径（`xdg-open`，freedesktop 各桌面通用）。不等待其退出。
pub fn open_url(url: &str) {
    let spawned = std::process::Command::new("xdg-open")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    match spawned {
        // 另起线程回收子进程，免得留下僵尸进程。
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => log::warn!("xdg-open 启动失败（{url}）：{e}"),
    }
}

/// 拖出到外部程序。Linux 侧（XDND 源端）尚未接入：返回 `None`，语义同 macOS 侧。
pub fn drag_files(
    _paths: &[impl AsRef<std::path::Path>],
    _allow_move: bool,
) -> crate::platform::DragEffect {
    crate::platform::DragEffect::None
}

/// 见 `win32::single_window_open`。Linux 的窗口登记表还没接常驻模式，恒为 `false`
/// （取值理由同 macOS 侧）。
pub(crate) fn single_window_open(_key: &str) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_prefers_wayland_session_and_may_fall_back() {
        let wl = Choice::Wayland { fallback: true };
        assert_eq!(choose_backend(None, true, true), (wl, None));
        assert_eq!(
            choose_backend(Some(""), true, true),
            (wl, None),
            "空值视同未设"
        );
        assert_eq!(choose_backend(None, false, true), (Choice::X11, None));
        assert_eq!(
            choose_backend(None, true, false),
            (Choice::X11, None),
            "没编进 Wayland 后端：Wayland 会话照样走 XWayland"
        );
    }

    #[test]
    fn forced_backend_wins_and_forced_wayland_does_not_fall_back() {
        assert_eq!(choose_backend(Some("x11"), true, true), (Choice::X11, None));
        assert_eq!(
            choose_backend(Some("Wayland"), false, true),
            (Choice::Wayland { fallback: false }, None)
        );
        assert_eq!(
            choose_backend(Some("wayland"), true, false),
            (Choice::X11, Some(Note::NotCompiled))
        );
    }

    #[test]
    fn unknown_forced_value_behaves_like_auto_with_a_note() {
        assert_eq!(
            choose_backend(Some("mir"), true, true),
            (Choice::Wayland { fallback: true }, Some(Note::Unknown))
        );
        assert_eq!(
            choose_backend(Some("mir"), false, true),
            (Choice::X11, Some(Note::Unknown))
        );
    }

    #[test]
    fn posix_locales_become_bcp47() {
        assert_eq!(posix_to_bcp47("zh_CN.UTF-8"), Some("zh-CN".into()));
        assert_eq!(posix_to_bcp47("en_US@euro"), Some("en-US".into()));
        assert_eq!(posix_to_bcp47("C"), None);
        assert_eq!(posix_to_bcp47("C.UTF-8"), None);
    }

    #[test]
    fn language_list_takes_priority_and_dedups() {
        let env = |k: &str| match k {
            "LANGUAGE" => Some("zh_CN:en_US".to_string()),
            "LANG" => Some("en_US.UTF-8".to_string()),
            _ => None,
        };
        assert_eq!(locales_from(env), vec!["zh-CN", "en-US"]);
    }

    #[test]
    fn falls_back_to_lang() {
        let env = |k: &str| (k == "LANG").then(|| "ja_JP.UTF-8".to_string());
        assert_eq!(locales_from(env), vec!["ja-JP"]);
    }
}
