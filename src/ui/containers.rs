//! 容器/导航控件的内部 widget：滚动滚轮、模态遮罩、下划线式标签条。

use std::cell::{Cell, RefCell};

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
use crate::ui::TextContent;

/// 标签内图标与文字间距。
const TAB_ICON_GAP: i32 = 8;
/// 每个标签的左右内边距（标签之间不再留 spacing，靠它自然分隔且使 hover 区连续）。
const TAB_PAD_X: i32 = 16;
/// 标签过多、整条放不下时内边距的收缩下限。
const TAB_PAD_MIN: i32 = 8;

/// 可嵌入任意控件的垂直滚动条辅助器（非独立 Widget）。
/// 封装绘制样式与拖动状态，由宿主控件在 `paint` / `on_event` 中调用。
///
/// 几何常量取自 `core::scrollbar`（唯一真相源）。**不含**窗口边缘内缩逻辑：现有宿主
/// （多行输入框）恒嵌在有内边距的表单/对话框里，右缘不会贴到窗口缩放边框；若将来有
/// 宿主要贴边铺满，需比照 `core::Tree::scrollbar_edge_inset` 补内缩。
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
    pub const TRACK_W: f32 = crate::core::scrollbar::TRACK_W;
    /// 上下及右侧边距（px）。
    pub const MARGIN: f32 = crate::core::scrollbar::MARGIN;
    /// 滑块最小高度（px）。
    pub const MIN_THUMB: f32 = crate::core::scrollbar::MIN_THUMB;
    /// 命中区宽度（比视觉宽，容易点到）。
    pub const HIT_W: i32 = crate::core::scrollbar::HIT_W;

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

    /// 滑块高度。这里的基准是**轨道高** `bar_h`（已扣掉上下 `MARGIN`），与 core 直接用
    /// 视口高的版本差一个常量留白，故不能直接复用 `core::scrollbar::thumb_h`；
    /// 下界 `MIN_THUMB` 仍取共享常量。
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
        // 配色取自主题（同 core 的滚动条）：写死黑色半透明会在深色主题下把滑块一起隐没。
        // 轨道默认不画，只露滑块。
        if let Some(track) = crate::core::scrollbar::track() {
            canvas.fill_round_rect(
                bx,
                by + Self::MARGIN,
                Self::TRACK_W,
                bh,
                r,
                &Paint::fill(track),
            );
        }
        // 滑块（拖动时加深，给出"抓住了"的反馈）
        let thumb = crate::core::scrollbar::thumb(self.dragging);
        canvas.fill_round_rect(bx, ty, Self::TRACK_W, th, r, &Paint::fill(thumb));
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
                // 命中到这里且在滚动条可抓取区时启动拖动（hit_node 已优先派发）。
                // 区间取自 ctx，与 hit_node 的判定同源——贴窗口边的容器滚动条整体内缩，
                // 这里若还按 `right - HIT_W` 自行推算就会错开一个内缩量。
                let (lo, hi) = ctx.scrollbar_hit_zone();
                let (scroll_y, content_h, view_h) = ctx.scroll_metrics();
                if content_h > view_h && p.pos.x >= lo && p.pos.x < hi {
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
                    // 按 thumb 实际行程换算，精确反演绘制映射（与 core paint 同源公式）。
                    let travel = crate::core::scrollbar::travel(view_h, content_h);
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

    fn is_modal(&self) -> bool {
        // 键盘侧的模态：Tab 焦点环圈在遮罩子树内，不走到被盖住的控件上。
        // 指针侧靠上面的 on_event 吞事件，两者合起来才是完整的模态。
        true
    }

    fn scrim_passthrough(&self) -> bool {
        // 仅对窗口拖动区判定透明：无边框窗口弹出对话框后，自绘标题栏仍可拖窗
        // （遮罩照常吞事件、照常屏蔽标题栏窗口按钮）。见 `Widget::scrim_passthrough`。
        true
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
    Glyph(TextContent),
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
    pub fn glyph(g: impl Into<TextContent>) -> Self {
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
                let t = text.measure(
                    g.resolve().as_ref(),
                    &crate::text::TextStyle::of(style),
                    None,
                );
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
                    g.resolve().as_ref(),
                    bounds,
                    color,
                    Align::Center,
                    &crate::text::TextStyle::of(style),
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

/// 标签条中的一项：标题 + 可选前置图标。
pub struct TabItem {
    pub label: String,
    pub icon: Option<ImageContent>,
}

impl TabItem {
    pub fn new(label: String) -> Self {
        Self { label, icon: None }
    }
    /// 前置图标（图片内容）。
    pub fn icon_content(mut self, icon: ImageContent) -> Self {
        self.icon = Some(icon);
        self
    }

    /// 改名为 [`TabItem::icon_content`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `icon_content`：`with_` 在 Rust 生态里表示「带某配置构造」而非链式设属性；用 `_content` 后缀与 `Element::icon_content` 对齐，区别于收字形串的 `MenuItem::icon`"
    )]
    pub fn with_icon(self, icon: ImageContent) -> Self {
        self.icon_content(icon)
    }
}

/// 标签条视觉风格。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TabStyle {
    /// 下划线式（默认，简洁）：底部贯穿基线 + 选中项整格宽的滑动下划线。
    #[default]
    Underline,
    /// 胶囊式：选中项为 accent 实底圆角胶囊、白字，胶囊在标签间滑动；无基线。
    Pill,
}

/// 标签条：**整条**是一个自绘控件，而非每个标签一个节点。
///
/// 之所以合并成一个控件：滑动指示条需要跨标签的布局信息（从哪滑到哪），独立子节点
/// 各自只知道自己的矩形，拿不到邻居的位置；另外整条只占一个焦点节点、内部用
/// Left/Right 在标签间移动，正是 WAI-ARIA tablist 的 roving tabindex 惯例。
///
/// 自己负责：测量所有标签、横向排布、绘制基线/悬停淡底/图标文字/滑动指示条，
/// 以及按 x 分段做命中测试。
pub struct TabBar {
    items: Vec<TabItem>,
    /// 视觉风格（下划线 / 胶囊）。
    style: TabStyle,
    /// 共享选中索引（与 `Element::tabs` 的内容面板 `visible_when` 同源）。
    group: Signal<usize>,
    hover: Option<usize>,
    /// 选中滑块（下划线条 / 胶囊）左端与宽度的补间：切换时同一块从旧标签滑到新标签，
    /// 宽度贴合新标签整格。两种风格共用这一套补间，只是最终绘制形态不同。
    ind_x: Cell<Transition<f32>>,
    ind_w: Cell<Transition<f32>>,
    /// 每项文字色补间（未选/hover/选中 三态淡变）。
    text_anim: Vec<Cell<Transition<Color>>>,
    /// 首帧落定标志：初次绘制时补间瞬时到位，不回放。
    primed: Cell<bool>,
    /// 每项布局 `(相对条左缘的 x, 宽)`；`on_event` 命中测试读取。
    ///
    /// 存**相对**偏移而非绝对 x，是为了让 `measure` 也能写这份缓存——布局依赖文字
    /// 测量，而 measure 拿不到最终 bounds，只有相对量对它才是可计算的。measure 先于
    /// paint 跑，于是首次 paint 之前到达的指针事件也有布局可查，不会被整条吞掉。
    ///
    /// 取舍：measure 只能按自然内边距（`TAB_PAD_X`）摊，而 paint 会在整条放不下时
    /// 收缩内边距（见 [`Self::shrink_pad`]）。因此**溢出场景**下首帧前的命中可能有
    /// 几个像素偏移，paint 一跑就以真实内边距覆盖、自校正。不溢出时二者完全一致。
    layout: RefCell<Vec<(i32, i32)>>,
}

/// 单个标签的度量：`(内容宽, 整宽, 文字高)`。内容宽即指示条宽度。
type TabMetrics = (i32, i32, i32);

impl TabBar {
    pub fn new(items: Vec<TabItem>, group: Signal<usize>) -> Self {
        let n = items.len();
        Self {
            items,
            style: TabStyle::default(),
            group,
            hover: None,
            ind_x: Cell::new(Transition::new(0.0)),
            ind_w: Cell::new(Transition::new(0.0)),
            text_anim: (0..n)
                .map(|_| Cell::new(Transition::new(Color::rgba(0, 0, 0, 0))))
                .collect(),
            primed: Cell::new(false),
            layout: RefCell::new(Vec::new()),
        }
    }

    /// 设定视觉风格（默认 [`TabStyle::Underline`]）。
    pub fn style(mut self, style: TabStyle) -> Self {
        self.style = style;
        self
    }

    /// 改名为 [`TabBar::style`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `style`：`with_` 在 Rust 生态里表示「带某配置构造」（Vec::with_capacity）而非链式设属性"
    )]
    pub fn with_style(self, style: TabStyle) -> Self {
        self.style(style)
    }

    /// 逐项度量。文字**恒按选中字重**测量：选中态会把字重提到 600，若按各自当前
    /// 字重测，选中项一变宽整条就跟着重排、标签左右抖动。故宽度只取最宽的那档。
    fn metrics(
        &self,
        style: &Style,
        mut measure: impl FnMut(&str, &crate::text::TextStyle) -> Size,
    ) -> Vec<TabMetrics> {
        let weight = crate::theme::current().tab.selected_weight();
        let ts = crate::text::TextStyle::of(style).with_weight(weight);
        self.items
            .iter()
            .map(|it| {
                let t = measure(&it.label, &ts);
                let content = if it.icon.is_some() {
                    t.h + TAB_ICON_GAP + t.w
                } else {
                    t.w
                };
                (content, content + TAB_PAD_X * 2, t.h)
            })
            .collect()
    }

    /// 按给定内边距把逐项度量摊成布局：每项 `(相对条左缘的 x, 宽)`，项间无间隙。
    fn lay_out(m: &[TabMetrics], pad: i32) -> Vec<(i32, i32)> {
        let mut out = Vec::with_capacity(m.len());
        let mut x = 0;
        for (content_w, _, _) in m {
            let w = content_w + pad * 2;
            out.push((x, w));
            x += w;
        }
        out
    }

    /// 实际使用的左右内边距：整条放不下 `avail_w` 时按需收缩到 `TAB_PAD_MIN`，
    /// 优先保证末项完整可见——被切掉一半的标签比略紧的间距难看得多。
    fn shrink_pad(m: &[TabMetrics], avail_w: i32) -> i32 {
        let natural: i32 = m.iter().map(|(_, iw, _)| *iw).sum();
        if m.is_empty() || natural <= avail_w {
            return TAB_PAD_X;
        }
        let sides = m.len() as i32 * 2;
        let per_side = (natural - avail_w + sides - 1) / sides;
        (TAB_PAD_X - per_side).max(TAB_PAD_MIN)
    }

    /// 有效选中索引（钳到项数内，容忍外部信号越界）。
    fn selected(&self) -> usize {
        self.group.get().min(self.items.len().saturating_sub(1))
    }

    /// 按**相对条左缘**的 x 命中某标签（绝对坐标须先减去 `bounds.x`）。
    fn index_at(&self, rel_x: i32) -> Option<usize> {
        self.layout
            .borrow()
            .iter()
            .position(|(ix, iw)| (*ix..*ix + *iw).contains(&rel_x))
    }

    /// 切到第 `i` 项。切页改变 `visible_when` 绑定的内容面板显隐（非局部 + 布局
    /// 变化）→ 重排整窗。
    fn select(&mut self, i: usize, ctx: &mut EventCtx) {
        if i < self.items.len() && self.group.get() != i {
            self.group.set(i);
            ctx.mark_layout_dirty();
        }
    }

    /// 键盘导航的目标项：Left/Right 循环、Home/End 跳首尾、Enter/Space 保持当前。
    fn key_target(key: Key, cur: usize, n: usize) -> Option<usize> {
        match key {
            Key::Left => Some((cur + n - 1) % n),
            Key::Right => Some((cur + 1) % n),
            Key::Home => Some(0),
            Key::End => Some(n - 1),
            Key::Enter | Key::Space => Some(cur),
            _ => None,
        }
    }
}

impl Widget for TabBar {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let m = self.metrics(style, |s, ts| text.measure(s, ts, None));
        // 顺手按自然内边距写一份布局缓存，使首次 paint 之前到达的指针事件也能命中
        // （见 `layout` 字段文档）。paint 会用真实内边距覆盖它。
        let xs = Self::lay_out(&m, TAB_PAD_X);
        let w = xs.last().map_or(0, |(x, iw)| x + iw);
        self.layout.replace(xs);
        Size::new(w, crate::theme::current().tab.height())
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
        let first = !self.primed.get();
        self.primed.set(true);
        let pill = self.style == TabStyle::Pill;

        // 布局：标签依次左排，间距 0（靠各自 padding 分隔，命中/居中都按整格算）。
        // 这里用真实可用宽算内边距，比 measure 那份更准，覆盖回缓存。
        let m = self.metrics(style, |s, ts| canvas.measure_text(s, ts));
        let xs = Self::lay_out(&m, Self::shrink_pad(&m, bounds.w));
        self.layout.replace(xs.clone());

        // 基线：仅下划线风格有，横贯整条宽度的 1px 底线，下划线压其上。胶囊风格无基线。
        if !pill {
            canvas.fill_rect(
                bounds.x as f32,
                (bounds.y + bounds.h - 1) as f32,
                bounds.w as f32,
                1.0,
                &Paint::fill(tab.baseline(pal)),
            );
        }

        let sel = self.selected();

        // 选中滑块补间（两风格共用）：目标 = 选中项**整格** (x, w)，切换时同一块从旧
        // 标签滑到新标签、宽度贴合新格。补间量存相对坐标，绘制时才加 bounds.x——否则整条
        // 被移动（窗口缩放等）会让滑块平白滑一段。首帧瞬时落定，不回放。
        let (px, pw) = if self.items.is_empty() {
            (0.0, 0.0)
        } else {
            let (sel_x, sel_w) = xs[sel];
            let (tx, tw) = (sel_x as f32, sel_w as f32);
            let (mut ax, mut aw) = (self.ind_x.get(), self.ind_w.get());
            if first {
                ax = Transition::new(tx);
                aw = Transition::new(tw);
            } else {
                if ax.target() != tx {
                    ax.retarget(tx, th.anim.normal(), Easing::EaseOut);
                }
                if aw.target() != tw {
                    aw.retarget(tw, th.anim.normal(), Easing::EaseOut);
                }
            }
            let out = (ax.animate(), aw.animate());
            self.ind_x.set(ax);
            self.ind_w.set(aw);
            out
        };

        // 胶囊风格：选中滑块是 accent 实底圆角胶囊，须在文字**之前**画（白字压其上）。
        // 上下、左右各内缩，使胶囊不贴边、相邻格间留空隙。禁用态不画。
        if pill && enabled && !self.items.is_empty() {
            let (inset_x, inset_y) = (3.0_f32, 5.0_f32);
            let cap_h = (bounds.h as f32 - inset_y * 2.0).max(0.0);
            canvas.fill_round_rect(
                bounds.x as f32 + px + inset_x,
                bounds.y as f32 + inset_y,
                (pw - inset_x * 2.0).max(0.0),
                cap_h,
                cap_h / 2.0,
                &Paint::fill(tab.accent(pal)),
            );
        }

        // 文字纵向居中区：下划线风格留出底部指示条带（-3），胶囊风格用满整条高。
        let th_box = if pill { bounds.h } else { bounds.h - 3 };
        for (i, ((content_w, _, text_h), (ix, iw))) in m.iter().zip(xs.iter()).enumerate() {
            // 缓存里存的是相对偏移，绘制需换回绝对 x。
            let (content_w, text_h, iw) = (*content_w, *text_h, *iw);
            let ix = bounds.x + *ix;

            // 文字色：禁用 > 选中 > 悬停 > 普通，三态补间；首帧落定。
            // 选中色随风格：下划线用 accent 本色，胶囊用 on_accent（压在实底胶囊上要反色）。
            let sel_color = if pill { pal.on_accent } else { tab.accent(pal) };
            let target_color = if !enabled {
                pal.text_disabled
            } else if i == sel {
                sel_color
            } else if self.hover == Some(i) {
                tab.hover(pal)
            } else {
                tab.inactive(pal)
            };
            let mut ca = self.text_anim[i].get();
            if first {
                ca = Transition::new(target_color);
            } else if ca.target() != target_color {
                ca.retarget(target_color, th.anim.fast(), Easing::EaseOut);
            }
            let color = ca.animate();
            self.text_anim[i].set(ca);

            let vstate = if !enabled {
                VisualState::Disabled
            } else if i == sel {
                VisualState::Selected
            } else if self.hover == Some(i) {
                VisualState::Hover
            } else {
                VisualState::Normal
            };
            // 选中项加粗。measure 恒按 600 测，故这里变粗不会改变布局。
            let ts = if i == sel {
                crate::text::TextStyle::of(style).with_weight(tab.selected_weight())
            } else {
                crate::text::TextStyle::of(style)
            };
            let cx = ix + ((iw - content_w) / 2).max(0);
            if let Some(icon) = &self.items[i].icon {
                let iy = bounds.y + ((th_box - text_h) / 2).max(0);
                let istyle = Style {
                    corner_radius: 0.0,
                    ..style.clone()
                };
                icon.paint_into(Rect::new(cx, iy, text_h, text_h), canvas, &istyle, vstate);
                let tx = cx + text_h + TAB_ICON_GAP;
                canvas.draw_text(
                    &self.items[i].label,
                    Rect::new(tx, bounds.y, ix + iw - tx, th_box),
                    color,
                    Align::Start,
                    &ts,
                );
            } else {
                canvas.draw_text(
                    &self.items[i].label,
                    Rect::new(ix, bounds.y, iw, th_box),
                    color,
                    Align::Center,
                    &ts,
                );
            }
        }

        // 下划线：选中项**整格宽**的实条，无圆角，压在基线上。文字之后画（在底部不与文字重叠）。
        if !pill && enabled && !self.items.is_empty() {
            let ih = tab.indicator_h();
            canvas.fill_rect(
                bounds.x as f32 + px,
                (bounds.y + bounds.h) as f32 - ih,
                pw,
                ih,
                &Paint::fill(tab.accent(pal)),
            );
        }
    }

    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        match ev {
            Event::Pointer(p) => match p.kind {
                PointerKind::Enter | PointerKind::Move => {
                    let h = if ctx.bounds().contains(p.pos) {
                        self.index_at(p.pos.x - ctx.bounds().x)
                    } else {
                        None
                    };
                    if h != self.hover {
                        self.hover = h;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Leave => {
                    if self.hover.is_some() {
                        self.hover = None;
                        ctx.mark_dirty();
                    }
                    true
                }
                PointerKind::Down => {
                    ctx.request_focus();
                    true
                }
                PointerKind::Up => {
                    if ctx.bounds().contains(p.pos) {
                        if let Some(i) = self.index_at(p.pos.x - ctx.bounds().x) {
                            self.select(i, ctx);
                        }
                    }
                    true
                }
                _ => false,
            },
            Event::Key(k) if k.pressed && !self.items.is_empty() => {
                let n = self.items.len();
                match Self::key_target(k.key, self.selected(), n) {
                    Some(i) => {
                        self.select(i, ctx);
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    fn focusable(&self) -> bool {
        true
    }

    fn cursor(&self) -> CursorShape {
        CursorShape::Hand
    }

    fn reset_interaction(&mut self) {
        self.hover = None;
        self.primed.set(false); // 下次显示瞬时落定，不回放旧 hover / 旧指示条位置
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::image::{Fit, Image};
    use crate::signal::signal;
    use crate::text::NullTextEngine;

    /// 字重敏感的测量引擎：宽度按 weight/400 放大。NullTextEngine 忽略字重，
    /// 用它测不出「measure 是否钉死在选中字重」——这正是本组测试要盯的点。
    struct WeightyEngine;
    impl crate::text::TextEngine for WeightyEngine {
        fn measure(
            &mut self,
            text: &str,
            ts: &crate::text::TextStyle,
            _max_width: Option<f32>,
        ) -> Size {
            let base = text.chars().count() as f32 * ts.size * 0.6;
            Size::new(
                (base * ts.weight as f32 / 400.0).ceil() as i32,
                ts.size.ceil() as i32,
            )
        }
        fn draw(
            &mut self,
            _pixmap: &mut tiny_skia::Pixmap,
            _text: &str,
            _rect: Rect,
            _color: Color,
            _align: Align,
            _ts: &crate::text::TextStyle,
            _clip: Option<Rect>,
        ) {
        }
    }

    fn bar(labels: &[&str], g: Signal<usize>) -> TabBar {
        TabBar::new(
            labels.iter().map(|s| TabItem::new((*s).into())).collect(),
            g,
        )
    }

    #[test]
    fn tab_measure_ignores_selection_and_pins_to_selected_weight() {
        let style = Style::default();
        let mut te = WeightyEngine;
        // 同一组标签，选中项不同，测量必须一致——否则切页会引起整条抖动重排。
        let w_sel0 = bar(&["列表", "关于"], signal(0))
            .measure(Size::ZERO, &style, &mut te)
            .w;
        let w_sel1 = bar(&["列表", "关于"], signal(1))
            .measure(Size::ZERO, &style, &mut te)
            .w;
        assert_eq!(w_sel0, w_sel1, "measure 不应随选中项变化");

        // 且是按选中字重（600）测的，不是按 Style 的常规字重。
        let weight = crate::theme::current().tab.selected_weight();
        let expect: i32 = ["列表", "关于"]
            .iter()
            .map(|s| {
                te.measure(
                    s,
                    &crate::text::TextStyle::of(&style).with_weight(weight),
                    None,
                )
                .w + TAB_PAD_X * 2
            })
            .sum();
        assert_eq!(w_sel0, expect, "measure 应恒按选中字重 {weight} 测量");
    }

    #[test]
    fn tab_icon_widens_measure() {
        let g = signal(0);
        let style = Style::default();
        let mut te = NullTextEngine;
        let w0 = bar(&["Home"], g).measure(Size::ZERO, &style, &mut te).w;
        let red = Image::from_rgba(4, 4, &[255u8, 0, 0, 255].repeat(4 * 4)).unwrap();
        let iconed = TabBar::new(
            vec![TabItem::new("Home".into())
                .icon_content(ImageContent::new(Some(red)).fit(Fit::Fill))],
            g,
        );
        let w1 = iconed.measure(Size::ZERO, &style, &mut te).w;
        assert!(w1 > w0, "带图标标签应更宽：w0={w0}, w1={w1}");
    }

    #[test]
    fn tab_hit_test_maps_x_to_index() {
        let b = bar(&["A", "B", "C"], signal(0));
        // 布局缓存存的是相对条左缘的偏移：宽 60/80/40 依次排开。
        b.layout.replace(vec![(0, 60), (60, 80), (140, 40)]);
        assert_eq!(b.index_at(-1), None, "条左侧不命中");
        assert_eq!(b.index_at(0), Some(0), "首项左边界属首项");
        assert_eq!(b.index_at(59), Some(0));
        assert_eq!(b.index_at(60), Some(1), "边界归右侧项，分段无重叠无空隙");
        assert_eq!(b.index_at(139), Some(1));
        assert_eq!(b.index_at(179), Some(2));
        assert_eq!(b.index_at(180), None, "条右侧不命中");
    }

    /// 回归护栏：`measure` 必须也写布局缓存。此前只有 `paint` 写，导致首次绘制之前
    /// 到达的指针事件恒命中 None，整条的点击与 hover 被静默吞掉。
    #[test]
    fn tab_hit_test_works_before_any_paint() {
        let style = Style::default();
        let mut te = NullTextEngine;
        let b = bar(&["列表", "关于"], signal(0));
        assert_eq!(b.index_at(5), None, "measure 之前尚无布局，命中为空");

        let total = b.measure(Size::ZERO, &style, &mut te).w;
        // 未经任何 paint，命中即应可用：首项左缘、末项右缘内、以及条外。
        assert_eq!(b.index_at(0), Some(0), "measure 后首项即可命中");
        assert_eq!(b.index_at(total - 1), Some(1), "末项右缘内应命中末项");
        assert_eq!(b.index_at(total), None, "整条右侧之外不命中");

        // 两项应分别落在各自区段，且分界点唯一。
        let first_w = b.layout.borrow()[1].0;
        assert_eq!(b.index_at(first_w - 1), Some(0));
        assert_eq!(b.index_at(first_w), Some(1));
    }

    #[test]
    fn tab_shrinks_padding_only_when_bar_overflows() {
        let style = Style::default();
        let mut te = NullTextEngine;
        let b = bar(&["设置", "控件", "表格", "图片", "历史", "关于"], signal(0));
        let natural = b.measure(Size::ZERO, &style, &mut te).w;
        let m = b.metrics(&style, |s, ts| te.measure(s, ts, None));

        assert_eq!(
            TabBar::shrink_pad(&m, natural),
            TAB_PAD_X,
            "放得下时用自然内边距"
        );
        let squeezed = TabBar::shrink_pad(&m, natural - 60);
        assert!(
            (TAB_PAD_MIN..TAB_PAD_X).contains(&squeezed),
            "放不下时收缩且不越过下限，实得 {squeezed}"
        );
        let laid = TabBar::lay_out(&m, squeezed);
        let total = laid.last().map_or(0, |(x, w)| x + w);
        assert!(total <= natural - 60, "收缩后应放得进可用宽，实得 {total}");
    }

    #[test]
    fn tab_arrow_keys_cycle_selection() {
        let n = 3;
        assert_eq!(TabBar::key_target(Key::Right, 0, n), Some(1));
        assert_eq!(
            TabBar::key_target(Key::Right, 2, n),
            Some(0),
            "末项右移回首项"
        );
        assert_eq!(
            TabBar::key_target(Key::Left, 0, n),
            Some(2),
            "首项左移到末项"
        );
        assert_eq!(TabBar::key_target(Key::Left, 2, n), Some(1));
        assert_eq!(TabBar::key_target(Key::Home, 2, n), Some(0));
        assert_eq!(TabBar::key_target(Key::End, 0, n), Some(2));
        assert_eq!(TabBar::key_target(Key::Enter, 1, n), Some(1));
        assert_eq!(
            TabBar::key_target(Key::Up, 1, n),
            None,
            "上下键不归标签条处理"
        );
    }

    #[test]
    fn tab_selected_index_clamps_to_item_count() {
        let b = bar(&["A", "B"], signal(9));
        assert_eq!(b.selected(), 1, "越界信号应钳到末项");
    }
}
