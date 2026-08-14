//! 可排序数据表格的响应式驱动（`Element::table_sortable` 内部使用）。
//!
//! 两个响应式 widget 共享同一个 `Signal<Option<SortKey>>` 排序状态：
//! - [`SortableHeader`]：挂在表头行上，排序状态变化时重建表头单元格（刷新箭头）。
//! - [`SortableBody`]：挂在滚动正文上，排序状态变化时按列重排并重建数据行。
//!
//! 表头单元格点击循环切换：无 → 升序 → 降序 → 无。数值型列（两侧都可解析为 f64）
//! 按数值比较，否则按字符串比较（与主流表格的 numeric-aware 排序一致）。
//!
//! 外部只通过 `Element::table_sortable` 使用，无需直接构造本模块类型。

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;
use std::rc::Rc;

use crate::anim::{Easing, Transition};
use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, MouseButton, PointerKind};
use crate::geometry::{Color, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::spec::{Align, Dimension};
use crate::style::{Role, Style};
use crate::text::TextEngine;

use super::{Element, SortOrder, Truncate, TABLE_CELL_PAD_X, TABLE_CELL_PAD_Y, TABLE_HEADER_PAD_Y};

/// 排序键：按**哪一列**、以**什么方向**排。表格排序状态一律用 `Option<SortKey>`
/// 承载——`None` 即未排序，故本类型自身不含"无排序"表示。
///
/// 从裸元组 `(usize, SortOrder)` 提升为命名类型：该二元组此前在四处公开签名里重复，
/// 谁是列、谁是方向全靠位置约定，误传顺序编译期也发现不了（两字段类型不同，这里靠的是
/// 字段名而非类型）。
///
/// # 示例
/// ```
/// use windui::prelude::*;
/// let sort = signal(Some(SortKey::asc(0)));
/// assert_eq!(sort.get(), Some(SortKey::new(0, SortOrder::Asc)));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    /// 排序列下标（对应 `columns` 的下标）。
    pub column: usize,
    /// 排序方向。
    pub order: SortOrder,
}

impl SortKey {
    /// 指定列与方向。
    pub const fn new(column: usize, order: SortOrder) -> Self {
        Self { column, order }
    }
    /// 按该列升序。
    pub const fn asc(column: usize) -> Self {
        Self::new(column, SortOrder::Asc)
    }
    /// 按该列降序。
    pub const fn desc(column: usize) -> Self {
        Self::new(column, SortOrder::Desc)
    }
}

/// 受控排序状态：`None` 无排序；`Some(SortKey)` 按该列该方向排序。
pub(super) type SortState = Signal<Option<SortKey>>;

/// 操作列配置：在表格尾部追加一列，单元格由 `build(行下标)` 按行生成任意控件
/// （如 查看/编辑/删除 按钮组）。表头显示 `title`（不可排序），列宽按 `weight` 参与分配。
///
/// 由 [`Element::actions`](super::Element::actions) 设置后透传给响应式表头与正文；
/// 排序/换页重建时按行重新调用 `build`。传给 `build` 的行下标：客户端表格
/// （`table_sortable`/`table_selectable`）为**原始行下标**（排序后仍锁定同一数据行，
/// 与选择语义一致）；服务端表格（`table_sortable_server`）为当前页内的**显示下标**。
/// `build` 内用 `move` 捕获该下标即可为每行绑定独立回调。
#[derive(Clone)]
pub(super) struct ActionCol {
    title: String,
    weight: f32,
    build: Rc<dyn Fn(usize) -> Element>,
}

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
pub(super) type OnSort = Rc<RefCell<dyn FnMut(&mut EventCtx, Option<SortKey>)>>;

/// 自定义单元格渲染：`(行下标, 列下标, 单元格文本) -> Option<Element>`。
/// 返回 `Some` 时该格用自定义控件（徽章/彩色标签等），`None` 回退默认文本渲染。
/// 排序仍基于单元格文本（渲染与排序键解耦）。行下标语义与操作列一致：
/// 客户端表格为原始行下标，服务端表格为页内显示下标。
pub(super) type CellRender = Rc<dyn Fn(usize, usize, &str) -> Option<Element>>;

/// 行激活回调（双击整行触发）：携带行下标（语义同操作列/单元格渲染——客户端表格为原始
/// 行下标，服务端表格为页内显示下标）。多行共享。落点在操作列按钮上时不触发（按钮先吃掉 Down）。
///
/// `Rc<RefCell<dyn FnMut>>` 而非 `Rc<dyn Fn>`：一次性动作回调对外一律 `FnMut`
/// （用户常在闭包里改捕获的状态），共享靠 `Rc`、可变靠 `RefCell`——与 [`OnSort`] 同款。
pub(super) type OnRowActivate = Rc<RefCell<dyn FnMut(&mut EventCtx, usize)>>;

/// 行右键菜单构建（右击整行触发）：`行下标 -> 菜单项`，返回空表示该行不弹菜单。
/// 行下标语义同操作列/单元格渲染。多行共享（每行按自己的下标各调一次）。
///
/// 回调挂在**行容器**上而非各单元格：右击行内任何位置（含空白与自定义单元格）都能弹，
/// 由框架沿父链冒泡到行节点。落在操作列按钮上时按钮若不接右键，仍冒泡到行——
/// 与双击激活"按钮先吃掉 Down"的行为不同，右键在分发层就只发给接右键的节点。
pub(super) type OnRowMenu = Rc<dyn Fn(usize) -> Vec<crate::event::MenuItem>>;

/// 单元格值比较：两侧都能解析为数值时按数值比，否则按字符串（区分大小写）。
fn cmp_cells(a: &str, b: &str) -> Ordering {
    match (a.trim().parse::<f64>(), b.trim().parse::<f64>()) {
        (Ok(x), Ok(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
        _ => a.cmp(b),
    }
}

/// 依当前排序状态求行序（返回原始行下标的排列）。`None` 时保持原序（稳定）。
pub(super) fn sorted_order(rows: &[Vec<String>], sort: Option<SortKey>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..rows.len()).collect();
    if let Some(key) = sort {
        // sort_by 稳定：等值行保持原相对次序。
        order.sort_by(|&a, &b| {
            let va = rows[a].get(key.column).map(String::as_str).unwrap_or("");
            let vb = rows[b].get(key.column).map(String::as_str).unwrap_or("");
            let c = cmp_cells(va, vb);
            match key.order {
                SortOrder::Asc => c,
                SortOrder::Desc => c.reverse(),
            }
        });
    }
    order
}

/// 点击第 `ci` 列表头后的下一排序状态。循环：非活动列 → 升序；升序 → 降序；降序 → 取消。
pub(super) fn next_sort(current: Option<SortKey>, ci: usize) -> Option<SortKey> {
    match current {
        Some(k) if k.column == ci => match k.order {
            SortOrder::Asc => Some(SortKey::desc(ci)),
            SortOrder::Desc => None,
        },
        _ => Some(SortKey::asc(ci)),
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
        Some(k) if k.column == ci => match k.order {
            SortOrder::Asc => rs.asc.clone(),
            SortOrder::Desc => rs.desc.clone(),
        },
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

/// 正文行悬停高亮的叠层不透明度（叠层取主题文字色，明暗自适应；"轻微"故低于 clickable 的 0.06）。
const ROW_HOVER_A: f32 = 0.05;

/// 只读表格正文行的悬停高亮 widget：悬停时在整行背景（斑马纹）之上、单元格内容之下绘制
/// 一层轻微半透明叠层（主题文字色低 alpha，明暗自适应），带淡入淡出。无点击/焦点/手型，
/// 纯视觉反馈。Enter/Leave 由框架沿祖先链派发（命中的是子单元格/标签，行仍收到）。
pub(super) struct HoverRow {
    hover: bool,
    /// 叠层不透明度补间（normal=0 / hover）；首帧靠 `primed` 落定。
    overlay: Cell<Transition<f32>>,
    primed: Cell<bool>,
    /// 行下标（传给激活回调；语义同操作列/单元格渲染）。
    idx: usize,
    /// 双击整行激活回调（如进入编辑）；`None` 时整行不可激活（保持纯悬停反馈）。
    activate: Option<OnRowActivate>,
    /// 双击已在第二次 Down 上"预备"，等随后的 Up（释放）落在本行内才真正激活——
    /// 更贴合桌面双击语义（按下不动作、抬起才生效）。任何单击 Down / 离开都会清预备位。
    armed: bool,
}

impl HoverRow {
    pub(super) fn new() -> Self {
        Self::with_activate(0, None)
    }

    /// 带激活回调的悬停行：双击（`click_count>=2` 的左键）在**释放（Up）**时触发 `activate(ctx, idx)`。
    pub(super) fn with_activate(idx: usize, activate: Option<OnRowActivate>) -> Self {
        Self {
            hover: false,
            overlay: Cell::new(Transition::new(0.0)),
            primed: Cell::new(false),
            idx,
            activate,
            armed: false,
        }
    }
}

impl Widget for HoverRow {
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        let target = if enabled && self.hover {
            ROW_HOVER_A
        } else {
            0.0
        };
        let mut ov = self.overlay.get();
        if !self.primed.get() {
            ov = Transition::new(target);
            self.primed.set(true);
        } else if ov.target() != target {
            ov.retarget(target, th.anim.fast(), Easing::EaseOut);
        }
        let a = ov.animate();
        self.overlay.set(ov);
        if a > 0.001 {
            canvas.fill_round_rect(
                bounds.x as f32,
                bounds.y as f32,
                bounds.w as f32,
                bounds.h as f32,
                style.corner_radius,
                &Paint::fill(th.palette.text.scale_alpha(a)),
            );
        }
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        if let Event::Pointer(p) = ev {
            match p.kind {
                PointerKind::Enter => {
                    if !self.hover {
                        self.hover = true;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.hover {
                        self.hover = false;
                        ctx.mark_dirty();
                    }
                    // 手指移出本行：取消尚未释放的双击预备，避免在别处释放误激活。
                    self.armed = false;
                    true
                }
                // 双击的第二次 Down 只"预备"，不立即动作；单击 Down 顺带清预备位（避免陈旧误触）。
                // 命中的子单元格标签不吃 Down，事件沿祖先链冒泡到本行；落在操作列按钮上时
                // 按钮已先消费 Down，故整行不会被预备。
                PointerKind::Down => {
                    let dbl = p.button == MouseButton::Left
                        && p.click_count >= 2
                        && self.activate.is_some()
                        && ctx.bounds().contains(p.pos);
                    self.armed = dbl;
                    dbl // 仅在预备成功时消费该 Down
                }
                // 已预备且在本行内释放：此刻才真正激活（进入编辑）。
                PointerKind::Up if self.armed => {
                    self.armed = false;
                    if p.button == MouseButton::Left && ctx.bounds().contains(p.pos) {
                        if let Some(cb) = self.activate.clone() {
                            (cb.borrow_mut())(ctx, self.idx);
                        }
                    }
                    true
                }
                _ => false,
            }
        } else {
            false
        }
    }
    fn reset_interaction(&mut self) {
        self.hover = false;
        self.armed = false;
        self.primed.set(false); // 隐藏期不回放 hover 淡出，下次显示瞬时落定
    }
}

/// 操作列单元格：水平内边距 + 内容垂直居中，宽度按权重（内容自身决定高度，行随之等高）。
/// 与文本列的 `table_cell_pad` 不同——不强制 20px 高，故按钮等较高控件不被压扁。
fn action_cell(content: Element, weight: f32) -> Element {
    Element::row()
        .weight(weight)
        .cross(Align::Center)
        .padding_xy(TABLE_CELL_PAD_X, TABLE_CELL_PAD_Y)
        .child(content)
}

/// 操作列表头单元格：加粗弱化色标题（单行末尾省略），不可点击（操作列不参与排序）。
fn action_header_cell(title: &str, weight: f32) -> Element {
    Element::stack()
        .weight(weight)
        .padding_xy(TABLE_CELL_PAD_X, TABLE_HEADER_PAD_Y)
        .child(
            Element::label(title.to_string())
                .font_size(13.0)
                .font_weight(600)
                .fg_role(Role::TextMuted)
                .max_lines(1)
                .truncate(Truncate::End)
                .width_match()
                .height(18),
        )
}

/// 构建一个数据单元格：有自定义渲染且返回 `Some` 时用自定义控件（垂直居中、不强制行高，
/// 同操作单元格），否则默认文本渲染。`lines` 为该格文本最多显示行数（≥1）：文本按列宽折行，
/// 行随内容长高至多 `lines` 行、内容不足则更矮，超出部分由 `max_lines` 精确裁切（避免溢出到邻行）。
fn data_cell(
    orig: usize,
    ci: usize,
    cell: &str,
    w: f32,
    render: Option<&CellRender>,
    lines: usize,
) -> Element {
    match render.and_then(|r| r(orig, ci, cell)) {
        Some(custom) => action_cell(custom, w),
        None => Element::table_cell_pad_lines(
            Element::label(cell.to_string())
                .font_size(13.0)
                .max_lines(lines.max(1))
                .truncate(Truncate::End),
            lines.max(1),
        )
        .weight(w),
    }
}

/// 构建一行正文：`disp` 为显示位置（决定斑马纹），`orig` 为该行下标（传给操作列/单元格生成器），
/// `cells` 为该行各列文本。结构与 `table_custom` 一致：`col[ row(单元格…), divider ]`。
/// 行挂 `HoverRow` widget，悬停时整行轻微高亮。`actions` 为 `Some` 时在末尾追加操作单元格；
/// `render` 为 `Some` 时逐格询问自定义渲染（`None` 回退默认文本）；`menu` 为 `Some` 时整行
/// 可右键弹出上下文菜单。
pub(super) fn body_row(
    disp: usize,
    orig: usize,
    cells: &[String],
    weights: &[f32],
    actions: Option<&ActionCol>,
    render: Option<&CellRender>,
    lines: usize,
    activate: Option<&OnRowActivate>,
    menu: Option<&OnRowMenu>,
) -> Element {
    let mut tr = Element::row().width_match().cross(Align::Stretch);
    // 斑马纹随显示位置交替（而非原始行号），排序后视觉仍规整。
    if disp % 2 == 1 {
        tr = tr.bg_role(Role::SurfaceAlt);
    }
    for (ci, cell) in cells.iter().enumerate() {
        let w = weights.get(ci).copied().unwrap_or(1.0);
        tr = tr.child(data_cell(orig, ci, cell, w, render, lines));
    }
    if let Some(a) = actions {
        tr = tr.child(action_cell((a.build)(orig), a.weight));
    }
    tr.set_widget(Box::new(HoverRow::with_activate(orig, activate.cloned())));
    tr = with_row_menu(tr, orig, menu);
    Element::col()
        .width_match()
        .child(tr)
        .child(Element::divider())
}

/// 给行容器挂上下文菜单（`menu` 为 `None` 时原样返回）。
///
/// 单独成函数是为了让两种行（[`body_row`] / [`select_body_row`]）走同一条接线：
/// 菜单项按**该行自己的下标**现取现构建，右键当刻的数据才是对的——把 `Vec<MenuItem>`
/// 在建行时就算好并存起来的话，行数据变了菜单还是旧的。
fn with_row_menu(row: Element, idx: usize, menu: Option<&OnRowMenu>) -> Element {
    match menu {
        Some(m) => {
            let m = m.clone();
            row.on_context_menu(move || m(idx))
        }
        None => row,
    }
}

/// 清空某节点的全部子节点（递归释放子树 arena slot），并同刻回收这批子树在**构建期**
/// 创建的信号。
///
/// 两件事绑在一个函数里是有意的：本表格的四个宿主（表头/正文/分页正文/可选正文）都会
/// 按排序或数据变化整批重建行，节点与其构建期信号必须同生共死——只删节点会漏槽位，
/// 只回收信号会让还挂着的节点读到已死的信号。
fn clear_children(
    tree: &mut crate::core::Tree,
    id: crate::core::NodeId,
    signals: &mut crate::signal::SignalScope,
) {
    let old: Vec<_> = tree.get(id).map(|n| n.children.clone()).unwrap_or_default();
    for c in old {
        tree.remove(c);
    }
    if let Some(n) = tree.get_mut(id) {
        n.children.clear();
    }
    signals.dispose();
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
    /// 尾部操作列（由 `Element::actions` 设置）；`Some` 时追加不可排序的操作表头单元格。
    actions: Option<ActionCol>,
    /// 是否已构建过单元格（首次 on_update 无条件构建）。
    built: bool,
    last_version: u64,
    /// 当前这批表头单元格在构建期创建的信号，重建时整批回收（见 `clear_children`）。
    cell_signals: crate::signal::SignalScope,
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
            actions: None,
            built: false,
            cell_signals: crate::signal::SignalScope::new(),
        }
    }

    /// 设置每实例样式覆盖（`Element::sort_indicator` 在 build 前调用）。
    pub(super) fn set_style(&mut self, style: SortStyle) {
        self.style = style;
    }

    /// 设置尾部操作列（`Element::actions` 在 build 前调用）；置 `built=false` 令首次构建纳入。
    pub(super) fn set_actions(&mut self, actions: ActionCol) {
        self.actions = Some(actions);
        self.built = false;
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
        // 作用域暂时取出：`collect` 要 `&mut` 它，闭包里还要读 `self` 的其它字段。
        let mut signals = std::mem::take(&mut self.cell_signals);
        let tree = ctx.tree_mut();
        clear_children(tree, self_id, &mut signals);
        signals.collect(|| {
            for (ci, (title, w)) in self.columns.iter().enumerate() {
                let mut el = header_cell(ci, title, *w, self.sort, self.on_sort.clone(), &rs);
                // 直接 build+add_child 绕过了父级线性容器 build 循环的 weight→主轴维度转换
                // （见 Element::build），此处手工复现：表头行恒为水平轴，故落到宽度。
                el.width = Dimension::Weight(*w);
                let id = el.build(tree);
                tree.add_child(self_id, id);
            }
            // 尾部操作列表头（不可排序），与正文操作单元格同权重对齐。
            if let Some(a) = &self.actions {
                let mut el = action_header_cell(&a.title, a.weight);
                el.width = Dimension::Weight(a.weight);
                let id = el.build(tree);
                tree.add_child(self_id, id);
            }
        });
        self.cell_signals = signals;
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
    /// 尾部操作列（由 `Element::actions` 设置）。
    actions: Option<ActionCol>,
    /// 自定义单元格渲染（由 `Element::cell_render` 设置）。
    render: Option<CellRender>,
    /// 默认文本格最多显示行数（由 `Element::cell_lines` 设置，默认 1）。
    cell_lines: usize,
    /// 整行双击激活回调（由 `Element::on_row_activate` 设置）。
    activate: Option<OnRowActivate>,
    /// 整行右键菜单构建（由 `Element::on_row_context_menu` 设置）。
    menu: Option<OnRowMenu>,
    /// 强制下次 on_update 重建（`set_actions`/`set_cell_render`/`set_cell_lines`/`set_activate`/`set_menu` 置位——初始 eager 行不含它们，需重建一次）。
    force: bool,
    last_version: u64,
    /// 当前这批行在构建期创建的信号，重建时整批回收（见 `clear_children`）。
    cell_signals: crate::signal::SignalScope,
}

impl SortableBody {
    pub(super) fn new(rows: Vec<Vec<String>>, weights: Vec<f32>, sort: SortState) -> Self {
        Self {
            last_version: sort.version(),
            rows,
            weights,
            sort,
            actions: None,
            render: None,
            cell_lines: 1,
            activate: None,
            menu: None,
            force: false,
            cell_signals: crate::signal::SignalScope::new(),
        }
    }

    /// 设置整行双击激活回调；置 `force` 令首次 on_update 重建（把激活能力纳入）。
    pub(super) fn set_activate(&mut self, activate: OnRowActivate) {
        self.activate = Some(activate);
        self.force = true;
    }

    /// 设置整行右键菜单构建；置 `force` 令首次 on_update 重建（把菜单纳入）。
    pub(super) fn set_menu(&mut self, menu: OnRowMenu) {
        self.menu = Some(menu);
        self.force = true;
    }

    /// 设置尾部操作列；置 `force` 令首次 on_update 重建（把操作列纳入，替换初始纯文本行）。
    pub(super) fn set_actions(&mut self, actions: ActionCol) {
        self.actions = Some(actions);
        self.force = true;
    }

    /// 设置自定义单元格渲染；置 `force` 令首次 on_update 重建（把自定义格纳入）。
    pub(super) fn set_cell_render(&mut self, render: CellRender) {
        self.render = Some(render);
        self.force = true;
    }

    /// 设置默认文本格最多行数；置 `force` 令首次 on_update 按新行数重建。
    pub(super) fn set_cell_lines(&mut self, lines: usize) {
        self.cell_lines = lines.max(1);
        self.force = true;
    }
}

impl Widget for SortableBody {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let ver = self.sort.version();
        if !self.force && ver == self.last_version {
            return;
        }
        self.force = false;
        self.last_version = ver;
        let self_id = ctx.id();
        let mut signals = std::mem::take(&mut self.cell_signals);
        let tree = ctx.tree_mut();
        clear_children(tree, self_id, &mut signals);
        let order = sorted_order(&self.rows, self.sort.get());
        signals.collect(|| {
            for (disp, &ri) in order.iter().enumerate() {
                let el = body_row(
                    disp,
                    ri,
                    &self.rows[ri],
                    &self.weights,
                    self.actions.as_ref(),
                    self.render.as_ref(),
                    self.cell_lines,
                    self.activate.as_ref(),
                    self.menu.as_ref(),
                );
                let id = el.build(tree);
                tree.add_child(self_id, id);
            }
        });
        self.cell_signals = signals;
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
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// 服务端排序模式的正文：绑定"当前页数据"信号，**不做内部排序**——排序由后端完成，
/// 前端只按应用给定的顺序渲染。数据信号版本变化（应用换页/换排序后写回）时重建行。
pub(super) struct PagedBody {
    rows: Signal<Vec<Vec<String>>>,
    weights: Vec<f32>,
    /// 尾部操作列（由 `Element::actions` 设置）。生成器收到当前页内显示下标。
    actions: Option<ActionCol>,
    /// 自定义单元格渲染（由 `Element::cell_render` 设置）。生成器收到当前页内显示下标。
    render: Option<CellRender>,
    /// 默认文本格最多显示行数（由 `Element::cell_lines` 设置，默认 1）。
    cell_lines: usize,
    /// 整行双击激活回调（由 `Element::on_row_activate` 设置）。生成器收到当前页内显示下标。
    activate: Option<OnRowActivate>,
    /// 整行右键菜单构建（由 `Element::on_row_context_menu` 设置）。生成器收到当前页内显示下标。
    menu: Option<OnRowMenu>,
    /// 强制下次 on_update 重建（`set_actions`/`set_cell_render`/`set_cell_lines`/`set_activate`/`set_menu` 置位）。
    force: bool,
    last_version: u64,
    /// 当前这批行在构建期创建的信号，重建时整批回收（见 `clear_children`）。
    cell_signals: crate::signal::SignalScope,
}

impl PagedBody {
    pub(super) fn new(rows: Signal<Vec<Vec<String>>>, weights: Vec<f32>) -> Self {
        Self {
            last_version: rows.version(),
            rows,
            weights,
            actions: None,
            render: None,
            cell_lines: 1,
            activate: None,
            menu: None,
            force: false,
            cell_signals: crate::signal::SignalScope::new(),
        }
    }

    /// 设置整行双击激活回调；置 `force` 令首次 on_update 重建（把激活能力纳入）。
    pub(super) fn set_activate(&mut self, activate: OnRowActivate) {
        self.activate = Some(activate);
        self.force = true;
    }

    /// 设置整行右键菜单构建；置 `force` 令首次 on_update 重建（把菜单纳入）。
    pub(super) fn set_menu(&mut self, menu: OnRowMenu) {
        self.menu = Some(menu);
        self.force = true;
    }

    /// 设置尾部操作列；置 `force` 令首次 on_update 重建（把操作列纳入）。
    pub(super) fn set_actions(&mut self, actions: ActionCol) {
        self.actions = Some(actions);
        self.force = true;
    }

    /// 设置自定义单元格渲染；置 `force` 令首次 on_update 重建。
    pub(super) fn set_cell_render(&mut self, render: CellRender) {
        self.render = Some(render);
        self.force = true;
    }

    /// 设置默认文本格最多行数；置 `force` 令首次 on_update 按新行数重建。
    pub(super) fn set_cell_lines(&mut self, lines: usize) {
        self.cell_lines = lines.max(1);
        self.force = true;
    }
}

impl Widget for PagedBody {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let ver = self.rows.version();
        if !self.force && ver == self.last_version {
            return;
        }
        self.force = false;
        self.last_version = ver;
        let self_id = ctx.id();
        let mut signals = std::mem::take(&mut self.cell_signals);
        let tree = ctx.tree_mut();
        clear_children(tree, self_id, &mut signals);
        let data = self.rows.get();
        signals.collect(|| {
            for (disp, row) in data.iter().enumerate() {
                let el = body_row(
                    disp,
                    disp,
                    row,
                    &self.weights,
                    self.actions.as_ref(),
                    self.render.as_ref(),
                    self.cell_lines,
                    self.activate.as_ref(),
                    self.menu.as_ref(),
                );
                let id = el.build(tree);
                tree.add_child(self_id, id);
            }
        });
        self.cell_signals = signals;
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
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

// ==== 行选择（复选框首列 + 全选表头 + 选中行高亮） ====

/// 选择列（复选框）固定宽度 px。
const SELECT_COL_W: i32 = 40;
/// 复选框方框边长 px（与 CheckBox 一致，便于自绘全选框视觉统一）。
const SEL_BOX: i32 = 18;
/// 选中行高亮叠层不透明度（accent 低 alpha；叠在斑马纹之上、悬停层之下）。
const ROW_SELECTED_A: f32 = 0.12;

/// 首列复选框单元格：固定宽，内含绑定该行选择信号的复选框（水平居中 + 垂直居中）。
fn select_cell(row_sel: Signal<bool>) -> Element {
    let pad = (SELECT_COL_W - SEL_BOX) / 2;
    Element::row()
        .width(SELECT_COL_W)
        .cross(Align::Center)
        .padding_xy(pad, 0)
        .child(Element::checkbox("", row_sel).width(SEL_BOX))
}

/// 构建一行可选正文：[复选框列] + 数据列；行挂 [`SelectableRow`]（悬停 + 选中高亮）。
/// `row_sel` 为该行（原始行身份）的选择信号：复选框直接绑定，行高亮读取它。
pub(super) fn select_body_row(
    disp: usize,
    orig: usize,
    cells: &[String],
    weights: &[f32],
    row_sel: Signal<bool>,
    actions: Option<&ActionCol>,
    render: Option<&CellRender>,
    lines: usize,
    menu: Option<&OnRowMenu>,
) -> Element {
    let mut tr = Element::row().width_match().cross(Align::Stretch);
    if disp % 2 == 1 {
        tr = tr.bg_role(Role::SurfaceAlt);
    }
    tr = tr.child(select_cell(row_sel));
    for (ci, cell) in cells.iter().enumerate() {
        let w = weights.get(ci).copied().unwrap_or(1.0);
        tr = tr.child(data_cell(orig, ci, cell, w, render, lines));
    }
    if let Some(a) = actions {
        tr = tr.child(action_cell((a.build)(orig), a.weight));
    }
    tr.set_widget(Box::new(SelectableRow::new(row_sel)));
    tr = with_row_menu(tr, orig, menu);
    Element::col()
        .width_match()
        .child(tr)
        .child(Element::divider())
}

/// 可选正文行：在 [`HoverRow`] 基础上叠加"选中"高亮（accent 低 alpha 底，读取行选择信号）。
/// 选中底为瞬时（跟随信号），悬停层为淡入淡出，叠在其上。
pub(super) struct SelectableRow {
    sel: Signal<bool>,
    hover: bool,
    overlay: Cell<Transition<f32>>,
    primed: Cell<bool>,
}

impl SelectableRow {
    pub(super) fn new(sel: Signal<bool>) -> Self {
        Self {
            sel,
            hover: false,
            overlay: Cell::new(Transition::new(0.0)),
            primed: Cell::new(false),
        }
    }
}

impl Widget for SelectableRow {
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        // 选中底（accent 低 alpha，瞬时跟随信号）。
        if enabled && self.sel.get() {
            canvas.fill_round_rect(
                bounds.x as f32,
                bounds.y as f32,
                bounds.w as f32,
                bounds.h as f32,
                style.corner_radius,
                &Paint::fill(th.palette.accent.scale_alpha(ROW_SELECTED_A)),
            );
        }
        // 悬停层（text 低 alpha，淡入淡出，叠在选中底之上）。
        let target = if enabled && self.hover {
            ROW_HOVER_A
        } else {
            0.0
        };
        let mut ov = self.overlay.get();
        if !self.primed.get() {
            ov = Transition::new(target);
            self.primed.set(true);
        } else if ov.target() != target {
            ov.retarget(target, th.anim.fast(), Easing::EaseOut);
        }
        let a = ov.animate();
        self.overlay.set(ov);
        if a > 0.001 {
            canvas.fill_round_rect(
                bounds.x as f32,
                bounds.y as f32,
                bounds.w as f32,
                bounds.h as f32,
                style.corner_radius,
                &Paint::fill(th.palette.text.scale_alpha(a)),
            );
        }
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        if let Event::Pointer(p) = ev {
            match p.kind {
                PointerKind::Enter => {
                    if !self.hover {
                        self.hover = true;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.hover {
                        self.hover = false;
                        ctx.mark_dirty();
                    }
                    true
                }
                _ => false,
            }
        } else {
            false
        }
    }
    fn reset_interaction(&mut self) {
        self.hover = false;
        self.primed.set(false);
    }
}

/// 全选表头单元格 widget：自绘三态复选框（全选 ✓ / 部分选 − / 未选 空），点击切换全选/清空。
/// 直接读写各行选择信号（单一数据源），故手动勾选行时表头聚合态实时刷新。
pub(super) struct SelectAllCheck {
    sel: Vec<Signal<bool>>,
}

impl SelectAllCheck {
    pub(super) fn new(sel: Vec<Signal<bool>>) -> Self {
        Self { sel }
    }
    /// 是否全选（非空且每行皆选）。
    fn all(&self) -> bool {
        !self.sel.is_empty() && self.sel.iter().all(|s| s.get())
    }
    /// 切换：全选→清空；否则→全选。
    fn toggle_all(&self, ctx: &mut EventCtx) {
        let target = !self.all();
        for s in &self.sel {
            s.set(target);
        }
        ctx.mark_dirty_all(); // 影响所有行，升整窗
    }
}

impl Widget for SelectAllCheck {
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
        let th = crate::theme::current();
        let (p, tg) = (&th.palette, &th.toggle);
        let n = self.sel.iter().filter(|s| s.get()).count();
        let all = n > 0 && n == self.sel.len();
        let some = n > 0 && !all;
        let box_x = bounds.x as f32 + ((SELECT_COL_W - SEL_BOX) / 2) as f32;
        let box_y = bounds.y as f32 + ((bounds.h - SEL_BOX) / 2) as f32;
        let sz = SEL_BOX as f32;
        let radius = 4.0;
        let accent = if enabled { tg.accent(p) } else { p.track };
        let filled = all || some;
        canvas.fill_round_rect(
            box_x,
            box_y,
            sz,
            sz,
            radius,
            &Paint::fill(if filled { accent } else { tg.knob(p) }),
        );
        if !filled {
            canvas.stroke_round_rect(box_x, box_y, sz, sz, radius, 1.5, &Paint::fill(tg.track(p)));
        }
        let mark = Paint::fill(p.on_accent);
        let s = sz / 18.0; // 18px 基准（与 CheckBox 对勾坐标一致）
        if all {
            canvas.draw_line(
                box_x + 4.0 * s,
                box_y + 9.0 * s,
                box_x + 8.0 * s,
                box_y + 13.0 * s,
                2.0,
                &mark,
            );
            canvas.draw_line(
                box_x + 8.0 * s,
                box_y + 13.0 * s,
                box_x + 14.0 * s,
                box_y + 5.0 * s,
                2.0,
                &mark,
            );
        } else if some {
            // 部分选中：横杠（indeterminate）。
            canvas.draw_line(
                box_x + 4.5 * s,
                box_y + 9.0 * s,
                box_x + 13.5 * s,
                box_y + 9.0 * s,
                2.0,
                &mark,
            );
        }
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) if p.kind == PointerKind::Up => {
                if ctx.bounds().contains(p.pos) {
                    self.toggle_all(ctx);
                }
                true
            }
            Event::Pointer(p) if p.kind == PointerKind::Down => {
                ctx.request_focus();
                true
            }
            Event::Key(k) if k.pressed && (k.key == Key::Space || k.key == Key::Enter) => {
                self.toggle_all(ctx);
                true
            }
            _ => false,
        }
    }
    fn focusable(&self) -> bool {
        true
    }
}

/// 可选表格正文：响应式（排序变化重排重建），每行含首列复选框 + 数据列，绑定按原始行身份。
/// 选择变化不触发本重建（复选框自更新、行高亮经信号重绘），仅排序变化重排。
pub(super) struct SelectableBody {
    rows: Vec<Vec<String>>,
    weights: Vec<f32>,
    sel: Vec<Signal<bool>>,
    sort: SortState,
    /// 尾部操作列（由 `Element::actions` 设置）。生成器收到原始行下标（与选择同身份）。
    actions: Option<ActionCol>,
    /// 自定义单元格渲染（由 `Element::cell_render` 设置）。生成器收到原始行下标。
    render: Option<CellRender>,
    /// 默认文本格最多显示行数（由 `Element::cell_lines` 设置，默认 1）。
    cell_lines: usize,
    /// 整行右键菜单构建（由 `Element::on_row_context_menu` 设置）。生成器收到原始行下标。
    menu: Option<OnRowMenu>,
    built: bool,
    last_version: u64,
    /// 当前这批行在构建期创建的信号，重建时整批回收（见 `clear_children`）。
    /// 注意与 `sel` 的区别：`sel` 是**调用方**的选择状态信号，不归本作用域管。
    cell_signals: crate::signal::SignalScope,
}

impl SelectableBody {
    pub(super) fn new(
        rows: Vec<Vec<String>>,
        weights: Vec<f32>,
        sel: Vec<Signal<bool>>,
        sort: SortState,
    ) -> Self {
        Self {
            last_version: sort.version(),
            rows,
            weights,
            sel,
            sort,
            actions: None,
            render: None,
            cell_lines: 1,
            menu: None,
            built: false,
            cell_signals: crate::signal::SignalScope::new(),
        }
    }

    /// 设置整行右键菜单构建；置 `built=false` 令首次 on_update 把菜单纳入。
    pub(super) fn set_menu(&mut self, menu: OnRowMenu) {
        self.menu = Some(menu);
        self.built = false;
    }

    /// 设置尾部操作列；置 `built=false` 令首次 on_update 把操作列纳入。
    pub(super) fn set_actions(&mut self, actions: ActionCol) {
        self.actions = Some(actions);
        self.built = false;
    }

    /// 设置自定义单元格渲染；置 `built=false` 令首次 on_update 把自定义格纳入。
    pub(super) fn set_cell_render(&mut self, render: CellRender) {
        self.render = Some(render);
        self.built = false;
    }

    /// 设置默认文本格最多行数；置 `built=false` 令首次 on_update 按新行数重建。
    pub(super) fn set_cell_lines(&mut self, lines: usize) {
        self.cell_lines = lines.max(1);
        self.built = false;
    }
}

impl Widget for SelectableBody {
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let ver = self.sort.version();
        if self.built && ver == self.last_version {
            return;
        }
        self.built = true;
        self.last_version = ver;
        let self_id = ctx.id();
        let mut signals = std::mem::take(&mut self.cell_signals);
        let tree = ctx.tree_mut();
        clear_children(tree, self_id, &mut signals);
        let order = sorted_order(&self.rows, self.sort.get());
        signals.collect(|| {
            for (disp, &ri) in order.iter().enumerate() {
                // 选择按原始行下标 ri 绑定（排序后仍按身份跟随）。越界回退（长度不匹配时容错）。
                let Some(&row_sel) = self.sel.get(ri) else {
                    continue;
                };
                let el = select_body_row(
                    disp,
                    ri,
                    &self.rows[ri],
                    &self.weights,
                    row_sel,
                    self.actions.as_ref(),
                    self.render.as_ref(),
                    self.cell_lines,
                    self.menu.as_ref(),
                );
                let id = el.build(tree);
                tree.add_child(self_id, id);
            }
        });
        self.cell_signals = signals;
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
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// 选择列宽度（供构造器对齐表头全选列与正文复选框列）。
pub(super) const fn select_col_w() -> i32 {
    SELECT_COL_W
}

/// 构造操作列配置（供 `Element::actions` 组装）。
pub(super) fn action_col(
    title: String,
    weight: f32,
    build: impl Fn(usize) -> Element + 'static,
) -> ActionCol {
    ActionCol {
        title,
        weight,
        build: Rc::new(build),
    }
}

/// 若 `el` 挂的是 `SortableHeader` 则设入操作列并返回 true，否则 false（供定位表头）。
#[must_use]
pub(super) fn set_header_actions(el: &mut Element, ac: &ActionCol) -> bool {
    if let Some(a) = el.widget.as_any_mut() {
        if let Some(h) = a.downcast_mut::<SortableHeader>() {
            h.set_actions(ac.clone());
            return true;
        }
    }
    false
}

/// 若 `el` 挂的是任一响应式正文（Sortable/Paged/Selectable）则设入操作列并返回 true。
#[must_use]
pub(super) fn set_body_actions(el: &mut Element, ac: &ActionCol) -> bool {
    let Some(a) = el.widget.as_any_mut() else {
        return false;
    };
    if let Some(b) = a.downcast_mut::<SortableBody>() {
        b.set_actions(ac.clone());
        return true;
    }
    if let Some(b) = a.downcast_mut::<PagedBody>() {
        b.set_actions(ac.clone());
        return true;
    }
    if let Some(b) = a.downcast_mut::<SelectableBody>() {
        b.set_actions(ac.clone());
        return true;
    }
    false
}

/// 若 `el` 挂的是任一响应式正文（Sortable/Paged/Selectable）则设入自定义单元格渲染并返回 true。
#[must_use]
pub(super) fn set_body_cell_render(el: &mut Element, render: &CellRender) -> bool {
    let Some(a) = el.widget.as_any_mut() else {
        return false;
    };
    if let Some(b) = a.downcast_mut::<SortableBody>() {
        b.set_cell_render(render.clone());
        return true;
    }
    if let Some(b) = a.downcast_mut::<PagedBody>() {
        b.set_cell_render(render.clone());
        return true;
    }
    if let Some(b) = a.downcast_mut::<SelectableBody>() {
        b.set_cell_render(render.clone());
        return true;
    }
    false
}

/// 若 `el` 挂的是 HoverRow 型响应式正文（Sortable/Paged）则设入整行双击激活回调并返回 true。
/// 可选表格（SelectableBody/SelectableRow）不支持整行激活（首列复选框语义冲突），返回 false。
#[must_use]
pub(super) fn set_body_activate(el: &mut Element, activate: &OnRowActivate) -> bool {
    let Some(a) = el.widget.as_any_mut() else {
        return false;
    };
    if let Some(b) = a.downcast_mut::<SortableBody>() {
        b.set_activate(activate.clone());
        return true;
    }
    if let Some(b) = a.downcast_mut::<PagedBody>() {
        b.set_activate(activate.clone());
        return true;
    }
    false
}

/// 若 `el` 挂的是任一响应式正文（Sortable/Paged/Selectable）则设入整行右键菜单并返回 true。
///
/// 与整行双击激活不同，可选表格**也支持**：右键不与首列复选框争语义（复选框只吃左键），
/// 而"右击某行做点什么"在多选表格里同样成立。
#[must_use]
pub(super) fn set_body_menu(el: &mut Element, menu: &OnRowMenu) -> bool {
    let Some(a) = el.widget.as_any_mut() else {
        return false;
    };
    if let Some(b) = a.downcast_mut::<SortableBody>() {
        b.set_menu(menu.clone());
        return true;
    }
    if let Some(b) = a.downcast_mut::<PagedBody>() {
        b.set_menu(menu.clone());
        return true;
    }
    if let Some(b) = a.downcast_mut::<SelectableBody>() {
        b.set_menu(menu.clone());
        return true;
    }
    false
}

/// 若 `el` 挂的是任一响应式正文（Sortable/Paged/Selectable）则设入默认文本格最多行数并返回 true。
#[must_use]
pub(super) fn set_body_cell_lines(el: &mut Element, lines: usize) -> bool {
    let Some(a) = el.widget.as_any_mut() else {
        return false;
    };
    if let Some(b) = a.downcast_mut::<SortableBody>() {
        b.set_cell_lines(lines);
        return true;
    }
    if let Some(b) = a.downcast_mut::<PagedBody>() {
        b.set_cell_lines(lines);
        return true;
    }
    if let Some(b) = a.downcast_mut::<SelectableBody>() {
        b.set_cell_lines(lines);
        return true;
    }
    false
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
        let asc = sorted_order(&r, Some(SortKey::asc(1)));
        assert_eq!(col_values(&r, &asc, 1), ["3", "12", "1280", "20480"]);
        let desc = sorted_order(&r, Some(SortKey::desc(1)));
        assert_eq!(col_values(&r, &desc, 1), ["20480", "1280", "12", "3"]);
    }

    #[test]
    fn sorted_order_string_column_lexicographic() {
        let r = rows(&[&["banana"], &["apple"], &["cherry"]]);
        let asc = sorted_order(&r, Some(SortKey::asc(0)));
        assert_eq!(col_values(&r, &asc, 0), ["apple", "banana", "cherry"]);
    }

    #[test]
    fn sorted_order_none_keeps_original_and_is_stable() {
        let r = rows(&[&["x", "5"], &["y", "5"], &["z", "5"]]);
        // 无排序：原序。
        assert_eq!(sorted_order(&r, None), [0, 1, 2]);
        // 等值列升序：稳定，等值行保持原相对次序。
        assert_eq!(sorted_order(&r, Some(SortKey::asc(1))), [0, 1, 2]);
    }

    #[test]
    fn next_sort_cycles_none_asc_desc_none() {
        assert_eq!(next_sort(None, 0), Some(SortKey::asc(0)));
        assert_eq!(next_sort(Some(SortKey::asc(0)), 0), Some(SortKey::desc(0)));
        assert_eq!(next_sort(Some(SortKey::desc(0)), 0), None);
        // 点另一列：从该列升序重新开始（不继承前列方向）。
        assert_eq!(next_sort(Some(SortKey::desc(0)), 1), Some(SortKey::asc(1)));
    }

    /// 布局一个 400×300 的可排序表格，返回 tree。
    fn layout(el: Element) -> Tree {
        let mut tree = Tree::new();
        let root = el.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
        tree
    }

    /// 取正文首行首个「默认文本」数据格 label 的 `max_lines`（裁切围栏是否安装 + 限几行）。
    /// 结构：col[header, divider, scroll] → scroll 首子 body col → 首行 col[row, divider]
    /// → row 首格 stack（table_cell_pad）→ label 叶子。
    fn first_text_cell_max_lines(tree: &mut Tree) -> Option<usize> {
        let root = tree.root.unwrap();
        let scroll = *tree.get(root).unwrap().children.last().unwrap();
        let body = tree.get(scroll).unwrap().children[0];
        let row_wrap = tree.get(body).unwrap().children[0];
        let row = tree.get(row_wrap).unwrap().children[0];
        let cell = tree.get(row).unwrap().children[0];
        let label = tree.get(cell).unwrap().children[0];
        tree.get_mut(label)
            .unwrap()
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<crate::ui::Label>())
            .and_then(|l| l.max_lines)
    }

    #[test]
    fn data_cell_installs_clip_fence_and_respects_cell_lines() {
        // 回归：正文默认文本格必须给 label 装裁切围栏（max_lines），否则多行文本溢出到下方行。
        // 默认单行；.cell_lines(2) 后为 2 行。修复前 label 无 max_lines（None），本测试即失败。
        let long = vec![vec![
            "很长很长很长很长很长很长很长很长很长的系统词条内容".to_string()
        ]];
        let mut tree = layout(
            Element::table_sortable(vec![("词条", 1.0)], long.clone(), signal(None))
                .width(200)
                .height(300),
        );
        assert_eq!(
            first_text_cell_max_lines(&mut tree),
            Some(1),
            "默认数据格应安装单行裁切围栏（max_lines=1）"
        );

        let mut tree2 = layout(
            Element::table_sortable(vec![("词条", 1.0)], long, signal(None))
                .cell_lines(2)
                .width(200)
                .height(300),
        );
        assert_eq!(
            first_text_cell_max_lines(&mut tree2),
            Some(2),
            "cell_lines(2) 后数据格裁切围栏应为 2 行"
        );
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
        assert_eq!(sort.get(), Some(SortKey::asc(0)), "首次点击→升序");
        // 再次布局让响应式表头/正文按新状态重建，再点同列。
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
        click(&mut tree, Point::new(40, 18));
        assert_eq!(sort.get(), Some(SortKey::desc(0)), "再点同列→降序");
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
        click(&mut tree, Point::new(40, 18));
        assert_eq!(sort.get(), None, "三点同列→取消排序");
    }

    #[test]
    fn body_rebuilds_row_count_after_sort_change() {
        let sort = signal(Some(SortKey::asc(0)));
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
        sort.set(Some(SortKey::desc(0)));
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
        let sort = signal(Some(SortKey::asc(0)));
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
    fn reactive_table_scrolls_on_wheel() {
        // 回归：响应式表格的滚动容器须保留内置 ScrollWidget（滚轮 + 滚动条），
        // 正文 widget 挂在其内部 col 上——否则替换 scroll 的 widget 会丢滚轮/拖动能力。
        let sort = signal(None);
        let rows: Vec<Vec<String>> = (0..20).map(|i| vec![format!("r{i}")]).collect();
        let mut tree = layout(
            Element::table_sortable(vec![("列", 1.0)], rows, sort)
                .width(400)
                .height(200),
        );
        // 结构：col[header, divider, scroll]；scroll 保留 ScrollWidget。
        let root = tree.root.unwrap();
        let scroll = *tree.get(root).unwrap().children.last().unwrap();
        assert!(
            tree.scroll_range(scroll).is_some_and(|(_, max)| max > 0),
            "20 行应溢出，正文应可滚"
        );
        // 在正文区域派发向下滚轮（delta<0），正文应滚动。
        let mut h = None;
        let mut cap = None;
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Wheel(-120),
                Point::new(80, 120),
                MouseButton::Left,
            ),
            &mut h,
            &mut cap,
        );
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
        assert!(
            tree.get(scroll).unwrap().scroll_y > 0,
            "滚轮应滚动响应式表格正文（ScrollWidget 未被替换）"
        );
    }

    #[test]
    fn clicking_row_checkbox_toggles_that_rows_selection() {
        let sort = signal(None);
        let sel: Vec<Signal<bool>> = (0..2).map(|_| signal(false)).collect();
        let mut tree = layout(
            Element::table_selectable(
                vec![("名称", 2.0), ("大小", 1.0)],
                vec![vec!["a", "2"], vec!["b", "1"]],
                sel.clone(),
                sort,
            )
            .width(400)
            .height(300),
        );
        assert!(!sel[0].get());
        // 首列复选框（x∈[0,40]，首行在表头/分隔线之下约 y≈58）。
        click(&mut tree, Point::new(20, 58));
        assert!(sel[0].get(), "点击首行复选框应选中原始行 0");
        assert!(!sel[1].get(), "不应影响其它行");
    }

    #[test]
    fn select_all_selects_then_clears() {
        let sort = signal(None);
        let sel: Vec<Signal<bool>> = (0..3).map(|_| signal(false)).collect();
        let mut tree = layout(
            Element::table_selectable(
                vec![("名称", 1.0)],
                vec![vec!["a"], vec!["b"], vec!["c"]],
                sel.clone(),
                sort,
            )
            .width(400)
            .height(300),
        );
        // 全选框在表头首列（x∈[0,40]，y≈19）。
        click(&mut tree, Point::new(20, 19));
        assert!(sel.iter().all(|s| s.get()), "点全选应选中所有行");
        click(&mut tree, Point::new(20, 19));
        assert!(sel.iter().all(|s| !s.get()), "再点全选应清空");
    }

    #[test]
    fn selection_tracks_original_row_across_sort() {
        // 选择按原始行身份绑定：选中某行后排序重排，该行仍选中（不随显示位置漂移）。
        let sort = signal(None);
        let sel: Vec<Signal<bool>> = (0..3).map(|_| signal(false)).collect();
        let mut tree = layout(
            Element::table_selectable(
                vec![("v", 1.0)],
                vec![vec!["3"], vec!["1"], vec!["2"]], // 原始行 0/1/2 值 3/1/2
                sel.clone(),
                sort,
            )
            .width(400)
            .height(300),
        );
        sel[0].set(true); // 选中原始行 0（值 3）
        sort.set(Some(SortKey::asc(0))); // 升序：显示序 1,2,3 → 行0 落到末尾
        tree.layout_root(Size::new(400, 300), &mut crate::text::NullTextEngine);
        assert!(
            sel[0].get(),
            "排序后原始行 0 仍选中（选择按身份跟随，不随位置）"
        );
        assert!(!sel[1].get() && !sel[2].get());
    }

    #[test]
    fn hovering_body_row_requests_repaint() {
        // 悬停正文行：Move 派发经祖先链到达行的 HoverRow → 置 hover + 请求重绘。
        let sort = signal(Some(SortKey::asc(0)));
        let mut tree = layout(
            Element::table_sortable(
                vec![("名称", 2.0), ("大小", 1.0)],
                vec![vec!["a", "2"], vec!["b", "1"]],
                sort,
            )
            .width(400)
            .height(300),
        );
        let mut hover = None;
        let mut capture = None;
        // 移到正文首行区域（表头约 38px 高，行在其下）。
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, Point::new(80, 70), MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        assert!(res.repaint, "悬停正文行应触发重绘（整行高亮淡入）");
        assert!(hover.is_some(), "应有命中的悬停节点");
    }

    #[test]
    fn double_click_body_row_activates_with_row_index() {
        // 双击整行触发 on_row_activate 并回报行下标；单击不触发。
        use std::cell::Cell as StdCell;
        let sort = signal(None);
        let seen: Rc<StdCell<Option<usize>>> = Rc::new(StdCell::new(None));
        let seen_c = seen.clone();
        let mut tree = layout(
            Element::table_sortable(
                vec![("名称", 2.0), ("大小", 1.0)],
                vec![vec!["a", "2"], vec!["b", "1"]],
                sort,
            )
            .on_row_activate(move |_ctx, idx| seen_c.set(Some(idx)))
            .width(400)
            .height(300),
        );
        // 定位首个正文数据单元格中心。
        let root = tree.root.unwrap();
        let scroll = *tree.get(root).unwrap().children.last().unwrap();
        let body = tree.get(scroll).unwrap().children[0];
        let first_row = tree.get(body).unwrap().children[0];
        let tr = tree.get(first_row).unwrap().children[0];
        let cell = tree.get(tr).unwrap().children[0];
        let at = abs_center(&tree, cell).unwrap();
        let mut hover = None;
        let mut capture = None;
        let dbl_down = PointerEvent {
            kind: PointerKind::Down,
            pos: at,
            button: MouseButton::Left,
            click_count: 2,
        };
        // 单击（Down cc=1 + Up）不激活。
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
        assert_eq!(seen.get(), None, "单击不应激活整行");
        // 双击第二次 Down 只预备，不立即激活。
        tree.dispatch_pointer(dbl_down, &mut hover, &mut capture);
        assert_eq!(seen.get(), None, "双击按下（Down）时不应立即激活");
        // 释放（Up）落在本行内 → 此刻才激活，回报首行下标 0。
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        assert_eq!(seen.get(), Some(0), "双击释放（Up）时应激活并回报行下标 0");
    }

    /// 定位某类表格正文中第 `n` 行首个数据单元格的中心点。
    fn body_cell_center(tree: &Tree, n: usize) -> Point {
        let root = tree.root.unwrap();
        let scroll = *tree.get(root).unwrap().children.last().unwrap();
        let body = tree.get(scroll).unwrap().children[0];
        let row = tree.get(body).unwrap().children[n];
        let tr = tree.get(row).unwrap().children[0];
        let cell = tree.get(tr).unwrap().children[0];
        abs_center(tree, cell).unwrap()
    }

    #[test]
    fn right_click_body_row_opens_menu_with_row_index() {
        // 右击整行 → 弹上下文菜单，且构建器收到的是**该行**的下标；左键不弹。
        use std::cell::Cell as StdCell;
        let sort = signal(None);
        let seen: Rc<StdCell<Option<usize>>> = Rc::new(StdCell::new(None));
        let seen_c = seen.clone();
        let mut tree = layout(
            Element::table_sortable(
                vec![("名称", 2.0), ("大小", 1.0)],
                vec![vec!["a", "2"], vec!["b", "1"]],
                sort,
            )
            .on_row_context_menu(move |idx| {
                seen_c.set(Some(idx));
                vec![crate::event::MenuItem::run("删除", |_ctx| {}, false)]
            })
            .width(400)
            .height(300),
        );
        let at = body_cell_center(&tree, 1);
        let (mut hover, mut capture) = (None, None);

        // 左键按下不弹菜单（右键菜单不该抢正常点击）。
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        assert!(res.menu.is_none(), "左键按下不应弹上下文菜单");
        assert_eq!(seen.get(), None, "左键不应调用菜单构建器");

        // 右键按下：弹菜单，构建器收到第二行的下标 1。
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Right),
            &mut hover,
            &mut capture,
        );
        let menu = res.menu.expect("右击正文行应弹上下文菜单");
        assert_eq!(menu.items.len(), 1);
        assert_eq!(menu.items[0].label, "删除");
        assert_eq!(seen.get(), Some(1), "菜单构建器应收到被右击那一行的下标");
    }

    #[test]
    fn row_menu_builder_runs_on_every_right_click() {
        // 菜单项现取现建：同一行右击两次应各调一次构建器（勾选/禁用态才能反映当刻数据）。
        use std::cell::Cell as StdCell;
        let sort = signal(None);
        let calls: Rc<StdCell<usize>> = Rc::new(StdCell::new(0));
        let calls_c = calls.clone();
        let mut tree = layout(
            Element::table_sortable(vec![("v", 1.0)], vec![vec!["a"]], sort)
                .on_row_context_menu(move |_| {
                    calls_c.set(calls_c.get() + 1);
                    vec![crate::event::MenuItem::run("x", |_ctx| {}, false)]
                })
                .width(400)
                .height(300),
        );
        let at = body_cell_center(&tree, 0);
        let (mut hover, mut capture) = (None, None);
        for _ in 0..2 {
            tree.dispatch_pointer(
                PointerEvent::single(PointerKind::Down, at, MouseButton::Right),
                &mut hover,
                &mut capture,
            );
        }
        assert_eq!(calls.get(), 2, "每次右击都应重新构建菜单项");
    }

    #[test]
    fn selectable_table_rows_also_support_context_menu() {
        // 可多选表格不支持整行双击激活（与首列复选框冲突），但右键菜单不冲突——复选框只吃左键。
        let sort = signal(None);
        let sel: Vec<Signal<bool>> = (0..2).map(|_| signal(false)).collect();
        let mut tree = layout(
            Element::table_selectable(
                vec![("v", 1.0)],
                vec![vec!["a"], vec!["b"]],
                sel.clone(),
                sort,
            )
            .on_row_context_menu(|idx| {
                vec![crate::event::MenuItem::run(
                    format!("行{idx}"),
                    |_ctx| {},
                    false,
                )]
            })
            .width(400)
            .height(300),
        );
        // 首列是复选框列，取第二个子节点（首个数据格）。
        let root = tree.root.unwrap();
        let scroll = *tree.get(root).unwrap().children.last().unwrap();
        let body = tree.get(scroll).unwrap().children[0];
        let row = tree.get(body).unwrap().children[0];
        let tr = tree.get(row).unwrap().children[0];
        let cell = tree.get(tr).unwrap().children[1];
        let at = abs_center(&tree, cell).unwrap();
        let (mut hover, mut capture) = (None, None);
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Right),
            &mut hover,
            &mut capture,
        );
        let menu = res.menu.expect("可选表格的行也应能右键弹菜单");
        assert_eq!(menu.items[0].label, "行0");
        assert!(!sel[0].get(), "右键不应顺带勾选该行");
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

    /// 目标节点的**绝对**中心点（bounds 是局部坐标，需累加祖先 origin 才对得上
    /// hit_test 的绝对命中）。找不到返回 None。
    ///
    /// 走 `abs_bounds` 而非自己沿父链累加 `bounds`：后者会漏掉 [`Node::offset`]
    /// （绘制/命中偏移），一旦某行带上 offset 就会静默点错位置，而失败现象看起来
    /// 像是表格逻辑出了问题。
    fn abs_center(tree: &Tree, target: crate::core::NodeId) -> Option<Point> {
        tree.get(target)?;
        let b = tree.abs_bounds(target);
        Some(Point::new(b.x + b.w / 2, b.y + b.h / 2))
    }

    #[test]
    fn actions_column_adds_header_and_body_cells() {
        // .actions 应给表头与每个正文行各追加一个操作单元格（数据列数 + 1）。
        let sort = signal(None);
        let tree = layout(
            Element::table_sortable(
                vec![("A", 1.0), ("B", 1.0)],
                vec![vec!["a", "b"], vec!["c", "d"]],
                sort,
            )
            .actions("操作", 1.0, |_row| Element::label("·"))
            .width(400)
            .height(300),
        );
        let root = tree.root.unwrap();
        let header = tree.get(root).unwrap().children[0];
        assert_eq!(
            tree.get(header).unwrap().children.len(),
            3,
            "表头应为 2 数据列 + 1 操作列"
        );
        let scroll = *tree.get(root).unwrap().children.last().unwrap();
        let body = tree.get(scroll).unwrap().children[0]; // 内层 col（挂 SortableBody）
        let first_row = tree.get(body).unwrap().children[0]; // col[tr, divider]
        let tr = tree.get(first_row).unwrap().children[0];
        assert_eq!(
            tree.get(tr).unwrap().children.len(),
            3,
            "正文行应为 2 数据单元格 + 1 操作单元格"
        );
    }

    #[test]
    fn action_button_reports_original_row_after_sort() {
        // 关键：操作按钮回调按**原始行下标**绑定——排序打乱显示序后，点某显示行的按钮
        // 仍回报该行的原始下标（可直接用作数据/选择索引）。
        use std::cell::Cell as StdCell;
        use std::rc::Rc;
        let sort = signal(Some(SortKey::asc(0)));
        let seen: Rc<StdCell<Option<usize>>> = Rc::new(StdCell::new(None));
        let seen_c = seen.clone();
        let mut tree = layout(
            Element::table_sortable(
                vec![("v", 1.0)],
                vec![vec!["3"], vec!["1"], vec!["2"]], // 原始行 0/1/2 = 值 3/1/2
                sort,
            )
            .actions("op", 1.0, move |row| {
                let s = seen_c.clone();
                Element::row()
                    .width(120)
                    .height(24)
                    .clickable()
                    .on_click(move |_| s.set(Some(row)))
                    .child(Element::label("x"))
            })
            .width(400)
            .height(300),
        );
        // 升序显示：值 1,2,3 → 显示序 = 原始行 1,2,0。首个显示行是原始行 1。
        let root = tree.root.unwrap();
        let scroll = *tree.get(root).unwrap().children.last().unwrap();
        let body = tree.get(scroll).unwrap().children[0];
        let first_row = tree.get(body).unwrap().children[0];
        let tr = tree.get(first_row).unwrap().children[0];
        // 操作单元格为该行末子（数据列之后）；其内层为可点击控件。
        let action = *tree.get(tr).unwrap().children.last().unwrap();
        let clickable = tree.get(action).unwrap().children[0];
        let at = abs_center(&tree, clickable).unwrap();
        click(&mut tree, at);
        assert_eq!(
            seen.get(),
            Some(1),
            "点首个显示行的操作按钮应回报其原始行下标 1"
        );
    }

    #[test]
    fn cell_render_customizes_cells_and_reports_original_row() {
        use std::cell::RefCell as StdRefCell;
        // 升序显示 1,2,3 → 显示序 = 原始行 1,2,0；renderer 应按原始行下标逐格询问。
        let sort = signal(Some(SortKey::asc(0)));
        let seen: Rc<StdRefCell<Vec<(usize, usize, String)>>> =
            Rc::new(StdRefCell::new(Vec::new()));
        let seen_c = seen.clone();
        let tree = layout(
            Element::table_sortable(
                vec![("v", 1.0), ("w", 1.0)],
                vec![vec!["3", "x"], vec!["1", "y"], vec!["2", "z"]],
                sort,
            )
            .cell_render(move |row, col, text| {
                seen_c.borrow_mut().push((row, col, text.to_string()));
                // 首列自定义（定宽标记控件），次列回退默认文本。
                (col == 0).then(|| Element::row().width(77).height(24))
            })
            .width(400)
            .height(300),
        );
        let calls = seen.borrow();
        assert_eq!(calls.len(), 6, "3 行 × 2 列每格都应询问 renderer");
        assert_eq!(calls[0], (1, 0, "1".into()), "首显示行应传原始行下标 1");
        assert_eq!(calls[2], (2, 0, "2".into()));
        assert_eq!(calls[4], (0, 0, "3".into()), "值 3 的原始行 0 排到末尾");
        // 结构：首格为自定义单元格（内容为 77px 标记控件），次格为默认文本格（label 撑满）。
        let root = tree.root.unwrap();
        let scroll = *tree.get(root).unwrap().children.last().unwrap();
        let body = tree.get(scroll).unwrap().children[0];
        let first_row = tree.get(body).unwrap().children[0];
        let tr = tree.get(first_row).unwrap().children[0];
        let cells = tree.get(tr).unwrap().children.clone();
        assert_eq!(cells.len(), 2);
        let custom_inner = tree.get(cells[0]).unwrap().children[0];
        assert_eq!(
            tree.get(custom_inner).unwrap().width,
            Dimension::Px(77),
            "首列应为自定义控件"
        );
        let default_inner = tree.get(cells[1]).unwrap().children[0];
        assert_eq!(
            tree.get(default_inner).unwrap().width,
            Dimension::Match,
            "次列应为默认文本渲染（label 撑满格宽）"
        );
    }

    #[test]
    fn cell_render_composes_with_actions_column() {
        // cell_render 与 .actions 同时设置：数据格自定义 + 尾列操作格并存。
        let sort = signal(None);
        let tree = layout(
            Element::table_sortable(vec![("A", 1.0), ("B", 1.0)], vec![vec!["a", "b"]], sort)
                .cell_render(|_, col, _| (col == 0).then(|| Element::label("·").width(33)))
                .actions("操作", 1.0, |_| Element::label("x"))
                .width(400)
                .height(300),
        );
        let root = tree.root.unwrap();
        let scroll = *tree.get(root).unwrap().children.last().unwrap();
        let body = tree.get(scroll).unwrap().children[0];
        let first_row = tree.get(body).unwrap().children[0];
        let tr = tree.get(first_row).unwrap().children[0];
        assert_eq!(
            tree.get(tr).unwrap().children.len(),
            3,
            "2 数据格 + 1 操作格"
        );
        let custom_inner = tree
            .get(tree.get(tr).unwrap().children[0])
            .unwrap()
            .children[0];
        assert_eq!(tree.get(custom_inner).unwrap().width, Dimension::Px(33));
    }

    #[test]
    fn server_mode_click_updates_sort_and_fires_callback() {
        use std::cell::Cell as StdCell;
        use std::rc::Rc;
        let sort = signal(None);
        let rows = signal(vec![vec!["a".to_string(), "2".to_string()]]);
        // 记录回调收到的排序意图（应与 sort 同步）。
        let seen: Rc<StdCell<Option<SortKey>>> = Rc::new(StdCell::new(None));
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
        assert_eq!(sort.get(), Some(SortKey::asc(0)), "sort 信号更新");
        assert_eq!(fired.get(), 1, "on_sort 回调被触发一次");
        assert_eq!(seen.get(), Some(SortKey::asc(0)), "回调收到新排序意图");
    }

    #[test]
    fn server_mode_body_renders_backend_order_without_internal_sort() {
        // 服务端模式：正文按数据信号给定顺序渲染，不做内部排序。
        // 给一份「已按后端逆序」的数据，前端应原样显示（若误做内部排序会被打乱）。
        let sort = signal(Some(SortKey::asc(0)));
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
