//! 容器/导航控件的内部 widget：滚动滚轮、模态遮罩、标签按钮。

use std::cell::Cell;

use crate::anim::{Easing, Transition};
use crate::core::{ClickFn, EventCtx, Widget};
use crate::event::{CursorShape, Event, Key, PointerKind};
use crate::geometry::{Color, Point, Rect, Size};
use crate::render::image::VisualState;
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;
use crate::ui::ImageContent;

/// 滚动条右缘可抓取宽度（与 core::hit_node 的命中区一致）。
const SCROLLBAR_HIT_W: i32 = 10;
/// 滚动条 thumb 最小高（与 core paint 一致）。
const SCROLLBAR_MIN_THUMB: f32 = 24.0;
/// 标签内图标与文字间距。
const TAB_ICON_GAP: i32 = 6;

/// 可嵌入任意控件的垂直滚动条辅助器（非独立 Widget）。
/// 封装绘制样式与拖动状态，由宿主控件在 `paint` / `on_event` 中调用。
pub struct VScrollbar {
    pub dragging: bool,
    start_y: i32,
    start_scroll: i32,
    /// 拖动开始时快照（on_move 无 canvas，用快照计算 thumb 行程）。
    drag_bar_h: f32,
    drag_content_h: i32,
    drag_view_h: i32,
}

impl Default for VScrollbar {
    fn default() -> Self {
        Self::new()
    }
}

impl VScrollbar {
    /// 轨道视觉宽度（px）。
    pub const TRACK_W: f32 = 5.0;
    /// 上下及右侧边距（px）。
    pub const MARGIN: f32 = 3.0;
    /// 滑块最小高度（px）。
    pub const MIN_THUMB: f32 = 16.0;
    /// 命中区宽度（比视觉宽，容易点到）。
    pub const HIT_W: i32 = 12;

    pub fn new() -> Self {
        Self {
            dragging: false,
            start_y: 0,
            start_scroll: 0,
            drag_bar_h: 0.0,
            drag_content_h: 0,
            drag_view_h: 0,
        }
    }

    fn bar_h(bounds: Rect) -> f32 {
        (bounds.h as f32 - 2.0 * Self::MARGIN).max(0.0)
    }

    fn thumb_h(bar_h: f32, content_h: i32, view_h: i32) -> f32 {
        let ratio = (view_h as f32 / content_h as f32).min(1.0);
        (bar_h * ratio).max(Self::MIN_THUMB)
    }

    fn max_scroll(content_h: i32, view_h: i32) -> i32 {
        (content_h - view_h).max(0)
    }

    /// 内容是否超出可见区域（需要显示滚动条）。
    pub fn has_overflow(content_h: i32, view_h: i32) -> bool {
        content_h > view_h
    }

    /// 命中判断：`pos` 是否在滚动条可点击区域内。`bounds` 为宿主控件绝对矩形。
    pub fn hit_test(&self, pos: Point, bounds: Rect, content_h: i32, view_h: i32) -> bool {
        Self::has_overflow(content_h, view_h)
            && pos.x >= bounds.right() - Self::HIT_W
            && pos.y >= bounds.y
            && pos.y < bounds.y + bounds.h
    }

    /// 绘制轨道 + 滑块。`view_h` 为去掉 padding 后的可见高度。
    pub fn paint(
        &self,
        canvas: &mut dyn Canvas,
        bounds: Rect,
        scroll_y: i32,
        content_h: i32,
        view_h: i32,
    ) {
        if !Self::has_overflow(content_h, view_h) {
            return;
        }
        let bx = bounds.x as f32 + bounds.w as f32 - Self::TRACK_W - Self::MARGIN;
        let by = bounds.y as f32;
        let bh = Self::bar_h(bounds);
        let th = Self::thumb_h(bh, content_h, view_h);
        let max = Self::max_scroll(content_h, view_h).max(1) as f32;
        let travel = (bh - th).max(1.0);
        let ty = by + Self::MARGIN + travel * (scroll_y as f32 / max);
        let r = Self::TRACK_W / 2.0;
        // 轨道（几乎透明）
        canvas.fill_round_rect(
            bx,
            by + Self::MARGIN,
            Self::TRACK_W,
            bh,
            r,
            &Paint::fill(Color::rgba(0, 0, 0, 0x14)),
        );
        // 滑块（拖动时加深）
        let alpha = if self.dragging { 0x78u8 } else { 0x52u8 };
        canvas.fill_round_rect(
            bx,
            ty,
            Self::TRACK_W,
            th,
            r,
            &Paint::fill(Color::rgba(0, 0, 0, alpha)),
        );
    }

    /// 按下处理：命中则开始拖动，返回 `true`。
    pub fn on_down(
        &mut self,
        pos: Point,
        bounds: Rect,
        scroll_y: i32,
        content_h: i32,
        view_h: i32,
        ctx: &mut EventCtx,
    ) -> bool {
        if !self.hit_test(pos, bounds, content_h, view_h) {
            return false;
        }
        self.dragging = true;
        self.start_y = pos.y;
        self.start_scroll = scroll_y;
        self.drag_bar_h = Self::bar_h(bounds);
        self.drag_content_h = content_h;
        self.drag_view_h = view_h;
        ctx.capture();
        true
    }

    /// 移动处理（拖动中）：返回新的 `scroll_y`。
    pub fn on_move(&self, pos: Point) -> Option<i32> {
        if !self.dragging {
            return None;
        }
        let th = Self::thumb_h(self.drag_bar_h, self.drag_content_h, self.drag_view_h);
        let travel = (self.drag_bar_h - th).max(1.0);
        let max = Self::max_scroll(self.drag_content_h, self.drag_view_h);
        let dy = pos.y - self.start_y;
        let delta = (dy as f32 * max as f32 / travel) as i32;
        Some((self.start_scroll + delta).clamp(0, max))
    }

    /// 抬起处理：返回 `true` 表示释放了拖动。
    pub fn on_up(&mut self, ctx: &mut EventCtx) -> bool {
        if self.dragging {
            self.dragging = false;
            ctx.release_capture();
            true
        } else {
            false
        }
    }
}

/// 滚动容器内部 widget：处理滚轮 + 拖动滚动条。
#[derive(Default)]
pub struct ScrollWidget {
    dragging: bool,
    start_y: i32,
    start_scroll: i32,
}

impl Widget for ScrollWidget {
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        let Event::Pointer(p) = ev else { return false };
        match p.kind {
            PointerKind::Wheel(delta) => {
                let (scroll_y, content_h, view_h) = ctx.scroll_metrics();
                let max_scroll = (content_h - view_h).max(0);
                // 无溢出内容 → 直接冒泡。
                if max_scroll == 0 {
                    return false;
                }
                // delta>0 向上（减小 scroll_y），delta<0 向下（增大 scroll_y）。
                let dy = -delta * 48 / 120;
                // 已到边界 → 冒泡给外层滚动容器，实现嵌套滚动。
                let at_boundary = (dy < 0 && scroll_y <= 0) || (dy > 0 && scroll_y >= max_scroll);
                if at_boundary {
                    return false;
                }
                ctx.scroll_by(dy);
                true
            }
            PointerKind::Down => {
                // 命中到这里且在右缘滚动条区域时启动拖动（hit_node 已优先派发）。
                let b = ctx.bounds();
                let (scroll_y, content_h, view_h) = ctx.scroll_metrics();
                if content_h > view_h && p.pos.x >= b.right() - SCROLLBAR_HIT_W {
                    self.dragging = true;
                    self.start_y = p.pos.y;
                    self.start_scroll = scroll_y;
                    ctx.capture();
                    true
                } else {
                    false
                }
            }
            PointerKind::Move if self.dragging => {
                let (_, content_h, view_h) = ctx.scroll_metrics();
                if view_h > 0 && content_h > view_h {
                    let max_scroll = content_h - view_h;
                    // 按 thumb 实际行程换算，精确反演绘制映射（thumb_h 与 core 同公式）。
                    let thumb_h =
                        (view_h as f32 * view_h as f32 / content_h as f32).max(SCROLLBAR_MIN_THUMB);
                    let travel = (view_h as f32 - thumb_h).max(1.0);
                    let dy = p.pos.y - self.start_y;
                    let delta = (dy as f32 * max_scroll as f32 / travel) as i32;
                    ctx.set_scroll((self.start_scroll + delta).clamp(0, max_scroll));
                }
                true
            }
            PointerKind::Up if self.dragging => {
                self.dragging = false;
                ctx.release_capture();
                true
            }
            _ => false,
        }
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

/// 可点击容器三态。
#[derive(PartialEq, Eq, Clone, Copy)]
enum ClickState {
    Normal,
    Hover,
    Press,
}

/// hover/press 叠层不透明度（叠层取主题文字色，明暗主题均自适应）。
const CLICK_HOVER_A: f32 = 0.06;
const CLICK_PRESS_A: f32 = 0.11;

/// 通用可点击容器 widget：为任意容器（卡片 / 列表项 / 自定义行）补上 hover/press
/// 视觉反馈 + 点击/键盘激活 + 手型光标。反馈用**主题自适应的半透明叠层**（绘制在节点
/// 背景之上、子内容之下），故明暗主题均成立、无需配置基色。
/// 由 `Element::clickable()` 接入；点击回调经 `Element::on_click` 注入。
pub struct Clickable {
    state: ClickState,
    on_click: Option<ClickFn>,
    /// 叠层不透明度补间（normal=0 / hover / press）；首帧靠 `primed` 落定。
    overlay: Cell<Transition<f32>>,
    primed: Cell<bool>,
}

impl Default for Clickable {
    fn default() -> Self {
        Self::new()
    }
}

impl Clickable {
    pub fn new() -> Self {
        Self {
            state: ClickState::Normal,
            on_click: None,
            overlay: Cell::new(Transition::new(0.0)),
            primed: Cell::new(false),
        }
    }
}

impl Widget for Clickable {
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        // 禁用：不显示 hover 反馈（核心层已拦事件，状态恒 Normal）。
        let th = crate::theme::current();
        let target = if !enabled {
            0.0
        } else {
            match self.state {
                ClickState::Normal => 0.0,
                ClickState::Hover => CLICK_HOVER_A,
                ClickState::Press => CLICK_PRESS_A,
            }
        };
        let mut ov = self.overlay.get();
        if !self.primed.get() {
            ov = Transition::new(target);
            self.primed.set(true);
        } else if ov.target() != target {
            ov.retarget(target, th.anim.fast(), Easing::EaseOut);
        }
        let a = ov.animate();
        self.overlay.set(ov);
        if a > 0.001 {
            canvas.fill_round_rect(
                bounds.x as f32,
                bounds.y as f32,
                bounds.w as f32,
                bounds.h as f32,
                style.corner_radius,
                &Paint::fill(th.palette.text.scale_alpha(a)),
            );
        }
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    if self.state == ClickState::Normal {
                        self.state = ClickState::Hover;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.state != ClickState::Press {
                        self.state = ClickState::Normal;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Down => {
                    self.state = ClickState::Press;
                    ctx.capture();
                    ctx.request_focus();
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Up => {
                    let was_press = self.state == ClickState::Press;
                    let inside = ctx.bounds().contains(p.pos);
                    self.state = if inside {
                        ClickState::Hover
                    } else {
                        ClickState::Normal
                    };
                    ctx.release_capture();
                    ctx.mark_dirty();
                    if was_press && inside {
                        if let Some(cb) = self.on_click.as_mut() {
                            cb(ctx);
                        }
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) => {
                if k.pressed && (k.key == Key::Enter || k.key == Key::Space) {
                    if let Some(cb) = self.on_click.as_mut() {
                        cb(ctx);
                    }
                    ctx.mark_dirty();
                    true
                } else {
                    false
                }
            }
        }
    }
    fn focusable(&self) -> bool {
        true
    }
    fn take_click(&mut self, f: ClickFn) {
        self.on_click = Some(f);
    }
    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }
    fn reset_interaction(&mut self) {
        self.state = ClickState::Normal;
        self.primed.set(false); // 下次显示瞬时落定到静止叠层，不回放旧的 hover/press
    }
}

/// 图标按钮内容：字形（draw_text）或图片（ImageContent）。
enum IconKind {
    Glyph(String),
    Image(ImageContent),
}

/// 图标按钮默认方形边长与内边距（px）。Element 可用 `.size()` 覆盖。
const ICON_BTN_SIZE: i32 = 30;
const ICON_BTN_PAD: i32 = 6;

/// 纯图标按钮：无文字、方形、hover/press 半透明圆底 + 点击/键盘激活 + 手型光标。
/// 用于 ⓘ 信息、▲▼ 调序、× 关闭等工具图标。字形随 `.fg()` 取色（默认主题文字色）；
/// 图片随状态调制。由 `Element::icon_button()/icon_button_content()` 接入。
pub struct IconButton {
    kind: IconKind,
    state: ClickState,
    on_click: Option<ClickFn>,
    overlay: Cell<Transition<f32>>,
    primed: Cell<bool>,
}

impl IconButton {
    pub fn glyph(g: impl Into<String>) -> Self {
        Self::with(IconKind::Glyph(g.into()))
    }
    pub fn image(content: ImageContent) -> Self {
        Self::with(IconKind::Image(content))
    }
    fn with(kind: IconKind) -> Self {
        Self {
            kind,
            state: ClickState::Normal,
            on_click: None,
            overlay: Cell::new(Transition::new(0.0)),
            primed: Cell::new(false),
        }
    }
    fn visual_state(&self, enabled: bool) -> VisualState {
        if !enabled {
            return VisualState::Disabled;
        }
        match self.state {
            ClickState::Normal => VisualState::Normal,
            ClickState::Hover => VisualState::Hover,
            ClickState::Press => VisualState::Pressed,
        }
    }
}

impl Widget for IconButton {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        match &self.kind {
            IconKind::Glyph(g) => {
                let t = text.measure(g, style.font_family.as_deref(), style.font_size, None);
                let side = t.w.max(t.h).max(style.font_size as i32) + 2 * ICON_BTN_PAD;
                Size::new(side.max(ICON_BTN_SIZE), side.max(ICON_BTN_SIZE))
            }
            IconKind::Image(_) => Size::new(ICON_BTN_SIZE, ICON_BTN_SIZE),
        }
    }
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        // hover/press 圆底（主题文字色低 alpha，自适应明暗）。
        let target = if !enabled {
            0.0
        } else {
            match self.state {
                ClickState::Normal => 0.0,
                ClickState::Hover => CLICK_HOVER_A,
                ClickState::Press => CLICK_PRESS_A,
            }
        };
        let mut ov = self.overlay.get();
        if !self.primed.get() {
            ov = Transition::new(target);
            self.primed.set(true);
        } else if ov.target() != target {
            ov.retarget(target, th.anim.fast(), Easing::EaseOut);
        }
        let a = ov.animate();
        self.overlay.set(ov);
        if a > 0.001 {
            let r = if style.corner_radius > 0.0 {
                style.corner_radius
            } else {
                th.metrics.corner_sm
            };
            canvas.fill_round_rect(
                bounds.x as f32,
                bounds.y as f32,
                bounds.w as f32,
                bounds.h as f32,
                r,
                &Paint::fill(th.palette.text.scale_alpha(a)),
            );
        }
        match &self.kind {
            IconKind::Glyph(g) => {
                let color = if enabled {
                    style.resolved_fg(&th)
                } else {
                    th.palette.text_disabled
                };
                canvas.draw_text(
                    g,
                    bounds,
                    color,
                    Align::Center,
                    style.font_family.as_deref(),
                    style.font_size,
                );
            }
            IconKind::Image(content) => {
                let side = (bounds.w.min(bounds.h) - 2 * ICON_BTN_PAD).max(1);
                let ix = bounds.x + (bounds.w - side) / 2;
                let iy = bounds.y + (bounds.h - side) / 2;
                content.paint_into(
                    Rect::new(ix, iy, side, side),
                    canvas,
                    style,
                    self.visual_state(enabled),
                );
            }
        }
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter => {
                    if self.state == ClickState::Normal {
                        self.state = ClickState::Hover;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.state != ClickState::Press {
                        self.state = ClickState::Normal;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Down => {
                    self.state = ClickState::Press;
                    ctx.capture();
                    ctx.request_focus();
                    ctx.mark_dirty();
                    true
                }
                PointerKind::Up => {
                    let was_press = self.state == ClickState::Press;
                    let inside = ctx.bounds().contains(p.pos);
                    self.state = if inside {
                        ClickState::Hover
                    } else {
                        ClickState::Normal
                    };
                    ctx.release_capture();
                    ctx.mark_dirty();
                    if was_press && inside {
                        if let Some(cb) = self.on_click.as_mut() {
                            cb(ctx);
                        }
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) => {
                if k.pressed && (k.key == Key::Enter || k.key == Key::Space) {
                    if let Some(cb) = self.on_click.as_mut() {
                        cb(ctx);
                    }
                    ctx.mark_dirty();
                    true
                } else {
                    false
                }
            }
        }
    }
    fn focusable(&self) -> bool {
        true
    }
    fn take_click(&mut self, f: ClickFn) {
        self.on_click = Some(f);
    }
    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }
    fn reset_interaction(&mut self) {
        self.state = ClickState::Normal;
        self.primed.set(false);
    }
}

/// 标签按钮：点击切换共享选中索引，选中时高亮 + 底部指示条。可选前置图标。
pub struct TabButton {
    label: String,
    icon: Option<ImageContent>,
    group: Signal<usize>,
    index: usize,
    hover: bool,
    /// 文字色补间（inactive/hover/accent 淡变）；首帧靠 `primed` 落定。
    color_anim: Cell<Transition<Color>>,
    primed: Cell<bool>,
    /// 选中底部指示条补间（0..1）：从中心展宽 + 淡入。
    ind: Cell<Transition<f32>>,
}

impl TabButton {
    pub fn new(label: String, group: Signal<usize>, index: usize) -> Self {
        let ind = if group.get() == index { 1.0 } else { 0.0 };
        Self {
            label,
            icon: None,
            group,
            index,
            hover: false,
            color_anim: Cell::new(Transition::new(Color::rgba(0, 0, 0, 0))),
            primed: Cell::new(false),
            ind: Cell::new(Transition::new(ind)),
        }
    }
    /// 附带前置图标。
    pub fn with_icon(mut self, icon: ImageContent) -> Self {
        self.icon = Some(icon);
        self
    }
    fn selected(&self) -> bool {
        self.group.get() == self.index
    }
    /// 标签视觉状态（供图标调制）：选中 > 悬停 > 普通。
    fn visual_state(&self) -> VisualState {
        if self.selected() {
            VisualState::Selected
        } else if self.hover {
            VisualState::Hover
        } else {
            VisualState::Normal
        }
    }
}

impl Widget for TabButton {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let t = text.measure(
            &self.label,
            style.font_family.as_deref(),
            style.font_size,
            None,
        );
        let icon_extra = if self.icon.is_some() {
            t.h + TAB_ICON_GAP
        } else {
            0
        };
        Size::new(t.w + 24 + icon_extra, t.h + 16)
    }
    fn paint(
        &self,
        bounds: Rect,
        _content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let th = crate::theme::current();
        let (pal, tab) = (&th.palette, &th.tab);
        let sel = self.selected();
        // 禁用：文字置灰、图标走 Disabled 调制；否则按选中/悬停三态。
        let target_color = if !enabled {
            pal.text_disabled
        } else if sel {
            tab.accent(pal)
        } else if self.hover {
            tab.hover(pal)
        } else {
            tab.inactive(pal)
        };
        // 文字色补间：首帧落定，其后三态淡变。
        let mut canim = self.color_anim.get();
        if !self.primed.get() {
            canim = Transition::new(target_color);
            self.primed.set(true);
        } else if canim.target() != target_color {
            canim.retarget(target_color, th.anim.fast(), Easing::EaseOut);
        }
        let color = canim.animate();
        self.color_anim.set(canim);
        let vstate = if !enabled {
            VisualState::Disabled
        } else {
            self.visual_state()
        };
        // 有图标：图标 + 文字作为整体水平居中（图标在左）；否则文字整体居中。
        if let Some(icon) = &self.icon {
            let ts =
                canvas.measure_text(&self.label, style.font_family.as_deref(), style.font_size);
            let ih = ts.h;
            let total_w = ih + TAB_ICON_GAP + ts.w;
            let sx = bounds.x + ((bounds.w - total_w) / 2).max(0);
            let iy = bounds.y + ((bounds.h - ih) / 2).max(0);
            let istyle = Style {
                corner_radius: 0.0,
                ..style.clone()
            };
            icon.paint_into(Rect::new(sx, iy, ih, ih), canvas, &istyle, vstate);
            let tr = Rect::new(sx + ih + TAB_ICON_GAP, bounds.y, ts.w + 2, bounds.h);
            canvas.draw_text(
                &self.label,
                tr,
                color,
                Align::Start,
                style.font_family.as_deref(),
                style.font_size,
            );
        } else {
            canvas.draw_text(
                &self.label,
                bounds,
                color,
                Align::Center,
                style.font_family.as_deref(),
                style.font_size,
            );
        }
        // 底部指示条补间：选中(且启用)→1，否则→0；从中心展宽 + 淡入。禁用不显示。
        let mut ind = self.ind.get();
        let ind_target = if sel && enabled { 1.0 } else { 0.0 };
        if ind.target() != ind_target {
            ind.retarget(ind_target, th.anim.normal(), Easing::EaseOut);
        }
        let amount = ind.animate();
        self.ind.set(ind);
        if amount > 0.0 {
            // 指示条比文字略宽（参考设计：蓝条宽于标签文字），按文字宽 + 两侧外扩，
            // 钳到 tab 宽内。有图标时把图标宽并入。
            let ts =
                canvas.measure_text(&self.label, style.font_family.as_deref(), style.font_size);
            let content_w = if self.icon.is_some() {
                ts.h + TAB_ICON_GAP + ts.w
            } else {
                ts.w
            };
            let full = ((content_w + 22) as f32)
                .min(bounds.w as f32 - 4.0)
                .max(0.0);
            let bw = full * amount;
            let cx = bounds.x as f32 + bounds.w as f32 / 2.0;
            canvas.fill_round_rect(
                cx - bw / 2.0,
                (bounds.y + bounds.h - 3) as f32,
                bw,
                3.0,
                1.5,
                &Paint::fill(tab.accent(pal).scale_alpha(amount)),
            );
        }
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
                        self.group.set(self.index);
                        // 切页改变 visible_when 绑定的内容面板显隐（非局部 + 布局变化）→ 重排整窗。
                        ctx.mark_layout_dirty();
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed && (k.key == Key::Enter || k.key == Key::Space) => {
                self.group.set(self.index);
                ctx.mark_layout_dirty();
                true
            }
            _ => false,
        }
    }
    fn focusable(&self) -> bool {
        true
    }
    fn reset_interaction(&mut self) {
        self.hover = false;
        self.primed.set(false); // 下次显示瞬时落定文字色，不回放旧 hover
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::image::{Fit, Image};
    use crate::signal::signal;
    use crate::text::NullTextEngine;

    #[test]
    fn tab_icon_widens_measure() {
        let g = signal(0);
        let style = Style::default();
        let mut te = NullTextEngine;
        let w0 = TabButton::new("Home".into(), g, 0)
            .measure(Size::ZERO, &style, &mut te)
            .w;
        let red = Image::from_rgba(4, 4, &[255u8, 0, 0, 255].repeat(4 * 4)).unwrap();
        let iconed = TabButton::new("Home".into(), g, 0)
            .with_icon(ImageContent::new(Some(red)).fit(Fit::Fill));
        let w1 = iconed.measure(Size::ZERO, &style, &mut te).w;
        assert!(w1 > w0, "带图标标签应更宽：w0={w0}, w1={w1}");
    }

    #[test]
    fn tab_visual_state_tracks_selection() {
        let g = signal(2);
        assert_eq!(
            TabButton::new("A".into(), g, 2).visual_state(),
            VisualState::Selected
        );
        assert_eq!(
            TabButton::new("B".into(), g, 0).visual_state(),
            VisualState::Normal
        );
    }
}
