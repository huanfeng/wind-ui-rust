//! 自定义标题栏窗口按钮：最小化 / 最大化-还原 / 关闭。
//!
//! 自绘标准图标（最小化=横线、最大化=方框、关闭=叉），hover/press 三态
//! （关闭键 hover 转 Windows 红、图标转白）。点击调对应窗口操作：
//! `EventCtx::minimize()` / `toggle_maximize()` / `request_close()`。
//! 仅在 `App::frameless()` 自定义标题栏中有意义。

use std::cell::Cell;

use crate::anim::{Easing, Transition};
use crate::core::{EventCtx, Widget};
use crate::event::{Event, PointerKind};
use crate::geometry::{Color, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::style::Style;
use crate::text::TextEngine;

/// 窗口按钮类型。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WindowButtonKind {
    Minimize,
    Maximize,
    Close,
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum BtnState {
    Normal,
    Hover,
    Press,
}

/// 标准标题栏按钮尺寸（逻辑 px，近 Windows 默认）。
const BTN_W: i32 = 46;
const BTN_H: i32 = 32;
/// 图标线宽与边长。
const GLYPH: i32 = 10;

pub struct WindowButton {
    kind: WindowButtonKind,
    state: BtnState,
    /// 底色补间（透明↔hover/press 底色淡入）。retarget-in-paint。
    bg_anim: Cell<Transition<Color>>,
    primed: Cell<bool>,
}

impl WindowButton {
    pub fn new(kind: WindowButtonKind) -> Self {
        Self {
            kind,
            state: BtnState::Normal,
            bg_anim: Cell::new(Transition::new(Color::rgba(0, 0, 0, 0))),
            primed: Cell::new(false),
        }
    }

    fn activate(&self, ctx: &mut EventCtx) {
        match self.kind {
            WindowButtonKind::Minimize => ctx.minimize(),
            WindowButtonKind::Maximize => ctx.toggle_maximize(),
            WindowButtonKind::Close => ctx.request_close(),
        }
    }
}

impl Widget for WindowButton {
    fn measure(&self, _avail: Size, _style: &Style, _text: &mut dyn TextEngine) -> Size {
        Size::new(BTN_W, BTN_H)
    }

    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        _enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let is_close = self.kind == WindowButtonKind::Close;
        // 悬停叠层随主题：暗色标题栏用浅色叠层（否则黑叠层在暗底上几乎不可见），
        // 亮色用深色叠层。据当前主题 surface（标题栏底）亮度判定明暗。
        let surf = crate::theme::current().palette.surface;
        let dark_titlebar = (surf.r as u32 + surf.g as u32 + surf.b as u32) < 384;
        let (hover_ov, press_ov) = if dark_titlebar {
            (
                Color::rgba(255, 255, 255, 0x20),
                Color::rgba(255, 255, 255, 0x33),
            )
        } else {
            (Color::rgba(0, 0, 0, 0x14), Color::rgba(0, 0, 0, 0x22))
        };
        // 悬停/按下底色：关闭键红，其余随主题淡叠层；Normal 全透明（淡入淡出的起止）。
        let target_bg = match self.state {
            BtnState::Normal => Color::rgba(0, 0, 0, 0),
            BtnState::Hover if is_close => Color::hex(0xE81123),
            BtnState::Press if is_close => Color::hex(0xC50F1F),
            BtnState::Hover => hover_ov,
            BtnState::Press => press_ov,
        };
        // 底色补间：首帧落定，其后淡入淡出。
        let mut anim = self.bg_anim.get();
        if !self.primed.get() {
            anim = Transition::new(target_bg);
            self.primed.set(true);
        } else if anim.target() != target_bg {
            anim.retarget(
                target_bg,
                crate::theme::current().anim.fast(),
                Easing::EaseOut,
            );
        }
        let bg = anim.animate();
        self.bg_anim.set(anim);
        if bg.a > 0 {
            canvas.fill_rect(
                bounds.x as f32,
                bounds.y as f32,
                bounds.w as f32,
                bounds.h as f32,
                &Paint::fill(bg),
            );
        }
        // 图标色：关闭键 hover/press 时白；否则取元素前景——
        // 设了 fg_role 时按当前主题解析（运行期换肤跟随，暗色标题栏自动转浅），
        // 与 label 等控件一致；未设角色则用显式 style.fg。
        let glyph = if is_close && self.state != BtnState::Normal {
            Color::WHITE
        } else if style.fg_role.is_some() {
            style.resolved_fg(&crate::theme::current())
        } else {
            style.fg
        };
        let paint = Paint::fill(glyph);
        // 居中的 GLYPH×GLYPH 区域。
        let cx = bounds.x + bounds.w / 2;
        let cy = bounds.y + bounds.h / 2;
        let (l, t) = ((cx - GLYPH / 2) as f32, (cy - GLYPH / 2) as f32);
        let (r, b) = ((cx + GLYPH / 2) as f32, (cy + GLYPH / 2) as f32);
        match self.kind {
            WindowButtonKind::Minimize => {
                canvas.draw_line(l, cy as f32, r, cy as f32, 1.0, &paint);
            }
            WindowButtonKind::Maximize => {
                canvas.stroke_round_rect(l, t, GLYPH as f32, GLYPH as f32, 0.0, 1.0, &paint);
            }
            WindowButtonKind::Close => {
                canvas.draw_line(l, t, r, b, 1.0, &paint);
                canvas.draw_line(l, b, r, t, 1.0, &paint);
            }
        }
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    if self.state == BtnState::Normal {
                        self.state = BtnState::Hover;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.state != BtnState::Press {
                        self.state = BtnState::Normal;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Down => {
                    self.state = BtnState::Press;
                    ctx.capture();
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Up => {
                    let was_press = self.state == BtnState::Press;
                    let inside = ctx.bounds().contains(p.pos);
                    self.state = if inside {
                        BtnState::Hover
                    } else {
                        BtnState::Normal
                    };
                    ctx.release_capture();
                    ctx.mark_dirty();
                    if was_press && inside {
                        self.activate(ctx);
                    }
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn focusable(&self) -> bool {
        // 可聚焦：使 drag_hit_at 视其为交互控件（标题栏拖动不吞掉按钮点击）。
        true
    }
}
