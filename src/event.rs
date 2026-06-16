//! 输入事件类型。平台层产生物理像素坐标，但 `UiHost::on_pointer` 在分发前
//! 已 ÷scale 转为**逻辑坐标**——控件 `on_event` 收到的 pos 是逻辑坐标。

use crate::geometry::Point;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// 指针动作。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerKind {
    Down,
    Up,
    Move,
    /// 进入某节点（hover 开始）。
    Enter,
    /// 离开某节点（hover 结束）。
    Leave,
    /// 滚轮，携带步进量（正=上滚）。
    Wheel(i32),
}

#[derive(Debug, Clone, Copy)]
pub struct PointerEvent {
    pub kind: PointerKind,
    pub pos: Point,
    pub button: MouseButton,
    /// 连续点击计数（由平台层填充）：1=单击，2=双击，3=三击。
    /// 仅 `Down` 有意义；其余动作恒为 1。控件据此实现双击选词/三击选行。
    pub click_count: u8,
}

impl PointerEvent {
    /// 构造一个单击事件（click_count=1）。便于测试与合成事件。
    pub fn single(kind: PointerKind, pos: Point, button: MouseButton) -> Self {
        Self { kind, pos, button, click_count: 1 }
    }
}

/// 键。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Tab,
    Enter,
    Escape,
    Backspace,
    Delete,
    Space,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Char(char),
    Other(u32),
}

#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub key: Key,
    pub pressed: bool,
    /// Shift 是否按下（用于 Shift+Tab 反向导航、Shift+方向扩展选区）。
    pub shift: bool,
    /// Ctrl 是否按下（用于 Ctrl+A/C/V/X 等）。
    pub ctrl: bool,
}

/// 统一事件。
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Pointer(PointerEvent),
    Key(KeyEvent),
}
