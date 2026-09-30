//! 剪贴板与文件拖入：`wl_data_device_manager` / `wl_data_device`。
//!
//! # 剪贴板
//!
//! 与 X11 同构——「谁拥有选区，谁负责应答读取」：
//! - **复制**：建一个 `wl_data_source`，声明几种文本 MIME，`set_selection` 带最近一次输入事件
//!   的 serial（合成器凭它认定是用户操作）。别人来读时合成器发 `send`，给一个 fd，我们把文本
//!   写进去再关掉。写是**非阻塞**的：写不完的挂进事件循环的 `poll`（见 [`pump_sends`]），
//!   对方读得慢也不卡界面；对方一直不读，超过 [`SEND_STALL`] 没有进展就放弃。
//! - **粘贴**：跟踪 `selection` 事件给的当前 offer 与它的 MIME 列表。读时 `receive` 一个管道
//!   写端、`flush`、对读端做**限时**读取（[`READ_TIMEOUT`]）。
//!
//! **自家的选区绝不走管道**：合成器同样会把我们自己的 source 包成 offer 发回来，要是照常
//! `receive`，写管道的 `send` 要等事件循环分发——而事件循环此刻正阻塞在读管道上，互等到
//! 超时。每个 source 额外声明一个带进程级随机标记与序号的私有 MIME（[`MARKER`]），见到带它的 offer、
//! 且那个 source 还活着，就按序号取本地文本（source 已没了则是剪贴板管理器照抄的副本，照常读）。
//! 这个 MIME 别的应用也看得见（`wl-paste -l` 会列出来），无害。
//!
//! 「现在剪贴板里是不是我们的」由 [`Owner`] 记账（纯逻辑，有单测）：`set_selection` 之后发一个
//! `wl_display.sync`，合成器按序处理请求，选区事件必先于它的 `done` 到达；到 `done` 还没见到
//! 自己的选区、而窗口又有键盘焦点（合成器只给焦点客户端发选区事件），说明请求被拒了
//! （serial 无效——比如没有任何输入就复制；wlroots 拒绝时既不采用也不发 `cancelled`）。
//! 这时销毁那个 source，读剪贴板回到实际选区，不再拿本地文本冒充。
//!
//! 剪贴板状态放在事件循环线程的 thread-local 里：`ClipboardProvider` 是个无状态的单元类型、
//! 碰不到 `Wl`，而 `QueueHandle<Wl>` 也不宜跨线程。其它线程读写剪贴板时记警告并当空处理
//! （X11 后端有独立的剪贴板线程，任何线程都能用，这是两边的差异）。
//!
//! 与 X11 的另一处差异：Wayland 没有剪贴板管理器协议，**应用退出后复制的内容随之消失**
//! （除非桌面自带剪贴板管理器接管，GNOME / KDE 通常有）。
//!
//! # 文件拖入
//!
//! `enter` 时 offer 的 MIME 已经到齐：含 `text/uri-list` 且窗口没被模态子窗挡住就 `accept` +
//! `set_actions(copy)`（拖动途中模态状态变了，`motion` 里改口）；`drop` 时同样经管道限时读出
//! uri-list，解析（`host::parse_uri_list`，与 XDND 同一份）后按落点交给 `on_drop_files`，读到了
//! 路径才 `finish`（否则只销毁 offer，源端收到 `cancelled` 即知失败）。
//!
//! 拖入读数据是**异步**的（与 XDND 一致）：放下时只发 `receive`，读端挂进事件循环的 `poll`
//! （[`Wl::pump_drops`]），读到 EOF 再解析交付；源端卡住也不冻界面，[`READ_TIMEOUT`] 内没写完
//! 就放弃。读的期间目标窗口关了，读完也不交付、不 `finish`。
//!
//! 不做：文件拖出（`start_drag`）、primary selection（中键粘贴，`zwp_primary_selection`）。

use std::cell::RefCell;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wayland_client::backend::WaylandError;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::{
    wl_callback, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_seat,
    wl_surface,
};
use wayland_client::{event_created_child, Connection, Dispatch, Proxy, QueueHandle, WEnum};

use super::super::host;
use super::Wl;
use crate::geometry::Point;

/// 读别家剪贴板 / 拖入数据的时限。与 X11 后端（1 秒等应答 + 1.5 秒总限）同量级：够对方
/// 应用在正常负载下应答，又不至于让一次卡死的源端把界面冻住太久。
const READ_TIMEOUT: Duration = Duration::from_millis(1500);

/// 读入上限：剪贴板里真有几十 MB 文本也不该让一次粘贴吃掉等量内存以上。超出即丢弃。
const READ_LIMIT: usize = 64 << 20;

/// 应答 `send` 时对方多久不读就放弃（关 fd，对方读到截断的内容）。
const SEND_STALL: Duration = Duration::from_secs(5);

/// 我们提供的文本 MIME。`UTF8_STRING` / `TEXT` / `STRING` 是 X11 的目标名：XWayland 把 X 应用
/// 的读取请求按原名转过来，不列的话 X 应用粘贴不到。内容一律写 UTF-8（与 X11 后端应答
/// `STRING` 的做法一致）。
const OFFER_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "TEXT",
    "STRING",
];

/// 私有标记 MIME 的前缀，完整形如 `application/x-windui-source;token=1f3a…;id=4`。token 是
/// 进程启动时取的随机值而不是 pid：Flatpak 等沙箱里各应用的 pid 常常相同（都是 2）。
const MARKER: &str = "application/x-windui-source;token=";

const URI_LIST: &str = "text/uri-list";

/// 文本的编码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Enc {
    Utf8,
    /// X11 的 `STRING` 按 ICCCM 是 Latin-1。
    Latin1,
}

/// 从 offer 的 MIME 里挑一个读文本用的：先 UTF-8 的，`STRING` 兜底。MIME 参数的大小写不
/// 敏感（有应用发 `charset=UTF-8`），返回 offer 原样的写法（`receive` 要原名）。
fn pick_text_mime(mimes: &[String]) -> Option<(&str, Enc)> {
    const PREF: [(&str, Enc); 4] = [
        ("text/plain;charset=utf-8", Enc::Utf8),
        ("UTF8_STRING", Enc::Utf8),
        ("text/plain", Enc::Utf8),
        ("STRING", Enc::Latin1),
    ];
    PREF.iter().find_map(|(want, enc)| {
        mimes
            .iter()
            .find(|m| m.eq_ignore_ascii_case(want))
            .map(|m| (m.as_str(), *enc))
    })
}

fn decode(bytes: &[u8], enc: Enc) -> String {
    match enc {
        Enc::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Enc::Latin1 => bytes.iter().map(|&b| char::from(b)).collect(),
    }
}

fn marker(token: u64, id: u64) -> String {
    format!("{MARKER}{token:x};id={id}")
}

/// offer 若是本进程某个 source 包成的，返回那个 source 的序号。别的进程（包括另一个 windui
/// 应用）的标记不算。
fn marker_id(mimes: &[String], token: u64) -> Option<u64> {
    let prefix = format!("{MARKER}{token:x};id=");
    mimes
        .iter()
        .find_map(|m| m.strip_prefix(&prefix)?.parse().ok())
}

/// 「剪贴板现在是不是我们的、是哪一次复制的」的记账。只管序号，协议对象由调用方按序号
/// 找、按返回值销毁。
#[derive(Default)]
struct Owner {
    next: u64,
    /// 尚未销毁的 source：（序号，合成器已确认成为选区，`sync` 已回）。
    live: Vec<(u64, bool, bool)>,
    /// 本地认定的当前选区：读剪贴板直接取它的文本。
    current: Option<u64>,
}

/// 当前选区是谁的。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sel {
    Empty,
    Ours(u64),
    Foreign,
}

/// 读剪贴板怎么读。
#[derive(Debug, PartialEq, Eq)]
enum ReadPlan {
    Local(u64),
    Pipe,
    Empty,
}

impl Owner {
    /// 发出一次 `set_selection`，返回新 source 的序号。
    fn set(&mut self) -> u64 {
        self.next += 1;
        self.live.push((self.next, false, false));
        self.current = Some(self.next);
        self.next
    }

    fn synced(&self, id: u64) -> bool {
        self.live.iter().any(|&(i, _, s)| i == id && s)
    }

    /// 收到 `selection` 事件。返回可以销毁的 source（被更新的选区取代了的旧 source）。
    fn on_selection(&mut self, sel: Sel) -> Vec<u64> {
        match sel {
            Sel::Ours(k) => {
                if let Some(e) = self.live.iter_mut().find(|e| e.0 == k) {
                    e.1 = true;
                }
                // 已 sync 过却从未被确认的都是被拒了（失焦时发出的：当时没法判定，现在选区仍是
                // k 才看出来）——被接受后又被取代的话，早该先收到 `cancelled`。比 k 旧的一并
                // 释放（已被 k 取代）；比 k 新、还在路上的不动，`current` 若是它，本地仍以它为准。
                let mut gone = self.release_rejected();
                gone.extend(self.release_older(k));
                gone
            }
            // 别人的（或清空了）：我们的那次复制若已被合成器处理过，就是被取代了；还在路上
            // 的不算（这条事件早于它产生）。已 sync 却从未被确认的都是被拒了（同上理），释放。
            Sel::Foreign | Sel::Empty => {
                if self.current.is_some_and(|c| self.synced(c)) {
                    self.current = None;
                }
                self.release_rejected()
            }
        }
    }

    /// 释放已 sync 却从未被确认成为选区的 source（被拒的）。
    fn release_rejected(&mut self) -> Vec<u64> {
        let gone: Vec<u64> = self
            .live
            .iter()
            .filter(|&&(_, confirmed, synced)| synced && !confirmed)
            .map(|e| e.0)
            .collect();
        for &id in &gone {
            self.forget(id);
        }
        gone
    }

    /// 释放序号比 `k` 小的 source（已被 k 取代）。
    fn release_older(&mut self, k: u64) -> Vec<u64> {
        let old: Vec<u64> = self.live.iter().map(|e| e.0).filter(|&i| i < k).collect();
        self.live.retain(|e| e.0 >= k);
        old
    }

    /// `set_selection` 之后的 `sync` 回来了。有键盘焦点却没见到自己的选区 = 被拒，返回 true，
    /// 调用方销毁该 source。没焦点时合成器本就不给我们发选区事件，无从判断，留着。
    fn on_synced(&mut self, id: u64, focused: bool) -> bool {
        let Some(e) = self.live.iter_mut().find(|e| e.0 == id) else {
            return false;
        };
        e.2 = true;
        if e.1 || !focused {
            return false;
        }
        self.forget(id);
        true
    }

    /// source 被合成器 `cancelled`，或被我们销毁。
    fn forget(&mut self, id: u64) {
        self.live.retain(|e| e.0 != id);
        if self.current == Some(id) {
            self.current = None;
        }
    }

    fn plan(&self, sel: Sel) -> ReadPlan {
        if let Some(c) = self.current {
            return ReadPlan::Local(c);
        }
        match sel {
            Sel::Ours(k) if self.live.iter().any(|e| e.0 == k) => ReadPlan::Local(k),
            // 带着我们的标记、但那个 source 已经没了：是剪贴板管理器（CopyQ 之类会照抄全部
            // MIME）接手后重新提供的副本，应答方是它而不是我们，照常走管道。source 在合成器
            // 那边一旦被取代就先给我们发 `cancelled`、再发新选区，所以「还活着」足以判定
            // 应答方是自己。
            Sel::Ours(_) | Sel::Foreign => ReadPlan::Pipe,
            Sel::Empty => ReadPlan::Empty,
        }
    }
}

/// `wl_data_offer` 的 user data：`offer` 事件陆续报来的 MIME，拖放时合成器选定的动作。
#[derive(Default)]
pub(super) struct OfferInfo {
    mimes: Mutex<Vec<String>>,
    action: AtomicU32,
}

impl OfferInfo {
    fn mimes(&self) -> Vec<String> {
        self.mimes.lock().map(|m| m.clone()).unwrap_or_default()
    }
}

fn offer_mimes(offer: &wl_data_offer::WlDataOffer) -> Vec<String> {
    offer
        .data::<OfferInfo>()
        .map(OfferInfo::mimes)
        .unwrap_or_default()
}

/// `wl_data_source` 的 user data。
pub(super) struct SourceData {
    id: u64,
    text: Arc<str>,
}

/// `set_selection` 之后那个 `wl_display.sync` 的 user data：对应的 source 序号。
pub(super) struct SelectionSync(u64);

/// 一次进行中的拖入（`enter` 到 `drop` / `leave`）。
pub(super) struct DropTarget {
    offer: wl_data_offer::WlDataOffer,
    /// `enter` 的 serial：途中改口 `accept` 要带它。
    serial: u32,
    /// 目标窗口号。
    key: u32,
    /// 表面坐标（逻辑，带小数）。
    pos: (f64, f64),
    has_uris: bool,
    accepted: bool,
}

/// 已放下、正在读 uri-list 的拖入。读端挂进事件循环的 `poll`，读到 EOF 再交给窗口。
pub(super) struct PendingDrop {
    offer: wl_data_offer::WlDataOffer,
    key: u32,
    /// 落点（表面坐标，逻辑）。完成时按窗口那时的缩放换算。
    pos: (f64, f64),
    file: File,
    data: Vec<u8>,
    deadline: Instant,
}

// ── 剪贴板状态（事件循环线程的 thread-local） ────────────────────────────────

struct Clip {
    conn: Connection,
    qh: QueueHandle<Wl>,
    manager: wl_data_device_manager::WlDataDeviceManager,
    device: wl_data_device::WlDataDevice,
    /// 本进程的标记值（见 [`MARKER`]）。
    token: u64,
    /// 最近一次输入事件（按键、按钮、键盘焦点进入）的 serial：`set_selection` 要带它。
    serial: u32,
    /// 本应用有没有键盘焦点（任一窗口）。
    focused: bool,
    owner: Owner,
    /// 尚未销毁的 source（序号见 user data）。
    sources: Vec<wl_data_source::WlDataSource>,
    /// 当前选区的 offer（别人的，或我们自己的 source 被包成的）及其归属。
    selection: Option<wl_data_offer::WlDataOffer>,
    sel: Sel,
    /// 还没写完的 `send` 应答。
    sends: Vec<Outgoing>,
}

impl Clip {
    fn source_text(&self, id: u64) -> Option<Arc<str>> {
        self.sources
            .iter()
            .filter_map(|s| s.data::<SourceData>())
            .find(|d| d.id == id)
            .map(|d| d.text.clone())
    }

    fn destroy_source(&mut self, id: u64) {
        self.sources.retain(|s| {
            let hit = s.data::<SourceData>().is_some_and(|d| d.id == id);
            if hit {
                s.destroy();
            }
            !hit
        });
    }
}

struct Outgoing {
    file: File,
    data: Arc<str>,
    written: usize,
    /// 最近一次有进展的时刻。
    progress: Instant,
}

thread_local! {
    static CLIP: RefCell<Option<Clip>> = const { RefCell::new(None) };
}

/// 进程里有 Wayland 剪贴板在用（某个线程上）。其它线程据此知道「不该回落 X11」。
static ACTIVE: AtomicBool = AtomicBool::new(false);

static OFF_THREAD_WARNED: AtomicBool = AtomicBool::new(false);

/// 事件循环启动时调用：有 `wl_data_device_manager` 与 seat 才建数据设备。没有的话不接管
/// 剪贴板（回落 X11 那一路：有 XWayland 时还能用，没有就是空），拖入不可用。
pub(super) fn init(
    conn: &Connection,
    qh: &QueueHandle<Wl>,
    manager: Option<wl_data_device_manager::WlDataDeviceManager>,
    seat: Option<&wl_seat::WlSeat>,
) {
    let (Some(manager), Some(seat)) = (manager, seat) else {
        log::warn!("合成器没有 wl_data_device_manager 或 wl_seat，Wayland 剪贴板与文件拖入不可用");
        return;
    };
    let device = manager.get_data_device(seat, qh, ());
    CLIP.with(|c| {
        *c.borrow_mut() = Some(Clip {
            conn: conn.clone(),
            qh: qh.clone(),
            manager,
            device,
            token: process_token(),
            serial: 0,
            focused: false,
            owner: Owner::default(),
            sources: Vec::new(),
            selection: None,
            sel: Sel::Empty,
            sends: Vec::new(),
        })
    });
    ACTIVE.store(true, Ordering::Relaxed);
}

/// 事件循环退出时调用：放掉协议对象；之后的剪贴板读写回到「没在用」。
pub(super) fn shutdown() {
    CLIP.with(|c| *c.borrow_mut() = None);
    ACTIVE.store(false, Ordering::Relaxed);
}

/// 记下一次输入事件的 serial。
pub(super) fn note_serial(serial: u32) {
    with_local(|c| c.serial = serial);
}

/// 键盘焦点进出本应用。
pub(super) fn note_focus(focused: bool) {
    with_local(|c| c.focused = focused);
}

fn with_local(f: impl FnOnce(&mut Clip)) {
    CLIP.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            f(c);
        }
    });
}

/// 外层 `None` = Wayland 剪贴板没在用，调用方走 X11。内层 `None` = 在用，但不在界面线程上。
fn with_clip<R>(f: impl FnOnce(&mut Clip) -> R) -> Option<Option<R>> {
    if !ACTIVE.load(Ordering::Relaxed) {
        return None;
    }
    Some(CLIP.with(|c| {
        let mut c = c.borrow_mut();
        if c.is_none() && !OFF_THREAD_WARNED.swap(true, Ordering::Relaxed) {
            log::warn!("Wayland 下剪贴板只能在界面线程上读写（其它线程的读写当空处理）");
        }
        c.as_mut().map(f)
    }))
}

/// 进程级的随机标记值：pid、启动时刻的纳秒数与标准库的随机哈希种子混在一起。
fn process_token() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    if let Ok(t) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        h.write_u128(t.as_nanos());
    }
    h.finish()
}

/// 读剪贴板文本。`None` = Wayland 剪贴板没在用，调用方走 X11；在用时一律 `Some`。
pub(in super::super) fn get_text() -> Option<Option<String>> {
    enum Job {
        Done(Option<String>),
        Pipe(wl_data_offer::WlDataOffer, Connection),
    }
    // 借用只在取值时持有：读管道期间不占着 thread-local。
    let job = with_clip(|c| match c.owner.plan(c.sel) {
        ReadPlan::Local(id) => Job::Done(c.source_text(id).map(|t| t.to_string())),
        ReadPlan::Empty => Job::Done(None),
        ReadPlan::Pipe => match &c.selection {
            Some(o) => Job::Pipe(o.clone(), c.conn.clone()),
            None => Job::Done(None),
        },
    })?;
    let Some(job) = job else {
        return Some(None);
    };
    Some(match job {
        Job::Done(t) => t,
        Job::Pipe(offer, conn) => {
            let mimes = offer_mimes(&offer);
            let Some((mime, enc)) = pick_text_mime(&mimes) else {
                log::debug!("剪贴板内容没有文本格式");
                return Some(None);
            };
            receive(&conn, &offer, mime).map(|b| decode(&b, enc))
        }
    })
}

/// 写剪贴板文本。返回 `false` = Wayland 剪贴板没在用，调用方走 X11。
pub(in super::super) fn set_text(text: &str) -> bool {
    with_clip(|c| {
        let id = c.owner.set();
        let source = c.manager.create_data_source(
            &c.qh,
            SourceData {
                id,
                text: Arc::from(text),
            },
        );
        for m in OFFER_MIMES {
            source.offer(m.to_string());
        }
        source.offer(marker(c.token, id));
        c.device.set_selection(Some(&source), c.serial);
        c.conn.display().sync(&c.qh, SelectionSync(id));
        // 旧 source 不在这里销毁：等合成器 `cancelled` 或新选区确认（期间别人对旧选区的读取
        // 照常应答）。
        c.sources.push(source);
        // 界面线程随后回到事件循环才 flush；这里先送出，别的应用立刻就能读到。
        let _ = c.conn.flush();
    })
    .is_some()
}

/// 向 offer 要 `mime` 格式的数据：管道写端交给对方（请求尚未 flush），返回已设非阻塞的读端。
fn start_receive(offer: &wl_data_offer::WlDataOffer, mime: &str) -> Option<File> {
    let (rd, wr) = match super::super::sys::pipe_cloexec() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("建管道失败：{e}");
            return None;
        }
    };
    offer.receive(mime.to_string(), wr.as_fd());
    // 协议层已复制了这个 fd；我们这份必须关掉，否则读端永远等不到 EOF。
    drop(wr);
    if let Err(e) = super::super::sys::set_nonblocking(&rd) {
        log::warn!("管道设为非阻塞失败：{e}");
        return None;
    }
    Some(rd)
}

/// 向 offer 要 `mime` 格式的数据，限时读完（剪贴板读取用：调用方要同步拿到结果）。
fn receive(conn: &Connection, offer: &wl_data_offer::WlDataOffer, mime: &str) -> Option<Vec<u8>> {
    let mut rd = start_receive(offer, mime)?;
    let deadline = Instant::now() + READ_TIMEOUT;
    if !flush_until(conn, deadline) {
        return None;
    }
    let r = read_until_eof(&mut rd, deadline);
    if r.is_none() {
        log::warn!("读取 {mime} 超时或出错（对方应用未应答）");
    }
    r
}

/// 把请求送出去；发送缓冲满时等连接可写再试，直到截止。
fn flush_until(conn: &Connection, deadline: Instant) -> bool {
    loop {
        match conn.flush() {
            Ok(()) => return true,
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    log::warn!("Wayland 发送缓冲一直满着，放弃本次读取");
                    return false;
                }
                let fd = conn.backend().poll_fd().as_raw_fd();
                super::super::sys::wait_io(&[], &[fd], Some(left));
            }
            Err(e) => {
                log::warn!("Wayland 连接写入失败：{e}");
                return false;
            }
        }
    }
}

/// 一轮非阻塞读的结果。
#[derive(Debug, PartialEq, Eq)]
enum ReadStep {
    /// 读到 EOF，数据齐了。
    Eof,
    /// 暂时没有更多数据。
    Pending,
    /// 出错或超过 [`READ_LIMIT`]。
    Failed,
}

/// 把 `rd` 里眼下能读的都读进 `out`。`rd` 须已设非阻塞。
fn read_available(rd: &mut File, out: &mut Vec<u8>) -> ReadStep {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match rd.read(&mut buf) {
            Ok(0) => return ReadStep::Eof,
            Ok(n) => {
                if out.len() + n > READ_LIMIT {
                    log::warn!("剪贴板 / 拖入数据超过 {} MB，已丢弃", READ_LIMIT >> 20);
                    return ReadStep::Failed;
                }
                out.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => return ReadStep::Pending,
            Err(_) => return ReadStep::Failed,
        }
    }
}

/// 读到 EOF；超时、出错或超过 [`READ_LIMIT`] 返回 `None`。`rd` 须已设非阻塞。
fn read_until_eof(rd: &mut File, deadline: Instant) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        match read_available(rd, &mut out) {
            ReadStep::Eof => return Some(out),
            ReadStep::Failed => return None,
            ReadStep::Pending => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return None;
                }
                super::super::sys::wait_readable(&[rd.as_raw_fd()], Some(left));
            }
        }
    }
}

/// 应答一次 `send`（`text` 是那个 source 自己的文本——被取代、尚未 `cancelled` 的旧 source
/// 也照它当时的内容答）：先写一轮，写不完的挂起等 `poll`。
fn on_send(text: &Arc<str>, fd: OwnedFd) {
    let file = File::from(fd);
    if let Err(e) = super::super::sys::set_nonblocking(&file) {
        log::warn!("剪贴板应答 fd 设为非阻塞失败：{e}");
        return;
    }
    let mut out = Outgoing {
        file,
        data: text.clone(),
        written: 0,
        progress: Instant::now(),
    };
    if !out.pump() {
        with_local(|c| c.sends.push(out));
    }
}

impl Outgoing {
    /// 尽量多写。返回 `true` = 结束（写完或出错），可以关 fd 了。
    fn pump(&mut self) -> bool {
        let bytes = self.data.as_bytes();
        while self.written < bytes.len() {
            match self.file.write(&bytes[self.written..]) {
                Ok(0) => return true,
                Ok(n) => {
                    self.written += n;
                    self.progress = Instant::now();
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => return false,
                // EPIPE：对方不读了、关了读端（Rust 运行时忽略 SIGPIPE，这里只是个错误）。
                Err(_) => return true,
            }
        }
        true
    }
}

/// 还有未写完的应答时，事件循环要等可写的 fd，以及最早的放弃时刻。
pub(super) fn pending_sends() -> (Vec<RawFd>, Option<Instant>) {
    CLIP.with(|c| {
        let c = c.borrow();
        let Some(c) = c.as_ref() else {
            return (Vec::new(), None);
        };
        let fds = c.sends.iter().map(|o| o.file.as_raw_fd()).collect();
        let deadline = c.sends.iter().map(|o| o.progress + SEND_STALL).min();
        (fds, deadline)
    })
}

/// 事件循环每轮调用：接着写挂起的应答，写完或卡住太久的关掉。
pub(super) fn pump_sends() {
    with_local(|c| {
        c.sends.retain_mut(|o| {
            if o.pump() {
                return false;
            }
            if o.progress.elapsed() >= SEND_STALL {
                log::warn!(
                    "剪贴板读取方 {} 秒没有读取，放弃应答（已写 {}/{} 字节）",
                    SEND_STALL.as_secs(),
                    o.written,
                    o.data.len()
                );
                return false;
            }
            true
        });
    });
}

// ── 协议事件 ──────────────────────────────────────────────────────────────

impl Dispatch<wl_data_device_manager::WlDataDeviceManager, ()> for Wl {
    fn event(
        _: &mut Self,
        _: &wl_data_device_manager::WlDataDeviceManager,
        _: wl_data_device_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_data_offer::WlDataOffer, OfferInfo> for Wl {
    fn event(
        _: &mut Self,
        _: &wl_data_offer::WlDataOffer,
        event: wl_data_offer::Event,
        info: &OfferInfo,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                if let Ok(mut m) = info.mimes.lock() {
                    m.push(mime_type);
                }
            }
            wl_data_offer::Event::Action {
                dnd_action: WEnum::Value(a),
            } => info.action.store(a.bits(), Ordering::Relaxed),
            _ => {}
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, SourceData> for Wl {
    fn event(
        _: &mut Self,
        source: &wl_data_source::WlDataSource,
        event: wl_data_source::Event,
        data: &SourceData,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_source::Event::Send { fd, .. } => on_send(&data.text, fd),
            // 选区被别人（或我们自己的新 source）取代。
            wl_data_source::Event::Cancelled => {
                let mut ours = false;
                with_local(|c| {
                    c.owner.forget(data.id);
                    let before = c.sources.len();
                    c.sources.retain(|s| s != source);
                    ours = c.sources.len() != before;
                });
                // 已被我们提前销毁的（不在列表里）不再销毁第二次。
                if ours {
                    source.destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, SelectionSync> for Wl {
    fn event(
        _: &mut Self,
        _: &wl_callback::WlCallback,
        event: wl_callback::Event,
        sync: &SelectionSync,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            with_local(|c| {
                if c.owner.on_synced(sync.0, c.focused) {
                    log::warn!("合成器没有采用这次剪贴板写入（输入 serial 无效，或窗口没有焦点）");
                    c.destroy_source(sync.0);
                }
            });
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for Wl {
    fn event(
        state: &mut Self,
        _: &wl_data_device::WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            // 新 offer 的 MIME 随后经它自己的 `offer` 事件到达（见 OfferInfo）。
            wl_data_device::Event::DataOffer { .. } => {}
            wl_data_device::Event::Selection { id } => with_local(|c| {
                let sel = match &id {
                    None => Sel::Empty,
                    Some(o) => marker_id(&offer_mimes(o), c.token).map_or(Sel::Foreign, Sel::Ours),
                };
                if let Some(old) = std::mem::replace(&mut c.selection, id) {
                    if Some(&old) != c.selection.as_ref() {
                        old.destroy();
                    }
                }
                c.sel = sel;
                for gone in c.owner.on_selection(sel) {
                    c.destroy_source(gone);
                }
            }),
            wl_data_device::Event::Enter {
                serial,
                surface,
                x,
                y,
                id,
            } => state.drag_enter(serial, &surface, (x, y), id),
            wl_data_device::Event::Motion { x, y, .. } => state.drag_motion((x, y)),
            wl_data_device::Event::Leave => {
                if let Some(d) = state.drag.take() {
                    d.offer.destroy();
                }
            }
            wl_data_device::Event::Drop => state.drag_drop(),
            _ => {}
        }
    }

    event_created_child!(Wl, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, OfferInfo::default()),
    ]);
}

// ── 拖入 ──────────────────────────────────────────────────────────────────

impl Wl {
    fn drag_enter(
        &mut self,
        serial: u32,
        surface: &wl_surface::WlSurface,
        pos: (f64, f64),
        offer: Option<wl_data_offer::WlDataOffer>,
    ) {
        if let Some(old) = self.drag.take() {
            old.offer.destroy();
        }
        // 没有 offer = 源端是同一客户端内部的拖动（我们不做拖出，不会出现），不理。
        let Some(offer) = offer else { return };
        let Some(key) = self.idx_of_surface(surface).map(|i| self.windows[i].key) else {
            offer.destroy();
            return;
        };
        let has_uris = offer_mimes(&offer).iter().any(|m| m == URI_LIST);
        let mut d = DropTarget {
            offer,
            serial,
            key,
            pos,
            has_uris,
            accepted: false,
        };
        let accept = self.drop_allowed(&d);
        set_accept(&mut d, accept, true);
        self.drag = Some(d);
    }

    fn drag_motion(&mut self, pos: (f64, f64)) {
        let Some(mut d) = self.drag.take() else {
            return;
        };
        d.pos = pos;
        // 拖动途中模态子窗开了 / 关了：改口，源端的光标随之变。
        let accept = self.drop_allowed(&d);
        set_accept(&mut d, accept, false);
        self.drag = Some(d);
    }

    fn drop_allowed(&self, d: &DropTarget) -> bool {
        d.has_uris && self.blocked_by_modal(d.key).is_none()
    }

    /// 放下：发出读取请求，读端交给事件循环（见 [`Wl::pump_drops`]），不在这里等。
    fn drag_drop(&mut self) {
        let Some(d) = self.drag.take() else { return };
        let file = if self.drop_allowed(&d) {
            start_receive(&d.offer, URI_LIST)
        } else {
            None
        };
        let Some(file) = file else {
            d.offer.destroy();
            return;
        };
        self.drops.push(PendingDrop {
            offer: d.offer,
            key: d.key,
            pos: d.pos,
            file,
            data: Vec::new(),
            deadline: Instant::now() + READ_TIMEOUT,
        });
    }

    /// 读端的 fd 与最早的放弃时刻，给事件循环的 `poll`。
    pub(super) fn pending_drops(&self) -> (Vec<RawFd>, Option<Instant>) {
        let fds = self.drops.iter().map(|d| d.file.as_raw_fd()).collect();
        (fds, self.drops.iter().map(|d| d.deadline).min())
    }

    /// 事件循环每轮调用：接着读正在进行的拖入，读完的交给窗口，超时的放弃。
    pub(super) fn pump_drops(&mut self) {
        if self.drops.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut done = Vec::new();
        let mut i = 0;
        while i < self.drops.len() {
            let d = &mut self.drops[i];
            match read_available(&mut d.file, &mut d.data) {
                ReadStep::Pending if now < d.deadline => i += 1,
                step => {
                    if step == ReadStep::Pending {
                        log::warn!(
                            "拖入的源端 {} 秒内没写完 uri-list，放弃",
                            READ_TIMEOUT.as_secs_f32()
                        );
                    }
                    done.push((self.drops.swap_remove(i), step == ReadStep::Eof));
                }
            }
        }
        for (d, ok) in done {
            self.finish_drop(d, ok);
        }
    }

    fn finish_drop(&mut self, d: PendingDrop, ok: bool) {
        // 读完期间窗口可能已经关了：没人收，就当失败（不 finish）。
        let target = self.idx(d.key).filter(|_| ok);
        let paths = match target {
            Some(_) => host::parse_uri_list(&d.data),
            None => Vec::new(),
        };
        // 读到了路径才算完成；合成器还要选定了动作，否则 `finish` 是协议错误（invalid_finish）。
        // 不 `finish` 直接销毁，源端收到 `cancelled`（与 XDND 回 Finished(accepted=0) 同义）。
        let action = d
            .offer
            .data::<OfferInfo>()
            .map_or(0, |i| i.action.load(Ordering::Relaxed));
        if !paths.is_empty() && d.offer.version() >= 3 && action != 0 {
            d.offer.finish();
        }
        d.offer.destroy();
        let (Some(i), false) = (target, paths.is_empty()) else {
            return;
        };
        let w = &mut self.windows[i];
        // 与 X11 同一口径：落点换成物理像素（宿主再按自己的缩放换回逻辑坐标去命中）。
        let pos = Point::new(
            w.scale.pos_to_physical(d.pos.0),
            w.scale.pos_to_physical(d.pos.1),
        );
        let r = {
            let _g = crate::platform::EventDispatchGuard::enter();
            w.handler.on_drop_files(pos, paths)
        };
        if r {
            w.needs_paint = true;
        }
        self.after_event(d.key);
    }
}

/// 告诉源端接不接受（`force`：`enter` 时无论如何都要表态一次）。只接受复制：接受「移动」的话，
/// 文件管理器会在放下后删掉源文件。
fn set_accept(d: &mut DropTarget, accept: bool, force: bool) {
    if !force && accept == d.accepted {
        return;
    }
    d.accepted = accept;
    if accept {
        d.offer.accept(d.serial, Some(URI_LIST.to_string()));
        if d.offer.version() >= 3 {
            d.offer.set_actions(DndAction::Copy, DndAction::Copy);
        }
    } else {
        // 只有网址 / 纯文本、或被模态挡住：回「不接受」，源端据此显示禁止光标（同 XDND）。
        d.offer.accept(d.serial, None);
        if d.offer.version() >= 3 {
            d.offer.set_actions(DndAction::empty(), DndAction::empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn text_mime_prefers_utf8_and_ignores_case() {
        let m = v(&["STRING", "text/plain", "TEXT/PLAIN;CHARSET=UTF-8"]);
        assert_eq!(
            pick_text_mime(&m),
            Some(("TEXT/PLAIN;CHARSET=UTF-8", Enc::Utf8)),
            "按 offer 原样的写法返回（receive 要原名）"
        );
        assert_eq!(
            pick_text_mime(&v(&["image/png", "STRING"])),
            Some(("STRING", Enc::Latin1))
        );
        assert_eq!(pick_text_mime(&v(&["image/png", "TEXT"])), None);
    }

    #[test]
    fn latin1_string_is_not_mangled_as_utf8() {
        assert_eq!(decode(b"caf\xe9", Enc::Latin1), "café");
        assert_eq!(decode("café".as_bytes(), Enc::Utf8), "café");
    }

    #[test]
    fn marker_identifies_only_this_process() {
        let m = v(&["text/plain", &marker(42, 7)]);
        assert_eq!(marker_id(&m, 42), Some(7));
        assert_eq!(
            marker_id(&m, 0x2),
            None,
            "别的 windui 进程（token 是本进程 token 的前缀）不算"
        );
        assert_eq!(marker_id(&v(&[&marker(0x2a0, 7)]), 0x2a), None);
        assert_ne!(
            process_token(),
            process_token(),
            "同一进程里每次取都不同，更别说跨进程"
        );
        assert_eq!(marker_id(&v(&["text/plain"]), 42), None);
    }

    #[test]
    fn copy_then_confirmed_reads_locally() {
        let mut o = Owner::default();
        let a = o.set();
        assert_eq!(o.plan(Sel::Foreign), ReadPlan::Local(a), "发出即以本地为准");
        assert!(o.on_selection(Sel::Ours(a)).is_empty());
        assert!(!o.on_synced(a, true));
        assert_eq!(o.plan(Sel::Ours(a)), ReadPlan::Local(a));
    }

    #[test]
    fn rejected_copy_falls_back_to_real_selection() {
        let mut o = Owner::default();
        let a = o.set();
        // 合成器不采用、也不发 cancelled：到 sync 回来都没见到自己的选区。
        assert!(o.on_synced(a, true), "有焦点却没确认 = 被拒，交调用方销毁");
        assert_eq!(o.plan(Sel::Foreign), ReadPlan::Pipe);
        assert_eq!(o.plan(Sel::Empty), ReadPlan::Empty);
    }

    #[test]
    fn unfocused_copy_is_kept_since_no_selection_event_is_expected() {
        let mut o = Owner::default();
        let a = o.set();
        assert!(!o.on_synced(a, false));
        assert_eq!(o.plan(Sel::Foreign), ReadPlan::Local(a));
        // 回到焦点时合成器补发当前选区：是别人的，说明我们那次被取代或被拒。
        o.on_selection(Sel::Foreign);
        assert_eq!(o.plan(Sel::Foreign), ReadPlan::Pipe);
    }

    #[test]
    fn foreign_selection_older_than_our_request_does_not_evict_it() {
        let mut o = Owner::default();
        let a = o.set();
        // 这条别人的选区事件是合成器处理我们的请求之前发的。
        o.on_selection(Sel::Foreign);
        assert_eq!(o.plan(Sel::Foreign), ReadPlan::Local(a));
        o.on_selection(Sel::Ours(a));
        o.on_synced(a, true);
        // 之后真被别人取代。
        o.on_selection(Sel::Foreign);
        assert_eq!(o.plan(Sel::Foreign), ReadPlan::Pipe);
    }

    #[test]
    fn newer_copy_in_flight_wins_and_older_sources_are_released() {
        let mut o = Owner::default();
        let a = o.set();
        let b = o.set();
        assert!(o.on_selection(Sel::Ours(a)).is_empty());
        assert_eq!(
            o.plan(Sel::Ours(a)),
            ReadPlan::Local(b),
            "b 还在路上，以 b 为准"
        );
        assert_eq!(
            o.on_selection(Sel::Ours(b)),
            vec![a],
            "a 被 b 取代，可以销毁"
        );
        assert_eq!(o.plan(Sel::Ours(b)), ReadPlan::Local(b));
    }

    #[test]
    fn live_own_offer_is_read_locally_even_after_a_newer_copy_was_rejected() {
        let mut o = Owner::default();
        let a = o.set();
        o.on_selection(Sel::Ours(a));
        o.on_synced(a, true);
        let b = o.set();
        assert!(o.on_synced(b, true), "b 被拒");
        assert_eq!(
            o.plan(Sel::Ours(a)),
            ReadPlan::Local(a),
            "选区仍是自家的 a：绝不能对它走管道（应答要等事件循环，会自锁到超时）"
        );
    }

    #[test]
    fn marker_copied_by_a_clipboard_manager_is_read_through_the_pipe() {
        let mut o = Owner::default();
        let a = o.set();
        o.on_selection(Sel::Ours(a));
        o.on_synced(a, true);
        // 管理器接手：我们的 source 先被 cancelled，随后它照抄的 offer（带我们的标记）成为选区。
        o.forget(a);
        o.on_selection(Sel::Ours(a));
        assert_eq!(o.plan(Sel::Ours(a)), ReadPlan::Pipe);
    }

    #[test]
    fn copy_rejected_while_unfocused_is_evicted_when_our_old_selection_comes_back() {
        let mut o = Owner::default();
        let a = o.set();
        o.on_selection(Sel::Ours(a));
        o.on_synced(a, true);
        let b = o.set();
        assert!(!o.on_synced(b, false), "失焦时无从判定，先留着");
        // 重获焦点：合成器补发的选区仍是 a，b 显然没被采用。
        assert_eq!(o.on_selection(Sel::Ours(a)), vec![b]);
        assert_eq!(o.plan(Sel::Ours(a)), ReadPlan::Local(a));
    }

    #[test]
    fn copy_rejected_while_unfocused_is_released_when_a_foreign_selection_comes_back() {
        let mut o = Owner::default();
        let a = o.set();
        o.on_synced(a, false);
        assert_eq!(
            o.on_selection(Sel::Foreign),
            vec![a],
            "从未确认过 = 被拒，释放"
        );
        assert_eq!(o.plan(Sel::Foreign), ReadPlan::Pipe);
        // 被接受过、后来被别人取代的，由 `cancelled` 释放，这里不重复。
        let b = o.set();
        o.on_selection(Sel::Ours(b));
        o.on_synced(b, true);
        assert!(o.on_selection(Sel::Foreign).is_empty());
    }

    #[test]
    fn several_copies_rejected_while_unfocused_are_all_released_on_our_old_selection() {
        let mut o = Owner::default();
        let a = o.set();
        o.on_selection(Sel::Ours(a));
        o.on_synced(a, true);
        let b = o.set();
        o.on_synced(b, false);
        let c = o.set();
        o.on_synced(c, false);
        let d = o.set(); // 还在路上
        let mut gone = o.on_selection(Sel::Ours(a));
        gone.sort();
        assert_eq!(gone, vec![b, c], "b 与 c 都被拒；在路上的 d 不动");
        assert_eq!(o.plan(Sel::Ours(a)), ReadPlan::Local(d));
    }

    #[test]
    fn several_copies_rejected_while_unfocused_are_all_released_on_a_foreign_selection() {
        let mut o = Owner::default();
        let a = o.set();
        o.on_synced(a, false);
        let b = o.set();
        o.on_synced(b, false);
        let mut gone = o.on_selection(Sel::Foreign);
        gone.sort();
        assert_eq!(gone, vec![a, b]);
        assert_eq!(o.plan(Sel::Foreign), ReadPlan::Pipe);
    }

    #[test]
    fn cancelled_clears_ownership() {
        let mut o = Owner::default();
        let a = o.set();
        o.on_selection(Sel::Ours(a));
        o.on_synced(a, true);
        o.forget(a);
        assert_eq!(o.plan(Sel::Foreign), ReadPlan::Pipe);
    }
}
