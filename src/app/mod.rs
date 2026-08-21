//! 应用入口与交互宿主。
//!
//! `App` 构建器组装窗口配置与控件树；`UiHost` 持有运行期交互状态
//! （树、文字引擎、hover/capture/focus）并实现 `AppHandler` 供平台驱动。
//!
//! 宿主自成一块的子职责各占一个子模块，`UiHost` 只保留把它们串起来的调度：
//!
//! | 子模块 | 职责 |
//! |---|---|
//! | [`menu`] | 上下文菜单浮层：级联面板、独立命中、滚动与键盘导航 |
//! | [`toast`] | 轻提示浮层：堆叠、淡入淡出、悬停冻结倒计时 |
//! | [`tooltip`] | 悬停提示浮层：延时、抑制、翻转定位 |
//! | [`fling`] | 触摸惯性滑动与平移残差 |
//! | [`focus`] | 焦点归属：Tab 顺序、焦点环、模态移交 |
//! | [`damage`] | 局部重绘仲裁与后备缓冲 |

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::sync::{new_channel, Sender, WakerShared};

use crate::core::{DamageReq, DispatchResult, EventCtx, NodeId, Tree};
use crate::event::{
    CursorShape, Key, MouseButton, PointerEvent, PointerKind, WindowOp, WindowRequest,
};
use crate::geometry::{Color, Point, Rect, Size};
use crate::platform::{self, AppHandler, DialogRequest, NewWindow, Renderer, WindowConfig};
use crate::render::Paint;
use crate::text::{PlatformTextEngine, TextEngine};
use crate::theme::Theme;
use crate::ui::Element;

mod damage;
mod fling;
mod focus;
mod menu;
mod toast;
mod tooltip;

use damage::DamageState;
use fling::ScrollState;
use focus::{FocusSource, FocusState};
use menu::MenuHost;
use toast::ToastHost;
use tooltip::TooltipState;

thread_local! {
    /// 待执行的延迟闭包（[`defer_blocking`] 排入，平台在事件分发完全返回后取走执行）。
    static DEFERRED: RefCell<Vec<Box<dyn FnOnce()>>> = const { RefCell::new(Vec::new()) };
}

/// 把一段包含阻塞式原生调用（文件对话框、`MessageBoxW` 等）的流程延迟到事件分发
/// **完全返回**之后执行——[`EventCtx::defer_blocking`](crate::core::EventCtx::defer_blocking)
/// 的无 `ctx` 版本。
///
/// 它当初存在是因为菜单项动作是无参 `Fn()`、拿不到 `ctx`；0.12.0 起
/// [`MenuItem::run`](crate::event::MenuItem::run) 的动作也收 `&mut EventCtx`，
/// 这个缺口没有了。
///
/// 闭包按排入顺序执行；同一轮排入多个会在同一次取走中依次跑完。
#[deprecated(
    since = "0.12.0",
    note = "改用 `ctx.defer_blocking(f)`：菜单项动作现在也收 `&mut EventCtx`（MenuItem::run 签名变更），自由函数版本存在的唯一理由——「有些回调拿不到 ctx」——已经消失。托盘菜单项另有 `TrayCtx`"
)]
pub fn defer_blocking(f: impl FnOnce() + 'static) {
    DEFERRED.with(|d| d.borrow_mut().push(Box::new(f)));
}

/// 取走全部延迟闭包（由已废弃的 [`defer_blocking`] 排入），打包成一个
/// [`DialogRequest::Custom`]（无待执行项时返回 None）。
///
/// 复用 `DialogRequest` 通道而不是另开一条平台回调：平台侧已经在"事件分发完全返回"
/// 这一时机轮询它，正是延迟闭包需要的时机，多开一条只会多一处要同步的时序约定。
///
/// 公开是给自定义 [`AppHandler`](crate::platform::AppHandler) 用的：覆盖了
/// `take_dialog_request` 就绕过了默认实现，得在自己的实现里回退到本函数，
/// 否则 [`defer_blocking`] 排入的闭包永远不会执行。
pub fn take_deferred() -> Option<DialogRequest> {
    let pending: Vec<Box<dyn FnOnce()>> = DEFERRED.with(|d| std::mem::take(&mut *d.borrow_mut()));
    if pending.is_empty() {
        return None;
    }
    Some(DialogRequest::Custom(Box::new(move || {
        for f in pending {
            f();
        }
    })))
}

/// 子窗口构建器：交给 [`EventCtx::open_window`](crate::core::EventCtx::open_window)。
///
/// 与 [`App`] 的构建器同形（`new(title, w, h)` 起手、链式配置、`content` 收尾），但只有
/// 对子窗有意义的那几项——托盘、全局热键、单实例、渲染后端都是**应用级**的，由主窗口
/// 那次 `App` 配置决定，子窗跟随（见 `AppHost`）。
///
/// ```no_run
/// # use windui::prelude::*;
/// Element::button("设置…").on_click(|ctx| {
///     ctx.open_window(
///         Window::new("设置", 560, 420)
///             .resizable(false)
///             .content(|| Element::col().padding(16).child(Element::label("设置项…"))),
///     );
/// });
/// ```
pub struct Window {
    req: WindowRequest,
}

impl Window {
    /// 新建子窗口配置。`width`/`height` 是**客户区**逻辑尺寸（同 [`App::new`]）。
    pub fn new(title: impl Into<String>, width: i32, height: i32) -> Self {
        Self {
            req: WindowRequest {
                title: title.into(),
                width,
                height,
                resizable: true,
                centered: false,
                frameless: false,
                min_width: 0,
                min_height: 0,
                bg: None,
                // 占位。取不走：`content` 是唯一产出 `WindowRequest` 的方法，
                // 没调它就拿不到能交给 `open_window` 的值——类型上已经封死。
                content: crate::event::WindowContent::new(|| NoContent),
                close_handler: None,
                intervals: Vec::new(),
                single: None,
            },
        }
    }

    /// 把本窗口设为**单例**：同一个 `key` 同时只存在一个窗口。
    ///
    /// 已经开着同键窗口时，本次请求被丢弃，那个窗口被激活到前台（最小化的会还原）。
    /// 设置页、关于框这类"再点一次应该跳到已开的那个"的窗口用它。
    ///
    /// 判定在平台层做，不是应用层自己拿 `Signal` 记一个"开着没有"：
    /// - 只有平台能把已有窗口拉到前台。应用层挡住重复开窗后什么都不发生，看着像按钮坏了；
    /// - 键随窗口登记表自动清除，`ctx.request_close()`（绕过 `on_close_request`）关掉的
    ///   窗口也不会漏复位——自己记标记就会漏，然后那个窗口再也开不出来。
    ///
    /// 已有窗口的配置**不会**被后续请求更新（标题、尺寸、内容都保持第一次那份）：
    /// 单例的语义是"就是那一个窗口"，把它按新配置重建反而丢掉用户在里面的状态。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// Element::button("打开设置…").on_click(|ctx| {
    ///     ctx.open_window(
    ///         Window::new("设置", 420, 320)
    ///             .single("settings")
    ///             .content(|| Element::col().padding(16).child(Element::label("设置项…"))),
    ///     );
    /// });
    /// ```
    pub fn single(mut self, key: impl Into<String>) -> Self {
        self.req.single = Some(key.into());
        self
    }

    /// 本窗口的关闭请求拦截器：返回 `true` 放行、`false` 取消。语义与
    /// [`App::on_close_request`] 完全一致（含"先返回 false 挡下、再另起一条路把确认送
    /// 回来"那套，见 API_GUIDE §8.7），只是作用在这个子窗上。
    ///
    /// 拦截器是**每个窗口自己的**：平台同步等这个 `bool`，问的是"这个窗口能不能关"。
    /// 主窗那个不会代管子窗，跨窗共享的 `Signal` 也表达不了它。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// # let dirty = signal(true);
    /// # let asking = signal(false);
    /// Window::new("设置", 420, 320)
    ///     .on_close_request(move |_ctx| {
    ///         if dirty.get() {
    ///             asking.set(true);   // 弹自绘确认框
    ///             return false;       // 挡下这一次
    ///         }
    ///         true
    ///     })
    ///     .content(|| Element::col().fill());
    /// ```
    pub fn on_close_request(mut self, f: impl FnMut(&mut EventCtx) -> bool + 'static) -> Self {
        self.req.close_handler = Some(Box::new(f));
        self
    }

    /// 本窗口的周期回调，语义同 [`App::on_interval`]，但**随本窗口关闭一并停止**。
    ///
    /// 子窗里的实时刷新用它，而不是在主窗挂一个定时器写 `Signal`——后者在子窗关掉之后
    /// 仍会一直跑。可多次调用。
    pub fn on_interval(mut self, every: Duration, f: impl FnMut(&mut EventCtx) + 'static) -> Self {
        self.req.intervals.push((every, Box::new(f)));
        self
    }

    /// 允许用户调整窗口大小（默认 true）。
    pub fn resizable(mut self, on: bool) -> Self {
        self.req.resizable = on;
        self
    }

    /// 窗口居中显示。
    pub fn centered(mut self, on: bool) -> Self {
        self.req.centered = on;
        self
    }

    /// 无标题栏窗口（自定义标题栏），同 [`App::frameless`]。
    pub fn frameless(mut self, on: bool) -> Self {
        self.req.frameless = on;
        self
    }

    /// 最小客户区尺寸（逻辑 dp）。
    pub fn min_size(mut self, w: i32, h: i32) -> Self {
        self.req.min_width = w;
        self.req.min_height = h;
        self
    }

    /// 固定窗口背景色。不设则随主题 `palette.bg`（换暗色主题时子窗底色同步转暗）。
    pub fn bg(mut self, c: Color) -> Self {
        self.req.bg = Some(c);
        self
    }

    /// 设置内容构建器，得到可交给 `ctx.open_window` 的请求。
    ///
    /// 收闭包而非建好的树：闭包在**窗口真正创建时**才求值，其间创建的 `Signal` 归这个
    /// 窗口所有，窗口关闭时一并回收。传一棵建好的树就晚了——那些信号在调用方写下
    /// `Element::col()…` 的那一刻就已经进了全局 arena，窗口关掉也没人收，反复开关设置窗
    /// 会让它们一直累积。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// # let ui = Element::col();
    /// Window::new("设置", 560, 420).content(move || Element::col().child(ui));
    /// ```
    pub fn content(mut self, build: impl FnOnce() -> Element + 'static) -> WindowRequest {
        self.req.content = crate::event::WindowContent::new(build);
        self.req
    }
}

/// `Window::new` 的内容占位类型：用户没调 `content` 就 `open_window` 时，宿主据此拦下。
struct NoContent;

type RenderClosure = Box<dyn FnMut(&mut dyn crate::render::RenderTarget, Size)>;

/// App 级回调：不在任何控件的事件时机上，却同样收 [`EventCtx`]（`on_interval` 等）。
/// 宿主以根节点为 `self_id` 借出（见 `UiHost::apply_app_effects`）。
type AppCallback = Box<dyn FnMut(&mut EventCtx)>;

/// 关闭请求拦截器（[`App::on_close_request`]）：返回 true 放行、false 取消。
/// 与 [`AppCallback`] 分开是因为它多一个返回值——那个 `bool` 被平台同步等待。
type CloseHandler = Box<dyn FnMut(&mut EventCtx) -> bool>;
/// 窗口唤起回调。与 `on_click` 同形（ctx 在首位），无返回值——唤起不是一次可否决的请求。
type ShowHandler = Box<dyn FnMut(&mut EventCtx)>;

/// 应用构建器。命令式 API 的根入口。
/// 运行期主题句柄：克隆到控件回调中，`set` 即可热切换主题（下一帧生效）。
/// 控件 paint 期读 `theme::current()` 自动跟随；用 `Brush::Role`/`bg_role` 等
/// 主题角色的背景/边框/文字也随之刷新，写死的 `bg(Color)` 定格色不变。
#[derive(Clone)]
pub struct ThemeHandle {
    inner: Rc<RefCell<Rc<Theme>>>,
}

impl ThemeHandle {
    fn new(t: Rc<Theme>) -> Self {
        Self {
            inner: Rc::new(RefCell::new(t)),
        }
    }
    /// 造一个**不连宿主**的句柄，供下游给自己的应用状态写测试。
    ///
    /// 真句柄只能从 [`App::theme_handle`] 取，而 `App` 在测试里建不起来（要开窗口）。
    /// 后果不是"换肤那部分测不了"，而是**整个持有句柄的状态结构测不了**——造不出实例，
    /// 它所有的方法都跟着一起测不了。这个构造口把可测性还原到"与换肤无关的逻辑照常能测"。
    ///
    /// 行为与真句柄**完全一致**（`set`/`update`/`current` 走同一份实现）：写入只改自己
    /// 那份 `Rc<Theme>`，另外置两个线程局部的重绘标记——无宿主时那两个标记没人消费，
    /// 是无害的空转，故无需为测试分叉出第二套行为。
    ///
    /// ```
    /// # use windui::prelude::*;
    /// let th = ThemeHandle::detached(Theme::default());
    /// th.update(|t| t.palette.accent = Color::hex(0x123456));
    /// assert_eq!(th.current().palette.accent, Color::hex(0x123456));
    /// ```
    pub fn detached(t: Theme) -> Self {
        Self::new(Rc::new(t))
    }
    /// 替换当前主题并请求重绘（所有窗口）。
    pub fn set(&self, t: Theme) {
        *self.inner.borrow_mut() = Rc::new(t);
        crate::anim::request_repaint();
        // 主题源为所有窗口共享，而 `request_repaint` 只唤起当前窗口。不标这一笔，
        // 换肤就只在应用碰巧同时写了信号时才联动（比如用 `Signal<bool>` 记明暗）。
        crate::signal::mark_cross_window_dirty();
    }
    /// 就地修改当前主题（快照 → 改 → 写回 → 请求重绘）。运行期局部调整的便捷入口：
    ///
    /// ```ignore
    /// th.update(|t| t.palette.accent = Color::hex(0x2E9E5B));   // 换强调色
    /// th.update(|t| t.metrics.font_size += 1.0);                // 全局调大字号
    /// ```
    pub fn update(&self, f: impl FnOnce(&mut Theme)) {
        let mut t: Theme = (**self.inner.borrow()).clone();
        f(&mut t);
        self.set(t);
    }
    /// 当前主题快照。
    pub fn current(&self) -> Rc<Theme> {
        self.inner.borrow().clone()
    }
}

/// 运行期热键句柄（`App::hotkey_handle` 返回）。克隆进控件回调，随时改绑/启停：
///
/// ```ignore
/// let hk = app.hotkey_handle(Hotkey::new(Key::Char('D')).ctrl().alt(), |ctx| ctx.show_window());
/// // 设置页回调里：
/// hk.set(Hotkey::new(Key::Char('J')).ctrl());   // 立即向系统换注册
/// hk.set_enabled(false);                        // 注销，把组合归还系统
/// ```
///
/// 操作经意图队列在平台层落地（下一次消息循环内生效）：改绑失败（新组合被其他
/// 程序占用）时**回滚保留旧绑定**，与注册失败不阻启动的既定语义一致。
#[derive(Clone)]
pub struct HotkeyHandle {
    id: usize,
    queue: Rc<RefCell<Vec<(usize, crate::event::HotkeyOp)>>>,
}

impl HotkeyHandle {
    /// 造一个**不连宿主**的句柄，供下游给自己的应用状态写测试。理由同
    /// [`ThemeHandle::detached`]：真句柄只能从 [`App::hotkey_handle`] 取，而 `App`
    /// 在测试里建不起来，于是持有它的整个状态结构都造不出实例。
    ///
    /// `set` / `set_enabled` 照常把意图排进队列，用 [`Self::pending_ops`] 断言。
    /// 队列无人消费——这正是"不连宿主"的意思：改绑不会真的向系统注册。
    ///
    /// ```
    /// # use windui::prelude::*;
    /// use windui::event::HotkeyOp;
    /// let hk = HotkeyHandle::detached();
    /// hk.set(Hotkey::new(Key::Char('J')).ctrl());
    /// hk.set_enabled(false);
    /// assert_eq!(
    ///     hk.pending_ops(),
    ///     vec![
    ///         HotkeyOp::Rebind(Hotkey::new(Key::Char('J')).ctrl()),
    ///         HotkeyOp::SetEnabled(false),
    ///     ],
    /// );
    /// ```
    pub fn detached() -> Self {
        Self {
            id: 0,
            queue: Rc::new(RefCell::new(Vec::new())),
        }
    }
    /// 本句柄已排队、尚未被平台层消费的意图（按调用顺序）。
    ///
    /// **只读不取走**——故对真句柄调用也是安全的，不会把宿主该执行的改绑偷掉。
    /// 队列由所有句柄共享，这里只返回 `id` 属于本句柄的那些：多热键应用中
    /// 断言"改的是这一个"才有意义。
    pub fn pending_ops(&self) -> Vec<crate::event::HotkeyOp> {
        self.queue
            .borrow()
            .iter()
            .filter(|(id, _)| *id == self.id)
            .map(|(_, op)| *op)
            .collect()
    }
    /// 换成这个热键（下一次消息循环生效；失败回滚保留旧绑定）。
    pub fn set(&self, hotkey: crate::event::Hotkey) {
        self.queue
            .borrow_mut()
            .push((self.id, crate::event::HotkeyOp::Rebind(hotkey)));
        // 唤一帧，让平台意图消费点尽快跑到。
        crate::anim::request_repaint();
    }
    /// 改名为 [`HotkeyHandle::set`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `set`：运行期句柄的方法面与 ThemeHandle/Signal 对齐（set/set_enabled），少记一个词"
    )]
    pub fn rebind(&self, hotkey: crate::event::Hotkey) {
        self.set(hotkey);
    }
    /// 启用/停用（停用即注销，组合归还给其他程序）。
    pub fn set_enabled(&self, on: bool) {
        self.queue
            .borrow_mut()
            .push((self.id, crate::event::HotkeyOp::SetEnabled(on)));
        crate::anim::request_repaint();
    }
}

pub struct App {
    cfg: WindowConfig,
    render: Option<RenderClosure>,
    content: Option<Element>,
    theme: Option<Theme>,
    theme_src: Option<ThemeHandle>,
    intervals: Vec<(Duration, AppCallback)>,
    waker_shared: Option<Arc<WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
    close_handler: Option<CloseHandler>,
    /// 窗口从隐藏态被唤起时的回调（见 [`App::on_show`]）。
    show_handler: Option<ShowHandler>,
    /// 关闭请求转为隐藏窗口。与 `close_handler` 同属核心层的关闭决策链输入，
    /// 平台层对此无感知，故不放 `WindowConfig`。
    hide_on_close: bool,
    /// 用户是否经 `App::bg` 显式指定了窗口背景（是 → 固定色；否 → 清屏色随主题
    /// palette.bg 热切换，修"切暗色主题后清屏仍是亮色底"）。
    bg_explicit: bool,
    /// 运行期热键操作队列（`hotkey_handle` 句柄写入、UiHost 中转、平台消费）。
    hotkey_ops: Rc<RefCell<Vec<(usize, crate::event::HotkeyOp)>>>,
}

impl App {
    pub fn new(title: impl Into<String>, width: i32, height: i32) -> Self {
        Self {
            cfg: WindowConfig {
                title: title.into(),
                width,
                height,
                bg: Color::hex(0xF3F3F3),
                centered: false,
                resizable: true,
                screenshot: None,
                screenshot_scale: 1.0,
                screenshot_rclick: None,
                screenshot_clicks: Vec::new(),
                screenshot_hover: None,
                screenshot_drag: None,
                screenshot_keys: Vec::new(),
                tray: None,
                hotkeys: Vec::new(),
                start_hidden: false,
                frameless: false,
                animations: None,
                renderer: Renderer::default(),
                min_width: 0,
                min_height: 0,
                // 主窗本就唯一，不参与单例去重（`Window::single` 只给子窗）。
                single: None,
            },
            render: None,
            content: None,
            theme: None,
            theme_src: None,
            intervals: Vec::new(),
            waker_shared: None,
            single: None,
            close_handler: None,
            show_handler: None,
            hide_on_close: false,
            bg_explicit: false,
            hotkey_ops: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// 窗口背景色。命名与 `Element::bg` 统一。
    pub fn bg(mut self, c: Color) -> Self {
        self.cfg.bg = c;
        // 显式指定即固定：清屏色不再随主题热切换。
        self.bg_explicit = true;
        self
    }

    /// 禁止用户拖拽调整窗口大小（去掉 WS_THICKFRAME 和最大化按钮）。
    pub fn resizable(mut self, v: bool) -> Self {
        self.cfg.resizable = v;
        self
    }

    /// 窗口最小客户区尺寸（逻辑 dp）。限制用户不能把窗口缩到操作不到内容/按钮。
    pub fn min_size(mut self, w: i32, h: i32) -> Self {
        self.cfg.min_width = w;
        self.cfg.min_height = h;
        self
    }

    /// 强制动画全局开关。默认（不调用）随系统"显示动画"设置；`true`/`false` 强制开/关。
    /// 关闭时所有补间瞬时收敛到终态（运行期也可改用 `anim::set_enabled`）。
    pub fn animations(mut self, on: bool) -> Self {
        self.cfg.animations = Some(on);
        self
    }

    /// 窗口居中显示。
    pub fn centered(mut self) -> Self {
        self.cfg.centered = true;
        self
    }

    /// 选择渲染后端。默认 [`Renderer::Software`]。
    ///
    /// - [`Renderer::Auto`]：GPU（Direct2D）优先，设备建不起来时自动回退软光栅。
    /// - [`Renderer::Software`]：强制软光栅，内存敏感场景用。
    /// - [`Renderer::Gpu`]：强制 GPU，拿不到就报错终止（测试与排障用）。
    ///
    /// GPU 路径按平台取最合适的实现：Windows 走 Direct2D（ClearType 子像素混合由
    /// D2D 直接完成，是更正统的一条）；macOS 走 wgpu/Metal（需开 `gpu` feature，
    /// 文字仍由 Core Text 光栅、GPU 只合成，观感与软路径一致）。环境变量
    /// `WINDUI_D2D`（Windows）/`WINDUI_GPU`（macOS）可在运行时强制或禁用，
    /// 便于排障与验证回退路径。
    pub fn renderer(mut self, r: Renderer) -> Self {
        self.cfg.renderer = r;
        self
    }

    /// 设置主题（默认使用内置默认主题）。窗口背景未显式设置时随主题 palette.bg。
    ///
    /// 主题会**当场**装进当前线程，而不是等到 `run()`。因为一部分组合子
    /// （`Element::field` / `card` / `badge` / `tag_field` / `dialog_panel` 等）在
    /// **构造期**就要读主题定尺寸和颜色，若等到 `run()` 才装，它们读到的是默认主题，
    /// 自定义主题里的行高、圆角、徽章色会静默失效——编译通过、也不报错，只是没生效。
    ///
    /// 因此控件树须在本方法**之后**构造。链式写法天然满足
    /// （`App::new(..).theme(t).content(build_ui())`：参数在 `.theme(t)` 之后才求值）；
    /// 若先把树建进变量再传，请把建树挪到 `.theme(t)` 之后，或自行先调
    /// [`theme::set_current`](crate::theme::set_current)。
    pub fn theme(mut self, t: Theme) -> Self {
        // 尊重 App::bg 的显式指定：`.bg(c).theme(t)` 与 `.theme(t).bg(c)` 结果一致。
        if !self.bg_explicit {
            self.cfg.bg = t.palette.bg;
        }
        let rc = Rc::new(t.clone());
        // 已有运行期句柄时同步初值，保证 theme()/theme_handle() 任意调用序结果一致。
        if let Some(h) = &self.theme_src {
            *h.inner.borrow_mut() = rc.clone();
        }
        crate::theme::set_current(rc);
        self.theme = Some(t);
        self
    }

    /// 获取运行期主题句柄（多次调用返回同一共享源的克隆）。把它克隆进控件回调，
    /// 调 `set(theme)` 即可在窗口内热切换暗/亮主题，下一帧整树跟随刷新。
    pub fn theme_handle(&mut self) -> ThemeHandle {
        let init = Rc::new(self.theme.clone().unwrap_or_default());
        self.theme_src
            .get_or_insert_with(|| ThemeHandle::new(init))
            .clone()
    }

    /// 截屏模式：渲染一帧存 PNG 后退出。常用于自动化验证。
    pub fn screenshot(mut self, path: impl Into<PathBuf>) -> Self {
        self.cfg.screenshot = Some(path.into());
        self
    }

    /// 从命令行解析截屏相关参数。
    ///
    /// 支持：`--screenshot <path>`、`--scale <f>`（高 DPI）、`--size W H`、
    /// `--renderer <auto|software|gpu>`，以及截屏前合成的交互
    /// `--click X Y` / `--rclick X Y` / `--drag X0 Y0 X1 Y1` / `--hover X Y` /
    /// `--type <text>` / `--key <name>`。
    pub fn screenshot_from_args(self) -> Self {
        let args: Vec<String> = std::env::args().collect();
        self.screenshot_args(&args)
    }

    /// 从任意参数序列解析（[`screenshot_from_args`](Self::screenshot_from_args) 用进程
    /// 参数调用它）。
    ///
    /// 独立成方法是为了**可测**：直接读 `std::env::args()` 的话，"`--size` 有没有把
    /// `min_size` 的下限一并放开"这类断言只能靠真起一个进程传参才验得了，进不了单测。
    fn screenshot_args(mut self, args: &[String]) -> Self {
        if let Some(i) = args.iter().position(|a| a == "--screenshot") {
            if let Some(p) = args.get(i + 1) {
                self.cfg.screenshot = Some(PathBuf::from(p));
            }
        }
        if let Some(i) = args.iter().position(|a| a == "--scale") {
            if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<f32>().ok()) {
                self.cfg.screenshot_scale = v;
            }
        }
        // --rclick X Y：截屏前在逻辑坐标 (X,Y) 合成右键，验证右键菜单等交互视觉。
        if let Some(i) = args.iter().position(|a| a == "--rclick") {
            if let (Some(x), Some(y)) = (
                args.get(i + 1).and_then(|s| s.parse::<i32>().ok()),
                args.get(i + 2).and_then(|s| s.parse::<i32>().ok()),
            ) {
                self.cfg.screenshot_rclick = Some((x, y));
            }
        }
        // --click X Y：截屏前合成左键单击，验证下拉展开等交互视觉。
        // 可重复出现，按序回放（如展开复选菜单后连点两个开关，验证菜单不关）。
        for (i, a) in args.iter().enumerate() {
            if a != "--click" {
                continue;
            }
            if let (Some(x), Some(y)) = (
                args.get(i + 1).and_then(|s| s.parse::<i32>().ok()),
                args.get(i + 2).and_then(|s| s.parse::<i32>().ok()),
            ) {
                self.cfg.screenshot_clicks.push((x, y));
            }
        }
        // --drag X0 Y0 X1 Y1：截屏前合成一次左键拖拽，验证划选高亮这类拖出才成立的视觉。
        if let Some(i) = args.iter().position(|a| a == "--drag") {
            let n = |k: usize| args.get(i + k).and_then(|s| s.parse::<i32>().ok());
            if let (Some(x0), Some(y0), Some(x1), Some(y1)) = (n(1), n(2), n(3), n(4)) {
                self.cfg.screenshot_drag = Some((x0, y0, x1, y1));
            }
        }
        // --hover X Y：截屏前在 (X,Y) 合成悬停并等待超过提示延时，验证 tooltip 等悬停视觉。
        if let Some(i) = args.iter().position(|a| a == "--hover") {
            if let (Some(x), Some(y)) = (
                args.get(i + 1).and_then(|s| s.parse::<i32>().ok()),
                args.get(i + 2).and_then(|s| s.parse::<i32>().ok()),
            ) {
                self.cfg.screenshot_hover = Some((x, y));
            }
        }
        // --size W H：覆盖 App::new 的窗口尺寸，**并把 min_size 的下限一并放开**——
        // 要测的正是下限处的表现，若还受 min_size 钳制就永远截不到"最小尺寸下布局
        // 是否还成立"这张图。此前尺寸写死在代码里，这类验证只能改代码重编译。
        if let Some(i) = args.iter().position(|a| a == "--size") {
            let n = |k: usize| args.get(i + k).and_then(|v| v.parse::<i32>().ok());
            if let (Some(w), Some(h)) = (n(1), n(2)) {
                if w > 0 && h > 0 {
                    self.cfg.width = w;
                    self.cfg.height = h;
                    self.cfg.min_width = 0;
                    self.cfg.min_height = 0;
                }
            }
        }
        // --type <text> / --key <name>：截屏前合成键盘输入，按序回放。
        //
        // 没有它时缺什么：截图路径此前只有指针（--click/--rclick/--drag/--hover），于是
        // 任何"输入中"的界面状态都截不到图——补全浮层、候选高亮、焦点落在哪，全都只能
        // 靠人眼在真机上看。键盘通路本身（autofocus/on_submit/on_nav_key）尤其需要它，
        // 否则那条路是唯一没有视觉回归覆盖的核心交互。
        //
        // 两者与 --click 一样可重复、按出现顺序回放，且**混合排序**：
        // `--type ab --key Enter --type c` 严格按写的顺序来。
        for (i, a) in args.iter().enumerate() {
            match a.as_str() {
                "--type" => {
                    if let Some(t) = args.get(i + 1) {
                        self.cfg
                            .screenshot_keys
                            .push(platform::ScreenshotKey::Text(t.clone()));
                    }
                }
                "--key" => {
                    if let Some(spec) = args.get(i + 1) {
                        match parse_key_spec(spec) {
                            Some(ev) => self.cfg.screenshot_keys.push(platform::ScreenshotKey::Key(ev)),
                            None => eprintln!(
                                "[windui] 无法识别的 --key {spec}（可用 Enter/Escape/Tab/Up/Down/Left/Right/Home/End/PageUp/PageDown/Backspace/Delete/Space，可加 ctrl+ / shift+ 前缀），已跳过"
                            ),
                        }
                    }
                }
                _ => {}
            }
        }
        // --renderer <auto|software|gpu>：选渲染后端，便于同一个 example 出软/硬两份
        // 截图做比对。`gpu` 拿不到 GPU 时报错终止，正是为了让比对结论可信——静默回退
        // 会让人拿两张软渲染图得出"软硬一致"。
        if let Some(v) = args
            .iter()
            .position(|a| a == "--renderer")
            .and_then(|i| args.get(i + 1))
        {
            match v.as_str() {
                "auto" => self.cfg.renderer = Renderer::Auto,
                "software" | "soft" => self.cfg.renderer = Renderer::Software,
                "gpu" => self.cfg.renderer = Renderer::Gpu,
                other => eprintln!(
                    "[windui] 无法识别的 --renderer {other}（可选 auto|software|gpu），沿用默认"
                ),
            }
        }
        // --accelerated：保留的旧写法，等价于 `--renderer auto`。
        if args.iter().any(|a| a == "--accelerated") {
            self.cfg.renderer = Renderer::Auto;
        }
        self
    }

    /// 底层渲染回调（无控件树时使用）。
    pub fn on_render(
        mut self,
        f: impl FnMut(&mut dyn crate::render::RenderTarget, Size) + 'static,
    ) -> Self {
        self.render = Some(Box::new(f));
        self
    }

    /// 设置控件树根（常规入口）。
    pub fn content(mut self, root: Element) -> Self {
        self.content = Some(root);
        self
    }

    /// 注册全局热键：应用无焦点、窗口隐藏时亦可触发。可多次调用注册多个。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// App::new("查词", 480, 360)
    ///     .start_hidden()
    ///     .hotkey(Hotkey::new(Key::Char('D')).ctrl().alt(), |ctx| ctx.show_window())
    ///     .run();
    /// ```
    ///
    /// 回调拿到的 [`HotkeyCtx`](crate::event::HotkeyCtx) **只能声明窗口操作意图**，
    /// 拿不到窗口句柄——回调在平台层持有窗口状态借用期间执行，直接调 OS 窗口 API 会
    /// 同步重入消息处理并造成 `&mut` 别名（见 `AGENTS.md` 铁律 6）。
    ///
    /// **注册可能失败且不报错**：热键是全局独占资源，组合被其他程序占用时系统会拒绝，
    /// 此时该热键静默失效、其余热键与应用本身不受影响。这是刻意的——为一个热键冲突
    /// 让整个应用起不来是不可接受的。
    ///
    /// **平台**：Windows 走 `RegisterHotKey`，macOS 走 Carbon `RegisterEventHotKey`
    /// （两者都不需要用户授权）。语义一致：全局生效、组合被占用则该热键静默失效、
    /// 运行期可经 [`HotkeyHandle`] 改绑与启停。
    pub fn hotkey(
        mut self,
        hotkey: crate::event::Hotkey,
        callback: impl FnMut(&mut crate::event::HotkeyCtx) + 'static,
    ) -> Self {
        self.cfg.hotkeys.push(platform::HotkeyBinding {
            hotkey,
            callback: Box::new(callback),
        });
        self
    }

    /// 注册全局热键并返回**运行期句柄**（改绑/启停即时生效，无需重启）。
    /// 语义同 [`Self::hotkey`]（注册失败静默、回调只声明意图）；句柄克隆进
    /// 控件回调，设置页"修改热键"场景用它：
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// # let mut app = App::new("demo", 320, 200);
    /// let hk = app.hotkey_handle(Hotkey::new(Key::Char('D')).ctrl().alt(), |ctx| ctx.show_window());
    /// // 之后某个按钮回调里：hk.set(Hotkey::new(Key::Char('J')).ctrl());
    /// ```
    pub fn hotkey_handle(
        &mut self,
        hotkey: crate::event::Hotkey,
        callback: impl FnMut(&mut crate::event::HotkeyCtx) + 'static,
    ) -> HotkeyHandle {
        let id = self.cfg.hotkeys.len();
        self.cfg.hotkeys.push(platform::HotkeyBinding {
            hotkey,
            callback: Box::new(callback),
        });
        HotkeyHandle {
            id,
            queue: self.hotkey_ops.clone(),
        }
    }

    /// 改名为 [`App::hotkey_handle`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `hotkey_handle`：返回运行期句柄的方法统一叫 `*_handle`（对齐 App::theme_handle），`_rc` 既非 Rc 又与「绑信号」用的 `_rc` 撞义"
    )]
    pub fn hotkey_rc(
        &mut self,
        hotkey: crate::event::Hotkey,
        callback: impl FnMut(&mut crate::event::HotkeyCtx) + 'static,
    ) -> HotkeyHandle {
        self.hotkey_handle(hotkey, callback)
    }

    /// 启动即隐藏：窗口创建后不显示，等托盘点击或全局热键唤起。
    ///
    /// 常驻托盘类应用用此项避免启动时闪一下窗口。
    ///
    /// # Panics
    ///
    /// debug 期，若既无托盘图标也无全局热键则 panic：那样用户将**永远无法唤起窗口**，
    /// 只能从任务管理器结束进程。这几乎总是误用而非有意为之。
    pub fn start_hidden(mut self) -> Self {
        self.cfg.start_hidden = true;
        self
    }

    /// 关闭请求转为隐藏窗口：按 ESC 或点标题栏关闭按钮时**隐藏而非退出进程**。
    ///
    /// 常驻托盘类应用用此项——用户的「关闭」意思通常是「收起来」，不是「杀掉它」。
    /// 真正的退出留给托盘右键菜单（`TrayMenuItem::item("退出", |ctx| ctx.quit())`）。
    ///
    /// 优先级低于既有拦截链：先关最顶层对话框，再问 [`Self::on_close_request`]；
    /// 只有拦截器放行后才轮到本项决定「关还是隐」。因此「有未保存数据时弹提示」与
    /// 「关闭即隐藏」可以并存。
    ///
    /// # Panics
    ///
    /// debug 期，若既无托盘图标也无全局热键则 panic：窗口一旦被隐藏就再也无法唤起。
    pub fn hide_on_close(mut self) -> Self {
        self.hide_on_close = true;
        self
    }

    /// 配置系统托盘图标（图标 + 提示 + 左键/双击 + 原生右键菜单）。
    /// 窗口创建后安装，窗口销毁时自动清理。截屏模式下忽略。
    pub fn tray(mut self, tray: platform::Tray) -> Self {
        self.cfg.tray = Some(tray);
        self
    }

    /// 无标题栏窗口（自定义标题栏）：去掉系统标题栏，客户区铺满整窗，
    /// 保留 Aero 吸附/阴影/缩放。用 `Element::window_drag()` 标记拖动区、
    /// `Element::window_button(...)` 放最小化/最大化/关闭按钮。
    pub fn frameless(mut self) -> Self {
        self.cfg.frameless = true;
        self
    }

    /// 单实例 + 二次运行激活/传参。`app_id` 唯一标识（建议含变体后缀，使 dev/release 互不干扰）。
    /// 仅首实例会被调用 `on_second_instance`（收到另一进程 argv 时，在 UI 线程）；
    /// 二次实例：argv 已转发给首实例，`run()` 直接返回、不建窗口。
    ///
    /// 把窗口带到前台**不需要**你动手：平台层在本回调返回后就会激活主窗口。
    ///
    /// # 为什么这个回调不收 `EventCtx`
    ///
    /// 与 [`App::channel`] / [`App::on_interval`] / [`App::on_close_request`] 不同，本回调
    /// 不是由主窗口驱动的：Windows 上它跑在一个独立的 message-only 窗口的 `wndproc` 里
    /// （`WM_COPYDATA`），macOS/Linux 上跑在 libdispatch 派回主线程的块里。两处都够不着
    /// 主窗口的宿主状态，而 `WM_COPYDATA` 又可能在任意嵌套消息泵里到达（托盘菜单的模态
    /// 循环、文件对话框），此刻去借宿主状态就是重入。
    ///
    /// 需要 ctx 的话自己搭一段回程即可——这正是通道存在的意义：
    ///
    /// ```no_run
    /// use windui::prelude::*;
    ///
    /// let mut app = App::new("单实例", 320, 160);
    /// let tx = app.channel::<Vec<String>>(|ctx, argv| {
    ///     ctx.toast(format!("又启动了一次：{} 个参数", argv.len()));
    /// });
    /// app.single_instance("myapp_dev", move |argv| {
    ///     let _ = tx.send(argv);
    /// })
    /// .content(Element::col().fill())
    /// .run();
    /// ```
    ///
    /// 代价是回调改为在**下一帧**执行：通道靠出帧排空，而窗口隐藏时（托盘常驻应用）不出帧，
    /// 消息会积压到窗口再次显示。库不替你做这层转发正是因为这个代价——多数
    /// `single_instance` 应用恰恰是托盘应用。
    pub fn single_instance(
        mut self,
        app_id: impl Into<String>,
        on_second_instance: impl FnMut(Vec<String>) + 'static,
    ) -> Self {
        self.single = Some(crate::single_instance::SingleInstance {
            app_id: app_id.into(),
            on_second: Box::new(on_second_instance),
        });
        self
    }

    /// 注册关闭请求拦截器。ESC 无对话框时，以及用户点击窗口关闭按钮时，
    /// 框架先调用此回调：返回 `true` 允许关闭，返回 `false` 取消关闭。
    /// 常用于"有未保存数据时弹提示"场景。
    ///
    /// 回调收 [`EventCtx`]（`self_id` 为根节点，见 [`App::on_interval`] 的同款说明），
    /// 故可以在挡下关闭的同时给出反馈——`ctx.toast(..)` 提示、`ctx.tree_mut()` 改树。
    ///
    /// # 弹确认框：必须异步，不能在这里同步弹
    ///
    /// 本回调的返回值是**同步**的（平台在 `WM_CLOSE` / `windowShouldClose:` 里等这个
    /// `bool`），而任何原生模态框自带消息泵，在这里同步弹会与宿主的泵冲突。正确形状是
    /// **先返回 `false` 挡住这一次，再另起一条路把"确认"送回来**。两条路，按需要选：
    ///
    /// **一、应用内模态（推荐，全同步、无额外管道）**：库自带的 `Element::dialog` 是画在
    /// 自己窗口里的，没有第二个消息泵的问题。拦截器把它打开并返回 `false`，对话框的
    /// "退出"按钮再 `ctx.request_close()`——那是"应用已决定关闭"的入口，不再经过本拦截器，
    /// 不会绕回来打转：
    ///
    /// ```no_run
    /// use windui::prelude::*;
    ///
    /// let dirty = signal(true);          // 有未保存的更改
    /// let asking = signal(false);        // 确认框是否显示
    ///
    /// let ui = Element::col().fill().child(Element::dialog(
    ///     asking,
    ///     Element::col()
    ///         .padding(20)
    ///         .spacing(12)
    ///         .child(Element::label("有未保存的更改，确认退出？"))
    ///         .child(Element::button("退出").on_click(move |ctx| {
    ///             dirty.set(false);      // 放行下一次关闭请求
    ///             ctx.request_close();
    ///         })),
    /// ));
    ///
    /// App::new("编辑器", 480, 320)
    ///     .on_close_request(move |_ctx| {
    ///         if dirty.get() {
    ///             asking.set(true);      // 弹自家对话框
    ///             return false;          // 挡下这一次
    ///         }
    ///         true
    ///     })
    ///     .content(ui)
    ///     .run();
    /// ```
    ///
    /// **二、原生 `MessageBoxW` 一类的阻塞流程**：用 `ctx.defer_blocking(f)` 把它排到事件
    /// 分发**完全返回**之后执行；那个闭包不收 `ctx`（它跑在宿主的分发之外），所以确认结果
    /// 要经 [`App::channel`] 的 `Sender` 回到 UI 线程，在 `on_message` 里用 ctx 收尾：
    ///
    /// ```no_run
    /// use windui::prelude::*;
    ///
    /// let mut app = App::new("编辑器", 480, 320);
    /// // 回程通道：确认结果从阻塞闭包送回 UI 线程，那里才有 ctx 能真正关窗。
    /// let tx = app.channel::<bool>(|ctx, ok| {
    ///     if ok {
    ///         ctx.request_close();
    ///     }
    /// });
    /// app.on_close_request(move |ctx| {
    ///     let tx = tx.clone();
    ///     ctx.defer_blocking(move || {
    ///         let ok = true;             // 此处换成真正的原生确认框
    ///         let _ = tx.send(ok);
    ///     });
    ///     false                          // 先挡下，等回程消息再关
    /// })
    /// .content(Element::col().fill())
    /// .run();
    /// ```
    ///
    /// 两条路都不适用时还有第三种：把"已确认"记在信号里、由拦截器下次直接放行——但那要求
    /// 用户再点一次关闭，通常不是想要的交互。
    pub fn on_close_request(mut self, f: impl FnMut(&mut EventCtx) -> bool + 'static) -> Self {
        self.close_handler = Some(Box::new(f));
        self
    }

    /// 窗口**从隐藏态**被唤起时调用（托盘点击 / 全局热键 / `WindowOp::Show`）。
    ///
    /// 常驻托盘工具的"每次唤起该做什么"落点：刷新一次数据、重置一段状态、清掉上次的
    /// 查询结果。只在隐藏→可见的**跃迁**上触发——已经可见时再按热键不算，否则每按一次
    /// 都会把界面重置一遍。
    ///
    /// 三个唤起入口里只有「控件请求」经过宿主，托盘与热键都由平台层直接执行，故这条
    /// 通知由平台发起（见 `AppHandler::on_window_shown`）。
    ///
    /// **焦点不必在这里处理**：[`Element::autofocus`](crate::ui::Element::autofocus) /
    /// [`autofocus_select_all`](crate::ui::Element::autofocus_select_all) 每次唤起都会
    /// 自动重新兑现一次——查询框重新拿到焦点、上次的词重新被全选，无需应用介入。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// App::new("查词", 480, 360)
    ///     .start_hidden()
    ///     .on_show(|ctx| ctx.toast_ok("已唤起"))
    ///     .content(Element::col())
    ///     .run();
    /// ```
    pub fn on_show(mut self, f: impl FnMut(&mut EventCtx) + 'static) -> Self {
        self.show_handler = Some(Box::new(f));
        self
    }

    pub fn run(mut self) {
        // 窗口会被隐藏（启动即隐 / 关闭转隐）却无任何唤起途径 = 用户再也看不到窗口，
        // 只能去任务管理器结束进程。在 run() 而非各 setter 里查：tray/hotkey 可能在其后才链上。
        debug_assert!(
            !(self.cfg.start_hidden || self.hide_on_close)
                || self.cfg.tray.is_some()
                || !self.cfg.hotkeys.is_empty(),
            "start_hidden / hide_on_close 需配合 tray 或 hotkey：否则窗口隐藏后无法被唤起"
        );
        let single = self.single.take();
        let theme_src = match self.theme_src {
            Some(h) => h,
            None => ThemeHandle::new(Rc::new(self.theme.unwrap_or_default())),
        };
        let waker = self.waker_shared.clone();
        let cfg = self.cfg;
        let handler: Box<dyn AppHandler> = if let Some(f) = self.render {
            Box::new(ClosureHandler { f })
        } else if let Some(root) = self.content {
            Box::new(UiHost::new(
                root,
                theme_src,
                cfg.bg,
                !self.bg_explicit,
                self.hotkey_ops.clone(),
                self.intervals,
                self.close_handler,
                self.show_handler,
                self.hide_on_close,
            ))
        } else {
            Box::new(ClosureHandler {
                f: Box::new(|_, _| {}),
            })
        };
        platform::run(cfg, handler, waker, single);
    }

    #[cfg(test)]
    fn into_handler_for_test(self) -> UiHost {
        let theme_src = match self.theme_src {
            Some(h) => h,
            None => ThemeHandle::new(Rc::new(self.theme.unwrap_or_default())),
        };
        UiHost::new(
            self.content.unwrap(),
            theme_src,
            self.cfg.bg,
            !self.bg_explicit,
            self.hotkey_ops.clone(),
            self.intervals,
            self.close_handler,
            self.show_handler,
            self.hide_on_close,
        )
    }

    fn shared_waker(&mut self) -> crate::sync::Waker {
        self.waker_shared
            .get_or_insert_with(WakerShared::new)
            .waker()
    }

    /// 注册 typed 消息通道。`on_message` 在 UI 线程调用（可写信号），并收一个
    /// [`EventCtx`]——后台任务完成后的**宿主级**反馈（`ctx.toast(..)` 轻提示、
    /// `ctx.request_close()`、`ctx.defer_blocking(..)`）只有它能表达：toast 是宿主
    /// 浮层而非控件状态，没有信号可以绑。
    ///
    /// 返回的 `Sender` 可 Clone 到任意后台线程；`send` 唤醒 UI 一帧。
    /// 每条消息各借一次 ctx，故一批消息里每条都能弹自己的 toast（不会互相覆盖）。
    ///
    /// ```no_run
    /// use windui::prelude::*;
    ///
    /// let done = signal(0u32);
    /// let mut app = App::new("后台任务", 320, 160);
    /// let tx = app.channel::<u32>(move |ctx, n| {
    ///     done.set(n);                       // 写信号：UI 自己刷新
    ///     ctx.toast_ok(format!("第 {n} 项完成"));  // 宿主浮层：只有 ctx 能给
    /// });
    /// std::thread::spawn(move || {
    ///     for i in 1..=3 {
    ///         let _ = tx.send(i);
    ///     }
    /// });
    /// app.content(Element::col().fill()).run();
    /// ```
    pub fn channel<Msg: Send + 'static>(
        &mut self,
        on_message: impl FnMut(&mut EventCtx, Msg) + 'static,
    ) -> Sender<Msg> {
        let waker = self.shared_waker();
        let (tx, pump) = new_channel(waker, on_message);
        crate::sync::register_pump(pump);
        tx
    }

    /// 注册 UI 线程定时回调（平台定时器，间隔内零 CPU）。可多次调用。
    ///
    /// 回调收 [`EventCtx`]，于是"到点了给个提示 / 到点了关窗"这类定时器也能表达，
    /// 不再只能写信号。ctx 的 `self_id` 是**根节点**（定时器不属于任何控件）：
    /// `ctx.bounds()` 因此是整个客户区、`ctx.mark_dirty()` 相当于整窗失效，
    /// 而 `ctx.capture()` 无效（没有指针事件可捕获，请求会被丢弃）。
    ///
    /// ```no_run
    /// use std::time::Duration;
    /// use windui::prelude::*;
    ///
    /// let left = signal(3u32);
    /// App::new("倒计时", 240, 120)
    ///     .on_interval(Duration::from_secs(1), move |ctx| {
    ///         left.update(|v| *v = v.saturating_sub(1));
    ///         if left.get() == 0 {
    ///             ctx.toast("时间到");
    ///         }
    ///     })
    ///     .content(Element::col().fill())
    ///     .run();
    /// ```
    pub fn on_interval(mut self, every: Duration, cb: impl FnMut(&mut EventCtx) + 'static) -> Self {
        self.intervals.push((every, Box::new(cb)));
        self
    }
}

/// 把底层渲染闭包适配为 AppHandler（不处理输入）。
struct ClosureHandler {
    f: RenderClosure,
}

impl AppHandler for ClosureHandler {
    fn render(&mut self, target: &mut dyn crate::render::RenderTarget, size: Size) {
        (self.f)(target, size);
    }
}

/// 控件树交互宿主：渲染 + 事件分发 + 焦点管理。
///
/// 自成一块的子职责（浮层、惯性、脏区仲裁、焦点）各自收进子模块的状态结构，
/// 本结构体只保留把它们串起来所需的东西。
struct UiHost {
    tree: Tree,
    engine: PlatformTextEngine,
    hover: Option<NodeId>,
    capture: Option<NodeId>,
    close: bool,
    /// DPI 缩放因子（逻辑→物理）。
    scale: f32,
    /// 焦点归属（Tab 顺序、焦点环、模态移交），见 [`focus`]。
    focus: FocusState,
    /// 上下文菜单浮层，见 [`menu`]。
    menu: MenuHost,
    /// 轻提示浮层，见 [`toast`]。
    toast: ToastHost,
    /// 悬停提示浮层，见 [`tooltip`]。
    tooltip: TooltipState,
    /// 触摸滚动手势（惯性 + 平移残差），见 [`fling`]。
    scroll: ScrollState,
    /// 最近一次指针事件的命中结果，供离屏截图路径诊断（见 `AppHandler::last_pointer_hit`）。
    /// 只在指针路径写，不参与任何绘制决策。
    last_hit: Option<crate::platform::PointerHit>,
    /// 局部重绘仲裁与后备缓冲，见 [`damage`]。
    damage: DamageState,
    /// 最近一帧的逻辑窗口尺寸（菜单弹出位置钳制用）。
    logical_size: Size,
    /// 活动主题快照（每帧从 theme_src 刷新，注入到线程局部供控件读取）。
    theme: Rc<Theme>,
    /// 运行期主题源：热切换时下一帧 render 据此刷新 theme。
    theme_src: ThemeHandle,
    /// 单调起点，用于动画相位时钟。
    start: std::time::Instant,
    /// 待执行的窗口操作（自定义标题栏按钮触发，平台分发后轮询执行）。
    pending_window_op: Option<WindowOp>,
    /// 待执行的原生文件对话框请求（平台在事件分发完全返回、OS 捕获同步后再执行）。
    pending_dialog: Option<DialogRequest>,
    /// 窗口背景色（与平台 fill 同色）：局部重绘的子缓冲按此填底，重建脏区与全窗一致。
    bg: Color,
    /// 清屏色是否随主题 palette.bg 热切换（未经 `App::bg` 显式固定时为 true）。
    bg_follows_theme: bool,
    /// 运行期热键操作队列（HotkeyHandle 写入；平台经 `take_hotkey_ops` 消费）。
    hotkey_ops: Rc<RefCell<Vec<(usize, crate::event::HotkeyOp)>>>,
    /// 一次「按下关闭浮层」后，吞掉随之而来的 Up：避免该 Up 下发到控件树重新激活
    /// 浮层下方控件（典型：下拉按钮点一下又弹一遍——Down 关、Up 再开）。
    swallow_up: bool,
    /// 本宿主的序号，用于认领/释放通道消费权（见 `crate::sync` 的 App 级登记表）。
    ///
    /// 通道本身挂在应用上而非本宿主上：主窗关掉之后，别的窗口要能继续收后台数据。
    /// 这里只留一个序号，回答"这一帧该不该由我来排空"。
    host_id: u64,
    /// 定时器回调列表（与 interval_durs 下标对应）。
    interval_cbs: Vec<AppCallback>,
    /// 定时器间隔列表（平台据此注册 SetTimer/NSTimer）。
    interval_durs: Vec<std::time::Duration>,
    /// 帧耗时浮层开关（环境变量 WINDUI_FPS 非空时开启）。
    show_fps: bool,
    /// 关闭请求拦截器：返回 true 允许关闭，false 取消。None 时默认允许。
    close_handler: Option<CloseHandler>,
    /// 窗口从隐藏态被唤起时的回调（见 [`App::on_show`]）。
    show_handler: Option<ShowHandler>,
    /// 关闭请求转为隐藏窗口（常驻托盘类应用）。
    hide_on_close: bool,
    /// 正在跑关闭决策链（防 `on_close_request` 回调内再请求关闭导致的自我递归）。
    resolving_close: bool,
    /// 待创建的子窗口（`ctx.open_window` 排入，平台在事件分发完全返回后取走）。
    pending_windows: Vec<crate::event::WindowRequest>,
    /// 本窗口内容构建期创建的信号（仅子窗有）。宿主析构时整批回收，使反复开关设置窗
    /// 不会让信号在全局 arena 里越积越多。
    ///
    /// 主窗为 `None`：它的内容在 `App` 上构建、活到进程结束，没有可回收的时机，
    /// 硬套一个作用域只会让「谁拥有这些信号」多出一条无意义的规则。
    ///
    /// 只管**构建期**的信号。控件内部的那些由控件自己的 `SignalScope` 管
    /// （`dyn_list`/`reorder`/`sortable_table` 都是如此），随 widget 析构一并回收。
    scope: Option<crate::signal::SignalScope>,
    /// 上一帧绘制中是否有控件请求了下一帧（帧末从 `anim` 全局态收割，见
    /// [`Self::finish_frame_damage`]）。平台的 `wants_animation` 读这里而**不是**直接读
    /// `anim::animation_requested()`：那是个线程全局位，同线程跑多个窗口时，后绘制的
    /// 窗口在帧首 `anim::reset_request()` 会清掉前一个窗口刚提出的续帧请求，让它的动画
    /// 掉帧甚至冻住。收进实例字段后，每个宿主只回答自己那一份。
    wants_anim: bool,
    /// 上一帧续帧请求里最早的截止（距那一帧的毫秒数），0 = 下一帧就要。与 `wants_anim`
    /// 同源同期收割，理由相同（多窗口下不能读线程全局位）。`wants_anim` 为假时无意义。
    next_delay: u32,
    /// 所属窗口是否激活（前台）。平台经 `on_window_activated` 通知，每帧注入给
    /// `ui::caret`：失活窗口的光标转静态、不再续帧。
    window_active: bool,
    /// 本帧实际更新的物理区域；`None` = 整帧。供平台收窄呈现范围（见
    /// [`crate::platform::AppHandler::last_frame_damage`]）。
    last_present: Option<Rect>,
}

impl Drop for UiHost {
    /// 交还通道消费权。持有者是主窗时，这一步让主窗关闭后仍活着的窗口能接手排空——
    /// 漏了它，通道就会连同那个已经不存在的窗口一起沉默（消息照常入队、永远没人消费）。
    fn drop(&mut self) {
        crate::sync::release_channel_owner(self.host_id);
    }
}

impl UiHost {
    /// 关闭请求的统一决策，ESC 与标题栏关闭按钮共用。返回 true 表示应当真正关闭窗口。
    ///
    /// 优先级：关最顶层对话框 → 问 `close_handler` → 按 `hide_on_close` 决定关还是隐。
    ///
    /// 隐藏走既有的 `WindowOp` 管道而非在此直接操作窗口：本函数在平台层持有窗口状态
    /// 借用期间被调用（win32 `WM_CLOSE` / macOS `windowShouldClose:`），此处碰 OS 会
    /// 同步重入（见 AGENTS.md 铁律 6）。
    fn resolve_close(&mut self) -> bool {
        // 防自我递归：`on_close_request` 的回调里再调 `ctx.request_close()` 会经
        // `apply_app_effects` 回到这里。那种写法本就没有意义（正在回答"能不能关"），
        // 直接放行不再重入。
        if self.resolving_close {
            return true;
        }
        self.resolving_close = true;
        let out = self.resolve_close_inner();
        self.resolving_close = false;
        out
    }

    fn resolve_close_inner(&mut self) -> bool {
        // 优先关闭本窗口最顶层的可见对话框（不退出窗口）。
        if self.tree.close_topmost_modal() {
            // 对话框被关闭，需要重绘以隐藏遮罩。
            self.damage.needs_full = true;
            return false;
        }
        // 无对话框时询问 close_handler，默认允许关闭。
        let allowed = self.ask_close_handler();
        if allowed && self.hide_on_close {
            self.pending_window_op = Some(WindowOp::Hide);
            return false;
        }
        allowed
    }

    /// 询问关闭请求拦截器（[`App::on_close_request`]），无拦截器时默认放行。
    ///
    /// 闭包先 `take` 出来再借树：`run_detached` 要 `&mut self.tree`，而闭包挂在同一个
    /// `self` 上，不取出就是两次可变借用。跑完放回，且**只在回调没自己换过拦截器时**
    /// 放回（对齐 `call_on_event` 的取出—放回契约）。
    ///
    /// 树上没有根节点时借不出 `EventCtx`，回调整个跳过并放行关闭：那种状态下已经没有
    /// 界面可保护，挡住关闭只会让窗口关不掉。
    fn ask_close_handler(&mut self) -> bool {
        let Some(mut h) = self.close_handler.take() else {
            return true;
        };
        let Some(root) = self.tree.root else {
            self.close_handler = Some(h);
            return true;
        };
        let mut allowed = true;
        let res = self.tree.run_detached(root, |ctx| allowed = h(ctx));
        if self.close_handler.is_none() {
            self.close_handler = Some(h);
        }
        self.apply_app_effects(res);
        allowed
    }

    /// App 级回调（`on_interval` / `channel` 的 `on_message` / `on_close_request`）借
    /// [`EventCtx`] 的统一时机：以**根节点**为 `self_id`（这些回调不属于任何控件），
    /// 副作用交 `apply_dispatch_effects` 落地——与指针/键盘分发同一条消费路径。
    ///
    /// `self_id` 取根节点的后果，写进各自的 rustdoc 供调用方预期：`ctx.bounds()` 是整个
    /// 客户区、`mark_dirty()` 等于整窗失效、`request_focus()` 把焦点落在根容器上；
    /// `capture()` 无效（`run_detached` 丢弃捕获请求——没有指针事件可捕获）。
    ///
    /// 焦点来源按 `Pointer` 记：这些回调不在键盘导航中，不该点亮焦点环。
    /// 完事置 `needs_relayout`——回调可以经 `ctx.tree_mut()` 改结构，交给 render 里的
    /// 结构签名去判本帧走局部还是整窗（与键盘路径同款保守）。
    fn apply_app_effects(&mut self, res: DispatchResult) {
        let (_, damage, _) = self.apply_dispatch_effects(res, FocusSource::Pointer, None);
        self.apply_damage(damage);
        self.damage.needs_relayout = true;
    }

    /// 跑应用的唤起回调（`App::on_show`），并落地它请求的副作用。
    ///
    /// 借 ctx 走 `run_detached`，与菜单动作、`on_close_request` 同一条路——回调因此能
    /// `ctx.toast_ok` / `ctx.request_close` / 改信号，与控件回调齐平。
    /// 回调先取出再放回：它可能反过来再触发一次唤起（`ctx` 能请求窗口操作），
    /// 独占借用会当场冲突。
    fn run_show_handler(&mut self) {
        let Some(mut f) = self.show_handler.take() else {
            return;
        };
        let root = self.tree.root;
        if let Some(root) = root {
            let res = self.tree.run_detached(root, |ctx| f(ctx));
            self.apply_app_effects(res);
        }
        self.show_handler = Some(f);
    }

    /// 落地**已决定**的关闭（`EventCtx::force_close`）：`hide_on_close` 时转为隐藏，
    /// 但不问 `close_handler`、也不先关对话框。
    ///
    /// 关闭意图有两类，走两条路，别搞混：
    /// - **用户请求关闭**（系统 × / Alt+F4 / ESC / 自绘 × 按钮 / `ctx.request_close()`）
    ///   → [`Self::resolve_close`]：关顶层对话框 → 问 `on_close_request` → `hide_on_close`。
    /// - **应用已决定关闭**（`ctx.force_close()`）→ **本函数**。
    ///
    /// 自绘 × 按钮曾经走本函数（`request_close` 当时表示"应用已决定"），后果是
    /// `on_close_request` 拦得住 Alt+F4 却拦不住 ×——而无边框窗口的 × 恰恰是主入口，
    /// 守卫因此形同虚设。现在它与系统 × 同走决策链。
    fn apply_close_intent(&mut self) {
        if self.hide_on_close {
            self.pending_window_op = Some(WindowOp::Hide);
        } else {
            self.close = true;
        }
    }

    fn new(
        root: Element,
        theme_src: ThemeHandle,
        bg: Color,
        bg_follows_theme: bool,
        hotkey_ops: Rc<RefCell<Vec<(usize, crate::event::HotkeyOp)>>>,
        intervals: Vec<(std::time::Duration, AppCallback)>,
        close_handler: Option<CloseHandler>,
        show_handler: Option<ShowHandler>,
        hide_on_close: bool,
    ) -> Self {
        // 尽早注入，使首个事件（首帧渲染前）也能读到正确主题。
        let theme = theme_src.current();
        crate::theme::set_current(theme.clone());
        let mut tree = Tree::new();
        tree.root = Some(root.build(&mut tree));
        tree.clipboard = Some(Box::new(crate::platform::Clipboard));
        let (interval_durs, interval_cbs): (Vec<_>, Vec<_>) = intervals.into_iter().unzip();
        Self {
            tree,
            engine: PlatformTextEngine::new(),
            hover: None,
            capture: None,
            close: false,
            scale: 1.0,
            focus: FocusState::default(),
            menu: MenuHost::default(),
            toast: ToastHost::default(),
            tooltip: TooltipState::default(),
            scroll: ScrollState::default(),
            last_hit: None,
            damage: DamageState::default(),
            logical_size: Size::new(0, 0),
            theme,
            theme_src,
            start: std::time::Instant::now(),
            pending_window_op: None,
            pending_dialog: None,
            bg,
            bg_follows_theme,
            hotkey_ops,
            swallow_up: false,
            host_id: crate::sync::next_host_id(),
            interval_cbs,
            interval_durs,
            show_fps: std::env::var("WINDUI_FPS").is_ok_and(|v| v != "0" && !v.is_empty()),
            close_handler,
            show_handler,
            hide_on_close,
            resolving_close: false,
            pending_windows: Vec::new(),
            scope: None,
            wants_anim: false,
            next_delay: 0,
            window_active: true,
            last_present: None,
        }
    }

    /// 把帧时钟同步到当前时刻并返回它。
    ///
    /// `anim::clock_ms()` 是控件唯一的时间源，而这里是它唯一的写入点。若只在 render 里刷，
    /// 空闲不出帧期间它会冻结在上一帧，控件在**事件路径**读到的便是「上一帧几点」而非
    /// 「现在几点」——两次交互之间的静默期会被整段算进任何基于它的时长判定（长按、双击、
    /// 拖动速度）。故事件分发前也刷一次，使 `EventCtx::now_ms()` 始终可信。
    ///
    /// 对动画相位无影响：所有 `Transition::retarget` 都在 paint 路径，那里本就会再刷一次。
    fn sync_clock(&self) -> u64 {
        let now = self.start.elapsed().as_millis() as u64;
        crate::anim::set_clock_ms(now);
        now
    }

    /// 结构变化后按当前指针位置重新求值 hover：合成一个 Move 事件复用既有的 Enter/Leave
    /// 逻辑——旧 hover 节点若被新浮层遮住会收到 Leave（清掉残留高亮），指针下的新节点收到
    /// Enter。修正"模态弹出/关闭、切页等在光标静止时改变命中节点导致 hover 卡住"。
    /// 菜单浮层有独立命中逻辑，激活时跳过。
    fn resync_hover_after_relayout(&mut self) {
        if self.menu.is_open() {
            return;
        }
        let mut hover = self.hover;
        let mut capture = self.capture;
        let _ = self.tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, self.tooltip.pos, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        self.hover = hover;
        self.capture = capture;
    }

    /// 一次分发的副作用消费（指针与键盘共用）。返回 `(repaint, damage, consumed)`
    /// ——这三项与事件类型强相关（Move 走局部、Escape 要看有没有被消费），交调用方处理。
    ///
    /// **刻意用解构而非逐字段读取**：`DispatchResult` 新增字段时这里会编译失败，逼作者
    /// 当场决定它归谁管。此前两条路径各自手写消费，键盘侧漏掉 `menu` 与 `focus` 没有
    /// 任何征兆——请求型副作用漏接是静默的，表现只是"按空格没反应"。
    ///
    /// `blur_at`：指针路径专用。`Some(pos)` 表示本次是可参与失焦裁决的按下（Down 且
    /// 事件前无捕获）；无人请求焦点时据此判断该不该清空。必须留在 `focus` 的 else 位置，
    /// 挪到 `menu` 之后会让"右键点空白"的菜单 target 从旧焦点变成 root。
    fn apply_dispatch_effects(
        &mut self,
        res: DispatchResult,
        focus_from: FocusSource,
        blur_at: Option<Point>,
    ) -> (bool, DamageReq, bool) {
        let DispatchResult {
            mut repaint,
            damage,
            close,
            close_forced,
            focus,
            consumed,
            menu,
            open_url,
            window_op,
            toast,
            dialog,
            open_windows,
        } = res;
        // 开窗请求排队：平台在事件分发完全返回后 `take_new_windows` 取走并建窗。
        self.pending_windows.extend(open_windows);
        if let Some(f) = focus {
            let old = self.focus.current;
            self.tree.set_focused(Some(f), old);
            self.focus.current = Some(f);
            match focus_from {
                // 鼠标聚焦不显示焦点环，保持纯鼠标操作的纯净观感。
                FocusSource::Pointer => {
                    self.focus.visible = false;
                    // 焦点在两个控件之间转移时必须整窗：**旧**焦点要重绘才能擦掉自己的
                    // 光标与焦点环，而它没有收到本次事件，压根不在脏区里——脏区只来自
                    // 被点中的那个控件。多个文本框互相点击时的光标竖条残留就是这么来的，
                    // 新框画出光标、旧框的光标仍留在后备缓冲里，直到下一次全窗刷新。
                    //
                    // 不改成"把旧节点矩形并进脏区"，与下面 blur 分支同理：焦点环画在节点
                    // 框外 1px，而此刻旧节点的 focused 已置 false，按它的脏区走会残留一圈。
                    // 焦点转移是人手点击的频率，整窗一帧换取正确性是划算的。
                    if old.is_some_and(|o| o != f) {
                        self.damage.needs_full = true;
                    }
                }
                // 键盘聚焦相反——本来就在键盘导航中。焦点环跨节点变化 → 整窗。
                FocusSource::Keyboard => {
                    self.focus.visible = true;
                    self.damage.needs_full = true;
                }
            }
        } else if let Some(pos) = blur_at {
            // 点在当前焦点控件之外 → 清空焦点（网页 blur 语义：焦点归属由宿主每次按下
            // 重新裁决，而不是"没人认领就维持原样"）。
            if let Some(f) = self.focus.current {
                if !self.tree.hit_inside(pos, f) {
                    self.tree.set_focused(None, Some(f));
                    self.focus.current = None;
                    self.focus.visible = false;
                    // 焦点环画在节点框外 1px，而 damage_rect 的额外余量只对 focused 节点
                    // 给足；此刻 focused 已置 false，按脏区走会残留一圈，故整窗。
                    self.damage.needs_full = true;
                    repaint = true;
                }
            }
        }
        if close_forced {
            self.apply_close_intent();
        } else if close && self.resolve_close() {
            self.close = true;
        }
        // 浮层菜单。target 是 SendKey 动作的派发对象：优先当前焦点控件（如 TextInput
        // 的右键剪贴板项），否则回退根节点（on_context_menu 容器不可聚焦，其菜单项多为
        // Run 闭包、不依赖 target）。
        if let Some(req) = menu {
            if let Some(target) = self.focus.current.or(self.tree.root) {
                self.open_menu(req, target);
            }
        }
        // 链接点击等：交平台用默认程序打开。
        if let Some(url) = open_url {
            platform::open_url(&url);
        }
        // 窗口操作（自定义标题栏按钮）：暂存，平台分发后轮询执行（需 hwnd）。
        if window_op.is_some() {
            self.pending_window_op = window_op;
        }
        // 原生文件对话框：暂存，待事件分发完全返回、OS 捕获同步后再执行，避免在事件
        // 回调栈内重入阻塞式模态对话框（见 DialogRequest 文档）。
        if dialog.is_some() {
            self.pending_dialog = dialog;
        }
        // 轻提示：居中浮层 + 淡入淡出 + 定时消失。
        if let Some(req) = toast {
            self.show_toast(req);
        }
        (repaint, damage, consumed)
    }

    // ---- 渲染调度：`render` 拆出的各阶段 ----

    /// 帧起始：排空跨线程通道、刷新主题快照与清屏色。
    fn begin_frame(&mut self) {
        self.drain_channels();
        // 窗口激活态注入线程局部，供光标判定要不要闪（与帧时钟、主题快照同一套路数：
        // 多窗口下每帧各注各的）。
        crate::ui::caret::set_window_active(self.window_active);
        // 从运行期句柄刷新主题快照（热切换下一帧生效），注入线程局部供控件读取。
        self.theme = self.theme_src.current();
        crate::theme::set_current(self.theme.clone());
        // 清屏色随主题（未经 App::bg 显式固定时）：暗色主题下窗口底色同步转暗。
        if self.bg_follows_theme {
            self.bg = self.theme.palette.bg;
        }
    }

    /// 跨线程消息：渲染前在 UI 线程一次性排空所有通道，每条消息借一个 App 级
    /// [`EventCtx`] 交给 `on_message`（见 [`Self::apply_app_effects`] 对 `self_id` 的说明），
    /// 副作用与控件回调走同一条消费路径。
    ///
    /// 契约：一帧 render 消费所有 pump 的全部积压消息（唤醒合并/批处理）——
    /// 多个 channel 共享单一 Waker，勿改成每 pump 独立 wake/独立帧。
    ///
    /// pump 先整体摘下来再跑：pump 要 `&mut self.tree`，而它产出的副作用要整个
    /// `&mut self` 才能落地，同时持有两者过不了借用检查。运行期不会有人往 `pumps` 里
    /// 追加（`App::channel` 只在建窗前可调），故直接装回去是安全的。
    fn drain_channels(&mut self) {
        let Some(root) = self.tree.root else {
            return;
        };
        // 只有消费权的持有者排空：pump 借的是**本宿主的树**，产出的 toast/focus/menu
        // 会落到本窗口上。若谁先出帧谁消费，同一条消息的提示会随渲染次序在窗口间跳。
        if !crate::sync::claim_channel_owner(self.host_id) {
            return;
        }
        let mut pumps = crate::sync::take_pumps();
        if pumps.is_empty() {
            return;
        }
        let mut results = Vec::new();
        for pump in pumps.iter_mut() {
            results.append(&mut pump(&mut self.tree, root));
        }
        crate::sync::put_pumps(pumps);
        for mut res in results {
            // 关窗与窗口操作在这条路径上一律丢弃。通道消息是发给**应用**的，而这里
            // 恰好借了哪个窗口的树是实现细节（主窗关掉后就换了一个）——让一条后台消息
            // 去决定"关掉/最小化当前这个窗口"，目标是不确定的。窗口的开关本就该由那个
            // 窗口自己的交互或 `on_interval` 触发。
            //
            // `open_windows` 反而保留：开窗是应用级动作，由宿主排队后统一建窗，不依赖
            // 是哪棵树发起的——主窗关闭后仍能从通道里开出新窗口，正是本次把 pump 提到
            // App 级要换来的能力。
            res.close = false;
            res.close_forced = false;
            res.window_op = None;
            self.apply_app_effects(res);
        }
    }

    /// 交互/结构可能改变布局：先重排，再用结构签名判定本次是否仅为局部视觉变化。
    /// 签名变（显隐/位移/尺寸，如对话框弹出、切页）→ 影响区域不可局部化 → 升级整窗；
    /// 签名不变（打字、按钮、勾选）→ 沿用控件上报的交互脏区走 1ms 局部重绘。
    ///
    /// 返回本帧是否已经布局过（全窗路径据此跳过重复的 `layout_root`）。
    fn relayout_if_needed(&mut self, logical: Size) -> bool {
        if !self.damage.needs_relayout {
            return false;
        }
        self.tree.layout_root(logical, &mut self.engine);
        let sig = self.tree.layout_signature();
        if self.damage.sig_valid && sig != self.damage.last_layout_sig {
            self.damage.needs_full = true;
            // 结构变化（模态弹出/关闭、切页等）的两类交互态修正（对齐 Flutter MouseTracker /
            // Qt 模态弹出补发 leave 的做法）：
            // 1) 被隐藏的控件（如关闭它所在的对话框）重置其 hover/press 与补间，避免下次
            //    显示瞬间闪出旧的按下/悬停态；
            self.tree.reset_hidden_interactions();
            // 2) 在光标静止时被新浮层遮住的旧 hover 节点补发 Leave/Enter，清掉残留高亮。
            self.resync_hover_after_relayout();
        }
        self.damage.last_layout_sig = sig;
        self.damage.sig_valid = true;
        self.damage.needs_relayout = false;
        true
    }

    /// 全窗帧的绘制前准备：补齐布局、上屏响应式 toast、刷新焦点、清除过期 toast。
    fn prepare_full_frame(&mut self, logical: Size, laid_out: bool, now_ms: u64) {
        // 重排块已布局过则跳过，避免重复 layout_root。
        if !laid_out {
            self.tree.layout_root(logical, &mut self.engine);
            self.damage.last_layout_sig = self.tree.layout_signature();
            self.damage.sig_valid = true;
        }
        // 全窗路径的这次 layout 也可能有响应式 toast（上面 needs_relayout 未触发时）；
        // 本就走全窗重绘，取走后随本帧 paint 上屏即可。
        self.flush_pending_toasts();
        // 布局后结构稳定：刷新 Tab 顺序、模态移交、归一化失效焦点。
        self.refresh_focus();
        // 过期 toast 先清除（需要 &mut self，必须在借用 self.engine 生成 canvas 之前完成）。
        self.retain_live_toasts(now_ms);
    }
}

/// 解析 `--key` 的键名，支持 `ctrl+` / `shift+` 前缀（可叠加，大小写不敏感）。
///
/// 只认**具名键**：字符输入走 `--type`，那条路不必逐个列举字符，也天然支持中文。
fn parse_key_spec(spec: &str) -> Option<crate::event::KeyEvent> {
    use crate::event::{Key, KeyEvent};
    let (mut ctrl, mut shift) = (false, false);
    let mut rest = spec;
    loop {
        let lower = rest.to_ascii_lowercase();
        if let Some(r) = lower.strip_prefix("ctrl+") {
            ctrl = true;
            rest = &rest[rest.len() - r.len()..];
        } else if let Some(r) = lower.strip_prefix("shift+") {
            shift = true;
            rest = &rest[rest.len() - r.len()..];
        } else {
            break;
        }
    }
    let key = match rest.to_ascii_lowercase().as_str() {
        "enter" | "return" => Key::Enter,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "space" => Key::Space,
        // 单个字符也放行（`--key ctrl+a` 全选）：Ctrl 组合在本库走 `Key::Other(vk)`，
        // 与 TextInput 对 Ctrl+A/C/V/X 的处理对齐。
        s if s.chars().count() == 1 => {
            let c = s.chars().next()?;
            if ctrl && c.is_ascii_alphabetic() {
                Key::Other(c.to_ascii_uppercase() as u32)
            } else {
                Key::Char(rest.chars().next()?)
            }
        }
        _ => return None,
    };
    Some(KeyEvent {
        key,
        pressed: true,
        shift,
        ctrl,
    })
}

/// 帧耗时浮层（WINDUI_FPS=1）：左上角显示本帧渲染耗时与估算 fps，用于排查卡顿。
fn paint_fps(canvas: &mut dyn crate::render::Canvas, frame_t0: std::time::Instant) {
    let ms = frame_t0.elapsed().as_secs_f32() * 1000.0;
    let fps = if ms > 0.01 { 1000.0 / ms } else { 999.0 };
    let txt = format!("{ms:.1} ms  ~{fps:.0} fps");
    canvas.fill_round_rect(
        4.0,
        4.0,
        132.0,
        22.0,
        4.0,
        &Paint::fill(Color::rgba(0, 0, 0, 180)),
    );
    canvas.draw_text(
        &txt,
        Rect::new(10, 4, 126, 22),
        Color::rgba(0, 255, 120, 255),
        crate::spec::Align::Start,
        &crate::text::TextStyle::new(12.0),
    );
}

/// 渲染画像（WINDUI_PROF=1）：打印本帧走的路径与耗时。
fn prof_frame(kind: &str, frame_t0: std::time::Instant) {
    if crate::render::prof::enabled() {
        eprintln!(
            "[prof] {kind} {:.2}ms  {}",
            frame_t0.elapsed().as_secs_f64() * 1000.0,
            crate::render::prof::take_summary()
        );
    }
}

impl AppHandler for UiHost {
    /// 一帧的调度流程：帧起始 → 惯性步进 → 条件重排 → 重绘决策 →（局部快路 |
    /// 全窗绘制 + 三层浮层 + 种入后备缓冲）。各段的实质逻辑都在下面的私有方法与
    /// 子模块里，这里只负责顺序。
    fn render(&mut self, target: &mut dyn crate::render::RenderTarget, size: Size) {
        // 帧耗时计时（WINDUI_FPS=1 时在左上角显示，用于排查渲染开销）。
        let frame_t0 = std::time::Instant::now();
        self.begin_frame();
        // 动画：清上一帧请求/脏区并刷新帧时钟，绘制中控件可重新请求。
        crate::anim::reset_request();
        let now_ms = self.sync_clock();
        // 惯性滑动：在布局前推进 scroll_y，本帧 arrange 据此钳制并重排。
        self.step_fling(now_ms);
        // pixmap 是物理像素；布局用逻辑坐标（物理 / scale），绘制时再 ×scale 放大。
        let s = self.scale;
        let logical = Size::new(
            (size.w as f32 / s).round().max(1.0) as i32,
            (size.h as f32 / s).round().max(1.0) as i32,
        );
        self.logical_size = logical;

        let laid_out = self.relayout_if_needed(logical);
        // 响应式相位（本帧 layout 内）可能发出 toast（如 toast_sink 监听 feedback 信号）。
        // 须在重绘决策前上屏：show_toast 置 needs_full 并使 overlay 成立，令新 toast 被绘制，
        // 否则会走局部重绘的 early-return 而漏画。
        self.flush_pending_toasts();

        let (do_full, damage) = self.decide_repaint(target, size);

        if !do_full {
            let pixmap = target.as_pixmap().expect("软目标必有 pixmap");
            self.render_partial(pixmap, size, s, damage.unwrap());
            self.finish_frame_damage();
            prof_frame("partial", frame_t0);
            return;
        }

        // ---- 全窗重绘：完整布局 + 整树绘制 + 浮层；结果种入后备缓冲供后续局部帧复用。----
        // 清底由这里做，不再由平台每帧无条件 fill：局部帧根本不需要清（内容随后被脏区
        // 覆盖），而那一次 fill 是整窗的，与脏区多小无关。
        if let Some(pixmap) = target.as_pixmap() {
            pixmap.fill(tiny_skia::Color::from_rgba8(
                self.bg.r, self.bg.g, self.bg.b, self.bg.a,
            ));
        }
        self.last_present = None;
        self.prepare_full_frame(logical, laid_out, now_ms);
        // canvas 借的是 self.engine，与下面各浮层状态是不相交字段，借用安全。
        let mut canvas = target.make_canvas(&mut self.engine, s);
        self.tree.paint(&mut *canvas);
        self.tooltip.paint(
            &mut *canvas,
            &self.tree,
            self.hover,
            self.menu.is_open(),
            &self.theme,
            self.logical_size,
            now_ms,
        );
        self.toast
            .paint(&mut *canvas, &self.theme, self.logical_size, now_ms);
        // 菜单画在 toast 之后，确保菜单不被 toast 遮挡。
        self.menu.paint(&mut *canvas, &self.theme);
        if self.show_fps {
            paint_fps(&mut *canvas, frame_t0);
        }
        drop(canvas);
        // 种入后备缓冲（整窗），供后续局部帧重建未变区域。
        // GPU 后端（as_pixmap=None）不走局部重绘，seed_back 无需调用；软后端必有 pixmap。
        if let Some(pixmap) = target.as_pixmap() {
            self.seed_back(pixmap, size);
        }
        self.finish_frame_damage();
        prof_frame("full", frame_t0);
    }

    fn on_pointer(&mut self, mut ev: crate::event::PointerEvent) -> bool {
        self.sync_clock();
        // 物理坐标 → 逻辑坐标（布局与命中均在逻辑空间）。
        let s = self.scale;
        ev.pos = Point::new(
            (ev.pos.x as f32 / s).round() as i32,
            (ev.pos.y as f32 / s).round() as i32,
        );
        // 按下打断进行中的惯性滑动（点击/拖动停住动量，符合滚动视图习惯）。
        if ev.kind == PointerKind::Down {
            self.clear_fling();
        }
        // 菜单激活时独占指针：命中项/点外关闭，不下发到控件树。
        if self.menu.is_open() {
            return self.handle_menu_pointer(ev);
        }
        // toast 浮层在控件树之上：命中则独占该事件。
        if self.toast.is_active() && self.handle_toast_pointer(ev) {
            return true;
        }
        // 关闭浮层的那次点击：Down 已关菜单，配对的 Up 在此吞掉（不重新激活下方控件）。
        // 新的一次按下（非关闭浮层）清掉标记，确保只吞紧随关闭的那一个 Up。
        match ev.kind {
            PointerKind::Up if self.swallow_up => {
                self.swallow_up = false;
                return false;
            }
            PointerKind::Down => self.swallow_up = false,
            _ => {}
        }
        let old_hover = self.hover;
        // 本次事件**之前**是否已有捕获：拖动过程中（按住不放）的按下不参与失焦判定。
        // 取事件前的值而非之后——Down 自身常会设置捕获，用之后的值会把"点在捕获型
        // 控件上"也算作拖动中，失焦就永远轮不到。
        let had_capture = self.capture.is_some();
        let mut hover = self.hover;
        let mut capture = self.capture;
        let mut res = self.tree.dispatch_pointer(ev, &mut hover, &mut capture);
        // 诊断快照：命中的是哪个节点、有没有被消费。用 `hit_test` 而不是 `hover`——
        // 后者在 Up/Leave 上会被清掉，而我们要记的是「这一下点到哪儿了」。
        let hit = self.tree.hit_test(ev.pos);
        self.last_hit = Some(crate::platform::PointerHit {
            node: hit,
            bounds: hit.map(|id| self.tree.abs_bounds(id)).unwrap_or_default(),
            consumed: res.consumed,
        });
        self.hover = hover;
        self.capture = capture;
        // 悬停提示：记录指针位置；悬停节点变化时重新计时（隐藏旧提示、对新节点计时）。
        // 按下抑制提示（点完控件不原地弹出盖住它），指针再次移动后解除抑制并重新计时。
        self.tooltip.pos = ev.pos;
        let now_ms = self.start.elapsed().as_millis() as u64;
        if hover != old_hover {
            self.tooltip.since_ms = now_ms;
            self.tooltip.suppressed = false;
            // tooltip 浮层画在控件自身范围之外（指针旁），普通 Label 又没有 hover
            // 视觉、不会主动上报 repaint——若不在此强制请求一次重绘，移出后旧提示
            // 残留不消失、移入后也要等到别的事件凑巧触发重绘才会出现（不稳定）。
            let node_has_tooltip = |id: Option<NodeId>| {
                id.is_some_and(|h| self.tree.get(h).is_some_and(|n| n.tooltip.is_some()))
            };
            if node_has_tooltip(old_hover) || node_has_tooltip(hover) {
                res.repaint = true;
            }
        }
        match ev.kind {
            PointerKind::Down => self.tooltip.suppressed = true,
            PointerKind::Move if self.tooltip.suppressed => {
                self.tooltip.suppressed = false;
                self.tooltip.since_ms = now_ms;
            }
            _ => {}
        }
        // 可参与失焦裁决的按下：Down 且事件前无捕获（拖动中的按下不算）。
        let blur_at = (ev.kind == PointerKind::Down && !had_capture).then_some(ev.pos);
        let (repaint, damage, _) = self.apply_dispatch_effects(res, FocusSource::Pointer, blur_at);
        // hover/拖动（Move）自包含（控件自身视觉）→ 直接用其脏区走局部。
        // 点击等可能改变布局/显隐 → 置 needs_relayout：render 重排后用结构签名判定，
        // 签名不变才用控件脏区走局部，变了（对话框/切页等）自动升级整窗。
        self.apply_damage(damage);
        if !matches!(ev.kind, PointerKind::Move) {
            self.damage.needs_relayout = true;
        }
        repaint
    }

    fn request_full_frame(&mut self) {
        self.damage.needs_full = true;
    }

    fn pending_damage(&self) -> Option<crate::geometry::Rect> {
        // 逻辑 → 物理，并外扩一像素吸收取整误差。
        self.next_frame_damage()
            .map(|r| r.scaled(self.scale).inflate(1))
    }

    fn last_frame_damage(&self) -> Option<crate::geometry::Rect> {
        self.last_present
    }

    fn on_window_activated(&mut self, active: bool) -> bool {
        if self.window_active == active {
            return false;
        }
        self.window_active = active;
        // 重绘一帧把光标切到（或切回）该有的状态：失活时它停在最后一帧的相位上，
        // 那可能正好是"淡到一半"，不重绘就定格成一根半透明的杠。
        //
        // 走整窗而非脏区：这一帧的目的就是让 caret 在新的激活态下重新决定自己，
        // 而失活前的最后一帧脏区未必覆盖得到它（比如刚好停在全灭相位、什么都没画）。
        self.damage.needs_full = true;
        true
    }

    fn on_window_shown(&mut self) -> bool {
        // 让 autofocus 重新兑现一次：从托盘/热键唤起是一段新交互的开始，与页面重新载入
        // 同构——查询框该重新拿到焦点、上次的词该重新被全选。没有这一步，第二次唤起时
        // 焦点还停在上次离开的地方，`autofocus_select_all` 的"全选"也不会再发生。
        self.rearm_autofocus();
        // 结构未变，故不能只靠脏区：焦点与选区都是纯视觉变化，必须整窗，理由同
        // `refresh_focus` 之后的那次重绘。
        self.damage.needs_relayout = true;
        self.damage.needs_full = true;
        self.run_show_handler();
        true
    }

    fn last_pointer_hit(&self) -> Option<crate::platform::PointerHit> {
        self.last_hit
    }

    fn on_key(&mut self, ev: crate::event::KeyEvent) -> bool {
        self.sync_clock();
        // 菜单激活时由浮层独占键盘：↑↓ 选项、←→ 进出子菜单、回车/空格执行、
        // Escape 关闭，其余吞掉（避免打到被遮住的控件上）。
        if self.menu.is_open() {
            return self.handle_menu_key(ev);
        }
        // 一律先交给焦点控件；宿主的 Tab 焦点导航与 Escape 关窗都是**兜底**，只在焦点
        // 控件没要这个键时才轮到。
        //
        // Tab 此前是唯一抢在分发之前的键——那让「输入框想自己用 Tab」在应用层无从表达
        // （补全接受、候选切换：Tab 在查询框里是常规语义）。兜底而非抢先也正是网页的做法：
        // 键先到焦点元素，它 `preventDefault()` 掉就轮不到浏览器导航。同文件的 Escape
        // 本就是这个形状，Tab 是那个不一致的例外。
        //
        // 对现有行为零影响：当前没有任何控件消费 Tab，故未声明 `on_nav_key` 的控件上
        // Tab 照旧走焦点导航（`tab_still_navigates_when_control_declines_it` 钉住）。
        let res = self.tree.dispatch_key(ev, self.focus.current);
        // 键盘路径不参与失焦裁决（没有"点在别处"这回事），故 blur_at 恒为 None。
        let (mut repaint, damage, consumed) =
            self.apply_dispatch_effects(res, FocusSource::Keyboard, None);
        // 键盘改动可能影响布局（文本增减）或他处（切页/对话框）→ 置 needs_relayout：
        // render 重排后用结构签名判定，签名不变（定宽输入打字）走局部，变了升级整窗。
        if repaint {
            self.apply_damage(damage);
            self.damage.needs_relayout = true;
        }
        if !consumed {
            match ev.key {
                // 焦点导航。焦点环跨节点变化（低频）→ 整窗。
                Key::Tab => {
                    self.focus.visible = true;
                    if self.move_focus(!ev.shift) {
                        self.damage.needs_full = true;
                        repaint = true;
                    }
                }
                Key::Escape if self.resolve_close() => self.close = true,
                _ => {}
            }
        }
        repaint
    }

    fn wants_close(&self) -> bool {
        self.close
    }

    fn take_hotkey_ops(&mut self) -> Vec<(usize, crate::event::HotkeyOp)> {
        std::mem::take(&mut *self.hotkey_ops.borrow_mut())
    }

    fn on_close_request(&mut self) -> bool {
        self.resolve_close()
    }

    fn capture_active(&self) -> bool {
        self.capture.is_some()
    }

    fn set_scale(&mut self, scale: f32) {
        self.damage.needs_full = true;
        self.scale = scale;
        // 文字引擎同步 scale，保证文字测量/绘制与图形缩放一致。
        self.engine.set_scale(scale);
    }

    /// 把排队的开窗请求变成「配置 + 宿主」交给平台。
    ///
    /// 子窗共享**同一个** `ThemeHandle`：运行期换主题时所有窗口一起变，而不是各拿一份
    /// 快照各自定格。其余应用级设施（托盘/热键/单实例/跨线程通道）都不复制——它们属于
    /// 应用，已经在主窗那次 `run` 里装好了。
    /// 单例（`Window::single`）在这里判定，**早于内容构建**：已有同键窗口就只回报一个
    /// `Focus`，那棵控件树根本不建（`req.content` 的闭包不跑，它的副作用也不发生）。
    ///
    /// `batch` 记本批已决定新建的键：`is_open` 查的是平台登记表，而同一次分发里连着两个
    /// 同键请求时，第一个还没建出来——只信 `is_open` 会一次开出两个"单例"窗口。
    fn take_new_windows(&mut self, is_open: &dyn Fn(&str) -> bool) -> Vec<NewWindow> {
        let mut batch: std::collections::HashSet<String> = std::collections::HashSet::new();
        std::mem::take(&mut self.pending_windows)
            .into_iter()
            .map(|req| {
                if let Some(key) = req.single.clone() {
                    if is_open(&key) || !batch.insert(key.clone()) {
                        return NewWindow::Focus(key);
                    }
                }
                let bg_explicit = req.bg.is_some();
                let bg = req.bg.unwrap_or(self.theme.palette.bg);
                let cfg = WindowConfig {
                    title: req.title,
                    width: req.width,
                    height: req.height,
                    bg,
                    centered: req.centered,
                    resizable: req.resizable,
                    frameless: req.frameless,
                    min_width: req.min_width,
                    min_height: req.min_height,
                    single: req.single,
                    // 渲染后端由平台按主窗那次的选择填（子窗不该比主窗更慢或更快）。
                    // 其余字段（托盘/热键/截图/单实例）对子窗一律无意义，保持默认。
                    ..WindowConfig::default()
                };
                // 内容构建期创建的信号归这个窗口所有：`scope` 随宿主析构，窗口关掉
                // 就整批回收，反复开关子窗不会在全局 arena 里越积越多。
                let mut scope = crate::signal::SignalScope::new();
                let root: Element = scope.collect(|| req.content.take());
                let mut host = UiHost::new(
                    root,
                    self.theme_src.clone(),
                    bg,
                    !bg_explicit,
                    // 热键队列共享：子窗里的控件也能改绑全局热键（句柄本就可克隆传入）。
                    self.hotkey_ops.clone(),
                    // 通道不在这里传：它登记在**应用**上（`crate::sync` 的 App 级表），
                    // 全部窗口共用同一份 pump，由消费权持有者每帧排空一次。共用而非各持
                    // 一份副本，故同一条消息不会被处理两次；而主窗关掉后消费权自动交还，
                    // 由还活着的窗口接手——通道不再随某个窗口一起死。
                    // 定时器与关闭拦截器则是**这个窗口自己的**，随它一起生灭。
                    req.intervals,
                    req.close_handler,
                    // 唤起回调也不给子窗：子窗没有隐藏→唤起这条路（托盘与热键唤的是主窗），
                    // 给了也永远不触发。
                    None,
                    // `hide_on_close` 不给子窗：隐藏后没有唤起途径（托盘唤的是主窗），
                    // 只会留下一个关不掉也看不见的窗口。
                    false,
                );
                // `UiHost::new` 内部的 `root.build(&mut tree)` 也在作用域外——那里创建的是
                // 节点而非信号，控件自带的信号由控件自己的 `SignalScope` 管。
                host.scope = Some(scope);
                NewWindow::Create(Box::new(cfg), Box::new(host) as Box<dyn AppHandler>)
            })
            .collect()
    }

    /// 当前清屏色。
    ///
    /// **当场从主题源取，不读 `self.bg`**：那个字段在 `begin_frame` 里刷新，而平台是在
    /// `render` **之前**清屏的——读它就永远慢一帧，换主题后的那一帧仍按旧底色清屏，
    /// 而控件已经用新主题画了。停在这一帧就是"浅色文字画在浅色底上"。
    ///
    /// 经 `App::bg` 显式固定过底色时不跟随主题（与 `bg_follows_theme` 的既定语义一致）。
    fn bg(&self) -> Option<Color> {
        if self.bg_follows_theme {
            Some(self.theme_src.current().palette.bg)
        } else {
            Some(self.bg)
        }
    }

    fn wants_animation(&self) -> bool {
        // 帧末收割的快照，不是当下的全局位——理由见 `UiHost::wants_anim`。
        //
        // 帧**之外**发生的重绘请求（后台线程写信号、热键/托盘回调、定时器）不经这里：
        // 那些路径各自有 `InvalidateRect` 兜底把帧唤起来，本方法只回答"上一帧画完后
        // 还要不要继续按帧驱动"这一件事。
        self.wants_anim
    }

    fn next_frame_delay_ms(&self) -> u32 {
        self.next_delay
    }

    fn intervals(&self) -> Vec<std::time::Duration> {
        self.interval_durs.clone()
    }

    /// 第 `idx` 个定时器到点：借一个 App 级 `EventCtx` 跑对应回调（见 `apply_app_effects`）。
    ///
    /// `interval_cbs` 与 `tree` 是不相交字段，可同时借；副作用消费要整个 `&mut self`，
    /// 故排在 `cb` 的借用结束之后。
    fn on_interval_fired(&mut self, idx: usize) -> bool {
        let Some(root) = self.tree.root else {
            return false;
        };
        let Some(cb) = self.interval_cbs.get_mut(idx) else {
            return false;
        };
        let res = self.tree.run_detached(root, |ctx| cb(ctx));
        self.apply_app_effects(res);
        true
    }

    fn on_drop_files(&mut self, pos: Point, paths: Vec<std::path::PathBuf>) -> bool {
        self.damage.needs_full = true;
        // 物理 → 逻辑（命中在逻辑空间），路由到落点下的控件。
        let s = self.scale;
        let p = Point::new(
            (pos.x as f32 / s).round() as i32,
            (pos.y as f32 / s).round() as i32,
        );
        let res = self.tree.dispatch_files(p, paths);
        if res.close {
            self.apply_close_intent();
        }
        if let Some(url) = res.open_url {
            platform::open_url(&url);
        }
        if let Some(req) = res.toast {
            self.show_toast(req);
        }
        if res.dialog.is_some() {
            self.pending_dialog = res.dialog;
        }
        res.repaint
    }

    fn window_drag_at(&self, pos: Point) -> bool {
        // 菜单浮层激活时不拖窗。物理 → 逻辑后查拖动区。
        if self.menu.is_open() {
            return false;
        }
        let s = self.scale;
        let p = Point::new(
            (pos.x as f32 / s).round() as i32,
            (pos.y as f32 / s).round() as i32,
        );
        self.tree.drag_hit_at(p)
    }

    fn interactive_at(&self, pos: Point) -> bool {
        // 物理 → 逻辑后查是否命中可聚焦控件（窗口按钮等）。
        let s = self.scale;
        let p = Point::new(
            (pos.x as f32 / s).round() as i32,
            (pos.y as f32 / s).round() as i32,
        );
        // 菜单浮层激活时，面板范围内全部判为客户区，防止窗口缩放边框夺走滚动条事件。
        if self.menu.hit_any_panel(p) {
            return true;
        }
        // toast 浮层同理：面板范围内判为客户区。否则无边框窗口的自绘标题栏拖动区
        // 会把落在其上的 toast 点击（✕ 关闭 / 右键复制菜单）当 HTCAPTION 吞掉。
        if self.toast.hit_any_panel(p) {
            return true;
        }
        self.tree.interactive_hit_at(p)
    }

    fn take_window_op(&mut self) -> Option<WindowOp> {
        self.pending_window_op.take()
    }

    /// 控件经 `EventCtx` 请求的对话框优先；没有则取延迟闭包队列（已废弃的自由函数
    /// `defer_blocking` 的遗留入口）。
    fn take_dialog_request(&mut self) -> Option<DialogRequest> {
        self.pending_dialog.take().or_else(take_deferred)
    }

    fn cursor(&self) -> CursorShape {
        // 菜单浮层激活时用箭头（菜单项自管悬停高亮）。
        if self.menu.is_open() {
            return CursorShape::Arrow;
        }
        // 取当前悬停节点的形状；禁用节点统一回退箭头（禁用链接不显示手型）。
        match self.hover {
            Some(h) if self.tree.node_enabled(h) => self.tree.cursor_at(h),
            _ => CursorShape::Arrow,
        }
    }

    fn on_pan(&mut self, pos: Point, dy: i32) -> bool {
        self.pan(pos, dy)
    }

    fn start_fling(&mut self, pos: Point, vy: f32) -> bool {
        self.begin_fling(pos, vy)
    }

    fn cancel_fling(&mut self) -> bool {
        self.clear_fling()
    }

    fn ime_caret(&self) -> Option<(i32, i32, i32)> {
        let focus = self.focus.current?;
        let (p, h) = self.tree.caret_of(focus)?;
        // 逻辑坐标 → 物理像素（与渲染缩放一致）。
        let s = self.scale;
        Some((
            (p.x as f32 * s).round() as i32,
            (p.y as f32 * s).round() as i32,
            ((h as f32 * s).round() as i32).max(1),
        ))
    }

    fn set_ime_composing(&mut self, composing: bool) -> bool {
        let Some(focus) = self.focus.current else {
            return false;
        };
        self.tree.set_composing(focus, composing)
    }

    fn on_capture_lost(&mut self) -> bool {
        self.damage.needs_full = true;
        // 菜单滚动条拖拽不走 `self.capture`，得单独收尾（见 abort_scrollbar_drag）。
        // 必须放在下面的 `capture.is_none()` 早退之前，否则菜单打开时这条永远收不掉。
        let had_scrollbar_drag = self.menu.abort_scrollbar_drag();
        // 给捕获节点派发一个远处坐标的合成 Up，复用 Up 语义让其收尾
        // （Slider 复位拖动、Button 因 inside=false 不误触发），并清逻辑捕获。
        if self.capture.is_none() {
            return had_scrollbar_drag;
        }
        let ev = PointerEvent::single(
            PointerKind::Up,
            Point::new(-1_000_000, -1_000_000),
            MouseButton::Left,
        );
        let mut hover = self.hover;
        let mut capture = self.capture;
        let res = self.tree.dispatch_pointer(ev, &mut hover, &mut capture);
        self.hover = hover;
        self.capture = capture;
        res.repaint
    }
}

/// 各子模块测试共用的搭台助手。
#[cfg(test)]
mod test_support {
    use super::*;

    /// 三项下拉（初值选中第 1 项）+ 已暖过布局的宿主。
    pub(crate) fn dropdown_handler() -> (UiHost, crate::signal::Signal<usize>) {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let sel = crate::signal::signal(1usize);
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(Element::dropdown(vec!["甲", "乙", "丙"], sel).width(200)),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        (handler, sel)
    }

    pub(crate) fn key_ev() -> impl Fn(Key) -> crate::event::KeyEvent {
        |key| crate::event::KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_returns_sendable_sender() {
        crate::sync::reset_pumps();
        let mut app = App::new("t", 100, 100);
        let tx = app.channel::<u32>(|_ctx, _m| {});
        let h = std::thread::spawn(move || tx.send(5));
        assert!(h.join().unwrap().is_ok());
        // pump 现在登记在 App 级（`crate::sync`）而不是 `App` 自己身上。
        let pumps = crate::sync::take_pumps();
        assert_eq!(pumps.len(), 1);
        crate::sync::put_pumps(pumps);
        crate::sync::reset_pumps();
    }

    /// 消费权：无人持有时先来先得，持有者之外一律为假；持有者析构后交还，
    /// 下一个出帧的窗口就地接管——主窗关掉后子窗要能继续排空通道，靠的就是这条。
    #[test]
    fn channel_owner_is_claimed_once_and_released() {
        crate::sync::reset_pumps();
        let (a, b) = (crate::sync::next_host_id(), crate::sync::next_host_id());

        assert!(crate::sync::claim_channel_owner(a), "无人持有时应就地接管");
        assert!(crate::sync::claim_channel_owner(a), "持有者可反复认领");
        assert!(!crate::sync::claim_channel_owner(b), "非持有者不得排空");

        crate::sync::release_channel_owner(b);
        assert!(
            !crate::sync::claim_channel_owner(b),
            "非持有者的释放应无副作用"
        );

        crate::sync::release_channel_owner(a);
        assert!(
            crate::sync::claim_channel_owner(b),
            "持有者交还后由下一个接管"
        );
        crate::sync::reset_pumps();
    }

    #[test]
    fn on_interval_registers() {
        let app =
            App::new("t", 100, 100).on_interval(std::time::Duration::from_millis(100), |_ctx| {});
        assert_eq!(app.intervals.len(), 1);
    }

    /// `defer_blocking` 排入的闭包由 `take_dialog_request` 取走，且**取走时还没跑**——
    /// 它必须等到平台在事件分发完全返回后才 `run()`，这正是它存在的意义。
    ///
    /// 自由函数已废弃（改用 `ctx.defer_blocking`），但队列还得为老代码工作，故继续测。
    #[test]
    #[allow(deprecated)]
    fn deferred_closures_run_only_when_dialog_request_is_executed() {
        use std::cell::Cell as StdCell;
        let hits: Rc<StdCell<usize>> = Rc::new(StdCell::new(0));
        let (a, b) = (hits.clone(), hits.clone());
        defer_blocking(move || a.set(a.get() + 1));
        defer_blocking(move || b.set(b.get() + 10));

        let mut host = App::new("t", 100, 100)
            .content(Element::col())
            .into_handler_for_test();
        let req = host.take_dialog_request().expect("应取到延迟闭包请求");
        assert_eq!(hits.get(), 0, "取走时不该已经执行");
        req.run();
        assert_eq!(hits.get(), 11, "两个闭包应按排入顺序都跑到");
        assert!(
            host.take_dialog_request().is_none(),
            "队列已取空，不应重复交付"
        );
    }

    /// 默认（未开 hide_on_close）：关闭请求获准 → 真关，不留窗口操作。
    #[test]
    fn close_request_closes_by_default() {
        let app = App::new("t", 100, 100).content(Element::col());
        let mut app = app.into_handler_for_test();
        assert!(app.on_close_request(), "默认应允许关闭");
        assert_eq!(app.take_window_op(), None, "不该留下窗口操作");
    }

    /// hide_on_close：关闭请求被拒（不关窗），改留下 Hide 意图交平台层执行。
    #[test]
    fn hide_on_close_turns_close_into_hide() {
        let app = App::new("t", 100, 100)
            .hide_on_close()
            .content(Element::col());
        let mut app = app.into_handler_for_test();
        assert!(!app.on_close_request(), "hide_on_close 时不该关窗");
        assert_eq!(
            app.take_window_op(),
            Some(WindowOp::Hide),
            "须留下 Hide 意图——平台层靠它在借用释放后隐藏窗口"
        );
    }

    /// 拦截器优先于 hide_on_close：拦截器拒绝时，连 Hide 都不该发生。
    /// 这是文档承诺的「未保存提示与关闭即隐藏可并存」的前提。
    #[test]
    fn close_handler_takes_priority_over_hide_on_close() {
        let app = App::new("t", 100, 100)
            .hide_on_close()
            .on_close_request(|_ctx| false)
            .content(Element::col());
        let mut app = app.into_handler_for_test();
        assert!(!app.on_close_request());
        assert_eq!(
            app.take_window_op(),
            None,
            "拦截器拒绝时窗口应原样留着，既不关也不隐"
        );
    }

    /// 关闭拦截器拿得到真正的 `EventCtx`，且它请求的副作用走的是与控件回调同一条宿主
    /// 消费路径。这是"挡下这次关闭 + 弹确认框"这个流程的前提：`defer_blocking` 排出的
    /// 闭包必须**在取走前不执行**（原生模态框只能在事件分发完全返回后弹），拦截器同时
    /// 还能 toast 提示。ctx 的 `self_id` 是根节点——这些回调不属于任何控件。
    #[test]
    fn close_handler_gets_ctx_and_its_requests_reach_the_host() {
        use std::cell::Cell as StdCell;
        let ran: Rc<StdCell<bool>> = Rc::new(StdCell::new(false));
        let seen_id: Rc<RefCell<Option<NodeId>>> = Rc::new(RefCell::new(None));
        let (r, sid) = (ran.clone(), seen_id.clone());
        let app = App::new("t", 100, 100)
            .on_close_request(move |ctx| {
                *sid.borrow_mut() = Some(ctx.id());
                ctx.toast("有未保存的更改");
                let r = r.clone();
                ctx.defer_blocking(move || r.set(true));
                false
            })
            .content(Element::col());
        let mut app = app.into_handler_for_test();
        let root = app.tree.root;

        assert!(!app.on_close_request(), "拦截器返回 false 应挡下关闭");
        assert_eq!(*seen_id.borrow(), root, "App 级回调的 self_id 是根节点");
        assert_eq!(app.toast.items.len(), 1, "ctx.toast 应经宿主上屏");
        assert!(!ran.get(), "延迟闭包在取走前不得执行");
        app.take_dialog_request()
            .expect("ctx.defer_blocking 应产出对话框请求")
            .run();
        assert!(ran.get(), "平台执行请求后闭包才跑");
    }

    /// 定时器回调的副作用同样到达宿主：toast 上屏、`request_close` 变成宿主的关窗意图
    /// （平台在帧路径轮询 `wants_close`）。
    #[test]
    fn interval_callback_gets_ctx_and_its_requests_reach_the_host() {
        let seen_id: Rc<RefCell<Option<NodeId>>> = Rc::new(RefCell::new(None));
        let sid = seen_id.clone();
        let app = App::new("t", 100, 100)
            .on_interval(Duration::from_millis(10), move |ctx| {
                *sid.borrow_mut() = Some(ctx.id());
                ctx.toast("时间到");
                ctx.request_close();
            })
            .content(Element::col());
        let mut app = app.into_handler_for_test();
        let root = app.tree.root;

        assert!(app.on_interval_fired(0), "回调跑到即需重绘");
        assert_eq!(*seen_id.borrow(), root, "App 级回调的 self_id 是根节点");
        assert_eq!(app.toast.items.len(), 1, "ctx.toast 应经宿主上屏");
        assert!(app.wants_close(), "ctx.request_close 应落成宿主关窗意图");
        assert!(!app.on_interval_fired(9), "越界下标不该被当成跑过");
    }

    /// 通道消息的处理器拿得到 ctx，且**每条消息各一份副作用**：`DispatchResult` 的
    /// toast 是 `Option`，一批消息共用一份就只剩最后一条，"三个任务完成弹三条提示"
    /// 会静默丢两条。排空发生在 render 起始，故用真实的一帧来验。
    #[test]
    fn channel_messages_get_ctx_and_each_ones_toast_reaches_the_host() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let seen_id: Rc<RefCell<Option<NodeId>>> = Rc::new(RefCell::new(None));
        let sid = seen_id.clone();
        let mut app = App::new("t", 50, 50);
        let tx = app.channel::<u32>(move |ctx, n| {
            *sid.borrow_mut() = Some(ctx.id());
            ctx.toast(format!("第 {n} 项完成"));
        });
        let app = app.content(Element::col());
        tx.send(1).unwrap();
        tx.send(2).unwrap();

        let mut handler = app.into_handler_for_test();
        let root = handler.tree.root;
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(50, 50).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(50, 50));

        assert_eq!(*seen_id.borrow(), root, "App 级回调的 self_id 是根节点");
        let texts: Vec<String> = handler
            .toast
            .items
            .iter()
            .map(|t| t.req.text.clone())
            .collect();
        assert_eq!(
            texts,
            vec!["第 1 项完成".to_string(), "第 2 项完成".to_string()],
            "两条消息各自的 toast 都该上屏，不能互相覆盖"
        );
    }

    /// 无边框窗口的自绘 × 必须与系统 ×、Alt+F4 走**同一条**决策链。
    ///
    /// 回归的是一个真实故障：自绘 × 当初走 `res.close` 直落，于是 `on_close_request`
    /// 拦得住 Alt+F4 却拦不住 ×——而无边框窗口的 × 恰恰是用户最常点的那个，守卫因此
    /// 形同虚设（下游那个"改了内容点 × 直接丢失、按 Alt+F4 才提示"的报告即由此而来）。
    #[test]
    fn frameless_close_button_goes_through_the_close_guard() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use crate::ui::WindowButtonKind;
        use std::cell::Cell;
        use tiny_skia::Pixmap;

        let asked = std::rc::Rc::new(Cell::new(0u32));
        let a = asked.clone();
        let app = App::new("t", 200, 100)
            // 一律拦下：模拟"有未保存的更改，先问一句"。
            .on_close_request(move |_ctx| {
                a.set(a.get() + 1);
                false
            })
            .content(
                Element::col()
                    .fill()
                    .child(Element::window_button(WindowButtonKind::Close)),
            );
        let mut handler = app.into_handler_for_test();
        let mut pm = Pixmap::new(200, 100).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 100));

        let at = Point::new(20, 20);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));

        assert_eq!(asked.get(), 1, "自绘 × 必须问过 on_close_request");
        assert!(!handler.wants_close(), "守卫拒绝时不该关窗");
    }

    /// `force_close` 是给"应用已决定"的场合（安装器要求退出、用户已在确认框里选过），
    /// 它**跳过**守卫——否则会变成"安装器等窗口关、窗口等用户回答"。
    #[test]
    fn force_close_skips_the_guard() {
        use std::cell::Cell;
        let asked = std::rc::Rc::new(Cell::new(0u32));
        let a = asked.clone();
        let app = App::new("t", 100, 100)
            .on_close_request(move |_ctx| {
                a.set(a.get() + 1);
                false
            })
            .content(Element::col());
        let mut app = app.into_handler_for_test();
        app.apply_close_intent(); // force_close 的落地路径
        assert_eq!(asked.get(), 0, "force_close 不该问守卫");
        assert!(app.wants_close());
    }

    /// 已决定的关闭（`force_close`）也须受 hide_on_close 约束。
    #[test]
    fn widget_request_close_respects_hide_on_close() {
        let app = App::new("t", 100, 100)
            .hide_on_close()
            .content(Element::col());
        let mut app = app.into_handler_for_test();
        app.apply_close_intent();
        assert!(
            !app.wants_close(),
            "hide_on_close 时控件请求关闭不该退出进程"
        );
        assert_eq!(app.take_window_op(), Some(WindowOp::Hide));
    }

    /// 未开 hide_on_close 时，已决定的关闭仍须真关——不可回归。
    #[test]
    fn widget_request_close_still_closes_by_default() {
        let app = App::new("t", 100, 100).content(Element::col());
        let mut app = app.into_handler_for_test();
        app.apply_close_intent();
        assert!(app.wants_close());
        assert_eq!(app.take_window_op(), None);
    }

    /// 回归（多窗口地基）：续帧请求必须归各宿主自己所有。
    ///
    /// `anim` 的请求位是**线程全局**的，而每帧起始都会 `reset_request()` 清一次。同线程
    /// 跑两个窗口时，后绘制的那个会把前一个刚提出的续帧请求一并抹掉；若 `wants_animation`
    /// 直接读全局位，前者的动画就在对方出帧的瞬间冻住。故各宿主在帧末把它收进自己的字段。
    /// 运行期换主题后，平台**下一次清屏**就要拿到新底色——不能等一帧。
    ///
    /// 平台是在 `render` **之前**清屏的，而 `UiHost::bg` 字段要到 `render` 里的
    /// `begin_frame` 才刷新。若 `AppHandler::bg` 读那个字段就永远慢一帧：换主题后的
    /// 那一帧按旧底色清屏、控件却已用新主题画好，停在这里就是"浅色文字画在浅色底上"。
    /// 故本测试**刻意不渲染**，直接查——那正是平台清屏时所处的时刻。
    #[test]
    fn bg_follows_theme_without_rendering_a_frame() {
        use crate::platform::AppHandler;
        let mut app = App::new("t", 100, 100);
        let theme = app.theme_handle();
        let h = app.content(Element::col().fill()).into_handler_for_test();

        let light_bg = h.bg().expect("宿主应报告清屏色");
        assert_eq!(light_bg, Theme::default().palette.bg);

        theme.set(Theme::dark());
        let dark_bg = h.bg().expect("宿主应报告清屏色");
        assert_ne!(light_bg, dark_bg, "换主题后底色必须立刻变，不能等下一帧");
        assert_eq!(dark_bg, Theme::dark().palette.bg);
    }

    /// 换主题要让**所有**窗口刷新，而不只是触发换肤的那个。
    ///
    /// 主题源为所有窗口共享，但 `anim::request_repaint` 只唤起当前窗口。此前换肤能联动
    /// 纯属巧合——示例用一个 `Signal<bool>` 记明暗，是那次**信号写入**顺带触发了跨窗
    /// 广播；只调 `theme.set()` 的写法则不会。
    #[test]
    fn theme_switch_requests_cross_window_refresh() {
        let mut app = App::new("t", 100, 100);
        let theme = app.theme_handle();
        let _h = app.content(Element::col().fill()).into_handler_for_test();
        // 清掉建树期间可能积累的标记，隔离本次断言。
        let _ = crate::signal::take_cross_window_dirty();

        theme.set(Theme::dark());
        assert!(
            crate::signal::take_cross_window_dirty(),
            "换主题必须请求跨窗刷新，否则其他窗口停在旧配色"
        );
    }

    /// 经 `App::bg` 显式固定的底色不跟随主题（既定语义，勿因上一条回归而改掉）。
    ///
    /// 与既有的 `explicit_bg_stays_fixed_across_theme_switch` 是两件事：那条查的是
    /// `handler.bg` **字段**（局部重绘子缓冲的填底色）、且在渲染之后；这条查的是平台
    /// 真正用来清屏的 `AppHandler::bg()` **方法**、且不渲染。
    #[test]
    fn explicit_bg_reported_to_platform_ignores_theme() {
        use crate::platform::AppHandler;
        let fixed = Color::hex(0x123456);
        let mut app = App::new("t", 100, 100).bg(fixed);
        let theme = app.theme_handle();
        let h = app.content(Element::col().fill()).into_handler_for_test();

        assert_eq!(h.bg(), Some(fixed));
        theme.set(Theme::dark());
        assert_eq!(h.bg(), Some(fixed), "显式指定的底色不该被换肤覆盖");
    }

    /// 测试辅助：把 `NewWindow` 解成 `(配置, 宿主)`。
    ///
    /// 遇到 `Focus` 直接 panic——用它的测试都断言"这批请求都该建窗"，静默跳过会让
    /// "单例误判把窗口吃掉了"表现成 `len` 对不上，而不是指着问题说话。
    fn expect_create(w: NewWindow) -> (WindowConfig, Box<dyn AppHandler>) {
        match w {
            NewWindow::Create(cfg, host) => (*cfg, host),
            NewWindow::Focus(key) => panic!("期望建窗，实际被判为激活已有单例窗口: {key}"),
        }
    }

    /// `ctx.open_window` 的请求要一路走到平台能消费的形状：配置照搬、内容还原成宿主。
    ///
    /// 建窗本身要真窗口才测得了（见提交说明里的外部 WM_CLOSE 冒烟），但**意图有没有
    /// 正确传出去**是纯逻辑，可以在这里钉住——这段链路跨了 core/app/platform 三层，
    /// 中间任何一环漏接，症状都是"点了没反应"。
    #[test]
    fn open_window_request_reaches_platform_as_config_and_host() {
        use crate::platform::AppHandler;
        let mut h = App::new("main", 200, 200)
            .content(
                Element::col()
                    .fill()
                    .child(Element::leaf().width(50).height(50)),
            )
            .into_handler_for_test();
        // 没人开窗时不该凭空冒出窗口。
        assert!(
            h.take_new_windows(&|_| false).is_empty(),
            "未请求时不应有待建窗口"
        );

        // 模拟控件回调里调 ctx.open_window。
        let root = h.tree.root.unwrap();
        let res = h.tree.run_detached(root, |ctx| {
            ctx.open_window(
                Window::new("设置", 420, 300)
                    .resizable(false)
                    .min_size(320, 240)
                    .content(|| Element::col().fill()),
            );
            ctx.open_window(Window::new("关于", 360, 200).content(|| Element::col().fill()));
        });
        h.apply_app_effects(res);

        let made: Vec<_> = h
            .take_new_windows(&|_| false)
            .into_iter()
            .map(expect_create)
            .collect();
        assert_eq!(made.len(), 2, "一次回调里连开两个窗都要送到");
        let (cfg_a, _) = &made[0];
        assert_eq!(cfg_a.title, "设置", "顺序须与调用顺序一致");
        assert_eq!((cfg_a.width, cfg_a.height), (420, 300));
        assert!(!cfg_a.resizable);
        assert_eq!((cfg_a.min_width, cfg_a.min_height), (320, 240));
        // 应用级设施不得随子窗复制：托盘会变成两个图标、热键会重复注册。
        assert!(cfg_a.tray.is_none(), "子窗不得带托盘");
        assert!(cfg_a.hotkeys.is_empty(), "子窗不得带全局热键");
        assert_eq!(made[1].0.title, "关于");

        // 取走即清空，下一轮不会重复建窗。
        assert!(
            h.take_new_windows(&|_| false).is_empty(),
            "请求应在取走时清空"
        );
    }

    /// 单例窗口（`Window::single`）：平台报"已开着"时，请求变成 `Focus` 且**内容不构建**。
    ///
    /// 内容不构建是这条路径的关键——判定若晚于构建，每点一次都要白搭一整棵控件树，
    /// 闭包里的副作用（这里用计数器代表）也会跟着重复执行。
    #[test]
    fn single_window_already_open_becomes_focus_without_building_content() {
        use crate::platform::AppHandler;
        use std::cell::Cell;
        use std::rc::Rc;
        let mut h = App::new("main", 200, 200)
            .content(Element::col().fill())
            .into_handler_for_test();

        let builds = Rc::new(Cell::new(0u32));
        let b = builds.clone();
        let root = h.tree.root.unwrap();
        let res =
            h.tree.run_detached(root, move |ctx| {
                ctx.open_window(Window::new("设置", 200, 150).single("settings").content(
                    move || {
                        b.set(b.get() + 1);
                        Element::col().fill()
                    },
                ))
            });
        h.apply_app_effects(res);

        // 平台报"settings 已经开着"。
        let made = h.take_new_windows(&|key| key == "settings");
        assert_eq!(made.len(), 1, "请求仍要送到平台——它得知道去激活哪个窗口");
        match &made[0] {
            NewWindow::Focus(key) => assert_eq!(key, "settings"),
            NewWindow::Create(..) => panic!("已有同键窗口时不该再建一个"),
        }
        assert_eq!(builds.get(), 0, "内容闭包不该被运行——单例判定必须早于构建");
    }

    /// 同一次分发里连开两个同键窗口：第一个建、第二个只激活。
    ///
    /// 只信平台的 `is_open` 会在这里失手——第一个窗口尚未建出来，登记表里还没有它，
    /// 于是一次回调开出两个"单例"窗口。
    #[test]
    fn two_same_key_requests_in_one_batch_create_only_one() {
        use crate::platform::AppHandler;
        let mut h = App::new("main", 200, 200)
            .content(Element::col().fill())
            .into_handler_for_test();

        let root = h.tree.root.unwrap();
        let res = h.tree.run_detached(root, |ctx| {
            for _ in 0..3 {
                ctx.open_window(
                    Window::new("设置", 200, 150)
                        .single("settings")
                        .content(|| Element::col().fill()),
                );
            }
        });
        h.apply_app_effects(res);

        let made = h.take_new_windows(&|_| false);
        assert_eq!(made.len(), 3, "三个请求都要送到，只是意图不同");
        assert!(matches!(made[0], NewWindow::Create(..)), "第一个该建窗");
        assert!(
            matches!(made[1], NewWindow::Focus(_)) && matches!(made[2], NewWindow::Focus(_)),
            "同批后续同键请求只该激活，不该再建"
        );
    }

    /// 无键窗口永不去重：点几次开几个，同批也是。
    #[test]
    fn keyless_windows_are_never_deduped() {
        use crate::platform::AppHandler;
        let mut h = App::new("main", 200, 200)
            .content(Element::col().fill())
            .into_handler_for_test();

        let root = h.tree.root.unwrap();
        let res = h.tree.run_detached(root, |ctx| {
            for _ in 0..3 {
                ctx.open_window(Window::new("便签", 200, 150).content(|| Element::col().fill()));
            }
        });
        h.apply_app_effects(res);

        let made = h.take_new_windows(&|_| true); // 即便平台对任何键都说"开着"
        assert_eq!(made.len(), 3);
        assert!(
            made.iter().all(|w| matches!(w, NewWindow::Create(..))),
            "没设 single 的窗口不参与去重"
        );
    }

    /// 不同单例键互不干扰：「设置」开着不该把「关于」也挡下。
    #[test]
    fn different_single_keys_do_not_block_each_other() {
        use crate::platform::AppHandler;
        let mut h = App::new("main", 200, 200)
            .content(Element::col().fill())
            .into_handler_for_test();

        let root = h.tree.root.unwrap();
        let res = h.tree.run_detached(root, |ctx| {
            ctx.open_window(
                Window::new("关于", 200, 150)
                    .single("about")
                    .content(|| Element::col().fill()),
            )
        });
        h.apply_app_effects(res);

        let made = h.take_new_windows(&|key| key == "settings");
        assert_eq!(made.len(), 1);
        assert!(
            matches!(made[0], NewWindow::Create(..)),
            "只有 settings 开着，about 该照常建出来"
        );
    }

    /// 子窗自己的关闭拦截器与定时器要真的挂到子窗宿主上。
    ///
    /// 这两样刻意**不**从主窗继承（主窗那份绑的是主窗的回调），所以只能由 `Window` 带过来；
    /// 中间漏接的症状是"设了回调没反应"，编译期抓不到。
    #[test]
    fn child_window_carries_its_own_close_handler_and_intervals() {
        use crate::platform::AppHandler;
        use std::time::Duration;
        let mut h = App::new("main", 200, 200)
            .content(Element::col().fill())
            .into_handler_for_test();

        let allow = crate::signal::signal(false);
        let ticks = crate::signal::signal(0i32);
        let root = h.tree.root.unwrap();
        let res = h.tree.run_detached(root, |ctx| {
            ctx.open_window(
                Window::new("子窗", 200, 150)
                    .on_close_request(move |_| allow.get())
                    .on_interval(Duration::from_millis(250), move |_| {
                        ticks.set(ticks.get() + 1)
                    })
                    .content(|| Element::col().fill()),
            )
        });
        h.apply_app_effects(res);

        let mut made: Vec<_> = h
            .take_new_windows(&|_| false)
            .into_iter()
            .map(expect_create)
            .collect();
        assert_eq!(made.len(), 1);
        let (_, child) = &mut made[0];

        // 定时器：平台按这个列表 SetTimer，故必须原样送到。
        assert_eq!(
            child.intervals(),
            vec![Duration::from_millis(250)],
            "子窗的 on_interval 应挂到子窗宿主上"
        );
        // 触发第 0 个定时器，回调应真的跑。
        assert!(child.on_interval_fired(0), "定时器回调应执行并请求重绘");
        assert_eq!(ticks.get(), 1);

        // 关闭拦截器：拦下时不放行。
        assert!(!child.on_close_request(), "拦截器返回 false 时不该放行关闭");
        allow.set(true);
        assert!(child.on_close_request(), "拦截器返回 true 时应放行");

        // 主窗不受子窗那份影响（各是各的）。
        assert!(h.on_close_request(), "主窗未设拦截器，应默认放行");
        assert!(h.intervals().is_empty(), "主窗没注册定时器");
    }

    /// 子窗内容构建期创建的信号，随窗口关闭整批回收。
    ///
    /// 没有这条，反复开关设置窗会让那些信号在全局 arena 里一直累积——单窗口时代无所谓
    /// （内容活到进程结束），多窗口下才成为真的泄漏。
    #[test]
    fn child_window_signals_are_reclaimed_on_close() {
        use crate::platform::AppHandler;
        let mut h = App::new("main", 200, 200)
            .content(Element::col().fill())
            .into_handler_for_test();
        let before = crate::signal::stats().live;

        let root = h.tree.root.unwrap();
        let res = h.tree.run_detached(root, |ctx| {
            ctx.open_window(Window::new("子窗", 200, 150).content(|| {
                // 典型写法：子窗自己的局部状态，在内容构建期创建。
                let a = crate::signal::signal(0i32);
                let b = crate::signal::signal(String::new());
                Element::col()
                    .fill()
                    .child(Element::label_signal(b))
                    .child(Element::label(format!("{}", a.get())))
            }))
        });
        h.apply_app_effects(res);

        let made = h.take_new_windows(&|_| false);
        assert_eq!(made.len(), 1);
        let during = crate::signal::stats().live;
        assert!(
            during >= before + 2,
            "子窗内容里创建的信号应当存在（before={before} during={during}）"
        );

        // 关掉子窗 = 丢掉它的宿主。
        drop(made);
        let after = crate::signal::stats().live;
        assert_eq!(
            after, before,
            "子窗关闭后其构建期信号应整批回收（before={before} after={after}）"
        );
    }

    /// 子窗与主窗共享同一个 `ThemeHandle`：换肤时所有窗口一起变，而不是各自定格。
    #[test]
    fn child_window_shares_theme_handle() {
        use crate::platform::AppHandler;
        let mut app = App::new("main", 200, 200);
        let theme = app.theme_handle();
        let mut h = app.content(Element::col().fill()).into_handler_for_test();
        let root = h.tree.root.unwrap();
        let res = h.tree.run_detached(root, |ctx| {
            ctx.open_window(Window::new("子窗", 200, 150).content(|| Element::col().fill()))
        });
        h.apply_app_effects(res);
        let made = h.take_new_windows(&|_| false);
        assert_eq!(made.len(), 1);

        // 换肤后，子窗宿主下一帧读到的必须是新主题。用底色区分两套主题
        // （强调色在明暗两套里是同一个值，区分不出来）。
        let before = theme.current().palette.bg;
        theme.set(Theme::dark());
        let after = theme.current().palette.bg;
        assert_ne!(before, after, "前置条件：两套主题的底色不同");
        // 子窗宿主持的是句柄而非快照，故 current() 与主窗一致。
        assert_eq!(
            h.theme_src.current().palette.bg,
            after,
            "子窗与主窗必须看到同一个主题源"
        );
    }

    #[test]
    fn animation_request_does_not_leak_between_hosts() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        // A：不确定进度条，每帧都请求下一帧。
        let mut a = App::new("a", 100, 40)
            .content(
                Element::col()
                    .fill()
                    .child(Element::progress_indeterminate().width_match()),
            )
            .into_handler_for_test();
        // B：纯静态内容，永不请求动画。
        let mut b = App::new("b", 100, 40)
            .content(
                Element::col()
                    .fill()
                    .child(Element::leaf().width(40).height(20)),
            )
            .into_handler_for_test();
        a.set_scale(1.0);
        b.set_scale(1.0);
        let mut pm = Pixmap::new(100, 40).unwrap();
        a.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(100, 40));
        assert!(a.wants_animation(), "不确定进度条应请求续帧");
        // B 出一帧：其帧首的 reset_request 会清掉 A 留在全局位上的请求。
        b.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(100, 40));
        assert!(!b.wants_animation(), "静态内容不应请求续帧");
        assert!(a.wants_animation(), "A 的续帧请求不得被 B 的帧清掉");
    }

    /// 局部帧只更新脏区那几行，**绝不碰框外像素**——平台正是靠这一点把 R/B 交换与
    /// 上传收窄到那几行（`AppHandler::last_frame_damage`）。若这里整窗拷贝回来，
    /// 平台的窄上传就会把没交换过的行当成交换过的送上屏，颜色红蓝颠倒。
    #[test]
    fn partial_frame_leaves_pixels_outside_damage_untouched() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let (w, h) = (120, 200);
        let mut a = App::new("a", w, h)
            .content(
                Element::col()
                    .fill()
                    .child(Element::progress_indeterminate().width_match().height(8)),
            )
            .into_handler_for_test();
        a.set_scale(1.0);
        let mut pm = Pixmap::new(w as u32, h as u32).unwrap();
        a.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(w, h));

        // 在远离进度条的底部埋一个标记色。
        let mark = [0xFFu8, 0x00, 0xFF, 0xFF];
        let off = ((h - 4) * w + 8) as usize * 4;
        pm.data_mut()[off..off + 4].copy_from_slice(&mark);

        a.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(w, h));
        assert!(!a.damage.last_frame_full, "第二帧应走局部路径");
        assert_eq!(
            &pm.data()[off..off + 4],
            &mark,
            "脏区之外的像素不该被局部帧覆盖"
        );
        let dmg = a.last_frame_damage().expect("局部帧应报告更新范围");
        assert!(
            dmg.bottom() < h - 4,
            "进度条的脏区不应延伸到底部标记处：{dmg:?}"
        );
    }

    /// 窗口激活态：失活后聚焦的输入框不再请求续帧（后台窗口不该按刷新率出帧），
    /// 重新激活后恢复。多窗口下各宿主各记各的，不能靠线程全局位。
    #[test]
    fn inactive_window_stops_asking_for_frames() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let text = crate::signal::signal(String::from("abc"));
        let mut a = App::new("a", 120, 60)
            .content(
                Element::col()
                    .fill()
                    .child(Element::text_input(text, "").width_match().autofocus()),
            )
            .into_handler_for_test();
        a.set_scale(1.0);
        let mut pm = Pixmap::new(120, 60).unwrap();
        macro_rules! frame {
            () => {
                a.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(120, 60))
            };
        }
        frame!();
        assert!(a.wants_animation(), "激活窗口里的光标应请求续帧");

        assert!(a.on_window_activated(false), "激活态变化应要求重绘");
        assert!(!a.on_window_activated(false), "同值重复通知不应再要求重绘");
        frame!();
        assert!(!a.wants_animation(), "失活后不应再请求续帧");

        assert!(a.on_window_activated(true));
        frame!();
        assert!(a.wants_animation(), "重新激活应恢复续帧");
    }

    /// 帧截止：闪烁光标只需要"半周期之后"的那一帧，不该被按刷新率唤醒。
    ///
    /// 这是把方波光标的续帧开销压到接近静态的关键。判据故意宽松（只验数量级），
    /// 精确的曲线对拍在 `ui::caret` 那边；这里验的是"控件报的截止确实传到了宿主"。
    #[test]
    fn blinking_caret_reports_a_far_frame_deadline() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use crate::ui::caret::{BLINK_HALF_MS, TYPING_PAUSE_MS};
        use tiny_skia::Pixmap;
        crate::ui::caret::set_animated(true);
        crate::ui::caret::set_window_active(true);
        crate::ui::caret::set_blink_period_ms(Some(BLINK_HALF_MS as u32));

        let text = crate::signal::signal(String::from("abc"));
        let mut a = App::new("a", 120, 60)
            .content(
                Element::col()
                    .fill()
                    .child(Element::text_input(text, "").width_match().autofocus()),
            )
            .into_handler_for_test();
        a.set_scale(1.0);
        let mut pm = Pixmap::new(120, 60).unwrap();
        a.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(120, 60));
        assert!(a.wants_animation(), "闪烁光标应请求续帧");
        let d = a.next_frame_delay_ms();
        // 刚出现的光标处在"打字暂停"里：先睡到暂停结束，再睡满一个亮半周期。
        let ceiling = (TYPING_PAUSE_MS + BLINK_HALF_MS + 1) as u32;
        assert!(
            (100..=ceiling).contains(&d),
            "闪烁光标不该要求按刷新率出帧（应在 100..={ceiling}ms），实报 {d}ms"
        );

        // 连续动画（不确定进度条）必须报 0：它每帧都在变，推迟唤醒就是掉帧。
        let mut b = App::new("b", 120, 60)
            .content(
                Element::col()
                    .fill()
                    .child(Element::progress_indeterminate().width_match().height(8)),
            )
            .into_handler_for_test();
        b.set_scale(1.0);
        b.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(120, 60));
        assert!(b.wants_animation());
        assert_eq!(b.next_frame_delay_ms(), 0, "连续动画应要求下一帧就出");

        // 同窗口里两者并存 → 取较早的那个（0）。一个控件不能把整窗睡过去。
        let text2 = crate::signal::signal(String::from("abc"));
        let mut c = App::new("c", 120, 90)
            .content(
                Element::col()
                    .fill()
                    .child(Element::text_input(text2, "").width_match().autofocus())
                    .child(Element::progress_indeterminate().width_match().height(8)),
            )
            .into_handler_for_test();
        c.set_scale(1.0);
        let mut pm2 = Pixmap::new(120, 90).unwrap();
        c.render(&mut PixmapTarget { pixmap: &mut pm2 }, Size::new(120, 90));
        assert_eq!(
            c.next_frame_delay_ms(),
            0,
            "有连续动画在场时整窗必须回到满帧配速"
        );

        // 各宿主各记各的：`anim` 的截止是线程全局量，帧首会被清掉——与 `wants_anim`
        // 同一个坑（见 `UiHost::wants_anim`），漏收就是"别的窗口把我的截止改了"。
        assert!(
            a.next_frame_delay_ms() >= 100,
            "A 的截止不得被 B/C 的帧清成 0"
        );
    }

    /// 回归（多窗口地基）：对话框栈必须归各自的树所有。
    ///
    /// 曾是线程全局栈，按 `Element::dialog` 的构造顺序累积。两个窗口下后构建的那个占住
    /// 栈顶，于是在 B 窗口按 ESC / 点关闭，关掉的是 A 窗口的对话框。
    #[test]
    fn close_request_only_touches_this_windows_dialog() {
        use crate::platform::AppHandler;
        let show_b = crate::signal::signal(true);
        let show_a = crate::signal::signal(true);
        // 刻意先建 B、后建 A：全局栈下栈顶将是 A 的对话框。
        let mut b = App::new("b", 100, 100)
            .content(Element::stack().fill().child(Element::dialog(
                show_b,
                Element::leaf().width(20).height(20),
            )))
            .into_handler_for_test();
        let a = App::new("a", 100, 100)
            .content(Element::stack().fill().child(Element::dialog(
                show_a,
                Element::leaf().width(20).height(20),
            )))
            .into_handler_for_test();
        assert!(!b.on_close_request(), "有对话框时关闭请求应被拦下，不关窗");
        assert!(!show_b.get(), "应关掉 B 自己的对话框");
        assert!(show_a.get(), "A 窗口的对话框不得被 B 的关闭请求波及");
        drop(a); // 持到断言之后：树活着，其上的信号才活着
    }

    /// 嵌套对话框：ESC 先关最内层的那个。
    ///
    /// 依赖 `build` 的先序遍历（父先 insert、再递归子），故栈顶恒为最内层。此前按
    /// `Element::dialog` 的构造顺序登记，而内层作为外层的参数**先**求值，栈序正好是反的。
    #[test]
    fn close_request_closes_innermost_dialog_first() {
        use crate::platform::AppHandler;
        let outer = crate::signal::signal(true);
        let inner = crate::signal::signal(true);
        let mut h = App::new("t", 100, 100)
            .content(
                Element::stack().fill().child(Element::dialog(
                    outer,
                    Element::col()
                        .width(60)
                        .height(60)
                        .child(Element::dialog(inner, Element::leaf().width(20).height(20))),
                )),
            )
            .into_handler_for_test();
        assert!(!h.on_close_request());
        assert!(!inner.get(), "第一次关闭请求应关掉最内层对话框");
        assert!(outer.get(), "外层对话框应还开着");
        assert!(!h.on_close_request());
        assert!(!outer.get(), "第二次才轮到外层");
    }

    #[test]
    fn modal_open_clears_stale_hover() {
        // 回归：点可点击行弹出模态后，光标静止，旧 hover 节点被新遮罩遮住须收到 Leave，
        // 否则其 hover 高亮残留（结构变化触发 resync_hover_after_relayout 修正）。
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let show = crate::signal::signal(false);
        let s2 = show;
        let ui = Element::stack()
            .fill()
            .child(
                Element::row()
                    .clickable()
                    .on_click(move |_| s2.set(true))
                    .width_match()
                    .height(60),
            )
            .child(Element::dialog(show, Element::leaf().width(40).height(40)));
        let app = App::new("t", 100, 100).content(ui);
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(100, 100).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(100, 100));
        // 悬停到可点击行。
        handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            Point::new(30, 30),
            MouseButton::Left,
        ));
        let row_hover = handler.hover;
        assert!(row_hover.is_some(), "应 hover 到可点击行");
        // 点击打开模态（光标不再移动）。
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            Point::new(30, 30),
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Up,
            Point::new(30, 30),
            MouseButton::Left,
        ));
        assert!(show.get(), "点击应打开模态");
        // 渲染：结构变化 → resync_hover 在原位置重新命中，旧 hover（被遮罩盖住）应被替换。
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(100, 100));
        assert_ne!(
            handler.hover, row_hover,
            "模态弹出后旧 hover 应被清掉，避免高亮残留"
        );
    }

    #[test]
    fn nested_modal_over_cell_clears_hover() {
        // 镜像 settings：单元格在 scroll 在对话框A（已开）内，点单元格开对话框B（在其上）。
        // 验证 B 弹出后该单元格（被 B 遮住）的 hover 被清。
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let show_a = crate::signal::signal(true);
        let show_b = crate::signal::signal(false);
        let sb = show_b;
        let cell = Element::stack()
            .clickable()
            .on_click(move |_| sb.set(true))
            .width(100)
            .height(40);
        let dialog_a =
            Element::dialog(show_a, Element::scroll().width(200).height(200).child(cell));
        let dialog_b = Element::dialog(show_b, Element::leaf().width(80).height(60));
        let ui = Element::stack()
            .fill()
            .child(Element::col().fill())
            .child(dialog_a)
            .child(dialog_b);
        let app = App::new("t", 300, 300).content(ui);
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 300).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 300));
        // 对话框A居中(scroll 200x200@(50,50))，cell 在 scroll 顶部(50,50,100,40)→中心(100,70)。
        handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            Point::new(100, 70),
            MouseButton::Left,
        ));
        let cell_hover = handler.hover;
        assert!(
            cell_hover.is_some(),
            "应 hover 到单元格，实得 {cell_hover:?}"
        );
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            Point::new(100, 70),
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Up,
            Point::new(100, 70),
            MouseButton::Left,
        ));
        assert!(show_b.get(), "点单元格应打开对话框B");
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 300));
        assert_ne!(
            handler.hover, cell_hover,
            "对话框B弹出后，被遮住的单元格 hover 应被清掉"
        );
    }

    #[test]
    fn hiding_node_resets_its_interaction_state() {
        // 回归：控件在按下/悬停态被隐藏（如关闭其所在对话框）时，框架应调 reset_interaction
        // 重置其交互态，避免下次显示瞬间闪出旧的按下/悬停态。
        use crate::core::Widget;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use std::cell::Cell as StdCell;
        use std::rc::Rc;
        use tiny_skia::Pixmap;
        struct ResetProbe(Rc<StdCell<u32>>);
        impl Widget for ResetProbe {
            fn reset_interaction(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let hits = Rc::new(StdCell::new(0u32));
        let show = crate::signal::signal(true);
        let probe = hits.clone();
        // 关键：探针**嵌在对话框内部**（自身无 vis_cond），对话框隐藏时探针的局部
        // effective_visible 不变——只有祖先链累积可见性才能检测到它被隐藏。
        let ui = Element::stack().fill().child(Element::dialog(
            show,
            Element::leaf()
                .width(20)
                .height(20)
                .widget(ResetProbe(probe)),
        ));
        let app = App::new("t", 40, 40).content(ui);
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(40, 40).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(40, 40));
        // 隐藏：模拟交互后置 needs_relayout（正常由事件置位），渲染触发结构变化处理。
        show.set(false);
        handler.damage.needs_relayout = true;
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(40, 40));
        assert!(
            hits.get() >= 1,
            "节点隐藏时应调用 reset_interaction 重置交互态"
        );
    }

    #[test]
    fn theme_handle_hot_swaps_into_host() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let mut app = App::new("t", 60, 60).theme(crate::theme::Theme::default());
        let handle = app.theme_handle();
        app = app.content(Element::col());
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(60, 60).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(60, 60));
        let lum = |c: Color| c.r as u32 + c.g as u32 + c.b as u32;
        assert!(lum(handler.theme.palette.bg) > 500, "初始亮色背景");
        // 句柄热切换为暗色 → 下一帧 render 后 host 主题快照应转暗。
        handle.set(crate::theme::Theme::dark());
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(60, 60));
        assert!(
            lum(handler.theme.palette.bg) < 300,
            "热切换后 host 应共享句柄的暗色主题"
        );
        // 清屏色（局部重绘子缓冲的填底色）也应随主题转暗——
        // 否则暗色主题下局部重绘区域会闪出亮色底。
        assert!(
            lum(handler.bg) < 300,
            "未经 App::bg 显式固定时，清屏色应随主题热切换"
        );
    }

    #[test]
    fn explicit_bg_stays_fixed_across_theme_switch() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let fixed = Color::hex(0x102030);
        let mut app = App::new("t", 60, 60).bg(fixed);
        let handle = app.theme_handle();
        app = app.content(Element::col());
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(60, 60).unwrap();
        handle.set(crate::theme::Theme::dark());
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(60, 60));
        assert_eq!(handler.bg, fixed, "App::bg 显式指定的清屏色不随主题变化");
    }

    #[test]
    fn explicit_bg_survives_later_theme_call() {
        // `.bg(c).theme(t)` 与 `.theme(t).bg(c)` 必须同义：显式底色不被 theme() 覆盖。
        let fixed = Color::hex(0x102030);
        let a = App::new("t", 60, 60)
            .bg(fixed)
            .theme(crate::theme::Theme::dark());
        assert_eq!(a.cfg.bg, fixed, "后调 theme() 不应覆盖显式 bg");
        let b = App::new("t", 60, 60)
            .theme(crate::theme::Theme::dark())
            .bg(fixed);
        assert_eq!(b.cfg.bg, fixed);
    }

    #[test]
    fn theme_update_mutates_in_place() {
        let mut app = App::new("t", 60, 60);
        let handle = app.theme_handle();
        handle.update(|t| t.palette.accent = Color::hex(0x123456));
        assert_eq!(
            handle.current().palette.accent,
            Color::hex(0x123456),
            "update 应就地修改当前主题"
        );
    }

    #[test]
    fn hotkey_handle_queues_and_host_drains_ops() {
        use crate::event::{Hotkey, HotkeyOp};
        use crate::platform::AppHandler;
        let mut app = App::new("t", 60, 60);
        let hk = app.hotkey_handle(Hotkey::new(Key::Char('D')).ctrl().alt(), |_| {});
        let hk2 = hk.clone();
        hk.set(Hotkey::new(Key::Char('J')).ctrl());
        hk2.set_enabled(false);
        let mut handler = app.content(Element::col()).into_handler_for_test();
        let ops = handler.take_hotkey_ops();
        assert_eq!(
            ops,
            vec![
                (0, HotkeyOp::Rebind(Hotkey::new(Key::Char('J')).ctrl())),
                (0, HotkeyOp::SetEnabled(false)),
            ],
            "句柄操作应按序入列且携带正确的槽位 id"
        );
        assert!(handler.take_hotkey_ops().is_empty(), "取走后队列应清空");
    }

    #[test]
    fn render_drains_pending_messages() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let got = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let g2 = got.clone();
        let mut app = App::new("t", 50, 50);
        let tx = app.channel::<u32>(move |_ctx, m| g2.set(m));
        app = app.content(Element::col());
        tx.send(7).unwrap();
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(50, 50).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(50, 50));
        assert_eq!(got.get(), 7, "render 前排空 pump，消息写入状态");
    }

    /// 复现离屏截图路径（`--click`）：先 render 暖布局，再经 `handler.on_pointer` 合成
    /// Down+Up，断言点击真的切了标签页。走的是宿主完整链路（坐标换算、dispatch、
    /// 状态维护），比直接调 `tree.dispatch_pointer` 更贴近 `run_offscreen` 实况。
    #[test]
    fn offscreen_pointer_click_switches_tab_through_handler() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let sel = crate::signal::signal(1usize);
        let tabs = Element::tabs(
            sel,
            vec![("甲", Element::label("A")), ("乙", Element::label("B"))],
        );
        let app = App::new("t", 300, 200).content(tabs);
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        // 首帧 render：暖布局（与 run_offscreen 首个 render 对应）。
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        // 合成点击首项（scale=1，物理=逻辑）。首项左缘内侧，padding≥8 必落在第 0 项。
        let at = Point::new(6, 20);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        assert_eq!(sel.get(), 0, "离屏合成点击首个标签应把选中索引切到 0");
    }

    /// P2-2 回归：持有运行期句柄的应用状态结构应当能在测试里造出来。
    ///
    /// 没有 `detached()` 时错在哪：这个 `Settings` 只能从 `App::theme_handle()` /
    /// `App::hotkey_handle()` 取句柄，而 `App` 建不起来（要开窗口），于是
    /// **整个结构造不出实例**——不是"换肤那部分测不了"，是 `label()` 这种跟句柄
    /// 毫无关系的方法也一并测不了。下游只能把逻辑一个个抽成不依赖状态结构的自由
    /// 函数，测的是抽出来的那一半。
    #[test]
    fn detached_handles_make_handle_holding_state_constructible() {
        use crate::event::{Hotkey, HotkeyOp};

        struct Settings {
            theme: ThemeHandle,
            hotkey: HotkeyHandle,
            dark: bool,
        }
        impl Settings {
            /// 与句柄无关的方法——正是它此前被连带牵累而测不了。
            fn label(&self) -> &'static str {
                if self.dark {
                    "深色"
                } else {
                    "浅色"
                }
            }
            fn apply_dark(&mut self) {
                self.dark = true;
                self.theme.set(Theme::dark());
            }
            fn rebind(&self, hk: Hotkey) {
                self.hotkey.set(hk);
            }
        }

        let mut s = Settings {
            theme: ThemeHandle::detached(Theme::default()),
            hotkey: HotkeyHandle::detached(),
            dark: false,
        };
        assert_eq!(s.label(), "浅色", "不依赖句柄的方法应照常可测");

        s.apply_dark();
        assert_eq!(s.label(), "深色");
        assert_eq!(
            s.theme.current().palette.bg,
            Theme::dark().palette.bg,
            "detached 句柄的 set 应真的换掉自己那份主题"
        );

        let want = Hotkey::new(Key::Char('J')).ctrl();
        s.rebind(want);
        assert_eq!(
            s.hotkey.pending_ops(),
            vec![HotkeyOp::Rebind(want)],
            "改绑意图应记在队列里供断言"
        );
    }

    /// `pending_ops` 只读不取走，且只报本句柄的那些。
    ///
    /// 没有这两条性质时错在哪：若它像 `take_hotkey_ops` 一样清空队列，下游在真句柄上
    /// 调一次断言，就把宿主本该执行的改绑偷走了——测试通过，真机上热键静默不改。
    /// 若不按 id 过滤，多热键应用里断言"改的是这一个"永远成立，等于没断言。
    #[test]
    fn pending_ops_is_non_draining_and_scoped_to_own_id() {
        use crate::event::{Hotkey, HotkeyOp};
        use crate::platform::AppHandler;
        let mut app = App::new("t", 60, 60);
        let a = app.hotkey_handle(Hotkey::new(Key::Char('D')).ctrl(), |_| {});
        let b = app.hotkey_handle(Hotkey::new(Key::Char('E')).ctrl(), |_| {});
        let a_want = Hotkey::new(Key::Char('J')).ctrl();
        a.set(a_want);
        b.set_enabled(false);

        assert_eq!(
            a.pending_ops(),
            vec![HotkeyOp::Rebind(a_want)],
            "只应报本句柄（槽位 0）的意图"
        );
        assert_eq!(b.pending_ops(), vec![HotkeyOp::SetEnabled(false)]);
        // 读两次结果相同 —— 没取走。
        assert_eq!(
            a.pending_ops(),
            vec![HotkeyOp::Rebind(a_want)],
            "读取不应清空"
        );

        // 宿主仍能拿到全部两条：断言没有偷走该执行的意图。
        let mut handler = app.content(Element::col()).into_handler_for_test();
        assert_eq!(
            handler.take_hotkey_ops(),
            vec![
                (0, HotkeyOp::Rebind(a_want)),
                (1, HotkeyOp::SetEnabled(false)),
            ],
            "pending_ops 读过之后宿主仍应消费到全部意图"
        );
    }

    /// `--size W H` 覆盖窗口尺寸，并把 `min_size` 的下限**一并放开**。
    ///
    /// 没有放开下限时错在哪：要测的正是「最小尺寸下布局还成不成立」，而 `min_size`
    /// 会把窗口钳回下限——于是 `--size` 看起来生效了，截出来的却还是下限那张图，
    /// 比对不出任何问题。没有 `--size` 时尺寸写死在 `App::new` 里，这类验证只能改
    /// 代码重编译，进不了 CI。
    #[test]
    fn size_arg_overrides_window_and_releases_min_size() {
        let args: Vec<String> = ["app", "--size", "720", "480"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let app = App::new("t", 920, 620)
            .min_size(800, 600)
            .screenshot_args(&args);
        assert_eq!((app.cfg.width, app.cfg.height), (720, 480));
        assert_eq!(
            (app.cfg.min_width, app.cfg.min_height),
            (0, 0),
            "下限须放开，否则窗口被钳回 800x600，截出来的不是要验的那张图"
        );
    }

    /// 非法 `--size` 不该把窗口改成 0 或负数（那会让后续 `Pixmap::new` 直接失败）。
    #[test]
    fn size_arg_rejects_nonpositive_and_keeps_min_size() {
        let mk = |a: &[&str]| -> Vec<String> { a.iter().map(|s| s.to_string()).collect() };
        for bad in [
            mk(&["app", "--size", "0", "480"]),
            mk(&["app", "--size", "-5", "480"]),
            mk(&["app", "--size", "abc", "480"]),
            mk(&["app", "--size"]),
        ] {
            let app = App::new("t", 920, 620)
                .min_size(800, 600)
                .screenshot_args(&bad);
            assert_eq!(
                (app.cfg.width, app.cfg.height, app.cfg.min_width),
                (920, 620, 800),
                "非法 --size 应整条忽略，含不动 min_size：{bad:?}"
            );
        }
    }

    /// `--type` / `--key` 按**出现顺序**混合入列。
    ///
    /// 没有它时错在哪：若两者各扫一遍参数分别入列，`--type ab --key Enter --type c`
    /// 会变成「ab、c、Enter」——回车跑在最后一段文本之后，截到的是另一个状态。
    #[test]
    fn type_and_key_args_interleave_in_source_order() {
        use crate::platform::ScreenshotKey;
        let args: Vec<String> = ["app", "--type", "ab", "--key", "Enter", "--type", "c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let app = App::new("t", 60, 60).screenshot_args(&args);
        let got: Vec<String> = app
            .cfg
            .screenshot_keys
            .iter()
            .map(|k| match k {
                ScreenshotKey::Text(t) => format!("T:{t}"),
                ScreenshotKey::Key(e) => format!("K:{:?}", e.key),
            })
            .collect();
        assert_eq!(got, vec!["T:ab", "K:Enter", "T:c"]);
    }

    /// 键名解析：具名键、修饰前缀、Ctrl+字母，以及无法识别时不入列。
    ///
    /// 没有它时错在哪：认错键名的症状是「截图跟没按一样」——不会报错、不会 panic，
    /// 只是那一步静默没发生，比对时只看到一张"怎么没反应"的图。
    #[test]
    fn key_spec_parses_names_modifiers_and_rejects_unknown() {
        use crate::event::Key;
        let p = |s: &str| super::parse_key_spec(s);

        assert_eq!(p("Enter").map(|e| e.key), Some(Key::Enter));
        assert_eq!(p("esc").map(|e| e.key), Some(Key::Escape), "别名 esc");
        assert_eq!(p("DOWN").map(|e| e.key), Some(Key::Down), "大小写不敏感");

        let st = p("shift+Tab").expect("shift+Tab 应可解析");
        assert_eq!(st.key, Key::Tab);
        assert!(st.shift && !st.ctrl);

        // Ctrl+字母走 Key::Other(vk)，与 TextInput 对 Ctrl+A/C/V/X 的处理对齐；
        // 若这里发成 Key::Char('a')，全选就会变成"输入一个字母 a"。
        let ca = p("ctrl+a").expect("ctrl+a 应可解析");
        assert_eq!(ca.key, Key::Other(u32::from(b'A')));
        assert!(ca.ctrl);

        assert_eq!(p("PageUp").map(|e| e.key), Some(Key::PageUp));
        assert_eq!(p("pgdn").map(|e| e.key), Some(Key::PageDown), "别名 pgdn");

        assert!(p("F13").is_none(), "未支持的键应回 None 而不是猜一个");
        assert!(p("").is_none());
    }

    /// 合成按键走的是 `handler.on_key` —— 与真实按键**同一条通路**，
    /// 焦点裁决与控件回调都照常。
    ///
    /// 没有它时错在哪：这条是整个键盘通路唯一能进视觉回归的途径。若回放绕过宿主
    /// （比如直接调 `tree.dispatch_key`），截到的就不是真实交互的结果——Tab 兜底、
    /// 失焦裁决、needs_relayout 全都不会发生。
    #[test]
    fn synthesized_typing_reaches_the_autofocused_control() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let text = crate::signal::signal(String::from("旧"));
        let app = App::new("t", 200, 120).content(
            Element::col().padding(8).child(
                Element::text_input(text, "")
                    .height(30)
                    .autofocus_select_all(),
            ),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(200, 120).unwrap();
        // 首帧兑现 autofocus（与 run_offscreen 的首个 render 对应）。
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 120));
        for ch in "新词".chars() {
            handler.on_key(crate::event::KeyEvent {
                key: crate::event::Key::Char(ch),
                pressed: true,
                shift: false,
                ctrl: false,
            });
        }
        assert_eq!(
            text.get(),
            "新词",
            "合成打字应落进 autofocus 的输入框，并覆盖被全选的旧内容"
        );
    }

    /// 合成点击的命中结果应当可读——落空与"命中但无人消费"必须分得开。
    ///
    /// 没有它时错在哪：合成点击不命中任何节点是**完全无声**的，不报错不 panic，截出来
    /// 的图和没点一样。于是"坐标写错了"与"框架丢了这次点击"在现象上无法区分，只能靠
    /// 反复猜坐标——wind-dict 就是这么把一次坐标失误误判成上游丢帧的。
    #[test]
    fn pointer_hit_distinguishes_miss_from_unconsumed() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let app = App::new("t", 200, 200).content(
            Element::col()
                .padding(10)
                .child(Element::button("可点").height(30))
                .child(Element::label("纯文本").width(120).height(30))
                .child(Element::flex_spacer()),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(200, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 200));
        // 必须 Down+Up 成对：只发 Down 的话按钮的指针捕获不释放，后续 Down 全被它接走，
        // 测出来的"命中"其实是捕获转发的结果，不是命中测试的结果。
        let click = |h: &mut UiHost, p: Point| {
            h.on_pointer(PointerEvent::single(
                PointerKind::Down,
                p,
                MouseButton::Left,
            ));
            h.on_pointer(PointerEvent::single(PointerKind::Up, p, MouseButton::Left));
        };

        assert!(
            handler.last_pointer_hit().is_none(),
            "尚无指针事件时不该编造命中结果"
        );

        // 按钮：命中且被消费。
        let btn = handler.focus.order[0];
        let b = handler.tree.abs_bounds(btn);
        click(&mut handler, Point::new(b.x + b.w / 2, b.y + b.h / 2));
        let hit = handler.last_pointer_hit().expect("应有命中记录");
        assert_eq!(hit.node, Some(btn));
        assert!(hit.consumed, "点按钮应被消费");

        // 纯文本：命中了节点，但没人消费——点在非交互区域上。
        // 定宽是为了让坐标确定：不设宽时标签按内容自适应，点在文字右侧的空处就落到
        // 容器上了，而无 widget 的容器本身不是命中目标。
        click(&mut handler, Point::new(60, 55));
        let hit = handler.last_pointer_hit().expect("应有命中记录");
        assert!(hit.node.is_some(), "标签本身是个节点，应当命中");
        assert!(
            !hit.consumed,
            "标签不消费点击——这一档必须与「完全没命中」分得开，两者的排查方向不同"
        );

        // 窗口内的空白：什么都没命中。
        click(&mut handler, Point::new(195, 195));
        let hit = handler.last_pointer_hit().expect("应有命中记录");
        assert_eq!(hit.node, None, "空白处应报未命中");
        assert!(!hit.consumed);
    }

    /// `NodeId` 的 Debug 是紧凑格式——诊断输出里最常出现的就是它。
    ///
    /// 没有它时错在哪：派生格式 `NodeId { index: 12, generation: 0 }` 一行里塞三五个就没法
    /// 读了。代际非 0 必须显示，否则"旧 id 指向新节点"这类问题在输出里看不出来。
    #[test]
    fn node_id_debug_is_compact_and_keeps_generation() {
        use crate::core::Tree;
        let mut tree = Tree::new();
        let a = Element::leaf().build(&mut tree);
        assert_eq!(format!("{a:?}"), "#0", "首个节点应是 #0，不带代际");
        tree.remove(a);
        let b = Element::leaf().build(&mut tree);
        assert_eq!(
            format!("{b:?}"),
            "#0g1",
            "复用槽位必须带出代际，否则与被回收的旧 id 在输出里长得一样"
        );
    }
}
