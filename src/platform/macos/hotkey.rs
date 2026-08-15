//! 全局热键（Carbon `RegisterEventHotKey`）：应用无焦点、窗口隐藏时亦可触发。
//!
//! 系统在热键按下时把事件投递到应用的事件队列——**事件驱动，不轮询**，故不破坏
//! 「空闲零 CPU」这条核心指标（AGENTS.md）。对照 win32 的 `RegisterHotKey` + `WM_HOTKEY`。
//!
//! ## 为什么是 Carbon
//!
//! Carbon 的 Event Manager 整体被标记为弃用，但 `RegisterEventHotKey` 至今仍是 macOS 上
//! 注册全局热键的**唯一**免授权途径，系统自带的快捷键面板也走它。另两条路都要用户在
//! 「系统设置 → 隐私与安全性 → 辅助功能」里手动授权：
//!
//! - `CGEventTap`：能拦截，但需授权，且权限被撤销后要整套降级路径；
//! - `NSEvent::addGlobalMonitorForEvents`：需授权，且**只能监听不能拦截**——按键仍会
//!   落到前台应用里，热键"生效"的同时前台程序也收到了这次按键。
//!
//! 授权流程涉及引导 UI、公证签名与撤销后的降级，且未打包成 `.app` 时根本走不通。
//! 全局热键是「后台工具」的基础能力，不该以此为前提。
//!
//! ## 状态存 thread_local 的理由
//!
//! Carbon 的事件处理器是个 **C 函数**，除 `userData` 外没有别的上下文通道；而 `userData`
//! 要求状态有稳定地址（`Box::into_raw` 之类），改绑时又要能拿回 `&mut`。用 thread_local
//! 更直接，且与"热键是应用级资源、只装一次"的语义吻合。win32 那边靠 message-only 窗口的
//! `GWLP_USERDATA` 挂 `AppHost`——同一个问题的两种解法。

use std::cell::RefCell;
use std::ffi::c_void;

use crate::event::{Hotkey, HotkeyCtx, HotkeyOp, Key, WindowOp};
use crate::platform::HotkeyBinding;

// ── Carbon FFI ──────────────────────────────────────────────────────────────

type OSStatus = i32;
type EventHotKeyRef = *mut c_void;
type EventTargetRef = *mut c_void;
type EventHandlerRef = *mut c_void;
type EventHandlerCallRef = *mut c_void;
type EventRef = *mut c_void;

/// 热键标识。`signature` 按 Carbon 惯例填应用自己的四字符码，`id` 是我们的槽位下标。
#[repr(C)]
#[derive(Clone, Copy)]
struct EventHotKeyID {
    signature: u32,
    id: u32,
}

/// 事件类型（类 + 种）。装处理器时声明关心哪些事件。
#[repr(C)]
struct EventTypeSpec {
    event_class: u32,
    event_kind: u32,
}

#[link(name = "Carbon", kind = "framework")]
extern "C" {
    fn RegisterEventHotKey(
        in_hot_key_code: u32,
        in_hot_key_modifiers: u32,
        in_hot_key_id: EventHotKeyID,
        in_target: EventTargetRef,
        in_options: u32,
        out_ref: *mut EventHotKeyRef,
    ) -> OSStatus;
    fn UnregisterEventHotKey(in_hot_key: EventHotKeyRef) -> OSStatus;
    fn GetApplicationEventTarget() -> EventTargetRef;
    fn InstallEventHandler(
        in_target: EventTargetRef,
        in_handler: extern "C" fn(EventHandlerCallRef, EventRef, *mut c_void) -> OSStatus,
        in_num_types: u32,
        in_list: *const EventTypeSpec,
        in_user_data: *mut c_void,
        out_ref: *mut EventHandlerRef,
    ) -> OSStatus;
    fn GetEventParameter(
        in_event: EventRef,
        in_name: u32,
        in_desired_type: u32,
        out_actual_type: *mut u32,
        in_buffer_size: usize,
        out_actual_size: *mut usize,
        out_data: *mut c_void,
    ) -> OSStatus;
}

/// 四字符码（Carbon 的 `OSType`）：`b"keyb"` → `0x6B657962`。
const fn four_cc(s: &[u8; 4]) -> u32 {
    ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
}

const NO_ERR: OSStatus = 0;
/// `eventNotHandledErr`：告诉系统这条事件我们没处理，让它继续往下传。
const EVENT_NOT_HANDLED: OSStatus = -9874;

/// 本应用的热键签名。取什么值都行，只要与 `EventHotKeyID.id` 一起唯一标识我们的热键。
const SIGNATURE: u32 = four_cc(b"wdui");

// ── 键码映射 ────────────────────────────────────────────────────────────────

/// ASCII 字母/数字 → macOS 虚拟键码。
///
/// macOS 的键码是**物理键位编号**（ANSI 布局下的位置），与字符值毫无关系——win32 那边
/// `VK_A == b'A'` 的巧合在这里不成立，只能逐个列出。数字区尤其反直觉：`5` 是 `0x17` 而
/// `6` 是 `0x16`，两者在表里是**反序**的（`7`/`8`/`9` 同样不连续）。
///
/// 键码指的是**键位**而非字符，所以它不随键盘布局变化：在 AZERTY 上按下"A 所在的那个
/// 物理键"送出的仍是 `0x00`。这正是全局热键要的语义——`Cmd+Q` 在哪种布局下都是右上角
/// 那两个键。
fn mac_keycode_from_ascii(c: char) -> Option<u32> {
    Some(match c.to_ascii_uppercase() {
        'A' => 0x00,
        'B' => 0x0B,
        'C' => 0x08,
        'D' => 0x02,
        'E' => 0x0E,
        'F' => 0x03,
        'G' => 0x05,
        'H' => 0x04,
        'I' => 0x22,
        'J' => 0x26,
        'K' => 0x28,
        'L' => 0x25,
        'M' => 0x2E,
        'N' => 0x2D,
        'O' => 0x1F,
        'P' => 0x23,
        'Q' => 0x0C,
        'R' => 0x0F,
        'S' => 0x01,
        'T' => 0x11,
        'U' => 0x20,
        'V' => 0x09,
        'W' => 0x0D,
        'X' => 0x07,
        'Y' => 0x10,
        'Z' => 0x06,
        '0' => 0x1D,
        '1' => 0x12,
        '2' => 0x13,
        '3' => 0x14,
        '4' => 0x15,
        '5' => 0x17,
        '6' => 0x16,
        '7' => 0x1A,
        '8' => 0x1C,
        '9' => 0x19,
        _ => return None,
    })
}

/// win32 虚拟键码 → macOS 虚拟键码。
///
/// `Key::Other(vk)` 在本仓库被定义为**跨平台对齐的 win32 VK 码**（见 win32 侧 `vk_of`
/// 的说明），它是 F1–F12、PageUp/PageDown 等键位的唯一表达途径——`Key` 枚举没有这些
/// 变体。macOS 这边就得把它翻回本地键码。
///
/// F 键的 macOS 键码同样是乱序的（F1=0x7A、F2=0x78、F3=0x63…），照抄 `Events.h`。
fn mac_keycode_from_vk(vk: u32) -> Option<u32> {
    Some(match vk {
        0x70 => 0x7A, // F1
        0x71 => 0x78, // F2
        0x72 => 0x63, // F3
        0x73 => 0x76, // F4
        0x74 => 0x60, // F5
        0x75 => 0x61, // F6
        0x76 => 0x62, // F7
        0x77 => 0x64, // F8
        0x78 => 0x65, // F9
        0x79 => 0x6D, // F10
        0x7A => 0x67, // F11
        0x7B => 0x6F, // F12
        0x21 => 0x74, // VK_PRIOR / PageUp
        0x22 => 0x79, // VK_NEXT / PageDown
        // 字母与数字的 VK 码就是其大写 ASCII 值，转交上面那张表。
        v if (0x30..=0x39).contains(&v) || (0x41..=0x5A).contains(&v) => {
            return mac_keycode_from_ascii(v as u8 as char)
        }
        _ => return None,
    })
}

/// `Key` → macOS 虚拟键码。无法映射者返回 `None`（该热键静默不注册，同 win32）。
///
/// 具名键的键码与 `window.rs` 的 `map_special` 是**同一套**（那边是输入路径的反向映射），
/// 两张表必须一致，否则会出现"注册的是 Home、按下去却当 End 处理"这种错位。有单测钉住。
fn mac_keycode_of(key: Key) -> Option<u32> {
    Some(match key {
        Key::Char(c) if c.is_ascii_alphanumeric() => return mac_keycode_from_ascii(c),
        // 非 ASCII 字符（如 `Key::Char('中')`）没有稳定的键位映射，作全局热键无意义。
        Key::Char(_) => return None,
        Key::Tab => 0x30,
        Key::Enter => 0x24,
        Key::Escape => 0x35,
        Key::Space => 0x31,
        Key::Left => 0x7B,
        Key::Right => 0x7C,
        Key::Up => 0x7E,
        Key::Down => 0x7D,
        Key::Home => 0x73,
        Key::End => 0x77,
        Key::Delete => 0x75, // ForwardDelete（0x33 是退格）
        Key::Other(vk) => return mac_keycode_from_vk(vk),
        // Backspace 作全局热键无实际用途（与 win32 侧一致）。
        Key::Backspace => return None,
    })
}

// Carbon 修饰键掩码（`Events.h` 的 `cmdKey` 等）。与 win32 的 `MOD_*` 一一对应。
const CMD_KEY: u32 = 0x0100;
const SHIFT_KEY: u32 = 0x0200;
const OPTION_KEY: u32 = 0x0800;
const CONTROL_KEY: u32 = 0x1000;

/// 修饰键 → Carbon 掩码。
///
/// `meta` 在 macOS 上是 Command 键（win32 上是 Win 键），这条平台差异由 `Mods` 的文档
/// 约定，调用方不必分平台。
fn mods_of(hk: Hotkey) -> u32 {
    let mut m = 0;
    if hk.mods.ctrl {
        m |= CONTROL_KEY;
    }
    if hk.mods.alt {
        m |= OPTION_KEY;
    }
    if hk.mods.shift {
        m |= SHIFT_KEY;
    }
    if hk.mods.meta {
        m |= CMD_KEY;
    }
    m
}

// ── 状态 ────────────────────────────────────────────────────────────────────

/// 一个热键槽：组合、回调与向系统的注册状态。回调**始终保留**（即使当前组合注册
/// 失败）——运行期 `Rebind` 换到可用组合后即恢复生效。与 win32 侧的 `Slot` 同形。
struct Slot {
    hotkey: Hotkey,
    callback: Box<dyn FnMut(&mut HotkeyCtx)>,
    /// 系统返回的注册句柄（`None` = 未注册，热键事件不会来）。
    /// win32 那边用 `bool` 即可（注销靠 hwnd+id），这边注销要拿回这个句柄。
    reg: Option<EventHotKeyRef>,
    /// 用户启用态（`SetEnabled` 控制；停用即注销、组合归还系统）。
    enabled: bool,
}

/// 已注册的热键集合。
pub(crate) struct HotkeyState {
    /// 索引即 `EventHotKeyID.id`。槽位固定不压缩——紧凑数组会让 id 错位，
    /// 令热键事件触发到错误的回调。
    slots: Vec<Slot>,
}

thread_local! {
    /// 应用级热键状态。Carbon 的处理器是个 C 函数，只能从这里取回上下文（见模块头部）。
    static HOTKEYS: RefCell<Option<HotkeyState>> = const { RefCell::new(None) };
}

/// 向系统注册一个热键，成功返回句柄。
///
/// 无法映射键码、或系统拒绝（组合已被别的程序占用）→ `None`。
fn try_register(id: usize, hk: Hotkey) -> Option<EventHotKeyRef> {
    let code = mac_keycode_of(hk.key)?;
    let mut out: EventHotKeyRef = std::ptr::null_mut();
    let status = unsafe {
        RegisterEventHotKey(
            code,
            mods_of(hk),
            EventHotKeyID {
                signature: SIGNATURE,
                id: id as u32,
            },
            GetApplicationEventTarget(),
            0,
            &mut out,
        )
    };
    // 句柄非空才算成功：某些错误路径下 status 为 0 但没给出句柄，拿它去注销会崩。
    (status == NO_ERR && !out.is_null()).then_some(out)
}

/// 注销一个热键句柄。
fn unregister(reg: EventHotKeyRef) {
    unsafe {
        UnregisterEventHotKey(reg);
    }
}

/// 安装全局热键（应用启动时调一次）。
///
/// 单个热键注册失败**不影响其余热键**，也不阻止窗口创建：热键是全局独占资源，组合被别的
/// 程序占用是常态而非异常，让整个应用起不来是不可接受的。失败者静默忽略；运行期可经
/// [`HotkeyOp::Rebind`] 换组合恢复。语义与 win32 侧完全一致。
pub(crate) fn install(bindings: Vec<HotkeyBinding>) {
    if bindings.is_empty() {
        return;
    }
    install_event_handler();
    let slots = bindings
        .into_iter()
        .enumerate()
        .map(|(id, b)| Slot {
            reg: try_register(id, b.hotkey),
            hotkey: b.hotkey,
            callback: b.callback,
            enabled: true,
        })
        .collect();
    HOTKEYS.with(|h| *h.borrow_mut() = Some(HotkeyState { slots }));
}

thread_local! {
    /// 事件处理器是否已装。装两遍会让每次热键触发两次回调——`App::run` 只跑一次，
    /// 眼下到不了这条路径，但双触发是那种"看着像业务逻辑写错了"的症状，一个布尔就能挡掉。
    static HANDLER_INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 装一次应用级事件处理器（只关心"热键按下"）。
///
/// 处理器**不注销**：它随应用存续，与热键槽位的增删无关。
fn install_event_handler() {
    if HANDLER_INSTALLED.with(|f| f.replace(true)) {
        return;
    }
    let spec = EventTypeSpec {
        event_class: four_cc(b"keyb"),
        event_kind: 5, // kEventHotKeyPressed
    };
    let mut out: EventHandlerRef = std::ptr::null_mut();
    let status = unsafe {
        InstallEventHandler(
            GetApplicationEventTarget(),
            hotkey_handler,
            1,
            &spec,
            std::ptr::null_mut(),
            &mut out,
        )
    };
    // 这一步失败意味着**所有**热键都收不到事件（注册本身仍会成功，症状是"注册了但按下
    // 去没反应"）。与单个组合被占用不同，那是常态、静默忽略即可；这个是环境层面的异常，
    // 不出声的话没人能从现象反推到这里。
    if status != NO_ERR {
        eprintln!("[windui] 全局热键事件处理器安装失败（status={status}），热键不会触发");
    }
}

/// Carbon 事件处理器：取出热键 id，跑对应回调，再执行它声明的窗口意图。
///
/// **两段式**（同 win32 的借用纪律）：借用 `HOTKEYS` 期间只跑回调取意图，释放之后才碰
/// AppKit——`WindowOp::Show` 会激活窗口并同步回调进视图，那条路径若再借一次 `HOTKEYS`
/// 就是 `RefCell` 重入 panic。回调本身拿到的 [`HotkeyCtx`] 也不持有任何窗口句柄，
/// 危险代码在类型上就写不出来。
extern "C" fn hotkey_handler(
    _call: EventHandlerCallRef,
    event: EventRef,
    _data: *mut c_void,
) -> OSStatus {
    let mut hk_id = EventHotKeyID {
        signature: 0,
        id: 0,
    };
    let status = unsafe {
        GetEventParameter(
            event,
            four_cc(b"----"), // kEventParamDirectObject
            four_cc(b"hkid"), // typeEventHotKeyID
            std::ptr::null_mut(),
            std::mem::size_of::<EventHotKeyID>(),
            std::ptr::null_mut(),
            &mut hk_id as *mut EventHotKeyID as *mut c_void,
        )
    };
    // 取不出 id 就没法知道是哪个热键——交还给系统，别吞掉这条事件。
    if status != NO_ERR || hk_id.signature != SIGNATURE {
        return EVENT_NOT_HANDLED;
    }
    let op = HOTKEYS.with(|h| {
        h.borrow_mut()
            .as_mut()
            .and_then(|s| s.dispatch(hk_id.id as usize))
    });
    // 借用已释放，可以安全地碰 AppKit。
    if let Some(op) = op {
        super::window::run_window_op_on_main(op);
    }
    NO_ERR
}

impl HotkeyState {
    /// 派发一次热键触发，返回回调声明的窗口操作意图。
    #[must_use]
    fn dispatch(&mut self, id: usize) -> Option<WindowOp> {
        let slot = self.slots.get_mut(id)?;
        let mut ctx = HotkeyCtx::default();
        (slot.callback)(&mut ctx);
        ctx.take_op()
    }
}

/// 运行期热键操作（`HotkeyHandle` 排队、窗口层在事件分发后消费）。
///
/// `Rebind`：先注销旧组合再注册新组合；新组合注册失败（被占用等）时**回滚**重注册旧
/// 组合——绝不让一次失败的改绑把原本可用的热键弄丢。语义与 win32 侧逐条对齐。
pub(crate) fn apply(id: usize, op: HotkeyOp) {
    HOTKEYS.with(|h| {
        let mut guard = h.borrow_mut();
        let Some(state) = guard.as_mut() else { return };
        let Some(slot) = state.slots.get_mut(id) else {
            return;
        };
        match op {
            HotkeyOp::Rebind(new) => {
                if let Some(reg) = slot.reg.take() {
                    unregister(reg);
                }
                if !slot.enabled {
                    // 停用中只记新组合，待 SetEnabled(true) 时再注册。
                    slot.hotkey = new;
                    return;
                }
                match try_register(id, new) {
                    Some(reg) => {
                        slot.hotkey = new;
                        slot.reg = Some(reg);
                    }
                    None => {
                        // 回滚：新组合拿不到，旧组合续命。
                        slot.reg = try_register(id, slot.hotkey);
                        #[cfg(debug_assertions)]
                        eprintln!("[windui] 热键改绑失败（组合被占用？），保留旧绑定");
                    }
                }
            }
            HotkeyOp::SetEnabled(on) => {
                slot.enabled = on;
                if !on {
                    if let Some(reg) = slot.reg.take() {
                        unregister(reg);
                    }
                } else if slot.reg.is_none() {
                    slot.reg = try_register(id, slot.hotkey);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 四字符码按大端拼接() {
        assert_eq!(four_cc(b"keyb"), 0x6B657962);
        assert_eq!(four_cc(b"hkid"), 0x686B6964);
        assert_eq!(four_cc(b"----"), 0x2D2D2D2D);
    }

    #[test]
    fn 字母键映射到键位编号而非ascii() {
        // macOS 键码是物理键位，与字符值无关：'A' 是 0x00 而不是 0x41。
        assert_eq!(mac_keycode_of(Key::Char('a')), Some(0x00));
        assert_eq!(mac_keycode_of(Key::Char('A')), Some(0x00));
        assert_eq!(mac_keycode_of(Key::Char('d')), Some(0x02));
        assert_eq!(mac_keycode_of(Key::Char('z')), Some(0x06));
    }

    #[test]
    fn 数字键的键码不连续且5与6反序() {
        // 这一段最容易照着直觉写错，单独钉住。
        assert_eq!(mac_keycode_of(Key::Char('5')), Some(0x17));
        assert_eq!(mac_keycode_of(Key::Char('6')), Some(0x16), "6 比 5 小");
        assert_eq!(mac_keycode_of(Key::Char('7')), Some(0x1A));
        assert_eq!(mac_keycode_of(Key::Char('8')), Some(0x1C));
        assert_eq!(mac_keycode_of(Key::Char('9')), Some(0x19));
        assert_eq!(mac_keycode_of(Key::Char('0')), Some(0x1D));
    }

    #[test]
    fn 字母数字键码两两不同() {
        // 抄这张表最可能的错法是把某两个键写成同一个码——那会让其中一个热键
        // 注册到别的键位上，且只在真机上按下去才发现。
        let mut seen = std::collections::HashMap::new();
        for c in ('a'..='z').chain('0'..='9') {
            let code = mac_keycode_from_ascii(c).expect("字母数字都该有键码");
            if let Some(prev) = seen.insert(code, c) {
                panic!("键码 {code:#04X} 被 {prev} 与 {c} 共用");
            }
        }
        assert_eq!(seen.len(), 36);
    }

    #[test]
    fn 非ascii字符不可作热键() {
        assert_eq!(mac_keycode_of(Key::Char('中')), None);
        assert_eq!(mac_keycode_of(Key::Char('é')), None);
    }

    #[test]
    fn 具名键与输入路径的键码表一致() {
        // `window.rs` 的 `map_special` 是同一套键码的反向映射（macOS keyCode → Key）。
        // 两张表若走偏，症状是"注册的是 Home、按下去却按 End 处理"——两处都自洽，
        // 只有对着看才发现。故在此钉住互逆关系。
        for key in [
            Key::Tab,
            Key::Enter,
            Key::Escape,
            Key::Space,
            Key::Left,
            Key::Right,
            Key::Up,
            Key::Down,
            Key::Home,
            Key::End,
            Key::Delete,
        ] {
            let code = mac_keycode_of(key).expect("具名键都该有键码");
            assert_eq!(
                super::super::window::map_special(code as u16),
                Some(key),
                "键码 {code:#04X} 在两张表里对应不同的键"
            );
        }
    }

    #[test]
    fn f键经other映射到本地键码() {
        // Key::Other 是 F 键的唯一表达途径（VK_F1 == 0x70）。macOS 侧 F1 是 0x7A。
        assert_eq!(mac_keycode_of(Key::Other(0x70)), Some(0x7A));
        assert_eq!(mac_keycode_of(Key::Other(0x7B)), Some(0x6F), "F12");
        // 字母的 VK 码走同一张 ASCII 表：VK_A == 0x41 → 键位 0x00。
        assert_eq!(mac_keycode_of(Key::Other(0x41)), Some(0x00));
    }

    #[test]
    fn f键键码两两不同() {
        let mut seen = std::collections::HashMap::new();
        for (i, vk) in (0x70..=0x7Bu32).enumerate() {
            let code = mac_keycode_from_vk(vk).expect("F1–F12 都该有键码");
            if let Some(prev) = seen.insert(code, i + 1) {
                panic!("键码 {code:#04X} 被 F{prev} 与 F{} 共用", i + 1);
            }
        }
        assert_eq!(seen.len(), 12);
    }

    #[test]
    fn 无法映射的键被拒绝() {
        assert_eq!(mac_keycode_of(Key::Backspace), None);
        // 没有对应键位的 VK（这里是 VK_LBUTTON）不该瞎猜一个键码出来。
        assert_eq!(mac_keycode_of(Key::Other(0x01)), None);
        assert_eq!(mac_keycode_of(Key::Other(u32::MAX)), None);
    }

    #[test]
    fn 修饰键组合为carbon掩码() {
        let hk = Hotkey::new(Key::Char('D')).ctrl().alt();
        let m = mods_of(hk);
        assert_eq!(m & CONTROL_KEY, CONTROL_KEY);
        assert_eq!(m & OPTION_KEY, OPTION_KEY);
        assert_eq!(m & SHIFT_KEY, 0, "未声明 shift");
        assert_eq!(m & CMD_KEY, 0, "未声明 meta");
    }

    #[test]
    fn meta映射到command键() {
        // Mods::meta 在 win32 是 Win 键、在 macOS 是 Command——这条约定由 `Mods` 文档
        // 给出，映射错了会让 `Cmd+空格` 之类的热键注册成 `Ctrl+空格`。
        let hk = Hotkey::new(Key::Space).meta();
        assert_eq!(mods_of(hk), CMD_KEY);
    }

    #[test]
    fn 无修饰键时掩码为零() {
        assert_eq!(mods_of(Hotkey::new(Key::Escape)), 0);
    }
}
