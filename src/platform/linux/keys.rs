//! X11 键码 → keysym → 框架键 / 字符。
//!
//! 不引 xkbcommon：用核心协议的 `GetKeyboardMapping` 表，只取第一组（group 1）的
//! 「未按 Shift / 按 Shift」两列，再按 CapsLock / NumLock 修正。覆盖常规布局下的
//! 文本输入与快捷键；死键、多组布局切换等交给输入法（XIM）处理。

use crate::event::Key;

/// 修饰键位（X 的 `KeyButMask` 低位）。
pub(super) const SHIFT: u16 = 1;
pub(super) const LOCK: u16 = 2;
pub(super) const CONTROL: u16 = 4;
pub(super) const MOD1: u16 = 8; // Alt
pub(super) const MOD2: u16 = 16; // NumLock（绝大多数布局）
pub(super) const MOD4: u16 = 64; // Super / Win

/// 键盘映射表：`syms[(keycode - min) * per + col]`。
#[derive(Default)]
pub(super) struct Keymap {
    min: u8,
    per: usize,
    syms: Vec<u32>,
}

impl Keymap {
    pub fn new(min: u8, per: u8, syms: Vec<u32>) -> Self {
        Self {
            min,
            per: per as usize,
            syms,
        }
    }

    fn col(&self, keycode: u8, col: usize) -> u32 {
        if self.per == 0 || keycode < self.min || col >= self.per {
            return 0;
        }
        let i = (keycode - self.min) as usize * self.per + col;
        self.syms.get(i).copied().unwrap_or(0)
    }

    /// 不计修饰的基础 keysym（快捷键用：Ctrl+Shift+A 要认成 A 而不是别的符号）。
    pub fn base(&self, keycode: u8) -> u32 {
        self.col(keycode, 0)
    }

    /// keysym → 产生它的键码（反查，热键注册用）。查**所有列**：`setxkbmap ru,us` 这类
    /// 多布局配置下拉丁字母在第 3/4 列，只查前两列的话热键注册失败、切布局后重抓也失败。
    pub fn keycode_of(&self, ks: u32) -> Option<u8> {
        if self.per == 0 {
            return None;
        }
        let n = self.syms.len() / self.per;
        (0..n).find_map(|i| {
            let row = &self.syms[i * self.per..(i + 1) * self.per];
            row.contains(&ks).then(|| self.min.wrapping_add(i as u8))
        })
    }

    /// 该键位上任一列里的 ASCII 字母 / 数字 keysym——快捷键在非拉丁布局下的回退：
    /// 俄语布局按 Ctrl+С（西里尔字母）时，应用要的仍是 Ctrl+C（同一物理键位），
    /// 与 win32 按物理 VK 取码的行为一致。多布局配置下拉丁组通常在第 3/4 列。
    pub fn ascii_on_key(&self, keycode: u8) -> Option<u32> {
        (0..self.per)
            .map(|c| self.col(keycode, c))
            .find(|&ks| keysym_char(ks).is_some_and(|c| c.is_ascii_alphanumeric()))
    }

    /// 按修饰状态取 keysym。
    pub fn keysym(&self, keycode: u8, state: u16) -> u32 {
        let lower = self.col(keycode, 0);
        let mut upper = self.col(keycode, 1);
        if upper == 0 {
            // 只有一列时按 X 协议约定补出大小写对。
            upper = to_upper(lower);
        }
        let shift = state & SHIFT != 0;
        // 小键盘：NumLock 开时取第二列（数字），与 Shift 相反。
        if is_keypad(upper) && state & MOD2 != 0 {
            return if shift { lower } else { upper };
        }
        let caps = state & LOCK != 0 && is_letter(lower);
        if shift ^ caps {
            upper
        } else {
            lower
        }
    }
}

fn is_keypad(ks: u32) -> bool {
    (0xff80..=0xffbd).contains(&ks)
}

fn is_letter(ks: u32) -> bool {
    (b'a' as u32..=b'z' as u32).contains(&ks)
        || (0xe0..=0xfe).contains(&ks) && ks != 0xf7
        || keysym_char(ks).is_some_and(|c| c.is_lowercase())
}

fn to_upper(ks: u32) -> u32 {
    match keysym_char(ks) {
        Some(c) if c.is_lowercase() => {
            let u = c.to_uppercase().next().unwrap_or(c);
            char_keysym(u)
        }
        _ => ks,
    }
}

fn char_keysym(c: char) -> u32 {
    let cp = c as u32;
    if (0x20..=0x7e).contains(&cp) || (0xa0..=0xff).contains(&cp) {
        cp
    } else {
        0x0100_0000 | cp
    }
}

/// keysym → 它代表的文本字符（控制键返回 `None`）。
pub(super) fn keysym_char(ks: u32) -> Option<char> {
    match ks {
        0x20..=0x7e | 0xa0..=0xff => char::from_u32(ks),
        // Unicode keysym。
        0x0100_0100..=0x0110_ffff => char::from_u32(ks - 0x0100_0000),
        // 小键盘上的可打印键。
        0xffb0..=0xffb9 => char::from_u32(b'0' as u32 + ks - 0xffb0),
        0xffaa => Some('*'),
        0xffab => Some('+'),
        0xffad => Some('-'),
        0xffae => Some('.'),
        0xffaf => Some('/'),
        0xff80 => Some(' '),
        // 旧式（非 Unicode）keysym：俄语、乌克兰语、希腊语等布局至今仍发这些码。
        0x06a1..=0x06ff => table_char(CYRILLIC, ks - 0x06a1),
        0x07c1..=0x07f9 => table_char(GREEK, ks - 0x07c1),
        _ => None,
    }
}

/// `table` 按 keysym 连续排列，`'\0'` 表示该码位未分配。
fn table_char(table: &str, off: u32) -> Option<char> {
    table.chars().nth(off as usize).filter(|c| *c != '\0')
}

/// keysym 0x06a1..=0x06ff（keysymdef.h 的 Cyrillic 段）。
const CYRILLIC: &str = concat!(
    "ђѓёєѕіїјљњћќґўџ",                  // 0x6a1..0x6af
    "№ЂЃЁЄЅІЇЈЉЊЋЌҐЎЏ",                 // 0x6b0..0x6bf
    "юабцдефгхийклмнопярстужвьызшэщчъ", // 0x6c0..0x6df
    "ЮАБЦДЕФГХИЙКЛМНОПЯРСТУЖВЬЫЗШЭЩЧЪ", // 0x6e0..0x6ff
);

/// keysym 0x07c1..=0x07f9（keysymdef.h 的 Greek 段，不含带重音的 0x7a1..0x7bb）。
const GREEK: &str = concat!(
    "ΑΒΓΔΕΖΗΘΙΚΛΜΝΞΟΠΡΣ\0ΤΥΦΧΨΩ", // 0x7c1..0x7d9
    "\0\0\0\0\0\0\0",             // 0x7da..0x7e0
    "αβγδεζηθικλμνξοπρσςτυφχψω",  // 0x7e1..0x7f9
);

/// 具名键。与 win32 `map_vk` / macOS `map_special` 覆盖同一组键。
pub(super) fn special_key(ks: u32) -> Option<Key> {
    Some(match ks {
        0xff09 | 0xfe20 => Key::Tab, // Tab / ISO_Left_Tab（Shift+Tab）
        0xff0d | 0xff8d => Key::Enter,
        0xff1b => Key::Escape,
        0x20 | 0xff80 => Key::Space,
        0xff08 => Key::Backspace,
        0xffff | 0xff9f => Key::Delete,
        0xff51 | 0xff96 => Key::Left,
        0xff53 | 0xff98 => Key::Right,
        0xff52 | 0xff97 => Key::Up,
        0xff54 | 0xff99 => Key::Down,
        0xff50 | 0xff95 => Key::Home,
        0xff57 | 0xff9c => Key::End,
        0xff55 | 0xff9a => Key::PageUp,
        0xff56 | 0xff9b => Key::PageDown,
        0xff63 | 0xff9e => Key::Insert,
        0xffbe..=0xffc9 => Key::F((ks - 0xffbe + 1) as u8),
        0xffab => Key::NumpadAdd,
        0xffad => Key::NumpadSubtract,
        0xffaa => Key::NumpadMultiply,
        0xffaf => Key::NumpadDivide,
        0xff67 => Key::ContextMenu,
        0xffe9 | 0xffea => Key::Alt,
        _ => return None,
    })
}

/// 纯修饰键（按下本身不产生任何事件，Alt 除外——它由 [`special_key`] 认）。
pub(super) fn is_modifier(ks: u32) -> bool {
    (0xffe1..=0xffee).contains(&ks) || ks == 0xfe03
}

/// 快捷键路径的 `Key::Other` 码：与 win32 VK 对齐（字母取大写 ASCII、数字取 ASCII）。
pub(super) fn shortcut_code(base_ks: u32) -> Option<u32> {
    let c = keysym_char(base_ks)?;
    if c.is_ascii_alphanumeric() {
        Some(c.to_ascii_uppercase() as u32)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两个键：keycode 10 = a/A，keycode 11 = KP_Home/KP_7。
    fn map() -> Keymap {
        Keymap::new(10, 2, vec![b'a' as u32, b'A' as u32, 0xff95, 0xffb7])
    }

    #[test]
    fn shift_and_caps_select_case() {
        let m = map();
        assert_eq!(m.keysym(10, 0), b'a' as u32);
        assert_eq!(m.keysym(10, SHIFT), b'A' as u32);
        assert_eq!(m.keysym(10, LOCK), b'A' as u32);
        assert_eq!(m.keysym(10, LOCK | SHIFT), b'a' as u32);
    }

    #[test]
    fn numlock_turns_keypad_into_digits() {
        let m = map();
        assert_eq!(special_key(m.keysym(11, 0)), Some(Key::Home));
        assert_eq!(keysym_char(m.keysym(11, MOD2)), Some('7'));
    }

    #[test]
    fn unicode_and_latin1_keysyms_map_to_chars() {
        assert_eq!(keysym_char(0x0100_4e2d), Some('中'));
        assert_eq!(keysym_char(0xe9), Some('é'));
        assert_eq!(keysym_char(0xff0d), None);
    }

    #[test]
    fn single_column_keysym_gets_implied_uppercase() {
        let m = Keymap::new(10, 1, vec![b'q' as u32]);
        assert_eq!(m.keysym(10, SHIFT), b'Q' as u32);
    }

    #[test]
    fn keycode_lookup_inverts_the_table() {
        let m = map();
        assert_eq!(m.keycode_of(b'a' as u32), Some(10));
        assert_eq!(m.keycode_of(b'A' as u32), Some(10));
        assert_eq!(m.keycode_of(0xffb7), Some(11));
        assert_eq!(m.keycode_of(b'z' as u32), None);
    }

    #[test]
    fn legacy_cyrillic_and_greek_keysyms_map_to_chars() {
        assert_eq!(keysym_char(0x6c1), Some('а'));
        assert_eq!(keysym_char(0x6df), Some('ъ'));
        assert_eq!(keysym_char(0x6e1), Some('А'));
        assert_eq!(keysym_char(0x6a3), Some('ё'));
        assert_eq!(keysym_char(0x6b0), Some('№'));
        assert_eq!(keysym_char(0x7c1), Some('Α'));
        assert_eq!(keysym_char(0x7d9), Some('Ω'));
        assert_eq!(keysym_char(0x7d3), None);
        assert_eq!(keysym_char(0x7e1), Some('α'));
        assert_eq!(keysym_char(0x7f3), Some('ς'));
        assert_eq!(keysym_char(0x7f9), Some('ω'));
    }

    #[test]
    fn caps_lock_uppercases_cyrillic() {
        let m = Keymap::new(10, 2, vec![0x6d3, 0x6f3]); // с / С
        assert_eq!(keysym_char(m.keysym(10, LOCK)), Some('С'));
    }

    #[test]
    fn shortcut_falls_back_to_latin_column() {
        // 俄语在第 1/2 列，美式拉丁在第 3/4 列（多布局配置的常见形状）。
        let m = Keymap::new(10, 4, vec![0x6d3, 0x6f3, b'c' as u32, b'C' as u32]);
        assert_eq!(shortcut_code(m.base(10)), None);
        assert_eq!(
            m.ascii_on_key(10).and_then(shortcut_code),
            Some(b'C' as u32)
        );
    }

    #[test]
    fn function_keys_and_shortcut_codes() {
        assert_eq!(special_key(0xffbe), Some(Key::F(1)));
        assert_eq!(special_key(0xffc9), Some(Key::F(12)));
        assert_eq!(shortcut_code(b'c' as u32), Some(b'C' as u32));
        assert_eq!(shortcut_code(b'5' as u32), Some(b'5' as u32));
        assert_eq!(shortcut_code(b'-' as u32), None);
    }
}
