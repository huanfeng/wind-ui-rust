//! 核心层：generational arena + Node 树 + Measure/Arrange/Paint 三阶段。
//!
//! 关键设计：布局递归由 `Tree` 独占 `&mut self` 驱动；`Widget` trait 退化为
//! 纯内容（只报固有尺寸、只画自身 content rect，绝不访问树），从根上避免
//! Rust 借用冲突。容器节点的 `widget` 为 `EmptyWidget`，视觉由 `Style` 表达。

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
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
    /// 轨道与滑块视觉宽度的**默认值**；实际取 [`track_w`]（主题 `scrollbar.width` 可改）。
    pub const TRACK_W: f32 = 7.0;
    /// 轨道距容器右缘（已计入 `WINDOW_EDGE_INSET`）边距的**默认值**；实际取 [`margin`]。
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
    ///
    /// 只有「无边框且可缩放」的窗口才内缩（宿主据此设 `Tree::scroll_edge_inset`）：带框
    /// 窗口的缩放边在客户区外，不可缩放的无边框窗也没有这一圈，白让 8px 只会让滚动条
    /// 看起来没贴边。
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
        (track_w() + margin()).ceil() as i32 + edge_inset
    }

    /// 当前主题下滑块（与轨道）的视觉宽度。
    pub fn track_w() -> f32 {
        crate::theme::current().scrollbar.width()
    }

    /// 当前主题下滑块距容器右缘的留白。
    pub fn margin() -> f32 {
        crate::theme::current().scrollbar.margin()
    }

    /// 命中区宽度：不小于 [`HIT_W`]，主题把滚动条加宽到超过它时跟着放宽，保证整条可抓。
    pub fn hit_w() -> i32 {
        HIT_W.max((track_w() + margin()).ceil() as i32)
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
    ///
    /// 想要轨道（逐值复刻某套设计稿的细滚动条）时在主题 `scrollbar.track` 里给色。
    pub fn track() -> Option<crate::geometry::Color> {
        crate::theme::current().scrollbar.track
    }

    /// 滑块色。取自当前主题，**不用**固定的黑色半透明——后者在深色主题下会连滑块
    /// 一起隐没（深底叠黑等于没画）。
    ///
    /// `active` 为拖动态，加深一档给出"抓住了"的反馈。配色见 [`ScrollbarTheme`](crate::theme::ScrollbarTheme)。
    pub fn thumb(active: bool) -> crate::geometry::Color {
        let t = crate::theme::current();
        if active {
            t.scrollbar.thumb_active(&t.palette)
        } else {
            t.scrollbar.thumb(&t.palette)
        }
    }
}

/// 剪贴板读写抽象。由平台层提供实现，UiHost 注入到 `Tree`，控件经 `EventCtx` 访问。
pub trait ClipboardProvider {
    fn get_text(&self) -> Option<String>;
    fn set_text(&self, text: &str);
}

/// 代际索引：删除节点后 generation 自增，旧 id 自然失效。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct NodeId {
    index: u32,
    generation: u32,
}

impl NodeId {
    /// 把（槽位，代际）拼成一个整数：同一时刻不同节点必不同，槽位复用后代际变了也不同。
    /// 给平台层当不透明身份比较用（如输入法的焦点控件身份）。
    pub(crate) fn as_u64(self) -> u64 {
        (u64::from(self.generation) << 32) | u64::from(self.index)
    }
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
    /// 按下本控件既不夺取焦点、也**不清掉别处的焦点**（默认 false，即通行的 blur 语义）。
    ///
    /// 为菜单栏而设，原生菜单栏正是如此：打开菜单时编辑框里的光标还在闪，关掉菜单
    /// 接着打字。没有这条豁免时错在哪：按下菜单栏是一次"点在焦点控件之外"，宿主按
    /// blur 语义把焦点清成 `None`，而 [`Tree::dispatch_key`] 的目标是 `Option<NodeId>`
    /// ——没有焦点时整个按键事件被丢弃。于是**点开过一次菜单，此后所有快捷键全部失灵**，
    /// 且看不出原因（界面毫无变化）。
    fn preserves_focus(&self) -> bool {
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
    /// 本控件若是菜单栏，交出联动信息（各标题的窗口矩形与项生成器；仅 `MenuBar` 实现）。
    ///
    /// 宿主靠它响应 F10 / 单击 Alt / Alt+助记键——这些键按下时没有任何菜单展开、
    /// 焦点也不在菜单栏上，宿主只能扫树找到它。见 [`Tree::menu_bar_link`]。
    fn menu_bar_link(&self) -> Option<crate::event::MenuBarLink> {
        None
    }
    /// 接收 Builder 传入的点击回调（仅交互控件实现）。
    fn take_click(&mut self, _f: ClickFn) {}
    /// 显隐切换时重置交互态（hover/press → 静止，并令下次绘制的补间瞬时落定不动画）。
    /// 框架在节点 `effective_visible` 翻转时调用——避免控件"按下/悬停未释放就被隐藏"，
    /// 其状态/补间冻结、下次显示瞬间闪出旧的按下/悬停态。默认无操作。
    fn reset_interaction(&mut self) {}
    /// 节点不在绘制路径上（被隐藏、或在被隐藏的祖先之下）时，每次结构变化后调用：把
    /// 跟随状态的补间就地落定到**当前**状态。
    ///
    /// 与 [`reset_interaction`](Self::reset_interaction) 的区别：那个只在"变为不可见"
    /// 的那一刻调一次，之后隐藏期间状态再被程序改过（标签页切走时改了选中项），它管不到——
    /// 补间只在 paint 里改向，露出来时就会补播一段过渡。默认无操作。
    fn settle_offscreen(&mut self) {}
    /// **用户**把焦点从本节点移走时调用一次：点了别处 / 点空白 / Tab 离开，或控件回调
    /// 经 ctx 把焦点要给了别人。与该次事件同一次分发内同步调用（在该次分发自己的
    /// 副作用落定之后）。
    ///
    /// 框架自己的焦点调度**不算**：节点被隐藏 / 禁用 / 删除后的焦点归一化、模态层弹出
    /// 时的焦点移交、`autofocus` 兑现、窗口唤起时的复位、`on_update` 相位里的焦点请求
    /// （由信号变化驱动，起因可能是定时器）、App 级回调要焦点——这些都不是"用户离开了
    /// 这个输入框"，挂在上面的"改完即提交"若跟着触发，就会在对话框弹出时把半成品提交掉。
    /// 默认无操作。`TextInput` 借它实现 `on_commit`。
    fn on_blur(&mut self, _ctx: &mut EventCtx) {}
    /// 焦点从本节点移走时调用，**不分原因**（用户点了别处、Tab 离开、程序把焦点交给
    /// 别的控件、点空白处失焦）。与 [`on_blur`](Self::on_blur) 不同，这里只该做纯视觉
    /// 的收尾，不该触发应用回调。
    ///
    /// 文本框借它清掉选区：桌面惯例是同一时刻只有拿着焦点的那个框显示选区（Win32 编辑框、
    /// WPF / WinUI 的 TextBox、浏览器的 input 失焦后都不再显示选区，Qt 的 QLineEdit 失焦即
    /// 取消选择）。窗口失活不经过这里，切回来选区仍在。默认无操作。
    fn focus_lost(&mut self) {}
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
    /// 是否接受输入法输入：焦点在本节点上时平台打开输入法（Windows 上不接受就摘掉输入法
    /// 上下文，免得在按钮上打出无处可去的组合串）。默认 = 报了光标（[`Self::ime_caret`]）。
    ///
    /// 覆盖它的两种情形：自绘控件没有光标可报、却要收输入法上屏的文字（画布自己排版）——
    /// 返回 `true`，候选窗退回输入法自己的默认位置；文本框只读——返回 `false`，不让用户组完
    /// 一串才发现被吞。
    fn accepts_ime(&self) -> bool {
        self.ime_caret().is_some()
    }
    /// 本控件是否**收文字输入**（可编辑文本框一类）：键盘焦点查询
    /// （[`EventCtx::focus_accepts_text`]、`ShortcutCtx::focus_accepts_text`）据此回答
    /// 「焦点在不在输入框里」，供全局快捷键让路。
    ///
    /// 默认 = [`Self::accepts_ime`]；但那个依赖最近一帧 paint 记下的光标，还没画过（或
    /// 被裁出视口）的文本框会答 `false`——自带配置能直接回答的控件应覆盖它。
    fn accepts_text(&self) -> bool {
        self.accepts_ime()
    }
    /// 按键处理里写的信号**只影响本控件自己的画面**（或控件已自行声明了覆盖全部读者的
    /// 脏区）——为真时宿主对这次按键只重绘本控件，否则整窗。
    ///
    /// 默认 = [`Self::accepts_text`]：文本框打字写的是自己的正文、频率又高，值得局部帧；
    /// 而按钮 / 复选框 / 列表的键盘激活写的多是别处读的共享状态（状态文案、`enabled_signal`
    /// 门控），局部帧会让那些读者停在旧值——与鼠标点同一个控件的失效口径不该分叉。
    /// 覆盖为 `true` 的前提：控件转交给应用的回调经 [`EventCtx::app_callback`] 跑，
    /// 自己写的共享状态由显式脏区或布局签名兜住。内置控件里有两处只兜住了框架控件：
    /// stepper 中部的数字框（继承 `TextInput` 的 `true`，方向键步进写共享的 `value`；启用态
    /// 门控经布局签名升整窗、别的框架控件在下次重排同步）与取色器三块面板（`drag_dirty`
    /// 只盖面板与触发器，与拖动同口径）——**自定义控件在 `paint` 里直接读它们的值信号**时，
    /// 键盘操作后会停在旧值，直到下一次整窗重画。
    fn keys_damage_self_only(&self) -> bool {
        self.accepts_text()
    }
    /// 输入法组合态变化（拼音等未上屏文字开始/结束合成）时由框架通知焦点节点。
    /// 文本控件借此在组合期间暂不绘制自绘光标（系统组合浮层自带光标）。默认无操作。
    ///
    /// **仅 win32 默认模式会调**——那边系统 IME 自己画合成串。需自绘的平台（以及开了
    /// `App::ime_inline` 的 win32）走 [`Self::set_preedit`]。
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
    /// 本控件交给输入法的内容类型（多行 / 密码）。只对报 [`Self::ime_caret`] 的可编辑控件
    /// 有意义；默认单行普通文本。
    fn ime_hints(&self) -> crate::event::ImeHints {
        crate::event::ImeHints::default()
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
    /// 控件显示的完整正文（截断前）。供 [`Tree::node_tooltip`] 判断节点提示是不是
    /// "正文全文"：只有这种提示才按 [`Widget::text_truncated`] 门控。
    fn display_text(&self) -> Option<String> {
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

/// 自上而下传递的进出场判据。
///
/// - `painted`：祖先链都在绘制路径上（逻辑可见，或带过渡而离场未完）。为假时本节点
///   不会被画到，其进度也不会在 paint 里改向——须就地落定。
/// - `shown`：只看**带过渡的**祖先是否逻辑可见。落定值取它与自身可见之积：藏在未弹出的
///   对话框里的面板落定为"隐藏"（弹出时照常进场）；普通隐藏的祖先（切走的标签页）不算，
///   切回来时不补播进场——与构建时的初值同一条规则（见 `Element::build_in`）。
#[derive(Clone, Copy)]
pub(crate) struct PresenceFlags {
    pub(crate) painted: bool,
    pub(crate) shown: bool,
}

impl PresenceFlags {
    pub(crate) const ROOT: Self = Self {
        painted: true,
        shown: true,
    };

    /// 传给 `n` 的子节点的判据。
    pub(crate) fn child(self, n: &Node) -> Self {
        let own = n.effective_visible();
        Self {
            painted: self.painted && n.layout_visible(),
            shown: match n.presence {
                Some(_) => self.shown && own,
                None if !own => true,
                None => self.shown,
            },
        }
    }
}

/// 节点投影伸出边框之外的最大距离（逻辑像素）；无投影为 0。脏区、剪枝与离屏层范围都按它外扩。
fn shadow_extent(n: &Node) -> i32 {
    match &n.style.shadow {
        Some(sh) if sh.color.a > 0 => crate::render::shadow_reach(sh.spread, sh.blur, sh.dx, sh.dy),
        _ => 0,
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
    /// 子树整体不透明度的目标值：`opacity_when` 压过静态 `opacity`。
    pub(crate) fn effective_opacity(&self) -> f32 {
        let o = self
            .opacity_cond
            .as_ref()
            .map_or(self.style.opacity, |f| f());
        // NaN 当不透明：clamp 不改 NaN，而 `NaN < 1.0` 为假会让它按不透明画，过渡也不改向。
        if o.is_nan() {
            1.0
        } else {
            o.clamp(0.0, 1.0)
        }
    }

    /// 背景的目标画笔：`bg_when` 压过静态 `bg`。
    fn bg_paint(&self, theme: &crate::theme::Theme) -> Option<Paint> {
        match &self.bg_cond {
            Some(f) => Some(Paint::fill(f())),
            None => self.style.bg.as_ref().map(|b| b.resolve_paint(theme)),
        }
    }

    /// 是否参与布局与绘制：逻辑可见，或带进出场过渡且离场未播完。
    ///
    /// 与 [`effective_visible`](Self::effective_visible) 的分工：布局、绘制看它；命中、焦点、
    /// 模态等一切"能不能交互"的判断看 `effective_visible`——离场中的残影点不到。
    pub fn layout_visible(&self) -> bool {
        self.effective_visible() || self.presence.as_ref().is_some_and(|p| p.alive())
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
    /// 横向流式：子节点从左往右排，一行放不下就折到下一行（徽章、标签云）。
    /// `spacing` 是同一行里的横向间距，`run_spacing` 是行与行之间的纵向间距，
    /// `cross` 是子节点在所在行内的纵向对齐。
    Flow {
        spacing: i32,
        run_spacing: i32,
        cross: Align,
    },
}

/// stack（`Layout::Frame`）子节点的**自由摆放**（[`crate::ui::Element::place`] /
/// [`crate::ui::Element::place_fn`]）：脱离 `align` 的九宫格对齐，按坐标落位。
pub enum Placement {
    /// 左上角落在父内容区的 `(x, y)`，尺寸照常由自身的 `width`/`height` 决定。
    At(Point),
    /// 整个矩形（相对父内容区左上角）由闭包按父内容区尺寸算出——几何由数据递推的
    /// 界面（卡片板、流程图、自由画布）。闭包读信号即可在运行期改位置与宽高。
    Rect(Box<dyn Fn(Size) -> Rect>),
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
    /// 聚焦，**即使别处已有焦点也夺过来**。
    ///
    /// [`Focus`](Self::Focus) 与 [`FocusSelectAll`](Self::FocusSelectAll) 在「这一帧
    /// 焦点已有归属」时主动让位——它们的定位是"没人要焦点时给个归宿"的兜底，抢走
    /// 用户刚点中的控件是不对的。但**就地编辑**（列表里原地改名、地址栏原地改路径）
    /// 的语义相反：这个输入框是被用户的按键唤出来的，它出现的唯一理由就是接收接下来
    /// 的输入。让位的话，键入会继续落到唤出它的那个控件上——看得见光标却打不进字。
    Take,
    /// [`Take`](Self::Take) + 全选已有内容（地址栏语义）。
    TakeSelectAll,
}

/// 定向送焦点的触发器：计数信号的值一变，宿主就把焦点交给这个节点（见 `Element::focus_when`）。
///
/// 用计数器而非布尔：布尔为真期间用户自己点了别处，本框不该再把焦点抢回来；而"下一次
/// 新建"要能**再**触发一次。比的是值、不是写入版本——写入同一个值不算触发。
pub struct FocusTrigger {
    signal: Signal<u64>,
    /// 上次兑现时的值。建节点时取树上记的这个信号的值（见 `Tree::focus_trigger`）。
    seen: u64,
    select: FocusSelect,
}

/// 滚进可视区的触发器：计数信号的值一变，宿主就把这个节点滚进祖先滚动容器的视口
/// （见 `Element::reveal_when`）。与 [`FocusTrigger`] 同一套「按信号记已兑现值」的规则。
pub struct RevealTrigger {
    signal: Signal<u64>,
    seen: u64,
}

/// 定向送焦点兑现时顺带对选区做什么（见 `Element::focus_when` / `select_when`）。
#[derive(Clone, Copy)]
pub(crate) enum FocusSelect {
    /// 只送焦点，选区不动。
    Keep,
    /// 全选正文。
    All,
    /// 选中信号里的 `[start, end)`（字符索引），兑现那一刻才读。
    Range(Signal<(usize, usize)>),
    /// 同 [`Range`](Self::Range)，但**不送焦点**（`Element::select_without_focus_when`）：
    /// 查找栏「焦点留在查询框、正文里亮出当前这一处」。
    RangeNoFocus(Signal<(usize, usize)>),
}

impl FocusSelect {
    /// 兑现时是否夺取焦点。
    pub(crate) fn takes_focus(self) -> bool {
        !matches!(self, FocusSelect::RangeNoFocus(_))
    }
}

impl Autofocus {
    /// 是否夺取已有焦点（见 [`Take`](Self::Take)）。
    pub fn takes_focus(self) -> bool {
        matches!(self, Autofocus::Take | Autofocus::TakeSelectAll)
    }
    /// 兑现后是否全选正文。
    pub fn selects_all(self) -> bool {
        matches!(self, Autofocus::FocusSelectAll | Autofocus::TakeSelectAll)
    }
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
    /// 自由摆放（仅父为 `Layout::Frame` 时生效，见 [`Placement`]）。设了它 `align`/`align_v`
    /// 与 `margin` 都不再参与定位。
    pub place: Option<Placement>,
    /// `Placement::Rect` 在**测量期**算出的矩形，排布期原样复用（见 `Tree::measure_frame`）。
    pub(crate) place_cache: Option<Rect>,
    pub layout: Layout,
    pub widget: Box<dyn Widget>,
    pub style: Style,
    pub visible: bool,
    /// 运行期可见信号（None=无约束）。与 `visible`/`vis_cond` 取与。
    pub vis_signal: Option<Signal<bool>>,
    /// 运行期可见条件（如 Tab 页绑定选中项、Dialog 绑定显示标志）。
    /// 与 `visible` 取与：返回 false 则该帧不参与测量/布局/绘制/命中。
    pub vis_cond: Option<Box<dyn Fn() -> bool>>,
    /// 运行期线性权重（None=用 `width`/`height` 里的静态 `Dimension`）。
    ///
    /// 父为线性容器时，每次测量把主轴维度换成 `Dimension::Weight(f())`。与 `vis_cond`
    /// 同一契约：纯函数、帧内值不变。为可拖动分栏而设——权重原本在构建期烘进
    /// `Dimension`，改一次就得重建整棵子树，拖动分隔条每帧重建两个文件面板不可接受。
    pub weight_fn: Option<Box<dyn Fn() -> f32>>,
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
    /// 用户让本节点失焦时的回调（`Element::on_blur`）。时机与过滤同 [`Widget::on_blur`]，
    /// 在控件自己的 `on_blur` 之后调用——输入框的 `on_commit` 先落定，这里读到的是提交后的状态。
    pub on_blur: Option<ClickFn>,
    /// 是否为窗口拖动区（自定义标题栏）：无边框窗口中在此区域按下可拖动窗口。
    /// 命中沿父链继承（标记容器即其内非交互区均可拖），但落在子交互控件上不拖动。
    pub window_drag: bool,
    /// Tab 焦点环参与度覆盖：`None` 按 `Widget::focusable()`；`Some(false)` 强制
    /// 退出焦点环（如词典正文——主焦点应常驻输入框）；`Some(true)` 强制加入。
    /// **仅影响 Tab 遍历**：不改变命中测试、点击交互与 `request_focus` 语义。
    pub focusable: Option<bool>,
    /// 悬停提示文本（None=无）。宿主在悬停延时后于指针附近绘制浮层；
    /// 像 `enabled`/`window_drag` 一样挂在节点上，适用于任意控件/容器。
    /// 存 [`TextContent`](crate::ui::TextContent)，弹出时现取：传 `t!` 进来换语言即跟随。
    pub tooltip: Option<crate::ui::TextContent>,
    /// 光标形状覆盖（`Element::cursor`）。`None` 按控件自己的 [`Widget::cursor`]；
    /// 见 [`Tree::cursor_at`] 的取值顺序。
    pub cursor: Option<CursorShape>,
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
    /// 进出场过渡状态（`Element::transition`）。只有声明了过渡的节点才分配。
    pub(crate) presence: Option<Box<crate::ui::presence::PresenceState>>,
    /// 高度过渡状态（`Element::animate_height`）。只有声明了的节点才分配。
    pub(crate) height_anim: Option<Box<crate::ui::presence::HeightAnim>>,
    /// 运行期背景色（`Element::bg_when`）：有则压过 `style.bg`。
    pub(crate) bg_cond: Option<Box<dyn Fn() -> Color>>,
    /// 运行期不透明度（`Element::opacity_when`）：有则压过 `style.opacity`。
    pub(crate) opacity_cond: Option<Box<dyn Fn() -> f32>>,
    /// 样式过渡状态（`Element::animate_style`）。只有声明了的节点才分配。
    pub(crate) style_anim: Option<Box<crate::ui::presence::StyleAnim>>,
    /// 布局过渡状态（`Element::animate_layout`）。只有声明了的节点才分配。
    pub(crate) layout_anim: Option<Box<crate::ui::presence::LayoutAnim>>,
    /// 声明式初始焦点（`None`=不参与）。宿主在布局稳定后**一次性**兑现，见
    /// [`Autofocus`] 与 `UiHost::refresh_focus`。
    pub autofocus: Option<Autofocus>,
    /// 本节点的 autofocus 已兑现过。
    ///
    /// 记在节点上而不是只有宿主那个全局标志：[`Autofocus::Take`] 的语义是"**每次出现**
    /// 都夺取焦点"，而全局标志一经置位就再不复位（只有窗口隐藏→唤起会 rearm），
    /// 于是**窗口生命周期内第二个**要求自动聚焦的控件永远拿不到焦点——就地编辑框
    /// 因此第二次唤出就打不进字。节点销毁时这个标志随之消失，天然不会错认复用的 id。
    pub autofocus_done: bool,
    /// 定向送焦点的触发器（`Element::focus_when`）。
    pub focus_when: Option<FocusTrigger>,
    /// 滚进可视区的触发器（`Element::reveal_when`）。
    pub reveal_when: Option<RevealTrigger>,
}

/// 一次布局同步里"哪些布局过渡节点被展开推着走"的索引（见 [`Tree::sync_layout_anims`]）。
///
/// 逐对判断（每个推进中的展开 × 每个布局过渡节点 × 树深，外加找兄弟下标的线性扫描）在
/// 长列表增删时是平方级：2000 行挂 `animate_item` 删掉一半，一帧 140ms。这里先把每个展开到
/// 根的链摊到沿途各层（记下展开落在哪些子支），再让每个节点只走自己到根的那一条链，判据与
/// 逐对判断逐条对应（测试里对拍）。兄弟下标、"前面有没有加权兄弟"都按父节点缓存。
struct PushIndex {
    /// 自身就在推进展开的节点。
    movers: HashSet<NodeId>,
    /// 子树里有展开的节点 → 展开所在子支的（最小, 最大）下标。
    branches: HashMap<NodeId, (usize, usize)>,
    /// 子节点 → 它在父节点 children 里的下标。
    index: HashMap<NodeId, usize>,
    /// 父节点 → 第一个主轴加权子节点的下标（没有为 `usize::MAX`）。
    first_weighted: HashMap<NodeId, usize>,
    /// 节点 → [`Tree::size_sensitive`]。
    sensitive: HashMap<NodeId, bool>,
}

impl PushIndex {
    fn new(tree: &Tree, movers: &[NodeId]) -> Self {
        let mut px = PushIndex {
            movers: movers.iter().copied().collect(),
            branches: HashMap::new(),
            index: HashMap::new(),
            first_weighted: HashMap::new(),
            sensitive: HashMap::new(),
        };
        for &m in movers {
            let mut child = m;
            while let Some(p) = tree.get(child).and_then(|n| n.parent) {
                // 有父指针却不在父节点 children 里（正常路径走不到）：这一层记不了子支，
                // 照样往上走，别让更高的层漏记——参照实现也不会因一层断掉就放弃。
                if let Some(i) = px.child_index(tree, child) {
                    let e = px.branches.entry(p).or_insert((i, i));
                    *e = (e.0.min(i), e.1.max(i));
                }
                child = p;
            }
        }
        px
    }

    fn child_index(&mut self, tree: &Tree, n: NodeId) -> Option<usize> {
        if let Some(&i) = self.index.get(&n) {
            return Some(i);
        }
        let parent = tree.get(tree.get(n)?.parent?)?;
        for (i, &c) in parent.children.iter().enumerate() {
            self.index.insert(c, i);
        }
        self.index.get(&n).copied()
    }

    fn sensitive(&mut self, tree: &Tree, n: NodeId) -> bool {
        if let Some(&s) = self.sensitive.get(&n) {
            return s;
        }
        let idx = self.child_index(tree, n);
        let parent_id = tree.get(n).and_then(|x| x.parent);
        let first_weighted = &mut self.first_weighted;
        let s = tree.size_sensitive_with(n, |parent, axis| {
            let Some(pid) = parent_id else {
                return false;
            };
            let first = *first_weighted.entry(pid).or_insert_with(|| {
                parent
                    .children
                    .iter()
                    .position(|&c| {
                        tree.get(c).is_some_and(|c| {
                            let main = if axis == Axis::Horizontal {
                                &c.width
                            } else {
                                &c.height
                            };
                            main.is_weight() || c.weight_fn.is_some()
                        })
                    })
                    .unwrap_or(usize::MAX)
            });
            idx.is_some_and(|i| first < i)
        });
        self.sensitive.insert(n, s);
        s
    }

    /// `id` 是否被某个展开推着走。逐层对应 [`Tree::pushes`] 的三种情形：
    /// 某层自己就是展开（展开是 `id` 的祖先或自身）、`id` 自己的子树里有展开（`id` 是祖先）、
    /// 某层是展开与 `id` 的共同父节点（展开落在别的子支上）。
    fn pushed(&mut self, tree: &Tree, id: NodeId) -> bool {
        let mut chain = vec![id];
        let mut n = id;
        while let Some(p) = tree.get(n).and_then(|x| x.parent) {
            chain.push(p);
            n = p;
        }
        chain.reverse();
        let sens: Vec<bool> = chain.iter().map(|&c| self.sensitive(tree, c)).collect();
        // prefix[k] = chain[..k] 里有落点随尺寸变化的；suffix[k] = chain[k..] 里有。
        let mut prefix = vec![false; chain.len() + 1];
        for k in 0..chain.len() {
            prefix[k + 1] = prefix[k] || sens[k];
        }
        let mut suffix = vec![false; chain.len() + 1];
        for k in (0..chain.len()).rev() {
            suffix[k] = suffix[k + 1] || sens[k];
        }
        for (k, &node) in chain.iter().enumerate() {
            if self.movers.contains(&node) && prefix[k + 1] {
                return true;
            }
            let Some(&(lo, hi)) = self.branches.get(&node) else {
                continue;
            };
            if k + 1 == chain.len() {
                // 展开在 `id` 自己的子树里。
                if prefix[k + 1] {
                    return true;
                }
                continue;
            }
            let Some(i) = self.child_index(tree, chain[k + 1]) else {
                continue;
            };
            if lo == i && hi == i {
                // 展开都在 `id` 这一支里，共同父节点在更深处。
                continue;
            }
            if prefix[k + 1] || suffix[k + 1] {
                return true;
            }
            match tree.get(node).map(|p| &p.layout) {
                Some(Layout::Linear { .. } | Layout::Scroll) => {
                    if lo < i {
                        return true;
                    }
                }
                Some(Layout::Frame) | None => {}
                Some(_) => return true,
            }
        }
        false
    }
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
    /// 贴窗口右缘的滚动条要额外内缩多少（见 `scrollbar::WINDOW_EDGE_INSET`），0 = 不内缩。
    ///
    /// 内缩只为避让**落在客户区里**的缩放边框，而只有「无边框且可缩放」的窗口才有这一圈：
    /// 带框窗口的缩放边在客户区外，不可缩放的窗口压根没有。由宿主按窗口能力设置（所有
    /// 窗口——含离屏截图——都经 `UiHost::new`）；默认取内缩值，只有直接 `Tree::new()` 的
    /// 单测吃到它。
    pub(crate) scroll_edge_inset: i32,
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
    /// on_update 相位里请求**交出焦点**的节点（`EventCtx::release_focus`）。与 `pending_focus`
    /// 同因同治：该相位丢弃整个 `EventOutcome`。事件路径走 `DispatchResult::release`，不经这里。
    pending_release: Vec<NodeId>,
    /// 各定向送焦点信号**已兑现到**的值（见 `Tree::focus_trigger`）。按信号而非按节点记：
    /// 一次点击里既让计数加一、又让输入框所在子树重建，是很自然的写法——新节点若取当时的
    /// 值，这次触发就被它自己吞掉了。每个触发信号一条，句柄带代际不会错认；表变大时清掉已回收的
    /// 信号（应用在重建闭包里每次新建触发信号时，不清就只增不减）。
    focus_seen: std::collections::HashMap<Signal<u64>, u64>,
    /// 各滚进视口信号已兑现到的值（见 `Tree::reveal_trigger`），规则同 `focus_seen`。
    reveal_seen: std::collections::HashMap<Signal<u64>, u64>,
    /// on_update 相位里控件请求的**开窗**暂存区。与 `pending_toasts` 同因同治：
    /// 该相位不经 `DispatchResult`，不暂存就会被整个丢掉——症状是
    /// "在 on_update 里 `ctx.open_window` 什么都不发生，且没有任何报错"。
    pending_windows: Vec<crate::event::WindowRequest>,
    /// 本窗口实际落成的渲染后端（平台经 `AppHandler::set_active_renderer` 告知）。
    pub(crate) active_renderer: crate::platform::ActiveRenderer,
    /// 同上，`ctx.show_context_menu`。延迟弹菜单（先记下、下一帧再弹）走的正是这条路。
    pending_menu: Option<MenuRequest>,
    /// 同上，`ctx.defer_blocking` 与各 `request_pick_*`。应用「先排队、下一帧由某个控件的
    /// on_update 兑现」的命令里，弹系统原生菜单 / 对话框的那些原先在此静默消失。
    pending_dialog: Option<DialogRequest>,
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
    /// 上一帧绘制时正在推进的展开 / 高度过渡节点（它们每帧请求一次重排）。被它们推着走的
    /// 节点，位置变化本身已经是平滑的，布局过渡只记位置、不另起过渡——否则每帧都去追一个
    /// 移动的目标，远远落在后面。取用即清，见 [`Tree::sync_layout_anims`]。
    layout_following: RefCell<Vec<NodeId>>,
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
    /// 回调期间被临时换成 `EmptyWidget` 的节点，连同它换出前的 `accepts_text()` 答案。
    /// 没有它，在焦点输入框**自己**的回调里问 `focus_accepts_text` 会查到 `EmptyWidget` 答 false。
    held_accepts_text: Option<(NodeId, bool)>,
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
            held_accepts_text: None,
            root: None,
            focus_ring_visible: false,
            clipboard: None,
            scroll_edge_inset: scrollbar::WINDOW_EDGE_INSET,
            reactive_nodes: Vec::new(),
            pending_toasts: Vec::new(),
            pending_focus: None,
            pending_release: Vec::new(),
            focus_seen: std::collections::HashMap::new(),
            reveal_seen: std::collections::HashMap::new(),
            pending_windows: Vec::new(),
            active_renderer: crate::platform::ActiveRenderer::Software,
            pending_menu: None,
            pending_dialog: None,
            arrange_origin: Point::new(0, 0),
            layout_size: Size::ZERO,
            layout_following: RefCell::new(Vec::new()),
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

    /// 取走 on_update 相位攒下的交出焦点请求（见 `pending_release`）。
    pub(crate) fn take_pending_release(&mut self) -> Vec<NodeId> {
        std::mem::take(&mut self.pending_release)
    }

    /// `node` 是否为 `ancestor` 本身或其后代。
    pub(crate) fn is_within(&self, node: NodeId, ancestor: NodeId) -> bool {
        self.ancestor_chain(node).contains(&ancestor)
    }

    /// 取走 on_update 相位攒下的开窗请求（见 `pending_windows`）。
    pub fn take_pending_windows(&mut self) -> Vec<crate::event::WindowRequest> {
        std::mem::take(&mut self.pending_windows)
    }

    /// 取走 on_update 相位攒下的菜单请求（见 `pending_menu`）。
    pub fn take_pending_menu(&mut self) -> Option<MenuRequest> {
        self.pending_menu.take()
    }

    /// 取走 on_update 相位攒下的对话框请求（见 `pending_dialog`）。
    pub fn take_pending_dialog(&mut self) -> Option<DialogRequest> {
        self.pending_dialog.take()
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
        let requested_release = ctx.out.release.take();
        let requested_windows = std::mem::take(&mut ctx.out.open_windows);
        let requested_menu = ctx.out.menu.take();
        let requested_dialog = ctx.out.dialog.take();
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
        self.pending_release.extend(requested_release);
        // 开窗与菜单同 toast：该相位不经 DispatchResult，丢了就是"调了没反应、
        // 也没报错"。应用把「下一帧再执行这条命令」排进队列、由某个控件的 on_update
        // 兑现，是很自然的写法，而命令十有八九要开个对话框。
        self.pending_windows.extend(requested_windows);
        // 一帧里最多弹一个菜单；后来者覆盖，与 `EventOutcome::menu` 的单值语义一致。
        if let Some(m) = requested_menu {
            self.pending_menu = Some(m);
        }
        // 对话框同理，单值、后来者覆盖（`EventOutcome::dialog` 本身也是单值）。
        if let Some(d) = requested_dialog {
            self.pending_dialog = Some(d);
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
                        .map(|n| n.layout_visible() && n.overlay.is_none())
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
        let resized = self.layout_size != size;
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
        self.sync_layout_anims(!resized);
    }

    /// 布局完成后，让声明了布局过渡的节点比对新旧位置（见 [`Node::layout_anim`]）。
    ///
    /// `animate`：窗口改尺寸引起的位移不过渡——拖窗口边时内容像橡皮筋一样拖在后面，
    /// 比一帧到位难受得多。
    fn sync_layout_anims(&mut self, animate: bool) {
        let moving = self.layout_following.take();
        let mut push = (!moving.is_empty()).then(|| PushIndex::new(self, &moving));
        let mut th = None;
        for (i, slot) in self.slots.iter().enumerate() {
            let Some(a) = slot.node.as_ref().and_then(|n| n.layout_anim.as_ref()) else {
                continue;
            };
            let id = NodeId {
                index: i as u32,
                generation: slot.generation,
            };
            let pos = self.layout_anim_pos(id);
            let pushed = push.as_mut().is_some_and(|px| px.pushed(self, id));
            #[cfg(test)]
            debug_assert_eq!(
                pushed,
                moving.iter().any(|&m| self.pushes(m, id)),
                "PushIndex 须与逐对判断一致"
            );
            let th = th.get_or_insert_with(crate::theme::current);
            a.relayout(pos, animate && !pushed, th);
        }
    }

    /// 逐对判断的参照实现（测试里与 [`PushIndex`] 对拍）：展开 / 高度过渡中的 `mover`
    /// 是否可能推动 `id` 的位置。宁可多判：多判只是这段
    /// 期间的位移不另起过渡，漏判则每帧起一段新过渡去追，明显拖在后面。
    ///
    /// `mover` 变高时，先把它到根的每一层容器撑大（共同祖先 `P` 以上的每一层都包含 `id`），
    /// 于是 `id` 被推动当且仅当：
    /// - 在 `P` 里，`mover` 那一支排在 `id` 那一支前面（线性 / 滚动容器按顺序堆叠）；或
    /// - `id` 那一支上（直到 `id` 自己）、`P` 及 `P` 以上任一层的落点随尺寸变化
    ///   （见 [`Self::size_sensitive`]）。
    ///
    /// `mover` 是 `id` 的祖先时，`id` 只在 `mover` 及其以上某层落点随尺寸变化时被推动——
    /// 祖先自己的高度过渡只裁剪，子节点按自然尺寸排布。`id` 是 `mover` 的祖先时同理。
    #[cfg(test)]
    fn pushes(&self, mover: NodeId, id: NodeId) -> bool {
        let chain = |mut n: NodeId| {
            let mut v = vec![n];
            while let Some(p) = self.get(n).and_then(|x| x.parent) {
                v.push(p);
                n = p;
            }
            v.reverse();
            v
        };
        let (a, b) = (chain(mover), chain(id));
        let common = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
        if common == 0 {
            return false;
        }
        let sensitive_up = |from: usize| a[..from].iter().any(|&n| self.size_sensitive(n));
        if common >= a.len() || common >= b.len() {
            // 一方是另一方的祖先（含自身）：只看较高那个及以上各层。
            return sensitive_up(common);
        }
        // `id` 那一支整条往下都查：顶层节点自己不挪，尺寸却可能随兄弟变（加权、拉伸），
        // 它里面居中的子孙照样被推。
        if sensitive_up(common) || b[common..].iter().any(|&n| self.size_sensitive(n)) {
            return true;
        }
        let Some(parent) = self.get(a[common - 1]) else {
            return false;
        };
        match parent.layout {
            Layout::Linear { .. } | Layout::Scroll => {}
            // 层叠的兄弟互相推不动；随 P 变大而挪的已在上面按对齐判过。
            Layout::Frame => return false,
            _ => return true,
        }
        let at = |n: NodeId| parent.children.iter().position(|&c| c == n);
        matches!((at(a[common]), at(b[common])), (Some(x), Some(y)) if x < y)
    }

    /// 节点在父容器里的落点是否随尺寸变化：自身变大、父容器变大或前面的兄弟变大时会挪。
    ///
    /// 居中 / 靠后对齐、`Placement::Rect`、浮层（翻到锚点上方时随高度上移）、排在它前面的
    /// 加权兄弟（它变大，那个兄弟就缩）都算。未知布局一律算，理由见 [`Self::pushes`]。
    #[cfg(test)]
    fn size_sensitive(&self, id: NodeId) -> bool {
        self.size_sensitive_with(id, |parent, axis| {
            let weighted = |c: &Node| {
                let main = if axis == Axis::Horizontal {
                    &c.width
                } else {
                    &c.height
                };
                main.is_weight() || c.weight_fn.is_some()
            };
            parent
                .children
                .iter()
                .take_while(|&&c| c != id)
                .any(|&c| self.get(c).is_some_and(weighted))
        })
    }

    /// [`Self::size_sensitive`] 的骨架；"前面有没有加权兄弟"交给调用方（[`PushIndex`] 按父节点缓存）。
    fn size_sensitive_with(
        &self,
        id: NodeId,
        weighted_before: impl FnOnce(&Node, Axis) -> bool,
    ) -> bool {
        let Some(n) = self.get(id) else {
            return false;
        };
        if n.overlay.is_some() {
            return true;
        }
        let Some(parent) = n.parent.and_then(|p| self.get(p)) else {
            return false;
        };
        let moves = |a: Align| !matches!(a, Align::Start | Align::Stretch);
        match parent.layout {
            Layout::Scroll => false,
            Layout::Frame => match &n.place {
                Some(Placement::At(_)) => false,
                Some(Placement::Rect(_)) => true,
                None => {
                    let h = n.align.unwrap_or(Align::Start);
                    moves(h) || moves(n.align_v.unwrap_or(h))
                }
            },
            Layout::Linear { axis, cross, .. } => {
                moves(n.align.unwrap_or(cross)) || weighted_before(parent, axis)
            }
            _ => true,
        }
    }

    /// 布局过渡比对用的位置：相对最近一个同样声明了布局过渡的祖先（没有就是根），并扣掉
    /// 沿途滚动容器的滚动量与越界回弹。不在布局里（自身或任一祖先不占位）时为 `None`。
    ///
    /// 相对祖先：父子都声明时，父的位移已经带着子走了，子只补自己相对父的那部分，
    /// 否则同一段位移会被叠两遍。扣滚动：滚动不是布局变化，滚一下不能让每一行都滑一段。
    /// `offset`（拖拽让位等）不计入——那是调用方逐帧写的视觉位移，跟着补间会拖在指针后面。
    fn layout_anim_pos(&self, id: NodeId) -> Option<Point> {
        let n = self.get(id)?;
        if !n.layout_visible() {
            return None;
        }
        let mut pos = Point::new(n.bounds.x, n.bounds.y);
        let mut anchored = false;
        let mut cur = n.parent;
        while let Some(pid) = cur {
            let pn = self.get(pid)?;
            if !pn.layout_visible() {
                return None;
            }
            if !anchored {
                if matches!(pn.layout, Layout::Scroll) {
                    // arrange 把子节点整体挪了 `-scroll_y + over_scroll`（见 `arrange_scroll`）。
                    pos.y += pn.scroll_y - pn.over_scroll;
                }
                if pn.layout_anim.is_some() {
                    anchored = true;
                } else {
                    pos.x += pn.bounds.x;
                    pos.y += pn.bounds.y;
                }
            }
            cur = pn.parent;
        }
        Some(pos)
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
                self.shut_overlay_now(id);
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
                self.shut_overlay_now(id);
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
    /// 由框架（而非用户）收起浮层：置展开信号为 false，且**不留离场残影**。
    ///
    /// 被对话框挡住而收起的，残影会在对话框关闭后以旧状态冒出来——正是收起它要防的
    /// "用户早已忘记的面板凭空重现"；锚点滚出视口的，残影会画在视口之外。写入有 `get()`
    /// 守卫，是一次性的，不会每帧弄脏。
    fn shut_overlay_now(&self, id: NodeId) {
        let Some(n) = self.get(id) else {
            return;
        };
        if let Some(sig) = n.overlay {
            if sig.get() {
                sig.set(false);
            }
        }
        if let Some(p) = &n.presence {
            p.settle(false);
        }
    }

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
                n.layout_visible(),
            ),
            None => return Size::ZERO,
        };
        if let Some(p) = self.get(id).and_then(|n| n.presence.as_ref()) {
            p.laid_exiting
                .set(visible && !self.get(id).is_some_and(|n| n.effective_visible()));
        }
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
            Layout::Flow {
                spacing,
                run_spacing,
                ..
            } => self.measure_flow(id, spacing, run_spacing, wspec, avail_w, text),
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
        let resolved_h = self.animated_height(id, resolved_h);
        let size = Size::new(resolved_w, resolved_h);
        if let Some(n) = self.get_mut(id) {
            n.measured = size;
        }
        size
    }

    /// 高度过渡与展开类进出场作用于占位高度：子树已按自然尺寸测完，这里只改本节点报给
    /// 父级的高度——同级据此平滑让位，子树在绘制时裁到这个高度。
    fn animated_height(&self, id: NodeId, natural: i32) -> i32 {
        let Some(n) = self.get(id) else {
            return natural;
        };
        if n.height_anim.is_none() && n.presence.is_none() {
            return natural;
        }
        let mut h = match &n.height_anim {
            Some(a) => a.measure(
                natural,
                n.children.iter().any(|&c| self.subtree_revealing(c)),
                &crate::theme::current(),
            ),
            None => natural as f32,
        };
        if let Some(f) = n.presence.as_ref().and_then(|p| p.height_factor()) {
            h *= f;
        }
        h.round().max(0.0) as i32
    }

    /// 子树（含 `id` 自己）里是否有展开类进出场或高度过渡正在进行——这些会让外层的自然
    /// 高度逐帧变化。只看仍参与布局的节点。
    fn subtree_revealing(&self, id: NodeId) -> bool {
        let Some(n) = self.get(id).filter(|n| n.layout_visible()) else {
            return false;
        };
        n.presence.as_ref().is_some_and(|p| p.expanding())
            || n.height_anim.as_ref().is_some_and(|a| a.is_active())
            || n.children.iter().any(|&c| self.subtree_revealing(c))
    }

    #[allow(clippy::too_many_arguments)]
    /// 线性子节点的 (宽, 高, margin)，主轴维度已套用运行期权重（见 [`Node::weight_fn`]）。
    fn linear_dims(&self, c: NodeId, horizontal: bool) -> (Dimension, Dimension, Insets) {
        let n = self.get(c).unwrap();
        let (mut cw, mut ch) = (n.width, n.height);
        if let Some(f) = &n.weight_fn {
            let w = Dimension::Weight(f().max(0.0));
            if horizontal {
                cw = w;
            } else {
                ch = w;
            }
        }
        (cw, ch, n.margin)
    }

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
            let (cw, ch, cm) = self.linear_dims(c, horizontal);
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
            // 交叉轴上先扣掉子节点自己的 margin 再发约束：不扣的话 `Match` 会量到整条
            // 交叉轴，arrange 再按 margin.left 右移，净效果是**朝末端溢出一个 margin**
            // （表现为卡片盖住旁边的分隔线）。主轴一直是预扣的，这里是补上交叉轴那一半。
            let cross_child =
                child_spec(cross_dim, (cross_avail - cm_cross).max(0), cross_unbounded);
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
                let (cw, ch, cm) = self.linear_dims(c, horizontal);
                let w = if horizontal { cw.weight() } else { ch.weight() };
                // 末位补余，消除整数截断误差，实现像素精确分配。
                let portion = if i == last {
                    (remaining - allocated).max(0)
                } else {
                    (remaining as f32 * w / total_weight) as i32
                };
                allocated += portion;
                let main_child = MeasureSpec::exactly(portion);
                let (_, cm_cross) = main_cross_insets(horizontal, cm);
                // 同上：交叉轴约束须先扣 margin（见第一遍里的注释）。
                let cross_child = child_spec(
                    if horizontal { ch } else { cw },
                    (cross_avail - cm_cross).max(0),
                    cross_unbounded,
                );
                let (cwspec, chspec) = if horizontal {
                    (main_child, cross_child)
                } else {
                    (cross_child, main_child)
                };
                let s = self.measure(c, cwspec, chspec, text);
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
            // 先扣子节点自己的横向 margin：`arrange_scroll` 排版时扣了
            // （`inner.w - scrollbar_w - cm.horizontal()`），这里不扣的话，子树会按
            // **更宽**的宽度算高、再按更窄的宽度排版——换行文本因此少算一行，底部被裁。
            // （`scrollbar_w` 那一份在 measure 阶段拿不到：是否可滚要等 arrange 才知道。）
            let cwspec = child_spec(cw, (avail_w - cm.horizontal()).max(0), false);
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

    /// 流式容器：子节点按自身宽度（不超过一整行）测量，再按可用宽折行。
    /// 宽不受限时等同于单行的 row。
    fn measure_flow(
        &mut self,
        id: NodeId,
        spacing: i32,
        run_spacing: i32,
        wspec: MeasureSpec,
        avail_w: i32,
        text: &mut dyn TextEngine,
    ) -> Size {
        let unbounded = wspec.mode == MeasureMode::Unbounded;
        let children = self.visible_children(id);
        let mut sizes = Vec::with_capacity(children.len());
        for &c in &children {
            let (cw, ch, cm) = {
                let n = self.get(c).unwrap();
                (n.width, n.height, n.margin)
            };
            // 单个子节点最宽占满一行：长文本据此换行 / 截断，而不是把容器撑出界。
            let cwspec = child_spec(cw, (avail_w - cm.horizontal()).max(0), unbounded);
            let chspec = child_spec(ch, 0, true);
            let s = self.measure(c, cwspec, chspec, text);
            sizes.push(Size::new(s.w + cm.horizontal(), s.h + cm.vertical()));
        }
        let limit = if unbounded { i32::MAX } else { avail_w };
        let lines = flow_lines(&sizes, limit, spacing);
        let mut w = 0;
        let mut h = 0;
        for (i, line) in lines.iter().enumerate() {
            w = w.max(line.width);
            h += line.height + if i > 0 { run_spacing } else { 0 };
        }
        Size::new(w, h)
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
        // 父尺寸不定（滚动容器里、或宽度不定的祖先下）的那一轴没有可报的值，给 0：闭包据此
        // 只按另一轴算。不给 0 的话，未定轴的 avail 是个极大的哨兵值，按比例换算会把 stack
        // 撑到天文数字。
        let place_avail = Size::new(
            if wspec.mode == MeasureMode::Unbounded {
                0
            } else {
                avail_w
            },
            if hspec.mode == MeasureMode::Unbounded {
                0
            } else {
                avail_h
            },
        );
        for &c in &children {
            // 先清掉上一帧的缓存：只有本帧测量期走到 `Placement::Rect` 才重新写入，排布期
            // 因此不会拿到别的摆放方式留下的旧矩形。
            if let Some(n) = self.get_mut(c) {
                n.place_cache = None;
            }
            // 自由摆放的子：尺寸照常（At）或由闭包钉死（Rect），并按落点右下角撑开 stack——
            // stack 不设高时放进滚动容器，内容总高因此就是最低那张卡片的底边。
            match self.placed_rect(c, place_avail) {
                Some((origin, Some(r))) => {
                    // 记下这次的矩形，排布期复用而不再调闭包：stack 不设宽高时排布期的内容区
                    // 是本次测量**包出来**的尺寸，与测量期的可用量不同——再调一次闭包，按
                    // 比例换算的几何就会与子树按之测量的尺寸对不上（文字排版、布局缓存错位）。
                    if let Some(n) = self.get_mut(c) {
                        n.place_cache = Some(r);
                    }
                    self.measure(
                        c,
                        MeasureSpec::exactly(r.w.max(0)),
                        MeasureSpec::exactly(r.h.max(0)),
                        text,
                    );
                    mw = mw.max(origin.x + r.w);
                    mh = mh.max(origin.y + r.h);
                    continue;
                }
                Some((p, None)) => {
                    let (cw, ch) = {
                        let n = self.get(c).unwrap();
                        (n.width, n.height)
                    };
                    let cwspec = child_spec(
                        cw,
                        (avail_w - p.x).max(0),
                        wspec.mode == MeasureMode::Unbounded,
                    );
                    let chspec = child_spec(
                        ch,
                        (avail_h - p.y).max(0),
                        hspec.mode == MeasureMode::Unbounded,
                    );
                    let s = self.measure(c, cwspec, chspec, text);
                    mw = mw.max(p.x + s.w);
                    mh = mh.max(p.y + s.h);
                    continue;
                }
                None => {}
            }
            let (cw, ch, cm) = {
                let n = self.get(c).unwrap();
                (n.width, n.height, n.margin)
            };
            // 同 `measure_linear`：交叉轴（Frame 两轴都是）先扣子节点自己的 margin。
            // 不扣的话，`Match` 子量到整条可用宽，`arrange_frame` 对**非 Stretch**
            // 对齐的子直接用这个测量值再按 margin.left 右移，右沿就溢出一个 margin。
            // Stretch 那条路本来是对的（它用的是已扣 margin 的 avail），所以这个 bug
            // 只在默认的 Start 对齐下出现，格外隐蔽。
            let cwspec = child_spec(
                cw,
                (avail_w - cm.horizontal()).max(0),
                wspec.mode == MeasureMode::Unbounded,
            );
            let chspec = child_spec(
                ch,
                (avail_h - cm.vertical()).max(0),
                hspec.mode == MeasureMode::Unbounded,
            );
            let s = self.measure(c, cwspec, chspec, text);
            mw = mw.max(s.w + cm.horizontal());
            mh = mh.max(s.h + cm.vertical());
        }
        Size::new(mw, mh)
    }

    // ---- Arrange ----

    fn arrange(&mut self, id: NodeId, bounds: Rect) {
        let (layout, padding, visible) = match self.get(id) {
            Some(n) => (n.layout, n.padding, n.layout_visible()),
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
            Layout::Flow {
                spacing,
                run_spacing,
                cross,
            } => self.arrange_flow(id, inner, spacing, run_spacing, cross),
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
        (hi - scrollbar::hit_w(), hi)
    }

    fn scrollbar_edge_inset(&self, abs_right: i32) -> i32 {
        let inset = self.scroll_edge_inset;
        if inset <= 0 {
            return 0;
        }
        let Some(root_w) = self.root.and_then(|r| self.get(r)).map(|n| n.bounds.w) else {
            return 0;
        };
        if abs_right >= root_w - inset {
            inset
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

    fn arrange_flow(
        &mut self,
        id: NodeId,
        inner: Rect,
        spacing: i32,
        run_spacing: i32,
        cross: Align,
    ) {
        let children = self.visible_children(id);
        let sizes: Vec<Size> = children
            .iter()
            .map(|&c| {
                let (s, m) = (self.measured_of(c), self.margin_of(c));
                Size::new(s.w + m.horizontal(), s.h + m.vertical())
            })
            .collect();
        let lines = flow_lines(&sizes, inner.w, spacing);
        let mut y = inner.y;
        for line in lines {
            let mut x = inner.x;
            for i in line.start..line.end {
                let c = children[i];
                let (cs, cm) = (self.measured_of(c), self.margin_of(c));
                let align = self.get(c).and_then(|n| n.align).unwrap_or(cross);
                let avail_h = (line.height - cm.vertical()).max(0);
                let ch = if align == Align::Stretch {
                    avail_h
                } else {
                    cs.h
                };
                let cy = y + cm.top + align_offset(align, avail_h, ch);
                self.arrange(c, Rect::new(x + cm.left, cy, cs.w, ch));
                x += sizes[i].w + spacing;
            }
            y += line.height + run_spacing;
        }
    }

    /// 自由摆放子节点在 `avail`（父内容区尺寸）下的落点：`None` = 未设摆放；
    /// `Some((左上角, None))` = 只定位（尺寸用测量值）；`Some((左上角, Some(矩形)))` = 整个
    /// 矩形由闭包给出。坐标相对父内容区左上角。
    fn placed_rect(&self, c: NodeId, avail: Size) -> Option<(Point, Option<Rect>)> {
        match self.get(c)?.place.as_ref()? {
            Placement::At(p) => Some((*p, None)),
            Placement::Rect(f) => {
                let r = f(avail);
                Some((Point::new(r.x, r.y), Some(r)))
            }
        }
    }

    fn arrange_frame(&mut self, id: NodeId, inner: Rect) {
        let children = self.visible_children(id);
        for c in children {
            // `place_cache` 只有 `Placement::Rect` 会写（测量期），有就用它，与子树测量同源。
            let placed = match self.get(c).and_then(|n| n.place_cache) {
                Some(r) => Some((Point::new(r.x, r.y), Some(r))),
                None => self.placed_rect(c, Size::new(inner.w, inner.h)),
            };
            match placed {
                Some((p, Some(r))) => {
                    self.arrange(
                        c,
                        Rect::new(inner.x + p.x, inner.y + p.y, r.w.max(0), r.h.max(0)),
                    );
                    continue;
                }
                Some((p, None)) => {
                    let cs = self.measured_of(c);
                    self.arrange(c, Rect::new(inner.x + p.x, inner.y + p.y, cs.w, cs.h));
                    continue;
                }
                None => {}
            }
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
            self.paint_node(canvas, root, Point::new(0, 0), true, true);
        }
        // 锚定浮层最后画，且从根级重新进入——不带任何祖先的裁剪栈，故能盖住滚动
        // 容器的边界与其后的一切兄弟。登记序即层叠序，后登记者压在上面。
        //
        // 收起的浮层若带进出场过渡且离场未完，照画残影：位置沿用最后一次排布（离场期
        // 它已不再参与布局），`paint_node` 据逻辑可见把它往 0 推。
        for &id in &self.overlays {
            let showing = self.overlay_showing(id);
            // 被模态层挡住而收起的不留残影：那是"对话框盖上来了"，残影会画在对话框之上。
            let exiting = !showing
                && self.overlay_in_modal_scope(id)
                && self
                    .get(id)
                    .and_then(|n| n.presence.as_ref())
                    .is_some_and(|p| p.alive());
            if !showing && !exiting {
                continue;
            }
            let (Some(origin), Some(parent)) =
                (self.overlay_origin(id), self.get(id).and_then(|n| n.parent))
            else {
                continue;
            };
            self.paint_node(canvas, id, origin, self.node_enabled(parent), showing);
        }
    }

    /// `parent_shown`：祖先链是否全部**逻辑可见**。祖先在离场时它为假，子孙照画（祖先
    /// 还活着），但其中声明了进出场过渡的节点会据此一起离场。
    fn paint_node(
        &self,
        canvas: &mut dyn Canvas,
        id: NodeId,
        origin: Point,
        parent_enabled: bool,
        parent_shown: bool,
    ) {
        let Some(n) = self.get(id) else {
            return;
        };
        let shown = parent_shown && n.effective_visible();
        // 进出场：进度乘进不透明度、换算成滑动偏移。没声明过渡的节点只看自身可见
        // （祖先离场时它跟着祖先的层一起淡出，不必自己动）。
        let (presence_alpha, slide, zoom) = match &n.presence {
            None if !n.effective_visible() => return,
            None => (1.0, Point::new(0, 0), None),
            Some(p) => {
                let shown = if p.own_only() {
                    n.effective_visible()
                } else {
                    shown
                };
                let progress = p.advance(shown, &crate::theme::current());
                if progress <= 0.0 && !shown {
                    // 离场播完：布局还为它留着位置的话，补一次重排让出来。只补一次：那次重排
                    // 里父容器已不再测量它（不占位的子节点不进测量），标志没人清，留着就会
                    // 每帧再要一次重排，永远回不到空闲。
                    if p.laid_exiting.replace(false) {
                        crate::anim::request_relayout();
                    }
                    return;
                }
                (p.opacity(progress), p.offset(progress), p.zoom(progress))
            }
        };
        // 高度过渡 / 展开类进出场：过渡期每帧重排一次，高度才跟得上进度。须在下面的
        // 空矩形检查之前请求——展开正是从高度 0 起步的。
        let revealing = n.presence.as_ref().is_some_and(|p| p.expanding())
            || n.height_anim.as_ref().is_some_and(|a| a.is_active());
        if revealing {
            crate::anim::request_relayout();
            self.layout_following.borrow_mut().push(id);
        }
        // 有效启用态 = 父链启用 ∧ 自身启用；向下传递实现父禁用子跟随。
        let enabled = parent_enabled && n.own_enabled();
        // 绘制偏移叠加在布局位置之上（见 `Node::offset`）。子节点以 abs 为原点递归，
        // 故父节点的位移自动带着整棵子树走。
        // 布局过渡（FLIP）的视觉偏移：只在绘制时叠加，命中与焦点按最终布局。
        let flip = n
            .layout_anim
            .as_ref()
            .map_or(Point::new(0, 0), |a| a.shift());
        let abs = Rect::new(
            origin.x + n.bounds.x + n.offset.x + slide.x + flip.x,
            origin.y + n.bounds.y + n.offset.y + slide.y + flip.y,
            n.bounds.w,
            n.bounds.h,
        );
        if abs.is_empty() {
            return;
        }
        if let Some(a) = n.layout_anim.as_ref().filter(|a| a.is_active()) {
            // 偏移不进结构签名（否则每帧整窗），脏区自己报：按整段幅度外扩，盖住上一帧位置。
            crate::anim::request_repaint_in(self.subtree_visual(n, abs).inflate(a.extent()));
        }
        if let Some(p) = n.presence.as_ref().filter(|p| p.is_active()) {
            // 过渡中：脏区取整棵子树的视觉范围，再按滑动幅度外扩，盖住上一帧与这一帧两处位置。
            // 缩放只缩小（见 `Presence::scale`），合成不越出原大的范围，无需另算。
            crate::anim::request_repaint_in(self.subtree_visual(n, abs).inflate(p.slide_extent()));
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
            let pad = if n.focused { 3 } else { DAMAGE_MARGIN }.max(shadow_extent(n));
            !abs.inflate(pad).intersect(&cull).is_empty()
        });

        // 子树整体不透明度：<1 时入离屏层，绘完整棵子树后按 opacity 合成回父层。
        // 与自绘剪枝无关——层的 push/pop 必须成对，且子树可能有内容要画。
        // 背景与不透明度的目标值（`bg_when` / `opacity_when` 压过静态样式），声明了
        // `animate_style` 的节点在这里过渡到目标。渐变背景不参与过渡。
        let theme = crate::theme::current();
        // 背景画笔只在要画（自绘可见）或要推进过渡时才解析：渐变画笔每次解析都分配。
        let mut bg_paint = if self_visible || n.style_anim.is_some() {
            n.bg_paint(&theme)
        } else {
            None
        };
        let mut target_opacity = n.effective_opacity();
        if let Some(sa) = &n.style_anim {
            use crate::ui::presence::BgTarget;
            let target = match &bg_paint {
                None => BgTarget::None,
                Some(p) if p.gradient.is_some() => BgTarget::Gradient,
                Some(p) => BgTarget::Solid(p.color),
            };
            let (bg, o) = sa.resolve(target, target_opacity, &theme);
            if !matches!(target, BgTarget::Gradient) {
                bg_paint = bg.map(Paint::fill);
            }
            target_opacity = o;
            if sa.is_active() {
                crate::anim::request_repaint_in(self.subtree_visual(n, abs));
            }
        }
        let opacity = target_opacity * presence_alpha;
        let use_layer = opacity < 1.0 || zoom.is_some();
        if use_layer {
            // 层只开子树实际画得到的范围（含子孙投影与偏移）：一个小浮层淡入不必每帧
            // 分配、合成一整窗的离屏缓冲。
            let visual = self.subtree_visual(n, abs);
            match zoom {
                // 缩放以节点中心为支点：子树先按原大画进层，合成时再缩放。
                Some(z) => canvas.push_layer_scaled(
                    opacity,
                    visual,
                    z,
                    (
                        abs.x as f32 + abs.w as f32 / 2.0,
                        abs.y as f32 + abs.h as f32 / 2.0,
                    ),
                ),
                None => canvas.push_layer_in(opacity, visual),
            }
        }
        // 卷帘：占位高度小于自然高度（高度过渡 / 展开类进出场）时，本节点的自绘连同子树
        // 一起裁到本节点矩形。只裁子树不够——叶子控件（label 当折叠内容）自己就会画出界。
        let reveal_clip = revealing
            || n.presence
                .as_ref()
                .and_then(|p| p.height_factor())
                .is_some_and(|f| f < 1.0);
        if reveal_clip {
            canvas.save();
            canvas.clip_rect(abs);
        }

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
            if let Some(p) = &bg_paint {
                match n.style.corner_radii {
                    Some(c) => canvas.fill_round_rect_corners(fx, fy, fw, fh, c, p),
                    None => canvas.fill_round_rect(fx, fy, fw, fh, radius, p),
                }
            }
            if let Some((bc, bw)) = &n.style.border {
                if *bw > 0 {
                    let bp = Paint::fill(bc.solid_color(&theme));
                    let e = n.style.border_edges;
                    if e.is_all() {
                        // 四边齐全走圆角描边，保住 corner_radius。
                        match n.style.corner_radii {
                            Some(c) => {
                                canvas.stroke_round_rect_corners(fx, fy, fw, fh, c, *bw as f32, &bp)
                            }
                            None => {
                                canvas.stroke_round_rect(fx, fy, fw, fh, radius, *bw as f32, &bp)
                            }
                        }
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
                let ring = Paint::fill(crate::theme::current().palette.accent);
                let (rx, ry, rw, rh) = (fx - 1.0, fy - 1.0, fw + 2.0, fh + 2.0);
                match n.style.corner_radii {
                    // 直角仍保持直角：外扩 1px 只给本来就圆的角加 1。
                    Some(c) => {
                        let grow = |r: f32| if r > 0.0 { r + 1.0 } else { 0.0 };
                        let c = crate::geometry::Corners::new(
                            grow(c.top_left),
                            grow(c.top_right),
                            grow(c.bottom_right),
                            grow(c.bottom_left),
                        );
                        canvas.stroke_round_rect_corners(rx, ry, rw, rh, c, 2.0, &ring);
                    }
                    None => canvas.stroke_round_rect(rx, ry, rw, rh, radius + 1.0, 2.0, &ring),
                }
            }
        }

        let child_origin = Point::new(abs.x, abs.y);
        // 子节点分两趟绘制：先普通、后 `raised`。拖拽浮起的行须画在其余兄弟之上，
        // 否则会被排在它后面的行盖住。绝大多数容器没有 raised 子节点，第二趟是空转。
        if n.clip_children {
            canvas.save();
            canvas.clip_rect(content);
            self.paint_children(canvas, n, child_origin, enabled, shown);
            canvas.restore();
        } else {
            self.paint_children(canvas, n, child_origin, enabled, shown);
        }

        // 滚动条：内容高于视口时在右缘绘制纵向指示条。贴窗口右缘时整体内缩，避开
        // 被 WM_NCHITTEST 判为缩放边框的那一段（否则画得出来、点不着）。
        // 卷帘过渡中视口小于自然内容是暂时的，不是"内容放不下"，不画滚动条。
        if matches!(n.layout, Layout::Scroll) && n.content_h > content.h && !reveal_clip {
            let track_w = scrollbar::track_w();
            let inset = self.scrollbar_edge_inset(abs.right());
            let tx = abs.right() as f32 - track_w - scrollbar::margin() - inset as f32;
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

        if reveal_clip {
            canvas.restore();
        }
        if use_layer {
            canvas.pop_layer();
        }
    }

    /// 绘制子节点：先非 `raised`、后 `raised`，各自保持原有相对顺序（稳定分区）。
    /// 与 [`Tree::hit_node`] 的倒序遍历互为镜像——那边先测 `raised`，两者对"谁在上层"
    /// 的判断必须一致，否则会出现"画在上面却点不到"。
    fn paint_children(
        &self,
        canvas: &mut dyn Canvas,
        n: &Node,
        origin: Point,
        enabled: bool,
        shown: bool,
    ) {
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
                self.paint_node(canvas, c, origin, enabled, shown);
            }
        }
        for &c in &n.children {
            if raised(c) {
                self.paint_node(canvas, c, origin, enabled, shown);
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
    /// 请求交出焦点的节点（`EventCtx::release_focus`）。
    release: Option<NodeId>,
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
    /// 请求重绘本控件连同往上 `levels` 层祖先的视觉矩形（仍是局部失效，不升整窗）。
    ///
    /// 给「改了一个值，跟着变的东西散在同一个复合控件里」的子控件用：它知道自己在复合
    /// 控件里的层级，却拿不到兄弟与祖先的矩形（`Widget` 不访问树）。祖先链不够 `levels`
    /// 层时到根为止。
    pub(crate) fn mark_dirty_with_ancestors(&mut self, levels: usize) {
        let mut r = self.tree.visual_bounds(self.self_id);
        let mut cur = self.self_id;
        for _ in 0..levels {
            let Some(p) = self.tree.get(cur).and_then(|n| n.parent) else {
                break;
            };
            r = r.union(&self.tree.visual_bounds(p));
            cur = p;
        }
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

    /// 跑一个控件转交的**应用回调**：回调里写过信号就升整窗重绘。
    ///
    /// 没有它时错在哪：按键事件里写信号默认只重绘发起者自己（打字高频，见
    /// `call_on_event`），这对「控件改自己的正文」是对的；可应用回调写的多是**别处**读
    /// 的共享状态——查找框 Enter 后更新命中计数、给另一个框挂底色标记——读者不在脏区
    /// 里，画面就停在旧值，直到下一次碰巧整窗重画。指针点击与定时器回调本就按整窗
    /// 处理，这里让按键路径上的应用回调与它们同一口径。回调是 Enter / 导航键这类
    /// 低频动作，不是每次打字都跑，局部帧的收益不受影响。
    pub fn app_callback<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let (r, wrote) = crate::signal::track_writes(|| f(self));
        if wrote {
            self.mark_dirty_all();
        }
        r
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
    /// 交出焦点（网页的 `input.blur()`）：焦点在本节点或其子孙上时清空，否则什么都不做。
    ///
    /// 典型用法是提交后让输入框失焦——数字框按回车提交，插入符却还在里面闪，用户会以为
    /// 没提交。同一轮里别处又要了焦点（`request_focus`、点了别的控件）时那边胜出。
    /// 在 App 级回调（定时器、`on_message`、`on_show`……）里调时节点是根，等于整窗失焦，
    /// 且不通知失焦（App 级回调里的焦点调度都不算用户离开）。菜单项动作的节点是弹出菜单
    /// 的那个控件，按事件回调处理。
    ///
    /// 在事件回调里调：失去焦点的那个控件照常收到失焦通知（`on_blur` / `Element::on_blur`），
    /// 于是祖先替子树交出焦点时，子孙输入框里没提交的编辑也会提交；输入框自己在提交回调里
    /// 交出焦点不会提交两遍——Enter 与失焦共用一个出口，提交过就不再提交。
    /// 在 `on_update` 里调则不通知，与那个相位里转移焦点同理：它由信号变化驱动、发生在
    /// 渲染中途，回调写的信号赶不上本帧重排。
    pub fn release_focus(&mut self) {
        self.out.release = Some(self.self_id);
    }
    /// 当前持有键盘焦点的节点（网页的 `document.activeElement`、Qt 的 `focusWidget()`）。
    ///
    /// 反映的是**本次事件分发开始时**的焦点：同一回调里先调 `request_focus` 再查，拿到的
    /// 仍是旧值——请求要等分发返回后由宿主落地。每次调用扫一遍节点树（O(节点数)），
    /// 事件回调里随便用，别放进 `paint`。
    pub fn focused_node(&self) -> Option<NodeId> {
        self.tree.focused_node()
    }
    /// 焦点是否落在**接受文字输入**的控件上（可编辑文本框、自绘的收字控件）。
    ///
    /// 全局快捷键让路的标准判据：焦点在输入框里时 ←/→、字母键都归它。只读文本框不算
    /// （它不收字），判据见 [`Widget::accepts_text`]。
    pub fn focus_accepts_text(&self) -> bool {
        self.tree
            .focused_node()
            .is_some_and(|f| self.tree.accepts_text(f))
    }
    /// 焦点是否在 `id` 本身或其子孙上（网页的 `el.contains(document.activeElement)`）。
    ///
    /// 给"焦点在这块面板里时才响应"一类判据用：面板根节点 id 存进共享状态，回调里一问即可。
    pub fn focus_within(&self, id: NodeId) -> bool {
        self.tree
            .focused_node()
            .is_some_and(|f| self.tree.is_within(f, id))
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
    /// 本窗口**实际在用**的渲染后端（[`Renderer::Auto`](crate::platform::Renderer::Auto)
    /// 落成了 GPU 还是回退了软光栅、运行中有没有降级），供「关于」页与诊断信息显示。
    pub fn active_renderer(&self) -> crate::platform::ActiveRenderer {
        self.tree.active_renderer
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
    /// `min_width`：最小宽度（0=按内容，兜 140 的默认下限；大于 0 时以它为准）。
    ///
    /// `pos` 按**指针位置**处理：面板错开它几像素弹出，放不下时翻到它的另一侧，保证
    /// 指针不落在面板里（见 API_GUIDE §8.2）。要贴着控件弹（下拉、按钮菜单），用
    /// [`show_dropdown_menu`](Self::show_dropdown_menu)，它以控件矩形为锚、只做上下翻转。
    pub fn show_menu(&mut self, pos: Point, items: Vec<MenuItem>, min_width: i32) {
        let mut req = MenuRequest::new(pos, items);
        req.min_width = min_width;
        self.show_menu_request(req);
    }
    /// 按完整请求弹出浮层菜单，用于便捷方法覆盖不到的组合。例如右键菜单开着时点别处
    /// **只收起、不触发底下的控件**：
    ///
    /// ```
    /// use windui::core::EventCtx;
    /// use windui::prelude::*;
    ///
    /// fn open_tree_menu(ctx: &mut EventCtx, pos: Point, items: Vec<MenuItem>) {
    ///     let mut req = MenuRequest::new(pos, items);
    ///     req.click_through = false;
    ///     ctx.show_menu_request(req);
    /// }
    /// ```
    ///
    /// 按钮弹菜单用 [`MenuRequest::below`]（不遮住按钮），而不是把按钮底边当 `pos` 传给
    /// [`MenuRequest::new`]——后者按指针位置处理，放不下时会翻上来盖住按钮。
    pub fn show_menu_request(&mut self, req: MenuRequest) {
        self.out.menu = Some(req);
        self.out.repaint = true;
    }
    /// 菜单栏专用：展开 `link.current` 那个标题的菜单，面板左上角贴标题左下角，并把
    /// 联动信息交给宿主（展开期间滑到相邻标题即切换、←→ 跨菜单、点标题收起）。
    /// 该标题的项生成器返回空则不弹。
    pub fn show_menu_bar(&mut self, link: crate::event::MenuBarLink) {
        let Some(slot) = link.slots.get(link.current) else {
            return;
        };
        let items = (slot.build)();
        if items.is_empty() {
            return;
        }
        let r = slot.rect;
        self.out.menu = Some(MenuRequest {
            pos: Point::new(r.x, r.y + r.h),
            items,
            min_width: 0,
            anchor_top: Some(r.y),
            rebuild: Some(slot.build.clone()),
            bar: Some(link),
            click_through: true,
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
            bar: None,
            click_through: false,
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
            bar: None,
            click_through: false,
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
    /// `pos` 按指针位置处理，定位规则同 [`show_menu`](Self::show_menu)。
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
    /// 本次分发里请求交出焦点的节点（`EventCtx::release_focus`），按发出顺序。
    pub release: Vec<NodeId>,
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

impl DispatchResult {
    /// 是否带有任何需要宿主落地的副作用（重绘、关窗、焦点、浮层、开窗……）。
    ///
    /// 供 App 级回调（`on_interval` 等）判断"这次什么都没做"，从而不凭空出帧。
    /// 逐字段解构而非 `..`：新增字段时编译器会逼着在这里表态，不会静默漏判。
    pub(crate) fn has_effects(&self) -> bool {
        let DispatchResult {
            repaint,
            damage,
            close,
            close_forced,
            focus,
            release,
            consumed: _, // 只对事件有意义，不是副作用
            menu,
            open_url,
            window_op,
            toast,
            dialog,
            open_windows,
        } = self;
        *repaint
            || !matches!(damage, DamageReq::None)
            || *close
            || *close_forced
            || focus.is_some()
            || !release.is_empty()
            || menu.is_some()
            || open_url.is_some()
            || window_op.is_some()
            || toast.is_some()
            || dialog.is_some()
            || !open_windows.is_empty()
    }
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
    ///
    /// 每一级先看节点上的显式覆盖（`Element::cursor`），再看控件自己声明的。显式覆盖成
    /// `Arrow` 同样算数并就此停下——用来压掉祖先给的手型；而子控件自己声明的形状（输入框
    /// 的 `Text`）仍先于祖先的覆盖，因为回溯是从命中节点往上走的。
    pub fn cursor_at(&self, id: NodeId) -> CursorShape {
        for nid in self.ancestor_chain(id) {
            if let Some(n) = self.get(nid) {
                if let Some(c) = n.cursor {
                    return c;
                }
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
            let Some(text) = n.tooltip.as_ref() else {
                continue;
            };
            // 截断判定按**持有这条提示的那个节点**算，不是按命中的子节点算。
            // 找到提示就到此为止：被自身判定压掉时不再往上找，否则一个没被截断的
            // Label 会转而弹出祖先容器的提示，看着像"弹错了别人的说明"。
            //
            // 只压"正文全文"型提示（与正文相同，截断时才有用）。内容不同的说明型提示
            // 与截不截断无关：曾经一律压掉，单行省略的 Label 配上说明就永远弹不出来。
            let tip = text.resolve();
            if n.widget.text_truncated() == Some(false)
                && n.widget.display_text().as_deref() == Some(&*tip)
            {
                return None;
            }
            return Some(tip.into_owned());
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
    /// 命中点所在的祖先链上是否有 [`Widget::preserves_focus`] 的控件（菜单栏）。
    /// 宿主据此跳过一次失焦裁决。
    pub fn hit_preserves_focus(&self, pos: Point) -> bool {
        let Some(hit) = self.hit_test(pos) else {
            return false;
        };
        self.ancestor_chain(hit)
            .into_iter()
            .any(|id| self.get(id).is_some_and(|n| n.widget.preserves_focus()))
    }

    pub fn hit_inside(&self, pos: Point, id: NodeId) -> bool {
        let Some(hit) = self.hit_test(pos) else {
            return false;
        };
        self.ancestor_chain(hit).contains(&id)
    }

    /// 节点绝对窗口矩形（累加各级父节点偏移）。
    /// 树里第一个**可见**菜单栏的联动信息（见 [`Widget::menu_bar_link`]）。
    ///
    /// 线性扫一遍槽位：只在 F10 / Alt 这类低频按键时调用，不值得为它维护一份登记表——
    /// 登记表还得跟着节点增删同步，而"扫一遍"没有可以失步的状态。
    pub fn menu_bar_link(&self) -> Option<crate::event::MenuBarLink> {
        self.slots
            .iter()
            .filter_map(|s| s.node.as_ref())
            .filter(|n| n.effective_visible())
            .find_map(|n| n.widget.menu_bar_link())
    }
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
        let pad = if n.focused { 3 } else { DAMAGE_MARGIN }.max(shadow_extent(n));
        abs.inflate(pad)
    }

    /// 全树**结构签名**：对每个存活节点哈希
    /// (索引, 代际, 有效可见, 有效启用, 生效前景角色, `bg_when` / `opacity_when` 的值,
    /// bounds, offset, raised)。
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
                // 运行期背景 / 不透明度同理：值变了不改任何 bounds，不进签名就只重画事件
                // 所在的节点，读这两个闭包的节点停在旧值。
                if let Some(f) = &n.bg_cond {
                    let c = f();
                    (c.r, c.g, c.b, c.a).hash(&mut h);
                }
                if n.opacity_cond.is_some() {
                    n.effective_opacity().to_bits().hash(&mut h);
                }
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
    ///
    /// 同一趟遍历顺带落定**不在绘制路径上**的进出场过渡（见 [`PresenceFlags`]）。
    pub fn reset_hidden_interactions(&mut self) {
        if let Some(root) = self.root {
            self.reset_hidden_rec(root, true, PresenceFlags::ROOT);
        }
    }

    fn reset_hidden_rec(&mut self, id: NodeId, parent_interactive: bool, pf: PresenceFlags) {
        let (interactive, children, transitioned, child_pf, offscreen) = match self.get(id) {
            Some(n) => {
                let v = parent_interactive && n.effective_visible() && n.own_enabled();
                let prev = n.prev_visible.replace(v);
                if !pf.painted || !n.layout_visible() {
                    // 不在绘制路径上也就不被测量、不被绘制：期间目标变了，露出来时直接落定。
                    if let Some(a) = &n.height_anim {
                        a.unprime();
                    }
                    if let Some(a) = &n.style_anim {
                        a.unprime();
                    }
                }
                if let Some(p) = &n.presence {
                    if n.overlay.is_some() {
                        // 浮层不走普通绘制递归，由 `Tree::paint` 末尾单独画：锚点链不在绘制
                        // 路径上（切走的标签页）或被模态挡住时，它画出来的只会是"离场残影"——
                        // 落定为隐藏。不能套下面的规则：普通隐藏的祖先把 `shown` 重置为真、
                        // 展开信号又还开着，会落定成"显示"，每次结构变化都整块闪一下。
                        if !pf.painted || !self.overlay_in_modal_scope(id) {
                            p.settle(false);
                        }
                    } else if !pf.painted {
                        let own = n.effective_visible();
                        p.settle(if p.own_only() { own } else { pf.shown && own });
                    }
                }
                let offscreen = !pf.painted || !n.layout_visible();
                (v, n.children.clone(), prev && !v, pf.child(n), offscreen)
            }
            None => return,
        };
        if transitioned || offscreen {
            if let Some(n) = self.get_mut(id) {
                if transitioned {
                    n.widget.reset_interaction();
                }
                if offscreen {
                    n.widget.settle_offscreen();
                }
            }
        }
        for c in children {
            self.reset_hidden_rec(c, interactive, child_pf);
        }
    }

    /// 子树的视觉范围（本节点矩形 + 投影 / 焦点环余量，并上所有仍在画的子孙，含其
    /// `offset`）。`abs` 为本节点当前的绘制矩形。进出场过渡期间的脏区按它报：子孙的投影、
    /// 偏移都可能伸出本节点矩形，只按本节点报，局部帧里伸出去的那部分就成了残影。
    fn subtree_visual(&self, n: &Node, abs: Rect) -> Rect {
        let pad = if n.focused { 3 } else { DAMAGE_MARGIN }.max(shadow_extent(n));
        let mut r = abs.inflate(pad);
        for &c in &n.children {
            let Some(cn) = self
                .get(c)
                .filter(|cn| cn.layout_visible() && cn.overlay.is_none())
            else {
                continue;
            };
            let ca = Rect::new(
                abs.x + cn.bounds.x + cn.offset.x,
                abs.y + cn.bounds.y + cn.offset.y,
                cn.bounds.w,
                cn.bounds.h,
            );
            // 子孙自己的进出场滑动（祖先离场时它们一起滑出）与布局过渡偏移都不在 `offset`
            // 里，按幅度外扩。
            let slide = cn
                .presence
                .as_ref()
                .filter(|p| p.alive())
                .map_or(0, |p| p.slide_extent())
                + cn.layout_anim.as_ref().map_or(0, |a| a.extent());
            r = r.union(&self.subtree_visual(cn, ca).inflate(slide));
        }
        r
    }

    /// 节点是否接受输入法输入（见 `Widget::accepts_ime`）。
    pub fn accepts_ime(&self, id: NodeId) -> bool {
        self.get(id).is_some_and(|n| n.widget.accepts_ime())
    }

    /// 节点是否收文字输入（见 `Widget::accepts_text`）。
    pub fn accepts_text(&self, id: NodeId) -> bool {
        if let Some((_, answer)) = self.held_accepts_text.filter(|&(held, _)| held == id) {
            return answer;
        }
        self.get(id).is_some_and(|n| n.widget.accepts_text())
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

    /// 读节点交给输入法的内容类型（见 `Widget::ime_hints`）。
    pub fn ime_hints_of(&self, id: NodeId) -> Option<crate::event::ImeHints> {
        Some(self.get(id)?.widget.ime_hints())
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
        let key_local = widget.keys_damage_self_only();
        let prev_held = self.held_accepts_text.replace((id, widget.accepts_text()));
        let mut ctx = EventCtx {
            tree: self,
            self_id: id,
            out: EventOutcome::default(),
        };
        // 括起事件期：期间 Signal::set 仅记"写过信号"，不强制整窗。
        crate::signal::begin_event();
        let consumed = widget.on_event(&mut ctx, ev);
        let mut out = ctx.out;
        self.held_accepts_text = prev_held;
        // 事件内写过信号但控件未显式 mark_dirty → 据事件类型选择失效强度：
        // - Move(hover)：写的是自身悬停态，局部重绘即可；
        // - `keys_damage_self_only` 控件的 Key：文本框打字高频、写的是自己的正文，保留局部
        //   重绘避免整窗卡顿（它转交给应用的回调另经 `EventCtx::app_callback` 升整窗）；
        // - 其余（指针 Down/Up/Click、按钮 / 开关 / 列表的键盘激活与导航）：可能写跨控件
        //   共享状态（计数器、状态文案、enabled_when 门控），升 Layout 使 apply_damage
        //   直接置 needs_full，覆盖所有读者。键盘激活按钮与鼠标点它是同一件事，失效口径
        //   不该因输入设备不同而分叉——曾经分叉过：Tab 到按钮按空格，回调改的文案不刷新。
        if crate::signal::end_event() {
            let r = self.visual_bounds(id);
            let local = match ev {
                Event::Pointer(ref pe) => pe.kind == crate::event::PointerKind::Move,
                Event::Key(_) => key_local,
            };
            let d = if local {
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
                res.release.extend(o.release);
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
                                bar: None,
                                click_through: true,
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
            release: o.release.into_iter().collect(),
            consumed: false,
            menu: o.menu,
            open_url: o.open_url,
            window_op: o.window_op,
            toast: o.toast,
            dialog: o.dialog,
            open_windows: o.open_windows,
        }
    }

    /// 通知 `id` 用户让它失焦了（见 [`Widget::on_blur`]），返回其回调的副作用。
    ///
    /// 节点已删除、被禁用或不可见（含祖先）时不通知：那是结构变化顺带丢了焦点，
    /// 不是用户离开——同一次点击里"隐藏编辑框并把焦点给别人"也算这一类。
    ///
    /// 回调期间写过信号即升整窗：失焦提交写的多是别处读的共享状态（改名后的标题、
    /// 列表项），与指针 Down/Up 同一档（见 `call_on_event`）。
    pub(crate) fn dispatch_blur(&mut self, id: NodeId) -> DispatchResult {
        let shown = self
            .ancestor_chain(id)
            .iter()
            .all(|&a| self.get(a).is_some_and(|n| n.effective_visible()));
        if !shown || !self.node_enabled(id) {
            return DispatchResult::default();
        }
        let Some(n) = self.get_mut(id) else {
            return DispatchResult::default();
        };
        let mut widget = std::mem::replace(&mut n.widget, Box::new(EmptyWidget));
        let mut callback = n.on_blur.take();
        let prev_held = self.held_accepts_text.replace((id, widget.accepts_text()));
        let mut ctx = EventCtx {
            tree: self,
            self_id: id,
            out: EventOutcome::default(),
        };
        crate::signal::begin_event();
        widget.on_blur(&mut ctx);
        if let Some(f) = callback.as_mut() {
            f(&mut ctx);
        }
        let mut o = ctx.out;
        self.held_accepts_text = prev_held;
        if crate::signal::end_event() {
            o.damage = o.damage.merge(DamageReq::Full);
            o.repaint = true;
        }
        match self.get_mut(id) {
            Some(n) => {
                n.widget = widget;
                n.on_blur = callback;
            }
            None => debug_assert!(
                false,
                "on_blur 回调内删除了 self 节点，违反与 call_on_event 相同的契约"
            ),
        }
        DispatchResult {
            repaint: o.repaint,
            damage: o.damage,
            close: o.close,
            close_forced: o.close_forced,
            focus: o.focus,
            release: o.release.into_iter().collect(),
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
            res.release.extend(o.release);
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
            res.release.extend(out.release);
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
    /// 焦点环里第一个**尚未兑现**的 autofocus 节点。
    ///
    /// [`Autofocus::Take`] 类优先：它们是被用户动作唤出来的（就地编辑框），比"给个
    /// 归宿"式的兜底更该拿到焦点，而焦点环顺序是按布局排的、与这份轻重无关。
    pub fn first_autofocus(&self, order: &[NodeId]) -> Option<(NodeId, Autofocus)> {
        let pending = |id: &NodeId| {
            self.get(*id)
                .filter(|n| !n.autofocus_done)
                .and_then(|n| n.autofocus)
                .map(|a| (*id, a))
        };
        order
            .iter()
            .find_map(|id| pending(id).filter(|(_, a)| a.takes_focus()))
            .or_else(|| order.iter().find_map(pending))
    }

    /// 为新节点建定向送焦点触发器。这个信号兑现过（或有别的节点先登记过）就沿用树上记的
    /// 值——同一次事件里加了计数又重建了节点，新节点照样认这次触发；第一次见到才取当前值，
    /// 故初值本身不触发。
    pub(crate) fn focus_trigger(
        &mut self,
        signal: Signal<u64>,
        select: FocusSelect,
    ) -> FocusTrigger {
        if self.focus_seen.len() >= 64 {
            self.focus_seen.retain(|s, _| s.is_alive());
        }
        let now = signal.try_get().unwrap_or(0);
        let seen = *self.focus_seen.entry(signal).or_insert(now);
        FocusTrigger {
            signal,
            seen,
            select,
        }
    }

    /// 取出焦点环里触发器已变值的节点，连同兑现时对选区做什么。环里所有已变值的触发器
    /// 都记为已兑现；不在环里的（隐藏、禁用、被模态遮住）不记，等它进环那一帧再兑现。
    ///
    /// 返回的列表里**要焦点的只留第一个**（同一轮只可能满足一个，其余的不该留到下一帧
    /// 再抢）；不要焦点的（`RangeNoFocus`）互不冲突，全部保留。
    pub(crate) fn take_fired_focus_triggers(
        &mut self,
        order: &[NodeId],
    ) -> Vec<(NodeId, FocusSelect)> {
        let mut fired: Vec<(NodeId, FocusSelect)> = Vec::new();
        for &id in order {
            let Some(t) = self.get_mut(id).and_then(|n| n.focus_when.as_mut()) else {
                continue;
            };
            let Some(v) = t.signal.try_get() else {
                continue;
            };
            if v != t.seen {
                t.seen = v;
                let (signal, select) = (t.signal, t.select);
                self.focus_seen.insert(signal, v);
                let focus_taken = fired.iter().any(|(_, f)| f.takes_focus());
                if !(select.takes_focus() && focus_taken) {
                    fired.push((id, select));
                }
            }
        }
        fired
    }

    /// 为新节点建滚进视口触发器（规则同 [`Tree::focus_trigger`]：初值不触发，
    /// 同一次事件里加了计数又重建了节点，新节点照样认这次触发）。
    pub(crate) fn reveal_trigger(&mut self, signal: Signal<u64>) -> RevealTrigger {
        if self.reveal_seen.len() >= 64 {
            self.reveal_seen.retain(|s, _| s.is_alive());
        }
        let now = signal.try_get().unwrap_or(0);
        let seen = *self.reveal_seen.entry(signal).or_insert(now);
        RevealTrigger { signal, seen }
    }

    /// 取出触发器已变值、且此刻在布局里（自身与祖先都可见）的节点，记为已兑现。
    /// 不可见的不记，等它出现那一帧再兑现。
    pub(crate) fn take_fired_reveal_triggers(&mut self) -> Vec<NodeId> {
        if self.reveal_seen.is_empty() {
            return Vec::new();
        }
        let ids: Vec<NodeId> = self
            .slots
            .iter()
            .enumerate()
            .filter_map(|(i, slot)| {
                let t = slot.node.as_ref()?.reveal_when.as_ref()?;
                (t.signal.try_get()? != t.seen).then_some(NodeId {
                    index: i as u32,
                    generation: slot.generation,
                })
            })
            .collect();
        let mut fired = Vec::new();
        for id in ids {
            if !self.shown_in_tree(id) {
                continue;
            }
            let Some(t) = self.get_mut(id).and_then(|n| n.reveal_when.as_mut()) else {
                continue;
            };
            let Some(v) = t.signal.try_get() else {
                continue;
            };
            t.seen = v;
            let signal = t.signal;
            self.reveal_seen.insert(signal, v);
            fired.push(id);
        }
        fired
    }

    /// 节点自身与所有祖先都有效可见。
    fn shown_in_tree(&self, id: NodeId) -> bool {
        let mut cur = Some(id);
        while let Some(c) = cur {
            let Some(n) = self.get(c) else {
                return false;
            };
            if !n.effective_visible() {
                return false;
            }
            cur = n.parent;
        }
        true
    }

    /// 标记某节点的 autofocus 已兑现。
    pub fn mark_autofocus_done(&mut self, id: NodeId) {
        if let Some(n) = self.get_mut(id) {
            n.autofocus_done = true;
        }
    }

    /// 清掉所有节点的"已兑现"标记（窗口重新唤起时与宿主的全局标志一起复位）。
    pub fn clear_autofocus_done(&mut self) {
        for slot in self.slots.iter_mut() {
            if let Some(n) = slot.node.as_mut() {
                n.autofocus_done = false;
            }
        }
    }

    /// 当前持有键盘焦点的节点（网页的 `document.activeElement`）；无焦点时为 `None`。
    ///
    /// 按节点上的焦点标记现扫一遍：宿主的焦点状态与节点标记由 `set_focused` 同步维护，
    /// 另存一份就多一处可能漂移的副本，而查询是低频操作（快捷键判据、命令派发）。
    /// 代价是 O(节点数)：别在 `paint` 这类每帧都跑的路径里调。
    pub fn focused_node(&self) -> Option<NodeId> {
        self.slots.iter().enumerate().find_map(|(i, slot)| {
            let n = slot.node.as_ref()?;
            n.focused.then_some(NodeId {
                index: i as u32,
                generation: slot.generation,
            })
        })
    }

    pub fn set_focused(&mut self, id: Option<NodeId>, old: Option<NodeId>) {
        if let Some(o) = old.filter(|&o| Some(o) != id) {
            if let Some(n) = self.get_mut(o) {
                n.focused = false;
                n.widget.focus_lost();
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

/// 流式布局的一行：子节点下标区间 `[start, end)` 与行宽、行高（均含 margin）。
struct FlowLine {
    start: usize,
    end: usize,
    width: i32,
    height: i32,
}

/// 按 `limit` 宽折行。每行至少放一个子节点（单个超宽的不再折，交给它自己截断）。
fn flow_lines(sizes: &[Size], limit: i32, spacing: i32) -> Vec<FlowLine> {
    let mut lines: Vec<FlowLine> = Vec::new();
    for (i, s) in sizes.iter().enumerate() {
        match lines.last_mut() {
            Some(l) if l.width + spacing + s.w <= limit => {
                l.end = i + 1;
                l.width += spacing + s.w;
                l.height = l.height.max(s.h);
            }
            _ => lines.push(FlowLine {
                start: i,
                end: i + 1,
                width: s.w,
                height: s.h,
            }),
        }
    }
    lines
}

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

    /// 流式容器：放不下就折行，容器高度随行数增长；每行按行高纵向居中。
    #[test]
    fn flow_wraps_children_onto_new_lines() {
        let leaf = |w, h| Element::leaf().width(w).height(h);
        let tree = layout(
            Element::col().width(200).child(
                Element::flow()
                    .width_match()
                    .spacing(10)
                    .run_spacing(5)
                    .child(leaf(80, 20))
                    .child(leaf(80, 30))
                    .child(leaf(80, 20)),
            ),
            200,
            400,
        );
        let root = tree.root.unwrap();
        let flow = tree.get(root).unwrap().children[0];
        let kids = tree.get(flow).unwrap().children.clone();
        let b = |i: usize| tree.get(kids[i]).unwrap().bounds;
        // 第一行 80 + 10 + 80 = 170 ≤ 200；第三个放不下，折到第二行。
        assert_eq!((b(0).x, b(1).x, b(2).x), (0, 90, 0));
        assert_eq!(b(0).y, 5, "矮的那个在 30 高的行里居中");
        assert_eq!(b(2).y, 30 + 5);
        assert_eq!(tree.get(flow).unwrap().bounds.h, 30 + 5 + 20);
    }

    /// `as_u64` 是给平台层当「焦点控件身份」比较用的：槽位被复用后必须不同，否则删掉一个
    /// 输入框、新建的恰好落在同一槽位时，输入法以为焦点没换。
    #[test]
    fn node_identity_changes_when_a_slot_is_reused() {
        let mut tree = Tree::new();
        let a = Element::leaf().build(&mut tree);
        tree.remove(a);
        let b = Element::leaf().build(&mut tree);
        assert_eq!(format!("{a:?}"), "#0");
        assert_eq!(format!("{b:?}"), "#0g1", "同一槽位被复用");
        assert_ne!(a.as_u64(), b.as_u64());
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

    /// 定向送焦点的已兑现值表在变大时清掉已回收的信号，不随重建闭包里新建的触发信号无限增长。
    #[test]
    fn focus_seen_drops_disposed_signals() {
        let mut tree = Tree::new();
        for _ in 0..64 {
            let s = crate::signal::signal(0u64);
            tree.focus_trigger(s, FocusSelect::Keep);
            s.dispose();
        }
        assert_eq!(tree.focus_seen.len(), 64, "前提：未到阈值时不清");
        let live = crate::signal::signal(0u64);
        tree.focus_trigger(live, FocusSelect::Keep);
        assert_eq!(tree.focus_seen.len(), 1, "到阈值时清掉已回收的，只剩活着的");
    }

    /// `Element::cursor` 覆盖：按钮挂手型生效；挂在容器上，内部 label 继承、输入框仍是
    /// I 形；子节点显式挂 `Arrow` 能压掉 clickable 祖先给的手型。
    #[test]
    fn explicit_cursor_overrides_in_ancestor_walk() {
        let s = crate::signal::signal(String::new());
        let tree = layout(
            Element::col()
                .width(200)
                .height(120)
                .child(
                    Element::button("保存")
                        .width(80)
                        .height(30)
                        .cursor(CursorShape::Hand),
                )
                .child(
                    Element::col()
                        .width(200)
                        .height(60)
                        .cursor(CursorShape::SizeAll)
                        .child(Element::label("标题").width(60).height(20))
                        .child(Element::text_input(s, "").width(100).height(30)),
                )
                .child(
                    Element::row()
                        .width(200)
                        .height(30)
                        .clickable()
                        .child(
                            Element::label("箭头")
                                .width(60)
                                .height(20)
                                .cursor(CursorShape::Arrow),
                        )
                        .child(Element::label("手型").width(60).height(20)),
                ),
            200,
            120,
        );
        let at = |x, y| tree.cursor_at(tree.hit_test(Point::new(x, y)).expect("应命中"));
        assert_eq!(at(10, 10), CursorShape::Hand, "按钮挂了手型");
        assert_eq!(
            at(10, 35),
            CursorShape::SizeAll,
            "容器内 label 继承容器的覆盖"
        );
        assert_eq!(
            at(10, 65),
            CursorShape::Text,
            "输入框自己的 I 形先于祖先覆盖"
        );
        assert_eq!(
            at(10, 95),
            CursorShape::Arrow,
            "显式 Arrow 压掉 clickable 的手型"
        );
        assert_eq!(
            at(70, 95),
            CursorShape::Hand,
            "对照：同一行未覆盖的 label 仍继承手型"
        );
    }

    /// 同一份 margin 在 Linear / Frame / Scroll 三种容器里必须是同一个语义。
    ///
    /// 曾经只有 Linear 预扣：`stack` 里一个 `width_match` + `margin_xy(4,0)`、默认
    /// Start 对齐的子会量到整条 200、再右移 4，右沿落到 204。Stretch 那条路一直是对的，
    /// 所以这个溢出只在默认对齐下出现，很难被看见。
    #[test]
    fn frame_cross_margin_dont_overflow() {
        for (name, el) in [
            (
                "start（默认对齐）",
                Element::leaf().width_match().height(20).margin_xy(4, 0),
            ),
            (
                "stretch",
                Element::leaf()
                    .width_match()
                    .height(20)
                    .margin_xy(4, 0)
                    .align(Align::Stretch),
            ),
        ] {
            let tree = layout(Element::stack().width(200).height(60).child(el), 200, 60);
            let root = tree.root.unwrap();
            let kid = tree.get(root).unwrap().children[0];
            let b = tree.get(kid).unwrap().bounds;
            assert_eq!((b.x, b.w), (4, 192), "{name}：应为 x=4 w=200-4-4");
            assert_eq!(b.x + b.w, 196, "{name}：右沿须留出右 margin，不得溢到 204");
        }
    }

    /// 滚动容器：measure 与 arrange 必须按**同一个**宽度看子节点。
    ///
    /// `arrange_scroll` 一直扣着 `cm.horizontal()`，measure 侧却没扣。两边不一致时
    /// 子树按更宽的宽度算高、按更窄的宽度排版，换行文本会少算一行、底部被裁。
    #[test]
    fn scroll_measures_child_at_the_width_it_will_be_arranged_at() {
        let tree = layout(
            Element::scroll()
                .width(200)
                .height(400)
                .child(Element::leaf().width_match().height(20).margin_xy(4, 0)),
            200,
            400,
        );
        let root = tree.root.unwrap();
        let kid = tree.get(root).unwrap().children[0];
        let b = tree.get(kid).unwrap().bounds;
        assert_eq!((b.x, b.w), (4, 192));
        // 测量值（measured_of）与最终排版宽度必须一致，否则高度是按另一个宽度算的
        assert_eq!(
            tree.measured_of(kid).w,
            b.w,
            "measure 与 arrange 的宽度必须一致：不一致时高度按错的宽度算"
        );
    }

    /// row 方向（交叉轴是纵向）的对称用例——之前只测了 col。
    #[test]
    fn cross_axis_margin_also_applies_on_the_vertical_cross_axis() {
        let tree = layout(
            Element::row()
                .width(200)
                .height(60)
                .child(Element::leaf().height_match().width(20).margin_xy(0, 6)),
            200,
            60,
        );
        let root = tree.root.unwrap();
        let kid = tree.get(root).unwrap().children[0];
        let b = tree.get(kid).unwrap().bounds;
        assert_eq!((b.y, b.h), (6, 48), "纵向交叉轴同样要扣上下 margin");
    }

    /// margin 大于可用空间时收敛到 0，不得算出负尺寸（钉住那几处 `.max(0)`）。
    #[test]
    fn margin_larger_than_available_collapses_to_zero_not_negative() {
        for el in [
            Element::col().width(10).height(40),
            Element::stack().width(10).height(40),
            Element::scroll().width(10).height(40),
        ] {
            let tree = layout(
                el.child(Element::leaf().width_match().height(20).margin_xy(40, 0)),
                10,
                40,
            );
            let root = tree.root.unwrap();
            let kid = tree.get(root).unwrap().children[0];
            let b = tree.get(kid).unwrap().bounds;
            assert!(b.w >= 0, "宽度不得为负，实为 {}", b.w);
            assert_eq!(b.w, 0, "margin 吃光可用空间时应收敛到 0");
        }
    }

    #[test]
    fn cross_axis_match_with_margin_dont_overflow() {
        // 交叉轴的对称情形：col 里的 `width_match` 子节点带左右 margin。
        // 曾经交叉轴不预扣 margin——子量到整条 200，arrange 再右移 4，
        // 右沿落到 204，把紧邻的分隔线压在身下（面板卡片就是这么盖住分栏拖柄的）。
        let tree = layout(
            Element::col()
                .width(200)
                .height(60)
                .child(Element::leaf().width_match().height(20).margin_xy(4, 0)),
            200,
            60,
        );
        let root = tree.root.unwrap();
        let kid = tree.get(root).unwrap().children[0];
        let b = tree.get(kid).unwrap().bounds;
        assert_eq!(b.x, 4, "左沿=margin");
        assert_eq!(b.w, 192, "宽应为 200-4-4");
        assert_eq!(b.x + b.w, 196, "右沿须留出右 margin，不得溢到 204");
    }

    #[test]
    fn cross_axis_match_with_margin_dont_overflow_under_weight() {
        // 同上，但父容器的这个子是**权重子**（第二遍分配那条路径）。
        let tree = layout(
            Element::col()
                .width(200)
                .height(60)
                .child(Element::leaf().width_match().margin_xy(4, 0).weight(1.0)),
            200,
            60,
        );
        let root = tree.root.unwrap();
        let kid = tree.get(root).unwrap().children[0];
        let b = tree.get(kid).unwrap().bounds;
        assert_eq!(b.x, 4, "左沿=margin");
        assert_eq!(b.w, 192, "权重子的交叉轴同样要扣 margin");
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

    /// 运行期权重：改信号即改分配，无需重建子树——可拖动分栏的根基。
    #[test]
    fn weight_fn_reallocates_when_signal_changes() {
        let ratio = crate::signal::signal(0.25f32);
        let row = Element::row()
            .width(400)
            .height(10)
            .child(Element::leaf().weight_when(move || ratio.get()))
            .child(Element::leaf().weight_when(move || 1.0 - ratio.get()));
        let mut tree = Tree::new();
        let id = row.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(400, 10), &mut te);
        let kids = tree.get(id).unwrap().children.clone();
        assert_eq!(tree.abs_bounds(kids[0]).w, 100);
        assert_eq!(tree.abs_bounds(kids[1]).w, 300);

        ratio.set(0.5);
        tree.layout_root(Size::new(400, 10), &mut te);
        assert_eq!(tree.abs_bounds(kids[0]).w, 200, "改信号后重排即生效");
        assert_eq!(tree.abs_bounds(kids[1]).w, 200);
    }

    /// 分隔条：按下捕获、拖动改比例、松开释放；比例被钳在上下限内。
    #[test]
    fn split_handle_drag_updates_ratio() {
        let ratio = crate::signal::signal(0.5f32);
        let split = Element::split(Axis::Horizontal, Element::leaf(), Element::leaf(), ratio)
            .width(406)
            .height(100);
        let mut tree = Tree::new();
        let id = split.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(406, 100), &mut te);
        let kids = tree.get(id).unwrap().children.clone();
        let handle = kids[1];
        let hb = tree.abs_bounds(handle);
        assert_eq!((hb.x, hb.w), (200, 6), "分隔条落在两栏之间、厚 6px");

        let (mut h, mut cap) = (None, None);
        let x = hb.x + 3;
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, Point::new(x, 50), MouseButton::Left),
            &mut h,
            &mut cap,
        );
        assert_eq!(cap, Some(handle), "按下应捕获分隔条");
        // 拖到 x=103：分隔条中心 103 → 第一栏 100px / 可分配 400px = 0.25
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, Point::new(103, 50), MouseButton::Left),
            &mut h,
            &mut cap,
        );
        assert!((ratio.get() - 0.25).abs() < 1e-3, "ratio = {}", ratio.get());
        tree.layout_root(Size::new(406, 100), &mut te);
        assert_eq!(tree.abs_bounds(kids[0]).w, 100);
        assert_eq!(tree.abs_bounds(kids[2]).w, 300);

        // 拖出下限：钳在 0.1
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, Point::new(-50, 50), MouseButton::Left),
            &mut h,
            &mut cap,
        );
        assert!((ratio.get() - 0.1).abs() < 1e-3);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, Point::new(-50, 50), MouseButton::Left),
            &mut h,
            &mut cap,
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
                        TabItem::new("甲").enabled(signal(first_enabled)),
                        Element::label("page A"),
                    ),
                    (TabItem::new("乙"), Element::label("page B")),
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

    /// 按钮的激活键是**裸** Enter / 空格；带 Ctrl 的不激活，也不消费。
    ///
    /// 没有后半条时错在哪：Ctrl+Enter 是应用级的提交通道，而表单里焦点常常停在某个
    /// 按钮上（Tab 过去的、点过「粘贴」的）。按钮若照吃不误，用户按 Ctrl+Enter 提交
    /// 得到的是「触发了焦点所在的取消按钮」——表单没提交，反倒关掉了。
    #[test]
    fn button_activates_on_bare_enter_but_lets_ctrl_enter_pass() {
        let clicks = signal(0);
        let (mut tree, btn, _) = overlay_tree(clicks, false);
        let key = |ctrl| KeyEvent {
            key: Key::Enter,
            pressed: true,
            shift: false,
            ctrl,
            alt: false,
            meta: false,
        };
        assert!(tree.dispatch_key(key(false), Some(btn)).consumed);
        assert_eq!(clicks.get(), 1, "裸 Enter 应激活按钮");
        let res = tree.dispatch_key(key(true), Some(btn));
        assert!(!res.consumed, "Ctrl+Enter 不该被按钮消费");
        assert_eq!(clicks.get(), 1, "Ctrl+Enter 不该触发 on_click");
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

    /// `place(x, y)`：子节点左上角落在 stack 内容区的坐标上（含 padding 偏移），尺寸照常；
    /// 不设尺寸的 stack 撑到包住所有子节点的右下角。
    #[test]
    fn place_positions_children_and_wraps_the_stack() {
        let root = Element::col().width(300).height(200).child(
            Element::stack()
                .padding(4)
                .child(Element::leaf().size(30, 40).place(10, 20))
                .child(Element::leaf().size(20, 20).place(50, 5)),
        );
        let tree = layout(root, 300, 200);
        let stack = tree.get(tree.root.unwrap()).unwrap().children[0];
        let kids = tree.get(stack).unwrap().children.clone();
        assert_eq!(tree.abs_bounds(kids[0]), Rect::new(14, 24, 30, 40));
        assert_eq!(tree.abs_bounds(kids[1]), Rect::new(54, 9, 20, 20));
        // 内容 70×60（最右 50+20、最低 20+40）+ 四边 padding。
        assert_eq!(tree.abs_bounds(stack), Rect::new(0, 0, 78, 68));
    }

    /// `place_fn`：几何由闭包按父内容区尺寸算，读信号即运行期可改宽高——卡片板「框宽 =
    /// 半幅、拖动改高」正是这条。改信号后重排，新的矩形与点击命中都跟上。
    #[test]
    fn place_fn_geometry_follows_signal_and_stays_clickable() {
        let card_h = signal(50);
        let hits = signal(0u32);
        let root = Element::col().width(400).height(300).child(
            Element::stack().width_match().height(300).child(
                Element::button("卡片")
                    .on_click(move |_| hits.update(|n| *n += 1))
                    .place_fn(move |avail| {
                        let w = avail.w / 2;
                        Rect::new(avail.w - w, 30, w, card_h.get())
                    }),
            ),
        );
        let mut tree = layout(root, 400, 300);
        let stack = tree.get(tree.root.unwrap()).unwrap().children[0];
        let card = tree.get(stack).unwrap().children[0];
        assert_eq!(tree.abs_bounds(card), Rect::new(200, 30, 200, 50));
        card_h.set(120);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(400, 300), &mut te);
        assert_eq!(
            tree.abs_bounds(card),
            Rect::new(200, 30, 200, 120),
            "高度随信号"
        );
        // 点新长出来的那段（原高 50 之外）：命中必须落到卡片上。
        let p = Point::new(300, 140);
        let (mut h, mut cap) = (None, None);
        tree.dispatch_pointer(ptr(PointerKind::Down, p), &mut h, &mut cap);
        tree.dispatch_pointer(ptr(PointerKind::Up, p), &mut h, &mut cap);
        assert_eq!(hits.get(), 1, "点到长高后的卡片下段应触发点击");
    }

    /// 不设宽的 stack（Wrap 宽，测量期可用量 ≠ 排布期内容区，AGENTS §5）：闭包只在测量期
    /// 算一次，排布期复用——否则「半宽」在排布期会按 stack 包出来的宽度再折半，子节点被
    /// 排成测量时的一半宽，按测量宽排好的文字与布局缓存全部错位。
    #[test]
    fn place_fn_in_wrap_width_stack_keeps_measured_geometry() {
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let k = calls.clone();
        let root = Element::col()
            .width(400)
            .height(100)
            .child(
                Element::stack().child(Element::leaf().place_fn(move |avail| {
                    k.set(k.get() + 1);
                    Rect::new(0, 0, avail.w / 2, 50)
                })),
            );
        let tree = layout(root, 400, 100);
        let stack = tree.get(tree.root.unwrap()).unwrap().children[0];
        let leaf = tree.get(stack).unwrap().children[0];
        assert_eq!(
            tree.abs_bounds(stack).w,
            200,
            "stack 包住测量期的 200 宽子节点"
        );
        assert_eq!(
            tree.abs_bounds(leaf),
            Rect::new(0, 0, 200, 50),
            "排布期须沿用测量期的矩形，而不是按包出来的 200 宽再折半"
        );
        assert_eq!(calls.get(), 1, "一次布局只调一次闭包");
    }

    /// 放进滚动容器的 stack 不设高：测量期闭包拿到的 `avail.h` 是 0，stack 高度由最低的
    /// 子节点撑出，滚动容器的内容总高随之成立。
    #[test]
    fn placed_children_give_scroll_content_height() {
        let seen_h = std::rc::Rc::new(std::cell::Cell::new(-1));
        let s = seen_h.clone();
        let root =
            Element::scroll()
                .width(200)
                .height(100)
                .child(
                    Element::stack()
                        .width_match()
                        .child(Element::leaf().place_fn(move |avail| {
                            if s.get() < 0 {
                                s.set(avail.h);
                            }
                            Rect::new(0, 500, avail.w, 100)
                        })),
                );
        let tree = layout(root, 200, 100);
        let sc = tree.root.unwrap();
        assert_eq!(seen_h.get(), 0, "父高度不定时测量期 avail.h 为 0");
        assert_eq!(
            tree.get(sc).unwrap().content_h,
            600,
            "内容总高 = 最低子节点底边"
        );
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

    /// `show_value` 时轨道只占左侧（右侧让给百分比标签），指针换算须与绘制同一条轨道：按在
    /// 轨道视觉右端应到 100%、视觉中点应到 50%（#11：曾按整宽换算，到头还差 44px）。
    /// 显式宽与 Wrap 宽各测一次（AGENTS §5）；大字号下标签放宽，换算须跟着变。
    #[test]
    fn slider_show_value_maps_pointer_on_visual_track() {
        use crate::ui::inputs::{value_label_w, KNOB_R, VALUE_LABEL_W};
        for (font, explicit_w) in [
            (None, Some(200)),
            (None, None),
            (Some(24.0), Some(200)),
            (Some(24.0), None),
        ] {
            let mut te = crate::text::PlatformTextEngine::default();
            te.set_scale(1.0);
            let label_w = match font {
                None => VALUE_LABEL_W,
                Some(sz) => {
                    let tw = te.measure("100%", &crate::text::TextStyle::new(sz), None).w;
                    assert!(
                        tw + 2 > VALUE_LABEL_W,
                        "前提：{sz} 号字的 100% 应放不下 44px"
                    );
                    value_label_w(tw)
                }
            };
            let v = signal(0.0f32);
            let mut sl = Element::slider(v).show_value(true);
            if let Some(sz) = font {
                sl = sl.font_size(sz);
            }
            if let Some(w) = explicit_w {
                sl = sl.width(w);
            }
            let mut tree = Tree::new();
            let id = Element::col()
                .width(300)
                .height(40)
                .child(sl)
                .build(&mut tree);
            tree.root = Some(id);
            tree.layout_root(Size::new(300, 40), &mut te);
            let b = tree.abs_bounds(tree.get(id).unwrap().children[0]);
            let (x0, x1) = (b.x + KNOB_R, b.x + b.w - label_w - KNOB_R);
            let press = |tree: &mut Tree, x: i32| {
                let p = Point::new(x, b.y + b.h / 2);
                let (mut h, mut cap) = (None, None);
                tree.dispatch_pointer(ptr(PointerKind::Down, p), &mut h, &mut cap);
                tree.dispatch_pointer(ptr(PointerKind::Up, p), &mut h, &mut cap);
            };
            press(&mut tree, x1);
            assert!(
                (v.get() - 1.0).abs() < 1e-3,
                "字号 {font:?} 宽 {explicit_w:?}：轨道视觉右端应为 100%，实际 {}",
                v.get()
            );
            press(&mut tree, (x0 + x1) / 2);
            assert!(
                (v.get() - 0.5).abs() < 0.02,
                "字号 {font:?} 宽 {explicit_w:?}：轨道视觉中点应为 50%，实际 {}",
                v.get()
            );
        }
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

    /// 亚格滚轮量（Wayland 的 `axis_value120`、触控板连续量）按比例累加；整格与改动前一样
    /// 每格 48 像素。改动前 `-delta * 48 / 120` 在 |delta| < 3 时恒为 0，触控板慢慢滑永远不动。
    #[test]
    fn scroll_wheel_sub_notch_deltas_accumulate() {
        let make = || {
            let mut sc = Element::scroll().width(100).height(100);
            for _ in 0..30 {
                sc = sc.child(Element::leaf().width_match().height(30));
            }
            let mut tree = Tree::new();
            let id = sc.build(&mut tree);
            tree.root = Some(id);
            let mut te = crate::text::NullTextEngine;
            tree.layout_root(Size::new(100, 100), &mut te);
            (tree, id, te)
        };
        let wheel = |d: i32| {
            PointerEvent::single(PointerKind::Wheel(d), Point::new(50, 50), MouseButton::Left)
        };
        let (mut h, mut cap) = (None, None);

        let (mut tree, id, mut te) = make();
        tree.dispatch_pointer(wheel(-120), &mut h, &mut cap);
        tree.layout_root(Size::new(100, 100), &mut te);
        assert_eq!(
            tree.get(id).unwrap().scroll_y,
            48,
            "整格：48 像素，与改动前相同"
        );
        tree.dispatch_pointer(wheel(120), &mut h, &mut cap);
        tree.layout_root(Size::new(100, 100), &mut te);
        assert_eq!(tree.get(id).unwrap().scroll_y, 0);

        let (mut tree, id, mut te) = make();
        for _ in 0..2 {
            tree.dispatch_pointer(wheel(-1), &mut h, &mut cap);
        }
        tree.layout_root(Size::new(100, 100), &mut te);
        assert_eq!(tree.get(id).unwrap().scroll_y, 0, "不足 1 像素时攒着");
        for _ in 0..118 {
            tree.dispatch_pointer(wheel(-1), &mut h, &mut cap);
        }
        tree.layout_root(Size::new(100, 100), &mut te);
        assert_eq!(
            tree.get(id).unwrap().scroll_y,
            48,
            "120 个 1/120 格合计正好一格"
        );
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
        assert_eq!((lo, hi), (expect_hi - scrollbar::hit_w(), expect_hi));
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

    /// `list_signal` 的滚动条同样拖得动（它的容器 widget 由列表驱动转发，见 `KeyedList`）。
    #[test]
    fn list_signal_scrollbar_drag_changes_offset() {
        let data = crate::signal::signal((0..10).collect::<Vec<u8>>());
        let list = Element::list_signal(data, |n| *n, |_| Element::leaf().width_match().height(30))
            .width(100)
            .height(100);
        let mut tree = Tree::new();
        let id = list.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te);
        let (lo, hi) = tree.scrollbar_hit_zone(tree.abs_bounds(id));
        let x = (lo + hi) / 2;
        let (mut h, mut cap) = (None, None);
        let down = PointerEvent::single(PointerKind::Down, Point::new(x, 10), MouseButton::Left);
        tree.dispatch_pointer(down, &mut h, &mut cap);
        assert_eq!(cap, Some(id), "滚动条区域按下应捕获列表容器");
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

    /// 主题改滚动条宽度/留白/轨道：绘制、让位、命中三处同源跟随；加宽超过默认命中区时
    /// 命中区跟着放宽，整条都抓得到。
    #[test]
    fn scrollbar_geometry_and_track_follow_theme() {
        use crate::theme::{set_current, Theme};
        let mut th = Theme::default();
        th.scrollbar.width = Some(4.0);
        th.scrollbar.margin = Some(1.0);
        th.scrollbar.track = Some(Color::rgb(255, 0, 0));
        set_current(std::rc::Rc::new(th));
        assert_eq!(scrollbar::occupied_w(0), 5, "让位 = 宽 + 留白");
        assert_eq!(scrollbar::hit_w(), scrollbar::HIT_W, "细滚动条命中区不缩");
        // 不贴窗口右缘（容器 100 宽放在 200 宽的行里），免掉边缘内缩。
        let (tree, _) = scroll_tree_of_width(200, 100);
        let mut pm = tiny_skia::Pixmap::new(200, 100).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let mut eng = crate::text::NullTextEngine;
        let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
        tree.paint(&mut canvas);
        drop(canvas);
        let red = |x: u32| {
            let p = pm.pixel(x, 90).unwrap();
            p.red() > 200 && p.green() < 60
        };
        // 轨道占 [95, 99)：底部（滑块在顶部）只剩轨道色。
        assert!(red(96) && red(97), "主题给了轨道色就画轨道");
        assert!(!red(93), "轨道只有 4px 宽，左侧不染");
        let mut wide = Theme::default();
        wide.scrollbar.width = Some(20.0);
        set_current(std::rc::Rc::new(wide));
        assert_eq!(scrollbar::hit_w(), 23, "加宽后命中区跟着放宽到宽 + 留白");
        set_current(std::rc::Rc::new(Theme::default()));
    }

    /// 对话框遮罩色取主题 `modal.scrim`（经角色延迟解析，换主题跟随）。
    #[test]
    fn dialog_scrim_follows_theme() {
        use crate::theme::{set_current, Theme};
        let mut th = Theme::default();
        th.modal.scrim = Some(Color::rgb(0, 0, 255));
        set_current(std::rc::Rc::new(th));
        let show = signal(true);
        let root = Element::stack()
            .width(200)
            .height(100)
            .child(Element::dialog(show, Element::leaf().size(20, 20)));
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 100), &mut te);
        let mut pm = tiny_skia::Pixmap::new(200, 100).unwrap();
        pm.fill(tiny_skia::Color::WHITE);
        let mut eng = crate::text::NullTextEngine;
        let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
        tree.paint(&mut canvas);
        drop(canvas);
        let p = pm.pixel(5, 5).unwrap();
        set_current(std::rc::Rc::new(Theme::default()));
        assert_eq!(
            (p.red(), p.green(), p.blue()),
            (0, 0, 255),
            "遮罩应取主题色"
        );
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
            alt: false,
            meta: false,
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
            alt: false,
            meta: false,
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
            alt: false,
            meta: false,
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
            alt: false,
            meta: false,
        };
        tree.dispatch_key(shift_left, Some(input));
        let bs = KeyEvent {
            key: Key::Backspace,
            pressed: true,
            shift: false,
            ctrl: false,
            alt: false,
            meta: false,
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
            alt: false,
            meta: false,
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
            alt: false,
            meta: false,
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
            mods: crate::event::Mods::default(),
        };
        tree.dispatch_pointer(down, &mut h, &mut cap);
        // 全选后输入替换全部内容。
        let key = KeyEvent {
            key: Key::Char('Z'),
            pressed: true,
            shift: false,
            ctrl: false,
            alt: false,
            meta: false,
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
            alt: false,
            meta: false,
        };
        tree.dispatch_key(k(Key::Enter), Some(input));
        tree.dispatch_key(k(Key::Char('c')), Some(input));
        assert_eq!(txt.get(), "ab\nc", "多行 Enter 应插入换行符");
    }

    /// 多行框里 **Ctrl+Enter 不换行、也不消费** —— 它是留给应用的提交通道。
    ///
    /// 没有这条时错在哪：多行框此前对 Enter 一律换行（不看修饰键），于是「多行表单
    /// 怎么用键盘提交」无解——挂 `on_submit` 收不到，挂在外层的 Widget 也收不到
    /// （`dispatch_key` 不冒泡）。放开之后 Ctrl+Enter 未被消费即可上达
    /// `App::on_shortcut`，wind-setting 的加词界面正是靠它。
    #[test]
    fn multiline_ctrl_enter_is_left_for_the_app() {
        let (mut tree, input, txt) = multiline_tree("ab");
        let res = tree.dispatch_key(
            KeyEvent {
                key: Key::Enter,
                pressed: true,
                shift: false,
                ctrl: true,
                alt: false,
                meta: false,
            },
            Some(input),
        );
        assert!(
            !res.consumed,
            "多行 Ctrl+Enter 不该被控件消费——消费掉就到不了 App::on_shortcut"
        );
        assert_eq!(txt.get(), "ab", "多行 Ctrl+Enter 不得插入换行");
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
                alt: false,
                meta: false,
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
                alt: false,
                meta: false,
            },
            Some(input),
        );
        assert_eq!(txt.get(), "x\ny", "多行粘贴应保留换行(\\r\\n 归一为 \\n)");
    }

    /// `EventCtx` 的焦点查询：从**别的节点**的回调里问焦点在哪（按钮回调判断编辑框是否
    /// 正在输入、面板根判断焦点是否在自己子树里）。
    #[test]
    fn event_ctx_reports_focus_held_elsewhere() {
        let mut tree = Tree::new();
        let root = Element::col()
            .child(Element::col().child(Element::text_input(signal(String::new()), "ph")))
            .child(Element::button("b"))
            .build(&mut tree);
        tree.root = Some(root);
        let panel = tree.get(root).unwrap().children[0];
        let input = tree.get(panel).unwrap().children[0];
        let button = tree.get(root).unwrap().children[1];
        let probe = |tree: &mut Tree| {
            let mut got = None;
            crate::testing::run_with_ctx_in(tree, button, |ctx| {
                got = Some((
                    ctx.focused_node(),
                    ctx.focus_accepts_text(),
                    ctx.focus_within(panel),
                    ctx.focus_within(button),
                ));
            });
            got.unwrap()
        };
        assert_eq!(probe(&mut tree), (None, false, false, false), "无焦点");
        tree.set_focused(Some(input), None);
        assert_eq!(probe(&mut tree), (Some(input), true, true, false));
        tree.set_focused(Some(button), Some(input));
        assert_eq!(probe(&mut tree), (Some(button), false, false, true));
    }

    /// 在焦点输入框**自己**的回调里问 `focus_accepts_text`：此时它的 widget 被临时换成
    /// `EmptyWidget`，必须仍答 true（快捷键让路辅助函数常同时挂在输入框回调上）。
    #[test]
    fn focus_accepts_text_holds_inside_the_focused_inputs_own_callback() {
        let seen = signal(None::<bool>);
        let mut tree = Tree::new();
        let root = Element::col()
            .child(
                Element::text_input(signal(String::new()), "ph")
                    .on_submit(move |ctx| seen.set(Some(ctx.focus_accepts_text()))),
            )
            .build(&mut tree);
        tree.root = Some(root);
        let input = tree.get(root).unwrap().children[0];
        tree.set_focused(Some(input), None);
        tree.dispatch_key(
            KeyEvent {
                key: Key::Enter,
                pressed: true,
                shift: false,
                ctrl: false,
                alt: false,
                meta: false,
            },
            Some(input),
        );
        assert_eq!(seen.get(), Some(true));
    }

    /// `Element::corners` 接到节点背景与边框：直角那个角有墨、圆角留白；后写的 `corner`
    /// 盖掉四角设置（后写者胜）。
    #[test]
    fn element_corners_shape_background_and_border() {
        use crate::geometry::Corners;
        let paint = |el: Element| {
            let root = Element::col().width(100).height(80).child(el);
            let mut tree = Tree::new();
            let id = root.build(&mut tree);
            tree.root = Some(id);
            let mut te = crate::text::NullTextEngine;
            tree.layout_root(Size::new(100, 80), &mut te);
            let mut pm = tiny_skia::Pixmap::new(100, 80).unwrap();
            pm.fill(tiny_skia::Color::WHITE);
            let mut eng = crate::text::NullTextEngine;
            let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
            tree.paint(&mut canvas);
            drop(canvas);
            pm
        };
        let ink = |pm: &tiny_skia::Pixmap, x: u32, y: u32| pm.pixel(x, y).unwrap().red() < 128;
        let card = || {
            Element::col()
                .width(100)
                .height(80)
                .bg(Color::BLACK)
                .corners(Corners {
                    bottom_left: 0.0,
                    ..Corners::all(20.0)
                })
        };
        let pm = paint(card());
        assert!(ink(&pm, 0, 79), "左下直角顶点要有墨");
        assert!(!ink(&pm, 0, 0), "左上圆角顶点留白");
        assert!(!ink(&pm, 99, 79), "右下圆角顶点留白");
        let pm = paint(card().corner(0.0));
        assert!(ink(&pm, 0, 0), "后写的 corner(0) 应盖掉四角设置");
        // 只有边框（无底色）：直角外顶点在边上，有墨。
        let pm = paint(
            Element::col()
                .width(100)
                .height(80)
                .border(Color::BLACK, 2)
                .corners(Corners::bottom(20.0)),
        );
        assert!(ink(&pm, 0, 0), "上面两角是直角，边框顶点有墨");
        assert!(!ink(&pm, 0, 79), "下面两角是圆角，顶点留白");
        assert!(!ink(&pm, 50, 40), "边框中间是空的");
    }

    /// 只读框：`width` 为 `None` 时**不设宽**（Wrap 宽，`avail.w != content.w`），
    /// 为 `Some` 时显式设宽——选区与命中两侧都要覆盖（见 AGENTS §5）。
    fn readonly_tree(
        initial: &str,
        multiline: bool,
        width: Option<i32>,
    ) -> (Tree, NodeId, Signal<String>, Rc<RefCell<String>>) {
        let txt = signal(String::from(initial));
        let mut ti = Element::text_input(txt, "ph").read_only();
        if multiline {
            ti = ti.multiline().wrap(true).height(100);
        }
        if let Some(w) = width {
            ti = ti.width(w);
        }
        let root = Element::col().width(200).height(120).child(ti);
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(200, 120), &mut te);
        let input = tree.get(id).unwrap().children[0];
        let clip = Rc::new(RefCell::new(String::from("seed")));
        tree.clipboard = Some(Box::new(SharedClip(clip.clone())));
        // 命中测试读 paint 建的视觉行缓存，先真画一帧。
        tree.set_focused(Some(input), None);
        let mut pm = tiny_skia::Pixmap::new(200, 120).unwrap();
        let mut eng = crate::text::NullTextEngine;
        let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
        tree.paint(&mut canvas);
        (tree, input, txt, clip)
    }

    fn ro_key(key: Key, ctrl: bool) -> KeyEvent {
        KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl,
            alt: false,
            meta: false,
        }
    }

    /// 一切键盘编辑途径都不改信号：键入、退格、Delete、剪切、粘贴、多行 Enter。
    /// 剪切也不顺手复制（剪贴板保持原样），多行 Enter 不消费、留给宿主。
    #[test]
    fn readonly_rejects_keyboard_edits() {
        for multiline in [false, true] {
            let (mut tree, input, txt, clip) = readonly_tree("ab\ncd", multiline, None);
            let before = txt.get();
            tree.dispatch_key(ro_key(Key::Other(0x41), true), Some(input)); // Ctrl+A
            tree.dispatch_key(ro_key(Key::Char('x'), false), Some(input));
            tree.dispatch_key(ro_key(Key::Backspace, false), Some(input));
            tree.dispatch_key(ro_key(Key::Delete, false), Some(input));
            tree.dispatch_key(ro_key(Key::Other(0x58), true), Some(input)); // Ctrl+X
            assert_eq!(&*clip.borrow(), "seed", "只读的 Ctrl+X 不应写剪贴板");
            tree.dispatch_key(ro_key(Key::Other(0x56), true), Some(input)); // Ctrl+V
            tree.dispatch_key(ro_key(Key::Home, false), Some(input));
            tree.dispatch_key(ro_key(Key::Backspace, false), Some(input));
            tree.dispatch_key(ro_key(Key::Delete, false), Some(input));
            let res = tree.dispatch_key(ro_key(Key::Enter, false), Some(input));
            assert!(
                !res.consumed,
                "只读框 Enter 不换行、不消费（multiline={multiline}）"
            );
            assert_eq!(
                txt.get(),
                before,
                "只读框正文不应被改动（multiline={multiline}）"
            );
        }
    }

    fn ro_paint(tree: &Tree) {
        let mut pm = tiny_skia::Pixmap::new(200, 120).unwrap();
        let mut eng = crate::text::NullTextEngine;
        let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
        tree.paint(&mut canvas);
    }

    /// 输入法：合成串不内联进正文（光标不被它推后）、提交的字被拒、正文不交给输入法；
    /// 但光标照常报给宿主——系统候选窗要有落点。
    #[test]
    fn readonly_rejects_ime() {
        let (mut tree, input, txt, _clip) = readonly_tree("ab", true, Some(180));
        let x0 = tree.caret_of(input).expect("只读框照常报光标").0.x;
        tree.set_preedit(
            input,
            &crate::event::Preedit {
                text: "hao".into(),
                caret: 3,
                sel: None,
                spans: Vec::new(),
            },
        );
        ro_paint(&tree);
        // 可编辑框此时光标会被合成串推到 "hao" 之后（见 app 层 ime_caret 测试）。
        assert_eq!(tree.caret_of(input).unwrap().0.x, x0, "合成串不内联进正文");
        assert!(
            tree.ime_text_of(input).is_none(),
            "不把正文交给输入法重转换"
        );
        tree.set_preedit(input, &crate::event::Preedit::default());
        tree.dispatch_key(ro_key(Key::Char('好'), false), Some(input)); // 提交经 Key::Char 到达
        assert_eq!(txt.get(), "ab");
    }

    /// 只读多行框的 Enter 不换行，改交 `on_submit`（与单行同一出口）。
    #[test]
    fn readonly_multiline_enter_goes_to_on_submit() {
        let txt = signal(String::from("ab"));
        let hits = Rc::new(std::cell::Cell::new(0));
        let h2 = hits.clone();
        let root = Element::col().width(200).height(120).child(
            Element::text_input(txt, "")
                .multiline()
                .read_only()
                .height(100)
                .on_submit(move |_| h2.set(h2.get() + 1)),
        );
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        tree.layout_root(Size::new(200, 120), &mut crate::text::NullTextEngine);
        let input = tree.get(id).unwrap().children[0];
        let res = tree.dispatch_key(ro_key(Key::Enter, false), Some(input));
        assert!(res.consumed && hits.get() == 1, "Enter 应交 on_submit");
        assert_eq!(txt.get(), "ab");
    }

    /// 程序把正文改短后，存着的光标先钳进新长度：第一下 Shift+Left 就从文末起扩选。
    #[test]
    fn readonly_keys_start_from_clamped_cursor_after_rewrite() {
        let (mut tree, input, txt, clip) = readonly_tree("hello world", false, None);
        tree.dispatch_key(ro_key(Key::End, false), Some(input)); // 光标 11
        txt.set(String::from("hi"));
        let mut shift_left = ro_key(Key::Left, false);
        shift_left.shift = true;
        tree.dispatch_key(shift_left, Some(input));
        assert_eq!(tree.selection_of(input), Some((1, 2)));
        tree.dispatch_key(ro_key(Key::Other(0x43), true), Some(input));
        assert_eq!(&*clip.borrow(), "i");
    }

    /// 可聚焦、可双击选词、Ctrl+C 得到选中片段。Wrap 宽与显式宽各跑一遍。
    #[test]
    fn readonly_click_focuses_and_double_click_copies_word() {
        for width in [None, Some(180)] {
            let (mut tree, input, txt, clip) = readonly_tree("hello world", false, width);
            let b = tree.abs_bounds(input);
            let p = Point::new(b.x + 10 + 12, b.y + b.h / 2); // "hello" 内部
            let (mut h, mut cap) = (None, None);
            let down = |n| PointerEvent {
                kind: PointerKind::Down,
                pos: p,
                button: MouseButton::Left,
                click_count: n,
                mods: crate::event::Mods::default(),
            };
            let res = tree.dispatch_pointer(down(1), &mut h, &mut cap);
            assert_eq!(res.focus, Some(input), "只读框可聚焦（width={width:?}）");
            tree.dispatch_pointer(
                PointerEvent {
                    kind: PointerKind::Up,
                    ..down(1)
                },
                &mut h,
                &mut cap,
            );
            tree.dispatch_pointer(down(2), &mut h, &mut cap);
            tree.dispatch_key(ro_key(Key::Other(0x43), true), Some(input)); // Ctrl+C
            assert_eq!(
                &*clip.borrow(),
                "hello",
                "双击选词后复制（width={width:?}）"
            );
            assert_eq!(txt.get(), "hello world");
        }
    }

    /// 拖选局部 + 复制得到该片段（多行，Wrap 宽与显式宽）。
    #[test]
    fn readonly_drag_select_copies_fragment() {
        for width in [None, Some(180)] {
            let (mut tree, input, _txt, clip) = readonly_tree("abcdef\nghij", true, width);
            let b = tree.abs_bounds(input);
            // NullTextEngine 下每字宽 14*0.6=8.4：从首行行首拖到第 3 个字符之后。
            let y = b.y + 10 + 4;
            let from = Point::new(b.x + 10, y);
            let to = Point::new(b.x + 10 + 25, y);
            let (mut h, mut cap) = (None, None);
            let ev = |kind, pos| PointerEvent {
                kind,
                pos,
                button: MouseButton::Left,
                click_count: 1,
                mods: crate::event::Mods::default(),
            };
            tree.dispatch_pointer(ev(PointerKind::Down, from), &mut h, &mut cap);
            tree.dispatch_pointer(ev(PointerKind::Move, to), &mut h, &mut cap);
            tree.dispatch_pointer(ev(PointerKind::Up, to), &mut h, &mut cap);
            tree.dispatch_key(ro_key(Key::Other(0x43), true), Some(input));
            assert_eq!(&*clip.borrow(), "abc", "拖选片段可复制（width={width:?}）");
        }
    }

    /// 程序改写信号：显示跟着变，旧选区钳到新正文范围内，复制得到的是新内容。
    #[test]
    fn readonly_signal_rewrite_clamps_selection() {
        let (mut tree, input, txt, clip) = readonly_tree("hello world", true, None);
        tree.dispatch_key(ro_key(Key::Other(0x41), true), Some(input)); // 全选 0..11
        txt.set(String::from("hi"));
        assert_eq!(tree.selection_of(input), Some((0, 2)), "选区钳到新长度");
        tree.dispatch_key(ro_key(Key::Other(0x43), true), Some(input));
        assert_eq!(&*clip.borrow(), "hi");
        // 再画一帧不越界（光标、选区都按新正文算）。
        let mut pm = tiny_skia::Pixmap::new(200, 120).unwrap();
        let mut eng = crate::text::NullTextEngine;
        let mut canvas = crate::render::SkiaCanvas::with_text(&mut pm, &mut eng, 1.0);
        tree.paint(&mut canvas);
    }

    /// 右键菜单只有「复制」「全选」，没有剪切 / 粘贴。
    #[test]
    fn readonly_context_menu_has_no_edit_items() {
        let (mut tree, input, _txt, _clip) = readonly_tree("hello", false, None);
        let b = tree.abs_bounds(input);
        // 落在正文内部：落在选区外的右键会把光标挪过去、清掉选区（既有口径）。
        let center = Point::new(b.x + 10 + 12, b.y + b.h / 2);
        let labels = |tree: &mut Tree| {
            let (mut h, mut cap) = (None, None);
            let down = PointerEvent {
                kind: PointerKind::Down,
                pos: center,
                button: MouseButton::Right,
                click_count: 1,
                mods: crate::event::Mods::default(),
            };
            let menu = tree
                .dispatch_pointer(down, &mut h, &mut cap)
                .menu
                .expect("右键应请求上下文菜单");
            menu.items
                .iter()
                .map(|i| (i.label.clone(), i.enabled))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&mut tree),
            vec![("复制".to_string(), false), ("全选".to_string(), true)]
        );
        tree.dispatch_key(ro_key(Key::Other(0x41), true), Some(input));
        assert_eq!(
            labels(&mut tree),
            vec![("复制".to_string(), true), ("全选".to_string(), true)],
            "有选区时复制可用（右键落在选区内不清选区）"
        );
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
                alt: false,
                meta: false,
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
                alt: false,
                meta: false,
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

    /// **整窗帧**里内容远高于视口的节点也要剪枝——这是"长列表"的日常形态。
    ///
    /// 与下面那条局部帧的用例形态不同，抓的东西也不同：那条测的是"小脏区里少画点"，
    /// 而这条测的是"整窗帧里，一个高 60 万 px 的列表只画视口那一屏"。后端若在整窗帧
    /// 返回 `cull_rect() == None`（D2D 一度根本没实现，吃的是 trait 默认值），
    /// 绘制遍历就会把全部三万行都画一遍——下游实测帧耗时 828 ms、内存多占 44 MB，
    /// 而画面与只画一屏**完全一样**，所以肉眼和像素断言都发现不了，只能靠图元计数。
    #[test]
    fn full_frame_culling_skips_rows_far_below_the_viewport() {
        const VIEW_H: i32 = 400;
        const ROW_H: i32 = 20;
        const ROWS: i32 = 3000; // 内容高 60000 px，是视口的 150 倍
        let mut root = Element::col().width(240).height(ROWS * ROW_H);
        for i in 0..ROWS {
            root = root.child(
                Element::row()
                    .width_match()
                    .height(ROW_H)
                    .child(Element::label(format!("行 {i}")).weight(1.0)),
            );
        }
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        // 视口只有 400 高，内容 60000——布局按内容高排，绘制该只碰视口那一屏
        tree.layout_root(Size::new(240, ROWS * ROW_H), &mut te);

        let count = |cull: Option<Rect>| -> usize {
            let mut pm = tiny_skia::Pixmap::new(240, VIEW_H as u32).unwrap();
            let mut eng = crate::text::NullTextEngine;
            let mut inner = crate::render::SkiaCanvas::with_text_offset(
                &mut pm,
                &mut eng,
                1.0,
                Point::new(0, 0),
            );
            let mut c = CountingCanvas {
                inner: &mut inner,
                cull,
                texts: 0,
                strokes: 0,
                fills: 0,
            };
            tree.paint(&mut c);
            c.texts
        };

        let all = count(None); // 后端报不出范围时的老行为
        let viewport = count(Some(Rect::new(0, 0, 240, VIEW_H)));
        assert_eq!(all, ROWS as usize, "不剪枝就是每行都画");
        assert!(
            viewport <= (VIEW_H / ROW_H) as usize + 4,
            "整窗帧也该只画视口那一屏（约 {} 行），实际画了 {viewport} 行",
            VIEW_H / ROW_H
        );
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

    /// `dispatch_blur` 对已隐藏 / 已禁用（含祖先）的节点不通知：那是结构变化带走了
    /// 焦点，不是用户离开。宿主层到不了这条（ctx 只能把焦点要给自己），故在树上直验。
    #[test]
    fn dispatch_blur_skips_hidden_or_disabled_nodes() {
        let hits = Rc::new(std::cell::Cell::new(0));
        let text = signal(String::new());
        let vis = signal(true);
        let en = signal(true);
        let h = hits.clone();
        let root = Element::col()
            .width(200)
            .height(40)
            .enabled_signal(en)
            .child(
                Element::text_input(text, "")
                    .width(180)
                    .visible_signal(vis)
                    .on_commit(move |_| h.set(h.get() + 1)),
            );
        let mut tree = Tree::new();
        let id = root.build(&mut tree);
        tree.root = Some(id);
        tree.layout_root(Size::new(200, 40), &mut crate::text::NullTextEngine);
        let field = tree.get(id).unwrap().children[0];
        tree.dispatch_key(skey(Key::Char('x'), false), Some(field));

        vis.set(false);
        tree.dispatch_blur(field);
        assert_eq!(hits.get(), 0, "隐藏的节点不该收到失焦通知");
        vis.set(true);
        en.set(false);
        tree.dispatch_blur(field);
        assert_eq!(hits.get(), 0, "祖先禁用的节点不该收到失焦通知");
        en.set(true);
        tree.dispatch_blur(field);
        assert_eq!(hits.get(), 1, "对照：可见且启用时应通知并提交");
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
            alt: false,
            meta: false,
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
                alt: false,
                meta: false,
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
            mods: crate::event::Mods::default(),
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
            mods: crate::event::Mods::default(),
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
            mods: crate::event::Mods::default(),
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
            alt: false,
            meta: false,
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
            mods: crate::event::Mods::default(),
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
            mods: crate::event::Mods::default(),
        };
        tree.dispatch_pointer(down, &mut h, &mut cap);
        let key = KeyEvent {
            key: Key::Char('Z'),
            pressed: true,
            shift: false,
            ctrl: false,
            alt: false,
            meta: false,
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

    #[test]
    fn on_update_dialog_is_captured_for_host() {
        // 回归：on_update 里的 `defer_blocking` 曾随 EventOutcome 一起被丢弃——应用把
        // 「弹系统右键菜单」排到下一帧兑现时，菜单永远不出来，也没有任何报错。
        struct DeferOnUpdate;
        impl Widget for DeferOnUpdate {
            fn on_update(&mut self, ctx: &mut EventCtx) {
                ctx.defer_blocking(|| {});
            }
        }
        let mut tree = Tree::new();
        let id = Element::leaf()
            .reactive()
            .widget(DeferOnUpdate)
            .build(&mut tree);
        tree.root = Some(id);
        let mut te = crate::text::NullTextEngine;
        tree.layout_root(Size::new(100, 100), &mut te);
        assert!(
            matches!(tree.take_pending_dialog(), Some(DialogRequest::Custom(_))),
            "on_update 发出的对话框请求应被暂存供宿主执行"
        );
        assert!(tree.take_pending_dialog().is_none(), "取走后应清空暂存");
    }
}
