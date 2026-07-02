//! 可排序数据表格的响应式驱动（`Element::table_sortable` 内部使用）。
//!
//! 两个响应式 widget 共享同一个 `Signal<Option<(列下标, SortOrder)>>` 排序状态：
//! - [`SortableHeader`]：挂在表头行上，排序状态变化时重建表头单元格（刷新箭头）。
//! - [`SortableBody`]：挂在滚动正文上，排序状态变化时按列重排并重建数据行。
//!
//! 表头单元格点击循环切换：无 → 升序 → 降序 → 无。数值型列（两侧都可解析为 f64）
//! 按数值比较，否则按字符串比较（与主流表格的 numeric-aware 排序一致）。
//!
//! 外部只通过 `Element::table_sortable` 使用，无需直接构造本模块类型。

use std::cell::RefCell;
use std::cmp::Ordering;
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::Event;
use crate::geometry::{Color, Rect, Size};
use crate::render::Canvas;
use crate::signal::Signal;
use crate::spec::{Align, Dimension};
use crate::style::{Role, Style};
use crate::text::TextEngine;

use super::{Element, SortOrder, Truncate, TABLE_CELL_PAD_X, TABLE_HEADER_PAD_Y};

/// 受控排序状态：`None` 无排序；`Some((列下标, 方向))` 按该列排序。
pub(super) type SortState = Signal<Option<(usize, SortOrder)>>;

/// 排序指示器（表头箭头）的每实例样式覆盖。用 `Element::sort_indicator(SortStyle{..})` 链式设置；
/// 字段为 `None` 时回退到主题 [`TableTheme`](crate::theme::TableTheme)，再回退到内置默认。
///
/// # 示例
/// ```ignore
/// use windui::prelude::*;
/// Element::table_sortable(cols, rows, sort)
///     .sort_indicator(SortStyle { asc: Some("↑".into()), desc: Some("↓".into()), ..Default::default() })
/// ```
#[derive(Clone, Default)]
pub struct SortStyle {
    /// 升序箭头字形（如 "↑" / "▲"）。
    pub asc: Option<String>,
    /// 降序箭头字形（如 "↓" / "▼"）。
    pub desc: Option<String>,
    /// 箭头字号 px。
    pub size: Option<f32>,
    /// 箭头颜色（定死色；不设则用主题 text_muted 并随换肤）。
    pub color: Option<Color>,
    /// 箭头槽宽度 px。
    pub slot: Option<i32>,
    /// 标题与箭头间距 px。
    pub gap: Option<i32>,
    /// 箭头置于标题左侧（默认右侧）。
    pub leading: Option<bool>,
}

/// 解析后的排序指示器样式（实例覆盖 → 主题 → 内置默认，合并完成）。
struct ResolvedSort {
    asc: String,
    desc: String,
    size: f32,
    /// `None` = 用 `Role::TextMuted`（随主题热切换）；`Some` = 定死色。
    color: Option<Color>,
    slot: i32,
    gap: i32,
    leading: bool,
}

/// 按 实例覆盖 → 主题 `TableTheme` → 内置默认 的优先级合并出最终样式。
fn resolve_sort_style(ov: &SortStyle) -> ResolvedSort {
    let t = crate::theme::current();
    let tt = &t.table;
    ResolvedSort {
        asc: ov.asc.clone().unwrap_or_else(|| tt.sort_asc().to_string()),
        desc: ov
            .desc
            .clone()
            .unwrap_or_else(|| tt.sort_desc().to_string()),
        size: ov.size.unwrap_or_else(|| tt.sort_size()),
        color: ov.color.or(tt.sort_color),
        slot: ov.slot.unwrap_or_else(|| tt.sort_slot()),
        gap: ov.gap.unwrap_or_else(|| tt.sort_gap()),
        leading: ov.leading.unwrap_or_else(|| tt.sort_leading()),
    }
}

/// 排序意图变更回调（服务端排序模式）：点表头更新 `sort` 后触发，携带新排序状态，
/// 由应用据此重新拉取"当前页 + 该排序"的数据并写回正文数据信号。多个表头单元格共享。
pub(super) type OnSort = Rc<RefCell<dyn FnMut(&mut EventCtx, Option<(usize, SortOrder)>)>>;

/// 单元格值比较：两侧都能解析为数值时按数值比，否则按字符串（区分大小写）。
fn cmp_cells(a: &str, b: &str) -> Ordering {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.cmp(b),
    }
}

/// 依当前排序状态求行序（返回原始行下标的排列）。`None` 时保持原序（稳定）。
pub(super) fn sorted_order(rows: &[Vec<String>], sort: Option<(usize, SortOrder)>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    if let Some((col, ord)) = sort {
        // sort_by 稳定：等值行保持原相对次序。
        order.sort_by(|&a, &b| {
            let va = rows[a].get(col).map(String::as_str).unwrap_or("");
            let vb = rows[b].get(col).map(String::as_str).unwrap_or("");
            let c = cmp_cells(va, vb);
            match ord {
                SortOrder::Asc => c,
                SortOrder::Desc => c.reverse(),
            }
        });
    }
    order
}

/// 点击第 `ci` 列表头后的下一排序状态。循环：非活动列 → 升序；升序 → 降序；降序 → 取消。
pub(super) fn next_sort(
    current: Option<(usize, SortOrder)>,
    ci: usize,
) -> Option<(usize, SortOrder)> {
    match current {
        Some((c, SortOrder::Asc)) if c == ci => Some((ci, SortOrder::Desc)),
        Some((c, SortOrder::Desc)) if c == ci => None,
        _ => Some((ci, SortOrder::Asc)),
    }
}

/// 构建一个表头单元格：标题（单行、末尾省略）+ 定宽排序箭头槽，整格可点击循环切换排序。
/// 箭头独立渲染于单元格首/末（由 `rs.leading` 决定），空间不足时省略标题而非让箭头换行。
/// 样式（字形/字号/颜色/槽宽/间距/位置）由 `rs` 提供（实例覆盖 → 主题 → 默认）。
/// `on_sort` 为 `Some` 时（服务端模式），点击在更新 `sort` 后再触发回调（供应用重新拉取）。
fn header_cell(
    ci: usize,
    title: &str,
    weight: f32,
    sort: SortState,
    on_sort: Option<OnSort>,
    rs: &ResolvedSort,
) -> Element {
    let glyph = match sort.get() {
        Some((c, SortOrder::Asc)) if c == ci => rs.asc.clone(),
        Some((c, SortOrder::Desc)) if c == ci => rs.desc.clone(),
        _ => String::new(),
    };
    // 定宽箭头槽：始终预留，仅活动列显示字形。颜色未定死时用 TextMuted 角色随主题热切换。
    let mut arrow = Element::label(glyph)
        .font_size(rs.size)
        .width(rs.slot)
        .height(18);
    arrow = match rs.color {
        Some(c) => arrow.fg(c),
        None => arrow.fg_role(Role::TextMuted),
    };
    // 标题占剩余宽度，单行 + 末尾省略号（空间不足时截断标题，不影响箭头）。
    let title_el = Element::label(title.to_string())
        .font_size(13.0)
        .font_weight(600)
        .fg_role(Role::TextMuted)
        .max_lines(1)
        .truncate(Truncate::End)
        .weight(1.0)
        .height(18);
    let mut inner = Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(rs.gap);
    inner = if rs.leading {
        inner.child(arrow).child(title_el)
    } else {
        inner.child(title_el).child(arrow)
    };
    Element::stack()
        .weight(weight)
        .clickable()
        .on_click(move |ctx| {
            let next = next_sort(sort.get(), ci);
            sort.set(next);
            if let Some(cb) = &on_sort {
                (cb.borrow_mut())(ctx, next);
            }
        })
        .padding_xy(TABLE_CELL_PAD_X, TABLE_HEADER_PAD_Y)
        .child(inner)
}

/// 构建一行正文：`disp` 为显示位置（决定斑马纹），`cells` 为该行各列文本。
/// 结构与 `table_custom` 一致：`col[ row(单元格…), divider ]`，便于列对齐与行分隔。
pub(super) fn body_row(disp: usize, cells: &[String], weights: &[f32]) -> Element {
    let mut tr = Element::row().width_match().cross(Align::Stretch);
    // 斑马纹随显示位置交替（而非原始行号），排序后视觉仍规整。
    if disp % 2 == 1 {
        tr = tr.bg_role(Role::SurfaceAlt);
    }
    for (ci, cell) in cells.iter().enumerate() {
        let w = weights.get(ci).copied().unwrap_or(1.0);
        tr = tr
            .child(Element::table_cell_pad(Element::label(cell.clone()).font_size(13.0)).weight(w));
    }
    Element::col()
        .width_match()
        .child(tr)
        .child(Element::divider())
}

/// 清空某节点的全部子节点（递归释放子树 arena slot）。
fn clear_children(tree: &mut crate::core::Tree, id: crate::core::NodeId) {
    let old: Vec<_> = tree.get(id).map(|n| n.children.clone()).unwrap_or_default();
    for c in old {
        tree.remove(c);
    }
    if let Some(n) = tree.get_mut(id) {
        n.children.clear();
    }
}

/// 响应式表头：首次布局构建单元格；排序状态变化时重建（刷新箭头方向）。
/// 单元格统一由本 widget 构建（不在构造期预建），故 `.sort_indicator(..)` 在 build 前设置的
/// 样式覆盖能被首次构建采纳。`on_sort` 在服务端模式下透传给单元格（点击时触发应用重新拉取）。
pub(super) struct SortableHeader {
    columns: Vec<(String, f32)>,
    sort: SortState,
    on_sort: Option<OnSort>,
    /// 每实例样式覆盖（由 `Element::sort_indicator` 设置）；未设字段回退主题。
    style: SortStyle,
    /// 是否已构建过单元格（首次 on_update 无条件构建）。
    built: bool,
    last_version: u64,
}

impl SortableHeader {
    pub(super) fn new(
        columns: Vec<(String, f32)>,
        sort: SortState,
        on_sort: Option<OnSort>,
    ) -> Self {
        Self {
            last_version: sort.version(),
            columns,
            sort,
            on_sort,
            style: SortStyle::default(),
            built: false,
        }
    }

    /// 设置每实例样式覆盖（`Element::sort_indicator` 在 build 前调用）。
    pub(super) fn set_style(&mut self, style: SortStyle) {
        self.style = style;
    }
}

impl Widget for SortableHeader {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let ver = self.sort.version();
        if self.built && ver == self.last_version {
            return;
        }
        self.built = true;
        self.last_version = ver;
        let rs = resolve_sort_style(&self.style);
        let self_id = ctx.id();
        let tree = ctx.tree_mut();
        clear_children(tree, self_id);
        for (ci, (title, w)) in self.columns.iter().enumerate() {
            let mut el = header_cell(ci, title, *w, self.sort, self.on_sort.clone(), &rs);
            // 直接 build+add_child 绕过了父级线性容器 build 循环的 weight→主轴维度转换
            // （见 Element::build），此处手工复现：表头行恒为水平轴，故落到宽度。
            el.width = Dimension::Weight(*w);
            let id = el.build(tree);
            tree.add_child(self_id, id);
        }
    }

    // 自身无视觉内容；背景/边框由容器 Style 处理（同 DynList）。
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::ZERO
    }
    fn paint(
        &self,
        _bounds: Rect,
        _content: Rect,
        _focused: bool,
        _enabled: bool,
        _canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
    }
    fn on_event(&mut self, _ctx: &mut EventCtx, _ev: &Event) -> bool {
        false
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// 响应式正文：排序状态变化时按列重排并重建数据行（详见模块级说明）。
pub(super) struct SortableBody {
    rows: Vec<Vec<String>>,
    weights: Vec<f32>,
    sort: SortState,
    last_version: u64,
}

impl SortableBody {
    pub(super) fn new(rows: Vec<Vec<String>>, weights: Vec<f32>, sort: SortState) -> Self {
        Self {
            last_version: sort.version(),
            rows,
            weights,
            sort,
        }
    }
}

impl Widget for SortableBody {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let ver = self.sort.version();
        if ver == self.last_version {
            return;
        }
        self.last_version = ver;
        let self_id = ctx.id();
        let tree = ctx.tree_mut();
        clear_children(tree, self_id);
        let order = sorted_order(&self.rows, self.sort.get());
        for (disp, &ri) in order.iter().enumerate() {
            let el = body_row(disp, &self.rows[ri], &self.weights);
            let id = el.build(tree);
            tree.add_child(self_id, id);
        }
    }

    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::ZERO
    }
    fn paint(
        &self,
        _bounds: Rect,
        _content: Rect,
        _focused: bool,
        _enabled: bool,
        _canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
    }
    fn on_event(&mut self, _ctx: &mut EventCtx, _ev: &Event) -> bool {
        false
    }
}

/// 服务端排序模式的正文：绑定"当前页数据"信号，**不做内部排序**——排序由后端完成，
/// 前端只按应用给定的顺序渲染。数据信号版本变化（应用换页/换排序后写回）时重建行。
pub(super) struct PagedBody {
    rows: Signal<Vec<Vec<String>>>,
    weights: Vec<f32>,
    last_version: u64,
}

impl PagedBody {
    pub(super) fn new(rows: Signal<Vec<Vec<String>>>, weights: Vec<f32>) -> Self {
        Self {
            last_version: rows.version(),
            rows,
            weights,
        }
    }
}

impl Widget for PagedBody {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let ver = self.rows.version();
        if ver == self.last_version {
            return;
        }
        self.last_version = ver;
        let self_id = ctx.id();
        let tree = ctx.tree_mut();
        clear_children(tree, self_id);
        let data = self.rows.get();
        for (disp, row) in data.iter().enumerate() {
            let el = body_row(disp, row, &self.weights);
            let id = el.build(tree);
            tree.add_child(self_id, id);
        }
    }

    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::ZERO
    }
    fn paint(
        &self,
        _bounds: Rect,
        _content: Rect,
        _focused: bool,
        _enabled: bool,
        _canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
    }
    fn on_event(&mut self, _ctx: &mut EventCtx, _ev: &Event) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Tree;
    use crate::event::{MouseButton, PointerEvent, PointerKind};
    use crate::geometry::Point;
    use crate::signal::signal;

    fn rows(data: &[&[&str]]) -> Vec<Vec<String>> {
        data.iter()
            .map(|r| r.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    /// 取第 `col` 列在给定行序下的值序列，便于断言。
    fn col_values(rows: &[Vec<String>], order: &[usize], col: usize) -> Vec<String> {
        order.iter().map(|&i| rows[i][col].clone()).collect()
    }

    #[test]
    fn sorted_order_numeric_column_compares_as_numbers() {
        // 数值列："1280" < "20480" 数值序，而非 "1280" > "20480" 的字典序。
        let r = rows(&[&["a", "1280"], &["b", "3"], &["c", "20480"], &["d", "12"]]);
        let asc = sorted_order(&r, Some((1, SortOrder::Asc)));
        assert_eq!(col_values(&r, &asc, 1), ["3", "12", "1280", "20480"]);
        let desc = sorted_order(&r, Some((1, SortOrder::Desc)));
        assert_eq!(col_values(&r, &desc, 1), ["20480", "1280", "12", "3"]);
    }

    #[test]
    fn sorted_order_string_column_lexicographic() {
        let r = rows(&[&["banana"], &["apple"], &["cherry"]]);
        let asc = sorted_order(&r, Some((0, SortOrder::Asc)));
        assert_eq!(col_values(&r, &asc, 0), ["apple", "banana", "cherry"]);
    }

    #[test]
    fn sorted_order_none_keeps_original_and_is_stable() {
        let r = rows(&[&["x", "5"], &["y", "5"], &["z", "5"]]);
        // 无排序：原序。
        assert_eq!(sorted_order(&r, None), [0, 1, 2]);
        // 等值列升序：稳定，等值行保持原相对次序。
        assert_eq!(sorted_order(&r, Some((1, SortOrder::Asc))), [0, 1, 2]);
    }

    #[test]
    fn next_sort_cycles_none_asc_desc_none() {
        assert_eq!(next_sort(None, 0), Some((0, SortOrder::Asc)));
        assert_eq!(
            next_sort(Some((0, SortOrder::Asc)), 0),
            Some((0, SortOrder::Desc))
        );
        assert_eq!(next_sort(Some((0, SortOrder::Desc)), 0), None);
        // 点另一列：从该列升序重新开始（不继承前列方向）。
        assert_eq!(
            next_sort(Some((0, SortOrder::Desc)), 1),
            Some((1, SortOrder::Asc))
        );
    }

    /// 布局一个 400×300 的可排序表格，返回 tree。
    fn layout(el: Element) -> Tree {
        let mut tree = Tree::new();
        let root = el.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
        tree
    }

    /// 合成一次完整点击（Down→Up）于 `at`。
    fn click(tree: &mut Tree, at: Point) {
        let mut hover = None;
        let mut capture = None;
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
    }

    #[test]
    fn clicking_header_advances_sort_signal() {
        let sort = signal(None);
        let mut tree = layout(
            Element::table_sortable(
                vec![("名称", 2.0), ("大小", 1.0)],
                vec![vec!["a", "2"], vec!["b", "1"]],
                sort,
            )
            .width(400)
            .height(300),
        );
        // 首列表头在左上（含内边距），点击落在其可点击区。
        click(&mut tree, Point::new(40, 18));
        assert_eq!(sort.get(), Some((0, SortOrder::Asc)), "首次点击→升序");
        // 再次布局让响应式表头/正文按新状态重建，再点同列。
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
        click(&mut tree, Point::new(40, 18));
        assert_eq!(sort.get(), Some((0, SortOrder::Desc)), "再点同列→降序");
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
        click(&mut tree, Point::new(40, 18));
        assert_eq!(sort.get(), None, "三点同列→取消排序");
    }

    #[test]
    fn body_rebuilds_row_count_after_sort_change() {
        let sort = signal(Some((0usize, SortOrder::Asc)));
        let mut tree = layout(
            Element::table_sortable(
                vec![("名称", 1.0)],
                vec![vec!["c"], vec!["a"], vec!["b"]],
                sort,
            )
            .width(400)
            .height(300),
        );
        // 改排序方向 → 下次布局触发正文 on_update 重建（clear+rebuild），不 panic 即路径健康；
        // 行序正确性由 sorted_order 单测覆盖。
        sort.set(Some((0, SortOrder::Desc)));
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
    }

    #[test]
    fn resolve_sort_style_falls_back_to_theme_defaults() {
        // 无覆盖：回退主题/内置默认（▲/▼、10px、槽 14、间距 2、右侧、颜色随主题）。
        let rs = resolve_sort_style(&SortStyle::default());
        assert_eq!(rs.asc, "\u{25B2}");
        assert_eq!(rs.desc, "\u{25BC}");
        assert_eq!(rs.size, 10.0);
        assert_eq!(rs.slot, 14);
        assert_eq!(rs.gap, 2);
        assert!(!rs.leading);
        assert!(
            rs.color.is_none(),
            "默认颜色应为 None（用 TextMuted 角色随主题）"
        );
    }

    #[test]
    fn resolve_sort_style_instance_override_wins() {
        // 实例覆盖优先于主题/默认。
        let ov = SortStyle {
            asc: Some("↑".into()),
            desc: Some("↓".into()),
            size: Some(14.0),
            slot: Some(20),
            gap: Some(6),
            leading: Some(true),
            color: Some(Color::hex(0xFF0000)),
        };
        let rs = resolve_sort_style(&ov);
        assert_eq!(rs.asc, "↑");
        assert_eq!(rs.desc, "↓");
        assert_eq!(rs.size, 14.0);
        assert_eq!(rs.slot, 20);
        assert_eq!(rs.gap, 6);
        assert!(rs.leading);
        assert_eq!(rs.color, Some(Color::hex(0xFF0000)));
    }

    #[test]
    fn sort_indicator_builder_reaches_header_widget() {
        // .sort_indicator(..) 应能定位表头并设入覆盖，且首次布局用该覆盖构建（不 panic 即链路通）。
        let sort = signal(Some((0usize, SortOrder::Asc)));
        let el = Element::table_sortable(
            vec![("名称", 1.0), ("大小", 1.0)],
            vec![vec!["a", "2"], vec!["b", "1"]],
            sort,
        )
        .sort_indicator(SortStyle {
            asc: Some("↑".into()),
            leading: Some(true),
            ..Default::default()
        })
        .width(400)
        .height(300);
        let _tree = layout(el); // 触发首次 on_update：用覆盖样式构建表头单元格
    }

    #[test]
    fn header_cells_keep_weighted_width_after_rebuild() {
        // 回归：点表头触发响应式重建后，表头单元格必须保留比例宽度
        // （weight→主轴维度）。曾因重建绕过父级 build 循环导致首格占满、其余消失。
        let sort = signal(None);
        let mut tree = layout(
            Element::table_sortable(
                vec![("A", 2.0), ("B", 1.0), ("C", 1.5)],
                vec![vec!["a", "b", "c"]],
                sort,
            )
            .width(400)
            .height(300),
        );
        click(&mut tree, Point::new(40, 18)); // 触发排序变更 → 重建表头
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
        let root = tree.root.unwrap();
        let header_id = tree.get(root).unwrap().children[0];
        let cells = tree.get(header_id).unwrap().children.clone();
        assert_eq!(cells.len(), 3, "重建后表头应仍有 3 个单元格");
        let widths: Vec<Dimension> = cells.iter().map(|&c| tree.get(c).unwrap().width).collect();
        assert_eq!(
            widths,
            vec![
                Dimension::Weight(2.0),
                Dimension::Weight(1.0),
                Dimension::Weight(1.5)
            ],
            "重建后各表头单元格应保持比例宽度"
        );
    }

    #[test]
    fn server_mode_click_updates_sort_and_fires_callback() {
        use std::cell::Cell as StdCell;
        use std::rc::Rc;
        let sort = signal(None);
        let rows = signal(vec![vec!["a".to_string(), "2".to_string()]]);
        // 记录回调收到的排序意图（应与 sort 同步）。
        let seen: Rc<StdCell<Option<(usize, SortOrder)>>> = Rc::new(StdCell::new(None));
        let fired = Rc::new(StdCell::new(0u32));
        let (seen_c, fired_c) = (seen.clone(), fired.clone());
        let mut tree = layout(
            Element::table_sortable_server(
                vec![("名称", 2.0), ("大小", 1.0)],
                rows,
                sort,
                move |_ctx, new_sort| {
                    seen_c.set(new_sort);
                    fired_c.set(fired_c.get() + 1);
                },
            )
            .width(400)
            .height(300),
        );
        // 点首列表头：更新 sort → 升序，并触发回调携带同一值。
        click(&mut tree, Point::new(40, 18));
        assert_eq!(sort.get(), Some((0, SortOrder::Asc)), "sort 信号更新");
        assert_eq!(fired.get(), 1, "on_sort 回调被触发一次");
        assert_eq!(seen.get(), Some((0, SortOrder::Asc)), "回调收到新排序意图");
    }

    #[test]
    fn server_mode_body_renders_backend_order_without_internal_sort() {
        // 服务端模式：正文按数据信号给定顺序渲染，不做内部排序。
        // 给一份「已按后端逆序」的数据，前端应原样显示（若误做内部排序会被打乱）。
        let sort = signal(Some((0usize, SortOrder::Asc)));
        let rows = signal(vec![
            vec!["c".to_string()],
            vec!["b".to_string()],
            vec!["a".to_string()],
        ]);
        let mut tree = layout(
            Element::table_sortable_server(vec![("名称", 1.0)], rows, sort, |_, _| {})
                .width(400)
                .height(300),
        );
        // 应用换页/换排序：写回新一页数据 → 下次布局触发 PagedBody 重建，不 panic 即路径健康。
        rows.set(vec![vec!["z".to_string()], vec!["y".to_string()]]);
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
    }
}
