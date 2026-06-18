//! 输入事件类型。平台层产生物理像素坐标，但 `UiHost::on_pointer` 在分发前
//! 已 ÷scale 转为**逻辑坐标**——控件 `on_event` 收到的 pos 是逻辑坐标。

use crate::geometry::Point;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// 控件期望的鼠标光标形状。`Widget::cursor()` 据交互语义声明，宿主取当前悬停
/// 节点的形状交平台层应答（win32 `WM_SETCURSOR`）。禁用节点恒回退 `Arrow`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    /// 默认箭头。
    #[default]
    Arrow,
    /// 手型（链接等可点击文本）。
    Hand,
    /// 文本 I 形（文本输入/可编辑区）。
    Text,
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

/// 浮层菜单/下拉项的动作。两种：向焦点控件合成按键（右键菜单复用控件键盘处理、
/// 可移植），或运行任意闭包（下拉选择设置绑定值等）。
#[derive(Clone)]
pub enum MenuAction {
    SendKey(KeyEvent),
    Run(std::rc::Rc<dyn Fn()>),
}

/// 一个浮层菜单/下拉项。
#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub action: MenuAction,
    /// 禁用项变灰且不可点击（如无选区时的"复制"）。
    pub enabled: bool,
    /// 当前选中项（下拉用，渲染勾选标记）。
    pub checked: bool,
}

impl MenuItem {
    /// 便捷构造：标签 + 合成按键。
    pub fn key(label: impl Into<String>, key: KeyEvent, enabled: bool) -> Self {
        Self { label: label.into(), action: MenuAction::SendKey(key), enabled, checked: false }
    }
    /// 便捷构造：标签 + 闭包动作。
    pub fn run(label: impl Into<String>, f: impl Fn() + 'static, checked: bool) -> Self {
        Self {
            label: label.into(),
            action: MenuAction::Run(std::rc::Rc::new(f)),
            enabled: true,
            checked,
        }
    }
}

/// 控件经 `EventCtx::show_context_menu` / `show_menu` 发起的浮层请求。
#[derive(Clone)]
pub struct MenuRequest {
    /// 锚点（逻辑坐标，菜单左上角，宿主据窗口边界钳制）。
    pub pos: Point,
    pub items: Vec<MenuItem>,
    /// 最小宽度（逻辑 px，0=按内容）。下拉用控件宽度对齐。
    pub min_width: i32,
}
