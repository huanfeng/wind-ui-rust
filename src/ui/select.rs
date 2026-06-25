//! 下拉选择控件 Dropdown：显示当前选项 + 点击弹出浮层列表选择。
//!
//! 复用宿主层浮层机制（与右键菜单同源）：点击经 `EventCtx::show_menu` 请求弹出，
//! 每个选项的动作是设置绑定的 `Rc<Cell<usize>>` 选中索引（`MenuAction::Run` 闭包）。

use std::cell::Cell;

use crate::anim::{Easing, Transition};
use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, MenuItem, PointerKind};
use crate::geometry::{Color, Point, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;

const PAD_X: i32 = 12;
const CHEVRON_W: i32 = 18;

pub struct Dropdown {
    options: Vec<String>,
    selected: Signal<usize>,
    hover: bool,
    /// 边框色补间（hover/focus 高亮淡变）；首帧靠 `primed` 落定。
    border_anim: Cell<Transition<Color>>,
    primed: Cell<bool>,
}

impl Dropdown {
    pub fn new(options: Vec<String>, selected: Signal<usize>) -> Self {
        Self {
            options,
            selected,
            hover: false,
            border_anim: Cell::new(Transition::new(Color::rgba(0, 0, 0, 0))),
            primed: Cell::new(false),
        }
    }

    fn current(&self) -> &str {
        let i = self
            .selected
            .get()
            .min(self.options.len().saturating_sub(1));
        self.options.get(i).map(|s| s.as_str()).unwrap_or("")
    }

    /// 弹出浮层列表：宽度对齐控件，每项点击设置选中索引。
    fn open(&self, ctx: &mut EventCtx) {
        if self.options.is_empty() {
            return;
        }
        let b = ctx.bounds();
        let cur = self.selected.get();
        let items: Vec<MenuItem> = self
            .options
            .iter()
            .enumerate()
            .map(|(i, o)| {
                let sel = self.selected;
                MenuItem::run(o.clone(), move || sel.set(i), i == cur)
            })
            .collect();
        ctx.show_menu(Point::new(b.x, b.y + b.h), items, b.w);
    }
}

impl Widget for Dropdown {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let mut w = 0;
        for o in &self.options {
            w = w.max(
                text.measure(o, style.font_family.as_deref(), style.font_size, None)
                    .w,
            );
        }
        Size::new(w + 2 * PAD_X + CHEVRON_W, (style.font_size as i32) + 16)
    }

    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        let (pal, dd) = (&th.palette, &th.dropdown);
        let (x, y, w, h) = (
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
        );
        let corner = dd.corner(&th.metrics);
        // 禁用：背景弱化、文字与箭头用 text_disabled。
        let bg = if enabled { dd.bg(pal) } else { pal.surface_alt };
        let text_color = if enabled {
            dd.text(pal)
        } else {
            pal.text_disabled
        };
        let chevron = if enabled {
            dd.chevron(pal)
        } else {
            pal.text_disabled
        };
        canvas.fill_round_rect(x, y, w, h, corner, &Paint::fill(bg));
        // 边框色补间：hover/focus 高亮淡变；首帧落定。
        let target_border = if focused || self.hover {
            dd.border_focus(pal)
        } else {
            dd.border(pal)
        };
        let mut ba = self.border_anim.get();
        if !self.primed.get() {
            ba = Transition::new(target_border);
            self.primed.set(true);
        } else if ba.target() != target_border {
            ba.retarget(target_border, th.anim.fast(), Easing::EaseOut);
        }
        let border = ba.animate();
        self.border_anim.set(ba);
        let bw = if focused { 1.8 } else { 1.5 };
        canvas.stroke_round_rect(x, y, w, h, corner, bw, &Paint::fill(border));

        // 当前选项文本（左侧，留出右侧 chevron）。
        let tr = Rect::new(
            bounds.x + PAD_X,
            bounds.y,
            bounds.w - 2 * PAD_X - CHEVRON_W,
            bounds.h,
        );
        canvas.draw_text(
            self.current(),
            tr,
            text_color,
            Align::Start,
            style.font_family.as_deref(),
            style.font_size,
        );

        // 右侧下拉箭头 ▼（两段线）。
        let cx = bounds.x as f32 + bounds.w as f32 - PAD_X as f32 - CHEVRON_W as f32 / 2.0;
        let cy = bounds.y as f32 + bounds.h as f32 / 2.0;
        let p = Paint::fill(chevron);
        canvas.draw_line(cx - 4.0, cy - 2.0, cx, cy + 3.0, 1.6, &p);
        canvas.draw_line(cx, cy + 3.0, cx + 4.0, cy - 2.0, 1.6, &p);
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
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
                PointerKind::Down => {
                    ctx.request_focus();
                    true
                }
                PointerKind::Up => {
                    if ctx.bounds().contains(p.pos) {
                        // 打开后宿主独占指针，控件收不到 Leave；提前清 hover 避免边框残留。
                        self.hover = false;
                        self.open(ctx);
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed => match k.key {
                Key::Enter | Key::Space | Key::Down => {
                    self.open(ctx);
                    true
                }
                _ => false,
            },
            _ => false,
        }
    }

    fn focusable(&self) -> bool {
        true
    }
}
