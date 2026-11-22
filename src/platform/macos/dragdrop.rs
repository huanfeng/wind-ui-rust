//! 拖出到外部程序：`NSView::beginDraggingSessionWithItems` + `public.file-url`。
//!
//! 对照 win32 的 `dragdrop`（OLE `DoDragDrop` + `CF_HDROP`），契约相同：接收方（Finder 等）
//! 拿到的是标准的文件 URL 列表，**自己完成**复制 / 移动，本方只提供路径与允许的效果。
//!
//! 与 win32 的两处结构差异：
//!
//! - **阻塞要自己做**。`DoDragDrop` 自带消息泵、松手才返回；AppKit 的拖动会话是异步的，
//!   `beginDraggingSession…` 立即返回。这里接着跑一个嵌套事件循环（`nextEventMatchingMask`
//!   加 `sendEvent`），直到 `draggingSession:endedAtPoint:operation:` 报告结束。性质与
//!   `DoDragDrop` 的内部消息泵相同：期间其它事件照常派发，窗口照常重绘。
//! - **`before_drop` 挂在延迟数据上**。路径以 `NSPasteboardItemDataProvider` 延迟提供，接收方
//!   第一次真正读取文件 URL 时才调 `before_drop`。Finder 在松手时读取；个别程序在指针悬停时
//!   就读（用来判断能否接收），这时回调会提前到悬停期间——文件同样在放下前就绪，只是落空
//!   的拖动也可能已经生成了文件，调用方本来就要处理「回调跑过但接收方没收」的情形。
//!
//! 触发拖动需要那次鼠标事件：调用方经 `ctx.defer_blocking` 调用时，延迟闭包在本次事件派发的
//! 末尾执行，`NSApp.currentEvent` 仍是触发它的 `mouseDragged:`，直接拿来用。

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::time::{Duration, Instant};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEventMask, NSEventType, NSPasteboard, NSPasteboardItem,
    NSPasteboardItemDataProvider, NSPasteboardType, NSPasteboardTypeFileURL, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSDate, NSDefaultRunLoopMode, NSPoint, NSRect, NSSize, NSString, NSURL,
};

use crate::platform::DragEffect;

/// 延迟提供时，把文件 URL 先存在条目的这个私有类型上，数据提供者从这里取。
const PATH_TYPE: &str = "com.windui.drag-out.file-url";

/// 看到 mouseUp 从嵌套循环里经过后最多再等多久。正常情况下 mouseUp 被拖动会话自己吞掉，
/// 不会出现在这里；出现说明会话没能开始（例如鼠标在闭包运行前已经松开），而会话结束
/// 回调可能永远不来——不设这个上限，`drag_files` 就会永远阻塞。要比落空时预览图滑回
/// 原处的动画（约半秒）长得多，免得把仍在收尾的会话当成没开始。
const SESSION_START_GRACE: Duration = Duration::from_secs(2);

thread_local! {
    /// 上一次拖出的数据源，留到下一次拖出开始后才释放。
    ///
    /// AppKit 在 `drag_files` 返回之后仍可能找它：兜底退出时会话可能还没真正结束；延迟
    /// 数据的提供者是否被 `NSPasteboardItem` 强引用，文档没有写明，拖动剪贴板被下一次拖动
    /// 清空前还可能来要数据。留着它，这些回调就不会落到已释放的对象上。代价是常驻一个
    /// 小对象，以及它捕获的 `before_drop`（已跑过的是 `None`）。
    static LAST_SOURCE: RefCell<Option<Retained<DragSource>>> =
        const { RefCell::new(None) };
}

struct DragState {
    allow_move: bool,
    before_drop: RefCell<Option<Box<dyn FnOnce() -> bool>>>,
    /// `before_drop` 的结果。`None` = 还没问过。
    verdict: Cell<Option<bool>>,
    ended: Cell<bool>,
    operation: Cell<NSDragOperation>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "WindUiDragOutSource"]
    #[ivars = DragState]
    struct DragSource;

    unsafe impl NSObjectProtocol for DragSource {}

    unsafe impl NSDraggingSource for DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn source_operation_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            allowed_operations(self.ivars().allow_move)
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, op: NSDragOperation) {
            self.ivars().operation.set(op);
            self.ivars().ended.set(true);
        }
    }

    unsafe impl NSPasteboardItemDataProvider for DragSource {
        #[unsafe(method(pasteboard:item:provideDataForType:))]
        fn provide_data(
            &self,
            _pasteboard: Option<&NSPasteboard>,
            item: &NSPasteboardItem,
            ty: &NSPasteboardType,
        ) {
            if !self.ivars().files_ready() {
                return;
            }
            if let Some(url) = item.stringForType(&NSString::from_str(PATH_TYPE)) {
                item.setString_forType(&url, ty);
            }
        }
    }
);

impl DragSource {
    fn new(
        mtm: MainThreadMarker,
        allow_move: bool,
        before_drop: Option<Box<dyn FnOnce() -> bool>>,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(DragState {
            allow_move,
            before_drop: RefCell::new(before_drop),
            verdict: Cell::new(None),
            ended: Cell::new(false),
            operation: Cell::new(NSDragOperation::None),
        });
        // SAFETY: NSObject 的指定初始化方法。
        unsafe { msg_send![super(this), init] }
    }
}

impl DragState {
    /// 接收方来要文件 URL 了：首次时调 `before_drop`，之后沿用结果（每个文件一个条目，
    /// 会来要多次，回调只跑一次）。
    fn files_ready(&self) -> bool {
        let st = self;
        if let Some(v) = st.verdict.get() {
            return v;
        }
        // 会话落空结束后才来要：是 AppKit 收尾时解析剩下的延迟数据，没有人要放下。
        if st.ended.get() && st.operation.get() == NSDragOperation::None {
            st.verdict.set(Some(false));
            return false;
        }
        // 先记成 false 再调：回调里弹模态框会跑嵌套事件循环，期间接收方再来要的话
        // 不能当成「已就绪」把还没生成好的路径交出去。
        st.verdict.set(Some(false));
        let cb = st.before_drop.borrow_mut().take();
        let ok = cb.is_none_or(|f| f());
        st.verdict.set(Some(ok));
        ok
    }
}

fn allowed_operations(allow_move: bool) -> NSDragOperation {
    let mut ops = NSDragOperation::Copy | NSDragOperation::Link;
    if allow_move {
        ops |= NSDragOperation::Move;
    }
    ops
}

/// 接收方执行的操作 → [`DragEffect`]。`Generic` 是「由接收方决定」，Finder 等按复制处理。
fn effect_of(op: NSDragOperation) -> DragEffect {
    if op.contains(NSDragOperation::Move) {
        DragEffect::Move
    } else if op.intersects(NSDragOperation::Copy | NSDragOperation::Generic) {
        DragEffect::Copy
    } else if op.contains(NSDragOperation::Link) {
        DragEffect::Link
    } else {
        DragEffect::None
    }
}

/// 见 win32 `drag_files`。
pub fn drag_files(paths: &[impl AsRef<Path>], allow_move: bool) -> DragEffect {
    drag_files_with(paths, allow_move, None)
}

/// 见 win32 `drag_files_with`。macOS 上 `before_drop` 的调用时机见模块文档。
pub fn drag_files_with(
    paths: &[impl AsRef<Path>],
    allow_move: bool,
    before_drop: Option<Box<dyn FnOnce() -> bool>>,
) -> DragEffect {
    if paths.is_empty() {
        return DragEffect::None;
    }
    let Some(mtm) = MainThreadMarker::new() else {
        log::warn!("drag_files 须在主线程调用");
        return DragEffect::None;
    };
    let app = NSApplication::sharedApplication(mtm);
    // 必须是一次鼠标左键的按下或拖动：拖动会话以它为起点，别的事件 AppKit 会抛异常。
    let event = app.currentEvent().filter(|e| {
        matches!(
            e.r#type(),
            NSEventType::LeftMouseDragged | NSEventType::LeftMouseDown
        )
    });
    let Some(event) = event else {
        log::warn!("drag_files 须在鼠标按下或拖动的事件里经 ctx.defer_blocking 调用");
        return DragEffect::None;
    };
    let Some(view) = event.window(mtm).and_then(|w| w.contentView()) else {
        return DragEffect::None;
    };

    let source = DragSource::new(mtm, allow_move, before_drop);
    let lazy = source.ivars().before_drop.borrow().is_some();
    let items = dragging_items(&source, &view, &event, paths, lazy);
    if items.is_empty() {
        return DragEffect::None;
    }
    let _session = view.beginDraggingSessionWithItems_event_source(
        &NSArray::from_retained_slice(&items),
        &event,
        ProtocolObject::from_ref(&*source),
    );
    // 新会话已占用拖动剪贴板，上一次的数据源到此才可以放手。
    LAST_SOURCE.with(|slot| *slot.borrow_mut() = Some(source.clone()));
    run_until_ended(&app, &source);
    // 拖动会话吞掉了那次 mouseUp，上层的按下态等不到抬起。对照 win32：`DoDragDrop`
    // 抢走鼠标捕获，窗口收到 `WM_CAPTURECHANGED`。
    super::window::notify_drag_out_ended(&view);

    if source.ivars().verdict.get() == Some(false) {
        return DragEffect::None;
    }
    effect_of(source.ivars().operation.get())
}

/// 每个文件一个拖动条目，预览图用系统文件图标，从指针处略微错开叠放。
fn dragging_items(
    source: &DragSource,
    view: &objc2_app_kit::NSView,
    event: &objc2_app_kit::NSEvent,
    paths: &[impl AsRef<Path>],
    lazy: bool,
) -> Vec<Retained<NSDraggingItem>> {
    let at = view.convertPoint_fromView(event.locationInWindow(), None);
    let workspace = NSWorkspace::sharedWorkspace();
    let file_url_type: &NSPasteboardType = unsafe { NSPasteboardTypeFileURL };
    let path_type = NSString::from_str(PATH_TYPE);
    let mut items = Vec::new();
    for p in paths {
        let Some(s) = p.as_ref().to_str() else {
            log::warn!("drag_files: 跳过非 UTF-8 路径 {}", p.as_ref().display());
            continue;
        };
        let ns_path = NSString::from_str(s);
        let Some(url) = NSURL::fileURLWithPath(&ns_path).absoluteString() else {
            continue;
        };
        let item = NSPasteboardItem::new();
        if lazy {
            item.setString_forType(&url, &path_type);
            item.setDataProvider_forTypes(
                ProtocolObject::from_ref(source),
                &NSArray::from_slice(&[file_url_type]),
            );
        } else {
            item.setString_forType(&url, file_url_type);
        }
        let dragging = NSDraggingItem::initWithPasteboardWriter(
            NSDraggingItem::alloc(),
            ProtocolObject::from_ref(&*item),
        );
        let offset = 4.0 * items.len().min(4) as f64;
        let frame = NSRect::new(
            NSPoint::new(at.x - 16.0 + offset, at.y - 16.0 + offset),
            NSSize::new(32.0, 32.0),
        );
        let icon = workspace.iconForFile(&ns_path);
        // SAFETY: contents 按文档可为 NSImage。
        unsafe { dragging.setDraggingFrame_contents(frame, Some(&icon)) };
        items.push(dragging);
    }
    items
}

/// 嵌套事件循环，直到拖动会话结束。
fn run_until_ended(app: &NSApplication, source: &DragSource) {
    let mut mouse_up_seen: Option<Instant> = None;
    while !source.ivars().ended.get() {
        // 带超时：会话结束回调不一定伴随一个事件，不能无限期地等下一个事件。
        let until = NSDate::dateWithTimeIntervalSinceNow(0.1);
        // SAFETY: 主线程上的标准事件泵调用。
        let ev = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&until),
                NSDefaultRunLoopMode,
                true,
            )
        };
        if let Some(ev) = ev {
            if ev.r#type() == NSEventType::LeftMouseUp && mouse_up_seen.is_none() {
                mouse_up_seen = Some(Instant::now());
            }
            app.sendEvent(&ev);
        }
        if mouse_up_seen.is_some_and(|t| t.elapsed() > SESSION_START_GRACE) {
            log::warn!("drag_files: 拖动会话没有开始（鼠标已松开），按落空处理");
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_maps_to_effect() {
        assert_eq!(effect_of(NSDragOperation::None), DragEffect::None);
        assert_eq!(effect_of(NSDragOperation::Copy), DragEffect::Copy);
        assert_eq!(effect_of(NSDragOperation::Generic), DragEffect::Copy);
        assert_eq!(effect_of(NSDragOperation::Link), DragEffect::Link);
        assert_eq!(effect_of(NSDragOperation::Move), DragEffect::Move);
        // 同时报多个位时，移动优先：调用方要据此刷新源目录。
        assert_eq!(
            effect_of(NSDragOperation::Move | NSDragOperation::Copy),
            DragEffect::Move
        );
    }

    #[test]
    fn move_only_when_allowed() {
        assert!(!allowed_operations(false).contains(NSDragOperation::Move));
        assert!(allowed_operations(true).contains(NSDragOperation::Move));
        assert!(allowed_operations(false).contains(NSDragOperation::Copy));
    }

    fn state(cb: Option<Box<dyn FnOnce() -> bool>>) -> DragState {
        DragState {
            allow_move: false,
            before_drop: RefCell::new(cb),
            verdict: Cell::new(None),
            ended: Cell::new(false),
            operation: Cell::new(NSDragOperation::None),
        }
    }

    /// 回调只跑一次，结果沿用到后续条目。
    #[test]
    fn before_drop_runs_once_and_verdict_sticks() {
        let calls = std::rc::Rc::new(Cell::new(0));
        let c = calls.clone();
        let s = state(Some(Box::new(move || {
            c.set(c.get() + 1);
            true
        })));
        assert!(s.files_ready());
        assert!(s.files_ready());
        assert_eq!(calls.get(), 1);
    }

    /// 会话落空结束后 AppKit 来解析剩余数据：不应生成文件。
    #[test]
    fn no_before_drop_after_failed_session() {
        let called = std::rc::Rc::new(Cell::new(false));
        let c = called.clone();
        let s = state(Some(Box::new(move || {
            c.set(true);
            true
        })));
        s.ended.set(true);
        assert!(!s.files_ready());
        assert!(!called.get());
    }

    /// 回调返回 false（生成失败 / 用户取消）：所有条目都不交出路径。
    #[test]
    fn rejected_before_drop_withholds_every_item() {
        let s = state(Some(Box::new(|| false)));
        assert!(!s.files_ready());
        assert!(!s.files_ready());
    }

    /// 回调里跑嵌套事件循环时接收方再来要：不能当成已就绪。
    #[test]
    fn reentrant_request_during_callback_is_not_ready() {
        let s = std::rc::Rc::new(state(None));
        let inner = std::rc::Rc::downgrade(&s);
        let seen = std::rc::Rc::new(Cell::new(None));
        let seen2 = seen.clone();
        *s.before_drop.borrow_mut() = Some(Box::new(move || {
            seen2.set(inner.upgrade().map(|st| st.files_ready()));
            true
        }));
        assert!(s.files_ready());
        assert_eq!(seen.get(), Some(false));
    }
}
