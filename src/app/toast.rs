//! 轻提示（toast）浮层：顶部居中堆叠、淡入淡出、定时消失。
//!
//! 与菜单一样是完全自洽的一块：自己的堆叠状态、自己的命中矩形（每帧渲染时重算）、
//! 自己的绘制；与控件树的往来只有"控件请求弹一条"这一个方向。

use crate::event::ToastRequest;
use crate::geometry::{Rect, Size};
use crate::render::{Canvas, Paint};
use crate::text::TextStyle;
use crate::theme::Theme;

use super::UiHost;

/// 轻提示浮层：字号、图标字号、内边距、图标与文字间距、淡入/淡出时长（ms）。
const TOAST_FONT: f32 = 14.0;
const TOAST_ICON_FONT: f32 = 18.0;
const TOAST_PAD_X: i32 = 16;
const TOAST_PAD_Y: i32 = 11;
const TOAST_ICON_GAP: i32 = 12;
const TOAST_MIN_W: i32 = 132;
const TOAST_FADE_IN_MS: u64 = 140;
const TOAST_FADE_OUT_MS: u64 = 280;
/// 同屏最多堆叠的轻提示条数：超过丢最旧。
const TOAST_MAX: usize = 4;
/// 顶部居中堆叠布局：距窗口顶边距、条间距、✕ 关闭命中宽、左强调色条宽。
const TOAST_TOP_MARGIN: i32 = 16;
const TOAST_GAP: i32 = 10;
const TOAST_CLOSE_W: i32 = 22;
/// 文字换行区最小宽度：即便窗口极窄，也保留基本可读宽度（宁可面板贴边也不塌缩为 0）。
const TOAST_TEXT_MIN_W: i32 = 60;

/// 活动轻提示：内容 + 起始时刻 + 悬停暂停累计。淡入淡出/过期均按「有效流逝」推算。
pub(super) struct ToastState {
    pub(super) req: ToastRequest,
    shown_at_ms: u64,
    /// 若正被悬停：记录进入悬停时刻（冻结倒计时）；否则 None。
    paused_at_ms: Option<u64>,
    /// 历史累计暂停总时长（ms）。
    paused_total_ms: u64,
    /// 上一帧算出的命中矩形 `(面板, ✕ 按钮)`（逻辑坐标）；未绘制过则 `None`。
    ///
    /// 跟着自己这一条走，而不是放在与 `items` 平行的另一个 `Vec` 里——平行数组
    /// 要求每一处改动 `items` 的地方都记得同步，而改动点有三个（堆叠上限淘汰、
    /// 过期清理、✕ 关闭），漏一个就会用陈旧下标索引到别条上，最坏是越界 panic。
    hit: Option<(Rect, Rect)>,
}

impl ToastState {
    /// 扣除暂停后的有效流逝（ms）。
    fn active_elapsed(&self, now_ms: u64) -> u64 {
        let raw = now_ms.saturating_sub(self.shown_at_ms);
        let cur_pause = self
            .paused_at_ms
            .map(|p| now_ms.saturating_sub(p))
            .unwrap_or(0);
        raw.saturating_sub(self.paused_total_ms)
            .saturating_sub(cur_pause)
    }
    /// 切换悬停：进入则起暂停，离开则把本段并入累计。
    fn set_hover(&mut self, now_ms: u64, hovered: bool) {
        match (hovered, self.paused_at_ms) {
            (true, None) => self.paused_at_ms = Some(now_ms),
            (false, Some(p)) => {
                self.paused_total_ms += now_ms.saturating_sub(p);
                self.paused_at_ms = None;
            }
            _ => {}
        }
    }
    /// 是否已过期（应清除）。
    fn expired(&self, now_ms: u64) -> bool {
        self.active_elapsed(now_ms) >= self.req.duration_ms
    }
    /// 当前不透明度系数 [0,1]：前段淡入、末段淡出、中间恒 1。
    fn alpha(&self, now_ms: u64) -> f32 {
        let e = self.active_elapsed(now_ms);
        let d = self.req.duration_ms;
        if e < TOAST_FADE_IN_MS {
            return e as f32 / TOAST_FADE_IN_MS as f32;
        }
        let fade_out_start = d.saturating_sub(TOAST_FADE_OUT_MS);
        if e >= fade_out_start && d > fade_out_start {
            return ((d - e) as f32 / TOAST_FADE_OUT_MS as f32).clamp(0.0, 1.0);
        }
        1.0
    }
}

/// 宿主持有的轻提示浮层状态：活动条堆栈 + 每帧重算的命中矩形。
#[derive(Default)]
pub(super) struct ToastHost {
    /// 活动的轻提示浮层堆栈（先进先出，超过 `TOAST_MAX` 丢最旧）：居中显示、淡入淡出、定时消失。
    /// 命中矩形跟在每条自己的 `hit` 字段里（见 [`ToastState::hit`］），故删条即删矩形。
    pub(super) items: Vec<ToastState>,
}

impl ToastHost {
    /// 当前是否有轻提示在屏（决定本帧能否走局部重绘）。
    pub(super) fn is_active(&self) -> bool {
        !self.items.is_empty()
    }

    /// 逻辑坐标是否落在任一条面板内（平台层用于把浮层区域判为客户区）。
    pub(super) fn hit_any_panel(&self, p: crate::geometry::Point) -> bool {
        self.items
            .iter()
            .any(|t| t.hit.is_some_and(|(panel, _)| panel.contains(p)))
    }
}

impl UiHost {
    /// 弹出/替换轻提示：以当前单调时钟为起点，强制整窗重绘叠加浮层。
    /// 后续帧会持续推进淡入淡出并在过期后自动清除（见 render 中的浮层段）。
    pub(super) fn show_toast(&mut self, req: ToastRequest) {
        self.push_toast(req);
        self.damage.needs_full = true;
    }
    /// 上屏 on_update（响应式相位）里累积的 toast——该相位不经 DispatchResult，
    /// 由 `Tree` 暂存、宿主在每次 layout 后取走（否则 toast_sink 等发的提示被吞）。
    pub(super) fn flush_pending_toasts(&mut self) {
        for req in self.tree.take_pending_toasts() {
            self.show_toast(req);
        }
    }
    /// 压入一条 toast；超过上限丢最旧。
    fn push_toast(&mut self, req: ToastRequest) {
        let now_ms = self.start.elapsed().as_millis() as u64;
        if self.toast.items.len() >= TOAST_MAX {
            self.toast.items.remove(0);
        }
        self.toast.items.push(ToastState {
            req,
            shown_at_ms: now_ms,
            paused_at_ms: None,
            paused_total_ms: 0,
            hit: None, // 本帧尚未绘制，绘制后才可命中
        });
    }
    /// 移除已过期（Task 3 先提供，供 render 调用）。
    pub(super) fn retain_live_toasts(&mut self, now_ms: u64) {
        self.toast.items.retain(|t| !t.expired(now_ms));
    }

    /// toast 面板命中测试（逻辑坐标）→ 命中条的下标。
    ///
    /// 下标直接来自 `items`，故拿它索引 `items` 恒安全。
    fn toast_hit(&self, p: crate::geometry::Point) -> Option<usize> {
        self.toast
            .items
            .iter()
            .position(|t| t.hit.is_some_and(|(panel, _)| panel.contains(p)))
    }
    /// toast ✕ 关闭按钮命中测试（逻辑坐标）→ 命中条的下标。
    fn toast_close_hit(&self, p: crate::geometry::Point) -> Option<usize> {
        self.toast
            .items
            .iter()
            .position(|t| t.hit.is_some_and(|(_, close)| close.contains(p)))
    }

    /// toast 浮层指针交互：命中则消费（悬停暂停 / ✕关闭 / 右键复制）。
    pub(super) fn handle_toast_pointer(&mut self, ev: crate::event::PointerEvent) -> bool {
        use crate::event::{MenuItem, MouseButton, PointerKind};
        let now_ms = self.start.elapsed().as_millis() as u64;
        // 悬停暂停：逐条按是否命中切换（未命中该条则恢复计时）。
        let hit = self.toast_hit(ev.pos);
        for (i, t) in self.toast.items.iter_mut().enumerate() {
            t.set_hover(now_ms, Some(i) == hit);
        }
        if hit.is_some() {
            self.damage.needs_full = true; // 冻结/恢复需重绘
        }
        // 主键按下命中 ✕：移除该条。
        if ev.kind == PointerKind::Down && ev.button == MouseButton::Left {
            if let Some(i) = self.toast_close_hit(ev.pos) {
                self.toast.items.remove(i);
                self.damage.needs_full = true;
                self.swallow_up = true; // 吞掉配对 Up
                return true;
            }
        }
        // 右键命中面板：弹「复制内容」菜单。
        if ev.kind == PointerKind::Down && ev.button == MouseButton::Right {
            if let Some(i) = hit {
                let text = self.toast.items[i].req.text.clone();
                let item = MenuItem::run(
                    "复制内容",
                    move |_ctx| {
                        use crate::core::ClipboardProvider;
                        crate::platform::Clipboard.set_text(&text);
                    },
                    false,
                );
                if let Some(target) = self.focus.current.or(self.tree.root) {
                    self.open_menu(
                        crate::event::MenuRequest {
                            pos: ev.pos,
                            items: vec![item],
                            min_width: 0,
                            anchor_top: None,
                            rebuild: None,
                        },
                        target,
                    );
                }
                return true;
            }
        }
        // 命中面板（非✕、非右键）：吞掉，避免点穿到下方控件。
        hit.is_some()
    }
}

impl ToastHost {
    /// 轻提示浮层绘制：顶部居中堆叠，单条横向 [图标][文字][✕关闭]，淡入淡出
    /// （过期条已由 `retain_live_toasts` 在建 canvas 前清除）。命中矩形逐帧重算，
    /// 写回每条自己的 `hit` 字段供点击测试使用。
    pub(super) fn paint(&mut self, canvas: &mut dyn Canvas, theme: &Theme, ws: Size, now_ms: u64) {
        let mut y = TOAST_TOP_MARGIN;
        for toast in &mut self.items {
            let alpha = toast.alpha(now_ms);
            let pal = &theme.palette;
            let tt = &theme.toast;
            let glyph = toast.req.kind.glyph();
            let icon_color = match toast.req.kind {
                crate::event::ToastKind::Info => tt.info(pal),
                crate::event::ToastKind::Success => tt.success(pal),
                crate::event::ToastKind::Error => tt.error(pal),
            };
            let icon_sz = canvas.measure_text(glyph, &TextStyle::new(TOAST_ICON_FONT));
            // 面板宽度上限：两侧各留 TOAST_TOP_MARGIN，保证不越窗口边界。
            let panel_max_w = (ws.w - 2 * TOAST_TOP_MARGIN).max(TOAST_MIN_W);
            // 文字最大宽度＝面板上限减去强调条/内边距/图标/图标间距/✕区/右内边距。
            let text_max_w = (panel_max_w
                - TOAST_PAD_X
                - icon_sz.w
                - TOAST_ICON_GAP
                - TOAST_ICON_GAP
                - TOAST_CLOSE_W
                - TOAST_PAD_X)
                .max(TOAST_TEXT_MIN_W);
            // 按 text_max_w 换行测量：短文本一行内即可测完，长文本自动折成多行。
            let ts = canvas.measure_text_wrapped(
                &toast.req.text,
                &TextStyle::new(TOAST_FONT),
                text_max_w as f32,
            );
            let panel_w = (TOAST_PAD_X
                + icon_sz.w
                + TOAST_ICON_GAP
                + ts.w
                + TOAST_ICON_GAP
                + TOAST_CLOSE_W
                + TOAST_PAD_X)
                .max(TOAST_MIN_W)
                .min(panel_max_w);
            let panel_h = TOAST_PAD_Y + ts.h.max(icon_sz.h) + TOAST_PAD_Y;
            let x = ((ws.w - panel_w) / 2).max(0);
            let corner = tt.corner(&theme.metrics);
            // 柔和投影（透明度跟随淡入淡出）。参数与菜单浮层同源，见 ToastTheme::shadow。
            let sh = tt.shadow();
            canvas.draw_shadow(
                x as f32 + sh.dx,
                y as f32 + sh.dy,
                panel_w as f32,
                panel_h as f32,
                corner,
                sh.blur,
                sh.color.scale_alpha(alpha),
            );
            canvas.fill_round_rect(
                x as f32,
                y as f32,
                panel_w as f32,
                panel_h as f32,
                corner,
                &Paint::fill(tt.bg(pal).scale_alpha(alpha)),
            );
            // 图标：面板左侧，垂直居中。
            let icon_x = x + TOAST_PAD_X;
            let icon_rect = Rect::new(icon_x, y, icon_sz.w, panel_h);
            canvas.draw_text(
                glyph,
                icon_rect,
                icon_color.scale_alpha(alpha),
                crate::spec::Align::Center,
                &TextStyle::new(TOAST_ICON_FONT),
            );
            // 文字：图标右侧，垂直居中、左对齐；rect 宽用 text_max_w（而非 ts.w）
            // 以保证绘制时的换行宽度与测量时一致（长文本才需要换行，短文本本就不超）。
            let text_x = icon_x + icon_sz.w + TOAST_ICON_GAP;
            let text_rect = Rect::new(text_x, y, text_max_w, panel_h);
            canvas.draw_text(
                &toast.req.text,
                text_rect,
                tt.text(pal).scale_alpha(alpha),
                crate::spec::Align::Start,
                &TextStyle::new(TOAST_FONT),
            );
            // ✕ 关闭：面板右侧固定宽区域。
            let close = Rect::new(
                x + panel_w - TOAST_CLOSE_W - TOAST_PAD_X / 2,
                y,
                TOAST_CLOSE_W,
                panel_h,
            );
            canvas.draw_text(
                "\u{2715}",
                close,
                pal.text_muted.scale_alpha(alpha),
                crate::spec::Align::Center,
                &TextStyle::new(TOAST_FONT),
            );
            let panel = Rect::new(x, y, panel_w, panel_h);
            toast.hit = Some((panel, close));
            y += panel_h + TOAST_GAP;
            // 持续推进淡入淡出与过期：请求下一帧。
            crate::anim::request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::ui::Element;

    #[test]
    fn toast_stack_caps_at_max_and_drops_oldest() {
        let app = App::new("t", 100, 100).content(Element::col());
        let mut app = app.into_handler_for_test();
        for i in 0..(TOAST_MAX + 2) {
            app.push_toast(ToastRequest {
                text: format!("t{i}"),
                kind: crate::event::ToastKind::Info,
                duration_ms: 3000,
            });
        }
        assert_eq!(app.toast.items.len(), TOAST_MAX, "不超过上限");
        assert_eq!(
            app.toast.items.first().unwrap().req.text,
            "t2",
            "最旧两条被丢弃"
        );
        assert_eq!(
            app.toast.items.last().unwrap().req.text,
            format!("t{}", TOAST_MAX + 1)
        );
    }

    #[test]
    fn toast_fade_curve_and_expiry() {
        let t = ToastState {
            req: ToastRequest {
                text: "hi".into(),
                kind: crate::event::ToastKind::Success,
                duration_ms: 1000,
            },
            shown_at_ms: 100,
            paused_at_ms: None,
            paused_total_ms: 0,
            hit: None,
        };
        assert_eq!(t.alpha(100), 0.0, "起点不可见");
        let mid_in = t.alpha(100 + TOAST_FADE_IN_MS / 2);
        assert!((0.4..=0.6).contains(&mid_in));
        assert_eq!(t.alpha(100 + 500), 1.0);
        assert!(!t.expired(100 + 999));
        assert!(t.expired(100 + 1000));
    }

    #[test]
    fn toast_hover_freezes_countdown() {
        let mut t = ToastState {
            req: ToastRequest {
                text: "hi".into(),
                kind: crate::event::ToastKind::Info,
                duration_ms: 1000,
            },
            shown_at_ms: 0,
            paused_at_ms: None,
            paused_total_ms: 0,
            hit: None,
        };
        // 200ms 时悬停，冻结；在 5000ms（远超 1000）仍不过期。
        t.set_hover(200, true);
        assert!(!t.expired(5000), "悬停期间不过期");
        assert_eq!(t.active_elapsed(5000), 200, "有效流逝冻结在 200");
        // 5000ms 移开，恢复计时；再过 800ms（累计有效 1000）到时过期。
        t.set_hover(5000, false);
        assert!(!t.expired(5000 + 799));
        assert!(t.expired(5000 + 800));
    }

    /// 造一条已绘制过（带命中矩形）的 toast。
    fn placed(text: &str, panel: Rect, close: Rect) -> ToastState {
        ToastState {
            req: ToastRequest {
                text: text.into(),
                kind: crate::event::ToastKind::Info,
                duration_ms: 1000,
            },
            shown_at_ms: 0,
            paused_at_ms: None,
            paused_total_ms: 0,
            hit: Some((panel, close)),
        }
    }

    #[test]
    fn toast_hit_and_close_hit() {
        use crate::geometry::Point;
        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        app.toast.items = vec![
            placed("a", Rect::new(100, 16, 200, 44), Rect::new(280, 16, 22, 44)),
            placed("b", Rect::new(100, 70, 200, 44), Rect::new(280, 70, 22, 44)),
        ];
        assert_eq!(app.toast_hit(Point::new(150, 30)), Some(0));
        assert_eq!(app.toast_hit(Point::new(150, 84)), Some(1));
        assert_eq!(app.toast_hit(Point::new(10, 10)), None);
        assert_eq!(app.toast_close_hit(Point::new(285, 30)), Some(0));
        assert_eq!(
            app.toast_close_hit(Point::new(150, 30)),
            None,
            "面板内非✕区不算关闭"
        );
    }

    /// 关掉一条之后，命中下标不会再指向别条——命中矩形跟着条走，删条即删矩形。
    ///
    /// 回归：矩形原先存在与 `items` 平行的 `rects` 里、只在 paint 时重建，而 ✕ 关闭
    /// 只 `items.remove(i)`。屏上两条时关掉第二条，在下一帧渲染前再点原位置，
    /// 命中仍返回 `Some(1)`，拿它索引只剩一条的 `items` 就是越界 panic。
    #[test]
    fn closing_a_toast_invalidates_its_hit_rect_immediately() {
        use crate::geometry::Point;
        let app = App::new("t", 400, 300).content(Element::col());
        let mut app = app.into_handler_for_test();
        app.toast.items = vec![
            placed("a", Rect::new(100, 16, 200, 44), Rect::new(280, 16, 22, 44)),
            placed("b", Rect::new(100, 70, 200, 44), Rect::new(280, 70, 22, 44)),
        ];
        let second = Point::new(150, 84);
        assert_eq!(app.toast_hit(second), Some(1), "关闭前命中第二条");

        app.toast.items.remove(1); // ✕ 关闭走的就是这一步

        assert_eq!(
            app.toast_hit(second),
            None,
            "第二条已移除，其命中矩形必须随之消失，不能再返回下标"
        );
        assert!(
            app.toast_hit(second)
                .is_none_or(|i| i < app.toast.items.len()),
            "任何命中下标都必须对 items 有效"
        );
    }
}
