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
}
