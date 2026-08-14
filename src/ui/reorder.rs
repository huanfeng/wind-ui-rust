//! 拖拽重排列表（`Element::reorder_list` 的内部驱动）。
//!
//! 两个 widget 协作，共享一份 [`ReorderCtl`]：
//! - [`DragHandle`]：挂在每行的手柄叶子上。按下时把自己的 `NodeId` 写进共享状态、
//!   捕获指针，然后**返回 false 让事件继续冒泡**。
//! - [`ReorderList`]：挂在列容器上。从冒泡上来的事件驱动状态机，在 `on_update`
//!   里推进补间、写各行 [`Node::offset`](crate::core::Node::offset)。
//!
//! 之所以让最内层的手柄"声明意图"、由外层列表"消费"，是因为反过来做（列表自己判断
//! 落点是不是某行的手柄）需要反查子孙节点的相对位置，很脆。指针捕获生效时事件目标
//! 锁定为手柄，但冒泡链仍是 手柄 → 行 → 列表，故两者天然解耦。
//!
//! 位移全部走 `Node::offset`（绘制/命中偏移）而非改 `bounds`：`bounds` 是布局结果，
//! 任何一次 relayout 都会重算它，把临时视觉状态写进去必被冲掉。
//!
//! 外部只通过 `Element::reorder_list` 使用，无需直接构造本模块类型。

use std::cell::RefCell;
use std::rc::Rc;

use super::Layout;
use crate::anim::{Easing, Transition};
use crate::core::{EventCtx, NodeId, Widget};
use crate::event::{CursorShape, Event, Key, MouseButton, PointerKind};
use crate::geometry::{Point, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::style::{Brush, Shadow, Style};
use crate::text::TextEngine;

/// 进入拖动的位移阈值（逻辑 px）：按下手柄后须移动超过它才算拖动，
/// 之下只是一次普通点击——避免手抖把点击变成微小重排。
const DRAG_THRESHOLD: i32 = 4;

/// 手柄点阵高度（逻辑 px）。宽度走主题 `ReorderTheme::handle_w`。
const HANDLE_H: i32 = 24;

/// 拖拽状态机阶段。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    /// 手柄已按下，位移尚未超过阈值。
    Pressed,
    /// 拖动中：被拖行跟随指针，其余行让位。
    Dragging,
    /// 已松手，正在播回落动画；动画结束才提交顺序。
    Settling,
}

/// 提交模式：决定重排后由谁负责子节点顺序。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CommitMode {
    /// 内部直接重排 `children`，**不重建行** → 行内控件状态天然保留。
    Children,
    /// 不动 `children`，只回调；由应用改数据信号触发重建。配 `list_signal` 用。
    Callback,
}

/// 被拖行浮起时被临时改写的样式，松手后逐字段还原。
struct SavedStyle {
    bg: Option<Brush>,
    shadow: Option<Shadow>,
    corner: f32,
}

/// 手柄与列表共享的拖拽状态。
pub(super) struct Ctl {
    phase: Phase,
    /// 手柄按下时写入的意图：(手柄 NodeId, 按下点)。由 `ReorderList` 消费后清空。
    pending: Option<(NodeId, Point)>,
    /// Esc 取消请求（手柄按键处写入，列表在下一帧 `on_update` 消费）。
    cancel: bool,
    /// 拖动源行下标（用于计算目标位与回调实参）。
    from: usize,
    /// 被拖行的节点 id。样式与 `raised` 的还原一律按它定位，不用 `from`——
    /// 拖动期间上游可能重建子节点，下标会指向另一个节点。
    row_id: Option<NodeId>,
    /// 当前目标插入位。
    to: usize,
    /// 按下时的指针 y（逻辑坐标）。
    start_y: i32,
    /// 当前指针 y。
    cur_y: i32,
    /// 拖动开始时各行的槽位快照 `(y, h)`，相对列表容器。支持不等高行。
    slots: Vec<(i32, i32)>,
    /// 各行的位移补间（与 `children` 同序）。
    tweens: Vec<Transition<f32>>,
    /// 被拖行的原样式，松手后还原。
    saved: Option<SavedStyle>,
}

impl Ctl {
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            pending: None,
            cancel: false,
            from: 0,
            row_id: None,
            to: 0,
            start_y: 0,
            cur_y: 0,
            slots: Vec::new(),
            tweens: Vec::new(),
            saved: None,
        }
    }
    /// 当前拖动位移。
    fn drag_dy(&self) -> i32 {
        self.cur_y - self.start_y
    }
}

pub(super) type ReorderCtl = Rc<RefCell<Ctl>>;

pub(super) fn new_ctl() -> ReorderCtl {
    Rc::new(RefCell::new(Ctl::new()))
}

/// 构造一枚绑定到 `ctl` 的拖动手柄元素。
///
/// 数据驱动模式下每次重建行都要现造一枚——手柄 widget 与节点一一对应，
/// 不能在多行间共享。
pub(super) fn handle_element(ctl: &ReorderCtl) -> super::Element {
    super::Element::base(Layout::None).widget(DragHandle::new(ctl.clone()))
}

// ----------------------------------------------------------------- RowSource

/// 行数据源：绑定数据的版本变化时，按新数据重建列容器的子节点。
///
/// 做成**非泛型 trait 对象内嵌进 [`ReorderList`]**，而不是像 `DynList` 那样独立成一个
/// widget——重排与重建都必须挂在同一个列容器上，而一个节点只能挂一个 widget。
/// `ReorderList` 因此保持非泛型，泛型只落在本 trait 的实现里。
pub(super) trait RowSource {
    /// 数据版本变了就重建 children；返回是否真的重建过。
    fn sync(&mut self, ctx: &mut EventCtx) -> bool;
}

/// `Signal<Vec<T>>` 驱动的行源：每行由 `row_fn(item, handle)` 构建，
/// 手柄元素放在行内哪个位置由调用方决定（见 `Element::reorder_list_signal`）。
struct SignalRows<T: Clone + 'static> {
    data: Signal<Vec<T>>,
    row_fn: Rc<dyn Fn(T, super::Element) -> super::Element>,
    ctl: ReorderCtl,
    last_version: u64,
    /// 当前这批行构建期创建的信号，下轮重建整批回收（同 `DynList`）。
    rows: crate::signal::SignalScope,
}

impl<T: Clone + 'static> RowSource for SignalRows<T> {
    fn sync(&mut self, ctx: &mut EventCtx) -> bool {
        let ver = self.data.version();
        if ver == self.last_version {
            return false;
        }
        self.last_version = ver;

        let self_id = ctx.id();
        let items = self.data.get();
        // 先拆字段：`rows.collect` 借 `&mut self.rows`，闭包里还要读 `row_fn`/`ctl`。
        let Self {
            row_fn, ctl, rows, ..
        } = self;
        let tree = ctx.tree_mut();
        let old: Vec<NodeId> = tree
            .get(self_id)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        for c in old {
            tree.remove(c);
        }
        if let Some(n) = tree.get_mut(self_id) {
            n.children.clear();
        }
        // 旧行节点已删，其构建期信号同刻回收。
        rows.dispose();
        rows.collect(|| {
            for item in items {
                let el = row_fn(item, handle_element(ctl));
                let child = el.build(tree);
                tree.add_child(self_id, child);
            }
        });
        true
    }
}

/// 装箱一个信号行源，供 `Element::reorder_list_signal` 注入 [`ReorderList`]。
pub(super) fn signal_rows<T: Clone + 'static>(
    data: Signal<Vec<T>>,
    row_fn: Rc<dyn Fn(T, super::Element) -> super::Element>,
    ctl: ReorderCtl,
) -> Box<dyn RowSource> {
    Box::new(SignalRows {
        last_version: data.version(),
        data,
        row_fn,
        ctl,
        rows: crate::signal::SignalScope::new(),
    })
}

/// 目标插入位 = 中心线在被拖行视觉中心**之上**的其他行数量。
///
/// 用"中心线越过"而非"边界越过"：后者在落点贴近行边界时会随 1px 抖动来回翻转。
fn target_index(slots: &[(i32, i32)], from: usize, center: i32) -> usize {
    let mut to = 0;
    for (i, &(y, h)) in slots.iter().enumerate() {
        if i != from && y + h / 2 < center {
            to += 1;
        }
    }
    to
}

/// 重堆叠让位：把 `from` 行抽出、插入到 `to` 位，返回**每行相对其原位的 y 偏移**。
///
/// 不假设行等高——表单行常带副标题/徽章，高度天然不一致。等高列表是本算法的特例，
/// 故不写两套。行间距按首两行的间隙推算（列容器 `spacing` 统一）。
fn stack_offsets(slots: &[(i32, i32)], from: usize, to: usize) -> Vec<i32> {
    let n = slots.len();
    let mut offs = vec![0; n];
    if n == 0 || from >= n {
        return offs;
    }
    let mut order: Vec<usize> = (0..n).collect();
    let item = order.remove(from);
    order.insert(to.min(n - 1), item);

    let gap = if n >= 2 {
        slots[1].0 - (slots[0].0 + slots[0].1)
    } else {
        0
    };
    let mut y = slots[0].0;
    for &idx in &order {
        offs[idx] = y - slots[idx].0;
        y += slots[idx].1 + gap;
    }
    offs
}

/// 识别宿主在捕获被系统夺走时补发的合成 `Up`：它带的是远处坐标
/// `(-1_000_000, -1_000_000)`（见 `UiHost::on_capture_lost`）。
///
/// 用一个远小于任何真实窗口坐标的阈值判定，而不是精确比对那个魔数——既能与
/// "拖到窗口上方"的正常负坐标（至多几千）区分，也不必与宿主的具体取值耦合。
fn is_synthetic_capture_lost(y: i32) -> bool {
    y <= -100_000
}

// ---------------------------------------------------------------- DragHandle

/// 行首/行尾的拖动手柄：自绘 2×3 圆点，按下即捕获指针并向列表声明拖动意图。
///
/// 圆点是自绘的而非 `⠿` 字形——盲文点字符的字体覆盖不可靠，缺字会渲染成豆腐块。
pub struct DragHandle {
    ctl: ReorderCtl,
    hover: bool,
    /// 本手柄正被按住（画高亮）。
    active: bool,
}

impl DragHandle {
    pub(super) fn new(ctl: ReorderCtl) -> Self {
        Self {
            ctl,
            hover: false,
            active: false,
        }
    }
}

impl Widget for DragHandle {
    /// 槽宽取自主题而非构建期常量——换肤后重新 measure 即生效，无需重建元素树。
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(crate::theme::current().reorder.handle_w(), HANDLE_H)
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
        let th = crate::theme::current();
        let ro = &th.reorder;
        let color = if !enabled {
            th.palette.text_disabled
        } else if self.active || self.hover {
            ro.handle_hover(&th.palette)
        } else {
            ro.handle(&th.palette)
        };
        let cx = bounds.x as f32 + bounds.w as f32 / 2.0;
        let cy = bounds.y as f32 + bounds.h as f32 / 2.0;
        let (r, gap_x, gap_y) = (1.5, 5.0, 5.0);
        let paint = Paint::fill(color);
        for row in 0..3 {
            for col in 0..2 {
                canvas.fill_circle(
                    cx + (col as f32 - 0.5) * gap_x,
                    cy + (row as f32 - 1.0) * gap_y,
                    r,
                    &paint,
                );
            }
        }
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            // 拖动/回落期间按 Esc 取消：只置标志，由列表在下一帧 on_update 里统一还原。
            // 手柄够不到各行节点，也不该越权去改它们——那是列表的职责。
            Event::Key(k) if k.pressed && k.key == Key::Escape => {
                let mut c = self.ctl.borrow_mut();
                if c.phase == Phase::Idle {
                    return false;
                }
                c.cancel = true;
                drop(c);
                // 取消后手柄不应停留在按下高亮态（指针可能仍按着，但拖动已作废）。
                self.active = false;
                // 这里**不能**释放捕获：键盘路径丢弃 capture 副作用（`DispatchResult`
                // 没有 capture 字段，`Tree::dispatch_key` 也不消费 `o.capture`），
                // 调了也是空操作。捕获由 `ReorderList` 的 `Up` 兜底臂在用户松手时归还
                // ——指针路径才传播它。同理 `finish()` 里调也无效：`on_update` 相位的
                // EventOutcome 除 toast 外一律被丢弃（见 `Tree::call_on_update`）。
                ctx.mark_layout_dirty();
                true
            }
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    self.hover = true;
                    ctx.mark_dirty();
                    false
                }
                PointerKind::Leave => {
                    self.hover = false;
                    ctx.mark_dirty();
                    false
                }
                PointerKind::Down if p.button == MouseButton::Left => {
                    self.ctl.borrow_mut().pending = Some((ctx.id(), p.pos));
                    self.active = true;
                    // 捕获指针：拖出手柄范围后事件仍送达本节点（进而冒泡到列表）。
                    ctx.capture();
                    // 取焦点是为了让 Esc 有处可去。
                    ctx.request_focus();
                    ctx.mark_dirty();
                    // 刻意不消费：列表在冒泡链上游接管逻辑。
                    false
                }
                PointerKind::Up => {
                    if self.active {
                        self.active = false;
                        ctx.mark_dirty();
                    }
                    false
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }

    fn focusable(&self) -> bool {
        true
    }

    fn reset_interaction(&mut self) {
        self.hover = false;
        self.active = false;
    }
}

// --------------------------------------------------------------- ReorderList

/// 重排回调：`(ctx, 原下标, 新下标)`。
pub type OnReorder = Box<dyn FnMut(&mut EventCtx, usize, usize)>;

/// 挂在列容器上的拖拽重排驱动。
pub struct ReorderList {
    ctl: ReorderCtl,
    mode: CommitMode,
    on_reorder: Option<OnReorder>,
    /// 数据驱动时的行源（`Element::reorder_list_signal`）；静态行为 `None`。
    source: Option<Box<dyn RowSource>>,
}

impl ReorderList {
    pub(super) fn new(ctl: ReorderCtl, mode: CommitMode) -> Self {
        Self {
            ctl,
            mode,
            on_reorder: None,
            source: None,
        }
    }
    pub(super) fn set_on_reorder(&mut self, f: OnReorder) {
        self.on_reorder = Some(f);
    }
    pub(super) fn set_mode(&mut self, mode: CommitMode) {
        self.mode = mode;
    }
    pub(super) fn set_source(&mut self, source: Box<dyn RowSource>) {
        self.source = Some(source);
    }

    /// 数据版本变了就重建行。**拖动中一律不调**——重建会把槽位快照、补间下标与
    /// 浮起样式指向的节点整批换掉，让位算法当场失准。版本不匹配会一直保留到落定，
    /// 由 [`finish`](Self::finish) 末尾补做。
    fn sync_source(&mut self, ctx: &mut EventCtx) {
        if let Some(s) = self.source.as_mut() {
            s.sync(ctx);
        }
    }

    /// 本列表的直接子节点（行）。
    fn rows(&self, ctx: &mut EventCtx) -> Vec<NodeId> {
        let self_id = ctx.id();
        ctx.tree_mut()
            .get(self_id)
            .map(|n| n.children.clone())
            .unwrap_or_default()
    }

    /// 从手柄节点沿父链上溯，定位它属于哪一行。
    fn row_index_of(&self, ctx: &mut EventCtx, handle: NodeId) -> Option<usize> {
        let self_id = ctx.id();
        let tree = ctx.tree_mut();
        let mut cur = handle;
        loop {
            let parent = tree.get(cur)?.parent?;
            if parent == self_id {
                return tree.get(self_id)?.children.iter().position(|&c| c == cur);
            }
            cur = parent;
        }
    }

    /// 进入拖动：快照各行槽位、初始化补间、把被拖行提为浮起态。
    fn begin_drag(&mut self, ctx: &mut EventCtx, y: i32) {
        let rows = self.rows(ctx);
        let slots: Vec<(i32, i32)> = rows
            .iter()
            .map(|&r| {
                ctx.tree_mut()
                    .get(r)
                    .map(|n| (n.bounds.y, n.bounds.h))
                    .unwrap_or((0, 0))
            })
            .collect();

        // 先校验被拖行存在，再落状态：反过来会在早退时留下"已进入 Dragging、
        // 却没有浮起行也没有 saved"的半吊子状态。
        let from = self.ctl.borrow().from;
        let Some(&row_id) = rows.get(from) else {
            return;
        };

        {
            let mut c = self.ctl.borrow_mut();
            c.phase = Phase::Dragging;
            c.cur_y = y;
            c.tweens = vec![Transition::new(0.0); slots.len()];
            c.slots = slots;
            c.to = from;
            c.row_id = Some(row_id);
        }

        // 浮起：raised 让它画在兄弟之上，不透明底色 + 投影把它从列表里"抬"出来。
        ctx.set_node_raised(row_id, true);
        let th = crate::theme::current();
        let (bg, shadow, corner) = (
            th.reorder.dragging_bg(&th.palette),
            th.reorder.shadow(),
            th.reorder.corner(&th.metrics),
        );
        if let Some(n) = ctx.tree_mut().get_mut(row_id) {
            let saved = SavedStyle {
                bg: n.style.bg.clone(),
                shadow: n.style.shadow,
                corner: n.style.corner_radius,
            };
            n.style.bg = Some(Brush::Solid(bg));
            n.style.shadow = Some(shadow);
            n.style.corner_radius = corner;
            self.ctl.borrow_mut().saved = Some(saved);
        }
    }

    /// 结束：还原浮起样式、清空所有偏移，必要时重排 children 并触发回调。
    fn finish(&mut self, ctx: &mut EventCtx, cancelled: bool) {
        let rows = self.rows(ctx);
        let (from, to, saved, row_id) = {
            let mut c = self.ctl.borrow_mut();
            c.phase = Phase::Idle;
            c.cancel = false;
            c.tweens.clear();
            c.slots.clear();
            (c.from, c.to, c.saved.take(), c.row_id.take())
        };

        // 还原浮起行的样式与层级——按 **NodeId** 而非下标定位。
        // 拖动期间若上游重建过子节点（DynList、visible_when 联动等），`from` 指向的
        // 已是另一个节点：那样既还原不了真正被改过的行（浮起底色与投影永久写死在它
        // 身上），又会把 raised 留在错误的行上让它一直盖住兄弟。
        if let Some(row_id) = row_id {
            ctx.set_node_raised(row_id, false);
            if let Some(s) = saved {
                if let Some(n) = ctx.tree_mut().get_mut(row_id) {
                    n.style.bg = s.bg;
                    n.style.shadow = s.shadow;
                    n.style.corner_radius = s.corner;
                }
            }
        }
        // 所有行归位：顺序的落实交给 children 重排或应用侧数据，视觉偏移必须清零。
        for &r in &rows {
            ctx.set_node_offset(r, Point::new(0, 0));
        }

        if !cancelled && from != to {
            if self.mode == CommitMode::Children {
                let self_id = ctx.id();
                if let Some(n) = ctx.tree_mut().get_mut(self_id) {
                    if from < n.children.len() {
                        let item = n.children.remove(from);
                        n.children.insert(to.min(n.children.len()), item);
                    }
                }
            }
            if let Some(cb) = self.on_reorder.as_mut() {
                cb(ctx, from, to);
            }
        }
        ctx.mark_layout_dirty();
        // 回调常在这里改数据信号（`Callback` 模式的全部意义）。**同一帧**就把新数据
        // 落成子节点，否则本帧偏移已清零、children 却还是旧序，会闪回一帧老顺序再跳正。
        self.sync_source(ctx);
    }
}

impl Widget for ReorderList {
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        let Event::Pointer(p) = ev else { return false };
        match p.kind {
            PointerKind::Down => {
                let pending = self.ctl.borrow_mut().pending.take();
                let Some((handle, pos)) = pending else {
                    return false;
                };
                let Some(row) = self.row_index_of(ctx, handle) else {
                    return false;
                };
                let mut c = self.ctl.borrow_mut();
                c.phase = Phase::Pressed;
                c.from = row;
                c.to = row;
                c.start_y = pos.y;
                c.cur_y = pos.y;
                true
            }
            PointerKind::Move => {
                let (phase, start_y) = {
                    let c = self.ctl.borrow();
                    (c.phase, c.start_y)
                };
                match phase {
                    Phase::Pressed => {
                        if (p.pos.y - start_y).abs() >= DRAG_THRESHOLD {
                            self.begin_drag(ctx, p.pos.y);
                            ctx.mark_layout_dirty();
                        }
                        true
                    }
                    Phase::Dragging => {
                        self.ctl.borrow_mut().cur_y = p.pos.y;
                        ctx.mark_layout_dirty();
                        true
                    }
                    _ => false,
                }
            }
            PointerKind::Up => {
                let phase = self.ctl.borrow().phase;
                match phase {
                    // 未超阈值：视作普通点击，什么都不改。
                    Phase::Pressed => {
                        self.ctl.borrow_mut().phase = Phase::Idle;
                        ctx.release_capture();
                        true
                    }
                    Phase::Dragging => {
                        let mut c = self.ctl.borrow_mut();
                        // 系统夺走捕获（Alt+Tab、别的窗口 SetCapture）时，宿主会补发一个
                        // 远处坐标的合成 Up。既有约定是"收尾/复位"而非"确认"（见
                        // `UiHost::on_capture_lost`：Slider 借它复位拖动），故这里必须
                        // 走取消——用户只是切了个窗口，设置项顺序不该被悄悄改掉。
                        if is_synthetic_capture_lost(p.pos.y) {
                            c.cancel = true;
                        }
                        // 先播回落动画，动画结束再提交——直接提交会让行瞬移，很跳。
                        c.phase = Phase::Settling;
                        let (from, dy) = (c.from, c.drag_dy());
                        if let Some(t) = c.tweens.get_mut(from) {
                            *t = Transition::new(dy as f32);
                        }
                        drop(c);
                        ctx.release_capture();
                        ctx.mark_layout_dirty();
                        true
                    }
                    // 回落中或已落定时才收到 Up（Esc 取消后用户才松手、或宿主补发的
                    // 合成 Up）：状态机无事可做，但**捕获必须在这里还掉**，否则会永久
                    // 泄漏、整窗失去响应。
                    Phase::Settling | Phase::Idle => {
                        ctx.release_capture();
                        false
                    }
                }
            }
            _ => false,
        }
    }

    /// 每帧（layout 前）推进补间并写各行偏移。
    ///
    /// 靠 `anim::request_relayout` 续帧而非 `request_repaint`：后者不触发 layout，
    /// 而 `on_update` 只在 `layout_root` 内广播，用错会直接断帧。
    fn on_update(&mut self, ctx: &mut EventCtx) {
        let (mut phase, cancel) = {
            let c = self.ctl.borrow();
            (c.phase, c.cancel)
        };
        if phase == Phase::Idle {
            // 空闲时才跟数据走：拖动中重建会打乱正在进行的让位（见 `sync_source`）。
            self.sync_source(ctx);
            return;
        }
        // Esc：目标位归回原位，走同一套回落动画，结束后不提交。
        if cancel {
            let mut c = self.ctl.borrow_mut();
            if c.phase == Phase::Dragging {
                let (from, dy) = (c.from, c.drag_dy());
                if let Some(t) = c.tweens.get_mut(from) {
                    *t = Transition::new(dy as f32);
                }
            }
            c.phase = Phase::Settling;
            c.to = c.from;
            drop(c);
            // 局部 phase 必须跟着更新：下面几处分支都据它决策，读旧值会让
            // `Pressed + Esc` 从早退分支直接返回——既不收尾也不安排下一帧，
            // 状态机卡在 Settling 且 cancel 标志一直挂着，整个列表从此拖不动。
            phase = Phase::Settling;
        }
        // 按下但未起拖：没有槽位快照、也没有动画可播，直接收尾。
        if phase == Phase::Pressed {
            if cancel {
                self.finish(ctx, true);
            }
            return;
        }

        let rows = self.rows(ctx);
        let th = crate::theme::current();
        let (fast, normal) = (th.anim.fast(), th.anim.normal());

        let (targets, from, dragging) = {
            let mut c = self.ctl.borrow_mut();
            if c.slots.is_empty() {
                (Vec::new(), c.from, false)
            } else {
                let dragging = c.phase == Phase::Dragging;
                if dragging {
                    // 被拖行视觉中心 → 目标插入位。
                    let (y, h) = c.slots[c.from];
                    let center = y + c.drag_dy() + h / 2;
                    c.to = target_index(&c.slots, c.from, center);
                }
                let targets = stack_offsets(&c.slots, c.from, c.to);
                (targets, c.from, dragging)
            }
        };
        // 快照缺失（行数中途归零等）：无从计算让位，直接取消收尾，不留悬空状态。
        if targets.is_empty() {
            self.finish(ctx, true);
            return;
        }

        let drag_dy = self.ctl.borrow().drag_dy() as f32;
        let mut active = false;
        for (i, &row) in rows.iter().enumerate() {
            let Some(&target) = targets.get(i) else {
                continue;
            };
            let value = {
                let mut c = self.ctl.borrow_mut();
                let Some(t) = c.tweens.get_mut(i) else {
                    continue;
                };
                if dragging && i == from {
                    // 被拖行直接跟指针：补间会产生橡皮筋般的滞后感。
                    *t = Transition::new(drag_dy);
                    drag_dy
                } else {
                    let dur = if i == from { fast } else { normal };
                    if (t.target() - target as f32).abs() > f32::EPSILON {
                        t.retarget(target as f32, dur, Easing::EaseOut);
                    }
                    if t.is_active() {
                        active = true;
                    }
                    t.value()
                }
            };
            ctx.set_node_offset(row, Point::new(0, value.round() as i32));
        }

        if active {
            crate::anim::request_relayout();
        } else if !dragging {
            // 回落动画播完 → 落定提交。
            //
            // `!dragging` 这个前提不能省：拖动中让位补间收敛后 `active` 同样是 false，
            // 但那只表示"此刻没有行在动"，不表示拖动结束——少了它，用户按住不动
            // 几百毫秒，行就会自己提交落定。
            self.finish(ctx, cancel);
        }
        // 拖动中且无补间活跃：什么都不做。指针移动与 Esc 各自会 mark_layout_dirty
        // 唤醒下一帧，无需恒续帧——`offset` 进了签名，每帧都会被判为结构变化而整窗重绘，
        // 按住不动却满帧重绘是纯粹的浪费（违反「空闲零 CPU」）。
    }

    /// 本方法实际不会被调用：`Widget::measure` 只作用于 `Layout::None` 的叶子，
    /// 而本 widget 挂在列容器上，尺寸由容器布局算法决定。留空实现仅为满足 trait。
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::ZERO
    }

    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }

    /// 纯容器：自身无视觉，命中应穿透给行与行内控件。
    fn hit_opaque(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 三行等高 40、间距 0。
    fn even_slots() -> Vec<(i32, i32)> {
        vec![(0, 40), (40, 40), (80, 40)]
    }

    #[test]
    fn target_index_uses_center_crossing() {
        let s = even_slots();
        // 第一行原地未动：其中心 20 在第二行中心 60 之上 → 目标仍是 0。
        assert_eq!(target_index(&s, 0, 20), 0);
        // 中心刚过第二行中心（60）→ 落到 1。
        assert_eq!(target_index(&s, 0, 61), 1);
        // 越过第三行中心（100）→ 落到 2。
        assert_eq!(target_index(&s, 0, 101), 2);
    }

    #[test]
    fn stack_offsets_moves_only_affected_rows() {
        let s = even_slots();
        // 第一行拖到末位：第二、三行各上移一个行高，第一行下移两个。
        let offs = stack_offsets(&s, 0, 2);
        assert_eq!(offs, vec![80, -40, -40]);
        // 原位不动时全零——避免无谓的补间与重绘。
        assert_eq!(stack_offsets(&s, 1, 1), vec![0, 0, 0]);
    }

    #[test]
    fn stack_offsets_handles_unequal_heights() {
        // 表单行高度天然不齐：40 / 60 / 40，间距 0。
        let s = vec![(0, 40), (40, 60), (100, 40)];
        // 首行（40 高）拖到末位：次行上移 40，三行上移 40，首行下移 100。
        let offs = stack_offsets(&s, 0, 2);
        assert_eq!(offs, vec![100, -40, -40]);
        // 末行（40 高）拖到首位：它上移 100，前两行各下移 40。
        let offs = stack_offsets(&s, 2, 0);
        assert_eq!(offs, vec![40, 40, -100]);
    }

    #[test]
    fn stack_offsets_respects_row_spacing() {
        // 行高 40、间距 8 → 槽位 y = 0 / 48 / 96。
        let s = vec![(0, 40), (48, 40), (96, 40)];
        let offs = stack_offsets(&s, 0, 1);
        // 首行与次行交换：各移动一个「行高 + 间距」。
        assert_eq!(offs, vec![48, -48, 0]);
    }
}
