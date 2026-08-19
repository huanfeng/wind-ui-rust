//! 给**下游**写测试用的辅助。
//!
//! [`EventCtx`] 的字段是私有的，借出它的 `Tree::run_detached` 是 `pub(crate)`——
//! 这是有意的：ctx 借的是宿主对控件树的可变访问，随手造一个等于绕开借用规则。
//! 代价是回调**在下游侧变得不可测**：本库的回调如今普遍收 `&mut EventCtx`
//! （菜单动作、`App::channel` 的 on_message、`on_close_request`、`Widget::on_event`），
//! 使用方想验证"点这一项确实弹了 toast"却造不出参数，只能把回调体抽成不收 ctx 的
//! 具名函数、再断言那个函数——测的是抽出来的那一半，回调本身有没有接对反而没人管。
//!
//! 本模块给出唯一一条受控的借出口：造一棵最小的树，借它的 ctx 跑一段闭包，
//! 把这段闭包请求的副作用（toast、对话框、关窗、菜单、URL、窗口操作、重绘）
//! 原样交回来。副作用汇总在 [`DispatchResult`] 里，与真实分发路径同一个类型。
//!
//! ```
//! use windui::prelude::*;
//! use windui::event::{MenuAction, MenuItem};
//!
//! let item = MenuItem::run("复制", |ctx| ctx.toast_ok("已复制"), false);
//! let MenuAction::Run(action) = &item.action else { panic!("应是可执行动作") };
//! let res = windui::testing::run_with_ctx(|ctx| action(ctx));
//! assert_eq!(res.toast.map(|t| t.text).as_deref(), Some("已复制"));
//! ```
//!
//! 只用于测试：生产代码里的 ctx 一律由宿主在真实分发路径上借出，自己造一棵树跑回调
//! 意味着那些副作用没有宿主去消费——toast 不会显示，关窗请求不会生效。

use crate::core::{DispatchResult, EventCtx, NodeId, Tree};
use crate::event::{HotkeyCtx, WindowOp};
use crate::platform::{TrayAction, TrayCtx, TrayMenuItem};
use crate::ui::Element;

/// 借一个 `EventCtx` 跑 `f`，返回它请求的副作用。
///
/// ctx 挂在一棵**只有一个空叶子**的临时树的根上：够用于绝大多数回调（toast、对话框、
/// 剪贴板、关窗这些请求都不看节点），但与节点几何相关的读取（`ctx.bounds()`）会得到
/// 零矩形。需要真实布局请用 [`run_with_ctx_in`] 自建树。
pub fn run_with_ctx(f: impl FnOnce(&mut EventCtx)) -> DispatchResult {
    let mut tree = Tree::new();
    let root = Element::leaf().build(&mut tree);
    tree.root = Some(root);
    run_with_ctx_in(&mut tree, root, f)
}

/// 在**你自己的树**上借 `id` 节点的 `EventCtx` 跑 `f`。
///
/// 与 [`run_with_ctx`] 的差别是树由调用方持有：回调对树的改动（`ctx.tree_mut()`、
/// 焦点、背景色）跑完仍可断言，节点几何也是真实布局后的值。
///
/// `id` 指向已被删除的节点是安全的（与宿主执行菜单动作时的处理一致：菜单弹出后
/// 目标节点可能已随重建消失），此时依赖节点的操作静默跳过。
pub fn run_with_ctx_in(
    tree: &mut Tree,
    id: NodeId,
    f: impl FnOnce(&mut EventCtx),
) -> DispatchResult {
    tree.run_detached(id, f)
}

/// 借一个 [`TrayCtx`] 跑 `f`，返回它请求的托盘意图（按调用顺序）。
///
/// 托盘是常驻工具**唯一的退出途径**，而 `TrayMenuItem::item("退出", |ctx| ctx.quit())`
/// 这类回调此前一行测试都写不了：`TrayCtx` 字段私有、无公开构造，且两个平台各有一份，
/// macOS 那份还持有真 `NSWindow`——测试里造不出来。
///
/// 现在 `TrayCtx` 是平台无关的意图容器，本函数与平台层走**同一条** `invoke` 路径，
/// 测的就是生产路径而非一份形似的复制品。
///
/// ```
/// use windui::prelude::*;
/// use windui::platform::TrayAction;
///
/// // 待测的托盘菜单项（真实应用里来自自己的构建函数）。
/// let item = TrayMenuItem::item("退出", |ctx| ctx.quit());
/// assert_eq!(
///     windui::testing::run_with_tray_ctx(item),
///     vec![TrayAction::Quit],
/// );
/// ```
///
/// 只用于测试：意图没有宿主去执行，窗口不会真的显隐、应用不会真的退出。
pub fn run_with_tray_ctx(item: TrayMenuItem) -> Vec<TrayAction> {
    let mut item = item;
    crate::platform::tray::invoke(item.callback())
}

/// 借一个 [`TrayCtx`] 跑任意闭包（不经菜单项），用于测 `Tray::on_left_click` 这类
/// 直接持有闭包的回调。
///
/// ```
/// use windui::prelude::*;
/// use windui::platform::TrayAction;
///
/// let res = windui::testing::run_with_tray_ctx_fn(|ctx| {
///     ctx.notify("已同步", "3 个词条");
///     ctx.show_window();
/// });
/// assert_eq!(res.len(), 2, "意图应按调用顺序累积，不是只留最后一个");
/// assert_eq!(res[1], TrayAction::Show);
/// ```
pub fn run_with_tray_ctx_fn(f: impl FnMut(&mut TrayCtx) + 'static) -> Vec<TrayAction> {
    let mut boxed: Box<dyn FnMut(&mut TrayCtx)> = Box::new(f);
    crate::platform::tray::invoke(Some(&mut boxed))
}

/// 借一个 [`HotkeyCtx`] 跑 `f`，返回它请求的窗口操作（未请求为 `None`）。
///
/// 与 [`run_with_tray_ctx`] 同一个理由：全局热键回调收 `HotkeyCtx`，其字段与
/// `take_op` 都是 `pub(crate)`，下游造不出参数也读不出结果——"按下热键确实把窗口
/// 调出来了"这件事测不了。
///
/// 返回 `Option` 而非 `Vec` 忠实反映了 `HotkeyCtx` 的形状：它只存**最后一个**意图
/// （字段是 `Option<WindowOp>`），与累积成队列的 `TrayCtx` 不同。
///
/// ```
/// use windui::prelude::*;
/// use windui::event::WindowOp;
///
/// let op = windui::testing::run_with_hotkey_ctx(|ctx| ctx.show_window());
/// assert_eq!(op, Some(WindowOp::Show));
/// ```
pub fn run_with_hotkey_ctx(f: impl FnOnce(&mut HotkeyCtx)) -> Option<WindowOp> {
    let mut ctx = HotkeyCtx::default();
    f(&mut ctx);
    ctx.take_op()
}

#[cfg(test)]
mod tests {
    use crate::core::Tree;
    use crate::event::{MenuAction, MenuItem};
    use crate::prelude::*;

    /// 菜单动作的副作用能被取回——这正是下游拿不到 ctx 时测不了的那一半。
    #[test]
    fn menu_action_side_effects_come_back() {
        let hit = std::rc::Rc::new(std::cell::Cell::new(0));
        let h = hit.clone();
        let item = MenuItem::run(
            "删除",
            move |ctx| {
                h.set(h.get() + 1);
                ctx.toast_err("删除失败");
                ctx.request_close();
            },
            false,
        );
        let MenuAction::Run(action) = &item.action else {
            panic!("应是可执行动作")
        };
        let res = crate::testing::run_with_ctx(|ctx| action(ctx));
        assert_eq!(hit.get(), 1, "动作应被执行一次");
        assert_eq!(res.toast.map(|t| t.text).as_deref(), Some("删除失败"));
        assert!(res.close, "关窗请求应一并交回");
    }

    /// 自带树的那一支：回调改到树上的东西跑完还在，节点几何也是布局后的真值。
    #[test]
    fn ctx_on_own_tree_keeps_changes_and_bounds() {
        let mut tree = Tree::new();
        let root = Element::col()
            .width(200)
            .height(80)
            .child(Element::leaf())
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 80), &mut crate::text::NullTextEngine);

        let res = crate::testing::run_with_ctx_in(&mut tree, root, |ctx| {
            assert_eq!(ctx.bounds().w, 200, "自带树能读到真实几何");
            ctx.set_bg(Color::hex(0xFF0000));
        });
        assert!(res.repaint, "改背景应请求重绘");
        assert!(
            tree.get(root).unwrap().style.bg.is_some(),
            "改动应留在调用方的树上"
        );
    }

    /// P2-1 回归：托盘菜单项的回调应当可测。
    ///
    /// 没有它时错在哪：`TrayCtx` 字段私有、无公开构造，且此前**两个平台各有一份**，
    /// macOS 那份持有真 `NSWindow`——测试里造不出参数。于是
    /// `TrayMenuItem::item("退出", |ctx| ctx.quit())` 这类回调一行测试都写不了，
    /// 而托盘往往是常驻工具**唯一的退出途径**。
    #[test]
    fn tray_menu_item_callbacks_are_testable() {
        use crate::platform::{TrayAction, TrayMenuItem};

        assert_eq!(
            super::run_with_tray_ctx(TrayMenuItem::item("退出", |ctx| ctx.quit())),
            vec![TrayAction::Quit]
        );
        assert_eq!(
            super::run_with_tray_ctx(TrayMenuItem::item("显示窗口", |ctx| ctx.show_window())),
            vec![TrayAction::Show]
        );
        // 分隔线没有回调，不该凭空产生意图。
        assert!(super::run_with_tray_ctx(TrayMenuItem::separator()).is_empty());
    }

    /// 勾选项的回调既能翻转自己的信号、又能请求窗口操作——两件事都要能断言。
    ///
    /// 这正是「把回调体抽成不收 ctx 的具名函数再测那个函数」测不到的部分：
    /// 抽出去之后，"它到底有没有请求隐藏窗口"没人管。
    #[test]
    fn tray_check_item_callback_reports_both_signal_and_intent() {
        use crate::platform::{TrayAction, TrayMenuItem};
        use crate::signal::signal;

        let pinned = signal(true);
        let item = TrayMenuItem::check("常驻前台", pinned, move |ctx| {
            pinned.set(!pinned.get());
            ctx.hide_window();
        });
        assert_eq!(super::run_with_tray_ctx(item), vec![TrayAction::Hide]);
        assert!(!pinned.get(), "回调应翻转勾选信号");
    }

    /// 热键回调的意图应当可读。`HotkeyCtx` 只存**最后一个**意图，与累积成队列的
    /// `TrayCtx` 不同——这个差别由返回类型（`Option` vs `Vec`）如实反映。
    ///
    /// 没有它时错在哪：`HotkeyCtx::take_op` 是 `pub(crate)`，下游读不出回调请求了什么，
    /// 「按下热键确实把窗口调出来了」测不了。
    #[test]
    fn hotkey_callback_intent_is_readable() {
        use crate::event::WindowOp;

        assert_eq!(
            super::run_with_hotkey_ctx(|ctx| ctx.show_window()),
            Some(WindowOp::Show)
        );
        assert_eq!(
            super::run_with_hotkey_ctx(|_| {}),
            None,
            "没请求就该是 None"
        );
        // 只保留最后一个：忠实于 HotkeyCtx 的字段形状，不是队列。
        assert_eq!(
            super::run_with_hotkey_ctx(|ctx| {
                ctx.show_window();
                ctx.hide_window();
            }),
            Some(WindowOp::Hide)
        );
    }
}
