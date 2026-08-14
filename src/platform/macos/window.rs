//! macOS 窗口、事件循环与呈现（Cocoa/AppKit + Core Graphics）。
//!
//! 渲染全在 CPU：单份 tiny-skia `Pixmap`（RGBA 预乘）作后备缓冲；呈现时用
//! `CGBitmapContext` 把缓冲转成 `CGImage`，在自定义 `NSView::drawRect:` 里
//! `CGContextDrawImage` 拷屏。空闲时阻塞在 `NSApplication` 的 run loop，零阻塞渲染。
//!
//! 对照 `platform/win32/mod.rs`（消息循环 + GDI 呈现）。坐标统一：事件按
//! **物理像素、相对客户区左上角**上交（视图设为 `isFlipped`，故点坐标即左上原点）。

use std::cell::RefCell;
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

use tiny_skia::Pixmap;

use super::{AppHandler, WindowConfig};
use crate::event::{Key, KeyEvent, MouseButton, PointerEvent, PointerKind, WindowOp};
use crate::geometry::{Color, Point, Size};
use crate::platform::to_skia_color;

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

        // 窗口即将销毁时退出应用（对照 win32 WM_DESTROY→PostQuitMessage）。
        // 注意 `orderOut`（隐藏到托盘）不触发此回调，故隐藏不会退出。
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            let mtm = MainThreadMarker::from(self);
            NSApplication::sharedApplication(mtm).terminate(None);
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

        // 渲染进 pixmap（借用期间不触发可重入的 OS 调用）。
        let image = {
            let mut st = self.ivars().borrow_mut();
            if (st.scale - scale).abs() > 0.001 {
                st.scale = scale;
                st.handler.set_scale(scale);
            }
            st.ensure_pixmap(pw, ph);
            let bg = st.bg;
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

    /// 动画帧驱动：废止上一个待触发的帧定时器，若仍在动画则按刷新率调度下一次一次性重绘。
    /// 仅在动画期间存在定时器，空闲时无定时器（零唤醒，优于常驻定时器）。对应 win32 消息循环的帧配速。
    fn schedule_next_frame(&self) {
        if let Some(t) = self.ivars().borrow_mut().frame_timer.take() {
            t.invalidate();
        }
        if !self.ivars().borrow().handler.wants_animation() {
            return;
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
        let (op, dialog, close) = {
            let mut st = self.ivars().borrow_mut();
            (
                st.handler.take_window_op(),
                st.handler.take_dialog_request(),
                st.handler.wants_close(),
            )
        };
        if let Some(op) = op {
            if let Some(win) = self.window() {
                match op {
                    WindowOp::Minimize => win.miniaturize(None),
                    WindowOp::ToggleMaximize => win.zoom(None),
                    // 与 macos/tray.rs 的 TrayCtx::show_window / hide_window 同源：
                    // 二者是同一语义的两个入口（托盘点击 / 控件请求），实现必须一致。
                    WindowOp::Show => {
                        win.makeKeyAndOrderFront(None);
                        // 隐藏期间应用可能已失去激活态，仅 orderFront 不足以到前台。
                        NSApplication::sharedApplication(MainThreadMarker::from(self)).activate();
                    }
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
        self.apply_cursor();
        if close {
            if let Some(win) = self.window() {
                win.close();
            }
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
fn map_special(key_code: u16) -> Option<Key> {
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

/// 主线程蹦床：dispatch 回主线程后标脏一帧。此刻必在主线程，裸指针解引用安全；
/// render 前会排空消息通道（UiHost::render 的 pump 排空），故唤醒即取到最新数据。
extern "C" fn wake_on_main(ctx: *mut c_void) {
    let view = ctx as *const ContentView;
    // 视图随窗口存活至进程退出，指针在 run loop 期间始终有效（对照 win32 PostMessage 到 HWND）。
    unsafe { (*view).setNeedsDisplay(true) };
}

/// 跨线程唤醒句柄：仅持视图裸指针（as usize 以满足 Send）。signal 经 dispatch 派回主线程，
/// 线程安全。对照 win32 的 `Win32Wake`（持 HWND 数值 + PostMessage）。
struct MacWake {
    view: usize,
}
unsafe impl Send for MacWake {}
impl crate::sync::RawWakeSignal for MacWake {
    fn signal(&self) {
        unsafe {
            dispatch_async_f(
                std::ptr::addr_of!(_dispatch_main_q),
                self.view as *mut c_void,
                wake_on_main,
            );
        }
    }
}

/// 窗口端运行：创建 `NSApplication` + `NSWindow` + 自定义 `NSView`，进入事件循环（阻塞至退出）。
pub(crate) fn run_windowed(
    mut cfg: WindowConfig,
    handler: Box<dyn AppHandler>,
    waker: Option<std::sync::Arc<crate::sync::WakerShared>>,
    single: Option<crate::single_instance::SingleInstance>,
) {
    let mtm = MainThreadMarker::new().expect("macOS GUI 必须在主线程运行");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

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
    window.setAcceptsMouseMovedEvents(true);
    // 窗口关闭时退出应用（视图兼任窗口委托）。隐藏到托盘走 orderOut，不触发关闭，故不退出。
    window.setDelegate(Some(ProtocolObject::from_ref(&*view)));
    view.refresh_tracking_area();
    let _ = window.makeFirstResponder(Some(&view));

    if cfg.centered {
        window.center();
    }

    // 动画帧驱动改为自调度的一次性定时器（见 ContentView::schedule_next_frame）：跟随显示器
    // 刷新率、空闲零唤醒。首帧 drawRect 由 makeKeyAndOrderFront 触发，其后按需自续约。

    // 跨线程唤醒：把视图裸指针回填进 WakerShared；后台线程 send 经 dispatch 派回主线程标脏一帧。
    // 窗口建好后再绑定，绑定前积压的 wake 由 WakerShared 的 pending 兜底补发。
    if let Some(w) = &waker {
        w.bind(Box::new(MacWake {
            view: Retained::as_ptr(&view) as usize,
        }));
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

    // on_interval：按 handler 注册的间隔安装周期 NSTimer。
    view.install_interval_timers();

    // 全局热键（若配置）：macOS 尚未实现，此处仅触发 debug 期提示。
    // 详见 platform/macos/hotkey.rs——Windows 侧已实现，macOS 需 Carbon RegisterEventHotKey。
    if !cfg.hotkeys.is_empty() {
        super::hotkey::HotkeyState::register(std::mem::take(&mut cfg.hotkeys));
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
    app.activate();
    app.run();
    drop(_tray);
}
