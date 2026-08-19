//! 全局热键（RegisterHotKey）：应用无焦点、窗口隐藏时亦可触发。
//!
//! 热键消息 `WM_HOTKEY` 由系统投递到本窗口的消息队列——**事件驱动，不轮询**，
//! 故不破坏「空闲零 CPU」这条核心指标（AGENTS.md）。
//!
//! ## 借用纪律
//!
//! 回调拿到的 [`HotkeyCtx`] **不持有 hwnd**，只能声明 [`WindowOp`] 意图。这是刻意的：
//! 回调在 `wnd_proc` 里持有 `WindowState` 借用期间执行，此时若直接调 `ShowWindow` /
//! `SetForegroundWindow`，这些 API 会**同步**派发 `WM_SHOWWINDOW` / `WM_ACTIVATE`
//! 回 `wnd_proc`，那里再 `state_from` 一次即造成 `&mut` 别名（铁律 6）。
//!
//! 把窗口操作降级为「意图」、由调用方在借用释放后统一执行，使该约束成为**类型上的
//! 保证**而非人的记性——ctx 里根本没有句柄，危险代码写不出来。

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR,
    VK_RETURN, VK_RIGHT, VK_SPACE, VK_TAB, VK_UP,
};

use crate::event::{Hotkey, HotkeyCtx, HotkeyOp, Key, WindowOp};
use crate::platform::HotkeyBinding;

/// 一个热键槽：组合、回调与向系统的注册状态。回调**始终保留**（即使当前组合
/// 注册失败）——运行期 `Rebind` 换到可用组合后即恢复生效。
struct Slot {
    hotkey: Hotkey,
    callback: Box<dyn FnMut(&mut HotkeyCtx)>,
    /// 当前是否已在系统注册成功（失败/停用为 false，WM_HOTKEY 不会来）。
    registered: bool,
    /// 用户启用态（`SetEnabled` 控制；停用即注销、组合归还系统）。
    enabled: bool,
}

/// 已注册的热键集合。窗口销毁时 `drop` 自动注销。
pub(crate) struct HotkeyState {
    hwnd: HWND,
    /// 索引即 `RegisterHotKey` 的 id，亦即 `WM_HOTKEY` 的 wParam。
    /// 槽位固定不压缩——紧凑数组会让 id 错位，令 WM_HOTKEY 触发到错误的回调。
    bindings: Vec<Slot>,
}

/// 尝试向系统注册（含 MOD_NOREPEAT：按住不放只触发一次，缺了它长按会以
/// 键盘重复率刷屏般触发回调）。无法映射虚拟键或系统拒绝 → false。
fn try_register(hwnd: HWND, id: usize, hk: Hotkey) -> bool {
    vk_of(hk.key).is_some_and(|vk| {
        let mods = mods_of(hk) | MOD_NOREPEAT;
        unsafe { RegisterHotKey(Some(hwnd), id as i32, mods, vk) }.is_ok()
    })
}

impl HotkeyState {
    /// 注册全部热键。
    ///
    /// 单个热键注册失败**不影响其余热键**，也不阻止窗口创建：热键是全局独占资源，
    /// 组合被别的程序占用是常态而非异常，让整个应用起不来是不可接受的。失败者静默
    /// 忽略；运行期可经 [`HotkeyOp::Rebind`] 换组合恢复。
    pub(crate) fn register(hwnd: HWND, bindings: Vec<HotkeyBinding>) -> Self {
        let bindings = bindings
            .into_iter()
            .enumerate()
            .map(|(id, b)| Slot {
                registered: try_register(hwnd, id, b.hotkey),
                hotkey: b.hotkey,
                callback: b.callback,
                enabled: true,
            })
            .collect();
        Self { hwnd, bindings }
    }

    /// 运行期热键操作（`HotkeyHandle` 排队、消息循环消费）。
    ///
    /// `Rebind`：先注销旧组合再注册新组合；新组合注册失败（被占用等）时**回滚**
    /// 重注册旧组合——绝不让一次失败的改绑把原本可用的热键弄丢。
    /// `RegisterHotKey`/`UnregisterHotKey` 不向本窗口同步派发消息，可在
    /// `WindowState` 借用内安全调用（与回调执行的借用纪律无冲突）。
    pub(crate) fn apply(&mut self, id: usize, op: HotkeyOp) {
        let hwnd = self.hwnd;
        let Some(slot) = self.bindings.get_mut(id) else {
            return;
        };
        match op {
            HotkeyOp::Rebind(new) => {
                if slot.registered {
                    unsafe {
                        let _ = UnregisterHotKey(Some(hwnd), id as i32);
                    }
                    slot.registered = false;
                }
                if slot.enabled && try_register(hwnd, id, new) {
                    slot.hotkey = new;
                    slot.registered = true;
                } else if slot.enabled {
                    // 回滚：新组合拿不到，旧组合续命。
                    slot.registered = try_register(hwnd, id, slot.hotkey);
                    #[cfg(debug_assertions)]
                    eprintln!("[windui] 热键改绑失败（组合被占用？），保留旧绑定");
                } else {
                    // 停用中只记新组合，待 SetEnabled(true) 时注册。
                    slot.hotkey = new;
                }
            }
            HotkeyOp::SetEnabled(on) => {
                slot.enabled = on;
                if !on && slot.registered {
                    unsafe {
                        let _ = UnregisterHotKey(Some(hwnd), id as i32);
                    }
                    slot.registered = false;
                } else if on && !slot.registered {
                    slot.registered = try_register(hwnd, id, slot.hotkey);
                }
            }
        }
    }

    /// 派发一条 `WM_HOTKEY`，返回回调声明的窗口操作意图。
    ///
    /// **调用方必须在释放 `WindowState` 借用之后**再执行返回的 [`WindowOp`]——
    /// 见本模块头部的借用纪律。
    #[must_use]
    pub(crate) fn dispatch(&mut self, id: usize) -> Option<WindowOp> {
        let slot = self.bindings.get_mut(id)?;
        let mut ctx = HotkeyCtx::default();
        (slot.callback)(&mut ctx);
        ctx.take_op()
    }
}

impl Drop for HotkeyState {
    fn drop(&mut self) {
        for (id, slot) in self.bindings.iter().enumerate() {
            // 只注销注册成功的：对未注册的 id 调 UnregisterHotKey 会失败（无害但无意义）。
            if slot.registered {
                unsafe {
                    let _ = UnregisterHotKey(Some(self.hwnd), id as i32);
                }
            }
        }
    }
}

/// 修饰键 → Win32 标志。
fn mods_of(hk: Hotkey) -> HOT_KEY_MODIFIERS {
    let mut m = HOT_KEY_MODIFIERS(0);
    if hk.mods.ctrl {
        m |= MOD_CONTROL;
    }
    if hk.mods.alt {
        m |= MOD_ALT;
    }
    if hk.mods.shift {
        m |= MOD_SHIFT;
    }
    if hk.mods.meta {
        m |= MOD_WIN;
    }
    m
}

/// `Key` → 虚拟键码。无法映射者返回 `None`。
///
/// 字母与数字的 VK 码等于其**大写 ASCII** 值（`VK_A == 0x41 == b'A'`），故直接换算，
/// 不必逐个枚举。非 ASCII 字符（如 `Key::Char('中')`）没有稳定的 VK 映射——它依赖
/// 当前键盘布局，作全局热键无意义，故拒绝。
///
/// `Key::Other(vk)` **直接放行**：本仓库已把它定义为跨平台对齐的虚拟键码
/// （win32 由 `to_key` 产出原始 VK；macOS 亦按 win32 VK 码对齐，见 `macos/window.rs`）。
/// 它是 F1–F12、PageUp/PageDown、Insert 等键位的**唯一表达途径**——`Key` 枚举没有
/// 这些变体，堵死 `Other` 等于让 `Ctrl+Alt+F1` 这类常见全局热键无法注册。
fn vk_of(key: Key) -> Option<u32> {
    let vk = match key {
        Key::Char(c) if c.is_ascii_alphanumeric() => c.to_ascii_uppercase() as u32,
        Key::Char(_) => return None,
        Key::Tab => VK_TAB.0 as u32,
        Key::Enter => VK_RETURN.0 as u32,
        Key::Escape => VK_ESCAPE.0 as u32,
        Key::Space => VK_SPACE.0 as u32,
        Key::Left => VK_LEFT.0 as u32,
        Key::Right => VK_RIGHT.0 as u32,
        Key::Up => VK_UP.0 as u32,
        Key::Down => VK_DOWN.0 as u32,
        Key::Home => VK_HOME.0 as u32,
        Key::End => VK_END.0 as u32,
        Key::PageUp => VK_PRIOR.0 as u32,
        Key::PageDown => VK_NEXT.0 as u32,
        Key::Delete => VK_DELETE.0 as u32,
        // 虚拟键码是 8 位的；越界值必是调用方搞错了，与其注册出个诡异热键不如拒绝。
        Key::Other(vk) if vk <= 0xFF => vk,
        Key::Other(_) => return None,
        // Backspace 作全局热键无实际用途。
        Key::Backspace => return None,
    };
    Some(vk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 字母键映射为大写ascii的虚拟键码() {
        // VK_A == 0x41 == b'A'。小写输入也须映射到同一个码。
        assert_eq!(vk_of(Key::Char('d')), Some(0x44));
        assert_eq!(vk_of(Key::Char('D')), Some(0x44));
        assert_eq!(vk_of(Key::Char('0')), Some(0x30));
    }

    #[test]
    fn 非ascii字符不可作热键() {
        // 汉字没有稳定的 VK 映射——取决于键盘布局，作全局热键无意义。
        assert_eq!(vk_of(Key::Char('中')), None);
        assert_eq!(vk_of(Key::Char('é')), None);
    }

    #[test]
    fn 具名键映射到对应虚拟键码() {
        assert_eq!(vk_of(Key::Escape), Some(VK_ESCAPE.0 as u32));
        assert_eq!(vk_of(Key::Space), Some(VK_SPACE.0 as u32));
    }

    #[test]
    fn 无意义的键被拒绝() {
        assert_eq!(vk_of(Key::Backspace), None);
    }

    #[test]
    fn other放行为虚拟键码() {
        // Key::Other 在本仓库即跨平台对齐的 VK 码，且是 F 键等键位的唯一表达途径。
        // VK_F1 == 0x70。堵死它等于让 Ctrl+Alt+F1 无法注册。
        assert_eq!(vk_of(Key::Other(0x70)), Some(0x70));
        assert_eq!(vk_of(Key::Other(0x41)), Some(0x41));
    }

    #[test]
    fn 越界的虚拟键码被拒绝() {
        // VK 是 8 位的；越界必是调用方搞错了。
        assert_eq!(vk_of(Key::Other(0x100)), None);
        assert_eq!(vk_of(Key::Other(u32::MAX)), None);
    }

    #[test]
    fn 修饰键组合为标志位() {
        let hk = Hotkey::new(Key::Char('D')).ctrl().alt();
        let m = mods_of(hk);
        assert_eq!(m & MOD_CONTROL, MOD_CONTROL);
        assert_eq!(m & MOD_ALT, MOD_ALT);
        assert_eq!(m & MOD_SHIFT, HOT_KEY_MODIFIERS(0), "未声明 shift");
        assert_eq!(m & MOD_WIN, HOT_KEY_MODIFIERS(0), "未声明 meta");
    }

    #[test]
    fn 无修饰键时标志为零() {
        assert_eq!(mods_of(Hotkey::new(Key::Escape)), HOT_KEY_MODIFIERS(0));
    }

    /// 具名键的两张表必须互逆：`vk_of`（Key→VK，热键注册用）与 `mod.rs` 的 `map_vk`
    /// （VK→Key，输入路径用）。
    ///
    /// 没有它时错在哪：两张表走偏的症状是"注册的是 Home，按下去却当 End 处理"——两处
    /// 各自看都自洽，只有对着看才发现。macOS 侧本就有这条护栏（`具名键与输入路径的键码表一致`），
    /// Windows 侧此前缺；而加 PageUp/PageDown 正是同时动这两张表。
    #[test]
    fn 具名键与输入路径的键码表一致() {
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
            Key::PageUp,
            Key::PageDown,
            Key::Delete,
            // 刻意不含 Backspace：`vk_of` 对它返回 None（作全局热键无实际用途），
            // 而输入路径照常识别 VK_BACK。两张表在这一项上有意不对称。
        ] {
            let vk = vk_of(key).expect("具名键都该有虚拟键码");
            assert_eq!(
                super::super::map_vk(vk as u16),
                key,
                "VK {vk:#04X} 在两张表里对应不同的键"
            );
        }
    }
}
