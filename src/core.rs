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
    ToastKind, ToastRequest, WindowOp, WindowRequest,
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
    ///
    /// **让位只作用于滚动容器的直接子节点**：`arrange_scroll` 收窄的是它的 bounds，而
    /// 更深一层的 `width_match` 子树在 arrange 期沿用 measure 量到的宽度（measure 是按
    /// 让位前的可用宽做的），并不会跟着缩。于是实际效果是"让位量由沿途的内边距吸收"——
    /// 内容有 ≥ `occupied_w` 的右内边距时滚动条恰好落在那段留白里（页面左右留白因此保持
    /// 对称，滚动条不额外占一条道）；内边距不足时，内容会伸到滚动条底下。
    ///
    /// 故**滚动区的内容须自带不小于本值的横向内边距**。曾试过让 `Match` 在 arrange 期
    /// 一律取父容器的最终可用量（严格让位），结果是每个带滚动条的页面右侧凭空多出一条
    /// 空道、左右留白不再对称——收益不抵代价，遂保留现行语义并在此写明。
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
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct NodeId {
    index: u32,
    generation: u32,
}

/// 紧凑格式 `#12`（复用过的槽位带代际：`#12g2`）。
///
/// 手写而非 `derive`：派生格式是 `NodeId { index: 12, generation: 0 }`，一行诊断里塞三五个
/// 就没法读了，而 id 恰恰是诊断输出里最常出现的东西。代际为 0 时省略——绝大多数节点从未
/// 被回收复用，带上只是噪声；非 0 时必须显示，否则"旧 id 指向新节点"这类问题看不出来。
impl std::fmt::Debug for NodeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.generation == 0 {
            write!(f, "#{}", self.index)
        } else {
            write!(f, "#{}g{}", self.index, self.generation)
        }
    }
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
    /// 本节点绑定的对话框显示信号（仅对话框遮罩）。`Element::build` 据此把节点登记进
    /// [`Tree::modals`]，使 ESC / 窗口关闭能优先关掉最顶层可见对话框。
    ///
    /// 与 [`Widget::is_modal`] 同为遮罩的属性但用途不同：那个管键盘焦点圈定，这个管
    /// "谁能被 ESC 关掉"。分开是因为前者只需知道"是不是模态"，后者需要拿到那个信号。
    fn modal_signal(&self) -> Option<Signal<bool>> {
        None
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
    ///
    /// **仅 win32 这条路会调**——那边系统 IME 自己画合成串。需自绘的平台走
    /// [`Self::set_preedit`]。
    fn set_composing(&mut self, _composing: bool) {}

    /// 输入法合成串变化时由框架通知焦点节点。文本控件借此把未提交的合成串
    /// 内联显示出来（见 `TextInput`）。默认无操作。
    ///
    /// 空 `text` 表示合成结束，控件应清掉合成串并恢复正常光标。
    fn set_preedit(&mut self, _pe: &crate::event::Preedit) {}

    /// 本控件当前选区（**字符**索引）。无选区的文本控件返回光标处空范围，
    /// 非文本控件返回 `None`。供输入法查询上下文。
    fn selection_range(&self) -> Option<(usize, usize)> {
        None
    }

    /// 本控件的已提交正文（不含未上屏合成串），供输入法读取上下文。
    /// 非文本控件返回 `None`；密码框应返回 `None`（不把密码交给输入法）。
    fn ime_text(&self) -> Option<String> {
        None
    }
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

/// 声明式初始焦点（[`crate::ui::Element::autofocus`]）。
///
/// 存在的理由：[`EventCtx::request_focus`] 只能把焦点给**自己**，且只在该控件自己的
/// 事件回调里可用——于是「进程起来时焦点该在谁身上」这件事在应用层无从表达。对常驻
/// 托盘、`start_hidden()` 起步的工具尤其致命：热键唤起后 `focus == None`，第一次按键
/// 无处可去（[`Tree::dispatch_key`] 的目标是 `Option<NodeId>`，为 `None` 时整个事件
/// 直接丢弃）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autofocus {
    /// 只聚焦，光标落在文本末尾。
    Focus,
    /// 聚焦并全选已有内容——查询框/地址栏语义：上次查的词还在框里，下次唤起直接
    /// 覆盖打字，不用先删。
    ///
    /// 实现走**合成 Ctrl+A 回送控件**，与右键菜单的复制/粘贴同一条既有通路
    /// （见 `TextInput::context_menu_items`）。故对不处理 Ctrl+A 的控件是无害空操作，
    /// 与 [`Focus`](Self::Focus) 等价。
    FocusSelectAll,
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
    ///
    /// 在 `Layout::Frame`（stack）里它同时管两个轴——`Align::End` 就是右下角。要把
    /// 子元素放到"右上"这类**两轴取值不同**的位置，再补 [`Node::align_v`]。
    pub align: Option<Align>,
    /// **纵轴对齐覆盖**（仅 `Layout::Frame`）：`None` 时纵轴跟随 [`Node::align`]，
    /// `Some(a)` 时纵轴单独取 `a`、`align` 只管横轴。
    ///
    /// 只为 Frame 而设：线性容器的主轴由排布本身决定、交叉轴才用 `align`，本就不存在
    /// "两轴各要一个值"的问题。stack 则是唯一能把子元素摆到任意角的布局，而角落是
    /// 两轴的组合——对话框右上角的小关闭按钮正是如此。
    pub align_v: Option<Align>,
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
    /// 上一次 [`Tree::reset_hidden_interactions`] 扫描时**是否参与交互**
    /// （祖先链累积可见 ∧ 祖先链累积启用）。用于检测「退出交互」的翻转。
    ///
    /// 名字里只有 `visible` 是历史遗留：这里存的一直是"控件还收不收得到事件"，
    /// 而**被禁用**与被隐藏一样会让它收不到（见那个方法的文档）。
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
    /// **锚定浮层（portal）**：`Some(open)` 时本节点脱离父容器的布局流，改由 [`Tree`]
    /// 在根级排布——锚在父节点正下方（下方放不下则上翻）、**绘制在整棵树之后**、
    /// **命中先于整棵树**，且不受任何祖先 `clip_children` 裁剪。
    ///
    /// 与 [`Node::raised`] 的区别是作用域：`raised` 只在**同级兄弟**间提升绘制顺序，
    /// 逃不出祖先的裁剪与绘制序；下拉面板必须浮在整个窗口内容之上，只能走本字段。
    ///
    /// 信号即「是否展开」：核心据它决定可见性，并在**浮层与锚点之外按下**或 ESC 时
    /// 置 false（轻量关闭）。锚点自身的点击**不**触发关闭——那由触发器控件自己 toggle，
    /// 否则会「先被核心关掉、再被控件打开」，点了等于没点。
    ///
    /// **只能经 [`Element::popup`](crate::ui::Element::popup) 设置。** 本字段只是一半
    /// 真相：另一半是 `Tree::overlays` 那份登记表，而它**只在 [`Tree::insert`] 时按本
    /// 字段登记**。事后直接给已建好的节点赋值，会得到一个既被排除出父容器布局流、
    /// 又从不被排布/绘制/命中的幽灵节点——彻底消失，且没有任何提示。
    pub overlay: Option<Signal<bool>>,
    /// 声明式初始焦点（`None`=不参与）。宿主在布局稳定后**一次性**兑现，见
    /// [`Autofocus`] 与 `UiHost::refresh_focus`。
    pub autofocus: Option<Autofocus>,
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
    /// on_update 相位里控件请求的**焦点转移**暂存区。与 `pending_toasts` 同因同治：
    /// 该相位丢弃整个 `EventOutcome`，`EventCtx::request_focus` 设的那一位也随之消失。
    ///
    /// 需要它的场景：一块列表整体占一个焦点位（roving tabindex），而点击是列表**行**
    /// 自己消费的——行没法替父容器要焦点，只能由父容器在 on_update 里替自己要。没有
    /// 这条通道，鼠标点完一行之后方向键就落不到列表上，键盘与鼠标接不起来。
    pending_focus: Option<NodeId>,
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
    /// 本帧根节点尺寸（`layout_root` 入口记录）。
    ///
    /// 与 `root.bounds` 的区别只在**时机**：bounds 要等 `arrange` 才更新，而响应式相位
    /// （`Widget::on_update`）跑在 `measure` 之前——那时读任何节点的 bounds 拿到的都是
    /// 上一帧的值。需要在响应式相位就知道"这一帧窗口有多大"的控件读这个字段，典型是
    /// 虚拟滚动：它按视口高决定构建多少行，而视口再大也不会超过窗口。
    pub layout_size: Size,
    /// 本树上所有对话框遮罩的显示信号，按 `build` 的先序遍历登记（父在前、子在后），
    /// 故**栈顶即最内层**：嵌套对话框下 ESC 先关最里面那个。
    ///
    /// 挂在树上而非线程全局：同线程跑多个窗口时，全局栈会让在 A 窗口按下的 ESC 关掉
    /// B 窗口的对话框。归属到树，每个宿主只看得见自己那棵树上的遮罩。
    modals: Vec<Signal<bool>>,
    /// 本树上所有锚定浮层节点（见 [`Node::overlay`]），按插入先序登记。
    ///
    /// 需要这份登记而不是每次遍历全树找 `overlay` 节点：绘制、命中、轻量关闭三条
    /// 路径每帧/每次鼠标移动都要用它，全 arena 扫描会把浮层的成本摊到所有界面上，
    /// 而绝大多数界面一个浮层也没有。**顺序即层叠序**：后登记者在上层。
    overlays: Vec<NodeId>,
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
            pending_focus: None,
            arrange_origin: Point::new(0, 0),
            layout_size: Size::ZERO,
            modals: Vec::new(),
            overlays: Vec::new(),
        }
    }

    /// 取走 on_update 相位累积的 toast 请求（宿主在 layout 后调用上屏），并清空暂存。
    pub fn take_pending_toasts(&mut self) -> Vec<ToastRequest> {
        std::mem::take(&mut self.pending_toasts)
    }

    /// 取走 on_update 相位攒下的焦点转移请求（见 `pending_focus`）。
    pub fn take_pending_focus(&mut self) -> Option<NodeId> {
        self.pending_focus.take()
    }

    /// 登记一个对话框遮罩的显示信号（`Element::build` 在插入遮罩节点时调用）。
    ///
    /// 同一个信号重复登记会被忽略：子树重建会让同一个 `Element::dialog` 再走一次
    /// `build`，不去重则栈随重建次数无界增长。
    pub(crate) fn register_modal(&mut self, show: Signal<bool>) {
        if !self.modals.contains(&show) {
            self.modals.push(show);
        }
    }

    /// 关闭本树上最顶层（最内层）的可见对话框。返回是否确实关掉了一个。
    ///
    /// 顺带清掉句柄已失效的登记（子树连同其信号一起被回收后留下的空壳），使反复重建
    /// 带对话框的子树不会让本栈越积越长。
    pub(crate) fn close_topmost_modal(&mut self) -> bool {
        self.modals.retain(|s| s.is_alive());
        for sig in self.modals.iter().rev() {
            if sig.get() {
                sig.set(false);
                return true;
            }
        }
        false
    }

    // ---- arena ----

    pub fn insert(&mut self, node: Node) -> NodeId {
        let is_overlay = node.overlay.is_some();
        let id = self.insert_slot(node);
        if is_overlay {
            self.overlays.push(id);
        }
        id
    }

    fn insert_slot(&mut self, node: Node) -> NodeId {
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
        // 浮层登记随节点一起回收：槽位会被后来的节点复用，留着的话新节点会凭空
        // 继承「我是浮层」的身份（代际校验只挡得住 `get`，挡不住这份 id 列表本身）。
        self.overlays.retain(|o| *o != id);
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
        let requested_focus = ctx.out.focus.take();
        if let Some(n) = self.get_mut(id) {
            n.widget = widget;
        }
        if let Some(req) = requested_toast {
            self.pending_toasts.push(req);
        }
        // 后来者覆盖前者：同一轮里多个控件都要焦点时，只可能满足一个，取最后那个与
        // 事件路径的语义一致（`EventOutcome::focus` 本身也是单值）。
        if let Some(id) = requested_focus {
            self.pending_focus = Some(id);
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

    /// 参与父容器布局的子节点：可见 ∧ 非锚定浮层。
    ///
    /// 浮层被排除在此，是它「脱离布局流」的**唯一**实现点——不占父容器的主轴长度、
    /// 不撑大父容器的测量结果，父容器完全不知道它的存在。它的测量与排布另由
    /// [`Tree::layout_overlays`] 在根级完成。
    fn visible_children(&self, id: NodeId) -> Vec<NodeId> {
        match self.get(id) {
            Some(n) => n
                .children
                .iter()
                .copied()
                .filter(|c| {
                    self.get(*c)
                        .map(|n| n.effective_visible() && n.overlay.is_none())
                        .unwrap_or(false)
                })
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
        // 先记本帧尺寸再广播响应式更新：`on_update` 里读得到"这一帧窗口多大"，
        // 而节点的 bounds 此刻还停在上一帧（见 `layout_size`）。
        self.layout_size = size;
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
        // 浮层必须排在整棵树 arrange **之后**：它的锚点取自父节点的绝对矩形，
        // 而那要等父级排完才成立。
        self.layout_overlays(size, text);
    }

    /// 锚定浮层与其锚点之间的间隙（逻辑 px）。
    const OVERLAY_GAP: i32 = 4;

    /// 锚定浮层的根级排布（见 [`Node::overlay`]）。
    ///
    /// 定位口径与 `EventCtx::show_dropdown_menu` 一致：左缘对齐锚点、贴在锚点下方，
    /// 下方装不下就翻到上方；两边都装不下时贴住窗口底/右缘钳制，宁可盖住锚点也不
    /// 让面板一半在窗口外——那等于控件不可用。
    ///
    /// 落到 `bounds` 里的是**相对父节点**的坐标，与其余节点同一口径。这样浮层会
    /// 自动跟着锚点走（滚动、拖拽让位都不必重排浮层），`abs_bounds` 也无需特判。
    fn layout_overlays(&mut self, win: Size, text: &mut dyn TextEngine) {
        if self.overlays.is_empty() {
            return;
        }
        // 取出再放回：measure/arrange 要 `&mut self`，借着这份列表就动不了树。
        let mut list = std::mem::take(&mut self.overlays);
        list.retain(|&id| self.get(id).is_some());
        self.overlays = list.clone();
        for id in list {
            // 被模态层挡在外面的浮层直接收起，而不是隐着等对话框关掉再冒出来——
            // 那会让一个用户早已忘记的面板在关闭对话框后凭空重现。写入有 `get()`
            // 守卫，是一次性的，不会每帧弄脏。
            if !self.overlay_in_modal_scope(id) {
                if let Some(sig) = self.get(id).and_then(|n| n.overlay) {
                    if sig.get() {
                        sig.set(false);
                    }
                }
                continue;
            }
            if !self.overlay_showing(id) {
                continue;
            }
            let Some(parent) = self.get(id).and_then(|n| n.parent) else {
                continue;
            };
            // 锚点被滚出视口后也收起：浮层刻意不受祖先裁剪，锚点滚没了它还浮着，
            // 位置又被下面的 clamp 钳在窗口边缘，就成了一块与任何东西都无关的浮块。
            // 滚轮不触发轻量关闭（那只挂在 Down 上），故必须在这里兜住。
            if self.anchor_scrolled_out(parent, win) {
                if let Some(sig) = self.get(id).and_then(|n| n.overlay) {
                    if sig.get() {
                        sig.set(false);
                    }
                }
                continue;
            }
            // 尺寸维度要走 `child_spec` 翻译成 MeasureSpec，跟普通父容器给子节点定规格
            // 是同一套：`measure` 本身**不看**节点自己的 `width`/`height`，那一直是父级
            // 的职责。直接丢一对 at_most 进去，固定尺寸的面板会被当成 Wrap 量成 0×0。
            let (w_dim, h_dim) = match self.get(id) {
                Some(n) => (n.width, n.height),
                None => continue,
            };
            let size = self.measure(
                id,
                child_spec(w_dim, win.w, false),
                child_spec(h_dim, win.h, false),
                text,
            );
            let anchor = self.abs_bounds(parent);
            let below = anchor.bottom() + Self::OVERLAY_GAP;
            let y = if below + size.h <= win.h {
                below
            } else {
                let above = anchor.y - Self::OVERLAY_GAP - size.h;
                if above >= 0 {
                    above
                } else {
                    (win.h - size.h).max(0)
                }
            };
            let x = anchor.x.clamp(0, (win.w - size.w).max(0));
            let saved = self.arrange_origin;
            self.arrange_origin = Point::new(anchor.x, anchor.y);
            self.arrange(id, Rect::new(x - anchor.x, y - anchor.y, size.w, size.h));
            self.arrange_origin = saved;
        }
    }

    /// 锚点是否已被滚出（或本来就落在）所有祖先裁剪容器的视口之外。
    ///
    /// 只看**裁剪**容器：非裁剪的祖先不会把子节点藏起来，拿它们的矩形做判据会误伤
    /// （自适应高的容器矩形可能比内容还小）。窗口本身也算一层——锚点整个滚出窗口时
    /// 同样该收起。
    fn anchor_scrolled_out(&self, anchor: NodeId, win: Size) -> bool {
        let a = self.abs_bounds(anchor);
        if a.intersect(&Rect::from_size(win)).is_empty() {
            return true;
        }
        // skip(1)：祖先链含自身，要找的是它的祖先。
        for c in self.ancestor_chain(anchor).into_iter().skip(1) {
            let Some(n) = self.get(c) else { continue };
            if !n.clip_children {
                continue;
            }
            if a.intersect(&self.abs_bounds(c).inset(n.padding)).is_empty() {
                return true;
            }
        }
        false
    }

    /// 浮层当前是否该上屏：自身可见 ∧ **整条祖先链**可见。
    ///
    /// 祖先链要自己走，是因为浮层的绘制与命中都从根级直接进入、不经过父节点那趟
    /// 递归——而正是那趟递归在替普通节点做「父不可见则整棵子树不画」。少了这一步，
    /// 切到另一个 Tab 页之后，上一页里展开着的下拉面板会继续浮在新页面上。
    fn overlay_showing(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            match self.get(c) {
                Some(n) if n.effective_visible() => cur = n.parent,
                _ => return false,
            }
        }
        self.overlay_in_modal_scope(id)
    }

    /// 浮层是否落在当前模态作用域内（无对话框打开时恒真）。
    ///
    /// 没有这道闸，浮层会**浮在对话框之上并抢走点击**：它无条件地画在整棵树之后、
    /// 命中先于整棵树，而遮罩只是树里的一个普通节点。焦点那一侧早就按模态作用域裁过了
    /// （见 [`Tree::focusable_order`]），命中与绘制不跟上，三者对「模态期间谁可交互」
    /// 的判断就不一致。
    ///
    /// 指针路径大多能自愈——点开对话框那一下会先被 `dismiss_overlays_outside` 收掉浮层；
    /// 但键盘快捷键、`on_submit`、定时器、跨线程消息弹出的对话框都走不到那一步。
    ///
    /// **先问 `modals` 再扫树**：`topmost_modal` 是一次全树前序遍历，而本方法在每次
    /// 鼠标移动的命中测试里都会被调用。绝大多数时候一个对话框都没开，那时只需扫一遍
    /// 长度通常为 0 的信号表。
    fn overlay_in_modal_scope(&self, id: NodeId) -> bool {
        if !self.modals.iter().any(|s| s.is_alive() && s.get()) {
            return true;
        }
        let Some(scope) = self.topmost_modal() else {
            return true;
        };
        let mut cur = Some(id);
        while let Some(c) = cur {
            if c == scope {
                return true;
            }
            cur = self.get(c).and_then(|n| n.parent);
        }
        false
    }

    /// 浮层的绘制/命中原点 = 其锚点（父节点）的绝对左上角。与 `paint_node` 里
    /// `child_origin` 的口径相同，故浮层内部的坐标推导与普通子树完全一致。
    fn overlay_origin(&self, id: NodeId) -> Option<Point> {
        let parent = self.get(id).and_then(|n| n.parent)?;
        let a = self.abs_bounds(parent);
        Some(Point::new(a.x, a.y))
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
            // 纵轴可单独覆盖（见 `Node::align_v`）：未设时跟随 `align`，两轴同值即旧行为。
            let align_v = self.get(c).and_then(|n| n.align_v).unwrap_or(align);
            let avail_w = (inner.w - cm.horizontal()).max(0);
            let avail_h = (inner.h - cm.vertical()).max(0);
            // Stretch 按轴各判各的：横轴 Stretch + 纵轴 Start 是合法组合（顶部通栏）。
            let cw = if align == Align::Stretch {
                avail_w
            } else {
                cs.w
            };
            let ch = if align_v == Align::Stretch {
                avail_h
            } else {
                cs.h
            };
            let x = inner.x + cm.left + align_offset(align, avail_w, cw);
            let y = inner.y + cm.top + align_offset(align_v, avail_h, ch);
            self.arrange(c, Rect::new(x, y, cw, ch));
        }
    }

    // ---- Paint ----

    /// 从根递归绘制到 canvas。
    pub fn paint(&self, canvas: &mut dyn Canvas) {
        if let Some(root) = self.root {
            self.paint_node(canvas, root, Point::new(0, 0), true);
        }
        // 锚定浮层最后画，且从根级重新进入——不带任何祖先的裁剪栈，故能盖住滚动
        // 容器的边界与其后的一切兄弟。登记序即层叠序，后登记者压在上面。
        for &id in &self.overlays {
            if !self.overlay_showing(id) {
                continue;
            }
            let (Some(origin), Some(parent)) =
                (self.overlay_origin(id), self.get(id).and_then(|n| n.parent))
            else {
                continue;
            };
            self.paint_node(canvas, id, origin, self.node_enabled(parent));
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

        // 本节点的自绘是否可能落进画布范围。局部重绘（光标闪烁这类只脏几十像素的动画）
        // 里绝大多数节点都落在外面：它们的图元照样会被光栅器逐像素丢弃，但构造与文字
        // 排版的开销**已经付掉了**——实测 120 个控件的界面每帧仍提交 61 次描边、
        // 122 次文字，0.83ms/帧 ≈ 5% 单核，而真正要重画的只有光标那一条。
        //
        // 只跳过**自绘**，子树照常递归：`offset`（拖拽浮起等）能把子节点搬到父矩形之外，
        // 按父矩形剪整棵子树会真的丢内容。遍历本身几乎不要钱，图元与排版才是成本。
        let self_visible = canvas.cull_rect().is_none_or(|cull| {
            // 与 `visual_bounds` 同一套余量：焦点环在框外、投影更远，报窄了会被切掉。
            let mut pad = if n.focused { 3 } else { DAMAGE_MARGIN };
            if let Some(sh) = &n.style.shadow {
                if sh.color.a > 0 {
                    let ext = (sh.spread + sh.blur).ceil() as i32
                        + (sh.dx.abs().max(sh.dy.abs())).ceil() as i32;
                    pad = pad.max(ext);
                }
            }
            !abs.inflate(pad).intersect(&cull).is_empty()
        });

        // 子树整体不透明度：<1 时入离屏层，绘完整棵子树后按 opacity 合成回父层。
        // 与自绘剪枝无关——层的 push/pop 必须成对，且子树可能有内容要画。
        let use_layer = n.style.opacity < 1.0;
        if use_layer {
            canvas.push_layer(n.style.opacity);
        }

        let theme = crate::theme::current();
        let content = abs.inset(n.padding);
        if self_visible {
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
        // 锚定浮层不在此列：它由 `Tree::paint` 在整棵树之后单独绘制。留在这里画的话
        // 它照样会被后续兄弟盖住，「浮层」二字就落空了。
        let ordinary = |c: NodeId| {
            self.get(c)
                .map(|cn| cn.overlay.is_none() && !cn.raised)
                .unwrap_or(false)
        };
        let raised = |c: NodeId| {
            self.get(c)
                .map(|cn| cn.overlay.is_none() && cn.raised)
                .unwrap_or(false)
        };
        for &c in &n.children {
            if ordinary(c) {
                self.paint_node(canvas, c, origin, enabled);
            }
        }
        for &c in &n.children {
            if raised(c) {
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
    /// 控件请求打开的子窗口（同上，宿主待分发完全返回后才真正建窗）。
    /// 用 `Vec` 而非 `Option`：一次回调里连开两个窗是合法的，后者不该顶掉前者。
    open_windows: Vec<WindowRequest>,
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
    /// 打开一个新窗口（设置页、关于框这类独立子窗）。
    ///
    /// 请求排队，平台在事件分发**完全返回**后才真正建窗——在回调里直接建窗会同步派发
    /// `WM_NCCREATE`/`WM_SIZE` 等消息重入窗口过程，那里再取一次窗口状态就是 `&mut`
    /// 别名（AGENTS.md 铁律 6），与 `WindowOp`、`DialogRequest` 走同一条延后通道。
    ///
    /// 新窗口与打开它的窗口**共享主题句柄**：运行期换主题时所有窗口一起变。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// Element::button("设置…").on_click(|ctx| {
    ///     ctx.open_window(Window::new("设置", 560, 420).content(|| Element::label("设置项…")));
    /// });
    /// ```
    ///
    /// 同一个回调里连开多个窗是合法的，按调用顺序创建。
    pub fn open_window(&mut self, req: WindowRequest) {
        self.out.open_windows.push(req);
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
    /// 请求最大化窗口（已最大化则无操作）。
    ///
    /// 与 [`toggle_maximize`](Self::toggle_maximize) 的分工见 [`WindowOp::Maximize`]：
    /// 按钮用 toggle，"最大化"与"还原"并列的菜单项用这一对。
    pub fn maximize(&mut self) {
        self.out.window_op = Some(WindowOp::Maximize);
    }
    /// 请求从最大化 / 最小化还原（本就是常规态则无操作）。
    pub fn restore(&mut self) {
        self.out.window_op = Some(WindowOp::Restore);
    }
    /// 当前窗口的状态与能力快照（是否最大化、能否最大化/最小化）。
    ///
    /// 读的是宿主在本次分发前注入的线程局部，见 [`crate::event::window_state`]。
    pub fn window_state(&self) -> crate::event::WindowState {
        crate::event::window_state()
    }
    /// 在 `pos`（逻辑坐标）弹出窗口系统菜单（还原/最小化/最大化/关闭，按当前状态禁用）。
    ///
    /// 无边框窗口的标题栏右键**默认就会弹**，无需调用本方法。它是为默认之外的入口准备的：
    /// 标题栏左端的应用图标点一下弹菜单、自定义快捷键唤起等。
    pub fn show_system_menu(&mut self, pos: Point) {
        self.show_context_menu(pos, crate::event::system_menu_items());
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
    /// 控件请求打开的子窗口（宿主待事件分发完全返回后才真正建窗）。
    pub open_windows: Vec<WindowRequest>,
}

/// 命中点的归属：无边框窗口的 `WM_NCHITTEST` 据此在客户区 / 拖动区之间分流。
/// 判定见 [`Tree::hit_role`]。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HitRole {
    /// 交互控件——平台判 HTCLIENT，优先于缩放边框与拖动区。
    Interactive,
    /// 窗口拖动区——平台判 HTCAPTION，由系统接管这次按下。
    Drag,
    /// 普通客户区。
    Plain,
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
    /// **沿命中节点向祖先回溯**（与 [`Self::cursor_at`] 同一心智）：取最近一个能给出
    /// 提示的节点。没有这层回溯，凡是内部由多个节点拼成的**复合控件**——
    /// `Element::stepper` 的 `[−][输入框][+]` 是最典型的一个——命中永远落在子节点上，
    /// 挂在控件本身上的 `.tooltip(..)` 就再也读不到，链上去不报错也不生效。
    pub fn node_tooltip(&self, id: NodeId) -> Option<String> {
        for nid in self.ancestor_chain(id) {
            let Some(n) = self.get(nid) else { continue };
            // 控件动态提示优先：自绘图表按指针所在的数据点给文案，静态文本给不了
            // （见 [`Widget::tooltip`]）。返回 None 才回退到节点上设的静态文本。
            if let Some(dynamic) = n.widget.tooltip() {
                return Some(dynamic);
            }
            let Some(text) = n.tooltip.clone() else {
                continue;
            };
            // 截断判定按**持有这条提示的那个节点**算，不是按命中的子节点算。
            // 找到提示就到此为止：被自身判定压掉时不再往上找，否则一个没被截断的
            // Label 会转而弹出祖先容器的提示，看着像"弹错了别人的说明"。
            if n.widget.text_truncated() == Some(false) {
                return None;
            }
            return Some(text);
        }
        None
    }

    /// `pos`（逻辑坐标）是否落在交互控件上（可聚焦节点，如自定义标题栏的窗口按钮）。
    /// 平台据此在 `WM_NCHITTEST` 把控件区强制判为 HTCLIENT——优先于缩放边框，
    /// 使整个按钮都是客户区、普通鼠标移动全程覆盖，避免顶部缩放条夺走 hover。
    ///
    /// 沿父链判定（见 [`Tree::hit_role`]），故可聚焦容器的**整个子树**都算交互控件。
    /// 代价：无边框窗口里若有可聚焦容器（列表行、`clickable()` 卡片）贴着窗口边缘，
    /// 那一段边缘让不出 8px 缩放带。此前只是"裸露部分让不出、文字上让得出"的斑马纹，
    /// 一致化并未新增受影响区域；真要留出缩放带得让容器躲开边缘
    /// （参照 `core::scrollbar::WINDOW_EDGE_INSET` 的做法）。
    pub fn interactive_hit_at(&self, pos: Point) -> bool {
        let Some(hit) = self.hit_test(pos) else {
            return false;
        };
        self.hit_role(hit) == HitRole::Interactive
    }

    /// `pos`（逻辑坐标）是否落在窗口拖动区（自定义标题栏）。沿父链自内向外找最近的
    /// 裁决者：先遇到可聚焦控件则不拖动——交控件处理；先遇到 `window_drag` 才判拖动区。
    /// 走穿透遮罩的命中（见 [`Tree::hit_test_for_drag`]）：模态对话框弹出时标题栏
    /// 仍可拖窗，但窗口按钮照旧被遮罩屏蔽（`interactive_hit_at` 用的是普通命中）。
    pub fn drag_hit_at(&self, pos: Point) -> bool {
        let Some(hit) = self.hit_test_for_drag(pos) else {
            return false;
        };
        self.hit_role(hit) == HitRole::Drag
    }

    /// 命中落定后沿父链自内向外裁决归属，交互与拖动共用这一次遍历。
    ///
    /// 两侧必须同源：曾经"交互只看落定节点、拖动却沿父链"，于是
    /// `clickable()` 容器里套一个 `Label`（`Label` 不可聚焦、却 `hit_opaque`，命中就在
    /// 它那里落定）会被判成拖动区——事件分发认得这次点击，`WM_NCHITTEST` 却先答了
    /// HTCAPTION，客户区连 `WM_LBUTTONDOWN` 都收不到，表现为"标题栏上的文字入口点不动，
    /// 只有文字周围的空隙能点"。
    ///
    /// 取**最近**的裁决者而非"链上有没有"：可聚焦容器里再嵌拖动区（如整块可点的卡片
    /// 顶部留一条拖动条）时，内层的声明更具体，该赢。
    fn hit_role(&self, hit: NodeId) -> HitRole {
        for id in self.ancestor_chain(hit) {
            let Some(n) = self.get(id) else { continue };
            if n.widget.focusable() {
                return HitRole::Interactive;
            }
            if n.window_drag {
                return HitRole::Drag;
            }
        }
        HitRole::Plain
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
                // 前景角色进签名：`fg_role_signal` 是"布局不变但像素变了"的一类，与上面
                // `own_enabled`（置灰）同理——纯颜色变化不改任何 bounds，只靠几何签名会被
                // 判成"结构没变"而走局部重绘，那一帧只重画事件所在的节点，改了色的那行字
                // 保持旧色不动。折进来即自动升整窗，无需为它单开特例分支。
                n.style.effective_fg_role().hash(&mut h);
                let b = n.bounds;
                (b.x, b.y, b.w, b.h).hash(&mut h);
                // 绘制偏移/层级提升进签名：拖拽让位这类"布局不变但像素位移"的变化据此
                // 自动升级整窗重绘，无需为其单开特例分支；hover 重同步也随之生效。
                (n.offset.x, n.offset.y, n.raised).hash(&mut h);
            }
        }
        h.finish()
    }

    /// **退出交互**后重置控件的交互态（名字里只有 `hidden` 是历史遗留，禁用同样在管）：
    /// 从根遍历，按祖先链累积的可见性与启用态判定每个
    /// 节点是否仍参与交互，对**由参与变为不参与**者调 `Widget::reset_interaction`
    /// （清 hover/press、令补间瞬时落定）。
    ///
    /// 「被隐藏」与「被禁用」是同一个问题的两种形态——控件不再收事件，其状态就**冻结在
    /// 最后一刻**，故必须一并处理：
    /// - **被隐藏**：控件在按下/悬停态被隐藏（如关闭它所在的对话框），下次显示瞬间闪出旧态。
    /// - **被禁用**：[`Self::hit_node`] 并不看启用态（禁用节点照样当得成 hover target），而
    ///   [`Self::call_on_event`] 对禁用节点（含父链）**直接丢弃事件**。于是按钮在 hover 态
    ///   被禁用后，指针移开时那记 Leave 被丢掉，`state` 冻结在 `Hover`；等它重新启用，就带着
    ///   一个指针早已不在的高亮显示出来，非得再移进移出一次才消得掉。分页条的「上一页 /
    ///   下一页」正是这个形态：翻到首末页即禁用，而指针多半就停在刚点过的那枚按钮上。
    ///
    /// 注意：必须用**累积**值而非节点局部的 `effective_visible`/`own_enabled`——对话框关闭
    /// 只翻转对话框节点本身，其子节点（关闭按钮等）的局部值不变，仅靠局部判定会漏掉它们。
    ///
    /// 由宿主在结构签名变化时调用；[`Self::layout_signature`] 已含 `own_enabled`，故启用翻转
    /// 同样会触发这条路径。对齐 Flutter MouseTracker / Qt 模态弹出补发 leave 的做法。
    ///
    /// ⚠️ 新增分页器一类"到边界即禁用"的控件时，这条通路就是它们不残留高亮的依据。
    pub fn reset_hidden_interactions(&mut self) {
        if let Some(root) = self.root {
            self.reset_hidden_rec(root, true);
        }
    }

    fn reset_hidden_rec(&mut self, id: NodeId, parent_interactive: bool) {
        let (interactive, children, transitioned) = match self.get(id) {
            Some(n) => {
                let v = parent_interactive && n.effective_visible() && n.own_enabled();
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
            self.reset_hidden_rec(c, interactive);
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

    /// 把输入法合成串下发给节点（见 `Widget::set_preedit`）。
    /// 返回 true 表示节点存在且已通知（调用方据此判断是否需要重绘）。
    pub fn set_preedit(&mut self, id: NodeId, pe: &crate::event::Preedit) -> bool {
        let Some(n) = self.get_mut(id) else {
            return false;
        };
        n.widget.set_preedit(pe);
        true
    }

    /// 读节点当前选区（见 `Widget::selection_range`）。
    pub fn selection_of(&self, id: NodeId) -> Option<(usize, usize)> {
        self.get(id)?.widget.selection_range()
    }

    /// 读节点的已提交正文（见 `Widget::ime_text`）。
    pub fn ime_text_of(&self, id: NodeId) -> Option<String> {
        self.get(id)?.widget.ime_text()
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
        if let Some(hit) = self.hit_overlays(p, false) {
            return Some(hit);
        }
        let root = self.root?;
        self.hit_node(root, p, Point::new(0, 0), false)
    }

    /// 先于整棵树测试锚定浮层（见 [`Node::overlay`]）：**倒序**遍历登记表，与
    /// `Tree::paint` 的正序绘制互为镜像——最后画的那层最先命中，否则会出现
    /// 「画在上面却点不到」。
    fn hit_overlays(&self, p: Point, for_drag: bool) -> Option<NodeId> {
        for &id in self.overlays.iter().rev() {
            if !self.overlay_showing(id) {
                continue;
            }
            // 用 continue 而不是 `?`：`overlay_origin` 在浮层没有父节点时返回 None，
            // 拿 `?` 一并把整个 hit_overlays 报成 None，会连累后面登记的所有浮层一个都测不到。
            let Some(origin) = self.overlay_origin(id) else {
                continue;
            };
            if let Some(hit) = self.hit_node(id, p, origin, for_drag) {
                return Some(hit);
            }
        }
        None
    }

    /// 轻量关闭：在浮层**与其锚点**之外按下时收起浮层。返回是否关掉了至少一个。
    ///
    /// 锚点也要排除，否则「点触发器收起面板」会退化成无操作——核心先把它关掉，
    /// 触发器的 toggle 紧接着又把它打开。锚点上的开合一律交给触发器控件自己决定。
    pub(crate) fn dismiss_overlays_outside(&mut self, p: Point) -> bool {
        let mut closed = false;
        for id in self.overlays.clone() {
            if !self.overlay_showing(id) {
                continue;
            }
            // 浮层的 bounds 存的是相对父节点的坐标，`abs_bounds` 沿父链累加出来的
            // 正好就是它的绝对矩形——不必再算一遍（算两遍迟早口径分叉）。
            let inside_panel = self.abs_bounds(id).contains(p);
            let inside_anchor = self
                .get(id)
                .and_then(|n| n.parent)
                .is_some_and(|pa| self.abs_bounds(pa).contains(p));
            if inside_panel || inside_anchor {
                continue;
            }
            if let Some(sig) = self.get(id).and_then(|n| n.overlay) {
                if sig.get() {
                    sig.set(false);
                    closed = true;
                }
            }
        }
        closed
    }

    /// 收起最顶层（最后登记）的可见浮层。返回是否确实收起了一个。ESC 用。
    pub(crate) fn close_topmost_overlay(&mut self) -> bool {
        for id in self.overlays.clone().into_iter().rev() {
            if !self.overlay_showing(id) {
                continue;
            }
            if let Some(sig) = self.get(id).and_then(|n| n.overlay) {
                if sig.get() {
                    sig.set(false);
                    return true;
                }
            }
        }
        false
    }

    /// 拖动区专用命中：同 [`Tree::hit_test`]，但模态遮罩（`Widget::scrim_passthrough`）
    /// 不落定、继续穿透到下层兄弟。供 [`Tree::drag_hit_at`] 判断标题栏——对话框弹出后
    /// 遮罩覆盖全窗，普通命中会停在遮罩上，标题栏因此失去 HTCAPTION、拖不动窗口。
    fn hit_test_for_drag(&self, p: Point) -> Option<NodeId> {
        if let Some(hit) = self.hit_overlays(p, true) {
            return Some(hit);
        }
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
            //
            // 锚定浮层不在此列：它已在 `hit_overlays` 里先于整棵树测过。留在这里再测
            // 一次是有害的——浮层的绝对矩形常常落在锚点之外，父节点的 `abs.contains(p)`
            // 会先把它挡掉，反而让「浮层内点击」漏判成命中锚点。
            let child_origin = Point::new(abs.x, abs.y);
            let ordinary = |c: NodeId| {
                self.get(c)
                    .map(|cn| cn.overlay.is_none() && !cn.raised)
                    .unwrap_or(false)
            };
            let raised = |c: NodeId| {
                self.get(c)
                    .map(|cn| cn.overlay.is_none() && cn.raised)
                    .unwrap_or(false)
            };
            for &c in n.children.iter().rev() {
                if raised(c) {
                    if let Some(hit) = self.hit_node(c, p, child_origin, for_drag) {
                        return Some(hit);
                    }
                }
            }
            for &c in n.children.iter().rev() {
                if ordinary(c) {
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

        // 轻量关闭：浮层外按下即收起。**在主事件之前**做，这样同一次按下里「关掉旧
        // 浮层」与「命中新控件」都能发生（点 A 的面板外的 B 按钮，面板收起且 B 被按到）。
        // 关闭改变显隐 → 无法局部化，直接升整窗。
        if matches!(ev.kind, PointerKind::Down)
            && capture.is_none()
            && !self.overlays.is_empty()
            && self.dismiss_overlays_outside(ev.pos)
        {
            res.repaint = true;
            res.damage = res.damage.merge(DamageReq::Full);
        }

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
                res.open_windows.extend(o.open_windows);
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
            open_windows: o.open_windows,
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
            res.open_windows = o.open_windows;
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
            res.open_windows.extend(out.open_windows);
            break; // 命中一个拖放处理者即止
        }
        res
    }

    /// 设置焦点节点（清旧设新，返回是否变化）。
    /// 在给定的焦点顺序里找第一个声明了 [`Autofocus`] 的节点。
    ///
    /// 取交集而不是全树扫描：`order` 已经过滤掉了不可见、被禁用、被模态遮住的节点
    /// （见 [`Tree::focusable_order`]）。少了这一层，对话框弹着的那一帧就会把焦点
    /// 兑现给遮罩后面的输入框——键盘从此能打到用户看不见的地方。
    pub fn first_autofocus(&self, order: &[NodeId]) -> Option<(NodeId, Autofocus)> {
        order
            .iter()
            .find_map(|&id| self.get(id).and_then(|n| n.autofocus).map(|a| (id, a)))
    }

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

    // ---- 锚定浮层（Node::overlay）----

    /// 浮层树：一列三行，第二行挂一个 60×80 的浮层。返回 (tree, 行 id, 浮层 id)。
    fn popup_tree(open: crate::signal::Signal<bool>) -> (Tree, Vec<NodeId>, NodeId) {
        let panel = Element::col().width(60).height(80).bg(Color::WHITE);
        // 锚点是**自适应高的线性容器**：浮层若没被排除在 `visible_children` 之外，
        // 它 80px 的高度会撑高锚点、把第三行顶下去。锚点若用 leaf（Layout::None
        // 压根不排布子节点）或写死高度，这条断言就恒成立、什么也验不到。
        let tree = layout(
            Element::col()
                .width(200)
                .height(300)
                .child(Element::leaf().width(200).height(40).bg(Color::WHITE))
                .child(
                    Element::col()
                        .width(200)
                        .child(Element::leaf().width(200).height(40).bg(Color::WHITE))
                        .popup(open, panel),
                )
                .child(Element::leaf().width(200).height(40).bg(Color::WHITE)),
            200,
            300,
        );
        let kids = tree.get(tree.root.unwrap()).unwrap().children.clone();
        // 浮层是 `popup()` 最后挂上去的那个子节点（前面还有锚点自己的内容）。
        let popup = *tree.get(kids[1]).unwrap().children.last().unwrap();
        (tree, kids, popup)
    }

    #[test]
    fn overlay_is_excluded_from_parent_layout_flow() {
        // 浮层脱离布局流的判据不是"看不见"，而是**父容器和后续兄弟的几何完全不受
        // 影响**——它一旦被算进主轴长度，第三行就会被顶下去。展开与收起两态都要验，
        // 只测收起态会漏掉"展开后才把兄弟挤走"这类回归。
        let open = signal(false);
        let (tree, kids, _) = popup_tree(open);
        assert_eq!(tree.abs_bounds(kids[2]).y, 80, "收起态第三行应紧跟第二行");

        open.set(true);
        let (tree, kids, popup) = popup_tree(open);
        assert_eq!(tree.abs_bounds(kids[2]).y, 80, "展开后第三行不得被顶下去");
        assert_eq!(
            tree.abs_bounds(kids[1]).h,
            40,
            "锚点自身的高度不含浮层——它自适应高，浮层一旦参与布局这里就是 80+"
        );
        assert!(tree.abs_bounds(popup).h > 0, "浮层本身应被排布出尺寸");
    }

    #[test]
    fn overlay_anchors_below_and_hit_tests_above_later_siblings() {
        // 浮层挂在第二行（y=40..80）上，展开后应落在第三行（y=80..120）之上。
        // 若走普通子节点路径，第三行是**后画**的兄弟，会盖住它并抢走点击。
        let open = signal(true);
        let (tree, kids, popup) = popup_tree(open);
        let r = tree.abs_bounds(popup);
        assert_eq!((r.x, r.y), (0, 84), "应贴在锚点下方 OVERLAY_GAP 处");

        let p = Point::new(30, 100); // 同时落在第三行与浮层里
        assert!(
            tree.abs_bounds(kids[2]).contains(p),
            "本例前提：该点确实也落在第三行上"
        );
        assert_eq!(tree.hit_test(p), Some(popup), "浮层必须先于其后的兄弟命中");
        assert_eq!(tree.hit_test(Point::new(30, 20)), Some(kids[0]));
    }

    #[test]
    fn overlay_flips_above_anchor_when_it_would_overflow_bottom() {
        // 锚点贴着窗口底部时下方放不下，必须上翻——否则面板一半在窗口外，等于不可用。
        let open = signal(true);
        let panel = Element::col().width(60).height(80).bg(Color::WHITE);
        let tree = layout(
            Element::col()
                .width(200)
                .height(200)
                .child(Element::leaf().width(200).height(160))
                .child(
                    Element::leaf()
                        .width(200)
                        .height(40)
                        .bg(Color::WHITE)
                        .popup(open, panel),
                ),
            200,
            200,
        );
        let anchor = tree.get(tree.root.unwrap()).unwrap().children[1];
        let popup = tree.get(anchor).unwrap().children[0];
        let a = tree.abs_bounds(anchor);
        let r = tree.abs_bounds(popup);
        assert_eq!(a.y, 160);
        assert_eq!(r.y, 160 - 4 - 80, "下方放不下应翻到锚点上方");
        assert!(r.y >= 0);
    }

    #[test]
    fn pointer_down_outside_dismisses_overlay_but_anchor_click_does_not() {
        // 轻量关闭的两半必须一起成立：点别处收起，点锚点**不**收起（那一下归触发器
        // 自己 toggle）。少了后半条，"点触发器关面板"会变成关了又立刻开、看着没反应。
        let open = signal(true);
        let (mut tree, kids, _) = popup_tree(open);
        let (mut hover, mut capture) = (None, None);

        let down = |pos: Point| PointerEvent::single(PointerKind::Down, pos, MouseButton::Left);

        let res = tree.dispatch_pointer(down(Point::new(30, 50)), &mut hover, &mut capture);
        assert!(open.get(), "点在锚点上不应由核心收起");
        assert_eq!(res.damage, DamageReq::None, "没关掉浮层就不该平白升整窗");

        assert_eq!(tree.abs_bounds(kids[1]).y, 40);
        let res = tree.dispatch_pointer(down(Point::new(30, 10)), &mut hover, &mut capture);
        assert!(!open.get(), "点在浮层与锚点之外应收起");
        // 关闭改变显隐，局部脏区盖不住浮层腾出的那片区域——降成节点级脏区的话
        // 测试照样全绿，画面上却留着面板残影。故这里连脏区一起断言。
        assert_eq!(res.damage, DamageReq::Full, "收起浮层必须升整窗重绘");
        assert!(res.repaint);
    }

    #[test]
    fn pointer_down_inside_overlay_keeps_it_open() {
        // 面板内部的交互（拖色相条）绝不能顺手把面板关掉。
        let open = signal(true);
        let (mut tree, _, popup) = popup_tree(open);
        let (mut hover, mut capture) = (None, None);
        let r = tree.abs_bounds(popup);
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Down,
                Point::new(r.x + r.w / 2, r.y + r.h / 2),
                MouseButton::Left,
            ),
            &mut hover,
            &mut capture,
        );
        assert!(open.get(), "面板内按下应保持展开");
    }

    /// 只记录填充色顺序的画布。绘制序是"谁盖住谁"的唯一真相，而它在几何断言里
    /// 完全看不见——两个节点的矩形重叠时，命中测试可能是对的、画面却是反的。
    struct OrderCanvas(Vec<Color>);

    impl crate::render::Canvas for OrderCanvas {
        fn dpi_scale(&self) -> f32 {
            1.0
        }
        fn fill_rect(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, p: &Paint) {
            self.0.push(p.color);
        }
        fn fill_round_rect(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, _r: f32, p: &Paint) {
            self.0.push(p.color);
        }
        fn stroke_round_rect(
            &mut self,
            _x: f32,
            _y: f32,
            _w: f32,
            _h: f32,
            _r: f32,
            _lw: f32,
            _p: &Paint,
        ) {
        }
        fn draw_line(&mut self, _a: f32, _b: f32, _c: f32, _d: f32, _w: f32, _p: &Paint) {}
        fn fill_circle(&mut self, _cx: f32, _cy: f32, _r: f32, _p: &Paint) {}
        fn draw_shadow(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, _r: f32, _b: f32, _c: Color) {
        }
        fn draw_image(
            &mut self,
            _i: &crate::render::image::Image,
            _d: Rect,
            _f: crate::render::image::Fit,
            _r: f32,
            _o: f32,
        ) {
        }
        fn draw_text(
            &mut self,
            _t: &str,
            _r: Rect,
            _c: Color,
            _a: crate::spec::Align,
            _ts: &crate::text::TextStyle,
        ) {
        }
        fn measure_text(&mut self, _t: &str, _ts: &crate::text::TextStyle) -> Size {
            Size::ZERO
        }
        fn push_layer(&mut self, _o: f32) {}
        fn pop_layer(&mut self) {}
        fn save(&mut self) {}
        fn restore(&mut self) {}
        fn clip_rect(&mut self, _r: Rect) {}
    }

    #[test]
    fn overlay_painted_once_and_last() {
        // 浮层必须**恰好画一次**且排在所有普通节点之后。画两次（普通遍历里没跳过它）
        // 在静态画面上看不出破绽——同样的内容盖在同样的位置；但它会被排在后面的兄弟
        // 盖住，"浮层"就名存实亡。故这里同时断言次数与次序。
        let open = signal(true);
        let panel_bg = Color::hex(0x123456);
        let sibling_bg = Color::hex(0xABCDEF);
        let tree = layout(
            Element::col()
                .width(200)
                .height(300)
                .child(
                    Element::col()
                        .width(200)
                        .child(Element::leaf().width(200).height(40))
                        .popup(open, Element::col().width(60).height(80).bg(panel_bg)),
                )
                .child(Element::leaf().width(200).height(200).bg(sibling_bg)),
            200,
            300,
        );

        let mut canvas = OrderCanvas(Vec::new());
        tree.paint(&mut canvas);
        let seq = canvas.0;
        let panel_at: Vec<usize> = seq
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == panel_bg)
            .map(|(i, _)| i)
            .collect();
        let sib_at = seq.iter().position(|c| *c == sibling_bg);
        assert_eq!(panel_at.len(), 1, "浮层应恰好绘制一次，实得 {panel_at:?}");
        assert!(sib_at.is_some(), "本例前提：后续兄弟确实画了自己的底色");
        assert!(
            panel_at[0] > sib_at.unwrap(),
            "浮层必须画在后续兄弟之后：浮层 @{} vs 兄弟 @{}",
            panel_at[0],
            sib_at.unwrap()
        );
    }

    #[test]
    fn overlay_is_hidden_when_an_ancestor_is_hidden() {
        // 浮层从根级直接绘制/命中，不走父节点那趟递归，也就没人替它做"父不可见则
        // 整棵子树不画"。切走 Tab 页后仍浮在新页面上的 bug 正出在这里。
        let open = signal(true);
        let page = signal(true);
        let panel = Element::col().width(60).height(80).bg(Color::WHITE);
        let tree = layout(
            Element::col().width(200).height(300).child(
                Element::col().visible_signal(page).child(
                    Element::leaf()
                        .width(200)
                        .height(40)
                        .bg(Color::WHITE)
                        .popup(open, panel),
                ),
            ),
            200,
            300,
        );
        let hit_at = Point::new(30, 60);
        assert!(tree.hit_test(hit_at).is_some(), "页面可见时浮层应命中");

        page.set(false);
        assert_eq!(tree.hit_test(hit_at), None, "祖先隐藏后浮层不得再命中");
    }

    /// 浮层必须能**逃出滚动容器的裁剪**。
    ///
    /// 这是浮层最容易悄悄失效的地方，也是 `Node::raised` 根本做不到的事：滚动容器
    /// `clip_children` 会把子树剪到视口内，而取色器在设置页里正是长在滚动区里的——
    /// 面板一旦被剪，下半截就凭空消失，且**看不见**的那半截也点不着。
    #[test]
    fn overlay_escapes_an_ancestor_scroll_clip() {
        let open = signal(true);
        let panel_bg = Color::hex(0x123456);
        // 视口只有 100 高，锚点贴着视口底部：面板必然伸出视口之外。
        let tree = layout(
            Element::col().width(200).height(300).child(
                Element::scroll().width(200).height(100).child(
                    Element::col()
                        .width(200)
                        .child(Element::leaf().width(200).height(60))
                        .child(
                            Element::col()
                                .width(200)
                                .child(Element::leaf().width(200).height(30))
                                .popup(open, Element::col().width(60).height(80).bg(panel_bg)),
                        ),
                ),
            ),
            200,
            300,
        );

        // 根节点恒被拉伸到整窗，故滚动容器必须自己占一层，否则视口就是整个窗口、
        // 面板根本伸不出去，这条测试会在前提不成立上空转。
        let scroll = tree.get(tree.root.unwrap()).unwrap().children[0];
        let viewport = tree.abs_bounds(scroll);
        let col = tree.get(scroll).unwrap().children[0];
        let anchor = tree.get(col).unwrap().children[1];
        let popup = *tree.get(anchor).unwrap().children.last().unwrap();
        let pr = tree.abs_bounds(popup);
        assert!(
            pr.bottom() > viewport.bottom(),
            "本例前提：面板确实伸出了滚动视口（视口底 {}，面板底 {}）",
            viewport.bottom(),
            pr.bottom()
        );

        // ① 命中：视口之外的那半截仍要点得到。
        let outside = Point::new(pr.x + pr.w / 2, viewport.bottom() + 10);
        assert!(pr.contains(outside));
        assert_eq!(
            tree.hit_test(outside),
            Some(popup),
            "伸出视口的那半截浮层仍应命中"
        );

        // ② 绘制：不能被裁掉。裁剪不改变提交次数，只改变落到画布上的像素，故这里
        //    比对的是「有没有画」而非「画在哪」——OrderCanvas 忽略裁剪，若实现把浮层
        //    留在滚动子树里绘制，它就会在真实画布上被剪掉而这里仍记一次。为此改看
        //    绘制时的裁剪栈：浮层绘制期间不得处在滚动容器的裁剪之下。
        let mut canvas = ClipWatchCanvas {
            clips: Vec::new(),
            depth: 0,
            fills: Vec::new(),
        };
        tree.paint(&mut canvas);
        let hit = canvas
            .fills
            .iter()
            .find(|(c, _)| *c == panel_bg)
            .expect("浮层应当被绘制");
        assert_eq!(
            hit.1, 0,
            "浮层绘制时不得处在任何祖先裁剪之下（当前深度 {})",
            hit.1
        );
    }

    /// 记录每次填充时的裁剪栈深度。裁剪是"看不见的状态"，只有把它连同颜色一起记下来
    /// 才验得到"画了但被剪掉"这种失败——单看提交次数与几何都是对的。
    struct ClipWatchCanvas {
        clips: Vec<()>,
        depth: usize,
        fills: Vec<(Color, usize)>,
    }

    impl crate::render::Canvas for ClipWatchCanvas {
        fn dpi_scale(&self) -> f32 {
            1.0
        }
        fn fill_rect(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, p: &Paint) {
            self.fills.push((p.color, self.depth));
        }
        fn fill_round_rect(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, _r: f32, p: &Paint) {
            self.fills.push((p.color, self.depth));
        }
        fn stroke_round_rect(
            &mut self,
            _x: f32,
            _y: f32,
            _w: f32,
            _h: f32,
            _r: f32,
            _lw: f32,
            _p: &Paint,
        ) {
        }
        fn draw_line(&mut self, _a: f32, _b: f32, _c: f32, _d: f32, _w: f32, _p: &Paint) {}
        fn fill_circle(&mut self, _cx: f32, _cy: f32, _r: f32, _p: &Paint) {}
        fn draw_shadow(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, _r: f32, _b: f32, _c: Color) {
        }
        fn draw_image(
            &mut self,
            _i: &crate::render::image::Image,
            _d: Rect,
            _f: crate::render::image::Fit,
            _r: f32,
            _o: f32,
        ) {
        }
        fn draw_text(
            &mut self,
            _t: &str,
            _r: Rect,
            _c: Color,
            _a: crate::spec::Align,
            _ts: &crate::text::TextStyle,
        ) {
        }
        fn measure_text(&mut self, _t: &str, _ts: &crate::text::TextStyle) -> Size {
            Size::ZERO
        }
        fn push_layer(&mut self, _o: f32) {}
        fn pop_layer(&mut self) {}
        fn save(&mut self) {
            self.clips.push(());
        }
        fn restore(&mut self) {
            self.clips.pop();
            self.depth = self.clips.len();
        }
        fn clip_rect(&mut self, _r: Rect) {
            self.depth = self.clips.len();
        }
    }

    /// 对话框弹出后，长在它**外面**的浮层不得再抢命中，并且会被收起。
    ///
    /// 浮层无条件画在整棵树之后、命中先于整棵树，而遮罩只是树里的普通节点——不设闸
    /// 的话一个开着的取色面板会浮在对话框之上照常接点击，模态语义当场破掉。焦点那侧
    /// 早就按模态作用域裁过了（`focusable_order`），命中与绘制必须跟上。
    #[test]
    fn an_overlay_outside_the_dialog_is_shut_out_by_it() {
        let open = signal(true);
        let show_dialog = signal(false);
        let panel_bg = Color::hex(0x123456);
        let tree_of = |open, show_dialog| {
            layout(
                Element::stack()
                    .width(300)
                    .height(400)
                    .child(
                        Element::col().width(300).child(
                            Element::col()
                                .width(200)
                                .child(Element::leaf().width(200).height(30))
                                .popup(open, Element::col().width(80).height(90).bg(panel_bg)),
                        ),
                    )
                    .child(Element::dialog(
                        show_dialog,
                        Element::col().width(120).height(80).bg(Color::WHITE),
                    )),
                300,
                400,
            )
        };

        // 没有对话框时，浮层照常命中。
        let tree = tree_of(open, show_dialog);
        let col = tree.get(tree.root.unwrap()).unwrap().children[0];
        let anchor = tree.get(col).unwrap().children[0];
        let popup = *tree.get(anchor).unwrap().children.last().unwrap();
        let pr = tree.abs_bounds(popup);
        let inside = Point::new(pr.x + pr.w / 2, pr.y + pr.h / 2);
        assert_eq!(
            tree.hit_test(inside),
            Some(popup),
            "本例前提：无对话框时能命中浮层"
        );

        // 弹出对话框（不经指针，故走不到 `dismiss_overlays_outside` 那条自愈路径）。
        show_dialog.set(true);

        // ① **同一帧内**（还没来得及重新布局）就必须失效。对话框常由快捷键、
        //    on_submit、定时器弹出，那些路径与下一次 layout 之间隔着完整的一轮
        //    命中与绘制；只靠布局阶段收起浮层，这段窗口里它照样盖在对话框上。
        assert_ne!(
            tree.hit_test(inside),
            Some(popup),
            "对话框一弹出，域外浮层当帧就不得再抢走命中"
        );
        let mut canvas = OrderCanvas(Vec::new());
        tree.paint(&mut canvas);
        assert!(
            !canvas.0.contains(&panel_bg),
            "域外浮层当帧就不得画在对话框之上"
        );
        assert!(open.get(), "此刻还没重新布局，展开信号应当原样未动");

        // ② 下一次布局把它真正收起来——而不是隐着等对话框关掉再凭空冒出来。
        let tree = tree_of(open, show_dialog);
        assert!(!open.get(), "重新布局后域外浮层应被收起");
        assert_ne!(tree.hit_test(inside), Some(popup));
    }

    /// 反向：长在对话框**里面**的浮层必须照常工作。
    ///
    /// 设置类对话框里放取色器是常见写法，闸门若按「有对话框就一律关掉浮层」来写，
    /// 这条会立刻断掉，而只测上一条是发现不了的。
    #[test]
    fn an_overlay_inside_the_dialog_still_works() {
        let open = signal(true);
        let show_dialog = signal(true);
        let panel_bg = Color::hex(0x654321);
        let tree = layout(
            Element::stack()
                .width(300)
                .height(400)
                .child(Element::dialog(
                    show_dialog,
                    Element::col().width(200).bg(Color::WHITE).child(
                        Element::col()
                            .width(180)
                            .child(Element::leaf().width(180).height(30))
                            .popup(open, Element::col().width(80).height(90).bg(panel_bg)),
                    ),
                )),
            300,
            400,
        );
        // 从遮罩往下找到那个浮层节点。
        fn find_overlay(tree: &Tree, id: NodeId) -> Option<NodeId> {
            let n = tree.get(id)?;
            if n.overlay.is_some() {
                return Some(id);
            }
            n.children.iter().find_map(|&c| find_overlay(tree, c))
        }
        let popup = find_overlay(&tree, tree.root.unwrap()).expect("应找得到浮层节点");
        assert!(open.get(), "对话框内的浮层不该被闸门收起");
        let pr = tree.abs_bounds(popup);
        assert!(!pr.is_empty(), "对话框内的浮层应被正常排布");
        assert_eq!(
            tree.hit_test(Point::new(pr.x + pr.w / 2, pr.y + pr.h / 2)),
            Some(popup),
            "对话框内的浮层应照常命中"
        );
    }

    /// `close_topmost_overlay` 在纯浮层树上的返回值与幂等性。
    ///
    /// **它不覆盖「ESC 排在对话框之前」**——那条排序在 `UiHost::resolve_close_inner`
    /// 里，由 `escape_closes_the_overlay_before_the_dialog_that_hosts_it` 守（在
    /// `src/app` 的测试里）。这个名字此前写成 `..._before_dialogs`，而用例里连一个
    /// 对话框都没有。
    /// 锚点被滚出视口后浮层要收起。
    ///
    /// 浮层刻意不受祖先裁剪（那正是 `overlay_escapes_an_ancestor_scroll_clip` 要的），
    /// 代价就是锚点滚没了它还浮着，且位置被钳在窗口边缘、与任何东西都不再相关。
    /// 滚轮不走轻量关闭（那只挂在 Down 上），必须由布局阶段兜住。
    #[test]
    fn scrolling_the_anchor_out_of_view_closes_the_overlay() {
        let open = signal(true);
        let mut tree = layout(
            Element::col().width(200).height(300).child(
                Element::scroll().width(200).height(100).child(
                    Element::col()
                        .width(200)
                        .child(Element::leaf().width(200).height(60))
                        .child(
                            Element::col()
                                .width(200)
                                .child(Element::leaf().width(200).height(30))
                                .popup(open, Element::col().width(60).height(80).bg(Color::WHITE)),
                        )
                        .child(Element::leaf().width(200).height(400)),
                ),
            ),
            200,
            300,
        );
        let scroll = tree.get(tree.root.unwrap()).unwrap().children[0];
        assert!(open.get(), "本例前提：初始时锚点在视口内、浮层开着");

        // 滚到锚点（y≈60..90）完全离开 100 高的视口之外。
        assert!(tree.set_scroll_y(scroll, 200), "本例前提：这块内容确实可滚");
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 300), &mut te);
        assert!(!open.get(), "锚点滚出视口后浮层应被收起");
    }

    #[test]
    fn close_topmost_overlay_is_idempotent() {
        let open = signal(true);
        let (mut tree, _, _) = popup_tree(open);
        assert!(tree.close_topmost_overlay());
        assert!(!open.get());
        assert!(!tree.close_topmost_overlay(), "已收起后不应再报关掉了一个");
    }

    #[test]
    fn removed_overlay_deregisters_so_a_recycled_slot_is_not_treated_as_one() {
        // arena 槽位会被复用。浮层登记若不随节点回收，后来占用同一槽位的普通节点会
        // 凭空继承"我是浮层"的身份——代际校验挡得住 `get`，挡不住这份 id 列表本身。
        let open = signal(true);
        let (mut tree, kids, popup) = popup_tree(open);
        assert_eq!(tree.overlays.len(), 1);
        tree.remove(popup);
        assert!(tree.overlays.is_empty(), "移除后登记应一并清掉");

        let fresh = Element::leaf().width(10).height(10).build(&mut tree);
        tree.add_child(kids[1], fresh);
        assert_eq!(fresh.index, popup.index, "本例前提：新节点复用了浮层的槽位");
        assert!(tree.overlays.is_empty(), "复用槽位的新节点不得被当成浮层");
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

    /// 记录 `reset_interaction` 被调次数的探针控件。
    struct ResetProbe(Rc<std::cell::Cell<usize>>);
    impl Widget for ResetProbe {
        fn reset_interaction(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// 控件**被禁用**时必须复位交互态，与「被隐藏」同等对待。
    ///
    /// 不复位时错在哪：`hit_node` 不看启用态，禁用节点照样是 hover target；而
    /// `call_on_event` 对禁用节点直接丢事件——指针移开时那记 Leave 被丢掉，控件的
    /// hover/press 就冻结在最后一刻，等它重新启用便带着一个指针早已不在的高亮出现。
    /// 分页条的「上一页/下一页」正是这形态（翻到首末页即禁用）。
    #[test]
    fn disabling_a_node_resets_its_interaction() {
        let hits = Rc::new(std::cell::Cell::new(0usize));
        let on = signal(true);
        let mut tree = layout(
            Element::col().child(
                Element::leaf()
                    .width(50)
                    .height(20)
                    .widget(ResetProbe(hits.clone()))
                    .enabled_signal(on),
            ),
            100,
            100,
        );

        // 建立基线（prev_visible = true），不该复位。
        tree.reset_hidden_interactions();
        assert_eq!(hits.get(), 0);

        on.set(false);
        tree.reset_hidden_interactions();
        assert_eq!(hits.get(), 1, "启用 → 禁用应复位交互态");

        // 只在**退出**交互那一刻复位；回到启用不重复触发（否则每次翻页都白跑一趟）。
        on.set(true);
        tree.reset_hidden_interactions();
        assert_eq!(hits.get(), 1, "禁用 → 启用不该再复位");
    }

    /// 父链禁用同样要复位子节点——判据必须是**累积**启用态。
    /// 只看局部 `own_enabled` 的话，禁用容器时内部控件的局部值没变，会被整片漏掉。
    #[test]
    fn disabling_a_container_resets_children() {
        let hits = Rc::new(std::cell::Cell::new(0usize));
        let on = signal(true);
        let mut tree = layout(
            Element::col().enabled_signal(on).child(
                Element::leaf()
                    .width(50)
                    .height(20)
                    .widget(ResetProbe(hits.clone())),
            ),
            100,
            100,
        );
        tree.reset_hidden_interactions();
        on.set(false);
        tree.reset_hidden_interactions();
        assert_eq!(hits.get(), 1, "父链禁用要传导到子节点");
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

    /// `Element::tabs_items` 是 `TabItem::enabled` 唯一的公开可达路径——三个便捷
    /// 构造器都在内部自己造项，收 `Vec<TabItem>` 的 `tabs_frame` 是私有的。
    /// 本例走完整分发链路验证这条路真的把禁用态带到了标签条上。
    ///
    /// **两棵树对照**是必须的：只断言「禁用项点不动」的话，一条彻底断掉的路径
    /// （比如 `tabs_items` 压根没把 items 传下去）也会照样通过。
    #[test]
    fn tabs_items_carries_per_item_enabled_through_dispatch() {
        use crate::ui::containers::{TabItem, TabStyle};

        fn click_first_tab(first_enabled: bool) -> Signal<usize> {
            let sel = signal(1);
            let root = Element::tabs_items(
                sel,
                vec![
                    (
                        TabItem::new("甲".into()).enabled(signal(first_enabled)),
                        Element::label("page A"),
                    ),
                    (TabItem::new("乙".into()), Element::label("page B")),
                ],
                TabStyle::Underline,
            );
            let mut tree = Tree::new();
            let id = root.build(&mut tree);
            tree.root = Some(id);
            let mut te = crate::text::NullTextEngine;
            tree.layout_root(Size::new(400, 300), &mut te);

            let bar = tree.get(id).unwrap().children[0];
            let b = tree.abs_bounds(bar);
            let p = Point::new(b.x + 2, b.y + b.h / 2);
            let (mut hover, mut cap) = (None, None);
            tree.dispatch_pointer(ptr(PointerKind::Move, p), &mut hover, &mut cap);
            tree.dispatch_pointer(ptr(PointerKind::Down, p), &mut hover, &mut cap);
            tree.dispatch_pointer(ptr(PointerKind::Up, p), &mut hover, &mut cap);
            sel
        }

        assert_eq!(
            click_first_tab(true).get(),
            0,
            "首项可选时，点它应切到 0——否则说明 tabs_items 这条路本身就是断的"
        );
        assert_eq!(
            click_first_tab(false).get(),
            1,
            "首项禁用时点击应无效，选中索引留在原处"
        );
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

    /// 转发到内层画布并计数的测试画布：`cull` 可控，用来对比"剪枝 / 不剪枝"两次绘制。
    struct CountingCanvas<'a, 'b> {
        inner: &'a mut crate::render::SkiaCanvas<'b>,
        cull: Option<Rect>,
        texts: usize,
        strokes: usize,
        fills: usize,
    }

    impl crate::render::Canvas for CountingCanvas<'_, '_> {
        fn dpi_scale(&self) -> f32 {
            self.inner.dpi_scale()
        }
        fn cull_rect(&self) -> Option<Rect> {
            self.cull
        }
        fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, p: &crate::render::Paint) {
            self.fills += 1;
            self.inner.fill_rect(x, y, w, h, p);
        }
        fn fill_round_rect(
            &mut self,
            x: f32,
            y: f32,
            w: f32,
            h: f32,
            r: f32,
            p: &crate::render::Paint,
        ) {
            self.fills += 1;
            self.inner.fill_round_rect(x, y, w, h, r, p);
        }
        fn stroke_round_rect(
            &mut self,
            x: f32,
            y: f32,
            w: f32,
            h: f32,
            r: f32,
            width: f32,
            p: &crate::render::Paint,
        ) {
            self.strokes += 1;
            self.inner.stroke_round_rect(x, y, w, h, r, width, p);
        }
        fn draw_line(
            &mut self,
            x0: f32,
            y0: f32,
            x1: f32,
            y1: f32,
            w: f32,
            p: &crate::render::Paint,
        ) {
            self.strokes += 1;
            self.inner.draw_line(x0, y0, x1, y1, w, p);
        }
        fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, p: &crate::render::Paint) {
            self.fills += 1;
            self.inner.fill_circle(cx, cy, r, p);
        }
        fn draw_shadow(
            &mut self,
            x: f32,
            y: f32,
            w: f32,
            h: f32,
            r: f32,
            blur: f32,
            c: crate::geometry::Color,
        ) {
            self.inner.draw_shadow(x, y, w, h, r, blur, c);
        }
        fn draw_image(
            &mut self,
            img: &crate::render::image::Image,
            dst: Rect,
            fit: crate::render::image::Fit,
            radius: f32,
            opacity: f32,
        ) {
            self.inner.draw_image(img, dst, fit, radius, opacity);
        }
        fn draw_text(
            &mut self,
            text: &str,
            rect: Rect,
            color: crate::geometry::Color,
            align: crate::spec::Align,
            ts: &crate::text::TextStyle,
        ) {
            self.texts += 1;
            self.inner.draw_text(text, rect, color, align, ts);
        }
        fn measure_text(
            &mut self,
            text: &str,
            ts: &crate::text::TextStyle,
        ) -> crate::geometry::Size {
            self.inner.measure_text(text, ts)
        }
        fn push_layer(&mut self, opacity: f32) {
            self.inner.push_layer(opacity);
        }
        fn pop_layer(&mut self) {
            self.inner.pop_layer();
        }
        fn save(&mut self) {
            self.inner.save();
        }
        fn restore(&mut self) {
            self.inner.restore();
        }
        fn clip_rect(&mut self, r: Rect) {
            self.inner.clip_rect(r);
        }
    }

    /// 局部帧的节点剪枝：**画面必须逐像素等同于不剪枝**，同时确实省掉了图元提交。
    ///
    /// 剪枝掉的图元本来就会被光栅器按子 pixmap 边界丢弃，所以两次绘制的像素必然相同；
    /// 这条测试锁住的是"相同"——一旦剪枝判据比实际绘制范围收得更紧（漏算焦点环、投影
    /// 之类的框外余量），差异会立刻在这里暴露，而不是等到某个动画在真机上缺一块。
    #[test]
    fn partial_frame_culling_preserves_pixels_and_skips_primitives() {
        let mut root = Element::col().width(240).height(400).padding(8).spacing(4);
        for i in 0..20 {
            root = root.child(
                Element::row()
                    .width_match()
                    .height(16)
                    .child(Element::label(format!("行 {i}")).weight(1.0))
                    .child(Element::button("op").outline()),
            );
        }
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(240, 400), &mut te);

        // 模拟局部帧：脏区大小的子 pixmap，原点对应世界 (8, 40)。
        let dirty = Rect::new(8, 40, 40, 24);
        let render = |cull: Option<Rect>| -> (Vec<u8>, usize, usize, usize) {
            let mut pm = tiny_skia::Pixmap::new(dirty.w as u32, dirty.h as u32).unwrap();
            let mut eng = crate::text::NullTextEngine;
            let (t, st, f);
            {
                let mut inner = crate::render::SkiaCanvas::with_text_offset(
                    &mut pm,
                    &mut eng,
                    1.0,
                    Point::new(dirty.x, dirty.y),
                );
                let mut c = CountingCanvas {
                    inner: &mut inner,
                    cull,
                    texts: 0,
                    strokes: 0,
                    fills: 0,
                };
                tree.paint(&mut c);
                (t, st, f) = (c.texts, c.strokes, c.fills);
            }
            (pm.data().to_vec(), t, st, f)
        };

        let (px_off, t_off, s_off, f_off) = render(None); // 不剪枝：老行为
        let (px_on, t_on, s_on, f_on) = render(Some(dirty)); // 剪枝
        assert_eq!(px_on, px_off, "剪枝不得改变任何一个像素");
        assert!(
            t_on < t_off && s_on < s_off && f_on < f_off,
            "剪枝应真的省掉图元：text {t_on}/{t_off} stroke {s_on}/{s_off} fill {f_on}/{f_off}"
        );
        // 40x24 的脏区只压到两三行，绝大多数控件应被跳过。
        assert!(
            t_on * 4 < t_off,
            "脏区只覆盖一小片，文字提交应大幅减少：{t_on}/{t_off}"
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
        // 必须成对收发：按下会捕获指针，不补 Up 的话后续事件全被锁在同一个按钮上
        // ——± 现在是两个独立节点，不像旧的自绘版那样每次按下都按 x 重算左右区。
        let mut click = |p: Point, tree: &mut Tree| {
            tree.dispatch_pointer(ptr(PointerKind::Down, p), &mut h, &mut cap);
            tree.dispatch_pointer(ptr(PointerKind::Up, p), &mut h, &mut cap);
        };
        // + → 3（达上限）
        click(plus, &mut tree);
        assert_eq!(v.get(), 3.0);
        // 再 + 钳制在 3
        click(plus, &mut tree);
        assert_eq!(v.get(), 3.0, "上限钳制");
        // − 三次到 0 并钳制
        for _ in 0..4 {
            click(minus, &mut tree);
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

    // ── Stepper 中部是真正的 TextInput ────────────────────────────────────────
    //
    // 旧实现自绘那份文本只会逐字符增删 + 左右移光标：选不中、复制不了、粘不进去。
    // 下面这组盯的就是「换成文本控件之后，那些能力确实到位了」，以及数值语义
    //（范围钳制、格式化）没有因此走丢。

    /// 建一棵只含 stepper 的树，返回 `(树, 中部数值框节点, value 信号)`。
    fn stepper_tree(init: f64, min: f64, max: f64, step: f64) -> (Tree, NodeId, Signal<f64>) {
        let v = signal(init);
        let root = Element::col()
            .width(120)
            .height(40)
            .child(Element::stepper(v, min, max, step).width(120));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        tree.layout_root(Size::new(120, 40), &mut crate::text::NullTextEngine);
        let row = tree.get(id).unwrap().children[0];
        // 行的三个子节点：[− 按钮, 数值框, + 按钮]。
        let field = tree.get(row).unwrap().children[1];
        (tree, field, v)
    }

    /// 重排一次——`value` ↔ `text` 的同步挂在 `on_update` 上，只在布局前跑。
    fn stepper_sync(tree: &mut Tree) {
        tree.layout_root(Size::new(120, 40), &mut crate::text::NullTextEngine);
    }

    fn skey(key: Key, ctrl: bool) -> KeyEvent {
        KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl,
        }
    }

    /// 全选 + 复制，返回剪贴板内容——即"用户此刻在框里看到的那串字"。
    fn stepper_copy(tree: &mut Tree, field: NodeId, clip: &Rc<RefCell<String>>) -> String {
        tree.dispatch_key(skey(Key::Other(0x41), true), Some(field)); // Ctrl+A
        tree.dispatch_key(skey(Key::Other(0x43), true), Some(field)); // Ctrl+C
        clip.borrow().clone()
    }

    #[test]
    fn stepper_value_is_selectable_and_copyable() {
        let clip = Rc::new(RefCell::new(String::new()));
        let (mut tree, field, _v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        assert_eq!(
            stepper_copy(&mut tree, field, &clip),
            "5",
            "全选+复制应把当前数值取出来（旧的自绘文本根本选不中）"
        );
    }

    #[test]
    fn stepper_typing_rewrites_value() {
        let (mut tree, field, v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        tree.dispatch_key(skey(Key::Other(0x41), true), Some(field)); // 全选
        tree.dispatch_key(skey(Key::Char('7'), false), Some(field)); // 整体替换
        stepper_sync(&mut tree);
        assert_eq!(v.get(), 7.0, "键入的数字应回写到绑定信号");
    }

    /// 非数字**打不进也粘不进**——键入与粘贴共用同一把尺子。
    #[test]
    fn stepper_rejects_non_numeric_input() {
        let clip = Rc::new(RefCell::new(String::from("abc")));
        let (mut tree, field, v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));

        tree.dispatch_key(skey(Key::Char('a'), false), Some(field));
        stepper_sync(&mut tree);
        assert_eq!(v.get(), 5.0, "字母键入不得改值");

        tree.dispatch_key(skey(Key::Other(0x41), true), Some(field)); // 全选
        tree.dispatch_key(skey(Key::Other(0x56), true), Some(field)); // 粘贴 "abc"
        stepper_sync(&mut tree);
        assert_eq!(v.get(), 5.0, "非数字粘贴不得改值");
        assert_eq!(
            stepper_copy(&mut tree, field, &clip),
            "5",
            "被拒的输入不得留在框里（整串丢弃，不做'挑能进的字符'）"
        );
    }

    #[test]
    fn stepper_arrow_keys_step_and_clamp() {
        let (mut tree, field, v) = stepper_tree(8.0, 1.0, 9.0, 1.0);
        tree.dispatch_key(skey(Key::Up, false), Some(field));
        assert_eq!(v.get(), 9.0);
        tree.dispatch_key(skey(Key::Up, false), Some(field));
        assert_eq!(v.get(), 9.0, "到上限后不再增");
        tree.dispatch_key(skey(Key::Down, false), Some(field));
        assert_eq!(v.get(), 8.0);
    }

    /// 越界的键入在**失焦提交**时钳回范围，并把文本规整成标准写法。
    ///
    /// 提交时机只有 paint 能感知（`focused` 参数），故这条必须真画一帧。
    #[test]
    fn stepper_commits_and_clamps_on_blur() {
        let clip = Rc::new(RefCell::new(String::new()));
        let (mut tree, field, v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        let mut pm = tiny_skia::Pixmap::new(120, 40).unwrap();
        let mut paint_once = |tree: &Tree| {
            let mut eng = crate::text::NullTextEngine;
            let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            tree.paint(&mut canvas);
        };

        tree.set_focused(Some(field), None);
        paint_once(&tree);

        tree.dispatch_key(skey(Key::Other(0x41), true), Some(field)); // 全选
        for c in ['9', '9'] {
            tree.dispatch_key(skey(Key::Char(c), false), Some(field));
        }
        stepper_sync(&mut tree);
        assert_eq!(v.get(), 9.0, "编辑途中就把值钳在范围内");

        tree.set_focused(None, Some(field));
        paint_once(&tree);
        assert_eq!(
            stepper_copy(&mut tree, field, &clip),
            "9",
            "失焦提交后框里应是钳制后的标准写法，而不是打进去的 99"
        );
    }

    /// 外部写 `value`（别处的按钮、恢复默认…）要反映到框里。
    #[test]
    fn stepper_external_value_write_updates_text() {
        let clip = Rc::new(RefCell::new(String::new()));
        let (mut tree, field, v) = stepper_tree(5.0, 0.0, 3.0, 0.25);
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        assert_eq!(
            stepper_copy(&mut tree, field, &clip),
            "3.00",
            "越界初值应先钳进范围，并按步长推断的小数位显示"
        );
        v.set(1.5);
        stepper_sync(&mut tree);
        assert_eq!(stepper_copy(&mut tree, field, &clip), "1.50");
    }

    /// 点击落点要按**居中后**的文字算。
    ///
    /// 数值是居中绘制的，而命中测试走的是另一条路径——绘制那边加了居中偏移、命中这边
    /// 忘了减，两边就整体错开半个空白宽度：点第一个数字会把光标放到第二个后面，
    /// 拖选出来的也是错位的一段。而且屏幕上一切正常，只有真去点才暴露。
    #[test]
    fn stepper_click_maps_to_the_centered_text() {
        let clip = Rc::new(RefCell::new(String::new()));
        let (mut tree, field, _v) = stepper_tree(123.0, 0.0, 999.0, 1.0);
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        // 居中偏移是 paint 算出来的，命中依赖它，故先真画一帧。
        {
            let mut pm = tiny_skia::Pixmap::new(120, 40).unwrap();
            let mut eng = crate::text::NullTextEngine;
            let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            tree.paint(&mut canvas);
        }
        // 落点在文字左侧的空白里 → 光标应停在串首。
        let b = tree.abs_bounds(field);
        let p = Point::new(b.x + 20, b.y + b.h / 2);
        let (mut h, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, p), &mut h, &mut cap);
        tree.dispatch_pointer(ptr(PointerKind::Up, p), &mut h, &mut cap);
        tree.dispatch_key(
            KeyEvent {
                key: Key::End,
                pressed: true,
                shift: true,
                ctrl: false,
            },
            Some(field),
        );
        tree.dispatch_key(skey(Key::Other(0x43), true), Some(field)); // Ctrl+C
        assert_eq!(
            &*clip.borrow(),
            "123",
            "点在数值左边再 Shift+End，应选中整串；只选到一部分说明命中没吃居中偏移"
        );
    }

    /// ± 按钮写完，框里的数字**当场**就得是新的——不能等下一次 `on_update`。
    ///
    /// 长按的重复步进跑在 `paint` 里，那条路不置 `needs_relayout`，下一帧的
    /// `layout_root` 整个会被跳过、`on_update` 自然也不跑。若按钮只写 `value` 而把
    /// 文本留给 `on_update` 排，长按期间数字会一直定在原地，松手那次点击才跳到终值。
    /// 故这里刻意**不调 `stepper_sync`**：中间隔一次重排，这条测试就测不出东西了。
    #[test]
    fn stepper_button_updates_text_without_waiting_for_relayout() {
        let clip = Rc::new(RefCell::new(String::new()));
        let (mut tree, field, v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        let row = tree.get(tree.root.unwrap()).unwrap().children[0];
        let rb = tree.abs_bounds(row);
        let plus = Point::new(rb.right() - 5, rb.y + rb.h / 2);
        let (mut h, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, plus), &mut h, &mut cap);
        tree.dispatch_pointer(ptr(PointerKind::Up, plus), &mut h, &mut cap);
        assert_eq!(v.get(), 6.0);
        assert_eq!(
            stepper_copy(&mut tree, field, &clip),
            "6",
            "按钮写完文本应当场更新，不依赖后续重排"
        );
    }

    /// Escape 撤销的是**本轮键入**，不是"聚焦以来发生的一切"。
    ///
    /// 键入会实时回写 `value`（绑定信号随打字更新），所以 Escape 必须能把值退回去；
    /// 而 ± 按钮和方向键的步进是当场落地的动作，退回时不该被一起吃掉——
    /// 聚焦 → 点两次 + → 打错一个字 → Escape，用户要的是撤销那个字。
    #[test]
    fn stepper_escape_undoes_typing_but_keeps_steps() {
        let clip = Rc::new(RefCell::new(String::new()));
        let (mut tree, field, v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        {
            let mut pm = tiny_skia::Pixmap::new(120, 40).unwrap();
            let mut eng = crate::text::NullTextEngine;
            let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            tree.set_focused(Some(field), None);
            tree.paint(&mut canvas); // 聚焦这一帧记下基线 5
        }
        // 点两次 +（左右按钮分列行首行尾，成对收发指针）。
        let row = tree.get(tree.root.unwrap()).unwrap().children[0];
        let rb = tree.abs_bounds(row);
        let plus = Point::new(rb.right() - 5, rb.y + rb.h / 2);
        let (mut h, mut cap) = (None, None);
        for _ in 0..2 {
            tree.dispatch_pointer(ptr(PointerKind::Down, plus), &mut h, &mut cap);
            tree.dispatch_pointer(ptr(PointerKind::Up, plus), &mut h, &mut cap);
        }
        assert_eq!(v.get(), 7.0, "两次 + 应到 7");

        // 再打错一个字：全选后键入 2。
        tree.dispatch_key(skey(Key::Other(0x41), true), Some(field));
        tree.dispatch_key(skey(Key::Char('2'), false), Some(field));
        stepper_sync(&mut tree);
        assert_eq!(v.get(), 2.0, "键入实时回写");

        tree.dispatch_key(skey(Key::Escape, false), Some(field));
        assert_eq!(v.get(), 7.0, "Escape 只撤销键入，不该退回两次 + 之前的 5");
        assert_eq!(stepper_copy(&mut tree, field, &clip), "7");
    }

    /// Enter 是一次提交，Escape 不该越过它退到更早的地方。
    ///
    /// 与上一条同源：`edit_origin` 的前移时机不能只有「获焦」和「步进」，
    /// 「Enter 定了一次」同样是当场落地。
    #[test]
    fn stepper_escape_stops_at_the_last_enter() {
        let (mut tree, field, v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        {
            let mut pm = tiny_skia::Pixmap::new(120, 40).unwrap();
            let mut eng = crate::text::NullTextEngine;
            let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            tree.set_focused(Some(field), None);
            tree.paint(&mut canvas); // 基线 = 5
        }
        tree.dispatch_key(skey(Key::Other(0x41), true), Some(field));
        tree.dispatch_key(skey(Key::Char('3'), false), Some(field));
        stepper_sync(&mut tree);
        tree.dispatch_key(skey(Key::Enter, false), Some(field)); // 提交 3

        tree.dispatch_key(skey(Key::Other(0x41), true), Some(field));
        tree.dispatch_key(skey(Key::Char('7'), false), Some(field));
        stepper_sync(&mut tree);
        tree.dispatch_key(skey(Key::Escape, false), Some(field));
        assert_eq!(v.get(), 3.0, "应退回上次 Enter 提交的 3，而不是聚焦时的 5");
    }

    /// 没有未提交改动时，Enter 要放行冒泡——否则对话框的默认按钮会被数字框吃掉。
    #[test]
    fn stepper_enter_bubbles_when_nothing_to_commit() {
        let (mut tree, field, _v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        let clean = tree.dispatch_key(skey(Key::Enter, false), Some(field));
        assert!(
            !clean.consumed,
            "什么都没改就按回车，Enter 应冒泡出去（对话框「确定」还得能按）"
        );
        tree.dispatch_key(skey(Key::Other(0x41), true), Some(field));
        tree.dispatch_key(skey(Key::Char('8'), false), Some(field));
        let dirty = tree.dispatch_key(skey(Key::Enter, false), Some(field));
        assert!(dirty.consumed, "有未提交改动时 Enter 是提交，应被消费");
    }

    /// **禁用期间**外部写 `value`，框里也得跟着变。
    ///
    /// 这条盯的是一个只在复合化之后才可能出现的回归：同步全挂在 `on_update` 上，而
    /// `Tree::call_on_update` 开头就把禁用节点整个跳过了——于是置灰的数字框会一直
    /// 停在禁用那一刻的旧数字，重新启用才跳到新值。旧的自绘实现每帧现读 `value`，
    /// 没有这个问题。兜底做在 `NumberField::paint` 里，故这条必须真画一帧。
    #[test]
    fn stepper_disabled_still_follows_external_value() {
        let v = signal(5.0f64);
        let root = Element::col()
            .width(120)
            .height(40)
            .child(Element::stepper(v, 1.0, 9.0, 1.0).width(120).disabled(true));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        tree.layout_root(Size::new(120, 40), &mut crate::text::NullTextEngine);
        let row = tree.get(id).unwrap().children[0];
        let field = tree.get(row).unwrap().children[1];

        let mut pm = tiny_skia::Pixmap::new(120, 40).unwrap();
        let mut paint_once = |tree: &Tree| {
            let mut eng = crate::text::NullTextEngine;
            let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            tree.paint(&mut canvas);
        };
        paint_once(&tree);
        assert_eq!(tree.ime_text_of(field).as_deref(), Some("5"));

        v.set(8.0);
        tree.layout_root(Size::new(120, 40), &mut crate::text::NullTextEngine);
        paint_once(&tree);
        assert_eq!(
            tree.ime_text_of(field).as_deref(),
            Some("8"),
            "置灰期间外部改值，框里仍须显示新值"
        );
    }

    /// 只重绘、不重排的帧里，`value` 变化也得跟上。
    ///
    /// 典型场景是同一个 `Signal<f64>` 同时绑 `slider` 和 `stepper`：拖滑块全程是指针
    /// `Move`，而 `Move` 刻意不置 `needs_relayout`（hover 高频），于是 `layout_root`
    /// 不跑、`on_update` 也不跑。同步只挂在 `on_update` 上的话，数字会一路纹丝不动，
    /// 直到松手才一次性跳到位。故这条**只 paint、不 layout**。
    #[test]
    fn stepper_follows_value_on_repaint_only_frames() {
        let (tree, field, v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        let mut pm = tiny_skia::Pixmap::new(120, 40).unwrap();
        let mut paint_once = |tree: &Tree| {
            let mut eng = crate::text::NullTextEngine;
            let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            tree.paint(&mut canvas);
        };
        paint_once(&tree);
        assert_eq!(tree.ime_text_of(field).as_deref(), Some("5"));

        v.set(2.0);
        paint_once(&tree); // 刻意不 layout_root：重排一跑，这条就测不出东西了
        assert_eq!(
            tree.ime_text_of(field).as_deref(),
            Some("2"),
            "没有重排的帧里也要跟上 value"
        );
    }

    /// 挂在 stepper 上的 `.tooltip(..)` 要能从内部任一处悬停触发。
    ///
    /// 复合控件的命中永远落在子节点（± 按钮、数值框）上，而 `node_tooltip` 原先只看
    /// 命中节点自身——于是 `.tooltip(..)` 链上去不报错也不生效。现已与 `cursor_at`
    /// 对齐：沿祖先链回溯。
    #[test]
    fn stepper_tooltip_reaches_inner_nodes() {
        let v = signal(5.0f64);
        let root = Element::col().width(120).height(40).child(
            Element::stepper(v, 1.0, 9.0, 1.0)
                .width(120)
                .tooltip("每次 ±1"),
        );
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        tree.layout_root(Size::new(120, 40), &mut crate::text::NullTextEngine);
        let row = tree.get(id).unwrap().children[0];
        let rb = tree.abs_bounds(row);
        // 悬停在 − 按钮上（命中的是子节点，不是挂提示的那个行容器）。
        let hit = tree
            .hit_test(Point::new(rb.x + 5, rb.y + rb.h / 2))
            .expect("应命中 − 按钮");
        assert_ne!(hit, row, "前置：命中的确实是子节点");
        assert_eq!(tree.node_tooltip(hit).as_deref(), Some("每次 ±1"));
    }

    /// 点 ± **不要**把焦点拽给数值框——调值和编辑是两件事。
    ///
    /// 光标是个很强的视觉信号：只想微调一下数字，框里却开始闪光标，既像是进了编辑态
    /// 又容易被后续误触改坏。想编辑就直接点中间那格。
    ///
    /// 判据取 `dispatch_pointer` 的 `res.focus`：宿主每次按下都重新裁决焦点，没有节点
    /// 认领就清空（`apply_dispatch_effects` 的 blur 分支），所以「不请求」就等于「不聚焦」。
    #[test]
    fn stepper_buttons_do_not_steal_focus() {
        let (mut tree, field, v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        let row = tree.get(tree.root.unwrap()).unwrap().children[0];
        let rb = tree.abs_bounds(row);
        let (mut h, mut cap) = (None, None);
        for p in [
            Point::new(rb.right() - 5, rb.y + rb.h / 2), // +
            Point::new(rb.x + 5, rb.y + rb.h / 2),       // −
        ] {
            let down = tree.dispatch_pointer(ptr(PointerKind::Down, p), &mut h, &mut cap);
            let up = tree.dispatch_pointer(ptr(PointerKind::Up, p), &mut h, &mut cap);
            assert!(
                down.focus.is_none() && up.focus.is_none(),
                "点 ± 不得请求焦点"
            );
        }
        assert_eq!(v.get(), 5.0, "前置：一加一减回到原值，说明两次都真的点中了");

        // 对照：直接点中部那格才聚焦。
        let fb = tree.abs_bounds(field);
        let p = Point::new(fb.x + fb.w / 2, fb.y + fb.h / 2);
        let down = tree.dispatch_pointer(ptr(PointerKind::Down, p), &mut h, &mut cap);
        assert_eq!(
            down.focus,
            Some(field),
            "手动点数值框才该聚焦，否则连编辑都进不去了"
        );
    }

    /// 整个 stepper 对 Tab 只占一个焦点位，且那一位是中部数值框。
    #[test]
    fn stepper_takes_a_single_tab_stop() {
        let (tree, field, _v) = stepper_tree(5.0, 1.0, 9.0, 1.0);
        assert_eq!(
            tree.focusable_order(),
            vec![field],
            "± 按钮不该各占一个焦点位，否则跨过一个数字框要按三次 Tab"
        );
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
