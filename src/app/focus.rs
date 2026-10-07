//! 焦点归属：Tab 顺序、焦点环可见性、模态层进出时的焦点移交。
//!
//! 「焦点该在谁身上」是一个独立于绘制与浮层的裁决：布局稳定后刷新可聚焦集合、
//! 模态作用域变化的那一帧移交、结构变更后把失效焦点归一化掉。

use crate::core::NodeId;

use super::UiHost;

/// 焦点由哪种设备转移而来。决定焦点环显不显示——`:focus-visible` 的判据是用户最近
/// 一次交互用的什么设备，而不是这次聚焦是不是程序性的。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum FocusSource {
    Pointer,
    Keyboard,
}

/// 宿主持有的焦点状态。
#[derive(Default)]
pub(super) struct FocusState {
    /// 当前焦点节点。
    pub(super) current: Option<NodeId>,
    /// Tab 焦点顺序（布局稳定后每帧刷新）。
    pub(super) order: Vec<NodeId>,
    /// 焦点环是否可见：键盘 Tab 导航时 true，鼠标聚焦时 false。
    pub(super) visible: bool,
    /// 上一帧的模态作用域（`Tree::topmost_modal`）。与本帧比较以侦测对话框
    /// 弹出/关闭/换层，据以移交焦点（见 `sync_modal_focus`）。
    scope: Option<NodeId>,
    /// 进入模态前的焦点，退出时归还。嵌套对话框只记最外那次进入。
    before_modal: Option<NodeId>,
    /// 声明式初始焦点是否已兑现过。**一次性**：兑现后焦点归用户，不会每帧粘回去。
    autofocus_done: bool,
    /// 正在落地 App 级回调（定时器 / 通道消息 / 关窗询问…）的副作用：其间的焦点转移
    /// 不是用户离开，不通知失焦（见 `notify_blur`）。
    pub(super) in_app_callback: bool,
}

impl UiHost {
    /// 布局稳定后刷新焦点：重算 Tab 顺序 → 模态移交 → 归一化失效焦点 → 同步焦点环。
    pub(super) fn refresh_focus(&mut self) {
        // 布局后结构稳定，刷新 Tab 焦点顺序。
        self.focus.order = self.tree.focusable_order();
        // 模态层进出时移交焦点。必须在下面的归一化之前——归一化只会把落在框外的
        // 旧焦点抹成 None，抹完就分不清"本该还给谁"了。
        self.sync_modal_focus();
        // 若当前焦点已不在可聚焦集合中（结构变更），归一化为无焦点。
        if let Some(f) = self.focus.current {
            if !self.focus.order.contains(&f) {
                self.tree.set_focused(None, Some(f));
                self.focus.current = None;
            }
        }
        // 控件在 on_update 里要过的焦点：在 autofocus **之前**落地。
        //
        // 顺序是承重的：这是用户动作（点了一行）直接引发的请求，而 autofocus 是
        // 「没人要焦点时给个归宿」的兜底。反过来的话，兜底会先把焦点安给查询框，
        // 这条请求再把它抢走——白抢一次，且 `autofocus_select_all` 的全选也白做了。
        self.apply_pending_focus();
        // 声明式初始焦点：在归一化**之后**兑现——归一化可能刚把失效焦点抹成 None，
        // 那正是"此刻没有焦点"该由 autofocus 接手的时机。
        self.honor_autofocus();
        self.tree.focus_ring_visible = self.focus.visible;
    }

    /// 窗口从隐藏态被唤起：让 [`Autofocus`] **重新兑现一次**。
    ///
    /// 为什么这是对的：`autofocus` 平时是一次性的（兑现后焦点归用户），但"从托盘/热键
    /// 唤起"是一段新的交互的开始，与页面重新载入同构——查询框该重新拿到焦点并全选上次
    /// 的词，用户直接覆盖打字。没有这一步，第二次唤起时焦点还停在上次离开的地方，
    /// `autofocus_select_all` 的"全选"也不会再发生。
    ///
    /// 只在**隐藏→可见的跃迁**上调用（平台层判定），故已经可见时再按热键不会重置界面。
    pub(super) fn rearm_autofocus(&mut self) {
        self.focus.autofocus_done = false;
        // 节点上的"已兑现"也一并复位，否则重新唤起时 `first_autofocus` 仍跳过它们
        self.tree.clear_autofocus_done();
        // 还得把焦点**让出来**，只清标志不够。
        //
        // 窗口隐藏不清焦点（`focus.current` 原样留着上次那个控件），而
        // `honor_autofocus` 有一步「不抢已有焦点」——于是重新 arm 之后它当场让位，
        // 既不把焦点移回查询框，也不会重新全选。表现是：用户上次把焦点留在了别处
        // （Tab 到列表、点了某个按钮），再按热键唤起就得先自己点回输入框；即便焦点
        // 本来就在输入框上，「全选上次的词」也从第二次唤起起就再没发生过——那正是
        // `autofocus_select_all` 存在的全部理由。
        //
        // 只在隐藏→可见的跃迁上走这一条（调用方保证），故不会打断用户正在进行的操作。
        if let Some(f) = self.focus.current.take() {
            self.tree.set_focused(None, Some(f));
        }
    }

    /// 落地 on_update 相位攒下的焦点转移请求（见 `Tree::pending_focus`）。
    ///
    /// 请求的目标必须仍在焦点环里——它是上一帧的节点 id，而这中间可能刚重建过子树。
    /// 不在就丢弃：把焦点交给一个已经不存在的节点，等于让键盘事件无处可去。
    fn apply_pending_focus(&mut self) {
        let Some(id) = self.tree.take_pending_focus() else {
            return;
        };
        if !self.focus.order.contains(&id) || self.focus.current == Some(id) {
            return;
        }
        let old = self.focus.current;
        self.tree.set_focused(Some(id), old);
        self.focus.current = Some(id);
        // 不通知失焦（见 `Widget::on_blur`）：on_update 相位由信号变化驱动，起因可能是
        // 定时器 / 通道消息而非用户；且这里在 render 中途，回调写的信号赶不上本帧重排。
        // 不点亮焦点环：这条路径的触发者是鼠标点击，沿用 `:focus-visible` 的判据
        // ——环显不显示取决于用户最近一次交互用的什么设备。理由同 `honor_autofocus`。
    }

    /// 兑现 [`Autofocus`]：节点首次进入焦点环那一帧把焦点交给它，只做一次。
    ///
    /// 没有它时错在哪：`focus == None` 时 `Tree::dispatch_key` 直接丢弃整个按键事件
    /// （它的目标是 `Option<NodeId>`，没有"派给根节点"的兜底）。`start_hidden()` 的
    /// 常驻工具因此在热键唤起后第一次按键完全无响应——不是打错了地方，是消失了。
    fn honor_autofocus(&mut self) {
        // 还没进焦点环（藏在未选中的 Tab 页里、被模态遮住、暂时禁用）→ 不算兑现过，
        // 下一帧继续等。故"对话框弹着的那一帧"不会把焦点送到遮罩后面去。
        let Some((id, mode)) = self.tree.first_autofocus(&self.focus.order) else {
            return;
        };
        // 兜底档（Focus / FocusSelectAll）守着宿主那个全局一次性标志：兑现过之后焦点
        // 就归用户了，不会每帧粘回去。
        //
        // 夺取档（Take*）只看**节点自己**的标志：它的语义是"每次出现都夺取"，而全局
        // 标志一经置位就再不复位，跟着它走的话窗口里第二个就地编辑框永远拿不到焦点
        // ——表现正是「第一次 F2 好使，第二次起光标在框里闪却打不进字」。
        if !mode.takes_focus() {
            if self.focus.autofocus_done {
                return;
            }
            self.focus.autofocus_done = true;
        }
        // 出现即算兑现，无论下面这一步是否真的动了焦点——否则用户点走焦点之后
        // 它会再抢回来，成了"焦点粘死在输入框上"。
        self.tree.mark_autofocus_done(id);
        // 不抢已有焦点：这一帧焦点已有归属（如模态移交）时让位。
        // `Autofocus::Take*` 例外——就地编辑框正是被按键唤出来接收输入的，让位就成了
        // "看得见光标却打不进字"，键入会继续落到唤出它的那个控件上。
        let old = self.focus.current;
        if old.is_some() && !mode.takes_focus() {
            return;
        }
        self.tree.set_focused(Some(id), old);
        self.focus.current = Some(id);
        // 目标滚出视口时滚过来。本帧已过 relayout，故请求下一帧重排让新 scroll_y 落地；
        // 未发生滚动则不请求，避免每次启动都白搭一帧。
        if self.tree.scroll_into_view(id) {
            self.damage.needs_relayout = true;
            self.damage.needs_full = true;
        }
        // 焦点环**不点亮**：程序性移交沿用 `:focus-visible` 判据（看用户最近一次交互
        // 用的什么设备），理由同 `sync_modal_focus` 末尾那段。
        if mode.selects_all() {
            self.select_all_in(id);
        }
    }

    /// 向 `id` 合成一次 Ctrl+A。
    ///
    /// 走合成按键而不是新加一个 `Widget::select_all()`：右键菜单的复制/粘贴/全选本就
    /// 是这么做的（见 `TextInput::context_menu_items`），复用它省掉一整个 trait 方法，
    /// 且对不处理 Ctrl+A 的控件天然退化为无害空操作。
    fn select_all_in(&mut self, id: NodeId) {
        let ev = crate::event::KeyEvent {
            key: crate::event::Key::Other(0x41), // 'A'
            pressed: true,
            shift: false,
            ctrl: true,
            alt: false,
            meta: false,
        };
        let res = self.tree.dispatch_key(ev, Some(id));
        // 副作用照常上交，不丢：合成的是一次真按键，控件请求的重绘/脏区/窗口操作都算。
        // 焦点来源按 Pointer 记 —— 这不是键盘导航，不该点亮焦点环。
        let (_, damage, _) = self.apply_dispatch_effects(res, FocusSource::Pointer, None);
        self.apply_damage(damage);
    }

    /// 模态层进出时移交焦点：弹出 → 落到对话框首个可聚焦控件并记下来处；
    /// 关闭 → 还给弹出前那个控件。同网页 `<dialog>.showModal()` 的语义。
    ///
    /// 只在作用域**变化**的那一帧动作，此后用户 Tab 到哪儿就是哪儿——每帧都强制
    /// 聚焦会把焦点粘死在首项上。
    fn sync_modal_focus(&mut self) {
        let scope = self.tree.topmost_modal();
        if scope == self.focus.scope {
            return;
        }
        let was_inside = self.focus.scope.is_some();
        self.focus.scope = scope;
        let target = if scope.is_some() {
            // 进入模态。A→B 的嵌套切换不覆盖来处，B 关掉回到 A 时才不会丢掉最初那个。
            if !was_inside {
                self.focus.before_modal = self.focus.current;
            }
            self.focus.order.first().copied()
        } else {
            // 退出模态：归还来处（它可能已随结构变更消失，故再验一次）。
            self.focus
                .before_modal
                .take()
                .filter(|f| self.focus.order.contains(f))
        };
        let old = self.focus.current;
        self.tree.set_focused(target, old);
        self.focus.current = target;
        // 焦点环可见性**沿用当前状态**，不因这次代挪而强制打开：鼠标点开的对话框
        // 凭空冒出焦点框很突兀，而键盘用户此前 Tab 过、focus_visible 本就是 true，
        // 焦点照常画得出来。同 :focus-visible 的启发式——聚焦虽是程序性的，判据是
        // 用户最近一次交互用的什么。
    }

    /// **用户**让 `old` 失焦（焦点去了 `new`）：通知它并落地回调的副作用。返回是否需要重绘。
    ///
    /// 只由用户导致的转移调用——点到别的控件、点空白、Tab、控件经 ctx 要焦点。
    /// 框架自己的调度（`refresh_focus` 的归一化与 on_update 相位的焦点请求、
    /// `sync_modal_focus`、`honor_autofocus`、`rearm_autofocus`）**不调**，理由见
    /// [`Widget::on_blur`](crate::core::Widget::on_blur)。
    ///
    /// App 级回调（定时器 / 通道消息…）落地副作用期间也不调：那是程序行为，不是用户
    /// 离开。判据显式记在 `in_app_callback` 上，而**不能**借焦点环（`focus.order`）判
    /// "去向正不正常"——Tab 环 ≠ 能拿焦点的节点：纯文本 RichText、`.focusable(false)`
    /// 的按钮都会在按下时要焦点却不在环里，借环判就把点它们引起的提交吞掉了。
    ///
    /// 回调可能再要焦点（校验失败留在原框），那会经 `apply_dispatch_effects` 再走一次
    /// 本函数——深度由应用代码决定。`source` 原样下传，否则 Tab 引发的连锁会把键盘
    /// 焦点环冲成鼠标态。
    pub(super) fn notify_blur(
        &mut self,
        old: Option<NodeId>,
        new: Option<NodeId>,
        source: FocusSource,
    ) -> bool {
        let Some(o) = old else {
            return false;
        };
        if Some(o) == new || self.focus.in_app_callback {
            return false;
        }
        let res = self.tree.dispatch_blur(o);
        if !res.has_effects() {
            return false;
        }
        let (repaint, damage, _) = self.apply_dispatch_effects(res, source, None);
        self.apply_damage(damage);
        repaint
    }

    /// Tab 焦点移动（forward=正向）。返回是否变化。
    pub(super) fn move_focus(&mut self, forward: bool) -> bool {
        if self.focus.order.is_empty() {
            return false;
        }
        let n = self.focus.order.len();
        let cur = self
            .focus
            .current
            .and_then(|f| self.focus.order.iter().position(|&x| x == f));
        let next = match cur {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None if forward => 0,
            None => n - 1,
        };
        let nf = Some(self.focus.order[next]);
        let old = self.focus.current;
        self.tree.set_focused(nf, old);
        self.focus.current = nf;
        // 新焦点可能在滚动区外（滚出视口的节点仍在焦点环里），滚过去让它露出来。
        // 调用方 Tab 分支已置 needs_full，本帧的全窗路径会重排并钳制新的 scroll_y。
        self.notify_blur(old, nf, FocusSource::Keyboard);
        // 滚向通知**之后**的焦点：提交回调可能把焦点要回原框（校验失败），视口该跟着它，
        // 而不是停在已经不是焦点的 nf 上。
        if let Some(f) = self.focus.current {
            self.tree.scroll_into_view(f);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use crate::app::test_support::key_ev;
    use crate::app::{App, UiHost};
    use crate::event::Key;
    use crate::geometry::Size;
    use crate::ui::Element;

    /// 点控件外的空白应清空焦点（网页 blur 语义）：否则聚焦边框会一直亮到
    /// 下一个可聚焦控件接手为止。同时校验两条不该误清的边界。
    #[test]
    fn click_outside_clears_focus() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(Element::button("A"))
                .child(Element::flex_spacer()),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));

        let click = |h: &mut UiHost, p: Point| {
            h.on_pointer(PointerEvent::single(
                PointerKind::Down,
                p,
                MouseButton::Left,
            ));
            h.on_pointer(PointerEvent::single(PointerKind::Up, p, MouseButton::Left));
        };
        let on_btn = Point::new(30, 20);
        let blank = Point::new(150, 180);

        click(&mut handler, on_btn);
        let focused = handler.focus.current;
        assert!(focused.is_some(), "点按钮应获得焦点");

        // 焦点控件内部的按下不该清（命中节点在其祖先链上）。
        click(&mut handler, on_btn);
        assert_eq!(handler.focus.current, focused, "重复点同一控件应保持焦点");

        // 移动不参与裁决：只有按下才重新裁定焦点归属。
        handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            blank,
            MouseButton::Left,
        ));
        assert_eq!(handler.focus.current, focused, "指针移出不应清焦点");

        click(&mut handler, blank);
        assert!(handler.focus.current.is_none(), "点空白应清空焦点");
    }

    /// 对话框弹出时焦点应进入框内、关闭后还给来处（同 `<dialog>.showModal()`）。
    /// 此前焦点留在后方按钮上，Tab 还能一路走到遮罩后面去。
    #[test]
    fn modal_open_moves_focus_into_dialog_and_restores_on_close() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let show = crate::signal::signal(false);
        let (open, close) = (show, show);
        let app = App::new("t", 300, 200).content(
            Element::stack()
                .fill()
                .child(
                    Element::col()
                        .padding(10)
                        .child(Element::button("打开").on_click(move |_| open.set(true))),
                )
                .child(Element::dialog(
                    show,
                    Element::col().child(
                        Element::button("确定")
                            .width(80)
                            .on_click(move |_| close.set(false)),
                    ),
                )),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        macro_rules! frame {
            () => {
                handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200))
            };
        }
        frame!();

        // 点开按钮：焦点落到它身上，同时请求弹出对话框。
        let at = Point::new(40, 25);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        let outside = handler.focus.current;
        assert!(outside.is_some(), "点按钮应先聚焦到它");

        frame!();
        assert_eq!(
            handler.focus.current,
            handler.focus.order.first().copied(),
            "对话框弹出后焦点应自动落到框内首个可聚焦控件"
        );
        assert_ne!(
            handler.focus.current, outside,
            "焦点不该留在遮罩后面的按钮上"
        );
        assert!(!handler.focus.visible, "鼠标点开的对话框不该凭空冒出焦点框");

        // 点框内「确定」关闭对话框。
        let inside = handler.focus.current.unwrap();
        let b = handler.tree.abs_bounds(inside);
        let at = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        frame!();
        assert_eq!(
            handler.focus.current, outside,
            "关闭后焦点应还给弹出前那个控件"
        );
    }

    /// Tab 走到滚动区外的控件时应把它滚进视口。断言的是「焦点控件可见」这个目标
    /// 本身，而不是 scroll_y 的具体数值。
    #[test]
    fn tab_scrolls_focus_into_view() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let mut col = Element::col();
        for i in 0..8 {
            col = col.child(Element::button(format!("B{i}")).height(40));
        }
        let app = App::new("t", 200, 100).content(
            Element::col()
                .fill()
                .child(Element::scroll().height(100).child(col)),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(200, 100).unwrap();
        macro_rules! frame {
            () => {
                handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 100))
            };
        }
        frame!();
        assert_eq!(handler.focus.order.len(), 8, "8 个按钮都在焦点环里");

        let k = key_ev();
        // Tab 到最后一项（视口只装得下前两个半）。
        for _ in 0..8 {
            handler.on_key(k(Key::Tab));
        }
        frame!(); // 重排应用新的 scroll_y
        let f = handler.focus.current.expect("应有焦点");
        assert_eq!(f, handler.focus.order[7], "应停在最后一项");
        let b = handler.tree.abs_bounds(f);
        assert!(
            b.y >= 0 && b.bottom() <= 100,
            "焦点控件应被滚进视口，实际 y={} bottom={}",
            b.y,
            b.bottom()
        );
    }

    /// 焦点环只跟随键盘：同一个对话框，鼠标点开不显示、键盘打开显示。
    /// 判据是「用户最近一次交互用的什么」，而不是「焦点这次是不是框架挪的」。
    #[test]
    fn focus_ring_follows_keyboard_not_mouse() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;

        // 每次从头搭一份：show 是构建期捕获的，两种打开方式不能共用同一棵树。
        let build = || {
            let show = crate::signal::signal(false);
            let open = show;
            let app = App::new("t", 300, 200).content(
                Element::stack()
                    .fill()
                    .child(
                        Element::col()
                            .padding(10)
                            .child(Element::button("打开").on_click(move |_| open.set(true))),
                    )
                    .child(Element::dialog(
                        show,
                        Element::col().child(Element::button("确定").width(80)),
                    )),
            );
            let mut h = app.into_handler_for_test();
            h.set_scale(1.0);
            h
        };
        let mut pm = Pixmap::new(300, 200).unwrap();

        // 鼠标路径：点按钮开框。
        let mut h = build();
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        let at = Point::new(40, 25);
        h.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        h.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        assert!(
            h.focus.current.is_some(),
            "焦点仍应移进对话框（只是不画环）"
        );
        assert!(!h.focus.visible, "纯鼠标操作全程不应出现焦点框");

        // 键盘路径：Tab 到按钮、空格激活。
        let mut h = build();
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        let k = key_ev();
        h.on_key(k(Key::Tab));
        assert!(h.focus.visible, "Tab 导航应打开焦点环");
        h.on_key(k(Key::Space));
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        assert!(h.focus.current.is_some(), "空格应激活按钮并弹出对话框");
        assert!(h.focus.visible, "键盘打开的对话框应保留焦点环");
    }

    /// P0-1 回归：`autofocus()` 应在首帧把焦点交给声明的节点，且按键真的到得了它。
    ///
    /// 没有它时错在哪：`focus == None`，而 `Tree::dispatch_key` 的目标是
    /// `Option<NodeId>`——为 `None` 时**整个按键事件被丢弃**，不是打错地方而是消失。
    /// `start_hidden()` 的常驻工具因此在热键唤起后第一次按键完全无响应。
    #[test]
    fn autofocus_gives_first_keypress_a_home() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let text = crate::signal::signal(String::new());
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(Element::button("先于输入框出现").height(30))
                .child(Element::text_input(text, "查词…").height(30).autofocus()),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));

        let focused = handler.focus.current.expect("首帧后应已有焦点");
        assert_eq!(
            focused, handler.focus.order[1],
            "焦点应落在声明了 autofocus 的输入框上，而不是焦点环首项（那是按钮）"
        );
        assert!(
            !handler.focus.visible,
            "程序性移交不该点亮焦点环（判据是用户最近一次交互用的什么设备）"
        );

        // 关键：按键真的到得了它。这是 autofocus 存在的全部意义。
        let k = crate::app::test_support::key_ev();
        handler.on_key(k(Key::Char('好')));
        assert_eq!(text.get(), "好", "第一次按键应落进输入框");
    }

    /// `autofocus` 只兑现一次：用户把焦点点走之后不该被抢回来。
    ///
    /// 没有这条约束时错在哪：若每帧见到 `focus == None` 就兑现，点空白清焦点会在下一帧
    /// 立刻被撤销——「焦点粘死在输入框上」，且与已修好的「点空白清焦点」语义直接冲突。
    #[test]
    fn autofocus_is_one_shot_and_does_not_steal_focus_back() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let text = crate::signal::signal(String::new());
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(Element::text_input(text, "查词…").height(30).autofocus())
                .child(Element::flex_spacer()),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        macro_rules! frame {
            () => {
                handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200))
            };
        }
        frame!();
        assert!(handler.focus.current.is_some(), "首帧应兑现 autofocus");

        // 点空白清焦点。
        let blank = Point::new(150, 180);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            blank,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Up,
            blank,
            MouseButton::Left,
        ));
        assert!(handler.focus.current.is_none(), "点空白应清空焦点");
        frame!();
        assert!(
            handler.focus.current.is_none(),
            "autofocus 不该把焦点抢回来——它只兑现一次"
        );
    }

    /// `autofocus_select_all()` 应聚焦并全选已有内容（查询框语义：直接覆盖打字）。
    ///
    /// 没有它时错在哪：`TextInput::select_all` 是私有的，应用层无从调用；唤起后上次
    /// 查的词还在框里，用户必须先手动删掉才能查下一个。
    #[test]
    fn autofocus_select_all_lets_next_keypress_replace_old_text() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let text = crate::signal::signal("上次查的词".to_string());
        let app = App::new("t", 300, 200).content(
            Element::col().padding(10).child(
                Element::text_input(text, "查词…")
                    .height(30)
                    .autofocus_select_all(),
            ),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        assert!(handler.focus.current.is_some(), "应已聚焦");

        // 全选态下打一个字 → 整段被替换。断言的是"覆盖打字"这个目标本身，
        // 而不是选区的内部字段（那是实现细节）。
        let k = crate::app::test_support::key_ev();
        handler.on_key(k(Key::Char('新')));
        assert_eq!(
            text.get(),
            "新",
            "全选后打字应覆盖旧内容，而不是追加成「上次查的词新」"
        );
    }

    /// `select_range(0, 主名长度)` + `autofocus()`：重命名框只选主名，打字替换主名、
    /// 扩展名原样保留（资源管理器 / TC 的 F2 语义）。
    #[test]
    fn select_range_replaces_only_stem_on_typing() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let text = crate::signal::signal("README.md".to_string());
        let app = App::new("t", 300, 200).content(
            Element::col().padding(10).child(
                Element::text_input(text, "")
                    .height(30)
                    .autofocus()
                    .select_range(0, 6),
            ),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        assert!(handler.focus.current.is_some(), "应已聚焦");
        let k = crate::app::test_support::key_ev();
        handler.on_key(k(Key::Char('x')));
        assert_eq!(
            text.get(),
            "x.md",
            "预置选区应在首帧之后仍然有效，打字只替换主名"
        );
    }

    /// 回归：`autofocus_take` 的"每次出现都夺取"不能被那个**全局**一次性标志挡住。
    ///
    /// 没有按节点标志时错在哪：`focus.autofocus_done` 一经置位就再不复位（只有窗口
    /// 隐藏→唤起才 rearm），于是窗口生命周期内**第二个**要求自动聚焦的控件永远拿不到
    /// 焦点。就地编辑框因此第一次 F2 好使、第二次起光标在框里闪却打不进字——最难查的
    /// 那种：同一段代码时灵时不灵。
    ///
    /// 这里直接钉住 `first_autofocus` 的两条判据（跳过已兑现的、夺取档优先），
    /// 端到端那一半在下游真机上验。
    #[test]
    fn first_autofocus_skips_fulfilled_nodes_and_prefers_take() {
        use crate::core::{Autofocus, Tree};
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::text_input(crate::signal::signal(String::new()), "a").autofocus())
            .child(Element::text_input(crate::signal::signal(String::new()), "b").autofocus_take())
            .build(&mut tree);
        tree.root = Some(root);
        let order = tree.focusable_order();
        assert_eq!(order.len(), 2, "两个输入框都该在焦点环里");

        // 夺取档优先，哪怕它排在后面
        let (id, mode) = tree.first_autofocus(&order).expect("应有待兑现的");
        assert_eq!(id, order[1]);
        assert_eq!(mode, Autofocus::Take);

        // 兑现之后不再返回它，轮到前面那个兜底档
        tree.mark_autofocus_done(id);
        let (id2, mode2) = tree.first_autofocus(&order).expect("还剩一个");
        assert_eq!(id2, order[0]);
        assert_eq!(mode2, Autofocus::Focus);

        // 两个都兑现完就没有了——不会每帧把焦点粘回去
        tree.mark_autofocus_done(id2);
        assert!(tree.first_autofocus(&order).is_none());

        // 窗口重新唤起时一起复位
        tree.clear_autofocus_done();
        assert!(tree.first_autofocus(&order).is_some());
    }

    /// `autofocus_take()`：**即使别处已有焦点也夺过来**。
    ///
    /// 没有它时错在哪：`autofocus` 家族对已有焦点主动让位（那是"没人要焦点时给个归宿"
    /// 的兜底语义）。但就地编辑框是被按键唤出来的，唤出它的那个控件此刻**正持有焦点**，
    /// 于是让位 → 输入框看得见光标却一个字也打不进去，键入继续落到原控件上。
    #[test]
    fn autofocus_take_steals_focus_from_the_control_that_summoned_it() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let show = crate::signal::signal(false);
        let text = crate::signal::signal(String::new());
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(Element::button("先点我").height(30))
                .child(
                    // 与就地编辑同一形状：靠信号整块出现的输入框
                    Element::host_signal(show.map(|on| if *on { vec![1u8] } else { vec![] }), {
                        move |_| Element::text_input(text, "").height(30).autofocus_take()
                    }),
                ),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        let mut frame =
            |h: &mut UiHost| h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        frame(&mut handler);

        // 前置：先让按钮拿到焦点（模拟"用户正在面板上操作"）
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        let at = crate::geometry::Point::new(40, 25);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        let button = handler.focus.current.expect("前置：按钮应已聚焦");

        // 唤出输入框：它必须把焦点抢过来，否则打字会落回按钮
        show.set(true);
        frame(&mut handler);
        let now = handler.focus.current.expect("输入框应已聚焦");
        assert_ne!(now, button, "autofocus_take 应从按钮手里夺走焦点");

        let k = crate::app::test_support::key_ev();
        handler.on_key(k(Key::Char('x')));
        assert_eq!(text.get(), "x", "键入应落进输入框");
    }

    /// 被模态遮住时不兑现：否则键盘能打到遮罩后面看不见的输入框里。
    ///
    /// 没有这条过滤时错在哪：`autofocus` 若全树扫描而不与 `focusable_order` 取交集，
    /// 对话框弹着的那一帧就会把焦点送给遮罩后方的节点。
    #[test]
    fn autofocus_waits_while_covered_by_modal() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let text = crate::signal::signal(String::new());
        let show = crate::signal::signal(true);
        let app = App::new("t", 300, 200).content(
            Element::stack()
                .fill()
                .child(
                    Element::col()
                        .padding(10)
                        .child(Element::text_input(text, "查词…").height(30).autofocus()),
                )
                .child(Element::dialog(
                    show,
                    Element::col().child(Element::button("确定").width(80)),
                )),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        macro_rules! frame {
            () => {
                handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200))
            };
        }
        frame!();
        let inside = handler.focus.current.expect("焦点应在对话框内");
        assert!(
            handler.focus.order.contains(&inside),
            "焦点应落在对话框作用域内（模态移交），而非遮罩后的输入框"
        );
        let k = crate::app::test_support::key_ev();
        handler.on_key(k(Key::Char('x')));
        assert_eq!(text.get(), "", "按键不该打到遮罩后面的输入框里");

        // 关掉对话框：输入框这时才进焦点环，autofocus 兑现。
        show.set(false);
        frame!();
        handler.on_key(k(Key::Char('好')));
        assert_eq!(text.get(), "好", "对话框关闭后 autofocus 才兑现");
    }

    /// **Tab 焦点导航是兜底**：控件放过 Tab 时照旧移动焦点；控件消费掉则不动。
    ///
    /// 没有这两条时各错在哪：
    /// - 少了"放过就导航"，把 Tab 从抢先改成兜底会**静默废掉整个焦点导航**——所有没声明
    ///   `on_nav_key` 的界面都不能再用 Tab 走，而这一条不会有任何编译错误提示。
    /// - 少了"消费则不动"，输入框把 Tab 用作接受补全时焦点会同时跳走，补全刚接受完
    ///   光标就不在框里了。
    #[test]
    fn tab_navigation_is_a_fallback_after_the_focused_control() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let plain = crate::signal::signal(String::new());
        let greedy = crate::signal::signal(String::new());
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                // 第 0 项：普通输入框，不声明 on_nav_key → Tab 应照旧导航。
                .child(Element::text_input(plain, "a").height(30))
                // 第 1 项：吞掉 Tab 的输入框。
                .child(
                    Element::text_input(greedy, "b")
                        .height(30)
                        .on_nav_key(|_, ev| ev.key == crate::event::Key::Tab && !ev.shift),
                )
                .child(Element::button("尾项").height(30)),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        let k = crate::app::test_support::key_ev();

        // 无焦点 → Tab 落到首项（原有行为，不能因改成兜底而丢）。
        handler.on_key(k(Key::Tab));
        assert_eq!(
            handler.focus.current,
            Some(handler.focus.order[0]),
            "Tab 应落到焦点环首项"
        );
        assert!(handler.focus.visible, "Tab 导航应点亮焦点环");

        // 首项是普通输入框，放过 Tab → 继续导航到第 1 项。
        handler.on_key(k(Key::Tab));
        assert_eq!(
            handler.focus.current,
            Some(handler.focus.order[1]),
            "控件放过 Tab 时焦点导航应照旧发生"
        );

        // 第 1 项吞掉 Tab → 焦点不动。
        handler.on_key(k(Key::Tab));
        assert_eq!(
            handler.focus.current,
            Some(handler.focus.order[1]),
            "控件消费了 Tab，宿主不该再移动焦点"
        );

        // Shift+Tab 被该控件放过 → 反向导航回首项，用户始终有键盘退路。
        let shift_tab = crate::event::KeyEvent {
            key: Key::Tab,
            pressed: true,
            shift: true,
            ctrl: false,
            alt: false,
            meta: false,
        };
        handler.on_key(shift_tab);
        assert_eq!(
            handler.focus.current,
            Some(handler.focus.order[0]),
            "放过的 Shift+Tab 应反向导航——这是吞 Tab 的控件留给用户的退路"
        );
    }

    /// 唤起后 `autofocus` 应**重新兑现一次**——这是常驻工具第二次唤起时的关键。
    ///
    /// 没有它时错在哪：`autofocus` 平时是一次性的（兑现后焦点归用户）。于是进程起来后的
    /// 第一次唤起是好的，**第二次**唤起时焦点还停在上次离开的地方，`autofocus_select_all`
    /// 的"全选"也不会再发生——上次查的词还在框里、光标在末尾，用户得先手动删。
    /// 这条恰恰只在真机反复唤起时才暴露，单看第一次唤起一切正常。
    #[test]
    fn window_shown_rearms_autofocus_for_the_next_wake() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let text = crate::signal::signal(String::from("上次查的词"));
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(
                    Element::text_input(text, "查词…")
                        .height(30)
                        .autofocus_select_all(),
                )
                .child(Element::flex_spacer()),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        macro_rules! frame {
            () => {
                handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200))
            };
        }
        frame!();
        assert!(handler.focus.current.is_some(), "首帧应兑现 autofocus");

        // 模拟用户点走焦点后把窗口收起（常驻工具的常态）。
        let blank = Point::new(150, 180);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            blank,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Up,
            blank,
            MouseButton::Left,
        ));
        frame!();
        assert!(
            handler.focus.current.is_none(),
            "前提：点空白清焦点，且 autofocus 不该抢回来"
        );

        // 再次唤起：平台在隐藏→可见的跃迁上通知宿主。
        assert!(handler.on_window_shown(), "唤起应请求重绘");
        frame!();
        assert_eq!(
            handler.focus.current,
            Some(handler.focus.order[0]),
            "唤起后焦点应重新落回查询框"
        );

        // 全选也应重新生效：打字覆盖旧内容，而不是追加。
        let k = crate::app::test_support::key_ev();
        handler.on_key(k(Key::Char('新')));
        assert_eq!(
            text.get(),
            "新",
            "唤起后应重新全选，打字覆盖旧词——否则用户每次都得先手动删"
        );
    }

    /// 唤起时焦点**停在另一个控件上**（不是没有焦点）也要收回来。
    ///
    /// 与上一条的分野是承重的：那条测的是「焦点已被清空」，此时 `honor_autofocus`
    /// 本就会接手；而常驻词典的真实情形是焦点**有主**——用户上次 Tab 到了列表、
    /// 或点了某个按钮就把窗口收了。`honor_autofocus` 有一步「不抢已有焦点」，于是
    /// 重新 arm 之后当场让位：焦点留在原处，`autofocus_select_all` 的全选也不再发生。
    ///
    /// 修法是 `rearm_autofocus` 一并把焦点让出来。这条测试钉住的就是那几行——它们
    /// 看起来像是多余的（标志已经清了），删掉之后本测试立刻红。
    #[test]
    fn window_shown_takes_focus_back_from_another_control() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let text = crate::signal::signal(String::from("上次查的词"));
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(
                    Element::text_input(text, "查词…")
                        .height(30)
                        .autofocus_select_all(),
                )
                .child(Element::button("别处").height(30)),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        macro_rules! frame {
            () => {
                handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200))
            };
        }
        frame!();
        assert!(handler.focus.current.is_some(), "首帧应兑现 autofocus");

        // Tab 到按钮上：焦点**有主**，这正是与上一条测试的分野。
        let k = crate::app::test_support::key_ev();
        handler.on_key(k(Key::Tab));
        frame!();
        assert_eq!(
            handler.focus.current,
            handler.focus.order.get(1).copied(),
            "前提：焦点已经交给了按钮"
        );

        // 收起再唤起。
        assert!(handler.on_window_shown(), "唤起应请求重绘");
        frame!();
        assert_eq!(
            handler.focus.current,
            handler.focus.order.first().copied(),
            "唤起后焦点必须收回查询框，而不是留在用户上次离开的那个控件上"
        );

        handler.on_key(k(Key::Char('新')));
        assert_eq!(
            text.get(),
            "新",
            "收回焦点还不够，全选也要跟着重新兑现——否则打字是追加不是覆盖"
        );
    }

    /// 应用处理了快捷键 → 宿主必须请求重绘。
    ///
    /// 缺了这一步的症状很具体：按下快捷键界面纹丝不动，晃一下鼠标才刷出来——因为要
    /// 等别的事件顺带触发一次重绘。而 `focus_main_input()` 那条路径自己置了标志，
    /// 于是「Ctrl+L 正常、Esc 切页没反应」，这个不对称最难查。
    #[test]
    fn handled_shortcut_requests_a_repaint() {
        use crate::platform::AppHandler;
        let hit = crate::signal::signal(false);
        let app = App::new("t", 200, 100)
            .on_shortcut(move |_ctx, ev| {
                if ev.key == Key::Escape {
                    // 典型的应用侧动作：改一个驱动 `visible_when` 的信号。
                    hit.set(true);
                    return true;
                }
                false
            })
            .content(Element::col().child(Element::button("b").height(30)));
        let mut handler = app.into_handler_for_test();
        let k = crate::app::test_support::key_ev();
        assert!(
            handler.on_key(k(Key::Escape)),
            "应用要了这个键就必须请求重绘，否则界面要等下一个事件才更新"
        );
        assert!(hit.get(), "前提：回调确实跑了");
    }

    /// `App::on_show` 回调应在唤起时触发，且它请求的副作用要落地。
    ///
    /// 没有它时错在哪：常驻工具"每次唤起刷新一次数据 / 清掉上次结果"没有落点——三个
    /// 唤起入口里只有控件请求经过宿主，托盘与热键都是平台层直接执行的，应用侧根本感知
    /// 不到自己被唤起了。
    #[test]
    fn show_handler_runs_and_its_effects_land() {
        use crate::platform::AppHandler;
        let hits = crate::signal::signal(0u32);
        let app = App::new("t", 120, 90)
            .on_show(move |ctx| {
                hits.update(|n| *n += 1);
                ctx.toast_ok("已唤起");
            })
            .content(Element::col());
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);

        handler.on_window_shown();
        assert_eq!(hits.get(), 1, "唤起应触发 on_show");
        assert!(
            handler.toast.is_active(),
            "回调请求的 toast 应被宿主收下，而不是随 ctx 一起丢掉"
        );

        handler.on_window_shown();
        assert_eq!(hits.get(), 2, "每次唤起都触发；回调可重复调用（FnMut）");
    }

    /// `App::on_window_activated` 只在激活态**真正变化**时触发，参数是新的激活态，
    /// 且它请求的副作用要落地。
    ///
    /// 没有它时错在哪：设置类程序想「切回窗口就刷新」没有落点——窗口一直可见、只是被
    /// Alt+Tab 切走再切回，`on_show`（只管隐藏→可见）不会触发，应用侧根本感知不到。
    #[test]
    fn activate_handler_fires_on_real_transitions_only_and_effects_land() {
        use crate::platform::AppHandler;
        let hits = crate::signal::signal(0u32);
        let last = crate::signal::signal(None::<bool>);
        let app = App::new("t", 120, 90)
            .on_window_activated(move |ctx, active| {
                hits.update(|n| *n += 1);
                last.set(Some(active));
                ctx.toast_ok("激活态变了");
            })
            .content(Element::col());
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);

        assert!(handler.on_window_activated(false));
        assert_eq!(
            (hits.get(), last.get()),
            (1, Some(false)),
            "失活应触发，参数为 false"
        );
        assert!(
            handler.toast.is_active(),
            "回调请求的 toast 应被宿主收下，而不是随 ctx 一起丢掉"
        );

        handler.on_window_activated(false);
        assert_eq!(hits.get(), 1, "同值重复通知不应再触发（平台会重复通知）");

        handler.on_window_activated(true);
        assert_eq!(
            (hits.get(), last.get()),
            (2, Some(true)),
            "重新激活应触发，参数为 true"
        );
    }

    /// 窗口刚建出来按「已激活」计：平台随后通知「激活」是同值，不该触发——首次加载要靠应用自己。
    #[test]
    fn activate_handler_does_not_fire_for_the_initial_state() {
        use crate::platform::AppHandler;
        let hits = crate::signal::signal(0u32);
        let app = App::new("t", 120, 90)
            .on_window_activated(move |_ctx, _active| hits.update(|n| *n += 1))
            .content(Element::col());
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        handler.on_window_activated(true);
        assert_eq!(hits.get(), 0, "初始就是激活态，同值通知不触发");
    }

    /// 没登记回调时激活态变化照旧（光标静止等既有行为不受影响）。
    #[test]
    fn window_activation_without_a_handler_keeps_working() {
        use crate::platform::AppHandler;
        let app = App::new("t", 120, 90).content(Element::col());
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        assert!(handler.on_window_activated(false), "激活态变化仍要求重绘");
        assert!(!handler.on_window_activated(false));
    }

    /// `on_commit` 测试台：两个输入框，A 挂 on_commit（记次数 + 记回调里读到的正文），
    /// B 不挂且**不设宽**（Wrap 宽路径，见 AGENTS §5）；A 可经 `vis` 隐藏。
    struct CommitBench {
        h: UiHost,
        pm: tiny_skia::Pixmap,
        a_text: crate::signal::Signal<String>,
        vis: crate::signal::Signal<bool>,
        en: crate::signal::Signal<bool>,
        /// 置真时 A 的提交回调把焦点要回自己（"校验失败留在原框"）。
        reject: crate::signal::Signal<bool>,
        commits: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }

    const A_AT: (i32, i32) = (40, 16);
    const B_AT: (i32, i32) = (20, 48);
    const BLANK: (i32, i32) = (250, 180);
    const HIDE_BTN: (i32, i32) = (20, 80);
    const TOOL_BTN: (i32, i32) = (20, 112);
    const RICH: (i32, i32) = (20, 144);

    impl CommitBench {
        fn new() -> Self {
            use crate::platform::AppHandler;
            let a_text = crate::signal::signal(String::new());
            let b_text = crate::signal::signal(String::new());
            let vis = crate::signal::signal(true);
            let en = crate::signal::signal(true);
            let reject = crate::signal::signal(false);
            let commits = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            let log = commits.clone();
            let app = App::new("t", 300, 200).content(
                Element::col()
                    .width(300)
                    .height(200)
                    .child(
                        Element::text_input(a_text, "甲")
                            .width(180)
                            .height(32)
                            .visible_signal(vis)
                            .enabled_signal(en)
                            .on_commit(move |ctx| {
                                log.borrow_mut().push(a_text.get());
                                if reject.get() {
                                    ctx.request_focus();
                                }
                            }),
                    )
                    .child(Element::text_input(b_text, "乙").height(32))
                    // 一次点击里"隐藏编辑框 + 焦点给自己"：dispatch_blur 那道可见性检查守的就是它。
                    .child(
                        Element::button("藏")
                            .height(32)
                            .on_click(move |_| vis.set(false)),
                    )
                    // 能要焦点却不在 Tab 环里的两类节点：点它们同样是用户离开了输入框。
                    .child(Element::button("工具").height(32).focusable(false))
                    .child(
                        Element::rich(crate::ui::RichDoc::new().para("一段纯文本预览"))
                            .width(200)
                            .height(32),
                    ),
            );
            let mut h = app.into_handler_for_test();
            h.set_scale(1.0);
            let mut b = Self {
                h,
                pm: tiny_skia::Pixmap::new(300, 200).unwrap(),
                a_text,
                vis,
                en,
                reject,
                commits,
            };
            b.frame();
            b
        }
        fn frame(&mut self) {
            use crate::platform::AppHandler;
            use crate::render::PixmapTarget;
            self.h.render(
                &mut PixmapTarget {
                    pixmap: &mut self.pm,
                },
                Size::new(300, 200),
            );
        }
        fn click(&mut self, (x, y): (i32, i32)) {
            use crate::event::{MouseButton, PointerEvent, PointerKind};
            use crate::platform::AppHandler;
            let p = crate::geometry::Point::new(x, y);
            self.h.on_pointer(PointerEvent::single(
                PointerKind::Down,
                p,
                MouseButton::Left,
            ));
            self.h
                .on_pointer(PointerEvent::single(PointerKind::Up, p, MouseButton::Left));
        }
        fn key(&mut self, k: Key) {
            use crate::platform::AppHandler;
            self.h.on_key(key_ev()(k));
        }
        fn commits(&self) -> Vec<String> {
            self.commits.borrow().clone()
        }
    }

    /// 改完点别处：同一次分发内提交一次，回调读到的是最终正文（不差一拍）。
    #[test]
    fn commit_fires_on_click_away_with_final_text() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.key(Key::Char('x'));
        b.key(Key::Char('y'));
        b.click(B_AT);
        // 不出帧即断言：要求"同一次分发"，不能等下一帧。
        assert_eq!(b.commits(), vec!["xy".to_string()]);
    }

    /// Enter 提交后紧跟失焦：只提交一次（下游被"改名跑两遍"坑过）。
    #[test]
    fn enter_then_blur_commits_once() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.key(Key::Char('x'));
        b.key(Key::Enter);
        assert_eq!(b.commits().len(), 1, "Enter 应提交");
        b.click(B_AT);
        assert_eq!(b.commits().len(), 1, "Enter 已提交过，紧跟的失焦不该再提交");
        // 再改一次后失焦：又算新的改动。
        b.click(A_AT);
        b.key(Key::End); // 点击把光标放在落点（正文开头），移到末尾再续写
        b.key(Key::Char('z'));
        b.click(B_AT);
        assert_eq!(b.commits(), vec!["x".to_string(), "xz".to_string()]);
    }

    /// 没动一个字地点进点出不提交；外部写信号也不算改动。
    #[test]
    fn untouched_or_externally_written_text_does_not_commit() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.click(B_AT);
        b.a_text.set("外部写入".into());
        b.frame();
        b.click(A_AT);
        b.click(B_AT);
        assert!(b.commits().is_empty(), "未编辑不该提交：{:?}", b.commits());
    }

    /// Tab 离开与点空白同样是用户失焦。
    #[test]
    fn commit_fires_on_tab_and_on_blank_click() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.key(Key::Char('x'));
        b.key(Key::Tab);
        assert_eq!(b.commits().len(), 1, "Tab 离开应提交");

        b.click(A_AT);
        b.key(Key::Char('y'));
        b.click(BLANK);
        assert_eq!(b.commits().len(), 2, "点空白应提交");
    }

    /// 被隐藏丢焦点不是"用户点了别处"：不提交，且未提交的改动随之作废——
    /// 再显示后没改动地点进点出，也不该把上一轮的改动补交出去。
    #[test]
    fn hiding_does_not_commit_and_discards_pending_edit() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.key(Key::Char('x'));
        b.vis.set(false);
        b.frame();
        b.frame();
        assert!(b.commits().is_empty(), "隐藏不该提交：{:?}", b.commits());

        b.vis.set(true);
        b.frame();
        b.click(A_AT);
        b.click(B_AT);
        assert!(
            b.commits().is_empty(),
            "隐藏前的改动应已作废：{:?}",
            b.commits()
        );
    }

    /// 点一个"会隐藏输入框"的按钮：**提交**。焦点在按钮 Down 时就转走了，那一刻
    /// 输入框仍可见，是用户离开了它；隐藏发生在随后 Up 的 on_click 里。与网页一致
    /// （mousedown 先 blur/change，click 才跑）。"取消编辑"该用 Escape 之类不经
    /// 失焦的路径，而不是指望点击隐藏能吞掉提交。
    #[test]
    fn clicking_a_button_that_hides_the_input_still_commits() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.key(Key::Char('x'));
        b.click(HIDE_BTN);
        assert!(!b.vis.get(), "前提：按钮应已隐藏输入框");
        assert_eq!(b.commits(), vec!["x".to_string()]);
    }

    /// 改了又改回原样不算改动（网页 change 同此）：误敲一个字再删掉，不该触发
    /// "同名冲突 → 自动加后缀"之类的提交逻辑。
    #[test]
    fn edit_reverted_to_original_does_not_commit() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.key(Key::Char('x'));
        b.key(Key::Backspace);
        b.click(B_AT);
        assert!(
            b.commits().is_empty(),
            "改回原样不该提交：{:?}",
            b.commits()
        );
    }

    /// 被禁用带走焦点：不提交，且改动作废（禁用翻转与隐藏一样调 `reset_interaction`）
    /// ——重新启用后没改动地点进点出，不得把旧改动补交出去（届时正文可能已被外部写成
    /// 服务端的值）。
    #[test]
    fn disabling_discards_pending_edit() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.key(Key::Char('x'));
        b.en.set(false);
        b.frame();
        b.frame();
        b.a_text.set("服务端的值".into());
        b.en.set(true);
        b.frame();
        b.click(A_AT);
        b.click(B_AT);
        assert!(
            b.commits().is_empty(),
            "禁用前的改动应已作废：{:?}",
            b.commits()
        );
    }

    /// 提交回调把焦点要回自己（校验失败留在原框）：Tab 引发的这条连锁要沿用键盘来源，
    /// 焦点环不能被冲成鼠标态；焦点最终留在 A。
    #[test]
    fn commit_callback_refocusing_keeps_keyboard_focus_ring() {
        let mut b = CommitBench::new();
        b.reject.set(true);
        b.click(A_AT);
        b.key(Key::Char('x'));
        let a = b.h.focus.current;
        b.key(Key::Tab);
        assert_eq!(b.commits().len(), 1, "Tab 离开应提交");
        assert_eq!(b.h.focus.current, a, "回调要回焦点后应留在 A");
        assert!(b.h.focus.visible, "键盘连锁不该把焦点环冲成鼠标态");
    }

    /// 提交只是旁路通知，不改变 Enter 的消费：多行框改过字后按 Ctrl+Enter，既提交，
    /// 也照常上达 `App::on_shortcut`（"发布"快捷键）。挂了自动保存就吞掉发布键，是
    /// 用户看不出原因的失灵。
    #[test]
    fn ctrl_enter_commits_and_still_reaches_shortcut() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        let body = crate::signal::signal(String::new());
        let commits = std::rc::Rc::new(std::cell::Cell::new(0));
        let shortcuts = std::rc::Rc::new(std::cell::Cell::new(0));
        let (c, sc) = (commits.clone(), shortcuts.clone());
        let app = App::new("t", 300, 200)
            .on_shortcut(move |_, k| {
                let hit = k.ctrl && k.key == Key::Enter;
                if hit {
                    sc.set(sc.get() + 1);
                }
                hit
            })
            .content(
                Element::col().width(300).height(200).child(
                    Element::text_input(body, "正文")
                        .multiline()
                        .width(280)
                        .height(120)
                        .autofocus()
                        .on_commit(move |_| c.set(c.get() + 1)),
                ),
            );
        let mut h = app.into_handler_for_test();
        h.set_scale(1.0);
        let mut pm = tiny_skia::Pixmap::new(300, 200).unwrap();
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        let k = key_ev();
        h.on_key(k(Key::Char('x')));
        let mut ctrl_enter = k(Key::Enter);
        ctrl_enter.ctrl = true;
        h.on_key(ctrl_enter);
        assert_eq!(commits.get(), 1, "Ctrl+Enter 应提交");
        assert_eq!(shortcuts.get(), 1, "提交不该吞掉 Ctrl+Enter 快捷键");
    }

    /// 点能要焦点却不在 Tab 环里的节点（`.focusable(false)` 的按钮、纯文本 RichText）
    /// 同样是用户离开了输入框，必须提交。Tab 环 ≠ 能拿焦点的节点：借环判"去向正不正常"
    /// 会把这类点击引起的提交静默吞掉，改动一直挂着。
    #[test]
    fn clicking_focus_taking_nodes_outside_tab_ring_commits() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.key(Key::Char('x'));
        let a = b.h.focus.current;
        b.click(TOOL_BTN);
        assert_ne!(b.h.focus.current, a, "前提：焦点应已离开 A");
        assert_eq!(
            b.commits(),
            vec!["x".to_string()],
            "点 focusable(false) 按钮应提交"
        );

        b.click(A_AT);
        b.key(Key::End);
        b.key(Key::Char('y'));
        b.click(RICH);
        assert_ne!(b.h.focus.current, a, "前提：焦点应已离开 A");
        assert_eq!(b.commits().len(), 2, "点纯文本 RichText 应提交");
    }

    /// App 级回调（定时器）把焦点要走不是用户离开：不提交。
    #[test]
    fn app_callback_taking_focus_does_not_commit() {
        let mut b = CommitBench::new();
        b.click(A_AT);
        b.key(Key::Char('x'));
        let root = b.h.tree.root;
        let res =
            b.h.tree
                .run_detached(root.unwrap(), |ctx| ctx.request_focus());
        b.h.apply_app_effects(res);
        assert_ne!(b.h.focus.current, None, "前提：焦点应已被要到根节点");
        assert!(
            b.commits().is_empty(),
            "App 级回调要焦点不该提交：{:?}",
            b.commits()
        );
    }
}
