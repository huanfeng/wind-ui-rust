//! 剪贴板：对外的 [`LinuxClipboard`]，以及 X11 的实现（`CLIPBOARD` 选区）。
//!
//! 在用原生 Wayland 后端时，读写转给 `wayland::data`（`wl_data_device`）；否则走这里的
//! X11 实现（Wayland 会话经 XWayland 时也是它）。
//!
//! X 的剪贴板不是一块共享存储，而是「谁拥有选区，谁负责应答别人的读取请求」：复制之后
//! 程序必须一直在线应答 `SelectionRequest`。把这件事放在主事件循环里会让 `set_text`
//! 与窗口生命周期缠在一起（最后一个窗口关了，剪贴板内容也就没了应答者），故这里起
//! 一个**独立线程 + 独立连接 + 隐藏窗口**专职做选区的拥有与读取，与 UI 线程只经
//! 通道交互。
//!
//! 图片：X11 下读写 `image/png` target（PNG 编解码见 [`crate::platform::clip_image`]）；
//! 读取支持 INCR 分段（截图工具给出的 PNG 常超过单请求上限）。原生 Wayland 不支持图片
//! （`get_image` / `set_image` 走默认实现）。
//!
//! 已知限制：读文本不支持 INCR 分段传输（读超过服务器单请求上限的大段文本会得到空）；
//! 写图片受服务器单请求上限（BIG-REQUESTS 下通常 16MB）约束，超限则对方读到空。

use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use crate::core::ClipboardProvider;
use crate::platform::clip_image::{png_to_rgba, rgba_to_png};
use crate::render::DecodedImage;

/// 剪贴板实现，由 `UiHost` 注入 `Tree`。
pub struct LinuxClipboard;

impl ClipboardProvider for LinuxClipboard {
    fn get_text(&self) -> Option<String> {
        #[cfg(feature = "wayland")]
        if let Some(r) = super::wayland::clipboard_get() {
            return r;
        }
        let (tx, rx) = mpsc::channel();
        send(Req::Get(tx))?;
        rx.recv_timeout(Duration::from_millis(1500)).ok().flatten()
    }
    fn set_text(&self, text: &str) {
        #[cfg(feature = "wayland")]
        if super::wayland::clipboard_set(text) {
            return;
        }
        let _ = send(Req::Set(Content::Text(text.to_string())));
    }
    fn get_image(&self) -> Option<DecodedImage> {
        #[cfg(feature = "wayland")]
        if super::wayland::clipboard_active() {
            return None; // 原生 Wayland 暂不支持图片
        }
        let (tx, rx) = mpsc::channel();
        send(Req::GetPng(tx))?;
        // 比文本多留时间：大图要走 INCR 分段（`fetch_as` 内自带 3 秒总上限）。
        let png = rx
            .recv_timeout(Duration::from_millis(3500))
            .ok()
            .flatten()?;
        png_to_rgba(&png)
    }
    fn set_image(&self, img: &DecodedImage) -> bool {
        #[cfg(feature = "wayland")]
        if super::wayland::clipboard_active() {
            return false;
        }
        match rgba_to_png(img) {
            Some(png) => send(Req::Set(Content::Png(png))).is_some(),
            None => false,
        }
    }
}

/// 我们当前作为选区拥有者提供的内容（同一时刻只有一种）。
enum Content {
    Text(String),
    Png(Vec<u8>),
}

enum Req {
    Set(Content),
    Get(mpsc::Sender<Option<String>>),
    GetPng(mpsc::Sender<Option<Vec<u8>>>),
}

struct Worker {
    tx: Mutex<mpsc::Sender<Req>>,
    /// 往剪贴板线程的 X 连接上投一条 ClientMessage 来唤醒它（它阻塞在 `wait_for_event`）。
    wake_conn: RustConnection,
    wake_win: Window,
}

fn send(req: Req) -> Option<()> {
    static W: OnceLock<Option<Worker>> = OnceLock::new();
    let w = W.get_or_init(spawn).as_ref()?;
    w.tx.lock().ok()?.send(req).ok()?;
    let ev = xproto::ClientMessageEvent::new(8, w.wake_win, AtomEnum::NONE, [0u8; 20]);
    let _ = w
        .wake_conn
        .send_event(false, w.wake_win, EventMask::NO_EVENT, ev);
    let _ = w.wake_conn.flush();
    Some(())
}

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        UTF8_STRING,
        IMAGE_PNG: b"image/png",
        TARGETS,
        TEXT,
        INCR,
        WINDUI_CLIP,
    }
}

fn spawn() -> Option<Worker> {
    let (conn, screen) = RustConnection::connect(None).ok()?;
    let root = conn.setup().roots[screen].root;
    let win = conn.generate_id().ok()?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )
    .ok()?;
    let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
    conn.flush().ok()?;
    let (wake_conn, _) = RustConnection::connect(None).ok()?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("windui-clipboard".into())
        .spawn(move || run(conn, win, atoms, rx))
        .ok()?;
    Some(Worker {
        tx: Mutex::new(tx),
        wake_conn,
        wake_win: win,
    })
}

fn run(conn: RustConnection, win: Window, atoms: Atoms, rx: mpsc::Receiver<Req>) {
    let mut owned: Option<Content> = None;
    loop {
        // 先处理积压请求，再阻塞等事件（唤醒靠 `send` 投来的 ClientMessage）。
        while let Ok(req) = rx.try_recv() {
            match req {
                Req::Set(c) => {
                    owned = Some(c);
                    let _ = conn.set_selection_owner(win, atoms.CLIPBOARD, CURRENT_TIME);
                    let _ = conn.flush();
                }
                Req::Get(reply) => {
                    let text = match &owned {
                        Some(Content::Text(s)) => Some(s.clone()),
                        Some(Content::Png(_)) => None, // 自己持有的是图片：没有文本
                        None => fetch(&conn, win, &atoms, &mut owned),
                    };
                    let _ = reply.send(text);
                }
                Req::GetPng(reply) => {
                    let png = match &owned {
                        Some(Content::Png(b)) => Some(b.clone()),
                        Some(Content::Text(_)) => None,
                        None => fetch_as(&conn, win, &atoms, atoms.IMAGE_PNG, &mut owned),
                    };
                    let _ = reply.send(png);
                }
            }
        }
        let Ok(ev) = conn.wait_for_event() else {
            return; // 连接断了：X 服务器已退出，剪贴板随之失效
        };
        handle(&conn, &atoms, &mut owned, ev);
    }
}

fn handle(conn: &RustConnection, atoms: &Atoms, owned: &mut Option<Content>, ev: Event) {
    match ev {
        Event::SelectionRequest(req) => answer(conn, atoms, owned.as_ref(), &req),
        Event::SelectionClear(_) => *owned = None,
        _ => {}
    }
}

/// 应答别的程序对我们选区的读取。
fn answer(
    conn: &RustConnection,
    atoms: &Atoms,
    owned: Option<&Content>,
    req: &SelectionRequestEvent,
) {
    // 老客户端会把 property 填 NONE，按 ICCCM 以 target 充当 property。
    let prop = if req.property == u32::from(AtomEnum::NONE) {
        req.target
    } else {
        req.property
    };
    let mut ok = false;
    if let Some(content) = owned {
        if req.target == atoms.TARGETS {
            let targets: &[Atom] = match content {
                Content::Text(_) => &[
                    atoms.TARGETS,
                    atoms.UTF8_STRING,
                    atoms.TEXT,
                    AtomEnum::STRING.into(),
                ],
                Content::Png(_) => &[atoms.TARGETS, atoms.IMAGE_PNG],
            };
            ok = conn
                .change_property32(
                    PropMode::REPLACE,
                    req.requestor,
                    prop,
                    AtomEnum::ATOM,
                    targets,
                )
                .is_ok();
        } else if let (Content::Text(text), true) = (
            content,
            req.target == atoms.UTF8_STRING
                || req.target == atoms.TEXT
                || req.target == u32::from(AtomEnum::STRING),
        ) {
            ok = conn
                .change_property8(
                    PropMode::REPLACE,
                    req.requestor,
                    prop,
                    req.target,
                    text.as_bytes(),
                )
                .is_ok();
        } else if let (Content::Png(png), true) = (content, req.target == atoms.IMAGE_PNG) {
            ok = conn
                .change_property8(PropMode::REPLACE, req.requestor, prop, req.target, png)
                .is_ok();
        }
    }
    let notify = SelectionNotifyEvent {
        response_type: xproto::SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: req.time,
        requestor: req.requestor,
        selection: req.selection,
        target: req.target,
        property: if ok { prop } else { AtomEnum::NONE.into() },
    };
    let _ = conn.send_event(false, req.requestor, EventMask::NO_EVENT, notify);
    let _ = conn.flush();
}

/// 向当前选区拥有者要文本：先要 UTF8_STRING，被拒再要 STRING。
fn fetch(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    owned: &mut Option<Content>,
) -> Option<String> {
    for target in [atoms.UTF8_STRING, u32::from(AtomEnum::STRING)] {
        if let Some(t) = fetch_as(conn, win, atoms, target, owned) {
            return Some(String::from_utf8_lossy(&t).into_owned());
        }
    }
    None
}

/// 向当前选区拥有者要指定 target 的原始字节。
fn fetch_as(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    target: Atom,
    owned: &mut Option<Content>,
) -> Option<Vec<u8>> {
    conn.convert_selection(
        win,
        atoms.CLIPBOARD,
        target,
        atoms.WINDUI_CLIP,
        CURRENT_TIME,
    )
    .ok()?;
    conn.flush().ok()?;
    let deadline = Instant::now() + Duration::from_millis(1000);
    loop {
        // 等应答期间照样要应答别人的请求（对方可能恰好在读我们的选区）。
        match conn.poll_for_event().ok()? {
            Some(Event::SelectionNotify(n)) if n.requestor == win => {
                if n.property == u32::from(AtomEnum::NONE) {
                    return None;
                }
                let reply = conn
                    .get_property(true, win, atoms.WINDUI_CLIP, AtomEnum::ANY, 0, u32::MAX / 4)
                    .ok()?
                    .reply()
                    .ok()?;
                if reply.type_ == atoms.INCR {
                    // 只有图片读取走 INCR（文本读取沿用既有的「不支持」）。
                    if target != atoms.IMAGE_PNG {
                        log::warn!("剪贴板内容过大（INCR 分段传输），暂不支持");
                        return None;
                    }
                    return fetch_incr(conn, win, atoms, owned);
                }
                return Some(reply.value);
            }
            Some(ev) => handle(conn, atoms, owned, ev),
            None => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// INCR 分段接收：上面那次 `get_property(delete=true)` 已通知拥有者开始发送；之后每次
/// 它把一段放进我们窗口的属性并触发 `PropertyNotify(NEW_VALUE)`，我们取走（顺带删除属性
/// 以通知它发下一段），直到收到长度为 0 的一段。总时长与总量都设上限，防止对端拖死或撑爆内存。
fn fetch_incr(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    owned: &mut Option<Content>,
) -> Option<Vec<u8>> {
    const MAX_BYTES: usize = 64 * 1024 * 1024;
    let deadline = Instant::now() + Duration::from_millis(3000);
    let mut data = Vec::new();
    loop {
        match conn.poll_for_event().ok()? {
            Some(Event::PropertyNotify(p))
                if p.window == win
                    && p.atom == atoms.WINDUI_CLIP
                    && p.state == xproto::Property::NEW_VALUE =>
            {
                let reply = conn
                    .get_property(true, win, atoms.WINDUI_CLIP, AtomEnum::ANY, 0, u32::MAX / 4)
                    .ok()?
                    .reply()
                    .ok()?;
                if reply.value.is_empty() {
                    return Some(data);
                }
                data.extend_from_slice(&reply.value);
                if data.len() > MAX_BYTES {
                    log::warn!("剪贴板图片超过 {MAX_BYTES} 字节上限，放弃读取");
                    return None;
                }
            }
            Some(ev) => handle(conn, atoms, owned, ev),
            None => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}
