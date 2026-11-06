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
//!
//! # 点击回调
//!
//! 每条通知都带一个 `default` action（规范里「点击通知本体」的约定键）。另起**一条监听线程**，
//! 用独立连接订阅 `ActionInvoked` / `NotificationClosed`，与第一条通知同时才起：
//! `ActionInvoked(id, "default")` 经通知 id → 标签表还原出标签，交 `on_click`
//! （由托盘宿主转成事件回 UI 线程）；`NotificationClosed(id, _)` 把该 id 从表里删掉。
//! 表只留最近 [`MAX_TAGS`] 条，守护进程从不发 `NotificationClosed` 时也不会无限增长。
//! 别的应用的通知 id 不在表里，信号被忽略。守护进程不存在或不支持 actions 时，
//! 信号根本不会来：点击回调不触发，也不报错。
//!
//! 监听线程连不上总线 / 建不起订阅时直接退出；Notifier 看到它已结束，会在**下一次发通知时**
//! 重新起一条重试（总线稍后才可达的场景，如会话启动早期）。
//!
//! 已知的良性竞态：订阅是异步建立的，订阅生效前到达的信号收不到；通知 id 在发送线程把它
//! 写入标签表之前到达的 `ActionInvoked` 也会被当作「别的应用的通知」忽略。两者都要求用户
//! 在通知刚弹出的毫秒内点击，真人速度不可达。
//!
//! 监听线程阻塞在信号迭代器上，Notifier 被丢掉时由 [`Listener`] 关闭它的连接把它唤醒退出
//! （与托盘监视线程同一约定）。

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};

use zbus::blocking::{Connection, MessageIterator};
use zbus::zvariant::Value;
use zbus::MatchRule;

use super::tray_sni::Bus;

const NOTIFICATIONS_IFACE: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
/// 通知 id → 标签表最多保留的条数。
const MAX_TAGS: usize = 64;

/// 用户点击通知时的回调（在监听线程上调用，只应入队事件并唤醒 UI 线程）。
pub(super) type ClickFn = Arc<dyn Fn(Option<String>) + Send + Sync>;

/// 一条待发送的通知。
struct Job {
    title: String,
    body: String,
    tag: Option<String>,
}

/// 通知 id → 标签。只留最近 [`MAX_TAGS`] 条（先进先出淘汰）。
#[derive(Default)]
struct TagTable {
    entries: VecDeque<(u32, Option<String>)>,
}

impl TagTable {
    fn insert(&mut self, id: u32, tag: Option<String>) {
        // 守护进程可能复用 id：新的顶掉旧的。
        self.entries.retain(|(i, _)| *i != id);
        self.entries.push_back((id, tag));
        while self.entries.len() > MAX_TAGS {
            self.entries.pop_front();
        }
    }

    /// 外层 `None` = 表里没有这个 id；内层是发送时给的标签。
    fn get(&self, id: u32) -> Option<Option<String>> {
        self.entries
            .iter()
            .find(|(i, _)| *i == id)
            .map(|(_, t)| t.clone())
    }

    fn remove(&mut self, id: u32) {
        self.entries.retain(|(i, _)| *i != id);
    }
}

type SharedTags = Arc<Mutex<TagTable>>;

fn lock_tags(t: &SharedTags) -> std::sync::MutexGuard<'_, TagTable> {
    t.lock().unwrap_or_else(|e| e.into_inner())
}

/// 监听线程与 [`Listener`] 交接连接的槽。`stopped` 防「drop 先于监听线程建好连接」的竞态。
#[derive(Default)]
struct ConnSlot {
    stopped: bool,
    conn: Option<Connection>,
}

/// 监听线程的句柄。丢掉它 = 关闭监听连接 = 监听线程退出。
struct Listener {
    slot: Arc<Mutex<ConnSlot>>,
    /// 判断监听线程是否已退出（失败重试），测试里也用来断言线程确实退出。
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Listener {
    fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let conn = {
            let mut s = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            s.stopped = true;
            s.conn.take()
        };
        // 读套接字出错 → zbus 清空订阅 → 迭代器返回 None → 线程退出。
        if let Some(c) = conn {
            let _ = c.close();
        }
    }
}

/// 通知发送端。**只在 UI 线程使用**（不 `Send` 也无妨）。
pub(super) struct Notifier {
    bus: Bus,
    tx: Option<Sender<Job>>,
    on_click: ClickFn,
    tags: SharedTags,
    listener: Option<Listener>,
}

impl Notifier {
    pub(super) fn new(bus: Bus, on_click: ClickFn) -> Self {
        Self {
            bus,
            tx: None,
            on_click,
            tags: SharedTags::default(),
            listener: None,
        }
    }

    /// 入队一条通知（立即返回）。`tag` 在用户点击该通知时交给 `on_click`。
    pub(super) fn send(&mut self, title: &str, body: &str, tag: Option<&str>) {
        // 监听线程没起来或已因总线不可达退出：丢掉旧句柄，趁这次发送重试。
        if self.listener.as_ref().is_none_or(|l| l.is_finished()) {
            self.listener = self.spawn_listener();
        }
        let job = Job {
            title: title.to_string(),
            body: body.to_string(),
            tag: tag.map(str::to_string),
        };
        // 线程只会在发送端被丢掉时退出，所以走到发送失败说明它意外没了：重起一次。
        if let Some(tx) = &self.tx {
            if tx.send(job).is_ok() {
                return;
            }
            self.tx = None;
            return self.send(title, body, tag);
        }
        let (tx, rx) = channel::<Job>();
        let bus = self.bus.clone();
        let tags = self.tags.clone();
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
                    match call_notify(c, &job) {
                        Ok(id) => {
                            warned = false;
                            lock_tags(&tags).insert(id, job.tag);
                        }
                        Err(e) => {
                            warn_once(&mut warned, &format!("发送失败：{e}"));
                            // 连接可能已断：下一条重新连。
                            conn = None;
                        }
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

    /// 起监听线程。失败只记警告：通知照发，只是点击不回调。
    fn spawn_listener(&self) -> Option<Listener> {
        let slot = Arc::new(Mutex::new(ConnSlot::default()));
        let (bus, tags, on_click, s2) = (
            self.bus.clone(),
            self.tags.clone(),
            self.on_click.clone(),
            slot.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("windui-notify-listen".into())
            .spawn(move || listen(bus, tags, on_click, s2));
        match spawned {
            Ok(thread) => Some(Listener {
                slot,
                thread: Some(thread),
            }),
            Err(e) => {
                log::warn!("通知点击监听线程创建失败，点击通知将不回调：{e}");
                None
            }
        }
    }
}

#[cfg(test)]
impl Notifier {
    pub(super) fn has_tag(&self, id: u32) -> bool {
        lock_tags(&self.tags).get(id).is_some()
    }
    pub(super) fn listener_finished(&self) -> bool {
        self.listener.as_ref().is_some_and(|l| l.is_finished())
    }
    pub(super) fn take_listener(&mut self) -> Option<TestListener> {
        self.listener.take().map(TestListener)
    }
}

/// 测试用：取出监听线程句柄后再丢掉 `Listener`，以便断言线程退出。
#[cfg(test)]
pub(super) struct TestListener(Listener);

#[cfg(test)]
impl TestListener {
    pub(super) fn take_thread(&mut self) -> std::thread::JoinHandle<()> {
        self.0.thread.take().expect("线程句柄")
    }
}

/// 监听线程主体：订阅通知守护进程的两个信号，直到连接被关。
/// 连不上总线 / 建不起订阅就直接退出（发送线程会报同一个错，这里不重复），
/// 由 [`Notifier::send`] 在下次发通知时重试。
fn listen(bus: Bus, tags: SharedTags, on_click: ClickFn, slot: Arc<Mutex<ConnSlot>>) {
    let Ok(conn) = bus.connect() else { return };
    {
        let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
        if s.stopped {
            return;
        }
        s.conn = Some(conn.clone());
    }
    let Ok(rule) = (|| {
        Ok::<_, zbus::Error>(
            MatchRule::builder()
                .msg_type(zbus::message::Type::Signal)
                .interface(NOTIFICATIONS_IFACE)?
                .path(NOTIFICATIONS_PATH)?
                .build(),
        )
    })() else {
        return;
    };
    let Ok(messages) = MessageIterator::for_match_rule(rule, &conn, None) else {
        return;
    };
    for msg in messages {
        let Ok(msg) = msg else { continue };
        let header = msg.header();
        match header.member().map(|m| m.as_str()) {
            Some("ActionInvoked") => {
                let Ok((id, key)) = msg.body().deserialize::<(u32, String)>() else {
                    continue;
                };
                if key != "default" {
                    continue;
                }
                // 先放锁再回调：回调里不该持有表锁。
                let tag = lock_tags(&tags).get(id);
                if let Some(tag) = tag {
                    on_click(tag);
                }
            }
            Some("NotificationClosed") => {
                if let Ok((id, _reason)) = msg.body().deserialize::<(u32, u32)>() {
                    lock_tags(&tags).remove(id);
                }
            }
            _ => {}
        }
    }
}

fn warn_once(warned: &mut bool, msg: &str) {
    if !*warned {
        *warned = true;
        eprintln!("[windui] 桌面通知不可用，已忽略（{msg}）");
    }
}

/// 发一条通知，返回守护进程分配的通知 id。
fn call_notify(conn: &Connection, job: &Job) -> zbus::Result<u32> {
    let hints: HashMap<&str, Value<'_>> = HashMap::new();
    // `default` = 点击通知本体；标签为空（动作名之后跟显示文字，留空免得守护进程多画按钮）。
    let actions: Vec<&str> = vec!["default", ""];
    let reply = conn.call_method(
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
    reply.body().deserialize::<u32>()
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

    #[test]
    fn tag_table_keeps_only_the_latest_entries() {
        let mut t = TagTable::default();
        for id in 1..=(MAX_TAGS as u32 + 10) {
            t.insert(id, Some(format!("c{id}")));
        }
        assert_eq!(t.entries.len(), MAX_TAGS, "超过上限后先进先出淘汰");
        assert_eq!(t.get(10), None, "最旧的已被淘汰");
        assert_eq!(t.get(11), Some(Some("c11".to_string())));
        assert_eq!(
            t.get(MAX_TAGS as u32 + 10),
            Some(Some(format!("c{}", MAX_TAGS + 10)))
        );
    }

    #[test]
    fn tag_table_replaces_reused_id_and_removes() {
        let mut t = TagTable::default();
        t.insert(7, Some("a".into()));
        t.insert(7, None);
        assert_eq!(t.entries.len(), 1);
        assert_eq!(
            t.get(7),
            Some(None),
            "无标签的通知也占位，点击回调收到 None"
        );
        t.remove(7);
        assert_eq!(t.get(7), None);
    }
}
