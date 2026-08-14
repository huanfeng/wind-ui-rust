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
use crate::platform::{DialogRequest, PickDialog};
use crate::render::{Canvas, Paint};
use crate::spec::{Align, Axis, Dimension, MeasureMode, MeasureSpec};
use crate::style::Style;
use crate::text::TextEngine;

/// 点击/激活回调类型。
pub type ClickFn = Box<dyn FnMut(&mut EventCtx)>;

/// 文件拖放回调类型：收到落在本节点（或其子节点冒泡上来）的文件路径列表。
pub type DropFn = Box<dyn FnMut(&mut EventCtx, &[PathBuf])>;
/// 右键上下文菜单构建回调：返回该次菜单项（空 = 不弹）。
///
/// 用 `Rc<dyn Fn>` 而非 `Box<dyn FnMut>`：菜单弹出后还要留一份给宿主当
/// [`MenuRequest::rebuild`](crate::event::MenuRequest::rebuild)——粘滞项
/// （`MenuItem::stay_open`，即右键菜单里的复选项）点击后菜单不关，得靠它重跑构建器
/// 才能把勾选态刷新过来。`FnMut` 独占，交不出第二份。
pub type MenuFn = std::rc::Rc<dyn Fn() -> Vec<crate::event::MenuItem>>;

/// 失效矩形的抗锯齿外扩余量（逻辑像素）。与宿主局部重绘的余量同源。
const DAMAGE_MARGIN: i32 = 2;

/// 纵向滚动条几何（逻辑像素）。**唯一真相源**：`core` 的滚动容器绘制/命中、
/// `ui::containers::VScrollbar`（多行输入框等自绘宿主）都从这里取值，避免两处漂移。
///
/// 此前 core 用 `track_w=6 / margin=2 / hit=10`、`VScrollbar` 用 `5 / 3 / 12`，
/// 而 `VScrollbar` 的注释却声称"与 core paint 一致"——注释断言的一致性没有编译期约束，
/// 抽成共享常量后才真正成立。
pub mod scrollbar {
    /// 轨道与滑块的视觉宽度。
    pub const TRACK_W: f32 = 7.0;
    /// 轨道距容器右缘（已计入 `WINDOW_EDGE_INSET`）的边距。
    pub const MARGIN: f32 = 3.0;
    /// 滑块最小高度：内容极长时不至于缩成一个点而抓不住。
    pub const MIN_THUMB: f32 = 24.0;
    /// 命中区宽度：比视觉宽度宽一倍有余，容忍手抖。
    pub const HIT_W: i32 = 16;

    /// 贴窗口右缘时滚动条整体额外内缩的距离。
    ///
    /// 无边框窗口在 `WM_NCHITTEST`（`platform::win32::handle_nchittest`）把客户区右缘
    /// 8 逻辑 px 判为 `HTRIGHT` 缩放边框——落在那里的指针事件根本进不到客户区。滚动条
    /// 原先画在 `[right-8, right-2]`，正好整条被压在缩放边框底下，看得见点不着。
    ///
    /// 取值恰为边框宽度本身，两个区间**边界相接而不重叠**：滚动条命中区止于
    /// `right-8`，缩放边框始于 `right-8`。这是能让两者共存的最小内缩——再小一像素
    /// 就会重新被边框吞掉，故不可低于 `win32::RESIZE_BORDER_LOGICAL`。
    ///
    /// 物理侧不会反超：边框物理宽 `(8 * dpi/96) as i32` 是**向下**取整，换算回逻辑坐标
    /// 恒 ≤ 8，故任意 DPI（含非整数缩放）下这条边界都成立。
    pub const WINDOW_EDGE_INSET: i32 = 8;

    /// 滚动条在容器内实际占用的水平宽度（含内缩）。arrange 据此为内容让位。
    pub fn occupied_w(edge_inset: i32) -> i32 {
        (TRACK_W + MARGIN) as i32 + edge_inset
    }

    /// 滑块高度。绘制与拖动换算必须同源——否则拖起来会"跟不上鼠标"。
    pub fn thumb_h(view_h: i32, content_h: i32) -> f32 {
        if content_h <= 0 {
            return MIN_THUMB;
        }
        let ratio = (view_h as f32 / content_h as f32).min(1.0);
        (view_h as f32 * ratio).max(MIN_THUMB)
    }

    /// 拖动 1px 鼠标对应的 `scroll_y` 增量所依据的滑块行程（视口高减滑块高）。
    pub fn travel(view_h: i32, content_h: i32) -> f32 {
        (view_h as f32 - thumb_h(view_h, content_h)).max(1.0)
    }

    /// 轨道底衬色。`None` = 不画。
    ///
    /// 默认不画是有意的：滚动条是内容的轻量指示，常态下只露一截滑块即可。画满全高的
    /// 底衬要么淡到没有意义，要么与滑块明度接近、整条糊成"全高一根"反而看不出当前
    /// 位置在哪——底衬与滑块本就争同一段视觉预算，取舍下来滑块更重要。
    pub fn track() -> Option<crate::geometry::Color> {
        None
    }

    /// 滑块色。取自当前主题，**不用**固定的黑色半透明——后者在深色主题下会连滑块
    /// 一起隐没（深底叠黑等于没画）。
    ///
    /// `active` 为拖动态，加深一档给出"抓住了"的反馈。
    pub fn thumb(active: bool) -> crate::geometry::Color {
        let p = &crate::theme::current().palette;
        if active {
            p.text_muted
        } else {
            p.border
        }
    }
}

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
    /// 本节点是否为**模态层根**（仅对话框遮罩）：可见时把 Tab 焦点环圈在其子树内，
    /// 使键盘无法走到被遮罩盖住、鼠标点都点不到的控件上（见 [`Tree::focusable_order`]）。
    ///
    /// 与 [`Widget::scrim_passthrough`] 是两件事，勿合并：那个说的是窗口拖动区判定
    /// 要不要穿透，这个说的是键盘焦点归谁管。
    fn is_modal(&self) -> bool {
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
    /// 输入法组合态变化（拼音等未上屏文字开始/结束合成）时由框架通知焦点节点。
    /// 文本控件借此在组合期间暂不绘制自绘光标（系统组合浮层自带光标）。默认无操作。
    fn set_composing(&mut self, _composing: bool) {}
    /// layout 前由框架向**已注册的响应式节点**调用（见 `Tree::register_reactive`）。
    /// 响应式控件在此检测绑定信号的版本变化，若有变化则通过 `ctx.tree_mut()` 重建子节点。
    /// 默认无操作；普通控件无需实现。
    fn on_update(&mut self, _ctx: &mut EventCtx) {}
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
    /// 命中是否在本节点「落定」：true（默认，所有真实控件）= 命中即停、吞掉事件；
    /// false（仅 `EmptyWidget` 纯容器）= 子节点都未命中时穿透，让父节点继续测下层兄弟。
    /// 防止透明纯布局容器（尤其根级全窗覆盖层）遮挡其下兄弟的指针事件。
    /// 节点级的背景/滚动/拖窗/拖放等仍由命中逻辑单独判为「吞命中」（见 `hit_node`）。
    fn hit_opaque(&self) -> bool {
        true
    }
    /// 本节点在**窗口拖动区判定**（`Tree::drag_hit_at`）中是否透明。true（仅模态遮罩）
    /// = 拖动判定时穿透到其下层兄弟，使无边框窗口的自绘标题栏在对话框弹出后仍可拖窗。
    /// 只影响 `WM_NCHITTEST` 侧的 HTCAPTION 判定，**不影响事件分发与交互控件判定**——
    /// 遮罩照常吞掉指针事件、照常屏蔽标题栏上的窗口按钮，模态语义不变。
    /// 覆写为 true 的容器必须自带背景（对话框面板都设了 `Role::Surface`），否则面板
    /// 空白区会一并穿透、被误判成拖动区。
    fn scrim_passthrough(&self) -> bool {
        false
    }
    /// 单行省略配置下，最近一次绘制的文本是否被实际截断。`None`=本控件不具备
    /// 该概念（如按钮/容器），`Some(false)`=配了省略但当前完整放得下、未截断。
    /// 供 [`Tree::node_tooltip`] 判定：仅在文本确被截断时才弹出与其重复的悬浮提示。
    fn text_truncated(&self) -> Option<bool> {
        None
    }
    /// 控件自报的悬停提示，**优先于**节点上 `.tooltip(..)` 设的静态文本。
    ///
    /// 给自绘控件用：图表类控件整个是一个节点，提示内容取决于指针落在哪个数据点上
    /// （日历热力图的哪一格、柱状图的哪一根），静态文本表达不了。控件在
    /// [`Widget::on_event`] 里记下当前命中项，这里据此返回对应文案；未命中返回
    /// `None`，宿主即回退到节点静态文本（没有则不弹）。
    ///
    /// 每帧在悬停节点上调用，实现应只读已有状态、不做重计算。
    fn tooltip(&self) -> Option<String> {
        None
    }
}

/// 容器/纯样式节点占位控件。
pub struct EmptyWidget;
impl Widget for EmptyWidget {
    fn hit_opaque(&self) -> bool {
        false
    }
}

impl Node {
    /// 该帧是否有效可见：静态标志、可见信号、可见条件闭包三者取与
    /// （对应 `Element::visible` / `visible_signal` / `visible_when`）。
    pub fn effective_visible(&self) -> bool {
        self.visible
            && self.vis_signal.as_ref().is_none_or(|s| s.get())
            && self.vis_cond.as_ref().map(|f| f()).unwrap_or(true)
    }
    /// 本节点自身启用态（不含父链继承）：静态标志、启用信号、启用条件闭包三者取与
    /// （对应 `Element::enabled` / `enabled_signal` / `enabled_when`）。与
    /// [`effective_visible`](Self::effective_visible) 三形态一一对应。
    pub fn own_enabled(&self) -> bool {
        self.enabled_static
            && self.enabled.as_ref().is_none_or(|c| c.get())
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
    /// 最小宽度（0=无约束）：measure 收敛后对宽度取下界。配合 `Dimension::Wrap`
    /// 宽实现「按内容自适应、但不小于此值」——短内容对齐统一基线宽，长内容自动
    /// 加宽不换行。与固定宽 `Dimension::Px` 互斥（后者已钉死宽度，下界不参与）。
    pub min_width: i32,
    /// 最大宽度（0=无约束）：measure **前**收窄可用宽、measure 后对宽度取上界。
    ///
    /// 必须在测量前生效，否则文字按更宽的可用宽排好版后才被裁掉——那是截断，不是限宽。
    /// 限宽的本意是让内容**在更窄的宽度内换行**，长正文的可读性正由此而来。
    pub max_width: i32,
    /// 最大高度（0=无约束）：measure **后**对高度取上界。
    ///
    /// 与 `max_width` 不对称是刻意的：限宽必须在测量前收窄可用宽，否则文字会按更宽的
    /// 宽度排完版才被裁（那是截断不是换行）；而高度方向没有"按高度重排"的语义，内容
    /// 本就该按完整高度测量——滚动容器尤其依赖这一点，其 `content_h`（可滚动量的来源）
    /// 正是完整内容高。故上界只收窄节点自身的占位，不影响内容测量。
    pub max_height: i32,
    pub padding: Insets,
    pub margin: Insets,
    /// 自身对齐覆盖：None=继承容器交叉轴对齐；Some(a)=显式覆盖。
    pub align: Option<Align>,
    pub layout: Layout,
    pub widget: Box<dyn Widget>,
    pub style: Style,
    pub visible: bool,
    /// 运行期可见信号（None=无约束）。与 `visible`/`vis_cond` 取与。
    pub vis_signal: Option<Signal<bool>>,
    /// 运行期可见条件（如 Tab 页绑定选中项、Dialog 绑定显示标志）。
    /// 与 `visible` 取与：返回 false 则该帧不参与测量/布局/绘制/命中。
    pub vis_cond: Option<Box<dyn Fn() -> bool>>,
    /// 静态启用标志（`Element::enabled(bool)` / `disabled(bool)`）。是 `visible`
    /// 在启用轴上的对应物——常量禁用不必为此占用一个信号槽。
    pub enabled_static: bool,
    /// 自身启用信号（None=无约束）。禁用沿父链继承：核心据有效启用态拦事件、
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
    /// Tab 焦点环参与度覆盖：`None` 按 `Widget::focusable()`；`Some(false)` 强制
    /// 退出焦点环（如词典正文——主焦点应常驻输入框）；`Some(true)` 强制加入。
    /// **仅影响 Tab 遍历**：不改变命中测试、点击交互与 `request_focus` 语义。
    pub focusable: Option<bool>,
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
    /// **绘制/命中偏移**（逻辑 px，相对布局位置）：不参与 measure/arrange，只在绘制与
    /// 命中时叠加到绝对坐标上。用于"视觉位移但布局不变"的场景——拖拽重排的让位与浮起、
    /// 列表增删的 FLIP 动画等。
    ///
    /// 与直接改 `bounds` 的本质区别：`bounds` 是布局结果，任何一次 relayout 都会重算它，
    /// 临时视觉状态写进去必被冲掉；`offset` 独立于布局，relayout 不影响。
    ///
    /// 变化会进入 [`Tree::layout_signature`]，故宿主自动判为结构变化并升级整窗重绘。
    ///
    /// 已知限制：`arrange` 侧的绝对原点（`arrange_origin`）**不含** offset，故带水平
    /// offset 的滚动容器贴近窗口右缘时，预留的滚动条宽度会与实际绘制位置差一个内缩量。
    /// 这是刻意取舍，理由见 `arrange_origin` 的文档。
    pub offset: Point,
    /// **同级绘制顺序提升**：为 true 的子节点在其余兄弟之后绘制、命中时优先测试。
    /// 拖拽浮起的行用，否则会被排在它后面的兄弟行盖住。
    pub raised: bool,
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
    /// 响应式节点列表：每次 `layout_root` 前广播 `on_update`，允许控件重建子节点。
    reactive_nodes: Vec<NodeId>,
    /// on_update（响应式相位）里控件请求的 toast 暂存区。该相位在 `call_on_update` 后
    /// 丢弃整个 `EventOutcome`，其中的 toast 无处上交宿主；单独在此累积，由宿主在
    /// layout 后 `take_pending_toasts` 取走上屏（否则 `toast_sink` 等经信号触发的提示全被吞）。
    pending_toasts: Vec<ToastRequest>,
    /// arrange 递归中当前节点父级的绝对左上角。
    ///
    /// `arrange` 全程使用相对父的坐标，但滚动条要判断"本容器是否贴着窗口右缘"必须知道
    /// 绝对位置。arrange 是严格嵌套的深度优先遍历，故用一个成员变量当栈顶（进入时累加、
    /// 退出时还原）即可，无需给 `arrange_*` 全家加参数。
    ///
    /// **刻意不累加 [`Node::offset`]**，与 paint/hit 两侧的口径不同。offset 是绘制期的
    /// 临时位移，改它只需重绘、不必重排；一旦 arrange 依赖它，就等于引入"改 offset 必须
    /// relayout"的隐含契约，漏一次就会让布局与视觉悄悄错位。代价是：带**水平** offset 的
    /// 滚动容器若贴近窗口右缘，这里预留的滚动条宽度会与实际绘制位置差一个内缩量。
    /// 当前无调用方使用水平 offset（拖拽重排只写 y），如需支持应改内缩判定本身，
    /// 而不是让 arrange 去读 offset。
    arrange_origin: Point,
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
            reactive_nodes: Vec::new(),
            pending_toasts: Vec::new(),
            arrange_origin: Point::new(0, 0),
        }
    }

    /// 取走 on_update 相位累积的 toast 请求（宿主在 layout 后调用上屏），并清空暂存。
    pub fn take_pending_toasts(&mut self) -> Vec<ToastRequest> {
        std::mem::take(&mut self.pending_toasts)
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

    /// 将节点注册为响应式：每次 `layout_root` 前收到 `Widget::on_update` 回调。
    /// 由 `Element::build` 在 `Element::reactive(true)` 时自动调用。
    pub fn register_reactive(&mut self, id: NodeId) {
        if !self.reactive_nodes.contains(&id) {
            self.reactive_nodes.push(id);
        }
    }

    /// 调用单个响应式节点的 `on_update`（与 call_on_event 同款 widget swap 模式）。
    fn call_on_update(&mut self, id: NodeId) {
        if !self.node_enabled(id) {
            return;
        }
        let mut widget = match self.get_mut(id) {
            Some(n) => std::mem::replace(&mut n.widget, Box::new(EmptyWidget)),
            None => return,
        };
        let mut ctx = EventCtx {
            tree: self,
            self_id: id,
            out: EventOutcome::default(),
        };
        widget.on_update(&mut ctx);
        // EventOutcome 大多可弃：update 后紧接着全量 layout，damage 等信息无意义。
        // 唯 toast 需上交宿主——on_update 相位不经 DispatchResult，若一并丢弃则 toast_sink
        // 等在此发的提示永不上屏，故先取出暂存（见 pending_toasts / take_pending_toasts）。
        let requested_toast = ctx.out.toast.take();
        if let Some(n) = self.get_mut(id) {
            n.widget = widget;
        }
        if let Some(req) = requested_toast {
            self.pending_toasts.push(req);
        }
    }

    /// 在 layout 前向所有响应式节点广播 on_update；同时剔除已被删除的节点。
    ///
    /// on_update 中动态重建的子树可能注册**新的**响应式节点（`register_reactive` 追加到
    /// 列表尾，如响应式重建宿主里挂的响应式表头/正文）——按批次迭代到收敛，令新节点在
    /// **同一帧**收到回调（否则首帧空白）。清理阶段基于真实列表 retain（而非广播快照的
    /// 存活集覆盖——那会把广播期间新注册的节点抹掉，使其永远收不到回调）。
    fn dispatch_reactive_updates(&mut self) {
        let mut start = 0;
        // 轮数上限防病态相互触发；正常场景一两轮即收敛。
        for _ in 0..16 {
            let end = self.reactive_nodes.len();
            if start >= end {
                break;
            }
            let batch: Vec<NodeId> = self.reactive_nodes[start..end].to_vec();
            start = end;
            for id in batch {
                if self.get(id).is_some() {
                    self.call_on_update(id);
                }
            }
        }
        let mut live = std::mem::take(&mut self.reactive_nodes);
        live.retain(|&id| self.get(id).is_some());
        self.reactive_nodes = live;
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
        // 先让响应式节点重建子树结构，再 measure/arrange
        self.dispatch_reactive_updates();
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
        let (layout, padding, min_width, max_width, max_height, visible) = match self.get(id) {
            Some(n) => (
                n.layout,
                n.padding,
                n.min_width,
                n.max_width,
                n.max_height,
                n.effective_visible(),
            ),
            None => return Size::ZERO,
        };
        if !visible {
            if let Some(n) = self.get_mut(id) {
                n.measured = Size::ZERO;
            }
            return Size::ZERO;
        }

        let mut avail_w = (wspec.avail() - padding.horizontal()).max(0);
        // 限宽在测量**前**收窄可用宽：子节点与文字据此换行，而非排完再裁。
        if max_width > 0 {
            avail_w = avail_w.min((max_width - padding.horizontal()).max(0));
        }
        let avail_h = (hspec.avail() - padding.vertical()).max(0);

        let content = match layout {
            Layout::None => {
                // 叶子：纯内容固有尺寸（可能需要测量文本）。字重与行高随 `Style` 一并
                // 交给控件，由控件构造 `TextStyle` 传给引擎——测量与绘制因此天然同源，
                // 不再依赖「调用前注入、调用后复位」这种容易漏掉一半的约定。
                let n = self.get(id).unwrap();
                n.widget
                    .measure(Size::new(avail_w, avail_h), &n.style, text)
            }
            Layout::Linear { axis, spacing, .. } => {
                self.measure_linear(id, axis, spacing, wspec, hspec, avail_w, avail_h, text)
            }
            Layout::Frame => self.measure_frame(id, wspec, hspec, avail_w, avail_h, text),
            Layout::Scroll => self.measure_scroll(id, avail_w, text),
        };

        let desired_w = content.w + padding.horizontal();
        let desired_h = content.h + padding.vertical();
        // min_width：约束收敛后对宽度取下界（0=无）。放在 resolve 之后，使
        // 「Wrap 自适应宽 < 下界」时抬到下界，而自适应宽更大时保留（避免长文本换行）。
        let mut resolved_w = wspec.resolve(desired_w).max(min_width);
        // 上界后于下界施加：两者同时设定且冲突时以上界为准，宽度才不会超出调用方
        // 明确给出的上限（下界的本意是「至少这么宽」，让位于硬上限是合理的）。
        if max_width > 0 {
            resolved_w = resolved_w.min(max_width);
        }
        // 限高只收窄本节点占位，内容已按完整高度测完（滚动容器的 content_h 因此不受影响，
        // 溢出部分转为可滚动量而非被丢弃）。
        let mut resolved_h = hspec.resolve(desired_h);
        if max_height > 0 {
            resolved_h = resolved_h.min(max_height);
        }
        let size = Size::new(resolved_w, resolved_h);
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
        // 进入子树前把本节点的绝对左上角推为新原点，退出时还原（见 `arrange_origin`）。
        let saved_origin = self.arrange_origin;
        self.arrange_origin = Point::new(saved_origin.x + bounds.x, saved_origin.y + bounds.y);
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
        self.arrange_origin = saved_origin;
    }

    /// 滚动条为避开窗口缩放边框需额外内缩的距离（见 `scrollbar::WINDOW_EDGE_INSET`）。
    ///
    /// `abs_right` 为滚动容器的绝对右边界。只有真正贴着窗口右缘的容器才内缩——对话框、
    /// 表单里那些远离窗口边的滚动区保持原有紧凑外观，不平白多出一段空白。
    /// 点 `p` 是否落在滚动条可抓取区（`abs` 为滚动容器绝对矩形）。
    ///
    /// 命中区比视觉宽度宽出一倍有余，且**有上界**：内缩出来的那 10px 归还给窗口缩放边框，
    /// 不被滚动条抢走——两种操作各占一段、互不干扰。控件侧（`ScrollWidget`）经
    /// `EventCtx::scrollbar_hit_zone` 取同一区间，判定不会与命中分发漂移。
    pub fn in_scrollbar_hit_zone(&self, p: Point, abs: Rect) -> bool {
        let (lo, hi) = self.scrollbar_hit_zone(abs);
        p.x >= lo && p.x < hi
    }

    /// 滚动条可抓取区的 x 区间 `[lo, hi)`（绝对坐标）。
    pub fn scrollbar_hit_zone(&self, abs: Rect) -> (i32, i32) {
        let hi = abs.right() - self.scrollbar_edge_inset(abs.right());
        (hi - scrollbar::HIT_W, hi)
    }

    fn scrollbar_edge_inset(&self, abs_right: i32) -> i32 {
        let Some(root_w) = self.root.and_then(|r| self.get(r)).map(|n| n.bounds.w) else {
            return 0;
        };
        if abs_right >= root_w - scrollbar::WINDOW_EDGE_INSET {
            scrollbar::WINDOW_EDGE_INSET
        } else {
            0
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
        // 可滚动时为右侧滚动条预留宽度，避免内容被遮挡。贴窗口右缘的容器滚动条会内缩，
        // 预留宽度须同步加上内缩量，否则滚动条会盖到内容上。
        let scrollbar_w = if content_h > inner.h {
            let abs_right = self.arrange_origin.x + inner.x + inner.w;
            scrollbar::occupied_w(self.scrollbar_edge_inset(abs_right))
        } else {
            0
        };
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
        // 绘制偏移叠加在布局位置之上（见 `Node::offset`）。子节点以 abs 为原点递归，
        // 故父节点的位移自动带着整棵子树走。
        let abs = Rect::new(
            origin.x + n.bounds.x + n.offset.x,
            origin.y + n.bounds.y + n.offset.y,
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
                let e = n.style.border_edges;
                if e.is_all() {
                    // 四边齐全走圆角描边，保住 corner_radius。
                    canvas.stroke_round_rect(fx, fy, fw, fh, radius, *bw as f32, &bp);
                } else {
                    // 缺边时逐边画矩形段：圆角在此无意义——一条底边不存在「圆角」，
                    // 硬套圆角描边会在缺口处留下两截弧线。
                    let w = *bw as f32;
                    if e.top {
                        canvas.fill_round_rect(fx, fy, fw, w, 0.0, &bp);
                    }
                    if e.bottom {
                        canvas.fill_round_rect(fx, fy + fh - w, fw, w, 0.0, &bp);
                    }
                    if e.left {
                        canvas.fill_round_rect(fx, fy, w, fh, 0.0, &bp);
                    }
                    if e.right {
                        canvas.fill_round_rect(fx + fw - w, fy, w, fh, 0.0, &bp);
                    }
                }
            }
        }

        let content = abs.inset(n.padding);
        // 标记当前节点矩形：节点内的 anim::request_repaint 会把脏区归到此处（局部重绘用）。
        crate::anim::set_paint_rect(Some(abs));
        n.widget
            .paint(abs, content, n.focused, enabled, canvas, &n.style);
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
        // 子节点分两趟绘制：先普通、后 `raised`。拖拽浮起的行须画在其余兄弟之上，
        // 否则会被排在它后面的行盖住。绝大多数容器没有 raised 子节点，第二趟是空转。
        if n.clip_children {
            canvas.save();
            canvas.clip_rect(content);
            self.paint_children(canvas, n, child_origin, enabled);
            canvas.restore();
        } else {
            self.paint_children(canvas, n, child_origin, enabled);
        }

        // 滚动条：内容高于视口时在右缘绘制纵向指示条。贴窗口右缘时整体内缩，避开
        // 被 WM_NCHITTEST 判为缩放边框的那一段（否则画得出来、点不着）。
        if matches!(n.layout, Layout::Scroll) && n.content_h > content.h {
            let track_w = scrollbar::TRACK_W;
            let inset = self.scrollbar_edge_inset(abs.right());
            let tx = abs.right() as f32 - track_w - scrollbar::MARGIN - inset as f32;
            let ty = content.y as f32;
            let th = content.h as f32;
            let thumb_h = scrollbar::thumb_h(content.h, n.content_h);
            let max_scroll = (n.content_h - content.h).max(1) as f32;
            let thumb_y = ty + (th - thumb_h) * (n.scroll_y as f32 / max_scroll);
            let r = track_w / 2.0;
            if let Some(track) = scrollbar::track() {
                canvas.fill_round_rect(tx, ty, track_w, th, r, &Paint::fill(track));
            }
            canvas.fill_round_rect(
                tx,
                thumb_y,
                track_w,
                thumb_h,
                r,
                &Paint::fill(scrollbar::thumb(false)),
            );
        }

        if use_layer {
            canvas.pop_layer();
        }
    }

    /// 绘制子节点：先非 `raised`、后 `raised`，各自保持原有相对顺序（稳定分区）。
    /// 与 [`Tree::hit_node`] 的倒序遍历互为镜像——那边先测 `raised`，两者对"谁在上层"
    /// 的判断必须一致，否则会出现"画在上面却点不到"。
    fn paint_children(&self, canvas: &mut dyn Canvas, n: &Node, origin: Point, enabled: bool) {
        for &c in &n.children {
            if !self.get(c).map(|cn| cn.raised).unwrap_or(false) {
                self.paint_node(canvas, c, origin, enabled);
            }
        }
        for &c in &n.children {
            if self.get(c).map(|cn| cn.raised).unwrap_or(false) {
                self.paint_node(canvas, c, origin, enabled);
            }
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
    /// 用户请求关闭窗口：交由宿主的关闭决策链处理（关顶层对话框 → 问
    /// `on_close_request` → `hide_on_close`）。
    close: bool,
    /// 应用已决定关闭：跳过决策链直接落地。
    close_forced: bool,
    focus: Option<NodeId>,
    /// 控件请求弹出的上下文菜单（宿主接管渲染与命中）。
    menu: Option<MenuRequest>,
    /// 控件请求宿主用系统默认程序打开的 URL/路径（链接点击等）。
    open_url: Option<String>,
    /// 控件请求的窗口操作（最小化/最大化切换，自定义标题栏按钮触发）。
    window_op: Option<WindowOp>,
    /// 控件请求弹出的轻提示（宿主接管居中浮层渲染与定时消失）。
    toast: Option<ToastRequest>,
    /// 控件请求弹出的原生文件对话框（宿主待事件分发完全返回后再执行，见 `DialogRequest`）。
    dialog: Option<DialogRequest>,
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
    /// 当前时刻（ms，单调，与挂钟无关，仅用差值）。宿主在事件分发前刷新，故长按、双击、
    /// 拖动速度一类的时长判定应取它，**不要**在事件里读 `anim::clock_ms()` 的历史语义。
    pub fn now_ms(&self) -> u64 {
        crate::anim::clock_ms()
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
    /// **请求**关闭窗口：交给宿主的关闭决策链——先关最顶层对话框，没有则问
    /// [`App::on_close_request`](crate::app::App::on_close_request)，最后按
    /// `hide_on_close` 决定是关还是隐。
    ///
    /// 自绘标题栏的关闭按钮（`Element::window_button(WindowButtonKind::Close)`）走的正是
    /// 这条路：无边框窗口的 × 与系统 × 在用户眼里是同一个按钮，没有理由一个过守卫、
    /// 另一个不过——`on_close_request` 拦得住 Alt+F4 却拦不住 ×，等于形同虚设。
    ///
    /// 已经确定要关（安装器要求退出、用户在确认框里选了"直接退出"）用
    /// [`force_close`](Self::force_close)。
    ///
    /// 在 `on_close_request` 的回调**内部**调用本方法无效（正在回答"能不能关"，
    /// 再请求一次没有意义），宿主会忽略以免自我递归。
    pub fn request_close(&mut self) {
        self.out.close = true;
    }
    /// **直接**关闭窗口：跳过关闭决策链（不问 `on_close_request`、不先关对话框），
    /// 但仍受 `hide_on_close` 约束。
    ///
    /// 用于"应用已经决定"的场合：安装器要求本进程退出、用户在未保存确认框里已经选过
    /// "直接退出"。这类地方再走一遍守卫，轻则多问一次，重则死锁——安装器等窗口关、
    /// 窗口等用户回答。
    pub fn force_close(&mut self) {
        self.out.close_forced = true;
    }
    /// 请求把焦点移到本节点。
    pub fn request_focus(&mut self) {
        self.out.focus = Some(self.self_id);
    }
    /// 请求打开**单文件**选择对话框；`on_result` 在对话框关闭、事件分发完全返回后
    /// 收到用户选择结果（取消为 `None`）。**不要**在回调里直接调用 `PickDialog::pick_file()`
    /// 等同步方法，见 [`DialogRequest`] 文档。
    pub fn request_pick_file(
        &mut self,
        dialog: PickDialog,
        on_result: impl FnOnce(Option<PathBuf>) + 'static,
    ) {
        self.out.dialog = Some(DialogRequest::PickFile(dialog, Box::new(on_result)));
    }
    /// 请求打开**多文件**选择对话框，语义同 [`EventCtx::request_pick_file`]。
    pub fn request_pick_files(
        &mut self,
        dialog: PickDialog,
        on_result: impl FnOnce(Option<Vec<PathBuf>>) + 'static,
    ) {
        self.out.dialog = Some(DialogRequest::PickFiles(dialog, Box::new(on_result)));
    }
    /// 请求打开**单目录**选择对话框，语义同 [`EventCtx::request_pick_file`]。
    pub fn request_pick_folder(
        &mut self,
        dialog: PickDialog,
        on_result: impl FnOnce(Option<PathBuf>) + 'static,
    ) {
        self.out.dialog = Some(DialogRequest::PickFolder(dialog, Box::new(on_result)));
    }
    /// 请求打开**多目录**选择对话框，语义同 [`EventCtx::request_pick_file`]。
    pub fn request_pick_folders(
        &mut self,
        dialog: PickDialog,
        on_result: impl FnOnce(Option<Vec<PathBuf>>) + 'static,
    ) {
        self.out.dialog = Some(DialogRequest::PickFolders(dialog, Box::new(on_result)));
    }
    /// 请求打开**保存文件**对话框，语义同 [`EventCtx::request_pick_file`]。
    pub fn request_save_file(
        &mut self,
        dialog: PickDialog,
        on_result: impl FnOnce(Option<PathBuf>) + 'static,
    ) {
        self.out.dialog = Some(DialogRequest::SaveFile(dialog, Box::new(on_result)));
    }
    /// 逃生舱：把一段包含**任意数量**阻塞式原生调用（文件对话框、`MessageBoxW` 等）
    /// 的流程延迟到事件分发完全返回之后执行。适用于"选文件→校验→选目录→确认"这类
    /// 需要连续弹多个原生模态框、`request_pick_file` 等单对话框便捷方法表达不了的
    /// 场景。闭包运行时已经不在事件回调栈内、OS 输入状态已同步，可以放心在里面
    /// 直接同步调用 `PickDialog::pick_file()` 等方法或系统 `MessageBox`。
    pub fn defer_blocking(&mut self, f: impl FnOnce() + 'static) {
        self.out.dialog = Some(DialogRequest::Custom(Box::new(f)));
    }
    /// 本节点绝对矩形（判断指针是否仍在控件内）。
    pub fn bounds(&self) -> Rect {
        self.tree.abs_bounds(self.self_id)
    }
    /// 暴露底层树，供响应式控件（`on_update` 内）重建子节点。
    /// 调用方负责维护树结构一致性（不要删除 `ctx.id()` 自身节点）。
    pub fn tree_mut(&mut self) -> &mut Tree {
        self.tree
    }
    /// 设置节点的**绘制/命中偏移**（见 [`Node::offset`]）：视觉位移但布局不变。
    /// 返回值是否真的发生了变化——调用方据此决定要不要打脏，避免每帧无谓失效。
    pub fn set_node_offset(&mut self, id: NodeId, off: Point) -> bool {
        match self.tree.get_mut(id) {
            Some(n) if n.offset != off => {
                n.offset = off;
                true
            }
            _ => false,
        }
    }
    /// 设置节点的**同级绘制顺序提升**（见 [`Node::raised`]）：拖拽浮起的行用。
    pub fn set_node_raised(&mut self, id: NodeId, raised: bool) {
        if let Some(n) = self.tree.get_mut(id) {
            n.raised = raised;
        }
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
    /// 本滚动节点的滚动条可抓取区 x 区间 `[lo, hi)`（绝对坐标）。
    /// 与 `Tree::hit_test` 的分发判定同源，避免"分发到了控件、控件却认为没点中"。
    pub fn scrollbar_hit_zone(&self) -> (i32, i32) {
        self.tree.scrollbar_hit_zone(self.bounds())
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
            rebuild: None,
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
            rebuild: None,
        });
        self.out.repaint = true;
    }
    /// 复选菜单专用：同 [`show_dropdown_menu`](Self::show_dropdown_menu) 的定位与翻转，
    /// 但项由 `rebuild` 生成，且粘滞项（[`MenuItem::stay_open`]）点击后会再次调用它
    /// 原地刷新勾选态——菜单保持展开，可连点多个开关。项为空则不弹。
    pub fn show_check_menu(
        &mut self,
        bounds: crate::geometry::Rect,
        rebuild: std::rc::Rc<dyn Fn() -> Vec<MenuItem>>,
    ) {
        let items = rebuild();
        if items.is_empty() {
            return;
        }
        self.out.menu = Some(MenuRequest {
            pos: Point::new(bounds.x, bounds.y + bounds.h),
            items,
            min_width: bounds.w,
            anchor_top: Some(bounds.y),
            rebuild: Some(rebuild),
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
    /// 请求显示并前置窗口。
    pub fn show_window(&mut self) {
        self.out.window_op = Some(WindowOp::Show);
    }
    /// 请求隐藏窗口（进程继续存活，可经托盘或全局热键唤起）。
    ///
    /// 与 `ctx.request_close()` 的区别是根本性的：隐藏只改变可见性，关闭会销毁窗口
    /// 并结束消息循环。常驻托盘类应用要的是前者。
    pub fn hide_window(&mut self) {
        self.out.window_op = Some(WindowOp::Hide);
    }

    /// 弹出轻提示（中性信息）。居中浮层 + 淡入淡出 + 定时自动消失，由宿主接管。
    /// **脱离布局树**——不绑定任何节点，任意控件回调内 `ctx.toast("…")` 即可。
    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast_with(text, ToastKind::Info, ToastKind::Info.default_duration_ms());
    }
    /// 弹出成功轻提示（✓ 图标），如"已添加到剪贴板"。
    pub fn toast_ok(&mut self, text: impl Into<String>) {
        self.toast_with(
            text,
            ToastKind::Success,
            ToastKind::Success.default_duration_ms(),
        );
    }
    /// 弹出错误轻提示（✕ 图标）。
    pub fn toast_err(&mut self, text: impl Into<String>) {
        self.toast_with(
            text,
            ToastKind::Error,
            ToastKind::Error.default_duration_ms(),
        );
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
#[derive(Default)]
pub struct DispatchResult {
    pub repaint: bool,
    /// 本次分发累积的失效区域（宿主据此选择局部/整窗重绘）。
    pub damage: DamageReq,
    /// 用户请求关闭窗口，须走关闭决策链（见 [`EventCtx::request_close`]）。
    pub close: bool,
    /// 应用已决定关闭，跳过决策链（见 [`EventCtx::force_close`]）。
    pub close_forced: bool,
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
    /// 控件请求弹出的原生文件对话框（宿主待事件分发完全返回后再执行）。
    pub dialog: Option<DialogRequest>,
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

    /// 节点期望的光标形状：沿命中节点向祖先回溯——子节点自身声明了非默认光标则用之，
    /// 否则继承最近祖先的非默认光标（如 `clickable()` 卡片的 `Hand`）。这样悬停在卡片内的
    /// label/图标等子控件上也显示手型，而非只有落在容器 padding 间隙时才手型。
    /// 禁用回退由宿主在查询前进行处理（见 `App` 的 `cursor()`）：命中节点启用则其祖先必启用。
    pub fn cursor_at(&self, id: NodeId) -> CursorShape {
        for nid in self.ancestor_chain(id) {
            if let Some(n) = self.get(nid) {
                let c = n.widget.cursor();
                if c != CursorShape::Arrow {
                    return c;
                }
            }
        }
        CursorShape::Arrow
    }

    /// 节点的悬停提示文本（无则 None）。宿主据此在悬停延时后绘制浮层。
    ///
    /// 若节点挂载的控件具备"文本截断"概念（如配了单行省略的 `Label`）且报告
    /// 当前**未**截断（`Some(false)`），视为原文已完整可见，不再弹出与其重复的
    /// 提示——避免"短文案也弹一模一样的浮层"。不具备该概念的控件（`None`）按
    /// 原语义正常返回，不受影响。
    pub fn node_tooltip(&self, id: NodeId) -> Option<String> {
        let n = self.get(id)?;
        // 控件动态提示优先：自绘图表按指针所在的数据点给文案，静态文本给不了
        // （见 [`Widget::tooltip`]）。返回 None 才回退到节点上设的静态文本。
        if let Some(dynamic) = n.widget.tooltip() {
            return Some(dynamic);
        }
        let text = n.tooltip.clone()?;
        if n.widget.text_truncated() == Some(false) {
            return None;
        }
        Some(text)
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
    /// 走穿透遮罩的命中（见 [`Tree::hit_test_for_drag`]）：模态对话框弹出时标题栏
    /// 仍可拖窗，但窗口按钮照旧被遮罩屏蔽（`interactive_hit_at` 用的是普通命中）。
    pub fn drag_hit_at(&self, pos: Point) -> bool {
        let Some(hit) = self.hit_test_for_drag(pos) else {
            return false;
        };
        if self.get(hit).map(|n| n.widget.focusable()).unwrap_or(false) {
            return false;
        }
        self.ancestor_chain(hit)
            .iter()
            .any(|&id| self.get(id).map(|n| n.window_drag).unwrap_or(false))
    }

    /// `pos`（逻辑坐标）命中的节点是否落在 `id` 的子树内（含 `id` 自身）。
    /// 供宿主判定"这次按下是否发生在当前焦点控件之外"，据此清空焦点。
    ///
    /// 判据取命中节点的祖先链而非"本次有没有控件 `request_focus`"：焦点控件的
    /// 内部子节点、以及按下被上层容器先消费的情况，都不该被误判成点了空白。
    pub fn hit_inside(&self, pos: Point, id: NodeId) -> bool {
        let Some(hit) = self.hit_test(pos) else {
            return false;
        };
        self.ancestor_chain(hit).contains(&id)
    }

    /// 节点绝对窗口矩形（累加各级父节点偏移）。
    pub fn abs_bounds(&self, id: NodeId) -> Rect {
        let mut r = match self.get(id) {
            Some(n) => {
                // 自身的绘制偏移也算进去——调用方（脏区、滚动可视、拖拽命中）要的是
                // "这个节点当前画在哪"，而非它的布局槽位。
                let mut b = n.bounds;
                b.x += n.offset.x;
                b.y += n.offset.y;
                b
            }
            None => return Rect::default(),
        };
        let mut cur = self.get(id).and_then(|n| n.parent);
        while let Some(p) = cur {
            match self.get(p) {
                Some(pn) => {
                    r.x += pn.bounds.x + pn.offset.x;
                    r.y += pn.bounds.y + pn.offset.y;
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

    /// 全树**结构签名**：对每个存活节点哈希
    /// (索引, 代际, 有效可见, 有效启用, bounds, offset, raised)。
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
                // 绘制偏移/层级提升进签名：拖拽让位这类"布局不变但像素位移"的变化据此
                // 自动升级整窗重绘，无需为其单开特例分支；hover 重同步也随之生效。
                (n.offset.x, n.offset.y, n.raised).hash(&mut h);
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

    /// 把输入法组合态变化通知给节点（见 `Widget::set_composing`）。
    /// 返回 true 表示节点存在且已通知（调用方据此判断是否需要重绘）。
    pub fn set_composing(&mut self, id: NodeId, composing: bool) -> bool {
        let Some(n) = self.get_mut(id) else {
            return false;
        };
        n.widget.set_composing(composing);
        true
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

    /// 找 `p` 下**能在指定方向继续滚动**的最近滚动容器：`increase=true` 需能增大 `scroll_y`
    /// （内容上移 / 向下滚），`false` 需能减小。内层滚动在该方向已到边界（或内容不溢出、
    /// 根本不可滚）时跳过，冒泡到外层——修正嵌套滚动"内层吃掉滚轮、外层滚不动"的问题。
    pub fn scroll_target(&self, p: Point, increase: bool) -> Option<NodeId> {
        let mut cur = self.hit_test(p);
        while let Some(id) = cur {
            let n = self.get(id)?;
            if matches!(n.layout, Layout::Scroll) {
                let view_h = (n.bounds.h - n.padding.vertical()).max(0);
                let max = (n.content_h - view_h).max(0);
                let can = if increase {
                    n.scroll_y < max
                } else {
                    n.scroll_y > 0
                };
                if can {
                    return Some(id);
                }
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
        // dy>0 减小 scroll_y、dy<0 增大；按方向找能继续滚动的容器（内层到界则冒泡外层）。
        if let Some(id) = self.scroll_target(p, dy < 0) {
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
        self.hit_node(root, p, Point::new(0, 0), false)
    }

    /// 拖动区专用命中：同 [`Tree::hit_test`]，但模态遮罩（`Widget::scrim_passthrough`）
    /// 不落定、继续穿透到下层兄弟。供 [`Tree::drag_hit_at`] 判断标题栏——对话框弹出后
    /// 遮罩覆盖全窗，普通命中会停在遮罩上，标题栏因此失去 HTCAPTION、拖不动窗口。
    fn hit_test_for_drag(&self, p: Point) -> Option<NodeId> {
        let root = self.root?;
        self.hit_node(root, p, Point::new(0, 0), true)
    }

    /// `for_drag`：拖动区判定模式，遇 `scrim_passthrough` 节点穿透（见 `hit_test_for_drag`）。
    fn hit_node(&self, id: NodeId, p: Point, origin: Point, for_drag: bool) -> Option<NodeId> {
        let n = self.get(id)?;
        if !n.effective_visible() {
            return None;
        }
        // 与 paint_node 同源：命中必须叠加同一个绘制偏移，否则移动过的节点"看得见、点不着"。
        let abs = Rect::new(
            origin.x + n.bounds.x + n.offset.x,
            origin.y + n.bounds.y + n.offset.y,
            n.bounds.w,
            n.bounds.h,
        );
        if !abs.contains(p) {
            return None;
        }
        // 滚动条区域优先命中滚动容器自身（用于拖动滚动条，而非下方内容）。
        if matches!(n.layout, Layout::Scroll) {
            let content = abs.inset(n.padding);
            if n.content_h > content.h && self.in_scrollbar_hit_zone(p, abs) {
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
            // 倒序遍历子节点：后绘制者在上层，优先命中。`raised` 子节点整体后绘制
            // （见 `Tree::paint_children`），故先倒序测它们，再倒序测其余。
            let child_origin = Point::new(abs.x, abs.y);
            for &c in n.children.iter().rev() {
                if self.get(c).map(|cn| cn.raised).unwrap_or(false) {
                    if let Some(hit) = self.hit_node(c, p, child_origin, for_drag) {
                        return Some(hit);
                    }
                }
            }
            for &c in n.children.iter().rev() {
                if !self.get(c).map(|cn| cn.raised).unwrap_or(false) {
                    if let Some(hit) = self.hit_node(c, p, child_origin, for_drag) {
                        return Some(hit);
                    }
                }
            }
        }
        // 拖动区判定：模态遮罩自身不落定，穿透到下层兄弟（标题栏在其下），
        // 使对话框弹出后仍能拖窗。遮罩内的面板有背景、会在上面的子遍历里落定，
        // 故被面板压住的标题栏区域仍判为不可拖。
        if for_drag && n.widget.scrim_passthrough() {
            return None;
        }
        // 子节点都未命中：仅当本节点「吞命中」时在此落定；否则穿透（None），
        // 让父节点继续测试其下层兄弟。防止透明纯布局容器（尤其根级全窗覆盖层，
        // 如关闭状态的对话框外层）遮挡其下内容的指针事件。
        // 吞命中 = 真实控件 / 有背景 / 滚动容器 / 拖窗区 / 拖放·右键菜单·悬停提示。
        let catches = n.widget.hit_opaque()
            || n.style.bg.is_some()
            || matches!(n.layout, Layout::Scroll)
            || n.window_drag
            || n.on_drop.is_some()
            || n.context_menu.is_some()
            || n.tooltip.is_some();
        if catches {
            Some(id)
        } else {
            None
        }
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
    ///
    /// 有可见模态层时只收集**最上层**模态子树内的节点（焦点陷阱）：对话框弹出后
    /// Tab 不该走到遮罩后面去——那些控件鼠标点不到（`ModalScrim` 吞指针），键盘却
    /// 能停上去并激活，是模态语义的破口。遮罩本身只吞指针，故这条必须在此另做。
    pub fn focusable_order(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        let scope = self.topmost_modal().or(self.root);
        if let Some(id) = scope {
            self.collect_focusable(id, &mut out);
        }
        out
    }

    /// 最上层的可见模态子树根：前序遍历中最后出现的 `is_modal` 节点。
    ///
    /// 取"最后"而非"最深"——绘制顺序靠后者盖在上面，嵌套对话框里后开的那个无论
    /// 是前者的子孙还是兄弟，都排在前序遍历的后面，与 hit_test 的层级语义一致。
    /// 沿途遇不可见或自身禁用的节点即止，使返回的模态层其父链必然可见且启用。
    ///
    /// 宿主另用它检测模态层进出，据以移交焦点（见 `UiHost::sync_modal_focus`）。
    pub(crate) fn topmost_modal(&self) -> Option<NodeId> {
        let mut found = None;
        self.scan_modal(self.root?, &mut found);
        found
    }

    fn scan_modal(&self, id: NodeId, found: &mut Option<NodeId>) {
        let Some(n) = self.get(id) else {
            return;
        };
        if !n.effective_visible() || !n.own_enabled() {
            return;
        }
        if n.widget.is_modal() {
            *found = Some(id);
        }
        for &c in &n.children {
            self.scan_modal(c, found);
        }
    }

    /// 把 `id` 滚进其各级祖先滚动容器的视口（由内向外逐级）。返回是否有容器滚动量变化。
    ///
    /// Tab 焦点落到滚动区外的控件时由宿主调用：滚出视口的节点 `visible` 仍为 true
    /// （只是被 `clip_children` 裁掉），照样在焦点环里，不滚过去焦点就"跑到看不见的
    /// 地方"了。反过来把它们踢出焦点环也不行——长列表下半截会变成键盘不可达。
    ///
    /// 逐级向外时目标矩形换成刚处理完的那一级容器自身：内层滚完后目标项就落在内层
    /// 视口内了，外层只需把内层容器整个滚进来。这样每级都只依赖当前帧的几何，不必
    /// 预演尚未发生的重排——`scroll_y` 的钳制要到下一帧 `arrange_scroll` 才生效。
    pub fn scroll_into_view(&mut self, id: NodeId) -> bool {
        let mut changed = false;
        let mut target = self.abs_bounds(id);
        // skip(1)：祖先链含自身，滚动容器要找的是它的**祖先**。
        for c in self.ancestor_chain(id).into_iter().skip(1) {
            let Some(n) = self.get(c) else {
                continue;
            };
            if !matches!(n.layout, Layout::Scroll) {
                continue;
            }
            let (pad, content_h, scroll_y) = (n.padding, n.content_h, n.scroll_y);
            let abs = self.abs_bounds(c);
            let view = Rect::new(
                abs.x + pad.left,
                abs.y + pad.top,
                (abs.w - pad.horizontal()).max(0),
                (abs.h - pad.vertical()).max(0),
            );
            // 上溢取负（内容下移、scroll 减小），下溢取正；都在视口内则不动。
            let delta = if target.y < view.y {
                target.y - view.y
            } else if target.bottom() > view.bottom() {
                target.bottom() - view.bottom()
            } else {
                0
            };
            if delta != 0 {
                let next = (scroll_y + delta).clamp(0, (content_h - view.h).max(0));
                if next != scroll_y {
                    if let Some(n) = self.get_mut(c) {
                        n.scroll_y = next;
                    }
                    changed = true;
                }
            }
            // 无论本级是否真滚了，下一级（更外层）要对齐的都是本级容器自身。
            target = self.abs_bounds(c);
        }
        changed
    }

    fn collect_focusable(&self, id: NodeId, out: &mut Vec<NodeId>) {
        if let Some(n) = self.get(id) {
            if !n.effective_visible() || !n.own_enabled() {
                // 禁用子树整体退出 Tab 导航（own_enabled 在递归中实现父链继承）。
                return;
            }
            // 节点级覆盖优先（.focusable(false) 退出焦点环），否则问控件本性。
            if n.focusable.unwrap_or_else(|| n.widget.focusable()) {
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
        //   升 Layout 使 apply_damage 直接置 needs_full，覆盖所有读者（含绑信号的文案/en_cond）。
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
                res.close_forced |= o.close_forced;
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
                if o.dialog.is_some() {
                    res.dialog = o.dialog;
                }
                // 右键上下文菜单：节点设了 context_menu 且 widget 未自行弹菜单时，
                // 构建项并请求级联浮层（沿父链冒泡，命中一个即止）。
                if secondary && matches!(ev.kind, PointerKind::Down) && res.menu.is_none() {
                    if let Some(cb) = self.get(id).and_then(|n| n.context_menu.clone()) {
                        let items = cb();
                        if !items.is_empty() {
                            res.menu = Some(crate::event::MenuRequest {
                                pos: ev.pos,
                                items,
                                min_width: 0,
                                anchor_top: None,
                                // 同一个构建器交宿主当重建器：粘滞项（复选）点击后菜单不关，
                                // 靠重跑它把勾选态刷新过来，否则勾了也不变、看着像没生效。
                                rebuild: Some(cb),
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

    /// 在事件分发之外的时机为节点 `id` 借一个 [`EventCtx`] 执行 `f`，副作用按
    /// `dispatch_key` 同款方式汇总成 [`DispatchResult`] 交宿主消费。
    ///
    /// 存在的理由：菜单项的动作闭包（[`MenuAction::Run`](crate::event::MenuAction::Run)）
    /// 由宿主在浮层里执行，那时早已不在任何控件的 `on_event` 栈内，却仍需要
    /// `ctx.defer_blocking` / `ctx.toast` / `ctx.request_close` 这些能力——没有这条
    /// 通道，"能弹对话框的回调"和"不能弹的回调"就会分成两等。
    ///
    /// 与 [`Tree::call_on_event`] 的三点不同：
    /// - **不取出目标节点的 widget**（调用者不是该控件自身），故闭包内经
    ///   `ctx.tree_mut()` 触碰目标节点是安全的，无 `call_on_event` 的那条禁令；
    /// - **不套 `signal::begin_event()` 括号**：括号会把信号写入降级成本节点局部
    ///   脏区，而菜单动作写的多半是别处读的共享状态（勾选态、列表数据）。留在括号外
    ///   即走 `Signal::set` 的"非事件期强制整窗"路径，宁可多画一帧；
    /// - 不产出 `consumed`（这里没有待消费的事件），指针捕获请求也被丢弃——浮层已
    ///   关闭，捕获无处安放。
    ///
    /// `id` 允许已失效（目标控件在菜单弹出后被重建）：`EventCtx` 的几何查询对死节点
    /// 返回零矩形，动作照常执行。
    pub(crate) fn run_detached(
        &mut self,
        id: NodeId,
        f: impl FnOnce(&mut EventCtx),
    ) -> DispatchResult {
        let mut ctx = EventCtx {
            tree: self,
            self_id: id,
            out: EventOutcome::default(),
        };
        f(&mut ctx);
        let o = ctx.out;
        DispatchResult {
            repaint: o.repaint,
            damage: o.damage,
            close: o.close,
            close_forced: o.close_forced,
            focus: o.focus,
            consumed: false,
            menu: o.menu,
            open_url: o.open_url,
            window_op: o.window_op,
            toast: o.toast,
            dialog: o.dialog,
        }
    }

    /// 分发键盘事件到焦点节点。
    pub fn dispatch_key(&mut self, ev: KeyEvent, focus: Option<NodeId>) -> DispatchResult {
        let mut res = DispatchResult::default();
        if let Some(f) = focus {
            let (consumed, o) = self.call_on_event(f, &Event::Key(ev));
            res.repaint = o.repaint;
            res.damage = o.damage;
            res.close = o.close;
            res.close_forced = o.close_forced;
            res.focus = o.focus;
            res.consumed = consumed;
            res.menu = o.menu;
            res.open_url = o.open_url;
            res.window_op = o.window_op;
            res.toast = o.toast;
            res.dialog = o.dialog;
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
            res.close_forced |= out.close_forced;
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
            if out.dialog.is_some() {
                res.dialog = out.dialog;
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
    use crate::event::{CursorShape, Key, KeyEvent, MouseButton, PointerEvent, PointerKind};
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

    /// 三行竖排（各 100×40）的树，返回 (tree, 三个子节点 id)。
    fn three_rows() -> (Tree, Vec<NodeId>) {
        let tree = layout(
            Element::col()
                .width(100)
                .height(120)
                .child(Element::leaf().width(100).height(40).bg(Color::WHITE))
                .child(Element::leaf().width(100).height(40).bg(Color::WHITE))
                .child(Element::leaf().width(100).height(40).bg(Color::WHITE)),
            100,
            120,
        );
        let kids = tree.get(tree.root.unwrap()).unwrap().children.clone();
        (tree, kids)
    }

    #[test]
    fn node_offset_shifts_both_paint_bounds_and_hit_test() {
        // offset 是绘制/命中偏移：abs_bounds（脏区与拖拽逻辑读它）与 hit_test
        // （点击落到谁身上）必须同步位移，否则控件会"看得见、点不着"。
        let (mut tree, kids) = three_rows();
        assert_eq!(tree.abs_bounds(kids[0]).y, 0);
        // 未偏移时 y=50 落在第二行。
        assert_eq!(tree.hit_test(Point::new(50, 50)), Some(kids[1]));

        tree.get_mut(kids[0]).unwrap().offset = Point::new(0, 45);

        assert_eq!(tree.abs_bounds(kids[0]).y, 45, "abs_bounds 应叠加 offset");
        assert_eq!(
            tree.get(kids[0]).unwrap().bounds.y,
            0,
            "布局 bounds 不应被 offset 污染"
        );
        // 第一行下移 45 后覆盖 y∈[45,85)，此处它排在第二行之前绘制，故第二行仍在其上层。
        assert_eq!(
            tree.hit_test(Point::new(50, 30)),
            None,
            "原位置已无节点（第一行已移走，该处是容器空白）"
        );
    }

    #[test]
    fn raised_node_wins_hit_test_over_later_siblings() {
        // raised 子节点最后绘制（画在最上层），命中也必须优先——两者不一致就会
        // 出现"画在上面却点不到"。此处让首行下移盖住次行，仅靠 raised 决定胜负。
        let (mut tree, kids) = three_rows();
        {
            let n = tree.get_mut(kids[0]).unwrap();
            n.offset = Point::new(0, 40);
        }
        // 未提升时：同一位置命中的是后绘制的第二行。
        assert_eq!(
            tree.hit_test(Point::new(50, 50)),
            Some(kids[1]),
            "未提升时后绘制的兄弟在上层"
        );

        tree.get_mut(kids[0]).unwrap().raised = true;
        assert_eq!(
            tree.hit_test(Point::new(50, 50)),
            Some(kids[0]),
            "raised 节点应优先命中"
        );
    }

    #[test]
    fn offset_change_alters_layout_signature() {
        // 签名把 offset 纳入后，拖拽让位这类"布局不变但像素位移"的变化会被宿主判为
        // 结构变化并升级整窗重绘——拖拽因此不需要任何重绘特例分支。
        let (mut tree, kids) = three_rows();
        let before = tree.layout_signature();
        tree.get_mut(kids[0]).unwrap().offset = Point::new(0, 7);
        assert_ne!(before, tree.layout_signature(), "offset 变化应改变签名");

        let mid = tree.layout_signature();
        tree.get_mut(kids[0]).unwrap().raised = true;
        assert_ne!(mid, tree.layout_signature(), "raised 变化应改变签名");
    }

    #[test]
    fn cursor_inherits_from_clickable_ancestor() {
        // clickable 卡片内的 label/图标子节点自身声明 Arrow，cursor_at 应沿父链回溯到
        // Clickable 的 Hand——否则悬停卡片内容区只显示箭头、只有 padding 间隙才手型。
        let tree = layout(
            Element::col()
                .width(100)
                .height(40)
                .clickable()
                .child(Element::label("x").width(60).height(20)),
            100,
            40,
        );
        let hit = tree
            .hit_test(Point::new(10, 10))
            .expect("应命中 label 子节点");
        assert_ne!(
            hit,
            tree.root.unwrap(),
            "命中的应是子 label 而非 clickable 根"
        );
        assert_eq!(
            tree.cursor_at(hit),
            CursorShape::Hand,
            "悬停在 clickable 卡片内的子控件上应显示手型"
        );
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

    /// 标签条：走**完整事件分发链路**验证点选切页（dispatch → 命中 → on_event →
    /// index_at → 信号）。TabBar 的其余测试都直接调 `index_at`/`key_target` 等内部方法，
    /// 单元通过并不能证明真实指针事件能落到它身上——本例补上这一段。
    #[test]
    fn tab_bar_pointer_click_switches_page_through_dispatch() {
        let sel = signal(1);
        let root = Element::tabs(
            sel,
            vec![
                ("甲", Element::label("page A")),
                ("乙", Element::label("page B")),
            ],
        );
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(400, 300), &mut te);

        // tabs = col[标签条, 内容区]；标签条是首个子节点。
        let bar = tree.get(id).unwrap().children[0];
        let b = tree.abs_bounds(bar);
        // 首项左缘内侧一点，必落在第 0 项（不依赖具体文字度量）。
        let p = Point::new(b.x + 2, b.y + b.h / 2);
        let (mut hover, mut cap) = (None, None);

        tree.dispatch_pointer(ptr(PointerKind::Move, p), &mut hover, &mut cap);
        assert_eq!(hover, Some(bar), "移动到标签条上应命中标签条节点");

        tree.dispatch_pointer(ptr(PointerKind::Down, p), &mut hover, &mut cap);
        tree.dispatch_pointer(ptr(PointerKind::Up, p), &mut hover, &mut cap);
        assert_eq!(sel.get(), 0, "点击首个标签应把选中索引切到 0");
    }

    /// 构建 [下层按钮 + 上层全覆盖容器]，返回 (tree, 按钮 id, 按钮中心点)。
    /// `opaque_bg`=true 时上层容器带背景（应吞命中），false 时为透明纯容器（应穿透）。
    fn overlay_tree(clicks: Signal<i32>, opaque_bg: bool) -> (Tree, NodeId, Point) {
        let c = clicks;
        let mut overlay = Element::stack().width_match().height_match();
        if opaque_bg {
            overlay = overlay.bg(crate::geometry::Color::rgba(0, 0, 0, 255));
        }
        let root = Element::stack()
            .width(200)
            .height(100)
            .child(Element::button("OK").on_click(move |_| c.set(c.get() + 1)))
            .child(overlay);
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 100), &mut te);
        let btn = tree.get(id).unwrap().children[0];
        let b = tree.abs_bounds(btn);
        (tree, btn, Point::new(b.x + b.w / 2, b.y + b.h / 2))
    }

    #[test]
    fn transparent_overlay_passes_pointer_through_to_lower_sibling() {
        // 透明纯容器（EmptyWidget、无背景）全覆盖在按钮之上：命中应穿透到下层按钮。
        let clicks = signal(0);
        let (mut tree, btn, center) = overlay_tree(clicks, false);
        assert_eq!(
            tree.hit_test(center),
            Some(btn),
            "透明覆盖容器应穿透命中下层按钮"
        );
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, center), &mut hover, &mut cap);
        tree.dispatch_pointer(ptr(PointerKind::Up, center), &mut hover, &mut cap);
        assert_eq!(clicks.get(), 1, "点击应穿透透明覆盖层触发下层按钮");
    }

    #[test]
    fn opaque_bg_overlay_blocks_pointer_to_lower_sibling() {
        // 带背景的容器全覆盖：吞掉命中，不穿透（卡片/面板/遮罩等视觉表面的既有行为）。
        let clicks = signal(0);
        let (mut tree, btn, center) = overlay_tree(clicks, true);
        assert_ne!(
            tree.hit_test(center),
            Some(btn),
            "带背景的覆盖容器应吞命中，不穿透"
        );
        let (mut hover, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, center), &mut hover, &mut cap);
        tree.dispatch_pointer(ptr(PointerKind::Up, center), &mut hover, &mut cap);
        assert_eq!(clicks.get(), 0, "带背景覆盖层应拦截点击，不触发下层按钮");
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
    fn scroll_into_view_brings_offscreen_node_into_viewport() {
        // 滚出视口的节点 visible 仍为 true（只是被 clip_children 裁掉），照样在焦点环里。
        // 焦点落上去必须把它滚出来，否则键盘用户"焦点跑到看不见的地方"。
        let mut col = Element::col();
        for i in 0..8 {
            col = col.child(Element::button(format!("B{i}")).height(40));
        }
        let tree_root = Element::col()
            .fill()
            .child(Element::scroll().height(100).child(col));
        let mut tree = layout(tree_root, 200, 100);
        let order = tree.focusable_order();
        assert_eq!(order.len(), 8, "8 个按钮都应在焦点环里（含滚出视口的）");

        let scroll_id = tree.ancestor_chain(order[0])[..]
            .iter()
            .copied()
            .find(|&c| matches!(tree.get(c).map(|n| &n.layout), Some(Layout::Scroll)))
            .expect("应能找到祖先滚动容器");
        assert_eq!(tree.get(scroll_id).unwrap().scroll_y, 0, "初始不滚动");

        // 首项本就在视口内：不该动。
        assert!(!tree.scroll_into_view(order[0]), "视口内的节点不应触发滚动");
        assert_eq!(tree.get(scroll_id).unwrap().scroll_y, 0);

        // 末项在视口外：应滚到刚好露出它（下溢对齐底边）。
        assert!(tree.scroll_into_view(order[7]), "视口外的节点应触发滚动");
        let sy = tree.get(scroll_id).unwrap().scroll_y;
        assert!(sy > 0, "应向下滚动，实际 scroll_y={sy}");
        let view_h = 100;
        let content_h = tree.get(scroll_id).unwrap().content_h;
        assert!(
            sy <= (content_h - view_h).max(0),
            "滚动量不应超过可滚动上限"
        );
    }

    #[test]
    fn modal_dialog_traps_tab_focus() {
        // 回归：ModalScrim 只吞指针，焦点环却仍从 root 遍历全树——对话框弹出后
        // Tab 会走到遮罩后面那些鼠标点不到的控件上。
        let show = signal(false);
        let root = Element::stack()
            .fill()
            .child(
                Element::col()
                    .child(Element::button("后方A"))
                    .child(Element::button("后方B")),
            )
            .child(Element::dialog(
                show,
                Element::col().child(Element::button("框内")),
            ));
        let tree = layout(root, 300, 200);
        assert_eq!(
            tree.focusable_order().len(),
            2,
            "对话框未显示时，Tab 应在后方两个按钮之间"
        );

        show.set(true);
        assert_eq!(
            tree.focusable_order().len(),
            1,
            "对话框弹出后 Tab 应被圈在框内，够不着后方按钮"
        );

        show.set(false);
        assert_eq!(
            tree.focusable_order().len(),
            2,
            "对话框关闭后焦点环应恢复到整树"
        );
    }

    #[test]
    fn nested_modal_traps_focus_to_topmost() {
        // 嵌套对话框：焦点归最后打开（绘制在最上）的那一个，与 hit_test 的层级一致。
        let (a, b) = (signal(true), signal(false));
        let root = Element::stack()
            .fill()
            .child(Element::button("后方"))
            .child(Element::dialog(
                a,
                Element::col().child(Element::button("A内")),
            ))
            .child(Element::dialog(
                b,
                Element::col()
                    .child(Element::button("B内1"))
                    .child(Element::button("B内2")),
            ));
        let tree = layout(root, 300, 200);
        assert_eq!(tree.focusable_order().len(), 1, "只开 A 时焦点在 A 内");

        b.set(true);
        assert_eq!(
            tree.focusable_order().len(),
            2,
            "B 压在 A 上时焦点应移交给 B，而不是留在 A"
        );
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
    fn scroll_target_bubbles_when_inner_at_edge() {
        // 嵌套滚动：外层可滚，内层内容溢出可滚。
        let inner = {
            let mut s = Element::scroll().width_match().height(40);
            for _ in 0..4 {
                s = s.child(Element::leaf().width_match().height(25)); // 内容 100 > 视口 40 → max=60
            }
            s
        };
        let outer = Element::scroll()
            .width(100)
            .height(100)
            .child(inner)
            .child(Element::leaf().width_match().height(300)); // 外层内容远超视口
        let mut tree = Tree::new();
        let oid = outer.build(&mut tree);
        tree.root = Some(oid);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te);
        let inner_id = tree.get(oid).unwrap().children[0];

        // 内层在顶部（scroll_y=0），向下滚（increase）内层仍有空间 → 命中内层。
        assert_eq!(
            tree.scroll_target(Point::new(20, 15), true),
            Some(inner_id),
            "内层未到底，向下滚应命中内层"
        );
        // 把内层滚到底（scroll_y=max=60），再向下滚 → 内层到界，冒泡外层。
        tree.set_scroll_y(inner_id, 60);
        tree.layout_root(Size::new(100, 100), &mut te);
        assert_eq!(
            tree.scroll_target(Point::new(20, 15), true),
            Some(oid),
            "内层到底后向下滚应冒泡到外层"
        );
        // 内层在底部，向上滚（decrease）内层仍可回滚 → 命中内层。
        assert_eq!(
            tree.scroll_target(Point::new(20, 15), false),
            Some(inner_id),
            "内层可上滚时向上应命中内层"
        );
    }

    #[test]
    fn scroll_target_skips_nonscrollable_inner() {
        // 内层内容不溢出（不可滚）→ 在其上滚动直接命中外层。
        let inner = Element::scroll()
            .width_match()
            .height(60)
            .child(Element::leaf().width_match().height(20)); // 20 < 60 → max=0
        let outer = Element::scroll()
            .width(100)
            .height(100)
            .child(inner)
            .child(Element::leaf().width_match().height(300));
        let mut tree = Tree::new();
        let oid = outer.build(&mut tree);
        tree.root = Some(oid);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te);
        assert_eq!(
            tree.scroll_target(Point::new(20, 10), true),
            Some(oid),
            "内层不可滚，滚动应直接命中外层"
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
        // content_h=300, view=100
        tree.layout_root(Size::new(100, 100), &mut te);
        // 容器贴窗口右缘 → 滚动条内缩，命中区止于 100 - WINDOW_EDGE_INSET。
        let (lo, hi) = tree.scrollbar_hit_zone(tree.abs_bounds(id));
        let expect_hi = 100 - scrollbar::WINDOW_EDGE_INSET;
        assert_eq!((lo, hi), (expect_hi - scrollbar::HIT_W, expect_hi));
        let x = (lo + hi) / 2;
        let (mut h, mut cap) = (None, None);
        let down = PointerEvent::single(PointerKind::Down, Point::new(x, 10), MouseButton::Left);
        tree.dispatch_pointer(down, &mut h, &mut cap);
        assert_eq!(cap, Some(id), "滚动条区域按下应捕获滚动容器");
        // 向下拖 30px → 内容按 content/view 比例移动
        let mv = PointerEvent::single(PointerKind::Move, Point::new(x, 40), MouseButton::Left);
        tree.dispatch_pointer(mv, &mut h, &mut cap);
        tree.layout_root(Size::new(100, 100), &mut te);
        assert!(tree.get(id).unwrap().scroll_y > 0, "拖动滚动条应增加偏移");
    }

    /// 贴窗口右缘的滚动条须整体内缩，把最外侧那圈让给 `WM_NCHITTEST` 的缩放边框——
    /// 否则滚动条画得出来却永远收不到指针事件（本次修复的核心回归）。
    fn scroll_tree_of_width(win_w: i32, container_w: i32) -> (Tree, NodeId) {
        let mut sc = Element::scroll().width(container_w).height(100);
        for _ in 0..10 {
            sc = sc.child(Element::leaf().width_match().height(30));
        }
        // 用一个左对齐的行包住，使容器右缘可控地远离/贴近窗口右缘。
        let root = Element::row().width(win_w).height(100).child(sc);
        let mut tree = Tree::new();
        let rid = root.build(&mut tree);
        tree.root = Some(rid);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(win_w, 100), &mut te);
        let sid = tree.get(rid).unwrap().children[0];
        (tree, sid)
    }

    #[test]
    fn scrollbar_insets_only_when_flush_with_window_edge() {
        // 贴右缘：命中区上界须停在窗口边缘内 WINDOW_EDGE_INSET 处。
        let (tree, sid) = scroll_tree_of_width(200, 200);
        let (_, hi) = tree.scrollbar_hit_zone(tree.abs_bounds(sid));
        assert_eq!(hi, 200 - scrollbar::WINDOW_EDGE_INSET, "贴边容器应内缩让位");
        // 缩放边框那一圈不再被滚动条抢走。
        assert!(
            !tree.in_scrollbar_hit_zone(Point::new(195, 50), tree.abs_bounds(sid)),
            "最外侧应归还给窗口缩放边框"
        );

        // 远离右缘（对话框内的滚动区）：保持紧凑，不平白多出一段空白。
        let (tree, sid) = scroll_tree_of_width(200, 100);
        let (_, hi) = tree.scrollbar_hit_zone(tree.abs_bounds(sid));
        assert_eq!(hi, 100, "非贴边容器不内缩");
    }

    /// 命中区必须有上界。旧实现是 `x >= right - 10` 的半开区间，等于宣称最右一切都归
    /// 滚动条，与窗口缩放边框直接争抢。
    #[test]
    fn scrollbar_hit_zone_is_bounded_on_both_sides() {
        let (tree, sid) = scroll_tree_of_width(200, 100);
        let b = tree.abs_bounds(sid);
        assert!(
            !tree.in_scrollbar_hit_zone(Point::new(83, 50), b),
            "左侧界外"
        );
        assert!(tree.in_scrollbar_hit_zone(Point::new(84, 50), b), "区间内");
        assert!(tree.in_scrollbar_hit_zone(Point::new(99, 50), b), "区间内");
        assert!(
            !tree.in_scrollbar_hit_zone(Point::new(100, 50), b),
            "右侧界外"
        );
    }

    /// 预留宽度必须跟着内缩量走，否则贴边容器的滚动条会压到内容上。
    #[test]
    fn scroll_content_width_reserves_room_for_inset_scrollbar() {
        let (tree, sid) = scroll_tree_of_width(200, 200);
        let child = tree.get(sid).unwrap().children[0];
        assert_eq!(
            tree.get(child).unwrap().bounds.w,
            200 - scrollbar::occupied_w(scrollbar::WINDOW_EDGE_INSET),
            "贴边容器内容宽须让出滚动条 + 内缩量"
        );
        let (tree, sid) = scroll_tree_of_width(200, 100);
        let child = tree.get(sid).unwrap().children[0];
        assert_eq!(
            tree.get(child).unwrap().bounds.w,
            100 - scrollbar::occupied_w(0),
            "非贴边容器只让出滚动条本身"
        );
    }

    /// 限宽必须在**测量前**收窄可用宽：节点撑满可用宽时，最终宽应被上界收住。
    #[test]
    fn max_width_caps_matched_width() {
        let root = Element::col()
            .width(400)
            .height(100)
            .child(Element::leaf().width_match().height(10).max_width(240));
        let tree = layout(root, 400, 100);
        let child = tree.get(tree.root.unwrap()).unwrap().children[0];
        assert_eq!(
            tree.get(child).unwrap().measured.w,
            240,
            "width_match 应被 max_width 收住"
        );
    }

    /// 内容本就比上界窄时，限宽不该把它撑宽——上界是上界，不是固定宽。
    #[test]
    fn max_width_leaves_narrow_content_alone() {
        let root = Element::col()
            .width(400)
            .height(100)
            .child(Element::leaf().width(80).height(10).max_width(240));
        let tree = layout(root, 400, 100);
        let child = tree.get(tree.root.unwrap()).unwrap().children[0];
        assert_eq!(tree.get(child).unwrap().measured.w, 80);
    }

    /// 上下界冲突时以**上界**为准：调用方给出的硬上限不应被下界顶破。
    #[test]
    fn max_width_wins_over_min_width() {
        let root = Element::col().width(400).height(100).child(
            Element::leaf()
                .width_match()
                .height(10)
                .min_width(300)
                .max_width(200),
        );
        let tree = layout(root, 400, 100);
        let child = tree.get(tree.root.unwrap()).unwrap().children[0];
        assert_eq!(tree.get(child).unwrap().measured.w, 200);
    }

    /// 限高封顶节点占位，但**不得**削减滚动容器的 `content_h`——溢出部分要转成可滚动量，
    /// 而不是在测量阶段就被丢掉（否则限高等于截断，滚动条根本不会出现）。
    #[test]
    fn max_height_caps_node_but_keeps_scrollable_content() {
        let mut sc = Element::scroll().width(100).max_height(80);
        for _ in 0..10 {
            sc = sc.child(Element::leaf().width_match().height(30));
        }
        let root = Element::col().width(200).height(400).child(sc);
        let tree = layout(root, 200, 400);
        let sid = tree.get(tree.root.unwrap()).unwrap().children[0];
        let n = tree.get(sid).unwrap();
        assert_eq!(n.measured.h, 80, "节点占位应被限高收住");
        assert_eq!(n.content_h, 300, "内容高须保持完整，供滚动使用");
    }

    /// 上界是上界，不是固定高：内容比上界矮时不该被撑高（对话框才能自然收缩）。
    #[test]
    fn max_height_leaves_short_content_alone() {
        let sc = Element::scroll()
            .width(100)
            .max_height(220)
            .child(Element::leaf().width_match().height(40));
        let root = Element::col().width(200).height(400).child(sc);
        let tree = layout(root, 200, 400);
        let sid = tree.get(tree.root.unwrap()).unwrap().children[0];
        assert_eq!(tree.get(sid).unwrap().measured.h, 40);
    }

    /// 行高直接改变文字节点的占位高度（`NullTextEngine` 如实反映倍数）。
    #[test]
    fn line_height_scales_text_node_height() {
        let plain = layout(
            Element::col()
                .width(200)
                .height(200)
                .child(Element::label("行高").font_size(20.0)),
            200,
            200,
        );
        let tall = layout(
            Element::col()
                .width(200)
                .height(200)
                .child(Element::label("行高").font_size(20.0).line_height(2.0)),
            200,
            200,
        );
        let h = |t: &Tree| {
            let c = t.get(t.root.unwrap()).unwrap().children[0];
            t.get(c).unwrap().measured.h
        };
        assert_eq!(h(&plain), 20, "未设行高时按字号占位");
        assert_eq!(h(&tall), 40, "行高 2.0 应使占位翻倍");
    }

    /// 单边边框**不参与布局**——这正是它相对「1px 色块」的价值所在。
    #[test]
    fn border_edges_does_not_affect_layout() {
        let mk = |e: Option<crate::style::Edges>| {
            let mut leaf = Element::leaf()
                .width(100)
                .height(50)
                .border(Color::BLACK, 1);
            if let Some(e) = e {
                leaf = leaf.border_edges(e);
            }
            let t = layout(Element::col().width(200).height(200).child(leaf), 200, 200);
            let c = t.get(t.root.unwrap()).unwrap().children[0];
            t.get(c).unwrap().measured
        };
        assert_eq!(mk(None), mk(Some(crate::style::Edges::BOTTOM)));
    }

    /// `Edges` 的按位合并语义：合并后两条边都在，其余仍不在。
    #[test]
    fn edges_bitor_merges() {
        use crate::style::Edges;
        let e = Edges::TOP | Edges::BOTTOM;
        assert!(e.top && e.bottom);
        assert!(!e.left && !e.right);
        assert!(!e.is_all(), "只有四边齐全才算 all");
        assert!((Edges::TOP | Edges::BOTTOM | Edges::LEFT | Edges::RIGHT).is_all());
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
        // 运行第三项动作 → 选中索引变 2。动作收 ctx，按宿主的执行方式借一个（run_detached）。
        if let crate::event::MenuAction::Run(f) = &menu.items[2].action {
            tree.run_detached(dd, |ctx| f(ctx));
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
                MenuItem::run("剪切", |_ctx| {}, false).icon("✂"),
                MenuItem::separator(),
                MenuItem::submenu("更多", vec![MenuItem::run("子项", |_ctx| {}, false)]).icon("⋯"),
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

    /// 上下文菜单必须把构建器一并交给宿主当重建器：粘滞项（菜单内的复选开关）点击后
    /// 菜单不关，靠重跑它刷新勾选态——不交的话勾了也不变，看着像没生效。
    #[test]
    fn on_context_menu_hands_builder_to_host_as_rebuilder() {
        use crate::event::MenuItem;
        use crate::ui::Element;
        use std::cell::Cell as StdCell;
        let on = Rc::new(StdCell::new(false));
        let (o_build, o_click) = (on.clone(), on.clone());
        let tree_el = Element::col().fill().on_context_menu(move || {
            let o = o_click.clone();
            vec![MenuItem::run("开关", move |_ctx| o.set(!o.get()), o_build.get()).stay_open()]
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
        assert!(!menu.items[0].checked, "初始未勾选");
        let rebuild = menu.rebuild.clone().expect("应交付重建器");
        // 模拟宿主：执行粘滞项动作后重跑重建器 → 勾选态跟着翻。
        let root = tree.root.unwrap();
        if let crate::event::MenuAction::Run(f) = &menu.items[0].action {
            tree.run_detached(root, |ctx| f(ctx));
        }
        assert!(rebuild()[0].checked, "重建后勾选态应反映新值");
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

    #[test]
    fn on_update_toast_is_captured_for_host() {
        // 回归：on_update（响应式相位）里发的 toast 曾随 EventOutcome 一起被丢弃，
        // 导致 toast_sink 等经信号触发的提示永不上屏。此处确认其被暂存供宿主取走。
        struct ToastOnUpdate;
        impl Widget for ToastOnUpdate {
            fn on_update(&mut self, ctx: &mut EventCtx) {
                ctx.toast_ok("已保存");
            }
        }
        let mut tree = Tree::new();
        let id = Element::leaf()
            .reactive()
            .widget(ToastOnUpdate)
            .build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te);
        let toasts = tree.take_pending_toasts();
        assert_eq!(toasts.len(), 1, "on_update 发出的 toast 应被暂存供宿主上屏");
        assert_eq!(toasts[0].text, "已保存");
        assert!(tree.take_pending_toasts().is_empty(), "取走后应清空暂存");
    }
}
