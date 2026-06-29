//! 输入事件类型。平台层产生物理像素坐标，但 `UiHost::on_pointer` 在分发前
//! 已 ÷scale 转为**逻辑坐标**——控件 `on_event` 收到的 pos 是逻辑坐标。

use crate::geometry::Point;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// 窗口操作请求（自定义标题栏按钮等触发，经 DispatchResult 上交宿主执行）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowOp {
    /// 最小化窗口。
    Minimize,
    /// 最大化 / 还原切换。
    ToggleMaximize,
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
        Self {
            kind,
            pos,
            button,
            click_count: 1,
        }
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

/// 一个浮层菜单/下拉项。支持图标、尾随快捷键、分隔线与级联子菜单。
#[derive(Clone)]
pub struct MenuItem {
    pub label: String,
    pub action: MenuAction,
    /// 禁用项变灰且不可点击（如无选区时的"复制"）。
    pub enabled: bool,
    /// 当前选中项（下拉用，渲染勾选标记）。
    pub checked: bool,
    /// 前置图标（字符/emoji，None=无图标列）。
    pub icon: Option<String>,
    /// 尾随快捷键文本（如 "⌘C"）。submenu 非空时显示右箭头优先。
    pub shortcut: Option<String>,
    /// 分隔线项（label/action 忽略，渲染为细线，不可命中）。
    pub separator: bool,
    /// 级联子菜单项（非空 → 悬停展开下一级，行尾显示 ›）。
    pub submenu: Vec<MenuItem>,
}

/// 空动作（分隔线/子菜单父项占位，永不执行）。
fn noop_action() -> MenuAction {
    MenuAction::Run(std::rc::Rc::new(|| {}))
}

impl MenuItem {
    /// 便捷构造：标签 + 合成按键。
    pub fn key(label: impl Into<String>, key: KeyEvent, enabled: bool) -> Self {
        Self {
            label: label.into(),
            action: MenuAction::SendKey(key),
            enabled,
            checked: false,
            icon: None,
            shortcut: None,
            separator: false,
            submenu: Vec::new(),
        }
    }
    /// 便捷构造：标签 + 闭包动作。
    pub fn run(label: impl Into<String>, f: impl Fn() + 'static, checked: bool) -> Self {
        Self {
            label: label.into(),
            action: MenuAction::Run(std::rc::Rc::new(f)),
            enabled: true,
            checked,
            icon: None,
            shortcut: None,
            separator: false,
            submenu: Vec::new(),
        }
    }
    /// 分隔线项。
    pub fn separator() -> Self {
        Self {
            label: String::new(),
            action: noop_action(),
            enabled: false,
            checked: false,
            icon: None,
            shortcut: None,
            separator: true,
            submenu: Vec::new(),
        }
    }
    /// 级联子菜单父项：悬停展开 `items`。
    pub fn submenu(label: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            label: label.into(),
            action: noop_action(),
            enabled: true,
            checked: false,
            icon: None,
            shortcut: None,
            separator: false,
            submenu: items,
        }
    }
    /// 设置前置图标（字符/emoji）。
    pub fn with_icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }
    /// 设置尾随快捷键文本。
    pub fn with_shortcut(mut self, s: impl Into<String>) -> Self {
        self.shortcut = Some(s.into());
        self
    }
    /// 设置选中勾。
    pub fn with_check(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    /// 是否可点击执行（非分隔、无子菜单、启用）。
    pub fn is_actionable(&self) -> bool {
        !self.separator && self.submenu.is_empty() && self.enabled
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
    /// 下拉控件自身的顶部 y（逻辑坐标）：空间不足时菜单向上翻转，避免遮住控件。
    /// 普通右键菜单留 None，不需要翻转语义。
    pub anchor_top: Option<i32>,
}

/// 轻提示语义类型：决定提示图标（及默认强调色）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum ToastKind {
    /// 中性信息（ℹ）。
    #[default]
    Info,
    /// 成功（✓），如"已添加到剪贴板"。
    Success,
    /// 失败/错误（✕）。
    Error,
}

impl ToastKind {
    /// 提示图标字形（用 `draw_text` 绘制）。
    pub fn glyph(self) -> &'static str {
        match self {
            ToastKind::Info => "\u{2139}",    // ℹ
            ToastKind::Success => "\u{2713}", // ✓
            ToastKind::Error => "\u{2715}",   // ✕
        }
    }
}

/// 控件经 `EventCtx::toast*` 发起的轻提示请求。宿主接管居中浮层渲染、淡入淡出与定时消失。
#[derive(Clone)]
pub struct ToastRequest {
    pub text: String,
    pub kind: ToastKind,
    /// 完整显示时长（毫秒，含淡入淡出）。
    pub duration_ms: u64,
}
