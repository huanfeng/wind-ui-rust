//! 全局热键（X11 `GrabKey`）：在根窗口上抓住指定组合，应用无焦点、窗口隐藏时亦可触发。
//!
//! 事件驱动：服务器把命中的按键作为 `KeyPress` 送给我们（`event` 是根窗口），不轮询，
//! 不破坏「空闲零 CPU」。对照 win32 `RegisterHotKey` / macOS `RegisterEventHotKey`。
//!
//! 一个组合要抓四遍：X 的修饰掩码把 CapsLock、NumLock 也算进去，只抓「Ctrl+Alt+K」的话，
//! 开着数字锁时按下去的是「Ctrl+Alt+Mod2+K」，匹配不上。
//!
//! Wayland 会话下经 XWayland 抓键只在 X 客户端有焦点时生效（合成器不把全局按键交给
//! XWayland）——那是协议层面的限制，需要走 xdg-desktop-portal 的 GlobalShortcuts。

use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, ModMask, Window};
use x11rb::rust_connection::RustConnection;

use super::keys::{self, Keymap};
use crate::event::{Hotkey, HotkeyCtx, HotkeyOp, Key, WindowOp};
use crate::platform::HotkeyBinding;

struct Slot {
    hotkey: Hotkey,
    callback: Box<dyn FnMut(&mut HotkeyCtx)>,
    enabled: bool,
    /// 已抓住的（键码, 修饰掩码）；`None` = 未注册（映射不出键码或组合被别的程序占着）。
    grabbed: Option<(u8, u16)>,
}

pub(super) struct Hotkeys {
    slots: Vec<Slot>,
}

/// 抓键时额外覆盖的「锁定类」修饰组合：无、CapsLock、NumLock、两者。
const LOCK_VARIANTS: [u16; 4] = [0, keys::LOCK, keys::MOD2, keys::LOCK | keys::MOD2];

impl Hotkeys {
    /// 注册全部绑定。单个失败不影响其余（组合被占用是常态而非异常），语义同其它平台。
    pub fn install(
        conn: &RustConnection,
        root: Window,
        keymap: &Keymap,
        bindings: Vec<HotkeyBinding>,
    ) -> Self {
        let slots = bindings
            .into_iter()
            .map(|b| Slot {
                grabbed: grab(conn, root, keymap, b.hotkey),
                hotkey: b.hotkey,
                callback: b.callback,
                enabled: true,
            })
            .collect();
        Hotkeys { slots }
    }

    /// 根窗口上的一次按键：命中某个热键就跑它的回调，返回回调声明的窗口意图。
    /// 未命中返回 `None`（外层 `Option`）。
    pub fn dispatch(&mut self, keycode: u8, state: u16) -> Option<Option<WindowOp>> {
        let mods = state & !(keys::LOCK | keys::MOD2);
        let slot = self
            .slots
            .iter_mut()
            .find(|s| s.enabled && s.grabbed == Some((keycode, mods)))?;
        let mut ctx = HotkeyCtx::default();
        (slot.callback)(&mut ctx);
        Some(ctx.take_op())
    }

    /// 运行期改绑 / 启停。改绑失败时回滚到旧组合——绝不让一次失败的改绑把原本可用的
    /// 热键弄丢（与 win32 / macOS 逐条对齐）。
    pub fn apply(
        &mut self,
        conn: &RustConnection,
        root: Window,
        keymap: &Keymap,
        id: usize,
        op: HotkeyOp,
    ) {
        let Some(slot) = self.slots.get_mut(id) else {
            return;
        };
        match op {
            HotkeyOp::Rebind(new) => {
                if let Some(g) = slot.grabbed.take() {
                    ungrab(conn, root, g);
                }
                if !slot.enabled {
                    slot.hotkey = new;
                    return;
                }
                match grab(conn, root, keymap, new) {
                    Some(g) => {
                        slot.hotkey = new;
                        slot.grabbed = Some(g);
                    }
                    None => {
                        slot.grabbed = grab(conn, root, keymap, slot.hotkey);
                        log::warn!("热键改绑失败（组合被占用？），保留旧绑定");
                    }
                }
            }
            HotkeyOp::SetEnabled(on) => {
                slot.enabled = on;
                if !on {
                    if let Some(g) = slot.grabbed.take() {
                        ungrab(conn, root, g);
                    }
                } else if slot.grabbed.is_none() {
                    slot.grabbed = grab(conn, root, keymap, slot.hotkey);
                }
            }
        }
    }
}

impl Hotkeys {
    /// 键盘映射变了（换布局、xmodmap）：键码可能对应到别的键，按新映射全部重抓。
    pub fn regrab_all(&mut self, conn: &RustConnection, root: Window, keymap: &Keymap) {
        for slot in &mut self.slots {
            if let Some(g) = slot.grabbed.take() {
                ungrab(conn, root, g);
            }
            if slot.enabled {
                slot.grabbed = grab(conn, root, keymap, slot.hotkey);
            }
        }
    }
}

fn mask_of(hk: Hotkey) -> u16 {
    let mut m = 0;
    if hk.mods.shift {
        m |= keys::SHIFT;
    }
    if hk.mods.ctrl {
        m |= keys::CONTROL;
    }
    if hk.mods.alt {
        m |= keys::MOD1;
    }
    if hk.mods.meta {
        m |= keys::MOD4;
    }
    m
}

/// 抓键。同步检查结果（组合被别的客户端抓着时服务器回 BadAccess）；四个变体里任何一个
/// 失败都整体回退，免得留下「开着数字锁才不灵」的半注册状态。
fn grab(conn: &RustConnection, root: Window, keymap: &Keymap, hk: Hotkey) -> Option<(u8, u16)> {
    let ks = keysym_of(hk.key)?;
    let keycode = keymap.keycode_of(ks)?;
    let mods = mask_of(hk);
    for (i, lock) in LOCK_VARIANTS.iter().enumerate() {
        let ok = conn
            .grab_key(
                // owner_events=false：本程序自己有焦点时事件也报在根窗口上，否则热键会以
                // 普通按键落进自己的窗口、被当成文本输入。
                false,
                root,
                ModMask::from(mods | lock),
                keycode,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .ok()
            .and_then(|c| c.check().ok())
            .is_some();
        if !ok {
            for lock in &LOCK_VARIANTS[..i] {
                let _ = conn.ungrab_key(keycode, root, ModMask::from(mods | lock));
            }
            log::warn!("全局热键 {hk:?} 注册失败（组合可能已被其它程序占用）");
            return None;
        }
    }
    Some((keycode, mods))
}

fn ungrab(conn: &RustConnection, root: Window, (keycode, mods): (u8, u16)) {
    for lock in LOCK_VARIANTS {
        let _ = conn.ungrab_key(keycode, root, ModMask::from(mods | lock));
    }
}

/// 框架键 → keysym（与 `keys::special_key` 互逆）。
fn keysym_of(key: Key) -> Option<u32> {
    Some(match key {
        Key::Tab => 0xff09,
        Key::Enter => 0xff0d,
        Key::Escape => 0xff1b,
        Key::Space => 0x20,
        Key::Backspace => 0xff08,
        Key::Delete => 0xffff,
        Key::Left => 0xff51,
        Key::Right => 0xff53,
        Key::Up => 0xff52,
        Key::Down => 0xff54,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::Insert => 0xff63,
        Key::F(n @ 1..=12) => 0xffbe + n as u32 - 1,
        Key::NumpadAdd => 0xffab,
        Key::NumpadSubtract => 0xffad,
        Key::NumpadMultiply => 0xffaa,
        Key::NumpadDivide => 0xffaf,
        Key::ContextMenu => 0xff67,
        // 字母键按小写 keysym 查（键盘映射表第一列就是小写）；Other 走 win32 VK 口径。
        Key::Char(c) => ascii_keysym(c)?,
        Key::Other(vk) => ascii_keysym(char::from_u32(vk)?)?,
        _ => return None,
    })
}

fn ascii_keysym(c: char) -> Option<u32> {
    if c.is_ascii_alphanumeric() {
        Some(c.to_ascii_lowercase() as u32)
    } else if c.is_ascii_graphic() {
        Some(c as u32)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_round_trip_through_special_key() {
        for k in [
            Key::Tab,
            Key::Enter,
            Key::Escape,
            Key::Space,
            Key::Backspace,
            Key::Delete,
            Key::Left,
            Key::Right,
            Key::Up,
            Key::Down,
            Key::Home,
            Key::End,
            Key::PageUp,
            Key::PageDown,
            Key::Insert,
            Key::F(1),
            Key::F(12),
            Key::NumpadAdd,
            Key::ContextMenu,
        ] {
            let ks = keysym_of(k).unwrap();
            assert_eq!(keys::special_key(ks), Some(k), "{k:?}");
        }
    }

    #[test]
    fn letters_map_to_lowercase_keysyms() {
        assert_eq!(keysym_of(Key::Char('K')), Some(b'k' as u32));
        assert_eq!(keysym_of(Key::Other(b'K' as u32)), Some(b'k' as u32));
        assert_eq!(keysym_of(Key::Other(b'5' as u32)), Some(b'5' as u32));
        assert_eq!(keysym_of(Key::F(13)), None);
    }

    #[test]
    fn modifiers_become_x_mask() {
        let hk = Hotkey::new(Key::Char('k')).ctrl().alt();
        assert_eq!(mask_of(hk), keys::CONTROL | keys::MOD1);
        assert_eq!(mask_of(Hotkey::new(Key::F(1)).meta()), keys::MOD4);
    }
}
