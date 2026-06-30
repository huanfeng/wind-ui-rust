//! 命令式 Builder：单一 `Element` 类型贯穿所有控件，链式构建后一次性落入 `Tree`。
//!
//! 容器（`col`/`row`/`stack`）与叶子（`leaf`、Phase 2 起的 `label` 等）都返回
//! `Element`，`.child(...)` 接受任意 `Element`，构建时递归插入 arena。

pub mod containers;
pub mod dyn_list;
pub mod image;
pub mod inputs;
pub mod link;
pub mod list;
pub mod nav;
pub mod progress;
pub mod segmented;
pub mod select;
pub mod stepper;
pub mod window_buttons;

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use crate::anim::{Easing, Transition};
use crate::core::{ClickFn, DropFn, EmptyWidget, EventCtx, Layout, Node, NodeId, Tree, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Color, Insets, Rect, Size};
use crate::render::image::{Fit, Image, VisualState};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::spec::{Align, Axis, Dimension};
use crate::style::Style;
use crate::text::TextEngine;
use crate::theme::{Intent, IntentColors};

pub use image::{ImageContent, ImageView};
pub use inputs::{CheckBox, CheckBoxSize, RadioButton, Slider, Switch, TextInput};
pub use link::Link;
pub use list::ListRow;
pub use nav::{AccordionHeader, CollapsibleHeader, ExpandState, NavRow};
pub use progress::ProgressBar;
pub use segmented::SegmentedControl;
pub use select::Dropdown;
pub use stepper::Stepper;
pub use window_buttons::{WindowButton, WindowButtonKind};

/// 图标与文字之间的间距（Button 等）。
const ICON_GAP: i32 = 6;

/// 表格单元格内边距（横/纵，px）与可点击单元格高亮圆角。内边距在单元格内部，
/// 使可点击单元格填满整格、hover 高亮覆盖整格（而非仅贴着文字）。
const TABLE_CELL_PAD_X: i32 = 14;
const TABLE_CELL_PAD_Y: i32 = 9;
const TABLE_HEADER_PAD_Y: i32 = 10;
const TABLE_CELL_CORNER: f32 = 4.0;

/// 文本溢出时的省略方式。对 `Label`/`DynLabel` 生效；配合 `.max_lines(1)` 使用最为常见。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Truncate {
    #[default]
    None, // 裁剪（默认行为）
    End,    // text…（最常用）
    Start,  // …text
    Middle, // te…xt
}

/// 文本控件（`Label`/`DynLabel`）前景色：启用取样式解析色；禁用统一降为 `text_disabled`，
/// 使整行（标签 + 说明）随容器禁用一并置灰（控件自身早已响应禁用，此处补齐文字部分）。
///
/// 契约提示：这是框架级语义——禁用子树内的**任何**文本一律置灰，暂无单节点豁免。
/// 若将来出现"禁用整块、但某段说明/警示文字需保持原色"的诉求，可在 `Style` 增设
/// `keep_fg_when_disabled` 之类标志，而非在此处特判。
fn text_fg(enabled: bool, style: &Style, theme: &crate::theme::Theme) -> Color {
    if enabled {
        style.resolved_fg(theme)
    } else {
        theme.palette.text_disabled
    }
}

/// 文本叶子控件。
pub struct Label {
    text: String,
    /// 最大显示行数；超出部分按 `truncate` 处理（`None` = 不限）。
    pub max_lines: Option<usize>,
    /// 溢出省略方式（仅 `max_lines = Some(1)` 单行时精确截断；多行仅高度裁剪）。
    pub truncate: Truncate,
    /// 截断结果缓存 `(content_w, fsize_bits) → 截断串`；text 不可变故不入 key。
    trunc_cache: RefCell<Option<(i32, u32, String)>>,
}

impl Label {
    pub fn new(text: String) -> Self {
        Self {
            text,
            max_lines: None,
            truncate: Truncate::None,
            trunc_cache: RefCell::new(None),
        }
    }

    /// 计算截断后显示串（含省略号）；结果会被 paint 缓存，通常只算一次。
    fn compute_truncated(
        &self,
        canvas: &mut dyn Canvas,
        family: Option<&str>,
        fsize: f32,
        avail_w: i32,
    ) -> String {
        let total_w = canvas.measure_text(&self.text, family, fsize).w;
        if total_w <= avail_w {
            return self.text.clone();
        }
        let ew = canvas.measure_text("…", family, fsize).w;
        let avail = (avail_w - ew).max(0);
        let chars: Vec<char> = self.text.chars().collect();
        let n = chars.len();
        // 前缀累计宽度表（O(N) 次 measure，之后 partition_point 二分）。
        let mut widths = vec![0i32; n + 1];
        let mut acc = String::new();
        for (i, &c) in chars.iter().enumerate() {
            acc.push(c);
            widths[i + 1] = canvas.measure_text(&acc, family, fsize).w;
        }
        match self.truncate {
            Truncate::End => {
                // partition_point 返回第一个 > avail 的下标，该位置的字符本身已超宽，
                // 需 -1 取最后一个能放下的字符数。
                let cut = widths
                    .partition_point(|&w| w <= avail)
                    .saturating_sub(1)
                    .min(n);
                format!("{}…", chars[..cut].iter().collect::<String>())
            }
            Truncate::Start => {
                // partition_point(w < threshold) 返回第一个 >= threshold 的下标，
                // 即从该字符起的后缀宽度 ≤ avail，此处无 off-by-one。
                let threshold = total_w - avail;
                let cut = widths.partition_point(|&w| w < threshold).min(n);
                format!("…{}", chars[cut..].iter().collect::<String>())
            }
            Truncate::Middle => {
                let lcut = widths
                    .partition_point(|&w| w <= avail / 2)
                    .saturating_sub(1)
                    .min(n);
                let right_avail = (avail - widths[lcut]).max(0);
                let threshold = total_w - right_avail;
                let rcut = widths.partition_point(|&w| w < threshold).min(n);
                let left: String = chars[..lcut].iter().collect();
                let right: String = chars[rcut..].iter().collect();
                format!("{left}…{right}")
            }
            Truncate::None => unreachable!(),
        }
    }
}

impl Widget for Label {
    fn measure(&self, avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        // 在可用宽度内换行：宽度受限时折行，宽松时单行。
        // 已知限制：换行准确仅保证于显式宽度的 Label（width/width_match/weight）；
        // 纯 Wrap 宽度的多行 Label，draw 会在收敛后的窄宽重新换行，可能与 measure 行数不符。
        let max_w = if avail.w > 0 {
            Some(avail.w as f32)
        } else {
            None
        };
        let full = text.measure(
            &self.text,
            style.font_family.as_deref(),
            style.font_size,
            max_w,
        );
        if let Some(max_n) = self.max_lines {
            let line_h = text
                .measure("Ay", style.font_family.as_deref(), style.font_size, None)
                .h
                .max(1);
            Size::new(full.w, full.h.min(max_n as i32 * line_h))
        } else {
            full
        }
    }
    fn paint(
        &self,
        _bounds: Rect,
        content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let family = style.font_family.as_deref();
        let fsize = style.font_size;
        // 禁用态：文字降为 text_disabled，使整行（含标签/说明）随容器禁用统一置灰。
        let fg = text_fg(enabled, style, &crate::theme::current());

        // max_lines：计算限高矩形；DirectWrite 高度始终为 f32::MAX，必须用 clip_rect 裁剪。
        let (paint_rect, need_clip) = if let Some(max_n) = self.max_lines {
            let line_h = canvas.measure_text("Ay", family, fsize).h.max(1);
            let clipped = Rect::new(
                content.x,
                content.y,
                content.w,
                content.h.min(max_n as i32 * line_h),
            );
            (clipped, true)
        } else {
            (content, false)
        };

        if need_clip {
            canvas.save();
            canvas.clip_rect(paint_rect);
        }

        // 单行省略（max_lines = 1 且配置了截断模式）。
        if self.truncate != Truncate::None && self.max_lines == Some(1) && !self.text.is_empty() {
            let key_w = content.w;
            let key_f = fsize.to_bits();
            let cached: Option<String> = {
                let c = self.trunc_cache.borrow();
                c.as_ref().and_then(|(cw, cf, s)| {
                    if *cw == key_w && *cf == key_f {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
            };
            let text_str = if let Some(s) = cached {
                s
            } else {
                let s = self.compute_truncated(canvas, family, fsize, content.w);
                *self.trunc_cache.borrow_mut() = Some((key_w, key_f, s.clone()));
                s
            };
            canvas.draw_text(
                &text_str,
                paint_rect,
                fg,
                style.text_align,
                family,
                fsize,
            );
        } else {
            canvas.draw_text(
                &self.text,
                paint_rect,
                fg,
                style.text_align,
                family,
                fsize,
            );
        }

        if need_clip {
            canvas.restore();
        }
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// 动态文本标签：绑定 `Signal<String>`，只读显示，内容随绑定变化而更新。
pub struct DynLabel {
    text: Signal<String>,
    pub max_lines: Option<usize>,
    pub truncate: Truncate,
    /// 截断缓存 `(text_clone, content_w, fsize_bits) → 截断串`。
    trunc_cache: RefCell<Option<(String, i32, u32, String)>>,
}

impl DynLabel {
    pub fn new(text: Signal<String>) -> Self {
        Self {
            text,
            max_lines: None,
            truncate: Truncate::None,
            trunc_cache: RefCell::new(None),
        }
    }

    fn compute_truncated(
        &self,
        s: &str,
        canvas: &mut dyn Canvas,
        family: Option<&str>,
        fsize: f32,
        avail_w: i32,
    ) -> String {
        let total_w = canvas.measure_text(s, family, fsize).w;
        if total_w <= avail_w {
            return s.to_string();
        }
        let ew = canvas.measure_text("…", family, fsize).w;
        let avail = (avail_w - ew).max(0);
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        let mut widths = vec![0i32; n + 1];
        let mut acc = String::new();
        for (i, &c) in chars.iter().enumerate() {
            acc.push(c);
            widths[i + 1] = canvas.measure_text(&acc, family, fsize).w;
        }
        match self.truncate {
            Truncate::End => {
                let cut = widths
                    .partition_point(|&w| w <= avail)
                    .saturating_sub(1)
                    .min(n);
                format!("{}…", chars[..cut].iter().collect::<String>())
            }
            Truncate::Start => {
                let threshold = total_w - avail;
                let cut = widths.partition_point(|&w| w < threshold).min(n);
                format!("…{}", chars[cut..].iter().collect::<String>())
            }
            Truncate::Middle => {
                let lcut = widths
                    .partition_point(|&w| w <= avail / 2)
                    .saturating_sub(1)
                    .min(n);
                let right_avail = (avail - widths[lcut]).max(0);
                let threshold = total_w - right_avail;
                let rcut = widths.partition_point(|&w| w < threshold).min(n);
                let left: String = chars[..lcut].iter().collect();
                let right: String = chars[rcut..].iter().collect();
                format!("{left}…{right}")
            }
            Truncate::None => unreachable!(),
        }
    }
}

impl Widget for DynLabel {
    fn measure(&self, avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let s = self.text.get();
        let max_w = if avail.w > 0 {
            Some(avail.w as f32)
        } else {
            None
        };
        let full = text.measure(&s, style.font_family.as_deref(), style.font_size, max_w);
        if let Some(max_n) = self.max_lines {
            let line_h = text
                .measure("Ay", style.font_family.as_deref(), style.font_size, None)
                .h
                .max(1);
            Size::new(full.w, full.h.min(max_n as i32 * line_h))
        } else {
            full
        }
    }
    fn paint(
        &self,
        _bounds: Rect,
        content: Rect,
        _focused: bool,
        enabled: bool,
        canvas: &mut dyn Canvas,
        style: &Style,
    ) {
        let s = self.text.get();
        let family = style.font_family.as_deref();
        let fsize = style.font_size;
        // 禁用态：文字降为 text_disabled（与 Label 一致，随容器禁用置灰）。
        let fg = text_fg(enabled, style, &crate::theme::current());

        let (paint_rect, need_clip) = if let Some(max_n) = self.max_lines {
            let line_h = canvas.measure_text("Ay", family, fsize).h.max(1);
            let clipped = Rect::new(
                content.x,
                content.y,
                content.w,
                content.h.min(max_n as i32 * line_h),
            );
            (clipped, true)
        } else {
            (content, false)
        };

        if need_clip {
            canvas.save();
            canvas.clip_rect(paint_rect);
        }

        if self.truncate != Truncate::None && self.max_lines == Some(1) && !s.is_empty() {
            let key_w = content.w;
            let key_f = fsize.to_bits();
            let cached: Option<String> = {
                let c = self.trunc_cache.borrow();
                c.as_ref().and_then(|(ks, cw, cf, out)| {
                    if ks.as_str() == s.as_str() && *cw == key_w && *cf == key_f {
                        Some(out.clone())
                    } else {
                        None
                    }
                })
            };
            let text_str = if let Some(out) = cached {
                out
            } else {
                let out = self.compute_truncated(&s, canvas, family, fsize, content.w);
                *self.trunc_cache.borrow_mut() = Some((s.clone(), key_w, key_f, out.clone()));
                out
            };
            canvas.draw_text(
                &text_str,
                paint_rect,
                fg,
                style.text_align,
                family,
                fsize,
            );
        } else {
            canvas.draw_text(
                &s,
                paint_rect,
                fg,
                style.text_align,
                family,
                fsize,
            );
        }

        if need_clip {
            canvas.restore();
        }
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// 按钮三态。
#[derive(PartialEq, Eq, Clone, Copy)]
enum BtnState {
    Normal,
    Hover,
    Press,
}

/// 按钮尺寸变体：内边距大小。默认 `Medium`；`Small` 用于密集工具栏（添加/导入/导出等）。
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum ButtonSize {
    Small,
    Medium,
}

impl ButtonSize {
    /// (横向总内边距, 纵向总内边距)（px）。
    fn padding(self) -> (i32, i32) {
        match self {
            ButtonSize::Small => (20, 10),
            ButtonSize::Medium => (32, 18),
        }
    }
}

/// 交互按钮：hover/press 三态 + 点击/回车回调。颜色取自当前主题。
/// 可选前置图标（`ImageContent`），证明"其它控件低成本嵌入图片"的 pattern。
/// 禁用态由核心层统一管理（`Element::enabled/disabled`）：禁用时核心拦事件、跳 Tab，
/// 并把启用态传入 paint，按钮据此置灰。
pub struct Button {
    label: String,
    icon: Option<ImageContent>,
    state: BtnState,
    on_click: Option<ClickFn>,
    /// 背景色补间（hover/press 淡入淡出）。retarget-in-paint；首帧靠 `primed` 直接落定。
    bg_anim: Cell<Transition<Color>>,
    primed: Cell<bool>,
    /// 语义意图色（默认 Primary=accent，现有代码零改动）。
    intent: Intent,
    /// 尺寸变体（默认 Medium）。
    size: ButtonSize,
    /// 填充变体（默认 Solid 实心；Outline 描边）。
    variant: ButtonVariant,
}

/// 按钮填充变体：实心或描边（透明底 + 意图色边框/文字）。
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum ButtonVariant {
    Solid,
    Outline,
}

impl Button {
    pub fn new(label: String) -> Self {
        Self {
            label,
            icon: None,
            state: BtnState::Normal,
            on_click: None,
            bg_anim: Cell::new(Transition::new(Color::rgba(0, 0, 0, 0))),
            primed: Cell::new(false),
            intent: Intent::Primary,
            size: ButtonSize::Medium,
            variant: ButtonVariant::Solid,
        }
    }

    /// 设置前置图标（供 Builder 的 `.icon_*()` 调用）。
    pub fn set_icon(&mut self, icon: ImageContent) {
        self.icon = Some(icon);
    }

    /// 设置语义意图色（供 Builder 的 `.intent()/.danger()/.neutral()/.accent()` 调用）。
    pub fn set_intent(&mut self, intent: Intent) {
        self.intent = intent;
    }

    /// 设置填充变体（供 Builder 的 `.outline()` 调用）。
    pub fn set_variant(&mut self, variant: ButtonVariant) {
        self.variant = variant;
    }

    /// 把内部三态 + 核心传入的启用态映射为通用视觉状态（供图标调制）。
    fn visual_state(&self, enabled: bool) -> VisualState {
        if !enabled {
            return VisualState::Disabled;
        }
        match self.state {
            BtnState::Normal => VisualState::Normal,
            BtnState::Hover => VisualState::Hover,
            BtnState::Press => VisualState::Pressed,
        }
    }
}

impl Widget for Button {
    fn measure(&self, _avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        let s = text.measure(
            &self.label,
            style.font_family.as_deref(),
            style.font_size,
            None,
        );
        // 图标为正方形，边长取文字高度；加图标宽 + 间距。
        let icon_extra = if self.icon.is_some() {
            s.h + ICON_GAP
        } else {
            0
        };
        // 按尺寸变体取内边距（Medium 左右16/上下9，Small 左右10/上下5）。
        let (pad_w, pad_h) = self.size.padding();
        Size::new(s.w + pad_w + icon_extra, s.h + pad_h)
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
        let t = crate::theme::current();
        let (pal, bt) = (&t.palette, &t.button);
        let vstate = self.visual_state(enabled);
        // intent 解析：Primary 走 ButtonTheme（保持全局换肤 + style.bg 单点覆盖），其余由 palette 派生。
        let is_primary = matches!(self.intent, Intent::Primary);
        let is_outline = self.variant == ButtonVariant::Outline;
        let ic = if is_primary {
            IntentColors {
                bg: bt.bg(pal),
                hover: bt.hover(pal),
                active: bt.active(pal),
                fg: bt.fg(pal),
            }
        } else {
            self.intent.colors(pal)
        };
        // 背景：
        // - Outline：透明底，hover/press 用意图色的淡色叠层（禁用恒透明）。
        // - Solid：禁用用置灰底；Primary 下 style.bg 单点覆盖优先；否则按三态取 intent 色。
        let target = if is_outline {
            match vstate {
                VisualState::Disabled => Color::TRANSPARENT,
                _ => match self.state {
                    BtnState::Normal => Color::TRANSPARENT,
                    BtnState::Hover => ic.bg.scale_alpha(0.10),
                    BtnState::Press => ic.bg.scale_alpha(0.18),
                },
            }
        } else {
            match vstate {
                VisualState::Disabled => bt.disabled(pal),
                _ => match &style.bg {
                    Some(bc) if is_primary => bc.solid_color(&t),
                    _ => match self.state {
                        BtnState::Normal => ic.bg,
                        BtnState::Hover => ic.hover,
                        BtnState::Press => ic.active,
                    },
                },
            }
        };
        // 背景色补间：首帧直接落定（构造期无主题色），其后状态变化淡入淡出。
        let mut anim = self.bg_anim.get();
        if !self.primed.get() {
            anim = Transition::new(target);
            self.primed.set(true);
        } else if anim.target() != target {
            anim.retarget(target, t.anim.fast(), Easing::EaseOut);
        }
        let color = anim.animate();
        self.bg_anim.set(anim);
        // Outline 的文字/描边色：Primary/Danger 用意图主色（蓝/红）；Neutral 的意图主色是
        // p.border（分割线色，过淡，启用态会比禁用态还不可见），改用 text_muted 保证可读对比。
        let outline_col = if matches!(self.intent, Intent::Neutral) {
            pal.text_muted
        } else {
            ic.bg
        };
        // 文字色：禁用用 text_disabled；Outline 用意图主色（蓝/红/灰）作文字；
        // 否则 fg_role 优先（运行期换主题跟随）；style.bg 有值时用显式 style.fg；再否则用意图前景。
        let fg = if vstate == VisualState::Disabled {
            pal.text_disabled
        } else if is_outline {
            outline_col
        } else if style.fg_role.is_some() {
            style.resolved_fg(&t)
        } else if is_primary && style.bg.is_some() {
            style.fg
        } else {
            ic.fg
        };
        // 每节点 corner 覆盖优先（>0），否则用主题。
        let r = if style.corner_radius > 0.0 {
            style.corner_radius
        } else {
            bt.corner(&t.metrics)
        };
        canvas.fill_round_rect(
            bounds.x as f32,
            bounds.y as f32,
            bounds.w as f32,
            bounds.h as f32,
            r,
            &Paint::fill(color),
        );
        // Outline：描边（意图主色；禁用用置灰边）。绘于填充之上、内容之下。
        if is_outline {
            let border = if vstate == VisualState::Disabled {
                pal.text_disabled
            } else {
                outline_col
            };
            let bw = t.metrics.border_width.to_logical(canvas.dpi_scale());
            canvas.stroke_round_rect(
                bounds.x as f32,
                bounds.y as f32,
                bounds.w as f32,
                bounds.h as f32,
                r,
                bw,
                &Paint::fill(border),
            );
        }
        // 无图标：文字整体居中（原行为）。
        let Some(icon) = self.icon.as_ref() else {
            canvas.draw_text(
                &self.label,
                bounds,
                fg,
                Align::Center,
                style.font_family.as_deref(),
                style.font_size,
            );
            return;
        };
        // 有图标：图标 + 文字作为整体水平居中，图标在左、垂直居中。
        let ts = canvas.measure_text(&self.label, style.font_family.as_deref(), style.font_size);
        let ih = ts.h; // 图标正方形边长 = 文字高
        let total_w = ih + ICON_GAP + ts.w;
        let start_x = bounds.x + ((bounds.w - total_w) / 2).max(0);
        let icon_y = bounds.y + ((bounds.h - ih) / 2).max(0);
        // 图标圆角不跟随按钮圆角（按钮圆角作用于整框）；图标默认直角，由其自身 fit 决定。
        let icon_style = Style {
            corner_radius: 0.0,
            ..style.clone()
        };
        icon.paint_into(
            Rect::new(start_x, icon_y, ih, ih),
            canvas,
            &icon_style,
            vstate,
        );
        // 文字紧随图标右侧，垂直方向交给 draw_text 居中。
        let text_rect = Rect::new(start_x + ih + ICON_GAP, bounds.y, ts.w + 2, bounds.h);
        canvas.draw_text(
            &self.label,
            text_rect,
            fg,
            Align::Start,
            style.font_family.as_deref(),
            style.font_size,
        );
    }
    fn on_event(&mut self, ctx: &mut EventCtx, ev: &Event) -> bool {
        // 禁用由核心层统一拦截（call_on_event 不会派发到禁用节点），此处无需判断。
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
                    ctx.request_focus();
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
        // 禁用按钮的 Tab 跳过由核心层 collect_focusable 统一处理。
        true
    }
    fn take_click(&mut self, f: ClickFn) {
        self.on_click = Some(f);
    }
    fn reset_interaction(&mut self) {
        self.state = BtnState::Normal;
        self.primed.set(false); // 下次显示瞬时落定背景色，不回放旧的 hover/press
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
}

/// 控件构建器。可表达容器或叶子。
pub struct Element {
    width: Dimension,
    height: Dimension,
    padding: Insets,
    margin: Insets,
    align: Option<Align>,
    weight: Option<f32>,
    layout: Layout,
    style: Style,
    widget: Box<dyn Widget>,
    children: Vec<Element>,
    visible: bool,
    vis_cond: Option<Box<dyn Fn() -> bool>>,
    clip_children: bool,
    click: Option<ClickFn>,
    on_drop: Option<DropFn>,
    context_menu: Option<crate::core::MenuFn>,
    window_drag: bool,
    enabled: Option<Signal<bool>>,
    en_cond: Option<Box<dyn Fn() -> bool>>,
    tooltip: Option<String>,
    /// 注册为响应式节点：build 后自动调用 `Tree::register_reactive`，
    /// 框架在每次 layout 前向其 widget 调用 `on_update`。
    reactive: bool,
}

impl Element {
    fn base(layout: Layout) -> Self {
        Self {
            width: Dimension::Wrap,
            height: Dimension::Wrap,
            padding: Insets::default(),
            margin: Insets::default(),
            align: None,
            weight: None,
            layout,
            style: Style::default(),
            widget: Box::new(EmptyWidget),
            children: Vec::new(),
            visible: true,
            vis_cond: None,
            clip_children: false,
            click: None,
            on_drop: None,
            context_menu: None,
            window_drag: false,
            enabled: None,
            en_cond: None,
            tooltip: None,
            reactive: false,
        }
    }

    /// 垂直线性容器。
    pub fn col() -> Self {
        Self::base(Layout::Linear {
            axis: Axis::Vertical,
            spacing: 0,
            cross: Align::Start,
        })
    }
    /// 水平线性容器。
    pub fn row() -> Self {
        Self::base(Layout::Linear {
            axis: Axis::Horizontal,
            spacing: 0,
            cross: Align::Start,
        })
    }
    /// 叠层容器（FrameLayout）。
    pub fn stack() -> Self {
        Self::base(Layout::Frame)
    }
    /// 叶子（无子布局）。配合 `.bg()` + 固定尺寸即为色块。
    pub fn leaf() -> Self {
        Self::base(Layout::None)
    }

    /// 文本标签。
    pub fn label(text: impl Into<String>) -> Self {
        Self::base(Layout::None).widget(Label::new(text.into()))
    }

    /// 动态标签（绑定 `Signal<String>`，只读显示）。
    pub fn label_rc(text: Signal<String>) -> Self {
        Self::base(Layout::None).widget(DynLabel::new(text))
    }

    /// 胶囊徽章/标签（如版本号 `v0.0.0-alpha`、状态 `新`）：小字号 + pill 圆角 +
    /// 意图色淡底 + 意图色文字。默认 Primary（强调色）。颜色在构造期据当前主题解析。
    pub fn badge(text: impl Into<String>) -> Self {
        Self::badge_intent(text, Intent::Primary)
    }

    /// 指定语义意图的徽章（Primary=强调蓝 / Neutral=灰 / Danger=红 / Custom=自定义基色）。
    pub fn badge_intent(text: impl Into<String>, intent: Intent) -> Self {
        let th = crate::theme::current();
        let base = match intent {
            Intent::Primary => th.palette.accent,
            other => other.colors(&th.palette).bg,
        };
        Element::row()
            .cross(Align::Center)
            .padding_xy(9, 3)
            .corner(999.0)
            .bg(base.scale_alpha(0.15))
            .child(
                Element::label(text.into())
                    .font_size(12.0)
                    .font_weight(600)
                    .fg(base),
            )
    }

    /// Label/DynLabel 专属配置入口。
    fn config_label(mut self, f: impl FnOnce(&mut Label)) -> Self {
        if let Some(a) = self.widget.as_any_mut() {
            if let Some(l) = a.downcast_mut::<Label>() {
                f(l);
                return self;
            }
        }
        debug_assert!(false, "max_lines()/truncate() 只能用于 Element::label(..)");
        self
    }
    fn config_dynlabel(mut self, f: impl FnOnce(&mut DynLabel)) -> Self {
        if let Some(a) = self.widget.as_any_mut() {
            if let Some(l) = a.downcast_mut::<DynLabel>() {
                f(l);
                return self;
            }
        }
        debug_assert!(
            false,
            "max_lines()/truncate() 只能用于 Element::label_rc(..)"
        );
        self
    }

    /// 限制显示行数（超出高度裁剪；配合 `.truncate()` 可在末行加省略号）。
    /// 同时适用于 `label` 和 `label_rc`。
    pub fn max_lines(mut self, n: usize) -> Self {
        if self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<Label>())
            .is_some()
        {
            return self.config_label(|l| l.max_lines = Some(n));
        }
        self.config_dynlabel(|l| l.max_lines = Some(n))
    }

    /// 文本溢出省略方式（`max_lines(1)` 时精确截断，多行仅高度裁剪）。
    /// 同时适用于 `label` 和 `label_rc`。
    pub fn truncate(mut self, mode: Truncate) -> Self {
        if self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<Label>())
            .is_some()
        {
            return self.config_label(|l| l.truncate = mode);
        }
        self.config_dynlabel(|l| l.truncate = mode)
    }

    /// 交互按钮。配合 `.on_click(...)` 设置回调。
    pub fn button(label: impl Into<String>) -> Self {
        Self::base(Layout::None).widget(Button::new(label.into()))
    }

    /// 纯图标按钮（字形）：无文字、方形、hover/press 圆底 + 点击/键盘激活 + 手型光标。
    /// 用于 ⓘ 信息、▲▼ 调序、× 关闭等工具图标。字形随 `.fg()` 取色，`.size()` 调尺寸，
    /// `.tooltip()` 加说明。配合 `.on_click(...)` 设回调。
    pub fn icon_button(glyph: impl Into<String>) -> Self {
        Self::base(Layout::None).widget(containers::IconButton::glyph(glyph))
    }

    /// 纯图标按钮（图片/SVG）：同 [`Element::icon_button`]，但图标用 `ImageContent`
    /// （随状态调制）。配合 `ImageContent::from_svg_bytes`/`from_bytes` 等构造。
    pub fn icon_button_content(content: ImageContent) -> Self {
        Self::base(Layout::None).widget(containers::IconButton::image(content))
    }

    /// 点击/激活回调（按钮等交互控件）。
    pub fn on_click(mut self, f: impl FnMut(&mut EventCtx) + 'static) -> Self {
        self.click = Some(Box::new(f));
        self
    }

    /// 让**任意容器**（`col`/`row`/`stack`）成为可点击面板：补上 hover/press 视觉反馈
    /// （主题自适应半透明叠层）、键盘可聚焦 + 回车/空格激活、悬停手型光标。
    /// 配合 `.on_click(...)` 设回调，`.bg()`/`.corner()`/`.border()` 设外观即得卡片。
    /// 注意：会替换该节点的占位 widget，故不可与叶子控件（label/button 等）叠加使用。
    pub fn clickable(mut self) -> Self {
        debug_assert!(
            self.widget.as_any_mut().is_none(),
            "clickable() 仅用于容器（col/row/stack），不能叠加在叶子控件上"
        );
        self.widget = Box::new(containers::Clickable::new());
        self
    }

    /// 复选框受控点击回调：设置后 CheckBox 点击/键盘激活**不再自动翻转**绑定的 state，
    /// 而是调用本回调，由 app 决定是否翻转（如先弹确认对话框、确认后再 `state.set(true)`）。
    /// 渲染始终跟随 state 当前值——确认前框不会勾上、零闪烁。底层复用 on_click 管线。
    pub fn on_toggle(mut self, f: impl FnMut(&mut EventCtx) + 'static) -> Self {
        self.click = Some(Box::new(f));
        self
    }

    /// 文件拖放回调：用户把文件拖放到本元素（或其子元素）时触发，收到文件路径列表。
    /// **适用于任意控件/容器**——挂到 `.fill()` 的根容器即"全窗接收拖放"；
    /// 落点命中后沿父链冒泡到首个设了回调的节点。回调签名 `FnMut(&mut EventCtx, &[PathBuf])`。
    pub fn on_drop_files(
        mut self,
        f: impl FnMut(&mut EventCtx, &[std::path::PathBuf]) + 'static,
    ) -> Self {
        self.on_drop = Some(Box::new(f));
        self
    }

    /// 右键上下文菜单：在本元素（或其子元素）上右击时，调用 `build` 取菜单项并以
    /// 级联浮层弹出。**适用于任意控件/容器**——挂到面板容器即"在该区域右击弹菜单"；
    /// 命中沿父链冒泡到首个设了回调的节点。项用 `MenuItem`（支持图标/分隔/快捷键/子菜单）。
    pub fn on_context_menu(
        mut self,
        build: impl FnMut() -> Vec<crate::event::MenuItem> + 'static,
    ) -> Self {
        self.context_menu = Some(Box::new(build));
        self
    }

    /// 标记为窗口拖动区（自定义标题栏）：无边框窗口中在此区域按下可拖动窗口。
    /// 命中沿父链生效——标记标题栏容器即其内非交互空白处都可拖；落在子按钮/输入等
    /// 可聚焦控件上不拖（交控件处理）。仅在 `App::frameless()` 窗口有意义。
    pub fn window_drag(mut self) -> Self {
        self.window_drag = true;
        self
    }

    /// 窗口控制按钮（自定义标题栏用）：最小化 / 最大化-还原 / 关闭。
    /// 自绘标准图标 + hover/press（关闭键 hover 转红），点击调对应窗口操作。
    pub fn window_button(kind: window_buttons::WindowButtonKind) -> Self {
        Self::base(Layout::None).widget(window_buttons::WindowButton::new(kind))
    }

    // ---- 链接 ----

    /// 可点击链接文本：链接色 + 下划线，hover/press 三态，点击/回车激活。
    /// 链 `.url(...)` 设置点击打开的地址，或 `.on_click(...)` 自定义动作（两者皆设时回调优先）。
    /// 悬停显示手型光标；禁用态由核心层统一管理（不可点 + 置灰 + 跳 Tab）。
    pub fn link(text: impl Into<String>) -> Self {
        Self::base(Layout::None).widget(link::Link::new(text.into()))
    }

    /// 配置内含的 Link。`url()/underline()` 是 link 专属修饰符，链到其他控件属误用——
    /// debug 构建下 panic 提示，release 下静默忽略（与 text_input/image 的误用检测一致）。
    fn config_link(mut self, f: impl FnOnce(&mut link::Link)) -> Self {
        match self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<link::Link>())
        {
            Some(l) => f(l),
            None => debug_assert!(false, "url()/underline() 只能用于 Element::link(..)"),
        }
        self
    }
    /// 链接点击时用系统默认程序打开的 URL/路径（未设 `on_click` 时生效）。
    pub fn url(self, url: impl Into<String>) -> Self {
        let url = url.into();
        self.config_link(move |l| l.set_url(url))
    }
    /// 是否绘制链接下划线（默认开）。
    pub fn underline(self, on: bool) -> Self {
        self.config_link(move |l| l.set_underline(on))
    }

    // ---- 图片 ----

    /// 图片控件：从文件路径加载（按字节嗅探格式，自适配已注册解码器）。
    /// 加载失败时显示占位框（不 panic）。默认 `Fit::Contain`，可链 `.fit()`/`.corner()`。
    pub fn image(path: impl AsRef<Path>) -> Self {
        Self::base(Layout::None).widget(ImageView::new(Image::from_file(path).ok()))
    }
    /// 图片控件：从嵌入字节加载（`include_bytes!`，按字节嗅探格式）。
    pub fn image_bytes(bytes: &[u8]) -> Self {
        Self::base(Layout::None).widget(ImageView::new(Image::from_bytes(bytes).ok()))
    }
    /// 图片控件：从 SVG 字节光栅化（`svg` feature）。`target_width=None` 用 SVG 固有尺寸，
    /// `Some(w)` 按该宽度等比光栅——HiDPI 求清晰可传 2× 逻辑宽度。加载失败显示占位框。
    #[cfg(feature = "svg")]
    pub fn image_svg(bytes: &[u8], target_width: Option<u32>) -> Self {
        Self::base(Layout::None).widget(ImageView::new(
            Image::from_svg_bytes(bytes, target_width).ok(),
        ))
    }
    /// 图片控件：从原始非预乘 RGBA8 像素构造（`rgba.len()==w*h*4`）。
    pub fn image_rgba(w: u32, h: u32, rgba: &[u8]) -> Self {
        Self::base(Layout::None).widget(ImageView::new(Image::from_rgba(w, h, rgba).ok()))
    }
    /// 图片控件：由预先组装的 `ImageContent` 构造（用于状态换图等高级用法）。
    pub fn image_content(content: ImageContent) -> Self {
        Self::base(Layout::None).widget(ImageView::from_content(content))
    }

    /// 配置内含的 ImageView。`fit()`/`tint()` 是图片专属修饰符，链到其他控件属误用——
    /// debug 构建下 panic 提示，release 下静默忽略（与 text_input 的误用检测一致）。
    fn config_image(mut self, f: impl FnOnce(&mut ImageView)) -> Self {
        match self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<ImageView>())
        {
            Some(iv) => f(iv),
            None => debug_assert!(false, "fit()/tint() 只能用于 Element::image*(..)"),
        }
        self
    }
    /// 图片适配缩放模式（默认 Contain）。
    pub fn fit(self, fit: Fit) -> Self {
        self.config_image(|iv| iv.set_fit(fit))
    }
    /// 图片模板着色（单色图标随颜色变色）。
    pub fn tint(self, color: Color) -> Self {
        self.config_image(|iv| iv.set_tint(color))
    }

    /// 给按钮设置前置图标（嵌入字节）。链到非按钮属误用——debug panic，release 忽略。
    pub fn icon_bytes(self, bytes: &[u8]) -> Self {
        self.config_button_icon(ImageContent::from_bytes(bytes))
    }
    /// 给按钮设置前置图标（文件路径）。
    pub fn icon(self, path: impl AsRef<Path>) -> Self {
        self.config_button_icon(ImageContent::from_file(path))
    }
    /// 给按钮设置前置图标（原始非预乘 RGBA8）。
    pub fn icon_rgba(self, w: u32, h: u32, rgba: &[u8]) -> Self {
        self.config_button_icon(ImageContent::from_rgba(w, h, rgba))
    }
    /// 给按钮设置前置图标（SVG 字节，`svg` feature）。`target_width` 同 [`Element::image_svg`]。
    #[cfg(feature = "svg")]
    pub fn icon_svg(self, bytes: &[u8], target_width: Option<u32>) -> Self {
        self.config_button_icon(ImageContent::from_svg_bytes(bytes, target_width))
    }
    /// 给按钮设置前置图标（预组装内容原语，支持状态换图/着色）。
    pub fn icon_content(self, icon: ImageContent) -> Self {
        self.config_button_icon(icon)
    }
    fn config_button_icon(self, icon: ImageContent) -> Self {
        self.config_button(|b| b.set_icon(icon), "icon()/icon_bytes()")
    }

    /// 小号变体（Button：紧凑内边距；CheckBox：14px 方框）。
    pub fn small(mut self) -> Self {
        if let Some(a) = self.widget.as_any_mut() {
            if let Some(c) = a.downcast_mut::<CheckBox>() {
                c.set_size(CheckBoxSize::Small);
                return self;
            }
        }
        self.config_button(|b| b.size = ButtonSize::Small, "small()")
    }

    /// 描边按钮（透明底 + 意图色边框/文字，hover 淡色叠层）。与 `.neutral()/.danger()/.accent()`
    /// 组合可得不同语义的描边按钮（如蓝色"检查更新"、红色"删除"次按钮）。仅 `Element::button(..)` 可用。
    pub fn outline(self) -> Self {
        self.config_button(|b| b.set_variant(ButtonVariant::Outline), "outline()")
    }

    /// 启用标志（绑定 `Signal<bool>`，运行期可切换）。**适用于任意控件/容器**：
    /// 核心据此拦事件、跳 Tab、令控件置灰；禁用沿父链继承（禁用容器即禁用其全部子节点）。
    pub fn enabled(mut self, flag: Signal<bool>) -> Self {
        self.enabled = Some(flag);
        self
    }
    /// 启用条件（闭包，运行期求值）。镜像 [`visible_when`](Self::visible_when)，但不影响布局：
    /// 条件为 false 时该元素（及子树）置灰、不可交互，仍占位参与测量/绘制。
    /// 适合设置项联动（如「细节项随开关置灰」），避免隐藏导致的分隔线残留与高度抖动。
    pub fn enabled_when(mut self, f: impl Fn() -> bool + 'static) -> Self {
        self.en_cond = Some(Box::new(f));
        self
    }
    /// 悬停提示：指针在本元素上停留片刻后，于指针附近弹出说明浮层。
    /// **适用于任意控件/容器**（像 `enabled`，挂在节点上）；命中取最深节点的提示。
    /// 仅支持单行文本（浮层按单行度量；含 `\n` 在 debug 下提示，release 忽略换行测量）。
    pub fn tooltip(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        debug_assert!(!text.contains('\n'), "tooltip 仅支持单行文本");
        self.tooltip = Some(text);
        self
    }

    /// 静态禁用（`true`=禁用）。`false` 为默认启用、无操作。适用于任意控件/容器。
    pub fn disabled(mut self, on: bool) -> Self {
        if on {
            self.enabled = Some(crate::signal::signal(false));
        }
        self
    }
    fn config_button(mut self, f: impl FnOnce(&mut Button), who: &str) -> Self {
        match self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<Button>())
        {
            Some(b) => f(b),
            None => debug_assert!(false, "{who} 只能用于 Element::button(..)"),
        }
        self
    }

    /// 复选框（绑定 `Signal<bool>`）。
    pub fn checkbox(label: impl Into<String>, state: Signal<bool>) -> Self {
        Self::base(Layout::None).widget(CheckBox::new(label.into(), state))
    }
    /// 显式设置语义意图色。Button / CheckBox 通用。
    /// 注意：非 Primary intent 接管整组视觉，此时 `.bg()` 单点覆盖不生效。
    pub fn intent(self, i: Intent) -> Self {
        self.config_intent("intent()", i)
    }
    /// 危险意图（主题 danger 红，如"删除数据"）。Button / CheckBox 通用。
    pub fn danger(self) -> Self {
        self.config_intent("danger()", Intent::Danger)
    }
    /// 次要意图（中性灰）。主要用于 Button 的次要按钮。
    pub fn neutral(self) -> Self {
        self.config_intent("neutral()", Intent::Neutral)
    }
    /// 自定义意图基色（扩展点）：框架派生整组视觉。Button / CheckBox 通用。
    pub fn accent(self, color: Color) -> Self {
        self.config_intent("accent()", Intent::Custom(color))
    }
    /// intent 修饰符落点：依次尝试 Button / CheckBox，命中即设；用于其他控件属误用。
    fn config_intent(mut self, who: &str, i: Intent) -> Self {
        if let Some(a) = self.widget.as_any_mut() {
            if let Some(b) = a.downcast_mut::<Button>() {
                b.set_intent(i);
                return self;
            }
            if let Some(c) = a.downcast_mut::<CheckBox>() {
                c.set_intent(i);
                return self;
            }
        }
        debug_assert!(false, "{who} 只能用于 Button / CheckBox");
        self
    }
    /// 开关（绑定 `Signal<bool>`）。
    pub fn switch(state: Signal<bool>) -> Self {
        Self::base(Layout::None).widget(Switch::new(state))
    }
    /// 单选按钮（共享 `Signal<usize>` 组状态 + 本项索引）。
    pub fn radio(label: impl Into<String>, group: Signal<usize>, index: usize) -> Self {
        Self::base(Layout::None).widget(RadioButton::new(label.into(), group, index))
    }
    /// 滑块（绑定 `Signal<f32>`，值域 0.0..=1.0）。
    pub fn slider(value: Signal<f32>) -> Self {
        Self::base(Layout::None).widget(Slider::new(value))
    }

    fn config_slider(mut self, f: impl FnOnce(&mut Slider)) -> Self {
        match self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<Slider>())
        {
            Some(s) => f(s),
            None => debug_assert!(false, "show_value() 只能用于 Element::slider(..)"),
        }
        self
    }

    /// 在旋钮右侧显示当前值百分比（如 "65%"）。仅 `Element::slider(..)` 可用。
    pub fn show_value(self, on: bool) -> Self {
        self.config_slider(|s| s.set_show_value(on))
    }

    /// 单行文本输入（绑定 `Signal<String>`）。
    /// 可链式 `.password()` / `.multiline()` / `.wrap(bool)` 配置行为。
    pub fn text_input(text: Signal<String>, placeholder: impl Into<String>) -> Self {
        Self::base(Layout::None).widget(TextInput::new(text, placeholder.into()))
    }

    /// 配置内含的 TextInput。`password()/multiline()/wrap()` 是 text_input 专属修饰符；
    /// 链到其他控件属误用——debug 构建下 panic 提示，release 下静默忽略（无类型分裂代价）。
    fn config_text_input(mut self, f: impl FnOnce(&mut inputs::TextConfig)) -> Self {
        match self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<TextInput>())
        {
            Some(ti) => f(ti.config_mut()),
            None => debug_assert!(
                false,
                "password()/multiline()/wrap() 只能用于 Element::text_input(..)"
            ),
        }
        self
    }
    /// 密码输入：显示掩码圆点、禁止复制/剪切明文。强制单行（密码不应多行）。
    pub fn password(self) -> Self {
        self.config_text_input(|c| {
            c.password = true;
            c.multiline = false;
        })
    }
    /// 多行输入（编辑/换行行为见 P4）。
    pub fn multiline(self) -> Self {
        self.config_text_input(|c| c.multiline = true)
    }
    /// 多行软换行开关（仅 multiline 生效）。
    pub fn wrap(self, on: bool) -> Self {
        self.config_text_input(|c| c.wrap = on)
    }

    /// 前置图标字形（如放大镜 `'\u{1F50D}'`）：在输入框左侧留出图标区并绘制，
    /// 文字/光标/点击命中相应右移。搜索框等用。仅 `Element::text_input(..)` 可用。
    pub fn leading_icon(self, glyph: char) -> Self {
        self.config_text_input(|c| c.leading = Some(glyph))
    }

    /// 运行期可见条件：闭包返回 false 时该节点本帧不显示/不命中。
    ///
    /// 契约：闭包**必须是纯函数**（仅读状态、无副作用）。它在每帧的
    /// measure/arrange/paint/hit-test/焦点收集中被多次调用，且帧内值不应变化。
    pub fn visible_when(mut self, f: impl Fn() -> bool + 'static) -> Self {
        self.vis_cond = Some(Box::new(f));
        self
    }

    /// 分段控制器（绑定 `Signal<usize>` 选中索引 + 段标签）：连体多段单选，
    /// 选中段高亮。语义同 `radio` 组，外观更紧凑——适合"二/三选一"切换。
    /// 点击选段、悬停逐段高亮、聚焦后左右方向键移动选中。
    pub fn segmented(options: Vec<impl Into<String>>, selected: Signal<usize>) -> Self {
        let opts: Vec<String> = options.into_iter().map(|o| o.into()).collect();
        debug_assert!(!opts.is_empty(), "Element::segmented 至少需要一段");
        Self::base(Layout::None).widget(segmented::SegmentedControl::new(opts, selected))
    }

    /// 下拉选择（绑定 `Signal<usize>` 选中索引 + 选项标签）。
    pub fn dropdown(options: Vec<impl Into<String>>, selected: Signal<usize>) -> Self {
        let opts: Vec<String> = options.into_iter().map(|o| o.into()).collect();
        Self::base(Layout::None).widget(select::Dropdown::new(opts, selected))
    }

    /// 响应式下拉：选项绑定 `Signal<Vec<String>>`，列表变更（如异步加载的主题/字体到达）
    /// 自动重新测量/渲染。选中索引仍由 `selected` 绑定。
    pub fn dropdown_reactive(options: Signal<Vec<String>>, selected: Signal<usize>) -> Self {
        Self::base(Layout::None).widget(select::Dropdown::new_reactive(options, selected))
    }

    /// 数字步进（绑定 `Signal<f64>`，带范围与步长；小数位由步长推断）。
    pub fn stepper(value: Signal<f64>, min: f64, max: f64, step: f64) -> Self {
        Self::base(Layout::None).widget(stepper::Stepper::new(value, min, max, step))
    }

    /// 单选列表（绑定 `Signal<usize>` 选中索引 + 行标签）。可滚动；
    /// 外观（背景/圆角/边框）由调用方在返回的滚动容器上设置。
    ///
    /// 已知限制：每行均可聚焦，长列表会拉长 Tab 焦点链（用户需多次 Tab 跨过）。
    /// 后续可改为整列表单一 tab-stop + 方向键内部移动。
    pub fn list(items: Vec<impl Into<String>>, selected: Signal<usize>) -> Self {
        let mut scroll = Self::scroll().fill();
        for (i, it) in items.into_iter().enumerate() {
            let row = list::ListRow::new(it.into(), selected, i);
            scroll = scroll.child(
                Self::base(Layout::None)
                    .widget(row)
                    .width_match()
                    .height(list::ROW_H),
            );
        }
        scroll
    }

    /// 同 `list`，但选中/悬停为内缩圆角 pill 底（无左缘强调条）。侧栏导航等现代样式用。
    pub fn list_pill(items: Vec<impl Into<String>>, selected: Signal<usize>) -> Self {
        let mut scroll = Self::scroll().fill();
        for (i, it) in items.into_iter().enumerate() {
            let row = list::ListRow::new(it.into(), selected, i).pill();
            scroll = scroll.child(
                Self::base(Layout::None)
                    .widget(row)
                    .width_match()
                    .height(list::ROW_H),
            );
        }
        scroll
    }

    /// 标记为响应式节点：build 后注册到框架，每次 layout 前收到 `Widget::on_update`。
    /// 通常由 `list_signal` 内部调用，手动使用时需搭配实现了 `on_update` 的自定义 widget。
    pub fn reactive(mut self) -> Self {
        self.reactive = true;
        self
    }

    /// 响应式动态列表：数据源绑定 `Signal<Vec<T>>`，信号变化时框架自动重建行元素。
    ///
    /// - `data`：数据源信号；写入新 Vec 即触发列表刷新（排序/过滤均可）。
    /// - `_key_fn`：预留 diff 优化用，当前版本做全量重建，传 `|_| ()` 即可。
    /// - `row_fn`：每行的构建函数，接收数据条目返回 `Element`。
    ///
    /// # 示例
    /// ```ignore
    /// let items = signal(vec!["苹果", "香蕉", "橙子"]);
    /// Element::list_signal(items, |_| (), |s, _i| Element::label(s))
    /// ```
    pub fn list_signal<T, K>(
        data: Signal<Vec<T>>,
        _key_fn: impl Fn(&T) -> K + 'static,
        row_fn: impl Fn(T) -> Self + 'static,
    ) -> Self
    where
        T: Clone + 'static,
        K: Eq + std::hash::Hash,
    {
        let row_fn = std::rc::Rc::new(row_fn);
        // 构建初始子元素
        let initial: Vec<Self> = data.get().into_iter().map(|item| row_fn(item)).collect();
        // DynList widget 持有 Rc 副本，信号变更时重建子节点
        let row_fn_clone = row_fn.clone();
        let widget = dyn_list::DynList::new(data, move |item: T| row_fn_clone(item));
        let mut container = Self::scroll().fill();
        container.widget = Box::new(widget);
        container.reactive = true;
        for el in initial {
            container.children.push(el);
        }
        container
    }

    /// 带前置图标的单选列表：`items` 为 (标签, 图标内容) 列表。其余同 `list`。
    /// 图标用 `ImageContent`，可链 `.fit()`/状态换图等；行图标随选中/悬停状态调制。
    pub fn list_icons(
        items: Vec<(impl Into<String>, ImageContent)>,
        selected: Signal<usize>,
    ) -> Self {
        let mut scroll = Self::scroll().fill();
        for (i, (label, icon)) in items.into_iter().enumerate() {
            let row = list::ListRow::new(label.into(), selected, i).with_icon(icon);
            scroll = scroll.child(
                Self::base(Layout::None)
                    .widget(row)
                    .width_match()
                    .height(list::ROW_H),
            );
        }
        scroll
    }

    /// 带 chevron 的导航行：左标签 + 右侧 `>`，悬停高亮，点击/回车触发 `.on_click(...)`。
    /// 适合"钻入子页 / 打开子设置"的设置行。无持久选中态——需要选中高亮的导航用 `list`。
    pub fn nav_row(label: impl Into<String>) -> Self {
        Self::base(Layout::None)
            .widget(nav::NavRow::new(label.into()))
            .width_match()
            .height(nav::NAV_ROW_H)
    }

    /// 可折叠分组：点击标题行展开 / 收起 `body`。`expanded` 绑定展开状态，
    /// body 经 `visible_when(expanded)` 显隐——收起时不占布局、不参与命中。
    /// 标题行右侧三角随状态翻转（展开向下 / 收起向右）。
    pub fn collapsible(title: impl Into<String>, expanded: Signal<bool>, body: Element) -> Self {
        let header = Self::base(Layout::None)
            .widget(nav::CollapsibleHeader::new(title.into(), expanded))
            .width_match()
            .height(nav::NAV_ROW_H);
        let show = expanded;
        Element::col()
            .width_match()
            .child(header)
            .child(body.visible_when(move || show.get()))
    }

    /// 手风琴（多面板折叠卡片）：带边框/圆角的卡片，逐面板「标题头 + 可折叠内容」，
    /// 面板间分隔线。**单开互斥**版——`selected` 共享选中索引，`-1` = 全收起，初值即
    /// 默认展开项（与 [`Element::tabs`] 的 `Signal<usize>` 选中模型同构）。
    /// 点击某面板头展开它会自动收起其它面板。
    pub fn accordion(selected: Signal<i32>, panels: Vec<(impl Into<String>, Element)>) -> Self {
        Self::accordion_impl(panels, |i| nav::ExpandState::Single {
            sel: selected,
            index: i,
        })
    }

    /// 手风琴**多开**版：各面板独立展开/收起、互不影响（初始全部收起）。
    pub fn accordion_multi(panels: Vec<(impl Into<String>, Element)>) -> Self {
        Self::accordion_impl(panels, |_| {
            nav::ExpandState::Multi(crate::signal::signal(false))
        })
    }

    /// 手风琴共用组装：外层卡片 + 逐面板（首面板前不加分隔线）头与显隐 body。
    /// `make_state(i)` 决定第 i 个面板的展开模型（单开共享索引 / 多开独立布尔）。
    fn accordion_impl(
        panels: Vec<(impl Into<String>, Element)>,
        make_state: impl Fn(usize) -> nav::ExpandState,
    ) -> Self {
        // 四色改用主题角色延迟解析（运行期换主题自动跟随）；corner 为度量，构建期取值即可
        // （换主题不改圆角，符合预期）。
        use crate::style::Role;
        let corner = {
            let th = crate::theme::current();
            th.accordion.corner(&th.metrics)
        };
        let mut card = Element::col()
            .width_match()
            .bg_role(Role::Surface)
            .border_role(Role::AccordionBorder, 1)
            .corner(corner);
        for (i, (title, body)) in panels.into_iter().enumerate() {
            if i > 0 {
                card = card.child(
                    Element::base(Layout::None)
                        .width_match()
                        .height(1)
                        .bg_role(Role::Divider),
                );
            }
            let state = make_state(i);
            let header = Self::base(Layout::None)
                .widget(nav::AccordionHeader::new(title.into(), state.clone()))
                .width_match()
                .height(nav::NAV_ROW_H)
                .bg_role(Role::AccordionHeaderBg);
            let show = state.clone();
            card = card
                .child(header)
                .child(body.visible_when(move || show.is_expanded()));
        }
        card
    }

    /// 确定进度条（绑定 `Signal<f32>`，值域 0.0..=1.0）。
    pub fn progress(value: Signal<f32>) -> Self {
        Self::base(Layout::None).widget(progress::ProgressBar::determinate(value))
    }
    /// 不确定进度条（忙碌动画）。需要宿主按帧驱动（仅可见时消耗 CPU）。
    pub fn progress_indeterminate() -> Self {
        Self::base(Layout::None).widget(progress::ProgressBar::indeterminate())
    }

    /// 垂直滚动容器：内容超出视口时可滚轮滚动并裁剪。
    pub fn scroll() -> Self {
        let mut e = Self::base(Layout::Scroll).widget(containers::ScrollWidget::default());
        e.clip_children = true;
        e
    }

    /// 水平分隔线。背景用主题角色，运行期换主题自动跟随。
    pub fn divider() -> Self {
        Self::base(Layout::None)
            .width_match()
            .height(1)
            .bg_role(crate::style::Role::Divider)
    }

    /// 标签页：顶部标签条切换、下方内容区按选中项显隐。
    /// `selected` 绑定当前选中索引，`pages` 为 (标题, 页面) 列表。
    /// 标题接受 `impl Into<String>`，与 `dropdown`/`list` 的选项类型一致。
    pub fn tabs(selected: Signal<usize>, pages: Vec<(impl Into<String>, Element)>) -> Self {
        let mut bar = Element::row()
            .width_match()
            .height(40)
            .spacing(6)
            .cross(Align::Stretch);
        let mut content = Element::stack().fill().weight(1.0);
        for (i, (title, page)) in pages.into_iter().enumerate() {
            let tab = containers::TabButton::new(title.into(), selected, i);
            bar = bar.child(Element::base(Layout::None).widget(tab));
            let sel2 = selected;
            content = content.child(page.fill().visible_when(move || sel2.get() == i));
        }
        Element::col().fill().spacing(10).child(bar).child(content)
    }

    /// 带前置图标的标签页：`pages` 为 (标题, 图标内容, 页面) 列表。其余同 `tabs`。
    /// 标签图标随选中/悬停状态调制。
    pub fn tabs_icons(
        selected: Signal<usize>,
        pages: Vec<(impl Into<String>, ImageContent, Element)>,
    ) -> Self {
        let mut bar = Element::row()
            .width_match()
            .height(40)
            .spacing(6)
            .cross(Align::Stretch);
        let mut content = Element::stack().fill().weight(1.0);
        for (i, (title, icon, page)) in pages.into_iter().enumerate() {
            let tab = containers::TabButton::new(title.into(), selected, i).with_icon(icon);
            bar = bar.child(Element::base(Layout::None).widget(tab));
            let sel2 = selected;
            content = content.child(page.fill().visible_when(move || sel2.get() == i));
        }
        Element::col().fill().spacing(10).child(bar).child(content)
    }

    /// 模态对话框：全窗半透明遮罩 + 居中内容，遮罩吞掉指针事件实现模态。
    /// `show` 绑定显示标志。
    pub fn dialog(show: Signal<bool>, content: Element) -> Self {
        Element::stack()
            .fill()
            .widget(containers::ModalScrim)
            .bg(Color::rgba(0, 0, 0, 120))
            .visible_when(move || show.get())
            .child(content.align(Align::Center))
    }

    /// 带标题栏 + 关闭按钮 + 底栏的对话框面板（在 `dialog` 遮罩之上居中）。
    /// `width` 为面板逻辑宽（标题区靠它分配 title/×）；`on_close` 点右上 × 触发
    /// （通常 `show.set(false)`）；`body` 为内容；`footer` 为底部按钮行（调用方组织，
    /// 用 `Element::flex_spacer()` 把按钮推到右侧）。
    pub fn dialog_panel(
        show: Signal<bool>,
        title: impl Into<String>,
        width: i32,
        on_close: impl FnMut(&mut EventCtx) + 'static,
        body: Element,
        footer: Element,
    ) -> Self {
        let th = crate::theme::current();
        let header = Element::row()
            .width_match()
            .cross(Align::Center)
            .child(
                Element::label(title.into())
                    .font_size(18.0)
                    .font_weight(700)
                    .fg(th.palette.text)
                    .weight(1.0)
                    .height(26),
            )
            .child(
                Element::icon_button("\u{2715}")
                    .size(28, 28)
                    .fg(th.palette.text_muted)
                    .on_click(on_close),
            );
        let panel = Element::col()
            .width(width)
            .bg(th.palette.surface)
            .corner(th.metrics.corner_lg)
            .padding(20)
            .spacing(16)
            .child(header)
            .child(body)
            .child(footer);
        Element::dialog(show, panel)
    }

    /// 弹性空白：主轴方向占据剩余空间，把其后的兄弟元素推到另一端（如底栏「左按钮 … 右按钮」）。
    pub fn flex_spacer() -> Self {
        Element::stack().weight(1.0)
    }

    /// 等宽网格：把 `items` 按每行 `cols` 个排布，行/列间距 `gap`，列按权重均分等宽；
    /// 末行不足时用空白补齐以保持列对齐。常用于复选框组、卡片墙。
    pub fn grid(cols: usize, gap: i32, items: Vec<Element>) -> Self {
        debug_assert!(cols >= 1, "grid 至少需要 1 列");
        let cols = cols.max(1);
        let mut container = Element::col().width_match().spacing(gap);
        let mut iter = items.into_iter();
        loop {
            let mut cells: Vec<Element> = Vec::with_capacity(cols);
            for _ in 0..cols {
                match iter.next() {
                    Some(e) => cells.push(e),
                    None => break,
                }
            }
            if cells.is_empty() {
                break;
            }
            let n = cells.len();
            let mut r = Element::row()
                .width_match()
                .spacing(gap)
                .cross(Align::Stretch);
            for e in cells {
                r = r.child(e.weight(1.0));
            }
            for _ in n..cols {
                r = r.child(Element::stack().weight(1.0)); // 末行补空占位
            }
            container = container.child(r);
        }
        container
    }

    /// 可删除标签（chip）：意图色淡底 pill + 文字 + 右侧 × 删除按钮。点 × 触发 `on_remove`。
    /// 纯展示标签（不可删）用 [`Element::badge`]。多值字段见 [`Element::tag_field`]。
    pub fn chip(text: impl Into<String>, on_remove: impl FnMut(&mut EventCtx) + 'static) -> Self {
        let th = crate::theme::current();
        let base = th.palette.accent;
        Element::row()
            .cross(Align::Center)
            .spacing(4)
            .padding_xy(9, 3)
            .corner(999.0)
            .bg(base.scale_alpha(0.14))
            .child(
                Element::label(text.into())
                    .font_size(12.5)
                    .fg(base)
                    .height(18),
            )
            .child(
                Element::icon_button("\u{2715}")
                    .size(16, 16)
                    .font_size(11.0)
                    .fg(base)
                    .on_click(on_remove),
            )
    }

    /// 标签字段：仿输入框的带边框容器，内含一组 chip（多值展示）。`chips` 用
    /// [`Element::chip`] 生成；为空时显示 `placeholder`。新增值由 app 驱动
    /// （维护值列表 Signal，变化后重建 chips 列表）。
    pub fn tag_field(placeholder: impl Into<String>, chips: Vec<Element>) -> Self {
        let th = crate::theme::current();
        let mut row = Element::row()
            .width_match()
            .cross(Align::Center)
            .spacing(6)
            .padding_xy(8, 6)
            .corner(th.input.corner(&th.metrics))
            .bg(th.input.bg(&th.palette))
            .border(th.input.border(&th.palette), 1);
        if chips.is_empty() {
            row = row.child(
                Element::label(placeholder.into())
                    .font_size(13.0)
                    .fg(th.palette.placeholder)
                    .weight(1.0)
                    .height(20),
            );
        } else {
            for c in chips {
                row = row.child(c);
            }
        }
        row
    }

    /// 数据表格（只读）：固定表头 + 可滚动正文 + 斑马纹。`columns` 为 (列标题, 权重)，
    /// 列宽按权重均分；`rows` 为每行的单元格文本。需在限高容器内使用（正文滚动）。
    /// 需要可编辑/可点击单元格时用 [`Element::table_custom`]，自带 cell 元素。
    pub fn table(
        columns: Vec<(impl Into<String>, f32)>,
        rows: Vec<Vec<impl Into<String>>>,
    ) -> Self {
        let th = crate::theme::current();
        let cols: Vec<(String, f32)> = columns.into_iter().map(|(t, w)| (t.into(), w)).collect();
        let fg = th.palette.text;
        let body: Vec<Vec<Element>> = rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|c| Self::table_cell_pad(Element::label(c.into()).font_size(13.0).fg(fg)))
                    .collect()
            })
            .collect();
        Self::table_custom(cols, body)
    }

    /// 表格单元格统一内边距包裹（文字内缩、单元格本身占满整格——便于整格背景/高亮）。
    /// 内边距在单元格**内部**而非行上，故可点击单元格的 hover 高亮能覆盖整格。
    fn table_cell_pad(content: Element) -> Self {
        Element::stack()
            .padding_xy(TABLE_CELL_PAD_X, TABLE_CELL_PAD_Y)
            .child(content.width_match().height(20))
    }

    /// 数据表格（自定义单元格）：同 [`Element::table`]，但每个单元格是任意 `Element`
    /// （可放 `clickable`/`text_input` 等实现选中/编辑）。`columns` 为 (列标题, 权重)。
    pub fn table_custom(columns: Vec<(String, f32)>, rows: Vec<Vec<Element>>) -> Self {
        let th = crate::theme::current();
        // 表头：加粗、弱化色、次级表面底。内边距在每列格内部（与正文同分布，列对齐）。
        let mut header = Element::row()
            .width_match()
            .cross(Align::Stretch)
            .bg(th.palette.surface_alt);
        for (title, w) in &columns {
            header = header.child(
                Element::stack()
                    .weight(*w)
                    .padding_xy(TABLE_CELL_PAD_X, TABLE_HEADER_PAD_Y)
                    .child(
                        Element::label(title.clone())
                            .font_size(13.0)
                            .font_weight(600)
                            .fg(th.palette.text_muted)
                            .width_match()
                            .height(18),
                    ),
            );
        }
        // 正文：逐行，斑马纹 + 行下分隔线。`cross(Stretch)` 让单元格撑满行高（便于整格高亮）；
        // 行本身不设内边距——内边距在各单元格内部（见 table_cell_pad / table_editable）。
        let mut scroll = Element::scroll().fill();
        for (ri, cells) in rows.into_iter().enumerate() {
            let mut tr = Element::row().width_match().cross(Align::Stretch);
            if ri % 2 == 1 {
                tr = tr.bg(th.palette.surface_alt.scale_alpha(0.5));
            }
            for (ci, cell) in cells.into_iter().enumerate() {
                let w = columns.get(ci).map(|c| c.1).unwrap_or(1.0);
                tr = tr.child(cell.weight(w));
            }
            scroll = scroll.child(
                Element::col()
                    .width_match()
                    .child(tr)
                    .child(Element::divider()),
            );
        }
        Element::col()
            .width_match()
            .child(header)
            .child(Element::divider())
            .child(scroll.weight(1.0))
    }

    /// 可编辑数据表格：单元格数据用 `Signal<String>` 承载（显示自动跟随），点单元格触发
    /// `on_edit(ctx, row, col)`——由 app 据 (row,col) 弹出编辑框（如 `dialog_panel` + `text_input`），
    /// 确认后写回对应 `cells[row][col]`，表格下一帧自动刷新。**非即时编辑**，编辑入口与提交解耦。
    ///
    /// `columns` 为 (列标题, 权重)；`cells` 为每行的单元格信号（与列一一对应）。
    pub fn table_editable(
        columns: Vec<(impl Into<String>, f32)>,
        cells: Vec<Vec<Signal<String>>>,
        on_edit: impl FnMut(&mut EventCtx, usize, usize) + 'static,
    ) -> Self {
        let cols: Vec<(String, f32)> = columns.into_iter().map(|(t, w)| (t.into(), w)).collect();
        let cb = Rc::new(RefCell::new(on_edit));
        let fg = crate::theme::current().palette.text;
        let rows: Vec<Vec<Element>> = cells
            .into_iter()
            .enumerate()
            .map(|(r, row)| {
                row.into_iter()
                    .enumerate()
                    .map(|(c, sig)| {
                        let cb = cb.clone();
                        // 每格为可点击容器（hover 反馈 + 手型），填满整格、内边距在内部，
                        // 故 hover 高亮覆盖整个单元格（带圆角），而非仅贴着文字。
                        Element::stack()
                            .clickable()
                            .on_click(move |ctx| (cb.borrow_mut())(ctx, r, c))
                            .corner(TABLE_CELL_CORNER)
                            .padding_xy(TABLE_CELL_PAD_X, TABLE_CELL_PAD_Y)
                            .child(
                                Element::label_rc(sig)
                                    .font_size(13.0)
                                    .fg(fg)
                                    .width_match()
                                    .height(20),
                            )
                    })
                    .collect()
            })
            .collect();
        Self::table_custom(cols, rows)
    }

    /// 设置自定义内容控件（叶子）。
    pub fn widget(mut self, w: impl Widget + 'static) -> Self {
        self.widget = Box::new(w);
        self
    }

    // ---- 尺寸 ----
    pub fn width(mut self, px: i32) -> Self {
        self.width = Dimension::Px(px);
        self
    }
    pub fn height(mut self, px: i32) -> Self {
        self.height = Dimension::Px(px);
        self
    }
    pub fn size(self, w: i32, h: i32) -> Self {
        self.width(w).height(h)
    }
    pub fn width_match(mut self) -> Self {
        self.width = Dimension::Match;
        self
    }
    pub fn height_match(mut self) -> Self {
        self.height = Dimension::Match;
        self
    }
    /// 宽高都撑满父容器。
    pub fn fill(self) -> Self {
        self.width_match().height_match()
    }
    /// 主轴权重（父为线性容器时按比例瓜分剩余空间）。
    pub fn weight(mut self, w: f32) -> Self {
        self.weight = Some(w);
        self
    }

    // ---- 间距 ----
    pub fn padding(mut self, p: i32) -> Self {
        self.padding = Insets::all(p);
        self
    }
    pub fn padding_xy(mut self, h: i32, v: i32) -> Self {
        self.padding = Insets::symmetric(h, v);
        self
    }
    pub fn margin(mut self, m: i32) -> Self {
        self.margin = Insets::all(m);
        self
    }
    pub fn margin_xy(mut self, h: i32, v: i32) -> Self {
        self.margin = Insets::symmetric(h, v);
        self
    }

    // ---- 对齐/布局参数 ----
    pub fn align(mut self, a: Align) -> Self {
        self.align = Some(a);
        self
    }
    /// 线性容器主轴子间距。
    pub fn spacing(mut self, s: i32) -> Self {
        if let Layout::Linear { spacing, .. } = &mut self.layout {
            *spacing = s;
        }
        self
    }
    /// 线性容器交叉轴默认对齐。
    pub fn cross(mut self, a: Align) -> Self {
        if let Layout::Linear { cross, .. } = &mut self.layout {
            *cross = a;
        }
        self
    }

    // ---- 样式 ----
    /// 背景填充色。命名与 `Style.bg` / `EventCtx::set_bg` / `fg` 保持一致（统一缩写）。
    pub fn bg(mut self, c: Color) -> Self {
        self.style.bg = Some(crate::style::Brush::Solid(c));
        self
    }
    /// 渐变背景（线性/径向，圆角随 `.corner()`）。
    pub fn bg_gradient(mut self, g: crate::render::Gradient) -> Self {
        self.style.bg = Some(crate::style::Brush::Gradient(g));
        self
    }
    /// 主题角色背景：运行期换主题时自动跟随刷新。
    pub fn bg_role(mut self, role: crate::style::Role) -> Self {
        self.style.bg = Some(crate::style::Brush::Role(role));
        self
    }
    pub fn border(mut self, c: Color, w: i32) -> Self {
        self.style.border = Some((crate::style::Brush::Solid(c), w));
        self
    }
    /// 主题角色边框（运行期换主题跟随）。
    pub fn border_role(mut self, role: crate::style::Role, w: i32) -> Self {
        self.style.border = Some((crate::style::Brush::Role(role), w));
        self
    }
    pub fn corner(mut self, r: f32) -> Self {
        self.style.corner_radius = r;
        self
    }
    pub fn fg(mut self, c: Color) -> Self {
        self.style.fg = c;
        self.style.fg_role = None;
        self
    }
    /// 主题角色前景/文字色（运行期换主题跟随）。
    pub fn fg_role(mut self, role: crate::style::Role) -> Self {
        self.style.fg_role = Some(role);
        self
    }
    /// 浮层投影（drop shadow）。
    pub fn shadow(mut self, s: crate::style::Shadow) -> Self {
        self.style.shadow = Some(s);
        self
    }
    /// 子树整体不透明度（0..=1）。
    pub fn opacity(mut self, o: f32) -> Self {
        self.style.opacity = o.clamp(0.0, 1.0);
        self
    }
    pub fn font_size(mut self, s: f32) -> Self {
        self.style.font_size = s;
        self
    }
    /// 字重（400=常规、500=中、600=半粗、700=粗）。标题/强调文字加粗更接近设计稿。
    pub fn font_weight(mut self, w: u16) -> Self {
        self.style.font_weight = w;
        self
    }
    /// 文字水平对齐。
    pub fn text_align(mut self, a: Align) -> Self {
        self.style.text_align = a;
        self
    }

    // ---- 子节点 ----
    pub fn child(mut self, c: Element) -> Self {
        self.children.push(c);
        self
    }
    pub fn children(mut self, cs: impl IntoIterator<Item = Element>) -> Self {
        self.children.extend(cs);
        self
    }
    pub fn visible(mut self, v: bool) -> Self {
        self.visible = v;
        self
    }

    /// 递归落入 arena，返回根 NodeId。
    pub fn build(mut self, tree: &mut Tree) -> NodeId {
        let is_reactive = self.reactive;
        let my_axis = match self.layout {
            Layout::Linear { axis, .. } => Some(axis),
            _ => None,
        };
        let children = std::mem::take(&mut self.children);
        // 把 Builder 上的点击回调注入控件（仅交互控件接收）。
        let mut widget = self.widget;
        if let Some(f) = self.click {
            widget.take_click(f);
        }
        let node = Node {
            parent: None,
            children: Vec::new(),
            bounds: Default::default(),
            measured: Default::default(),
            width: self.width,
            height: self.height,
            padding: self.padding,
            margin: self.margin,
            align: self.align,
            layout: self.layout,
            widget,
            style: self.style,
            visible: self.visible,
            vis_cond: self.vis_cond,
            enabled: self.enabled,
            en_cond: self.en_cond,
            on_drop: self.on_drop,
            context_menu: self.context_menu,
            window_drag: self.window_drag,
            tooltip: self.tooltip,
            focused: false,
            clip_children: self.clip_children,
            scroll_y: 0,
            content_h: 0,
            over_scroll: 0,
            prev_visible: Cell::new(true),
        };
        let id = tree.insert(node);
        if is_reactive {
            tree.register_reactive(id);
        }
        for mut ce in children {
            // 父为线性容器时，把请求的 weight 落到主轴维度
            if let (Some(axis), Some(w)) = (my_axis, ce.weight) {
                match axis {
                    Axis::Horizontal => ce.width = Dimension::Weight(w),
                    Axis::Vertical => ce.height = Dimension::Weight(w),
                }
            }
            let cid = ce.build(tree);
            tree.add_child(id, cid);
        }
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Point;
    use crate::signal::signal;
    use std::path::PathBuf;
    use std::rc::Rc;

    /// 在 200×200 窗口里布局并返回 (tree, root)。
    fn layout(el: Element) -> Tree {
        let mut tree = Tree::new();
        let root = el.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        tree
    }

    #[test]
    fn disabled_text_uses_text_disabled_color() {
        let theme = crate::theme::Theme::default();
        let style = Style {
            fg: Color::hex(0x123456),
            ..Style::default()
        };
        // 启用：取样式自身前景色。
        assert_eq!(text_fg(true, &style, &theme), Color::hex(0x123456));
        // 禁用：统一降为 text_disabled（标签/说明随容器禁用一并置灰）。
        assert_eq!(text_fg(false, &style, &theme), theme.palette.text_disabled);
        // 启用 + fg_role（hint 的真实形态）：经 role 解析为 text_muted，不被禁用分支吞掉。
        let muted = Style {
            fg_role: Some(crate::style::Role::TextMuted),
            ..Style::default()
        };
        assert_eq!(text_fg(true, &muted, &theme), theme.palette.text_muted);
        assert_eq!(text_fg(false, &muted, &theme), theme.palette.text_disabled);
    }

    /// 记录 `draw_text` 颜色实参的最小 Canvas，用于在 paint 级守护"禁用置灰"接线。
    struct CaptureCanvas {
        last_text_color: std::cell::Cell<Option<Color>>,
    }
    impl crate::render::Canvas for CaptureCanvas {
        fn dpi_scale(&self) -> f32 {
            1.0
        }
        fn fill_rect(&mut self, _: f32, _: f32, _: f32, _: f32, _: &crate::render::Paint) {}
        fn fill_round_rect(
            &mut self,
            _: f32,
            _: f32,
            _: f32,
            _: f32,
            _: f32,
            _: &crate::render::Paint,
        ) {
        }
        fn stroke_round_rect(
            &mut self,
            _: f32,
            _: f32,
            _: f32,
            _: f32,
            _: f32,
            _: f32,
            _: &crate::render::Paint,
        ) {
        }
        fn draw_line(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: &crate::render::Paint) {}
        fn fill_circle(&mut self, _: f32, _: f32, _: f32, _: &crate::render::Paint) {}
        fn draw_shadow(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32, _: Color) {}
        fn draw_image(
            &mut self,
            _: &crate::render::image::Image,
            _: Rect,
            _: crate::render::image::Fit,
            _: f32,
            _: f32,
        ) {
        }
        fn draw_text(
            &mut self,
            _text: &str,
            _rect: Rect,
            color: Color,
            _align: crate::spec::Align,
            _family: Option<&str>,
            _size: f32,
        ) {
            self.last_text_color.set(Some(color));
        }
        fn measure_text(&mut self, _: &str, _: Option<&str>, _: f32) -> Size {
            Size::ZERO
        }
        fn push_layer(&mut self, _: f32) {}
        fn pop_layer(&mut self) {}
        fn save(&mut self) {}
        fn restore(&mut self) {}
        fn clip_rect(&mut self, _: Rect) {}
    }

    #[test]
    fn label_paint_wires_enabled_to_text_color() {
        use crate::core::Widget;
        let style = Style {
            fg: Color::hex(0x123456),
            ..Style::default()
        };
        let r = Rect::new(0, 0, 100, 20);
        let disabled_col = crate::theme::current().palette.text_disabled;

        let paint_color = |draw: &dyn Fn(&mut CaptureCanvas)| {
            let mut cv = CaptureCanvas {
                last_text_color: std::cell::Cell::new(None),
            };
            draw(&mut cv);
            cv.last_text_color.get()
        };

        // Label：启用取 style.fg，禁用取 text_disabled。
        let label = Label::new("x".into());
        assert_eq!(
            paint_color(&|cv| label.paint(r, r, false, true, cv, &style)),
            Some(Color::hex(0x123456))
        );
        assert_eq!(
            paint_color(&|cv| label.paint(r, r, false, false, cv, &style)),
            Some(disabled_col)
        );

        // DynLabel：独立覆盖（不依赖"共用同一函数"的隐含推理）。
        let dl = DynLabel::new(crate::signal::signal(String::from("y")));
        assert_eq!(
            paint_color(&|cv| dl.paint(r, r, false, true, cv, &style)),
            Some(Color::hex(0x123456))
        );
        assert_eq!(
            paint_color(&|cv| dl.paint(r, r, false, false, cv, &style)),
            Some(disabled_col)
        );
    }

    #[test]
    fn hover_leave_reaches_interactive_container_with_child() {
        // 回归：可点击容器内有子节点（命中返回最深子节点）时，hover 移开容器后容器本身
        // 仍须收到 Leave——否则带 label 的表格单元格点击后高亮卡住（"点击过的一直高亮"）。
        use crate::core::{EventCtx, Widget};
        use crate::event::{Event, MouseButton, PointerEvent, PointerKind};
        use crate::geometry::Point;
        use std::cell::Cell as StdCell;
        use std::rc::Rc;
        struct LeaveProbe(Rc<StdCell<u32>>);
        impl Widget for LeaveProbe {
            fn on_event(&mut self, _ctx: &mut EventCtx, ev: &Event) -> bool {
                if let Event::Pointer(p) = ev {
                    if p.kind == PointerKind::Leave {
                        self.0.set(self.0.get() + 1);
                    }
                }
                false
            }
        }
        let leaves = Rc::new(StdCell::new(0u32));
        // A：带子 label 的容器（探针）；B：相邻普通块。
        let ui = Element::row()
            .fill()
            .child(
                Element::stack()
                    .width(50)
                    .height(50)
                    .widget(LeaveProbe(leaves.clone()))
                    .child(Element::label("x").fill()),
            )
            .child(Element::leaf().width(50).height(50));
        let mut tree = Tree::new();
        let root = ui.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(100, 50), &mut crate::text::NullTextEngine);
        let (mut hover, mut capture) = (None, None);
        // 移到 A（命中其子 label），再移到 B → A 容器应收到 Leave。
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, Point::new(25, 25), MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, Point::new(75, 25), MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        assert!(
            leaves.get() >= 1,
            "hover 移开容器后容器应收到 Leave，实得 {}",
            leaves.get()
        );
    }

    #[test]
    fn grid_chunks_items_into_rows_and_pads_last() {
        // 5 项 2 列 → 3 行；末行 1 真项 + 1 空占位，列数对齐。
        let items: Vec<Element> = (0..5).map(|_| Element::label("x")).collect();
        let tree = layout(Element::grid(2, 8, items));
        let root = tree.root.unwrap();
        let rows = tree.get(root).unwrap().children.clone();
        assert_eq!(rows.len(), 3, "5 项 2 列应分 3 行");
        assert_eq!(
            tree.get(rows[0]).unwrap().children.len(),
            2,
            "整行应有 2 个单元格"
        );
        assert_eq!(
            tree.get(rows[2]).unwrap().children.len(),
            2,
            "末行应补空占位到 2 列"
        );
    }

    #[test]
    fn table_builds_header_divider_and_scroll_body() {
        // table → col[header, divider, scroll]；scroll 内每行一个 (row + divider) 包裹。
        let tree = layout(Element::table(
            vec![("A", 1.0), ("B", 1.0)],
            vec![vec!["1", "2"], vec!["3", "4"]],
        ));
        let root = tree.root.unwrap();
        let top = tree.get(root).unwrap().children.clone();
        assert_eq!(top.len(), 3, "表格 = 表头 + 分隔线 + 滚动正文");
        let scroll = top[2];
        assert_eq!(tree.get(scroll).unwrap().children.len(), 2, "正文应有 2 行");
    }

    #[test]
    fn drop_routes_to_widget_under_point() {
        let got: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(Vec::new()));
        let sink = got.clone();
        // 占满窗口的容器挂拖放回调（等价全窗接收）。
        let tree = layout(Element::col().fill().on_drop_files(move |_ctx, paths| {
            sink.borrow_mut().extend_from_slice(paths);
        }));
        let mut tree = tree;
        let res = tree.dispatch_files(
            Point::new(50, 50),
            vec![PathBuf::from("a.txt"), PathBuf::from("b.png")],
        );
        assert!(res.consumed, "落点命中带回调的容器应消费");
        assert_eq!(got.borrow().len(), 2, "回调应收到 2 个文件");
        assert_eq!(got.borrow()[0], PathBuf::from("a.txt"));
    }

    #[test]
    fn drop_ignored_when_no_handler() {
        let mut tree = layout(Element::col().fill());
        let res = tree.dispatch_files(Point::new(50, 50), vec![PathBuf::from("a.txt")]);
        assert!(!res.consumed, "无回调时拖放不消费");
    }

    #[test]
    fn window_drag_hits_caption_not_button() {
        // 标题栏行（window_drag）：左半 Label（非交互）、右侧关闭按钮（可聚焦）。
        let tree = layout(
            Element::row()
                .width_match()
                .height(40)
                .window_drag()
                .child(Element::label("标题").width(120).height(40))
                .child(
                    Element::window_button(WindowButtonKind::Close)
                        .width(46)
                        .height(40),
                ),
        );
        // Label 区域 → 可拖（拖动窗口）。
        assert!(tree.drag_hit_at(Point::new(40, 20)), "标题文字区应为拖动区");
        // 按钮区域 → 不拖（交按钮处理点击）。
        assert!(!tree.drag_hit_at(Point::new(130, 20)), "按钮区不应拖动窗口");
        // 交互命中：按钮区为交互控件（平台据此判 HTCLIENT），拖动区/文字区不是。
        assert!(
            tree.interactive_hit_at(Point::new(130, 20)),
            "按钮区应判为交互控件"
        );
        assert!(
            !tree.interactive_hit_at(Point::new(40, 20)),
            "标题文字区不应判为交互控件"
        );
    }

    #[test]
    fn window_button_click_requests_op() {
        let mut tree = layout(
            Element::window_button(WindowButtonKind::Minimize)
                .width(46)
                .height(40),
        );
        let mut hover = None;
        let mut capture = None;
        let at = Point::new(20, 20);
        tree.dispatch_pointer(
            crate::event::PointerEvent::single(
                PointerKind::Down,
                at,
                crate::event::MouseButton::Left,
            ),
            &mut hover,
            &mut capture,
        );
        let res = tree.dispatch_pointer(
            crate::event::PointerEvent::single(
                PointerKind::Up,
                at,
                crate::event::MouseButton::Left,
            ),
            &mut hover,
            &mut capture,
        );
        assert_eq!(
            res.window_op,
            Some(crate::event::WindowOp::Minimize),
            "最小化按钮点击应请求 Minimize"
        );
    }

    #[test]
    fn tooltip_attaches_to_node_and_resolves_by_hit() {
        // .tooltip(..) 挂到节点上；命中最深节点即可取到其提示文本。
        let tree = layout(
            Element::col().fill().child(
                Element::label("帮助")
                    .width(100)
                    .height(30)
                    .tooltip("说明文本"),
            ),
        );
        let hit = tree.hit_test(Point::new(20, 15)).expect("应命中标签");
        assert_eq!(
            tree.node_tooltip(hit).as_deref(),
            Some("说明文本"),
            "命中节点应取到 tooltip"
        );
        // 根容器未设 tooltip → None。
        assert_eq!(
            tree.node_tooltip(tree.root.unwrap()),
            None,
            "未设 tooltip 的节点应为 None"
        );
    }

    #[test]
    fn drop_skips_disabled_subtree() {
        let got = signal(0u32);
        let sink = got;
        // 回调挂在被禁用的容器上：核心拦截，不触发。
        let mut tree = layout(
            Element::col()
                .fill()
                .disabled(true)
                .on_drop_files(move |_ctx, _paths| sink.set(sink.get() + 1)),
        );
        let res = tree.dispatch_files(Point::new(50, 50), vec![PathBuf::from("a.txt")]);
        assert!(!res.consumed, "禁用子树不接收拖放");
        assert_eq!(got.get(), 0);
    }
}
