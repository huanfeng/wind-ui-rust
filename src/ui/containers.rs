//! Phase 5 容器/导航控件的内部 widget：滚动滚轮处理、模态遮罩。

use crate::core::{EventCtx, Widget};
use crate::event::{Event, PointerKind};

/// 滚动容器内部 widget：处理滚轮，调整节点滚动偏移。
pub struct ScrollWidget;

impl Widget for ScrollWidget {
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        if let Event::Pointer(p) = ev {
            if let PointerKind::Wheel(delta) = p.kind {
                // Windows 一刻度为 ±120；每刻度滚动 48px（delta>0 向上）。
                ctx.scroll_by(-delta * 48 / 120);
                return true;
            }
        }
        false
    }
}

/// 模态遮罩 widget：吞掉所有指针事件，阻止穿透到下层（命中链先于其下内容）。
pub struct ModalScrim;

impl Widget for ModalScrim {
    fn on_event(&mut self, _ctx: &mut EventCtx, ev: &Event) -> bool {
        // 仅吞指针事件；键盘仍可冒泡（如 Escape 关闭由宿主处理）。
        matches!(ev, Event::Pointer(_))
    }
}
