//! macOS 系统托盘（`NSStatusItem`）：图标 + 提示 + 左键/双击回调 + 原生右键菜单。
//!
//! 构建器 API（`Tray` / `TrayMenuItem` / `TrayCtx`）不在本模块——它们是**平台无关的
//! 声明层**，收在 [`crate::platform::tray`]，本模块只负责执行 `TrayAction`。此前这里有
//! 一份完整副本且**立即调 OS**，与 win32 的意图队列语义不同：同一段回调在两个平台上
//! 行为不一致，而 macOS 那份因为要持有真 `NSWindow`，托盘回调在下游根本没法测。
//! 左键单击/双击触发回调，
//! 右键弹原生 `NSMenu`（勾选项按 `Signal<bool>` 当前值显示对勾、分隔线）。气泡走
//! `NSUserNotification`（已弃用；未打包为 .app 时系统可能不展示，属系统限制）。
//!
//! 实现要点：状态项按钮的 `target` 是**弱引用**，故承载回调闭包的 `TrayTarget` 必须由
//! `TrayState` 强持有（随窗口存续）；窗口销毁时 `TrayState::drop` 从状态栏移除图标。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;

pub(crate) use crate::platform::tray::{invoke, ItemKind, Tray, TrayAction};

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

thread_local! {
    /// 已安装的状态栏图标。
    ///
    /// `TrayHandle` 的运行期意图要够得着它，而 `TrayState` 是 `run` 里的一个局部变量
    /// ——window.rs 的 `after_event` 拿不到。win32 那边靠 `app_host()` 上挂着的
    /// `TrayState`，这里补一个同样作用的登记处。
    static INSTALLED: RefCell<Option<Retained<NSStatusItem>>> = const { RefCell::new(None) };
}

/// 落实运行期托盘意图（`TrayHandle::set_tooltip`），与 win32 的 `apply_tray_ops` 对齐。
///
/// 没装托盘时意图被丢弃而不是攒着：一个没有托盘的应用改托盘提示是调用方的错，
/// 攒起来只会让它在某天真装了托盘时突然生效，那更难查。
pub(crate) fn apply_tray_ops() {
    let ops = crate::platform::tray::take_tray_ops();
    if ops.is_empty() {
        return;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    INSTALLED.with(|cell| {
        let borrowed = cell.borrow();
        let Some(item) = borrowed.as_ref() else {
            return;
        };
        let Some(button) = item.button(mtm) else {
            return;
        };
        for op in ops {
            match op {
                crate::platform::tray::TrayOp::SetTooltip(s) => {
                    button.setToolTip(Some(&NSString::from_str(&s)));
                }
            }
        }
    });
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

    /// 触发左键单/双击回调，随即执行它请求的意图。
    ///
    /// **借用必须在执行之前释放**：`tray` 是 `RefCell`，而 `TrayAction::Show` 会
    /// `makeKeyAndOrderFront` + 激活应用，这条路径可能同步回调进来（激活会派发事件）；
    /// 那时若还持着 `borrow_mut`，就是 `already borrowed` panic。故先收意图、丢借用、
    /// 再执行——与 win32 「释放 `&mut WindowState` 之后再跑意图」是同一条铁律的两个面。
    fn invoke_click(&self, double: bool) {
        let actions = {
            let mut tray = self.ivars().tray.borrow_mut();
            let cb = if double {
                tray.on_double_click.as_mut()
            } else {
                tray.on_left_click.as_mut()
            };
            invoke(cb)
        };
        self.run_actions(actions);
    }

    /// 执行托盘意图。窗口显隐与退出的语义须与热键/事件路径一致，故显隐复用同一组
    /// NSWindow 调用。
    ///
    /// `Quit` 处**截断**其后的意图：`NSApp::terminate` 本就不返回，显式 break 让这个
    /// 事实可读，也与 win32 的 `run_tray_actions` 在构造上一致（那边窗口已销毁，
    /// 后续意图同样无从生效）。
    fn run_actions(&self, actions: Vec<TrayAction>) {
        let window = self.ivars().window.clone();
        for action in actions {
            match action {
                // 与控件请求、全局热键共用 `show_and_activate`：唤起是同一语义，
                // 三处各写一份必然走偏（此前这份就漏了 deminiaturize）。
                TrayAction::Show => {
                    if let Some(mtm) = MainThreadMarker::new() {
                        super::window::show_and_activate(&window, mtm);
                    }
                }
                TrayAction::Hide => window.orderOut(None),
                TrayAction::Quit => {
                    if let Some(mtm) = MainThreadMarker::new() {
                        NSApplication::sharedApplication(mtm).terminate(None);
                    }
                    break;
                }
                TrayAction::Notify { title, body } => deliver_notification(&title, &body),
            }
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
                        let on = checked.is_some_and(|c| c.get());
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
        // 借用范围止于收完意图，理由同 `invoke_click`。
        let actions = invoke(self.ivars().tray.borrow_mut().item_callback(idx));
        self.run_actions(actions);
    }
}

/// 运行期托盘状态：强持有 target（按钮 target 为弱引用）与 status item；drop 时移除图标。
pub(crate) struct TrayState {
    status_item: Retained<NSStatusItem>,
    _target: Retained<TrayTarget>,
}

impl Drop for TrayState {
    fn drop(&mut self) {
        INSTALLED.with(|cell| *cell.borrow_mut() = None);
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
    INSTALLED.with(|cell| *cell.borrow_mut() = Some(status_item.clone()));
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
