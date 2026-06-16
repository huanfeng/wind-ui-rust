//! 进度条 ProgressBar：确定（0~1 绑定值）与不确定（忙碌动画）两种。
//!
//! 不确定模式经 [`crate::anim::request_repaint`] 请求持续帧，宿主按帧驱动重绘；
//! 动画相位由 [`crate::anim::clock_ms`] 的单调时钟计算，与挂钟无关。

use std::cell::Cell;
use std::rc::Rc;

use crate::core::{EventCtx, Widget};
use crate::event::Event;
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::style::Style;
use crate::text::TextEngine;

/// 条高（逻辑 px）。
const BAR_H: i32 = 6;
/// 不确定模式动画周期（ms）与滑块占比。
const PERIOD_MS: u64 = 1100;
const SEG_FRAC: f32 = 0.35;

pub struct ProgressBar {
    /// Some=确定(0..=1)；None=不确定（忙碌动画）。
    value: Option<Rc<Cell<f32>>>,
}

impl ProgressBar {
    pub fn determinate(value: Rc<Cell<f32>>) -> Self {
        Self { value: Some(value) }
    }
    pub fn indeterminate() -> Self {
        Self { value: None }
    }
}

impl Widget for ProgressBar {
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(160, BAR_H + 4)
    }

    fn paint(&self, bounds: Rect, _content: Rect, _focused: bool, canvas: &mut dyn Canvas, _style: &Style) {
        let th = crate::theme::current();
        let (pal, pr) = (&th.palette, &th.progress);
        let h = BAR_H.min(bounds.h) as f32;
        let x = bounds.x as f32;
        let y = bounds.y as f32 + (bounds.h as f32 - h) / 2.0;
        let w = bounds.w as f32;
        let r = h / 2.0;
        // 轨道（pill）。
        canvas.fill_round_rect(x, y, w, h, r, &Paint::fill(pr.track(pal)));

        let fill = Paint::fill(pr.fill(pal));
        match &self.value {
            Some(v) => {
                // 确定：按值填充左侧。
                let frac = v.get().clamp(0.0, 1.0);
                let fw = w * frac;
                if fw > 0.0 {
                    canvas.fill_round_rect(x, y, fw, h, r, &fill);
                }
            }
            None => {
                // 不确定：滑块在轨道内平滑往复（三角波），始终落在轨道内、无需裁剪。
                crate::anim::request_repaint();
                let phase = (crate::anim::clock_ms() % PERIOD_MS) as f32 / PERIOD_MS as f32;
                let seg = w * SEG_FRAC;
                let travel = (w - seg).max(0.0);
                let t = phase * 2.0;
                let tri = if t < 1.0 { t } else { 2.0 - t }; // 0→1→0
                let sx = tri * travel;
                // 滑块高度等于轨道、往复范围 [0, w-seg]，端点恰好与轨道圆角对齐，故无需裁剪。
                canvas.fill_round_rect(x + sx, y, seg, h, r, &fill);
            }
        }
    }

    fn on_event(&mut self, _ctx: &mut EventCtx, _ev: &Event) -> bool {
        false
    }
}
