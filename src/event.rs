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
    /// 最大化窗口（已最大化时无操作）。
    ///
    /// 与 [`ToggleMaximize`](Self::ToggleMaximize) 并存而非取代它：标题栏的最大化**按钮**
    /// 是一个能翻转的开关（toggle 正好），而系统菜单里「最大化」与「还原」是**两个并列
    /// 的项**、其中一个恒为禁用——toggle 表达不了"点这一项只会最大化"。
    Maximize,
    /// 从最大化 / 最小化还原（本就是常规态时无操作）。
    Restore,
    /// 显示并前置窗口（从隐藏态唤起）。
    Show,
    /// 隐藏窗口（进程继续存活）。配合托盘或全局热键使用；
    /// 无托盘图标也无热键时隐藏窗口，用户将无法再唤起它。
    Hide,
}

/// 窗口的当前状态与能力快照。
///
/// 平台层单向推送（`AppHandler::on_window_state`），宿主缓存一份并在事件分发 / 绘制前
/// 注入线程局部，供 [`window_state()`] 与 `EventCtx::window_state()` 读取。典型用途是
/// 自绘标题栏：系统菜单据此禁用不适用的项、最大化按钮据此在"方框"与"还原"图标间切换。
///
/// **不含 `resizable`**：可缩放与可最大化在 win32 上是同一个样式位的两面
/// （`resizable(false)` 会同时剥掉 `WS_THICKFRAME` 与 `WS_MAXIMIZEBOX`），暴露两个字段
/// 只会让调用方纠结该看哪个。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowState {
    /// 当前已最大化。
    pub maximized: bool,
    /// 当前已最小化。
    pub minimized: bool,
    /// 可最大化。win32 下等价于窗口带 `WS_MAXIMIZEBOX`（`App::resizable(false)` 会剥掉它）。
    pub maximizable: bool,
    /// 可最小化。win32 下等价于窗口带 `WS_MINIMIZEBOX`。
    pub minimizable: bool,
}

impl WindowState {
    /// 一无所知时的保守快照：既没最大化也没最小化，且**什么都不能做**。
    ///
    /// 故意全 false 而不是"看着合理"的全 true。这个值只在**没有任何宿主注入过**时才会被
    /// 读到（裸单测、库被当纯渲染器用），而那时框架确实不知道窗口能干什么。全 true 的
    /// 失败模式是"菜单项可点、点了没反应"——无声且难查；全 false 的失败模式是"菜单项全灰"
    /// ——一眼看得见。默认值本身就是答案的字段，必须选那个错得显眼的。
    pub const UNKNOWN: Self = Self {
        maximized: false,
        minimized: false,
        maximizable: false,
        minimizable: false,
    };

    /// 建窗配置推导出的初始快照——平台推来真值**之前**就已经是对的。
    ///
    /// 不等平台推送是承重的：`resizable(false)` 的对话框式窗口若在首次推送前被问到
    /// `maximizable`，拿到 `true` 就会画出一个可点的"最大化"菜单项。这与
    /// `CoreTextEngine` 漏实现 `scale()` 拿默认 1.0 是同一类事故——默认值本身就是错的，
    /// 且不报错。
    pub(crate) fn from_config(resizable: bool) -> Self {
        Self {
            maximized: false,
            minimized: false,
            // 可最大化 == 可缩放：win32 建窗时 `resizable(false)` 一并剥掉 WS_MAXIMIZEBOX。
            maximizable: resizable,
            // 最小化不受可缩放影响：不可缩放的对话框照样能最小化。
            minimizable: true,
        }
    }
}

thread_local! {
    /// 当前窗口状态快照。宿主每次事件分发 / 绘制前注入（多窗口下各注各的，
    /// 与主题快照、帧时钟同一套路数）。
    static WINDOW_STATE: std::cell::Cell<WindowState> =
        const { std::cell::Cell::new(WindowState::UNKNOWN) };
}

/// 当前窗口的状态与能力快照。
///
/// **仅在事件回调 / 菜单构建 / `paint` 期间有效**：宿主在进入这些阶段前注入，
/// 与 [`theme::current()`](crate::theme::current) 同一机制。在这些阶段之外读到的是上一次
/// 注入的残值（或 [`WindowState::UNKNOWN`]）。
///
/// 有 `EventCtx` 时优先用 `EventCtx::window_state()`——同一个值，但读的路径显式。
/// 本自由函数是为**拿不到 ctx** 的地方准备的：`Element::on_context_menu` 的构建器签名是
/// `Fn() -> Vec<MenuItem>`，不收 ctx。
pub fn window_state() -> WindowState {
    WINDOW_STATE.with(|s| s.get())
}

/// 注入当前窗口状态（宿主专用）。
pub(crate) fn set_window_state(st: WindowState) {
    WINDOW_STATE.with(|s| s.set(st));
}

/// 标准窗口系统菜单四项：还原 / 最小化 / 最大化 /（分隔）/ 关闭。
///
/// 禁用态按 [`window_state()`] 当场决定，故**必须在菜单弹出的那一刻调用**（`on_context_menu`
/// 的构建器里正合适），不能在构建界面时预先算好一份存起来——那份会停在窗口刚建出来的状态上。
///
/// 无边框窗口默认已经接管了标题栏右键（见 `App::system_menu`），本函数是给"想要系统菜单
/// **再加几项自己的**"准备的：
///
/// ```no_run
/// # use windui::prelude::*;
/// Element::row().window_drag().on_context_menu(|| {
///     let mut items = windui::event::system_menu_items();
///     items.push(MenuItem::separator());
///     items.push(MenuItem::run("关于", |ctx| ctx.toast("v0.1"), true));
///     items
/// });
/// ```
///
/// **项数恒为五（含分隔线），只改可用性**，与 Windows 系统菜单一致：项数固定，用户的
/// 肌肉记忆（"第三行是最大化"）才成立；按条件增删会让同一个位置每次点到不同的东西。
pub fn system_menu_items() -> Vec<MenuItem> {
    let st = window_state();
    // 关闭的快捷键是**平台惯例**、不是框架注册的绑定：win32 上 Alt+F4 由 DefWindowProc
    // 处理。别的平台上不写——标一个按了没反应的快捷键比不标更糟。
    // 一律走 `.enabled(..)` builder，**不用** `MenuItem::run` 的第三个参数——那个是
    // `checked`（勾选标记），而紧邻的 `MenuItem::key` 第三个参数却是 `enabled`。
    // 两个构造器同形状、同为 bool、含义不同，写错的症状是"该灰的项没灰，还多了个勾"。
    let item = |label: &str, enabled: bool, f: fn(&mut crate::core::EventCtx)| {
        MenuItem::run(label, f, false).enabled(enabled)
    };
    let close = item("关闭", true, |ctx| ctx.request_close());
    let close = if cfg!(target_os = "windows") {
        close.shortcut("Alt+F4")
    } else {
        close
    };
    vec![
        // 「还原」只在最大化时可用。最小化态在这里不可达（窗口最小化时标题栏点不到），
        // 但 `ctx.restore()` 两种都能还原，故无需分支。
        item("还原", st.maximized, |ctx| ctx.restore()),
        item("最小化", st.minimizable, |ctx| ctx.minimize()),
        item("最大化", st.maximizable && !st.maximized, |ctx| {
            ctx.maximize()
        }),
        MenuItem::separator(),
        close,
    ]
}

/// 全局热键的修饰键组合。
///
/// `meta` 在 Windows 上是 Win 键、macOS 上是 Command 键——同一概念的平台命名差异
/// 收口于此，调用方不必分平台。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

/// 全局热键：由系统注册，**应用无焦点、窗口隐藏时亦可触发**。
///
/// ```no_run
/// # use windui::prelude::*;
/// // Ctrl+Alt+D
/// let hk = Hotkey::new(Key::Char('D')).ctrl().alt();
/// ```
///
/// 注册可能失败——热键是**全局独占**资源，组合已被其他程序占用时系统会拒绝。
/// 见 `App::hotkey` 对失败处理的说明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub mods: Mods,
    pub key: Key,
}

impl Hotkey {
    /// 无修饰键的热键。单独的字母键作全局热键会抢走全系统的该按键，
    /// 实践中应至少加一个修饰键。
    pub fn new(key: Key) -> Self {
        Self {
            mods: Mods::default(),
            key,
        }
    }
    pub fn ctrl(mut self) -> Self {
        self.mods.ctrl = true;
        self
    }
    pub fn alt(mut self) -> Self {
        self.mods.alt = true;
        self
    }
    pub fn shift(mut self) -> Self {
        self.mods.shift = true;
        self
    }
    /// Windows 键 / macOS Command 键。
    pub fn meta(mut self) -> Self {
        self.mods.meta = true;
        self
    }
}

/// 运行期热键操作意图（[`crate::app::HotkeyHandle`] 排队、平台层消费执行）。
/// 与 `WindowOp` 同属"核心声明意图、平台落地"的管线——核心层不碰平台句柄。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyOp {
    /// 改绑到新组合（旧组合注销；新组合注册失败时回滚保留旧绑定）。
    Rebind(Hotkey),
    /// 启用/停用（停用即向系统注销，把组合归还给其他程序；再启用重新注册）。
    SetEnabled(bool),
}

/// 全局热键回调的上下文。
///
/// **刻意只能声明意图，拿不到窗口句柄。** 回调在平台层持有窗口状态借用期间执行，
/// 此时若直接调用 `ShowWindow` 等会同步重入消息处理的 API，将造成 `&mut` 别名
/// （见 `AGENTS.md` 铁律 6「OS 重入前释放借用」）。把窗口操作降级为「意图」、由平台层
/// 在借用释放后统一执行，使该约束成为**类型上的保证**而非人的记性。
#[derive(Debug, Clone, Copy, Default)]
pub struct HotkeyCtx {
    pub(crate) op: Option<WindowOp>,
}

impl HotkeyCtx {
    /// 请求显示并前置窗口。
    pub fn show_window(&mut self) {
        self.op = Some(WindowOp::Show);
    }
    /// 请求隐藏窗口。
    pub fn hide_window(&mut self) {
        self.op = Some(WindowOp::Hide);
    }
    /// 取出回调声明的意图（供平台层在**释放窗口状态借用之后**执行）。
    ///
    /// 两平台的热键派发路径各调一次（win32 的 `WM_HOTKEY`、macOS 的 Carbon 处理器）。
    pub(crate) fn take_op(&mut self) -> Option<WindowOp> {
        self.op.take()
    }
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
    /// 上翻页。与 `Home`/`End` 同属"文档级导航"，控件未处理时可经
    /// [`Element::on_nav_key`](crate::ui::Element::on_nav_key) 交给应用（翻候选页等）。
    PageUp,
    /// 下翻页。见 [`Key::PageUp`]。
    PageDown,
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
///
/// `Run` 的闭包与控件回调同形，收 `&mut EventCtx` 作第一参数——菜单项能做的事
/// 因此与 `on_click` 齐平（`ctx.defer_blocking` 弹原生对话框、`ctx.toast`、
/// `ctx.request_close`）。宿主在浮层里经 `Tree::run_detached` 借出这个 ctx。
///
/// 闭包是 `Fn` 而非 `FnMut`：菜单项会被克隆进浮层的每一级面板（`MenuItem: Clone`，
/// 动作存 `Rc`），粘滞项还要在原地重建后再执行同一份动作，独占可变借用无处安放。
/// 需要在动作里改状态时用 `Signal`（`Copy` 且内部可变，正是为此）。
#[derive(Clone)]
pub enum MenuAction {
    SendKey(KeyEvent),
    Run(MenuActionFn),
}

/// 菜单项动作闭包：与控件回调同形（`ctx` 在首位），`Rc` 是因为项会被克隆进浮层的
/// 每一级面板（详见 [`MenuAction`]）。
pub type MenuActionFn = std::rc::Rc<dyn Fn(&mut crate::core::EventCtx)>;

/// 一个浮层菜单/下拉项。支持图标、尾随快捷键、分隔线与级联子菜单。
///
/// `#[non_exhaustive]`：字段全 `pub`，本版已因加字段破坏过两次（`intent` 让字面量构造
/// 报 `E0063`、`on_trailing_click` 换类型）。菜单项的可选修饰只会越来越多，故封住
/// 字面量构造这条路——下游一律走 [`MenuItem::run`] / [`key`](MenuItem::key) /
/// [`separator`](MenuItem::separator) / [`submenu`](MenuItem::submenu) 四个便捷构造
/// 加链式设置器（它们收敛到同一个底座，日后加字段不波及调用方）。
/// 字段读取不受影响，仍可 `item.label` / `item.checked`。
///
/// 下游的字面量构造会报 `E0639`：
///
/// ```compile_fail,E0639
/// # use windui::prelude::*;
/// let _ = MenuItem {
///     label: String::from("复制"),
///     ..todo!()
/// };
/// ```
///
/// 改用便捷构造 + 链式设置器：
///
/// ```
/// # use windui::prelude::*;
/// let _ = MenuItem::run("复制", |_ctx| {}, false).shortcut("Ctrl+C");
/// ```
#[derive(Clone)]
#[non_exhaustive]
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
    /// 第二行小字说明（Some → 该项渲染为两行，行高变高）。
    pub subtitle: Option<String>,
    /// 尾随徽章胶囊：纯展示，(文本, 意图色)。
    pub badge: Option<(String, crate::theme::Intent)>,
    /// 尾随可独立点击的图标（字符/emoji，None=无图标）。
    pub trailing_icon: Option<String>,
    /// 点击尾随图标的回调，与主项 `action` 完全独立。`None` 则图标是**纯展示**的：
    /// 图标区不再单独抢命中，点它等同于点本项（照常执行 `action` 并关闭菜单）——
    /// 见 [`MenuItem::trailing_icon_display`]。注意"纯展示"说的是图标没有自己的动作，
    /// 不是那一小块区域变得点不动。
    /// 签名与 [`MenuAction::Run`] 一致（ctx 在前、`Fn` 的理由同上）。
    pub on_trailing_click: Option<MenuActionFn>,
    /// 粘滞项：点击执行 `action` 后菜单保持展开（复选菜单的开关项）。
    /// 默认 `false`——单选下拉/右键菜单里"点中即完成决定"，理应关闭。
    /// 粘滞项每次点击只翻转一个状态，决定要到点面板外才算完成，故不关。
    /// 需配合 [`MenuRequest::rebuild`] 才能在原地刷新勾选态。
    ///
    /// 仅对 [`MenuAction::Run`] 有效：`SendKey` 是"把按键交给控件、菜单退场"的语义，
    /// 与粘滞矛盾——粘滞的 `SendKey` 项点击后按键不派发，只保持展开。
    pub stay_open: bool,
    /// 语义色（`None` = 常规文字色）。`Danger` 把标签染成 `palette.danger`，
    /// 用于「删除 / 清空」这类不可逆项——菜单里所有项长得一样时，破坏性操作与「复制」
    /// 只差一行的距离，颜色是唯一能在扫读时拦住手的信号。
    ///
    /// 与 `enabled` 的优先级：禁用胜出（变灰）——不可点的项不该还在喊"危险"。
    /// 与悬停/勾选的优先级：intent 胜出——危险项被指向时更该保持红，而不是变成中性的强调色。
    pub intent: Option<crate::theme::Intent>,
}

/// 空动作（分隔线/子菜单父项占位，永不执行）。
fn noop_action() -> MenuAction {
    MenuAction::Run(std::rc::Rc::new(|_| {}))
}

impl MenuItem {
    /// 各便捷构造的共同底座：动作以外全取默认。
    ///
    /// 四个构造各写一遍全字段的话，加字段要改四处；四份一模一样的 `None` 也没人愿意读。
    fn base(action: MenuAction) -> Self {
        Self {
            label: String::new(),
            action,
            enabled: true,
            checked: false,
            icon: None,
            shortcut: None,
            separator: false,
            submenu: Vec::new(),
            subtitle: None,
            badge: None,
            trailing_icon: None,
            on_trailing_click: None,
            stay_open: false,
            intent: None,
        }
    }
    /// 便捷构造：标签 + 合成按键。
    pub fn key(label: impl Into<String>, key: KeyEvent, enabled: bool) -> Self {
        Self {
            label: label.into(),
            enabled,
            ..Self::base(MenuAction::SendKey(key))
        }
    }
    /// 便捷构造：标签 + 闭包动作。动作收 `&mut EventCtx`（见 [`MenuAction::Run`]），
    /// 与 `on_click` 同形——菜单项里要弹原生对话框写 `ctx.defer_blocking(..)` 即可。
    pub fn run(
        label: impl Into<String>,
        f: impl Fn(&mut crate::core::EventCtx) + 'static,
        checked: bool,
    ) -> Self {
        Self {
            label: label.into(),
            checked,
            ..Self::base(MenuAction::Run(std::rc::Rc::new(f)))
        }
    }
    /// 分隔线项。
    pub fn separator() -> Self {
        Self {
            separator: true,
            enabled: false,
            ..Self::base(noop_action())
        }
    }
    /// 级联子菜单父项：悬停展开 `items`。
    pub fn submenu(label: impl Into<String>, items: Vec<MenuItem>) -> Self {
        Self {
            label: label.into(),
            submenu: items,
            ..Self::base(noop_action())
        }
    }
    /// 设置语义色（字段见 [`MenuItem::intent`] 的文档）。
    pub fn intent(mut self, intent: crate::theme::Intent) -> Self {
        self.intent = Some(intent);
        self
    }
    /// 标为危险项：标签用 `palette.danger`（删除 / 清空这类不可逆操作）。
    pub fn danger(self) -> Self {
        self.intent(crate::theme::Intent::Danger)
    }
    /// 设置前置图标（字符/emoji）。
    pub fn icon(mut self, icon: impl Into<String>) -> Self {
        self.icon = Some(icon.into());
        self
    }
    /// 设置尾随快捷键文本。
    pub fn shortcut(mut self, s: impl Into<String>) -> Self {
        self.shortcut = Some(s.into());
        self
    }
    /// 设置选中勾。
    pub fn check(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
    /// 设置第二行小字说明（该项渲染为两行，行高变高）。
    pub fn subtitle(mut self, s: impl Into<String>) -> Self {
        self.subtitle = Some(s.into());
        self
    }
    /// 设置尾随徽章胶囊（纯展示，不参与命中）。
    pub fn badge(mut self, text: impl Into<String>, intent: crate::theme::Intent) -> Self {
        self.badge = Some((text.into(), intent));
        self
    }
    /// 设置尾随可独立点击的图标：点击只触发 `on_click`，不触发本项的 `action`。
    /// 回调签名同 [`MenuItem::run`] 的动作。
    pub fn trailing_icon(
        mut self,
        icon: impl Into<String>,
        on_click: impl Fn(&mut crate::core::EventCtx) + 'static,
    ) -> Self {
        self.trailing_icon = Some(icon.into());
        self.on_trailing_click = Some(std::rc::Rc::new(on_click));
        self
    }
    /// 设置尾随图标但**不接回调**：纯展示（状态点、锁形标记等），点击该图标等同于点本项。
    /// 图标要能独立点击用 [`MenuItem::trailing_icon`]。
    ///
    /// 单独成一个设置器，是因为 `trailing_icon` 必须同时收回调，于是
    /// 「有图标、无回调」这个字段组合此前只有字面量构造才写得出来——而字面量构造已被
    /// `#[non_exhaustive]` 封住。
    pub fn trailing_icon_display(mut self, icon: impl Into<String>) -> Self {
        self.trailing_icon = Some(icon.into());
        self.on_trailing_click = None;
        self
    }
    /// 标记为粘滞项：点击执行后菜单保持展开（见 [`MenuItem::stay_open`]）。
    pub fn stay_open(mut self) -> Self {
        self.stay_open = true;
        self
    }
    /// 设置启用态（禁用项变灰且不可点击）。
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// 改名为 [`MenuItem::icon`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `icon`：builder 属性设置统一去掉 `with_` 前缀，与 DropdownItem/CheckMenuItem 一致；`with_*` 在 Rust 生态里通常表示「带某配置构造」（如 Vec::with_capacity），而非设属性"
    )]
    pub fn with_icon(self, icon: impl Into<String>) -> Self {
        self.icon(icon)
    }
    /// 改名为 [`MenuItem::intent`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `intent`：builder 属性设置统一去掉 `with_` 前缀，与 DropdownItem/CheckMenuItem 一致"
    )]
    pub fn with_intent(self, intent: crate::theme::Intent) -> Self {
        self.intent(intent)
    }
    /// 改名为 [`MenuItem::shortcut`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `shortcut`：builder 属性设置统一去掉 `with_` 前缀，与 DropdownItem/CheckMenuItem 一致"
    )]
    pub fn with_shortcut(self, s: impl Into<String>) -> Self {
        self.shortcut(s)
    }
    /// 改名为 [`MenuItem::check`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `check`：builder 属性设置统一去掉 `with_` 前缀，与 DropdownItem/CheckMenuItem 一致"
    )]
    pub fn with_check(self, checked: bool) -> Self {
        self.check(checked)
    }
    /// 改名为 [`MenuItem::subtitle`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `subtitle`：builder 属性设置统一去掉 `with_` 前缀，与 DropdownItem/CheckMenuItem 一致"
    )]
    pub fn with_subtitle(self, s: impl Into<String>) -> Self {
        self.subtitle(s)
    }
    /// 改名为 [`MenuItem::badge`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `badge`：builder 属性设置统一去掉 `with_` 前缀，与 DropdownItem/CheckMenuItem 一致"
    )]
    pub fn with_badge(self, text: impl Into<String>, intent: crate::theme::Intent) -> Self {
        self.badge(text, intent)
    }
    /// 改名为 [`MenuItem::trailing_icon`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `trailing_icon`：builder 属性设置统一去掉 `with_` 前缀，与 DropdownItem/CheckMenuItem 一致"
    )]
    pub fn with_trailing_icon(
        self,
        icon: impl Into<String>,
        on_click: impl Fn(&mut crate::core::EventCtx) + 'static,
    ) -> Self {
        self.trailing_icon(icon, on_click)
    }
    /// 改名为 [`MenuItem::enabled`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `enabled`：builder 属性设置统一去掉 `with_` 前缀，与 DropdownItem/CheckMenuItem 一致"
    )]
    pub fn with_enabled(self, enabled: bool) -> Self {
        self.enabled(enabled)
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
    /// 项重建器：粘滞项（[`MenuItem::stay_open`]）点击后调用它重新生成整棵项树，
    /// 使勾选态/标签在菜单不关闭的前提下原地刷新。`None` 则粘滞项点击后菜单内容不变。
    ///
    /// 面板宽度与位置**不随重建变化**——项文本变化会让面板忽宽忽窄，而指针正停在
    /// 上面准备点下一项。宽度以首次弹出的测量结果为准。
    pub rebuild: Option<std::rc::Rc<dyn Fn() -> Vec<MenuItem>>>,
}

/// 子窗口内容的不透明载体。
///
/// 核心层不认识控件树类型（分层上 `ui` 在 `core` 之上，见 `docs/DESIGN.md` §4），而
/// 打开子窗这件事的**意图**产生在控件回调里。于是内容在这里只作为不透明值传递，由
/// 应用层在取走时还原成控件树——与 [`HotkeyCtx`] 只给意图不给窗口句柄是同一个手法。
///
/// 只有 `Window::content` 一个构造入口，故应用层的还原必然成功。
///
/// 装的是**构建器**而非建好的树：控件树构建期用户创建的 `Signal` 要归到新窗口名下
/// （窗口关闭时随之回收），而那只能在构建**发生时**收集。收一棵建好的树就太晚了——
/// 那些信号在调用方写下 `Element::col()…` 的那一刻就已经进了全局 arena。
pub struct WindowContent(Box<dyn FnOnce() -> Box<dyn std::any::Any>>);

impl WindowContent {
    /// 装入内容构建器。**仅供应用层构造器调用**。
    pub(crate) fn new<T: 'static>(build: impl FnOnce() -> T + 'static) -> Self {
        Self(Box::new(move || Box::new(build())))
    }

    /// 求值构建器并取回内容。类型必须与装入时一致，否则 panic。
    ///
    /// 调用方负责在合适的信号作用域内调用本方法（见 `UiHost::take_new_windows`）。
    pub(crate) fn take<T: 'static>(self) -> T {
        *(self.0)()
            .downcast::<T>()
            .expect("WindowContent 只能由 Window::content 构造，类型必然匹配")
    }
}

/// 控件经 [`EventCtx::open_window`](crate::core::EventCtx::open_window) 发起的开窗请求。
///
/// **不在回调里直接建窗**，与 [`WindowOp`] / `DialogRequest` 同一个理由：回调运行在平台
/// 层持有窗口状态借用期间，此时创建窗口会同步派发 `WM_NCCREATE`/`WM_SIZE` 等消息重入
/// 窗口过程，那里再取一次状态就是 `&mut` 别名（AGENTS.md 铁律 6）。请求经宿主排队，
/// 平台在事件分发**完全返回**后才真正建窗。
pub struct WindowRequest {
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub resizable: bool,
    pub centered: bool,
    pub frameless: bool,
    /// 自绘标题栏的拖动区右键是否弹出窗口系统菜单（默认 true）。
    pub system_menu: bool,
    /// 最小客户区尺寸（逻辑 dp，0=不限制）。
    pub min_width: i32,
    pub min_height: i32,
    /// 窗口背景色。`None` 则随主题 `palette.bg`（同 `App` 未显式 `bg` 时的行为）。
    pub bg: Option<crate::geometry::Color>,
    /// 内容控件树（不透明，见 [`WindowContent`]）。
    pub content: WindowContent,
    /// 关闭请求拦截器（`Window::on_close_request`）。返回 true 放行、false 取消。
    ///
    /// 必须是**每个窗口自己的**：平台在 `WM_CLOSE` / `windowShouldClose:` 里同步等这个
    /// `bool`，问的是"这个窗口能不能关"。跨窗共享的 `Signal` 表达不了它。
    pub close_handler: Option<WindowCloseHandler>,
    /// 本窗口的周期回调（`Window::on_interval`）。随窗口关闭一并停止。
    pub intervals: Vec<(std::time::Duration, WindowIntervalFn)>,
    /// 单例键（`Window::single`）。`None` = 每次请求都开一个新窗口。
    ///
    /// 有键时平台先查窗口登记表：已有同键窗口就**丢弃本次请求**并把那个窗口激活到前台。
    /// 判定放在平台层而非应用层，是因为"把已有窗口拉到前台"只有平台做得到。
    pub single: Option<String>,
    /// 窗口图标（`Window::icon`）。`None` 则跟随系统默认——子窗**不会**自动继承主窗
    /// 那次 `App::icon`：那是设到主窗 HWND 上的，不是设到窗口类上的。
    pub icon: Option<crate::icon::IconSource>,
}

/// 窗口关闭拦截器：返回 `true` 放行、`false` 取消。
///
/// 与 `App::on_close_request` 收的是同一种闭包——那个作用在主窗，这个作用在
/// [`WindowRequest`] 对应的子窗上。
pub type WindowCloseHandler = Box<dyn FnMut(&mut crate::core::EventCtx) -> bool>;

/// 窗口周期回调，与 `App::on_interval` 同形。
pub type WindowIntervalFn = Box<dyn FnMut(&mut crate::core::EventCtx)>;

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

    /// 该语义的默认显示时长（毫秒）。错误更持久，便于阅读/复制。
    pub fn default_duration_ms(self) -> u64 {
        match self {
            ToastKind::Error => 5000,
            ToastKind::Info | ToastKind::Success => 3000,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn kind_default_durations() {
        assert_eq!(ToastKind::Error.default_duration_ms(), 5000);
        assert_eq!(ToastKind::Success.default_duration_ms(), 3000);
        assert_eq!(ToastKind::Info.default_duration_ms(), 3000);
    }
}
