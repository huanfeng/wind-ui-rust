//! macOS 系统托盘（`NSStatusItem`）：图标 + 提示 + 左键/双击回调 + 原生右键菜单。
//!
//! 公共构建器 API（`Tray` / `TrayMenuItem` / `TrayCtx`）与 win32 同形。左键单击/双击触发回调，
//! 右键弹原生 `NSMenu`（勾选项按 `Rc<Cell>` 当前值显示对勾、分隔线）。气泡走
//! `NSUserNotification`（已弃用；未打包为 .app 时系统可能不展示，属系统限制）。
//!
//! 实现要点：状态项按钮的 `target` 是**弱引用**，故承载回调闭包的 `TrayTarget` 必须由
//! `TrayState` 强持有（随窗口存续）；窗口销毁时 `TrayState::drop` 从状态栏移除图标。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;

use crate::signal::Signal;

use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly};

use objc2_app_kit::{
    NSApplication, NSControlStateValueOff, NSControlStateValueOn, NSEventMask, NSEventType,
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength, NSWindow,
};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGImageAlphaInfo,
};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSSize, NSString};

type TrayFn = Box<dyn FnMut(&mut TrayCtx)>;

/// 托盘回调上下文：操作窗口与弹通知（不暴露原生句柄）。
pub struct TrayCtx {
    window: Retained<NSWindow>,
    status_item: Retained<NSStatusItem>,
}

impl TrayCtx {
    /// 显示并前置窗口（托盘最常见动作）。
    pub fn show_window(&self) {
        if let Some(mtm) = MainThreadMarker::new() {
            self.window.makeKeyAndOrderFront(None);
            NSApplication::sharedApplication(mtm).activate();
        }
    }
    /// 隐藏窗口（最小化到托盘）。
    pub fn hide_window(&self) {
        self.window.orderOut(None);
    }
    /// 退出应用。
    pub fn quit(&self) {
        if let Some(mtm) = MainThreadMarker::new() {
            NSApplication::sharedApplication(mtm).terminate(None);
        }
    }
    /// 弹出系统通知（标题 + 正文）。未打包为 .app 时可能不展示。
    pub fn notify(&self, title: &str, body: &str) {
        deliver_notification(title, body);
        let _ = &self.status_item; // 保留字段（未来可用 status item 锚定通知）。
    }
}

#[allow(deprecated)]
fn deliver_notification(title: &str, body: &str) {
    use objc2::ClassType;
    use objc2_foundation::{NSUserNotification, NSUserNotificationCenter};
    // 未打包为 .app 时 `defaultUserNotificationCenter` 可能为 nil；生成的绑定会对非空断言而崩溃，
    // 故用裸消息取可空值并提前返回（无 bundle 即静默跳过通知，不影响其余托盘功能）。
    let center: Option<Retained<NSUserNotificationCenter>> = unsafe {
        msg_send![
            NSUserNotificationCenter::class(),
            defaultUserNotificationCenter
        ]
    };
    let Some(center) = center else { return };
    let note = NSUserNotification::new();
    note.setTitle(Some(&NSString::from_str(title)));
    note.setInformativeText(Some(&NSString::from_str(body)));
    center.deliverNotification(&note);
}

enum ItemKind {
    Action {
        label: String,
        checked: Option<Rc<Cell<bool>>>,
        /// 禁用态绑定（None=始终可用）；菜单弹出时读当前值，false 则灰显且不可点。
        enabled: Option<Signal<bool>>,
        cb: TrayFn,
    },
    Separator,
}

/// 托盘右键菜单项：普通项 / 勾选项 / 分隔线。
pub struct TrayMenuItem {
    kind: ItemKind,
}

impl TrayMenuItem {
    /// 普通项：点击触发回调。
    pub fn item(label: impl Into<String>, cb: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        Self {
            kind: ItemKind::Action {
                label: label.into(),
                checked: None,
                enabled: None,
                cb: Box::new(cb),
            },
        }
    }
    /// 勾选项：`checked` 绑定状态，菜单弹出时按当前值显示对勾；点击触发回调
    /// （回调内自行翻转 `checked`，框架不自动改）。
    pub fn check(
        label: impl Into<String>,
        checked: Rc<Cell<bool>>,
        cb: impl FnMut(&mut TrayCtx) + 'static,
    ) -> Self {
        Self {
            kind: ItemKind::Action {
                label: label.into(),
                checked: Some(checked),
                enabled: None,
                cb: Box::new(cb),
            },
        }
    }
    /// 绑定禁用态：`flag` 为 false 时该项灰显且不可点（菜单弹出时读当前值）。
    /// 对分隔线无效。永久禁用可传 `signal(false)`。
    pub fn enabled(mut self, flag: Signal<bool>) -> Self {
        if let ItemKind::Action { enabled, .. } = &mut self.kind {
            *enabled = Some(flag);
        }
        self
    }
    /// 分隔线。
    pub fn separator() -> Self {
        Self {
            kind: ItemKind::Separator,
        }
    }
}

/// 托盘图标构建器。交给 `App::tray(...)`。
#[derive(Default)]
pub struct Tray {
    tooltip: String,
    icon: Option<(u32, u32, Vec<u8>)>,
    on_left_click: Option<TrayFn>,
    on_double_click: Option<TrayFn>,
    items: Vec<TrayMenuItem>,
}

impl Tray {
    pub fn new() -> Self {
        Self::default()
    }
    /// 鼠标悬停提示。
    pub fn tooltip(mut self, s: impl Into<String>) -> Self {
        self.tooltip = s.into();
        self
    }
    /// 自定义图标：原始非预乘 RGBA8（`rgba.len()==w*h*4`）。未设则用系统默认图标。
    pub fn icon_rgba(mut self, w: u32, h: u32, rgba: &[u8]) -> Self {
        self.icon = Some((w, h, rgba.to_vec()));
        self
    }
    /// 左键单击回调（常见用于显隐窗口）。
    pub fn on_left_click(mut self, f: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        self.on_left_click = Some(Box::new(f));
        self
    }
    /// 左键双击回调。
    pub fn on_double_click(mut self, f: impl FnMut(&mut TrayCtx) + 'static) -> Self {
        self.on_double_click = Some(Box::new(f));
        self
    }
    /// 右键菜单项（普通/勾选/分隔线）。
    pub fn menu(mut self, items: Vec<TrayMenuItem>) -> Self {
        self.items = items;
        self
    }
}

/// `TrayTarget` 的内部状态。
struct TargetIvars {
    tray: RefCell<Tray>,
    window: Retained<NSWindow>,
    status_item: RefCell<Option<Retained<NSStatusItem>>>,
    /// 模态菜单选中项下标（-1=无）。菜单回调发生在 `popUpStatusItemMenu` 的嵌套模态
    /// 循环内，若就地执行用户闭包易在嵌套模态中重入崩溃；故仅记录，模态结束后再分发
    /// （对齐 win32 `TrackPopupMenu(TPM_RETURNCMD)` 的「关闭后再回调」语义）。
    pending: Cell<isize>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WindUiTrayTarget"]
    #[ivars = TargetIvars]
    struct TrayTarget;

    unsafe impl NSObjectProtocol for TrayTarget {}

    impl TrayTarget {
        /// 状态项按钮点击：左键→回调（单/双击），右键或 Ctrl+左键→弹菜单。
        #[unsafe(method(statusClick:))]
        fn status_click(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let app = NSApplication::sharedApplication(mtm);
            let (ty, clicks, ctrl) = match app.currentEvent() {
                Some(ev) => {
                    let flags = ev.modifierFlags();
                    (ev.r#type(), ev.clickCount(), flags.contains(objc2_app_kit::NSEventModifierFlags::Control))
                }
                None => (NSEventType::LeftMouseUp, 1, false),
            };
            let is_right = ty == NSEventType::RightMouseUp || ctrl;
            if is_right {
                self.pop_menu(mtm);
            } else {
                self.invoke_click(clicks >= 2);
            }
        }

        /// 菜单项点击：仅记录选中下标，待模态菜单关闭后再分发（见 `pop_menu`）。
        #[unsafe(method(menuClick:))]
        fn menu_click(&self, sender: &NSMenuItem) {
            self.ivars().pending.set(sender.tag());
        }
    }
);

impl TrayTarget {
    fn new(mtm: MainThreadMarker, tray: Tray, window: Retained<NSWindow>) -> Retained<Self> {
        let ivars = TargetIvars {
            tray: RefCell::new(tray),
            window,
            status_item: RefCell::new(None),
            pending: Cell::new(-1),
        };
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    /// 构造一次性回调上下文（克隆 window + status item 的强引用）。
    fn ctx(&self) -> TrayCtx {
        let status_item = self.ivars().status_item.borrow().as_ref().unwrap().clone();
        TrayCtx {
            window: self.ivars().window.clone(),
            status_item,
        }
    }

    /// 触发左键单/双击回调。
    fn invoke_click(&self, double: bool) {
        let mut ctx = self.ctx();
        let mut tray = self.ivars().tray.borrow_mut();
        let cb = if double {
            tray.on_double_click.as_mut()
        } else {
            tray.on_left_click.as_mut()
        };
        if let Some(cb) = cb {
            cb(&mut ctx);
        }
    }

    /// 按当前菜单项数据（含勾选状态）构建并弹出原生菜单。
    // popUpStatusItemMenu 已弃用，但它是「左键回调 + 右键菜单」分流下按需弹出的最简方式。
    #[allow(deprecated)]
    fn pop_menu(&self, mtm: MainThreadMarker) {
        let menu = NSMenu::new(mtm);
        // 默认 autoenablesItems=YES 会按「target 是否响应 action」自动决定可用态，
        // 覆盖我们的 setEnabled:；关掉以手动控制禁用态。
        menu.setAutoenablesItems(false);
        {
            let tray = self.ivars().tray.borrow();
            for (i, it) in tray.items.iter().enumerate() {
                match &it.kind {
                    ItemKind::Separator => menu.addItem(&NSMenuItem::separatorItem(mtm)),
                    ItemKind::Action {
                        label,
                        checked,
                        enabled,
                        ..
                    } => {
                        let item = unsafe {
                            NSMenuItem::initWithTitle_action_keyEquivalent(
                                NSMenuItem::alloc(mtm),
                                &NSString::from_str(label),
                                Some(sel!(menuClick:)),
                                &NSString::from_str(""),
                            )
                        };
                        item.setTag(i as isize);
                        unsafe { item.setTarget(Some(self)) };
                        let on = checked.as_ref().is_some_and(|c| c.get());
                        item.setState(if on {
                            NSControlStateValueOn
                        } else {
                            NSControlStateValueOff
                        });
                        // 禁用态：enabled 绑定为 false 则灰显不可点（默认可用）。
                        let usable = enabled.map(|e| e.get()).unwrap_or(true);
                        item.setEnabled(usable);
                        menu.addItem(&item);
                    }
                }
            }
        }
        // 模态前克隆 status item，避免跨嵌套模态循环持有 RefCell 借用。
        let si = match self.ivars().status_item.borrow().as_ref() {
            Some(s) => s.clone(),
            None => return,
        };
        self.ivars().pending.set(-1);
        si.popUpStatusItemMenu(&menu); // 阻塞：选中项在此期间经 menu_click 记入 pending
        let idx = self.ivars().pending.replace(-1);
        if idx >= 0 {
            self.invoke_menu(idx as usize);
        }
    }

    /// 在模态菜单关闭后执行选中项回调（不在嵌套模态中，重入安全）。
    fn invoke_menu(&self, idx: usize) {
        let mut ctx = self.ctx();
        let mut tray = self.ivars().tray.borrow_mut();
        if let Some(it) = tray.items.get_mut(idx) {
            if let ItemKind::Action { cb, .. } = &mut it.kind {
                cb(&mut ctx);
            }
        }
    }
}

/// 运行期托盘状态：强持有 target（按钮 target 为弱引用）与 status item；drop 时移除图标。
pub(crate) struct TrayState {
    status_item: Retained<NSStatusItem>,
    _target: Retained<TrayTarget>,
}

impl Drop for TrayState {
    fn drop(&mut self) {
        if let Some(mtm) = MainThreadMarker::new() {
            NSStatusBar::systemStatusBar().removeStatusItem(&self.status_item);
            let _ = mtm;
        }
    }
}

/// 安装托盘图标。窗口创建后调用，状态存入 `TrayState`（窗口销毁时清理）。
pub(crate) fn install(
    mtm: MainThreadMarker,
    window: Retained<NSWindow>,
    tray: Tray,
) -> Option<TrayState> {
    let icon = tray.icon.clone();
    let tooltip = tray.tooltip.clone();

    let target = TrayTarget::new(mtm, tray, window);

    let status_item =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    let button = status_item.button(mtm)?;

    if let Some((w, h, rgba)) = icon {
        if let Some(img) = nsimage_from_rgba(w as i32, h as i32, &rgba) {
            button.setImage(Some(&img));
        }
    }
    if !tooltip.is_empty() {
        button.setToolTip(Some(&NSString::from_str(&tooltip)));
    }
    // 左键/右键均派发到 statusClick:，由其按事件类型分流。
    unsafe {
        button.setTarget(Some(&target));
        button.setAction(Some(sel!(statusClick:)));
        button.sendActionOn(NSEventMask::LeftMouseUp | NSEventMask::RightMouseUp);
    }

    *target.ivars().status_item.borrow_mut() = Some(status_item.clone());

    Some(TrayState {
        status_item,
        _target: target,
    })
}

/// 非预乘 RGBA8 → NSImage（预乘进位图上下文后转 CGImage）。
fn nsimage_from_rgba(w: i32, h: i32, rgba: &[u8]) -> Option<Retained<NSImage>> {
    if w <= 0 || h <= 0 || rgba.len() < (w * h * 4) as usize {
        return None;
    }
    // 预乘（图标多为不透明，alpha=255 时即直通）。
    let mut buf = vec![0u8; (w * h * 4) as usize];
    for i in 0..(w * h) as usize {
        let s = i * 4;
        let a = rgba[s + 3] as u32;
        buf[s] = (rgba[s] as u32 * a / 255) as u8;
        buf[s + 1] = (rgba[s + 1] as u32 * a / 255) as u8;
        buf[s + 2] = (rgba[s + 2] as u32 * a / 255) as u8;
        buf[s + 3] = a as u8;
    }
    let cs = CGColorSpace::new_device_rgb()?;
    let image = unsafe {
        let ctx = CGBitmapContextCreate(
            buf.as_mut_ptr() as *mut c_void,
            w as usize,
            h as usize,
            8,
            (w * 4) as usize,
            Some(&cs),
            CGImageAlphaInfo::PremultipliedLast.0,
        )?;
        CGBitmapContextCreateImage(Some(&ctx))?
    };
    let size = NSSize {
        width: w as f64,
        height: h as f64,
    };
    let nsimage = NSImage::initWithCGImage_size(NSImage::alloc(), &image, size);
    // 彩色图标（非模板），避免被渲染成单色。
    nsimage.setTemplate(false);
    Some(nsimage)
}
