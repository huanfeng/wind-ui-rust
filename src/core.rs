//! 核心层：generational arena + Node 树 + Measure/Arrange/Paint 三阶段。
//!
//! 关键设计：布局递归由 `Tree` 独占 `&mut self` 驱动；`Widget` trait 退化为
//! 纯内容（只报固有尺寸、只画自身 content rect，绝不访问树），从根上避免
//! Rust 借用冲突。容器节点的 `widget` 为 `EmptyWidget`，视觉由 `Style` 表达。

use crate::event::{Event, KeyEvent, PointerEvent, PointerKind};
use crate::geometry::{Color, Insets, Point, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::{Align, Axis, Dimension, MeasureMode, MeasureSpec};
use crate::style::Style;
use crate::text::TextEngine;

/// 点击/激活回调类型。
pub type ClickFn = Box<dyn FnMut(&mut EventCtx)>;

/// 剪贴板读写抽象。由平台层提供实现，UiHost 注入到 `Tree`，控件经 `EventCtx` 访问。
pub trait ClipboardProvider {
    fn get_text(&self) -> Option<String>;
    fn set_text(&self, text: &str);
}

/// 代际索引：删除节点后 generation 自增，旧 id 自然失效。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NodeId {
    index: u32,
    generation: u32,
}

/// 纯内容控件接口。不持有也不访问树。
pub trait Widget {
    /// 内容固有尺寸（content box，不含 padding）。容器/空控件返回 ZERO。
    /// `text` 供需要测量文本的控件（如 Label）使用。
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::ZERO
    }
    /// 绘制内容。`bounds`=节点绝对全矩形，`content`=扣除 padding 后的内容矩形，
    /// `focused`=本节点是否持有键盘焦点。背景/边框由核心层统一绘制；
    /// 自绘控件（如 Button）可用 `bounds` 画全尺寸背景。
    fn paint(
        &self,
        _bounds: Rect,
        _content: Rect,
        _focused: bool,
        _canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
    }
    /// 处理命中到本节点的事件，返回是否消费（消费则停止冒泡）。
    fn on_event(&mut self, _ctx: &mut EventCtx, _ev: &Event) -> bool {
        false
    }
    /// 是否可获得键盘焦点（参与 Tab 导航）。
    fn focusable(&self) -> bool {
        false
    }
    /// 接收 Builder 传入的点击回调（仅交互控件实现）。
    fn take_click(&mut self, _f: ClickFn) {}
    /// 类型擦除下转钩子：供 Builder 对具体控件做类型化配置（如 TextInput 的
    /// 多行/密码开关）。默认返回 None，需要的控件返回 `Some(self)`。
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }
}

/// 容器/纯样式节点占位控件。
pub struct EmptyWidget;
impl Widget for EmptyWidget {}

impl Node {
    /// 该帧是否有效可见（静态 visible 与可见条件取与）。
    pub fn effective_visible(&self) -> bool {
        self.visible && self.vis_cond.as_ref().map(|f| f()).unwrap_or(true)
    }
}

/// 容器布局算法。`None` 表示叶子。
#[derive(Clone, Copy)]
pub enum Layout {
    None,
    Linear { axis: Axis, spacing: i32, cross: Align },
    Frame,
    /// 垂直滚动容器：子内容按无限高度测量，按 scroll_y 偏移并裁剪到视口。
    Scroll,
}

/// 树节点。几何为物理像素，`bounds` 相对父节点。
pub struct Node {
    pub parent: Option<NodeId>,
    pub children: Vec<NodeId>,
    pub bounds: Rect,
    pub measured: Size,
    pub width: Dimension,
    pub height: Dimension,
    pub padding: Insets,
    pub margin: Insets,
    /// 自身对齐覆盖：None=继承容器交叉轴对齐；Some(a)=显式覆盖。
    pub align: Option<Align>,
    pub layout: Layout,
    pub widget: Box<dyn Widget>,
    pub style: Style,
    pub visible: bool,
    /// 运行期可见条件（如 Tab 页绑定选中项、Dialog 绑定显示标志）。
    /// 与 `visible` 取与：返回 false 则该帧不参与测量/布局/绘制/命中。
    pub vis_cond: Option<Box<dyn Fn() -> bool>>,
    /// 当前是否持有键盘焦点（由 UiHost 维护，核心层据此绘制焦点环）。
    pub focused: bool,
    /// 是否把子节点裁剪到自身内容区（滚动容器等）。
    pub clip_children: bool,
    /// 垂直滚动偏移（Scroll 容器）。
    pub scroll_y: i32,
    /// 内容总高（measure 记录，用于滚动钳制与滚动条）。
    pub content_h: i32,
}

struct Slot {
    generation: u32,
    node: Option<Node>,
}

/// 节点树 + arena。
pub struct Tree {
    slots: Vec<Slot>,
    free: Vec<u32>,
    pub root: Option<NodeId>,
    /// 是否绘制焦点环。仅在键盘（Tab）导航时为 true，纯鼠标操作时为 false，
    /// 使纯鼠标交互更纯净。
    pub focus_ring_visible: bool,
    /// 剪贴板实现（平台注入）；None 时复制粘贴为空操作。
    pub clipboard: Option<Box<dyn ClipboardProvider>>,
}

impl Default for Tree {
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            root: None,
            focus_ring_visible: false,
            clipboard: None,
        }
    }

    // ---- arena ----

    pub fn insert(&mut self, node: Node) -> NodeId {
        if let Some(idx) = self.free.pop() {
            let slot = &mut self.slots[idx as usize];
            slot.node = Some(node);
            NodeId { index: idx, generation: slot.generation }
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(Slot { generation: 0, node: Some(node) });
            NodeId { index: idx, generation: 0 }
        }
    }

    pub fn get(&self, id: NodeId) -> Option<&Node> {
        let slot = self.slots.get(id.index as usize)?;
        if slot.generation == id.generation {
            slot.node.as_ref()
        } else {
            None
        }
    }

    pub fn get_mut(&mut self, id: NodeId) -> Option<&mut Node> {
        let slot = self.slots.get_mut(id.index as usize)?;
        if slot.generation == id.generation {
            slot.node.as_mut()
        } else {
            None
        }
    }

    /// 删除子树（递归）。旧 id 因 generation 自增而失效。
    pub fn remove(&mut self, id: NodeId) {
        let children = match self.get(id) {
            Some(n) => n.children.clone(),
            None => return,
        };
        for c in children {
            self.remove(c);
        }
        if let Some(slot) = self.slots.get_mut(id.index as usize) {
            if slot.generation == id.generation {
                slot.node = None;
                slot.generation = slot.generation.wrapping_add(1);
                self.free.push(id.index);
            }
        }
    }

    pub fn add_child(&mut self, parent: NodeId, child: NodeId) {
        if let Some(p) = self.get_mut(parent) {
            p.children.push(child);
        }
        if let Some(c) = self.get_mut(child) {
            c.parent = Some(parent);
        }
    }

    fn visible_children(&self, id: NodeId) -> Vec<NodeId> {
        match self.get(id) {
            Some(n) => n
                .children
                .iter()
                .copied()
                .filter(|c| self.get(*c).map(|n| n.effective_visible()).unwrap_or(false))
                .collect(),
            None => Vec::new(),
        }
    }

    fn measured_of(&self, id: NodeId) -> Size {
        self.get(id).map(|n| n.measured).unwrap_or(Size::ZERO)
    }
    fn margin_of(&self, id: NodeId) -> Insets {
        self.get(id).map(|n| n.margin).unwrap_or_default()
    }

    // ---- 布局入口 ----

    /// 用窗口尺寸测量并排布整棵树。
    pub fn layout_root(&mut self, size: Size, text: &mut dyn TextEngine) {
        if let Some(root) = self.root {
            self.measure(root, MeasureSpec::exactly(size.w), MeasureSpec::exactly(size.h), text);
            self.arrange(root, Rect::from_size(size));
        }
    }

    // ---- Measure ----

    fn measure(
        &mut self,
        id: NodeId,
        wspec: MeasureSpec,
        hspec: MeasureSpec,
        text: &mut dyn TextEngine,
    ) -> Size {
        let (layout, padding, visible) = match self.get(id) {
            Some(n) => (n.layout, n.padding, n.effective_visible()),
            None => return Size::ZERO,
        };
        if !visible {
            if let Some(n) = self.get_mut(id) {
                n.measured = Size::ZERO;
            }
            return Size::ZERO;
        }

        let avail_w = (wspec.avail() - padding.horizontal()).max(0);
        let avail_h = (hspec.avail() - padding.vertical()).max(0);

        let content = match layout {
            Layout::None => {
                // 叶子：纯内容固有尺寸（可能需要测量文本）
                let n = self.get(id).unwrap();
                n.widget.measure(Size::new(avail_w, avail_h), &n.style, text)
            }
            Layout::Linear { axis, spacing, .. } => {
                self.measure_linear(id, axis, spacing, wspec, hspec, avail_w, avail_h, text)
            }
            Layout::Frame => self.measure_frame(id, wspec, hspec, avail_w, avail_h, text),
            Layout::Scroll => self.measure_scroll(id, avail_w, text),
        };

        let desired_w = content.w + padding.horizontal();
        let desired_h = content.h + padding.vertical();
        let size = Size::new(wspec.resolve(desired_w), hspec.resolve(desired_h));
        if let Some(n) = self.get_mut(id) {
            n.measured = size;
        }
        size
    }

    #[allow(clippy::too_many_arguments)]
    fn measure_linear(
        &mut self,
        id: NodeId,
        axis: Axis,
        spacing: i32,
        wspec: MeasureSpec,
        hspec: MeasureSpec,
        avail_w: i32,
        avail_h: i32,
        text: &mut dyn TextEngine,
    ) -> Size {
        let horizontal = axis == Axis::Horizontal;
        let (main_spec, cross_spec) = if horizontal { (wspec, hspec) } else { (hspec, wspec) };
        let main_avail = if horizontal { avail_w } else { avail_h };
        let cross_avail = if horizontal { avail_h } else { avail_w };
        let main_unbounded = main_spec.mode == MeasureMode::Unbounded;
        let cross_unbounded = cross_spec.mode == MeasureMode::Unbounded;

        let children = self.visible_children(id);
        let mut used_main = 0;
        let mut max_cross = 0;
        let mut total_weight = 0.0f32;
        let mut weighted: Vec<NodeId> = Vec::new();

        // 第一遍：非权重子节点。权重子的主轴 margin 在此预扣，使第二遍
        // 的 remaining 恰好等于可供 portion 瓜分的空间（避免超分）。
        for &c in &children {
            let (cw, ch, cm) = {
                let n = self.get(c).unwrap();
                (n.width, n.height, n.margin)
            };
            let main_dim = if horizontal { cw } else { ch };
            let cross_dim = if horizontal { ch } else { cw };
            let (cm_main, cm_cross) = main_cross_insets(horizontal, cm);
            if main_dim.is_weight() {
                total_weight += main_dim.weight();
                used_main += cm_main; // 预扣权重子主轴 margin
                weighted.push(c);
                continue;
            }
            // 主轴上的 Match 降级为 Wrap，避免单个子独占整条主轴。
            let main_eff = if matches!(main_dim, Dimension::Match) { Dimension::Wrap } else { main_dim };
            let main_child = child_spec(main_eff, main_avail, main_unbounded);
            let cross_child = child_spec(cross_dim, cross_avail, cross_unbounded);
            let (cwspec, chspec) =
                if horizontal { (main_child, cross_child) } else { (cross_child, main_child) };
            let s = self.measure(c, cwspec, chspec, text);
            let (s_main, s_cross) = main_cross(horizontal, s);
            used_main += s_main + cm_main;
            max_cross = max_cross.max(s_cross + cm_cross);
        }
        let gaps = spacing * (children.len() as i32 - 1).max(0);
        used_main += gaps;

        // 第二遍：按权重瓜分剩余主轴空间（margin 已在第一遍预扣）。
        if total_weight > 0.0 && !main_unbounded {
            let remaining = (main_avail - used_main).max(0);
            let mut allocated = 0;
            let last = weighted.len().saturating_sub(1);
            for (i, &c) in weighted.iter().enumerate() {
                let (cw, ch, cm) = {
                    let n = self.get(c).unwrap();
                    (n.width, n.height, n.margin)
                };
                let w = if horizontal { cw.weight() } else { ch.weight() };
                // 末位补余，消除整数截断误差，实现像素精确分配。
                let portion = if i == last {
                    (remaining - allocated).max(0)
                } else {
                    (remaining as f32 * w / total_weight) as i32
                };
                allocated += portion;
                let main_child = MeasureSpec::exactly(portion);
                let cross_child = child_spec(
                    if horizontal { ch } else { cw },
                    cross_avail,
                    cross_unbounded,
                );
                let (cwspec, chspec) =
                    if horizontal { (main_child, cross_child) } else { (cross_child, main_child) };
                let s = self.measure(c, cwspec, chspec, text);
                let (_, cm_cross) = main_cross_insets(horizontal, cm);
                let (s_main, s_cross) = main_cross(horizontal, s);
                used_main += s_main; // margin 已预扣，此处只加 portion
                max_cross = max_cross.max(s_cross + cm_cross);
            }
        }

        if horizontal {
            Size::new(used_main, max_cross)
        } else {
            Size::new(max_cross, used_main)
        }
    }

    /// 垂直滚动容器：子按受限宽度、无限高度测量；记录内容总高。
    fn measure_scroll(&mut self, id: NodeId, avail_w: i32, text: &mut dyn TextEngine) -> Size {
        let children = self.visible_children(id);
        let mut total_h = 0;
        let mut max_w = 0;
        for &c in &children {
            let (cw, ch, cm) = {
                let n = self.get(c).unwrap();
                (n.width, n.height, n.margin)
            };
            let cwspec = child_spec(cw, avail_w, false);
            // 高度方向视为无限：Px 固定其值，Wrap/Match 按内容展开。
            let chspec = child_spec(ch, 0, true);
            let s = self.measure(c, cwspec, chspec, text);
            total_h += s.h + cm.vertical();
            max_w = max_w.max(s.w + cm.horizontal());
        }
        if let Some(n) = self.get_mut(id) {
            n.content_h = total_h;
        }
        Size::new(max_w, total_h)
    }

    fn measure_frame(
        &mut self,
        id: NodeId,
        wspec: MeasureSpec,
        hspec: MeasureSpec,
        avail_w: i32,
        avail_h: i32,
        text: &mut dyn TextEngine,
    ) -> Size {
        let children = self.visible_children(id);
        let mut mw = 0;
        let mut mh = 0;
        for &c in &children {
            let (cw, ch, cm) = {
                let n = self.get(c).unwrap();
                (n.width, n.height, n.margin)
            };
            let cwspec = child_spec(cw, avail_w, wspec.mode == MeasureMode::Unbounded);
            let chspec = child_spec(ch, avail_h, hspec.mode == MeasureMode::Unbounded);
            let s = self.measure(c, cwspec, chspec, text);
            mw = mw.max(s.w + cm.horizontal());
            mh = mh.max(s.h + cm.vertical());
        }
        Size::new(mw, mh)
    }

    // ---- Arrange ----

    fn arrange(&mut self, id: NodeId, bounds: Rect) {
        let (layout, padding, visible) = match self.get(id) {
            Some(n) => (n.layout, n.padding, n.effective_visible()),
            None => return,
        };
        if let Some(n) = self.get_mut(id) {
            n.bounds = bounds;
        }
        if !visible {
            return;
        }
        // 内容区相对本节点左上角（含 padding 偏移）
        let inner = Rect::new(
            padding.left,
            padding.top,
            (bounds.w - padding.horizontal()).max(0),
            (bounds.h - padding.vertical()).max(0),
        );
        match layout {
            Layout::None => {}
            Layout::Linear { axis, spacing, cross } => {
                self.arrange_linear(id, inner, axis, spacing, cross)
            }
            Layout::Frame => self.arrange_frame(id, inner),
            Layout::Scroll => self.arrange_scroll(id, inner),
        }
    }

    fn arrange_scroll(&mut self, id: NodeId, inner: Rect) {
        // 钳制滚动量：[0, content_h - 视口高]。
        let (content_h, mut scroll_y) = {
            let n = self.get(id).unwrap();
            (n.content_h, n.scroll_y)
        };
        let max_scroll = (content_h - inner.h).max(0);
        scroll_y = scroll_y.clamp(0, max_scroll);
        if let Some(n) = self.get_mut(id) {
            n.scroll_y = scroll_y;
        }
        // 可滚动时为右侧滚动条预留宽度，避免内容被遮挡。
        let scrollbar_w = if content_h > inner.h { 8 } else { 0 };
        // 子节点从视口顶起按内容顺序堆叠，整体上移 scroll_y。
        let children = self.visible_children(id);
        let mut y = inner.y - scroll_y;
        for c in children {
            let (cs, cm) = (self.measured_of(c), self.margin_of(c));
            let cw = (inner.w - scrollbar_w - cm.horizontal()).max(0);
            let bounds = Rect::new(inner.x + cm.left, y + cm.top, cw, cs.h);
            self.arrange(c, bounds);
            y += cs.h + cm.vertical();
        }
    }

    fn arrange_linear(&mut self, id: NodeId, inner: Rect, axis: Axis, spacing: i32, cross: Align) {
        let horizontal = axis == Axis::Horizontal;
        let children = self.visible_children(id);
        let mut cursor = if horizontal { inner.x } else { inner.y };
        let cross_start = if horizontal { inner.y } else { inner.x };
        let cross_avail_full = if horizontal { inner.h } else { inner.w };

        for c in children {
            let cs = self.measured_of(c);
            let cm = self.margin_of(c);
            let (s_main, s_cross) = main_cross(horizontal, cs);
            let (cm_main_start, cm_cross_start) = if horizontal {
                (cm.left, cm.top)
            } else {
                (cm.top, cm.left)
            };
            let cm_cross_total = if horizontal { cm.vertical() } else { cm.horizontal() };
            let cm_main_end = if horizontal { cm.right } else { cm.bottom };

            let cross_avail = (cross_avail_full - cm_cross_total).max(0);
            // None=继承容器交叉轴对齐；Some=显式覆盖（含显式 Start）。
            let eff_align = self.get(c).and_then(|n| n.align).unwrap_or(cross);
            let cross_size = if eff_align == Align::Stretch { cross_avail } else { s_cross };
            let cross_off = align_offset(eff_align, cross_avail, cross_size);

            let main_pos = cursor + cm_main_start;
            let cross_pos = cross_start + cm_cross_start + cross_off;

            let child_bounds = if horizontal {
                Rect::new(main_pos, cross_pos, s_main, cross_size)
            } else {
                Rect::new(cross_pos, main_pos, cross_size, s_main)
            };
            self.arrange(c, child_bounds);
            cursor = main_pos + s_main + cm_main_end + spacing;
        }
    }

    fn arrange_frame(&mut self, id: NodeId, inner: Rect) {
        let children = self.visible_children(id);
        for c in children {
            let cs = self.measured_of(c);
            let cm = self.margin_of(c);
            let align = self.get(c).and_then(|n| n.align).unwrap_or(Align::Start);
            let avail_w = (inner.w - cm.horizontal()).max(0);
            let avail_h = (inner.h - cm.vertical()).max(0);
            let (cw, ch) = if align == Align::Stretch {
                (avail_w, avail_h)
            } else {
                (cs.w, cs.h)
            };
            let x = inner.x + cm.left + align_offset(align, avail_w, cw);
            let y = inner.y + cm.top + align_offset(align, avail_h, ch);
            self.arrange(c, Rect::new(x, y, cw, ch));
        }
    }

    // ---- Paint ----

    /// 从根递归绘制到 canvas。
    pub fn paint(&self, canvas: &mut dyn Canvas) {
        if let Some(root) = self.root {
            self.paint_node(canvas, root, Point::new(0, 0));
        }
    }

    fn paint_node(&self, canvas: &mut dyn Canvas, id: NodeId, origin: Point) {
        let n = match self.get(id) {
            Some(n) if n.effective_visible() => n,
            _ => return,
        };
        let abs = Rect::new(origin.x + n.bounds.x, origin.y + n.bounds.y, n.bounds.w, n.bounds.h);
        if abs.is_empty() {
            return;
        }
        let (fx, fy, fw, fh) = (abs.x as f32, abs.y as f32, abs.w as f32, abs.h as f32);
        let radius = n.style.corner_radius;

        if let Some(bg) = n.style.bg {
            canvas.fill_round_rect(fx, fy, fw, fh, radius, &Paint::fill(bg));
        }
        if let Some((bc, bw)) = n.style.border {
            if bw > 0 {
                canvas.stroke_round_rect(fx, fy, fw, fh, radius, bw as f32, &Paint::fill(bc));
            }
        }

        let content = abs.inset(n.padding);
        n.widget.paint(abs, content, n.focused, canvas, &n.style);

        // 焦点环：仅在键盘导航时（focus_ring_visible）绘制，纯鼠标操作不显示。
        if n.focused && self.focus_ring_visible {
            let ring = Color::hex(0x4C8BF5);
            canvas.stroke_round_rect(fx - 1.0, fy - 1.0, fw + 2.0, fh + 2.0, radius + 1.0, 2.0, &Paint::fill(ring));
        }

        let child_origin = Point::new(abs.x, abs.y);
        if n.clip_children {
            canvas.save();
            canvas.clip_rect(content);
            for &c in &n.children {
                self.paint_node(canvas, c, child_origin);
            }
            canvas.restore();
        } else {
            for &c in &n.children {
                self.paint_node(canvas, c, child_origin);
            }
        }

        // 滚动条：内容高于视口时在右缘绘制纵向指示条。
        if matches!(n.layout, Layout::Scroll) && n.content_h > content.h {
            let track_w = 6.0;
            let tx = (abs.right() - track_w as i32 - 2) as f32;
            let ty = content.y as f32;
            let th = content.h as f32;
            let ratio = content.h as f32 / n.content_h as f32;
            let thumb_h = (th * ratio).max(24.0);
            let max_scroll = (n.content_h - content.h).max(1) as f32;
            let thumb_y = ty + (th - thumb_h) * (n.scroll_y as f32 / max_scroll);
            canvas.fill_round_rect(tx, thumb_y, track_w, thumb_h, track_w / 2.0, &Paint::fill(Color::hex(0xBFC6CF)));
        }
    }
}

// ---- 事件分发 ----

/// 一次事件处理累积的副作用指令。
#[derive(Default)]
pub(crate) struct EventOutcome {
    repaint: bool,
    /// Some(Some(id))=设置捕获；Some(None)=释放捕获。
    capture: Option<Option<NodeId>>,
    close: bool,
    focus: Option<NodeId>,
}

/// 传给 `Widget::on_event` 的受控句柄：在不暴露裸 arena 的前提下操作本节点与请求副作用。
pub struct EventCtx<'a> {
    tree: &'a mut Tree,
    self_id: NodeId,
    out: EventOutcome,
}

impl EventCtx<'_> {
    pub fn id(&self) -> NodeId {
        self.self_id
    }
    /// 请求重绘。
    pub fn mark_dirty(&mut self) {
        self.out.repaint = true;
    }
    /// 修改本节点背景色并重绘（交互态切换常用）。
    pub fn set_bg(&mut self, c: Color) {
        if let Some(n) = self.tree.get_mut(self.self_id) {
            n.style.bg = Some(c);
        }
        self.out.repaint = true;
    }
    /// 捕获指针（后续指针事件锁定到本节点）。
    pub fn capture(&mut self) {
        self.out.capture = Some(Some(self.self_id));
    }
    /// 释放指针捕获。
    pub fn release_capture(&mut self) {
        self.out.capture = Some(None);
    }
    /// 请求关闭窗口。
    pub fn request_close(&mut self) {
        self.out.close = true;
    }
    /// 请求把焦点移到本节点。
    pub fn request_focus(&mut self) {
        self.out.focus = Some(self.self_id);
    }
    /// 本节点绝对矩形（判断指针是否仍在控件内）。
    pub fn bounds(&self) -> Rect {
        self.tree.abs_bounds(self.self_id)
    }
    /// 调整本节点滚动偏移（滚动容器），下一帧 arrange 会钳制范围。
    pub fn scroll_by(&mut self, dy: i32) {
        if let Some(n) = self.tree.get_mut(self.self_id) {
            n.scroll_y += dy;
        }
        self.out.repaint = true;
    }
    /// 读取本滚动节点的 (scroll_y, content_h, 视口高)。
    pub fn scroll_metrics(&self) -> (i32, i32, i32) {
        if let Some(n) = self.tree.get(self.self_id) {
            let view_h = (n.bounds.h - n.padding.vertical()).max(0);
            (n.scroll_y, n.content_h, view_h)
        } else {
            (0, 0, 0)
        }
    }
    /// 直接设置滚动偏移（拖动滚动条用），下一帧 arrange 钳制范围。
    pub fn set_scroll(&mut self, y: i32) {
        if let Some(n) = self.tree.get_mut(self.self_id) {
            n.scroll_y = y;
        }
        self.out.repaint = true;
    }
    /// 读取剪贴板文本（无剪贴板实现时返回 None）。
    pub fn clipboard_get(&self) -> Option<String> {
        self.tree.clipboard.as_ref().and_then(|c| c.get_text())
    }
    /// 写入剪贴板文本（无剪贴板实现时为空操作）。
    pub fn clipboard_set(&self, text: &str) {
        if let Some(c) = self.tree.clipboard.as_ref() {
            c.set_text(text);
        }
    }
}

/// 指针/键盘分发的对外结果。
#[derive(Default, Debug, Clone, Copy)]
pub struct DispatchResult {
    pub repaint: bool,
    pub close: bool,
    pub focus: Option<NodeId>,
    /// 事件是否被某个控件消费（供宿主决定是否回退到默认行为，如 Escape 关窗）。
    pub consumed: bool,
}

impl Tree {
    /// 节点绝对窗口矩形（累加各级父节点偏移）。
    pub fn abs_bounds(&self, id: NodeId) -> Rect {
        let mut r = match self.get(id) {
            Some(n) => n.bounds,
            None => return Rect::default(),
        };
        let mut cur = self.get(id).and_then(|n| n.parent);
        while let Some(p) = cur {
            match self.get(p) {
                Some(pn) => {
                    r.x += pn.bounds.x;
                    r.y += pn.bounds.y;
                    cur = pn.parent;
                }
                None => break,
            }
        }
        r
    }

    /// 命中测试：返回包含该点的最深可见节点。
    pub fn hit_test(&self, p: Point) -> Option<NodeId> {
        let root = self.root?;
        self.hit_node(root, p, Point::new(0, 0))
    }

    fn hit_node(&self, id: NodeId, p: Point, origin: Point) -> Option<NodeId> {
        let n = self.get(id)?;
        if !n.effective_visible() {
            return None;
        }
        let abs = Rect::new(origin.x + n.bounds.x, origin.y + n.bounds.y, n.bounds.w, n.bounds.h);
        if !abs.contains(p) {
            return None;
        }
        // 滚动条区域优先命中滚动容器自身（用于拖动滚动条，而非下方内容）。
        // 命中区 10px 与 containers::SCROLLBAR_HIT_W 一致。
        if matches!(n.layout, Layout::Scroll) {
            let content = abs.inset(n.padding);
            if n.content_h > content.h && p.x >= abs.right() - 10 {
                return Some(id);
            }
        }
        // 裁剪容器：点不在内容区时不下探子节点（仍可命中容器自身处理滚轮）。
        let in_content = if n.clip_children {
            abs.inset(n.padding).contains(p)
        } else {
            true
        };
        if in_content {
            // 倒序遍历子节点：后绘制者在上层，优先命中。
            let child_origin = Point::new(abs.x, abs.y);
            for &c in n.children.iter().rev() {
                if let Some(hit) = self.hit_node(c, p, child_origin) {
                    return Some(hit);
                }
            }
        }
        Some(id)
    }

    /// 祖先链：从节点自身到根。
    fn ancestor_chain(&self, id: NodeId) -> Vec<NodeId> {
        let mut chain = vec![id];
        let mut cur = self.get(id).and_then(|n| n.parent);
        while let Some(p) = cur {
            chain.push(p);
            cur = self.get(p).and_then(|n| n.parent);
        }
        chain
    }

    /// 收集可聚焦节点（前序遍历顺序），供 Tab 导航。
    pub fn focusable_order(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            self.collect_focusable(root, &mut out);
        }
        out
    }

    fn collect_focusable(&self, id: NodeId, out: &mut Vec<NodeId>) {
        if let Some(n) = self.get(id) {
            if !n.effective_visible() {
                return;
            }
            if n.widget.focusable() {
                out.push(id);
            }
            for &c in &n.children {
                self.collect_focusable(c, out);
            }
        }
    }

    /// 取出 widget 调用 on_event 再放回，打破 `&mut widget` 与 `&mut tree` 的借用环。
    ///
    /// Directive（契约，供未来修改者遵守）：`on_event`/`on_click` 回调内**不得**
    /// 删除本节点（self），也不得同步再分发触及 self 的事件。期间 self 的 widget 被
    /// 临时换为 EmptyWidget：删除 self 会使末尾放回因 generation 不匹配而静默跳过，
    /// 令该控件退化为哑控件；重入 self 则内层事件落到 EmptyWidget 被丢弃。
    /// 需要这类操作时应改用命令队列在分发结束后统一执行。
    fn call_on_event(&mut self, id: NodeId, ev: &Event) -> (bool, EventOutcome) {
        let mut widget = match self.get_mut(id) {
            Some(n) => std::mem::replace(&mut n.widget, Box::new(EmptyWidget)),
            None => return (false, EventOutcome::default()),
        };
        let mut ctx = EventCtx { tree: self, self_id: id, out: EventOutcome::default() };
        let consumed = widget.on_event(&mut ctx, ev);
        let out = ctx.out;
        match self.get_mut(id) {
            Some(n) => n.widget = widget,
            None => debug_assert!(false, "on_event 回调内删除了 self 节点，违反 call_on_event 契约"),
        }
        (consumed, out)
    }

    /// 分发指针事件：维护 hover/capture，冒泡处理，汇总副作用。
    pub fn dispatch_pointer(
        &mut self,
        ev: PointerEvent,
        hover: &mut Option<NodeId>,
        capture: &mut Option<NodeId>,
    ) -> DispatchResult {
        let mut res = DispatchResult::default();

        // hover 进出（仅 Move 且无捕获时）
        if matches!(ev.kind, PointerKind::Move) && capture.is_none() {
            let target = self.hit_test(ev.pos);
            if *hover != target {
                if let Some(old) = *hover {
                    let (_, o) =
                        self.call_on_event(old, &Event::Pointer(PointerEvent { kind: PointerKind::Leave, ..ev }));
                    res.repaint |= o.repaint;
                }
                if let Some(new) = target {
                    let (_, o) =
                        self.call_on_event(new, &Event::Pointer(PointerEvent { kind: PointerKind::Enter, ..ev }));
                    res.repaint |= o.repaint;
                }
                *hover = target;
            }
        }

        // 主事件：捕获优先，否则命中目标，沿祖先链冒泡。
        let had_capture = capture.is_some();
        let target = capture.or_else(|| self.hit_test(ev.pos));
        if let Some(t) = target {
            for id in self.ancestor_chain(t) {
                let (consumed, o) = self.call_on_event(id, &Event::Pointer(ev));
                res.repaint |= o.repaint;
                res.close |= o.close;
                res.consumed |= consumed;
                if o.focus.is_some() {
                    res.focus = o.focus;
                }
                if let Some(cap) = o.capture {
                    *capture = cap;
                }
                if consumed {
                    break;
                }
            }
        }

        // 捕获在本次（如 Up）被释放后，按当前位置重算 hover 并补发 Enter/Leave，
        // 修正"按下拖到另一控件上释放"后 hover 滞留在原控件的问题。
        if had_capture && capture.is_none() {
            let target = self.hit_test(ev.pos);
            if *hover != target {
                if let Some(old) = *hover {
                    let (_, o) = self
                        .call_on_event(old, &Event::Pointer(PointerEvent { kind: PointerKind::Leave, ..ev }));
                    res.repaint |= o.repaint;
                }
                if let Some(new) = target {
                    let (_, o) = self
                        .call_on_event(new, &Event::Pointer(PointerEvent { kind: PointerKind::Enter, ..ev }));
                    res.repaint |= o.repaint;
                }
                *hover = target;
            }
        }
        res
    }

    /// 分发键盘事件到焦点节点。
    pub fn dispatch_key(&mut self, ev: KeyEvent, focus: Option<NodeId>) -> DispatchResult {
        let mut res = DispatchResult::default();
        if let Some(f) = focus {
            let (consumed, o) = self.call_on_event(f, &Event::Key(ev));
            res.repaint = o.repaint;
            res.close = o.close;
            res.focus = o.focus;
            res.consumed = consumed;
        }
        res
    }

    /// 设置焦点节点（清旧设新，返回是否变化）。
    pub fn set_focused(&mut self, id: Option<NodeId>, old: Option<NodeId>) {
        if let Some(o) = old {
            if let Some(n) = self.get_mut(o) {
                n.focused = false;
            }
        }
        if let Some(i) = id {
            if let Some(n) = self.get_mut(i) {
                n.focused = true;
            }
        }
    }
}

// ---- 辅助 ----

fn child_spec(dim: Dimension, avail: i32, parent_unbounded: bool) -> MeasureSpec {
    match dim {
        Dimension::Px(v) => MeasureSpec::exactly(v.max(0)),
        Dimension::Match => {
            if parent_unbounded {
                MeasureSpec::unbounded()
            } else {
                MeasureSpec::exactly(avail.max(0))
            }
        }
        Dimension::Wrap | Dimension::Weight(_) => {
            if parent_unbounded {
                MeasureSpec::unbounded()
            } else {
                MeasureSpec::at_most(avail.max(0))
            }
        }
    }
}

fn main_cross(horizontal: bool, s: Size) -> (i32, i32) {
    if horizontal {
        (s.w, s.h)
    } else {
        (s.h, s.w)
    }
}

fn main_cross_insets(horizontal: bool, i: Insets) -> (i32, i32) {
    if horizontal {
        (i.horizontal(), i.vertical())
    } else {
        (i.vertical(), i.horizontal())
    }
}

fn align_offset(a: Align, avail: i32, size: i32) -> i32 {
    // clamp >=0：子尺寸超过可用空间时不产生负偏移（避免双向溢出）。
    match a {
        Align::Start | Align::Stretch => 0,
        Align::Center => ((avail - size) / 2).max(0),
        Align::End => (avail - size).max(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Key, KeyEvent, MouseButton, PointerEvent, PointerKind};
    use crate::geometry::{Point, Size};
    use crate::ui::Element;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    fn layout(root: Element, w: i32, h: i32) -> Tree {
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(w, h), &mut te);
        tree
    }

    #[test]
    fn weighted_children_with_margin_dont_overflow() {
        // 容器 200 宽，两个 weight=1 子各 margin 10。
        // 预扣 margin 总 40 → remaining 160 → 每个 portion 80。
        let tree = layout(
            Element::row()
                .width(200)
                .height(50)
                .child(Element::leaf().height(20).margin(10).weight(1.0))
                .child(Element::leaf().height(20).margin(10).weight(1.0)),
            200,
            50,
        );
        let root = tree.root.unwrap();
        let kids = tree.get(root).unwrap().children.clone();
        let b0 = tree.get(kids[0]).unwrap().bounds;
        let b1 = tree.get(kids[1]).unwrap().bounds;
        assert_eq!(b0.w, 80, "首个权重子宽应为 80");
        assert_eq!(b1.w, 80, "次个权重子宽应为 80");
        assert_eq!(b0.x, 10, "首子左边界=margin");
        // 末子右边界 + 右 margin 不超过容器宽（无超分）
        assert!(b1.x + b1.w + 10 <= 200, "右边界 {} 超出 200", b1.x + b1.w + 10);
    }

    #[test]
    fn weight_ratio_split_is_pixel_exact() {
        // weight 1:2，容器 300，无 margin/spacing → 100 + 200，总和精确等于 300。
        let tree = layout(
            Element::row()
                .width(300)
                .height(30)
                .child(Element::leaf().weight(1.0))
                .child(Element::leaf().weight(2.0)),
            300,
            30,
        );
        let root = tree.root.unwrap();
        let kids = tree.get(root).unwrap().children.clone();
        let b0 = tree.get(kids[0]).unwrap().bounds;
        let b1 = tree.get(kids[1]).unwrap().bounds;
        assert_eq!(b0.w, 100);
        assert_eq!(b1.w, 200);
        assert_eq!(b0.w + b1.w, 300, "像素精确：和应等于容器宽");
    }

    #[test]
    fn explicit_start_overrides_container_center() {
        // 容器交叉轴 Center，子显式 align Start 应停在顶部（不被强制居中）。
        let tree = layout(
            Element::row()
                .width(200)
                .height(100)
                .cross(Align::Center)
                .child(Element::leaf().size(20, 20).align(Align::Start)),
            200,
            100,
        );
        let root = tree.root.unwrap();
        let kid = tree.get(root).unwrap().children[0];
        let b = tree.get(kid).unwrap().bounds;
        assert_eq!(b.y, 0, "显式 Start 应贴顶，y=0");
    }

    fn ptr(kind: PointerKind, p: Point) -> PointerEvent {
        PointerEvent::single(kind, p, MouseButton::Left)
    }

    fn button_tree(clicks: Rc<Cell<i32>>) -> (Tree, NodeId) {
        let c = clicks;
        let root = Element::col()
            .width(200)
            .height(100)
            .padding(10)
            .child(Element::button("OK").on_click(move |_| c.set(c.get() + 1)));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 100), &mut te);
        let btn = tree.get(id).unwrap().children[0];
        (tree, btn)
    }

    #[test]
    fn button_click_fires_callback_and_captures() {
        let clicks = Rc::new(Cell::new(0));
        let (mut tree, btn) = button_tree(clicks.clone());
        let b = tree.abs_bounds(btn);
        let center = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut hover, mut cap) = (None, None);

        tree.dispatch_pointer(ptr(PointerKind::Down, center), &mut hover, &mut cap);
        assert_eq!(cap, Some(btn), "按下应捕获按钮");
        assert_eq!(clicks.get(), 0, "按下不触发点击");

        tree.dispatch_pointer(ptr(PointerKind::Up, center), &mut hover, &mut cap);
        assert_eq!(clicks.get(), 1, "在按钮内释放应触发一次点击");
        assert_eq!(cap, None, "释放应取消捕获");
    }

    #[test]
    fn release_outside_does_not_click() {
        let clicks = Rc::new(Cell::new(0));
        let (mut tree, btn) = button_tree(clicks.clone());
        let b = tree.abs_bounds(btn);
        let center = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let outside = Point::new(b.x + b.w + 60, b.y);
        let (mut hover, mut cap) = (None, None);

        tree.dispatch_pointer(ptr(PointerKind::Down, center), &mut hover, &mut cap);
        // 捕获使 Up 仍路由到按钮，但位置在外 → 不触发
        tree.dispatch_pointer(ptr(PointerKind::Up, outside), &mut hover, &mut cap);
        assert_eq!(clicks.get(), 0, "按钮外释放不应触发点击");
        assert_eq!(cap, None);
    }

    #[test]
    fn hover_tracks_pointer() {
        let clicks = Rc::new(Cell::new(0));
        let (mut tree, btn) = button_tree(clicks);
        let b = tree.abs_bounds(btn);
        let center = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let outside = Point::new(b.x + b.w + 60, b.y + b.h + 60);
        let (mut hover, mut cap) = (None, None);

        tree.dispatch_pointer(ptr(PointerKind::Move, center), &mut hover, &mut cap);
        assert_eq!(hover, Some(btn), "移入按钮应记录 hover");
        tree.dispatch_pointer(ptr(PointerKind::Move, outside), &mut hover, &mut cap);
        assert_eq!(hover, None, "移出按钮应清除 hover");
    }

    #[test]
    fn focusable_order_collects_buttons() {
        let root = Element::row()
            .child(Element::label("x"))
            .child(Element::button("A"))
            .child(Element::button("B"));
        let tree = layout(root, 300, 50);
        assert_eq!(tree.focusable_order().len(), 2, "应收集到 2 个可聚焦按钮");
    }

    fn click(tree: &mut Tree, id: NodeId) {
        let b = tree.abs_bounds(id);
        let c = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, c), &mut h, &mut cap);
        tree.dispatch_pointer(ptr(PointerKind::Up, c), &mut h, &mut cap);
    }

    #[test]
    fn checkbox_binds_and_toggles() {
        let st = Rc::new(Cell::new(false));
        let root = Element::col()
            .width(200)
            .height(60)
            .padding(5)
            .child(Element::checkbox("启用", st.clone()));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 60), &mut te);
        let cb = tree.get(id).unwrap().children[0];
        click(&mut tree, cb);
        assert!(st.get(), "点击应选中");
        click(&mut tree, cb);
        assert!(!st.get(), "再次点击应取消");
    }

    #[test]
    fn radio_group_is_exclusive() {
        let g = Rc::new(Cell::new(0usize));
        let root = Element::row()
            .width(360)
            .height(40)
            .padding(5)
            .spacing(20)
            .child(Element::radio("A", g.clone(), 0))
            .child(Element::radio("B", g.clone(), 1));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(360, 40), &mut te);
        let b1 = tree.get(id).unwrap().children[1];
        click(&mut tree, b1);
        assert_eq!(g.get(), 1, "点击第二项应使组值为 1");
    }

    #[test]
    fn slider_sets_value_on_press() {
        let v = Rc::new(Cell::new(0.0f32));
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::slider(v.clone()).width(100));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 40), &mut te);
        let sl = tree.get(id).unwrap().children[0];
        let b = tree.abs_bounds(sl);
        let right = Point::new(b.x + b.w - 1, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, right), &mut h, &mut cap);
        assert!(v.get() > 0.9, "在最右端按下应使值接近 1，实际 {}", v.get());
    }

    #[test]
    fn scroll_wheel_offsets_and_clamps() {
        let mut sc = Element::scroll().width(100).height(100);
        for _ in 0..10 {
            sc = sc.child(Element::leaf().width_match().height(30));
        }
        let mut tree = Tree::new();
        let id = sc.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te);
        // 内容总高 300 > 视口 100，最大滚动量 200。
        assert_eq!(tree.get(id).unwrap().content_h, 300);

        let wheel = |d: i32| {
            PointerEvent::single(PointerKind::Wheel(d), Point::new(50, 50), MouseButton::Left)
        };
        let (mut h, mut cap) = (None, None);
        tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        tree.layout_root(Size::new(100, 100), &mut te); // 重排以应用钳制
        assert!(tree.get(id).unwrap().scroll_y > 0, "向下滚应增加偏移");

        for _ in 0..20 {
            tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        }
        tree.layout_root(Size::new(100, 100), &mut te);
        assert_eq!(tree.get(id).unwrap().scroll_y, 200, "应钳制到最大滚动量");
    }

    #[test]
    fn scrollbar_drag_changes_offset() {
        let mut sc = Element::scroll().width(100).height(100);
        for _ in 0..10 {
            sc = sc.child(Element::leaf().width_match().height(30));
        }
        let mut tree = Tree::new();
        let id = sc.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te); // content_h=300, view=100
        let (mut h, mut cap) = (None, None);
        // 右缘滚动条区域按下（x>=88）→ 捕获滚动容器
        let down = PointerEvent::single(PointerKind::Down, Point::new(95, 10), MouseButton::Left);
        tree.dispatch_pointer(down, &mut h, &mut cap);
        assert_eq!(cap, Some(id), "滚动条区域按下应捕获滚动容器");
        // 向下拖 30px → 内容按 content/view 比例移动
        let mv = PointerEvent::single(PointerKind::Move, Point::new(95, 40), MouseButton::Left);
        tree.dispatch_pointer(mv, &mut h, &mut cap);
        tree.layout_root(Size::new(100, 100), &mut te);
        assert!(tree.get(id).unwrap().scroll_y > 0, "拖动滚动条应增加偏移");
    }

    #[test]
    fn vis_cond_toggles_visibility() {
        let flag = Rc::new(Cell::new(false));
        let f2 = flag.clone();
        let root = Element::col()
            .width(100)
            .height(100)
            .child(Element::button("X").visible_when(move || f2.get()));
        let tree = layout(root, 100, 100);
        assert_eq!(tree.focusable_order().len(), 0, "隐藏时不可聚焦");
        flag.set(true);
        assert_eq!(tree.focusable_order().len(), 1, "显示后可聚焦");
    }

    #[test]
    fn text_input_edits_via_keys() {
        let txt = Rc::new(RefCell::new(String::new()));
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::text_input(txt.clone(), "ph"));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 40), &mut te);
        let input = tree.get(id).unwrap().children[0];
        let key = |k: Key| KeyEvent { key: k, pressed: true, shift: false, ctrl: false };
        tree.dispatch_key(key(Key::Char('a')), Some(input));
        tree.dispatch_key(key(Key::Char('中')), Some(input));
        assert_eq!(&*txt.borrow(), "a中", "应插入字符");
        tree.dispatch_key(key(Key::Backspace), Some(input));
        assert_eq!(&*txt.borrow(), "a", "退格应删除一个字符");
    }

    fn input_tree(initial: &str) -> (Tree, NodeId, Rc<RefCell<String>>) {
        let txt = Rc::new(RefCell::new(String::from(initial)));
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::text_input(txt.clone(), "ph"));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 40), &mut te);
        let input = tree.get(id).unwrap().children[0];
        (tree, input, txt)
    }

    #[test]
    fn text_input_select_all_and_replace() {
        let (mut tree, input, txt) = input_tree("hello");
        let k = |key, ctrl| KeyEvent { key, pressed: true, shift: false, ctrl };
        tree.dispatch_key(k(Key::Other(0x41), true), Some(input)); // Ctrl+A 全选
        tree.dispatch_key(k(Key::Char('X'), false), Some(input));
        assert_eq!(&*txt.borrow(), "X", "全选后输入应替换全部");
    }

    #[test]
    fn text_input_home_and_delete() {
        let (mut tree, input, txt) = input_tree("abc");
        let k = |key| KeyEvent { key, pressed: true, shift: false, ctrl: false };
        tree.dispatch_key(k(Key::Home), Some(input)); // 光标到行首
        tree.dispatch_key(k(Key::Delete), Some(input)); // 删首字符
        assert_eq!(&*txt.borrow(), "bc", "Home 后 Delete 应删除首字符");
    }

    #[test]
    fn text_input_shift_select_then_backspace() {
        let (mut tree, input, txt) = input_tree("abc");
        // 光标在末尾(=3)，Shift+Left 选中最后一个字符，退格删除选区
        let shift_left = KeyEvent { key: Key::Left, pressed: true, shift: true, ctrl: false };
        tree.dispatch_key(shift_left, Some(input));
        let bs = KeyEvent { key: Key::Backspace, pressed: true, shift: false, ctrl: false };
        tree.dispatch_key(bs, Some(input));
        assert_eq!(&*txt.borrow(), "ab", "Shift 选区后退格应删除选区");
    }

    struct SharedClip(Rc<RefCell<String>>);
    impl ClipboardProvider for SharedClip {
        fn get_text(&self) -> Option<String> {
            Some(self.0.borrow().clone())
        }
        fn set_text(&self, t: &str) {
            *self.0.borrow_mut() = t.to_string();
        }
    }

    #[test]
    fn text_input_copy_and_paste() {
        let clip = Rc::new(RefCell::new(String::new()));
        let (mut tree, input, txt) = input_tree("hello");
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        let k = |key, ctrl| KeyEvent { key, pressed: true, shift: false, ctrl };
        tree.dispatch_key(k(Key::Other(0x41), true), Some(input)); // Ctrl+A 全选
        tree.dispatch_key(k(Key::Other(0x43), true), Some(input)); // Ctrl+C 复制
        assert_eq!(&*clip.borrow(), "hello", "复制应写入剪贴板");
        tree.dispatch_key(k(Key::End, false), Some(input)); // 光标到末尾、清选区
        tree.dispatch_key(k(Key::Other(0x56), true), Some(input)); // Ctrl+V 粘贴
        assert_eq!(&*txt.borrow(), "hellohello", "粘贴应在光标处插入剪贴板文本");
    }

    #[test]
    fn password_input_blocks_copy_allows_paste() {
        let clip = Rc::new(RefCell::new(String::from("seed")));
        let txt = Rc::new(RefCell::new(String::from("secret")));
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::text_input(txt.clone(), "ph").password());
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 40), &mut te);
        let input = tree.get(id).unwrap().children[0];
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        let k = |key, ctrl| KeyEvent { key, pressed: true, shift: false, ctrl };
        tree.dispatch_key(k(Key::Other(0x41), true), Some(input)); // Ctrl+A 全选
        tree.dispatch_key(k(Key::Other(0x43), true), Some(input)); // Ctrl+C
        assert_eq!(&*clip.borrow(), "seed", "密码模式 Ctrl+C 不得写出明文");
        // 但粘贴仍可用：全选状态下粘贴替换内容。
        tree.dispatch_key(k(Key::Other(0x56), true), Some(input)); // Ctrl+V
        assert_eq!(&*txt.borrow(), "seed", "密码模式仍允许粘贴");
    }

    #[test]
    fn triple_click_selects_all() {
        let (mut tree, input, txt) = input_tree("hello world");
        let b = tree.abs_bounds(input);
        let center = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        let down = PointerEvent {
            kind: PointerKind::Down,
            pos: center,
            button: MouseButton::Left,
            click_count: 3,
        };
        tree.dispatch_pointer(down, &mut h, &mut cap);
        // 全选后输入替换全部内容。
        let key = KeyEvent { key: Key::Char('Z'), pressed: true, shift: false, ctrl: false };
        tree.dispatch_key(key, Some(input));
        assert_eq!(&*txt.borrow(), "Z", "三击全选后输入应替换全部");
    }

    #[test]
    fn double_click_selects_word() {
        // 无 paint 时 index_at 落到 0，故双击选中首词 "hello"。
        let (mut tree, input, txt) = input_tree("hello world");
        let b = tree.abs_bounds(input);
        let center = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        let down = PointerEvent {
            kind: PointerKind::Down,
            pos: center,
            button: MouseButton::Left,
            click_count: 2,
        };
        tree.dispatch_pointer(down, &mut h, &mut cap);
        let key = KeyEvent { key: Key::Char('Z'), pressed: true, shift: false, ctrl: false };
        tree.dispatch_key(key, Some(input));
        assert_eq!(&*txt.borrow(), "Z world", "双击应选中首词并被输入替换");
    }
}
