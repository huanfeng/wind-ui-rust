//! 输入事件类型。平台层产生物理像素坐标，但 `UiHost::on_pointer` 在分发前
//! 已 ÷scale 转为**逻辑坐标**——控件 `on_event` 收到的 pos 是逻辑坐标。

use crate::geometry::{Point, Rect};

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
    /// 窗口当前可见（未被隐藏）。
    ///
    /// 常驻托盘类应用要用它：全局热键的通常语义是**切换**——窗口露着就收起、藏着就唤起，
    /// 而应用侧自己跟不住这个状态。`App::on_show` 只在唤起时回调，隐藏则可能发生在
    /// 框架内部（ESC 关窗、`hide_on_close` 的关闭按钮），应用完全收不到通知，自建的
    /// 布尔标志迟早与真实状态对不上。
    ///
    /// 与 [`minimized`](Self::minimized) 是两件事：最小化的窗口仍然「可见」（在任务栏上、
    /// 能被还原），隐藏的窗口则从任务栏与 Alt-Tab 里整个消失。
    pub visible: bool,
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
        // 与其余字段的「错得显眼」原则不同，这一项取 true。
        //
        // 它不是能力位而是**当前事实**，没有对应的菜单项会因此变灰；而取 false 的
        // 失败模式很具体：热键的切换逻辑会以为窗口藏着，于是去「显示」一个本就显示着
        // 的窗口——按下去毫无反应。取 true 时最坏是多隐藏一次，用户再按一下就回来了。
        visible: true,
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
            // 建窗配置推不出可见性：`App::start_hidden()` 的窗口建出来就是藏着的，而这个
            // 构造函数拿不到那个标志。平台会在首次 `push_window_state` 推来真值——在那
            // 之前只有 `root.build` 期间的读取会看到这里的值，那段里没人问可见性。
            visible: true,
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

/// 带指定单例键（[`Window::single`](crate::app::Window::single)）的窗口当前是否开着。
///
/// **常驻模式（[`App::run_resident`](crate::app::App::run_resident)）下热键要做「按一下
/// 出来、再按一下收回」就得靠它**：那个模式没有主窗，[`window_state()`] 恒为
/// [`WindowState::UNKNOWN`]（其 `visible` 为 `true`），照搬按可见性分支的写法会恒走隐藏
/// 一侧而毫无反应。窗口在那里是**开着或不存在**两态，可见性根本不是那个问题的答案。
///
/// 与 `window_state()` 的另一处分别：本函数读的是平台的**窗口登记表**，不是线程局部
/// 快照，故在任何时机都成立——热键回调抵达时进程可能一帧都没渲染过。
///
/// ```no_run
/// # use windui::prelude::*;
/// # let make = || Window::new("查词", 480, 320).single("main").content(|| Element::col().fill());
/// App::resident("查词")
///     .hotkey(Hotkey::new(Key::Char('D')).ctrl().alt(), move |ctx| {
///         if window_open("main") {
///             ctx.close_window("main");
///         } else {
///             ctx.open_window(make());
///         }
///     })
///     .run_resident();
/// ```
///
/// # 平台
///
/// **仅 Windows。** macOS 的窗口登记表尚未接上常驻模式，那里恒返回 `false`
/// ——与 `run_resident` 在该平台的现状一致（它在那里直接返回，不建任何窗口）。
pub fn window_open(key: &str) -> bool {
    crate::platform::single_window_open(key)
}

/// 系统是否偏好**暗色**外观。
///
/// Windows 上读的是「设置 → 个性化 → 颜色 → 选择应用模式」那一项。读不到时按**亮色**
/// 处理：那是 Windows 的出厂默认，且亮色界面在暗色系统上只是不协调，反过来（暗色界面
/// 配亮色系统）更容易让人以为程序坏了。
///
/// 只回答「此刻是什么」。要在用户改动系统设置时**当场跟随**，用
/// [`App::on_system_theme_changed`](crate::app::App::on_system_theme_changed)——
/// 常驻类应用尤其需要它：一次会话可能横跨日出日落，而进程一直不重启。
pub fn system_prefers_dark() -> bool {
    crate::platform::system_prefers_dark()
}

/// 窗口级快捷键回调的受控句柄：收集意图，由宿主在回调返回后落地。
///
/// 与 [`HotkeyCtx`] 同构（那个管全局热键，这个管窗口内的快捷键），也同样是**纯意图
/// 收集**而不是立即执行——回调跑在按键分发的中途，此时宿主正借着焦点状态与节点树，
/// 在这里直接改它们会撞上借用，也会让副作用的发生顺序变得难以推理。
#[derive(Default)]
pub struct ShortcutCtx {
    pub(crate) focus_main: bool,
    pub(crate) close: bool,
}

impl ShortcutCtx {
    /// 把键盘焦点交还给声明了 [`Element::autofocus`](crate::ui::Element::autofocus)
    /// 的那个控件，并按其模式处理（`autofocus_select_all` 会一并全选）。
    ///
    /// 这正是「Ctrl+L 回到搜索框」这类快捷键要的语义——与热键唤起窗口时发生的是同
    /// 一件事，故复用同一条路径，而不是让应用去记那个控件的 id（它也拿不到）。
    pub fn focus_main_input(&mut self) {
        self.focus_main = true;
    }

    /// 请求关闭窗口，走完整的关闭决策链（关顶层对话框 → 问 `on_close_request` →
    /// `hide_on_close`）。常驻托盘类应用据此把 Ctrl+W 落成「收起窗口」。
    pub fn request_close(&mut self) {
        self.close = true;
    }
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

thread_local! {
    /// 托盘 / 全局热键回调排队的开窗请求。平台层在**回调返回、借用释放之后**取走并建窗。
    ///
    /// **为什么不装在 `HotkeyCtx` / `TrayCtx` 里**：`WindowRequest` 带闭包，既不是
    /// `Clone` 也不是 `PartialEq`/`Debug`，而 `HotkeyCtx` 是 `Copy`、`Vec<TrayAction>`
    /// 的相等比较更是下游写测试的正式入口（见 `testing::run_with_tray_ctx` 的文档示例）。
    /// 把请求塞进那两个类型会连带砸掉这些约定，于是请求走这条旁路、意图那边只留一个
    /// 位置标记（[`crate::platform::TrayAction::OpenWindow`]）。
    ///
    /// 线程局部而非穿构造器，与托盘运行期队列同理：开窗是**应用级**的动作，常驻模式下
    /// 更可能一个窗口都没有，挂不到任何窗口的宿主上。
    static PENDING_CALLBACK_WINDOWS: std::cell::RefCell<Vec<WindowRequest>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 排入一个来自托盘 / 热键回调的开窗请求。
pub(crate) fn push_callback_window(req: WindowRequest) {
    PENDING_CALLBACK_WINDOWS.with(|q| q.borrow_mut().push(req));
}

/// 取走**最早**排队的那个开窗请求（按调用顺序消费）。
pub(crate) fn take_callback_window() -> Option<WindowRequest> {
    PENDING_CALLBACK_WINDOWS.with(|q| {
        let mut q = q.borrow_mut();
        if q.is_empty() {
            None
        } else {
            Some(q.remove(0))
        }
    })
}

/// 取走全部排队的开窗请求（热键路径：没有意图队列给它们定位置，一次全取）。
pub(crate) fn take_callback_windows() -> Vec<WindowRequest> {
    PENDING_CALLBACK_WINDOWS.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

thread_local! {
    /// 全局热键回调排队的**关窗**请求（单例键），与开窗队列并列。
    ///
    /// 为什么热键这条走队列而托盘那条把键带在 [`TrayAction::CloseWindow`]
    /// （[`crate::platform::TrayAction`]）里：[`HotkeyCtx`] 是 `Copy`，一个 `String` 放不
    /// 进去；`TrayAction` 则本就是带载荷的枚举，`String` 不碍它的 `Debug`/`Clone`/`PartialEq`
    /// ——那正是 `OpenWindow` 当初只能留位置标记的理由（`WindowRequest` 带闭包），关窗
    /// 没有这个问题，故把键直接写在意图里，读起来与测起来都更实在。
    static PENDING_CALLBACK_CLOSES: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// 排入一个来自全局热键回调的关窗请求（按单例键）。
pub(crate) fn push_callback_close(key: String) {
    PENDING_CALLBACK_CLOSES.with(|q| q.borrow_mut().push(key));
}

/// 取走全部排队的关窗请求。平台层在开窗之前落地它们。
///
/// **同一个回调里对同一个键既关又开是不支持的**，别照着「先关后开」的字面去写「换一个
/// 尺寸重开」：关窗是**投递** `WM_CLOSE`（下一轮消息循环才处理），而开窗是当场执行的，
/// 那一刻旧窗口仍在窗口登记表里，于是 `Window::single` 的去重判定命中、本次开窗退化成
/// 「激活那个正在关闭的窗口」，随后它被关掉——净结果是**一个窗口都没有**。
///
/// 要重开请分两次回调（关掉之后，下一次热键 / 托盘点击再开），或用不同的单例键。
/// win32 的 `apply_app_effects` 在两条队列撞上同一个键时会打一行提示——这个错法的表现
/// （"按了一下，窗口没了"）与起因隔得太远，不说一声查不出来。
pub(crate) fn take_callback_closes() -> Vec<String> {
    PENDING_CALLBACK_CLOSES.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// 偷看当前排队的开窗请求各自的单例键（**不取走**，无键者留 `None`）。
///
/// 只为上面那条诊断而存在：平台层要在关窗之前知道「这一批开窗请求里有没有同一个键」，
/// 而真正取走队列的是 `open_callback_windows` 那条路，两者不能抢。
///
/// **返回拥有的值而不是借用**是承重的：调用点紧接着就要走到 `take_callback_windows`
/// 的 `borrow_mut`，借用若跨过那一步就是当场 `BorrowMutError`。把 clone 收在函数体里，
/// 这个约束就不依赖调用者的自觉。
// 只有 win32 的 `close_single_window` 用它做诊断；macOS 那条路尚未实现（见
// `platform/macos/tray.rs` 的 TODO），在那里它确实没有调用者。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn pending_window_singles() -> Vec<Option<String>> {
    PENDING_CALLBACK_WINDOWS.with(|q| q.borrow().iter().map(|r| r.single.clone()).collect())
}

/// 这个待关闭的键，是否与某个**待建窗口**的单例键撞上了（即那条不支持的写法）。
///
/// 抽成不碰任何全局状态的纯函数，是为了能把「不该喊的时候别喊」测出来：异键组合是唯一
/// 真正受支持的组合，而一个见谁都喊的提示三天后就会被当噪音忽略，那时它等于不存在。
// 同上：调用点在 win32。单测两个平台都跑得到，故这里不是"没人用"，只是"那个平台没人用"。
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn key_collides(key: &str, pending: &[Option<String>]) -> bool {
    pending.iter().any(|p| p.as_deref() == Some(key))
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
    /// 请求**新建**一个窗口，语义同
    /// [`EventCtx::open_window`](crate::core::EventCtx::open_window)。
    ///
    /// 零窗口常驻模式（[`App::run_resident`](crate::app::App::run_resident)）下这是热键
    /// 唯一能唤出界面的途径——那时没有窗口可 `show_window`，进程只有托盘与热键。
    ///
    /// 请求不占 [`show_window`](Self::show_window) 那个位置（两者不是同一件事，可并用），
    /// 由平台层在回调返回后建窗。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// App::resident("查词")
    ///     .hotkey(Hotkey::new(Key::Char('D')).ctrl().alt(), |ctx| {
    ///         ctx.open_window(Window::new("查词", 480, 320).content(|| Element::col().fill()));
    ///     })
    ///     .run_resident();
    /// ```
    pub fn open_window(&mut self, req: WindowRequest) {
        push_callback_window(req);
    }
    /// 请求关闭带指定单例键（[`Window::single`](crate::app::Window::single)）的窗口。
    /// 没有这样一个窗口时什么也不做。
    ///
    /// 这是 [`open_window`](Self::open_window) 的对侧，**常驻模式下热键的「再按一下
    /// 收回去」只能这么写**：那时没有主窗，[`hide_window`](Self::hide_window) 无处可施
    /// （显隐意图的作用对象是主窗），而窗口本就关掉即销毁——收回去与关掉是同一件事，
    /// 内存也随之归还。配合 [`window_open`] 判分支。
    ///
    /// 走的是**完整的关闭决策链**（`Window::on_close_request` 会被问到），与用户按标题栏
    /// 的关闭按钮同义：热键收起窗口不该绕过应用自己的「有未保存内容」拦截。
    ///
    /// 与其他意图一样只排队不执行：真正的关闭发生在回调返回、平台层释放借用之后。关窗
    /// 统一排在开窗之前落地，但**同一个回调里对同一个键既关又开是不支持的**——关窗是
    /// 投递、开窗是即时，那样写的净结果是一个窗口都没有（平台层会打一行提示）。要重开
    /// 请分两次回调，或用不同的键。
    ///
    /// 另一条同源的边界：窗口「正在关闭」期间它仍在窗口登记表里，故 `on_close_request`
    /// 执行期间 [`window_open`] 对这个键**仍返回 `true`**。平时无碍——`WM_CLOSE` 与
    /// `WM_HOTKEY` 同队列 FIFO，下一次热键必然排在那条关闭之后；但 `on_close_request`
    /// 里若弹了原生模态对话框（自带嵌套消息泵），这个先后就不再成立，那时热键读到的
    /// 是「还开着」。
    pub fn close_window(&mut self, key: impl Into<String>) {
        push_callback_close(key.into());
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
    /// 左右调整（↔）。分栏分隔条、可拖宽的列边界。
    ///
    /// 没有它时只能退而用 [`Hand`](Self::Hand)——手型说的是「这里能点」，而分隔条
    /// 要说的是「这里能左右拖」，两者指向不同的操作，用户据此预期的动作也不同。
    SizeWE,
    /// 上下调整（↕）。横向分栏的分隔条、可拖高的行边界。与 [`SizeWE`](Self::SizeWE)
    /// 对称：分栏容器两个方向都有，光标形状也得两个方向都有。
    SizeNS,
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
    /// 连续点击计数（由平台层填充）：1=单击，2=双击。仅 `Down` 有意义，其余动作恒为 1。
    ///
    /// **到 2 即重新起算**，与 Win32（`WM_LBUTTONDBLCLK` 发完就重算）和 Qt 一致：
    /// 一串快点得到 1,2,1,2,… 而不是 1,2,3,3,…。不这样的话"双击进目录、紧接着再双击
    /// 往下钻"里的第三、四下会被读成 3 和 1，一次都匹配不上双击，连续钻目录就断了。
    ///
    /// 代价是平台层不再报三击。需要三击选段的文本控件用 [`TripleClick`] 自己认。
    pub click_count: u8,
    /// 事件发生时按着的修饰键。列表控件靠它做 Ctrl+点击切换选中、Shift+点击范围选中
    /// ——桌面软件最基本的选择手势，此前平台层收得到却没送上来。
    pub mods: Mods,
}

impl PointerEvent {
    /// 构造一个单击事件（click_count=1）。便于测试与合成事件。
    pub fn single(kind: PointerKind, pos: Point, button: MouseButton) -> Self {
        Self {
            kind,
            pos,
            button,
            click_count: 1,
            mods: Mods::default(),
        }
    }

    /// 带修饰键的单击（测试 Ctrl+点击、Shift+点击用）。
    pub fn single_with(kind: PointerKind, pos: Point, button: MouseButton, mods: Mods) -> Self {
        Self {
            mods,
            ..Self::single(kind, pos, button)
        }
    }
}

/// 三击判定器：给需要"三击选段/选行"的文本控件用。
///
/// 平台层的 [`PointerEvent::click_count`] 到 2 就重新起算，所以一次真正的三击到达时
/// 是 `1, 2, 1`——第三下伪装成新一轮的首击。这个结构把它认回来：记下最近那次双击的
/// 时刻与位置，若紧接着来的首击仍在双击时限与漂移阈值内，就判为三击。
///
/// 为什么这份策略归控件而不归平台：第三下究竟是"三击的最后一下"还是"新一轮双击的
/// 第一下"，在消息层面无法区分。文本里前者几乎总是对的，列表里后者几乎总是对的
/// （双击进目录后接着双击往下钻）。让各自的控件按自己的语境决定，比在平台层押一边强。
#[derive(Default, Clone, Copy, Debug)]
pub struct TripleClick {
    last_dbl: Option<(std::time::Instant, Point)>,
}

impl TripleClick {
    /// 喂一次左键 `Down`，返回**有效**连击数：1 / 2 / 3。
    ///
    /// 非左键、非 `Down` 一律原样放行且不动内部状态。这道关是必须的：`TextInput`
    /// 声明了 `wants_right_click()`，于是**所有**非左键的 Down 都会投递进来，右键
    /// 在上游分流了、中键却会一路落到调用点。不挡的话，左键双击选词后原地按一下
    /// 中键（滚轮误按、Linux 的中键粘贴习惯）就会被认成三击、整段被选中。
    ///
    /// 平台已经给出 >= 3 的（合成事件、或将来某个自己数三击的平台）原样放行，
    /// 不再二次判定。
    pub fn feed(&mut self, p: &PointerEvent) -> u8 {
        if p.kind != PointerKind::Down || p.button != MouseButton::Left {
            return p.click_count.max(1);
        }
        if p.click_count >= 3 {
            self.last_dbl = None;
            return p.click_count;
        }
        if p.click_count == 2 {
            self.last_dbl = Some((std::time::Instant::now(), p.pos));
            return 2;
        }
        // 首击：紧跟在一次就近的双击之后，即视为三击。
        //
        // 阈值取**系统**的双击设置而不是写死的常量：平台层判第一、二下用的就是它，
        // 两边不同口径的话，把双击速度调慢的用户会遇到"双击好使、三击不灵"。
        let (ms, drift) = crate::platform::double_click_thresholds();
        if let Some((t, at)) = self.last_dbl.take() {
            if t.elapsed().as_millis() <= u128::from(ms)
                && (p.pos.x - at.x).abs() <= drift
                && (p.pos.y - at.y).abs() <= drift
            {
                return 3;
            }
        }
        1
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
    /// Insert 键。文件管理器用它「标记并下移」，编辑器用它切换插入/覆盖。
    Insert,
    /// 功能键 F1–F12（`F(1)`..=`F(12)`）。
    ///
    /// 此前只能经 [`Key::Other`] 里的 Windows 虚拟键码表达（macOS 侧为此对齐了一张
    /// VK 表），应用要写 `Key::Other(0x74)` 才是 F5——既不可读，也把平台键码泄漏进
    /// 应用代码。具名以后两平台的映射表各自翻译，应用只认 `F(5)`。
    F(u8),
    /// 小键盘 `+`。与主键盘 `Key::Char('+')` 区分：TC 系文件管理器把小键盘的
    /// `+ - *` 专用于选择/反选，而主键盘的同名字符仍是可输入文本。
    NumpadAdd,
    /// 小键盘 `-`。见 [`Key::NumpadAdd`]。
    NumpadSubtract,
    /// 小键盘 `*`。见 [`Key::NumpadAdd`]。
    NumpadMultiply,
    /// 小键盘 `/`。见 [`Key::NumpadAdd`]。
    NumpadDivide,
    /// 键盘上的「菜单」键（Windows 的 Apps 键）：弹出当前项的上下文菜单。
    ContextMenu,
    /// Alt 键**本身**（macOS 的 Option）。只有它会带着 `pressed: false` 上来：
    /// 单击 Alt（按下、期间没碰别的键、松开）是桌面惯例里激活菜单栏的手势，判定
    /// 必须看到松开那一下。宿主在分发前截下它，控件永远收不到——控件要的是
    /// [`KeyEvent::alt`] 那个修饰标志，不是这个键。
    Alt,
    Char(char),
    Other(u32),
}

#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub key: Key,
    pub pressed: bool,
    /// Shift 是否按下（用于 Shift+Tab 反向导航、Shift+方向扩展选区）。
    pub shift: bool,
    /// Ctrl 是否按下（用于 Ctrl+A/C/V/X 等）。macOS 上 Command 也落到这里——
    /// 平台惯用的「主修饰键」统一成一个标志，应用写一次 `Ctrl+C` 两平台都对。
    pub ctrl: bool,
    /// Alt（macOS：Option）是否按下。
    ///
    /// 没有它时 `Alt+F1`、`Alt+Enter` 这类桌面软件的常规快捷键无从表达：平台层收得到
    /// 修饰键状态，却在这里被丢掉，应用侧看到的 `Alt+Enter` 与裸 `Enter` 一模一样。
    pub alt: bool,
    /// Meta（Windows：Win 键；macOS：Command）是否按下。
    ///
    /// macOS 上 Command 同时置 [`ctrl`](Self::ctrl) 与本标志：前者服务「跨平台写一次」，
    /// 后者留给确需区分 Command 与 Control 的应用（如把 Ctrl+方向留给行首行尾）。
    pub meta: bool,
}

impl KeyEvent {
    /// 按下某键、无任何修饰。测试与合成事件用；四个修饰键都要写一遍的字面量太长。
    pub const fn pressed(key: Key) -> Self {
        Self {
            key,
            pressed: true,
            shift: false,
            ctrl: false,
            alt: false,
            meta: false,
        }
    }
    /// 修饰键组合，与全局热键的 [`Mods`] 同形，便于键位表按 `(Key, Mods)` 查找。
    pub const fn mods(&self) -> Mods {
        Mods {
            ctrl: self.ctrl,
            alt: self.alt,
            shift: self.shift,
            meta: self.meta,
        }
    }
}

/// 统一事件。
#[derive(Debug, Clone, Copy)]
pub enum Event {
    Pointer(PointerEvent),
    Key(KeyEvent),
}

/// 输入法**未提交**的合成串（preedit / marked text）：拼音打到一半、还没选定候选词的那段。
///
/// 空 `text` 表示没有合成在进行（合成刚结束或从未开始）。
///
/// **为什么这个类型必须存在**：两个平台的输入法模型不同。Windows 的 IMM32 允许应用只用
/// `ImmSetCompositionWindow` 告诉系统「画在哪」，合成串由系统 IME 自己画；AppKit 的
/// `NSTextInputClient` 则**只有内联一档**——实现了协议就等于承诺自己画，系统绝不代画。
/// Linux 的 GTK/IBus 同样把绘制责任交给客户端。故除 win32 外，合成串必须由本库自绘，
/// 而自绘的前提是这段文字能从平台层送到控件层——这个类型就是那条通路上的载体。
///
/// 索引单位是**字符**（`char`），不是字节、也不是 UTF-16 码元。平台层是唯一知道
/// UTF-16 的地方（`NSRange` 以码元计），换算在那里做完，上层因此与平台编码无关。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Preedit {
    /// 合成串本体。空串 = 无合成。
    pub text: String,
    /// 合成串**内部**的光标位置（字符索引，`0..=text.chars().count()`）。
    /// 输入法边打边移动它，控件据此画合成内光标。
    pub caret: usize,
    /// 合成串内当前「选中分句」的字符范围。日文分节转换会把长串切成几段、
    /// 高亮其中一段；中文拼音一般整段或 `None`。
    pub sel: Option<(usize, usize)>,
}

impl Preedit {
    /// 是否有合成在进行。等价于旧 `set_ime_composing(true)` 的语义。
    pub fn is_active(&self) -> bool {
        !self.text.is_empty()
    }

    /// 合成串字符数。
    pub fn char_len(&self) -> usize {
        self.text.chars().count()
    }
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
    /// 助记字母（不分大小写）：菜单展开时按这个字母即激活本项（有子菜单则展开）。
    /// 标签里首个匹配的字符画下划线；标签里没有这个字母（中文标签常见）就只响应按键、
    /// 不画线——习惯写法是 `"复制(C)"` 配 `.mnemonic('C')`，括号里的字母自然被画上线。
    pub mnemonic: Option<char>,
}

/// 把标签按助记字母切成 `(前缀, 该字符, 后缀)`：首个不分大小写匹配的字符处。
/// 标签里没有这个字母（中文标签只在括号里带字母时会有）返回 `None`——只响应按键、不画线。
pub fn mnemonic_split(label: &str, m: char) -> Option<(&str, &str, &str)> {
    let lower: Vec<char> = m.to_lowercase().collect();
    let (i, c) = label
        .char_indices()
        .find(|(_, c)| c.to_lowercase().eq(lower.iter().copied()))?;
    let end = i + c.len_utf8();
    Some((&label[..i], &label[i..end], &label[end..]))
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
            mnemonic: None,
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
    /// 设置助记字母（见 [`MenuItem::mnemonic`] 字段）。
    pub fn mnemonic(mut self, c: char) -> Self {
        self.mnemonic = Some(c);
        self
    }
    /// 本项是否响应助记字母 `c`（不分大小写；分隔线、禁用项不响应）。
    pub fn matches_mnemonic(&self, c: char) -> bool {
        !self.separator
            && self.enabled
            && self
                .mnemonic
                .is_some_and(|m| m.to_lowercase().eq(c.to_lowercase()))
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
    /// 发起本菜单的菜单栏（[`EventCtx::show_menu_bar`](crate::core::EventCtx::show_menu_bar)
    /// 才填）：宿主据此在展开期间做栏级联动——指针滑到相邻标题即切换、←→ 在根级跨菜单、
    /// 点标题收起。普通右键菜单与下拉留 `None`。
    pub bar: Option<MenuBarLink>,
    /// 点在浮层外的那一下是否**穿透**给下面的控件：`true` = 收起菜单，同一下按下照常
    /// 分发（Windows 的右键菜单开着时点另一行，菜单收起且那一行被选中，不用点两次）；
    /// `false` = 那一下只负责收起。右键菜单与菜单栏默认穿透；下拉 / 复选菜单不穿透——
    /// 点别处是"放弃这次选择"，且点回下拉控件自己会立刻再展开一次。
    pub click_through: bool,
}

/// 菜单栏交给宿主的联动信息：各标题的位置与项生成器。
///
/// 菜单栏控件自己只画标题、收第一下点击；展开之后指针与键盘都归宿主浮层独占，
/// 控件再也收不到事件。"滑到相邻标题自动切换"这种原生手感只能由宿主做——它得知道
/// 其它标题在哪、点开是什么，这份信息就是为此打包的。
///
/// `open` 是控件与宿主之间的**共享单元格**：宿主切换 / 关闭时写"当前展开（或键盘
/// 激活）的标题下标"，控件绘制时读它画按下态。不用 `Signal`：它不属于任何窗口的信号
/// 作用域，控件销毁即随之回收。
#[derive(Clone)]
pub struct MenuBarLink {
    pub slots: Vec<MenuBarSlot>,
    /// 本次要展开的标题下标。
    pub current: usize,
    /// 由键盘打开（F10 / Alt / ←→ 切换）：首项先高亮，让键盘用户看得见起点。
    /// 鼠标点开则不预选——桌面惯例。
    pub keyboard: bool,
    pub open: std::rc::Rc<std::cell::Cell<Option<usize>>>,
    /// 同为共享单元格：**助记字母的下划线是否显示**。宿主在键盘触达菜单栏时置位
    /// （按下 Alt、F10、Alt+助记键），鼠标按下与窗口失活时复位——与 Windows 一致：
    /// 纯鼠标操作的界面上不该常年挂着一排下划线。
    pub mnemonics: std::rc::Rc<std::cell::Cell<bool>>,
}

/// 菜单栏里的一个标题：窗口坐标下的矩形、助记字母、项生成器。
#[derive(Clone)]
pub struct MenuBarSlot {
    pub rect: Rect,
    pub mnemonic: Option<char>,
    /// 每次展开现建：项的启用 / 勾选态因此总反映当前状态。
    pub build: std::rc::Rc<dyn Fn() -> Vec<MenuItem>>,
}

impl MenuBarLink {
    /// 命中点落在哪个标题上。
    pub fn slot_at(&self, p: Point) -> Option<usize> {
        self.slots.iter().position(|s| s.rect.contains(p))
    }
    /// 响应助记字母 `c` 的标题下标（不分大小写）。
    pub fn slot_of_mnemonic(&self, c: char) -> Option<usize> {
        self.slots.iter().position(|s| {
            s.mnemonic
                .is_some_and(|m| m.to_lowercase().eq(c.to_lowercase()))
        })
    }
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
    /// 居中。设了 `owned` 就居中在发起窗口上，否则居中在屏幕上。
    pub centered: bool,
    /// 归属于**发起它的那个窗口**（Windows 的 owner、macOS 的 child window）：始终浮在
    /// 它上方、随它最小化 / 隐藏、不单独占任务栏、它关掉时一并关掉。对话框都该设。
    pub owned: bool,
    /// 模态（隐含 `owned`）：打开期间发起窗口不接受输入，关闭后焦点回到它。
    pub modal: bool,
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
    /// 窗口级快捷键回调（`Window::on_shortcut`）。返回 true = 已处理。
    ///
    /// 必须是**每个窗口自己的**，理由同 `close_handler`：快捷键作用在一棵具体的控件树与
    /// 它的焦点环上。`App::on_shortcut` 那一份只归主窗，常驻模式下更是没有主窗可归——
    /// 界面全由 `Window` 建出来，快捷键也就只能由 `Window` 自己声明。
    pub shortcut: Option<WindowShortcutHandler>,
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

/// 窗口级快捷键回调：返回 `true` = 已处理，宿主不再走 Tab / Escape 兜底。
///
/// 与 `App::on_shortcut` 收的是同一种闭包——那个作用在主窗，这个作用在
/// [`WindowRequest`] 对应的窗口上（见 [`Window::on_shortcut`](crate::app::Window::on_shortcut)）。
pub type WindowShortcutHandler = Box<dyn FnMut(&mut ShortcutCtx, KeyEvent) -> bool>;

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
    /// 关窗请求按调用顺序排队、取走即清空。
    ///
    /// 「取走即清空」不是顺带的性质：队列是线程局部的，若取不干净，下一次热键会把上一次
    /// 那条关窗请求再执行一遍——表现是「刚开出来的窗口自己关掉了」。
    #[test]
    fn close_requests_queue_in_order_and_drain_once() {
        let _ = take_callback_closes();
        let mut ctx = HotkeyCtx::default();
        ctx.close_window("main");
        ctx.close_window("settings");
        assert_eq!(take_callback_closes(), vec!["main", "settings"]);
        assert!(take_callback_closes().is_empty(), "取走一次就该空了");
    }

    /// 撞键检测的**三个方向都要钉**，而后两个比第一个重要：一个见谁都喊的提示，三天后
    /// 就会被当噪音忽略掉，那时它等于不存在。
    #[test]
    fn collision_warns_only_on_the_same_key() {
        let pending = vec![Some("main".to_string()), None, Some("settings".to_string())];
        assert!(
            key_collides("main", &pending),
            "同键：这正是不支持的那个写法"
        );
        assert!(
            !key_collides("about", &pending),
            "异键是唯一真正受支持的组合，不该报"
        );
        assert!(
            !key_collides("main", &[None, None]),
            "没有单例键的开窗请求与任何关窗都撞不上"
        );
        assert!(!key_collides("main", &[]), "没有待建窗口时更不该报");
    }

    /// 关窗与开窗是两条独立的队列，取一条不会动到另一条。
    ///
    /// **异键组合（关 a、开 b）是唯一真正被支持的组合**，而「关窗排在开窗之前」这个次序
    /// 存在的全部理由就在它身上；两条队列若互相干扰，那个次序就无从谈起。
    #[test]
    fn close_and_open_queues_do_not_disturb_each_other() {
        let _ = take_callback_closes();
        let _ = take_callback_windows();

        let mut ctx = HotkeyCtx::default();
        ctx.close_window("a");
        ctx.open_window(
            crate::app::Window::new("b 窗", 100, 80)
                .single("b")
                .content(crate::ui::Element::col),
        );

        assert_eq!(take_callback_closes(), vec!["a"], "关窗队列只该有 a");
        let opens = take_callback_windows();
        assert_eq!(opens.len(), 1, "开窗请求不该被关窗那条取走");
        assert_eq!(opens[0].single.as_deref(), Some("b"));
    }

    #[test]
    fn kind_default_durations() {
        assert_eq!(ToastKind::Error.default_duration_ms(), 5000);
        assert_eq!(ToastKind::Success.default_duration_ms(), 3000);
        assert_eq!(ToastKind::Info.default_duration_ms(), 3000);
    }
}

#[cfg(test)]
mod triple_click_tests {
    use super::*;

    fn down(count: u8, x: i32, y: i32) -> PointerEvent {
        PointerEvent {
            kind: PointerKind::Down,
            pos: Point::new(x, y),
            button: MouseButton::Left,
            click_count: count,
            mods: Mods::default(),
        }
    }

    #[test]
    fn 平台的_1_2_1_认回三击() {
        let mut t = TripleClick::default();
        assert_eq!(t.feed(&down(1, 10, 10)), 1);
        assert_eq!(t.feed(&down(2, 10, 10)), 2, "双击原样放行");
        assert_eq!(
            t.feed(&down(1, 11, 10)), 
            3,
            "平台把三击的最后一下报成新一轮首击，这里认回来"
        );
        assert_eq!(t.feed(&down(1, 11, 10)), 1, "认过一次就不再重复认");
    }

    #[test]
    fn 离得远的首击不算三击() {
        let mut t = TripleClick::default();
        t.feed(&down(2, 10, 10));
        assert_eq!(t.feed(&down(1, 200, 10)), 1, "漂移超阈值：是另一处的新单击");
    }

    #[test]
    fn 非左键与非按下一律不参与三击判定() {
        // TextInput 声明了 wants_right_click，中键会一路落到调用点（见 feed 的文档）。
        // 左键双击之后原地按一下中键，不得被认成三击。
        let mut t = TripleClick::default();
        assert_eq!(t.feed(&down(2, 10, 10)), 2);
        let mid = PointerEvent {
            button: MouseButton::Middle,
            ..down(1, 10, 10)
        };
        assert_eq!(t.feed(&mid), 1, "中键不得窃取那次双击");
        // 中键既没消费也没污染状态：随后的左键首击仍应认回三击
        assert_eq!(t.feed(&down(1, 10, 10)), 3, "左键的三击序列不受中键影响");

        // 非 Down（Up / Move）同样不参与
        let mut t2 = TripleClick::default();
        t2.feed(&down(2, 10, 10));
        let up = PointerEvent {
            kind: PointerKind::Up,
            ..down(1, 10, 10)
        };
        assert_eq!(t2.feed(&up), 1);
        assert_eq!(t2.feed(&down(1, 10, 10)), 3, "抬起不该吃掉三击");
    }

    #[test]
    fn 平台已给三击的原样放行() {
        // 合成事件与非 win32 平台可能直接给 >= 3，不该被二次判定改写。
        let mut t = TripleClick::default();
        assert_eq!(t.feed(&down(3, 10, 10)), 3);
    }
}
