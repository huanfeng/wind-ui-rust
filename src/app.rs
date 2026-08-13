//! 应用入口与交互宿主。
//!
//! `App` 构建器组装窗口配置与控件树；`UiHost` 持有运行期交互状态
//! （树、文字引擎、hover/capture/focus）并实现 `AppHandler` 供平台驱动。

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::sync::{new_channel, Sender, WakerShared};

use tiny_skia::Pixmap;

use crate::core::{DamageReq, DispatchResult, NodeId, Tree};
use crate::event::{
    CursorShape, Key, MenuAction, MenuItem, MouseButton, PointerEvent, PointerKind, ToastRequest,
    WindowOp,
};
use crate::geometry::{Color, Point, Rect, Size};
use crate::platform::{self, AppHandler, DialogRequest, WindowConfig};
use crate::render::{Paint, SkiaCanvas};
use crate::signal::Signal;
use crate::text::{PlatformTextEngine, TextEngine};
use crate::theme::Theme;

thread_local! {
    /// 构建期收集所有 `Element::dialog` 注册的显示 Signal，供 ESC / WM_CLOSE 优先关闭对话框。
    static MODAL_SIGNALS: RefCell<Vec<Signal<bool>>> = const { RefCell::new(Vec::new()) };
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
use crate::ui::Element;

// ---- 上下文菜单（宿主层自绘浮层）----

const MENU_ITEM_H: i32 = 30;
/// 两行项（带 subtitle）行高。
const MENU_ITEM_H_TALL: i32 = 46;
const MENU_SEP_H: i32 = 9;
const MENU_PAD_X: i32 = 12;
const MENU_VPAD: i32 = 6;
const MENU_MIN_W: i32 = 140;
/// 下拉菜单面板最大可视高度（超出后启用滚动）。
const MENU_MAX_H: i32 = 320;
const MENU_FONT: f32 = 13.5;
/// 图标列宽（有图标项时预留），也用作尾随可点击图标的命中/绘制列宽。
const MENU_ICON_W: i32 = 18;
/// 图标与标签间距。
const MENU_GAP: i32 = 8;
/// 标签与尾随（快捷键/箭头）间最小间距。
const MENU_TRAIL_GAP: i32 = 18;
/// 尾随徽章胶囊左右内边距。
const BADGE_PAD_X: i32 = 8;
/// 尾随徽章胶囊高度。
const BADGE_H: i32 = 20;
/// 菜单弹层距窗口四边最小留白（逻辑像素）：与 resize 边框区域宽度对齐，
/// 确保弹层滚动条不会覆盖到缩放操作区域，无需修改 WM_NCHITTEST 优先级。
const MENU_EDGE_MARGIN: i32 = 10;

/// 单项行高：分隔线固定细线高；带 subtitle 的项两行更高；否则单行标准高。
fn menu_item_height(it: &MenuItem) -> i32 {
    if it.separator {
        MENU_SEP_H
    } else if it.subtitle.is_some() {
        MENU_ITEM_H_TALL
    } else {
        MENU_ITEM_H
    }
}

/// 焦点由哪种设备转移而来。决定焦点环显不显示——`:focus-visible` 的判据是用户最近
/// 一次交互用的什么设备，而不是这次聚焦是不是程序性的。
#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusSource {
    Pointer,
    Keyboard,
}

/// 该项能否被键盘高亮停留：分隔线与禁用项跳过。子菜单父项**可以**停留
/// （停在它上面按 → 展开），故不看 `submenu`——这与 `MenuItem::is_actionable`
/// （能否执行）是两个问题。
fn menu_item_selectable(it: &MenuItem) -> bool {
    !it.separator && it.enabled
}

/// 悬停提示：触发延时（ms）、字号、内边距、相对指针的偏移。
/// 换行宽度上限由宿主经 `Theme.tooltip.max_width` 配置（见 [`crate::theme::TooltipTheme::max_width`]）。
const TOOLTIP_DELAY_MS: u64 = 500;
const TOOLTIP_FONT: f32 = 13.0;
const TOOLTIP_PAD_X: i32 = 8;
const TOOLTIP_PAD_Y: i32 = 4;
const TOOLTIP_CURSOR_DX: i32 = 12;
const TOOLTIP_CURSOR_DY: i32 = 20;

/// 轻提示浮层：字号、图标字号、内边距、图标与文字间距、淡入/淡出时长（ms）。
const TOAST_FONT: f32 = 14.0;
const TOAST_ICON_FONT: f32 = 18.0;
const TOAST_PAD_X: i32 = 16;
const TOAST_PAD_Y: i32 = 11;
const TOAST_ICON_GAP: i32 = 12;
const TOAST_MIN_W: i32 = 132;
const TOAST_FADE_IN_MS: u64 = 140;
const TOAST_FADE_OUT_MS: u64 = 280;
/// 同屏最多堆叠的轻提示条数：超过丢最旧。
const TOAST_MAX: usize = 4;
/// 顶部居中堆叠布局：距窗口顶边距、条间距、✕ 关闭命中宽、左强调色条宽。
const TOAST_TOP_MARGIN: i32 = 16;
const TOAST_GAP: i32 = 10;
const TOAST_CLOSE_W: i32 = 22;
/// 文字换行区最小宽度：即便窗口极窄，也保留基本可读宽度（宁可面板贴边也不塌缩为 0）。
const TOAST_TEXT_MIN_W: i32 = 60;

/// 活动轻提示：内容 + 起始时刻 + 悬停暂停累计。淡入淡出/过期均按「有效流逝」推算。
struct ToastState {
    req: ToastRequest,
    shown_at_ms: u64,
    /// 若正被悬停：记录进入悬停时刻（冻结倒计时）；否则 None。
    paused_at_ms: Option<u64>,
    /// 历史累计暂停总时长（ms）。
    paused_total_ms: u64,
}

impl ToastState {
    /// 扣除暂停后的有效流逝（ms）。
    fn active_elapsed(&self, now_ms: u64) -> u64 {
        let raw = now_ms.saturating_sub(self.shown_at_ms);
        let cur_pause = self
            .paused_at_ms
            .map(|p| now_ms.saturating_sub(p))
            .unwrap_or(0);
        raw.saturating_sub(self.paused_total_ms)
            .saturating_sub(cur_pause)
    }
    /// 切换悬停：进入则起暂停，离开则把本段并入累计。
    fn set_hover(&mut self, now_ms: u64, hovered: bool) {
        match (hovered, self.paused_at_ms) {
            (true, None) => self.paused_at_ms = Some(now_ms),
            (false, Some(p)) => {
                self.paused_total_ms += now_ms.saturating_sub(p);
                self.paused_at_ms = None;
            }
            _ => {}
        }
    }
    /// 是否已过期（应清除）。
    fn expired(&self, now_ms: u64) -> bool {
        self.active_elapsed(now_ms) >= self.req.duration_ms
    }
    /// 当前不透明度系数 [0,1]：前段淡入、末段淡出、中间恒 1。
    fn alpha(&self, now_ms: u64) -> f32 {
        let e = self.active_elapsed(now_ms);
        let d = self.req.duration_ms;
        if e < TOAST_FADE_IN_MS {
            return e as f32 / TOAST_FADE_IN_MS as f32;
        }
        let fade_out_start = d.saturating_sub(TOAST_FADE_OUT_MS);
        if e >= fade_out_start && d > fade_out_start {
            return ((d - e) as f32 / TOAST_FADE_OUT_MS as f32).clamp(0.0, 1.0);
        }
        1.0
    }
}

/// 单级菜单面板：一组项 + 面板矩形 + 悬停项 + 是否含图标列。
struct MenuLevel {
    items: Vec<MenuItem>,
    rect: Rect,
    hover: Option<usize>,
    has_icons: bool,
    /// 该级由父级哪一项展开（根级为 None）；用于避免同项重复重建子菜单。
    spawn: Option<usize>,
    /// 项内容总高（含上下内边距，未截断）；超出 rect.h 时启用滚动。
    content_h: i32,
    /// 当前滚动偏移（像素，0=顶部）。
    scroll: i32,
}

impl MenuLevel {
    /// 每项的 (顶部 y, 高度)（逻辑坐标，已减去 scroll 偏移）。
    fn item_rows(&self) -> Vec<(i32, i32)> {
        let mut y = self.rect.y + MENU_VPAD - self.scroll;
        let mut rows = Vec::with_capacity(self.items.len());
        for it in &self.items {
            let h = menu_item_height(it);
            rows.push((y, h));
            y += h;
        }
        rows
    }
    /// 最大可滚动量（content_h 超出面板高时才有效）。
    fn max_scroll(&self) -> i32 {
        (self.content_h - self.rect.h).max(0)
    }
    /// 命中点 → 项下标（分隔线不可命中）。
    fn item_at(&self, p: Point) -> Option<usize> {
        if !self.rect.contains(p) {
            return None;
        }
        for (i, (top, h)) in self.item_rows().into_iter().enumerate() {
            if p.y >= top && p.y < top + h {
                return if self.items[i].separator {
                    None
                } else {
                    Some(i)
                };
            }
        }
        None
    }
    /// 命中尾随可点击图标 → 项下标。图标固定贴右绘制（`r.right()-MENU_PAD_X-MENU_ICON_W`
    /// 起始），与 badge 是否存在无关，故命中矩形无需重算 badge 宽度即可复刻绘制位置。
    fn trailing_icon_at(&self, p: Point) -> Option<usize> {
        if !self.rect.contains(p) {
            return None;
        }
        let icon_left = self.rect.right() - MENU_PAD_X - MENU_ICON_W;
        let icon_right = self.rect.right() - MENU_PAD_X;
        if p.x < icon_left || p.x >= icon_right {
            return None;
        }
        for (i, (top, h)) in self.item_rows().into_iter().enumerate() {
            if p.y >= top && p.y < top + h && self.items[i].trailing_icon.is_some() {
                return Some(i);
            }
        }
        None
    }
}

/// 宿主管理的上下文菜单浮层：可级联多级面板，在控件树之上自绘、拦截指针，
/// 叶子项激活时向目标控件合成按键或运行闭包。
struct ContextMenu {
    /// 面板栈：levels[0]=根，其后为依次展开的子菜单。
    levels: Vec<MenuLevel>,
    /// 发起菜单的控件（合成按键的派发目标）。
    target: NodeId,
    /// 项重建器（见 [`crate::event::MenuRequest::rebuild`]）：粘滞项点击后原地刷新。
    rebuild: Option<Rc<dyn Fn() -> Vec<MenuItem>>>,
}

impl ContextMenu {
    /// 命中点落在最深（最上层）的哪一级面板内。
    fn level_at(&self, p: Point) -> Option<usize> {
        self.levels.iter().rposition(|l| l.rect.contains(p))
    }

    /// 粘滞项点击后原地刷新各级项：沿 `spawn` 路径把重建结果逐级换进去，
    /// **保留每级的 rect/scroll/hover**（见 `MenuRequest::rebuild` 关于宽度不变的说明）。
    /// 重建后项数变少导致某级的 spawn 越界或不再是子菜单父项时，截断其后的级。
    fn refresh_items(&mut self) {
        let Some(rb) = self.rebuild.clone() else {
            return;
        };
        let mut items = rb();
        let mut keep = self.levels.len();
        for k in 0..self.levels.len() {
            let next_spawn = self.levels.get(k + 1).and_then(|l| l.spawn);
            let sub = next_spawn
                .and_then(|i| items.get(i))
                .map(|it| it.submenu.clone());
            self.levels[k].has_icons = items.iter().any(|it| it.icon.is_some());
            self.levels[k].content_h =
                items.iter().map(menu_item_height).sum::<i32>() + 2 * MENU_VPAD;
            self.levels[k].items = items;
            let max_sc = self.levels[k].max_scroll();
            self.levels[k].scroll = self.levels[k].scroll.clamp(0, max_sc);
            match sub {
                // 子菜单父项仍在：继续把它的子项换进下一级。
                Some(s) if !s.is_empty() => items = s,
                // 下一级已无来源（项没了/不再有子菜单）：截断到本级。
                _ => {
                    keep = k + 1;
                    break;
                }
            }
        }
        self.levels.truncate(keep);
    }
}

type RenderClosure = Box<dyn FnMut(&mut dyn crate::render::RenderTarget, Size)>;

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

/// 运行期热键句柄（`App::hotkey_rc` 返回）。克隆进控件回调，随时改绑/启停：
///
/// ```ignore
/// let hk = app.hotkey_rc(Hotkey::new(Key::Char('D')).ctrl().alt(), |ctx| ctx.show_window());
/// // 设置页回调里：
/// hk.rebind(Hotkey::new(Key::Char('J')).ctrl());   // 立即向系统换注册
/// hk.set_enabled(false);                            // 注销，把组合归还系统
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
    /// 改绑到新组合（下一次消息循环生效；失败回滚保留旧绑定）。
    pub fn rebind(&self, hotkey: crate::event::Hotkey) {
        self.queue
            .borrow_mut()
            .push((self.id, crate::event::HotkeyOp::Rebind(hotkey)));
        // 唤一帧，让平台意图消费点尽快跑到。
        crate::anim::request_repaint();
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
    pumps: Vec<Box<dyn FnMut()>>,
    intervals: Vec<(Duration, Box<dyn FnMut()>)>,
    waker_shared: Option<Arc<WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
    close_handler: Option<Box<dyn FnMut() -> bool>>,
    /// 关闭请求转为隐藏窗口。与 `close_handler` 同属核心层的关闭决策链输入，
    /// 平台层对此无感知，故不放 `WindowConfig`。
    hide_on_close: bool,
    /// 用户是否经 `App::bg` 显式指定了窗口背景（是 → 固定色；否 → 清屏色随主题
    /// palette.bg 热切换，修"切暗色主题后清屏仍是亮色底"）。
    bg_explicit: bool,
    /// 运行期热键操作队列（`hotkey_rc` 句柄写入、UiHost 中转、平台消费）。
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
                topmost: false,
                on_ready: None,
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

    /// 窗口置顶：始终浮于其他窗口之上（如系统提示弹框）。
    pub fn topmost(mut self) -> Self {
        self.cfg.topmost = true;
        self
    }

    /// 窗口创建完成、**首次显示前**的回调。参数为平台句柄数值（win32=HWND、macOS=NSWindow
    /// 指针），用于定位窗口、调整样式后自行显示，避免"先默认显示再跳位"的闪现。
    /// 与 `start_hidden` 搭配时须在回调内自行显示窗口。
    pub fn on_ready(mut self, f: impl FnMut(isize) + 'static) -> Self {
        self.cfg.on_ready = Some(Box::new(f));
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
    pub fn theme(mut self, t: Theme) -> Self {
        // 尊重 App::bg 的显式指定：`.bg(c).theme(t)` 与 `.theme(t).bg(c)` 结果一致。
        if !self.bg_explicit {
            self.cfg.bg = t.palette.bg;
        }
        // 已有运行期句柄时同步初值，保证 theme()/theme_handle() 任意调用序结果一致。
        if let Some(h) = &self.theme_src {
            *h.inner.borrow_mut() = Rc::new(t.clone());
        }
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
    /// let hk = app.hotkey_rc(Hotkey::new(Key::Char('D')).ctrl().alt(), |ctx| ctx.show_window());
    /// // 之后某个按钮回调里：hk.rebind(Hotkey::new(Key::Char('J')).ctrl());
    /// ```
    pub fn hotkey_rc(
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
    pub fn on_close_request(mut self, f: impl FnMut() -> bool + 'static) -> Self {
        self.close_handler = Some(Box::new(f));
        self
    }

    pub fn run(mut self) {
        // 窗口会被隐藏（启动即隐 / 关闭转隐）却无任何唤起途径 = 用户再也看不到窗口，
        // 只能去任务管理器结束进程。在 run() 而非各 setter 里查：tray/hotkey 可能在其后才链上。
        debug_assert!(
            !(self.cfg.start_hidden || self.hide_on_close)
                || self.cfg.tray.is_some()
                || !self.cfg.hotkeys.is_empty()
                || self.cfg.on_ready.is_some(),
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

    /// 注册 typed 消息通道。`on_message` 在 UI 线程调用（可写 Rc 状态）。
    /// 返回的 `Sender` 可 Clone 到任意后台线程；`send` 唤醒 UI 一帧。
    pub fn channel<Msg: Send + 'static>(
        &mut self,
        on_message: impl FnMut(Msg) + 'static,
    ) -> Sender<Msg> {
        let waker = self.shared_waker();
        let (tx, pump) = new_channel(waker, on_message);
        self.pumps.push(pump);
        tx
    }

    /// 注册 UI 线程定时回调（平台定时器，间隔内零 CPU）。可多次调用。
    pub fn on_interval(mut self, every: Duration, cb: impl FnMut() + 'static) -> Self {
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

// ---- 触摸惯性滑动（fling）----

/// 每 ms 速度保留系数（指数摩擦）。0.996 ≈ 衰减常数 0.004/ms，松手后约 1s 内停下。
const FLING_FRICTION: f32 = 0.996;
/// 启动惯性的最小释放速度，比较对象是 `vy`（**物理像素/ms**）；低于此视为缓慢拖放，不滑。
const FLING_TRIGGER: f32 = 0.25;
/// 停止阈值，比较对象是 `Fling::vel`（**逻辑像素/ms**，与触发阈值差一个 scale）；
/// 速度低于此即结束（约 <0.3px/帧@60）。
const FLING_STOP: f32 = 0.02;
/// 两帧间隔超过此值（ms）视为长停滞（最小化、卡顿、后台恢复）→ 结算惯性，避免巨跳。
const FLING_STALL_MS: u64 = 100;
/// 撞界回弹冲量增益（ms）：越界偏移 ≈ 撞界速度 × 此值（逻辑像素/ms × ms = 像素）。
const BOUNCE_GAIN: f32 = 22.0;
/// 越界偏移上限（逻辑像素）：保证"轻微缓冲"而非大幅橡皮筋。
const MAX_BOUNCE: f32 = 26.0;
/// 回弹每 ms 衰减系数：0.98 ≈ 150ms 内弹回归零，短促不拖沓。
const BOUNCE_DECAY: f32 = 0.98;

/// 惯性滑动相位。
#[derive(Clone, Copy, PartialEq)]
enum FlingPhase {
    /// 滑行：按速度推进 scroll_y、摩擦衰减。
    Glide,
    /// 回弹：撞界后短暂越界偏移弹回归零。
    Bounce,
}

/// 进行中的惯性滑动状态。
struct Fling {
    /// 目标滚动容器节点。
    node: NodeId,
    /// 当前相位（滑行/回弹）。
    phase: FlingPhase,
    /// scroll_y 速度（**逻辑像素/ms**）；正=继续增大 scroll_y（内容上移）。
    vel: f32,
    /// 回弹越界偏移（逻辑像素，Bounce 相位用）；正=顶部回弹、负=底部回弹。
    over: f32,
    /// 亚像素累积，避免逐帧取整丢失。
    residual: f32,
    /// 上次步进时的帧时钟（ms）；None=尚未步进（首帧用标称帧长起步，
    /// 避免借用 fling 前可能陈旧的渲染时钟得到巨 dt）。
    last_ms: Option<u64>,
}

/// 菜单滚动条鼠标拖拽状态。
struct MenuScrollbarDrag {
    /// 正在拖拽的面板层级下标。
    level: usize,
    /// 拖拽起始的鼠标 y（逻辑坐标）。
    start_y: i32,
    /// 拖拽起始时的 scroll 偏移。
    start_scroll: i32,
    /// 可滑动轨道高度（面板高 - 上下 padding）。
    track_h: f32,
    /// 拖拽起始时的滑块高度（同帧渲染几何）。
    thumb_h: f32,
}

/// 控件树交互宿主：渲染 + 事件分发 + 焦点管理。
struct UiHost {
    tree: Tree,
    engine: PlatformTextEngine,
    hover: Option<NodeId>,
    capture: Option<NodeId>,
    focus: Option<NodeId>,
    focus_order: Vec<NodeId>,
    close: bool,
    /// DPI 缩放因子（逻辑→物理）。
    scale: f32,
    /// 焦点环是否可见：键盘 Tab 导航时 true，鼠标聚焦时 false。
    focus_visible: bool,
    /// 上一帧的模态作用域（`Tree::topmost_modal`）。与本帧比较以侦测对话框
    /// 弹出/关闭/换层，据以移交焦点（见 `sync_modal_focus`）。
    modal_scope: Option<NodeId>,
    /// 进入模态前的焦点，退出时归还。嵌套对话框只记最外那次进入。
    focus_before_modal: Option<NodeId>,
    /// 活动的上下文菜单浮层（None=无）。
    menu: Option<ContextMenu>,
    /// 最近一帧的逻辑窗口尺寸（菜单弹出位置钳制用）。
    logical_size: Size,
    /// 活动主题快照（每帧从 theme_src 刷新，注入到线程局部供控件读取）。
    theme: Rc<Theme>,
    /// 运行期主题源：热切换时下一帧 render 据此刷新 theme。
    theme_src: ThemeHandle,
    /// 单调起点，用于动画相位时钟。
    start: std::time::Instant,
    /// 触摸平移的亚像素残差（物理→逻辑取整丢失部分累积，避免高 DPI 细微平移发黏）。
    pan_residual: f32,
    /// 触摸惯性滑动状态（None=无）。
    fling: Option<Fling>,
    /// 待执行的窗口操作（自定义标题栏按钮触发，平台分发后轮询执行）。
    pending_window_op: Option<WindowOp>,
    /// 待执行的原生文件对话框请求（平台在事件分发完全返回、OS 捕获同步后再执行）。
    pending_dialog: Option<DialogRequest>,
    /// 最近一次指针位置（逻辑坐标），用于悬停提示浮层定位。
    hover_pos: Point,
    /// 当前悬停起始时刻（ms，单调时钟）。悬停节点变化或点击时复位；
    /// 渲染据 `now - hover_since >= TOOLTIP_DELAY_MS` 决定是否弹出提示。
    hover_since_ms: u64,
    /// 点击后抑制提示，直到指针再次移动（避免点完控件原地又弹出盖住它）。
    tooltip_suppressed: bool,
    /// 活动的轻提示浮层堆栈（先进先出，超过 `TOAST_MAX` 丢最旧）：居中显示、淡入淡出、定时消失。
    toasts: Vec<ToastState>,
    /// 每帧渲染重算的命中矩形缓存，与 `toasts` 同序：`(面板矩形, ✕ 按钮矩形)`（逻辑坐标）。
    toast_rects: Vec<(Rect, Rect)>,
    /// 窗口背景色（与平台 fill 同色）：局部重绘的子缓冲按此填底，重建脏区与全窗一致。
    bg: Color,
    /// 清屏色是否随主题 palette.bg 热切换（未经 `App::bg` 显式固定时为 true）。
    bg_follows_theme: bool,
    /// 运行期热键操作队列（HotkeyHandle 写入；平台经 `take_hotkey_ops` 消费）。
    hotkey_ops: Rc<RefCell<Vec<(usize, crate::event::HotkeyOp)>>>,
    /// 持久后备缓冲（物理像素，整窗）：保留上一全窗帧，供局部帧重建未变区域。
    back: Option<Pixmap>,
    /// 上一帧累积的动画脏区（逻辑坐标）：下一动画帧据此局部重绘；None=下一帧需全窗。
    pending_damage: Option<Rect>,
    /// 交互事件累积的失效区域（逻辑坐标）：下一帧与动画脏区并集后决定局部/整窗。
    event_damage: Option<Rect>,
    /// 本帧需重排（点击/按键后置位）：render 先 layout_root，再以结构签名判定是否升级整窗。
    needs_relayout: bool,
    /// 上一帧的结构签名（可见性+布局）；与重排后签名比对，变则升级整窗。
    last_layout_sig: u64,
    /// `last_layout_sig` 是否已就绪（首帧布局后置真）。
    sig_valid: bool,
    /// 强制本帧全窗重绘（输入/结构/尺寸变更触发）。
    needs_full: bool,
    /// 测试钩子：上一帧是否走了整窗路径（验证交互是否成功局部重绘）。
    #[cfg(test)]
    last_frame_full: bool,
    /// 一次「按下关闭浮层」后，吞掉随之而来的 Up：避免该 Up 下发到控件树重新激活
    /// 浮层下方控件（典型：下拉按钮点一下又弹一遍——Down 关、Up 再开）。
    swallow_up: bool,
    /// 菜单滚动条拖拽状态（None=无）。
    menu_scrollbar_drag: Option<MenuScrollbarDrag>,
    /// 跨线程通道的排空回调：渲染前在 UI 线程依次调用，把后台数据写入控件状态。
    pumps: Vec<Box<dyn FnMut()>>,
    /// 定时器回调列表（与 interval_durs 下标对应）。
    interval_cbs: Vec<Box<dyn FnMut()>>,
    /// 定时器间隔列表（平台据此注册 SetTimer/NSTimer）。
    interval_durs: Vec<std::time::Duration>,
    /// 帧耗时浮层开关（环境变量 WINDUI_FPS 非空时开启）。
    show_fps: bool,
    /// 关闭请求拦截器：返回 true 允许关闭，false 取消。None 时默认允许。
    close_handler: Option<Box<dyn FnMut() -> bool>>,
    /// 关闭请求转为隐藏窗口（常驻托盘类应用）。
    hide_on_close: bool,
}

/// 脏区四周外扩的抗锯齿余量（逻辑像素）：覆盖滑块边缘 AA 与子像素取整，杜绝残影。
const DAMAGE_MARGIN: i32 = 2;

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
            self.needs_full = true;
            return false;
        }
        // 无对话框时询问 close_handler，默认允许关闭。
        let allowed = self.close_handler.as_mut().map(|h| h()).unwrap_or(true);
        if allowed && self.hide_on_close {
            self.pending_window_op = Some(WindowOp::Hide);
            return false;
        }
        allowed
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
        pumps: Vec<Box<dyn FnMut()>>,
        intervals: Vec<(std::time::Duration, Box<dyn FnMut()>)>,
        close_handler: Option<Box<dyn FnMut() -> bool>>,
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
            focus: None,
            focus_order: Vec::new(),
            close: false,
            scale: 1.0,
            focus_visible: false,
            modal_scope: None,
            focus_before_modal: None,
            menu: None,
            logical_size: Size::new(0, 0),
            theme,
            theme_src,
            start: std::time::Instant::now(),
            pan_residual: 0.0,
            fling: None,
            pending_window_op: None,
            pending_dialog: None,
            hover_pos: Point::new(0, 0),
            hover_since_ms: 0,
            tooltip_suppressed: false,
            toasts: Vec::new(),
            toast_rects: Vec::new(),
            bg,
            bg_follows_theme,
            hotkey_ops,
            back: None,
            pending_damage: None,
            event_damage: None,
            needs_relayout: false,
            last_layout_sig: 0,
            sig_valid: false,
            needs_full: true,
            #[cfg(test)]
            last_frame_full: false,
            swallow_up: false,
            menu_scrollbar_drag: None,
            pumps,
            interval_cbs,
            interval_durs,
            show_fps: std::env::var("WINDUI_FPS").is_ok_and(|v| v != "0" && !v.is_empty()),
            close_handler,
            hide_on_close,
        }
    }

    /// 消费一次分发的失效请求：`Rect` 累积为局部脏区，`Layout`/`Full` 升级为整窗。
    /// （Layer 1：`Layout` 暂等价整窗，精确子树重排留待 Layer 2。）
    fn apply_damage(&mut self, d: DamageReq) {
        match d {
            DamageReq::Rect(r) => {
                self.event_damage = Some(match self.event_damage {
                    Some(e) => e.union(&r),
                    None => r,
                });
            }
            DamageReq::Layout(_) | DamageReq::Full => self.needs_full = true,
            DamageReq::None => {}
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

    /// 结束惯性滑动并复位目标节点的越界回弹偏移（打断/取消/重启时必经，
    /// 否则 Bounce 相位中途清除会残留 over_scroll 使内容卡偏）。返回此前是否在滑动。
    fn clear_fling(&mut self) -> bool {
        match self.fling.take() {
            Some(f) => {
                self.tree.set_over_scroll(f.node, 0);
                true
            }
            None => false,
        }
    }

    /// 步进惯性滑动一帧：Glide 按速度推进 scroll_y、摩擦衰减，撞界转 Bounce；
    /// Bounce 短暂越界偏移弹回归零。仍在进行时请求下一帧重绘。
    fn step_fling(&mut self, now_ms: u64) {
        let Some(f) = self.fling.as_ref() else { return };
        let (node, phase, last) = (f.node, f.phase, f.last_ms);
        // 首帧用标称帧长起步；其后按真实间隔，长停滞（最小化/卡顿）直接结算防巨跳。
        let dt = match last {
            None => 16,
            Some(prev) => {
                let raw = now_ms.saturating_sub(prev);
                if raw > FLING_STALL_MS {
                    self.tree.set_over_scroll(node, 0);
                    self.fling = None;
                    return;
                }
                raw.min(64)
            }
        } as f32;
        match phase {
            FlingPhase::Glide => {
                let f = self.fling.as_mut().unwrap();
                f.last_ms = Some(now_ms);
                f.vel *= FLING_FRICTION.powf(dt);
                let advance = f.vel * dt + f.residual;
                let delta = advance.trunc() as i32;
                f.residual = advance - delta as f32;
                let vel = f.vel;
                // 推进并检测撞界（scroll_y 始终钳制；clamp 改变值即撞界）。
                let hit = match self.tree.scroll_range(node) {
                    Some((cur, max)) => {
                        let next = cur + delta;
                        let clamped = next.clamp(0, max);
                        self.tree.set_scroll_y(node, clamped);
                        clamped != next
                    }
                    None => {
                        self.fling = None; // 节点消失（结构变更）→ 结束
                        return;
                    }
                };
                if hit {
                    // 撞界 → 按撞界速度给一个小幅越界偏移，转入回弹。
                    let impulse = (-vel * BOUNCE_GAIN).clamp(-MAX_BOUNCE, MAX_BOUNCE);
                    if impulse.abs() < 1.0 {
                        self.tree.set_over_scroll(node, 0);
                        self.fling = None;
                    } else {
                        self.tree.set_over_scroll(node, impulse.round() as i32);
                        let f = self.fling.as_mut().unwrap();
                        f.phase = FlingPhase::Bounce;
                        f.over = impulse;
                        crate::anim::request_repaint();
                    }
                } else if vel.abs() < FLING_STOP {
                    self.fling = None;
                } else {
                    crate::anim::request_repaint();
                }
            }
            FlingPhase::Bounce => {
                let f = self.fling.as_mut().unwrap();
                f.last_ms = Some(now_ms);
                f.over *= BOUNCE_DECAY.powf(dt);
                let over = f.over;
                if over.abs() < 0.5 {
                    self.tree.set_over_scroll(node, 0);
                    self.fling = None;
                } else {
                    self.tree.set_over_scroll(node, over.round() as i32);
                    crate::anim::request_repaint();
                }
            }
        }
    }

    /// 测量一组菜单项所需面板宽度（图标列 + 标签 + 尾随快捷键/箭头）及是否含图标列。
    fn level_width(&mut self, items: &[MenuItem], min_width: i32) -> (i32, bool) {
        let has_icons = items.iter().any(|it| it.icon.is_some());
        let mut max_label = 0;
        let mut max_trail = 0;
        for it in items {
            if it.separator {
                continue;
            }
            max_label = max_label.max(
                self.engine
                    .measure(&it.label, &crate::text::TextStyle::new(MENU_FONT), None)
                    .w,
            );
            if let Some(sub) = &it.subtitle {
                max_label = max_label.max(
                    self.engine
                        .measure(sub, &crate::text::TextStyle::new(MENU_FONT - 2.5), None)
                        .w,
                );
            }
            let tw = if !it.submenu.is_empty() {
                10
            } else if let Some(s) = &it.shortcut {
                self.engine
                    .measure(s, &crate::text::TextStyle::new(MENU_FONT - 2.0), None)
                    .w
            } else if it.checked {
                12
            } else {
                0
            };
            let mut total = tw;
            if let Some((text, _)) = &it.badge {
                let bw = self
                    .engine
                    .measure(text, &crate::text::TextStyle::new(12.0), None)
                    .w
                    + 2 * BADGE_PAD_X;
                total += if total > 0 { MENU_GAP } else { 0 } + bw;
            }
            if it.trailing_icon.is_some() {
                total += if total > 0 { MENU_GAP } else { 0 } + MENU_ICON_W;
            }
            max_trail = max_trail.max(total);
        }
        let icon_w = if has_icons { MENU_ICON_W + MENU_GAP } else { 0 };
        let trail_w = if max_trail > 0 {
            MENU_TRAIL_GAP + max_trail
        } else {
            0
        };
        let w = (MENU_PAD_X + icon_w + max_label + trail_w + MENU_PAD_X)
            .max(MENU_MIN_W)
            .max(min_width);
        (w, has_icons)
    }

    /// 构造一级面板：锚点 (ax, ay) 为期望左上角；越窗右缘时按 `flip_right` 左翻；
    /// 越窗下缘时：若 `anchor_top` 有值（下拉控件顶部 y），优先向上翻转（菜单底对齐控件顶），
    /// 保证控件自身不被遮挡；否则退化为向上钳制。
    fn build_level(
        &mut self,
        items: Vec<MenuItem>,
        ax: i32,
        ay: i32,
        min_width: i32,
        flip_right: Option<i32>,
        anchor_top: Option<i32>,
    ) -> MenuLevel {
        let (w, has_icons) = self.level_width(&items, min_width);
        let body: i32 = items.iter().map(menu_item_height).sum();
        let content_h = body + 2 * MENU_VPAD;
        // 面板可视高度：不超过 MENU_MAX_H，也不超过窗口高的 3/4。
        let ws = self.logical_size;
        let max_h = MENU_MAX_H.min(if ws.h > 0 { ws.h * 3 / 4 } else { MENU_MAX_H });
        let h = content_h.min(max_h);
        let mut x = ax;
        let mut y = ay;
        // MENU_EDGE_MARGIN：弹层与窗口四边保留距离，避免滚动条落入 resize 边框区。
        let em = if ws.w > 0 { MENU_EDGE_MARGIN } else { 0 };
        if ws.w > 0 && x + w > ws.w - em {
            x = match flip_right {
                Some(parent_left) => (parent_left - w).max(em),
                None => (ws.w - w - em).max(em),
            };
        }
        x = x.max(em);
        if ws.h > 0 && y + h > ws.h - em {
            if let Some(top) = anchor_top {
                // 下拉控件：优先向上翻转（菜单底对齐控件顶），避免遮住控件。
                // 若上方空间也不足，取上下哪侧空间大的一侧并钳制。
                let y_above = top - h;
                if y_above >= em {
                    y = y_above;
                } else {
                    let space_below = ws.h - ay;
                    let space_above = top;
                    if space_above >= space_below {
                        y = em; // 上方更大，贴顶留边
                    } else {
                        y = (ws.h - h - em).max(em); // 下方更大，贴底留边
                    }
                }
            } else {
                y = (ws.h - h - em).max(em);
            }
        }
        y = y.max(em);
        // 计算初始滚动偏移：使 checked 项（当前选中）居中于可视区域。
        let initial_scroll = if content_h > h {
            let mut offset = MENU_VPAD;
            let mut result = 0i32;
            for it in &items {
                let ih = menu_item_height(it);
                if it.checked {
                    result = offset + ih / 2 - h / 2;
                    break;
                }
                offset += ih;
            }
            result.clamp(0, (content_h - h).max(0))
        } else {
            0
        };
        MenuLevel {
            items,
            rect: Rect::new(x, y, w, h),
            hover: None,
            has_icons,
            spawn: None,
            content_h,
            scroll: initial_scroll,
        }
    }

    /// 打开上下文菜单（根级）。
    fn open_menu(&mut self, req: crate::event::MenuRequest, target: NodeId) {
        let level = self.build_level(
            req.items,
            req.pos.x,
            req.pos.y,
            req.min_width,
            None,
            req.anchor_top,
        );
        self.menu = Some(ContextMenu {
            levels: vec![level],
            target,
            rebuild: req.rebuild,
        });
    }

    /// 结构变化后按当前指针位置重新求值 hover：合成一个 Move 事件复用既有的 Enter/Leave
    /// 逻辑——旧 hover 节点若被新浮层遮住会收到 Leave（清掉残留高亮），指针下的新节点收到
    /// Enter。修正"模态弹出/关闭、切页等在光标静止时改变命中节点导致 hover 卡住"。
    /// 菜单浮层有独立命中逻辑，激活时跳过。
    fn resync_hover_after_relayout(&mut self) {
        if self.menu.is_some() {
            return;
        }
        let mut hover = self.hover;
        let mut capture = self.capture;
        let _ = self.tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, self.hover_pos, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        self.hover = hover;
        self.capture = capture;
    }

    /// 弹出/替换轻提示：以当前单调时钟为起点，强制整窗重绘叠加浮层。
    /// 后续帧会持续推进淡入淡出并在过期后自动清除（见 render 中的浮层段）。
    fn show_toast(&mut self, req: ToastRequest) {
        self.push_toast(req);
        self.needs_full = true;
    }
    /// 上屏 on_update（响应式相位）里累积的 toast——该相位不经 DispatchResult，
    /// 由 `Tree` 暂存、宿主在每次 layout 后取走（否则 toast_sink 等发的提示被吞）。
    fn flush_pending_toasts(&mut self) {
        for req in self.tree.take_pending_toasts() {
            self.show_toast(req);
        }
    }
    /// 压入一条 toast；超过上限丢最旧。
    fn push_toast(&mut self, req: ToastRequest) {
        let now_ms = self.start.elapsed().as_millis() as u64;
        if self.toasts.len() >= TOAST_MAX {
            self.toasts.remove(0);
        }
        self.toasts.push(ToastState {
            req,
            shown_at_ms: now_ms,
            paused_at_ms: None,
            paused_total_ms: 0,
        });
    }
    /// 移除已过期（Task 3 先提供，供 render 调用）。
    fn retain_live_toasts(&mut self, now_ms: u64) {
        self.toasts.retain(|t| !t.expired(now_ms));
    }

    /// 按指针位置更新悬停路径：设置所在层悬停项，并按需展开/收起其级联子菜单。
    fn menu_hover_update(&mut self, pos: Point) -> bool {
        let Some(k) = self.menu.as_ref().and_then(|m| m.level_at(pos)) else {
            return false;
        };
        let item_idx = self.menu.as_ref().unwrap().levels[k].item_at(pos);
        let mut changed = false;
        {
            let m = self.menu.as_mut().unwrap();
            if m.levels[k].hover != item_idx {
                m.levels[k].hover = item_idx;
                changed = true;
            }
        }
        // 悬停项是否有可展开的子菜单（锚点计算与压栈见 menu_spawn_submenu，键盘 → 共用）。
        let spawnable = {
            let lvl = &self.menu.as_ref().unwrap().levels[k];
            matches!(item_idx, Some(i) if !lvl.items[i].submenu.is_empty() && lvl.items[i].enabled)
        };
        let existing_spawn = self
            .menu
            .as_ref()
            .and_then(|m| m.levels.get(k + 1).map(|l| l.spawn));
        match item_idx {
            Some(i) if spawnable => {
                if existing_spawn == Some(Some(i)) {
                    // 该子菜单已展开：仅收起更深层。
                    let m = self.menu.as_mut().unwrap();
                    if m.levels.len() > k + 2 {
                        m.levels.truncate(k + 2);
                        changed = true;
                    }
                } else {
                    changed |= self.menu_spawn_submenu(k, i);
                }
            }
            _ => {
                // 悬停项无子菜单：收起本层之下的所有子菜单。
                let m = self.menu.as_mut().unwrap();
                if m.levels.len() > k + 1 {
                    m.levels.truncate(k + 1);
                    changed = true;
                }
            }
        }
        changed
    }

    /// 关闭浮层菜单，并标记下一帧整窗重绘。
    ///
    /// 浮层画在控件树之上、不属于任何节点，故不在任何控件的交互脏区内。而 `render` 的
    /// `overlay` 判定问的是"**本帧**有没有浮层"——关闭帧已经没有了，此时若恰好存在一小块
    /// 脏区（如打开菜单时清 hover 触发的边框补间仍在跑），就会走局部重绘，只擦那一小块，
    /// 面板像素留在屏上。关闭浮层必经此处，勿直接写 `self.menu = None`。
    fn close_menu(&mut self) {
        self.menu = None;
        self.needs_full = true;
    }

    /// 菜单激活时处理指针；返回是否需重绘。
    fn handle_menu_pointer(&mut self, ev: PointerEvent) -> bool {
        match ev.kind {
            PointerKind::Move => {
                // 滚动条拖拽中：按拖拽量更新 scroll，不做悬停高亮。
                if let Some(drag) = &self.menu_scrollbar_drag {
                    let dy = ev.pos.y - drag.start_y;
                    let travel = (drag.track_h - drag.thumb_h).max(1.0);
                    let level_idx = drag.level;
                    let start_scroll = drag.start_scroll;
                    if let Some(m) = self.menu.as_mut() {
                        if let Some(level) = m.levels.get_mut(level_idx) {
                            let max_sc = level.max_scroll();
                            let new_scroll =
                                start_scroll + (dy as f32 * max_sc as f32 / travel).round() as i32;
                            level.scroll = new_scroll.clamp(0, max_sc);
                        }
                    }
                    return true;
                }
                self.menu_hover_update(ev.pos)
            }
            PointerKind::Down => {
                // 滚动条命中检测：面板右侧 10px 区域内且该面板有滚动内容。
                if let Some(k) = self.menu.as_ref().and_then(|m| m.level_at(ev.pos)) {
                    let level = &self.menu.as_ref().unwrap().levels[k];
                    let r = level.rect;
                    if level.content_h > r.h && ev.pos.x >= r.right() - 16 {
                        // 命中滚动条（命中区 16px）：开始拖拽，不关闭菜单也不触发项。
                        let track_h = (r.h - 8) as f32;
                        let ratio = r.h as f32 / level.content_h as f32;
                        let thumb_h = (track_h * ratio).max(20.0);
                        self.menu_scrollbar_drag = Some(MenuScrollbarDrag {
                            level: k,
                            start_y: ev.pos.y,
                            start_scroll: level.scroll,
                            track_h,
                            thumb_h,
                        });
                        self.swallow_up = true;
                        return true;
                    }
                }
                // 常规 Down：关闭菜单（命中叶子项执行后关 / 点外关）。
                self.swallow_up = true;
                let Some(k) = self.menu.as_ref().and_then(|m| m.level_at(ev.pos)) else {
                    self.close_menu(); // 点击所有面板之外：关闭
                    return true;
                };
                // 同步悬停路径（保证子菜单按当前指针展开）。
                self.menu_hover_update(ev.pos);
                // 尾随可点击图标优先命中：独立于主项 action，点击只触发图标自己的回调
                // （如"删除该项"），不触发本项被选中。
                let trailing_hit = self
                    .menu
                    .as_ref()
                    .and_then(|m| m.levels[k].trailing_icon_at(ev.pos))
                    .and_then(|i| {
                        self.menu.as_ref().unwrap().levels[k].items[i]
                            .on_trailing_click
                            .clone()
                    });
                if let Some(f) = trailing_hit {
                    self.close_menu();
                    f();
                    return true;
                }
                // 命中项：叶子执行并关闭；子菜单父项/禁用项保持展开。
                let hit = self.menu.as_ref().and_then(|m| {
                    let lvl = &m.levels[k];
                    lvl.item_at(ev.pos).map(|i| lvl.items[i].clone())
                });
                if let Some(item) = hit {
                    return self.activate_menu_item(item);
                }
                true
            }
            PointerKind::Up => {
                // 结束滚动条拖拽（若有）。
                self.menu_scrollbar_drag = None;
                true
            }
            PointerKind::Wheel(delta) => {
                // 滚轮在菜单面板内滚动：delta>0=上滚（内容下移，scroll 减小）。
                if let Some(k) = self.menu.as_ref().and_then(|m| m.level_at(ev.pos)) {
                    let level = &mut self.menu.as_mut().unwrap().levels[k];
                    let step = (delta.abs() / 3).max(MENU_ITEM_H);
                    let dir = if delta > 0 { -step } else { step };
                    level.scroll = (level.scroll + dir).clamp(0, level.max_scroll());
                }
                true
            }
            _ => true, // 其余事件吞掉，避免穿透到下层
        }
    }

    /// 执行一个菜单项：粘滞项原地刷新勾选态、菜单留在原处，其余关闭菜单后执行。
    /// 非 actionable（分隔线 / 禁用 / 子菜单父项）为空操作。
    ///
    /// 指针命中与键盘回车共用此处——两条入口各写一份迟早会分叉（粘滞项、SendKey
    /// 的关闭时机都藏在这里）。
    fn activate_menu_item(&mut self, item: MenuItem) -> bool {
        if !item.is_actionable() {
            return true;
        }
        // 粘滞项（复选菜单的开关）：执行后菜单留在原地并刷新勾选态，可连点多个开关。
        if item.stay_open {
            if let MenuAction::Run(f) = item.action {
                f();
            }
            if let Some(m) = self.menu.as_mut() {
                m.refresh_items();
            }
            return true;
        }
        let target = self.menu.as_ref().map(|m| m.target);
        self.close_menu();
        match item.action {
            MenuAction::SendKey(key) => {
                if let Some(t) = target {
                    let res = self.tree.dispatch_key(key, Some(t));
                    if res.close {
                        self.apply_close_intent();
                    }
                }
            }
            MenuAction::Run(f) => f(),
        }
        true
    }

    /// 在第 `k` 级的第 `i` 项上展开子菜单：截断更深层后压入新级。返回是否压入。
    /// 锚点为父项右缘、顶部对齐该项；鼠标悬停展开与键盘 → 共用此处。
    fn menu_spawn_submenu(&mut self, k: usize, i: usize) -> bool {
        let Some(m) = self.menu.as_ref() else {
            return false;
        };
        let Some(lvl) = m.levels.get(k) else {
            return false;
        };
        let Some(it) = lvl.items.get(i) else {
            return false;
        };
        if it.submenu.is_empty() || !it.enabled {
            return false;
        }
        let items = it.submenu.clone();
        let (top, _) = lvl.item_rows()[i];
        let (ax, ay, parent_left) = (lvl.rect.right(), top - MENU_VPAD, lvl.rect.x);
        if let Some(m) = self.menu.as_mut() {
            m.levels.truncate(k + 1);
        }
        let mut child = self.build_level(items, ax - 2, ay, 0, Some(parent_left + 2), None);
        child.spawn = Some(i);
        self.menu.as_mut().unwrap().levels.push(child);
        true
    }

    /// 最深一级面板的下标（菜单必然非空时才调用）。
    fn menu_top_level(&self) -> Option<usize> {
        self.menu
            .as_ref()
            .and_then(|m| m.levels.len().checked_sub(1))
    }

    /// 设置第 `k` 级高亮项：收起其下已展开的子菜单（同鼠标移开），并滚进可视区。
    /// 有子菜单的项不在此自动展开——键盘上由 → 显式进入（同 Windows 菜单）。
    fn menu_set_hover(&mut self, k: usize, i: usize) {
        if let Some(m) = self.menu.as_mut() {
            if m.levels.len() > k + 1 {
                m.levels.truncate(k + 1);
            }
            if let Some(lvl) = m.levels.get_mut(k) {
                lvl.hover = Some(i);
            }
        }
        self.menu_scroll_into_view(k, i);
    }

    /// 把第 `k` 级的第 `i` 项滚进面板可视区（已在视口内则不动）。
    /// 内容坐标 `off` 与 `MenuLevel::item_rows` 同源：`MENU_VPAD` + 前序项高之和，
    /// 屏幕 y = `rect.y + off - scroll`，故可视范围即 `[scroll, scroll + rect.h]`。
    fn menu_scroll_into_view(&mut self, k: usize, i: usize) {
        let Some(m) = self.menu.as_mut() else {
            return;
        };
        let Some(lvl) = m.levels.get_mut(k) else {
            return;
        };
        let Some(h) = lvl.items.get(i).map(menu_item_height) else {
            return;
        };
        let off = MENU_VPAD + lvl.items.iter().take(i).map(menu_item_height).sum::<i32>();
        let max_sc = lvl.max_scroll();
        if off < lvl.scroll {
            lvl.scroll = off;
        } else if off + h > lvl.scroll + lvl.rect.h {
            lvl.scroll = off + h - lvl.rect.h;
        }
        lvl.scroll = lvl.scroll.clamp(0, max_sc);
    }

    /// ↑/↓：最深层内移动高亮，跳过分隔线与禁用项，到头循环。
    ///
    /// 尚无高亮时落到 checked 项（下拉的当前选中），没有则落到首/末项——让键盘用户
    /// 先看清起点在哪，而不是凭空跳走一格。
    fn menu_move_hover(&mut self, forward: bool) -> bool {
        let Some(k) = self.menu_top_level() else {
            return true;
        };
        let lvl = &self.menu.as_ref().unwrap().levels[k];
        let sel: Vec<usize> = (0..lvl.items.len())
            .filter(|&i| menu_item_selectable(&lvl.items[i]))
            .collect();
        if sel.is_empty() {
            return true;
        }
        let target = match lvl.hover.and_then(|h| sel.iter().position(|&i| i == h)) {
            Some(p) => {
                let step = if forward { 1 } else { sel.len() - 1 };
                sel[(p + step) % sel.len()]
            }
            None => sel
                .iter()
                .copied()
                .find(|&i| lvl.items[i].checked)
                .unwrap_or(if forward { sel[0] } else { sel[sel.len() - 1] }),
        };
        self.menu_set_hover(k, target);
        true
    }

    /// Home/End：跳到本级首个/末个可选项。
    fn menu_jump_hover(&mut self, first: bool) -> bool {
        let Some(k) = self.menu_top_level() else {
            return true;
        };
        let lvl = &self.menu.as_ref().unwrap().levels[k];
        let target = if first {
            lvl.items.iter().position(menu_item_selectable)
        } else {
            lvl.items.iter().rposition(menu_item_selectable)
        };
        if let Some(i) = target {
            self.menu_set_hover(k, i);
        }
        true
    }

    /// →：进入当前高亮项的子菜单，高亮落到子菜单首个可选项。无子菜单则不动。
    fn menu_enter_submenu(&mut self) -> bool {
        let Some(k) = self.menu_top_level() else {
            return true;
        };
        let Some(i) = self.menu.as_ref().unwrap().levels[k].hover else {
            return true;
        };
        if !self.menu_spawn_submenu(k, i) {
            return true;
        }
        let nk = self.menu.as_ref().unwrap().levels.len() - 1;
        let first = self.menu.as_ref().unwrap().levels[nk]
            .items
            .iter()
            .position(menu_item_selectable);
        if let Some(f) = first {
            self.menu_set_hover(nk, f);
        }
        true
    }

    /// ←：收起最深一级回到父级。已在根级则不动（不关闭整个菜单，同 Windows 菜单）。
    fn menu_leave_level(&mut self) -> bool {
        if let Some(m) = self.menu.as_mut() {
            if m.levels.len() > 1 {
                m.levels.pop();
            }
        }
        true
    }

    /// Enter/Space：激活当前高亮项。子菜单父项等同 →（展开而非执行）。
    fn menu_activate_hover(&mut self) -> bool {
        let Some(k) = self.menu_top_level() else {
            return true;
        };
        let m = self.menu.as_ref().unwrap();
        let Some(i) = m.levels[k].hover else {
            return true;
        };
        let Some(item) = m.levels[k].items.get(i).cloned() else {
            return true;
        };
        if !item.submenu.is_empty() {
            return self.menu_enter_submenu();
        }
        self.activate_menu_item(item)
    }

    /// 菜单激活时处理键盘；返回是否需重绘。
    ///
    /// 键盘与指针是菜单的两套并行入口：指针按坐标命中（`handle_menu_pointer`），
    /// 键盘按最深层的 hover 下标走，两者共用 `activate_menu_item` /
    /// `menu_spawn_submenu`。未识别的键一律吞掉——菜单是模态浮层，放行会让按键
    /// 打到被遮住的控件上。
    fn handle_menu_key(&mut self, ev: crate::event::KeyEvent) -> bool {
        if !ev.pressed {
            return true;
        }
        match ev.key {
            Key::Escape => {
                self.close_menu();
                true
            }
            Key::Down => self.menu_move_hover(true),
            Key::Up => self.menu_move_hover(false),
            Key::Home => self.menu_jump_hover(true),
            Key::End => self.menu_jump_hover(false),
            Key::Right => self.menu_enter_submenu(),
            Key::Left => self.menu_leave_level(),
            Key::Enter | Key::Space => self.menu_activate_hover(),
            // Tab 不在菜单里导航焦点：先收起浮层，让焦点回到发起控件。
            Key::Tab => {
                self.close_menu();
                true
            }
            _ => true,
        }
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
            let old = self.focus;
            self.tree.set_focused(Some(f), old);
            self.focus = Some(f);
            match focus_from {
                // 鼠标聚焦不显示焦点环，保持纯鼠标操作的纯净观感。
                FocusSource::Pointer => self.focus_visible = false,
                // 键盘聚焦相反——本来就在键盘导航中。焦点环跨节点变化 → 整窗。
                FocusSource::Keyboard => {
                    self.focus_visible = true;
                    self.needs_full = true;
                }
            }
        } else if let Some(pos) = blur_at {
            // 点在当前焦点控件之外 → 清空焦点（网页 blur 语义：焦点归属由宿主每次按下
            // 重新裁决，而不是"没人认领就维持原样"）。
            if let Some(f) = self.focus {
                if !self.tree.hit_inside(pos, f) {
                    self.tree.set_focused(None, Some(f));
                    self.focus = None;
                    self.focus_visible = false;
                    // 焦点环画在节点框外 1px，而 damage_rect 的额外余量只对 focused 节点
                    // 给足；此刻 focused 已置 false，按脏区走会残留一圈，故整窗。
                    self.needs_full = true;
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
            if let Some(target) = self.focus.or(self.tree.root) {
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

    /// 模态层进出时移交焦点：弹出 → 落到对话框首个可聚焦控件并记下来处；
    /// 关闭 → 还给弹出前那个控件。同网页 `<dialog>.showModal()` 的语义。
    ///
    /// 只在作用域**变化**的那一帧动作，此后用户 Tab 到哪儿就是哪儿——每帧都强制
    /// 聚焦会把焦点粘死在首项上。
    fn sync_modal_focus(&mut self) {
        let scope = self.tree.topmost_modal();
        if scope == self.modal_scope {
            return;
        }
        let was_inside = self.modal_scope.is_some();
        self.modal_scope = scope;
        let target = if scope.is_some() {
            // 进入模态。A→B 的嵌套切换不覆盖来处，B 关掉回到 A 时才不会丢掉最初那个。
            if !was_inside {
                self.focus_before_modal = self.focus;
            }
            self.focus_order.first().copied()
        } else {
            // 退出模态：归还来处（它可能已随结构变更消失，故再验一次）。
            self.focus_before_modal
                .take()
                .filter(|f| self.focus_order.contains(f))
        };
        let old = self.focus;
        self.tree.set_focused(target, old);
        self.focus = target;
        // 焦点环可见性**沿用当前状态**，不因这次代挪而强制打开：鼠标点开的对话框
        // 凭空冒出焦点框很突兀，而键盘用户此前 Tab 过、focus_visible 本就是 true，
        // 焦点照常画得出来。同 :focus-visible 的启发式——聚焦虽是程序性的，判据是
        // 用户最近一次交互用的什么。
    }

    /// Tab 焦点移动（forward=正向）。返回是否变化。
    fn move_focus(&mut self, forward: bool) -> bool {
        if self.focus_order.is_empty() {
            return false;
        }
        let n = self.focus_order.len();
        let cur = self
            .focus
            .and_then(|f| self.focus_order.iter().position(|&x| x == f));
        let next = match cur {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None if forward => 0,
            None => n - 1,
        };
        let nf = Some(self.focus_order[next]);
        let old = self.focus;
        self.tree.set_focused(nf, old);
        self.focus = nf;
        // 新焦点可能在滚动区外（滚出视口的节点仍在焦点环里），滚过去让它露出来。
        // 调用方 Tab 分支已置 needs_full，本帧的全窗路径会重排并钳制新的 scroll_y。
        if let Some(f) = nf {
            self.tree.scroll_into_view(f);
        }
        true
    }

    /// toast 面板命中测试（逻辑坐标）→ 命中条的下标。
    fn toast_hit(&self, p: Point) -> Option<usize> {
        self.toast_rects
            .iter()
            .position(|(panel, _)| panel.contains(p))
    }
    /// toast ✕ 关闭按钮命中测试（逻辑坐标）→ 命中条的下标。
    fn toast_close_hit(&self, p: Point) -> Option<usize> {
        self.toast_rects
            .iter()
            .position(|(_, close)| close.contains(p))
    }

    /// toast 浮层指针交互：命中则消费（悬停暂停 / ✕关闭 / 右键复制）。
    fn handle_toast_pointer(&mut self, ev: crate::event::PointerEvent) -> bool {
        use crate::event::{MenuAction, MenuItem, MouseButton, PointerKind};
        let now_ms = self.start.elapsed().as_millis() as u64;
        // 悬停暂停：逐条按是否命中切换（未命中该条则恢复计时）。
        let hit = self.toast_hit(ev.pos);
        for (i, t) in self.toasts.iter_mut().enumerate() {
            t.set_hover(now_ms, Some(i) == hit);
        }
        if hit.is_some() {
            self.needs_full = true; // 冻结/恢复需重绘
        }
        // 主键按下命中 ✕：移除该条。
        if ev.kind == PointerKind::Down && ev.button == MouseButton::Left {
            if let Some(i) = self.toast_close_hit(ev.pos) {
                self.toasts.remove(i);
                self.needs_full = true;
                self.swallow_up = true; // 吞掉配对 Up
                return true;
            }
        }
        // 右键命中面板：弹「复制内容」菜单。
        if ev.kind == PointerKind::Down && ev.button == MouseButton::Right {
            if let Some(i) = hit {
                let text = self.toasts[i].req.text.clone();
                let item = MenuItem {
                    label: "复制内容".to_string(),
                    action: MenuAction::Run(std::rc::Rc::new(move || {
                        use crate::core::ClipboardProvider;
                        crate::platform::Clipboard.set_text(&text);
                    })),
                    enabled: true,
                    checked: false,
                    icon: None,
                    shortcut: None,
                    separator: false,
                    submenu: Vec::new(),
                    subtitle: None,
                    badge: None,
                    trailing_icon: None,
                    on_trailing_click: None,
                    stay_open: false,
                };
                if let Some(target) = self.focus.or(self.tree.root) {
                    self.open_menu(
                        crate::event::MenuRequest {
                            pos: ev.pos,
                            items: vec![item],
                            min_width: 0,
                            anchor_top: None,
                            rebuild: None,
                        },
                        target,
                    );
                }
                return true;
            }
        }
        // 命中面板（非✕、非右键）：吞掉，避免点穿到下方控件。
        hit.is_some()
    }
}

impl AppHandler for UiHost {
    fn render(&mut self, target: &mut dyn crate::render::RenderTarget, size: Size) {
        // 帧耗时计时（WINDUI_FPS=1 时在左上角显示，用于排查渲染开销）。
        let frame_t0 = std::time::Instant::now();
        // 跨线程消息：渲染前在 UI 线程一次性排空所有通道，把后台数据写入控件状态。
        // 契约：一帧 render 消费所有 pump 的全部积压消息（唤醒合并/批处理）——
        // 多个 channel 共享单一 Waker，勿改成每 pump 独立 wake/独立帧。
        for pump in self.pumps.iter_mut() {
            pump();
        }
        // 从运行期句柄刷新主题快照（热切换下一帧生效），注入线程局部供控件读取。
        self.theme = self.theme_src.current();
        crate::theme::set_current(self.theme.clone());
        // 清屏色随主题（未经 App::bg 显式固定时）：暗色主题下窗口底色同步转暗。
        if self.bg_follows_theme {
            self.bg = self.theme.palette.bg;
        }
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

        // 交互/结构可能改变布局：先重排，再用结构签名判定本次是否仅为局部视觉变化。
        // 签名变（显隐/位移/尺寸，如对话框弹出、切页）→ 影响区域不可局部化 → 升级整窗；
        // 签名不变（打字、按钮、勾选）→ 沿用控件上报的交互脏区走 1ms 局部重绘。
        let mut laid_out = false;
        if self.needs_relayout {
            self.tree.layout_root(logical, &mut self.engine);
            laid_out = true;
            let sig = self.tree.layout_signature();
            if self.sig_valid && sig != self.last_layout_sig {
                self.needs_full = true;
                // 结构变化（模态弹出/关闭、切页等）的两类交互态修正（对齐 Flutter MouseTracker /
                // Qt 模态弹出补发 leave 的做法）：
                // 1) 被隐藏的控件（如关闭它所在的对话框）重置其 hover/press 与补间，避免下次
                //    显示瞬间闪出旧的按下/悬停态；
                self.tree.reset_hidden_interactions();
                // 2) 在光标静止时被新浮层遮住的旧 hover 节点补发 Leave/Enter，清掉残留高亮。
                self.resync_hover_after_relayout();
            }
            self.last_layout_sig = sig;
            self.sig_valid = true;
            self.needs_relayout = false;
        }
        // 响应式相位（本帧 layout 内）可能发出 toast（如 toast_sink 监听 feedback 信号）。
        // 须在重绘决策前上屏：show_toast 置 needs_full 并使 overlay 成立，令新 toast 被绘制，
        // 否则会走局部重绘的 early-return 而漏画。
        self.flush_pending_toasts();

        // 全窗 vs 局部重绘决策：
        // - needs_full（输入/结构/尺寸变更）、后备缓冲缺失/尺寸不符、有浮层、无脏区 → 全窗。
        // - 否则用上一帧动画脏区做局部重绘（仅重画动的那一小块，高 DPI 也稳 60fps）。
        let back_ok = self
            .back
            .as_ref()
            .map(|b| b.width() == size.w as u32 && b.height() == size.h as u32)
            .unwrap_or(false);
        let overlay = self.menu.is_some()
            || !self.toasts.is_empty()
            || (!self.tooltip_suppressed
                && self.hover.and_then(|h| self.tree.node_tooltip(h)).is_some());
        // 下一帧脏区 = 动画脏区（上帧遗留）∪ 交互脏区（事件累积）。
        let damage = match (self.pending_damage.take(), self.event_damage.take()) {
            (Some(a), Some(b)) => Some(a.union(&b)),
            (a, b) => a.or(b),
        };
        // 局部重绘前提：scale 为 0.25 的倍数——4 逻辑像素 ×scale 才为整数，子 pixmap 与全窗帧才
        // 逐像素对齐（否则文字纵向 1px 抖动）。非 25% 倍数缩放（罕见的分数缩放）一律退全窗，
        // 这也使「平台层零改动、各平台始终拿到完整 pixmap」的不变量在任何 scale 下都安全。
        let scale_ok = {
            let q = s * 4.0;
            (q - q.round()).abs() < 1e-3
        };
        // 脏区超过窗口一半 → 退全窗：多控件并集过大时，局部重绘的子 pixmap 分配+合成反而净亏损。
        let damage_small = damage
            .map(|d| {
                let win = self.logical_size.w as i64 * self.logical_size.h as i64;
                win > 0 && (d.w as i64 * d.h as i64) * 2 <= win
            })
            .unwrap_or(false);
        let do_full = self.needs_full
            || !back_ok
            || overlay
            || !scale_ok
            || !damage_small
            || target.as_pixmap().is_none();
        self.needs_full = false;
        #[cfg(test)]
        {
            self.last_frame_full = do_full;
        }

        if !do_full {
            let pixmap = target.as_pixmap().expect("软目标必有 pixmap");
            self.render_partial(pixmap, size, s, damage.unwrap());
            self.pending_damage = next_damage(&mut self.needs_full);
            // 布局动画（高度补间等）请求下一帧重排：走 needs_relayout 正规门，
            // 重排后按结构签名升级整窗并执行 hover 重同步。
            if crate::anim::take_relayout() {
                self.needs_relayout = true;
            }
            if crate::render::prof::enabled() {
                eprintln!(
                    "[prof] partial {:.2}ms  {}",
                    frame_t0.elapsed().as_secs_f64() * 1000.0,
                    crate::render::prof::take_summary()
                );
            }
            return;
        }

        // ---- 全窗重绘：完整布局 + 整树绘制 + 浮层；结果种入后备缓冲供后续局部帧复用。----
        // 重排块已布局过则跳过，避免重复 layout_root。
        if !laid_out {
            self.tree.layout_root(logical, &mut self.engine);
            self.last_layout_sig = self.tree.layout_signature();
            self.sig_valid = true;
        }
        // 全窗路径的这次 layout 也可能有响应式 toast（上面 needs_relayout 未触发时）；
        // 本就走全窗重绘，取走后随本帧 paint 上屏即可。
        self.flush_pending_toasts();
        // 布局后结构稳定，刷新 Tab 焦点顺序。
        self.focus_order = self.tree.focusable_order();
        // 模态层进出时移交焦点。必须在下面的归一化之前——归一化只会把落在框外的
        // 旧焦点抹成 None，抹完就分不清"本该还给谁"了。
        self.sync_modal_focus();
        // 若当前焦点已不在可聚焦集合中（结构变更），归一化为无焦点。
        if let Some(f) = self.focus {
            if !self.focus_order.contains(&f) {
                self.tree.set_focused(None, Some(f));
                self.focus = None;
            }
        }
        self.tree.focus_ring_visible = self.focus_visible;
        // 过期 toast 先清除（需要 &mut self，必须在借用 self.engine 生成 canvas 之前完成）。
        self.retain_live_toasts(now_ms);
        let mut canvas = target.make_canvas(&mut self.engine, s);
        self.tree.paint(&mut *canvas);
        // 悬停提示浮层（菜单激活时不显示）：悬停节点带 tooltip 且停留超过延时则弹出；
        // 未到延时则请求下一帧——鼠标静止后无事件，需靠 anim 续帧推进计时（与不确定进度条同源）。
        if self.menu.is_none() && !self.tooltip_suppressed {
            if let Some(text) = self.hover.and_then(|h| self.tree.node_tooltip(h)) {
                if now_ms.saturating_sub(self.hover_since_ms) < TOOLTIP_DELAY_MS {
                    crate::anim::request_repaint();
                } else {
                    let (pal, tt) = (&self.theme.palette, &self.theme.tooltip);
                    let ts = canvas.measure_text_wrapped(
                        &text,
                        &crate::text::TextStyle::new(TOOLTIP_FONT),
                        tt.max_width(),
                    );
                    let (w, h) = (ts.w + 2 * TOOLTIP_PAD_X, ts.h + 2 * TOOLTIP_PAD_Y);
                    let ws = self.logical_size;
                    let mut x = self.hover_pos.x + TOOLTIP_CURSOR_DX;
                    let mut y = self.hover_pos.y + TOOLTIP_CURSOR_DY;
                    if ws.w > 0 && x + w > ws.w {
                        x = (ws.w - w).max(0);
                    }
                    if ws.h > 0 && y + h > ws.h {
                        y = (self.hover_pos.y - h - 4).max(0); // 下方放不下则翻到指针上方
                    }
                    let corner = tt.corner(&self.theme.metrics);
                    canvas.fill_round_rect(
                        x as f32,
                        y as f32,
                        w as f32,
                        h as f32,
                        corner,
                        &Paint::fill(tt.bg(pal)),
                    );
                    let tr = Rect::new(x + TOOLTIP_PAD_X, y, w - 2 * TOOLTIP_PAD_X, h);
                    canvas.draw_text(
                        &text,
                        tr,
                        tt.text(pal),
                        crate::spec::Align::Start,
                        &crate::text::TextStyle::new(TOOLTIP_FONT),
                    );
                }
            }
        }
        // 轻提示浮层：顶部居中堆叠，单条横向 [图标][文字][✕关闭]，淡入淡出
        // （过期条已在上方清除）。命中矩形逐帧重算写入 toast_rects，供点击测试使用。
        self.toast_rects.clear();
        let ws = self.logical_size;
        let mut y = TOAST_TOP_MARGIN;
        for toast in &self.toasts {
            let alpha = toast.alpha(now_ms);
            let pal = &self.theme.palette;
            let tt = &self.theme.toast;
            let glyph = toast.req.kind.glyph();
            let icon_color = match toast.req.kind {
                crate::event::ToastKind::Info => tt.info(pal),
                crate::event::ToastKind::Success => tt.success(pal),
                crate::event::ToastKind::Error => tt.error(pal),
            };
            let icon_sz = canvas.measure_text(glyph, &crate::text::TextStyle::new(TOAST_ICON_FONT));
            // 面板宽度上限：两侧各留 TOAST_TOP_MARGIN，保证不越窗口边界。
            let panel_max_w = (ws.w - 2 * TOAST_TOP_MARGIN).max(TOAST_MIN_W);
            // 文字最大宽度＝面板上限减去强调条/内边距/图标/图标间距/✕区/右内边距。
            let text_max_w = (panel_max_w
                - TOAST_PAD_X
                - icon_sz.w
                - TOAST_ICON_GAP
                - TOAST_ICON_GAP
                - TOAST_CLOSE_W
                - TOAST_PAD_X)
                .max(TOAST_TEXT_MIN_W);
            // 按 text_max_w 换行测量：短文本一行内即可测完，长文本自动折成多行。
            let ts = canvas.measure_text_wrapped(
                &toast.req.text,
                &crate::text::TextStyle::new(TOAST_FONT),
                text_max_w as f32,
            );
            let panel_w = (TOAST_PAD_X
                + icon_sz.w
                + TOAST_ICON_GAP
                + ts.w
                + TOAST_ICON_GAP
                + TOAST_CLOSE_W
                + TOAST_PAD_X)
                .max(TOAST_MIN_W)
                .min(panel_max_w);
            let panel_h = TOAST_PAD_Y + ts.h.max(icon_sz.h) + TOAST_PAD_Y;
            let x = ((ws.w - panel_w) / 2).max(0);
            let corner = tt.corner(&self.theme.metrics);
            // 柔和投影（透明度跟随淡入淡出）。
            canvas.draw_shadow(
                x as f32,
                (y + 6) as f32,
                panel_w as f32,
                panel_h as f32,
                corner,
                22.0,
                Color::rgba(0, 0, 0, 90).scale_alpha(alpha),
            );
            canvas.fill_round_rect(
                x as f32,
                y as f32,
                panel_w as f32,
                panel_h as f32,
                corner,
                &Paint::fill(tt.bg(pal).scale_alpha(alpha)),
            );
            // 图标：面板左侧，垂直居中。
            let icon_x = x + TOAST_PAD_X;
            let icon_rect = Rect::new(icon_x, y, icon_sz.w, panel_h);
            canvas.draw_text(
                glyph,
                icon_rect,
                icon_color.scale_alpha(alpha),
                crate::spec::Align::Center,
                &crate::text::TextStyle::new(TOAST_ICON_FONT),
            );
            // 文字：图标右侧，垂直居中、左对齐；rect 宽用 text_max_w（而非 ts.w）
            // 以保证绘制时的换行宽度与测量时一致（长文本才需要换行，短文本本就不超）。
            let text_x = icon_x + icon_sz.w + TOAST_ICON_GAP;
            let text_rect = Rect::new(text_x, y, text_max_w, panel_h);
            canvas.draw_text(
                &toast.req.text,
                text_rect,
                tt.text(pal).scale_alpha(alpha),
                crate::spec::Align::Start,
                &crate::text::TextStyle::new(TOAST_FONT),
            );
            // ✕ 关闭：面板右侧固定宽区域。
            let close = Rect::new(
                x + panel_w - TOAST_CLOSE_W - TOAST_PAD_X / 2,
                y,
                TOAST_CLOSE_W,
                panel_h,
            );
            canvas.draw_text(
                "\u{2715}",
                close,
                pal.text_muted.scale_alpha(alpha),
                crate::spec::Align::Center,
                &crate::text::TextStyle::new(TOAST_FONT),
            );
            let panel = Rect::new(x, y, panel_w, panel_h);
            self.toast_rects.push((panel, close));
            y += panel_h + TOAST_GAP;
            // 持续推进淡入淡出与过期：请求下一帧。
            crate::anim::request_repaint();
        }
        // 上下文菜单浮层绘制在控件树之上（self.menu 与 self.engine 为不相交字段，借用安全）。
        // 级联：从根到子菜单逐级绘制（子菜单覆盖在上）。绘制在 toast 之后，确保菜单不被 toast 遮挡。
        if let Some(menu) = self.menu.as_ref() {
            let (pal, mt) = (&self.theme.palette, &self.theme.menu);
            for (li, level) in menu.levels.iter().enumerate() {
                let r = level.rect;
                // 面板投影 + 圆角底 + 描边。
                canvas.draw_shadow(
                    r.x as f32,
                    (r.y + 6) as f32,
                    r.w as f32,
                    r.h as f32,
                    10.0,
                    18.0,
                    Color::rgba(0, 0, 0, 110),
                );
                canvas.fill_round_rect(
                    r.x as f32,
                    r.y as f32,
                    r.w as f32,
                    r.h as f32,
                    10.0,
                    &Paint::fill(mt.bg(pal)),
                );
                canvas.stroke_round_rect(
                    r.x as f32,
                    r.y as f32,
                    r.w as f32,
                    r.h as f32,
                    10.0,
                    1.0,
                    &Paint::fill(mt.border(pal)),
                );
                let child_spawn = menu.levels.get(li + 1).and_then(|l| l.spawn);
                let label_x = r.x
                    + MENU_PAD_X
                    + if level.has_icons {
                        MENU_ICON_W + MENU_GAP
                    } else {
                        0
                    };
                // 裁剪到内缩矩形：上下各留 MENU_VPAD 像素，使条目在触达圆角边框前
                // 自然裁切（scroll=0 时第一项恰在裁剪边界，滚动时产生平滑"滚出"效果）。
                canvas.save();
                canvas.clip_rect(Rect::new(
                    r.x,
                    r.y + MENU_VPAD,
                    r.w,
                    (r.h - 2 * MENU_VPAD).max(0),
                ));
                for (i, (top, h)) in level.item_rows().into_iter().enumerate() {
                    let it = &level.items[i];
                    if it.separator {
                        canvas.fill_rect(
                            (r.x + 8) as f32,
                            (top + h / 2) as f32,
                            (r.w - 16) as f32,
                            1.0,
                            &Paint::fill(mt.border(pal)),
                        );
                        continue;
                    }
                    // 激活：本层悬停项，或展开了子菜单的父项（指针深入子菜单时父项保持高亮）。
                    let active = (level.hover == Some(i) || child_spawn == Some(i)) && it.enabled;
                    if active {
                        canvas.fill_round_rect(
                            (r.x + 4) as f32,
                            (top + 1) as f32,
                            (r.w - 8) as f32,
                            (h - 2) as f32,
                            6.0,
                            &Paint::fill(mt.hover(pal)),
                        );
                    }
                    let color = if !it.enabled {
                        mt.text_disabled(pal)
                    } else if active || it.checked {
                        mt.accent(pal)
                    } else {
                        mt.text(pal)
                    };
                    // 图标列。
                    if let Some(icon) = &it.icon {
                        let ir = Rect::new(r.x + MENU_PAD_X, top, MENU_ICON_W, h);
                        canvas.draw_text(
                            icon,
                            ir,
                            color,
                            crate::spec::Align::Center,
                            &crate::text::TextStyle::new(MENU_FONT),
                        );
                    }
                    // 尾随区域从右向左依次收窄：可点击图标 → 徽章胶囊 → 剩余内容右边界。
                    let mut content_right = r.right() - MENU_PAD_X;
                    if let Some(icon) = &it.trailing_icon {
                        let ir = Rect::new(content_right - MENU_ICON_W, top, MENU_ICON_W, h);
                        canvas.draw_text(
                            icon,
                            ir,
                            color,
                            crate::spec::Align::Center,
                            &crate::text::TextStyle::new(MENU_FONT),
                        );
                        content_right -= MENU_ICON_W + MENU_GAP;
                    }
                    if let Some((text, intent)) = &it.badge {
                        let (fill, fg) = intent.badge_colors(pal);
                        let bw = canvas
                            .measure_text(text, &crate::text::TextStyle::new(12.0))
                            .w
                            + 2 * BADGE_PAD_X;
                        let br =
                            Rect::new(content_right - bw, top + (h - BADGE_H) / 2, bw, BADGE_H);
                        canvas.fill_round_rect(
                            br.x as f32,
                            br.y as f32,
                            br.w as f32,
                            br.h as f32,
                            999.0,
                            &Paint::fill(fill),
                        );
                        canvas.draw_text(
                            text,
                            br,
                            fg,
                            crate::spec::Align::Center,
                            &crate::text::TextStyle::new(12.0),
                        );
                        content_right -= bw + MENU_GAP;
                    }
                    // 标签（+ 可选第二行小字说明）。
                    let label_w = (content_right - label_x).max(0);
                    if let Some(sub) = &it.subtitle {
                        let lr = Rect::new(label_x, top, label_w, h / 2);
                        canvas.draw_text(
                            &it.label,
                            lr,
                            color,
                            crate::spec::Align::Start,
                            &crate::text::TextStyle::new(MENU_FONT),
                        );
                        let sr = Rect::new(label_x, top + h / 2, label_w, h - h / 2);
                        canvas.draw_text(
                            sub,
                            sr,
                            mt.text_disabled(pal),
                            crate::spec::Align::Start,
                            &crate::text::TextStyle::new(MENU_FONT - 2.5),
                        );
                    } else {
                        let lr = Rect::new(label_x, top, label_w, h);
                        canvas.draw_text(
                            &it.label,
                            lr,
                            color,
                            crate::spec::Align::Start,
                            &crate::text::TextStyle::new(MENU_FONT),
                        );
                    }
                    // 尾随：子菜单箭头 › / 快捷键 / 勾选（收窄到 content_right，避免与徽章/图标重叠）。
                    let tr = Rect::new(r.x, top, (content_right - r.x).max(0), h);
                    if !it.submenu.is_empty() {
                        canvas.draw_text(
                            "\u{203A}",
                            tr,
                            color,
                            crate::spec::Align::End,
                            &crate::text::TextStyle::new(MENU_FONT + 1.0),
                        );
                    } else if let Some(s) = &it.shortcut {
                        canvas.draw_text(
                            s,
                            tr,
                            mt.text_disabled(pal),
                            crate::spec::Align::End,
                            &crate::text::TextStyle::new(MENU_FONT - 2.0),
                        );
                    } else if it.checked {
                        canvas.draw_text(
                            "\u{2713}",
                            tr,
                            mt.accent(pal),
                            crate::spec::Align::End,
                            &crate::text::TextStyle::new(MENU_FONT),
                        );
                    }
                }
                canvas.restore();
                // 内容超高时绘制右侧滚动指示条。
                if level.content_h > r.h {
                    let track_h = (r.h - 8) as f32;
                    let ratio = r.h as f32 / level.content_h as f32;
                    let thumb_h = (track_h * ratio).max(20.0);
                    let max_sc = level.max_scroll().max(1) as f32;
                    let thumb_y =
                        (r.y + 4) as f32 + (track_h - thumb_h) * (level.scroll as f32 / max_sc);
                    canvas.fill_round_rect(
                        (r.right() - 8) as f32,
                        thumb_y,
                        5.0,
                        thumb_h,
                        2.5,
                        &Paint::fill(mt.border(pal)),
                    );
                }
            }
        }
        // 帧耗时浮层（WINDUI_FPS=1）：左上角显示本帧渲染耗时与估算 fps，用于排查卡顿。
        if self.show_fps {
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
        drop(canvas);
        // 种入后备缓冲（整窗），供后续局部帧重建未变区域。
        // GPU 后端（as_pixmap=None）不走局部重绘，seed_back 无需调用；软后端必有 pixmap。
        if let Some(pixmap) = target.as_pixmap() {
            self.seed_back(pixmap, size);
        }
        self.pending_damage = next_damage(&mut self.needs_full);
        // 布局动画请求下一帧重排（同局部路径：走 needs_relayout 正规门）。
        if crate::anim::take_relayout() {
            self.needs_relayout = true;
        }
        if crate::render::prof::enabled() {
            eprintln!(
                "[prof] full {:.2}ms  {}",
                frame_t0.elapsed().as_secs_f64() * 1000.0,
                crate::render::prof::take_summary()
            );
        }
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
        if self.menu.is_some() {
            return self.handle_menu_pointer(ev);
        }
        // toast 浮层在控件树之上：命中则独占该事件。
        if !self.toasts.is_empty() && self.handle_toast_pointer(ev) {
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
        self.hover_pos = ev.pos;
        let now_ms = self.start.elapsed().as_millis() as u64;
        if hover != old_hover {
            self.hover_since_ms = now_ms;
            self.tooltip_suppressed = false;
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
            PointerKind::Down => self.tooltip_suppressed = true,
            PointerKind::Move if self.tooltip_suppressed => {
                self.tooltip_suppressed = false;
                self.hover_since_ms = now_ms;
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
            self.needs_relayout = true;
        }
        repaint
    }

    fn on_key(&mut self, ev: crate::event::KeyEvent) -> bool {
        self.sync_clock();
        // 菜单激活时由浮层独占键盘：↑↓ 选项、←→ 进出子菜单、回车/空格执行、
        // Escape 关闭，其余吞掉（避免打到被遮住的控件上）。
        if self.menu.is_some() {
            return self.handle_menu_key(ev);
        }
        // Tab 由宿主独占用于焦点导航，并启用焦点环显示。焦点环跨节点变化（低频）→ 整窗。
        if ev.key == Key::Tab {
            self.focus_visible = true;
            let moved = self.move_focus(!ev.shift);
            if moved {
                self.needs_full = true;
            }
            return moved;
        }
        // 其余键先交给焦点控件；未被消费的 Escape 回退为关闭窗口。
        let res = self.tree.dispatch_key(ev, self.focus);
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
            self.needs_relayout = true;
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
        self.needs_full = true;
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

    fn on_interval_fired(&mut self, idx: usize) -> bool {
        if let Some(cb) = self.interval_cbs.get_mut(idx) {
            cb();
            true
        } else {
            false
        }
    }

    fn on_drop_files(&mut self, pos: Point, paths: Vec<std::path::PathBuf>) -> bool {
        self.needs_full = true;
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
        if self.menu.is_some() {
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
        if let Some(menu) = &self.menu {
            if menu.levels.iter().any(|l| l.rect.contains(p)) {
                return true;
            }
        }
        // toast 浮层同理：面板范围内判为客户区。否则无边框窗口的自绘标题栏拖动区
        // 会把落在其上的 toast 点击（✕ 关闭 / 右键复制菜单）当 HTCAPTION 吞掉。
        if self.toast_rects.iter().any(|(panel, _)| panel.contains(p)) {
            return true;
        }
        self.tree.interactive_hit_at(p)
    }

    fn take_window_op(&mut self) -> Option<WindowOp> {
        self.pending_window_op.take()
    }

    fn take_dialog_request(&mut self) -> Option<DialogRequest> {
        self.pending_dialog.take()
    }

    fn cursor(&self) -> CursorShape {
        // 菜单浮层激活时用箭头（菜单项自管悬停高亮）。
        if self.menu.is_some() {
            return CursorShape::Arrow;
        }
        // 取当前悬停节点的形状；禁用节点统一回退箭头（禁用链接不显示手型）。
        match self.hover {
            Some(h) if self.tree.node_enabled(h) => self.tree.cursor_at(h),
            _ => CursorShape::Arrow,
        }
    }

    fn on_pan(&mut self, pos: Point, dy: i32) -> bool {
        self.needs_full = true; // 滚动改变大片区域 → 全窗重绘。
                                // 菜单激活时忽略平移（并清残差，避免菜单关闭后跳变）。
        if self.menu.is_some() {
            self.pan_residual = 0.0;
            return false;
        }
        // 物理 → 逻辑（命中与滚动均在逻辑空间）；亚像素残差累积，避免高 DPI 发黏。
        let s = self.scale;
        let p = Point::new(
            (pos.x as f32 / s).round() as i32,
            (pos.y as f32 / s).round() as i32,
        );
        let total = dy as f32 / s + self.pan_residual;
        let dyl = total.trunc() as i32;
        self.pan_residual = total - dyl as f32;
        if dyl == 0 {
            return false;
        }
        // 拖动跟手时打断残留惯性/回弹，避免方向冲突。
        self.clear_fling();
        self.tree.pan_scroll(p, dyl)
    }

    fn start_fling(&mut self, pos: Point, vy: f32) -> bool {
        // 复位任何残留惯性/回弹偏移，再决定是否启动新的。
        self.clear_fling();
        // 菜单激活时不滑。
        if self.menu.is_some() {
            return false;
        }
        // 释放速度过低 → 视为缓慢拖放，不进入惯性。
        if vy.abs() < FLING_TRIGGER {
            return false;
        }
        let s = self.scale;
        let p = Point::new(
            (pos.x as f32 / s).round() as i32,
            (pos.y as f32 / s).round() as i32,
        );
        // scroll_y 速度 = −手指速度（手指上移 vy<0 → 内容上移、scroll_y 增大）；物理→逻辑。
        let vel = -vy / s;
        // 按惯性方向找能继续滚动的容器：内层到界则冒泡外层（与 pan 一致）。
        let Some(node) = self.tree.scroll_target(p, vel > 0.0) else {
            return false;
        };
        self.fling = Some(Fling {
            node,
            phase: FlingPhase::Glide,
            vel,
            over: 0.0,
            residual: 0.0,
            last_ms: None,
        });
        // 触发持续动画，下一帧起由 step_fling 推进。
        crate::anim::request_repaint();
        true
    }

    fn cancel_fling(&mut self) -> bool {
        self.clear_fling()
    }

    fn ime_caret(&self) -> Option<(i32, i32, i32)> {
        let focus = self.focus?;
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
        let Some(focus) = self.focus else {
            return false;
        };
        self.tree.set_composing(focus, composing)
    }

    fn on_capture_lost(&mut self) -> bool {
        self.needs_full = true;
        // 给捕获节点派发一个远处坐标的合成 Up，复用 Up 语义让其收尾
        // （Slider 复位拖动、Button 因 inside=false 不误触发），并清逻辑捕获。
        if self.capture.is_none() {
            return false;
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

impl UiHost {
    /// 局部重绘：把脏区渲染进脏区大小的子 pixmap（tiny-skia 按 pixmap 边界自动剔除框外
    /// 图元，成本降到脏区面积），合成进后备缓冲，再整窗拷给平台 pixmap。复用上一全窗帧的
    /// 布局（当前动画均为视觉位移、不改布局）。
    fn render_partial(&mut self, pixmap: &mut Pixmap, size: Size, s: f32, damage: Rect) {
        // 脏区外扩 AA 余量并钳到窗口逻辑范围。
        let raw = damage
            .inflate(DAMAGE_MARGIN)
            .intersect(&Rect::from_size(self.logical_size));
        // 原点对齐到 4 逻辑像素网格：Windows DPI 缩放恒为 25% 的倍数（scale=m/4），故 4 的倍数 ×scale
        // 必为整数，子 pixmap 物理原点 dmg.origin×scale 精确无取整 → 文字定位与全窗帧逐像素一致，
        // 消除局部帧的纵向 1px 抖动。
        const GRID: i32 = 4;
        let x0 = raw.x - raw.x.rem_euclid(GRID);
        let y0 = raw.y - raw.y.rem_euclid(GRID);
        let x1 = raw.right() + (GRID - raw.right().rem_euclid(GRID)) % GRID;
        let y1 = raw.bottom() + (GRID - raw.bottom().rem_euclid(GRID)) % GRID;
        let dmg =
            Rect::new(x0, y0, x1 - x0, y1 - y0).intersect(&Rect::from_size(self.logical_size));
        // 物理化并钳到 pixmap 边界。
        let pdmg = dmg.scaled(s).intersect(&Rect::new(0, 0, size.w, size.h));
        if pdmg.is_empty() {
            self.blit_back_to(pixmap);
            return;
        }
        // 子 pixmap：脏区大小，按窗口背景填底（与全窗帧平台 fill 同色，重建一致）。
        let Some(mut sub) = Pixmap::new(pdmg.w as u32, pdmg.h as u32) else {
            self.blit_back_to(pixmap);
            return;
        };
        sub.fill(tiny_skia::Color::from_rgba8(
            self.bg.r, self.bg.g, self.bg.b, self.bg.a,
        ));
        // 以脏区左上角（逻辑）为偏移绘制整树：框外图元由 tiny-skia 廉价剔除。
        {
            let mut canvas = SkiaCanvas::with_text_offset(
                &mut sub,
                &mut self.engine,
                s,
                Point::new(dmg.x, dmg.y),
            );
            self.tree.paint(&mut canvas);
        }
        // 合成进后备缓冲（脏区物理原点），再整窗拷给平台 pixmap。
        if let Some(back) = self.back.as_mut() {
            blit(&sub, back, pdmg.x, pdmg.y);
        }
        self.blit_back_to(pixmap);
    }

    /// 把后备缓冲整窗拷入 pixmap（两者同尺寸时）。
    fn blit_back_to(&self, pixmap: &mut Pixmap) {
        if let Some(back) = self.back.as_ref() {
            if back.width() == pixmap.width() && back.height() == pixmap.height() {
                pixmap.data_mut().copy_from_slice(back.data());
            }
        }
    }

    /// 全窗帧结束：把刚绘好的 pixmap 整窗种入后备缓冲，供后续局部帧复用（按需重建尺寸）。
    fn seed_back(&mut self, pixmap: &Pixmap, size: Size) {
        let need_new = self
            .back
            .as_ref()
            .map(|b| b.width() != size.w as u32 || b.height() != size.h as u32)
            .unwrap_or(true);
        if need_new {
            self.back = Pixmap::new(size.w as u32, size.h as u32);
        }
        if let Some(back) = self.back.as_mut() {
            back.data_mut().copy_from_slice(pixmap.data());
        }
    }
}

/// 取本帧累积的动画脏区，映射为下一帧的局部脏区；Full（浮层/fling 等节点外请求）→
/// 标记下一帧全窗、返回 None。
fn next_damage(needs_full: &mut bool) -> Option<Rect> {
    match crate::anim::take_damage() {
        crate::anim::Damage::Rect(r) => Some(r),
        crate::anim::Damage::Full => {
            *needs_full = true;
            None
        }
        crate::anim::Damage::None => None,
    }
}

/// 把 src（RGBA8）整块覆盖拷入 dst 的 (x,y)（src 不超出 dst；不做 alpha 混合）。
fn blit(src: &Pixmap, dst: &mut Pixmap, x: i32, y: i32) {
    let (sw, sh) = (src.width() as usize, src.height() as usize);
    let (dw, dh) = (dst.width() as usize, dst.height() as usize);
    let (x, y) = (x.max(0) as usize, y.max(0) as usize);
    // 契约：src 必须完整落在 dst 内（调用方已把脏区钳到 pixmap 边界）。越界即逻辑错误。
    debug_assert!(
        x + sw <= dw && y + sh <= dh,
        "blit 越界：({x},{y})+{sw}x{sh} 超出 {dw}x{dh}"
    );
    let sd = src.data();
    let dd = dst.data_mut();
    for row in 0..sh {
        let s0 = row * sw * 4;
        let d0 = ((y + row) * dw + x) * 4;
        dd[d0..d0 + sw * 4].copy_from_slice(&sd[s0..s0 + sw * 4]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_returns_sendable_sender() {
        let mut app = App::new("t", 100, 100);
        let tx = app.channel::<u32>(|_| {});
        let h = std::thread::spawn(move || tx.send(5));
        assert!(h.join().unwrap().is_ok());
        assert_eq!(app.pumps.len(), 1);
    }

    #[test]
    fn on_interval_registers() {
        let app = App::new("t", 100, 100).on_interval(std::time::Duration::from_millis(100), || {});
        assert_eq!(app.intervals.len(), 1);
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
            .on_close_request(|| false)
            .content(Element::col());
        let mut app = app.into_handler_for_test();
        assert!(!app.on_close_request());
        assert_eq!(
            app.take_window_op(),
            None,
            "拦截器拒绝时窗口应原样留着，既不关也不隐"
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
    fn toast_stack_caps_at_max_and_drops_oldest() {
        let app = App::new("t", 100, 100).content(Element::col());
        let mut app = app.into_handler_for_test();
        for i in 0..(TOAST_MAX + 2) {
            app.push_toast(ToastRequest {
                text: format!("t{i}"),
                kind: crate::event::ToastKind::Info,
                duration_ms: 3000,
            });
        }
        assert_eq!(app.toasts.len(), TOAST_MAX, "不超过上限");
        assert_eq!(app.toasts.first().unwrap().req.text, "t2", "最旧两条被丢弃");
        assert_eq!(
            app.toasts.last().unwrap().req.text,
            format!("t{}", TOAST_MAX + 1)
        );
    }

    #[test]
    fn toast_fade_curve_and_expiry() {
        let t = ToastState {
            req: ToastRequest {
                text: "hi".into(),
                kind: crate::event::ToastKind::Success,
                duration_ms: 1000,
            },
            shown_at_ms: 100,
            paused_at_ms: None,
            paused_total_ms: 0,
        };
        assert_eq!(t.alpha(100), 0.0, "起点不可见");
        let mid_in = t.alpha(100 + TOAST_FADE_IN_MS / 2);
        assert!((0.4..=0.6).contains(&mid_in));
        assert_eq!(t.alpha(100 + 500), 1.0);
        assert!(!t.expired(100 + 999));
        assert!(t.expired(100 + 1000));
    }

    #[test]
    fn toast_hover_freezes_countdown() {
        let mut t = ToastState {
            req: ToastRequest {
                text: "hi".into(),
                kind: crate::event::ToastKind::Info,
                duration_ms: 1000,
            },
            shown_at_ms: 0,
            paused_at_ms: None,
            paused_total_ms: 0,
        };
        // 200ms 时悬停，冻结；在 5000ms（远超 1000）仍不过期。
        t.set_hover(200, true);
        assert!(!t.expired(5000), "悬停期间不过期");
        assert_eq!(t.active_elapsed(5000), 200, "有效流逝冻结在 200");
        // 5000ms 移开，恢复计时；再过 800ms（累计有效 1000）到时过期。
        t.set_hover(5000, false);
        assert!(!t.expired(5000 + 799));
        assert!(t.expired(5000 + 800));
    }

    #[test]
    fn toast_hit_and_close_hit() {
        use crate::geometry::Point;
        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        app.toast_rects = vec![
            (Rect::new(100, 16, 200, 44), Rect::new(280, 16, 22, 44)),
            (Rect::new(100, 70, 200, 44), Rect::new(280, 70, 22, 44)),
        ];
        assert_eq!(app.toast_hit(Point::new(150, 30)), Some(0));
        assert_eq!(app.toast_hit(Point::new(150, 84)), Some(1));
        assert_eq!(app.toast_hit(Point::new(10, 10)), None);
        assert_eq!(app.toast_close_hit(Point::new(285, 30)), Some(0));
        assert_eq!(
            app.toast_close_hit(Point::new(150, 30)),
            None,
            "面板内非✕区不算关闭"
        );
    }

    #[test]
    fn trailing_icon_click_fires_independently_of_item_selection() {
        // 回归：菜单项的尾随可点击图标（如"删除该项"）点击只应触发它自己的回调，
        // 不应选中该项——验证 trailing_icon_at 命中 + handle_menu_pointer 优先分支。
        use crate::event::{MenuItem, MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;

        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        let target = app.tree.root.unwrap();

        let selected = std::rc::Rc::new(std::cell::Cell::new(false));
        let trashed = std::rc::Rc::new(std::cell::Cell::new(false));
        let (sel, trash) = (selected.clone(), trashed.clone());
        let item = MenuItem::run("团队版", move || sel.set(true), false)
            .with_subtitle("多人协作 + 权限管理")
            .with_badge("New", crate::theme::Intent::Danger)
            .with_trailing_icon("🗑", move || trash.set(true));

        let level = app.build_level(vec![item], 20, 20, 0, None, None);
        app.menu = Some(ContextMenu {
            levels: vec![level],
            target,
            rebuild: None,
        });

        let rect = app.menu.as_ref().unwrap().levels[0].rect;
        let icon_pos = Point::new(
            rect.right() - MENU_PAD_X - MENU_ICON_W / 2,
            rect.y + MENU_VPAD + 5,
        );
        assert_eq!(
            app.menu.as_ref().unwrap().levels[0].trailing_icon_at(icon_pos),
            Some(0),
            "尾随图标矩形应命中该项"
        );

        app.handle_menu_pointer(PointerEvent::single(
            PointerKind::Down,
            icon_pos,
            MouseButton::Left,
        ));

        assert!(trashed.get(), "点击尾随图标应触发其自身回调");
        assert!(!selected.get(), "点击尾随图标不应触发主项 action（选中）");
        assert!(app.menu.is_none(), "点击后菜单应关闭");
    }

    #[test]
    fn sticky_item_keeps_menu_open_and_refreshes_checks() {
        // 复选菜单的核心回归：开关项点击后菜单**不关闭**、勾选态原地刷新，
        // 可连点多个；混排的动作项仍是"点了执行并关闭"。
        use crate::event::{MenuItem, MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;

        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        let target = app.tree.root.unwrap();

        let a = crate::signal::signal(false);
        let b = crate::signal::signal(false);
        let ran = std::rc::Rc::new(std::cell::Cell::new(false));
        let ran_cb = ran.clone();
        let rebuild: Rc<dyn Fn() -> Vec<MenuItem>> = Rc::new(move || {
            let r = ran_cb.clone();
            vec![
                MenuItem::run("甲", move || a.set(!a.get()), a.get()).stay_open(),
                MenuItem::run("乙", move || b.set(!b.get()), b.get()).stay_open(),
                MenuItem::run("执行", move || r.set(true), false),
            ]
        });

        let level = app.build_level(rebuild(), 20, 20, 0, None, None);
        let rect = level.rect;
        app.menu = Some(ContextMenu {
            levels: vec![level],
            target,
            rebuild: Some(rebuild),
        });
        macro_rules! click {
            ($i:expr) => {
                app.handle_menu_pointer(PointerEvent::single(
                    PointerKind::Down,
                    Point::new(
                        rect.x + 20,
                        rect.y + MENU_VPAD + $i * MENU_ITEM_H + MENU_ITEM_H / 2,
                    ),
                    MouseButton::Left,
                ))
            };
        }

        click!(0);
        assert!(a.get(), "开关项应翻转绑定值");
        assert!(app.menu.is_some(), "开关项点击后菜单须保持展开");
        assert!(
            app.menu.as_ref().unwrap().levels[0].items[0].checked,
            "重建后勾选态应原地刷新"
        );

        // 连点第二个开关：无需重新打开菜单。
        click!(1);
        assert!(b.get());
        assert!(app.menu.is_some());
        let items = &app.menu.as_ref().unwrap().levels[0].items;
        assert!(items[0].checked && items[1].checked, "两个开关都应为开");

        // 再点第一个：翻回关闭态，菜单仍在。
        click!(0);
        assert!(!a.get());
        assert!(!app.menu.as_ref().unwrap().levels[0].items[0].checked);

        // 混排的动作项不粘滞：执行并关闭。
        click!(2);
        assert!(ran.get(), "动作项应执行");
        assert!(app.menu.is_none(), "动作项点击后菜单须关闭");
    }

    #[test]
    fn sticky_refresh_preserves_panel_geometry() {
        // 面板宽度/位置不随重建变化：项文本变宽也不重新测量，否则指针下的项会在
        // 两次点击之间挪位，用户点到的不是他瞄准的那一项。
        use crate::event::{MenuItem, MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;

        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        let target = app.tree.root.unwrap();

        let wide = crate::signal::signal(false);
        let rebuild: Rc<dyn Fn() -> Vec<MenuItem>> = Rc::new(move || {
            let label = if wide.get() {
                "开关项——展开后标签显著变长以撑宽面板"
            } else {
                "短"
            };
            vec![MenuItem::run(label, move || wide.set(!wide.get()), wide.get()).stay_open()]
        });

        let level = app.build_level(rebuild(), 20, 20, 0, None, None);
        let before = level.rect;
        app.menu = Some(ContextMenu {
            levels: vec![level],
            target,
            rebuild: Some(rebuild),
        });
        app.handle_menu_pointer(PointerEvent::single(
            PointerKind::Down,
            Point::new(before.x + 20, before.y + MENU_VPAD + MENU_ITEM_H / 2),
            MouseButton::Left,
        ));
        assert!(wide.get());
        assert_eq!(
            app.menu.as_ref().unwrap().levels[0].rect,
            before,
            "重建不得改变面板矩形"
        );
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
        handler.needs_relayout = true;
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
        let hk = app.hotkey_rc(Hotkey::new(Key::Char('D')).ctrl().alt(), |_| {});
        let hk2 = hk.clone();
        hk.rebind(Hotkey::new(Key::Char('J')).ctrl());
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
    fn interaction_takes_partial_path() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let app = App::new("t", 60, 60).content(Element::col().width(60).height(60));
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(60, 60).unwrap();
        // 首帧：全窗，种入后备缓冲。
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(60, 60));
        assert!(handler.last_frame_full, "首帧应为全窗");
        // 模拟交互产生的小脏区：下一帧应走局部重绘，不重排整树。
        handler.event_damage = Some(Rect::new(10, 10, 12, 12));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(60, 60));
        assert!(!handler.last_frame_full, "带小脏区的交互帧应走局部重绘");
    }

    #[test]
    fn structural_click_repaints_full() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        // 按钮点击切换 visible_when 面板显隐（结构变化）→ 重排后签名变 → 必须整窗。
        let flag = std::rc::Rc::new(std::cell::Cell::new(false));
        let f2 = flag.clone();
        let app = App::new("t", 80, 80).content(
            Element::col()
                .width(80)
                .height(80)
                .child(Element::button("X").on_click(move |_| f2.set(true)))
                .child(
                    Element::col()
                        .width(80)
                        .height(30)
                        .visible_when(move || flag.get()),
                ),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(80, 80).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(80, 80)); // 首帧全窗 + 建立结构签名
        let at = Point::new(15, 12);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(80, 80));
        assert!(handler.last_frame_full, "切换 visible_when 面板应整窗刷新");
    }

    #[test]
    fn local_click_stays_partial() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        // 无结构副作用的按钮点击：重排后签名不变 → 走局部重绘（不整窗）。
        let app = App::new("t", 120, 120).content(
            Element::col()
                .width(120)
                .height(120)
                .child(Element::button("X")),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(120, 120).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(120, 120)); // 首帧全窗
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            Point::new(15, 12),
            MouseButton::Left,
        ));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(120, 120));
        assert!(!handler.last_frame_full, "无结构变化的点击应走局部重绘");
    }

    #[test]
    fn closing_menu_repaints_full() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        // 回归：关闭浮层的那一帧必须整窗——菜单画在控件树之上，局部重绘只擦交互脏区，
        // 面板像素会残留在屏上。overlay 判定读的是"本帧有没有浮层"，而关闭帧已经没有了，
        // 恰好此时补间还在跑（打开时 hover 清零触发边框补间）就会带着小脏区走局部路径。
        let app = App::new("t", 200, 200).content(Element::col().width(200).height(200).child(
            Element::dropdown(vec!["甲", "乙", "丙"], crate::signal::signal(0usize)).width(120),
        ));
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(200, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 200));

        // 点控件展开菜单。
        let on_ctl = Point::new(40, 12);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            on_ctl,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(
            PointerKind::Up,
            on_ctl,
            MouseButton::Left,
        ));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 200));
        assert!(handler.menu.is_some(), "应已展开菜单");
        assert!(handler.last_frame_full, "有浮层的帧本就整窗");

        // 点面板外关闭：这一帧浮层已消失，必须整窗把面板像素擦掉。
        let outside = Point::new(190, 190);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            outside,
            MouseButton::Left,
        ));
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 200));
        assert!(handler.menu.is_none(), "点面板外应关闭菜单");
        assert!(
            handler.last_frame_full,
            "关闭浮层的那一帧必须整窗，否则面板像素残留"
        );
    }

    #[test]
    fn render_drains_pending_messages() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let got = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let g2 = got.clone();
        let mut app = App::new("t", 50, 50);
        let tx = app.channel::<u32>(move |m| g2.set(m));
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

    /// 点控件外的空白应清空焦点（网页 blur 语义）：否则聚焦边框会一直亮到
    /// 下一个可聚焦控件接手为止。同时校验两条不该误清的边界。
    #[test]
    fn click_outside_clears_focus() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(Element::button("A"))
                .child(Element::flex_spacer()),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));

        let click = |h: &mut UiHost, p: Point| {
            h.on_pointer(PointerEvent::single(
                PointerKind::Down,
                p,
                MouseButton::Left,
            ));
            h.on_pointer(PointerEvent::single(PointerKind::Up, p, MouseButton::Left));
        };
        let on_btn = Point::new(30, 20);
        let blank = Point::new(150, 180);

        click(&mut handler, on_btn);
        let focused = handler.focus;
        assert!(focused.is_some(), "点按钮应获得焦点");

        // 焦点控件内部的按下不该清（命中节点在其祖先链上）。
        click(&mut handler, on_btn);
        assert_eq!(handler.focus, focused, "重复点同一控件应保持焦点");

        // 移动不参与裁决：只有按下才重新裁定焦点归属。
        handler.on_pointer(PointerEvent::single(
            PointerKind::Move,
            blank,
            MouseButton::Left,
        ));
        assert_eq!(handler.focus, focused, "指针移出不应清焦点");

        click(&mut handler, blank);
        assert!(handler.focus.is_none(), "点空白应清空焦点");
    }

    /// 回归：Dropdown 一直正确处理 Key::Space（select.rs 的 Key 分支），断的是宿主——
    /// on_key 消费了 close/open_url/window_op/dialog/toast，唯独漏了 res.menu，
    /// 控件的展开请求被 dispatch_key 收进 DispatchResult 后静默丢弃。
    #[test]
    fn keyboard_space_opens_dropdown_menu() {
        use crate::event::KeyEvent;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let sel = crate::signal::signal(0usize);
        let app = App::new("t", 300, 200).content(
            Element::col()
                .padding(10)
                .child(Element::dropdown(vec!["甲", "乙"], sel).width(200)),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));

        let k = |key| KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl: false,
        };
        handler.on_key(k(Key::Tab));
        assert!(handler.focus.is_some(), "Tab 应把焦点落到下拉框");
        assert!(handler.menu.is_none(), "此时尚无浮层");

        handler.on_key(k(Key::Space));
        assert!(handler.menu.is_some(), "按空格应展开下拉菜单");
    }

    /// 菜单展开后的键盘操作：此前 on_key 在 menu.is_some() 时只放行 Escape、
    /// 其余全吞，弹出来也只能用鼠标点。
    #[test]
    fn keyboard_navigates_and_activates_dropdown_menu() {
        let (mut handler, sel) = dropdown_handler();
        let k = key_ev();
        handler.on_key(k(Key::Tab));
        handler.on_key(k(Key::Space));

        // 尚无高亮时首次 ↓ 落到 checked 项（当前选中的第 1 项），而不是凭空跳走一格。
        handler.on_key(k(Key::Down));
        assert_eq!(
            handler.menu.as_ref().unwrap().levels[0].hover,
            Some(1),
            "首次 ↓ 应停在当前选中项上"
        );
        handler.on_key(k(Key::Down));
        assert_eq!(
            handler.menu.as_ref().unwrap().levels[0].hover,
            Some(2),
            "再次 ↓ 应移到下一项"
        );

        handler.on_key(k(Key::Enter));
        assert!(handler.menu.is_none(), "回车执行后菜单应关闭");
        assert_eq!(sel.get(), 2, "回车应选中高亮项");
    }

    #[test]
    fn keyboard_wraps_and_escapes_dropdown_menu() {
        let (mut handler, sel) = dropdown_handler();
        let k = key_ev();
        handler.on_key(k(Key::Tab));
        handler.on_key(k(Key::Space));

        // ↑ 从无高亮起同样落到 checked 项，再 ↑ 回到上一项；首项继续 ↑ 循环到末项。
        handler.on_key(k(Key::Up));
        handler.on_key(k(Key::Up));
        assert_eq!(handler.menu.as_ref().unwrap().levels[0].hover, Some(0));
        handler.on_key(k(Key::Up));
        assert_eq!(
            handler.menu.as_ref().unwrap().levels[0].hover,
            Some(2),
            "首项再 ↑ 应循环到末项"
        );

        handler.on_key(k(Key::Escape));
        assert!(handler.menu.is_none(), "Escape 应关闭菜单");
        assert_eq!(sel.get(), 1, "Escape 不应改变选中值");
    }

    /// 对话框弹出时焦点应进入框内、关闭后还给来处（同 `<dialog>.showModal()`）。
    /// 此前焦点留在后方按钮上，Tab 还能一路走到遮罩后面去。
    #[test]
    fn modal_open_moves_focus_into_dialog_and_restores_on_close() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let show = crate::signal::signal(false);
        let (open, close) = (show, show);
        let app = App::new("t", 300, 200).content(
            Element::stack()
                .fill()
                .child(
                    Element::col()
                        .padding(10)
                        .child(Element::button("打开").on_click(move |_| open.set(true))),
                )
                .child(Element::dialog(
                    show,
                    Element::col().child(
                        Element::button("确定")
                            .width(80)
                            .on_click(move |_| close.set(false)),
                    ),
                )),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(300, 200).unwrap();
        macro_rules! frame {
            () => {
                handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200))
            };
        }
        frame!();

        // 点开按钮：焦点落到它身上，同时请求弹出对话框。
        let at = Point::new(40, 25);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        let outside = handler.focus;
        assert!(outside.is_some(), "点按钮应先聚焦到它");

        frame!();
        assert_eq!(
            handler.focus,
            handler.focus_order.first().copied(),
            "对话框弹出后焦点应自动落到框内首个可聚焦控件"
        );
        assert_ne!(handler.focus, outside, "焦点不该留在遮罩后面的按钮上");
        assert!(!handler.focus_visible, "鼠标点开的对话框不该凭空冒出焦点框");

        // 点框内「确定」关闭对话框。
        let inside = handler.focus.unwrap();
        let b = handler.tree.abs_bounds(inside);
        let at = Point::new(b.x + b.w / 2, b.y + b.h / 2);
        handler.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        handler.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        frame!();
        assert_eq!(handler.focus, outside, "关闭后焦点应还给弹出前那个控件");
    }

    /// Tab 走到滚动区外的控件时应把它滚进视口。断言的是「焦点控件可见」这个目标
    /// 本身，而不是 scroll_y 的具体数值。
    #[test]
    fn tab_scrolls_focus_into_view() {
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;
        let mut col = Element::col();
        for i in 0..8 {
            col = col.child(Element::button(format!("B{i}")).height(40));
        }
        let app = App::new("t", 200, 100).content(
            Element::col()
                .fill()
                .child(Element::scroll().height(100).child(col)),
        );
        let mut handler = app.into_handler_for_test();
        handler.set_scale(1.0);
        let mut pm = Pixmap::new(200, 100).unwrap();
        macro_rules! frame {
            () => {
                handler.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(200, 100))
            };
        }
        frame!();
        assert_eq!(handler.focus_order.len(), 8, "8 个按钮都在焦点环里");

        let k = key_ev();
        // Tab 到最后一项（视口只装得下前两个半）。
        for _ in 0..8 {
            handler.on_key(k(Key::Tab));
        }
        frame!(); // 重排应用新的 scroll_y
        let f = handler.focus.expect("应有焦点");
        assert_eq!(f, handler.focus_order[7], "应停在最后一项");
        let b = handler.tree.abs_bounds(f);
        assert!(
            b.y >= 0 && b.bottom() <= 100,
            "焦点控件应被滚进视口，实际 y={} bottom={}",
            b.y,
            b.bottom()
        );
    }

    /// 焦点环只跟随键盘：同一个对话框，鼠标点开不显示、键盘打开显示。
    /// 判据是「用户最近一次交互用的什么」，而不是「焦点这次是不是框架挪的」。
    #[test]
    fn focus_ring_follows_keyboard_not_mouse() {
        use crate::event::{MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use crate::platform::AppHandler;
        use crate::render::PixmapTarget;
        use tiny_skia::Pixmap;

        // 每次从头搭一份：show 是构建期捕获的，两种打开方式不能共用同一棵树。
        let build = || {
            let show = crate::signal::signal(false);
            let open = show;
            let app = App::new("t", 300, 200).content(
                Element::stack()
                    .fill()
                    .child(
                        Element::col()
                            .padding(10)
                            .child(Element::button("打开").on_click(move |_| open.set(true))),
                    )
                    .child(Element::dialog(
                        show,
                        Element::col().child(Element::button("确定").width(80)),
                    )),
            );
            let mut h = app.into_handler_for_test();
            h.set_scale(1.0);
            h
        };
        let mut pm = Pixmap::new(300, 200).unwrap();

        // 鼠标路径：点按钮开框。
        let mut h = build();
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        let at = Point::new(40, 25);
        h.on_pointer(PointerEvent::single(
            PointerKind::Down,
            at,
            MouseButton::Left,
        ));
        h.on_pointer(PointerEvent::single(PointerKind::Up, at, MouseButton::Left));
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        assert!(h.focus.is_some(), "焦点仍应移进对话框（只是不画环）");
        assert!(!h.focus_visible, "纯鼠标操作全程不应出现焦点框");

        // 键盘路径：Tab 到按钮、空格激活。
        let mut h = build();
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        let k = key_ev();
        h.on_key(k(Key::Tab));
        assert!(h.focus_visible, "Tab 导航应打开焦点环");
        h.on_key(k(Key::Space));
        h.render(&mut PixmapTarget { pixmap: &mut pm }, Size::new(300, 200));
        assert!(h.focus.is_some(), "空格应激活按钮并弹出对话框");
        assert!(h.focus_visible, "键盘打开的对话框应保留焦点环");
    }

    /// 三项下拉（初值选中第 1 项）+ 已暖过布局的宿主。
    fn dropdown_handler() -> (UiHost, crate::signal::Signal<usize>) {
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

    fn key_ev() -> impl Fn(Key) -> crate::event::KeyEvent {
        |key| crate::event::KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl: false,
        }
    }
}
