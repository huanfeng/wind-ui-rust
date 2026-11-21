//! 表格列宽：共享的列权重句柄 [`TableCols`] + 表头分界上的拖动手柄 [`ColHandle`]。
//!
//! 列宽仍是**权重**（与构造器里 `(标题, 权重)` 同语义），只是不再在构建期烘进各单元格的
//! `Dimension`：表头与正文的每个数据单元格都经 [`Element::weight_when`] 读同一个
//! `Signal<Vec<f32>>`。于是拖动手柄只改信号 + 请求重排，**一行都不重建**——排序表、
//! 虚拟表在之后重建出来的行也读同一个句柄，天然对齐。这是可拖动分栏 `split` 的同一招。
//!
//! 权重而非像素，是为了持久化：权重与窗口尺寸、DPI 都无关，原样存盘、下次原样读回即可。
//! 拖动只在**相邻两列**之间转移权重（总和不变），表格始终撑满宽度、不出横向滚动。
//!
//! 句柄外再套一层 `Rc<Cell<Signal>>`，是因为 `.column_weights(信号)` 是构建**之后**才链上的
//! 修饰符——单元格的闭包早已捕获了句柄，换绑只能换句柄里装的那个信号。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::{CursorShape, Event, MouseButton, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::style::Style;
use crate::text::TextEngine;
use crate::ui::Element;

/// 列宽调整结束回调：参数为调整后的整组列权重（与构造器的列一一对应）。
pub(super) type OnColumnsResized = Rc<RefCell<dyn FnMut(&mut EventCtx, &[f32])>>;

/// 手柄命中区向分界**每一侧**外扩的宽度（逻辑 px）：合起来是一条跨在列边界上的 8px
/// 抓取带。手柄本身零宽、不占布局，这一带只影响命中。
pub(super) const RESIZE_HIT: i32 = 4;

/// 拖动时每列至少保留的宽度（逻辑 px）：再窄标题连一个字都放不下，也就无从再拖回来。
pub(super) const TABLE_MIN_COL_W: i32 = 48;

struct Inner {
    weights: Cell<Signal<Vec<f32>>>,
    /// 构造时的列数。外部绑入的信号长度对不上时以它为准重置（见 [`TableCols::bind`]）。
    len: usize,
    resizable: Cell<bool>,
    on_resized: RefCell<Option<OnColumnsResized>>,
    /// 当前悬停的分界。
    hot: Cell<Option<usize>>,
    /// 正在拖动的分界。
    drag: Cell<Option<usize>>,
}

/// 一张表格的列权重句柄（克隆即共享）。
#[derive(Clone)]
pub(super) struct TableCols(Rc<Inner>);

impl TableCols {
    pub(super) fn new(weights: Vec<f32>) -> Self {
        let len = weights.len();
        Self(Rc::new(Inner {
            weights: Cell::new(crate::signal::signal(sanitize(weights, len))),
            len,
            resizable: Cell::new(true),
            on_resized: RefCell::new(None),
            hot: Cell::new(None),
            drag: Cell::new(None),
        }))
    }

    /// 第 `ci` 列当前权重。信号已被回收（表格所在窗口关了还在被读）或越界时回退 1。
    pub(super) fn weight(&self, ci: usize) -> f32 {
        self.0
            .weights
            .get()
            .try_with(|v| v.get(ci).copied())
            .flatten()
            .unwrap_or(1.0)
    }

    /// 数据列数（构造时定）。
    pub(super) fn len(&self) -> usize {
        self.0.len
    }

    pub(super) fn signal(&self) -> Signal<Vec<f32>> {
        self.0.weights.get()
    }

    /// 改绑到应用持有的信号（持久化用）。信号里的值**优先**——那是上次存下的列宽；
    /// 长度与列数对不上（首次运行的空 Vec、版本升级增减了列）或含非正/非有限值时，
    /// 先写回构造器给的默认权重，免得列宽错位到别的列上。
    pub(super) fn bind(&self, sig: Signal<Vec<f32>>) {
        let current = self.signal().get();
        let ok = sig.with(|v| v.len() == self.0.len && v.iter().all(|w| w.is_finite() && *w > 0.0));
        if !ok {
            sig.set(current);
        }
        self.0.weights.set(sig);
    }

    pub(super) fn set_resizable(&self, on: bool) {
        self.0.resizable.set(on);
    }

    pub(super) fn set_on_resized(&self, cb: OnColumnsResized) {
        *self.0.on_resized.borrow_mut() = Some(cb);
    }

    /// 给第 `ci` 列的单元格挂运行期权重（替代构建期的 `.weight(w)`）。
    pub(super) fn weigh(&self, el: Element, ci: usize) -> Element {
        let me = self.clone();
        el.weight_when(move || me.weight(ci))
    }

    /// 第 `ci` 列的表头单元格：按列权重占宽（与正文同一个句柄，故逐列对齐）。
    pub(super) fn header_cell(&self, cell: Element, ci: usize) -> Element {
        self.weigh(cell, ci)
    }

    /// 第 `boundary` 与 `boundary + 1` 列之间的拖动手柄：插在表头两格之间的**零宽**节点。
    ///
    /// 不占任何布局宽度——表头单元格、悬停底色与正文完全不受它影响；可抓的范围靠
    /// [`Widget::hit_slop`] 向两侧各扩 [`RESIZE_HIT`]，平时什么都不画，只在悬停 / 拖动时
    /// 在分界上画一条线。外扩部分与两侧单元格重叠，故设 `raised` 让它先于它们受测。
    pub(super) fn handle(&self, boundary: usize) -> Element {
        let mut h = Element::leaf().width(0).widget(ColHandle {
            cols: self.clone(),
            boundary,
            drag: None,
        });
        h.raised = true;
        h
    }
}

/// 把权重收拾成可用的：长度补齐/截断到 `len`，非正/非有限值换成 1。
fn sanitize(mut v: Vec<f32>, len: usize) -> Vec<f32> {
    v.resize(len, 1.0);
    for w in &mut v {
        if !w.is_finite() || *w <= 0.0 {
            *w = 1.0;
        }
    }
    v
}

/// 由两列拖动前的像素宽与权重，算出把分界挪 `dx` 之后两列的新权重。
///
/// 两列权重之和不变（其余列分到的宽度也就不变），按像素比例重分；各自至少留 `min` px。
/// 两列加起来都不够 `2 × min` 时不动——那是窗口太窄，硬拖只会把一列挤成负数。
pub(super) fn split_pair(
    l_px: i32,
    r_px: i32,
    wl: f32,
    wr: f32,
    dx: i32,
    min: i32,
) -> Option<(f32, f32)> {
    let total = l_px + r_px;
    if total < 2 * min || total <= 0 {
        return None;
    }
    let new_l = (l_px + dx).clamp(min, total - min);
    let pair = wl + wr;
    let nl = pair * new_l as f32 / total as f32;
    Some((nl, pair - nl))
}

/// 拖动起点的快照：按下那一刻两列的像素宽与权重。之后每次 Move 都相对它算，
/// 不累加增量——累加会把每帧的取整误差攒成漂移。
#[derive(Clone, Copy)]
struct DragStart {
    x: i32,
    l_px: i32,
    r_px: i32,
    wl: f32,
    wr: f32,
    changed: bool,
}

/// 表头分界的拖动手柄（零宽节点，见 [`TableCols::handle`]）。
pub(super) struct ColHandle {
    cols: TableCols,
    boundary: usize,
    drag: Option<DragStart>,
}

impl ColHandle {
    /// 列宽拖动是否开着（`.resizable_columns(false)` 关掉后手柄只占位，不命中、不绘制）。
    fn on(&self) -> bool {
        self.cols.0.resizable.get()
    }

    fn active(&self) -> bool {
        let b = Some(self.boundary);
        self.cols.0.hot.get() == b || self.cols.0.drag.get() == b
    }

    /// 分界两侧那两个表头单元格的绝对矩形：手柄在表头行里的前一个与后一个兄弟。
    fn pair_bounds(&self, ctx: &mut EventCtx) -> Option<(Rect, Rect)> {
        let me = ctx.id();
        let tree = ctx.tree_mut();
        let row = tree.get(me)?.parent?;
        let siblings = &tree.get(row)?.children;
        let i = siblings.iter().position(|&c| c == me)?;
        let l = *siblings.get(i.checked_sub(1)?)?;
        let r = *siblings.get(i + 1)?;
        Some((tree.abs_bounds(l), tree.abs_bounds(r)))
    }

    /// 分界附近（抓取带加线宽）的重绘区。
    fn damage(&self, ctx: &mut EventCtx) {
        let b = ctx.bounds();
        ctx.mark_dirty_rect(b.inflate(RESIZE_HIT + 2));
    }
}

impl Widget for ColHandle {
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        // 零宽；高度由表头行的 cross(Stretch) 拉伸，自己不报——报了会把表头撑高。
        Size::ZERO
    }

    fn hit_slop(&self) -> crate::geometry::Insets {
        if self.on() {
            crate::geometry::Insets {
                left: RESIZE_HIT,
                right: RESIZE_HIT,
                top: 0,
                bottom: 0,
            }
        } else {
            crate::geometry::Insets::default()
        }
    }

    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
        if !enabled || !self.on() || !self.active() {
            return;
        }
        // 节点零宽、恰在列边界上：画一条以它为中线的 2px 线（越出自身矩形，表头行不裁剪）。
        let th = crate::theme::current();
        let color = th.table.resize_line(&th.palette);
        canvas.fill_rect(
            bounds.x as f32 - 1.0,
            bounds.y as f32,
            2.0,
            bounds.h as f32,
            &Paint::fill(color),
        );
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        let Event::Pointer(p) = ev else {
            return false;
        };
        if !self.on() && self.drag.is_none() {
            return false;
        }
        match p.kind {
            PointerKind::Enter => {
                self.cols.0.hot.set(Some(self.boundary));
                self.damage(ctx);
                true
            }
            PointerKind::Leave => {
                if self.cols.0.hot.get() == Some(self.boundary) {
                    self.cols.0.hot.set(None);
                }
                self.damage(ctx);
                true
            }
            PointerKind::Down if p.button == MouseButton::Left => {
                let Some((l, r)) = self.pair_bounds(ctx) else {
                    return true;
                };
                self.drag = Some(DragStart {
                    x: p.pos.x,
                    l_px: l.w,
                    r_px: r.w,
                    wl: self.cols.weight(self.boundary),
                    wr: self.cols.weight(self.boundary + 1),
                    changed: false,
                });
                self.cols.0.drag.set(Some(self.boundary));
                ctx.capture();
                self.damage(ctx);
                true
            }
            PointerKind::Move if self.drag.is_some() => {
                let d = self.drag.as_mut().expect("上面已判过");
                let Some((nl, nr)) =
                    split_pair(d.l_px, d.r_px, d.wl, d.wr, p.pos.x - d.x, TABLE_MIN_COL_W)
                else {
                    return true;
                };
                let b = self.boundary;
                let sig = self.cols.signal();
                let same = sig.with(|v| {
                    v.get(b).is_some_and(|w| (w - nl).abs() < 1e-6)
                        && v.get(b + 1).is_some_and(|w| (w - nr).abs() < 1e-6)
                });
                if !same {
                    sig.update(|v| {
                        if b + 1 < v.len() {
                            v[b] = nl;
                            v[b + 1] = nr;
                        }
                    });
                    d.changed = true;
                    // 权重变了要重排；信号写入只保证重绘（同 `SplitHandle`）。
                    ctx.mark_layout_dirty();
                }
                true
            }
            PointerKind::Up if self.drag.is_some() => {
                let d = self.drag.take().expect("上面已判过");
                self.cols.0.drag.set(None);
                ctx.release_capture();
                self.damage(ctx);
                // 只在真的变了时通知：单击一下手柄不该触发一次存盘。
                if d.changed {
                    let cb = self.cols.0.on_resized.borrow().clone();
                    if let Some(cb) = cb {
                        let weights = self.cols.signal().get();
                        ctx.app_callback(|ctx| (cb.borrow_mut())(ctx, &weights));
                    }
                }
                true
            }
            _ => false,
        }
    }

    fn cursor(&self) -> CursorShape {
        if self.on() {
            CursorShape::SizeWE
        } else {
            CursorShape::Arrow
        }
    }

    /// 关掉拖动后命中穿透：不吞表头的点击。
    fn hit_opaque(&self) -> bool {
        self.on()
    }

    fn on_removed(&mut self) {
        // 表头重建（点排序 / 服务端换页）会回收旧手柄；按着或悬停时被回收收不到 Up/Leave，
        // 共享句柄上的状态得在这里清，否则新手柄一建出来分界线就一直亮着。
        self.reset_interaction();
    }

    fn reset_interaction(&mut self) {
        // 拖到一半节点被隐藏/表格被换掉：别把"正在拖"的状态留在共享句柄里。
        if self.drag.take().is_some() {
            self.cols.0.drag.set(None);
        }
        if self.cols.0.hot.get() == Some(self.boundary) {
            self.cols.0.hot.set(None);
        }
    }
}

/// 表格根节点的 widget：只为让构建后链上的修饰符（`.column_weights` 等）找到 [`TableCols`]。
/// 自身不画、不吃事件，命中穿透（同纯容器）。
pub(super) struct TableRoot {
    pub(super) cols: TableCols,
}

impl Widget for TableRoot {
    fn hit_opaque(&self) -> bool {
        false
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// 在构建好的表格元素上取它的 [`TableCols`]（不是表格时 `None`）。
pub(super) fn cols_of(el: &mut Element) -> Option<TableCols> {
    el.widget
        .as_any_mut()?
        .downcast_mut::<TableRoot>()
        .map(|r| r.cols.clone())
}

/// 测试用：表头行里的单元格（跳过零宽拖动手柄）。
#[cfg(test)]
pub(crate) fn header_cells(
    tree: &crate::core::Tree,
    header: crate::core::NodeId,
) -> Vec<crate::core::NodeId> {
    tree.get(header)
        .unwrap()
        .children
        .iter()
        .copied()
        .filter(|&c| !matches!(tree.get(c).unwrap().width, crate::spec::Dimension::Px(0)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_pair_keeps_the_sum_and_follows_pixels() {
        // 两列 100px + 100px、权重 1 + 1，分界右移 20px → 120:80。
        let (l, r) = split_pair(100, 100, 1.0, 1.0, 20, 36).unwrap();
        assert!((l + r - 2.0).abs() < 1e-6, "权重和应不变");
        assert!(
            (l - 1.2).abs() < 1e-6 && (r - 0.8).abs() < 1e-6,
            "实得 {l}/{r}"
        );
    }

    #[test]
    fn split_pair_clamps_to_min_width() {
        let (l, r) = split_pair(100, 100, 1.0, 1.0, -500, 36).unwrap();
        assert!(
            (l - 2.0 * 36.0 / 200.0).abs() < 1e-6,
            "左列应钳在 36px，实得 {l}"
        );
        assert!((l + r - 2.0).abs() < 1e-6);
        assert!(
            split_pair(30, 30, 1.0, 1.0, 5, 36).is_none(),
            "两列合计不足 2×min 时不动"
        );
    }

    #[test]
    fn bind_keeps_saved_weights_and_resets_mismatched_ones() {
        let cols = TableCols::new(vec![2.0, 1.0]);
        let saved = crate::signal::signal(vec![3.0, 1.5]);
        cols.bind(saved);
        assert_eq!(cols.weight(0), 3.0, "存下的列宽优先");
        let stale = crate::signal::signal(vec![1.0, 1.0, 1.0]);
        cols.bind(stale);
        assert_eq!(stale.get(), vec![3.0, 1.5], "长度不符时写回当前权重");
        let empty = crate::signal::signal(Vec::<f32>::new());
        cols.bind(empty);
        assert_eq!(empty.get().len(), 2, "首次运行的空 Vec 填成默认权重");
    }

    // ---- 契约：经真实 build → layout → dispatch_pointer，表头与正文一起量 ----

    use crate::core::{Layout, NodeId, Tree};
    use crate::event::PointerEvent;
    use crate::geometry::Point;
    use crate::spec::Axis;

    const W: i32 = 400;

    struct T {
        tree: Tree,
        root: NodeId,
        hover: Option<NodeId>,
        cap: Option<NodeId>,
    }

    impl T {
        /// `wrap_w`：`Some(w)` 时外包一层定宽容器（表格本身恒 width_match）。两种宽度来源都要测：
        /// 撑满窗口与父级显式定宽（AGENTS §5「宽度不能只测一种」）。
        fn new(table: Element, wrap_w: Option<i32>) -> Self {
            let el = match wrap_w {
                Some(w) => Element::col().width(w).height(300).child(table.weight(1.0)),
                None => table,
            };
            let mut tree = Tree::new();
            let root = el.build(&mut tree);
            tree.root = Some(root);
            let mut t = T {
                tree,
                root,
                hover: None,
                cap: None,
            };
            t.relayout();
            t
        }
        fn relayout(&mut self) {
            self.tree
                .layout_root(Size::new(W, 300), &mut crate::text::NullTextEngine);
        }
        fn table(&self) -> NodeId {
            // 外包了定宽容器时表格是它唯一的子节点。
            let n = self.tree.get(self.root).unwrap();
            if n.children.len() == 1 {
                n.children[0]
            } else {
                self.root
            }
        }
        fn kids(&self, id: NodeId) -> Vec<NodeId> {
            self.tree.get(id).unwrap().children.clone()
        }
        /// 表头的数据列单元格矩形。多选表的表头是 `[全选, 子行]`，数据列在子行里。
        fn header(&self) -> Vec<Rect> {
            let hdr = self.kids(self.table())[0];
            let mut cells = self.kids(hdr);
            if cells.len() == 2 && self.kids(cells[1]).len() > 1 {
                cells = self.kids(cells[1]);
            }
            // 零宽的拖动手柄夹在单元格之间，跳过。
            cells
                .iter()
                .map(|&c| self.tree.abs_bounds(c))
                .filter(|r| r.w > 0)
                .collect()
        }
        /// 正文第一行的全部单元格矩形（深度优先找第一个水平行）。
        fn body_row(&self) -> Vec<Rect> {
            let scroll = self.kids(self.table())[2];
            let mut stack = vec![scroll];
            while let Some(id) = stack.pop() {
                let n = self.tree.get(id).unwrap();
                if let Layout::Linear {
                    axis: Axis::Horizontal,
                    ..
                } = n.layout
                {
                    if n.children.len() >= 2 && n.bounds.h > 0 {
                        return n
                            .children
                            .iter()
                            .map(|&c| self.tree.abs_bounds(c))
                            .collect();
                    }
                }
                stack.extend(n.children.iter().rev());
            }
            panic!("没找到正文行");
        }
        fn ptr(&mut self, kind: PointerKind, x: i32, y: i32) {
            self.tree.dispatch_pointer(
                PointerEvent::single(kind, Point::new(x, y), MouseButton::Left),
                &mut self.hover,
                &mut self.cap,
            );
        }
        /// 从 `x` 拖到 `x + dx`（表头竖直中线上），每步后重排。
        fn drag(&mut self, x: i32, dx: i32) {
            let y = self.header()[0].y + self.header()[0].h / 2;
            self.ptr(PointerKind::Down, x, y);
            self.ptr(PointerKind::Move, x + dx, y);
            self.relayout();
            self.ptr(PointerKind::Up, x + dx, y);
            self.relayout();
        }
    }

    fn rows() -> Vec<Vec<&'static str>> {
        vec![vec!["a", "1", "x"], vec!["b", "2", "y"]]
    }
    fn cols3() -> Vec<(&'static str, f32)> {
        vec![("名称", 2.0), ("大小", 1.0), ("类型", 1.0)]
    }

    /// 正文数据格的 x 区间必须与表头逐列重合（`skip` 跳过多选表的复选框格）。
    fn assert_aligned(t: &T, skip: usize, what: &str) {
        let h = t.header();
        let b = t.body_row();
        for (ci, hc) in h.iter().enumerate() {
            let bc = b[ci + skip];
            assert_eq!(
                (hc.x, hc.w),
                (bc.x, bc.w),
                "{what}：第 {ci} 列表头与正文错位"
            );
        }
    }

    fn check_drag_moves_boundary(table: Element, wrap_w: Option<i32>, skip: usize, what: &str) {
        let weights = crate::signal::signal(Vec::new());
        let fired = Rc::new(Cell::new(0));
        let f = fired.clone();
        let table = table
            .column_weights(weights)
            .on_columns_resized(move |_, w| {
                assert_eq!(w.len(), 3);
                f.set(f.get() + 1);
            });
        let mut t = T::new(table, wrap_w);
        let before = t.header();
        assert_aligned(&t, skip, what);
        let sum0: f32 = weights.get().iter().sum();
        // 按在分界左侧的手柄上（左列右缘内 1px），右拖 30px。
        let x = before[0].right() - 1;
        t.drag(x, 30);
        let after = t.header();
        assert!(
            (after[0].w - (before[0].w + 30)).abs() <= 1,
            "{what}：左列应宽 30px，{before:?} → {after:?}"
        );
        assert!(
            (after[1].w - (before[1].w - 30)).abs() <= 1,
            "{what}：右邻列应窄 30px"
        );
        assert_eq!(after[2].w, before[2].w, "{what}：不相邻的列不受影响");
        assert_aligned(&t, skip, what);
        let sum1: f32 = weights.get().iter().sum();
        assert!((sum0 - sum1).abs() < 1e-4, "{what}：权重和应不变");
        assert_eq!(fired.get(), 1, "{what}：松手时回调恰好一次");
    }

    #[test]
    fn drag_resizes_plain_table_header_and_body_together() {
        check_drag_moves_boundary(Element::table(cols3(), rows()), None, 0, "table/撑满");
        check_drag_moves_boundary(Element::table(cols3(), rows()), Some(360), 0, "table/定宽");
    }

    #[test]
    fn drag_resizes_sortable_table_whose_header_is_rebuilt() {
        let sort = crate::signal::signal(None);
        check_drag_moves_boundary(
            Element::table_sortable(cols3(), rows(), sort),
            None,
            0,
            "sortable",
        );
        let sort = crate::signal::signal(None);
        let t = Element::table_sortable(cols3(), rows(), sort)
            .actions("操作", 1.0, |_| Element::label("…"));
        check_drag_moves_boundary(t, Some(380), 0, "sortable+actions");
    }

    #[test]
    fn drag_resizes_virtual_and_selectable_tables() {
        let data = crate::signal::signal(
            rows()
                .into_iter()
                .map(|r| r.into_iter().map(String::from).collect())
                .collect::<Vec<Vec<String>>>(),
        );
        check_drag_moves_boundary(
            Element::table_virtual(cols3(), data, crate::ui::TABLE_ROW_H).height(200),
            None,
            0,
            "virtual",
        );
        let sel = vec![crate::signal::signal(false), crate::signal::signal(false)];
        let sort = crate::signal::signal(None);
        check_drag_moves_boundary(
            Element::table_selectable(cols3(), rows(), sel, sort),
            None,
            1,
            "selectable",
        );
    }

    /// 分界右侧那一列左缘的手柄管的是同一条分界。
    #[test]
    fn left_side_handle_drives_the_same_boundary() {
        let mut t = T::new(Element::table(cols3(), rows()), None);
        let before = t.header();
        t.drag(before[1].x + 1, -20);
        let after = t.header();
        assert!(
            (after[0].w - (before[0].w - 20)).abs() <= 1,
            "{before:?} → {after:?}"
        );
        assert_aligned(&t, 0, "左侧手柄");
    }

    #[test]
    fn drag_clamps_to_min_column_width() {
        let mut t = T::new(Element::table(cols3(), rows()), None);
        let before = t.header();
        t.drag(before[0].right() - 1, 1000);
        let after = t.header();
        assert!(
            (after[1].w - TABLE_MIN_COL_W).abs() <= 1,
            "右邻列应钳在最小宽，实得 {}",
            after[1].w
        );
        assert_eq!(
            after[0].w + after[1].w,
            before[0].w + before[1].w,
            "两列合计不变"
        );
    }

    #[test]
    fn saved_weights_apply_on_first_layout() {
        let saved = crate::signal::signal(vec![1.0f32, 1.0, 2.0]);
        let t = T::new(Element::table(cols3(), rows()).column_weights(saved), None);
        let h = t.header();
        assert_eq!(
            (h[0].w, h[1].w, h[2].w),
            (100, 100, 200),
            "应按存下的 1:1:2 分宽"
        );
        assert_aligned(&t, 0, "存盘权重");
    }

    /// 外部改写权重（「恢复默认列宽」）后重排即生效，不必重建表格。
    #[test]
    fn writing_the_signal_from_outside_relays_out() {
        let w = crate::signal::signal(Vec::new());
        let mut t = T::new(Element::table(cols3(), rows()).column_weights(w), None);
        w.set(vec![1.0, 1.0, 2.0]);
        t.relayout();
        let h = t.header();
        assert_eq!((h[0].w, h[2].w), (100, 200));
        assert_aligned(&t, 0, "外部改写");
    }

    /// 关掉拖动：拖不动、不回调、无拖动光标，分界附近的点击落回表头（排序）。
    #[test]
    fn disabled_resize_does_nothing() {
        let sort = crate::signal::signal(None);
        let fired = Rc::new(Cell::new(0));
        let f = fired.clone();
        let table = Element::table_sortable(cols3(), rows(), sort)
            .resizable_columns(false)
            .on_columns_resized(move |_, _| f.set(f.get() + 1));
        let mut t = T::new(table, None);
        let before = t.header();
        let x = before[0].right() - 1;
        let y = before[0].y + before[0].h / 2;
        assert_ne!(
            t.tree.cursor_at(t.tree.hit_test(Point::new(x, y)).unwrap()),
            CursorShape::SizeWE,
            "关掉后分界处不再是拖动光标"
        );
        t.drag(x, 30);
        assert_eq!(t.header(), before, "关掉后拖不动");
        assert_eq!(fired.get(), 0);
        assert_eq!(sort.get(), None, "拖动手势不应误触排序");
        t.ptr(PointerKind::Down, x, y);
        t.ptr(PointerKind::Up, x, y);
        assert_eq!(
            sort.get().map(|k| k.column),
            Some(0),
            "分界附近的点击应落到表头排序"
        );
    }

    /// 手柄零宽、不占布局：相邻表头格首尾相接、合计占满整宽；分界两侧各 RESIZE_HIT 内抓得住。
    #[test]
    fn handles_take_no_layout_space() {
        let t = T::new(Element::table(cols3(), rows()), None);
        let h = t.header();
        assert_eq!(h[0].right(), h[1].x, "相邻表头格应首尾相接");
        assert_eq!(h[1].right(), h[2].x);
        assert_eq!(
            h.iter().map(|r| r.w).sum::<i32>(),
            W,
            "表头格合计应占满整宽"
        );
        let y = h[0].y + h[0].h / 2;
        for dx in [-RESIZE_HIT, -1, 0, RESIZE_HIT - 1] {
            let hit = t.tree.hit_test(Point::new(h[0].right() + dx, y)).unwrap();
            assert_eq!(
                t.tree.cursor_at(hit),
                CursorShape::SizeWE,
                "dx={dx} 应落在抓取带内"
            );
        }
        let far = t
            .tree
            .hit_test(Point::new(h[0].right() + RESIZE_HIT + 2, y))
            .unwrap();
        assert_ne!(
            t.tree.cursor_at(far),
            CursorShape::SizeWE,
            "抓取带外不应是拖动光标"
        );
    }

    #[test]
    fn enabled_handle_shows_resize_cursor() {
        let t = T::new(Element::table(cols3(), rows()), None);
        let h = t.header();
        let p = Point::new(h[0].right() - 1, h[0].y + h[0].h / 2);
        assert_eq!(
            t.tree.cursor_at(t.tree.hit_test(p).unwrap()),
            CursorShape::SizeWE
        );
    }

    /// 开着拖动时，表头单元格中部的点击仍是排序（手柄只占边缘窄条）。
    #[test]
    fn header_center_click_still_sorts_with_resize_on() {
        let sort = crate::signal::signal(None);
        let mut t = T::new(Element::table_sortable(cols3(), rows(), sort), None);
        let h = t.header();
        let (x, y) = (h[1].x + h[1].w / 2, h[1].y + h[1].h / 2);
        t.ptr(PointerKind::Down, x, y);
        t.ptr(PointerKind::Up, x, y);
        assert_eq!(sort.get().map(|k| k.column), Some(1));
    }

    /// 悬停 / 按住手柄时表头被重建（排序变化），旧手柄回收后共享状态不得残留。
    #[test]
    fn header_rebuild_clears_handle_state() {
        let sort = crate::signal::signal(None);
        let cols_probe = Rc::new(RefCell::new(None));
        let mut table = Element::table_sortable(cols3(), rows(), sort);
        *cols_probe.borrow_mut() = cols_of(&mut table);
        let mut t = T::new(table, None);
        let h = t.header();
        let (x, y) = (h[0].right() - 1, h[0].y + h[0].h / 2);
        t.ptr(PointerKind::Move, x, y); // 悬停 → hot
        t.ptr(PointerKind::Down, x, y); // 按住 → drag
        let cols = cols_probe.borrow().clone().unwrap();
        assert!(
            cols.0.hot.get().is_some() && cols.0.drag.get().is_some(),
            "前提：悬停且按住"
        );
        sort.set(Some(crate::ui::SortKey::asc(1))); // 触发表头重建
        t.relayout();
        assert_eq!(cols.0.hot.get(), None, "重建后 hot 不得残留");
        assert_eq!(cols.0.drag.get(), None, "重建后 drag 不得残留");
    }

    /// 悬停分界时画一条以列边界为中线的 2px 强调线；移开即消失、平时不画。
    /// 手柄零宽，绘制在自身矩形之外——核心不得因空矩形把它跳过。
    #[test]
    fn hover_draws_line_centered_on_boundary() {
        let t = &mut T::new(Element::table(cols3(), rows()), None);
        let h = t.header();
        let (x, y) = (h[0].right(), h[0].y + h[0].h / 2);
        let accent = {
            let th = crate::theme::current();
            th.table.resize_line(&th.palette)
        };
        let shot = |t: &T| {
            let mut pm = tiny_skia::Pixmap::new(W as u32, 300).unwrap();
            {
                let mut c = crate::render::skia::SkiaCanvas::new(&mut pm);
                t.tree.paint(&mut c);
            }
            (-2..2)
                .map(|dx| {
                    let p = pm.pixel((x + dx) as u32, y as u32).unwrap();
                    (p.red(), p.green(), p.blue()) == (accent.r, accent.g, accent.b)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(shot(t), vec![false; 4], "平时不画线");
        t.ptr(PointerKind::Move, x + 2, y);
        assert_eq!(
            shot(t),
            vec![false, true, true, false],
            "悬停时线居中于分界、宽 2px"
        );
        t.ptr(PointerKind::Move, h[1].x + h[1].w / 2, y);
        assert_eq!(shot(t), vec![false; 4], "移开即消失");
    }
}
