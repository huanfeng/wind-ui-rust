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

use crate::sync::{new_channel, ChannelPump, Sender, WakerShared};

use crate::core::{DamageReq, DispatchResult, EventCtx, NodeId, Tree};
use crate::event::{CursorShape, Key, MouseButton, PointerEvent, PointerKind, WindowOp};
use crate::geometry::{Color, Point, Rect, Size};
use crate::platform::{self, AppHandler, DialogRequest, WindowConfig};
use crate::render::Paint;
use crate::signal::Signal;
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
    /// 构建期收集所有 `Element::dialog` 注册的显示 Signal，供 ESC / WM_CLOSE 优先关闭对话框。
    static MODAL_SIGNALS: RefCell<Vec<Signal<bool>>> = const { RefCell::new(Vec::new()) };
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

/// 注册一个对话框显示信号（由 `Element::dialog` 在构建期调用）。
pub(crate) fn register_modal(show: Signal<bool>) {
    MODAL_SIGNALS.with(|s| s.borrow_mut().push(show));
}

/// 关闭当前最顶层（最后注册）的可见对话框。返回 true 表示确实关闭了一个。
fn close_topmost_modal() -> bool {
    MODAL_SIGNALS.with(|s| {
        for sig in s.borrow().iter().rev() {
            if sig.get() {
                sig.set(false);
                return true;
            }
        }
        false
    })
}

type RenderClosure = Box<dyn FnMut(&mut dyn crate::render::RenderTarget, Size)>;

/// App 级回调：不在任何控件的事件时机上，却同样收 [`EventCtx`]（`on_interval` 等）。
/// 宿主以根节点为 `self_id` 借出（见 `UiHost::apply_app_effects`）。
type AppCallback = Box<dyn FnMut(&mut EventCtx)>;

/// 关闭请求拦截器（[`App::on_close_request`]）：返回 true 放行、false 取消。
/// 与 [`AppCallback`] 分开是因为它多一个返回值——那个 `bool` 被平台同步等待。
type CloseHandler = Box<dyn FnMut(&mut EventCtx) -> bool>;

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
    /// 替换当前主题并请求重绘。
    pub fn set(&self, t: Theme) {
        *self.inner.borrow_mut() = Rc::new(t);
        crate::anim::request_repaint();
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
    pumps: Vec<ChannelPump>,
    intervals: Vec<(Duration, AppCallback)>,
    waker_shared: Option<Arc<WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
    close_handler: Option<CloseHandler>,
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
                tray: None,
                hotkeys: Vec::new(),
                start_hidden: false,
                frameless: false,
                animations: None,
                accelerated: false,
                min_width: 0,
                min_height: 0,
            },
            render: None,
            content: None,
            theme: None,
            theme_src: None,
            pumps: Vec::new(),
            intervals: Vec::new(),
            waker_shared: None,
            single: None,
            close_handler: None,
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

    /// 启用 GPU 加速渲染（Direct2D 后端）。默认关闭走软渲染。仅对不透明大窗有意义；
    /// RDP 远程会话、无可用 GPU、离屏截图等情形会自动回退软渲染（绝不 panic）。
    pub fn accelerated(mut self, on: bool) -> Self {
        self.cfg.accelerated = on;
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

    /// 从命令行解析 `--screenshot <path>` 与可选 `--scale <f>`（高 DPI 截屏验证）。
    pub fn screenshot_from_args(mut self) -> Self {
        let args: Vec<String> = std::env::args().collect();
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
        // --hover X Y：截屏前在 (X,Y) 合成悬停并等待超过提示延时，验证 tooltip 等悬停视觉。
        if let Some(i) = args.iter().position(|a| a == "--hover") {
            if let (Some(x), Some(y)) = (
                args.get(i + 1).and_then(|s| s.parse::<i32>().ok()),
                args.get(i + 2).and_then(|s| s.parse::<i32>().ok()),
            ) {
                self.cfg.screenshot_hover = Some((x, y));
            }
        }
        // --accelerated：启用 GPU（Direct2D）后端，便于与软渲染对比测试（仅窗口模式生效）。
        if args.iter().any(|a| a == "--accelerated") {
            self.cfg.accelerated = true;
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
    /// **平台状态：全局热键当前仅 Windows 实现。** macOS 上本方法在 debug 期 panic
    /// （提示未实现）、release 期静默忽略；托盘、[`Self::start_hidden`] 与窗口显隐在
    /// 两平台均可用。详见 `src/platform/macos/hotkey.rs`。
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
                self.pumps,
                self.intervals,
                self.close_handler,
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
            self.pumps,
            self.intervals,
            self.close_handler,
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
        self.pumps.push(pump);
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
    /// 跨线程通道的排空回调：渲染前在 UI 线程依次调用，把后台数据写入控件状态。
    pumps: Vec<ChannelPump>,
    /// 定时器回调列表（与 interval_durs 下标对应）。
    interval_cbs: Vec<AppCallback>,
    /// 定时器间隔列表（平台据此注册 SetTimer/NSTimer）。
    interval_durs: Vec<std::time::Duration>,
    /// 帧耗时浮层开关（环境变量 WINDUI_FPS 非空时开启）。
    show_fps: bool,
    /// 关闭请求拦截器：返回 true 允许关闭，false 取消。None 时默认允许。
    close_handler: Option<CloseHandler>,
    /// 关闭请求转为隐藏窗口（常驻托盘类应用）。
    hide_on_close: bool,
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
        // 优先关闭最顶层可见对话框（不退出窗口）。
        if close_topmost_modal() {
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

    /// 落地控件发出的关闭请求（`EventCtx::request_close`）：`hide_on_close` 时转为隐藏。
    ///
    /// 关闭请求有**三个**入口，走两套管道，容易漏：
    /// - ESC 与系统标题栏 × → `on_close_request` → [`Self::resolve_close`]
    /// - 控件主动请求 → `res.close` → **本函数**
    ///
    /// 第三个入口最易被忽略：有边框窗口的 × 由系统绘制、走 `WM_CLOSE`；而**无边框窗口
    /// 的 × 是自绘控件**（`Element::window_button(WindowButtonKind::Close)`），走的是
    /// `request_close()`。漏掉本函数，`.frameless().hide_on_close()` 会直接杀进程。
    ///
    /// 此处**不询问 `close_handler`**：`request_close()` 的语义是「应用已决定关闭」，
    /// 而非「用户请求关闭」，沿用既有行为不变。
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
        pumps: Vec<ChannelPump>,
        intervals: Vec<(std::time::Duration, AppCallback)>,
        close_handler: Option<CloseHandler>,
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
            pumps,
            interval_cbs,
            interval_durs,
            show_fps: std::env::var("WINDUI_FPS").is_ok_and(|v| v != "0" && !v.is_empty()),
            close_handler,
            hide_on_close,
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
            focus,
            consumed,
            menu,
            open_url,
            window_op,
            toast,
            dialog,
        } = res;
        if let Some(f) = focus {
            let old = self.focus.current;
            self.tree.set_focused(Some(f), old);
            self.focus.current = Some(f);
            match focus_from {
                // 鼠标聚焦不显示焦点环，保持纯鼠标操作的纯净观感。
                FocusSource::Pointer => self.focus.visible = false,
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
        if close {
            self.apply_close_intent();
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
        if self.pumps.is_empty() {
            return;
        }
        let mut pumps = std::mem::take(&mut self.pumps);
        let mut results = Vec::new();
        for pump in pumps.iter_mut() {
            results.append(&mut pump(&mut self.tree, root));
        }
        self.pumps = pumps;
        for res in results {
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

    fn on_key(&mut self, ev: crate::event::KeyEvent) -> bool {
        self.sync_clock();
        // 菜单激活时由浮层独占键盘：↑↓ 选项、←→ 进出子菜单、回车/空格执行、
        // Escape 关闭，其余吞掉（避免打到被遮住的控件上）。
        if self.menu.is_open() {
            return self.handle_menu_key(ev);
        }
        // Tab 由宿主独占用于焦点导航，并启用焦点环显示。焦点环跨节点变化（低频）→ 整窗。
        if ev.key == Key::Tab {
            self.focus.visible = true;
            let moved = self.move_focus(!ev.shift);
            if moved {
                self.damage.needs_full = true;
            }
            return moved;
        }
        // 其余键先交给焦点控件；未被消费的 Escape 回退为关闭窗口。
        let res = self.tree.dispatch_key(ev, self.focus.current);
        // 键盘路径不参与失焦裁决（没有"点在别处"这回事），故 blur_at 恒为 None。
        let (repaint, damage, consumed) =
            self.apply_dispatch_effects(res, FocusSource::Keyboard, None);
        if !consumed && ev.key == Key::Escape && self.resolve_close() {
            self.close = true;
        }
        // 键盘改动可能影响布局（文本增减）或他处（切页/对话框）→ 置 needs_relayout：
        // render 重排后用结构签名判定，签名不变（定宽输入打字）走局部，变了升级整窗。
        if repaint {
            self.apply_damage(damage);
            self.damage.needs_relayout = true;
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

    fn wants_animation(&self) -> bool {
        crate::anim::animation_requested()
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
        let mut app = App::new("t", 100, 100);
        let tx = app.channel::<u32>(|_ctx, _m| {});
        let h = std::thread::spawn(move || tx.send(5));
        assert!(h.join().unwrap().is_ok());
        assert_eq!(app.pumps.len(), 1);
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

    /// 控件的 request_close（无边框窗口的自绘 × 走此路）也须受 hide_on_close 约束。
    /// 它与 ESC/系统 × 走的是**另一条管道**（res.close 而非 on_close_request），
    /// 漏接会让 .frameless().hide_on_close() 直接杀进程。
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

    /// 未开 hide_on_close 时，控件的 request_close 仍须真关——不可回归。
    #[test]
    fn widget_request_close_still_closes_by_default() {
        let app = App::new("t", 100, 100).content(Element::col());
        let mut app = app.into_handler_for_test();
        app.apply_close_intent();
        assert!(app.wants_close());
        assert_eq!(app.take_window_op(), None);
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
}
