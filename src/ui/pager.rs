//! 分页操作栏（`Element::pager` 内部使用）。
//!
//! 条目总数 + 当前页 + 首/上/下/末页 + 跳转框。是一个**独立控件**，不绑定任何表格——
//! 它只读写两个信号（当前页、条目总数）并在翻页时回调一次，谁来渲染那一页是调用方的事。
//!
//! 与 §6.7 的虚拟滚动是两套并列的 UX，不是替代关系：
//!
//! - **翻页**：一次只有一页数据在手，`table_sortable_server` + 本控件。用户知道自己在
//!   第几页、总共多少页，能直接跳到第 137 页。
//! - **虚拟滚动**：一条长长的滚动，`table_virtual_server`。没有"页"的概念，滚动条就是
//!   进度指示。
//!
//! 别把两者叠在一起用：虚拟滚动的表格上再挂一个页码栏，"当前页"没有对应的视觉锚点，
//! 滚动一下页码就变，用户会以为自己点错了。

use std::cell::RefCell;
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::signal::Signal;

/// 翻页意图回调：`(ctx, 新页码)`，页码 **0 基**。
pub(super) type OnPage = Rc<RefCell<dyn FnMut(&mut EventCtx, usize)>>;

/// 总页数。
///
/// **至少 1 页**：0 条数据也显示"第 1 / 1 页"。返回 0 的话界面上会出现"第 1 / 0 页"，
/// 而且末页按钮要跳到第 −1 页。
pub fn page_count(total_items: usize, page_size: usize) -> usize {
    let size = page_size.max(1);
    total_items.div_ceil(size).max(1)
}

/// 跳到某页（钳到有效范围）并回调应用。目标页与当前页相同时**什么都不做**——
/// 信号一写就是一次重绘请求，点已经禁用不掉的边界按钮不该让界面动一下。
pub(super) fn goto(
    page: Signal<usize>,
    total: Signal<usize>,
    page_size: usize,
    cb: &OnPage,
    ctx: &mut EventCtx,
    target: i64,
) {
    let pages = page_count(total.get(), page_size) as i64;
    let next = target.clamp(0, pages - 1) as usize;
    if next == page.get() {
        return;
    }
    page.set(next);
    (cb.borrow_mut())(ctx, next);
}

/// 把跳转框里的文本解析成 **0 基**页码。
///
/// 框里填的是**人类页码**（1 基）：用户照着旁边"第 3 / 42 页"输入，两处必须一致。
/// 空、非数字、以及 0 都算无效——第 0 页在界面上不存在，把它当成第 1 页会让"跳到 0"
/// 与"跳到 1"落到同一处，用户以为自己输错了。
pub(super) fn parse_jump(text: &str) -> Option<usize> {
    let n = text.trim().parse::<usize>().ok()?;
    (n >= 1).then(|| n - 1)
}

/// 跳转框提交：解析失败就**原样留着不动**——把用户打错的字悄悄清掉，他就不知道错在哪了。
pub(super) fn jump_to(
    page: Signal<usize>,
    total: Signal<usize>,
    page_size: usize,
    jump: Signal<String>,
    cb: &OnPage,
    ctx: &mut EventCtx,
) {
    let Some(target) = jump.with(|s| parse_jump(s)) else {
        return;
    };
    goto(page, total, page_size, cb, ctx, target as i64);
    jump.set(String::new());
}

/// 值真的变了才写：`Signal::set` 无条件推版本并请求重绘，写回同一个字符串等于
/// 平白多画一帧。
fn set_if_changed(sig: &Signal<String>, value: String) {
    if sig.with(|cur| *cur != value) {
        sig.set(value);
    }
}

/// 页码栏的响应式部分：当前页或总条目数变化时刷新两处文案，并把越界的页码钳回来。
///
/// 之所以要个 widget 而不是把文案做成派生信号：那两句文案各依赖**两个**信号
/// （页码与总数），而 `Signal::map` 只跟一个源——总数变了而页码没变时，派生值不会更新。
pub(super) struct PagerBar {
    page: Signal<usize>,
    total: Signal<usize>,
    page_size: usize,
    count_text: Signal<String>,
    page_text: Signal<String>,
    on_page: OnPage,
    /// 上次处理过的 `(页码版本, 总数版本)`。相同即跳过——**一个信号都不能写**，
    /// 否则每帧一次重绘请求，「空闲零 CPU」当场作废。
    last: Option<(u64, u64)>,
}

impl PagerBar {
    pub(super) fn new(
        page: Signal<usize>,
        total: Signal<usize>,
        page_size: usize,
        count_text: Signal<String>,
        page_text: Signal<String>,
        on_page: OnPage,
    ) -> Self {
        Self {
            page,
            total,
            page_size: page_size.max(1),
            count_text,
            page_text,
            on_page,
            last: None,
        }
    }
}

impl Widget for PagerBar {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let ver = (self.page.version(), self.total.version());
        if self.last == Some(ver) {
            return;
        }

        let total = self.total.get();
        let pages = page_count(total, self.page_size);
        let mut cur = self.page.get();
        if cur >= pages {
            // 换了筛选条件或删了数据之后，当前页可能已经不存在了。停在那儿的表现是
            // "第 21 / 3 页"配一张空表，而且怎么点都回不来——就地钳到末页并让应用重取。
            cur = pages - 1;
            self.page.set(cur);
            (self.on_page.borrow_mut())(ctx, cur);
        }
        // 钳过之后再记版本。只是省掉下一帧一次白跑的 on_update——不记也不会反复触发回调
        // （下一帧 `cur < pages` 已经成立），故这一行是省事，不是纠错。
        self.last = Some((self.page.version(), self.total.version()));

        set_if_changed(&self.count_text, format!("共 {total} 条"));
        set_if_changed(&self.page_text, format!("第 {} / {} 页", cur + 1, pages));
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{NodeId, Tree};
    use crate::event::{Key, KeyEvent, MouseButton, PointerEvent, PointerKind};
    use crate::geometry::{Point, Size};
    use crate::signal::signal;
    use crate::ui::Element;

    const SIZE: usize = 10;

    /// 每次翻页回调收到的页码——"应用真的被通知去取那一页了吗"只能从这里看出来，
    /// 光看 `page` 信号的值看不出回调有没有发。
    type Fired = Rc<RefCell<Vec<usize>>>;

    /// 页码栏里第 n 个子元素的下标。
    const FIRST: usize = 2;
    const PREV: usize = 3;
    const NEXT: usize = 5;
    const LAST: usize = 6;
    const INPUT: usize = 8;
    const GO: usize = 9;

    fn setup(total_items: usize) -> (Tree, NodeId, Signal<usize>, Signal<usize>, Fired) {
        let page = signal(0usize);
        let total = signal(total_items);
        let fired: Fired = Rc::new(RefCell::new(Vec::new()));
        let rec = fired.clone();
        let root = Element::col().width(700).height(60).child(Element::pager(
            page,
            total,
            SIZE,
            move |_ctx, p| rec.borrow_mut().push(p),
        ));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(700, 60), &mut te);
        let bar = tree.get(id).unwrap().children[0];
        (tree, bar, page, total, fired)
    }

    fn relayout(tree: &mut Tree) {
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(700, 60), &mut te);
    }

    fn part(tree: &Tree, bar: NodeId, n: usize) -> NodeId {
        tree.get(bar).unwrap().children[n]
    }

    /// 点页码栏里的第 n 个子元素（先取 id 再点，避免同时借 tree 的可变与不可变）。
    fn click_part(tree: &mut Tree, bar: NodeId, n: usize) {
        let id = part(tree, bar, n);
        click(tree, id);
    }

    fn click(tree: &mut Tree, id: NodeId) {
        let b = tree.abs_bounds(id);
        let at = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut h, mut c) = (None, None);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
            &mut h,
            &mut c,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut h,
            &mut c,
        );
    }

    fn type_key(tree: &mut Tree, target: NodeId, key: Key) {
        tree.dispatch_key(
            KeyEvent {
                key,
                pressed: true,
                shift: false,
                ctrl: false,
            },
            Some(target),
        );
    }

    #[test]
    fn page_count_never_returns_zero() {
        // 返回 0 的话界面上会出现"第 1 / 0 页"，末页按钮还要跳到第 −1 页。
        assert_eq!(page_count(0, 10), 1, "0 条也是一页（空的那一页）");
        assert_eq!(page_count(1, 10), 1);
        assert_eq!(page_count(10, 10), 1, "整除时不该多出一空页");
        assert_eq!(page_count(11, 10), 2);
        assert_eq!(page_count(1_234, 12), 103);
        assert_eq!(page_count(5, 0), 5, "每页 0 条按 1 条兜底，别除零");
    }

    #[test]
    fn parse_jump_reads_human_page_numbers() {
        // 框里填的是 1 基页码（照着旁边"第 3 / 42 页"输），回调收的是 0 基。
        assert_eq!(parse_jump("1"), Some(0));
        assert_eq!(parse_jump(" 42 "), Some(41));
        assert_eq!(parse_jump("0"), None, "第 0 页在界面上不存在");
        assert_eq!(parse_jump(""), None);
        assert_eq!(parse_jump("abc"), None);
        assert_eq!(parse_jump("-3"), None);
        assert_eq!(parse_jump("3.5"), None);
    }

    #[test]
    fn nav_buttons_move_and_report_the_new_page() {
        let (mut tree, bar, page, _t, fired) = setup(1_000); // 100 页
        click_part(&mut tree, bar, NEXT);
        assert_eq!(page.get(), 1);
        click_part(&mut tree, bar, NEXT);
        click_part(&mut tree, bar, PREV);
        assert_eq!(page.get(), 1);
        click_part(&mut tree, bar, LAST);
        assert_eq!(page.get(), 99, "末页应是第 100 页（0 基 99）");
        click_part(&mut tree, bar, FIRST);
        assert_eq!(page.get(), 0);
        assert_eq!(
            *fired.borrow(),
            vec![1, 2, 1, 99, 0],
            "每次翻页都要通知应用去取那一页，且页码是 0 基"
        );
    }

    /// 某个子元素当前是否可用（`own_enabled` 会现场求值 `enabled_when` 的闭包）。
    fn is_enabled(tree: &Tree, bar: NodeId, n: usize) -> bool {
        tree.get(part(tree, bar, n)).unwrap().own_enabled()
    }

    #[test]
    fn boundary_buttons_are_disabled_and_dead() {
        // 两件事都要验，缺一不可：
        // - **看得出**：边界上的按钮要置灰，否则用户会一直点一个没反应的东西。
        // - **点不动**：置灰只是视觉，真正拦事件的是核心层。
        //
        // 只验后者是不够的——`goto` 里还有一道"目标页与当前页相同就什么都不做"的兜底，
        // 于是把禁用条件整个删掉，行为上照样"点了没反应"，测试全绿而按钮却亮着。
        let (mut tree, bar, page, _t, fired) = setup(1_000);
        assert!(!is_enabled(&tree, bar, FIRST), "第一页上「首页」应置灰");
        assert!(!is_enabled(&tree, bar, PREV), "第一页上「上一页」应置灰");
        assert!(is_enabled(&tree, bar, NEXT), "还有下一页时不该置灰");
        assert!(is_enabled(&tree, bar, LAST));

        click_part(&mut tree, bar, FIRST);
        click_part(&mut tree, bar, PREV);
        assert_eq!(page.get(), 0);
        assert!(
            fired.borrow().is_empty(),
            "第一页上点首页/上一页不该翻页，更不该白发一次取数请求"
        );

        click_part(&mut tree, bar, LAST);
        relayout(&mut tree);
        assert!(!is_enabled(&tree, bar, NEXT), "末页上「下一页」应置灰");
        assert!(!is_enabled(&tree, bar, LAST), "末页上「末页」应置灰");
        assert!(is_enabled(&tree, bar, PREV), "末页上「上一页」应可用");

        fired.borrow_mut().clear();
        click_part(&mut tree, bar, NEXT);
        click_part(&mut tree, bar, LAST);
        assert_eq!(page.get(), 99);
        assert!(
            fired.borrow().is_empty(),
            "末页上点下一页/末页同样不该再发请求"
        );
    }

    #[test]
    fn shrinking_the_total_clamps_the_page_and_refetches_once() {
        // 换筛选条件之后条目变少，当前页可能已经不存在。停在那儿的表现是"第 100 / 4 页"
        // 配一张空表，而且怎么点都回不来。
        let (mut tree, bar, page, total, fired) = setup(1_000);
        click_part(&mut tree, bar, LAST);
        assert_eq!(page.get(), 99);
        fired.borrow_mut().clear();

        total.set(35); // 4 页
        relayout(&mut tree);
        assert_eq!(page.get(), 3, "越界的页码应钳到末页");
        assert_eq!(*fired.borrow(), vec![3], "钳回来之后要让应用重取那一页");

        for _ in 0..3 {
            relayout(&mut tree);
        }
        assert_eq!(
            *fired.borrow(),
            vec![3],
            "钳一次就该稳住；反复触发等于每帧一次取数请求"
        );
    }

    #[test]
    fn jump_box_takes_a_human_page_number_and_clears_on_success() {
        let (mut tree, bar, page, _t, fired) = setup(1_000);
        let input = part(&tree, bar, INPUT);
        click(&mut tree, input);
        for ch in "37".chars() {
            type_key(&mut tree, input, Key::Char(ch));
        }
        click_part(&mut tree, bar, GO);
        assert_eq!(page.get(), 36, "框里填 37 应跳到 0 基的第 36 页");
        assert_eq!(*fired.borrow(), vec![36]);

        // 成功之后框应清空：它是一次性输入，留着旧页码下次点"跳转"会莫名其妙又跳一次。
        fired.borrow_mut().clear();
        click_part(&mut tree, bar, GO);
        assert!(fired.borrow().is_empty(), "框已清空，再点跳转不该重复跳");

        // 跳到**当前页**也不该发请求：页码没变，重取一遍纯属白跑一次后端。
        for ch in "37".chars() {
            type_key(&mut tree, input, Key::Char(ch));
        }
        click_part(&mut tree, bar, GO);
        assert_eq!(page.get(), 36);
        assert!(fired.borrow().is_empty(), "跳到当前页不该重新取数");
    }

    #[test]
    fn jump_box_clamps_and_ignores_garbage() {
        let (mut tree, bar, page, _t, fired) = setup(1_000);
        let input = part(&tree, bar, INPUT);
        click(&mut tree, input);
        for ch in "abc".chars() {
            type_key(&mut tree, input, Key::Char(ch));
        }
        click_part(&mut tree, bar, GO);
        assert_eq!(page.get(), 0, "非数字应原地不动");
        assert!(fired.borrow().is_empty(), "非数字不该发请求");

        // 打错的字**不会被清掉**（清了用户就不知道自己错在哪）。这里顺带证明它还在：
        // 再补一个数字，整串仍然解析失败。
        type_key(&mut tree, input, Key::Char('1'));
        click_part(&mut tree, bar, GO);
        assert_eq!(page.get(), 0, "\"abc1\" 仍不是数字");

        for _ in 0..4 {
            type_key(&mut tree, input, Key::Backspace);
        }
        for ch in "99999".chars() {
            type_key(&mut tree, input, Key::Char(ch));
        }
        click_part(&mut tree, bar, GO);
        assert_eq!(page.get(), 99, "超出范围应钳到末页，而不是跳进空页");
    }

    #[test]
    fn enter_in_the_jump_box_submits() {
        // 输入框里按回车是最自然的手势；只认"跳转"按钮的话，用户会以为自己输的没生效。
        let (mut tree, bar, page, _t, _f) = setup(1_000);
        let input = part(&tree, bar, INPUT);
        click(&mut tree, input);
        for ch in "5".chars() {
            type_key(&mut tree, input, Key::Char(ch));
        }
        type_key(&mut tree, input, Key::Enter);
        assert_eq!(page.get(), 4, "回车应与点「跳转」等效");
    }

    #[test]
    fn idle_frames_write_no_signals() {
        // 「空闲零 CPU」：信号一写就是一次重绘请求。页码栏每帧都要看一眼页码与总数，
        // 看一眼绝不能变成写一笔。
        let (mut tree, bar, _p, _t, _f) = setup(1_000);
        click_part(&mut tree, bar, NEXT);
        for _ in 0..3 {
            relayout(&mut tree);
        }
        crate::anim::reset_request();
        for _ in 0..5 {
            relayout(&mut tree);
        }
        assert!(
            !crate::anim::animation_requested(),
            "稳态下页码栏不该再写任何信号"
        );
    }
}
