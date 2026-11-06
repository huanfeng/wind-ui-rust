//! 桌面通知：`org.freedesktop.Notifications.Notify`（会话总线）。
//!
//! # 线程与空闲开销
//!
//! `Notify` 是对通知守护进程的一次同步往返，守护进程缺席时 D-Bus 还可能先尝试服务激活，
//! 最坏要等几十秒——绝不能在 UI 线程上做。所以这里起**一条专职线程**，用
//! `std::sync::mpsc` 阻塞在 `recv()` 上：没有通知时线程睡在内核里，零 CPU、零唤醒；
//! UI 线程只做「入队」。线程在**第一条**通知到来时才起，从不发通知的应用一个线程都不多。
//!
//! # 降级
//!
//! 连不上会话总线、守护进程不存在、调用出错，一律只记**一次**警告并丢掉这条通知，
//! 线程继续活着等下一条（总线 / 守护进程可能稍后才起来，下条通知重新连）。永不 panic，
//! 永不阻塞 UI 线程。

use std::collections::HashMap;
use std::sync::mpsc::{channel, Sender};

use zbus::blocking::Connection;
use zbus::zvariant::Value;

use super::tray_sni::Bus;

/// 一条待发送的通知。
struct Job {
    title: String,
    body: String,
}

/// 通知发送端。**只在 UI 线程使用**（不 `Send` 也无妨）。
pub(super) struct Notifier {
    bus: Bus,
    tx: Option<Sender<Job>>,
}

impl Notifier {
    pub(super) fn new(bus: Bus) -> Self {
        Self { bus, tx: None }
    }

    /// 入队一条通知（立即返回）。
    pub(super) fn send(&mut self, title: &str, body: &str) {
        let job = Job {
            title: title.to_string(),
            body: body.to_string(),
        };
        // 线程只会在发送端被丢掉时退出，所以走到发送失败说明它意外没了：重起一次。
        if let Some(tx) = &self.tx {
            if tx.send(job).is_ok() {
                return;
            }
            self.tx = None;
            return self.send(title, body);
        }
        let (tx, rx) = channel::<Job>();
        let bus = self.bus.clone();
        let spawned = std::thread::Builder::new()
            .name("windui-notify".into())
            .spawn(move || {
                let mut conn: Option<Connection> = None;
                let mut warned = false;
                for job in rx {
                    if conn.is_none() {
                        match bus.connect() {
                            Ok(c) => conn = Some(c),
                            Err(e) => {
                                warn_once(&mut warned, &format!("连不上会话总线：{e}"));
                                continue;
                            }
                        }
                    }
                    let Some(c) = conn.as_ref() else { continue };
                    if let Err(e) = call_notify(c, &job) {
                        warn_once(&mut warned, &format!("发送失败：{e}"));
                        // 连接可能已断：下一条重新连。
                        conn = None;
                    } else {
                        warned = false;
                    }
                }
            });
        match spawned {
            Ok(_) => {
                let _ = tx.send(job);
                self.tx = Some(tx);
            }
            Err(e) => log::warn!("桌面通知线程创建失败，通知被丢弃：{e}"),
        }
    }
}

fn warn_once(warned: &mut bool, msg: &str) {
    if !*warned {
        *warned = true;
        eprintln!("[windui] 桌面通知不可用，已忽略（{msg}）");
    }
}

fn call_notify(conn: &Connection, job: &Job) -> zbus::Result<()> {
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    let actions: Vec<&str> = Vec::new();
    conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            app_name(),
            0u32,
            "",
            job.title.as_str(),
            escape_markup(&job.body),
            actions,
            hints,
            -1i32,
        ),
    )?;
    Ok(())
}

/// 应用名：取可执行文件名（通知中心按它分组），取不到退回 `windui`。
pub(super) fn app_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "windui".into())
}

/// 通知正文按规范可含 `<b>` 等简单标记，主流守护进程（GNOME / KDE / dunst）都会解析：
/// 原样传入的 `<`、`&` 会被吞掉或显示错乱，所以转义。
fn escape_markup(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_markup_is_escaped() {
        assert_eq!(escape_markup("a<b>&c"), "a&lt;b&gt;&amp;c");
        assert_eq!(escape_markup("纯文本"), "纯文本");
    }
}
