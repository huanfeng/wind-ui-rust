//! 键盘映射：运行期 dlopen 的 libxkbcommon（`xkbcommon-dl`）的最小安全封装。
//!
//! Wayland 合成器把 XKB 文本格式的 keymap 以 fd 发来、之后只报「物理键码 + 修饰键掩码」，
//! 键码 → keysym → 字符的解析全在客户端做，这正是 libxkbcommon 的活。它在任何 Wayland
//! 桌面上都装着（合成器自己就要用），故运行期加载即可，编译期不要 `-dev` 包——与
//! fontconfig 同一做法。加载不到时 [`Xkb::available`] 为假，键盘不可用、指针照常（见 `events.rs`）。

use std::ffi::CString;
use std::ptr;

use xkbcommon_dl::{
    xkb_context, xkb_context_flags, xkb_keymap, xkb_keymap_compile_flags, xkb_keymap_format,
    xkb_state, xkb_state_component, xkbcommon_option, XkbCommon,
};

use crate::event::Mods;

/// 一张编译好的 keymap 及其状态。随 `wl_keyboard.keymap` 换新。
pub(super) struct Xkb {
    lib: &'static XkbCommon,
    ctx: *mut xkb_context,
    keymap: *mut xkb_keymap,
    state: *mut xkb_state,
}

impl Xkb {
    /// libxkbcommon 能否加载（进程内只尝试一次）。
    pub fn available() -> bool {
        xkbcommon_option().is_some()
    }

    /// 从 XKB 文本格式编译 keymap。库加载不到或编译失败返回 `None`。
    pub fn from_text(text: &[u8]) -> Option<Self> {
        let lib = xkbcommon_option()?;
        // 合成器给的 buffer 末尾带 NUL，也可能不带；统一去掉再补。
        let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
        let src = CString::new(&text[..end]).ok()?;
        // SAFETY：以下均为 libxkbcommon 的公开 C API；每个返回指针都先判空再用，
        // 失败路径逐个 unref 已拿到的对象，所有权最终交给 `Xkb` 的 Drop。
        unsafe {
            // 从完整文本编译不需要 include 路径（合成器发来的 keymap 是自包含的）。
            let ctx = (lib.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_DEFAULT_INCLUDES);
            if ctx.is_null() {
                return None;
            }
            let keymap = (lib.xkb_keymap_new_from_string)(
                ctx,
                src.as_ptr(),
                xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            );
            if keymap.is_null() {
                (lib.xkb_context_unref)(ctx);
                return None;
            }
            let state = (lib.xkb_state_new)(keymap);
            if state.is_null() {
                (lib.xkb_keymap_unref)(keymap);
                (lib.xkb_context_unref)(ctx);
                return None;
            }
            Some(Self {
                lib,
                ctx,
                keymap,
                state,
            })
        }
    }

    /// 按系统默认 RMLVO（环境变量 / 内置默认）编一张 keymap。只给单测用：真跑时 keymap
    /// 恒由合成器发来。
    #[cfg(test)]
    pub fn from_default_names() -> Option<Self> {
        let lib = xkbcommon_option()?;
        // SAFETY：同 `from_text`。
        unsafe {
            let ctx = (lib.xkb_context_new)(xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            if ctx.is_null() {
                return None;
            }
            let keymap = (lib.xkb_keymap_new_from_names)(
                ctx,
                ptr::null(),
                xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS,
            );
            if keymap.is_null() {
                (lib.xkb_context_unref)(ctx);
                return None;
            }
            let state = (lib.xkb_state_new)(keymap);
            if state.is_null() {
                (lib.xkb_keymap_unref)(keymap);
                (lib.xkb_context_unref)(ctx);
                return None;
            }
            Some(Self {
                lib,
                ctx,
                keymap,
                state,
            })
        }
    }

    /// `wl_keyboard.modifiers`：合成器同步来的修饰键与布局组。
    pub fn update_mask(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        // SAFETY：state 在 self 存活期间有效。
        unsafe {
            (self.lib.xkb_state_update_mask)(self.state, depressed, latched, locked, 0, 0, group);
        }
    }

    /// 单测里模拟按下 / 松开（真跑时修饰键状态只经 `update_mask` 同步）。
    #[cfg(test)]
    pub fn update_key(&mut self, keycode: u32, down: bool) {
        use xkbcommon_dl::xkb_key_direction;
        let dir = if down {
            xkb_key_direction::XKB_KEY_DOWN
        } else {
            xkb_key_direction::XKB_KEY_UP
        };
        // SAFETY：同上。
        unsafe {
            (self.lib.xkb_state_update_key)(self.state, keycode, dir);
        }
    }

    /// 按当前修饰键与布局解析出的 keysym（XKB 键码 = evdev 键码 + 8）。
    pub fn keysym(&self, keycode: u32) -> u32 {
        // SAFETY：同上。
        unsafe { (self.lib.xkb_state_key_get_one_sym)(self.state, keycode) }
    }

    /// 该键按住时是否应自动重复（修饰键等不重复，由 keymap 决定）。
    pub fn repeats(&self, keycode: u32) -> bool {
        // SAFETY：同上。
        unsafe { (self.lib.xkb_keymap_key_repeats)(self.keymap, keycode) != 0 }
    }

    /// 当前生效的修饰键。
    pub fn mods(&self) -> Mods {
        let active = |name: &[u8]| {
            // SAFETY：name 是以 NUL 结尾的常量（xkbcommon-dl 的 XKB_MOD_NAME_*）。
            unsafe {
                (self.lib.xkb_state_mod_name_is_active)(
                    self.state,
                    name.as_ptr().cast(),
                    xkb_state_component::XKB_STATE_MODS_EFFECTIVE,
                ) > 0
            }
        };
        Mods {
            shift: active(xkbcommon_dl::XKB_MOD_NAME_SHIFT),
            ctrl: active(xkbcommon_dl::XKB_MOD_NAME_CTRL),
            alt: active(xkbcommon_dl::XKB_MOD_NAME_ALT),
            meta: active(xkbcommon_dl::XKB_MOD_NAME_LOGO),
        }
    }

    /// 某键在某布局第 0 级（不按 Shift）上的 keysym 列表。
    fn level0(&self, keycode: u32, layout: u32) -> &[u32] {
        let mut syms: *const u32 = ptr::null();
        // SAFETY：返回的数组归 keymap 所有，生命周期不短于 &self。
        unsafe {
            let n = (self.lib.xkb_keymap_key_get_syms_by_level)(
                self.keymap,
                keycode,
                layout,
                0,
                &mut syms,
            );
            if n <= 0 || syms.is_null() {
                &[]
            } else {
                std::slice::from_raw_parts(syms, n as usize)
            }
        }
    }

    /// 快捷键码（`Key::Other`）：当前布局基础层的 keysym 取码；非拉丁布局（俄文等）回退到
    /// 同一键位上其它布局的拉丁字母——Ctrl+С 即 Ctrl+C，与 win32 按物理键取码一致。
    ///
    /// 与 X11 后端的一处差异：X11 取的是**第一布局**的基础层，这里取**当前布局**。两个都是
    /// 拉丁布局（如 `de,us`）时，切到第二布局后 Z / Y 位置的快捷键码两边不同——这里跟用户
    /// 眼前的布局走，更符合直觉。
    pub fn shortcut_code(&self, keycode: u32) -> Option<u32> {
        // SAFETY：同上。
        let (layout, layouts) = unsafe {
            (
                (self.lib.xkb_state_key_get_layout)(self.state, keycode),
                (self.lib.xkb_keymap_num_layouts_for_key)(self.keymap, keycode),
            )
        };
        let first = self.level0(keycode, layout).first().copied();
        first
            .and_then(super::super::keys::shortcut_code)
            .or_else(|| {
                (0..layouts)
                    .filter_map(|l| self.level0(keycode, l).first().copied())
                    .find_map(super::super::keys::shortcut_code)
            })
    }
}

impl Drop for Xkb {
    fn drop(&mut self) {
        // SAFETY：三个对象都由本结构体独占持有，这里各 unref 一次。
        unsafe {
            (self.lib.xkb_state_unref)(self.state);
            (self.lib.xkb_keymap_unref)(self.keymap);
            (self.lib.xkb_context_unref)(self.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// evdev KEY_A = 30，XKB 键码 +8。
    const KC_A: u32 = 38;
    const KC_LSHIFT: u32 = 50;

    /// 需要本机装有 libxkbcommon 与 XKB 数据（任何装过 X / Wayland 的系统都有）。
    /// 拿不到时明说跳过；设了 `WINDUI_REQUIRE_XKB=1`（CI 该设）则直接失败，免得空过。
    fn default_map() -> Option<Xkb> {
        let x = Xkb::from_default_names();
        if x.is_none() {
            assert!(
                std::env::var("WINDUI_REQUIRE_XKB").as_deref() != Ok("1"),
                "WINDUI_REQUIRE_XKB 已设，但本机无 libxkbcommon 或 XKB 数据"
            );
            eprintln!("跳过：本机无 libxkbcommon 或 XKB 数据");
        }
        x
    }

    #[test]
    fn letters_resolve_through_shift_and_give_shortcut_codes() {
        let Some(mut x) = default_map() else { return };
        assert_eq!(x.keysym(KC_A), 0x61, "a");
        assert!(x.repeats(KC_A));
        assert!(!x.repeats(KC_LSHIFT), "修饰键不自动重复");
        x.update_key(KC_LSHIFT, true);
        assert_eq!(x.keysym(KC_A), 0x41, "Shift+a = A");
        assert!(x.mods().shift);
        assert_eq!(
            x.shortcut_code(KC_A),
            Some(b'A' as u32),
            "快捷键码按基础层取"
        );
        x.update_key(KC_LSHIFT, false);
        assert!(!x.mods().shift);
    }

    #[test]
    fn text_keymap_round_trips() {
        let Some(x) = default_map() else { return };
        // SAFETY：get_as_string 返回 malloc 的 C 字符串，读完 free。
        let text = unsafe {
            let p = (x.lib.xkb_keymap_get_as_string)(
                x.keymap,
                xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
            );
            assert!(!p.is_null(), "xkb_keymap_get_as_string 返回空指针");
            let s = std::ffi::CStr::from_ptr(p).to_bytes_with_nul().to_vec();
            extern "C" {
                fn free(p: *mut std::ffi::c_void);
            }
            free(p as *mut _);
            s
        };
        let y = Xkb::from_text(&text).expect("合成器发来的就是这种文本");
        assert_eq!(y.keysym(KC_A), 0x61);
    }
}
