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
    /// 退出应用。**刻意绕过 `hide_on_close`**——托盘退出是常驻工具的唯一真实出口，
    /// 若也转成隐藏，开了关闭转隐藏的应用将永远退不掉。
    Quit,
    /// 弹出系统气泡通知。
    Notify { title: String, body: String },
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
    /// 退出应用。
    pub fn quit(&mut self) {
        self.actions.push(TrayAction::Quit);
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

pub(crate) enum ItemKind {
    Action {
        label: String,
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
    pub fn item(label: impl Into<String>, cb: impl FnMut(&mut TrayCtx) + 'static) -> Self {
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
        label: impl Into<String>,
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

    /// 没有回调时不该凭空产生意图（左键未绑定的托盘图标点了应当什么都不发生）。
    #[test]
    fn absent_callback_yields_no_actions() {
        assert!(invoke(None).is_empty());
    }
}
