//! 虚拟滚动列表 widget（`Element::virtual_list` / `Element::table_virtual` 的内部驱动）。
//!
//! 常规列表（[`dyn_list::DynList`](super::dyn_list::DynList)、
//! [`SortableBody`](super::sortable_table)）把**每一行**都建成真实节点：行数一多，
//! 建树、每帧 measure/arrange、每帧 paint 三条 O(N) 成本一条都躲不掉，且滚动一格就要
//! 全付一遍（`ScrollWidget` 滚动即 `mark_layout_dirty`）。实测 3 列表格在 release 下
//! 10000 行的稳态重排是 47ms/帧——已经不是"有点卡"，是滚不动。
//!
//! `VirtualList` 只构建**视口内**的行，两端各插一个空占位节点撑出被略过那部分的高度：
//!
//! ```text
//! scroll ┌─────────────────┐
//!        │ spacer  first×h │ ← 上方未渲染行的总高
//!        ├─────────────────┤
//!        │ row  first      │ ┐
//!        │ …               │ ├ 视口内 + 上下 overscan，真实节点
//!        │ row  last-1     │ ┘
//!        ├─────────────────┤
//!        │ spacer (n-last)×h │ ← 下方未渲染行的总高
//!        └─────────────────┘
//! ```
//!
//! 占位节点撑高这一招的价值在于**核心层零改动**：`Tree::measure_scroll` 照常把占位节点的
//! 高度计进 `content_h`，于是滚动钳制、滚动条滑块的高度与位置、`scroll_into_view` 全部
//! 自动正确——没有任何一处需要知道"这个列表是虚拟的"。
//!
//! 代价是**行高必须固定**：索引与像素偏移之间靠一次乘法互换。行高由构建器参数给定，且
//! 行元素会被强制设成该高度（见 `rebuild`）——即便调用方传的值与行内容的自然高度不符，
//! 占位高度与实际布局也**永远**一致，不会出现滚动条与内容错位这种最难查的故障。
//!
//! # 已知限制
//!
//! - **变高行不支持**。行高由调用方给定并被强制施加，内容更高会溢出到邻行。表格的行内
//!   控件（`actions`）与多行单元格（`cell_lines`）照常可用，但 `row_height` 得自己按内容
//!   调大——`TABLE_ROW_H` 只够放单行文本。
//! - **Tab 焦点环只覆盖已渲染的行**。`Tree::focusable_order` 遍历真实节点，未渲染的行
//!   不在环里，`scroll_into_view` 也够不着它们。行内放可聚焦控件时键盘用户会在列表边界
//!   "掉出去"——需要全键盘可达的场景暂不适用。
//! - **行的内部状态每次重建都会重置**。hover、补间动画这类挂在行 widget 里的 `Cell`
//!   状态，滚动跨行时会丢。选中态等必须由外部 `Signal` 承载（`Element::list` 的
//!   `Signal<usize>` 选中模型天然满足）。
//! - **与拖拽重排互斥**。[`ReorderList`](super::reorder::ReorderList) 依赖所有行同时
//!   存在并写 `offset`/`raised`。

use std::rc::Rc;

use crate::core::{EventCtx, Layout, NodeId, Tree, Widget};
use crate::signal::{Signal, SignalScope};
use crate::ui::Element;

/// [`Element::table_virtual`](super::Element::table_virtual) 的推荐行高：与非虚拟表格的
/// 单行高逐像素一致（单元格 20px 单行盒 + 上下内边距 + 行下分隔线 1px）。
///
/// 只有内边距是与 `table_cell_pad_lines` 共享的常量，单元格 20px 的单行盒与行下分隔线的
/// 1px 在这里是**第二份**字面量——那边改了这边不会自动跟上。两者不分叉靠的是
/// `table_row_height_matches_the_non_virtual_table` 那条测试（它拿真实布局对账），
/// 不是靠这个表达式。
pub const TABLE_ROW_H: i32 = 20 + 2 * super::TABLE_CELL_PAD_Y + 1;

/// 视口上下各多渲染的行数。
///
/// 不为零是因为滚动偏移不必是行高的整数倍，且 `Node::over_scroll`（撞界回弹）会让内容
/// 整体临时位移——两者都会让视口边缘露出"下一行"。多画几行远比露白便宜。
const OVERSCAN: usize = 4;

// 渲染多少行由**本帧窗口高**决定，而不是滚动容器上一帧的实测视口高。
//
// 实测值看着更精确，却有两个方向的失真，各自对应一个可见故障：
//
// - **偏小**：`on_update` 跑在 `measure` 之前，读到的是上一帧的 `bounds`。窗口最大化、
//   侧栏收起这类让视口骤然变大的操作，本帧按旧视口算出的行数不够铺满新视口，底部留白；
//   而 resize 只触发一次布局，那个"下一帧"根本不会来（除非用户又动了鼠标）。
// - **偏大**：把列表放进无限高的父容器（页面级 `scroll` 里不设高度），滚动容器的高度会
//   收敛成**整个内容高**，于是"视口"等于全表——十万行全部建出来，静默卡死。
//
// 窗口高没有这两个毛病：`layout_root` 入口就记下了本帧的值（`Tree::layout_size`），且
// 视口再大也不可能超过窗口。代价是视口远小于窗口时会多渲染几行——一个 2000px 的窗口里
// 摆着 200px 高的表格，多建约 (2000−200)/行高 行，几十微秒的事，换掉上面两个可见故障。

/// 视口窗口的计算与「占位撑高」的重建骨架，三种虚拟滚动正文共用
/// （[`VirtualList`]、`VirtualTableBody`、`VirtualServerBody`）。
///
/// 抽出来是因为这两件事**必须只有一份**：占位高度按 `first × row_h` / `(n − last) × row_h`
/// 算，而行的位置由同一个 `first` 决定——两处一旦分头演化，滚动条与内容就会渐行渐远，
/// 而且是那种滚很远才看得出来的偏移。
pub(super) struct VirtualWindow {
    row_h: i32,
    /// 上次构建的 `(首行, 末行(不含), 数据版本)`。三者全同才跳过重建——只比区间的话，
    /// 原地改数据（`Signal::set` 同长度新 Vec）就刷不出来了。
    last: Option<(usize, usize, u64)>,
}

impl VirtualWindow {
    pub(super) fn new(row_h: i32) -> Self {
        Self {
            // 行高必须为正：0 或负会让 `scroll_y / row_h` 除零或算出负索引。
            row_h: row_h.max(1),
            last: None,
        }
    }

    /// 作废上次构建结果，令下一帧无条件重建。
    ///
    /// 给的是"行怎么建"变了的场合（表格的操作列/自定义单元格/行回调是构建期设进来的，
    /// 见 [`VirtualTableBody`](super::sortable_table::VirtualTableBody)）——区间和数据版本
    /// 都没动，不作废的话新配置要等到下次滚动才生效。
    pub(super) fn invalidate(&mut self) {
        self.last = None;
    }

    /// 当前应渲染的行区间 `[first, last)`。
    fn range(&self, scroll_y: i32, view_h: i32, n: usize) -> (usize, usize) {
        let max_scroll = (n as i32 * self.row_h - view_h).max(0);
        let scroll_y = scroll_y.clamp(0, max_scroll);
        let first = (scroll_y / self.row_h) as usize;
        let first = first.saturating_sub(OVERSCAN).min(n);
        // 视口能露出的行数：向上取整，再 +1 补上顶部被切掉半行时末尾多出来的那一行。
        let span = (view_h + self.row_h - 1) / self.row_h + 1;
        let last = first
            .saturating_add(span as usize)
            .saturating_add(OVERSCAN * 2)
            .min(n);
        (first, last)
    }

    /// 本帧该渲染的区间；与上次完全相同（区间与数据版本都没变）时返回 `None`，
    /// 调用方据此跳过重建。
    pub(super) fn poll(
        &mut self,
        ctx: &mut EventCtx,
        n: usize,
        ver: u64,
    ) -> Option<(usize, usize)> {
        let self_id = ctx.id();
        let tree = ctx.tree_mut();
        // 滚动量取自最近的滚动祖先，是事件刚写进去的**当前**值；渲染量则按本帧窗口高算
        // （理由见文件上方那段注释）。两者时序不同，但都不滞后。
        let scroll_y = scroll_offset(tree, self_id);
        let view_h = tree.layout_size.h.max(1);
        let (first, last) = self.range(scroll_y, view_h, n);
        if self.last == Some((first, last, ver)) {
            return None;
        }
        self.last = Some((first, last, ver));
        Some((first, last))
    }

    /// 按区间重建子节点：`[占位, 行…, 占位]`。`row(i)` 按**真实行下标**产出行元素。
    pub(super) fn rebuild(
        &self,
        tree: &mut Tree,
        self_id: NodeId,
        first: usize,
        last: usize,
        n: usize,
        signals: &mut SignalScope,
        mut row: impl FnMut(usize) -> Element,
    ) {
        let before = first as i32 * self.row_h;
        let after = (n.saturating_sub(last)) as i32 * self.row_h;
        let row_h = self.row_h;

        let mut sc = std::mem::take(signals);
        clear_children(tree, self_id, &mut sc);
        sc.collect(|| {
            push(tree, self_id, spacer(before));
            for i in first..last {
                // 强制行高：占位高度按 `row_h` 算，行的实际高度就必须是 `row_h`，
                // 否则内容会与滚动条渐行渐远。
                push(tree, self_id, row(i).width_match().height(row_h));
            }
            push(tree, self_id, spacer(after));
        });
        *signals = sc;
    }
}

/// 虚拟滚动正文：挂在滚动容器**内部的列容器**上，按当前滚动量重建可见行。
///
/// 刻意不挂在滚动节点自己身上：`Element::scroll()` 自带的
/// [`ScrollWidget`](super::containers::ScrollWidget) 才是滚轮与滚动条拖拽的实现，
/// 换掉它列表就滚不动了。这与 `SortableBody` 的挂法是同一条理由。
pub(super) struct VirtualList<T: Clone + 'static> {
    data: Signal<Vec<T>>,
    row_fn: Rc<dyn Fn(usize, T) -> Element>,
    win: VirtualWindow,
    /// 当前这批行在构建期创建的信号，重建时整批回收（同 `DynList`）。
    rows: SignalScope,
}

impl<T: Clone + 'static> VirtualList<T> {
    pub(super) fn new(
        data: Signal<Vec<T>>,
        row_h: i32,
        row_fn: impl Fn(usize, T) -> Element + 'static,
    ) -> Self {
        Self {
            data,
            row_fn: Rc::new(row_fn),
            win: VirtualWindow::new(row_h),
            rows: SignalScope::new(),
        }
    }
}

impl<T: Clone + 'static> Widget for VirtualList<T> {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let ver = self.data.version();
        let n = self.data.with(Vec::len);
        let Some((first, mut last)) = self.win.poll(ctx, n, ver) else {
            return;
        };
        // 只克隆可见的那几十行。`Signal::with` 借用期间不得回调用户代码——`row_fn` 里若
        // 写回同一个信号会撞上 RefCell 双借用，故先取出、放掉借用，再构建。
        let visible: Vec<T> = self
            .data
            .with(|v| v[first.min(v.len())..last.min(v.len())].to_vec());
        // 数据比 `n` 报的还短时按实到长度收窄，行数与占位高度因此始终对得上。
        last = first + visible.len();

        let self_id = ctx.id();
        let row_fn = self.row_fn.clone();
        let mut signals = std::mem::take(&mut self.rows);
        self.win
            .rebuild(ctx.tree_mut(), self_id, first, last, n, &mut signals, |i| {
                row_fn(i, visible[i - first].clone())
            });
        self.rows = signals;
    }
}

/// 空占位节点：只有高度，不画任何东西。
fn spacer(h: i32) -> Element {
    Element::leaf().width_match().height(h.max(0))
}

fn push(tree: &mut Tree, parent: NodeId, el: Element) {
    let id = el.build(tree);
    tree.add_child(parent, id);
}

/// 清空某节点的全部子节点（递归释放子树 arena slot），并同刻回收这批子树在**构建期**
/// 创建的信号。
///
/// 两件事绑在一个函数里是有意的：所有按数据/排序/滚动整批重建行的宿主（本模块的
/// [`VirtualList`]，以及 `sortable_table` 的表头 / 正文 / 分页正文 / 可选正文）都要求
/// 节点与其构建期信号同生共死——只删节点会漏槽位，只回收信号会让还挂着的节点读到已死的信号。
pub(super) fn clear_children(tree: &mut Tree, id: NodeId, signals: &mut SignalScope) {
    let old: Vec<_> = tree.get(id).map(|n| n.children.clone()).unwrap_or_default();
    for c in old {
        tree.remove(c);
    }
    if let Some(n) = tree.get_mut(id) {
        n.children.clear();
    }
    signals.dispose();
}

/// 最近的滚动祖先的滚动量；没有滚动祖先时返回 0。
///
/// 用祖先而非自身，是因为本 widget 挂在滚动容器**内部**的列上（见 [`VirtualList`] 的
/// 说明）。这个值不滞后：滚轮/拖拽/触摸惯性都是在事件里直接写 `scroll_y`，而本函数在
/// 随后那一帧的响应式相位读它。**只读滚动量，不读视口高**——后者在这个时点是上一帧的
/// 值，会两头失真（见文件上方那段注释）。
fn scroll_offset(tree: &Tree, id: NodeId) -> i32 {
    let mut cur = tree.get(id).and_then(|n| n.parent);
    while let Some(p) = cur {
        let Some(node) = tree.get(p) else { break };
        if matches!(node.layout, Layout::Scroll) {
            return node.scroll_y;
        }
        cur = node.parent;
    }
    0
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use crate::core::{NodeId, Tree};
    use crate::event::{MouseButton, PointerEvent, PointerKind};
    use crate::geometry::{Point, Size};
    use crate::signal::{signal, Signal};
    use crate::ui::row_source::RowSource;
    use crate::ui::{Element, SortKey, SortOrder};

    const ROW_H: i32 = 40;
    const VIEW_H: i32 = 200;

    /// 记录 `row_fn` 每次被调用时收到的索引——"哪些行真的被构建了"只能从这里看出来，
    /// 数节点个数看不出行的身份。
    type Seen = Rc<RefCell<Vec<usize>>>;

    /// 造一棵 `[限高容器 → virtual_list]` 的树并布局一次。
    ///
    /// **只布局一次**是有意的：渲染量按本帧窗口高算，首帧就该正确。多布局一帧会掩盖
    /// "第一帧渲染不足"这类故障——这条测试基建本身就踩过（见 `viewport_growth_*`）。
    fn setup(n: usize, view_h: i32) -> (Tree, NodeId, Seen, Signal<Vec<usize>>) {
        let data = signal((0..n).collect::<Vec<usize>>());
        let seen: Seen = Rc::new(RefCell::new(Vec::new()));
        let rec = seen.clone();
        let root = Element::col().width(300).height(view_h).child(
            Element::virtual_list(data, ROW_H, move |i, _v: usize| {
                rec.borrow_mut().push(i);
                Element::leaf().width_match()
            })
            .fill(),
        );
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(300, view_h), &mut te);
        seen.borrow_mut().clear(); // 只关心稳态之后的重建
        (tree, id, seen, data)
    }

    fn relayout(tree: &mut Tree, view_h: i32) {
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(300, view_h), &mut te);
    }

    /// 滚动容器节点（root → `virtual_list` 返回的 scroll）。
    fn scroll_of(tree: &Tree, root: NodeId) -> NodeId {
        tree.get(root).unwrap().children[0]
    }

    fn wheel(d: i32) -> PointerEvent {
        PointerEvent::single(
            PointerKind::Wheel(d),
            Point::new(150, 100),
            MouseButton::Left,
        )
    }

    /// 树里的节点总数——虚拟化的全部意义就是让它与行数脱钩。
    fn node_count(tree: &Tree, id: NodeId) -> usize {
        let Some(n) = tree.get(id) else { return 0 };
        1 + n
            .children
            .iter()
            .map(|&c| node_count(tree, c))
            .sum::<usize>()
    }

    #[test]
    fn builds_only_visible_rows_regardless_of_length() {
        let (tree, root, _seen, _data) = setup(100_000, VIEW_H);
        // 视口 200 / 行高 40 = 5 行可见，加两端 overscan，几十个节点封顶。
        let n = node_count(&tree, root);
        assert!(n < 40, "10 万行只应建视口内那几行，实际节点数 {n}");
    }

    #[test]
    fn spacers_make_content_height_exact() {
        // 占位节点撑高是整个设计的地基：content_h 对了，滚动钳制、滚动条滑块尺寸与
        // 位置、scroll_into_view 才全部自动正确——核心层一行都不用改。
        let (mut tree, root, _seen, _data) = setup(1_000, VIEW_H);
        let sc = scroll_of(&tree, root);
        assert_eq!(
            tree.get(sc).unwrap().content_h,
            1_000 * ROW_H,
            "内容总高应等于 行数 × 行高，与非虚拟列表无从区分"
        );
        // 必须滚开再验一次：停在顶部时上方占位天然为 0，那种状态下"忘了撑高上方"
        // 与"撑对了"完全同形——只测初始态等于没测。
        let (mut h, mut cap) = (None, None);
        for _ in 0..50 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        relayout(&mut tree, VIEW_H);
        assert_eq!(
            tree.get(sc).unwrap().content_h,
            1_000 * ROW_H,
            "滚到中段后内容总高不应变化（上下占位之和必须补齐未渲染的部分）"
        );
    }

    #[test]
    fn wheel_scroll_moves_the_rendered_window() {
        let (mut tree, root, seen, _data) = setup(10_000, VIEW_H);
        let (mut h, mut cap) = (None, None);
        // 滚 40 格：每格 48px（ScrollWidget 的 120→48 换算），共 1920px = 48 行。
        for _ in 0..40 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        relayout(&mut tree, VIEW_H);

        let sc = scroll_of(&tree, root);
        let scroll_y = tree.get(sc).unwrap().scroll_y;
        assert_eq!(scroll_y, 40 * 48, "滚轮应累加滚动量");

        let rows = seen.borrow().clone();
        let first_visible = (scroll_y / ROW_H) as usize;
        assert!(
            rows.contains(&first_visible),
            "视口首行 {first_visible} 应被构建，实际构建了 {:?}",
            &rows[..rows.len().min(8)]
        );
        assert!(
            rows.iter().all(|&i| i > 20),
            "滚过 48 行后不该再构建顶部的行，实际最小索引 {:?}",
            rows.iter().min()
        );
    }

    #[test]
    fn rendered_rows_land_where_the_spacers_promise() {
        // 最容易悄悄坏掉的一条：占位高度按 row_h 算，行的真实高度若与之不符，内容会与
        // 滚动条渐行渐远。这里直接拿绝对几何对账。
        let (mut tree, root, _seen, _data) = setup(10_000, VIEW_H);
        let (mut h, mut cap) = (None, None);
        for _ in 0..10 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        relayout(&mut tree, VIEW_H);

        let sc = scroll_of(&tree, root);
        let scroll_y = tree.get(sc).unwrap().scroll_y;
        let body = tree.get(sc).unwrap().children[0];
        let kids = tree.get(body).unwrap().children.clone();
        // [占位, 行…, 占位]：第二个子节点就是渲染窗口的首行。
        let first_row = kids[1];
        // 索引取自 `row_fn` 实际收到的值，**不从占位高度反推**——反推等于拿被测量
        // 自己当标尺，占位一旦算错，期望值会跟着一起错，测试永远通过。
        let first_idx = *_seen.borrow().iter().min().expect("应至少构建一行") as i32;
        let expect_y = first_idx * ROW_H - scroll_y;
        assert_eq!(
            tree.abs_bounds(first_row).y,
            expect_y,
            "首个渲染行的实际 y 必须等于 索引×行高 − 滚动量"
        );
        assert_eq!(
            tree.abs_bounds(first_row).h,
            ROW_H,
            "行高应被强制为 row_height"
        );
    }

    #[test]
    fn scrolls_to_the_very_last_row() {
        let (mut tree, root, seen, _data) = setup(1_000, VIEW_H);
        let (mut h, mut cap) = (None, None);
        for _ in 0..2_000 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        relayout(&mut tree, VIEW_H);

        let sc = scroll_of(&tree, root);
        assert_eq!(
            tree.get(sc).unwrap().scroll_y,
            1_000 * ROW_H - VIEW_H,
            "应钳制到最大滚动量"
        );
        assert!(
            seen.borrow().contains(&999),
            "滚到底应能构建最后一行，实际最大索引 {:?}",
            seen.borrow().iter().max()
        );
    }

    #[test]
    fn data_change_rebuilds_in_place() {
        let (mut tree, root, seen, data) = setup(1_000, VIEW_H);
        seen.borrow_mut().clear();
        data.set((0..20).collect()); // 行数骤减
        relayout(&mut tree, VIEW_H);

        assert!(!seen.borrow().is_empty(), "数据版本变化应触发重建");
        let sc = scroll_of(&tree, root);
        assert_eq!(
            tree.get(sc).unwrap().content_h,
            20 * ROW_H,
            "内容总高应跟随新数据"
        );
    }

    #[test]
    fn handles_empty_and_shorter_than_viewport() {
        let (tree, root, _seen, _data) = setup(0, VIEW_H);
        assert_eq!(tree.get(scroll_of(&tree, root)).unwrap().content_h, 0);

        // 行数不足一屏：不应越界取数据，稳态后也不该反复重建。
        let (tree, root, seen, _data) = setup(3, VIEW_H);
        assert_eq!(
            tree.get(scroll_of(&tree, root)).unwrap().content_h,
            3 * ROW_H
        );
        assert_eq!(*seen.borrow(), Vec::<usize>::new(), "稳态后无需重建");
    }

    #[test]
    fn viewport_growth_fills_the_new_viewport_in_the_same_frame() {
        // 窗口最大化 / 侧栏收起会让视口骤然变大。这**必须在当帧补足**——resize 只触发一次
        // 布局，没有"下一帧"可以指望（除非用户又动了鼠标）。故这里只布局一帧。
        //
        // 早先这条测试连着调两次 relayout，于是"第一帧渲染不足、第二帧才补上"照样绿；
        // 实际表现是窗口一最大化，列表下半截空白，直到你动一下鼠标才填上。
        let (mut tree, root, seen, _data) = setup(10_000, 100);
        seen.borrow_mut().clear();
        relayout(&mut tree, 900);

        // 几何对账：最后一个渲染行的底边必须盖过视口底，否则就是留白。
        let sc = scroll_of(&tree, root);
        let body = tree.get(sc).unwrap().children[0];
        let kids = tree.get(body).unwrap().children.clone();
        let last_row = kids[kids.len() - 2]; // 末尾那个是下方占位
        let bottom = tree.abs_bounds(last_row).bottom();
        assert!(
            bottom >= 900,
            "视口变高到 900 后，最后一渲染行的底边应盖过视口底，实际 {bottom}（差 {} px 留白）",
            900 - bottom
        );
        assert!(!seen.borrow().is_empty(), "应在这一帧就补建新行");
    }

    #[test]
    fn table_row_height_matches_the_non_virtual_table() {
        // 两种表格的行高一旦分叉，同一份数据换个构建器行距就变了。用真实布局对账，
        // 而不是让两个字面量各自漂移。
        let mut te = crate::text::NullTextEngine;
        let plain = Element::col().width(400).height(300).child(Element::table(
            vec![("A", 1.0), ("B", 1.0)],
            vec![vec!["x", "y"], vec!["z", "w"]],
        ));
        let mut tree = Tree::new();
        let id = plain.build(&mut tree);
        tree.root = Some(id);
        tree.layout_root(Size::new(400, 300), &mut te);
        // col[table] → table = col[header, divider, scroll] → scroll 的首个子即第一行。
        let table = tree.get(id).unwrap().children[0];
        let scroll = tree.get(table).unwrap().children[2];
        let row0 = tree.get(scroll).unwrap().children[0];
        assert_eq!(
            tree.get(row0).unwrap().bounds.h,
            super::TABLE_ROW_H,
            "TABLE_ROW_H 必须等于非虚拟表格的实际单行高"
        );
    }

    /// 建一棵 `table_virtual` 的树并布局到稳态，返回 `(树, 根, 滚动节点)`。
    fn setup_table(n: usize, view_h: i32) -> (Tree, NodeId, NodeId) {
        let rows = signal(
            (0..n)
                .map(|i| vec![format!("r{i}"), format!("{}", i * 3)])
                .collect::<Vec<_>>(),
        );
        let root = Element::col().width(400).height(view_h).child(
            Element::table_virtual(vec![("名称", 2.0), ("值", 1.0)], rows, super::TABLE_ROW_H)
                .fill(),
        );
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(400, view_h), &mut te);
        tree.layout_root(Size::new(400, view_h), &mut te);
        // col[table] → table = col[header, divider, scroll]
        let table = tree.get(id).unwrap().children[0];
        let scroll = tree.get(table).unwrap().children[2];
        (tree, id, scroll)
    }

    /// 行是否有斑马纹底色（`body_row` 给奇数显示位置铺 `Role::SurfaceAlt`）。
    fn is_striped(tree: &Tree, row: NodeId) -> bool {
        // body_row 返回 col[tr, divider]，底色在 tr 上。
        let tr = tree.get(row).unwrap().children[0];
        matches!(
            tree.get(tr).unwrap().style.bg,
            Some(crate::style::Brush::Role(crate::style::Role::SurfaceAlt))
        )
    }

    #[test]
    fn table_virtual_stripes_follow_the_real_row_index() {
        // 截图看不出的一类故障：斑马纹若按"在渲染窗口里的第几个"交替，滚动时深浅条纹
        // 会随窗口起点来回跳，肉眼是"列表在闪"，几何断言却全绿。这里钉死它按真实下标走。
        //
        // 这条自己有个盲区：整体偏移了**偶数**行时条纹奇偶不变，它看不出来。把"下标"
        // 锚到数据上的是 `table_virtual_row_callbacks_see_the_real_row_index`（拿单元格
        // 文本对账），两条合起来才完整。
        let (mut tree, _root, scroll) = setup_table(5_000, 300);
        let (mut h, mut cap) = (None, None);
        for _ in 0..7 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        relayout(&mut tree, 300);

        let body = tree.get(scroll).unwrap().children[0];
        let kids = tree.get(body).unwrap().children.clone();
        let first_idx = (tree.get(kids[0]).unwrap().bounds.h / super::TABLE_ROW_H) as usize;
        assert!(first_idx > 0, "滚动后上方应有被略过的行");
        // 取渲染窗口里相邻两行：底色必须与各自**真实下标**的奇偶一致。
        for k in 0..2 {
            let idx = first_idx + k;
            assert_eq!(
                is_striped(&tree, kids[1 + k]),
                idx % 2 == 1,
                "第 {idx} 行的斑马纹应由真实下标奇偶决定"
            );
        }
    }

    #[test]
    fn table_virtual_keeps_header_outside_the_scroll() {
        // 表头必须留在滚动容器**外面**，否则会跟着正文一起滚走。
        let (tree, root, scroll) = setup_table(1_000, 300);
        let table = tree.get(root).unwrap().children[0];
        let kids = tree.get(table).unwrap().children.clone();
        assert_eq!(
            kids.len(),
            3,
            "table_virtual 应为 col[表头, 分隔线, 滚动区]"
        );
        assert_ne!(kids[0], scroll, "表头不应在滚动区内");
        assert_eq!(
            tree.get(scroll).unwrap().content_h,
            1_000 * super::TABLE_ROW_H,
            "正文内容高应等于 行数 × TABLE_ROW_H"
        );
    }

    #[test]
    fn never_asks_for_extra_frames() {
        // 「空闲零 CPU」是本库的立身指标之一。渲染量改按本帧窗口高算之后，首帧就是对的，
        // 于是这里的契约比"只请求一次"更强：**一次都不请求**。
        //
        // 这条盯着的退化是：某天有人为了修某个几何问题在 on_update 里加回
        // `request_relayout()`，界面看起来一切正常，代价是空闲 CPU 再也回不到零——
        // 那种退化不会有任何测试或视觉表现暴露它。
        let data = signal((0..10_000usize).collect::<Vec<_>>());
        let root = Element::col().width(300).height(VIEW_H).child(
            Element::virtual_list(data, ROW_H, |_i, _v: usize| Element::leaf().width_match())
                .fill(),
        );
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;

        crate::anim::take_relayout(); // 清掉别处遗留的请求
        tree.layout_root(Size::new(300, VIEW_H), &mut te);
        assert!(!crate::anim::take_relayout(), "首帧不该请求续帧");

        for _ in 0..3 {
            tree.layout_root(Size::new(300, VIEW_H), &mut te);
        }
        let (mut h, mut cap) = (None, None);
        for _ in 0..20 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        tree.layout_root(Size::new(300, VIEW_H), &mut te);
        assert!(
            !crate::anim::take_relayout(),
            "滚动与重排都不该请求续帧（否则空闲 CPU 永不归零）"
        );

        // 视口高为 0（列表在收起的分组或未激活的页里）同样不能续帧。
        let data2 = signal((0..1_000usize).collect::<Vec<_>>());
        let hidden = Element::col().width(300).height(0).child(
            Element::virtual_list(data2, ROW_H, |_i, _v: usize| Element::leaf().width_match())
                .fill(),
        );
        let mut tree2 = Tree::new();
        let id2 = hidden.build(&mut tree2);
        tree2.root = Some(id2);
        crate::anim::take_relayout();
        for _ in 0..5 {
            tree2.layout_root(Size::new(300, 0), &mut te);
        }
        assert!(
            !crate::anim::take_relayout(),
            "视口为 0 时也不得请求续帧——每帧都请求等于把空闲 CPU 钉在满转"
        );
    }

    #[test]
    fn unbounded_height_parent_does_not_materialize_everything() {
        // 最阴的一种用错法：把列表直接丢进页面级 scroll 而不给高度。滚动容器按无限高度
        // 测量子元素，于是这个列表的"视口"会收敛成**整个内容高**——按实测视口高算渲染量
        // 的话，十万行会全部建出来，没有 panic、没有警告，就是卡死。
        //
        // 渲染量按窗口高算天然封顶：窗口只有那么高，视口不可能更大。
        let data = signal((0..50_000usize).collect::<Vec<_>>());
        let page = Element::scroll().width(300).height(600).child(
            // 注意：这里**没有** .height()/.weight()，正是那种用错法。
            Element::virtual_list(data, ROW_H, |_i, _v: usize| Element::leaf().width_match()),
        );
        let mut tree = Tree::new();
        let id = page.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(300, 600), &mut te);
        tree.layout_root(Size::new(300, 600), &mut te); // 第二帧才是原本会爆的那一帧

        let total = node_count(&tree, id);
        assert!(
            total < 100,
            "无限高父容器下也必须只建视口那几行，实际节点数 {total}"
        );
    }
    /// 同 [`setup_table`]，但允许先对 `table_virtual` 链上行内修饰符。
    fn setup_table_with(
        n: usize,
        view_h: i32,
        f: impl FnOnce(Element) -> Element,
    ) -> (Tree, NodeId, NodeId) {
        let rows = signal(
            (0..n)
                .map(|i| vec![format!("r{i}"), format!("{}", i * 3)])
                .collect::<Vec<_>>(),
        );
        let table = f(Element::table_virtual(
            vec![("名称", 2.0), ("值", 1.0)],
            rows,
            super::TABLE_ROW_H,
        ));
        let root = Element::col().width(400).height(view_h).child(table.fill());
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(400, view_h), &mut te);
        tree.layout_root(Size::new(400, view_h), &mut te);
        let table = tree.get(id).unwrap().children[0];
        let scroll = tree.get(table).unwrap().children[2];
        (tree, id, scroll)
    }

    /// 渲染窗口里第 `k` 行（`body` 的子节点是 `[占位, 行…, 占位]`）。
    fn rendered_row(tree: &Tree, scroll: NodeId, k: usize) -> NodeId {
        let body = tree.get(scroll).unwrap().children[0];
        tree.get(body).unwrap().children[1 + k]
    }

    /// 视口内第一行**完整可见**的行，连同由几何独立算出的真实下标。
    ///
    /// 两点都不是随手写的：渲染窗口的头几行是视口**上方**的 overscan，被滚动容器裁掉、
    /// 点不到；下标取自"该行相对内容顶端的偏移 ÷ 行高"（即占位撑出来的那段），与回调
    /// 自己报的下标是两条来路——把窗口内位置当行号传给回调，用它做期望值才照得出来。
    fn first_visible_row(tree: &Tree, scroll: NodeId) -> (NodeId, usize) {
        let body = tree.get(scroll).unwrap().children[0];
        let vp = tree.abs_bounds(scroll);
        let kids = tree.get(body).unwrap().children.clone();
        let row = kids[1..kids.len() - 1]
            .iter()
            .copied()
            .find(|&r| {
                let b = tree.abs_bounds(r);
                b.y >= vp.y && b.bottom() <= vp.bottom()
            })
            .expect("视口内应有完整可见的行");
        let idx = ((tree.abs_bounds(row).y - tree.abs_bounds(body).y) / super::TABLE_ROW_H).max(0);
        (row, idx as usize)
    }

    /// 某行的首个数据单元格（`body_row` 结构为 `col[tr, divider]`）。
    fn first_cell(tree: &Tree, row: NodeId) -> NodeId {
        let tr = tree.get(row).unwrap().children[0];
        tree.get(tr).unwrap().children[0]
    }

    fn center(tree: &Tree, id: NodeId) -> Point {
        let b = tree.abs_bounds(id);
        Point::new(b.x + b.w / 2, b.y + b.h / 2)
    }

    #[test]
    fn table_virtual_actions_add_a_column_to_both_header_and_rows() {
        // 操作列必须同时进表头和正文。只进正文的话列数对不上，权重分配随之错位——
        // 表头三列的边界与正文两列的边界从此各画各的。
        let (tree, root, scroll) = setup_table_with(5_000, 300, |t| {
            t.actions("操作", 1.0, |_row| Element::label("·"))
        });
        let table = tree.get(root).unwrap().children[0];
        let header = tree.get(table).unwrap().children[0];
        assert_eq!(
            tree.get(header).unwrap().children.len(),
            3,
            "表头应为 2 数据列 + 1 操作列"
        );
        let tr = tree.get(rendered_row(&tree, scroll, 0)).unwrap().children[0];
        assert_eq!(
            tree.get(tr).unwrap().children.len(),
            3,
            "正文行也应为 2 数据列 + 1 操作列"
        );
        // 逐列对齐：表头与正文的每一列左右边界必须重合。
        let hcols = tree.get(header).unwrap().children.clone();
        let bcols = tree.get(tr).unwrap().children.clone();
        for (ci, (&h, &b)) in hcols.iter().zip(bcols.iter()).enumerate() {
            let (hb, bb) = (tree.abs_bounds(h), tree.abs_bounds(b));
            assert_eq!(
                (hb.x, hb.w),
                (bb.x, bb.w),
                "第 {ci} 列的表头与正文应逐像素对齐"
            );
        }
    }

    #[test]
    fn table_virtual_row_callbacks_see_the_real_row_index() {
        // 虚拟滚动特有的错法：把"在渲染窗口里的第几个"当成行下标传给回调。首屏发现不了
        // ——窗口起点是 0，两者恰好相等；滚下去之后每个按钮都绑到错误的行上。
        //
        // 期望值取自**数据本身**（单元格文本 `r{i}`），不从渲染窗口的几何反推：后者一旦
        // 算错，期望值会跟着一起错，那就等于没测。
        let seen: Rc<RefCell<Vec<(usize, String)>>> = Rc::new(RefCell::new(Vec::new()));
        let rec = seen.clone();
        let acted: Rc<RefCell<Vec<usize>>> = Rc::new(RefCell::new(Vec::new()));
        let rec_a = acted.clone();
        let (mut tree, _root, _scroll) = setup_table_with(5_000, 300, move |t| {
            t.cell_render(move |row, col, text| {
                if col == 0 {
                    rec.borrow_mut().push((row, text.to_string()));
                }
                None
            })
            .actions("操作", 1.0, move |row| {
                rec_a.borrow_mut().push(row);
                Element::label("·")
            })
        });
        seen.borrow_mut().clear();
        acted.borrow_mut().clear();

        let (mut h, mut cap) = (None, None);
        for _ in 0..9 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        relayout(&mut tree, 300);

        let seen = seen.borrow();
        let acted = acted.borrow();
        assert!(!seen.is_empty(), "滚动后应重建出一批新行");
        assert!(seen.iter().any(|(i, _)| *i > 0), "应已滚离首行");
        for (row, text) in seen.iter() {
            assert_eq!(
                text,
                &format!("r{row}"),
                "回调说这是第 {row} 行，格里装的却是 {text} 的数据"
            );
        }
        assert_eq!(
            *acted,
            seen.iter().map(|(i, _)| *i).collect::<Vec<_>>(),
            "操作列与单元格渲染应看到同一批行下标"
        );
    }

    #[test]
    fn table_virtual_context_menu_targets_the_scrolled_row() {
        // 右击的是屏幕上那一行，菜单必须按**它的真实下标**构建。
        //
        // 期望下标由几何独立算出（行相对内容顶端的偏移 ÷ 行高，即占位撑出来的那段），
        // 与回调自己报的下标是两条来路——把窗口内位置当行号传给回调，这条就会红。
        let seen: Rc<RefCell<Option<usize>>> = Rc::new(RefCell::new(None));
        let rec = seen.clone();
        let (mut tree, _root, scroll) = setup_table_with(5_000, 300, move |t| {
            t.on_row_context_menu(move |idx| {
                *rec.borrow_mut() = Some(idx);
                vec![crate::event::MenuItem::run("删除", |_ctx| {}, false)]
            })
        });
        let (mut h, mut cap) = (None, None);
        for _ in 0..9 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        relayout(&mut tree, 300);

        let (row, expect) = first_visible_row(&tree, scroll);
        assert!(expect > 0, "滚动后渲染窗口不应还停在首行");
        let at = center(&tree, first_cell(&tree, row));
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Right),
            &mut h,
            &mut cap,
        );
        assert!(res.menu.is_some(), "右击虚拟表格的行应弹出上下文菜单");
        assert_eq!(
            *seen.borrow(),
            Some(expect),
            "菜单构建器应收到被右击那一行的真实下标"
        );
    }

    #[test]
    fn table_virtual_rebuilds_when_a_modifier_arrives_after_the_first_frame() {
        // 修饰符正常都在 build 之前链上，首帧就带着它们建行。但 `VirtualList` 只在
        // 区间或数据版本变化时重建——若哪天配置能在运行中改，少了这次作废，新的操作列
        // 得等用户滚动一下才出现，且没有任何报错提示。
        let (mut tree, _root, scroll) = setup_table_with(5_000, 300, |t| t);
        let tr = tree.get(rendered_row(&tree, scroll, 0)).unwrap().children[0];
        assert_eq!(tree.get(tr).unwrap().children.len(), 2, "起始应只有两列");

        let body = tree.get(scroll).unwrap().children[0];
        let ac =
            super::super::sortable_table::action_col("操作".into(), 1.0, |_| Element::label("·"));
        tree.get_mut(body)
            .unwrap()
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<super::super::sortable_table::VirtualTableBody>())
            .expect("table_virtual 的正文应是 VirtualTableBody")
            .set_actions(ac);
        relayout(&mut tree, 300);

        let tr = tree.get(rendered_row(&tree, scroll, 0)).unwrap().children[0];
        assert_eq!(
            tree.get(tr).unwrap().children.len(),
            3,
            "设入操作列后应当帧重建出三列，而不是等下次滚动"
        );
    }
    #[test]
    fn table_virtual_double_click_activates_the_scrolled_row() {
        // 与右键菜单同源（都挂在 HoverRow 上），但走的是另一个 setter，单独钉一条：
        // 少接一个分支的表现是"双击没反应"，没有任何报错。
        use std::cell::Cell as StdCell;
        let seen: Rc<StdCell<Option<usize>>> = Rc::new(StdCell::new(None));
        let rec = seen.clone();
        let (mut tree, _root, scroll) = setup_table_with(5_000, 300, move |t| {
            t.on_row_activate(move |_ctx, idx| rec.set(Some(idx)))
        });
        let (mut h, mut cap) = (None, None);
        for _ in 0..9 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        relayout(&mut tree, 300);

        let (row, expect) = first_visible_row(&tree, scroll);
        assert!(expect > 0, "滚动后渲染窗口不应还停在首行");
        let at = center(&tree, first_cell(&tree, row));

        tree.dispatch_pointer(
            PointerEvent {
                kind: PointerKind::Down,
                pos: at,
                button: MouseButton::Left,
                click_count: 2,
            },
            &mut h,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut h,
            &mut cap,
        );
        assert_eq!(seen.get(), Some(expect), "双击应回报被点那一行的真实下标");
    }

    #[test]
    fn table_virtual_cell_lines_reaches_the_text_cells() {
        // `cell_lines` 装的是文本格的裁切围栏（`max_lines`）。虚拟模式下行高是强制的，
        // 少了这个围栏，两行文本会直接画到下一行身上——而行高看着仍然规整，
        // 几何断言一条都不会红。
        let (mut tree, _root, scroll) = setup_table_with(1_000, 300, |t| t.cell_lines(2));
        let cell = first_cell(&tree, rendered_row(&tree, scroll, 0));
        let label = tree.get(cell).unwrap().children[0];
        let lines = tree
            .get_mut(label)
            .unwrap()
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::ui::Label>())
            .and_then(|l| l.max_lines);
        assert_eq!(lines, Some(2), "cell_lines(2) 应传到虚拟表格的文本格上");
    }
    // ---- 服务端分页（table_virtual_server / RowSource）----

    type Asked = Rc<RefCell<Vec<std::ops::Range<usize>>>>;

    /// 造一棵 `[限高容器 → table_virtual_server]` 的树。
    ///
    /// `auto_fill` 为真时在回调里**同步**回填（模拟本地库或极快的后端）；为假时只记录
    /// 请求、永不回填——那正是"数据还在路上"的状态，骨架占位与滚动条都要在这个状态下成立。
    fn setup_server(
        total: usize,
        view_h: i32,
        auto_fill: bool,
        f: impl FnOnce(Element) -> Element,
    ) -> (Tree, NodeId, RowSource, Asked) {
        let src = RowSource::new(total);
        let asked: Asked = Rc::new(RefCell::new(Vec::new()));
        let rec = asked.clone();
        let table = f(Element::table_virtual_server(
            vec![("名称", 2.0), ("值", 1.0)],
            src,
            super::TABLE_ROW_H,
            move |_ctx, req| {
                rec.borrow_mut().push(req.rows.clone());
                if auto_fill {
                    let rows = req
                        .rows
                        .clone()
                        .map(|i| vec![format!("r{i}"), format!("{}", i * 3)])
                        .collect();
                    src.fill(&req, rows);
                }
            },
        ));
        let root = Element::col().width(400).height(view_h).child(table.fill());
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(400, view_h), &mut te);
        (tree, id, src, asked)
    }

    /// 服务端表格的滚动容器（root → table → scroll）。
    fn server_scroll(tree: &Tree, root: NodeId) -> NodeId {
        let table = tree.get(root).unwrap().children[0];
        tree.get(table).unwrap().children[2]
    }

    /// 该行是不是骨架占位（首格里装的是灰条而非文本）。
    fn is_skeleton(tree: &Tree, row: NodeId) -> bool {
        let bar = tree.get(first_cell(tree, row)).unwrap().children[0];
        matches!(
            tree.get(bar).unwrap().style.bg,
            Some(crate::style::Brush::Role(crate::style::Role::Border))
        )
    }

    #[test]
    fn server_scrollbar_spans_the_dataset_before_any_row_arrives() {
        // 滚动条按**总行数**撑高，第一帧就是对的。若改成按已到货行数撑，滑块会随数据到货
        // 不停缩小、位置乱跳，滚动体验就散了。
        let (tree, root, _src, _asked) = setup_server(50_000, 300, false, |t| t);
        let sc = server_scroll(&tree, root);
        assert_eq!(
            tree.get(sc).unwrap().content_h,
            50_000 * super::TABLE_ROW_H,
            "一行都没到货时，内容高也应等于 总行数 × 行高"
        );
    }

    #[test]
    fn server_missing_rows_render_as_skeletons() {
        // 未到货 ≠ 空行。空白与"这一行本来就没内容"无从区分，用户只会以为表格坏了。
        let (tree, root, _src, asked) = setup_server(50_000, 300, false, |t| t);
        let sc = server_scroll(&tree, root);
        assert!(
            is_skeleton(&tree, rendered_row(&tree, sc, 0)),
            "还没到货的行应画骨架占位"
        );
        assert!(!asked.borrow().is_empty(), "首帧就该为视口内的段发出请求");
    }

    #[test]
    fn server_asks_for_each_chunk_only_once_across_scrolling() {
        // 去重靠的是行源的在途台账，不是"窗口没动就不重算"——用户来回滚动时窗口一直在动，
        // 每一步都会走到发请求那一步。这条来回滚几趟，断言同一段只被问过一次。
        //
        // 早先这条只是原地重排 20 帧：窗口没变，压根走不到发请求那一步，把台账整个删掉
        // 它照样绿。反向验证才照出来。
        let (mut tree, _root, _src, asked) = setup_server(50_000, 300, false, |t| t);
        let (mut h, mut cap) = (None, None);
        for _ in 0..3 {
            for _ in 0..30 {
                tree.dispatch_pointer(wheel(-1200), &mut h, &mut cap);
                relayout(&mut tree, 300);
            }
            for _ in 0..30 {
                tree.dispatch_pointer(wheel(1200), &mut h, &mut cap);
                relayout(&mut tree, 300);
            }
        }
        let got = asked.borrow();
        let mut uniq: Vec<usize> = got.iter().map(|r| r.start).collect();
        uniq.sort_unstable();
        uniq.dedup();
        assert!(
            uniq.len() >= 3,
            "来回滚动应跨过至少三段，否则这条测不到什么；实际只碰到 {} 段",
            uniq.len()
        );
        assert_eq!(
            got.len(),
            uniq.len(),
            "同一段被重复请求了：共发 {} 次，却只有 {} 个不同的段",
            got.len(),
            uniq.len()
        );
        for r in got.iter() {
            assert_eq!(r.start % crate::ui::ROW_CHUNK, 0, "请求应对齐分段边界");
        }
    }

    #[test]
    fn server_idle_frames_ask_for_nothing() {
        // 「空闲零 CPU」的端到端烟测：稳态下响应式相位一个信号都不写（信号一写就是一次
        // 重绘请求）。
        //
        // 它的灵敏度有限，别指望它单独兜住这条性质：窗口没变时 `VirtualWindow::poll` 会
        // 提前返回，发请求与记窗口那两步压根不跑，所以"台账被无条件写"这类退化它照不出来
        // ——那条由 `row_source` 的 `idle_never_writes_the_signals` 直接钉住。
        let (mut tree, _root, _src, _asked) = setup_server(50_000, 300, true, |t| t);
        for _ in 0..4 {
            relayout(&mut tree, 300);
        }
        crate::anim::reset_request();
        for _ in 0..5 {
            relayout(&mut tree, 300);
        }
        assert!(
            !crate::anim::animation_requested(),
            "稳态下不该再请求重绘——信号一写就是一次请求"
        );
    }

    #[test]
    fn server_sort_change_refetches_with_the_new_sort() {
        // 排序变了旧顺序的缓存就是错的。作废这一步由正文替应用做——忘了它的表现是
        // "行还在原位、内容按新序错位"，看着像数据错乱，查不到排序头上。
        let (mut tree, _root, src, asked) = setup_server(50_000, 300, true, |t| t);
        assert!(src.loaded_rows() > 0, "首帧应已到货");
        asked.borrow_mut().clear();

        src.sort().set(Some(SortKey::new(1, SortOrder::Desc)));
        relayout(&mut tree, 300);

        let got = asked.borrow();
        assert!(!got.is_empty(), "排序变化后应按新排序重新请求");
        assert!(
            src.loaded_rows() > 0,
            "重新请求的数据应当帧回填（同步取数不必先闪骨架）"
        );
    }

    #[test]
    fn server_row_callbacks_see_the_real_row_index() {
        // 同本地虚拟表格的那条：期望值取自数据本身（首格文本 `r{i}`），不从几何反推。
        // 服务端版还多一层风险——下标若按"段内位置"给，滚到第二段时就全错了。
        let seen: Rc<RefCell<Vec<(usize, String)>>> = Rc::new(RefCell::new(Vec::new()));
        let rec = seen.clone();
        let (mut tree, _root, _src, _asked) = setup_server(50_000, 300, true, move |t| {
            t.cell_render(move |row, col, text| {
                if col == 0 {
                    rec.borrow_mut().push((row, text.to_string()));
                }
                None
            })
        });
        // 滚过第一段（100 行 × 行高），否则"段内位置"与"真实下标"恰好相等，测不出差别。
        let (mut h, mut cap) = (None, None);
        for _ in 0..200 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
            relayout(&mut tree, 300);
        }
        // 清空之后再滚一格，好让这一帧确实重建（上面已滚到稳态，不动就不会重建）。
        seen.borrow_mut().clear();
        tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        relayout(&mut tree, 300);

        let seen = seen.borrow();
        assert!(!seen.is_empty(), "滚动后应重建出一批新行");
        assert!(
            seen.iter().any(|(i, _)| *i >= crate::ui::ROW_CHUNK),
            "应已滚过第一段，否则测不出段内下标与真实下标的差别"
        );
        for (row, text) in seen.iter() {
            assert_eq!(
                text,
                &format!("r{row}"),
                "回调说这是第 {row} 行，格里装的却是 {text} 的数据"
            );
        }
    }

    #[test]
    fn server_actions_reach_header_and_rows() {
        // 服务端表格的表头是**响应式**的（要画排序箭头），故操作列走的是与本地虚拟表格
        // 不同的那条接线——单独钉一条。
        let (tree, root, _src, _asked) = setup_server(50_000, 300, true, |t| {
            t.actions("操作", 1.0, |_row| Element::label("·"))
        });
        let table = tree.get(root).unwrap().children[0];
        let header = tree.get(table).unwrap().children[0];
        assert_eq!(
            tree.get(header).unwrap().children.len(),
            3,
            "表头应为 2 数据列 + 1 操作列"
        );
        let sc = server_scroll(&tree, root);
        let tr = tree.get(rendered_row(&tree, sc, 0)).unwrap().children[0];
        assert_eq!(tree.get(tr).unwrap().children.len(), 3, "正文行也应三列");
    }

    #[test]
    fn server_skeleton_rows_keep_the_action_column() {
        // 骨架行若少一列，未到货区与已到货区的列边界会错开——滚动时列宽像在呼吸。
        let (tree, root, _src, _asked) = setup_server(50_000, 300, false, |t| {
            t.actions("操作", 1.0, |_row| Element::label("·"))
        });
        let sc = server_scroll(&tree, root);
        let row = rendered_row(&tree, sc, 0);
        assert!(is_skeleton(&tree, row), "这一行应是骨架");
        let tr = tree.get(row).unwrap().children[0];
        assert_eq!(
            tree.get(tr).unwrap().children.len(),
            3,
            "骨架行也要占住操作列，否则列边界与数据行对不齐"
        );
        let table = tree.get(root).unwrap().children[0];
        let header = tree.get(table).unwrap().children[0];
        let hcols = tree.get(header).unwrap().children.clone();
        let bcols = tree.get(tr).unwrap().children.clone();
        for (ci, (&hc, &bc)) in hcols.iter().zip(bcols.iter()).enumerate() {
            let (hb, bb) = (tree.abs_bounds(hc), tree.abs_bounds(bc));
            assert_eq!((hb.x, hb.w), (bb.x, bb.w), "第 {ci} 列应逐像素对齐");
        }
    }
}
