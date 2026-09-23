//! 系统托盘的**平台无关声明层**：`Tray` / `TrayMenuItem` / `TrayCtx` / `TrayAction`。
//!
//! 与 [`HotkeyCtx`](crate::event::HotkeyCtx) / [`WindowOp`](crate::event::WindowOp) 同一条
//! 缝合原则——**核心层声明意图，平台层落地执行**。本模块只有声明，不含任何 OS 调用；
//! 两个平台各自消费 [`TrayAction`]（win32 `run_tray_actions`、macOS `run_tray_actions`）。
//!
//! # 为什么必须收在这里
//!
//! 此前 `Tray` 三件套在 win32 与 macOS 各有一份**完整副本**，`platform/mod.rs` 按 `cfg`
//! 分别 re-export。于是：
//!
//! - 下游的跨平台性是**巧合**——两边方法名恰好一致，类型其实是两个，语义还不同：
//!   win32 的 `TrayCtx` 累积意图，macOS 的持有 `NSWindow` 并**立即**调 OS。
//! - 回调因此**不可测**：`run_with_tray_ctx` 在 macOS 上造不出参数（要一个真 NSWindow）。
//!   而托盘往往是常驻工具唯一的退出途径，`|ctx| ctx.quit()` 这类回调一行测试都写不了。
//! - 声明层的行为约束（勾选态是弹出时现读、`Tray` 必须 `!Send`）各写一份测试，
//!   每份只在自己平台跑。
//!
//! 收口后：一个类型、一套语义、一份测试，两平台都跑。

use crate::signal::Signal;
use std::cell::RefCell;
use std::rc::Rc;

/// 应用**在运行期**对托盘图标提出的改动。
///
/// 与 [`TrayAction`] 分开是因为两者的来源与时机都不同：`TrayAction` 是**托盘回调**
/// 发出的（用户点了托盘菜单），`TrayOp` 是**应用自己**发出的（设置改了、状态变了），
/// 后者与托盘上有没有发生交互毫无关系。混成一个枚举会让「谁能发这条」失去约束。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayOp {
    /// 换鼠标悬停提示。
    SetTooltip(String),
    /// 弹出系统通知。与托盘回调的 [`TrayAction::Notify`] 走同一平台实现。
    Notify { title: String, body: String },
}

thread_local! {
    /// 运行期托盘意图队列。**线程局部而非穿过各层构造器**：托盘是**应用级单例**
    /// （一个进程一个图标，装在 app 宿主上），不属于任何一个窗口；把它的队列挂进
    /// 每个窗口的 handler，等于把「哪个窗口的托盘」这个不存在的问题引进来。
    ///
    /// 热键那条队列穿构造器是有理由的——热键有 id、可以有多个，且注册绑在窗口上。
    static TRAY_OPS: Rc<RefCell<Vec<TrayOp>>> = Rc::new(RefCell::new(Vec::new()));
}

/// 托盘的运行期句柄：造好之后仍能改托盘图标的属性。
///
/// **为什么需要它**：[`Tray`] 是构造器，`App::tray` 之后就消费掉了，而提示文字往往
/// 要随应用状态变（「Ctrl+Alt+D 查询」——用户改了热键，这句话就成了假话）。没有这条
/// 路子时，唯一的出路是让提示不提任何会变的东西，那是拿信息量换正确性。
///
/// 与 [`ThemeHandle`](crate::app::ThemeHandle) / [`HotkeyHandle`](crate::app::HotkeyHandle)
/// 同一条路子：句柄只**排队意图**，平台层在事件分发之后落地——托盘的 OS 调用
/// （`Shell_NotifyIcon`）会跨线程发消息并泵入站消息，在借用里直接调是铁律 6 的老问题。
#[derive(Clone)]
pub struct TrayHandle {
    queue: Rc<RefCell<Vec<TrayOp>>>,
}

impl Default for TrayHandle {
    fn default() -> Self {
        Self::new()
    }
}

impl TrayHandle {
    /// 连宿主的句柄。`App::tray_handle()` 走这里。
    pub fn new() -> Self {
        Self {
            queue: TRAY_OPS.with(|q| q.clone()),
        }
    }

    /// 造一个**不连宿主**的句柄，供下游给自己的应用状态写测试。理由同
    /// [`HotkeyHandle::detached`](crate::app::HotkeyHandle::detached)：真句柄要有
    /// `App`，而 `App` 在测试里建不起来（要开窗口），于是持有它的整个状态结构都
    /// 造不出实例。
    ///
    /// ```
    /// # use windui::prelude::*;
    /// use windui::platform::TrayOp;
    /// let t = TrayHandle::detached();
    /// t.set_tooltip("清风词典 — Ctrl+Alt+D 查询");
    /// assert_eq!(
    ///     t.pending_ops(),
    ///     vec![TrayOp::SetTooltip("清风词典 — Ctrl+Alt+D 查询".into())],
    /// );
    /// ```
    pub fn detached() -> Self {
        Self {
            queue: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// 换鼠标悬停提示。下一次事件分发之后落地。
    pub fn set_tooltip(&self, s: impl Into<String>) {
        self.queue.borrow_mut().push(TrayOp::SetTooltip(s.into()));
        // 排完队要**踢一帧**，否则这条意图要等到下一次有人动鼠标才被消费。改提示
        // 多半发生在设置页里，而那之后用户可能直接去托盘上悬停——中间一个事件都没有。
        crate::anim::request_repaint();
    }

    /// 弹出系统通知。与托盘菜单里的 [`TrayCtx::notify`] 是同一条平台实现，区别只在
    /// 发起方：这里是**应用状态**变了（后台任务完成、同步失败），不必先有一次托盘交互。
    ///
    /// 通知挂在托盘图标上（Win32 的气泡本就是 `NOTIFYICONDATAW` 的一部分），故没装托盘时
    /// 同样被丢弃；Linux 尚无托盘，也一并丢弃。
    pub fn notify(&self, title: impl Into<String>, body: impl Into<String>) {
        self.queue.borrow_mut().push(TrayOp::Notify {
            title: title.into(),
            body: body.into(),
        });
        // 踢一帧的理由同 `set_tooltip`：发起方多在后台状态回调里，之后未必再有事件。
        crate::anim::request_repaint();
    }

    /// 已排队、尚未被平台层消费的意图（按调用顺序）。**只读不取走**，故对真句柄
    /// 调用也是安全的，不会把宿主该执行的改动偷掉。
    pub fn pending_ops(&self) -> Vec<TrayOp> {
        self.queue.borrow().clone()
    }
}

/// 取走并清空运行期托盘意图。平台层在事件分发之后调用。
pub(crate) fn take_tray_ops() -> Vec<TrayOp> {
    TRAY_OPS.with(|q| std::mem::take(&mut *q.borrow_mut()))
}

/// 托盘回调想做的事。**纯意图，不含任何 OS 调用**。
///
/// 存在的理由见 [`TrayCtx`]。`pub` 是为了下游能在测试里断言回调请求了什么
/// （见 [`crate::testing::run_with_tray_ctx`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayAction {
    /// 显示并前置窗口（从隐藏态唤起）。
    Show,
    /// 隐藏窗口（最小化到托盘），进程继续存活。
    Hide,
    /// 在显示与隐藏之间切换，由**平台层**按窗口当前的真实可见性决定往哪边走。
    ///
    /// 为什么不让应用自己记一个 bool：窗口可以从应用不经手的地方隐去——最小化到托盘
    /// （`App::hide_on_minimize`）、用户点任务栏、Win+D 显示桌面。应用记的那份状态
    /// 会跟真实情况岔开，症状是"最小化之后点托盘图标没反应，要点两下才出来"。
    Toggle,
    /// 退出应用。**刻意绕过 `hide_on_close`**——托盘退出是常驻工具的唯一真实出口，
    /// 若也转成隐藏，开了关闭转隐藏的应用将永远退不掉。
    Quit,
    /// 弹出系统气泡通知。
    Notify { title: String, body: String },
    /// 新建一个窗口（[`TrayCtx::open_window`]）。
    ///
    /// **不带载荷**：窗口配置（[`WindowRequest`](crate::event::WindowRequest)）带闭包，
    /// 塞进来就得放弃本枚举的 `Debug`/`Clone`/`PartialEq`——而 `Vec<TrayAction>` 的相等
    /// 比较正是下游给托盘回调写测试的正式入口（见 [`crate::testing::run_with_tray_ctx`]）。
    /// 故配置走核心层的旁路队列，这里只留**位置标记**：平台层执行到本变体时取走队首那
    /// 一个，于是「先弹气泡再开窗」这类顺序仍然成立。
    ///
    /// 推论：把一份 `Vec<TrayAction>` 执行两遍，第二遍的 `OpenWindow` 会落空（队列已空）
    /// 而不是开出第二个窗口。意图队列本就只该被执行一次，这里不做额外防护。
    OpenWindow,
    /// 关掉带这个单例键（[`Window::single`](crate::app::Window::single)）的窗口
    /// （[`TrayCtx::close_window`]）。没有这样一个窗口时什么也不做。
    ///
    /// **这个带载荷而 [`OpenWindow`](Self::OpenWindow) 不带**：那边的载荷是带闭包的
    /// `WindowRequest`，塞进来会砸掉本枚举的 `Debug`/`Clone`/`PartialEq`；一个 `String`
    /// 不会，于是键直接写在意图里——下游给托盘回调写测试时读到的是
    /// `CloseWindow("main")`，比一个还要再去查旁路队列的位置标记实在得多。
    CloseWindow(String),
}

/// 托盘回调上下文：显隐窗口 / 退出 / 气泡通知。
///
/// **这里的方法只记录意图，不调用任何 OS API。** win32 侧回调运行时 `wnd_proc` 正持有
/// `&mut WindowState`，而 `ShowWindow` / `DestroyWindow` / `TrackPopupMenu` 都会同步派发
/// 消息重入 `wnd_proc`，届时再取一次 `&mut WindowState` 即形成别名 UB（铁律 6，无 RefCell
/// 故不会 panic，只会静默出错）。真正的执行发生在借用释放之后。
///
/// 把窗口操作降级为「意图」使该约束成为**类型上的保证**而非人的记性，代价是两平台都要有
/// 一个消费点——换来的是同一个 `TrayCtx` 类型、同一套语义，以及回调可测。
///
/// 意图按调用顺序累积成队列、逐条执行，故一个回调内 `notify` 后再 `show_window` 两者都
/// 生效，与「立即执行」的直觉一致。`Quit` 之后的意图被丢弃（窗口已销毁，后续意图本就
/// 无从生效；macOS 的 `NSApp::terminate` 更是根本不返回）。
#[derive(Debug, Default)]
pub struct TrayCtx {
    actions: Vec<TrayAction>,
}

impl TrayCtx {
    /// 显示并前置窗口（托盘最常见动作）。
    pub fn show_window(&mut self) {
        self.actions.push(TrayAction::Show);
    }
    /// 隐藏窗口（最小化到托盘）。
    pub fn hide_window(&mut self) {
        self.actions.push(TrayAction::Hide);
    }
    /// 在显示与隐藏之间切换（左键单击最常见的用法）。
    ///
    /// 由平台按窗口此刻**真实的**可见性决定往哪边走，应用不必自己记——见
    /// [`TrayAction::Toggle`]。
    pub fn toggle_window(&mut self) {
        self.actions.push(TrayAction::Toggle);
    }
    /// 退出应用。
    pub fn quit(&mut self) {
        self.actions.push(TrayAction::Quit);
    }
    /// 新建一个窗口，语义同
    /// [`EventCtx::open_window`](crate::core::EventCtx::open_window)。
    ///
    /// 零窗口常驻模式（[`App::run_resident`](crate::app::App::run_resident)）下，托盘菜单
    /// 的「打开窗口」是界面唯一的入口——那时没有窗口可 [`show_window`](Self::show_window)，
    /// 窗口关掉即销毁，下次再点这一项重新建一个。
    ///
    /// 与其他意图一样**只排队不执行**（理由见类型文档）：窗口在回调返回、平台层释放
    /// 借用之后才真正创建。想让重复点击复用同一个窗口就给它
    /// [`Window::single`](crate::app::Window::single) 一个键。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// TrayMenuItem::item("打开窗口", |ctx| {
    ///     ctx.open_window(
    ///         Window::new("主界面", 480, 320)
    ///             .single("main")
    ///             .content(|| Element::col().fill()),
    ///     );
    /// });
    /// ```
    pub fn open_window(&mut self, req: crate::event::WindowRequest) {
        crate::event::push_callback_window(req);
        self.actions.push(TrayAction::OpenWindow);
    }
    /// 关掉带指定单例键（[`Window::single`](crate::app::Window::single)）的窗口。
    /// 没有这样一个窗口时什么也不做。
    ///
    /// 常驻模式下这是 [`hide_window`](Self::hide_window) 的替代：那时没有主窗可隐藏，
    /// 而窗口关掉即销毁——「收起来」与「关掉」在那个模式里本就是同一件事，渲染资源
    /// 也随之归还。配合 [`window_open`](crate::event::window_open) 判分支，即可让托盘
    /// 单击在「唤出」与「收起」之间切换。
    ///
    /// 走完整的关闭决策链（`Window::on_close_request` 会被问到），与用户点标题栏关闭
    /// 按钮同义。
    ///
    /// **次序按声明来**，与本类型其余意图一致（热键那条路不同：它的关窗请求统一排在开窗
    /// 之前）。但两条路有一条共同的禁忌：**同一个回调里对同一个键既关又开是不支持的**
    /// ——关窗是投递 `WM_CLOSE`、开窗是即时，那样写时开窗会命中那个尚未关掉的窗口、退化
    /// 成激活它，随后它被关掉，净结果是一个窗口都没有（平台层会打一行提示）。要重开请
    /// 分两次回调，或用不同的键。
    pub fn close_window(&mut self, key: impl Into<String>) {
        self.actions.push(TrayAction::CloseWindow(key.into()));
    }
    /// 弹出气泡通知（标题 + 正文）。macOS 上未打包为 .app 时可能不展示。
    pub fn notify(&mut self, title: &str, body: &str) {
        self.actions.push(TrayAction::Notify {
            title: title.to_string(),
            body: body.to_string(),
        });
    }
    /// 取出累积的意图（平台层在**释放窗口状态借用之后**调用）。
    pub(crate) fn take_actions(&mut self) -> Vec<TrayAction> {
        std::mem::take(&mut self.actions)
    }
}

pub(crate) type TrayFn = Box<dyn FnMut(&mut TrayCtx)>;

/// 借一个受控的 `TrayCtx` 跑回调，把它请求的意图交回来。
///
/// 平台层与 [`crate::testing::run_with_tray_ctx`] 共用同一条路径——测试跑的就是生产路径，
/// 不是一份形似的复制品。
pub(crate) fn invoke(cb: Option<&mut TrayFn>) -> Vec<TrayAction> {
    let mut ctx = TrayCtx::default();
    if let Some(cb) = cb {
        cb(&mut ctx);
    }
    ctx.take_actions()
}

// Linux 后端尚无托盘，菜单项的标签 / 勾选态与按下标取回调在那里无人读取。
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub(crate) enum ItemKind {
    Action {
        /// 标签。收 [`TextContent`] 而不是 `String`，于是它能是一条待翻译消息
        /// （[`t!`](crate::t)）或一个信号——**托盘菜单在每次弹出时才构建**
        /// （win32 `build_menu`、macOS `pop_menu`），那时才解析，于是换语言下一次
        /// 右键就是新文案，不必重建整个 `Tray`。
        label: crate::ui::TextContent,
        /// 勾选态绑定（None=从不打勾）；菜单弹出时读当前值。
        checked: Option<Signal<bool>>,
        /// 禁用态绑定（None=始终可用）；菜单弹出时读当前值，false 则灰显且不可点。
        enabled: Option<Signal<bool>>,
        cb: TrayFn,
    },
    Separator,
}

/// 托盘右键菜单项：普通项 / 勾选项 / 分隔线。
pub struct TrayMenuItem {
    pub(crate) kind: ItemKind,
}

impl TrayMenuItem {
    /// 普通项：点击触发回调。
    ///
    /// 标签可传 `&str` / `String` / `Signal<String>` / [`t!`](crate::t)，与控件文案同一套
    /// 规则（见 [`TextContent`]）。菜单每次弹出时现取，故信号与语言都自动跟随。
    pub fn item(
        label: impl Into<crate::ui::TextContent>,
        cb: impl FnMut(&mut TrayCtx) + 'static,
    ) -> Self {
        Self {
            kind: ItemKind::Action {
                label: label.into(),
                checked: None,
                enabled: None,
                cb: Box::new(cb),
            },
        }
    }
    /// 勾选项：`checked` 绑定状态，菜单弹出时按当前值显示对勾；点击触发回调
    /// （回调内自行翻转 `checked` 即可，框架不自动改）。
    ///
    /// `Signal<bool>` 是 `!Send` 的（存储线程局部），故整个 `Tray` 也是 `!Send`——
    /// 托盘菜单在 UI 线程构建、勾选态也在 UI 线程的菜单弹出路径上读取，
    /// 把构建好的 `Tray` 搬到别的线程会在编译期就被拦下。
    pub fn check(
        label: impl Into<crate::ui::TextContent>,
        checked: Signal<bool>,
        cb: impl FnMut(&mut TrayCtx) + 'static,
    ) -> Self {
        Self {
            kind: ItemKind::Action {
                label: label.into(),
                checked: Some(checked),
                enabled: None,
                cb: Box::new(cb),
            },
        }
    }
    /// 绑定禁用态：`flag` 为 false 时该项灰显且不可点（菜单弹出时读当前值）。
    /// 对分隔线无效。永久禁用可传 `signal(false)`。
    pub fn enabled(mut self, flag: Signal<bool>) -> Self {
        if let ItemKind::Action { enabled, .. } = &mut self.kind {
            *enabled = Some(flag);
        }
        self
    }
    /// 分隔线。
    pub fn separator() -> Self {
        Self {
            kind: ItemKind::Separator,
        }
    }
    /// 本项的回调（分隔线为 `None`）。平台层的菜单分发与
    /// [`crate::testing::run_with_tray_ctx`] 共用。
    pub(crate) fn callback(&mut self) -> Option<&mut TrayFn> {
        match self.kind {
            ItemKind::Action { ref mut cb, .. } => Some(cb),
            ItemKind::Separator => None,
        }
    }
}

/// 托盘图标构建器。交给 `App::tray(...)`。
#[derive(Default)]
pub struct Tray {
    pub(crate) tooltip: String,
    pub(crate) icon: Option<(u32, u32, Vec<u8>)>,
    pub(crate) on_left_click: Option<TrayFn>,
    pub(crate) on_double_click: Option<TrayFn>,
    pub(crate) items: Vec<TrayMenuItem>,
}

impl Tray {
    pub fn new() -> Self {
        Self::default()
    }
    /// 鼠标悬停提示。
    pub fn tooltip(mut self, s: impl Into<String>) -> Self {
        self.tooltip = s.into();
        self
    }
    /// 自定义图标：原始非预乘 RGBA8（`rgba.len()==w*h*4`）。未设则用系统默认应用图标。
    pub fn icon_rgba(mut self, w: u32, h: u32, rgba: &[u8]) -> Self {
        self.icon = Some((w, h, rgba.to_vec()));
        self
    }
    /// 左键单击回调（常见用于显隐窗口）。
    pub fn on_left_click(mut self, f: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        self.on_left_click = Some(Box::new(f));
        self
    }
    /// 左键双击回调。
    pub fn on_double_click(mut self, f: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        self.on_double_click = Some(Box::new(f));
        self
    }
    /// 右键菜单项（普通/勾选/分隔线）。
    pub fn menu(mut self, items: Vec<TrayMenuItem>) -> Self {
        self.items = items;
        self
    }
    /// 取指定下标菜单项的回调（平台层在模态菜单关闭后分发用）。
    #[cfg_attr(target_os = "linux", allow(dead_code))]
    pub(crate) fn item_callback(&mut self, idx: usize) -> Option<&mut TrayFn> {
        self.items.get_mut(idx)?.callback()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::signal;

    /// 编译期护栏：`Tray` 必须保持 `!Send`。
    ///
    /// 勾选态与禁用态都绑 `Signal<bool>`，而信号的存储是**线程局部**的——句柄搬到别的
    /// 线程再读，读到的是那个线程的槽位表。`!Send` 让「构建 Tray 的线程」与「弹菜单读
    /// 勾选态的线程」必然是同一个：`Tray` 只能原地交给 `App::tray`，`App` 因此也 `!Send`，
    /// `App::run` 在同一线程消费它并在那里建窗口。
    ///
    /// 一旦 `Tray` 变成 `Send`，下面两条 impl 同时适用，方法解析出歧义，编译失败。
    ///
    /// 此前这条护栏在两个平台各有一份，各自只在自己平台编译；收进声明层后一份即够，
    /// 且两平台都会跑到。
    const _: fn() = || {
        trait AmbiguousIfSend<A> {
            fn tag() {}
        }
        impl<T: ?Sized> AmbiguousIfSend<()> for T {}
        struct Invalid;
        impl<T: ?Sized + Send> AmbiguousIfSend<Invalid> for T {}
        let _ = <Tray as AmbiguousIfSend<_>>::tag;
    };

    /// 关窗意图把键带在自己身上，不经旁路队列——这正是它与 `OpenWindow` 的分别，
    /// 也是下游能对着 `Vec<TrayAction>` 断言「点了这一项会收起主界面」的前提。
    ///
    /// 常驻模式下托盘的「收起」只能这么表达：那时没有主窗，`hide_window` 无处可施。
    #[test]
    fn close_window_carries_its_key_in_the_action() {
        let got = crate::testing::run_with_tray_ctx_fn(|ctx| {
            ctx.notify("已同步", "3 个词条");
            ctx.close_window("main");
        });
        assert_eq!(
            got,
            vec![
                TrayAction::Notify {
                    title: "已同步".into(),
                    body: "3 个词条".into()
                },
                TrayAction::CloseWindow("main".into()),
            ],
            "意图按调用顺序累积，且关窗那条自带键"
        );
    }

    /// 标签是**弹出时现取**而非构建时定格——托盘菜单在应用启动时就建好了，
    /// 而它可能活到用户换过语言之后。
    ///
    /// 本测试钉住的是「存储不提前定格」这一半；另一半（平台在 `build_menu` /
    /// `pop_menu` 里真的调 `resolve`）要真托盘才验得到，但那一半有编译器兜底：
    /// `label` 的类型已不是 `String`，忘了解析根本编不过。
    #[test]
    fn label_is_taken_when_the_menu_pops_not_when_it_is_built() {
        crate::i18n::install(
            crate::i18n::Locales::builder()
                .embed("[meta]\nlocale = \"zh-CN\"\n[tray]\nquit = \"退出\"\n")
                .embed("[meta]\nlocale = \"en\"\n[tray]\nquit = \"Quit\"\n")
                .initial(crate::i18n::Initial::Fixed("zh-CN".into()))
                .build(),
        );
        let it = TrayMenuItem::item(crate::t!("tray.quit"), |_| {});
        let ItemKind::Action { label, .. } = &it.kind else {
            unreachable!("item() 建的就是 Action 项");
        };
        assert_eq!(label.resolve(), "退出");

        assert!(crate::i18n::LocaleHandle::new().set("en"));
        assert_eq!(
            label.resolve(),
            "Quit",
            "换语言后下一次弹出就该是新文案，不必重建整个 Tray"
        );

        // 复原：`LOCALES`/`CURRENT` 是线程局部的，而 libtest 会复用线程。留一个只装着
        // `[tray] quit`、语言停在 en 的目录给下一条测试，会让「不 install 就该读到
        // windui.*」那类断言按调度顺序随机红——最难查的那种 flaky。
        crate::i18n::install(crate::i18n::Locales::default());
    }

    /// 勾选态是**弹出时现读**而非构建时快照：构建完菜单项后翻转信号，
    /// 下一次弹出就该显示新状态（这正是 `check` 收信号而非 `bool` 的全部理由）。
    #[test]
    fn check_binds_the_signal_instead_of_snapshotting_its_value() {
        let on = signal(false);
        let it = TrayMenuItem::check("启用通知", on, |_| {});
        let ItemKind::Action { checked, .. } = &it.kind else {
            unreachable!("check() 建的就是 Action 项");
        };
        assert_eq!(checked.map(|c| c.get()), Some(false));
        on.set(true);
        assert_eq!(checked.map(|c| c.get()), Some(true));
    }

    /// 普通项不带勾选绑定（`None` = 从不打勾）；分隔线根本没有这组字段。
    #[test]
    fn item_and_separator_carry_no_check_binding() {
        let it = TrayMenuItem::item("显示窗口", |_| {});
        let ItemKind::Action { checked, .. } = &it.kind else {
            unreachable!()
        };
        assert!(checked.is_none());
        assert!(matches!(
            TrayMenuItem::separator().kind,
            ItemKind::Separator
        ));
    }

    /// 意图按调用顺序累积——回调内 `notify` 后再 `show_window`，两者都生效且不乱序。
    ///
    /// 没有这条时错在哪：若 `TrayCtx` 只存**最后一个**意图（`HotkeyCtx` 就是那样，
    /// 它只有一个 `Option<WindowOp>`），"弹个通知然后把窗口调出来"就会静默只做后半段。
    #[test]
    fn actions_accumulate_in_call_order() {
        let mut cb: TrayFn = Box::new(|ctx| {
            ctx.notify("标题", "正文");
            ctx.show_window();
        });
        assert_eq!(
            invoke(Some(&mut cb)),
            vec![
                TrayAction::Notify {
                    title: "标题".into(),
                    body: "正文".into()
                },
                TrayAction::Show,
            ]
        );
    }

    /// 运行期通知与改提示走同一条意图队列，按调用顺序排队（不会被合并或重排）。
    #[test]
    fn runtime_handle_queues_notification_in_call_order() {
        let handle = TrayHandle::detached();
        handle.set_tooltip("同步中");
        handle.notify("同步完成", "已上传 3 个文件");
        assert_eq!(
            handle.pending_ops(),
            vec![
                TrayOp::SetTooltip("同步中".into()),
                TrayOp::Notify {
                    title: "同步完成".into(),
                    body: "已上传 3 个文件".into(),
                },
            ]
        );
    }

    /// 没有回调时不该凭空产生意图（左键未绑定的托盘图标点了应当什么都不发生）。
    #[test]
    fn absent_callback_yields_no_actions() {
        assert!(invoke(None).is_empty());
    }

    /// 开窗意图：标记进意图队列**占住位置**，配置进核心层的旁路队列。
    ///
    /// 两条队列必须同步——标记多了会让平台层去取一个不存在的配置（静默不开窗），
    /// 配置多了则会有一个请求永远没人取（下一次开窗开出上一次那个）。
    #[test]
    fn open_window_marks_its_place_and_queues_the_request() {
        let _ = crate::event::take_callback_windows(); // 清掉别的用例可能留下的残渣
        let mut cb: TrayFn = Box::new(|ctx| {
            ctx.notify("标题", "正文");
            ctx.open_window(
                crate::app::Window::new("设置", 400, 300).content(crate::ui::Element::col),
            );
        });
        assert_eq!(
            invoke(Some(&mut cb)),
            vec![
                TrayAction::Notify {
                    title: "标题".into(),
                    body: "正文".into()
                },
                TrayAction::OpenWindow,
            ],
            "开窗要排在气泡之后，顺序与调用一致"
        );
        let req = crate::event::take_callback_window().expect("配置应已在旁路队列里");
        assert_eq!(req.title, "设置");
        assert!(
            crate::event::take_callback_window().is_none(),
            "一次 open_window 只该排一个请求"
        );
    }
}
