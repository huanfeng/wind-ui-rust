//! macOS 窗口、事件循环与呈现（Cocoa/AppKit + Core Graphics）。
//!
//! 默认渲染全在 CPU：单份 tiny-skia `Pixmap`（RGBA 预乘）作后备缓冲；呈现时用
//! `CGBitmapContext` 把缓冲转成 `CGImage`，在自定义 `NSView::drawRect:` 里
//! `CGContextDrawImage` 拷屏。空闲时阻塞在 `NSApplication` 的 run loop，零阻塞渲染。
//!
//! 开 `gpu` feature 且 [`Renderer`] 选了 GPU 档时另有一条路：内容视图挂 `CAMetalLayer`，
//! wgpu 从该 layer 建 surface，出帧点从 `drawRect:` 换成 `updateLayer`（见 [`attach_gpu`] 与
//! `ContentView::draw_gpu`）。两条路径在**窗口创建时二选一**，运行期不切换；GPU 建不起来时
//! `Renderer::Auto` 静默回退本文件的软路径。软路径的代码一行未动。
//!
//! 对照 `platform/win32/mod.rs`（消息循环 + GDI 呈现）。坐标统一：事件按
//! **物理像素、相对客户区左上角**上交（视图设为 `isFlipped`，故点坐标即左上原点）。

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::path::PathBuf;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject, Sel};
use objc2::{define_class, msg_send, sel, AllocAnyThread, DefinedClass, MainThreadOnly};

use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSCursor, NSDragOperation,
    NSDraggingDestination, NSDraggingInfo, NSEvent, NSEventPhase, NSGraphicsContext,
    NSPasteboardType, NSScreen, NSTextInputClient, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSWindow, NSWindowButton, NSWindowDelegate, NSWindowStyleMask, NSWindowTitleVisibility,
};
// 已弃用但在现行 macOS 仍有效，且读取拖入路径列表最简。
#[allow(deprecated)]
use objc2_app_kit::NSFilenamesPboardType;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGContext, CGDataProvider, CGImage,
    CGImageAlphaInfo,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSAttributedString, NSAttributedStringKey, NSNotFound,
    NSNotification, NSObjectProtocol, NSPoint, NSRange, NSRangePointer, NSRect, NSSize, NSString,
    NSTimer, NSUInteger,
};

#[cfg(feature = "gpu")]
use objc2_quartz_core::{CAMetalLayer, CATransaction};

use tiny_skia::Pixmap;

use super::{AppHandler, NewWindow, WindowConfig};
use crate::event::{Key, KeyEvent, MouseButton, PointerEvent, PointerKind, WindowOp};
use crate::geometry::{Color, Point, Size};
use crate::platform::{to_skia_color, Renderer};
#[cfg(feature = "gpu")]
use crate::render::gpu::{FrameError, SharedGpu, WindowGpu};

thread_local! {
    /// 仍存活的 windui 窗口，**并且是它们的所有者**——对照 win32 的 `LiveWindows`。
    ///
    /// 那边存 `HWND`（窗口由 OS 拥有，登记表只是名册）；这边必须存 `Retained`：从代码
    /// 创建的 `NSWindow` 默认 `releasedWhenClosed = true`，`alloc/init` 给出的 +1 会在
    /// `close` 时被 AppKit 抵消掉，我们手上的 `Retained` 随即悬垂。故建窗时一律
    /// `setReleasedWhenClosed(false)`，把生命周期完全交给这张表：表里在，窗口在；移出
    /// 并释放，`NSWindow` → `ContentView` → `ViewState` → `handler` 顺次析构，子窗的
    /// 信号也就此整批回收（`UiHost::scope`）。
    ///
    /// 单窗口时代看不到这个问题：`terminate` 直接结束进程，那份 `Retained` 从未 drop 过。
    static WINDOWS: RefCell<Vec<LiveWindow>> = const { RefCell::new(Vec::new()) };
    /// 已注销、等待延迟释放的窗口，见 [`unregister_window`]。
    static CLOSED: RefCell<Vec<Retained<NSWindow>>> = const { RefCell::new(Vec::new()) };
    /// 应用已进入退出流程（`terminate` 已发出），见 [`ContentView::window_will_close`]。
    static TERMINATING: Cell<bool> = const { Cell::new(false) };
    /// 主窗——`App::run` 建的那一个。
    ///
    /// 托盘点击与全局热键说的"唤出窗口"指的都是它；子窗（设置页之类）不是这些操作的
    /// 对象。对照 win32 `AppHost::main`。不取 `WINDOWS[0]`：主窗关掉之后那个位置会变成
    /// 别的窗口，而"热键唤出的是哪个窗口"不该随之改变。
    static MAIN_WINDOW: RefCell<Option<Retained<NSWindow>>> = const { RefCell::new(None) };
    /// 应用选定的渲染后端档位（`App::renderer`）。
    ///
    /// 子窗（`ctx.open_window`）的 `WindowConfig` 由应用层现构造，那里不知道主窗当初选了
    /// 哪个后端，而"主窗跑 GPU、子窗悄悄退回软件"是没人想要的结果。对照 win32：那边存在
    /// `AppHost` 上，这边没有 App 级宿主对象，就近放进这张线程局部表。
    static APP_RENDERER: Cell<Renderer> = const { Cell::new(Renderer::Software) };
}

/// 对主窗执行一个窗口操作。全局热键的回调声明意图后由此落地。
///
/// **调用方须已释放各类借用**：这里的 AppKit 调用会同步回调进视图（激活窗口触发
/// `windowDidResignKey:` 之类），届时会再借 `ViewState`。对照 win32 的 `run_window_op`。
pub(super) fn run_window_op_on_main(op: WindowOp) {
    let Some(win) = MAIN_WINDOW.with(|w| w.borrow().clone()) else {
        return;
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    match op {
        WindowOp::Minimize => win.miniaturize(None),
        WindowOp::ToggleMaximize => win.zoom(None),
        // 三个入口（托盘点击 / 控件请求 / 全局热键）共用 `show_and_activate`，不再各写一份。
        WindowOp::Show => show_and_activate(&win, mtm),
        WindowOp::Hide => win.orderOut(None),
    }
}

/// 显示并前置窗口——**唤起语义的唯一实现**。
///
/// 三个入口（托盘点击 / 控件请求 `WindowOp::Show` / 全局热键）此前各写一份，注释也写着
/// "实现必须一致"，但实际已经走偏：`run_window_op_on_main` 会先 `deminiaturize`，
/// `after_event` 那份不会——最小化状态下从控件请求唤起，窗口不会还原。收口到这里之后
/// 那处缺陷一并消失，唤起通知也只需挂一个点。
///
/// 隐藏→可见的**跃迁**上通知宿主（见 [`notify_shown`]）。已经可见时再调不算唤起，
/// 否则常驻工具每按一次热键都会重置一遍界面状态。
pub(super) fn show_and_activate(win: &NSWindow, mtm: MainThreadMarker) {
    // 先问再显示：`makeKeyAndOrderFront` 之后 `isVisible` 恒为真，跃迁就无从判断了。
    let was_hidden = !win.isVisible();
    // 最小化时仅 makeKeyAndOrderFront 不会还原窗口，先取消最小化。
    if win.isMiniaturized() {
        win.deminiaturize(None);
    }
    win.makeKeyAndOrderFront(None);
    // 热键是在别的应用前台时按的，本应用多半不在激活态——不 activate 的话窗口只是排到
    // 自己应用的最前，用户仍看不到它。
    super::activate_app(&NSApplication::sharedApplication(mtm));
    if was_hidden {
        notify_shown(win);
    }
    // 帧驱动在隐藏期间是断链的（`schedule_next_frame` 见不可见即不续约，链式定时器
    // 一旦停就没人再起）。这里补一次起链：否则窗口回来了，光标不闪、补间不动，
    // 直到用户碰一下界面才恢复。放在最后，前面那些调用都不得持有 `ViewState` 借用。
    if let Some(view) = win
        .contentView()
        .and_then(|v| v.downcast::<ContentView>().ok())
    {
        view.schedule_next_frame();
    }
}

/// 通知宿主"窗口刚被唤起"。
///
/// 放在 `makeKeyAndOrderFront` / `activate_app` **之后**：那两个调用会同步触发窗口与应用
/// 的委托回调（`windowDidBecomeKey:` 等），期间若持着 `ViewState` 的借用就是重入 panic
/// （同 win32 的铁律 6）。借用只活在取 repaint 那一条语句里。
fn notify_shown(win: &NSWindow) {
    let Some(view) = win.contentView() else {
        return;
    };
    let Ok(view) = view.downcast::<ContentView>() else {
        return;
    };
    let repaint = view.ivars().borrow_mut().handler.on_window_shown();
    if repaint {
        view.setNeedsDisplay(true);
    }
}

/// 登记表里的一条。对照 win32 的同名结构，差别只在这边持的是所有权。
struct LiveWindow {
    win: Retained<NSWindow>,
    /// 单例键（`Window::single`）。`None` = 普通窗口，永不参与去重。
    single: Option<String>,
}

/// 两个引用是否指向同一个 `NSWindow`（登记表按对象身份增删，不能用 `==`）。
fn same_window(a: &NSWindow, b: &NSWindow) -> bool {
    std::ptr::eq(a as *const NSWindow, b as *const NSWindow)
}

/// 窗口建成时登记（连同所有权）。
fn register_window(win: Retained<NSWindow>, single: Option<String>) {
    WINDOWS.with(|v| {
        let mut v = v.borrow_mut();
        debug_assert!(!v.iter().any(|w| same_window(&w.win, &win)), "窗口重复登记");
        v.push(LiveWindow { win, single });
    });
}

/// 查找带指定单例键的既有窗口。
///
/// 键随窗口注销一并消失，故找到的那个必然还活着——单例判定不需要额外的存活校验。
fn find_single_window(key: &str) -> Option<Retained<NSWindow>> {
    WINDOWS.with(|v| {
        v.borrow()
            .iter()
            .find(|w| w.single.as_deref() == Some(key))
            .map(|w| w.win.clone())
    })
}

/// 窗口即将关闭时注销，返回**这是不是最后一个**（调用方据此 terminate）。
///
/// 取出的 `Retained` **不在此处 drop**，而是移进 `CLOSED` 交主队列稍后清空：此刻调用栈
/// 正在这个窗口的 `windowWillClose:` 里，就地放掉最后一个强引用会让 `NSWindow`（连同
/// 兼任 contentView 与 delegate 的 `ContentView`，也就是 `self`）在回调返回前析构。
/// 对照 win32：那边 `WM_DESTROY` 里 `Box::from_raw` 是安全的，因为 `WindowState` 不是
/// 正在执行的那个对象本身。
///
/// 未登记的窗口返回 `false`：那说明有条关闭路径没配对登记，此时按"最后一个"处理会在
/// 还有窗口活着时杀掉整个应用。
fn unregister_window(win: &NSWindow) -> bool {
    let (found, left) = WINDOWS.with(|v| {
        let mut v = v.borrow_mut();
        match v.iter().position(|w| same_window(&w.win, win)) {
            Some(i) => {
                let owned = v.remove(i);
                CLOSED.with(|c| c.borrow_mut().push(owned.win));
                (true, v.len())
            }
            None => (false, v.len()),
        }
    });
    debug_assert!(found, "注销未登记的窗口");
    if found {
        // 派回主队列（此刻即主线程）：调用栈退干净后再析构。
        unsafe {
            dispatch_async_f(
                std::ptr::addr_of!(_dispatch_main_q),
                std::ptr::null_mut(),
                drain_closed_windows,
            );
        }
    }
    found && left == 0
}

/// 释放已注销的窗口（[`unregister_window`] 的延迟释放落点）。
///
/// 先整体取出再 drop：析构 `NSWindow` 会连锁析构 `ContentView` 与整个 `UiHost`，若那
/// 期间有任何路径回头碰 `CLOSED`，持着借用就是运行期 panic。
extern "C" fn drain_closed_windows(_ctx: *mut c_void) {
    let pending: Vec<Retained<NSWindow>> = CLOSED.with(|c| std::mem::take(&mut *c.borrow_mut()));
    drop(pending);
}

/// 当前仍存活的窗口快照。
///
/// 返回**拷贝**而非借用：调用方拿着它去标脏、甚至建窗关窗，那些路径会回头改这张表；
/// 持着 `RefCell` 的借用走进去就是 panic。同 win32 `live_windows` 的理由。
fn live_windows() -> Vec<Retained<NSWindow>> {
    WINDOWS.with(|v| v.borrow().iter().map(|w| w.win.clone()).collect())
}

/// 标脏一个窗口的内容视图（下一轮 run loop 出帧）。
fn mark_dirty(win: &NSWindow) {
    if let Some(view) = win.contentView() {
        view.setNeedsDisplay(true);
    }
}

/// 标脏所有窗口。跨线程唤醒走这里——唤的是"这个应用"，不是某一个窗口。
fn mark_all_windows_dirty() {
    for w in live_windows() {
        mark_dirty(&w);
    }
}

/// 已关闭窗口的占位 handler：全部走 `AppHandler` 的默认实现（不画、不响应任何输入）。
///
/// 见 [`ContentView::release_handler`]——窗口关掉后真正的 `UiHost` 就地析构，位置由它顶上。
struct DeadHandler;

impl AppHandler for DeadHandler {
    fn render(&mut self, _target: &mut dyn crate::render::RenderTarget, _size: Size) {}
}

/// 视图运行期状态（对应 win32 的 `WindowState`）。
struct ViewState {
    handler: Box<dyn AppHandler>,
    bg: Color,
    /// 单份后备缓冲（tiny-skia 渲染目标，物理像素）。
    pixmap: Option<Pixmap>,
    buf_w: i32,
    buf_h: i32,
    /// 当前 DPI 缩放（= backingScaleFactor）。
    scale: f32,
    /// 无标题栏窗口：mouseDown 命中拖动区时走系统窗口拖动。
    frameless: bool,
    /// 输入法合成进行中（有未提交的 marked text）：此间所有按键交输入法处理。
    composing: bool,
    /// 逻辑捕获态镜像（每次指针事件后取 `handler.capture_active()`）。
    ///
    /// **macOS 没有 win32 `SetCapture` 的对应物，也不需要**：`mouseDown:` 之后的
    /// `mouseDragged:` / `mouseUp:` 由 AppKit 隐式续派发给同一个 view，指针拖出窗口外
    /// 照送。故此字段不驱动任何 OS 调用，只用来判断窗口失活时要不要通知上层收尾
    /// （见 `notify_capture_lost`），语义对齐 win32 `WindowState::capturing` 的门控作用。
    capturing: bool,
    /// 滚轮增量的亚像素残差（见 `on_wheel`）：触控板慢速滑动单次增量常不足 1 个框架单位，
    /// 直接取整会整格丢掉，残差留到下次事件补齐（同 `app/fling.rs` 的 `pan_residual`）。
    wheel_residual: f32,
    /// 动画帧的一次性定时器（仅动画期间存在；空闲为 None → 零唤醒）。每帧续约前先废止旧的。
    frame_timer: Option<Retained<NSTimer>>,
    /// `on_interval` 的周期定时器（按 handler.intervals() 顺序，下标即回调 idx）。
    /// 随窗口存活；进程退出时连同 run loop 一并销毁（对照 win32 的 SetTimer）。
    interval_timers: Vec<Retained<NSTimer>>,
    /// 复用的 DeviceRGB 色彩空间。
    color_space: CFRetained<CGColorSpace>,
    /// GPU 呈现目标。`Some` = 本窗走 GPU 路径（`pixmap` 与 CGImage 拷屏那一整套全不走）。
    ///
    /// **声明顺序即析构顺序，这两个字段的先后是必须的**：`WindowGpu` 里的 `wgpu::Surface`
    /// 存着下面那张 layer 的裸指针，layer 先释放就是悬垂。
    #[cfg(feature = "gpu")]
    gpu: Option<WindowGpu>,
    /// 挂在视图 backing layer 下的那张 `CAMetalLayer` 子层（见 [`attach_gpu`]）。
    ///
    /// 这份 `Retained` 是 surface 那个裸指针的**存活担保**：视图的 layer 树里也有它一份，
    /// 但那份由 AppKit 管、我们说了不算，而本字段的生命周期是明确的——它随 `ViewState`
    /// 一起死，且死在 `gpu` 之后。
    #[cfg(feature = "gpu")]
    metal_layer: Option<Retained<CAMetalLayer>>,
}

impl ViewState {
    /// 确保后备缓冲匹配物理尺寸；变化时重建。
    fn ensure_pixmap(&mut self, w: i32, h: i32) {
        let w = w.max(1);
        let h = h.max(1);
        if self.buf_w == w && self.buf_h == h && self.pixmap.is_some() {
            return;
        }
        self.pixmap = Some(Pixmap::new(w as u32, h as u32).expect("分配 pixmap 失败"));
        self.buf_w = w;
        self.buf_h = h;
    }
}

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "WindUiContentView"]
    #[ivars = RefCell<ViewState>]
    struct ContentView;

    impl ContentView {
        /// 左上原点、Y 向下——与框架的物理坐标约定一致。
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        /// 接收键盘事件（成为第一响应者）。
        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        /// GPU 路径下让 AppKit 走 `updateLayer` 而不是 `drawRect:`。
        ///
        /// 不这么做的话，AppKit 会为视图额外分配一整张窗口大小的 CPU backing store 供
        /// `drawRect:` 用——而我们的像素全在 Metal 子层里，那张缓冲一个字节都不会被写。
        #[cfg(feature = "gpu")]
        #[unsafe(method(wantsUpdateLayer))]
        fn wants_update_layer(&self) -> bool {
            self.ivars().borrow().metal_layer.is_some()
        }

        /// GPU 路径的出帧点，与下面的 `drawRect:` 等价（含帧内意图的排空，理由见那边）。
        /// `setNeedsDisplay(true)` 在两条路径下分别落到这里和 `drawRect:`，故上层的所有
        /// 标脏调用不必区分后端。
        #[cfg(feature = "gpu")]
        #[unsafe(method(updateLayer))]
        fn update_layer(&self) {
            self.do_draw();
            self.after_event();
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.do_draw();
            // 帧内产生的意图也要消费：App 级回调（channel 的 on_message、on_interval）
            // 拿到 EventCtx 后可以请求关窗、弹原生对话框、改窗口显隐，而它们是在出帧
            // 时跑的，不经过任何输入事件。不在这里排空，这些请求要拖到用户下一次点键
            // 敲键才生效——win32 侧在 WM_PAINT 里出于同样理由排空。
            self.after_event();
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, ev: &NSEvent) {
            // 无边框窗口：命中自定义标题栏拖动区（且非交互控件）→ 交系统窗口拖动，不下发点击。
            if self.ivars().borrow().frameless {
                let pos = self.loc_phys(ev);
                let (drag, interactive) = {
                    let st = self.ivars().borrow();
                    (st.handler.window_drag_at(pos), st.handler.interactive_at(pos))
                };
                if drag && !interactive {
                    if let Some(win) = self.window() {
                        win.performWindowDragWithEvent(ev);
                    }
                    return;
                }
            }
            self.on_pointer(ev, PointerKind::Down, MouseButton::Left);
        }
        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, ev: &NSEvent) {
            self.on_pointer(ev, PointerKind::Up, MouseButton::Left);
        }
        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, ev: &NSEvent) {
            self.on_pointer(ev, PointerKind::Move, MouseButton::Left);
        }
        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, ev: &NSEvent) {
            self.on_pointer(ev, PointerKind::Move, MouseButton::Left);
        }
        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, ev: &NSEvent) {
            self.on_pointer(ev, PointerKind::Down, MouseButton::Right);
        }
        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, ev: &NSEvent) {
            self.on_pointer(ev, PointerKind::Up, MouseButton::Right);
        }
        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, _ev: &NSEvent) {
            // 鼠标离开客户区：派发一个远处 Move 清除悬停态（对应 win32 WM_MOUSELEAVE）。
            self.dispatch_pointer(PointerEvent::single(
                PointerKind::Move,
                Point::new(-1, -1),
                MouseButton::Left,
            ));
        }
        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, ev: &NSEvent) {
            self.on_wheel(ev);
        }
        #[unsafe(method(keyDown:))]
        fn key_down(&self, ev: &NSEvent) {
            self.on_key(ev);
        }

        /// 维护覆盖整个可见区域的跟踪区（鼠标移动 / 进出）。
        #[unsafe(method(updateTrackingAreas))]
        fn update_tracking_areas(&self) {
            self.refresh_tracking_area();
            let _: () = unsafe { msg_send![super(self), updateTrackingAreas] };
        }

        /// 动画帧定时器到点：请求重绘。下一帧 do_draw 若仍在动画则自行续约（见 schedule_next_frame）。
        #[unsafe(method(frameTick:))]
        fn frame_tick(&self, _timer: &NSTimer) {
            self.setNeedsDisplay(true);
        }

        /// `on_interval` 周期定时器到点：按定时器在 interval_timers 中的下标调对应回调，
        /// 需重绘则标脏（对照 win32 的 WM_TIMER → on_interval_fired）。
        #[unsafe(method(intervalTick:))]
        fn interval_tick(&self, timer: &NSTimer) {
            let idx = {
                let st = self.ivars().borrow();
                st.interval_timers
                    .iter()
                    .position(|t| std::ptr::eq(Retained::as_ptr(t), timer))
            };
            let Some(idx) = idx else { return };
            let need = self.ivars().borrow_mut().handler.on_interval_fired(idx);
            if need {
                self.setNeedsDisplay(true);
            }
        }
    }

    unsafe impl NSObjectProtocol for ContentView {}

    // 文件拖放目标（对应 win32 的 WM_DROPFILES）。
    unsafe impl NSDraggingDestination for ContentView {
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            self.on_drop(sender)
        }
    }

    // 输入法客户端（对应 win32 的 WM_IME_* + ImmSetCompositionWindow）。我们不内联显示
    // 合成串（无对应上层 API），但跟踪合成态并把候选窗定位到光标处；提交文本经 insertText: 回灌。
    //
    // 合成态经 setMarkedText:/unmarkText/insertText: 三处上报给上层
    // （dispatch_composing → AppHandler::set_ime_composing），与 win32 的
    // WM_IME_START/ENDCOMPOSITION 对齐；控件据此在合成期间不画自绘光标。
    //
    // ⚠️ 两平台在这里有一处**未消除的语义差**：Windows 的 IME 自己会在
    // ImmSetCompositionWindow 指定的位置画出合成串和它自带的光标，所以隐藏自绘光标是在
    // 消除双光标；macOS 则把画 marked text 的责任完全交给客户端，而本后端不画——于是
    // 合成期间文本框里既没有合成串也没有光标。这不是本次改动引入的（下发链路一直如此），
    // 但根治需要上层给出「显示未提交合成串」的 API（RichText 的 span 模型可承载），
    // 属于另一个量级的工作。真机上若观感不可接受，短期缓解是让 macOS 后端不上报
    // composing=true（保留光标闪烁），代价是与 win32 的行为不再一致。
    unsafe impl NSTextInputClient for ContentView {
        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &AnyObject, _replacement: NSRange) {
            self.ime_insert(string);
        }

        #[unsafe(method(doCommandBySelector:))]
        fn do_command_by_selector(&self, _selector: Sel) {}

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(&self, string: &AnyObject, _selected: NSRange, _replacement: NSRange) {
            // 合成态 = 还有未提交的 marked text。
            let composing = !anyobject_to_string(string).is_empty();
            self.ivars().borrow_mut().composing = composing;
            self.dispatch_composing(composing);
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            self.ivars().borrow_mut().composing = false;
            self.dispatch_composing(false);
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            NSRange { location: 0, length: 0 }
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            if self.ivars().borrow().composing {
                NSRange { location: 0, length: 0 }
            } else {
                NSRange { location: NSNotFound as NSUInteger, length: 0 }
            }
        }

        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            self.ivars().borrow().composing
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring(
            &self,
            _range: NSRange,
            _actual: NSRangePointer,
        ) -> Option<Retained<NSAttributedString>> {
            None
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            NSArray::new()
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect(&self, _range: NSRange, _actual: NSRangePointer) -> NSRect {
            self.ime_caret_rect()
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index(&self, _point: NSPoint) -> NSUInteger {
            0
        }
    }

    // 窗口委托：拦截关闭请求并驱动真正的退出——对照 win32 的 WM_CLOSE / WM_DESTROY。
    unsafe impl NSWindowDelegate for ContentView {
        // 拦截标题栏 × 按钮（`performClose:` 会先咨询本回调）：交应用层 `on_close_request`
        // 走同一优先链——先关最顶层可见对话框、再问未保存拦截回调，返回 false 则取消关闭并
        // 重绘（反映对话框已关），返回 true 才放行，随后系统触发 `windowWillClose:` 退出。
        // 注意 `win.close()`（app 主动关窗，见 after_event）不经过本回调，故不会重复询问。
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &AnyObject) -> bool {
            let allow = self.ivars().borrow_mut().handler.on_close_request();
            if !allow {
                self.setNeedsDisplay(true);
                // 取消关闭时排一次待处理窗口操作：hide_on_close 正是在 on_close_request
                // 里返回 false 并留下 WindowOp::Hide。不排的话点关闭按钮会既不关也不隐。
                //
                // 借用已在上一条语句结束时释放——borrow_mut 是临时值，且 orderOut
                // 会重入本视图的回调（同 win32 两段式）。
                let op = self.ivars().borrow_mut().handler.take_window_op();
                if let (Some(WindowOp::Hide), Some(win)) = (op, self.window()) {
                    win.orderOut(None);
                }
                // 对话框请求同理：拦截器现在收 EventCtx，"挡下这次关闭、同时
                // ctx.defer_blocking 弹一个确认框"是文档推荐的确认退出流程
                // （见 API_GUIDE §8.7）。不在这里排空，那个框要等用户下一次
                // 输入才弹出来，看着就像点了关闭没反应。
                let dialog = self.ivars().borrow_mut().handler.take_dialog_request();
                if let Some(req) = dialog {
                    req.run();
                    self.setNeedsDisplay(true);
                }
            }
            allow
        }

        // 窗口失去 key 状态（Cmd+Tab 切走、点到别的窗口、原生模态框接管）：打断逻辑
        // 捕获与输入法合成。两者是同一类问题——"手离开了，却没有收尾事件送回来"，
        // 收尾都必须由这里补上。对照 win32 的 WM_CAPTURECHANGED + WM_IME_ENDCOMPOSITION。
        //
        // 未经真机验证的边界：状态栏（托盘）菜单弹出时窗口是否也会 resignKey。若会，
        // 表现是"托盘菜单一弹，进行中的拖动被取消"——与 win32 上 Alt+Tab 打断拖动同属
        // 保守方向（宁可多收尾一次，不能卡在拖动态），故即便如此也不算错。
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            self.notify_capture_lost();
            self.abort_composition();
        }

        // 窗口可见性变化（被别的窗口盖住、最小化、切 Space、以及**刚 orderFront 之后**）：
        // 重绘一帧。
        //
        // 这条是 GPU 路径的必需品，不是优化：窗口不可见时 Metal 的 `nextDrawable` 会一直
        // 等 vsync（wgpu 因此先查 `occlusionState`，不可见就直接判 `Occluded`），而窗口
        // 刚 `makeKeyAndOrderFront` 时系统还没把它标成 visible——首帧正好撞上这一档被丢掉。
        // 丢了就再也没人标脏，窗口永远空白（真机上就是这么表现的）。变可见时补一次即可。
        #[cfg(feature = "gpu")]
        #[unsafe(method(windowDidChangeOcclusionState:))]
        fn window_did_change_occlusion_state(&self, _notification: &NSNotification) {
            self.setNeedsDisplay(true);
        }

        // 窗口即将销毁：最后一个关掉才退出应用（对照 win32 WM_DESTROY→PostQuitMessage）。
        // 注意 `orderOut`（隐藏到托盘）不触发此回调，故隐藏不会退出。
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            // 已在退出流程中：`NSApplication::terminate` 会**同步挨个关掉所有窗口**，
            // 包括正是在本回调里发起 terminate 的那一个——它早已从登记表除名，再走一遍
            // 就会撞上"注销未登记的窗口"。win32 无此重入：`PostQuitMessage` 只是往队列
            // 里放一条 WM_QUIT，不会回头再触发一遍 WM_DESTROY。
            if TERMINATING.with(|t| t.get()) {
                return;
            }
            // 先废止定时器：`NSTimer` 持 target 的**强**引用，不废止就与本视图构成循环，
            // 窗口关掉后 `ContentView` 永不析构——`on_interval` 会继续对着已关闭的窗口
            // 跑，子窗那批信号也回收不掉。win32 无此问题：`SetTimer(hwnd,…)` 的定时器
            // 随窗口销毁由 OS 清理，不反过来持有任何东西。
            self.invalidate_timers();
            // `windowWillClose:` 是"将要关"，此刻 window 尚未析构、contentView 仍挂着，
            // 故 `self.window()` 必然拿得到。拿不到说明本回调的触发时机与假设不符，
            // 那样注销就会漏掉一个窗口（应用再也退不出去），debug 下拦下。
            // 上层状态就地回收（子窗那批信号在此归还），不等 NSWindow 析构。
            self.release_handler();
            let Some(win) = self.window() else {
                debug_assert!(false, "windowWillClose: 取不到 window，窗口无法注销");
                return;
            };
            if unregister_window(&win) {
                // 置位**先于** terminate：它会同步回到本回调（见开头那道闸）。
                TERMINATING.with(|t| t.set(true));
                let mtm = MainThreadMarker::from(self);
                NSApplication::sharedApplication(mtm).terminate(None);
            }
        }
    }
);

/// NSString 或 NSAttributedString（输入法回灌的文本载体）→ Rust String。
fn anyobject_to_string(obj: &AnyObject) -> String {
    if let Some(s) = obj.downcast_ref::<NSString>() {
        s.to_string()
    } else if let Some(a) = obj.downcast_ref::<NSAttributedString>() {
        a.string().to_string()
    } else {
        String::new()
    }
}

impl ContentView {
    // NSFilenamesPboardType 已弃用但在现行 macOS 仍有效，且读取路径列表最简；保留并抑制告警。
    #[allow(deprecated)]
    fn new(
        mtm: MainThreadMarker,
        frame: NSRect,
        handler: Box<dyn AppHandler>,
        bg: Color,
        frameless: bool,
    ) -> Retained<Self> {
        let color_space = CGColorSpace::new_device_rgb().expect("CGColorSpaceCreateDeviceRGB 失败");
        let state = ViewState {
            handler,
            bg,
            pixmap: None,
            buf_w: 0,
            buf_h: 0,
            scale: 1.0,
            frameless,
            composing: false,
            capturing: false,
            wheel_residual: 0.0,
            frame_timer: None,
            interval_timers: Vec::new(),
            color_space,
            #[cfg(feature = "gpu")]
            gpu: None,
            #[cfg(feature = "gpu")]
            metal_layer: None,
        };
        let this = Self::alloc(mtm).set_ivars(RefCell::new(state));
        let this: Retained<Self> = unsafe { msg_send![super(this), initWithFrame: frame] };
        // 注册接收文件拖放（路径列表）。
        let ty: &NSPasteboardType = unsafe { NSFilenamesPboardType };
        let types = NSArray::from_slice(&[ty]);
        this.registerForDraggedTypes(&types);
        this
    }

    /// 处理文件拖放：取落点（物理像素）与路径列表，交宿主路由。
    #[allow(deprecated)]
    fn on_drop(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
        let pb = sender.draggingPasteboard();
        let ty: &NSPasteboardType = unsafe { NSFilenamesPboardType };
        let Some(plist) = pb.propertyListForType(ty) else {
            return false;
        };
        // 属性列表为 NSArray<NSString>（路径）；无参 NSArray 才是可降级目标，逐项再降为 NSString。
        let Ok(arr) = plist.downcast::<NSArray>() else {
            return false;
        };
        let mut paths: Vec<PathBuf> = Vec::new();
        for i in 0..arr.count() {
            if let Ok(s) = arr.objectAtIndex(i).downcast::<NSString>() {
                paths.push(PathBuf::from(s.to_string()));
            }
        }
        if paths.is_empty() {
            return false;
        }
        // 落点：窗口坐标 → 视图（翻转，点）→ 物理像素。
        let view_pt = self.convertPoint_fromView(sender.draggingLocation(), None);
        let scale = self.ivars().borrow().scale;
        let pos = Point::new(
            (view_pt.x as f32 * scale).round() as i32,
            (view_pt.y as f32 * scale).round() as i32,
        );
        let repaint = {
            let _guard = crate::platform::EventDispatchGuard::enter();
            self.ivars().borrow_mut().handler.on_drop_files(pos, paths)
        };
        if repaint {
            self.setNeedsDisplay(true);
        }
        self.after_event();
        true
    }

    /// 重建覆盖可见区域的跟踪区（InVisibleRect 自适应尺寸）。
    fn refresh_tracking_area(&self) {
        // 移除旧跟踪区，避免叠加。
        let areas = self.trackingAreas();
        for area in areas.iter() {
            self.removeTrackingArea(&area);
        }
        let opts = NSTrackingAreaOptions::MouseEnteredAndExited
            | NSTrackingAreaOptions::MouseMoved
            | NSTrackingAreaOptions::ActiveInKeyWindow
            | NSTrackingAreaOptions::InVisibleRect;
        let area = unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                self.bounds(),
                opts,
                Some(self),
                None,
            )
        };
        self.addTrackingArea(&area);
    }

    /// 渲染一帧并 blit 到屏。
    fn do_draw(&self) {
        let bounds = self.bounds();
        let scale = self
            .window()
            .map(|w| w.backingScaleFactor() as f32)
            .unwrap_or(1.0)
            .max(0.1);
        let pw = (bounds.size.width as f32 * scale).round().max(1.0) as i32;
        let ph = (bounds.size.height as f32 * scale).round().max(1.0) as i32;

        // GPU 路径：像素直接进 `CAMetalLayer` 的 surface，下面 pixmap→CGImage→拷屏那一整套
        // 全不走（连后备缓冲都不分配）。两条路径在窗口创建时二选一，运行期不切换。
        #[cfg(feature = "gpu")]
        if self.ivars().borrow().gpu.is_some() {
            self.draw_gpu(bounds.size, pw, ph, scale);
            self.schedule_next_frame();
            return;
        }

        // 渲染进 pixmap（借用期间不触发可重入的 OS 调用）。
        let image = {
            let mut st = self.ivars().borrow_mut();
            if (st.scale - scale).abs() > 0.001 {
                st.scale = scale;
                st.handler.set_scale(scale);
            }
            st.ensure_pixmap(pw, ph);
            // 每帧问宿主要底色：运行期换主题时 `st.bg`（创建时抄的那份）不会跟着变。
            let bg = st.handler.bg().unwrap_or(st.bg);
            let pixmap = st.pixmap.as_mut().unwrap();
            pixmap.fill(to_skia_color(bg));
            // 借用拆分：handler 与 pixmap 是不同字段，但都在 st 里，需先取出 pixmap 的裸数据后渲染。
            let size = Size::new(pw, ph);
            // 安全：render 只写 pixmap，不访问 st 其他字段。
            let ptr = pixmap as *mut Pixmap;
            let mut tgt = crate::render::PixmapTarget {
                pixmap: unsafe { &mut *ptr },
            };
            st.handler.render(&mut tgt, size);

            // 直接把 pixmap 缓冲包成 CGImage：经 CGDataProvider **引用**缓冲（不拷贝像素），
            // release 回调为 None（缓冲由 pixmap 拥有）。CGImage 在本帧 draw_image 后即析构，
            // 期间缓冲不被改写，故无拷贝也安全——相较 CGBitmapContextCreateImage 省去每帧整窗拷贝。
            let bytes_per_row = pw as usize * 4;
            let pixmap = st.pixmap.as_ref().unwrap();
            let data = pixmap.data().as_ptr() as *const c_void;
            let size = bytes_per_row * ph as usize;
            let cs = st.color_space.clone();
            let provider =
                unsafe { CGDataProvider::with_data(std::ptr::null_mut(), data, size, None) };
            provider.and_then(|p| unsafe {
                CGImage::new(
                    pw as usize,
                    ph as usize,
                    8,
                    32,
                    bytes_per_row,
                    Some(&cs),
                    CGBitmapInfo(CGImageAlphaInfo::PremultipliedLast.0),
                    Some(&p),
                    std::ptr::null(),
                    false,
                    CGColorRenderingIntent::RenderingIntentDefault,
                )
            })
        };

        let Some(image) = image else { return };
        let Some(gctx) = NSGraphicsContext::currentContext() else {
            return;
        };
        let cg = gctx.CGContext();

        // 翻转视图的 drawRect 上下文里，自上而下缓冲派生的 CGImage 需再翻转一次才正立
        //（已用离屏探针验证）：translate(0,H) scale(1,-1)，H 为视图点高。
        let h = bounds.size.height;
        CGContext::save_g_state(Some(&cg));
        CGContext::translate_ctm(Some(&cg), 0.0, h);
        CGContext::scale_ctm(Some(&cg), 1.0, -1.0);
        CGContext::draw_image(
            Some(&cg),
            CGRect {
                origin: CGPoint { x: 0.0, y: 0.0 },
                size: CGSize {
                    width: bounds.size.width,
                    height: bounds.size.height,
                },
            },
            Some(&image),
        );
        CGContext::restore_g_state(Some(&cg));

        // 本帧画完后，若仍有控件请求持续动画，按显示器刷新率自调度下一帧。
        self.schedule_next_frame();
    }

    /// GPU 路径出一帧：按需重配 surface → 取帧铺底 → 交宿主绘制 → present。
    ///
    /// 与软路径的 `do_draw` 一一对应：`ensure_pixmap` 对应 `resize`、`pixmap.fill(bg)` 对应
    /// 取帧时的清屏、`CGContextDrawImage` 拷屏对应 `present`。DPI 变化的处理也照抄那边
    /// （比较 `st.scale` 后通知宿主），只多一件事：同步 layer 的 `contentsScale`。
    ///
    /// 取不到帧不是错误（窗口被遮挡、drawable 一时用尽都会撞上），三档各有各的反应，
    /// 见 [`FrameError`]。只有"重配过还是不行"才提示一次。
    #[cfg(feature = "gpu")]
    fn draw_gpu(&self, size_pt: NSSize, pw: i32, ph: i32, scale: f32) {
        // 两段式（同本文件其余各处）：借用内只跟宿主与 GPU 打交道，可能重入本视图回调的
        // AppKit 调用（这里是补排一次重绘）留到借用释放之后。
        let retry = self.draw_gpu_frame(size_pt, pw, ph, scale);
        if retry {
            self.setNeedsDisplay(true);
        }
    }

    /// [`Self::draw_gpu`] 的借用段。返回 `true` 表示这一帧没画成、需要再排一次重绘。
    #[cfg(feature = "gpu")]
    fn draw_gpu_frame(&self, size_pt: NSSize, pw: i32, ph: i32, scale: f32) -> bool {
        let mut borrow = self.ivars().borrow_mut();
        let st = &mut *borrow;
        if let Some(layer) = &st.metal_layer {
            // Metal 层是子层，不随视图自动改尺寸——每帧对一次账（尺寸没变则不动）。
            let f = layer.frame().size;
            if (f.width - size_pt.width).abs() > 0.5 || (f.height - size_pt.height).abs() > 0.5 {
                set_layer_frame(layer, size_pt);
            }
            if (st.scale - scale).abs() > 0.001 {
                // AppKit 只替它自己那张 backing layer 维护 contentsScale，子层得自己设。
                layer.setContentsScale(scale as f64);
            }
        }
        if (st.scale - scale).abs() > 0.001 {
            st.scale = scale;
            st.handler.set_scale(scale);
        }
        // 每帧问宿主要底色（运行期换主题时创建时抄的那份不会变），与软路径同源。
        let bg = st.handler.bg().unwrap_or(st.bg);
        // 分字段借用：`gpu` 与 `handler` 是 `ViewState` 的两个字段，可同时可变借出。
        let Some(gpu) = st.gpu.as_mut() else {
            return false;
        };
        gpu.resize((pw.max(1) as u32, ph.max(1) as u32));
        match gpu.begin_frame(bg) {
            Ok(mut frame) => {
                {
                    let mut target = frame.target();
                    st.handler.render(&mut target, Size::new(pw, ph));
                    // target（连同它开出的 canvas）必须先析构：GPU canvas 是攒够一帧在
                    // 析构时才提交的，present 早于它就会 present 一张空底。
                }
                frame.present();
                false
            }
            // 一时取不到 drawable：补排一次。不补的话这次标脏就白丢了——事件驱动的宿主
            // 没有"下一帧"兜底，界面会停在旧内容上直到用户再动一下。
            Err(FrameError::Skipped) => true,
            // 窗口不可见：重排只会空转，等 `windowDidChangeOcclusionState:` 唤回。
            Err(FrameError::Occluded) => false,
            Err(FrameError::Lost) => {
                notice_once(
                    &GPU_LOST_NOTICE,
                    "windui: GPU surface 已丢失且重配无效，窗口内容不再更新（请以软渲染重启）",
                );
                false
            }
        }
    }

    /// 安装 `on_interval` 周期定时器：按 handler 注册的间隔各建一个重复 NSTimer，存入
    /// interval_timers（下标即 on_interval_fired 的 idx）。窗口创建后调用一次。
    fn install_interval_timers(&self) {
        let durs = self.ivars().borrow().handler.intervals();
        let mut timers = Vec::with_capacity(durs.len());
        for d in durs {
            // 间隔下限 1ms，避免 0 间隔空转。
            let secs = d.as_secs_f64().max(0.001);
            let timer = unsafe {
                NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                    secs,
                    self,
                    sel!(intervalTick:),
                    None,
                    true, // repeats：周期触发
                )
            };
            timers.push(timer);
        }
        self.ivars().borrow_mut().interval_timers = timers;
    }

    /// 窗口关闭时**就地**释放上层状态（`UiHost`），换 [`DeadHandler`] 占位。
    ///
    /// **不能等 `NSWindow` 析构**：`close` 之后 AppKit 内部仍持有窗口好几份引用（实测
    /// `retainCount` 是我们那份的九倍），何时归零不由我们决定、也没有任何承诺。把
    /// `UiHost` 的回收挂上去，子窗那批 `Signal` 就会一直滞留到进程退出——反复开关子窗
    /// 让全局 arena 越积越多，恰恰是 `SignalScope` 要解决的问题。
    ///
    /// 对照 win32：那边 `WM_DESTROY` 里显式 `Box::from_raw` 收掉 `WindowState`，同样不
    /// 依赖 `HWND` 的生命周期。**两个平台在这一点上是一致的：上层状态由框架自己回收。**
    ///
    /// 换占位而不是留空，是因为 AppKit 在 `windowWillClose:` 之后仍会给视图派几条消息
    /// （光标、跟踪区收尾）；让它们打在一个全默认实现上，比在每个调用点判空要稳。
    fn release_handler(&self) {
        let old = {
            let mut st = self.ivars().borrow_mut();
            std::mem::replace(
                &mut st.handler,
                Box::new(DeadHandler) as Box<dyn AppHandler>,
            )
        };
        // 借用之外析构：`UiHost` 的 drop 会连锁回收整棵树与那批信号，其间不该有人还借着
        // `ViewState`（析构路径上的任何回调都会撞上 RefCell）。
        drop(old);
    }

    /// 废止本视图的全部定时器（动画帧 + `on_interval`），打破 `NSTimer` → target 的强引用。
    ///
    /// 两段式：`invalidate` 会释放 target（可能就是最后一个引用），必须在借用之外调。
    fn invalidate_timers(&self) {
        let (frame, intervals) = {
            let mut st = self.ivars().borrow_mut();
            (
                st.frame_timer.take(),
                std::mem::take(&mut st.interval_timers),
            )
        };
        if let Some(t) = frame {
            t.invalidate();
        }
        for t in intervals {
            t.invalidate();
        }
    }

    /// 动画帧驱动：废止上一个待触发的帧定时器，若仍在动画则按刷新率调度下一次一次性重绘。
    /// 仅在动画期间存在定时器，空闲时无定时器（零唤醒，优于常驻定时器）。对应 win32 消息循环的帧配速。
    fn schedule_next_frame(&self) {
        if let Some(t) = self.ivars().borrow_mut().frame_timer.take() {
            t.invalidate();
        }
        if !self.ivars().borrow().handler.wants_animation() {
            return;
        }
        // 不可见（`orderOut` 隐藏、或最小化）时不续约：托盘常驻应用「关窗即隐藏」后，
        // 窗口里若留着聚焦的输入框，光标闪烁会一直请求续帧——不可见的窗口按刷新率
        // 白烧 CPU。重新显示时 `show_and_activate` 会把帧链重新起上。
        if let Some(win) = self.window() {
            if !win.isVisible() || win.isMiniaturized() {
                return;
            }
        }
        let interval = self.display_frame_interval();
        // repeats=false：一次性；下一帧 do_draw 再续约，故动画停止即自然停。
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                interval,
                self,
                sel!(frameTick:),
                None,
                false,
            )
        };
        self.ivars().borrow_mut().frame_timer = Some(timer);
    }

    /// 帧间隔（秒）= 1 / 显示器最大刷新率。跟随窗口所在屏（高刷屏吃到 120/144Hz），
    /// clamp 到 [60, 240]；取不到时回退 60。对应 win32 的 `frame_interval_ms`。
    fn display_frame_interval(&self) -> f64 {
        let mtm = MainThreadMarker::from(self);
        let fps = self
            .window()
            .and_then(|w| w.screen())
            .or_else(|| NSScreen::mainScreen(mtm))
            .map(|s| s.maximumFramesPerSecond())
            .unwrap_or(60)
            .clamp(60, 240);
        1.0 / fps as f64
    }

    /// 窗口坐标 → 客户区物理像素（左上原点）。
    fn loc_phys(&self, ev: &NSEvent) -> Point {
        let win_pt = ev.locationInWindow();
        let view_pt = self.convertPoint_fromView(win_pt, None);
        let scale = self.ivars().borrow().scale;
        Point::new(
            (view_pt.x as f32 * scale).round() as i32,
            (view_pt.y as f32 * scale).round() as i32,
        )
    }

    /// 鼠标按下/抬起/移动 → PointerEvent。
    fn on_pointer(&self, ev: &NSEvent, kind: PointerKind, button: MouseButton) {
        let pos = self.loc_phys(ev);
        let click_count = if matches!(kind, PointerKind::Down) {
            (ev.clickCount().max(1) as u8).min(3)
        } else {
            1
        };
        self.dispatch_pointer(PointerEvent {
            kind,
            pos,
            button,
            click_count,
        });
    }

    /// 滚轮 / 触控板两指滑动 → Wheel 事件。框架约定一刻度 ±120（正=上滚）。
    ///
    /// # 惯性由系统给，本后端**刻意不实现** `start_fling` / `cancel_fling`
    ///
    /// 触控板抬指后 AppKit 会继续投递 `momentumPhase != None` 的滚动事件——动量、摩擦、
    /// 衰减全由系统按用户的触控板设置算好，并在用户重新把手指放上触控板时自动中止。
    /// 这些事件在此与"手指仍在滑"的事件走同一条路径，于是列表自然滑行、再次触摸自然打断，
    /// 无需框架介入。
    ///
    /// win32 那套自研惯性状态机（`Touch` + `start_fling`/`cancel_fling`）是为了补
    /// `WM_TOUCH` **只给位置不给动量**的缺口，**不要移植到这里**：移过来只会与系统动量
    /// 叠加成双倍速度，还会因两套摩擦系数不同而在松手瞬间跳一下。因此
    /// `AppHandler::{on_pan, start_fling, cancel_fling}` 在 macOS 后端保持默认空实现，
    /// `app/fling.rs` 的整套状态机在 macOS 上是永不激活的死代码（不加 `#[cfg]` 门控：
    /// 它是平台无关的 `AppHandler` 实现体，门控只会把跨平台的单元测试也一并切掉）。
    ///
    /// 动量事件与用户主动滚动**不作区分**是有意的：下游（`ScrollWidget` / 菜单浮层）
    /// 对滚轮只有"滚多少"这一个语义，没有任何逻辑会因"这是动量"而需要改判。
    ///
    /// **未经真机验证**。验法：Mac 触控板在长列表上两指快滑后抬手——预期列表继续滑行
    /// 并逐渐停下；滑行途中再把两指放上触控板，预期立即停住而不是叠加加速。
    fn on_wheel(&self, ev: &NSEvent) {
        // 新手势起手（手指刚落到触控板上）：清掉上一段的亚像素残差，免得方向相反的
        // 旧残差把新手势的第一格吃掉。鼠标滚轮的 phase 恒为 None，走不到这里。
        let phase = ev.phase();
        if phase.contains(NSEventPhase::Began) || phase.contains(NSEventPhase::MayBegin) {
            self.ivars().borrow_mut().wheel_residual = 0.0;
        }
        let dy = ev.scrollingDeltaY();
        // 触控板（精确增量，单位=点）：按点位细粒度滚；鼠标滚轮（行增量）：约 3 行/刻度。
        let raw = if ev.hasPreciseScrollingDeltas() {
            dy as f32 * 3.0
        } else {
            dy as f32 * 40.0
        };
        // 亚像素累积后再取整：触控板慢速滑动单次增量常 <1，直接 `as i32` 截成 0 就把整个
        // 事件丢了，表现为"轻推没反应、猛推才动"，动量尾段也会提前断掉。
        let delta = {
            let mut st = self.ivars().borrow_mut();
            let total = raw + st.wheel_residual;
            let whole = total.trunc();
            st.wheel_residual = total - whole;
            whole as i32
        };
        if delta == 0 {
            return;
        }
        let pos = self.loc_phys(ev);
        self.dispatch_pointer(PointerEvent::single(
            PointerKind::Wheel(delta),
            pos,
            MouseButton::Left,
        ));
    }

    /// 键盘按下：特殊键直发；普通文本交输入法（IME 提交后经 `insertText:` 回到 Key::Char），
    /// 使中文/emoji 可在文本框输入（对照 win32 的 WM_KEYDOWN + WM_CHAR + IME）。
    fn on_key(&self, ev: &NSEvent) {
        // 合成进行中：全部交输入法（候选切换/确认/退格在 IME 内完成）。
        if self.ivars().borrow().composing {
            self.route_ime(ev);
            return;
        }
        let key_code = ev.keyCode();
        let flags = ev.modifierFlags();
        let shift = flags.contains(objc2_app_kit::NSEventModifierFlags::Shift);
        // macOS 习惯用 Command 做快捷键；同时接受 Control，统一映射到框架的 `ctrl` 标志，
        // 使 Cmd+C/V/X/A 原生可用。
        let modk = flags.contains(objc2_app_kit::NSEventModifierFlags::Command)
            || flags.contains(objc2_app_kit::NSEventModifierFlags::Control);

        let special = map_special(key_code);
        if let Some(k) = special {
            self.dispatch_key(KeyEvent {
                key: k,
                pressed: true,
                shift,
                ctrl: modk,
            });
            // 非空格特殊键到此为止；空格还需交输入法产出 Key::Char(' ')（文本框插入空格）。
            if k != Key::Space {
                return;
            }
        }
        if modk {
            // 快捷键：用 Key::Other(大写 ASCII 码) + ctrl（与 win32 VK 码对齐：'A'=0x41…）。不进输入法。
            if special.is_none() {
                if let Some(s) = ev.charactersIgnoringModifiers() {
                    if let Some(c) = s.to_string().chars().next() {
                        let up = c.to_ascii_uppercase();
                        self.dispatch_key(KeyEvent {
                            key: Key::Other(up as u32),
                            pressed: true,
                            shift,
                            ctrl: true,
                        });
                    }
                }
            }
            return;
        }
        // 普通文本（含空格）：交输入法。英文直接经 insertText: 回灌 Key::Char；
        // 中文/emoji 经候选窗合成后提交。
        self.route_ime(ev);
    }

    /// 把按键交给视图的输入法上下文处理（触发 NSTextInputClient 回调）。
    fn route_ime(&self, ev: &NSEvent) {
        if let Some(ic) = self.inputContext() {
            let _ = ic.handleEvent(ev);
        }
    }

    /// 输入法提交文本（`insertText:`）：逐字符派发为 Key::Char，并结束合成态。
    fn ime_insert(&self, string: &AnyObject) {
        let text = anyobject_to_string(string);
        self.ivars().borrow_mut().composing = false;
        self.dispatch_composing(false);
        for c in text.chars() {
            if c.is_control() {
                continue;
            }
            self.dispatch_key(KeyEvent {
                key: Key::Char(c),
                pressed: true,
                shift: false,
                ctrl: false,
            });
        }
    }

    /// 焦点文本控件的光标矩形（屏幕坐标，y 向上），供输入法定位候选窗。
    fn ime_caret_rect(&self) -> NSRect {
        let (caret, scale) = {
            let st = self.ivars().borrow();
            (st.handler.ime_caret(), st.scale)
        };
        let Some((x, y, h)) = caret else {
            return NSRect {
                origin: NSPoint { x: 0.0, y: 0.0 },
                size: NSSize {
                    width: 0.0,
                    height: 0.0,
                },
            };
        };
        // 物理像素 → 视图点（翻转视图：左上原点、y 向下）。
        let s = scale as f64;
        let view_rect = NSRect {
            origin: NSPoint {
                x: x as f64 / s,
                y: y as f64 / s,
            },
            size: NSSize {
                width: 1.0,
                height: (h as f64 / s).max(1.0),
            },
        };
        // 视图 → 窗口 → 屏幕（AppKit 转换自动处理翻转与 y 轴朝向）。
        let win_rect = self.convertRect_toView(view_rect, None);
        match self.window() {
            Some(w) => w.convertRectToScreen(win_rect),
            None => win_rect,
        }
    }

    /// 两段式分发指针事件：借用内运行 handler，释放后再做可能重入的 OS 调用。
    fn dispatch_pointer(&self, ev: PointerEvent) {
        let repaint = {
            let _guard = crate::platform::EventDispatchGuard::enter();
            let mut st = self.ivars().borrow_mut();
            let repaint = st.handler.on_pointer(ev);
            // 同步逻辑捕获态镜像。win32 在这一步真的调 SetCapture/ReleaseCapture；
            // macOS 不需要（AppKit 隐式续派发，见 ViewState::capturing），只记录状态。
            st.capturing = st.handler.capture_active();
            repaint
        };
        if repaint {
            self.setNeedsDisplay(true);
        }
        self.after_event();
    }

    fn dispatch_key(&self, ev: KeyEvent) {
        let repaint = {
            let _guard = crate::platform::EventDispatchGuard::enter();
            self.ivars().borrow_mut().handler.on_key(ev)
        };
        if repaint {
            self.setNeedsDisplay(true);
        }
        self.after_event();
    }

    /// 通知焦点控件输入法组合态变化（见 win32 的 `WM_IME_START/ENDCOMPOSITION`）。
    fn dispatch_composing(&self, composing: bool) {
        let repaint = {
            let _guard = crate::platform::EventDispatchGuard::enter();
            self.ivars()
                .borrow_mut()
                .handler
                .set_ime_composing(composing)
        };
        if repaint {
            self.setNeedsDisplay(true);
        }
    }

    /// 窗口失去 key 状态时通知上层的逻辑捕获方收尾（复位拖动态）。对照 win32 的
    /// `WM_CAPTURECHANGED`：那边是 OS 真把 `SetCapture` 抢走了，macOS 没有显式捕获可抢
    /// （见 `ViewState::capturing`），但"手还按着、抬起事件却永远不会来"这件事一样会发生
    /// ——按住 reorder 列表项拖到一半 Cmd+Tab 切走，`mouseUp:` 就此不再送达，
    /// 上层会永远卡在拖动态。门控与 win32 一致：仅在上层自认持有捕获时才通知。
    ///
    /// **未经真机验证**（开发机为 Windows）。验法：Mac 上跑 `examples/reorder.rs`，
    /// 按住一行拖到列表中部**不松手**，按 Cmd+Tab 切到别的应用再切回——预期该行已落回
    /// 合法位置、不再跟随指针；若仍粘在指针上，说明本回调没被调用或 `capturing` 没镜像上。
    fn notify_capture_lost(&self) {
        let repaint = {
            let mut st = self.ivars().borrow_mut();
            if !st.capturing {
                return;
            }
            st.capturing = false;
            st.handler.on_capture_lost()
        };
        if repaint {
            self.setNeedsDisplay(true);
        }
    }

    /// 窗口失活时收掉未提交的输入法合成：清本地合成态、通知上层恢复自绘光标，
    /// 再让输入上下文丢弃 marked text。不收的话，合成中途切走应用后 `composing`
    /// 会一直为 true，而组合态期间 `TextInput` 不画自绘光标——焦点文本框的光标就此
    /// 再也不闪（win32 侧由 `WM_IME_ENDCOMPOSITION` 保证不会出现这种悬挂）。
    ///
    /// 两段式：先在借用内改本地状态并释放借用，再调可能重入本视图回调的
    /// `discardMarkedText`（它若反过来触发 `unmarkText:` 是幂等的）。
    ///
    /// **未经真机验证**。验法：Mac 上用拼音输入法在 `examples/ime.rs` 的文本框里打到一半
    /// （候选窗已弹出）时 Cmd+Tab 切走再切回——预期光标恢复闪烁、候选窗未残留、
    /// 已输入的拼音不会莫名其妙上屏。
    fn abort_composition(&self) {
        let was_composing = {
            let mut st = self.ivars().borrow_mut();
            let was = st.composing;
            st.composing = false;
            was
        };
        if !was_composing {
            return;
        }
        self.dispatch_composing(false);
        if let Some(ic) = self.inputContext() {
            ic.discardMarkedText();
        }
    }

    /// 事件分发后：执行待处理窗口操作、原生对话框请求、应用光标、必要时关窗。
    fn after_event(&self) {
        let (op, dialog, close, hotkey_ops, new_windows) = {
            let mut st = self.ivars().borrow_mut();
            (
                st.handler.take_window_op(),
                st.handler.take_dialog_request(),
                st.handler.wants_close(),
                st.handler.take_hotkey_ops(),
                // `is_open` 查的是 `WINDOWS`，与这里借着的 `ViewState` 是两个独立
                // RefCell，可同时活着。单例判定必须由宿主在**构建内容之前**做。
                st.handler
                    .take_new_windows(&|key| find_single_window(key).is_some()),
            )
        };
        // 运行期热键操作（`HotkeyHandle` 改绑/启停）。热键状态是应用级的（thread_local），
        // 与本窗的 `ViewState` 是两份东西——借用已释放，这里不会与之相撞。
        for (id, hop) in hotkey_ops {
            super::hotkey::apply(id, hop);
        }
        if let Some(op) = op {
            if let Some(win) = self.window() {
                match op {
                    WindowOp::Minimize => win.miniaturize(None),
                    WindowOp::ToggleMaximize => win.zoom(None),
                    // 与托盘、热键共用 `show_and_activate`（此前这份漏了 deminiaturize，
                    // 最小化时从控件请求唤起不会还原窗口）。
                    WindowOp::Show => show_and_activate(&win, MainThreadMarker::from(self)),
                    WindowOp::Hide => win.orderOut(None),
                }
            }
        }
        // 此时 borrow_mut 已释放：对话框自带模态消息泵，运行期间会重入本视图的事件
        // 回调，须先放开借用再调用，避免与 RefCell 形成重入 panic（同 win32 两段式）。
        if let Some(req) = dialog {
            req.run();
            self.setNeedsDisplay(true);
        }
        // 建出 `ctx.open_window` 排队的子窗。借用早已释放（`new_windows` 是拥有的值），
        // 这一步会 `makeKeyAndOrderFront`，进而同步回调本窗的 `windowDidResignKey:`
        // ——那里要借 `ViewState`，两个 `&mut` 并存就是 panic（铁律 6 的 AppKit 版）。
        self.open_child_windows(new_windows);
        self.apply_cursor();
        if close {
            if let Some(win) = self.window() {
                win.close();
            }
        }
        // 跨窗口状态：本次分发若写过信号，让其余窗口也重绘一次。放在关窗之后——本窗若
        // 已关掉，它早从登记表里除名了，正好不必为一个要消失的窗口排帧。
        self.broadcast_signal_dirty();
    }

    /// 建出应用层排队的子窗（`EventCtx::open_window`）并显示。
    ///
    /// **调用方须已释放 `ViewState` 借用**：建窗会同步走完 `setContentView` /
    /// `makeKeyAndOrderFront`，其中激活新窗口会让当前窗口收到 `windowDidResignKey:`，
    /// 那条回调要借本视图的 `ViewState`。对照 win32 的 `open_pending_windows`。
    fn open_child_windows(&self, requests: Vec<NewWindow>) {
        if requests.is_empty() {
            return;
        }
        let mtm = MainThreadMarker::from(self);
        for item in requests {
            match item {
                // 单例（`Window::single`）：把已有的那个激活到前台。找不到说明它在判定与
                // 执行之间被关掉了——正常竞态，什么都不做即可。
                NewWindow::Focus(key) => {
                    let Some(existing) = find_single_window(&key) else {
                        continue;
                    };
                    // 与 win32 的 `show_and_activate` 同语义：最小化的先还原，再置前。
                    // 期间应用可能已不在前台，仅 orderFront 不足以把窗口带上来。
                    if existing.isMiniaturized() {
                        existing.deminiaturize(None);
                    }
                    existing.makeKeyAndOrderFront(None);
                    super::activate_app(&NSApplication::sharedApplication(mtm));
                }
                NewWindow::Create(cfg, handler) => {
                    // 后端档位取应用级的那份而不是 `cfg.renderer`：子窗配置由应用层现构造，
                    // 不知道主窗当初选了什么（见 `APP_RENDERER`）。
                    let win = create_window(mtm, &cfg, handler, APP_RENDERER.with(|r| r.get()));
                    win.makeKeyAndOrderFront(None);
                }
            }
        }
    }

    /// 本次事件分发写过信号时，让**除本窗外**的窗口各失效一次。
    ///
    /// `Signal` 是跨窗口共享状态的唯一原语（`Copy` 句柄，传进子窗即可共享），但事件
    /// 分发只会让发起方产生脏区——"在设置窗里改了名字，主窗显示的还是旧的"就是这么来的。
    /// 发起方跳过：它自己已经有精确脏区，整窗失效反而把局部重绘的收益抹掉。
    ///
    /// 单窗口下恒为空操作。与 win32 的 `broadcast_signal_dirty` 同源，语义必须一致。
    fn broadcast_signal_dirty(&self) {
        if !crate::signal::take_cross_window_dirty() {
            return;
        }
        let me = self.window();
        for w in live_windows() {
            if me.as_ref().is_some_and(|m| same_window(m, &w)) {
                continue;
            }
            mark_dirty(&w);
        }
    }

    /// 按当前悬停控件期望形状设置光标。
    fn apply_cursor(&self) {
        let shape = self.ivars().borrow().handler.cursor();
        let cursor = match shape {
            crate::event::CursorShape::Hand => NSCursor::pointingHandCursor(),
            crate::event::CursorShape::Text => NSCursor::IBeamCursor(),
            crate::event::CursorShape::Arrow => NSCursor::arrowCursor(),
        };
        cursor.set();
    }
}

/// macOS keyCode → 框架特殊键。返回 None 表示走文本/快捷键路径。
///
/// `pub(super)` 是给 `hotkey.rs` 的单测用的：那边有一张反向的表（`Key` → keyCode，
/// 注册全局热键用），两张表必须互逆，否则会出现"注册的是 Home、按下去却当 End 处理"
/// 这种两处各自看着都对的错位。
pub(super) fn map_special(key_code: u16) -> Option<Key> {
    Some(match key_code {
        0x30 => Key::Tab,       // 48
        0x24 => Key::Enter,     // 36 Return
        0x4C => Key::Enter,     // 76 KeypadEnter
        0x35 => Key::Escape,    // 53
        0x31 => Key::Space,     // 49
        0x33 => Key::Backspace, // 51 Delete(退格)
        0x75 => Key::Delete,    // 117 ForwardDelete
        0x7B => Key::Left,      // 123
        0x7C => Key::Right,     // 124
        0x7D => Key::Down,      // 125
        0x7E => Key::Up,        // 126
        0x73 => Key::Home,      // 115
        0x77 => Key::End,       // 119
        0x74 => Key::PageUp,    // 116
        0x79 => Key::PageDown,  // 121
        _ => return None,
    })
}

// libdispatch FFI：跨线程把工作派回主线程。`_dispatch_main_q` 即 `dispatch_get_main_queue()`
// 宏所取的全局主队列对象；`dispatch_async_f` 异步入队一个无捕获的 C 函数。
extern "C" {
    static _dispatch_main_q: c_void;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
}

/// 主线程蹦床：dispatch 回主线程后标脏所有窗口。此刻必在主线程，thread_local 的登记表
/// 与建表方同属主线程；render 前会排空消息通道（`UiHost::render` 的 pump），故唤醒即
/// 取到最新数据。
extern "C" fn wake_on_main(_ctx: *mut c_void) {
    mark_all_windows_dirty();
}

/// 跨线程唤醒句柄。`signal` 经 libdispatch 派回主线程标脏**所有**窗口。
///
/// **不绑定任何一个视图**（早期版本持视图裸指针）：唤醒的目标是"这个应用"——后台线程
/// 送来的消息由主窗的 `App::channel` pump 排空——绑在某个窗口上，那个窗口一关唤醒就
/// 静默丢失。对照 win32：那边同样从"投给唯一那个窗口"改成了投给 App 级 message-only
/// 宿主再广播。托盘无此问题：`NSStatusItem` 本就由 `run_windowed` 的栈持有，已是 App 级。
///
/// 无字段故自动 `Send`：不再需要为裸指针补 `unsafe impl`。
struct MacWake;
impl crate::sync::RawWakeSignal for MacWake {
    fn signal(&self) {
        unsafe {
            dispatch_async_f(
                std::ptr::addr_of!(_dispatch_main_q),
                std::ptr::null_mut(),
                wake_on_main,
            );
        }
    }
}

#[cfg(feature = "gpu")]
static GPU_LOST_NOTICE: std::sync::Once = std::sync::Once::new();

/// 进程内只提示一次。每帧刷屏没人会看，一次刚好够把判断送到眼前（同 `render/gpu/canvas.rs`）。
#[cfg(feature = "gpu")]
fn notice_once(once: &std::sync::Once, msg: &str) {
    once.call_once(|| eprintln!("{msg}"));
}

/// `WINDUI_GPU` 的三档语义。对标 win32 的 `WINDUI_D2D`，多一档"显式关"：
/// 没有它就没法在有 GPU 的机器上验证回退路径（而回退路径本身就是被测对象）。
///
/// | 取值 | 含义 |
/// | --- | --- |
/// | 未设置 | 听 [`Renderer`]：`Auto`/`Gpu` 尝试，`Software` 不试 |
/// | `0` 或空 | 一律不试。此时 `Renderer::Gpu` **报错终止**——它的用途就是"拿不到 GPU 要告诉我" |
/// | 其他值 | 一律尝试（`Software` 也当 `Auto`），排障用 |
#[cfg(feature = "gpu")]
fn wants_gpu(renderer: Renderer) -> bool {
    match std::env::var("WINDUI_GPU") {
        Err(_) => renderer.wants_gpu(),
        Ok(v) if v.is_empty() || v == "0" => false,
        Ok(_) => true,
    }
}

/// 给窗口接上 GPU 后端：建 `CAMetalLayer` → 由它建 wgpu surface → 挂进视图的图层树。
///
/// 顺序是刻意的：**先把 surface 建成再碰视图**。反过来（先 `setWantsLayer` 再建 surface）
/// 一旦 surface 失败，就得把已经装上去的 Metal 层再摘掉，而"摘一半"的视图状态没人验证过。
///
/// 失败处理与 win32 的 D2D 接线同语义：`Renderer::Auto` 静默回退软路径（stderr 一行），
/// `Renderer::Gpu` 报错终止（静默换一条路会让基于它做的验证失去意义）。
#[cfg(feature = "gpu")]
fn attach_gpu(view: &ContentView, window: &NSWindow, renderer: Renderer) {
    if !wants_gpu(renderer) {
        assert!(
            !renderer.requires_gpu(),
            "Renderer::Gpu 要求 GPU 渲染，但 WINDUI_GPU=0 已显式禁用 GPU 后端。\
             需要自动回退请改用 Renderer::Auto"
        );
        return;
    }
    let Some(gpu) = SharedGpu::get() else {
        assert!(
            !renderer.requires_gpu(),
            "Renderer::Gpu 要求 GPU 渲染，但 wgpu 设备创建失败（硬件与软件适配器都拿不到）。\
             需要自动回退请改用 Renderer::Auto"
        );
        eprintln!("[windui] wgpu 设备不可用，回退软渲染（Skia）");
        return;
    };

    let scale = window.backingScaleFactor().max(0.1);
    let bounds = view.bounds();
    let size = (
        (bounds.size.width * scale).round().max(1.0) as u32,
        (bounds.size.height * scale).round().max(1.0) as u32,
    );

    let layer = CAMetalLayer::new();
    // 内容按物理像素呈现：contentsScale 决定 layer 的点→像素换算，不设的话 Retina 屏上
    // 会拿 1x 的像素去铺 2x 的区域（画面糊一半）。
    layer.setContentsScale(scale);
    // 窗口底色恒不透明（每帧先铺 bg），告诉合成器不必在它背后再混一层。
    layer.setOpaque(true);
    set_layer_frame(&layer, bounds.size);

    // 安全：`SurfaceTargetUnsafe::CoreAnimationLayer` 只存裸指针，要求 layer 活得比 surface 久。
    // 这一条由 `ViewState` 的字段顺序保证：`gpu`（持 surface）声明在 `metal_layer`（持这份
    // `Retained`）之前，故析构时 surface 先没、layer 后没；两者又同属一个 `ViewState`，
    // 中途不可能只掉一个。视图的图层树里也持着这张 layer（下面 `addSublayer` 之后），是第二道保险。
    let surface = unsafe {
        gpu.instance()
            .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::CoreAnimationLayer(
                Retained::as_ptr(&layer) as *mut c_void,
            ))
    };
    let win_gpu = match surface {
        Ok(s) => WindowGpu::new(gpu, s, size),
        Err(e) => {
            eprintln!("[windui] wgpu surface 创建失败: {e}");
            None
        }
    };
    let Some(win_gpu) = win_gpu else {
        assert!(
            !renderer.requires_gpu(),
            "Renderer::Gpu 要求 GPU 渲染，但 CAMetalLayer surface 建不起来。\
             需要自动回退请改用 Renderer::Auto"
        );
        eprintln!("[windui] GPU surface 不可用，回退软渲染（Skia）");
        return;
    };
    if crate::render::prof::enabled() {
        eprintln!("[windui] {}", win_gpu.info());
    }

    {
        let mut st = view.ivars().borrow_mut();
        st.gpu = Some(win_gpu);
        st.metal_layer = Some(layer.clone());
    }
    // Metal 层挂成 **AppKit 那张 backing layer 的子层**，而不是用 `makeBackingLayer` 直接
    // 顶替 backing layer 本身（`MTKView` 那种写法）。
    //
    // 理由是实测出来的：视图自带的 layer 一旦不是 AppKit 建的，AppKit 就把 `layerContents-
    // RedrawPolicy` 置成 `Never` 并**再也不发 `drawRect:`/`updateLayer`**——`wantsUpdateLayer`
    // 照问不误（真机上问了两次），但出帧回调一次都不来，窗口停在空白。手动把 policy 改回
    // `DuringViewResize` 也救不回来。而挂成子层时 backing layer 仍是 AppKit 自己的那张，
    // `setNeedsDisplay(true)` → `updateLayer` 这条链路完全照旧——于是本文件里十几处标脏调用
    // 一处都不用改，两条渲染路径共用同一套失效机制。
    //
    // 借用须先释放：这些 AppKit 调用可能同步回调进本视图（那里要借 `ViewState`）。
    view.setWantsLayer(true);
    if let Some(root) = view.layer() {
        root.addSublayer(&layer);
    }
}

/// 让 Metal 子层铺满视图（原点恒为 0），并**关掉隐式动画**。
///
/// CALayer 的几何属性默认带 0.25s 隐式动画：不关的话拖动窗口边框时 Metal 层会一路"追"着
/// 视图慢半拍，看起来像整个界面在弹。
#[cfg(feature = "gpu")]
fn set_layer_frame(layer: &CAMetalLayer, size: NSSize) {
    CATransaction::begin();
    CATransaction::setDisableActions(true);
    layer.setFrame(NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size,
    });
    CATransaction::commit();
}

/// 建一个窗口：`NSWindow` + 内容视图 + 委托 + `on_interval` 定时器，登记进 [`WINDOWS`]。
///
/// **不显示**——主窗要照顾 `start_hidden`、子窗建好即显，由调用方决定（对照 win32 把
/// `create_window` 与 `show_window` 拆开的同一理由）。
///
/// 与 win32 的同名函数不同，这里不返回 `Option`：`NSWindow` 的初始化不像
/// `CreateWindowExW` 那样会因为类没注册、句柄无效而失败，拿不到窗口只有内存耗尽一途。
fn create_window(
    mtm: MainThreadMarker,
    cfg: &WindowConfig,
    handler: Box<dyn AppHandler>,
    // 只在 `gpu` feature 下用于选后端。签名对两档保持一致（调用方不必分 feature 分支），
    // 故仅在关掉那一档时抑制未使用告警——同 win32 `create_window` 的 `renderer` 参数。
    #[cfg_attr(not(feature = "gpu"), allow(unused_variables))] renderer: Renderer,
) -> Retained<NSWindow> {
    // 内容矩形为逻辑点尺寸（AppKit 在高 DPI 下自动按 backingScale 放大像素）。
    let content_rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: cfg.width as f64,
            height: cfg.height as f64,
        },
    };

    let mut style =
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
    if cfg.resizable {
        style |= NSWindowStyleMask::Resizable;
    }

    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content_rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str(&cfg.title));
    // 生命周期归 [`WINDOWS`] 表，不让 AppKit 在 `close` 时替我们 release——否则表里那份
    // `Retained` 立刻悬垂。详见 `WINDOWS` 的说明。
    //
    // 安全：本方法之所以是 `unsafe`，正因为它改的就是窗口的所有权语义——置 `true` 会让
    // AppKit 在关闭时替对象 release 一次，与 `Retained` 各自记账的引用计数对不上。我们
    // 置的是 `false`，即"谁也别替我 release"，引用计数从此完全由 `WINDOWS`/`CLOSED` 两张
    // 表决定，这正是 `Retained` 成立的前提。
    unsafe { window.setReleasedWhenClosed(false) };

    // 最小客户区尺寸（逻辑点，0=不限制某轴）：对照 win32 的 WM_GETMINMAXINFO，防止用户把
    // 窗口缩到操作不到按钮。macOS 以点为单位、无需按 DPI 换算——AppKit 自动按 backingScale
    // 映射到物理像素；setContentMinSize 约束的是内容区（与 win32 的 ptMinTrackSize 客户区语义一致）。
    if cfg.min_width > 0 || cfg.min_height > 0 {
        window.setContentMinSize(NSSize {
            width: cfg.min_width.max(0) as f64,
            height: cfg.min_height.max(0) as f64,
        });
    }

    // 无边框窗口：隐藏系统标题栏与三枚标准按钮（应用自绘标题栏与按钮），客户区铺满整窗，
    // 保留系统级吸附/阴影/缩放。拖动经 mouseDown→performWindowDragWithEvent 自管。
    if cfg.frameless {
        window.setStyleMask(style | NSWindowStyleMask::FullSizeContentView);
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        for b in [
            NSWindowButton::CloseButton,
            NSWindowButton::MiniaturizeButton,
            NSWindowButton::ZoomButton,
        ] {
            if let Some(btn) = window.standardWindowButton(b) {
                btn.setHidden(true);
            }
        }
    }

    let view = ContentView::new(mtm, content_rect, handler, cfg.bg, cfg.frameless);
    // 首帧用窗口实际 backingScale 设好缩放。
    let scale = window.backingScaleFactor() as f32;
    {
        let mut st = view.ivars().borrow_mut();
        st.scale = scale;
        st.handler.set_scale(scale);
    }
    window.setContentView(Some(&view));
    // GPU 后端选择：`renderer` 想要 GPU（或 `WINDUI_GPU` 强制）时，把内容视图的呈现换成
    // CAMetalLayer。放在 `setContentView` 之后——layer 要挂进窗口的图层树，且此刻
    // `backingScaleFactor` 才是这个窗口真实的那个。离屏截图走 `run_offscreen`，不到此处。
    #[cfg(feature = "gpu")]
    attach_gpu(&view, &window, renderer);
    window.setAcceptsMouseMovedEvents(true);
    // 登记进活动窗口表（连同所有权）：`windowWillClose:` 据此判断自己是不是最后一个。
    // 设 delegate **之前**登记——设完就可能收到关闭回调，那时窗口必须已经在表里。
    register_window(window.clone(), cfg.single.clone());
    // 窗口关闭时退出应用（视图兼任窗口委托）。隐藏到托盘走 orderOut，不触发关闭，故不退出。
    window.setDelegate(Some(ProtocolObject::from_ref(&*view)));
    view.refresh_tracking_area();
    let _ = window.makeFirstResponder(Some(&view));

    if cfg.centered {
        window.center();
    }

    // 动画帧驱动改为自调度的一次性定时器（见 ContentView::schedule_next_frame）：跟随显示器
    // 刷新率、空闲零唤醒。首帧 drawRect 由 makeKeyAndOrderFront 触发，其后按需自续约。

    // on_interval：按 handler 注册的间隔安装周期 NSTimer。
    view.install_interval_timers();

    window
}

/// 窗口端运行：创建 `NSApplication` + 主窗，进入事件循环（阻塞至退出）。
pub(crate) fn run_windowed(
    mut cfg: WindowConfig,
    handler: Box<dyn AppHandler>,
    waker: Option<std::sync::Arc<crate::sync::WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
) {
    let mtm = MainThreadMarker::new().expect("macOS GUI 必须在主线程运行");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // 后端档位登记为应用级：子窗建出来时要跟主窗走同一条渲染路径（见 `APP_RENDERER`）。
    APP_RENDERER.with(|r| r.set(cfg.renderer));
    let window = create_window(mtm, &cfg, handler, cfg.renderer);

    // 跨线程唤醒：绑一个不指向任何窗口的句柄（见 MacWake）；后台线程 send 经 dispatch
    // 派回主线程标脏。绑定前积压的 wake 由 WakerShared 的 pending 兜底补发。
    if let Some(w) = &waker {
        w.bind(Box::new(MacWake));
    }
    // 单实例首实例：起 accept 线程接收二次实例 argv（收到后经 libdispatch 派回主线程
    // 切页 + 激活窗口）。窗口指针存活至进程退出，对照 MacWake 持视图指针的做法。
    if let Some(si) = single {
        crate::single_instance::install_listener(
            &si.app_id,
            Retained::as_ptr(&window) as isize,
            si.on_second,
        );
        // 自定义 URL scheme（`myapp://…`）与二次实例共用 on_second，故与它同一处安装。
        // **必须在事件循环起来之前**：由链接拉起的那一次启动，Apple Event 已排在队列里，
        // 装晚了就直接丢掉（表现为「第一次点链接没反应，第二次才行」）。
        super::url_scheme::install();
    }

    // 主窗登记：托盘与全局热键的"唤出窗口"指的是它。装热键**之前**设好——热键一旦
    // 注册就可能立刻被按下。
    MAIN_WINDOW.with(|w| *w.borrow_mut() = Some(window.clone()));

    // 全局热键（若配置）：Carbon RegisterEventHotKey，见 platform/macos/hotkey.rs。
    if !cfg.hotkeys.is_empty() {
        super::hotkey::install(std::mem::take(&mut cfg.hotkeys));
    }

    // 系统托盘（若配置）：窗口创建后安装；TrayState 须存活至退出（按钮 target 为弱引用）。
    let _tray = cfg
        .tray
        .take()
        .and_then(|t| super::tray::install(mtm, window.clone(), t));

    // 启动即隐藏：不 orderFront，窗口保持不可见，等托盘唤起。
    // 注意 app.activate() 仍需调用——否则应用不在前台，托盘菜单交互会异常。
    if !cfg.start_hidden {
        window.makeKeyAndOrderFront(None);
    }
    super::activate_app(&app);
    app.run();
    drop(_tray);

    // 事件循环结束后释放 GPU 共享设备链，对照 win32 消息循环之后的 `release_shared_device`。
    //
    // 实话：`NSApplication::terminate:` 正常情况下直接结束进程，这一行执行不到（`drop(_tray)`
    // 同理，它一直就在这儿）。保留它是因为"事件循环结束就释放设备"这条契约得有个落点——
    // 将来若改用可被 `stop:` 打断的 run loop，缺了它就是设备泄漏，而那种泄漏很难被注意到。
    #[cfg(feature = "gpu")]
    crate::render::gpu::release_shared_gpu();
}
