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
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSCursor,
    NSDraggingDestination, NSDraggingInfo, NSDragOperation, NSEvent, NSGraphicsContext,
    NSPasteboardType, NSTextInputClient, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow,
    NSWindowButton, NSWindowDelegate, NSWindowStyleMask, NSWindowTitleVisibility,
};
// 已弃用但在现行 macOS 仍有效，且读取拖入路径列表最简。
#[allow(deprecated)]
use objc2_app_kit::NSFilenamesPboardType;
use objc2_core_foundation::{CFRetained, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{
    CGBitmapContextCreate, CGBitmapContextCreateImage, CGColorSpace, CGContext, CGImageAlphaInfo,
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

        /// 动画帧驱动：有控件请求持续动画时按帧重绘（对应 win32 的帧配速循环）。
        #[unsafe(method(tick:))]
        fn tick(&self, _timer: &NSTimer) {
            let animating = self.ivars().borrow().handler.wants_animation();
            if animating {
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
            self.ivars().borrow_mut().composing = !anyobject_to_string(string).is_empty();
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            self.ivars().borrow_mut().composing = false;
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

    // 窗口委托：窗口被关闭（关闭按钮 / `wants_close`）时退出应用——对照 win32 的
    // WM_DESTROY→PostQuitMessage。注意 `orderOut`（隐藏到托盘）不触发此回调，故隐藏不会退出。
    unsafe impl NSWindowDelegate for ContentView {
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
        let Some(plist) = pb.propertyListForType(ty) else { return false };
        // 属性列表为 NSArray<NSString>（路径）；无参 NSArray 才是可降级目标，逐项再降为 NSString。
        let Ok(arr) = plist.downcast::<NSArray>() else { return false };
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
        let pos = Point::new((view_pt.x as f32 * scale).round() as i32, (view_pt.y as f32 * scale).round() as i32);
        let repaint = self.ivars().borrow_mut().handler.on_drop_files(pos, paths);
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
            st.handler.render(unsafe { &mut *ptr }, size);

            // 把缓冲包成位图上下文 → CGImage（CGImage 会复制数据，复用缓冲安全）。
            let bytes_per_row = pw as usize * 4;
            let data = st.pixmap.as_mut().unwrap().data_mut().as_mut_ptr() as *mut c_void;
            let cs = st.color_space.clone();
            let ctx = unsafe {
                CGBitmapContextCreate(
                    data,
                    pw as usize,
                    ph as usize,
                    8,
                    bytes_per_row,
                    Some(&cs),
                    CGImageAlphaInfo::PremultipliedLast.0,
                )
            };
            ctx.and_then(|c| CGBitmapContextCreateImage(Some(&c)))
        };

        let Some(image) = image else { return };
        let Some(gctx) = NSGraphicsContext::currentContext() else { return };
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
                size: CGSize { width: bounds.size.width, height: bounds.size.height },
            },
            Some(&image),
        );
        CGContext::restore_g_state(Some(&cg));
    }

    /// 窗口坐标 → 客户区物理像素（左上原点）。
    fn loc_phys(&self, ev: &NSEvent) -> Point {
        let win_pt = ev.locationInWindow();
        let view_pt = self.convertPoint_fromView(win_pt, None);
        let scale = self.ivars().borrow().scale;
        Point::new((view_pt.x as f32 * scale).round() as i32, (view_pt.y as f32 * scale).round() as i32)
    }

    /// 鼠标按下/抬起/移动 → PointerEvent。
    fn on_pointer(&self, ev: &NSEvent, kind: PointerKind, button: MouseButton) {
        let pos = self.loc_phys(ev);
        let click_count = if matches!(kind, PointerKind::Down) {
            (ev.clickCount().max(1) as u8).min(3)
        } else {
            1
        };
        self.dispatch_pointer(PointerEvent { kind, pos, button, click_count });
    }

    /// 滚轮 → Wheel 事件。框架约定一刻度 ±120（正=上滚）。
    fn on_wheel(&self, ev: &NSEvent) {
        let dy = ev.scrollingDeltaY();
        // 触控板（精确增量）：按点位细粒度滚；鼠标滚轮（行增量）：每行约一刻度。
        let delta = if ev.hasPreciseScrollingDeltas() {
            (dy * 3.0) as i32
        } else {
            (dy * 40.0) as i32
        };
        if delta == 0 {
            return;
        }
        let pos = self.loc_phys(ev);
        self.dispatch_pointer(PointerEvent::single(PointerKind::Wheel(delta), pos, MouseButton::Left));
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
            self.dispatch_key(KeyEvent { key: k, pressed: true, shift, ctrl: modk });
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
        for c in text.chars() {
            if c.is_control() {
                continue;
            }
            self.dispatch_key(KeyEvent { key: Key::Char(c), pressed: true, shift: false, ctrl: false });
        }
    }

    /// 焦点文本控件的光标矩形（屏幕坐标，y 向上），供输入法定位候选窗。
    fn ime_caret_rect(&self) -> NSRect {
        let (caret, scale) = {
            let st = self.ivars().borrow();
            (st.handler.ime_caret(), st.scale)
        };
        let Some((x, y, h)) = caret else {
            return NSRect { origin: NSPoint { x: 0.0, y: 0.0 }, size: NSSize { width: 0.0, height: 0.0 } };
        };
        // 物理像素 → 视图点（翻转视图：左上原点、y 向下）。
        let s = scale as f64;
        let view_rect = NSRect {
            origin: NSPoint { x: x as f64 / s, y: y as f64 / s },
            size: NSSize { width: 1.0, height: (h as f64 / s).max(1.0) },
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
        let repaint = self.ivars().borrow_mut().handler.on_pointer(ev);
        if repaint {
            self.setNeedsDisplay(true);
        }
        self.after_event();
    }

    fn dispatch_key(&self, ev: KeyEvent) {
        let repaint = self.ivars().borrow_mut().handler.on_key(ev);
        if repaint {
            self.setNeedsDisplay(true);
        }
        self.after_event();
    }

    /// 事件分发后：执行待处理窗口操作、应用光标、必要时关窗。
    fn after_event(&self) {
        let (op, close) = {
            let mut st = self.ivars().borrow_mut();
            (st.handler.take_window_op(), st.handler.wants_close())
        };
        if let Some(op) = op {
            if let Some(win) = self.window() {
                match op {
                    WindowOp::Minimize => win.miniaturize(None),
                    WindowOp::ToggleMaximize => win.zoom(None),
                }
            }
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

/// 窗口端运行：创建 `NSApplication` + `NSWindow` + 自定义 `NSView`，进入事件循环（阻塞至退出）。
pub fn run_windowed(mut cfg: WindowConfig, handler: Box<dyn AppHandler>) {
    let mtm = MainThreadMarker::new().expect("macOS GUI 必须在主线程运行");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    // 内容矩形为逻辑点尺寸（AppKit 在高 DPI 下自动按 backingScale 放大像素）。
    let content_rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize { width: cfg.width as f64, height: cfg.height as f64 },
    };

    let mut style = NSWindowStyleMask::Titled
        | NSWindowStyleMask::Closable
        | NSWindowStyleMask::Miniaturizable;
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

    // 无边框窗口：隐藏系统标题栏与三枚标准按钮（应用自绘标题栏与按钮），客户区铺满整窗，
    // 保留系统级吸附/阴影/缩放。拖动经 mouseDown→performWindowDragWithEvent 自管。
    if cfg.frameless {
        window.setStyleMask(style | NSWindowStyleMask::FullSizeContentView);
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::Hidden);
        for b in [NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton] {
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

    // 动画帧驱动：60Hz 定时器，仅在有动画请求时重绘（空闲为空操作）。
    unsafe {
        NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
            1.0 / 60.0,
            &view,
            sel!(tick:),
            None,
            true,
        );
    }

    // 系统托盘（若配置）：窗口创建后安装；TrayState 须存活至退出（按钮 target 为弱引用）。
    let _tray = cfg.tray.take().and_then(|t| super::tray::install(mtm, window.clone(), t));

    window.makeKeyAndOrderFront(None);
    app.activate();
    app.run();
    drop(_tray);
}
