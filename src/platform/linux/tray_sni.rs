//! Linux 系统托盘：StatusNotifierItem（图标）+ `com.canonical.dbusmenu`（右键菜单）。
//!
//! KDE Plasma 原生支持；GNOME 需要 AppIndicator 扩展；没有 `org.kde.StatusNotifierWatcher`
//! 的桌面（未装扩展的 GNOME、无头机器）上本模块**优雅降级**：不显示托盘、记一次警告，
//! 应用照常运行，通知（[`super::notify`]）不受影响。
//!
//! # 与事件循环的集成（空闲零 CPU 的依据）
//!
//! 全程**事件驱动，没有任何轮询**：
//!
//! - **D-Bus → UI**：zbus（`async-io`）自带一条内部执行器线程收发消息，托盘 / 菜单的方法
//!   调用（`Activate`、`Event`……）在那条线程上执行。处理函数只做两件事：把事件塞进
//!   [`Shared::events`]，再调 [`super::sys::wake`] 写唤醒管道——这根管道本来就在事件循环的
//!   `poll(2)` 集合里，所以 UI 线程是被内核唤醒的，不是自己醒来查的。回调（`!Send`，且读
//!   `Signal`）仍只在 UI 线程执行，遵守 AGENTS.md 铁律 6。
//! - **UI → D-Bus**：改提示 / 换图标 / 菜单变化只更新共享快照，再经 `mpsc` 通知专职线程发
//!   `NewToolTip` 等信号；线程阻塞在 `recv()` 上。UI 线程从不做 D-Bus 往返。
//! - 连总线、挂接口、占名、向 Watcher 登记（对别的进程的同步调用）全在专职线程上做，UI 线程
//!   启动时只建一份共享状态，不做任何 D-Bus 往返。
//! - **Watcher 可能晚到或重启**（会话早期启动的应用、shell 重启、扩展被重新启用）：另有一条
//!   监视线程订阅 `NameOwnerChanged`（`arg0 = org.kde.StatusNotifierWatcher`），Watcher 上线
//!   （含启动时已在场）就通知专职线程重新登记；可用性翻转经 [`TrayEvent::Availability`] 回到
//!   UI 线程，触发 [`Tray::on_availability_changed`] 回调。监视线程阻塞在信号迭代器上，托盘释放时由 `SniLink` 关其连接把它唤醒，
//!   同样零轮询。
//!
//! 菜单标签 / 勾选态 / 禁用态绑的是线程局部的 `Signal`，D-Bus 线程读不了，所以 UI 线程在每轮
//! 事件循环末尾（[`TrayHost::refresh_menu`]）把它们**快照**进共享状态，快照变了才发
//! `LayoutUpdated`；D-Bus 线程只读快照。

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};

use zbus::blocking::fdo::DBusProxy;
use zbus::blocking::{Connection, ConnectionBuilder};
use zbus::zvariant::{Array, Dict, ObjectPath, OwnedValue, StructureBuilder, Value};

use super::notify::Notifier;
use crate::platform::tray::{invoke, ItemKind, Tray, TrayAction, TrayMenuItem, TrayOp};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const ITEM_PATH: &str = "/StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
/// 没给自定义图标时用的主题图标名。
const FALLBACK_ICON_NAME: &str = "application-x-executable";

/// 连哪条总线。生产恒为会话总线；测试指向私有 `dbus-daemon`（不碰进程环境变量）。
#[derive(Clone, Debug)]
pub(crate) enum Bus {
    Session,
    #[cfg_attr(not(test), allow(dead_code))]
    Address(String),
}

impl Bus {
    pub(crate) fn connect(&self) -> zbus::Result<Connection> {
        match self {
            Bus::Session => Connection::session(),
            Bus::Address(a) => ConnectionBuilder::address(a.as_str())?.build(),
        }
    }
}

/// 总线上是否有 StatusNotifierWatcher。连不上总线也算没有。
fn watcher_present(conn: &Connection) -> bool {
    conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &(WATCHER_NAME,),
    )
    .ok()
    .and_then(|m| m.body().deserialize::<bool>().ok())
    .unwrap_or(false)
}

/// 指定总线上托盘是否可用（有 Watcher）。
pub(crate) fn supported_on(bus: &Bus) -> bool {
    bus.connect().is_ok_and(|c| watcher_present(&c))
}

// ── 纯函数：数据格式转换 ────────────────────────────────────────────────────

/// 非预乘 RGBA8 → SNI `IconPixmap` 的 ARGB32（网络字节序，即每像素字节依次为 A、R、G、B）。
pub(crate) fn rgba_to_sni_argb(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.as_chunks::<4>().0 {
        out.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
    }
    out
}

/// 菜单项快照（UI 线程从 `TrayMenuItem` 解析出来，D-Bus 线程只读它）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MenuEntry {
    pub label: String,
    pub separator: bool,
    /// `None` = 不是勾选项。
    pub checked: Option<bool>,
    pub enabled: bool,
}

/// 此刻的菜单快照。标签 / 勾选态 / 禁用态在这里现取。
pub(crate) fn menu_entries(items: &[TrayMenuItem]) -> Vec<MenuEntry> {
    items
        .iter()
        .map(|it| match &it.kind {
            ItemKind::Separator => MenuEntry {
                label: String::new(),
                separator: true,
                checked: None,
                enabled: true,
            },
            ItemKind::Action {
                label,
                checked,
                enabled,
                ..
            } => MenuEntry {
                label: label.resolve().into_owned(),
                separator: false,
                checked: checked.map(|c| c.get()),
                enabled: enabled.map(|e| e.get()).unwrap_or(true),
            },
        })
        .collect()
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Prop {
    Str(String),
    Bool(bool),
    Int(i32),
}

/// dbusmenu 布局节点（纯数据，便于单测；上线路前再转 zvariant）。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LayoutNode {
    pub id: i32,
    pub props: BTreeMap<String, Prop>,
    pub children: Vec<LayoutNode>,
}

/// dbusmenu 里 `_` 是助记符前缀，字面下划线要写成 `__`。
fn escape_mnemonic(s: &str) -> String {
    s.replace('_', "__")
}

fn entry_node(id: i32, e: &MenuEntry) -> LayoutNode {
    let mut props = BTreeMap::new();
    if e.separator {
        props.insert("type".into(), Prop::Str("separator".into()));
    } else {
        props.insert("label".into(), Prop::Str(escape_mnemonic(&e.label)));
        props.insert("enabled".into(), Prop::Bool(e.enabled));
        if let Some(c) = e.checked {
            props.insert("toggle-type".into(), Prop::Str("checkmark".into()));
            props.insert("toggle-state".into(), Prop::Int(c as i32));
        }
    }
    LayoutNode {
        id,
        props,
        children: Vec::new(),
    }
}

/// 由菜单快照生成布局：根 id 0，子项 id 为 1..=n（与 `Tray::menu` 的下标 + 1 对应）。
pub(crate) fn build_layout(entries: &[MenuEntry]) -> LayoutNode {
    let mut props = BTreeMap::new();
    props.insert("children-display".into(), Prop::Str("submenu".into()));
    LayoutNode {
        id: 0,
        props,
        children: entries
            .iter()
            .enumerate()
            .map(|(i, e)| entry_node(i as i32 + 1, e))
            .collect(),
    }
}

fn prop_value(p: &Prop) -> Value<'static> {
    match p {
        Prop::Str(s) => Value::from(s.clone()),
        Prop::Bool(b) => Value::from(*b),
        Prop::Int(i) => Value::from(*i),
    }
}

fn props_dict(props: &BTreeMap<String, Prop>) -> Dict<'static, 'static> {
    let map: HashMap<String, Value<'static>> = props
        .iter()
        .map(|(k, v)| (k.clone(), prop_value(v)))
        .collect();
    Dict::from(map)
}

fn node_value(n: &LayoutNode) -> Value<'static> {
    let children: Vec<Value<'static>> = n.children.iter().map(node_value).collect();
    Value::Structure(
        StructureBuilder::new()
            .add_field(n.id)
            .append_field(Value::Dict(props_dict(&n.props)))
            .append_field(Value::Array(Array::from(children)))
            .build(),
    )
}

type WireLayout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

fn owned(v: Value<'static>) -> OwnedValue {
    // 不含 fd 的值转换不会失败。
    OwnedValue::try_from(v).expect("不含 fd 的 Value 转 OwnedValue")
}

fn wire_layout(n: &LayoutNode) -> WireLayout {
    (
        n.id,
        n.props
            .iter()
            .map(|(k, v)| (k.clone(), owned(prop_value(v))))
            .collect(),
        n.children.iter().map(|c| owned(node_value(c))).collect(),
    )
}

// ── 共享状态与事件 ──────────────────────────────────────────────────────────

/// D-Bus 线程产出、UI 线程消费的托盘事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TrayEvent {
    /// 左键单击（SNI `Activate`）。
    Left,
    /// 菜单项被点（下标对应 `Tray::menu` 的顺序）。
    Menu(usize),
    /// 托盘可用性翻转（Watcher 上线 / 下线）。
    Availability(bool),
    /// 用户点了通知（带发送时给的标签）。
    Notification(Option<String>),
}

type Pixmap = Vec<(i32, i32, Vec<u8>)>;

struct SniState {
    tooltip: String,
    icon: Pixmap,
    menu: Vec<MenuEntry>,
    revision: u32,
}

struct Shared {
    events: Mutex<VecDeque<TrayEvent>>,
    state: Mutex<SniState>,
}

impl Shared {
    fn push(&self, ev: TrayEvent) {
        if let Ok(mut q) = self.events.lock() {
            q.push_back(ev);
        }
        // 唤醒事件循环：它睡在 poll(2) 上，这是它被叫醒的唯一途径。
        super::sys::wake();
    }
}

// ── D-Bus 接口 ──────────────────────────────────────────────────────────────

struct Item {
    shared: Arc<Shared>,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    fn activate(&self, _x: i32, _y: i32) {
        self.shared.push(TrayEvent::Left);
    }
    // 中键 / 右键：宿主按 `Menu` 属性自己弹菜单，无需我们响应。
    fn secondary_activate(&self, _x: i32, _y: i32) {}
    fn context_menu(&self, _x: i32, _y: i32) {}
    fn scroll(&self, _delta: i32, _orientation: String) {}

    #[zbus(property)]
    fn category(&self) -> String {
        "ApplicationStatus".into()
    }
    #[zbus(property)]
    fn id(&self) -> String {
        super::notify::app_name()
    }
    #[zbus(property)]
    fn title(&self) -> String {
        self.shared
            .state
            .lock()
            .map(|s| s.tooltip.clone())
            .unwrap_or_default()
    }
    #[zbus(property)]
    fn status(&self) -> String {
        "Active".into()
    }
    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }
    #[zbus(property)]
    fn icon_name(&self) -> String {
        let has_pixmap = self
            .shared
            .state
            .lock()
            .map(|s| !s.icon.is_empty())
            .unwrap_or(false);
        if has_pixmap {
            String::new()
        } else {
            FALLBACK_ICON_NAME.into()
        }
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmap {
        self.shared
            .state
            .lock()
            .map(|s| s.icon.clone())
            .unwrap_or_default()
    }
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Pixmap, String, String) {
        let tip = self
            .shared
            .state
            .lock()
            .map(|s| s.tooltip.clone())
            .unwrap_or_default();
        (String::new(), Vec::new(), tip, String::new())
    }
    /// 左键走 `Activate`，不让宿主把左键也当成「弹菜单」。
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::from_static_str_unchecked(MENU_PATH)
    }
}

struct Menu {
    shared: Arc<Shared>,
}

impl Menu {
    /// 取快照构建布局；`parent_id` 非 0 时返回那一项自己（本菜单只有一层）。
    fn layout_of(&self, parent_id: i32) -> Option<LayoutNode> {
        let entries = self.shared.state.lock().ok()?.menu.clone();
        let root = build_layout(&entries);
        if parent_id == 0 {
            Some(root)
        } else {
            root.children.into_iter().find(|c| c.id == parent_id)
        }
    }

    fn click(&self, id: i32) {
        let Some(idx) = (id as usize).checked_sub(1) else {
            return;
        };
        let ok = self
            .shared
            .state
            .lock()
            .ok()
            .and_then(|s| s.menu.get(idx).map(|e| !e.separator && e.enabled))
            .unwrap_or(false);
        if ok {
            self.shared.push(TrayEvent::Menu(idx));
        }
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    fn get_layout(
        &self,
        parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, WireLayout)> {
        let rev = self.shared.state.lock().map(|s| s.revision).unwrap_or(0);
        let node = self
            .layout_of(parent_id)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("没有菜单项 {parent_id}")))?;
        Ok((rev, wire_layout(&node)))
    }

    fn get_group_properties(
        &self,
        ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        ids.into_iter()
            .filter_map(|id| self.layout_of(id).map(|n| (id, wire_layout(&n).1)))
            .collect()
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        self.layout_of(id)
            .and_then(|n| n.props.get(&name).map(|p| owned(prop_value(p))))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("没有属性 {id}/{name}")))
    }

    fn event(&self, id: i32, event_id: String, _data: Value<'_>, _timestamp: u32) {
        if event_id == "clicked" {
            self.click(id);
        }
    }

    fn event_group(&self, events: Vec<(i32, String, Value<'_>, u32)>) -> Vec<i32> {
        for (id, event_id, _, _) in events {
            if event_id == "clicked" {
                self.click(id);
            }
        }
        Vec::new()
    }

    /// 快照在 UI 线程每轮循环末尾刷新，这里无需再要更新。
    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> String {
        "ltr".into()
    }
    #[zbus(property)]
    fn status(&self) -> String {
        "normal".into()
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }
}

// ── UI 侧句柄 ───────────────────────────────────────────────────────────────

/// 发给专职线程的信号请求。
enum SniCmd {
    NewToolTip,
    NewIcon,
    LayoutUpdated(u32),
    /// Watcher 在场（启动时已在，或新上线 / 换了主人）：（重新）登记。由监视线程发出。
    RegisterWithWatcher,
    /// 托盘被丢掉：专职线程退出。
    Stop,
}

/// 与专职线程的通道。丢掉它 = 专职线程退出、连接关闭、图标从托盘消失。
struct SniLink {
    tx: Sender<SniCmd>,
    /// 监视线程的连接槽：drop 时据此关连接，叫醒阻塞在信号迭代器上的监视线程。
    watch: Arc<Mutex<WatchSlot>>,
    /// 两条线程的句柄，仅测试用来断言它们确实退出。
    #[cfg(test)]
    threads: Vec<std::thread::JoinHandle<()>>,
}

/// 监视线程与 [`SniLink`] 交接连接的槽。`stopped` 防「drop 先于监视线程建好连接」的竞态。
#[derive(Default)]
struct WatchSlot {
    stopped: bool,
    conn: Option<Connection>,
}

impl Drop for SniLink {
    fn drop(&mut self) {
        // 监视线程手里还有一份发送端，光靠「发送端全丢」收不了线程，得显式叫停。
        let _ = self.tx.send(SniCmd::Stop);
        // 监视线程阻塞在信号迭代器上，Watcher 不变化就永远不返回：关掉它的连接，
        // 读套接字出错 → zbus 清空订阅发送端 → 迭代器返回 None → 线程退出。
        let conn = {
            let mut slot = self.watch.lock().unwrap_or_else(|e| e.into_inner());
            slot.stopped = true;
            slot.conn.take()
        };
        if let Some(c) = conn {
            let _ = c.close();
        }
    }
}

/// 托盘在 UI 线程的总管：持有 `Tray` 声明（回调只能在这里跑）、共享状态与通知发送端。
pub(super) struct TrayHost {
    tray: Tray,
    shared: Arc<Shared>,
    link: Option<SniLink>,
    notifier: Notifier,
    /// 本托盘项的总线服务名（何时真正登记取决于 Watcher 在不在场）。仅测试读取。
    #[cfg_attr(not(test), allow(dead_code))]
    service: String,
}

static SEQ: AtomicU32 = AtomicU32::new(1);

impl TrayHost {
    /// 安装托盘。**永不失败，也不做任何 D-Bus 往返**：UI 线程只建共享状态并起线程，
    /// 连总线、占名、等 Watcher 都在后台——总线 / Watcher 不可用或晚到都不影响启动，
    /// Watcher 之后上线图标随即出现（见模块文档）。通知功能与此无关，照常。
    pub(super) fn install(mut tray: Tray, bus: &Bus) -> Self {
        let icon = match tray.icon.take() {
            Some((w, h, rgba))
                if w > 0 && h > 0 && rgba.len() as u64 == w as u64 * h as u64 * 4 =>
            {
                vec![(w as i32, h as i32, rgba_to_sni_argb(&rgba))]
            }
            _ => Vec::new(),
        };
        let shared = Arc::new(Shared {
            events: Mutex::new(VecDeque::new()),
            state: Mutex::new(SniState {
                tooltip: tray.tooltip.clone(),
                icon,
                menu: menu_entries(&tray.items),
                revision: 1,
            }),
        });
        let service = format!(
            "org.kde.StatusNotifierItem-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let link = match spawn_tray_threads(bus, &shared, &service) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("[windui] 托盘线程创建失败，不显示托盘图标：{e}");
                None
            }
        };
        let shared2 = shared.clone();
        Self {
            tray,
            shared,
            link,
            notifier: Notifier::new(
                bus.clone(),
                Arc::new(move |tag| shared2.push(TrayEvent::Notification(tag))),
            ),
            service,
        }
    }

    /// 取走 D-Bus 线程攒下的事件，在 UI 线程跑对应回调，返回它们排的意图。
    pub(super) fn poll_actions(&mut self) -> Vec<TrayAction> {
        let events: Vec<TrayEvent> = match self.shared.events.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => return Vec::new(),
        };
        let mut actions = Vec::new();
        for ev in events {
            match ev {
                TrayEvent::Left => actions.extend(invoke(self.tray.on_left_click.as_mut())),
                TrayEvent::Menu(i) => actions.extend(invoke(self.tray.item_callback(i))),
                TrayEvent::Notification(tag) => {
                    actions.extend(self.tray.notification_click(tag));
                }
                TrayEvent::Availability(up) => {
                    if let Some(cb) = self.tray.on_availability.as_mut() {
                        let mut ctx = crate::platform::tray::TrayCtx::default();
                        cb(&mut ctx, up);
                        actions.extend(ctx.take_actions());
                    }
                }
            }
        }
        actions
    }

    /// 重新快照菜单；与上次不同才通知宿主刷新。每轮事件循环末尾调用，开销是几个
    /// `Signal::get`，无 I/O。
    pub(super) fn refresh_menu(&mut self) {
        let Some(link) = &self.link else { return };
        let entries = menu_entries(&self.tray.items);
        let Ok(mut st) = self.shared.state.lock() else {
            return;
        };
        if st.menu == entries {
            return;
        }
        st.menu = entries;
        st.revision = st.revision.wrapping_add(1);
        let _ = link.tx.send(SniCmd::LayoutUpdated(st.revision));
    }

    pub(super) fn apply_op(&mut self, op: TrayOp) {
        match op {
            TrayOp::SetTooltip(s) => self.set_tooltip(s),
            TrayOp::Notify { title, body, tag } => {
                self.notifier.send(&title, &body, tag.as_deref())
            }
            TrayOp::SetIcon { w, h, rgba } => {
                // 与 install / `TrayHandle::set_icon_rgba` 同口径：对不上的像素数据丢弃。
                if w == 0 || h == 0 || rgba.len() as u64 != w as u64 * h as u64 * 4 {
                    log::warn!(
                        "托盘 SetIcon：{w}×{h} 与像素数据 {} 字节对不上，已忽略",
                        rgba.len()
                    );
                    return;
                }
                if let Ok(mut st) = self.shared.state.lock() {
                    st.icon = vec![(w as i32, h as i32, rgba_to_sni_argb(&rgba))];
                }
                self.signal(SniCmd::NewIcon);
            }
        }
    }

    pub(super) fn notify(&mut self, title: &str, body: &str, tag: Option<&str>) {
        self.notifier.send(title, body, tag);
    }

    fn set_tooltip(&mut self, s: String) {
        if let Ok(mut st) = self.shared.state.lock() {
            st.tooltip = s;
        }
        self.signal(SniCmd::NewToolTip);
    }

    fn signal(&self, cmd: SniCmd) {
        if let Some(l) = &self.link {
            let _ = l.tx.send(cmd);
        }
    }

    /// 本托盘项的总线服务名。
    #[cfg(test)]
    pub(super) fn service_name(&self) -> &str {
        &self.service
    }
}

/// 起专职线程（连总线、挂接口、占名、发信号、向 Watcher 登记）与监视线程（盯 Watcher 上下线）。
fn spawn_tray_threads(bus: &Bus, shared: &Arc<Shared>, service: &str) -> std::io::Result<SniLink> {
    let (tx, rx) = channel::<SniCmd>();
    let watch = Arc::new(Mutex::new(WatchSlot::default()));
    let (b, sh, svc) = (bus.clone(), shared.clone(), service.to_string());
    let tray_handle = std::thread::Builder::new()
        .name("windui-tray".into())
        .spawn(move || tray_thread(b, sh, svc, rx))?;
    let (b, sh, tx2, w) = (bus.clone(), shared.clone(), tx.clone(), watch.clone());
    let watch_handle = std::thread::Builder::new()
        .name("windui-tray-watch".into())
        .spawn(move || watch_watcher(b, sh, tx2, w));
    let watch_handle = match watch_handle {
        Ok(h) => h,
        Err(e) => {
            // 专职线程已起：叫停它，免得空转。
            let _ = tx.send(SniCmd::Stop);
            return Err(e);
        }
    };
    #[cfg(not(test))]
    drop((tray_handle, watch_handle));
    Ok(SniLink {
        tx,
        watch,
        #[cfg(test)]
        threads: vec![tray_handle, watch_handle],
    })
}

/// 专职线程主体：对别的进程的同步调用都放在这条线程上，UI 线程不为它们停顿。
fn tray_thread(bus: Bus, shared: Arc<Shared>, service: String, rx: Receiver<SniCmd>) {
    let conn = match bus.connect() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[windui] 无法接入会话总线，不显示托盘图标：{e}");
            return;
        }
    };
    let setup = (|| -> zbus::Result<()> {
        let os = conn.object_server();
        os.at(
            ITEM_PATH,
            Item {
                shared: shared.clone(),
            },
        )?;
        os.at(MENU_PATH, Menu { shared })?;
        conn.request_name(service.as_str())
    })();
    if let Err(e) = setup {
        eprintln!("[windui] 托盘接口挂接失败，不显示托盘图标：{e}");
        return;
    }
    // 阻塞等请求；收到 `Stop`（托盘被丢掉）或发送端全没了就退出。
    while let Ok(cmd) = rx.recv() {
        let r = match cmd {
            SniCmd::Stop => break,
            SniCmd::RegisterWithWatcher => conn
                .call_method(
                    Some(WATCHER_NAME),
                    WATCHER_PATH,
                    Some(WATCHER_NAME),
                    "RegisterStatusNotifierItem",
                    &(service.as_str(),),
                )
                .map(|_| ()),
            SniCmd::NewToolTip => emit(
                &conn,
                ITEM_PATH,
                "org.kde.StatusNotifierItem",
                "NewToolTip",
                &(),
            ),
            SniCmd::NewIcon => emit(
                &conn,
                ITEM_PATH,
                "org.kde.StatusNotifierItem",
                "NewIcon",
                &(),
            ),
            SniCmd::LayoutUpdated(rev) => emit(
                &conn,
                MENU_PATH,
                "com.canonical.dbusmenu",
                "LayoutUpdated",
                &(rev, 0i32),
            ),
        };
        if let Err(e) = r {
            log::warn!("托盘 D-Bus 调用失败：{e}");
        }
    }
}

/// 监视线程主体：订阅 `NameOwnerChanged(arg0 = Watcher)`，Watcher 在场（含启动时已在）就让
/// 专职线程（重新）登记；在场状态翻转时经 [`TrayEvent::Availability`] 通知 UI 线程。
/// 用独立连接，免得它阻塞在迭代器上时拖住专职线程的连接生命周期（专职线程退出即释放服务名）。
fn watch_watcher(bus: Bus, shared: Arc<Shared>, tx: Sender<SniCmd>, slot: Arc<Mutex<WatchSlot>>) {
    let Ok(conn) = bus.connect() else {
        return; // 总线连不上：专职线程会报，这里不重复
    };
    // 把连接交给 SniLink，drop 时由它关连接来叫醒下面的信号迭代器；已叫停就直接退。
    {
        let mut s = slot.lock().unwrap_or_else(|e| e.into_inner());
        if s.stopped {
            return;
        }
        s.conn = Some(conn.clone());
    }
    let Ok(proxy) = DBusProxy::new(&conn) else {
        return;
    };
    // 先订阅、后查当前状态：反过来会漏掉两步之间发生的上线。
    let Ok(signals) = proxy.receive_name_owner_changed_with_args(&[(0, WATCHER_NAME)]) else {
        return;
    };
    let mut up = watcher_present(&conn);
    if up && tx.send(SniCmd::RegisterWithWatcher).is_err() {
        return;
    }
    for sig in signals {
        let Ok(args) = sig.args() else { continue };
        let now_up = args.new_owner().is_some();
        // 新主人出现（含主人直接易手）都要重新登记：新 Watcher 不记得旧的登记。
        if now_up && tx.send(SniCmd::RegisterWithWatcher).is_err() {
            return;
        }
        if now_up != up {
            up = now_up;
            shared.push(TrayEvent::Availability(up));
        }
    }
}

fn emit<B>(conn: &Connection, path: &str, iface: &str, name: &str, body: &B) -> zbus::Result<()>
where
    B: serde::ser::Serialize + zbus::zvariant::DynamicType,
{
    conn.emit_signal(None::<&str>, path, iface, name, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::tray::TrayMenuItem;
    use crate::signal::signal;

    #[test]
    fn rgba_to_sni_argb_network_order() {
        assert_eq!(
            rgba_to_sni_argb(&[0x11, 0x22, 0x33, 0x44]),
            vec![0x44, 0x11, 0x22, 0x33]
        );
        // 多像素逐个转换，不串位。
        assert_eq!(
            rgba_to_sni_argb(&[1, 2, 3, 4, 5, 6, 7, 8]),
            vec![4, 1, 2, 3, 8, 5, 6, 7]
        );
        assert!(rgba_to_sni_argb(&[]).is_empty());
    }

    #[test]
    fn dbusmenu_layout_from_tray_items() {
        let on = signal(true);
        let items = vec![
            TrayMenuItem::item("打开", |_| {}),
            TrayMenuItem::separator(),
            TrayMenuItem::check("隐身", on, |_| {}),
            TrayMenuItem::item("退出", |_| {}),
        ];
        let layout = build_layout(&menu_entries(&items));
        assert_eq!(layout.id, 0);
        let ids: Vec<i32> = layout.children.iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![1, 2, 3, 4]);
        assert_eq!(
            layout.children[1].props.get("type"),
            Some(&Prop::Str("separator".into()))
        );
        assert_eq!(
            layout.children[2].props.get("toggle-type"),
            Some(&Prop::Str("checkmark".into()))
        );
        assert_eq!(
            layout.children[2].props.get("toggle-state"),
            Some(&Prop::Int(1))
        );
        assert_eq!(
            layout.children[0].props.get("label"),
            Some(&Prop::Str("打开".into()))
        );
        // 勾选态是现取的：翻转信号后下一次快照就变。
        on.set(false);
        let again = build_layout(&menu_entries(&items));
        assert_eq!(
            again.children[2].props.get("toggle-state"),
            Some(&Prop::Int(0))
        );
    }

    #[test]
    fn mnemonic_underscore_is_escaped_and_disabled_item_flagged() {
        let off = signal(false);
        let items = vec![TrayMenuItem::item("a_b", |_| {}).enabled(off)];
        let layout = build_layout(&menu_entries(&items));
        assert_eq!(
            layout.children[0].props.get("label"),
            Some(&Prop::Str("a__b".into()))
        );
        assert_eq!(
            layout.children[0].props.get("enabled"),
            Some(&Prop::Bool(false))
        );
    }

    #[test]
    fn layout_converts_to_wire_format() {
        let items = vec![TrayMenuItem::item("x", |_| {}), TrayMenuItem::separator()];
        let (id, props, children) = wire_layout(&build_layout(&menu_entries(&items)));
        assert_eq!(id, 0);
        assert!(props.contains_key("children-display"));
        assert_eq!(children.len(), 2);
    }

    // ── 私有总线集成测试 ────────────────────────────────────────────────────

    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// 私有 `dbus-daemon`；本机没有可执行文件时 `start` 返回 `None`，测试跳过并说明。
    struct PrivateBus {
        child: Child,
        pub addr: String,
    }

    impl PrivateBus {
        fn start() -> Option<Self> {
            let mut child = Command::new("dbus-daemon")
                .args(["--session", "--nofork", "--print-address=1"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let mut line = String::new();
            BufReader::new(child.stdout.take()?)
                .read_line(&mut line)
                .ok()?;
            let addr = line.trim().to_string();
            if addr.is_empty() {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Some(Self { child, addr })
        }
        fn bus(&self) -> Bus {
            Bus::Address(self.addr.clone())
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    macro_rules! private_bus_or_skip {
        () => {
            match PrivateBus::start() {
                Some(b) => b,
                None => {
                    eprintln!("跳过：本机没有可用的 dbus-daemon");
                    return;
                }
            }
        };
    }

    struct FakeWatcher {
        registered: Arc<Mutex<Vec<String>>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
    impl FakeWatcher {
        fn register_status_notifier_item(&self, service: String) {
            self.registered.lock().unwrap().push(service);
        }
    }

    fn start_watcher(bus: &Bus) -> (Connection, Arc<Mutex<Vec<String>>>) {
        let registered = Arc::new(Mutex::new(Vec::new()));
        let conn = ConnectionBuilder::address(match bus {
            Bus::Address(a) => a.as_str(),
            Bus::Session => unreachable!(),
        })
        .unwrap()
        .serve_at(
            WATCHER_PATH,
            FakeWatcher {
                registered: registered.clone(),
            },
        )
        .unwrap()
        .name(WATCHER_NAME)
        .unwrap()
        .build()
        .unwrap();
        (conn, registered)
    }

    type Note = (String, String);

    struct FakeNotifications {
        got: Arc<Mutex<Vec<Note>>>,
        /// 每次 Notify 收到的 actions。
        actions: Arc<Mutex<Vec<Vec<String>>>>,
        next_id: AtomicU32,
    }

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl FakeNotifications {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            _app: String,
            _replaces: u32,
            _icon: String,
            summary: String,
            body: String,
            actions: Vec<String>,
            _hints: HashMap<String, OwnedValue>,
            _timeout: i32,
        ) -> u32 {
            self.got.lock().unwrap().push((summary, body));
            self.actions.lock().unwrap().push(actions);
            self.next_id.fetch_add(1, Ordering::Relaxed)
        }
    }

    type Actions = Arc<Mutex<Vec<Vec<String>>>>;

    fn start_notification_server(bus: &Bus) -> (Connection, Arc<Mutex<Vec<Note>>>, Actions) {
        let got = Arc::new(Mutex::new(Vec::new()));
        let actions: Actions = Arc::default();
        let Bus::Address(a) = bus else { unreachable!() };
        let conn = ConnectionBuilder::address(a.as_str())
            .unwrap()
            .serve_at(
                "/org/freedesktop/Notifications",
                FakeNotifications {
                    got: got.clone(),
                    actions: actions.clone(),
                    next_id: AtomicU32::new(1),
                },
            )
            .unwrap()
            .name("org.freedesktop.Notifications")
            .unwrap()
            .build()
            .unwrap();
        (conn, got, actions)
    }

    /// 假通知服务端发信号。
    fn emit_notification_signal<B>(srv: &Connection, name: &str, body: &B)
    where
        B: serde::ser::Serialize + zbus::zvariant::DynamicType,
    {
        emit(
            srv,
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            name,
            body,
        )
        .unwrap();
    }

    fn wait_for(mut cond: impl FnMut() -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(5);
        while Instant::now() < end {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn tray_supported_false_without_watcher() {
        let pb = private_bus_or_skip!();
        assert!(!supported_on(&pb.bus()), "总线上没有 Watcher");
        let (_w, _) = start_watcher(&pb.bus());
        assert!(supported_on(&pb.bus()), "对照：有 Watcher 就是 true");
    }

    #[test]
    fn tray_supported_false_when_bus_unreachable() {
        // 不需要 dbus-daemon：指向一个不存在的 socket。
        let bus = Bus::Address("unix:path=/nonexistent/windui-no-bus".into());
        assert!(!supported_on(&bus));
    }

    #[test]
    fn install_degrades_without_bus_and_never_blocks() {
        let bus = Bus::Address("unix:path=/nonexistent/windui-no-bus".into());
        let t0 = Instant::now();
        let mut host = TrayHost::install(Tray::new().tooltip("x"), &bus);
        // 降级后各操作都是安全的空转 / 丢弃。
        host.apply_op(TrayOp::SetTooltip("y".into()));
        host.apply_op(TrayOp::Notify {
            title: "t".into(),
            body: "b".into(),
            tag: Some("c".into()),
        });
        host.refresh_menu();
        assert!(host.poll_actions().is_empty());
        assert!(t0.elapsed() < Duration::from_secs(2), "降级路径不得阻塞");
    }

    #[test]
    fn install_degrades_without_watcher_but_notifications_still_flow() {
        let pb = private_bus_or_skip!();
        let (_srv, got, _) = start_notification_server(&pb.bus());
        let mut host = TrayHost::install(Tray::new(), &pb.bus());
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            host.poll_actions().is_empty(),
            "初始没有 Watcher 不算「变化」"
        );
        host.notify("标题", "正文 <b>&", None);
        assert!(wait_for(|| !got.lock().unwrap().is_empty()));
        assert_eq!(
            got.lock().unwrap()[0],
            ("标题".to_string(), "正文 &lt;b&gt;&amp;".to_string())
        );
    }

    #[test]
    fn notify_without_daemon_does_not_block_or_panic() {
        let pb = private_bus_or_skip!();
        let mut n = Notifier::new(pb.bus(), Arc::new(|_| panic!("没有守护进程，不该有点击")));
        let t0 = Instant::now();
        n.send("t", "b", Some("c")); // 总线上没有通知守护进程
        assert!(
            t0.elapsed() < Duration::from_millis(500),
            "入队必须立即返回"
        );
    }

    #[test]
    fn tray_registers_and_routes_clicks_over_dbus() {
        let pb = private_bus_or_skip!();
        let (_watcher, registered) = start_watcher(&pb.bus());

        let on = signal(false);
        let tray = Tray::new()
            .tooltip("LanMsg")
            .icon_rgba(1, 1, &[0x11, 0x22, 0x33, 0x44])
            .on_left_click(|ctx| ctx.show_window())
            .menu(vec![
                TrayMenuItem::item("打开", |ctx| ctx.show_window()),
                TrayMenuItem::separator(),
                TrayMenuItem::check("隐身", on, |ctx| ctx.quit()),
            ]);
        let mut host = TrayHost::install(tray, &pb.bus());
        let svc = host.service_name().to_string();
        assert!(wait_for(|| registered.lock().unwrap().contains(&svc)));

        // 模拟宿主（托盘面板）作为另一个客户端访问。
        let host_conn = pb.bus().connect().unwrap();
        let get = |path: &str, iface: &str, m: &str, body: &(i32, i32)| {
            host_conn
                .call_method(Some(svc.as_str()), path, Some(iface), m, body)
                .unwrap()
        };

        // 属性：图标像素按 ARGB 网络序。
        let pm: Pixmap = host_conn
            .call_method(
                Some(svc.as_str()),
                ITEM_PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.kde.StatusNotifierItem", "IconPixmap"),
            )
            .unwrap()
            .body()
            .deserialize::<OwnedValue>()
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(pm, vec![(1, 1, vec![0x44, 0x11, 0x22, 0x33])]);

        // 左键。
        get(ITEM_PATH, "org.kde.StatusNotifierItem", "Activate", &(0, 0));
        assert_eq!(host.poll_actions(), vec![TrayAction::Show]);

        // 菜单布局经线路取回，且点击「隐身」（id 3）触发对应回调。
        let reply = host_conn
            .call_method(
                Some(svc.as_str()),
                MENU_PATH,
                Some("com.canonical.dbusmenu"),
                "GetLayout",
                &(0i32, -1i32, Vec::<String>::new()),
            )
            .unwrap();
        let (_rev, (root_id, _props, children)): (u32, WireLayout) =
            reply.body().deserialize().unwrap();
        assert_eq!(root_id, 0);
        assert_eq!(children.len(), 3);

        let click = |id: i32| {
            host_conn
                .call_method(
                    Some(svc.as_str()),
                    MENU_PATH,
                    Some("com.canonical.dbusmenu"),
                    "Event",
                    &(id, "clicked", Value::from(0i32), 0u32),
                )
                .unwrap();
        };
        click(2); // 分隔线：不产生事件
        click(3);
        assert_eq!(host.poll_actions(), vec![TrayAction::Quit]);

        // 菜单变化：翻转勾选态后快照刷新，修订号递增。
        let rev_of = |h: &TrayHost| h.shared.state.lock().unwrap().revision;
        let before = rev_of(&host);
        on.set(true);
        host.refresh_menu();
        assert_eq!(rev_of(&host), before + 1);
        host.refresh_menu();
        assert_eq!(rev_of(&host), before + 1, "没变化不再递增");
    }

    /// Watcher 晚于应用启动、中途下线再重启：每次上线都（重新）登记，可用性翻转触发回调。
    #[test]
    fn tray_registers_when_watcher_starts_late_and_after_restart() {
        use std::cell::RefCell;
        use std::rc::Rc;

        let pb = private_bus_or_skip!();
        let seen: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
        let s2 = seen.clone();
        let tray = Tray::new().on_availability_changed(move |_ctx, up| s2.borrow_mut().push(up));
        let mut host = TrayHost::install(tray, &pb.bus());
        let svc = host.service_name().to_string();

        // 先装托盘、后起 Watcher。
        std::thread::sleep(Duration::from_millis(200)); // 让监视线程先订阅上
        let (w1, reg1) = start_watcher(&pb.bus());
        assert!(
            wait_for(|| reg1.lock().unwrap().contains(&svc)),
            "晚启动的 Watcher 应收到登记"
        );
        assert!(wait_for(|| {
            host.poll_actions();
            seen.borrow().last() == Some(&true)
        }));

        // Watcher 掉线 → 可用性 false；重启 → 再次登记且回到 true。
        drop(w1);
        assert!(wait_for(|| {
            host.poll_actions();
            seen.borrow().last() == Some(&false)
        }));
        let (_w2, reg2) = start_watcher(&pb.bus());
        assert!(
            wait_for(|| reg2.lock().unwrap().contains(&svc)),
            "重启后的 Watcher 应再次收到登记"
        );
        assert!(wait_for(|| {
            host.poll_actions();
            seen.borrow().last() == Some(&true)
        }));
        assert_eq!(*seen.borrow(), vec![true, false, true]);
    }

    /// Watcher 保持在线、不产生任何 NameOwnerChanged 时，释放托盘也要让两条线程退出。
    #[test]
    fn dropping_tray_stops_both_threads_while_watcher_stays_up() {
        let pb = private_bus_or_skip!();
        let (_w, reg) = start_watcher(&pb.bus());
        let mut host = TrayHost::install(Tray::new(), &pb.bus());
        let svc = host.service_name().to_string();
        assert!(wait_for(|| reg.lock().unwrap().contains(&svc)));
        std::thread::sleep(Duration::from_millis(200)); // 监视线程已阻塞在迭代器上

        let mut link = host.link.take().expect("线程应已创建");
        let threads = std::mem::take(&mut link.threads);
        drop(link);
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end && !threads.iter().all(|t| t.is_finished()) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            threads.iter().all(|t| t.is_finished()),
            "释放托盘后专职线程与监视线程应在 2 秒内退出"
        );
    }

    /// 启动时 Watcher 已在场：登记照常，初始状态不当作「变化」。
    #[test]
    fn availability_callback_ignores_initial_state() {
        let pb = private_bus_or_skip!();
        let (_w, reg) = start_watcher(&pb.bus());
        let mut host = TrayHost::install(
            Tray::new().on_availability_changed(|_, _| panic!("初始状态不该触发")),
            &pb.bus(),
        );
        let svc = host.service_name().to_string();
        assert!(wait_for(|| reg.lock().unwrap().contains(&svc)));
        host.poll_actions();
    }

    #[test]
    fn set_icon_with_mismatched_pixels_is_ignored() {
        let pb = private_bus_or_skip!();
        let mut host = TrayHost::install(Tray::new().icon_rgba(1, 1, &[1, 2, 3, 4]), &pb.bus());
        host.apply_op(TrayOp::SetIcon {
            w: 2,
            h: 2,
            rgba: vec![0; 3],
        });
        let icon = host.shared.state.lock().unwrap().icon.clone();
        assert_eq!(
            icon,
            vec![(1, 1, vec![4, 1, 2, 3])],
            "非法像素数据不得覆盖原图标"
        );
    }

    use std::sync::mpsc;

    fn click_channel() -> (ClickFn, mpsc::Receiver<Option<String>>) {
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        (
            Arc::new(move |tag| {
                let _ = tx.lock().unwrap().send(tag);
            }),
            rx,
        )
    }

    use super::super::notify::ClickFn;

    #[test]
    fn notify_carries_default_action_and_click_maps_back_to_tag() {
        let pb = private_bus_or_skip!();
        let (srv, got, actions) = start_notification_server(&pb.bus());
        let (cb, rx) = click_channel();
        let mut n = Notifier::new(pb.bus(), cb);
        n.send("t", "b", Some("conv1"));
        assert!(wait_for(|| !got.lock().unwrap().is_empty()));
        assert!(
            actions.lock().unwrap()[0].iter().any(|a| a == "default"),
            "Notify 的 actions 必须含 default：{:?}",
            actions.lock().unwrap()[0]
        );
        assert!(wait_for(|| n.has_tag(1)));

        // 干扰信号：非 default 的 action、别的应用的通知 id —— 都不得回调。
        emit_notification_signal(&srv, "ActionInvoked", &(1u32, "other"));
        emit_notification_signal(&srv, "ActionInvoked", &(999u32, "default"));
        emit_notification_signal(&srv, "ActionInvoked", &(1u32, "default"));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Some("conv1".to_string())
        );
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "干扰信号不该产生额外回调"
        );
    }

    #[test]
    fn closed_notification_is_removed_from_tag_table() {
        let pb = private_bus_or_skip!();
        let (srv, got, _) = start_notification_server(&pb.bus());
        let (cb, rx) = click_channel();
        let mut n = Notifier::new(pb.bus(), cb);
        n.send("t", "b", Some("old"));
        assert!(wait_for(|| n.has_tag(1)));
        emit_notification_signal(&srv, "NotificationClosed", &(1u32, 2u32));
        assert!(
            wait_for(|| !n.has_tag(1)),
            "NotificationClosed 后映射应被移除"
        );
        // 已关闭的通知再被点：不回调。用第二条通知作哨兵，确认先到的那条被忽略。
        n.send("t2", "b2", None);
        assert!(wait_for(|| got.lock().unwrap().len() == 2));
        assert!(wait_for(|| n.has_tag(2)));
        emit_notification_signal(&srv, "ActionInvoked", &(1u32, "default"));
        emit_notification_signal(&srv, "ActionInvoked", &(2u32, "default"));
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            None,
            "第一个回调应来自无标签的第二条通知"
        );
        assert!(rx.recv_timeout(Duration::from_millis(200)).is_err());
    }

    #[test]
    fn notification_click_reaches_tray_callback_on_ui_thread() {
        let pb = private_bus_or_skip!();
        let (srv, _got, _) = start_notification_server(&pb.bus());
        let tray = Tray::new().on_notification_click(|ctx, tag| {
            if tag.as_deref() == Some("conv1") {
                ctx.show_window();
            }
        });
        let mut host = TrayHost::install(tray, &pb.bus());
        host.apply_op(TrayOp::Notify {
            title: "t".into(),
            body: "b".into(),
            tag: Some("conv1".into()),
        });
        assert!(wait_for(|| host.notifier.has_tag(1)));
        emit_notification_signal(&srv, "ActionInvoked", &(1u32, "default"));
        let mut acts = Vec::new();
        assert!(wait_for(|| {
            acts.extend(host.poll_actions());
            !acts.is_empty()
        }));
        assert_eq!(acts, vec![TrayAction::Show]);
    }

    #[test]
    fn dropping_notifier_stops_listener_thread() {
        let pb = private_bus_or_skip!();
        let (_srv, got, _) = start_notification_server(&pb.bus());
        let (cb, _rx) = click_channel();
        let mut n = Notifier::new(pb.bus(), cb);
        n.send("t", "b", None);
        assert!(wait_for(|| !got.lock().unwrap().is_empty()));
        std::thread::sleep(Duration::from_millis(200)); // 监听线程已阻塞在迭代器上

        let mut listener = n.take_listener().expect("监听线程应已创建");
        let thread = listener.take_thread();
        drop(listener);
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end && !thread.is_finished() {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(thread.is_finished(), "释放后监听线程应在 2 秒内退出");
    }
}
