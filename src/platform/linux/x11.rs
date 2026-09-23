//! X11 窗口与事件循环（x11rb，纯 Rust 协议实现）。
//!
//! # 与 win32 / macOS 的一个根本差异：没有同步重入
//!
//! X 是异步协议：`MapWindow`、`ConfigureWindow` 之类的请求只是写进发送缓冲，结果以事件的
//! 形式在**之后的某轮循环**里回来，从不在调用栈上同步回调我们。铁律 6（OS 重入前释放
//! 借用）在这里天然成立——宿主回调与 X 请求可以在同一个借用里先后发生。仍然沿用
//! 「事件分发 → `after_event` 收尾」的两段结构，是为了与另两个平台的语义逐项对齐
//! （窗口操作、对话框、开窗、关窗的时机），不是为了规避重入。
//!
//! # 呈现
//!
//! tiny-skia 画进每窗一份 `Pixmap`（RGBA 预乘），上屏时把本帧重画的矩形换成服务器的
//! 像素字节序（通常 BGRX），经 `PutImage` 送出。窗口背景设成 `None` 之外的底色、
//! `bit_gravity = NorthWest`，缩放时服务器保留旧内容、只清新露出的部分，避免闪白。
//!
//! # 帧配速
//!
//! 与 win32 同一套：无动画时阻塞在 `poll`（零 CPU）；有动画时按
//! `max(刷新率间隔, 控件自报的下次变化时刻)` 定超时。X 没有垂直同步信号可等（软件呈现
//! 不经合成器的 present 路径），刷新率按 60Hz 计。

use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use tiny_skia::Pixmap;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::properties::{WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::xproto::{
    self, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Gravity, ImageFormat, ImageOrder,
    PropMode, StackMode, Window, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::CURRENT_TIME;

use super::dnd::{self, DndAtoms, DragState};
use super::hotkey::Hotkeys;
use super::ime::{Ime, ImeOut};
use super::keys::{self, Keymap};
use crate::event::{
    CursorShape, Key, KeyEvent, MouseButton, PointerEvent, PointerKind, Preedit, WindowOp,
};
use crate::geometry::{Color, Point, Rect, Size};
use crate::platform::{to_skia_color, AppHandler, NewWindow, WindowConfig};

x11rb::atom_manager! {
    pub(super) Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_CHANGE_STATE,
        _NET_WM_NAME,
        UTF8_STRING,
        _NET_WM_PID,
        _NET_WM_ICON,
        _NET_WM_STATE,
        _NET_WM_STATE_MAXIMIZED_VERT,
        _NET_WM_STATE_MAXIMIZED_HORZ,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_STATE_MODAL,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_WINDOW_TYPE_DIALOG,
        _NET_ACTIVE_WINDOW,
        _NET_WM_MOVERESIZE,
        _MOTIF_WM_HINTS,
        RESOURCE_MANAGER,
    }
}

/// 动画帧间隔（ms）。X 上拿不到可靠的刷新率，按 60Hz。
const FRAME_MS: u64 = 16;
/// 控件自报的「下次变化」最长只睡这么久（与 win32 `MAX_FRAME_DELAY_MS` 同值）。
const MAX_FRAME_DELAY_MS: u64 = 5000;
/// 双击判定：时限与漂移（逻辑像素，按 scale 换算）。与 `platform::double_click_thresholds` 一致。
const DOUBLE_CLICK_MS: u32 = 400;
const DOUBLE_CLICK_SLOP: f32 = 4.0;
/// 无边框窗口的缩放边宽（逻辑像素）。
const RESIZE_BORDER: f32 = 6.0;

/// 上屏转换缓冲的上限（字节），见 `X11::upload`。
const UPLOAD_CHUNK: usize = 256 * 1024;

/// `_NET_WM_MOVERESIZE` 的方向码。
const MOVERESIZE_MOVE: u32 = 8;

/// 每个窗口的状态。
struct Win {
    id: Window,
    handler: Box<dyn AppHandler>,
    bg: Color,
    frameless: bool,
    resizable: bool,
    single: Option<String>,
    owner: Option<Window>,
    modal: bool,
    /// 物理像素尺寸。
    w: i32,
    h: i32,
    pixmap: Option<Pixmap>,
    /// 缓冲刚重建：宿主本帧必须画整窗。
    fresh: bool,
    needs_paint: bool,
    /// 服务器报来的暴露区（需要从 pixmap 重传，不必重画）。
    exposed: Option<Rect>,
    mapped: bool,
    /// 被我们隐藏（`WindowOp::Hide`、最小化即隐藏、启动即隐藏）。只有从这个状态唤起才发
    /// `on_window_shown`——契约是「隐藏→可见」的跃迁，不含首次显示与从最小化还原。
    hidden: bool,
    maximized: bool,
    minimized: bool,
    intervals: Vec<(Duration, Instant)>,
    capturing: bool,
    cursor: CursorShape,
    click: ClickTracker,
    /// 无边框窗口：在拖动区 / 缩放边上按下、尚未移动够阈值的待定拖动（方向码 + 按下点根坐标）。
    ///
    /// 不在按下时就交给 WM：`_NET_WM_MOVERESIZE` 到达 WM 时松开可能已经发生，WM 于是一直
    /// 停在移动态、把下一次点击当作「结束移动」吃掉——双击最大化因此永远凑不齐第二下。
    /// 移动超过阈值才发，与 GTK 的做法一致。
    pending_drag: Option<(u32, i16, i16)>,
    /// 这一次左键按下被标题栏 / 缩放边接管了，配对的松开也不下发给控件（含双击切最大化
    /// 那一下——那时没有待定拖动，只靠这个标志吞掉松开）。
    swallow_up: bool,
    /// 输入法合成进行中（已向宿主推过非空合成串）。
    composing: bool,
}

#[derive(Default)]
struct ClickTracker {
    time: u32,
    pos: (i32, i32),
    button: u8,
    count: u8,
}

impl ClickTracker {
    /// 折进 1 / 2 的循环（见 `PointerEvent::click_count`）。
    fn press(&mut self, time: u32, pos: (i32, i32), button: u8, slop: i32) -> u8 {
        let near = (pos.0 - self.pos.0).abs() <= slop && (pos.1 - self.pos.1).abs() <= slop;
        let quick = time.wrapping_sub(self.time) <= DOUBLE_CLICK_MS;
        self.count = if near && quick && button == self.button && self.count == 1 {
            2
        } else {
            1
        };
        self.time = time;
        self.pos = pos;
        self.button = button;
        self.count
    }
}

/// 像素上屏所需的服务器格式信息。
struct PixelFormat {
    depth: u8,
    /// true = 小端（B,G,R,X），false = 大端（X,R,G,B）。
    lsb: bool,
}

struct X11 {
    conn: Rc<RustConnection>,
    /// XIM 输入法连接（没配置 / 连不上为 `None`，按键走本地）。
    ime: Option<Ime>,
    /// 全局热键（未配置为 `None`）。
    hotkeys: Option<Hotkeys>,
    dnd_atoms: DndAtoms,
    /// 进行中的文件拖入。
    drag: Option<DragState>,
    /// 上屏转换缓冲（复用，容量不超过 `UPLOAD_CHUNK`）。
    upload_buf: Vec<u8>,
    /// 主窗口：托盘 / 热键的「唤出窗口」指的是它。
    main: Window,
    atoms: Atoms,
    root: Window,
    screen_w: i32,
    screen_h: i32,
    fmt: PixelFormat,
    gc: xproto::Gcontext,
    scale: f32,
    cursors: [xproto::Cursor; 5],
    keymap: Keymap,
    windows: Vec<Win>,
    last_anim_frame: Instant,
    /// 纯修饰键之外是否按过别的键——判「单击 Alt」用不到这里（宿主自己判），
    /// 这里只防止 X 的自动重复把 Alt 按住报成一串按下。
    alt_down: bool,
}

/// 连接 X 服务器并运行，阻塞至最后一个窗口关闭。
pub(super) fn run_windowed(
    mut cfg: WindowConfig,
    handler: Box<dyn AppHandler>,
    waker: Option<std::sync::Arc<crate::sync::WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
) {
    let (conn, screen_num) = match RustConnection::connect(None) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[windui] 无法连接 X 服务器（DISPLAY={:?}）：{e}。Wayland 会话需要启用 XWayland。",
                std::env::var("DISPLAY").ok()
            );
            return;
        }
    };
    let Some(mut x) = X11::new(conn, screen_num) else {
        eprintln!("[windui] X 服务器初始化失败（不支持的像素格式或原子请求失败）");
        return;
    };
    if cfg.tray.is_some() {
        log::warn!("Linux 后端暂不支持系统托盘，托盘配置被忽略");
    }
    if cfg.renderer.requires_gpu() {
        eprintln!("[windui] Renderer::Gpu 在 Linux 后端尚不可用，改用软件渲染");
    }
    let main = x.create_window(&cfg, handler, None);
    x.main = main;
    if cfg.start_hidden {
        if let Some(i) = x.idx(main) {
            x.windows[i].hidden = true;
        }
    }
    if !cfg.hotkeys.is_empty() {
        let bindings = std::mem::take(&mut cfg.hotkeys);
        x.hotkeys = Some(Hotkeys::install(&x.conn, x.root, &x.keymap, bindings));
    }
    if !cfg.start_hidden {
        x.show(main);
    }

    if let Some(w) = &waker {
        w.bind(Box::new(LinuxWake));
    }
    if let Some(si) = single {
        crate::single_instance::install_listener(&si.app_id, main as isize, si.on_second);
    }
    x.run_loop(main);
}

/// 跨线程唤醒：写唤醒管道，事件循环醒来后标脏所有窗口（宿主 render 时排空消息通道）。
struct LinuxWake;
impl crate::sync::RawWakeSignal for LinuxWake {
    fn signal(&self) {
        super::sys::wake();
    }
}

impl X11 {
    fn new(conn: RustConnection, screen_num: usize) -> Option<Self> {
        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let root = screen.root;
        let depth = screen.root_depth;
        let bpp = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .map(|f| f.bits_per_pixel)?;
        // 只收 8bit 通道的真彩：depth 30（10bit 通道）也是 32bpp，按 BGRX 写进去颜色全错。
        if bpp != 32 || !(depth == 24 || depth == 32) {
            log::error!("X 屏幕像素格式 depth={depth} bpp={bpp} 不受支持（需要 24/32 位真彩）");
            return None;
        }
        let fmt = PixelFormat {
            depth,
            lsb: setup.image_byte_order == ImageOrder::LSB_FIRST,
        };
        let (screen_w, screen_h) = (
            screen.width_in_pixels as i32,
            screen.height_in_pixels as i32,
        );
        let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
        let dnd_atoms = DndAtoms::new(&conn).ok()?.reply().ok()?;
        let gc = conn.generate_id().ok()?;
        conn.create_gc(gc, root, &CreateGCAux::new().graphics_exposures(0))
            .ok()?;
        let scale = detect_scale(&conn, root, &atoms);
        let cursors = create_cursors(&conn).unwrap_or([0; 5]);
        let conn = Rc::new(conn);
        let ime = Ime::connect(conn.clone(), screen_num);
        let mut x = X11 {
            conn,
            ime,
            hotkeys: None,
            main: 0,
            dnd_atoms,
            drag: None,
            upload_buf: Vec::new(),
            atoms,
            root,
            screen_w,
            screen_h,
            fmt,
            gc,
            scale,
            cursors,
            keymap: Keymap::default(),
            windows: Vec::new(),
            last_anim_frame: Instant::now(),
            alt_down: false,
        };
        x.reload_keymap();
        Some(x)
    }

    fn reload_keymap(&mut self) {
        let setup = self.conn.setup();
        let (min, max) = (setup.min_keycode, setup.max_keycode);
        if let Ok(Ok(r)) = self
            .conn
            .get_keyboard_mapping(min, max - min + 1)
            .map(|c| c.reply())
        {
            self.keymap = Keymap::new(min, r.keysyms_per_keycode, r.keysyms);
        }
    }

    fn idx(&self, id: Window) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    // ── 建窗 ──────────────────────────────────────────────────────────────

    fn create_window(
        &mut self,
        cfg: &WindowConfig,
        mut handler: Box<dyn AppHandler>,
        owner: Option<Window>,
    ) -> Window {
        let s = self.scale;
        let pw = ((cfg.width as f32 * s).round() as i32).max(1);
        let ph = ((cfg.height as f32 * s).round() as i32).max(1);
        let (mut x, mut y) = (0i32, 0i32);
        if cfg.centered {
            let (ox, oy, ow, oh) = owner.and_then(|o| self.frame_of(o)).unwrap_or((
                0,
                0,
                self.screen_w,
                self.screen_h,
            ));
            x = ox + (ow - pw) / 2;
            y = oy + (oh - ph) / 2;
        }
        let id = self.conn.generate_id().expect("X 资源 id 耗尽");
        let mask = EventMask::EXPOSURE
            | EventMask::STRUCTURE_NOTIFY
            | EventMask::KEY_PRESS
            | EventMask::KEY_RELEASE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::POINTER_MOTION
            | EventMask::LEAVE_WINDOW
            | EventMask::FOCUS_CHANGE
            | EventMask::PROPERTY_CHANGE;
        let bg = cfg.bg;
        let _ = self.conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            id,
            self.root,
            x as i16,
            y as i16,
            pw as u16,
            ph as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new()
                .event_mask(mask)
                .background_pixel(((bg.r as u32) << 16) | ((bg.g as u32) << 8) | bg.b as u32)
                .bit_gravity(Gravity::NORTH_WEST)
                .cursor(self.cursors[0]),
        );
        let a = self.atoms;
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            id,
            a.WM_PROTOCOLS,
            AtomEnum::ATOM,
            &[a.WM_DELETE_WINDOW],
        );
        // 声明接受 XDND 拖放（文件拖入，见 `dnd.rs`）。
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            id,
            self.dnd_atoms.XdndAware,
            AtomEnum::ATOM,
            &[dnd::XDND_VERSION],
        );
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            id,
            a._NET_WM_PID,
            AtomEnum::CARDINAL,
            &[std::process::id()],
        );
        self.set_title(id, &cfg.title);
        let (inst, class) = wm_class();
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            format!("{inst}\0{class}\0").as_bytes(),
        );
        // 尺寸约束：最小客户区（逻辑 → 物理）；不可缩放即最小 = 最大 = 当前。
        let mut hints = WmSizeHints::new();
        if cfg.centered {
            hints.position = Some((WmSizeHintsSpecification::ProgramSpecified, x, y));
        }
        if !cfg.resizable {
            hints.min_size = Some((pw, ph));
            hints.max_size = Some((pw, ph));
        } else if cfg.min_width > 0 || cfg.min_height > 0 {
            hints.min_size = Some((
                (cfg.min_width.max(0) as f32 * s).round() as i32,
                (cfg.min_height.max(0) as f32 * s).round() as i32,
            ));
        }
        let _ = hints.set_normal_hints(&*self.conn, id);
        if cfg.frameless {
            // Motif 提示：flags = MWM_HINTS_DECORATIONS，decorations = 0。所有主流 WM 都认。
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                id,
                a._MOTIF_WM_HINTS,
                a._MOTIF_WM_HINTS,
                &[2, 0, 0, 0, 0],
            );
        }
        let wtype = if owner.is_some() {
            a._NET_WM_WINDOW_TYPE_DIALOG
        } else {
            a._NET_WM_WINDOW_TYPE_NORMAL
        };
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            id,
            a._NET_WM_WINDOW_TYPE,
            AtomEnum::ATOM,
            &[wtype],
        );
        if let Some(o) = owner {
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                id,
                AtomEnum::WM_TRANSIENT_FOR,
                AtomEnum::WINDOW,
                &[o],
            );
            if cfg.modal {
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    id,
                    a._NET_WM_STATE,
                    AtomEnum::ATOM,
                    &[a._NET_WM_STATE_MODAL],
                );
            }
        }
        if let Some(src) = &cfg.icon {
            self.set_icon(id, src);
        }

        handler.set_scale(s);
        let now = Instant::now();
        let intervals = handler
            .intervals()
            .into_iter()
            // 下限 10ms（同 win32 `SetTimer` 的最小周期）：周期 0 会让循环超时恒为 0、空转。
            .map(|d| d.max(Duration::from_millis(10)))
            .map(|d| (d, now + d))
            .collect();
        self.windows.push(Win {
            id,
            handler,
            bg,
            frameless: cfg.frameless,
            resizable: cfg.resizable,
            single: cfg.single.clone(),
            owner,
            modal: cfg.modal && owner.is_some(),
            w: pw,
            h: ph,
            pixmap: None,
            fresh: true,
            needs_paint: true,
            exposed: None,
            mapped: false,
            hidden: false,
            maximized: false,
            minimized: false,
            intervals,
            capturing: false,
            cursor: CursorShape::Arrow,
            click: ClickTracker::default(),
            pending_drag: None,
            swallow_up: false,
            composing: false,
        });
        if let Some(ime) = &mut self.ime {
            ime.create_ic(id);
        }
        id
    }

    /// 窗口在根窗口坐标系里的矩形（含装饰前的客户区）。
    fn frame_of(&self, id: Window) -> Option<(i32, i32, i32, i32)> {
        let g = self.conn.get_geometry(id).ok()?.reply().ok()?;
        let t = self
            .conn
            .translate_coordinates(id, self.root, 0, 0)
            .ok()?
            .reply()
            .ok()?;
        Some((
            t.dst_x as i32,
            t.dst_y as i32,
            g.width as i32,
            g.height as i32,
        ))
    }

    fn set_title(&self, id: Window, title: &str) {
        let a = self.atoms;
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            id,
            a._NET_WM_NAME,
            a.UTF8_STRING,
            title.as_bytes(),
        );
        let _ = self.conn.change_property8(
            PropMode::REPLACE,
            id,
            AtomEnum::WM_NAME,
            a.UTF8_STRING,
            title.as_bytes(),
        );
    }

    /// `_NET_WM_ICON`：多尺寸 ARGB，WM 按需挑（标题栏 16/24、任务栏 32/48、Alt-Tab 64+）。
    fn set_icon(&self, id: Window, src: &crate::icon::IconSource) {
        let mut data: Vec<u32> = Vec::new();
        for px in [16u32, 32, 64] {
            let icon = src.at((px as f32 * self.scale).round() as u32);
            data.push(icon.width());
            data.push(icon.height());
            data.extend(icon.rgba().as_chunks::<4>().0.iter().map(|p| {
                ((p[3] as u32) << 24) | ((p[0] as u32) << 16) | ((p[1] as u32) << 8) | p[2] as u32
            }));
        }
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            id,
            self.atoms._NET_WM_ICON,
            AtomEnum::CARDINAL,
            &data,
        );
    }

    fn show(&mut self, id: Window) {
        if let Some(i) = self.idx(id) {
            let w = &mut self.windows[i];
            if std::mem::take(&mut w.hidden) && w.handler.on_window_shown() {
                w.needs_paint = true;
            }
        }
        let _ = self.conn.map_window(id);
        let _ = self
            .conn
            .configure_window(id, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        // 源 = 1（应用）：WM 可能按焦点窃取策略拒绝，但被 map 的新窗口通常照样获得焦点。
        self.client_message(
            id,
            self.atoms._NET_ACTIVE_WINDOW,
            [1, CURRENT_TIME, 0, 0, 0],
        );
    }

    /// 发给根窗口的 EWMH 客户端消息（WM 在根上监听）。
    fn client_message(&self, id: Window, type_: xproto::Atom, data: [u32; 5]) {
        let ev = ClientMessageEvent::new(32, id, type_, data);
        let _ = self.conn.send_event(
            false,
            self.root,
            EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
            ev,
        );
    }

    fn set_maximized(&self, id: Window, action: u32) {
        let a = self.atoms;
        self.client_message(
            id,
            a._NET_WM_STATE,
            [
                action,
                a._NET_WM_STATE_MAXIMIZED_VERT,
                a._NET_WM_STATE_MAXIMIZED_HORZ,
                1,
                0,
            ],
        );
    }

    fn apply_window_op(&mut self, id: Window, op: WindowOp) {
        let Some(i) = self.idx(id) else { return };
        match op {
            // ICCCM：WM_CHANGE_STATE(IconicState=3) 发给根窗口。
            WindowOp::Minimize => {
                self.client_message(id, self.atoms.WM_CHANGE_STATE, [3, 0, 0, 0, 0])
            }
            WindowOp::ToggleMaximize => self.set_maximized(id, 2),
            WindowOp::Maximize => self.set_maximized(id, 1),
            WindowOp::Restore => {
                if self.windows[i].minimized || !self.windows[i].mapped {
                    self.show(id);
                } else {
                    self.set_maximized(id, 0);
                }
            }
            WindowOp::Show => self.show(id),
            WindowOp::Hide => {
                self.windows[i].hidden = true;
                let _ = self.conn.unmap_window(id);
            }
        }
    }

    fn close_window(&mut self, id: Window) {
        // 从属窗口随主人一起关（X 不像 Windows 那样替我们销毁 owned 窗口）。
        let owned: Vec<Window> = self
            .windows
            .iter()
            .filter(|w| w.owner == Some(id))
            .map(|w| w.id)
            .collect();
        for o in owned {
            self.close_window(o);
        }
        if let Some(ime) = &mut self.ime {
            ime.destroy_ic(id);
        }
        if let Some(i) = self.idx(id) {
            // handler 随 Win 一起 drop：宿主状态、渲染资源在此归还。
            self.windows.remove(i);
            let _ = self.conn.destroy_window(id);
        }
    }

    // ── 事件循环 ──────────────────────────────────────────────────────────

    fn run_loop(&mut self, main: Window) {
        let xfd = self.conn.stream().as_raw_fd();
        let pipe = super::sys::pipe();
        let mut fds = vec![xfd];
        if let Some(p) = pipe {
            fds.push(p.read.as_raw_fd());
        }
        loop {
            // 1. 排空 X 事件（含等回复期间被读进内部队列的那些）。
            loop {
                match self.conn.poll_for_event() {
                    Ok(Some(ev)) => self.handle_event(ev),
                    Ok(None) => break,
                    Err(e) => {
                        log::error!("X 连接断开：{e}");
                        return;
                    }
                }
            }
            // 2. 跨线程唤醒：后台消息、单实例转发的 argv。
            if pipe.is_some_and(|p| p.drain()) {
                if crate::single_instance::run_pending_on_main() && self.idx(main).is_some() {
                    self.show(main);
                }
                for w in &mut self.windows {
                    w.needs_paint = true;
                }
            }
            // 3. 定时器与动画。
            let timeout = self.tick_timers();
            // 4. 出帧。
            self.paint_dirty();
            if self.windows.is_empty() {
                return;
            }
            let _ = self.conn.flush();
            // 出帧期间宿主可能已把新事件读进队列：有就不睡。
            if let Ok(Some(ev)) = self.conn.poll_for_event() {
                self.handle_event(ev);
                continue;
            }
            let any_dirty = self.windows.iter().any(|w| w.needs_paint && w.mapped);
            let timeout = if any_dirty {
                Some(Duration::ZERO)
            } else {
                timeout
            };
            super::sys::wait_readable(&fds, timeout);
        }
    }

    /// 触发到期的 interval 与动画帧，返回距下一个截止的时长（`None` = 无定时需求）。
    fn tick_timers(&mut self) -> Option<Duration> {
        let now = Instant::now();
        let mut next: Option<Instant> = None;
        // 记窗口 id 而不是下标：回调可能关窗（连带关掉从属窗口），`Vec::remove` 之后下标整体前移。
        let mut fired: Vec<(Window, usize)> = Vec::new();
        for w in self.windows.iter_mut() {
            let id = w.id;
            for (idx, (period, due)) in w.intervals.iter_mut().enumerate() {
                if now >= *due {
                    fired.push((id, idx));
                    // 按周期推进而不是从 now 起算，但落后太多时直接对齐到 now（挂起恢复后不补发一串）。
                    *due += *period;
                    if *due < now {
                        *due = now + *period;
                    }
                }
                next = Some(next.map_or(*due, |n: Instant| n.min(*due)));
            }
        }
        for (id, idx) in fired {
            let Some(wi) = self.idx(id) else { continue };
            let r = {
                let _g = crate::platform::EventDispatchGuard::enter();
                self.windows[wi].handler.on_interval_fired(idx)
            };
            if r {
                self.windows[wi].needs_paint = true;
            }
            self.after_event(id);
        }

        // 动画：可见且在请求续帧的窗口。
        let asks: Vec<u64> = self
            .windows
            .iter()
            .filter(|w| w.mapped && !w.minimized && w.handler.wants_animation())
            .map(|w| w.handler.next_frame_delay_ms() as u64)
            .collect();
        if let Some(&ask) = asks.iter().min() {
            let due = Duration::from_millis(FRAME_MS.max(ask.min(MAX_FRAME_DELAY_MS)));
            let elapsed = self.last_anim_frame.elapsed();
            if elapsed >= due {
                self.last_anim_frame = now;
                for w in &mut self.windows {
                    if w.mapped && !w.minimized && w.handler.wants_animation() {
                        w.needs_paint = true;
                    }
                }
                let at = now + due;
                next = Some(next.map_or(at, |n| n.min(at)));
            } else {
                let at = now + (due - elapsed);
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        }
        next.map(|n| n.saturating_duration_since(Instant::now()))
    }

    fn paint_dirty(&mut self) {
        // 按 id 迭代：`after_event` 可能关窗、开窗，下标会漂。
        let ids: Vec<Window> = self.windows.iter().map(|w| w.id).collect();
        for id in ids {
            let Some(i) = self.idx(id) else { continue };
            let w = &self.windows[i];
            if !w.mapped {
                continue;
            }
            if w.needs_paint || w.pixmap.is_none() {
                self.paint(i);
                self.after_event(id);
            } else if w.exposed.is_some() {
                self.upload_exposed(i);
            }
        }
    }

    /// 渲染一帧并上屏本帧重画的部分（∪ 服务器报的暴露区）。
    fn paint(&mut self, i: usize) {
        let w = &mut self.windows[i];
        w.needs_paint = false;
        if w.w <= 0 || w.h <= 0 {
            return;
        }
        let rebuilt = match &w.pixmap {
            Some(p) => p.width() as i32 != w.w || p.height() as i32 != w.h,
            None => true,
        };
        if rebuilt {
            w.pixmap = Pixmap::new(w.w as u32, w.h as u32);
            w.fresh = true;
        }
        let Some(pixmap) = w.pixmap.as_mut() else {
            return;
        };
        if w.fresh {
            // 新缓冲内容不完整：宿主本帧必须画整窗（对照 win32 / macOS 的同名分支）。
            w.handler.request_full_frame();
            pixmap.fill(to_skia_color(w.handler.bg().unwrap_or(w.bg)));
        }
        {
            let mut tgt = crate::render::PixmapTarget { pixmap };
            w.handler.render(&mut tgt, Size::new(w.w, w.h));
        }
        let full = Rect::new(0, 0, w.w, w.h);
        let drawn = match (w.fresh, w.handler.last_frame_damage()) {
            (false, Some(d)) => d.intersect(&full),
            _ => full,
        };
        w.fresh = false;
        let rect = match w.exposed.take() {
            Some(e) => drawn.union(&e.intersect(&full)),
            None => drawn,
        };
        self.upload(i, rect);
    }

    fn upload_exposed(&mut self, i: usize) {
        let w = &mut self.windows[i];
        let Some(e) = w.exposed.take() else { return };
        let full = Rect::new(0, 0, w.w, w.h);
        self.upload(i, e.intersect(&full));
    }

    /// 把 pixmap 的 `r` 区域转成服务器字节序后 `PutImage`。
    fn upload(&mut self, i: usize, r: Rect) {
        let w = &self.windows[i];
        let Some(pm) = w.pixmap.as_ref() else { return };
        let r = r.intersect(&Rect::new(0, 0, pm.width() as i32, pm.height() as i32));
        if r.is_empty() {
            return;
        }
        let stride = pm.width() as usize * 4;
        let data = pm.data();
        let row_bytes = r.w as usize * 4;
        // 按行切块：既不超过服务器的单请求上限（无 BIG-REQUESTS 时 256KB），也把转换缓冲
        // 钉在 `UPLOAD_CHUNK` 以内——整窗帧一次转完的话，200% 大窗口要多出几 MB 峰值。
        let max = self
            .conn
            .maximum_request_bytes()
            .saturating_sub(64)
            .min(UPLOAD_CHUNK)
            .max(row_bytes);
        let rows_per = (max / row_bytes).max(1);
        let mut buf = std::mem::take(&mut self.upload_buf);
        let mut y = r.y;
        while y < r.bottom() {
            let n = rows_per.min((r.bottom() - y) as usize);
            buf.clear();
            for row in y..y + n as i32 {
                let off = row as usize * stride + r.x as usize * 4;
                for px in data[off..off + row_bytes].as_chunks::<4>().0 {
                    if self.fmt.lsb {
                        buf.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
                    } else {
                        buf.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
                    }
                }
            }
            let _ = self.conn.put_image(
                ImageFormat::Z_PIXMAP,
                w.id,
                self.gc,
                r.w as u16,
                n as u16,
                r.x as i16,
                y as i16,
                0,
                self.fmt.depth,
                &buf,
            );
            y += n as i32;
        }
        self.upload_buf = buf;
    }

    // ── 事件分发 ──────────────────────────────────────────────────────────

    fn handle_event(&mut self, ev: Event) {
        if let Some(ime) = &mut self.ime {
            if ime.filter(&ev) {
                self.process_ime();
                return;
            }
        }
        match ev {
            Event::Expose(e) => {
                if let Some(i) = self.idx(e.window) {
                    let r = Rect::new(e.x as i32, e.y as i32, e.width as i32, e.height as i32);
                    let w = &mut self.windows[i];
                    w.exposed = Some(match w.exposed {
                        Some(o) => o.union(&r),
                        None => r,
                    });
                }
            }
            Event::ConfigureNotify(e) => {
                if let Some(i) = self.idx(e.window) {
                    let w = &mut self.windows[i];
                    let (nw, nh) = (e.width as i32, e.height as i32);
                    if nw != w.w || nh != w.h {
                        w.w = nw;
                        w.h = nh;
                        w.needs_paint = true;
                    }
                }
            }
            Event::MapNotify(e) => {
                if let Some(i) = self.idx(e.window) {
                    let w = &mut self.windows[i];
                    w.mapped = true;
                    w.minimized = false;
                    w.needs_paint = true;
                    self.report_state(e.window);
                }
            }
            Event::UnmapNotify(e) => {
                if let Some(i) = self.idx(e.window) {
                    self.windows[i].mapped = false;
                    self.report_state(e.window);
                }
            }
            Event::PropertyNotify(e) => {
                if e.atom == self.atoms._NET_WM_STATE {
                    self.refresh_wm_state(e.window);
                }
            }
            Event::ClientMessage(e) if self.is_dnd_message(e.type_) => self.on_dnd_message(&e),
            Event::SelectionNotify(e) if e.selection == self.dnd_atoms.XdndSelection => {
                self.on_dnd_data(e.requestor, e.property)
            }
            Event::ClientMessage(e) => {
                if e.type_ == self.atoms.WM_PROTOCOLS
                    && e.format == 32
                    && e.data.as_data32()[0] == self.atoms.WM_DELETE_WINDOW
                {
                    self.request_close(e.window);
                }
            }
            Event::ButtonPress(e) => self.on_button(e, true),
            Event::ButtonRelease(e) => self.on_button(e, false),
            Event::MotionNotify(e) => {
                if self.pending_drag_motion(e.event, (e.root_x, e.root_y)) {
                    return;
                }
                let mods = mods_of(u16::from(e.state));
                let ev = PointerEvent {
                    kind: PointerKind::Move,
                    pos: Point::new(e.event_x as i32, e.event_y as i32),
                    button: MouseButton::Left,
                    click_count: 1,
                    mods,
                };
                self.dispatch_pointer(e.event, ev);
            }
            Event::LeaveNotify(e) => {
                // 按住按钮拖出窗口时 X 仍把指针事件送给我们（隐式抓取），那时不清悬停。
                if e.mode == xproto::NotifyMode::NORMAL && u16::from(e.state) & 0x1f00 == 0 {
                    let ev = PointerEvent::single(
                        PointerKind::Move,
                        Point::new(-1, -1),
                        MouseButton::Left,
                    );
                    self.dispatch_pointer(e.event, ev);
                }
            }
            Event::KeyPress(e) if e.event == self.root => {
                self.on_hotkey(e.detail, u16::from(e.state))
            }
            Event::KeyRelease(e) if e.event == self.root => {}
            Event::KeyPress(e) | Event::KeyRelease(e) => {
                // 被模态子窗挡住的窗口不收键盘——X 不会像 Windows 那样禁用 owner 窗口，
                // 焦点可能还留在它的文本框里。拦在交给输入法之前，免得提交的文字漏进去。
                if self.blocked_by_modal(e.event).is_some() {
                    return;
                }
                // 有输入法时先交给它，它不要的会经 `ImeOut::Forward` 转回来再走本地。
                if self.ime.as_mut().is_some_and(|ime| ime.forward_key(&e)) {
                    return;
                }
                let press = e.response_type & 0x7f == xproto::KEY_PRESS_EVENT;
                self.on_key(e.event, e.detail, u16::from(e.state), press);
            }
            Event::FocusIn(e) => {
                if e.mode != xproto::NotifyMode::GRAB && e.mode != xproto::NotifyMode::UNGRAB {
                    if let Some(ime) = &mut self.ime {
                        ime.focus(e.event, true);
                    }
                    self.activated(e.event, true);
                }
            }
            Event::FocusOut(e) => {
                if e.mode != xproto::NotifyMode::GRAB && e.mode != xproto::NotifyMode::UNGRAB {
                    // 合成到一半切走：放弃合成，免得合成串悬挂在文本框里（对照 macOS
                    // `abort_composition`）。
                    self.abort_composition(e.event);
                    if let Some(ime) = &mut self.ime {
                        ime.focus(e.event, false);
                    }
                    self.activated(e.event, false);
                }
            }
            Event::MappingNotify(e) => {
                if e.request == xproto::Mapping::KEYBOARD {
                    self.reload_keymap();
                    if let Some(hk) = &mut self.hotkeys {
                        hk.regrab_all(&self.conn, self.root, &self.keymap);
                    }
                }
            }
            Event::Error(e) => log::debug!("X 错误：{e:?}"),
            _ => {}
        }
    }

    fn is_dnd_message(&self, t: xproto::Atom) -> bool {
        let a = &self.dnd_atoms;
        t == a.XdndEnter || t == a.XdndPosition || t == a.XdndDrop || t == a.XdndLeave
    }

    fn dnd_reply(&self, source: Window, type_: xproto::Atom, data: [u32; 5]) {
        let ev = ClientMessageEvent::new(32, source, type_, data);
        let _ = self.conn.send_event(false, source, EventMask::NO_EVENT, ev);
    }

    /// XDND 目标端的四条消息（见 `dnd.rs` 的流程说明）。
    fn on_dnd_message(&mut self, e: &ClientMessageEvent) {
        let a = self.dnd_atoms;
        let d = e.data.as_data32();
        let (target, source) = (e.window, d[0]);
        if e.type_ == a.XdndEnter {
            // 超过 3 种类型时完整列表挂在源窗口的 XdndTypeList 属性上。
            let extra: Vec<u32> = if d[1] & 1 != 0 {
                self.conn
                    .get_property(false, source, a.XdndTypeList, AtomEnum::ATOM, 0, 1024)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .and_then(|r| r.value32().map(|v| v.collect()))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            self.drag = Some(DragState {
                source,
                target,
                has_uris: dnd::enter_has_uris(e, a.TEXT_URI_LIST, &extra),
                ..Default::default()
            });
        } else if e.type_ == a.XdndPosition {
            let (rx, ry) = ((d[2] >> 16) as i16, (d[2] & 0xffff) as i16);
            let pos = self
                .conn
                .translate_coordinates(self.root, target, rx, ry)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| (r.dst_x as i32, r.dst_y as i32))
                .unwrap_or((0, 0));
            let accept = self.blocked_by_modal(target).is_none()
                && self
                    .drag
                    .as_ref()
                    .is_some_and(|g| g.source == source && g.has_uris);
            if let Some(g) = &mut self.drag {
                g.pos = pos;
            }
            // flags：bit0 接受，bit1 每次移动都发 Position（我们没有「免打扰矩形」）。
            let action = if accept { a.XdndActionCopy } else { 0 };
            self.dnd_reply(
                source,
                a.XdndStatus,
                [target, accept as u32 | 2, 0, 0, action],
            );
        } else if e.type_ == a.XdndDrop {
            let ok = self
                .drag
                .as_ref()
                .is_some_and(|g| g.source == source && g.has_uris)
                && self.blocked_by_modal(target).is_none();
            if !ok {
                self.dnd_reply(source, a.XdndFinished, [target, 0, 0, 0, 0]);
                self.drag = None;
                return;
            }
            if let Some(g) = &mut self.drag {
                g.awaiting_data = true;
            }
            let _ = self.conn.convert_selection(
                target,
                a.XdndSelection,
                a.TEXT_URI_LIST,
                a.XdndSelection,
                d[2],
            );
        } else if e.type_ == a.XdndLeave {
            self.drag = None;
        }
    }

    /// 拖放数据到了：解析路径交宿主，回 XdndFinished。
    fn on_dnd_data(&mut self, requestor: Window, property: xproto::Atom) {
        let Some(g) = self.drag.take() else { return };
        if !g.awaiting_data || g.target != requestor {
            self.drag = Some(g);
            return;
        }
        let a = self.dnd_atoms;
        let paths = if property == u32::from(AtomEnum::NONE) {
            Vec::new()
        } else {
            self.conn
                .get_property(
                    true,
                    requestor,
                    a.XdndSelection,
                    AtomEnum::ANY,
                    0,
                    u32::MAX / 4,
                )
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| dnd::parse_uri_list(&r.value))
                .unwrap_or_default()
        };
        let accepted = !paths.is_empty();
        if accepted {
            if let Some(i) = self.idx(g.target) {
                let w = &mut self.windows[i];
                let r = {
                    let _g = crate::platform::EventDispatchGuard::enter();
                    w.handler.on_drop_files(Point::new(g.pos.0, g.pos.1), paths)
                };
                if r {
                    w.needs_paint = true;
                }
                self.after_event(g.target);
            }
        }
        let action = if accepted { a.XdndActionCopy } else { 0 };
        self.dnd_reply(
            g.source,
            a.XdndFinished,
            [g.target, accepted as u32, action, 0, 0],
        );
    }

    /// 根窗口上的按键 = 全局热键。回调的窗口意图落到主窗口，开 / 关窗请求照常消费。
    fn on_hotkey(&mut self, keycode: u8, state: u16) {
        let Some(hk) = &mut self.hotkeys else { return };
        let Some(op) = hk.dispatch(keycode, state) else {
            return;
        };
        let main = self.main;
        if let Some(op) = op {
            self.apply_window_op(main, op);
        }
        self.run_callback_window_requests(main);
    }

    /// 托盘 / 热键回调排队的关窗、开窗请求（`HotkeyCtx::open_window` 等）。
    fn run_callback_window_requests(&mut self, owner: Window) {
        for key in crate::event::take_callback_closes() {
            if let Some(w) = self
                .windows
                .iter()
                .find(|w| w.single.as_deref() == Some(key.as_str()))
            {
                let id = w.id;
                self.request_close(id);
            }
        }
        let open: Vec<String> = self
            .windows
            .iter()
            .filter_map(|w| w.single.clone())
            .collect();
        for item in crate::app::take_callback_windows(&|k| open.iter().any(|o| o == k)) {
            match item {
                NewWindow::Focus(key) => {
                    if let Some(w) = self
                        .windows
                        .iter()
                        .find(|w| w.single.as_deref() == Some(key.as_str()))
                    {
                        let wid = w.id;
                        self.show(wid);
                    }
                }
                NewWindow::Create(cfg, handler) => {
                    let o = (cfg.owned || cfg.modal)
                        .then_some(owner)
                        .filter(|o| self.idx(*o).is_some());
                    let nid = self.create_window(&cfg, handler, o);
                    self.show(nid);
                }
            }
        }
    }

    /// 派发 XIM 回调的产出。
    fn process_ime(&mut self) {
        let Some(ime) = &mut self.ime else { return };
        let outs = ime.take_out();
        for o in outs {
            match o {
                ImeOut::Commit(ic, text) => {
                    let Some(win) = self.ime.as_ref().and_then(|m| m.window_of(ic)) else {
                        continue;
                    };
                    // 先撤合成串再插字符：插入位置才落在合成开始处。
                    self.dispatch_preedit(win, Preedit::default());
                    for c in text.chars().filter(|c| !c.is_control()) {
                        self.dispatch_key(
                            win,
                            KeyEvent {
                                key: Key::Char(c),
                                pressed: true,
                                shift: false,
                                ctrl: false,
                                alt: false,
                                meta: false,
                            },
                        );
                    }
                }
                ImeOut::Preedit(ic, pe) => {
                    if let Some(win) = self.ime.as_ref().and_then(|m| m.window_of(ic)) {
                        self.dispatch_preedit(win, pe);
                    }
                }
                ImeOut::Forward(e) => {
                    let press = e.response_type & 0x7f == xproto::KEY_PRESS_EVENT;
                    self.on_key(e.event, e.detail, u16::from(e.state), press);
                }
                ImeOut::Ready | ImeOut::IcCreated(_) => {}
            }
        }
    }

    fn dispatch_preedit(&mut self, id: Window, pe: Preedit) {
        let Some(i) = self.idx(id) else { return };
        let active = pe.is_active();
        // 模态挡着时只放行「清空」（收尾一段已开始的合成），不放行新的合成串。
        if active && self.blocked_by_modal(id).is_some() {
            return;
        }
        let w = &mut self.windows[i];
        if !active && !w.composing {
            return;
        }
        w.composing = active;
        let r = {
            let _g = crate::platform::EventDispatchGuard::enter();
            w.handler.set_ime_preedit(&pe)
        };
        if r {
            w.needs_paint = true;
        }
        self.after_event(id);
    }

    /// 放弃进行中的合成：清本地合成串并让输入法丢弃它。
    fn abort_composition(&mut self, id: Window) {
        let composing = self.idx(id).is_some_and(|i| self.windows[i].composing);
        if !composing {
            return;
        }
        if let Some(ime) = &mut self.ime {
            ime.reset(id);
        }
        self.dispatch_preedit(id, Preedit::default());
    }

    fn blocked_by_modal(&self, id: Window) -> Option<Window> {
        self.windows
            .iter()
            .find(|w| w.modal && w.owner == Some(id))
            .map(|w| w.id)
    }

    fn on_button(&mut self, e: xproto::ButtonPressEvent, press: bool) {
        let id = e.event;
        let Some(i) = self.idx(id) else { return };
        if let Some(m) = self.blocked_by_modal(id) {
            if press {
                self.show(m);
            }
            return;
        }
        let pos = Point::new(e.event_x as i32, e.event_y as i32);
        let mods = mods_of(u16::from(e.state));
        let button = match e.detail {
            1 => MouseButton::Left,
            2 => MouseButton::Middle,
            3 => MouseButton::Right,
            // 4/5 = 纵向滚轮（按下即一格，松开忽略）；6/7 横向滚轮暂不支持。
            4 | 5 => {
                if press {
                    let d = if e.detail == 4 { 120 } else { -120 };
                    let mut ev =
                        PointerEvent::single(PointerKind::Wheel(d), pos, MouseButton::Left);
                    ev.mods = mods;
                    self.dispatch_pointer(id, ev);
                }
                return;
            }
            _ => return,
        };
        if e.detail == 1 && self.windows[i].frameless {
            if press {
                if self.try_frameless_drag(i, &e, pos) {
                    let w = &mut self.windows[i];
                    w.swallow_up = true;
                    // 非客户区按下：收起菜单类浮层（对照 win32 `WM_NCLBUTTONDOWN`）——
                    // 这一下不会作为指针事件下发，宿主自己看不到。
                    let r = {
                        let _g = crate::platform::EventDispatchGuard::enter();
                        w.handler.on_dismiss_overlays()
                    };
                    if r {
                        w.needs_paint = true;
                    }
                    self.after_event(id);
                    return;
                }
            } else if std::mem::take(&mut self.windows[i].swallow_up) {
                self.windows[i].pending_drag = None;
                return;
            }
        }
        if press {
            // 合成中点击别处：先放弃合成——命中位置要按不含合成串的文本算。
            self.abort_composition(id);
        }
        let slop = (DOUBLE_CLICK_SLOP * self.scale).round() as i32;
        let click_count = if press {
            self.windows[i]
                .click
                .press(e.time, (pos.x, pos.y), e.detail, slop)
        } else {
            1
        };
        let ev = PointerEvent {
            kind: if press {
                PointerKind::Down
            } else {
                PointerKind::Up
            },
            pos,
            button,
            click_count,
            mods,
        };
        self.dispatch_pointer(id, ev);
    }

    /// 无边框窗口：边缘 → 交 WM 缩放；标题栏拖动区 → 交 WM 移动（双击切最大化）。
    /// 返回 true 表示这一下已被接管、不再下发给控件。真正交给 WM 要等指针移出阈值
    /// （见 `Win::pending_drag`）。
    fn try_frameless_drag(&mut self, i: usize, e: &xproto::ButtonPressEvent, pos: Point) -> bool {
        let w = &mut self.windows[i];
        let border = (RESIZE_BORDER * self.scale).round() as i32;
        if w.resizable && !w.maximized {
            if let Some(dir) = edge_direction(pos, w.w, w.h, border) {
                if !w.handler.interactive_at(pos) {
                    w.pending_drag = Some((dir, e.root_x, e.root_y));
                    return true;
                }
            }
        }
        if w.handler.window_drag_at(pos) && !w.handler.interactive_at(pos) {
            let slop = (DOUBLE_CLICK_SLOP * self.scale).round() as i32;
            let n = w.click.press(e.time, (pos.x, pos.y), 1, slop);
            let (id, resizable) = (w.id, w.resizable);
            if n == 2 {
                w.pending_drag = None;
                if resizable {
                    self.set_maximized(id, 2);
                }
            } else {
                w.pending_drag = Some((MOVERESIZE_MOVE, e.root_x, e.root_y));
            }
            return true;
        }
        false
    }

    /// 待定拖动的指针移动：超过阈值就交给 WM。返回 true 表示这次移动已被接管。
    fn pending_drag_motion(&mut self, id: Window, root: (i16, i16)) -> bool {
        let Some(i) = self.idx(id) else { return false };
        let Some((dir, x0, y0)) = self.windows[i].pending_drag else {
            return false;
        };
        let slop = (DOUBLE_CLICK_SLOP * self.scale).round().max(1.0) as i32;
        if (root.0 as i32 - x0 as i32).abs() > slop || (root.1 as i32 - y0 as i32).abs() > slop {
            self.windows[i].pending_drag = None;
            // 先放掉按下时的隐式抓取，WM 才抓得到指针。
            let _ = self.conn.ungrab_pointer(CURRENT_TIME);
            self.client_message(
                id,
                self.atoms._NET_WM_MOVERESIZE,
                [x0 as u32, y0 as u32, dir, 1, 1],
            );
        }
        true
    }

    fn dispatch_pointer(&mut self, id: Window, ev: PointerEvent) {
        let Some(i) = self.idx(id) else { return };
        if self.blocked_by_modal(id).is_some() {
            return;
        }
        let w = &mut self.windows[i];
        let r = {
            let _g = crate::platform::EventDispatchGuard::enter();
            w.handler.on_pointer(ev)
        };
        w.capturing = w.handler.capture_active();
        if r {
            w.needs_paint = true;
        }
        self.after_event(id);
    }

    fn on_key(&mut self, id: Window, keycode: u8, state: u16, press: bool) {
        let Some(i) = self.idx(id) else { return };
        if self.blocked_by_modal(id).is_some() {
            return;
        }
        let ks = self.keymap.keysym(keycode, state);
        let shift = state & keys::SHIFT != 0;
        let ctrl = state & keys::CONTROL != 0;
        let alt = state & keys::MOD1 != 0;
        let meta = state & keys::MOD4 != 0;
        let mk = |key| KeyEvent {
            key,
            pressed: true,
            shift,
            ctrl,
            alt,
            meta,
        };
        if !press {
            // 只报 Alt 的松开（见 `Key::Alt`）。
            if keys::special_key(ks) == Some(Key::Alt) {
                self.alt_down = false;
                self.dispatch_key(
                    id,
                    KeyEvent {
                        key: Key::Alt,
                        pressed: false,
                        shift: false,
                        ctrl: false,
                        alt: false,
                        meta: false,
                    },
                );
            }
            return;
        }
        let _ = i;
        if let Some(k) = keys::special_key(ks) {
            if k == Key::Alt {
                if self.alt_down {
                    return; // 自动重复
                }
                self.alt_down = true;
            }
            self.dispatch_key(id, mk(k));
            // 空格与小键盘运算键还要产出字符（与 win32 WM_KEYDOWN + WM_CHAR 的双发一致）。
            let emits_char = matches!(
                k,
                Key::Space
                    | Key::NumpadAdd
                    | Key::NumpadSubtract
                    | Key::NumpadMultiply
                    | Key::NumpadDivide
            );
            if !emits_char || ctrl || alt {
                return;
            }
        } else if keys::is_modifier(ks) {
            return;
        }
        if ctrl || alt {
            // 快捷键：Key::Other(大写 ASCII)，与 win32 VK / macOS 同一口径。不产出文本。
            if keys::special_key(ks).is_none() {
                let code = keys::shortcut_code(self.keymap.base(keycode)).or_else(|| {
                    self.keymap
                        .ascii_on_key(keycode)
                        .and_then(keys::shortcut_code)
                });
                if let Some(code) = code {
                    self.dispatch_key(id, mk(Key::Other(code)));
                }
            }
            return;
        }
        if let Some(c) = keys::keysym_char(ks) {
            if !c.is_control() {
                self.dispatch_key(
                    id,
                    KeyEvent {
                        key: Key::Char(c),
                        pressed: true,
                        shift: false,
                        ctrl: false,
                        alt: false,
                        meta: false,
                    },
                );
            }
        }
    }

    fn dispatch_key(&mut self, id: Window, ev: KeyEvent) {
        let Some(i) = self.idx(id) else { return };
        if self.blocked_by_modal(id).is_some() {
            return;
        }
        let w = &mut self.windows[i];
        let r = {
            let _g = crate::platform::EventDispatchGuard::enter();
            w.handler.on_key(ev)
        };
        if r {
            w.needs_paint = true;
        }
        self.after_event(id);
    }

    fn activated(&mut self, id: Window, active: bool) {
        let Some(i) = self.idx(id) else { return };
        let w = &mut self.windows[i];
        let mut r = w.handler.on_window_activated(active);
        if !active {
            self.alt_down = false;
            // 失活 = 指针捕获被抢（拖到一半 Alt+Tab 走，Up 再也不会来）。
            if w.capturing {
                w.capturing = false;
                r |= w.handler.on_capture_lost();
            }
        }
        if r {
            w.needs_paint = true;
        }
        self.after_event(id);
    }

    fn request_close(&mut self, id: Window) {
        let Some(i) = self.idx(id) else { return };
        // 模态子窗开着：owner 的关闭按钮不生效，改为把模态框带到前面——否则会连带关掉
        // 模态框且绕过它自己的 `on_close_request`（「未保存」提示被跳过）。
        if let Some(m) = self.blocked_by_modal(id) {
            self.show(m);
            return;
        }
        let allow = {
            let _g = crate::platform::EventDispatchGuard::enter();
            self.windows[i].handler.on_close_request()
        };
        if allow {
            self.close_window(id);
        } else {
            self.windows[i].needs_paint = true;
            self.after_event(id);
        }
    }

    fn refresh_wm_state(&mut self, id: Window) {
        let Some(i) = self.idx(id) else { return };
        let a = self.atoms;
        let Ok(Ok(r)) = self
            .conn
            .get_property(false, id, a._NET_WM_STATE, AtomEnum::ATOM, 0, 64)
            .map(|c| c.reply())
        else {
            return;
        };
        let atoms: Vec<u32> = r.value32().map(|v| v.collect()).unwrap_or_default();
        let maximized = atoms.contains(&a._NET_WM_STATE_MAXIMIZED_VERT)
            && atoms.contains(&a._NET_WM_STATE_MAXIMIZED_HORZ);
        let minimized = atoms.contains(&a._NET_WM_STATE_HIDDEN);
        let w = &mut self.windows[i];
        let became_min = minimized && !w.minimized;
        w.maximized = maximized;
        w.minimized = minimized;
        if became_min && w.handler.hide_on_minimize() {
            w.hidden = true;
            let _ = self.conn.unmap_window(id);
        }
        self.report_state(id);
    }

    fn report_state(&mut self, id: Window) {
        let Some(i) = self.idx(id) else { return };
        let w = &mut self.windows[i];
        let st = crate::event::WindowState {
            maximized: w.maximized,
            minimized: w.minimized,
            visible: w.mapped || w.minimized,
            maximizable: w.resizable,
            minimizable: true,
        };
        w.handler.on_window_state(st);
        w.needs_paint = true;
    }

    /// 事件分发后的收尾：窗口操作、标题、对话框、开窗、光标、关窗、跨窗口脏。
    fn after_event(&mut self, id: Window) {
        let Some(i) = self.idx(id) else { return };
        let open: Vec<String> = self
            .windows
            .iter()
            .filter_map(|w| w.single.clone())
            .collect();
        let w = &mut self.windows[i];
        let op = w.handler.take_window_op();
        let dialog = w.handler.take_dialog_request();
        let close = w.handler.wants_close();
        let title = w.handler.take_window_title();
        let hotkey_ops = w.handler.take_hotkey_ops();
        // 托盘操作：本后端尚无托盘，取走丢弃以免队列无限增长。
        let _ = crate::platform::tray::take_tray_ops();
        let new_windows = w
            .handler
            .take_new_windows(&|key| open.iter().any(|k| k == key));
        let shape = w.handler.cursor();
        let cursor_changed = shape != w.cursor;
        w.cursor = shape;
        let caret = w.handler.ime_caret();
        if let (Some(ime), Some((cx, cy, ch))) = (&mut self.ime, caret) {
            // 候选窗锚在光标底边（窗口内物理像素）。
            ime.set_spot(id, cx, cy + ch);
        }

        if let Some(t) = title {
            self.set_title(id, &t);
        }
        if let Some(hk) = &mut self.hotkeys {
            for (hid, op) in hotkey_ops {
                hk.apply(&self.conn, self.root, &self.keymap, hid, op);
            }
        }
        if cursor_changed {
            let c = self.cursors[cursor_index(shape)];
            let _ = self
                .conn
                .change_window_attributes(id, &ChangeWindowAttributesAux::new().cursor(c));
        }
        if let Some(op) = op {
            self.apply_window_op(id, op);
        }
        if let Some(req) = dialog {
            // 对话框是阻塞的（门户 / zenity 在自己的进程里跑）。先把已排的请求送出去，
            // 免得对话框期间窗口停在半帧状态。
            let _ = self.conn.flush();
            req.run();
            if let Some(i) = self.idx(id) {
                self.windows[i].needs_paint = true;
            }
        }
        for item in new_windows {
            match item {
                NewWindow::Focus(key) => {
                    if let Some(w) = self
                        .windows
                        .iter()
                        .find(|w| w.single.as_deref() == Some(key.as_str()))
                    {
                        let wid = w.id;
                        self.show(wid);
                    }
                }
                NewWindow::Create(cfg, handler) => {
                    let owner = (cfg.owned || cfg.modal).then_some(id);
                    let nid = self.create_window(&cfg, handler, owner);
                    self.show(nid);
                }
            }
        }
        if close {
            self.close_window(id);
        }
        if crate::signal::take_cross_window_dirty() {
            for w in &mut self.windows {
                if w.id != id {
                    w.needs_paint = true;
                }
            }
        }
    }
}

fn mods_of(state: u16) -> crate::event::Mods {
    crate::event::Mods {
        ctrl: state & keys::CONTROL != 0,
        alt: state & keys::MOD1 != 0,
        shift: state & keys::SHIFT != 0,
        meta: state & keys::MOD4 != 0,
    }
}

/// 无边框窗口的边缘命中 → `_NET_WM_MOVERESIZE` 方向码（0 = 左上，顺时针到 7 = 左）。
fn edge_direction(p: Point, w: i32, h: i32, border: i32) -> Option<u32> {
    let left = p.x < border;
    let right = p.x >= w - border;
    let top = p.y < border;
    let bottom = p.y >= h - border;
    Some(match (left, right, top, bottom) {
        (true, _, true, _) => 0,
        (_, true, true, _) => 2,
        (_, true, _, true) => 4,
        (true, _, _, true) => 6,
        (_, _, true, _) => 1,
        (_, true, _, _) => 3,
        (_, _, _, true) => 5,
        (true, _, _, _) => 7,
        _ => return None,
    })
}

fn cursor_index(s: CursorShape) -> usize {
    match s {
        CursorShape::Arrow => 0,
        CursorShape::Hand => 1,
        CursorShape::Text => 2,
        CursorShape::SizeWE => 3,
        CursorShape::SizeNS => 4,
    }
}

/// 核心协议的 `cursor` 字体光标（不依赖 Xcursor 主题，所有 X 服务器都有）。
fn create_cursors(conn: &RustConnection) -> Option<[xproto::Cursor; 5]> {
    let font = conn.generate_id().ok()?;
    conn.open_font(font, b"cursor").ok()?;
    // XC_left_ptr, XC_hand2, XC_xterm, XC_sb_h_double_arrow, XC_sb_v_double_arrow
    let glyphs = [68u16, 60, 152, 108, 116];
    let mut out = [0u32; 5];
    for (i, g) in glyphs.iter().enumerate() {
        let c = conn.generate_id().ok()?;
        conn.create_glyph_cursor(c, font, font, *g, g + 1, 0, 0, 0, 0xffff, 0xffff, 0xffff)
            .ok()?;
        out[i] = c;
    }
    let _ = conn.close_font(font);
    Some(out)
}

/// DPI 缩放：`WINDUI_SCALE` 环境变量优先，其次 X 资源库里的 `Xft.dpi`（桌面环境的
/// 缩放设置最终都落到这里），再次 `GDK_SCALE`，都没有按 1.0。
fn detect_scale(conn: &RustConnection, root: Window, atoms: &Atoms) -> f32 {
    if let Some(s) = std::env::var("WINDUI_SCALE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
    {
        return s.clamp(0.5, 4.0);
    }
    let dpi = conn
        .get_property(
            false,
            root,
            atoms.RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            1 << 20,
        )
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| xft_dpi(&String::from_utf8_lossy(&r.value)));
    if let Some(dpi) = dpi {
        return (dpi / 96.0).clamp(0.5, 4.0);
    }
    std::env::var("GDK_SCALE")
        .ok()
        .and_then(|v| v.parse::<f32>().ok())
        .map(|s| s.clamp(0.5, 4.0))
        .unwrap_or(1.0)
}

fn xft_dpi(resources: &str) -> Option<f32> {
    resources.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == "Xft.dpi")
            .then(|| v.trim().parse().ok())
            .flatten()
    })
}

/// WM_CLASS 的 (instance, class)：可执行文件名与其首字母大写形式。任务栏据此归组窗口、
/// 匹配 .desktop 文件的 `StartupWMClass`。
fn wm_class() -> (String, String) {
    let inst = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "windui".into());
    let mut class = inst.clone();
    if let Some(f) = class.get_mut(0..1) {
        f.make_ascii_uppercase();
    }
    (inst, class)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xft_dpi_is_parsed_from_resource_string() {
        assert_eq!(xft_dpi("Xft.antialias:\t1\nXft.dpi:\t192\n"), Some(192.0));
        assert_eq!(xft_dpi("Xft.antialias:\t1\n"), None);
    }

    #[test]
    fn edges_map_to_ewmh_directions() {
        assert_eq!(edge_direction(Point::new(0, 0), 100, 100, 5), Some(0));
        assert_eq!(edge_direction(Point::new(50, 0), 100, 100, 5), Some(1));
        assert_eq!(edge_direction(Point::new(99, 99), 100, 100, 5), Some(4));
        assert_eq!(edge_direction(Point::new(0, 50), 100, 100, 5), Some(7));
        assert_eq!(edge_direction(Point::new(50, 50), 100, 100, 5), None);
    }

    #[test]
    fn click_count_folds_into_one_two_cycle() {
        let mut t = ClickTracker::default();
        assert_eq!(t.press(1000, (10, 10), 1, 4), 1);
        assert_eq!(t.press(1100, (11, 10), 1, 4), 2);
        assert_eq!(t.press(1200, (11, 10), 1, 4), 1, "第三下重新起算");
        assert_eq!(t.press(1300, (11, 10), 1, 4), 2);
        assert_eq!(t.press(2000, (11, 10), 1, 4), 1, "超时不算双击");
        assert_eq!(t.press(2100, (40, 10), 1, 4), 1, "漂移过大不算双击");
    }
}
