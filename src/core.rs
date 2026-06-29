//! 核心层：generational arena + Node 树 + Measure/Arrange/Paint 三阶段。
//!
//! 关键设计：布局递归由 `Tree` 独占 `&mut self` 驱动；`Widget` trait 退化为
//! 纯内容（只报固有尺寸、只画自身 content rect，绝不访问树），从根上避免
//! Rust 借用冲突。容器节点的 `widget` 为 `EmptyWidget`，视觉由 `Style` 表达。

use std::cell::Cell;
use std::path::PathBuf;

use crate::signal::Signal;

use crate::event::{
    CursorShape, Event, KeyEvent, MenuItem, MenuRequest, MouseButton, PointerEvent, PointerKind,
    ToastKind, ToastRequest, WindowOp,
};
use crate::geometry::{Color, Insets, Point, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::spec::{Align, Axis, Dimension, MeasureMode, MeasureSpec};
use crate::style::Style;
use crate::text::TextEngine;

/// 点击/激活回调类型。
pub type ClickFn = Box<dyn FnMut(&mut EventCtx)>;

/// 文件拖放回调类型：收到落在本节点（或其子节点冒泡上来）的文件路径列表。
pub type DropFn = Box<dyn FnMut(&mut EventCtx, &[PathBuf])>;
/// 右键上下文菜单构建回调：返回该次菜单项（空 = 不弹）。
pub type MenuFn = Box<dyn FnMut() -> Vec<crate::event::MenuItem>>;

/// 失效矩形的抗锯齿外扩余量（逻辑像素）。与宿主局部重绘的余量同源。
const DAMAGE_MARGIN: i32 = 2;

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
    /// `focused`=本节点是否持有键盘焦点，`enabled`=本节点有效启用态（已并入父链继承；
    /// 交互控件据此置灰）。背景/边框由核心层统一绘制；自绘控件可用 `bounds` 画全尺寸背景。
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
    /// 显隐切换时重置交互态（hover/press → 静止，并令下次绘制的补间瞬时落定不动画）。
    /// 框架在节点 `effective_visible` 翻转时调用——避免控件"按下/悬停未释放就被隐藏"，
    /// 其状态/补间冻结、下次显示瞬间闪出旧的按下/悬停态。默认无操作。
    fn reset_interaction(&mut self) {}
    /// 类型擦除下转钩子：供 Builder 对具体控件做类型化配置（如 TextInput 的
    /// 多行/密码开关）。默认返回 None，需要的控件返回 `Some(self)`。
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }
    /// 文本光标在**本节点局部坐标**（相对节点左上角，逻辑 px）的位置：
    /// `(x, y_top, height)`。供宿主定位输入法候选窗。非文本控件返回 None。
    /// 依赖最近一帧 paint 记录的光标位置。
    fn ime_caret(&self) -> Option<(i32, i32, i32)> {
        None
    }
    /// 是否接收非左键（右/中键）的按下/抬起。默认 false——右键**不**作为单击，
    /// 符合桌面习惯。仅需右键交互的控件（如 TextInput 的上下文菜单）返回 true。
    fn wants_right_click(&self) -> bool {
        false
    }
    /// 指针悬停于本控件时期望的光标形状。默认箭头；链接返回 `Hand`、文本输入返回 `Text`。
    /// 宿主取当前悬停节点的形状交平台应答；禁用节点由宿主统一回退 `Arrow`。
    fn cursor(&self) -> CursorShape {
        CursorShape::Arrow
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
    /// 本节点自身启用态（不含父链继承）：静态/响应式启用标志与启用条件闭包取与。
    pub fn own_enabled(&self) -> bool {
        self.enabled.as_ref().is_none_or(|c| c.get())
            && self.en_cond.as_ref().map(|f| f()).unwrap_or(true)
    }
}

/// 容器布局算法。`None` 表示叶子。
#[derive(Clone, Copy)]
pub enum Layout {
    None,
    Linear {
        axis: Axis,
        spacing: i32,
        cross: Align,
    },
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
    /// 自身启用标志（None=无约束）。禁用沿父链继承：核心据有效启用态拦事件、
    /// 跳焦点，并把启用态传入 `Widget::paint` 供控件置灰。
    pub enabled: Option<Signal<bool>>,
    /// 运行期启用条件（如设置项的 enabled_when 联动）。与 `enabled` 取与：
    /// 返回 false 则该节点（及子树）置灰、不可交互，但仍占位参与布局/绘制（区别于 vis_cond）。
    pub en_cond: Option<Box<dyn Fn() -> bool>>,
    /// 文件拖放回调（None=不接收拖放）。落点命中本节点或其子节点时，沿父链冒泡
    /// 到首个设了回调的节点触发；放在 fill 容器/根上即等价"全窗拖放"。
    pub on_drop: Option<DropFn>,
    /// 右键上下文菜单构建回调（None=不弹）。落点命中本节点或子节点时沿父链冒泡到
    /// 首个设了回调的节点触发，返回的项交宿主以级联浮层呈现。
    pub context_menu: Option<MenuFn>,
    /// 是否为窗口拖动区（自定义标题栏）：无边框窗口中在此区域按下可拖动窗口。
    /// 命中沿父链继承（标记容器即其内非交互区均可拖），但落在子交互控件上不拖动。
    pub window_drag: bool,
    /// 悬停提示文本（None=无）。宿主在悬停延时后于指针附近绘制浮层；
    /// 像 `enabled`/`window_drag` 一样挂在节点上，适用于任意控件/容器。
    pub tooltip: Option<String>,
    /// 当前是否持有键盘焦点（由 UiHost 维护，核心层据此绘制焦点环）。
    pub focused: bool,
    /// 是否把子节点裁剪到自身内容区（滚动容器等）。
    pub clip_children: bool,
    /// 垂直滚动偏移（Scroll 容器）。
    pub scroll_y: i32,
    /// 内容总高（measure 记录，用于滚动钳制与滚动条）。
    pub content_h: i32,
    /// 越界回弹的瞬时视觉偏移（不参与钳制，仅惯性撞界时短暂非零）。
    /// 正=内容下移（顶部回弹），负=内容上移（底部回弹）。
    pub over_scroll: i32,
    /// 上一次 `reset_hidden_interactions` 扫描时的有效可见性（显隐翻转检测用）。
    pub prev_visible: Cell<bool>,
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
            NodeId {
                index: idx,
                generation: slot.generation,
            }
        } else {
            let idx = self.slots.len() as u32;
            self.slots.push(Slot {
                generation: 0,
                node: Some(node),
            });
            NodeId {
                index: idx,
                generation: 0,
            }
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
            self.measure(
                root,
                MeasureSpec::exactly(size.w),
                MeasureSpec::exactly(size.h),
                text,
            );
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
                // 叶子：纯内容固有尺寸（可能需要测量文本）。按节点字重注入线程局部，
                // 使文本测量与绘制走同一字重（加粗标题宽度不被低估而误裁/误换行）。
                let n = self.get(id).unwrap();
                crate::text::set_weight(n.style.font_weight);
                let m = n
                    .widget
                    .measure(Size::new(avail_w, avail_h), &n.style, text);
                crate::text::set_weight(crate::text::WEIGHT_NORMAL);
                m
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
        let (main_spec, cross_spec) = if horizontal {
            (wspec, hspec)
        } else {
            (hspec, wspec)
        };
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
            let main_eff = if matches!(main_dim, Dimension::Match) {
                Dimension::Wrap
            } else {
                main_dim
            };
            let main_child = child_spec(main_eff, main_avail, main_unbounded);
            let cross_child = child_spec(cross_dim, cross_avail, cross_unbounded);
            let (cwspec, chspec) = if horizontal {
                (main_child, cross_child)
            } else {
                (cross_child, main_child)
            };
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
                let (cwspec, chspec) = if horizontal {
                    (main_child, cross_child)
                } else {
                    (cross_child, main_child)
                };
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
            Layout::Linear {
                axis,
                spacing,
                cross,
            } => self.arrange_linear(id, inner, axis, spacing, cross),
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
        let over = self.get(id).map(|n| n.over_scroll).unwrap_or(0);
        if let Some(n) = self.get_mut(id) {
            n.scroll_y = scroll_y;
        }
        // 可滚动时为右侧滚动条预留宽度，避免内容被遮挡。
        let scrollbar_w = if content_h > inner.h { 8 } else { 0 };
        // 子节点从视口顶起按内容顺序堆叠，整体上移 scroll_y；over_scroll 为越界回弹瞬时偏移。
        let children = self.visible_children(id);
        let mut y = inner.y - scroll_y + over;
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
            let cm_cross_total = if horizontal {
                cm.vertical()
            } else {
                cm.horizontal()
            };
            let cm_main_end = if horizontal { cm.right } else { cm.bottom };

            let cross_avail = (cross_avail_full - cm_cross_total).max(0);
            // None=继承容器交叉轴对齐；Some=显式覆盖（含显式 Start）。
            let eff_align = self.get(c).and_then(|n| n.align).unwrap_or(cross);
            let cross_size = if eff_align == Align::Stretch {
                cross_avail
            } else {
                s_cross
            };
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
            self.paint_node(canvas, root, Point::new(0, 0), true);
        }
    }

    fn paint_node(&self, canvas: &mut dyn Canvas, id: NodeId, origin: Point, parent_enabled: bool) {
        let n = match self.get(id) {
            Some(n) if n.effective_visible() => n,
            _ => return,
        };
        // 有效启用态 = 父链启用 ∧ 自身启用；向下传递实现父禁用子跟随。
        let enabled = parent_enabled && n.own_enabled();
        let abs = Rect::new(
            origin.x + n.bounds.x,
            origin.y + n.bounds.y,
            n.bounds.w,
            n.bounds.h,
        );
        if abs.is_empty() {
            return;
        }
        let (fx, fy, fw, fh) = (abs.x as f32, abs.y as f32, abs.w as f32, abs.h as f32);
        let radius = n.style.corner_radius;

        // 子树整体不透明度：<1 时入离屏层，绘完整棵子树后按 opacity 合成回父层。
        let use_layer = n.style.opacity < 1.0;
        if use_layer {
            canvas.push_layer(n.style.opacity);
        }

        let theme = crate::theme::current();
        // 投影：在背景之下、按 spread 外扩并按 dx/dy 偏移后柔化绘制。
        if let Some(sh) = &n.style.shadow {
            if sh.color.a > 0 {
                let sp = sh.spread;
                canvas.draw_shadow(
                    fx - sp + sh.dx,
                    fy - sp + sh.dy,
                    fw + 2.0 * sp,
                    fh + 2.0 * sp,
                    (radius + sp).max(0.0),
                    sh.blur,
                    sh.color,
                );
            }
        }
        if let Some(bg) = &n.style.bg {
            canvas.fill_round_rect(fx, fy, fw, fh, radius, &bg.resolve_paint(&theme));
        }
        if let Some((bc, bw)) = &n.style.border {
            if *bw > 0 {
                let bp = Paint::fill(bc.solid_color(&theme));
                canvas.stroke_round_rect(fx, fy, fw, fh, radius, *bw as f32, &bp);
            }
        }

        let content = abs.inset(n.padding);
        // 标记当前节点矩形：节点内的 anim::request_repaint 会把脏区归到此处（局部重绘用）。
        crate::anim::set_paint_rect(Some(abs));
        // 按节点字重注入线程局部，使 widget 内的文字绘制按 Style.font_weight 取字体格式。
        crate::text::set_weight(n.style.font_weight);
        n.widget
            .paint(abs, content, n.focused, enabled, canvas, &n.style);
        crate::text::set_weight(crate::text::WEIGHT_NORMAL);
        crate::anim::set_paint_rect(None);

        // 焦点环：仅在键盘导航时（focus_ring_visible）绘制，纯鼠标操作不显示。
        if n.focused && self.focus_ring_visible {
            let ring = crate::theme::current().palette.accent;
            canvas.stroke_round_rect(
                fx - 1.0,
                fy - 1.0,
                fw + 2.0,
                fh + 2.0,
                radius + 1.0,
                2.0,
                &Paint::fill(ring),
            );
        }

        let child_origin = Point::new(abs.x, abs.y);
        if n.clip_children {
            canvas.save();
            canvas.clip_rect(content);
            for &c in &n.children {
                self.paint_node(canvas, c, child_origin, enabled);
            }
            canvas.restore();
        } else {
            for &c in &n.children {
                self.paint_node(canvas, c, child_origin, enabled);
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
            let thumb = crate::theme::current().palette.border;
            canvas.fill_round_rect(
                tx,
                thumb_y,
                track_w,
                thumb_h,
                track_w / 2.0,
                &Paint::fill(thumb),
            );
        }

        if use_layer {
            canvas.pop_layer();
        }
    }
}

// ---- 事件分发 ----

/// 失效请求：控件/宿主上报"哪里需要刷新"。事件期由 `EventCtx` 把节点解析为绝对矩形。
///
/// 合并优先级 `None < Rect < Layout < Full`：同为 `Rect`/`Layout` 取并集，遇 `Full` 吞没。
/// Layer 1 中 `Layout` 暂等价整窗（宿主置 `needs_full`），其携带的矩形供后续 Layer 2 精确重排用。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub enum DamageReq {
    /// 无失效。
    #[default]
    None,
    /// 仅重画该绝对矩形（不改布局）：hover/按下/光标移动/补间等。
    Rect(Rect),
    /// 该绝对矩形对应子树需重排（尺寸/结构变化）：滚动、文本增删等。
    Layout(Rect),
    /// 整窗重绘（无法局部化）。
    Full,
}

impl DamageReq {
    fn rank(self) -> u8 {
        match self {
            DamageReq::None => 0,
            DamageReq::Rect(_) => 1,
            DamageReq::Layout(_) => 2,
            DamageReq::Full => 3,
        }
    }
    /// 合并两个失效请求（取更强者；同级矩形取并集）。
    pub fn merge(self, o: DamageReq) -> DamageReq {
        use DamageReq::*;
        match (self, o) {
            (Full, _) | (_, Full) => Full,
            (Layout(a), Layout(b)) => Layout(a.union(&b)),
            (Layout(a), Rect(b)) | (Rect(b), Layout(a)) => Layout(a.union(&b)),
            (Rect(a), Rect(b)) => Rect(a.union(&b)),
            // 其余必含 None：取 rank 更高一方。
            (a, b) => {
                if a.rank() >= b.rank() {
                    a
                } else {
                    b
                }
            }
        }
    }
    fn merge_with(&mut self, o: DamageReq) {
        *self = (*self).merge(o);
    }
}

/// 一次事件处理累积的副作用指令。
#[derive(Default)]
pub(crate) struct EventOutcome {
    repaint: bool,
    /// 本次处理上报的失效区域（节点已在 `EventCtx` 解析为绝对矩形）。
    damage: DamageReq,
    /// Some(Some(id))=设置捕获；Some(None)=释放捕获。
    capture: Option<Option<NodeId>>,
    close: bool,
    focus: Option<NodeId>,
    /// 控件请求弹出的上下文菜单（宿主接管渲染与命中）。
    menu: Option<MenuRequest>,
    /// 控件请求宿主用系统默认程序打开的 URL/路径（链接点击等）。
    open_url: Option<String>,
    /// 控件请求的窗口操作（最小化/最大化切换，自定义标题栏按钮触发）。
    window_op: Option<WindowOp>,
    /// 控件请求弹出的轻提示（宿主接管居中浮层渲染与定时消失）。
    toast: Option<ToastRequest>,
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
    /// 请求重绘本控件（纯视觉变化，不改布局）。失效区域取本节点视觉矩形（含投影/焦点环）。
    pub fn mark_dirty(&mut self) {
        let r = self.tree.visual_bounds(self.self_id);
        self.out.damage.merge_with(DamageReq::Rect(r));
        self.out.repaint = true;
    }
    /// 请求重绘一个比自身更大的绝对区域（投影/溢出绘制超出本框时用）。
    pub fn mark_dirty_rect(&mut self, r: Rect) {
        self.out.damage.merge_with(DamageReq::Rect(r));
        self.out.repaint = true;
    }
    /// 本控件尺寸/子结构变化，需重排（Layer 1 暂等价整窗）。
    pub fn mark_layout_dirty(&mut self) {
        let r = self.tree.visual_bounds(self.self_id);
        self.out.damage.merge_with(DamageReq::Layout(r));
        self.out.repaint = true;
    }
    /// 整窗重绘：当本次改动影响到**本控件矩形之外**的区域时使用——例如改写了被其他
    /// 节点读取的共享状态（单选组同伴、`visible_when` 绑定的显隐标志）。在读者订阅
    /// （Signal Phase 2）落地前，这是非局部变更的安全兜底。
    pub fn mark_dirty_all(&mut self) {
        self.out.damage.merge_with(DamageReq::Full);
        self.out.repaint = true;
    }
    /// 修改本节点背景色并重绘（交互态切换常用）。
    pub fn set_bg(&mut self, c: Color) {
        if let Some(n) = self.tree.get_mut(self.self_id) {
            n.style.bg = Some(crate::style::Brush::Solid(c));
        }
        self.mark_dirty();
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
        self.mark_layout_dirty();
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
        self.mark_layout_dirty();
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
    /// 请求在 `pos`（逻辑坐标）弹出浮层菜单。宿主接管渲染、命中与项激活。
    /// `min_width`：最小宽度（0=按内容；下拉传控件宽度对齐）。
    pub fn show_menu(&mut self, pos: Point, items: Vec<MenuItem>, min_width: i32) {
        self.out.menu = Some(MenuRequest {
            pos,
            items,
            min_width,
            anchor_top: None,
        });
        self.out.repaint = true;
    }
    /// 请求在 `pos` 弹出上下文菜单（内容宽度）。
    pub fn show_context_menu(&mut self, pos: Point, items: Vec<MenuItem>) {
        self.show_menu(pos, items, 0);
    }
    /// 下拉控件专用：按控件 bounds 弹出浮层，空间不足时自动向上翻转以避免遮住控件。
    pub fn show_dropdown_menu(&mut self, bounds: crate::geometry::Rect, items: Vec<MenuItem>) {
        self.out.menu = Some(MenuRequest {
            pos: Point::new(bounds.x, bounds.y + bounds.h),
            items,
            min_width: bounds.w,
            anchor_top: Some(bounds.y),
        });
        self.out.repaint = true;
    }
    /// 请求宿主用系统默认程序打开 URL/路径（链接点击等）。fire-and-forget：
    /// 经 `DispatchResult` 上交宿主，由平台执行（win32 `ShellExecuteW`），核心保持平台无关。
    pub fn open_url(&mut self, url: &str) {
        self.out.open_url = Some(url.to_string());
    }
    /// 请求最小化窗口（自定义标题栏的最小化按钮）。
    pub fn minimize(&mut self) {
        self.out.window_op = Some(WindowOp::Minimize);
    }
    /// 请求最大化/还原切换（自定义标题栏的最大化按钮）。
    pub fn toggle_maximize(&mut self) {
        self.out.window_op = Some(WindowOp::ToggleMaximize);
    }

    /// 默认时长（毫秒）：约 1.8s，含淡入淡出。
    const TOAST_DEFAULT_MS: u64 = 1800;

    /// 弹出轻提示（中性信息）。居中浮层 + 淡入淡出 + 定时自动消失，由宿主接管。
    /// **脱离布局树**——不绑定任何节点，任意控件回调内 `ctx.toast("…")` 即可。
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast_with(text, ToastKind::Info, Self::TOAST_DEFAULT_MS);
    }
    /// 弹出成功轻提示（✓ 图标），如"已添加到剪贴板"。
    pub fn toast_ok(&mut self, text: impl Into<String>) {
        self.toast_with(text, ToastKind::Success, Self::TOAST_DEFAULT_MS);
    }
    /// 弹出错误轻提示（✕ 图标）。
    pub fn toast_err(&mut self, text: impl Into<String>) {
        self.toast_with(text, ToastKind::Error, Self::TOAST_DEFAULT_MS);
    }
    /// 弹出轻提示（完全指定语义与时长）。`duration_ms` 含淡入淡出。
    pub fn toast_with(&mut self, text: impl Into<String>, kind: ToastKind, duration_ms: u64) {
        self.out.toast = Some(ToastRequest {
            text: text.into(),
            kind,
            duration_ms,
        });
        self.out.repaint = true;
    }
}

/// 指针/键盘分发的对外结果。
#[derive(Default, Clone)]
pub struct DispatchResult {
    pub repaint: bool,
    /// 本次分发累积的失效区域（宿主据此选择局部/整窗重绘）。
    pub damage: DamageReq,
    pub close: bool,
    pub focus: Option<NodeId>,
    /// 事件是否被某个控件消费（供宿主决定是否回退到默认行为，如 Escape 关窗）。
    pub consumed: bool,
    /// 控件请求弹出的上下文菜单（宿主接管）。
    pub menu: Option<MenuRequest>,
    /// 控件请求宿主打开的 URL/路径（链接点击等）。
    pub open_url: Option<String>,
    /// 控件请求的窗口操作（最小化/最大化切换）。
    pub window_op: Option<WindowOp>,
    /// 控件请求弹出的轻提示（宿主接管居中浮层渲染与定时消失）。
    pub toast: Option<ToastRequest>,
}

impl Tree {
    /// 节点有效启用态：自身与所有祖先均启用才为 true（父链继承）。
    pub fn node_enabled(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(i) = cur {
            match self.get(i) {
                Some(n) => {
                    if !n.own_enabled() {
                        return false;
                    }
                    cur = n.parent;
                }
                None => break,
            }
        }
        true
    }

    /// 节点期望的光标形状（取其控件声明；节点缺失回退 Arrow）。
    /// 禁用回退由宿主在查询前进行处理（见 `App` 的 `cursor()`）。
    pub fn cursor_at(&self, id: NodeId) -> CursorShape {
        self.get(id)
            .map(|n| n.widget.cursor())
            .unwrap_or(CursorShape::Arrow)
    }

    /// 节点的悬停提示文本（无则 None）。宿主据此在悬停延时后绘制浮层。
    pub fn node_tooltip(&self, id: NodeId) -> Option<String> {
        self.get(id).and_then(|n| n.tooltip.clone())
    }

    /// `pos`（逻辑坐标）是否落在交互控件上（可聚焦节点，如自定义标题栏的窗口按钮）。
    /// 平台据此在 `WM_NCHITTEST` 把控件区强制判为 HTCLIENT——优先于缩放边框，
    /// 使整个按钮都是客户区、普通鼠标移动全程覆盖，避免顶部缩放条夺走 hover。
    pub fn interactive_hit_at(&self, pos: Point) -> bool {
        let Some(hit) = self.hit_test(pos) else {
            return false;
        };
        self.get(hit).map(|n| n.widget.focusable()).unwrap_or(false)
    }

    /// `pos`（逻辑坐标）是否落在窗口拖动区（自定义标题栏）。命中的是可聚焦控件
    /// （按钮等）则不拖动——交控件处理；否则自身或任一祖先标了 `window_drag` 即可拖。
    pub fn drag_hit_at(&self, pos: Point) -> bool {
        let Some(hit) = self.hit_test(pos) else {
            return false;
        };
        if self.get(hit).map(|n| n.widget.focusable()).unwrap_or(false) {
            return false;
        }
        self.ancestor_chain(hit)
            .iter()
            .any(|&id| self.get(id).map(|n| n.window_drag).unwrap_or(false))
    }

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

    /// 节点用于失效的**视觉矩形**（逻辑坐标）：在 `abs_bounds` 基础上外扩，覆盖控件全部可见
    /// 像素——抗锯齿余量、焦点环（外扩 1px 描 2px）、投影（spread+blur 再叠 |dx|/|dy|）。
    /// 局部重绘据此取脏区；原则宁大勿漏，避免残影。
    pub fn visual_bounds(&self, id: NodeId) -> Rect {
        let abs = self.abs_bounds(id);
        let n = match self.get(id) {
            Some(n) => n,
            None => return abs,
        };
        // 焦点环在框外 1px、线宽 2px → 至少 3px 余量；否则 AA 余量 2px。
        let mut pad = if n.focused { 3 } else { DAMAGE_MARGIN };
        if let Some(sh) = &n.style.shadow {
            if sh.color.a > 0 {
                let ext = (sh.spread + sh.blur).ceil() as i32
                    + (sh.dx.abs().max(sh.dy.abs())).ceil() as i32;
                pad = pad.max(ext);
            }
        }
        abs.inflate(pad)
    }

    /// 全树**结构签名**：对每个存活节点哈希 (索引, 代际, 有效可见, 有效启用, bounds)。
    /// 用于交互后判定"是否发生了显隐/启用/位移/尺寸变化"——签名不变即本次仅为局部视觉
    /// 变化（可局部重绘），变了则说明结构改变（影响区域不可局部化，需整窗）。
    /// 注：`own_enabled()` 含 `en_cond` 闭包求值，确保 `enabled_when` 联动能被签名感知。
    pub fn layout_signature(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(n) = &slot.node {
                (i as u32).hash(&mut h);
                slot.generation.hash(&mut h);
                n.effective_visible().hash(&mut h);
                n.own_enabled().hash(&mut h);
                let b = n.bounds;
                (b.x, b.y, b.w, b.h).hash(&mut h);
            }
        }
        h.finish()
    }

    /// 显隐翻转后重置交互态：从根遍历，按**祖先链累积可见性**（父隐藏则子也隐藏）对每个
    /// 节点判定真实可见，对**由可见变为隐藏**者调 `Widget::reset_interaction`（清 hover/press、
    /// 令补间瞬时落定）。修正"控件在按下/悬停态被隐藏（如关闭它所在的对话框）、其状态/动画
    /// 冻结、下次显示瞬间闪出旧态"。
    ///
    /// 注意：必须用累积可见性而非节点局部 `effective_visible`——对话框关闭只翻转对话框节点本身，
    /// 其子节点（关闭按钮等）的局部可见性不变，仅靠局部判定会漏掉它们。
    /// 由宿主在结构签名变化时调用（对齐 Flutter MouseTracker / Qt 模态弹出补发 leave 的做法）。
    pub fn reset_hidden_interactions(&mut self) {
        if let Some(root) = self.root {
            self.reset_hidden_rec(root, true);
        }
    }

    fn reset_hidden_rec(&mut self, id: NodeId, parent_visible: bool) {
        let (vis, children, transitioned) = match self.get(id) {
            Some(n) => {
                let v = parent_visible && n.effective_visible();
                let prev = n.prev_visible.replace(v);
                (v, n.children.clone(), prev && !v)
            }
            None => return,
        };
        if transitioned {
            if let Some(n) = self.get_mut(id) {
                n.widget.reset_interaction();
            }
        }
        for c in children {
            self.reset_hidden_rec(c, vis);
        }
    }

    /// 节点的文本光标绝对位置（逻辑坐标）+ 高度：`(左上角, height)`。
    /// 用于宿主定位输入法候选窗。节点非文本控件或无光标时返回 None。
    pub fn caret_of(&self, id: NodeId) -> Option<(Point, i32)> {
        let n = self.get(id)?;
        let (lx, ly, h) = n.widget.ime_caret()?;
        let abs = self.abs_bounds(id);
        Some((Point::new(abs.x + lx, abs.y + ly), h))
    }

    /// 找 `p`（逻辑坐标）下最近的滚动容器节点（命中点向上找首个 `Layout::Scroll`）。
    pub fn scroll_node_at(&self, p: Point) -> Option<NodeId> {
        let mut cur = self.hit_test(p);
        while let Some(id) = cur {
            let n = self.get(id)?;
            if matches!(n.layout, Layout::Scroll) {
                return Some(id);
            }
            cur = n.parent;
        }
        None
    }

    /// 滚动节点的 `(当前偏移, 最大偏移)`（基于上一帧布局的内容高/视口高）。
    /// 非滚动节点返回 None。供惯性滑动按边界结算。
    pub fn scroll_range(&self, id: NodeId) -> Option<(i32, i32)> {
        let n = self.get(id)?;
        if !matches!(n.layout, Layout::Scroll) {
            return None;
        }
        let view_h = (n.bounds.h - n.padding.vertical()).max(0);
        Some((n.scroll_y, (n.content_h - view_h).max(0)))
    }

    /// 直接设置滚动节点偏移（惯性滑动用，不钳制；下一帧 arrange 钳制）。
    /// 节点不存在或非滚动容器时返回 false。
    pub fn set_scroll_y(&mut self, id: NodeId, y: i32) -> bool {
        match self.get_mut(id) {
            Some(n) if matches!(n.layout, Layout::Scroll) => {
                n.scroll_y = y;
                true
            }
            _ => false,
        }
    }

    /// 设置滚动节点的越界回弹偏移（不参与钳制；惯性撞界回弹用）。
    pub fn set_over_scroll(&mut self, id: NodeId, over: i32) {
        if let Some(n) = self.get_mut(id) {
            n.over_scroll = over;
        }
    }

    /// 触摸平移滚动：找 `p`（逻辑坐标）下最近的滚动容器，按 `dy`（逻辑 px）平移。
    /// `dy>0`（手指下移）→ 内容下移（scroll_y 减小，自然跟手）。下一帧 arrange 钳制范围。
    /// 返回是否命中可滚动容器。
    pub fn pan_scroll(&mut self, p: Point, dy: i32) -> bool {
        if let Some(id) = self.scroll_node_at(p) {
            if let Some(n) = self.get_mut(id) {
                n.scroll_y -= dy;
            }
            return true;
        }
        false
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
        let abs = Rect::new(
            origin.x + n.bounds.x,
            origin.y + n.bounds.y,
            n.bounds.w,
            n.bounds.h,
        );
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
            if !n.effective_visible() || !n.own_enabled() {
                // 禁用子树整体退出 Tab 导航（own_enabled 在递归中实现父链继承）。
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
        // 禁用节点（含父链禁用）不接收任何事件：不消费 → 自然冒泡到祖先。
        if !self.node_enabled(id) {
            return (false, EventOutcome::default());
        }
        let mut widget = match self.get_mut(id) {
            Some(n) => std::mem::replace(&mut n.widget, Box::new(EmptyWidget)),
            None => return (false, EventOutcome::default()),
        };
        let mut ctx = EventCtx {
            tree: self,
            self_id: id,
            out: EventOutcome::default(),
        };
        // 括起事件期：期间 Signal::set 仅记"写过信号"，不强制整窗。
        crate::signal::begin_event();
        let consumed = widget.on_event(&mut ctx, ev);
        let mut out = ctx.out;
        // 事件内写过信号但控件未显式 mark_dirty → 据事件类型选择失效强度：
        // - Move(hover)：写的是自身悬停态，局部重绘即可；
        // - Key：打字高频，保留局部重绘避免整窗卡顿；
        // - 其余指针事件(Down/Up/Click 等)：可能写跨控件共享状态（计数器、enabled_when 门控），
        //   升 Layout 使 apply_damage 直接置 needs_full，覆盖所有读者（含 DynLabel/en_cond）。
        if crate::signal::end_event() {
            let r = self.visual_bounds(id);
            let is_hover_or_key = matches!(
                ev,
                Event::Pointer(ref pe) if pe.kind == crate::event::PointerKind::Move
            ) || matches!(ev, Event::Key(_));
            let d = if is_hover_or_key {
                DamageReq::Rect(r)
            } else {
                DamageReq::Layout(r)
            };
            out.damage = out.damage.merge(d);
            out.repaint = true;
        }
        match self.get_mut(id) {
            Some(n) => n.widget = widget,
            None => debug_assert!(
                false,
                "on_event 回调内删除了 self 节点，违反 call_on_event 契约"
            ),
        }
        (consumed, out)
    }

    /// hover 目标变化时沿**祖先链**派发 Leave/Enter：旧链中不在新链的节点收 Leave（叶→根序），
    /// 新链中不在旧链的节点收 Enter（根→叶序）。匹配 DOM mouseenter/mouseleave 传播语义——
    /// hover 一个子节点等于 hover 其所有祖先。
    ///
    /// 关键：命中测试返回**最深**节点，但可点击容器（如带 label 子节点的表格单元格）的
    /// hover/press 态由点击冒泡设上，其子节点拦截了命中点，单点派发的 Leave 永远到不了
    /// 容器 → 高亮卡住（"点击过的一直高亮"）。沿祖先链派发即修正。
    fn dispatch_hover_change(
        &mut self,
        old: Option<NodeId>,
        new: Option<NodeId>,
        ev: &PointerEvent,
        res: &mut DispatchResult,
    ) {
        let old_chain = old.map(|h| self.ancestor_chain(h)).unwrap_or_default();
        let new_chain = new.map(|t| self.ancestor_chain(t)).unwrap_or_default();
        for &id in old_chain.iter().filter(|id| !new_chain.contains(id)) {
            let (_, o) = self.call_on_event(
                id,
                &Event::Pointer(PointerEvent {
                    kind: PointerKind::Leave,
                    ..*ev
                }),
            );
            res.repaint |= o.repaint;
            res.damage = res.damage.merge(o.damage);
        }
        for &id in new_chain.iter().rev().filter(|id| !old_chain.contains(id)) {
            let (_, o) = self.call_on_event(
                id,
                &Event::Pointer(PointerEvent {
                    kind: PointerKind::Enter,
                    ..*ev
                }),
            );
            res.repaint |= o.repaint;
            res.damage = res.damage.merge(o.damage);
        }
    }

    /// 分发指针事件：维护 hover/capture，冒泡处理，汇总副作用。
    pub fn dispatch_pointer(
        &mut self,
        ev: PointerEvent,
        hover: &mut Option<NodeId>,
        capture: &mut Option<NodeId>,
    ) -> DispatchResult {
        let mut res = DispatchResult::default();

        // hover 进出（仅 Move 且无捕获时）：沿祖先链派发，使可点击容器也能收到 Enter/Leave。
        if matches!(ev.kind, PointerKind::Move) && capture.is_none() {
            let target = self.hit_test(ev.pos);
            if *hover != target {
                self.dispatch_hover_change(*hover, target, &ev, &mut res);
                *hover = target;
            }
        }

        // 非左键的按下/抬起：默认不当作单击。只投递给显式接收右键的控件
        // （如 TextInput 上下文菜单），其余跳过——符合桌面右键不激活的习惯。
        let secondary = matches!(ev.kind, PointerKind::Down | PointerKind::Up)
            && ev.button != MouseButton::Left;

        // 主事件：捕获优先，否则命中目标，沿祖先链冒泡。
        let had_capture = capture.is_some();
        let target = capture.or_else(|| self.hit_test(ev.pos));
        if let Some(t) = target {
            for id in self.ancestor_chain(t) {
                if secondary
                    && !self
                        .get(id)
                        .map(|n| n.widget.wants_right_click() || n.context_menu.is_some())
                        .unwrap_or(false)
                {
                    continue;
                }
                let (consumed, o) = self.call_on_event(id, &Event::Pointer(ev));
                res.repaint |= o.repaint;
                res.damage = res.damage.merge(o.damage);
                res.close |= o.close;
                res.consumed |= consumed;
                if o.focus.is_some() {
                    res.focus = o.focus;
                }
                if let Some(cap) = o.capture {
                    *capture = cap;
                }
                if o.menu.is_some() {
                    res.menu = o.menu;
                }
                if o.open_url.is_some() {
                    res.open_url = o.open_url;
                }
                if o.window_op.is_some() {
                    res.window_op = o.window_op;
                }
                if o.toast.is_some() {
                    res.toast = o.toast;
                }
                // 右键上下文菜单：节点设了 context_menu 且 widget 未自行弹菜单时，
                // 构建项并请求级联浮层（沿父链冒泡，命中一个即止）。
                if secondary && matches!(ev.kind, PointerKind::Down) && res.menu.is_none() {
                    if let Some(mut cb) = self.get_mut(id).and_then(|n| n.context_menu.take()) {
                        let items = cb();
                        if let Some(n) = self.get_mut(id) {
                            n.context_menu = Some(cb);
                        }
                        if !items.is_empty() {
                            res.menu = Some(crate::event::MenuRequest {
                                pos: ev.pos,
                                items,
                                min_width: 0,
                                anchor_top: None,
                            });
                            res.consumed = true;
                        }
                    }
                }
                if consumed || res.consumed {
                    break;
                }
            }
        }

        // 捕获在本次（如 Up）被释放后，按当前位置重算 hover 并补发 Enter/Leave，
        // 修正"按下拖到另一控件上释放"后 hover 滞留在原控件的问题。
        if had_capture && capture.is_none() {
            let target = self.hit_test(ev.pos);
            if *hover != target {
                self.dispatch_hover_change(*hover, target, &ev, &mut res);
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
            res.damage = o.damage;
            res.close = o.close;
            res.focus = o.focus;
            res.consumed = consumed;
            res.menu = o.menu;
            res.open_url = o.open_url;
            res.window_op = o.window_op;
            res.toast = o.toast;
        }
        res
    }

    /// 分发文件拖放：命中 `pos`（逻辑坐标）下的节点，沿父链冒泡到首个设了
    /// `on_drop` 的节点并触发（传入文件路径）。禁用子树不接收。返回副作用。
    /// 借用拆解同 `call_on_event`：取出闭包→调用→放回（generation 不匹配则丢弃）。
    pub fn dispatch_files(&mut self, pos: Point, paths: Vec<PathBuf>) -> DispatchResult {
        let mut res = DispatchResult::default();
        let Some(target) = self.hit_test(pos) else {
            return res;
        };
        for id in self.ancestor_chain(target) {
            if !self.node_enabled(id) {
                continue;
            }
            let mut cb = match self.get_mut(id).and_then(|n| n.on_drop.take()) {
                Some(cb) => cb,
                None => continue,
            };
            let mut ctx = EventCtx {
                tree: self,
                self_id: id,
                out: EventOutcome::default(),
            };
            cb(&mut ctx, &paths);
            let out = ctx.out;
            if let Some(n) = self.get_mut(id) {
                n.on_drop = Some(cb); // 放回（节点仍在才放回，遵循 call_on_event 契约）
            }
            res.repaint |= out.repaint;
            res.damage = res.damage.merge(out.damage);
            res.close |= out.close;
            res.consumed = true;
            if out.focus.is_some() {
                res.focus = out.focus;
            }
            if out.open_url.is_some() {
                res.open_url = out.open_url;
            }
            if out.toast.is_some() {
                res.toast = out.toast;
            }
            break; // 命中一个拖放处理者即止
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
    use crate::signal::signal;
    use crate::ui::Element;
    use std::cell::RefCell;
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
        assert!(
            b1.x + b1.w + 10 <= 200,
            "右边界 {} 超出 200",
            b1.x + b1.w + 10
        );
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
    fn rptr(kind: PointerKind, p: Point) -> PointerEvent {
        PointerEvent::single(kind, p, MouseButton::Right)
    }

    #[test]
    fn right_click_does_not_activate_button() {
        let clicks = signal(0);
        let (mut tree, btn) = button_tree(clicks);
        let b = tree.abs_bounds(btn);
        let c = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(rptr(PointerKind::Down, c), &mut hover, &mut cap);
        tree.dispatch_pointer(rptr(PointerKind::Up, c), &mut hover, &mut cap);
        assert_eq!(clicks.get(), 0, "右键不应触发按钮点击");
        assert_eq!(cap, None, "右键不应捕获指针");
    }

    #[test]
    fn right_click_does_not_toggle_checkbox() {
        let state = signal(false);
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::checkbox("x", state));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 40), &mut te);
        let cb = tree.get(id).unwrap().children[0];
        let b = tree.abs_bounds(cb);
        let c = Point::new(b.x + 5, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        tree.dispatch_pointer(rptr(PointerKind::Up, c), &mut h, &mut cap);
        assert!(!state.get(), "右键不应切换复选框");
    }

    fn button_tree(clicks: Signal<i32>) -> (Tree, NodeId) {
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
        let clicks = signal(0);
        let (mut tree, btn) = button_tree(clicks);
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
    fn damage_req_merge_precedence() {
        let r1 = Rect::new(0, 0, 10, 10);
        let r2 = Rect::new(20, 20, 10, 10);
        // None 被吸收。
        assert_eq!(
            DamageReq::None.merge(DamageReq::Rect(r1)),
            DamageReq::Rect(r1)
        );
        // Rect ∪ Rect。
        assert_eq!(
            DamageReq::Rect(r1).merge(DamageReq::Rect(r2)),
            DamageReq::Rect(r1.union(&r2))
        );
        // Layout 强于 Rect，且取并集。
        assert_eq!(
            DamageReq::Rect(r1).merge(DamageReq::Layout(r2)),
            DamageReq::Layout(r1.union(&r2))
        );
        // Full 吞没一切。
        assert_eq!(
            DamageReq::Layout(r1).merge(DamageReq::Full),
            DamageReq::Full
        );
        assert_eq!(DamageReq::Full.merge(DamageReq::Rect(r1)), DamageReq::Full);
    }

    #[test]
    fn button_press_reports_visual_rect_damage() {
        // 按钮按下走 mark_dirty → DispatchResult 应带本节点视觉矩形的 Rect 失效（供局部重绘）。
        let clicks = signal(0);
        let (mut tree, btn) = button_tree(clicks);
        let b = tree.abs_bounds(btn);
        let center = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut hover, mut cap) = (None, None);
        let res = tree.dispatch_pointer(ptr(PointerKind::Down, center), &mut hover, &mut cap);
        match res.damage {
            DamageReq::Rect(r) => {
                assert_eq!(r, tree.visual_bounds(btn), "应为按钮视觉矩形")
            }
            other => panic!("按下应上报 Rect 失效，实得 {other:?}"),
        }
    }

    #[test]
    fn release_outside_does_not_click() {
        let clicks = signal(0);
        let (mut tree, btn) = button_tree(clicks);
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
        let clicks = signal(0);
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

    #[test]
    fn disabled_button_ignores_click_and_skips_focus() {
        let clicks = signal(0);
        let c = clicks;
        let root = Element::col().width(200).height(100).padding(10).child(
            Element::button("OK")
                .on_click(move |_| c.set(c.get() + 1))
                .disabled(true),
        );
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 100), &mut te);
        let btn = tree.get(id).unwrap().children[0];
        let b = tree.abs_bounds(btn);
        let center = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, center), &mut hover, &mut cap);
        tree.dispatch_pointer(ptr(PointerKind::Up, center), &mut hover, &mut cap);
        assert_eq!(clicks.get(), 0, "禁用按钮不应触发点击");
        assert!(
            !tree.focusable_order().contains(&btn),
            "禁用按钮不应进入焦点链"
        );
        assert!(!tree.node_enabled(btn), "node_enabled 应为 false");
    }

    #[test]
    fn disabled_container_propagates_to_children() {
        // 禁用容器 → 内部按钮均不可聚焦（父链继承）。
        let root = Element::col()
            .disabled(true)
            .child(Element::button("A"))
            .child(Element::button("B"));
        let tree = layout(root, 200, 100);
        assert_eq!(tree.focusable_order().len(), 0, "禁用容器内按钮均不可聚焦");
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
        let st = signal(false);
        let root = Element::col()
            .width(200)
            .height(60)
            .padding(5)
            .child(Element::checkbox("启用", st));
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
    fn checkbox_on_toggle_intercepts_and_is_controlled() {
        // 设了 on_toggle 后：点击只触发回调、不自动翻转 state（受控），
        // 渲染完全跟随外部 state——app 可在翻转前弹确认、确认后才置真。
        let st = signal(false);
        let fired = signal(0u32);
        let f = fired;
        let root = Element::col()
            .width(200)
            .height(60)
            .padding(5)
            .child(Element::checkbox("启用", st).on_toggle(move |_| f.set(f.get() + 1)));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 60), &mut te);
        let cb = tree.get(id).unwrap().children[0];

        click(&mut tree, cb);
        assert_eq!(fired.get(), 1, "点击应触发 on_toggle 回调");
        assert!(!st.get(), "受控：设了 on_toggle 后点击不应自动翻转 state");

        // app 决定置真后，state 完全由 app 控制，控件不覆盖它。
        st.set(true);
        click(&mut tree, cb);
        assert_eq!(fired.get(), 2, "再次点击再次回调");
        assert!(st.get(), "state 完全由 app 控制");
    }

    #[test]
    fn radio_group_is_exclusive() {
        let g = signal(0usize);
        let root = Element::row()
            .width(360)
            .height(40)
            .padding(5)
            .spacing(20)
            .child(Element::radio("A", g, 0))
            .child(Element::radio("B", g, 1));
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
        let v = signal(0.0f32);
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::slider(v).width(100));
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
    fn pan_scroll_scrolls_container() {
        let mut sc = Element::scroll().width(100).height(100);
        for _ in 0..10 {
            sc = sc.child(Element::leaf().width_match().height(30));
        }
        let mut tree = Tree::new();
        let id = sc.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te); // content_h=300, max scroll 200
                                                        // 手指上滑(dy<0) → 内容上移 → scroll_y 增大。
        assert!(tree.pan_scroll(Point::new(50, 50), -40), "命中滚动容器");
        tree.layout_root(Size::new(100, 100), &mut te); // 钳制
        assert_eq!(
            tree.get(id).unwrap().scroll_y,
            40,
            "上滑 40px 应增加 scroll_y"
        );
        // 非滚动区域返回 false。
        assert!(
            !tree.pan_scroll(Point::new(-100, -100), 10),
            "命中外返回 false"
        );
    }

    #[test]
    fn scroll_range_and_set_for_fling() {
        let mut sc = Element::scroll().width(100).height(100);
        for _ in 0..10 {
            sc = sc.child(Element::leaf().width_match().height(30));
        }
        let mut tree = Tree::new();
        let id = sc.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te); // content_h=300, view=100 → max=200
                                                        // 惯性滑动定位到的滚动节点。
        assert_eq!(tree.scroll_node_at(Point::new(50, 50)), Some(id));
        let (cur, max) = tree.scroll_range(id).expect("滚动节点应有范围");
        assert_eq!((cur, max), (0, 200), "初始偏移 0、最大 200");
        // 惯性推进越界 → set 后 arrange 钳制；范围读数据反映撞底。
        assert!(tree.set_scroll_y(id, 500), "设置滚动偏移成功");
        tree.layout_root(Size::new(100, 100), &mut te);
        assert_eq!(tree.scroll_range(id).unwrap().0, 200, "越界应钳制到 max");
        // 非滚动节点 / 不存在节点：范围与设置均失败。
        let leaf = tree.get(id).unwrap().children[0];
        assert!(tree.scroll_range(leaf).is_none(), "非滚动节点无范围");
        assert!(!tree.set_scroll_y(leaf, 10), "非滚动节点不可设置滚动");
    }

    #[test]
    fn over_scroll_shifts_content_without_clamping() {
        let mut sc = Element::scroll().width(100).height(100);
        for _ in 0..10 {
            sc = sc.child(Element::leaf().width_match().height(30));
        }
        let mut tree = Tree::new();
        let id = sc.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te);
        let child0 = tree.get(id).unwrap().children[0];
        let y0 = tree.abs_bounds(child0).y;
        // 越界回弹偏移：内容整体下移 12px，且不被 arrange 钳掉（区别于 scroll_y）。
        tree.set_over_scroll(id, 12);
        tree.layout_root(Size::new(100, 100), &mut te);
        assert_eq!(
            tree.get(id).unwrap().over_scroll,
            12,
            "over_scroll 不参与钳制"
        );
        assert_eq!(
            tree.abs_bounds(child0).y,
            y0 + 12,
            "内容随 over_scroll 整体偏移"
        );
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
        let flag = signal(false);
        let f2 = flag;
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
        let txt = signal(String::new());
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::text_input(txt, "ph"));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 40), &mut te);
        let input = tree.get(id).unwrap().children[0];
        let key = |k: Key| KeyEvent {
            key: k,
            pressed: true,
            shift: false,
            ctrl: false,
        };
        tree.dispatch_key(key(Key::Char('a')), Some(input));
        tree.dispatch_key(key(Key::Char('中')), Some(input));
        assert_eq!(txt.get(), "a中", "应插入字符");
        tree.dispatch_key(key(Key::Backspace), Some(input));
        assert_eq!(txt.get(), "a", "退格应删除一个字符");
    }

    fn input_tree(initial: &str) -> (Tree, NodeId, Signal<String>) {
        let txt = signal(String::from(initial));
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::text_input(txt, "ph"));
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
        let k = |key, ctrl| KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl,
        };
        tree.dispatch_key(k(Key::Other(0x41), true), Some(input)); // Ctrl+A 全选
        tree.dispatch_key(k(Key::Char('X'), false), Some(input));
        assert_eq!(txt.get(), "X", "全选后输入应替换全部");
    }

    #[test]
    fn text_input_home_and_delete() {
        let (mut tree, input, txt) = input_tree("abc");
        let k = |key| KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl: false,
        };
        tree.dispatch_key(k(Key::Home), Some(input)); // 光标到行首
        tree.dispatch_key(k(Key::Delete), Some(input)); // 删首字符
        assert_eq!(txt.get(), "bc", "Home 后 Delete 应删除首字符");
    }

    #[test]
    fn text_input_shift_select_then_backspace() {
        let (mut tree, input, txt) = input_tree("abc");
        // 光标在末尾(=3)，Shift+Left 选中最后一个字符，退格删除选区
        let shift_left = KeyEvent {
            key: Key::Left,
            pressed: true,
            shift: true,
            ctrl: false,
        };
        tree.dispatch_key(shift_left, Some(input));
        let bs = KeyEvent {
            key: Key::Backspace,
            pressed: true,
            shift: false,
            ctrl: false,
        };
        tree.dispatch_key(bs, Some(input));
        assert_eq!(txt.get(), "ab", "Shift 选区后退格应删除选区");
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
        let k = |key, ctrl| KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl,
        };
        tree.dispatch_key(k(Key::Other(0x41), true), Some(input)); // Ctrl+A 全选
        tree.dispatch_key(k(Key::Other(0x43), true), Some(input)); // Ctrl+C 复制
        assert_eq!(&*clip.borrow(), "hello", "复制应写入剪贴板");
        tree.dispatch_key(k(Key::End, false), Some(input)); // 光标到末尾、清选区
        tree.dispatch_key(k(Key::Other(0x56), true), Some(input)); // Ctrl+V 粘贴
        assert_eq!(txt.get(), "hellohello", "粘贴应在光标处插入剪贴板文本");
    }

    #[test]
    fn password_input_blocks_copy_allows_paste() {
        let clip = Rc::new(RefCell::new(String::from("seed")));
        let txt = signal(String::from("secret"));
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::text_input(txt, "ph").password());
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 40), &mut te);
        let input = tree.get(id).unwrap().children[0];
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        let k = |key, ctrl| KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl,
        };
        tree.dispatch_key(k(Key::Other(0x41), true), Some(input)); // Ctrl+A 全选
        tree.dispatch_key(k(Key::Other(0x43), true), Some(input)); // Ctrl+C
        assert_eq!(&*clip.borrow(), "seed", "密码模式 Ctrl+C 不得写出明文");
        // 但粘贴仍可用：全选状态下粘贴替换内容。
        tree.dispatch_key(k(Key::Other(0x56), true), Some(input)); // Ctrl+V
        assert_eq!(txt.get(), "seed", "密码模式仍允许粘贴");
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
        let key = KeyEvent {
            key: Key::Char('Z'),
            pressed: true,
            shift: false,
            ctrl: false,
        };
        tree.dispatch_key(key, Some(input));
        assert_eq!(txt.get(), "Z", "三击全选后输入应替换全部");
    }

    fn multiline_tree(initial: &str) -> (Tree, NodeId, Signal<String>) {
        let txt = signal(String::from(initial));
        let root = Element::col()
            .width(200)
            .height(120)
            .child(Element::text_input(txt, "ph").multiline().height(120));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 120), &mut te);
        let input = tree.get(id).unwrap().children[0];
        (tree, input, txt)
    }

    #[test]
    fn multiline_enter_inserts_newline() {
        let (mut tree, input, txt) = multiline_tree("ab");
        // 光标在末尾(=2)，Enter 插入换行，再输入。
        let k = |key| KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl: false,
        };
        tree.dispatch_key(k(Key::Enter), Some(input));
        tree.dispatch_key(k(Key::Char('c')), Some(input));
        assert_eq!(txt.get(), "ab\nc", "多行 Enter 应插入换行符");
    }

    #[test]
    fn singleline_enter_not_consumed() {
        let (mut tree, input, txt) = input_tree("ab");
        let res = tree.dispatch_key(
            KeyEvent {
                key: Key::Enter,
                pressed: true,
                shift: false,
                ctrl: false,
            },
            Some(input),
        );
        assert!(!res.consumed, "单行 Enter 不应被消费(冒泡给默认行为)");
        assert_eq!(txt.get(), "ab", "单行 Enter 不改文本");
    }

    #[test]
    fn multiline_paste_preserves_newlines() {
        let clip = Rc::new(RefCell::new(String::from("x\r\ny")));
        let (mut tree, input, txt) = multiline_tree("");
        tree.clipboard = Some(Box::new(SharedClip(clip)));
        tree.dispatch_key(
            KeyEvent {
                key: Key::Other(0x56),
                pressed: true,
                shift: false,
                ctrl: true,
            },
            Some(input),
        );
        assert_eq!(txt.get(), "x\ny", "多行粘贴应保留换行(\\r\\n 归一为 \\n)");
    }

    #[test]
    fn password_multiline_order_still_single_line() {
        // .password().multiline() 顺序也不能让换行进入密码底层文本。
        let txt = signal(String::from("pw"));
        let root = Element::col()
            .width(200)
            .height(40)
            .child(Element::text_input(txt, "ph").password().multiline());
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 40), &mut te);
        let input = tree.get(id).unwrap().children[0];
        let res = tree.dispatch_key(
            KeyEvent {
                key: Key::Enter,
                pressed: true,
                shift: false,
                ctrl: false,
            },
            Some(input),
        );
        assert!(!res.consumed, "密码框 Enter 不应被消费");
        assert_eq!(txt.get(), "pw", "密码框 Enter 不得插入换行");
    }

    #[test]
    fn caret_of_tracks_cursor_after_paint() {
        let (mut tree, input, _txt) = input_tree("hello");
        let mut pm = tiny_skia::Pixmap::new(200, 40).unwrap();
        let mut eng = crate::text::NullTextEngine;
        // 末尾光标：paint 记录位置。
        {
            let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            tree.paint(&mut canvas);
        }
        let end_caret = tree.caret_of(input).expect("paint 后应有光标位置");
        // 移到行首再 paint。
        tree.dispatch_key(
            KeyEvent {
                key: Key::Home,
                pressed: true,
                shift: false,
                ctrl: false,
            },
            Some(input),
        );
        {
            let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            tree.paint(&mut canvas);
        }
        let home_caret = tree.caret_of(input).unwrap();
        assert!(home_caret.0.x < end_caret.0.x, "行首光标应在末尾光标左侧");
        assert!(home_caret.1 > 0, "光标高度应为正");
    }

    #[test]
    fn caret_of_none_for_non_text() {
        // 按钮等非文本控件无光标。
        let (tree, btn) = button_tree(signal(0));
        assert!(
            tree.caret_of(btn).is_none(),
            "非文本控件 caret_of 应为 None"
        );
    }

    fn paint_once(tree: &Tree) {
        let mut pm = tiny_skia::Pixmap::new(200, 60).unwrap();
        let mut eng = crate::text::NullTextEngine;
        let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
        tree.paint(&mut canvas);
    }

    #[test]
    fn list_click_selects_row() {
        let sel = signal(0usize);
        let root = Element::col().width(200).height(200).child(
            Element::list(vec!["A", "B", "C"], sel)
                .width_match()
                .height(200),
        );
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 200), &mut te);
        // list 是 children[0]=滚动容器，其子为各行。
        let scroll = tree.get(id).unwrap().children[0];
        let rows = tree.get(scroll).unwrap().children.clone();
        assert_eq!(rows.len(), 3, "三行");
        let b = tree.abs_bounds(rows[1]);
        let c = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, c), &mut h, &mut cap);
        tree.dispatch_pointer(ptr(PointerKind::Up, c), &mut h, &mut cap);
        assert_eq!(sel.get(), 1, "点击第二行应选中索引 1");
    }

    #[test]
    fn stepper_buttons_adjust_and_clamp() {
        let v = signal(2.0f64);
        let root = Element::col()
            .width(120)
            .height(40)
            .child(Element::stepper(v, 0.0, 3.0, 1.0).width(120));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(120, 40), &mut te);
        let st = tree.get(id).unwrap().children[0];
        let b = tree.abs_bounds(st);
        let cy = b.y + b.h / 2;
        let plus = Point::new(b.right() - 5, cy);
        let minus = Point::new(b.x + 5, cy);
        let (mut h, mut cap) = (None, None);
        // + → 3（达上限）
        tree.dispatch_pointer(ptr(PointerKind::Down, plus), &mut h, &mut cap);
        assert_eq!(v.get(), 3.0);
        // 再 + 钳制在 3
        tree.dispatch_pointer(ptr(PointerKind::Down, plus), &mut h, &mut cap);
        assert_eq!(v.get(), 3.0, "上限钳制");
        // − 三次到 0 并钳制
        for _ in 0..4 {
            tree.dispatch_pointer(ptr(PointerKind::Down, minus), &mut h, &mut cap);
        }
        assert_eq!(v.get(), 0.0, "下限钳制");
    }

    #[test]
    fn stepper_degenerate_inputs_no_panic() {
        // min>max 且 step=0：构造期归一(step→1, min/max 互换)，点击不得 panic。
        let v = signal(5.0f64);
        let root = Element::col()
            .width(120)
            .height(40)
            .child(Element::stepper(v, 10.0, 0.0, 0.0).width(120));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(120, 40), &mut te);
        let st = tree.get(id).unwrap().children[0];
        let b = tree.abs_bounds(st);
        let plus = Point::new(b.right() - 5, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, plus), &mut h, &mut cap);
        assert_eq!(v.get(), 6.0, "归一后 step=1，5→6");
    }

    #[test]
    fn indeterminate_progress_requests_animation() {
        crate::anim::reset_request();
        let root = Element::col()
            .width(200)
            .height(20)
            .child(Element::progress_indeterminate().width_match());
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 20), &mut te);
        paint_once(&tree);
        assert!(crate::anim::animation_requested(), "不确定进度应请求动画");
    }

    #[test]
    fn determinate_progress_no_animation() {
        crate::anim::reset_request();
        let v = signal(0.5f32);
        let root = Element::col()
            .width(200)
            .height(20)
            .child(Element::progress(v).width_match());
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 20), &mut te);
        paint_once(&tree);
        assert!(!crate::anim::animation_requested(), "确定进度不应请求动画");
    }

    #[test]
    fn dropdown_click_opens_menu_and_selects() {
        let sel = signal(0usize);
        let root = Element::col()
            .width(220)
            .height(40)
            .child(Element::dropdown(vec!["A", "B", "C"], sel).width(220));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(220, 40), &mut te);
        let dd = tree.get(id).unwrap().children[0];
        let b = tree.abs_bounds(dd);
        let pos = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        // 单击（Down+Up）展开：Up 产出菜单请求。
        tree.dispatch_pointer(ptr(PointerKind::Down, pos), &mut h, &mut cap);
        let res = tree.dispatch_pointer(ptr(PointerKind::Up, pos), &mut h, &mut cap);
        let menu = res.menu.expect("下拉单击应弹出菜单");
        assert_eq!(menu.items.len(), 3, "三个选项");
        assert!(menu.items[0].checked, "当前项 A 应勾选");
        assert!(!menu.items[1].checked);
        // 运行第三项动作 → 选中索引变 2。
        if let crate::event::MenuAction::Run(f) = &menu.items[2].action {
            f();
        } else {
            panic!("下拉项应为 Run 动作");
        }
        assert_eq!(sel.get(), 2, "运行选项动作应设置选中索引");
    }

    #[test]
    fn right_click_requests_context_menu() {
        let (mut tree, input, _txt) = input_tree("hello");
        let b = tree.abs_bounds(input);
        let center = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        let down = PointerEvent {
            kind: PointerKind::Down,
            pos: center,
            button: MouseButton::Right,
            click_count: 1,
        };
        let res = tree.dispatch_pointer(down, &mut h, &mut cap);
        let menu = res.menu.expect("右键应请求上下文菜单");
        let labels: Vec<_> = menu
            .items
            .iter()
            .map(|i| (i.label.as_str(), i.enabled))
            .collect();
        // 无选区：剪切/复制禁用；有文本：全选启用；粘贴恒启用。
        assert_eq!(
            labels,
            vec![
                ("剪切", false),
                ("复制", false),
                ("粘贴", true),
                ("全选", true)
            ]
        );
    }

    #[test]
    fn on_context_menu_opens_cascading_menu_on_right_click() {
        use crate::event::MenuItem;
        use crate::ui::Element;
        let tree_el = Element::col().fill().on_context_menu(|| {
            vec![
                MenuItem::run("剪切", || {}, false).with_icon("✂"),
                MenuItem::separator(),
                MenuItem::submenu("更多", vec![MenuItem::run("子项", || {}, false)]).with_icon("⋯"),
            ]
        });
        let mut tree = layout(tree_el, 200, 200);
        let (mut h, mut cap) = (None, None);
        let down = PointerEvent {
            kind: PointerKind::Down,
            pos: Point::new(100, 100),
            button: MouseButton::Right,
            click_count: 1,
        };
        let res = tree.dispatch_pointer(down, &mut h, &mut cap);
        let menu = res.menu.expect("右键容器应请求上下文菜单");
        assert_eq!(menu.pos, Point::new(100, 100));
        assert_eq!(menu.items.len(), 3);
        assert_eq!(menu.items[0].icon.as_deref(), Some("✂"));
        assert!(menu.items[1].separator);
        assert_eq!(menu.items[2].submenu.len(), 1, "子菜单项应携带级联项");
        assert!(!menu.items[2].is_actionable(), "子菜单父项不可直接执行");
    }

    #[test]
    fn right_click_menu_enables_cut_copy_with_selection() {
        let (mut tree, input, _txt) = input_tree("hello");
        let k = |key, ctrl| KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl,
        };
        tree.dispatch_key(k(Key::Other(0x41), true), Some(input)); // 全选
        let b = tree.abs_bounds(input);
        // 在选区内右键（idx=0 落在 [0,5) 内）→ 保留选区。
        let pos = Point::new(b.x + 5, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        let down = PointerEvent {
            kind: PointerKind::Down,
            pos,
            button: MouseButton::Right,
            click_count: 1,
        };
        let res = tree.dispatch_pointer(down, &mut h, &mut cap);
        let menu = res.menu.expect("右键应请求上下文菜单");
        assert!(
            menu.items[0].enabled && menu.items[1].enabled,
            "有选区时剪切/复制应启用"
        );
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
        let key = KeyEvent {
            key: Key::Char('Z'),
            pressed: true,
            shift: false,
            ctrl: false,
        };
        tree.dispatch_key(key, Some(input));
        assert_eq!(txt.get(), "Z world", "双击应选中首词并被输入替换");
    }
}
