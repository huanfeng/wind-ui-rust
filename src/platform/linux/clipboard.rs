//! X11 剪贴板（`CLIPBOARD` 选区）。
//!
//! X 的剪贴板不是一块共享存储，而是「谁拥有选区，谁负责应答别人的读取请求」：复制之后
//! 程序必须一直在线应答 `SelectionRequest`。把这件事放在主事件循环里会让 `set_text`
//! 与窗口生命周期缠在一起（最后一个窗口关了，剪贴板内容也就没了应答者），故这里起
//! 一个**独立线程 + 独立连接 + 隐藏窗口**专职做选区的拥有与读取，与 UI 线程只经
//! 通道交互。
//!
//! 已知限制：不支持 INCR 分段传输（读超过服务器单请求上限的大段文本会得到空）。

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

/// 剪贴板实现，由 `UiHost` 注入 `Tree`。
pub struct X11Clipboard;

impl ClipboardProvider for X11Clipboard {
    fn get_text(&self) -> Option<String> {
        let (tx, rx) = mpsc::channel();
        send(Req::Get(tx))?;
        rx.recv_timeout(Duration::from_millis(1500)).ok().flatten()
    }
    fn set_text(&self, text: &str) {
        let _ = send(Req::Set(text.to_string()));
    }
}

enum Req {
    Set(String),
    Get(mpsc::Sender<Option<String>>),
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
    let mut owned: Option<String> = None;
    loop {
        // 先处理积压请求，再阻塞等事件（唤醒靠 `send` 投来的 ClientMessage）。
        while let Ok(req) = rx.try_recv() {
            match req {
                Req::Set(s) => {
                    owned = Some(s);
                    let _ = conn.set_selection_owner(win, atoms.CLIPBOARD, CURRENT_TIME);
                    let _ = conn.flush();
                }
                Req::Get(reply) => {
                    let text = match &owned {
                        Some(s) => Some(s.clone()),
                        None => fetch(&conn, win, &atoms, &mut owned),
                    };
                    let _ = reply.send(text);
                }
            }
        }
        let Ok(ev) = conn.wait_for_event() else {
            return; // 连接断了：X 服务器已退出，剪贴板随之失效
        };
        handle(&conn, &atoms, &mut owned, ev);
    }
}

fn handle(conn: &RustConnection, atoms: &Atoms, owned: &mut Option<String>, ev: Event) {
    match ev {
        Event::SelectionRequest(req) => answer(conn, atoms, owned.as_deref(), &req),
        Event::SelectionClear(_) => *owned = None,
        _ => {}
    }
}

/// 应答别的程序对我们选区的读取。
fn answer(conn: &RustConnection, atoms: &Atoms, owned: Option<&str>, req: &SelectionRequestEvent) {
    // 老客户端会把 property 填 NONE，按 ICCCM 以 target 充当 property。
    let prop = if req.property == u32::from(AtomEnum::NONE) {
        req.target
    } else {
        req.property
    };
    let mut ok = false;
    if let Some(text) = owned {
        if req.target == atoms.TARGETS {
            let targets = [
                atoms.TARGETS,
                atoms.UTF8_STRING,
                atoms.TEXT,
                AtomEnum::STRING.into(),
            ];
            ok = conn
                .change_property32(
                    PropMode::REPLACE,
                    req.requestor,
                    prop,
                    AtomEnum::ATOM,
                    &targets,
                )
                .is_ok();
        } else if req.target == atoms.UTF8_STRING
            || req.target == atoms.TEXT
            || req.target == u32::from(AtomEnum::STRING)
        {
            ok = conn
                .change_property8(
                    PropMode::REPLACE,
                    req.requestor,
                    prop,
                    req.target,
                    text.as_bytes(),
                )
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
    owned: &mut Option<String>,
) -> Option<String> {
    for target in [atoms.UTF8_STRING, u32::from(AtomEnum::STRING)] {
        if let Some(t) = fetch_as(conn, win, atoms, target, owned) {
            return Some(t);
        }
    }
    None
}

fn fetch_as(
    conn: &RustConnection,
    win: Window,
    atoms: &Atoms,
    target: Atom,
    owned: &mut Option<String>,
) -> Option<String> {
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
                    log::warn!("剪贴板内容过大（INCR 分段传输），暂不支持");
                    return None;
                }
                return Some(String::from_utf8_lossy(&reply.value).into_owned());
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
