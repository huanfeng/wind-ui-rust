//! 应用入口与交互宿主。
//!
//! `App` 构建器组装窗口配置与控件树；`UiHost` 持有运行期交互状态
//! （树、文字引擎、hover/capture/focus）并实现 `AppHandler` 供平台驱动。

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::sync::{new_channel, Sender, WakerShared};

use tiny_skia::Pixmap;

use crate::core::{NodeId, Tree};
use crate::theme::Theme;
use crate::event::{
    CursorShape, Key, MenuAction, MenuItem, MouseButton, PointerEvent, PointerKind, WindowOp,
};
use crate::geometry::{Color, Point, Rect, Size};
use crate::platform::{self, AppHandler, WindowConfig};
use crate::render::{Canvas, Paint, SkiaCanvas};
use crate::text::{PlatformTextEngine, TextEngine};
use crate::ui::Element;

// ---- 上下文菜单（宿主层自绘浮层）----

const MENU_ITEM_H: i32 = 30;
const MENU_PAD_X: i32 = 14;
const MENU_VPAD: i32 = 4;
const MENU_MIN_W: i32 = 130;
const MENU_FONT: f32 = 14.0;

/// 悬停提示：触发延时（ms）、字号、内边距、相对指针的偏移。
const TOOLTIP_DELAY_MS: u64 = 500;
const TOOLTIP_FONT: f32 = 13.0;
const TOOLTIP_PAD_X: i32 = 8;
const TOOLTIP_PAD_Y: i32 = 4;
const TOOLTIP_CURSOR_DX: i32 = 12;
const TOOLTIP_CURSOR_DY: i32 = 20;

/// 宿主管理的上下文菜单浮层：在控件树之上自绘，拦截指针，项激活时向目标控件合成按键。
struct ContextMenu {
    items: Vec<MenuItem>,
    rect: Rect,
    hover: Option<usize>,
    /// 发起菜单的控件（合成按键的派发目标）。
    target: NodeId,
}

impl ContextMenu {
    /// 项 i 在 y 轴的命中区间起点（逻辑坐标）。
    fn item_y(&self, i: usize) -> i32 {
        self.rect.y + MENU_VPAD + i as i32 * MENU_ITEM_H
    }
    /// 逻辑坐标 y → 菜单项下标（仅当落在某项区域内）。
    fn item_at(&self, p: Point) -> Option<usize> {
        if !self.rect.contains(p) {
            return None;
        }
        let i = (p.y - self.rect.y - MENU_VPAD) / MENU_ITEM_H;
        if i >= 0 && (i as usize) < self.items.len() {
            Some(i as usize)
        } else {
            None
        }
    }
}

type RenderClosure = Box<dyn FnMut(&mut Pixmap, Size)>;

/// 应用构建器。命令式 API 的根入口。
pub struct App {
    cfg: WindowConfig,
    render: Option<RenderClosure>,
    content: Option<Element>,
    theme: Option<Theme>,
    pumps: Vec<Box<dyn FnMut()>>,
    intervals: Vec<(Duration, Box<dyn FnMut()>)>,
    waker_shared: Option<Arc<WakerShared>>,
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
                screenshot_click: None,
                screenshot_hover: None,
                tray: None,
                frameless: false,
                animations: None,
            },
            render: None,
            content: None,
            theme: None,
            pumps: Vec::new(),
            intervals: Vec::new(),
            waker_shared: None,
        }
    }

    /// 窗口背景色。命名与 `Element::bg` 统一。
    pub fn bg(mut self, c: Color) -> Self {
        self.cfg.bg = c;
        self
    }

    /// 禁止用户拖拽调整窗口大小（去掉 WS_THICKFRAME 和最大化按钮）。
    pub fn resizable(mut self, v: bool) -> Self {
        self.cfg.resizable = v;
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

    /// 设置主题（默认使用内置默认主题）。窗口背景未显式设置时随主题 palette.bg。
    pub fn theme(mut self, t: Theme) -> Self {
        self.cfg.bg = t.palette.bg;
        self.theme = Some(t);
        self
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
        if let Some(i) = args.iter().position(|a| a == "--click") {
            if let (Some(x), Some(y)) = (
                args.get(i + 1).and_then(|s| s.parse::<i32>().ok()),
                args.get(i + 2).and_then(|s| s.parse::<i32>().ok()),
            ) {
                self.cfg.screenshot_click = Some((x, y));
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
        self
    }

    /// 底层渲染回调（无控件树时使用）。
    pub fn on_render(mut self, f: impl FnMut(&mut Pixmap, Size) + 'static) -> Self {
        self.render = Some(Box::new(f));
        self
    }

    /// 设置控件树根（常规入口）。
    pub fn content(mut self, root: Element) -> Self {
        self.content = Some(root);
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

    pub fn run(self) {
        let theme = Rc::new(self.theme.unwrap_or_default());
        let waker = self.waker_shared.clone();
        let cfg = self.cfg;
        let handler: Box<dyn AppHandler> = if let Some(f) = self.render {
            Box::new(ClosureHandler { f })
        } else if let Some(root) = self.content {
            Box::new(UiHost::new(root, theme, cfg.bg, self.pumps, self.intervals))
        } else {
            Box::new(ClosureHandler { f: Box::new(|_, _| {}) })
        };
        platform::run(cfg, handler, waker);
    }

    #[cfg(test)]
    fn into_handler_for_test(self) -> UiHost {
        let theme = Rc::new(self.theme.unwrap_or_default());
        UiHost::new(self.content.unwrap(), theme, self.cfg.bg, self.pumps, self.intervals)
    }

    fn shared_waker(&mut self) -> crate::sync::Waker {
        self.waker_shared.get_or_insert_with(WakerShared::new).waker()
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
    fn render(&mut self, pixmap: &mut Pixmap, size: Size) {
        (self.f)(pixmap, size);
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
    /// 活动的上下文菜单浮层（None=无）。
    menu: Option<ContextMenu>,
    /// 最近一帧的逻辑窗口尺寸（菜单弹出位置钳制用）。
    logical_size: Size,
    /// 活动主题（注入到线程局部供控件读取）。
    theme: Rc<Theme>,
    /// 单调起点，用于动画相位时钟。
    start: std::time::Instant,
    /// 触摸平移的亚像素残差（物理→逻辑取整丢失部分累积，避免高 DPI 细微平移发黏）。
    pan_residual: f32,
    /// 触摸惯性滑动状态（None=无）。
    fling: Option<Fling>,
    /// 待执行的窗口操作（自定义标题栏按钮触发，平台分发后轮询执行）。
    pending_window_op: Option<WindowOp>,
    /// 最近一次指针位置（逻辑坐标），用于悬停提示浮层定位。
    hover_pos: Point,
    /// 当前悬停起始时刻（ms，单调时钟）。悬停节点变化或点击时复位；
    /// 渲染据 `now - hover_since >= TOOLTIP_DELAY_MS` 决定是否弹出提示。
    hover_since_ms: u64,
    /// 点击后抑制提示，直到指针再次移动（避免点完控件原地又弹出盖住它）。
    tooltip_suppressed: bool,
    /// 窗口背景色（与平台 fill 同色）：局部重绘的子缓冲按此填底，重建脏区与全窗一致。
    bg: Color,
    /// 持久后备缓冲（物理像素，整窗）：保留上一全窗帧，供局部帧重建未变区域。
    back: Option<Pixmap>,
    /// 上一帧累积的动画脏区（逻辑坐标）：下一动画帧据此局部重绘；None=下一帧需全窗。
    pending_damage: Option<Rect>,
    /// 强制本帧全窗重绘（输入/结构/尺寸变更触发）。
    needs_full: bool,
    /// 一次「按下关闭浮层」后，吞掉随之而来的 Up：避免该 Up 下发到控件树重新激活
    /// 浮层下方控件（典型：下拉按钮点一下又弹一遍——Down 关、Up 再开）。
    swallow_up: bool,
    /// 跨线程通道的排空回调：渲染前在 UI 线程依次调用，把后台数据写入控件状态。
    pumps: Vec<Box<dyn FnMut()>>,
    /// 定时器回调列表（与 interval_durs 下标对应）。
    interval_cbs: Vec<Box<dyn FnMut()>>,
    /// 定时器间隔列表（平台据此注册 SetTimer/NSTimer）。
    interval_durs: Vec<std::time::Duration>,
}

/// 脏区四周外扩的抗锯齿余量（逻辑像素）：覆盖滑块边缘 AA 与子像素取整，杜绝残影。
const DAMAGE_MARGIN: i32 = 2;

impl UiHost {
    fn new(
        root: Element,
        theme: Rc<Theme>,
        bg: Color,
        pumps: Vec<Box<dyn FnMut()>>,
        intervals: Vec<(std::time::Duration, Box<dyn FnMut()>)>,
    ) -> Self {
        // 尽早注入，使首个事件（首帧渲染前）也能读到正确主题。
        crate::theme::set_current(theme.clone());
        let mut tree = Tree::new();
        tree.root = Some(root.build(&mut tree));
        tree.clipboard = Some(Box::new(crate::platform::Clipboard));
        let (interval_durs, interval_cbs): (Vec<_>, Vec<_>) =
            intervals.into_iter().unzip();
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
            menu: None,
            logical_size: Size::new(0, 0),
            theme,
            start: std::time::Instant::now(),
            pan_residual: 0.0,
            fling: None,
            pending_window_op: None,
            hover_pos: Point::new(0, 0),
            hover_since_ms: 0,
            tooltip_suppressed: false,
            bg,
            back: None,
            pending_damage: None,
            needs_full: true,
            swallow_up: false,
            pumps,
            interval_cbs,
            interval_durs,
        }
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

    /// 打开上下文菜单：用文字引擎测量项宽，计算并钳制到窗口内的菜单矩形。
    fn open_menu(&mut self, req: crate::event::MenuRequest, target: NodeId) {
        let mut max_w = 0;
        for it in &req.items {
            let w = self.engine.measure(&it.label, None, MENU_FONT, None).w;
            max_w = max_w.max(w);
        }
        let menu_w = (max_w + 2 * MENU_PAD_X).max(MENU_MIN_W).max(req.min_width);
        let menu_h = req.items.len() as i32 * MENU_ITEM_H + 2 * MENU_VPAD;
        let ws = self.logical_size;
        let mut x = req.pos.x;
        let mut y = req.pos.y;
        if ws.w > 0 && x + menu_w > ws.w {
            x = (ws.w - menu_w).max(0);
        }
        if ws.h > 0 && y + menu_h > ws.h {
            y = (ws.h - menu_h).max(0);
        }
        self.menu = Some(ContextMenu {
            items: req.items,
            rect: Rect::new(x, y, menu_w, menu_h),
            hover: None,
            target,
        });
    }

    /// 菜单激活时处理指针；返回是否需重绘。
    fn handle_menu_pointer(&mut self, ev: PointerEvent) -> bool {
        match ev.kind {
            PointerKind::Move => {
                let h = self.menu.as_ref().and_then(|m| m.item_at(ev.pos));
                if let Some(m) = self.menu.as_mut() {
                    if m.hover != h {
                        m.hover = h;
                        return true;
                    }
                }
                false
            }
            PointerKind::Down => {
                // 本次 Down 必关闭菜单（命中项执行后关 / 点外关）；标记吞掉随后的 Up，
                // 否则该 Up 会下发到控件树，把菜单下方控件再次激活（下拉点一下又弹一遍）。
                self.swallow_up = true;
                let inside = self.menu.as_ref().is_some_and(|m| m.rect.contains(ev.pos));
                if !inside {
                    self.menu = None; // 点击菜单外：关闭
                    return true;
                }
                // 命中可用项 → 执行动作并关闭。
                let hit = self.menu.as_ref().and_then(|m| {
                    m.item_at(ev.pos)
                        .filter(|&i| m.items[i].enabled)
                        .map(|i| (m.items[i].action.clone(), m.target))
                });
                if let Some((action, target)) = hit {
                    self.menu = None;
                    match action {
                        // 合成按键直达目标控件，绕过 on_key（不重跑 Tab/Escape 导航）。
                        MenuAction::SendKey(k) => {
                            let res = self.tree.dispatch_key(k, Some(target));
                            if res.close {
                                self.close = true;
                            }
                        }
                        // 运行闭包（下拉选择设置绑定值等）。
                        MenuAction::Run(f) => f(),
                    }
                }
                true // 菜单内（含禁用项/间隙）始终吞掉
            }
            _ => true, // 吞掉 Up/Wheel 等，避免穿透到下层
        }
    }

    /// Tab 焦点移动（forward=正向）。返回是否变化。
    fn move_focus(&mut self, forward: bool) -> bool {
        if self.focus_order.is_empty() {
            return false;
        }
        let n = self.focus_order.len();
        let cur = self.focus.and_then(|f| self.focus_order.iter().position(|&x| x == f));
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
        true
    }
}

impl AppHandler for UiHost {
    fn render(&mut self, pixmap: &mut Pixmap, size: Size) {
        // 跨线程消息：渲染前在 UI 线程排空所有通道，把后台数据写入控件状态。
        for pump in self.pumps.iter_mut() {
            pump();
        }
        // 注入主题（离屏路径首帧、主题变更时均生效）。
        crate::theme::set_current(self.theme.clone());
        // 动画：清上一帧请求/脏区并刷新帧时钟，绘制中控件可重新请求。
        crate::anim::reset_request();
        let now_ms = self.start.elapsed().as_millis() as u64;
        crate::anim::set_clock_ms(now_ms);
        // 惯性滑动：在布局前推进 scroll_y，本帧 arrange 据此钳制并重排。
        self.step_fling(now_ms);
        // pixmap 是物理像素；布局用逻辑坐标（物理 / scale），绘制时再 ×scale 放大。
        let s = self.scale;
        let logical = Size::new(
            (size.w as f32 / s).round().max(1.0) as i32,
            (size.h as f32 / s).round().max(1.0) as i32,
        );
        self.logical_size = logical;

        // 全窗 vs 局部重绘决策：
        // - needs_full（输入/结构/尺寸变更）、后备缓冲缺失/尺寸不符、有浮层、无脏区 → 全窗。
        // - 否则用上一帧动画脏区做局部重绘（仅重画动的那一小块，高 DPI 也稳 60fps）。
        let back_ok = self
            .back
            .as_ref()
            .map(|b| b.width() == size.w as u32 && b.height() == size.h as u32)
            .unwrap_or(false);
        let overlay = self.menu.is_some()
            || (!self.tooltip_suppressed
                && self.hover.and_then(|h| self.tree.node_tooltip(h)).is_some());
        let damage = self.pending_damage.take();
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
        let do_full = self.needs_full || !back_ok || overlay || !scale_ok || !damage_small;
        self.needs_full = false;

        if !do_full {
            self.render_partial(pixmap, size, s, damage.unwrap());
            self.pending_damage = next_damage(&mut self.needs_full);
            return;
        }

        // ---- 全窗重绘：完整布局 + 整树绘制 + 浮层；结果种入后备缓冲供后续局部帧复用。----
        self.tree.layout_root(logical, &mut self.engine);
        // 布局后结构稳定，刷新 Tab 焦点顺序。
        self.focus_order = self.tree.focusable_order();
        // 若当前焦点已不在可聚焦集合中（结构变更），归一化为无焦点。
        if let Some(f) = self.focus {
            if !self.focus_order.contains(&f) {
                self.tree.set_focused(None, Some(f));
                self.focus = None;
            }
        }
        self.tree.focus_ring_visible = self.focus_visible;
        let mut canvas = SkiaCanvas::with_text(pixmap, &mut self.engine, s);
        self.tree.paint(&mut canvas);
        // 上下文菜单浮层绘制在控件树之上（self.menu 与 self.engine 为不相交字段，借用安全）。
        if let Some(menu) = self.menu.as_ref() {
            let (pal, mt) = (&self.theme.palette, &self.theme.menu);
            let r = menu.rect;
            canvas.fill_round_rect(r.x as f32, r.y as f32, r.w as f32, r.h as f32, 8.0, &Paint::fill(mt.bg(pal)));
            canvas.stroke_round_rect(r.x as f32, r.y as f32, r.w as f32, r.h as f32, 8.0, 1.0, &Paint::fill(mt.border(pal)));
            for (i, it) in menu.items.iter().enumerate() {
                let iy = menu.item_y(i);
                if menu.hover == Some(i) && it.enabled {
                    canvas.fill_round_rect((r.x + 4) as f32, iy as f32, (r.w - 8) as f32, MENU_ITEM_H as f32, 5.0, &Paint::fill(mt.hover(pal)));
                }
                // 选中项用强调色 + 行尾勾选标记（下拉当前项）。
                let color = if !it.enabled {
                    mt.text_disabled(pal)
                } else if it.checked {
                    mt.accent(pal)
                } else {
                    mt.text(pal)
                };
                let tr = Rect::new(r.x + MENU_PAD_X, iy, r.w - 2 * MENU_PAD_X, MENU_ITEM_H);
                canvas.draw_text(&it.label, tr, color, crate::spec::Align::Start, None, MENU_FONT);
                if it.checked {
                    let cr = Rect::new(r.x, iy, r.w - MENU_PAD_X, MENU_ITEM_H);
                    canvas.draw_text("\u{2713}", cr, mt.accent(pal), crate::spec::Align::End, None, MENU_FONT);
                }
            }
        }
        // 悬停提示浮层（菜单激活时不显示）：悬停节点带 tooltip 且停留超过延时则弹出；
        // 未到延时则请求下一帧——鼠标静止后无事件，需靠 anim 续帧推进计时（与不确定进度条同源）。
        if self.menu.is_none() && !self.tooltip_suppressed {
            if let Some(text) = self.hover.and_then(|h| self.tree.node_tooltip(h)) {
                if now_ms.saturating_sub(self.hover_since_ms) < TOOLTIP_DELAY_MS {
                    crate::anim::request_repaint();
                } else {
                    let (pal, tt) = (&self.theme.palette, &self.theme.tooltip);
                    let ts = canvas.measure_text(&text, None, TOOLTIP_FONT);
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
                    canvas.fill_round_rect(x as f32, y as f32, w as f32, h as f32, corner, &Paint::fill(tt.bg(pal)));
                    let tr = Rect::new(x + TOOLTIP_PAD_X, y, w - 2 * TOOLTIP_PAD_X, h);
                    canvas.draw_text(&text, tr, tt.text(pal), crate::spec::Align::Start, None, TOOLTIP_FONT);
                }
            }
        }
        drop(canvas);
        // 种入后备缓冲（整窗），供后续局部帧重建未变区域。
        self.seed_back(pixmap, size);
        self.pending_damage = next_damage(&mut self.needs_full);
    }

    fn on_pointer(&mut self, mut ev: crate::event::PointerEvent) -> bool {
        self.needs_full = true;
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
        let mut hover = self.hover;
        let mut capture = self.capture;
        let res = self.tree.dispatch_pointer(ev, &mut hover, &mut capture);
        self.hover = hover;
        self.capture = capture;
        // 悬停提示：记录指针位置；悬停节点变化时重新计时（隐藏旧提示、对新节点计时）。
        // 按下抑制提示（点完控件不原地弹出盖住它），指针再次移动后解除抑制并重新计时。
        self.hover_pos = ev.pos;
        let now_ms = self.start.elapsed().as_millis() as u64;
        if hover != old_hover {
            self.hover_since_ms = now_ms;
            self.tooltip_suppressed = false;
        }
        match ev.kind {
            PointerKind::Down => self.tooltip_suppressed = true,
            PointerKind::Move if self.tooltip_suppressed => {
                self.tooltip_suppressed = false;
                self.hover_since_ms = now_ms;
            }
            _ => {}
        }
        if let Some(f) = res.focus {
            let old = self.focus;
            self.tree.set_focused(Some(f), old);
            self.focus = Some(f);
            // 鼠标聚焦：不显示焦点环，保持纯鼠标操作的纯净观感。
            self.focus_visible = false;
        }
        if res.close {
            self.close = true;
        }
        // 控件请求弹出上下文菜单（目标为刚获焦的控件）。
        if let Some(req) = res.menu {
            if let Some(target) = self.focus {
                self.open_menu(req, target);
            }
        }
        // 控件请求打开 URL/路径（链接点击）：交平台用默认程序打开。
        if let Some(url) = res.open_url {
            platform::open_url(&url);
        }
        // 窗口操作（自定义标题栏按钮）：暂存，平台分发后轮询执行（需 hwnd）。
        if res.window_op.is_some() {
            self.pending_window_op = res.window_op;
        }
        res.repaint
    }

    fn on_key(&mut self, ev: crate::event::KeyEvent) -> bool {
        self.needs_full = true;
        // 菜单激活时：Escape 关闭，其余键吞掉（避免在菜单后误编辑）。
        if self.menu.is_some() {
            if ev.key == Key::Escape {
                self.menu = None;
            }
            return true;
        }
        // Tab 由宿主独占用于焦点导航，并启用焦点环显示。
        if ev.key == Key::Tab {
            self.focus_visible = true;
            return self.move_focus(!ev.shift);
        }
        // 其余键先交给焦点控件；未被消费的 Escape 回退为关闭窗口。
        let res = self.tree.dispatch_key(ev, self.focus);
        if res.close {
            self.close = true;
        }
        if let Some(url) = res.open_url {
            platform::open_url(&url);
        }
        if res.window_op.is_some() {
            self.pending_window_op = res.window_op;
        }
        if !res.consumed && ev.key == Key::Escape {
            self.close = true;
        }
        res.repaint
    }

    fn wants_close(&self) -> bool {
        self.close
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
        let p = Point::new((pos.x as f32 / s).round() as i32, (pos.y as f32 / s).round() as i32);
        let res = self.tree.dispatch_files(p, paths);
        if res.close {
            self.close = true;
        }
        if let Some(url) = res.open_url {
            platform::open_url(&url);
        }
        res.repaint
    }

    fn window_drag_at(&self, pos: Point) -> bool {
        // 菜单浮层激活时不拖窗。物理 → 逻辑后查拖动区。
        if self.menu.is_some() {
            return false;
        }
        let s = self.scale;
        let p = Point::new((pos.x as f32 / s).round() as i32, (pos.y as f32 / s).round() as i32);
        self.tree.drag_hit_at(p)
    }

    fn interactive_at(&self, pos: Point) -> bool {
        // 物理 → 逻辑后查是否命中可聚焦控件（窗口按钮等）。
        let s = self.scale;
        let p = Point::new((pos.x as f32 / s).round() as i32, (pos.y as f32 / s).round() as i32);
        self.tree.interactive_hit_at(p)
    }

    fn take_window_op(&mut self) -> Option<WindowOp> {
        self.pending_window_op.take()
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
        let p = Point::new((pos.x as f32 / s).round() as i32, (pos.y as f32 / s).round() as i32);
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
        let p = Point::new((pos.x as f32 / s).round() as i32, (pos.y as f32 / s).round() as i32);
        let Some(node) = self.tree.scroll_node_at(p) else {
            return false;
        };
        // scroll_y 速度 = −手指速度（手指上移 vy<0 → 内容上移、scroll_y 增大）；物理→逻辑。
        let vel = -vy / s;
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
        let raw = damage.inflate(DAMAGE_MARGIN).intersect(&Rect::from_size(self.logical_size));
        // 原点对齐到 4 逻辑像素网格：Windows DPI 缩放恒为 25% 的倍数（scale=m/4），故 4 的倍数 ×scale
        // 必为整数，子 pixmap 物理原点 dmg.origin×scale 精确无取整 → 文字定位与全窗帧逐像素一致，
        // 消除局部帧的纵向 1px 抖动。
        const GRID: i32 = 4;
        let x0 = raw.x - raw.x.rem_euclid(GRID);
        let y0 = raw.y - raw.y.rem_euclid(GRID);
        let x1 = raw.right() + (GRID - raw.right().rem_euclid(GRID)) % GRID;
        let y1 = raw.bottom() + (GRID - raw.bottom().rem_euclid(GRID)) % GRID;
        let dmg = Rect::new(x0, y0, x1 - x0, y1 - y0).intersect(&Rect::from_size(self.logical_size));
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
        sub.fill(tiny_skia::Color::from_rgba8(self.bg.r, self.bg.g, self.bg.b, self.bg.a));
        // 以脏区左上角（逻辑）为偏移绘制整树：框外图元由 tiny-skia 廉价剔除。
        {
            let mut canvas =
                SkiaCanvas::with_text_offset(&mut sub, &mut self.engine, s, Point::new(dmg.x, dmg.y));
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
    debug_assert!(x + sw <= dw && y + sh <= dh, "blit 越界：({x},{y})+{sw}x{sh} 超出 {dw}x{dh}");
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
        let app = App::new("t", 100, 100)
            .on_interval(std::time::Duration::from_millis(100), || {});
        assert_eq!(app.intervals.len(), 1);
    }

    #[test]
    fn render_drains_pending_messages() {
        use crate::platform::AppHandler;
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
        handler.render(&mut pm, Size::new(50, 50));
        assert_eq!(got.get(), 7, "render 前排空 pump，消息写入状态");
    }
}
