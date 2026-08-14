//! 下拉选择控件 Dropdown：显示当前选项 + 点击弹出浮层列表选择。
//!
//! 复用宿主层浮层机制（与右键菜单同源）：点击经 `EventCtx::show_menu` 请求弹出，
//! 每个选项的动作是设置绑定的 `Rc<Cell<usize>>` 选中索引（`MenuAction::Run` 闭包）。
//!
//! 富内容（副标题/徽章/可点击尾随图标）走 [`DropdownItem`] + `with_items`/`with_items_reactive`；
//! 纯文本场景仍用原有 `Vec<String>` 入口，两者内部分别存储、互不影响。

use std::cell::Cell;
use std::rc::Rc;

use crate::anim::{Easing, Transition};
use crate::core::{EventCtx, Widget};
use crate::event::{Event, Key, MenuItem, PointerKind};
use crate::geometry::{Color, Rect, Size};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::spec::Align;
use crate::style::Style;
use crate::text::TextEngine;
use crate::theme::Intent;

const PAD_X: i32 = 12;
const CHEVRON_W: i32 = 18;
/// 收起态徽章胶囊左右内边距/高度/与文本间距（与 `app.rs` 菜单尾随徽章同规格）。
const BADGE_PAD_X: i32 = 8;
const BADGE_H: i32 = 20;
const BADGE_GAP: i32 = 8;

/// 富内容选项：主文本 + 可选第二行说明 + 可选尾随徽章（纯展示）+ 可选尾随可点击图标。
///
/// 与 [`crate::event::MenuItem`] 一样标了 `#[non_exhaustive]`：两者常出现在同一份
/// 菜单代码里，只有一个封住字面量构造反而是新的记忆负担。请用 [`DropdownItem::new`]
/// 加链式设置器构造；四个字段都有对应设置器，日后加字段不会波及调用方。
#[derive(Clone)]
#[non_exhaustive]
pub struct DropdownItem {
    pub label: String,
    pub subtitle: Option<String>,
    pub badge: Option<(String, Intent)>,
    pub trailing_icon: Option<(String, crate::event::MenuActionFn)>,
}

impl DropdownItem {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            subtitle: None,
            badge: None,
            trailing_icon: None,
        }
    }
    /// 第二行小字说明（展开态渲染为两行）。
    pub fn subtitle(mut self, s: impl Into<String>) -> Self {
        self.subtitle = Some(s.into());
        self
    }
    /// 尾随徽章胶囊（纯展示，展开态与收起态当前项均显示）。
    pub fn badge(mut self, text: impl Into<String>, intent: Intent) -> Self {
        self.badge = Some((text.into(), intent));
        self
    }
    /// 尾随可独立点击的图标（仅展开态列表项）：点击只触发 `on_click`，不选中该项。
    /// 回调签名同 [`MenuItem::run`](crate::event::MenuItem::run) 的动作（`ctx` 在前，
    /// `Fn` 是因为项要被克隆进浮层）。
    pub fn trailing_icon(
        mut self,
        icon: impl Into<String>,
        on_click: impl Fn(&mut EventCtx) + 'static,
    ) -> Self {
        self.trailing_icon = Some((icon.into(), Rc::new(on_click)));
        self
    }
}

impl From<String> for DropdownItem {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}
impl From<&str> for DropdownItem {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

/// 绘制 [`Dropdown`] 与 [`CheckMenu`] 共用的字段外壳：圆角底 + hover/focus 补间边框
/// + 右侧 ▼ 箭头。返回正文文字色（禁用态已折算为 `text_disabled`）。
///
/// 两者外观必须一致——它们在同一排工具栏里并列，一个是"选一项"、一个是"开几个开关"，
/// 边框圆角差半像素都会看出来。故共用同一段绘制，而不是各画各的。
fn paint_field_chrome(
    bounds: Rect,
    focused: bool,
    hover: bool,
    enabled: bool,
    canvas: &mut dyn Canvas,
    border_anim: &Cell<Transition<Color>>,
    primed: &Cell<bool>,
) -> Color {
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
    // 边框色补间：hover/focus 高亮淡变；首帧靠 primed 落定。
    let target_border = if focused || hover {
        dd.border_focus(pal)
    } else {
        dd.border(pal)
    };
    let mut ba = border_anim.get();
    if !primed.get() {
        ba = Transition::new(target_border);
        primed.set(true);
    } else if ba.target() != target_border {
        ba.retarget(target_border, th.anim.fast(), Easing::EaseOut);
    }
    let border = ba.animate();
    border_anim.set(ba);
    let bw = if focused {
        th.metrics.border_width_focus.to_logical(canvas.dpi_scale())
    } else {
        th.metrics.border_width.to_logical(canvas.dpi_scale())
    };
    canvas.stroke_round_rect(x, y, w, h, corner, bw, &Paint::fill(border));

    // 右侧下拉箭头 ▼（两段线）。
    let cx = bounds.x as f32 + bounds.w as f32 - PAD_X as f32 - CHEVRON_W as f32 / 2.0;
    let cy = bounds.y as f32 + bounds.h as f32 / 2.0;
    let p = Paint::fill(chevron);
    canvas.draw_line(cx - 4.0, cy - 2.0, cx, cy + 3.0, 1.6, &p);
    canvas.draw_line(cx, cy + 3.0, cx + 4.0, cy - 2.0, 1.6, &p);

    text_color
}

/// 一列选项的来源：构建期定下的静态表，或绑定的信号。
///
/// 与 [`TextContent`](crate::ui::TextContent) 同一套心智——动态性是**字段的类型**，
/// 不是控件的类型，所以 `Dropdown` 只有一个、读取路径只有一条。
///
/// 静态表刻意**不**包一层信号：那样等于为一个永不变的常量长期占住一个运行时槽位，
/// 而它既没有 owner 也没人会去回收（见 `crate::signal` 的所有权模型）。
enum Options<T> {
    Static(Vec<T>),
    Bound(Signal<Vec<T>>),
}

impl<T: Clone + 'static> Options<T> {
    /// 借用当前选项表（静态表零拷贝，信号经运行时借出）。
    fn with<R>(&self, f: impl FnOnce(&Vec<T>) -> R) -> R {
        match self {
            Options::Static(v) => f(v),
            Options::Bound(sig) => sig.with(f),
        }
    }
    /// 克隆一份当前选项表（构建菜单项时要按值消耗）。
    fn get(&self) -> Vec<T> {
        self.with(|v| v.clone())
    }
}

/// 选项存储：纯文本（原有 `Vec<String>` 入口）或富内容（`DropdownItem`）。
enum OptionSource {
    Plain(Options<String>),
    Rich(Options<DropdownItem>),
}

pub struct Dropdown {
    options: OptionSource,
    selected: Signal<usize>,
    hover: bool,
    /// 边框色补间（hover/focus 高亮淡变）；首帧靠 `primed` 落定。
    border_anim: Cell<Transition<Color>>,
    primed: Cell<bool>,
}

impl Dropdown {
    pub fn new(options: Vec<String>, selected: Signal<usize>) -> Self {
        Self::from_source(OptionSource::Plain(Options::Static(options)), selected)
    }

    /// 响应式选项：选项列表绑定外部 `Signal<Vec<String>>`，变更即重新测量/渲染。
    pub fn new_reactive(options: Signal<Vec<String>>, selected: Signal<usize>) -> Self {
        Self::from_source(OptionSource::Plain(Options::Bound(options)), selected)
    }

    /// 富内容选项（副标题/徽章/尾随图标）。
    pub fn with_items(items: Vec<DropdownItem>, selected: Signal<usize>) -> Self {
        Self::from_source(OptionSource::Rich(Options::Static(items)), selected)
    }

    /// 响应式富内容选项：绑定外部 `Signal<Vec<DropdownItem>>`。
    pub fn with_items_reactive(items: Signal<Vec<DropdownItem>>, selected: Signal<usize>) -> Self {
        Self::from_source(OptionSource::Rich(Options::Bound(items)), selected)
    }

    fn from_source(options: OptionSource, selected: Signal<usize>) -> Self {
        Self {
            options,
            selected,
            hover: false,
            border_anim: Cell::new(Transition::new(Color::rgba(0, 0, 0, 0))),
            primed: Cell::new(false),
        }
    }

    fn current(&self) -> String {
        match &self.options {
            OptionSource::Plain(opts) => opts.with(|list| {
                let i = self.selected.get().min(list.len().saturating_sub(1));
                list.get(i).cloned().unwrap_or_default()
            }),
            OptionSource::Rich(items) => items.with(|list| {
                let i = self.selected.get().min(list.len().saturating_sub(1));
                list.get(i).map(|it| it.label.clone()).unwrap_or_default()
            }),
        }
    }

    /// 当前选中项的尾随徽章（仅富内容来源；纯文本来源恒为 `None`）。
    fn current_badge(&self) -> Option<(String, Intent)> {
        match &self.options {
            OptionSource::Plain(_) => None,
            OptionSource::Rich(items) => items.with(|list| {
                let i = self.selected.get().min(list.len().saturating_sub(1));
                list.get(i).and_then(|it| it.badge.clone())
            }),
        }
    }

    /// 弹出浮层列表：宽度对齐控件，每项点击设置选中索引。
    fn open(&self, ctx: &mut EventCtx) {
        let b = ctx.bounds();
        let cur = self.selected.get();
        let items: Vec<MenuItem> = match &self.options {
            OptionSource::Plain(opts) => {
                let list = opts.get();
                if list.is_empty() {
                    return;
                }
                list.into_iter()
                    .enumerate()
                    .map(|(i, o)| {
                        let sel = self.selected;
                        MenuItem::run(o, move |_ctx| sel.set(i), i == cur)
                    })
                    .collect()
            }
            OptionSource::Rich(items_sig) => {
                let list = items_sig.get();
                if list.is_empty() {
                    return;
                }
                list.into_iter()
                    .enumerate()
                    .map(|(i, it)| {
                        let sel = self.selected;
                        let mut mi = MenuItem::run(it.label, move |_ctx| sel.set(i), i == cur);
                        if let Some(sub) = it.subtitle {
                            mi = mi.subtitle(sub);
                        }
                        if let Some((text, intent)) = it.badge {
                            mi = mi.badge(text, intent);
                        }
                        if let Some((icon, cb)) = it.trailing_icon {
                            mi = mi.trailing_icon(icon, move |ctx| (*cb)(ctx));
                        }
                        mi
                    })
                    .collect()
            }
        };
        ctx.show_dropdown_menu(b, items);
    }
}

impl Widget for Dropdown {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let mut w = 0;
        match &self.options {
            OptionSource::Plain(opts) => opts.with(|list| {
                for o in list {
                    w = w.max(text.measure(o, &crate::text::TextStyle::of(style), None).w);
                }
            }),
            OptionSource::Rich(items) => items.with(|list| {
                for it in list {
                    let mut iw = text
                        .measure(&it.label, &crate::text::TextStyle::of(style), None)
                        .w;
                    if let Some((btext, _)) = &it.badge {
                        iw += text
                            .measure(btext, &crate::text::TextStyle::new(12.0), None)
                            .w
                            + 2 * BADGE_PAD_X
                            + BADGE_GAP;
                    }
                    w = w.max(iw);
                }
            }),
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
        let pal = &th.palette;
        let text_color = paint_field_chrome(
            bounds,
            focused,
            self.hover,
            enabled,
            canvas,
            &self.border_anim,
            &self.primed,
        );

        // 当前选中项的尾随徽章（若有）：贴 chevron 左侧，文本区相应收窄。
        let badge = self.current_badge();
        let badge_w = badge
            .as_ref()
            .map(|(text, _)| {
                canvas
                    .measure_text(text, &crate::text::TextStyle::new(12.0))
                    .w
                    + 2 * BADGE_PAD_X
            })
            .unwrap_or(0);
        if let Some((text, intent)) = &badge {
            let (fill, fg) = intent.badge_colors(pal);
            let br = Rect::new(
                bounds.x + bounds.w - PAD_X - CHEVRON_W - badge_w,
                bounds.y + (bounds.h - BADGE_H) / 2,
                badge_w,
                BADGE_H,
            );
            canvas.fill_round_rect(
                br.x as f32,
                br.y as f32,
                br.w as f32,
                br.h as f32,
                999.0,
                &Paint::fill(fill),
            );
            canvas.draw_text(
                text,
                br,
                fg,
                Align::Center,
                &crate::text::TextStyle::new(12.0),
            );
        }

        // 当前选项文本（左侧，留出右侧 chevron 与徽章）。
        let badge_reserve = if badge_w > 0 { badge_w + BADGE_GAP } else { 0 };
        let tr = Rect::new(
            bounds.x + PAD_X,
            bounds.y,
            bounds.w - 2 * PAD_X - CHEVRON_W - badge_reserve,
            bounds.h,
        );
        let cur = self.current();
        canvas.draw_text(
            &cur,
            tr,
            text_color,
            Align::Start,
            &crate::text::TextStyle::of(style),
        );
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

/// 收起态文案生成器：入参是已开启的开关项标签（按声明顺序）。见 [`CheckMenu`]。
/// 生成器（每次渲染现算文案）而非事件回调，故无 `ctx`、且是 `Fn`（要反复调用）。
type SummaryFn = Rc<dyn Fn(&[&str]) -> String>;

/// 开关项翻转后的通知：`ctx` 在首位，其后是已生效的新值。见 [`CheckMenuItem::on_change`]。
type CheckChangeFn = Rc<dyn Fn(&mut EventCtx, bool)>;

/// [`CheckMenu`] 的一项：开关项 / 普通动作项 / 分隔线。
///
/// 枚举与各变体都标了 `#[non_exhaustive]`，理由同 [`DropdownItem`]：菜单项类型会随
/// 需求长新变体和新字段。请用 [`CheckMenuItem::check`] / [`CheckMenuItem::action`] /
/// [`CheckMenuItem::separator`] 三个构造器加 [`CheckMenuItem::on_change`] /
/// [`CheckMenuItem::enabled`] 两个设置器构造，它们覆盖了全部字段。
#[derive(Clone)]
#[non_exhaustive]
pub enum CheckMenuItem {
    /// 开关项：绑定 `Signal<bool>`，点击原地翻转且**菜单不关闭**，可连点多个。
    #[non_exhaustive]
    Check {
        label: String,
        state: Signal<bool>,
        /// 翻转后通知（收到的是**新值**，默认翻转已经执行完）。用于落盘等副作用。
        on_change: Option<CheckChangeFn>,
        enabled: bool,
    },
    /// 普通动作项：点击执行并关闭菜单（与右键菜单的项同语义）。
    #[non_exhaustive]
    Action {
        label: String,
        on_click: crate::event::MenuActionFn,
        enabled: bool,
    },
    /// 分隔线（不可命中）。
    Separator,
}

impl CheckMenuItem {
    /// 开关项：点击翻转 `state`，菜单保持展开。
    pub fn check(label: impl Into<String>, state: Signal<bool>) -> Self {
        Self::Check {
            label: label.into(),
            state,
            on_change: None,
            enabled: true,
        }
    }
    /// 动作项：点击执行 `f` 并关闭菜单。回调签名同
    /// [`MenuItem::run`](crate::event::MenuItem::run) 的动作（`ctx` 在前，`Fn` 是因为
    /// 项要被克隆进浮层、粘滞时还要重建后再执行）。
    pub fn action(label: impl Into<String>, f: impl Fn(&mut EventCtx) + 'static) -> Self {
        Self::Action {
            label: label.into(),
            on_click: Rc::new(f),
            enabled: true,
        }
    }
    /// 分隔线。
    pub fn separator() -> Self {
        Self::Separator
    }
    /// 开关翻转后的通知（仅 `Check` 项有效）。回调收到新值，**不需要**自己再 `set`
    /// ——与 `CheckBox::on_toggle`「取代默认翻转」不同，这里是翻转之后的副作用钩子。
    /// 签名 `Fn(&mut EventCtx, bool)`：`ctx` 恒在首位，新值跟在后面。`Fn` 而非 `FnMut`
    /// 的理由同 [`CheckMenuItem::action`]（项被克隆进浮层）。
    pub fn on_change(mut self, f: impl Fn(&mut EventCtx, bool) + 'static) -> Self {
        if let Self::Check { on_change, .. } = &mut self {
            *on_change = Some(Rc::new(f));
        }
        self
    }
    /// 设置启用态（禁用项变灰且不可点击；分隔线忽略）。
    pub fn enabled(mut self, v: bool) -> Self {
        match &mut self {
            Self::Check { enabled, .. } | Self::Action { enabled, .. } => *enabled = v,
            Self::Separator => {}
        }
        self
    }
}

/// 下拉式复选菜单：外观同 [`Dropdown`]（当前项即入口），面板是菜单，项可单独开关。
///
/// **默认点击即关闭**，与右键菜单/单选下拉一致——菜单的通行惯例是「点一下、做一件事、
/// 退场」，多数开关也确实一次只改一个。要连改多个再用
/// [`set_stay_open`](Self::set_stay_open) 显式打开粘滞，此时开关项点了不关、
/// 点面板外才收起（动作项无论如何都关闭，它本就是「执行完就退场」的语义）。
///
/// 与 `Dropdown` 的语义分工：`Dropdown` 是"选一项"（选中即改变唯一的选择），
/// `CheckMenu` 是"开几个开关"（每项是独立的一位）。用 `Dropdown` 模拟复选需要
/// 哨兵项 + 索引复位的绕法，且入口文案会被最后点的那一项顶替。
///
/// 收起态默认恒显示 `title`；用 [`set_summary`](Self::set_summary) 可改为按已开项
/// 生成文案。摘要会改变控件宽度，故用摘要时建议在 `Element` 上显式 `.width(..)` 固定。
pub struct CheckMenu {
    title: String,
    items: Rc<Vec<CheckMenuItem>>,
    /// 收起态文案生成器：入参是**已开启**的开关项标签（按声明顺序）。
    summary: Option<SummaryFn>,
    /// 粘滞：开关项点击后菜单保持展开、可连点。默认 false（点击即关，同普通菜单）。
    stay_open: bool,
    hover: bool,
    /// 边框色补间（与 Dropdown 同源，见 [`paint_field_chrome`]）。
    border_anim: Cell<Transition<Color>>,
    primed: Cell<bool>,
}

impl CheckMenu {
    pub fn new(title: impl Into<String>, items: Vec<CheckMenuItem>) -> Self {
        Self {
            title: title.into(),
            items: Rc::new(items),
            summary: None,
            stay_open: false,
            hover: false,
            border_anim: Cell::new(Transition::new(Color::rgba(0, 0, 0, 0))),
            primed: Cell::new(false),
        }
    }

    /// 设置收起态文案生成器（见 [`CheckMenu`] 的宽度提示）。
    pub fn set_summary(&mut self, f: impl Fn(&[&str]) -> String + 'static) {
        self.summary = Some(Rc::new(f));
    }

    /// 粘滞开关：`true` 时开关项点击后菜单保持展开、可连点多个，点面板外才收起。
    /// 默认 `false`（点击即关）。整菜单统一，不做逐项差异——同一个面板里
    /// 有的项点了关、有的不关，用户没法预期下一次点击会发生什么。
    pub fn set_stay_open(&mut self, on: bool) {
        self.stay_open = on;
    }

    /// 收起态显示文本：无 summary 恒为标题，有则按当前已开项生成。
    fn display_text(&self) -> String {
        let Some(f) = &self.summary else {
            return self.title.clone();
        };
        let on: Vec<&str> = self
            .items
            .iter()
            .filter_map(|it| match it {
                CheckMenuItem::Check { label, state, .. } if state.get() => Some(label.as_str()),
                _ => None,
            })
            .collect();
        f(&on)
    }

    /// 把声明的项翻译成浮层菜单项：`checked` 取 Signal 当前值，`stay_open` 时开关项标记
    /// 粘滞。该函数即 `MenuRequest::rebuild`，粘滞下每次点击后重跑一遍刷新勾选态；
    /// 非粘滞下菜单点完就关，rebuild 只在下次展开时用到。
    fn build_menu_items(items: &[CheckMenuItem], stay_open: bool) -> Vec<MenuItem> {
        items
            .iter()
            .map(|it| match it {
                CheckMenuItem::Check {
                    label,
                    state,
                    on_change,
                    enabled,
                } => {
                    let (st, cb) = (*state, on_change.clone());
                    let mut mi = MenuItem::run(
                        label.clone(),
                        move |ctx| {
                            let v = !st.get();
                            st.set(v);
                            if let Some(f) = &cb {
                                f(ctx, v);
                            }
                        },
                        st.get(),
                    );
                    if stay_open {
                        mi = mi.stay_open();
                    }
                    mi.enabled(*enabled)
                }
                CheckMenuItem::Action {
                    label,
                    on_click,
                    enabled,
                } => {
                    let f = on_click.clone();
                    MenuItem::run(label.clone(), move |ctx| f(ctx), false).enabled(*enabled)
                }
                CheckMenuItem::Separator => MenuItem::separator(),
            })
            .collect()
    }

    fn open(&self, ctx: &mut EventCtx) {
        let b = ctx.bounds();
        let items = self.items.clone();
        let sticky = self.stay_open;
        ctx.show_check_menu(b, Rc::new(move || Self::build_menu_items(&items, sticky)));
    }
}

impl Widget for CheckMenu {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        // 取标题与当前摘要中较宽者：摘要为空时回落到标题，宽度不至于塌缩。
        let ts = crate::text::TextStyle::of(style);
        let w = text
            .measure(&self.title, &ts, None)
            .w
            .max(text.measure(&self.display_text(), &ts, None).w);
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
        let text_color = paint_field_chrome(
            bounds,
            focused,
            self.hover,
            enabled,
            canvas,
            &self.border_anim,
            &self.primed,
        );
        let tr = Rect::new(
            bounds.x + PAD_X,
            bounds.y,
            bounds.w - 2 * PAD_X - CHEVRON_W,
            bounds.h,
        );
        canvas.draw_text(
            &self.display_text(),
            tr,
            text_color,
            Align::Start,
            &crate::text::TextStyle::of(style),
        );
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
                        // 同 Dropdown：宿主打开后独占指针，控件收不到 Leave。
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

    /// 供 `Element::summary()` 向下转型配置（默认实现返回 None，不实现则 builder 失效）。
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::signal;

    /// 跑一个菜单项的动作。动作收 `&mut EventCtx`，宿主经 `Tree::run_detached` 借出，
    /// 这里借一棵只有根节点的空树复现同一时机。
    fn run_action(item: &MenuItem) {
        let mut tree = crate::core::Tree::new();
        let id = crate::ui::Element::col().build(&mut tree);
        tree.root = Some(id);
        match &item.action {
            crate::event::MenuAction::Run(f) => {
                tree.run_detached(id, |ctx| f(ctx));
            }
            _ => panic!("应为 Run 动作"),
        }
    }

    #[test]
    fn reactive_dropdown_reflects_option_signal() {
        let opts = signal(vec!["甲".to_string(), "乙".to_string()]);
        let sel = signal(1usize);
        let dd = Dropdown::new_reactive(opts, sel);
        assert_eq!(dd.current(), "乙");
        // 选项异步更新后，current 立即反映新列表（按同一索引）。
        opts.set(vec!["X".to_string(), "Y".to_string(), "Z".to_string()]);
        assert_eq!(dd.current(), "Y");
        sel.set(2);
        assert_eq!(dd.current(), "Z");
    }

    #[test]
    fn dropdown_current_clamps_when_index_overflows() {
        let opts = signal(vec!["a".to_string(), "b".to_string()]);
        let sel = signal(5usize); // 越界
        let dd = Dropdown::new_reactive(opts, sel);
        assert_eq!(dd.current(), "b"); // 钳到末项
        opts.set(vec![]); // 空列表
        assert_eq!(dd.current(), ""); // 不 panic，返回空
    }

    #[test]
    fn check_menu_items_track_signal_and_close_by_default() {
        let a = signal(false);
        let b = signal(true);
        let items = vec![
            CheckMenuItem::check("甲", a),
            CheckMenuItem::separator(),
            CheckMenuItem::check("乙", b),
            CheckMenuItem::action("执行", |_ctx| {}),
        ];
        let built = CheckMenu::build_menu_items(&items, false);
        assert_eq!(built.len(), 4);
        assert!(!built[0].stay_open, "默认点击即关，与右键菜单/单选下拉一致");
        assert!(!built[0].checked);
        assert!(built[1].separator);
        assert!(built[2].checked, "checked 取自 Signal 当前值");
        assert!(!built[3].stay_open, "动作项恒为点击即关");

        // 触发开关项的动作 → Signal 翻转；重建后 checked 随之刷新（rebuild 的作用）。
        run_action(&built[0]);
        assert!(a.get());
        assert!(CheckMenu::build_menu_items(&items, false)[0].checked);
    }

    #[test]
    fn check_menu_stay_open_marks_only_check_items() {
        // 粘滞是整菜单开关，但只作用于开关项——动作项本就是"执行完退场"的语义，
        // 粘滞它会让菜单在动作已经发生后还赖着不走。
        let items = vec![
            CheckMenuItem::check("甲", signal(false)),
            CheckMenuItem::separator(),
            CheckMenuItem::action("执行", |_ctx| {}),
        ];
        let built = CheckMenu::build_menu_items(&items, true);
        assert!(built[0].stay_open, "开关项在粘滞模式下点了不关");
        assert!(!built[2].stay_open, "动作项即使在粘滞模式下也关闭");

        let mut m = CheckMenu::new("t", vec![CheckMenuItem::check("甲", signal(false))]);
        assert!(!m.stay_open, "构造默认非粘滞");
        m.set_stay_open(true);
        assert!(m.stay_open);
    }

    #[test]
    fn check_menu_on_change_receives_new_value_after_default_flip() {
        // on_change 是「翻转之后的副作用钩子」，与 CheckBox::on_toggle「取代默认翻转」
        // 语义不同：调用方不必自己 set，回调收到的就是已生效的新值。
        let s = signal(false);
        let seen = Rc::new(Cell::new(None::<bool>));
        let sink = seen.clone();
        let items = vec![CheckMenuItem::check("x", s).on_change(move |_ctx, v| sink.set(Some(v)))];
        let built = CheckMenu::build_menu_items(&items, false);
        run_action(&built[0]);
        assert_eq!(seen.get(), Some(true));
        assert!(s.get(), "默认翻转已执行，回调无需自己 set");
    }

    #[test]
    fn check_menu_display_text_defaults_to_title() {
        let s = signal(true);
        let mut m = CheckMenu::new("列表显示", vec![CheckMenuItem::check("隐藏未启用", s)]);
        assert_eq!(m.display_text(), "列表显示", "无 summary 时恒为标题");
        m.set_summary(|on| match on.len() {
            0 => "列表显示".to_string(),
            _ => format!("列表显示 ({})", on.join("、")),
        });
        assert_eq!(m.display_text(), "列表显示 (隐藏未启用)");
        s.set(false);
        assert_eq!(m.display_text(), "列表显示", "全关时回落");
    }

    #[test]
    fn measure_empty_list_is_chrome_only_width() {
        use crate::text::NullTextEngine;
        let dd = Dropdown::new_reactive(signal(vec![]), signal(0usize));
        let style = Style::default();
        let mut te = NullTextEngine;
        // 空列表：宽度仅为左右内边距 + 箭头区（无选项文本贡献）。
        let w = dd.measure(Size::ZERO, &style, &mut te).w;
        assert_eq!(w, 2 * PAD_X + CHEVRON_W);
    }
}
