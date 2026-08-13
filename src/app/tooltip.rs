//! 悬停提示（tooltip）浮层：延时触发、点击抑制、越界翻转定位。
//!
//! 状态只有三样（指针位置、悬停起始时刻、是否被抑制），绘制时按当前悬停节点
//! 现取文案，故与控件树无耦合。

use crate::core::{NodeId, Tree};
use crate::geometry::{Point, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::text::TextStyle;
use crate::theme::Theme;

/// 悬停提示：触发延时（ms）、字号、内边距、相对指针的偏移。
/// 换行宽度上限由宿主经 `Theme.tooltip.max_width` 配置（见 [`crate::theme::TooltipTheme::max_width`]）。
const TOOLTIP_DELAY_MS: u64 = 500;
const TOOLTIP_FONT: f32 = 13.0;
const TOOLTIP_PAD_X: i32 = 8;
const TOOLTIP_PAD_Y: i32 = 4;
const TOOLTIP_CURSOR_DX: i32 = 12;
const TOOLTIP_CURSOR_DY: i32 = 20;

/// 宿主持有的悬停提示状态。
#[derive(Default)]
pub(super) struct TooltipState {
    /// 最近一次指针位置（逻辑坐标），用于悬停提示浮层定位。
    pub(super) pos: Point,
    /// 当前悬停起始时刻（ms，单调时钟）。悬停节点变化或点击时复位；
    /// 渲染据 `now - since_ms >= TOOLTIP_DELAY_MS` 决定是否弹出提示。
    pub(super) since_ms: u64,
    /// 点击后抑制提示，直到指针再次移动（避免点完控件原地又弹出盖住它）。
    pub(super) suppressed: bool,
}

impl TooltipState {
    /// 当前悬停节点是否会弹出提示（决定本帧算不算"有浮层"，进而能否局部重绘）。
    pub(super) fn will_show(&self, tree: &Tree, hover: Option<NodeId>) -> bool {
        !self.suppressed && hover.and_then(|h| tree.node_tooltip(h)).is_some()
    }

    /// 悬停提示浮层绘制（菜单激活时不显示）：悬停节点带 tooltip 且停留超过延时则弹出；
    /// 未到延时则请求下一帧——鼠标静止后无事件，需靠 anim 续帧推进计时
    /// （与不确定进度条同源）。
    pub(super) fn paint(
        &self,
        canvas: &mut dyn Canvas,
        tree: &Tree,
        hover: Option<NodeId>,
        menu_open: bool,
        theme: &Theme,
        ws: Size,
        now_ms: u64,
    ) {
        if menu_open || self.suppressed {
            return;
        }
        let Some(text) = hover.and_then(|h| tree.node_tooltip(h)) else {
            return;
        };
        if now_ms.saturating_sub(self.since_ms) < TOOLTIP_DELAY_MS {
            crate::anim::request_repaint();
            return;
        }
        let (pal, tt) = (&theme.palette, &theme.tooltip);
        let ts = canvas.measure_text_wrapped(&text, &TextStyle::new(TOOLTIP_FONT), tt.max_width());
        let (w, h) = (ts.w + 2 * TOOLTIP_PAD_X, ts.h + 2 * TOOLTIP_PAD_Y);
        let mut x = self.pos.x + TOOLTIP_CURSOR_DX;
        let mut y = self.pos.y + TOOLTIP_CURSOR_DY;
        if ws.w > 0 && x + w > ws.w {
            x = (ws.w - w).max(0);
        }
        if ws.h > 0 && y + h > ws.h {
            y = (self.pos.y - h - 4).max(0); // 下方放不下则翻到指针上方
        }
        let corner = tt.corner(&theme.metrics);
        canvas.fill_round_rect(
            x as f32,
            y as f32,
            w as f32,
            h as f32,
            corner,
            &Paint::fill(tt.bg(pal)),
        );
        let tr = Rect::new(x + TOOLTIP_PAD_X, y, w - 2 * TOOLTIP_PAD_X, h);
        canvas.draw_text(
            &text,
            tr,
            tt.text(pal),
            crate::spec::Align::Start,
            &TextStyle::new(TOOLTIP_FONT),
        );
    }
}
