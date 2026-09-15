//! 分栏容器的分隔条 SplitHandle：拖动它改写 `Signal<f32>` 比例，两侧子节点经
//! [`Element::weight_when`](crate::ui::Element::weight_when) 读同一个信号，于是布局
//! 每帧跟着走——核心层不知道"分栏"这回事，它看到的只是两个权重会变的线性子节点。
//!
//! 比例存在信号里而不是控件内部，是为了让应用能持久化（文件管理器要记住上次的分栏
//! 位置）、也能从外部改（快捷键"均分"）。

use crate::core::{EventCtx, Widget};
use crate::event::{CursorShape, Event, MouseButton, PointerKind};
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::spec::Axis;
use crate::style::Style;
use crate::text::TextEngine;

/// 分栏参数。`Default` 即「分隔条 6px、比例钳在 0.1..=0.9」。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitOpts {
    /// 分隔条在主轴上的厚度（逻辑 px）。可命中区就是这么宽，太窄不好抓。
    pub thickness: i32,
    /// 比例下限：第一栏最少占多少。
    pub min: f32,
    /// 比例上限：第一栏最多占多少。
    pub max: f32,
}

impl Default for SplitOpts {
    fn default() -> Self {
        Self {
            thickness: 6,
            min: 0.1,
            max: 0.9,
        }
    }
}

/// 分隔条控件。挂在线性容器中间那个固定厚度的节点上。
pub struct SplitHandle {
    axis: Axis,
    ratio: Signal<f32>,
    opts: SplitOpts,
    dragging: bool,
    hover: bool,
}

impl SplitHandle {
    pub fn new(axis: Axis, ratio: Signal<f32>, opts: SplitOpts) -> Self {
        Self {
            axis,
            ratio,
            opts,
            dragging: false,
            hover: false,
        }
    }

    /// 由指针的绝对位置反算比例：第一栏尺寸 = (容器主轴 − 分隔条厚) × ratio，
    /// 分隔条中心应落在指针下，故先扣掉半个厚度。
    fn ratio_at(&self, parent: Rect, pos_main: i32) -> f32 {
        let (start, size) = match self.axis {
            Axis::Horizontal => (parent.x, parent.w),
            Axis::Vertical => (parent.y, parent.h),
        };
        let usable = (size - self.opts.thickness).max(1) as f32;
        let r = (pos_main - start - self.opts.thickness / 2) as f32 / usable;
        r.clamp(self.opts.min, self.opts.max)
    }
}

impl Widget for SplitHandle {
    fn measure(&self, avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        // 主轴固定厚度，交叉轴撑满——分隔线要贯穿整个容器。
        match self.axis {
            Axis::Horizontal => Size::new(self.opts.thickness, avail.h),
            Axis::Vertical => Size::new(avail.w, self.opts.thickness),
        }
    }

    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        _style: &Style,
    ) {
        let th = crate::theme::current();
        let active = enabled && (self.dragging || self.hover);
        let color = if active {
            th.split.handle_active(&th.palette)
        } else {
            th.split.handle(&th.palette)
        };
        // 静态画 1px 细线居中；激活加粗到 2px 并换强调色——可命中区仍是整条 `thickness`
        // 宽，但视觉上不把整条染成一块粗条：它与界面里其他分隔线同一量级才不突兀。
        let t = if active { 2 } else { 1 };
        let (x, y, w, h) = match self.axis {
            Axis::Horizontal => (bounds.x + (bounds.w - t) / 2, bounds.y, t, bounds.h),
            Axis::Vertical => (bounds.x, bounds.y + (bounds.h - t) / 2, bounds.w, t),
        };
        canvas.fill_rect(x as f32, y as f32, w as f32, h as f32, &Paint::fill(color));
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        let Event::Pointer(p) = ev else {
            return false;
        };
        match p.kind {
            PointerKind::Enter => {
                self.hover = true;
                ctx.mark_dirty();
                true
            }
            PointerKind::Leave => {
                self.hover = false;
                ctx.mark_dirty();
                true
            }
            PointerKind::Down if p.button == MouseButton::Left => {
                self.dragging = true;
                ctx.capture();
                ctx.mark_dirty();
                true
            }
            PointerKind::Move if self.dragging => {
                let me = ctx.id();
                let parent = {
                    let tree = ctx.tree_mut();
                    tree.get(me)
                        .and_then(|n| n.parent)
                        .map(|pid| tree.abs_bounds(pid))
                };
                if let Some(parent) = parent {
                    let pos_main = match self.axis {
                        Axis::Horizontal => p.pos.x,
                        Axis::Vertical => p.pos.y,
                    };
                    let r = self.ratio_at(parent, pos_main);
                    if (r - self.ratio.get()).abs() > f32::EPSILON {
                        self.ratio.set(r);
                        // 权重变了要重排；信号写入只保证重绘。
                        ctx.mark_layout_dirty();
                    }
                }
                true
            }
            PointerKind::Up if self.dragging => {
                self.dragging = false;
                ctx.release_capture();
                ctx.mark_dirty();
                true
            }
            _ => false,
        }
    }

    fn cursor(&self) -> CursorShape {
        match self.axis {
            Axis::Horizontal => CursorShape::SizeWE,
            Axis::Vertical => CursorShape::SizeNS,
        }
    }
}
