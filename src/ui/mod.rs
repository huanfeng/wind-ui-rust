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
pub mod reorder;
pub mod rich;
pub mod segmented;
pub mod select;
pub mod sortable_table;
pub mod stepper;
pub mod text_content;
pub mod window_buttons;

use std::cell::{Cell, RefCell};
use std::path::Path;
use std::rc::Rc;

use crate::anim::{Easing, Transition};
use crate::core::{ClickFn, DropFn, EmptyWidget, EventCtx, Layout, Node, NodeId, Tree, Widget};
use crate::event::{Event, Key, PointerKind};
use crate::geometry::{Color, Insets, Point, Rect, Size};
use crate::render::image::{Fit, Image, VisualState};
use crate::render::{Canvas, Paint};
use crate::signal::Signal;
use crate::spec::{Align, Axis, Dimension};
use crate::style::{Role, Style};
use crate::text::TextEngine;
use crate::theme::{Intent, IntentColors};

pub use image::{ImageContent, ImageView};
pub use inputs::{CheckBox, CheckBoxSize, RadioButton, Slider, Switch, SwitchSize, TextInput};
pub use link::Link;
pub use list::ListRow;
pub use nav::{AccordionHeader, CollapsibleHeader, ExpandState, NavRow};
pub use progress::ProgressBar;
pub use reorder::{CommitMode, DragHandle, ReorderList};
pub use rich::{Para, RichColor, RichDoc, RichText, SpanStyle};
pub use segmented::SegmentedControl;
pub use select::{CheckMenu, CheckMenuItem, Dropdown, DropdownItem};
pub use sortable_table::{SortKey, SortStyle};
pub use stepper::Stepper;
pub use text_content::TextContent;
pub use window_buttons::{WindowButton, WindowButtonKind};

/// 图标与文字之间的间距（Button 等）。
const ICON_GAP: i32 = 6;

/// 表格单元格内边距（横/纵，px）与可点击单元格高亮圆角。内边距在单元格内部，
/// 使可点击单元格填满整格、hover 高亮覆盖整格（而非仅贴着文字）。
const TABLE_CELL_PAD_X: i32 = 14;
const TABLE_CELL_PAD_Y: i32 = 9;
const TABLE_HEADER_PAD_Y: i32 = 10;
const TABLE_CELL_CORNER: f32 = 4.0;

/// 文本溢出时的省略方式。对 [`Label`] 生效（静态文案与绑信号的一并适用）；
/// 配合 `.max_lines(1)` 使用最为常见。
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Truncate {
    #[default]
    None, // 裁剪（默认行为）
    End,    // text…（最常用）
    Start,  // …text
    Middle, // te…xt
}

/// 表格排序方向。与列下标一起构成 [`SortKey`]，配合 [`Element::table_sortable`]
/// 的受控排序状态 `Signal<Option<SortKey>>` 使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    /// 升序（数值从小到大 / 字符串字典序）。
    Asc,
    /// 降序。
    Desc,
}

/// 文本控件（[`Label`]）前景色：启用取样式解析色；禁用统一降为 `text_disabled`，
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

/// 截断缓存键值：`(文案, content_w, fsize_bits, 截断串, 是否发生了截断)`。
type TruncCacheEntry = (String, i32, u32, String, bool);

/// 文本叶子控件。
///
/// 文案是 [`TextContent`]：传 `&str`/`String` 得到固定文案，传 `Signal<String>` 则文案
/// 随信号变化（旧名 `DynLabel` 即此情形，已并入本类型）。
pub struct Label {
    text: TextContent,
    /// 最大显示行数；超出部分按 `truncate` 处理（`None` = 不限）。
    pub max_lines: Option<usize>,
    /// 溢出省略方式（仅 `max_lines = Some(1)` 单行时精确截断；多行仅高度裁剪）。
    pub truncate: Truncate,
    /// 截断结果缓存 `(文案, content_w, fsize_bits) → (截断串, 是否发生了截断)`。
    /// 文案进缓存键是为绑了信号的 Label：它的文案会变，不入 key 就会一直画上一次的截断串。
    trunc_cache: RefCell<Option<TruncCacheEntry>>,
    /// 多行限行下文本是否溢出（排版高度超过 `max_lines` 封顶值），measure 期记下。
    ///
    /// 单行截断在 paint 期精确算（还要拼省略号），多行则只做高度裁剪、不重排文本，
    /// 故它的"截没截"只能在 measure 那一刻比出来——那里恰好已经有完整排版高度与封顶值。
    multiline_overflow: Cell<bool>,
}

impl Label {
    pub fn new(text: impl Into<TextContent>) -> Self {
        Self {
            text: text.into(),
            max_lines: None,
            truncate: Truncate::None,
            trunc_cache: RefCell::new(None),
            multiline_overflow: Cell::new(false),
        }
    }

    /// 计算截断后显示串（含省略号）及是否实际发生了截断；结果会被 paint 缓存，通常只算一次。
    fn compute_truncated(
        &self,
        s: &str,
        canvas: &mut dyn Canvas,
        ts: &crate::text::TextStyle,
        avail_w: i32,
    ) -> (String, bool) {
        let total_w = canvas.measure_text(s, ts).w;
        if total_w <= avail_w {
            return (s.to_string(), false);
        }
        let ew = canvas.measure_text("…", ts).w;
        let avail = (avail_w - ew).max(0);
        let chars: Vec<char> = s.chars().collect();
        let n = chars.len();
        // 前缀累计宽度表（O(N) 次 measure，之后 partition_point 二分）。
        let mut widths = vec![0i32; n + 1];
        let mut acc = String::new();
        for (i, &c) in chars.iter().enumerate() {
            acc.push(c);
            widths[i + 1] = canvas.measure_text(&acc, ts).w;
        }
        let out = match self.truncate {
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
        };
        (out, true)
    }
}

impl Widget for Label {
    fn measure(&self, avail: Size, style: &Style, text: &mut dyn TextEngine) -> Size {
        // 绑了信号时这里现取当前值——文案变化因此**自然改变测量宽度**，无需控件自己
        // 比对版本号：写信号已经把本帧顶成整窗帧（见 `Signal` 的失效通道），而整窗帧
        // 必先 `layout_root` 重新 measure。
        let s = self.text.resolve();
        // 在可用宽度内换行：宽度受限时折行，宽松时单行。
        // 已知限制：换行准确仅保证于显式宽度的 Label（width/width_match/weight）；
        // 纯 Wrap 宽度的多行 Label，draw 会在收敛后的窄宽重新换行，可能与 measure 行数不符。
        let max_w = if avail.w > 0 {
            Some(avail.w as f32)
        } else {
            None
        };
        let full = text.measure(&s, &crate::text::TextStyle::of(style), max_w);
        if let Some(max_n) = self.max_lines {
            let line_h = text
                .measure("Ay", &crate::text::TextStyle::of(style), None)
                .h
                .max(1);
            let cap = max_n as i32 * line_h;
            // 排版高度超过封顶值 = 有内容被裁掉。记在这里而不是 paint 期：多行路径只做
            // 高度裁剪、不重排文本，paint 拿不到"完整排版有多高"这个数。
            self.multiline_overflow.set(full.h > cap);
            Size::new(full.w, full.h.min(cap))
        } else {
            self.multiline_overflow.set(false);
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
        let s = self.text.resolve();
        // 文字属性打包传递：字重与行高随 `Style` 自动带上，不必在每个调用点重列。
        let ts = &crate::text::TextStyle::of(style);
        let fsize = ts.size;
        // 禁用态：文字降为 text_disabled，使整行（含标签/说明）随容器禁用统一置灰。
        let fg = text_fg(enabled, style, &crate::theme::current());

        // max_lines：计算限高矩形；DirectWrite 高度始终为 f32::MAX，必须用 clip_rect 裁剪。
        let (paint_rect, need_clip) = if let Some(max_n) = self.max_lines {
            let line_h = canvas.measure_text("Ay", ts).h.max(1);
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
        if self.truncate != Truncate::None && self.max_lines == Some(1) && !s.is_empty() {
            let key_w = content.w;
            let key_f = fsize.to_bits();
            let cached: Option<(String, bool)> = {
                let c = self.trunc_cache.borrow();
                c.as_ref().and_then(|(ks, cw, cf, out, t)| {
                    if ks.as_str() == s.as_ref() && *cw == key_w && *cf == key_f {
                        Some((out.clone(), *t))
                    } else {
                        None
                    }
                })
            };
            let (text_str, _truncated) = if let Some(hit) = cached {
                hit
            } else {
                let (out, t) = self.compute_truncated(&s, canvas, ts, content.w);
                *self.trunc_cache.borrow_mut() =
                    Some((s.to_string(), key_w, key_f, out.clone(), t));
                (out, t)
            };
            canvas.draw_text(&text_str, paint_rect, fg, style.text_align, ts);
        } else {
            canvas.draw_text(&s, paint_rect, fg, style.text_align, ts);
        }

        if need_clip {
            canvas.restore();
        }
    }
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        Some(self)
    }
    fn text_truncated(&self) -> Option<bool> {
        // 不限行就不会被裁，交回 None 表示"这个问题对我不适用"。
        let max_n = self.max_lines?;
        // 单行 + 省略模式：paint 期按实际宽度精确算过（还拼了省略号），读那份缓存。
        if max_n == 1 && self.truncate != Truncate::None {
            return Some(
                self.trunc_cache
                    .borrow()
                    .as_ref()
                    .map(|(_, _, _, _, t)| *t)
                    .unwrap_or(false),
            );
        }
        // 其余限行情形（多行，或单行但只裁不加省略号）：看 measure 期比出的高度溢出。
        Some(self.multiline_overflow.get())
    }
}

/// 旧名：绑定 `Signal<String>` 的只读文本控件。已并入 [`Label`]——文案的动态性现在
/// 由字段类型 [`TextContent`] 承担，不再需要一个孪生 widget 类型。
#[deprecated(
    since = "0.12.0",
    note = "并入 `Label`：`Label::new(signal)` 与旧 `DynLabel::new(signal)` 等价，且 max_lines()/truncate() 等修饰符不再需要按两种类型分别 downcast"
)]
pub type DynLabel = Label;

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
    label: TextContent,
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
/// `OutlineSoft`：柔和描边——静默态中性灰边（`palette.border`，与 dropdown/输入框
/// 的 field 边框同源）+ 意图色文字；hover/press 边框转意图主色（同 field 控件的
/// hover 反馈）。适合成排次级按钮（全蓝描边过于喧闹时）。
#[derive(PartialEq, Eq, Clone, Copy)]
pub enum ButtonVariant {
    Solid,
    Outline,
    OutlineSoft,
}

impl Button {
    pub fn new(label: impl Into<TextContent>) -> Self {
        Self {
            label: label.into(),
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
        // 绑了信号的按钮在这里现取当前文案，故换字必然改变按钮宽度——点击已把本帧
        // 顶成整窗帧（`DamageReq::Layout`），整窗帧必先 layout_root 重新 measure。
        let s = text.measure(
            self.label.resolve().as_ref(),
            &crate::text::TextStyle::of(style),
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
        let label = self.label.resolve();
        let t = crate::theme::current();
        let (pal, bt) = (&t.palette, &t.button);
        let vstate = self.visual_state(enabled);
        // intent 解析：Primary 走 ButtonTheme（保持全局换肤 + style.bg 单点覆盖），其余由 palette 派生。
        let is_primary = matches!(self.intent, Intent::Primary);
        let is_outline = matches!(
            self.variant,
            ButtonVariant::Outline | ButtonVariant::OutlineSoft
        );
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
        // 显式 .fg(color) 覆盖优先；显式 .fg_role(非默认角色) 次之；否则填充按钮用意图对比
        // 前景 ic.fg（on_accent，跟随主题，保证蓝底白字）。
        // 注：Style::default() 的 fg_role 为 Some(Role::Text)（为让 Label/CheckBox 等跟随主题），
        // 填充按钮须把这个「默认角色」视作未覆盖、回落到 ic.fg，否则蓝底会被刷成深色文字。
        let fg = if vstate == VisualState::Disabled {
            pal.text_disabled
        } else if is_outline {
            outline_col
        } else if style.fg_role.is_none() {
            style.fg
        } else if style.fg_role != Some(Role::Text) {
            style.resolved_fg(&t)
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
        // OutlineSoft：静默态中性灰边（track，比卡片 border 深一档、清晰可辨），
        // hover/press 转意图主色反馈；禁用边降为 border（最浅）——保证「正常态边框
        // 深于禁用态」的层级不反转（text_disabled 比 border/track 深，不能作禁用边）。
        if is_outline {
            let is_soft = self.variant == ButtonVariant::OutlineSoft;
            let border = if vstate == VisualState::Disabled {
                if is_soft {
                    pal.border
                } else {
                    pal.text_disabled
                }
            } else if is_soft && self.state == BtnState::Normal {
                pal.track
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
                label.as_ref(),
                bounds,
                fg,
                Align::Center,
                &crate::text::TextStyle::of(style),
            );
            return;
        };
        // 有图标：图标 + 文字作为整体水平居中，图标在左、垂直居中。
        let ts = canvas.measure_text(label.as_ref(), &crate::text::TextStyle::of(style));
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
            label.as_ref(),
            text_rect,
            fg,
            Align::Start,
            &crate::text::TextStyle::of(style),
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
    /// 最小宽度下界（None=无）：配合 Wrap 宽实现自适应但不小于该值，见 [`Element::min_width`]。
    min_width: Option<i32>,
    max_width: Option<i32>,
    /// 最大高度上界（None=无）：配合 Wrap 高实现"短则收缩、长则封顶"，见 [`Element::max_height`]。
    max_height: Option<i32>,
    padding: Insets,
    margin: Insets,
    align: Option<Align>,
    weight: Option<f32>,
    layout: Layout,
    style: Style,
    widget: Box<dyn Widget>,
    /// 是否已挂过真正的 widget（`base` 里那个 `EmptyWidget` 占位不算）。
    /// 用独立标志而非"探测 widget 是不是空"，是因为后者只能靠 `as_any_mut()`
    /// 这类可选实现来猜——自定义 widget 不实现它就漏判。
    has_widget: bool,
    children: Vec<Element>,
    visible: bool,
    vis_signal: Option<Signal<bool>>,
    vis_cond: Option<Box<dyn Fn() -> bool>>,
    clip_children: bool,
    click: Option<ClickFn>,
    on_drop: Option<DropFn>,
    context_menu: Option<crate::core::MenuFn>,
    window_drag: bool,
    focusable: Option<bool>,
    enabled_static: bool,
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
            min_width: None,
            max_width: None,
            max_height: None,
            padding: Insets::default(),
            margin: Insets::default(),
            align: None,
            weight: None,
            layout,
            style: Style::default(),
            widget: Box::new(EmptyWidget),
            has_widget: false,
            children: Vec::new(),
            visible: true,
            vis_signal: None,
            vis_cond: None,
            clip_children: false,
            click: None,
            on_drop: None,
            context_menu: None,
            window_drag: false,
            focusable: None,
            enabled_static: true,
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
    ///
    /// 文案收 [`TextContent`]：传 `&str`/`String` 是固定文案，传 `Signal<String>` 则
    /// 文案随信号变化（写信号即换字，连宽度一并重新测量）。
    ///
    /// ```
    /// use windui::prelude::*;
    /// let status = signal(String::from("就绪"));
    /// let ui = Element::label(status);
    /// status.set(String::from("同步中…")); // 下一帧显示新文案
    /// ```
    pub fn label(text: impl Into<TextContent>) -> Self {
        Self::base(Layout::None).widget(Label::new(text))
    }

    /// 动态标签（绑定 `Signal<String>`，只读显示）。
    ///
    /// 等价于 `Element::label(signal)`——自 [`TextContent`] 起，所有收文案的构造器都
    /// 直接接受 `Signal<String>`，本构造器只是它出现之前就存在的显式写法，保留不动。
    pub fn label_signal(text: Signal<String>) -> Self {
        Self::label(text)
    }

    /// 改名为 [`Element::label_signal`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `label_signal`：参数早已是 Signal，`_rc` 是 Rc 时代的化石，会让人以为要传 Rc 而绕开它"
    )]
    pub fn label_rc(text: Signal<String>) -> Self {
        Self::label_signal(text)
    }

    /// 胶囊徽章/标签（如版本号 `v0.0.0-alpha`、状态 `新`）：小字号 + pill 圆角 +
    /// 意图色淡底 + 意图色文字。默认 Primary（强调色）。颜色在构造期据当前主题解析。
    pub fn badge(text: impl Into<TextContent>) -> Self {
        Self::badge_intent(text, Intent::Primary)
    }

    /// 指定语义意图的徽章（Primary=强调蓝 / Neutral=灰 / Danger=红 / Custom=自定义基色）。
    /// 内置三意图的颜色走主题角色延迟解析（运行期换主题自动跟随）；
    /// `Custom` 为调用方给定的固定基色，本就不随主题——要随主题请传
    /// [`Intent::CustomRole`]。
    pub fn badge_intent(text: impl Into<TextContent>, intent: Intent) -> Self {
        use crate::style::Role;
        let shell = Element::row()
            .cross(Align::Center)
            .padding_xy(9, 3)
            .corner(999.0);
        let label = Element::label(text).font_size(12.0).font_weight(600);
        match intent {
            Intent::Primary => shell
                .bg_role_alpha(Role::Accent, 0.15)
                .child(label.fg_role(Role::Accent)),
            // Neutral 前景用 text_muted（够深可读）——不能用浅灰 border 当字色，
            // 灰字灰底几乎看不清（badge_colors 已修过的同一个坑，此处对齐）。
            Intent::Neutral => shell
                .bg_role_alpha(Role::TextMuted, 0.15)
                .child(label.fg_role(Role::TextMuted)),
            Intent::Danger => shell
                .bg_role_alpha(Role::Danger, 0.15)
                .child(label.fg_role(Role::Danger)),
            Intent::Success => shell
                .bg_role_alpha(Role::Success, 0.15)
                .child(label.fg_role(Role::Success)),
            Intent::Warning => shell
                .bg_role_alpha(Role::Warning, 0.15)
                .child(label.fg_role(Role::Warning)),
            Intent::Custom(c) => shell.bg(c.scale_alpha(0.15)).child(label.fg(c)),
            // CustomRole 跟内置意图同路：淡底与前景都走角色延迟解析，故也跟随换主题。
            Intent::CustomRole(r) => shell.bg_role_alpha(r, 0.15).child(label.fg_role(r)),
        }
    }

    /// Label 专属配置入口。静态文案与信号绑定的 label 现已是同一个 widget 类型，
    /// 故只需一次 downcast（合并前要先试 `Label` 再试 `DynLabel`）。
    #[track_caller]
    fn config_label(mut self, f: impl FnOnce(&mut Label)) -> Self {
        if let Some(a) = self.widget.as_any_mut() {
            if let Some(l) = a.downcast_mut::<Label>() {
                f(l);
                return self;
            }
        }
        debug_assert!(
            false,
            "max_lines()/truncate() 只能用于 Element::label(..) / label_signal(..)"
        );
        self
    }

    /// 限制显示行数（超出高度裁剪；配合 `.truncate()` 可在末行加省略号）。
    /// 静态文案与绑信号的 label 一并适用。
    #[track_caller]
    pub fn max_lines(self, n: usize) -> Self {
        self.config_label(|l| l.max_lines = Some(n))
    }

    /// 文本溢出省略方式（`max_lines(1)` 时精确截断，多行仅高度裁剪）。
    /// 静态文案与绑信号的 label 一并适用。
    #[track_caller]
    pub fn truncate(self, mode: Truncate) -> Self {
        self.config_label(|l| l.truncate = mode)
    }

    /// 交互按钮。配合 `.on_click(...)` 设置回调。
    ///
    /// 文案收 [`TextContent`]：传 `Signal<String>` 即得"切换类按钮"（播放/暂停、
    /// 展开/收起、隐藏已完成/显示全部），点击时改信号，按钮上的字与按钮宽度一起更新。
    ///
    /// ```
    /// use windui::prelude::*;
    /// let playing = signal(false);
    /// let caption = signal(String::from("播放"));
    /// let btn = Element::button(caption).on_click(move |_| {
    ///     let next = !playing.get();
    ///     playing.set(next);
    ///     caption.set(String::from(if next { "暂停" } else { "播放" }));
    /// });
    /// ```
    pub fn button(label: impl Into<TextContent>) -> Self {
        Self::base(Layout::None).widget(Button::new(label))
    }

    /// 纯图标按钮（字形）：无文字、方形、hover/press 圆底 + 点击/键盘激活 + 手型光标。
    /// 用于 ⓘ 信息、▲▼ 调序、× 关闭等工具图标。字形随 `.fg()` 取色，`.size()` 调尺寸，
    /// `.tooltip()` 加说明。配合 `.on_click(...)` 设回调。
    pub fn icon_button(glyph: impl Into<TextContent>) -> Self {
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
    #[track_caller]
    pub fn clickable(mut self) -> Self {
        debug_assert!(
            self.widget.as_any_mut().is_none(),
            "clickable() 仅用于容器（col/row/stack），不能叠加在叶子控件上"
        );
        self.set_widget(Box::new(containers::Clickable::new()));
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
    /// 项**每次右击现取现建**，且粘滞项（[`MenuItem::stay_open`](crate::event::MenuItem::stay_open)，
    /// 即菜单内的复选开关）点击后会原地重跑本构建器刷新勾选态，故 `build` 须为 `Fn`
    /// （可重入），捕获的可变状态放 `Cell`/`RefCell`/`Signal`。
    ///
    /// `build` 是**生成器**而非事件回调（"每次要用时重建"，不是"发生了什么之后调"），
    /// 因此不收 `&mut EventCtx`——它的产出是一份数据。要在菜单里做事的是各项的**动作**，
    /// 那里有 ctx（见 [`MenuItem::run`](crate::event::MenuItem::run)）。
    ///
    /// ⚠ 挂了菜单的节点会**吞命中**（同 `on_drop`/`tooltip`）：透明的纯布局容器一旦挂上
    /// 就开始拦截指针事件、遮住其下内容。挂在本就吞命中的节点上（有背景 / `clickable()` /
    /// 真实控件）。表格数据行用 [`on_row_context_menu`](Self::on_row_context_menu)。
    pub fn on_context_menu(
        mut self,
        build: impl Fn() -> Vec<crate::event::MenuItem> + 'static,
    ) -> Self {
        self.context_menu = Some(Rc::new(build));
        self
    }

    /// Tab 焦点环参与度覆盖（节点级，适用于任意控件）：`false` 强制退出焦点环、
    /// `true` 强制加入。典型场景：词典正文含可折叠区默认可聚焦，但应用希望焦点
    /// 常驻搜索框——`.focusable(false)` 退出。**仅影响 Tab 遍历**，不改变命中
    /// 测试、鼠标交互与 `request_focus`。
    pub fn focusable(mut self, on: bool) -> Self {
        self.focusable = Some(on);
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
    pub fn link(text: impl Into<TextContent>) -> Self {
        Self::base(Layout::None).widget(link::Link::new(text))
    }

    /// 配置内含的 Link。`url()/underline()` 是 link 专属修饰符，链到其他控件属误用——
    /// debug 构建下 panic 提示，release 下静默忽略（与 text_input/image 的误用检测一致）。
    #[track_caller]
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
    #[track_caller]
    pub fn url(self, url: impl Into<String>) -> Self {
        let url = url.into();
        self.config_link(move |l| l.set_url(url))
    }
    /// 是否绘制链接下划线（默认开）。
    #[track_caller]
    pub fn underline(self, on: bool) -> Self {
        self.config_link(move |l| l.set_underline(on))
    }

    // ---- 富文本 ----

    /// 富文本控件：span 数据模型 + 行内流式布局（同行混字号基线对齐、CJK/Latin
    /// 断行、胶囊标签、可折叠 Section）。文档经 [`RichDoc`] builder 构造：
    ///
    /// ```ignore
    /// Element::rich(
    ///     RichDoc::new()
    ///         .style("headword", SpanStyle::new().size(26.0).bold())
    ///         .style("pos", SpanStyle::new().size(11.0).chip())
    ///         .para(Para::new().styled("headword", "apple").text("  ")
    ///             .styled("pos", "n."))
    ///         .section("例句", collapsed_signal, |s| s.para("An apple a day…")),
    /// )
    /// ```
    pub fn rich(doc: RichDoc) -> Self {
        Self::base(Layout::None).widget(rich::RichText::new(doc))
    }

    /// 动态富文本：绑定 `Signal<RichDoc>`，信号变化时整篇换文档（词典切词条）。
    /// 布局缓存与选区随之失效；折叠/clamp 的 Signal 在应用侧持有，跨词条是否
    /// 复位由应用决定。范式同 [`Element::label_signal`]。
    pub fn rich_signal(doc: Signal<RichDoc>) -> Self {
        Self::base(Layout::None)
            .widget(rich::RichText::new_dynamic(doc))
            .reactive()
    }

    /// 改名为 [`Element::rich_signal`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `rich_signal`：参数早已是 Signal，`_rc` 是 Rc 时代的化石，会让人以为要传 Rc 而绕开它"
    )]
    pub fn rich_rc(doc: Signal<RichDoc>) -> Self {
        Self::rich_signal(doc)
    }

    /// RichText 专属配置入口（误用检测同 text_input/link）。
    #[track_caller]
    fn config_rich(mut self, f: impl FnOnce(&mut rich::RichText)) -> Self {
        match self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<rich::RichText>())
        {
            Some(r) => f(r),
            None => debug_assert!(
                false,
                "on_span_click()/copy_menu() 只能用于 Element::rich(..)"
            ),
        }
        self
    }

    /// 富文本 span 点击回调：文档中经 [`rich::Para::span_id`]/`styled_id` 标注 id 的
    /// 文字被点击时触发，携带该 id（词典交叉引用跳转）。未标 id 的文字不响应、
    /// 不显示手型。回调挂控件层，`RichDoc` 保持纯数据可 Clone。
    ///
    /// 签名 `FnMut(&mut EventCtx, &str)`——`ctx` 恒在首位（全库一致），其后才是这个
    /// 回调真正关心的数据。
    #[track_caller]
    pub fn on_span_click(self, f: impl FnMut(&mut EventCtx, &str) + 'static) -> Self {
        self.config_rich(move |r| r.set_on_span_click(Box::new(f)))
    }

    /// 富文本内建右键「复制全部」菜单开关（默认开）。应用要挂自定义
    /// `on_context_menu` 时先关掉它，避免内建菜单抢占右键。
    #[track_caller]
    pub fn copy_menu(self, on: bool) -> Self {
        self.config_rich(move |r| r.set_copy_menu(on))
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
    /// 图片控件：从 SVG 字节光栅化（`svg` feature）。加载失败显示占位框。
    ///
    /// `target_width=None`（**推荐**）为 **DPI 感知**：SVG 固有尺寸即逻辑尺寸，
    /// paint 期按实际物理尺寸光栅化，任何 DPI 下都 1:1 落像素。`Some(w)` 写死光栅
    /// 宽（逻辑尺寸随之为 `w` dp），在物理宽 ≠ `w` 的 DPI 下要经一次重采样。
    #[cfg(feature = "svg")]
    pub fn image_svg(bytes: &[u8], target_width: Option<u32>) -> Self {
        Self::base(Layout::None).widget(ImageView::from_content(ImageContent::from_svg_bytes(
            bytes,
            target_width,
        )))
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
    #[track_caller]
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
    #[track_caller]
    pub fn fit(self, fit: Fit) -> Self {
        self.config_image(|iv| iv.set_fit(fit))
    }
    /// 图片模板着色（单色图标随颜色变色）。
    #[track_caller]
    pub fn tint(self, color: Color) -> Self {
        self.config_image(|iv| iv.set_tint(color))
    }

    /// 给按钮设置前置图标（嵌入字节）。链到非按钮属误用——debug panic，release 忽略。
    #[track_caller]
    pub fn icon_bytes(self, bytes: &[u8]) -> Self {
        self.config_button_icon(ImageContent::from_bytes(bytes))
    }
    /// 给按钮设置前置图标（**运行期读文件**）。
    ///
    /// 名字带 `_file` 而不叫 `icon`：这一族里 `icon_bytes` / `icon_rgba` / `icon_svg` /
    /// `icon_content` 都是纯内存操作，只有它会碰文件系统、也只有它会在路径写错或文件
    /// 缺失时失败（失败即无图标，不 panic）。最短的名字给了唯一可能失败的形态，
    /// 会被当成"通用图标入口"误用。
    #[track_caller]
    pub fn icon_file(self, path: impl AsRef<Path>) -> Self {
        self.config_button_icon(ImageContent::from_file(path))
    }
    /// 改名为 [`Element::icon_file`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `icon_file`：`icon` 是这一族里唯一读文件、唯一可能失败的形态，最短的名字给它容易被误当成通用入口"
    )]
    #[track_caller]
    pub fn icon(self, path: impl AsRef<Path>) -> Self {
        self.icon_file(path)
    }
    /// 给按钮设置前置图标（原始非预乘 RGBA8）。
    #[track_caller]
    pub fn icon_rgba(self, w: u32, h: u32, rgba: &[u8]) -> Self {
        self.config_button_icon(ImageContent::from_rgba(w, h, rgba))
    }
    /// 给按钮设置前置图标（SVG 字节，`svg` feature）。`target_width` 同 [`Element::image_svg`]。
    #[cfg(feature = "svg")]
    #[track_caller]
    pub fn icon_svg(self, bytes: &[u8], target_width: Option<u32>) -> Self {
        self.config_button_icon(ImageContent::from_svg_bytes(bytes, target_width))
    }
    /// 给按钮设置前置图标（预组装内容原语，支持状态换图/着色）。
    #[track_caller]
    pub fn icon_content(self, icon: ImageContent) -> Self {
        self.config_button_icon(icon)
    }
    #[track_caller]
    fn config_button_icon(self, icon: ImageContent) -> Self {
        self.config_button(|b| b.set_icon(icon), "icon*()")
    }

    /// 小号变体（Button：紧凑内边距；CheckBox：14px 方框；Switch：36×20 轨道）。
    #[track_caller]
    pub fn small(mut self) -> Self {
        if let Some(a) = self.widget.as_any_mut() {
            if let Some(c) = a.downcast_mut::<CheckBox>() {
                c.set_size(CheckBoxSize::Small);
                return self;
            }
            if let Some(s) = a.downcast_mut::<Switch>() {
                s.set_size(SwitchSize::Small);
                return self;
            }
        }
        self.config_button(|b| b.size = ButtonSize::Small, "small()")
    }

    /// 描边按钮（透明底 + 意图色边框/文字，hover 淡色叠层）。与 `.neutral()/.danger()/.accent()`
    /// 组合可得不同语义的描边按钮（如蓝色"检查更新"、红色"删除"次按钮）。仅 `Element::button(..)` 可用。
    #[track_caller]
    pub fn outline(self) -> Self {
        self.config_button(|b| b.set_variant(ButtonVariant::Outline), "outline()")
    }

    /// 柔和描边按钮（中性灰边 + 意图色文字；hover 边框转意图主色）。
    /// 参考「灰边框、主色文字」的次级按钮惯例，成排放置比全意图色描边安静。
    /// 与 `.neutral()/.danger()/.accent()` 组合同 [`outline`](Self::outline)。
    #[track_caller]
    pub fn outline_soft(self) -> Self {
        self.config_button(
            |b| b.set_variant(ButtonVariant::OutlineSoft),
            "outline_soft()",
        )
    }

    /// 静态启用标志。**适用于任意控件/容器**：核心据此拦事件、跳 Tab、令控件置灰；
    /// 禁用沿父链继承（禁用容器即禁用其全部子节点）。
    ///
    /// 启用轴与可见轴形态一致，按状态来源三选一：
    ///
    /// | | 静态 | 信号 | 闭包 |
    /// |---|---|---|---|
    /// | 启用 | `enabled(bool)` | [`enabled_signal`](Self::enabled_signal) | [`enabled_when`](Self::enabled_when) |
    /// | 可见 | [`visible`](Self::visible) | [`visible_signal`](Self::visible_signal) | [`visible_when`](Self::visible_when) |
    ///
    /// 三者可叠加，取与。`enabled(false)` 与 [`disabled(true)`](Self::disabled) 等价。
    pub fn enabled(mut self, on: bool) -> Self {
        self.enabled_static = on;
        self
    }
    /// 启用信号（绑定 `Signal<bool>`，运行期可切换）。语义同 [`enabled`](Self::enabled)。
    pub fn enabled_signal(mut self, flag: Signal<bool>) -> Self {
        self.enabled = Some(flag);
        self
    }
    /// 启用条件（闭包，运行期求值）。镜像 [`visible_when`](Self::visible_when)，但不影响布局：
    /// 条件为 false 时该元素（及子树）置灰、不可交互，仍占位参与测量/绘制。
    /// 适合设置项联动（如「细节项随开关置灰」），避免隐藏导致的分隔线残留与高度抖动。
    ///
    /// 是 `Fn` 不是 `FnMut`：同 [`visible_when`](Self::visible_when)，它是每帧被反复
    /// 求值的纯谓词，不是"发生了什么之后调一次"的动作回调。
    pub fn enabled_when(mut self, f: impl Fn() -> bool + 'static) -> Self {
        self.en_cond = Some(Box::new(f));
        self
    }
    /// 悬停提示：指针在本元素上停留片刻后，于指针附近弹出说明浮层。
    /// **适用于任意控件/容器**（像 `enabled`，挂在节点上）；命中取最深节点的提示。
    /// 超过 `TOOLTIP_MAX_W`（`app.rs`）自动按宽度换行为多行；调用方仍传一整句
    /// 不含显式换行的文本（含 `\n` 在 debug 下提示，排版结果未做专门测试）。
    #[track_caller]
    pub fn tooltip(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        debug_assert!(!text.contains('\n'), "tooltip 仅支持单行文本");
        self.tooltip = Some(text);
        self
    }

    /// 静态禁用（`true`=禁用）：[`enabled`](Self::enabled) 的取反便捷式，适用于任意控件/容器。
    /// 调用点常读作「这个按钮是禁用的」而非「启用为假」，故两者并存。
    pub fn disabled(self, on: bool) -> Self {
        self.enabled(!on)
    }
    #[track_caller]
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
    pub fn checkbox(label: impl Into<TextContent>, state: Signal<bool>) -> Self {
        Self::base(Layout::None).widget(CheckBox::new(label, state))
    }
    /// 显式设置语义意图色。Button / CheckBox 通用。
    /// 注意：非 Primary intent 接管整组视觉，此时 `.bg()` 单点覆盖不生效。
    #[track_caller]
    pub fn intent(self, i: Intent) -> Self {
        self.config_intent("intent()", i)
    }
    /// 危险意图（主题 danger 红，如"删除数据"）。Button / CheckBox 通用。
    #[track_caller]
    pub fn danger(self) -> Self {
        self.config_intent("danger()", Intent::Danger)
    }
    /// 次要意图（中性灰）。主要用于 Button 的次要按钮。
    #[track_caller]
    pub fn neutral(self) -> Self {
        self.config_intent("neutral()", Intent::Neutral)
    }
    /// 自定义意图基色（扩展点）：框架派生整组视觉。Button / CheckBox 通用。
    ///
    /// 传的是**定色**，运行期换主题不跟随。要跟随主题请用
    /// [`accent_role`](Self::accent_role)（与 `fg` / `fg_role` 同一套成对约定）。
    #[track_caller]
    pub fn accent(self, color: Color) -> Self {
        self.config_intent("accent()", Intent::Custom(color))
    }
    /// 自定义意图基色的主题角色版（扩展点）：基色延迟到绘制时按当前主题解析，
    /// 运行期换主题自动跟随。Button / CheckBox 通用，其余同 [`accent`](Self::accent)。
    ///
    /// 内置意图（`primary`/`neutral`/`danger` 等）已覆盖的语义**不要**改用本方法绕道——
    /// 它是给"想用 palette 里别的色槽当基色"准备的。
    #[track_caller]
    pub fn accent_role(self, role: crate::style::Role) -> Self {
        self.config_intent("accent_role()", Intent::CustomRole(role))
    }
    /// intent 修饰符落点：依次尝试 Button / CheckBox，命中即设；用于其他控件属误用。
    #[track_caller]
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
    pub fn radio(label: impl Into<TextContent>, group: Signal<usize>, index: usize) -> Self {
        Self::base(Layout::None).widget(RadioButton::new(label, group, index))
    }
    /// 滑块（绑定 `Signal<f32>`，值域 0.0..=1.0）。
    pub fn slider(value: Signal<f32>) -> Self {
        Self::base(Layout::None).widget(Slider::new(value))
    }

    #[track_caller]
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
    #[track_caller]
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
    #[track_caller]
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
    #[track_caller]
    pub fn password(self) -> Self {
        self.config_text_input(|c| {
            c.password = true;
            c.multiline = false;
        })
    }
    /// 多行输入（编辑/换行行为见 P4）。
    #[track_caller]
    pub fn multiline(self) -> Self {
        self.config_text_input(|c| c.multiline = true)
    }
    /// 多行软换行开关（仅 multiline 生效）。
    #[track_caller]
    pub fn wrap(self, on: bool) -> Self {
        self.config_text_input(|c| c.wrap = on)
    }

    /// 前置图标字形（如放大镜 `'\u{1F50D}'`）：在输入框左侧留出图标区并绘制，
    /// 文字/光标/点击命中相应右移。搜索框等用。仅 `Element::text_input(..)` 可用。
    #[track_caller]
    pub fn leading_icon(self, glyph: char) -> Self {
        self.config_text_input(|c| c.leading = Some(glyph))
    }

    /// 静态可见标志。不可见的节点本帧不显示、不命中，且**不占布局**
    /// （区别于禁用——见 [`enabled`](Self::enabled) 处的三形态对照表）。
    pub fn visible(mut self, v: bool) -> Self {
        self.visible = v;
        self
    }

    /// 可见信号（绑定 `Signal<bool>`，运行期可切换）。等价于
    /// `visible_when(move || flag.get())`，但省掉闭包、与 [`enabled_signal`](Self::enabled_signal) 对称。
    pub fn visible_signal(mut self, flag: Signal<bool>) -> Self {
        self.vis_signal = Some(flag);
        self
    }

    /// 运行期可见条件：闭包返回 false 时该节点本帧不显示/不命中。
    ///
    /// 契约：闭包**必须是纯函数**（仅读状态、无副作用）。它在每帧的
    /// measure/arrange/paint/hit-test/焦点收集中被多次调用，且帧内值不应变化。
    /// 反复求值正是它必须是 `Fn` 而非 `FnMut` 的原因。
    pub fn visible_when(mut self, f: impl Fn() -> bool + 'static) -> Self {
        self.vis_cond = Some(Box::new(f));
        self
    }

    /// 分段控制器（绑定 `Signal<usize>` 选中索引 + 段标签）：连体多段单选，
    /// 选中段高亮。语义同 `radio` 组，外观更紧凑——适合"二/三选一"切换。
    /// 点击选段、悬停逐段高亮、聚焦后左右方向键移动选中。
    #[track_caller]
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
    pub fn dropdown_signal(options: Signal<Vec<String>>, selected: Signal<usize>) -> Self {
        Self::base(Layout::None).widget(select::Dropdown::new_reactive(options, selected))
    }

    /// 改名为 [`Element::dropdown_signal`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `dropdown_signal`：绑信号的构造统一用 `_signal` 后缀（对齐 list_signal/host_signal），`_reactive` 与标记节点响应式的 `Element::reactive()` 撞概念"
    )]
    pub fn dropdown_reactive(options: Signal<Vec<String>>, selected: Signal<usize>) -> Self {
        Self::dropdown_signal(options, selected)
    }

    /// 富内容下拉：选项支持副标题（两行）、尾随徽章（收起态当前项与展开态列表项均显示）、
    /// 尾随可独立点击图标（如删除该项）。见 [`select::DropdownItem`]。
    pub fn dropdown_items(items: Vec<select::DropdownItem>, selected: Signal<usize>) -> Self {
        Self::base(Layout::None).widget(select::Dropdown::with_items(items, selected))
    }

    /// 响应式富内容下拉：选项列表绑定外部 `Signal<Vec<DropdownItem>>`。
    pub fn dropdown_items_signal(
        items: Signal<Vec<select::DropdownItem>>,
        selected: Signal<usize>,
    ) -> Self {
        Self::base(Layout::None).widget(select::Dropdown::with_items_reactive(items, selected))
    }

    /// 改名为 [`Element::dropdown_items_signal`]。
    #[deprecated(
        since = "0.12.0",
        note = "改名为 `dropdown_items_signal`：绑信号的构造统一用 `_signal` 后缀（对齐 list_signal/host_signal），`_reactive` 与标记节点响应式的 `Element::reactive()` 撞概念"
    )]
    pub fn dropdown_items_reactive(
        items: Signal<Vec<select::DropdownItem>>,
        selected: Signal<usize>,
    ) -> Self {
        Self::dropdown_items_signal(items, selected)
    }

    /// 下拉式复选菜单：外观同 `dropdown`，面板是菜单，项可单独开关。
    /// 项支持开关 / 动作 / 分隔线混排，见 [`select::CheckMenuItem`]。
    ///
    /// 默认点击即关闭（同普通菜单）；要连改多个开关用 [`stay_open`](Self::stay_open)。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// # let (hide, special) = (signal(false), signal(false));
    /// Element::check_menu("列表显示", vec![
    ///     CheckMenuItem::check("隐藏未启用", hide).on_change(|_ctx, v| println!("{v}")),
    ///     CheckMenuItem::check("显示特殊方案", special),
    ///     CheckMenuItem::separator(),
    ///     CheckMenuItem::action("全部展开", |_ctx| {}),
    /// ]).width(132);
    /// ```
    pub fn check_menu(title: impl Into<String>, items: Vec<select::CheckMenuItem>) -> Self {
        Self::base(Layout::None).widget(select::CheckMenu::new(title, items))
    }

    /// 复选菜单粘滞：开关项点击后菜单保持展开、可连点多个，点面板外才收起。
    /// 默认关闭——菜单的通行惯例是「点一下、做一件事、退场」，多数开关一次也只改一个。
    /// 一次要连改多个（如一组显示过滤）才值得打开。动作项不受影响，恒为点击即关。
    /// 仅 `Element::check_menu(..)` 可用。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// # let (a, b) = (signal(false), signal(false));
    /// Element::check_menu("列表显示", vec![
    ///     CheckMenuItem::check("隐藏未启用", a),
    ///     CheckMenuItem::check("显示特殊项", b),
    /// ]).stay_open();
    /// ```
    #[track_caller]
    pub fn stay_open(mut self) -> Self {
        if let Some(m) = self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<select::CheckMenu>())
        {
            m.set_stay_open(true);
        } else {
            debug_assert!(false, "stay_open() 仅适用于 Element::check_menu(..)");
        }
        self
    }

    /// 复选菜单的收起态文案生成器：入参是**已开启**的开关项标签（按声明顺序），
    /// 默认恒显示标题。摘要会改变控件宽度，建议同时 `.width(..)` 固定，
    /// 否则工具栏会随开关增减而抖动。仅 `Element::check_menu(..)` 可用。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// Element::check_menu("列表显示", vec![])
    ///     .summary(|on| match on.len() {
    ///         0 => "列表显示".to_string(),
    ///         n => format!("列表显示 ({n})"),
    ///     })
    ///     .width(132);
    /// ```
    ///
    /// 是**生成器**不是事件回调（每次渲染现算文案），故无 `on_` 前缀、无 `ctx`，
    /// 且必须是 `Fn`（要反复调用）。
    #[track_caller]
    pub fn summary(mut self, f: impl Fn(&[&str]) -> String + 'static) -> Self {
        if let Some(m) = self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<select::CheckMenu>())
        {
            m.set_summary(f);
        } else {
            debug_assert!(false, "summary() 仅适用于 Element::check_menu(..)");
        }
        self
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
    /// Element::list_signal(items, |_| (), |s| Element::label(s))
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
        // 构建初始子元素。圈进作用域交给 widget：首批行的构建期信号（`row_fn` 里现造的、
        // 或行内控件内部造的）才能在第一次重建时随旧行一起回收，否则永久漏一代。
        let mut rows = crate::signal::SignalScope::new();
        let initial: Vec<Self> =
            rows.collect(|| data.get().into_iter().map(|item| row_fn(item)).collect());
        // DynList widget 持有 Rc 副本，信号变更时重建子节点
        let row_fn_clone = row_fn.clone();
        let widget = dyn_list::DynList::with_scope(data, move |item: T| row_fn_clone(item), rows);
        let mut container = Self::scroll().fill();
        container.set_widget(Box::new(widget));
        container.reactive = true;
        for el in initial {
            container.children.push(el);
        }
        container
    }

    /// 响应式动态宿主：同 [`list_signal`](Self::list_signal)，但容器是**普通列容器**（非滚动）——
    /// 子元素按正常 col 布局，`weight`/`fill` 能拿到确定高度。适合"信号变化时整体重建一段
    /// 结构随状态变化的子树"（如列集随类别切换的表格），且内容自带滚动或无需滚动的场景。
    /// （滚动容器按无限高度测量子元素，内含 `weight` 正文的表格会高度崩塌——此时用本方法。）
    pub fn host_signal<T>(data: Signal<Vec<T>>, build_fn: impl Fn(T) -> Self + 'static) -> Self
    where
        T: Clone + 'static,
    {
        let build_fn = std::rc::Rc::new(build_fn);
        // 首批子树的构建期信号交给 widget 的作用域，理由同 `list_signal`。
        let mut rows = crate::signal::SignalScope::new();
        let initial: Vec<Self> =
            rows.collect(|| data.get().into_iter().map(|item| build_fn(item)).collect());
        let build_fn_clone = build_fn.clone();
        let widget = dyn_list::DynList::with_scope(data, move |item: T| build_fn_clone(item), rows);
        let mut container = Self::col().fill();
        container.set_widget(Box::new(widget));
        container.reactive = true;
        for el in initial {
            container.children.push(el);
        }
        container
    }

    /// **可手动排序的列表**：每行前置一个拖动手柄，按住手柄上下拖动即可调整顺序。
    ///
    /// 面向设置类应用——行内可以放任意控件（开关、下拉、按钮），拖拽走独立手柄
    /// 因而不与它们抢事件。行高允许不等（带副标题/徽章的表单行），让位算法按实际
    /// 高度重新堆叠。
    ///
    /// 默认 [`CommitMode::Children`]：内部直接重排子节点，**不重建行**，故行内控件
    /// 状态天然保留。若列表由数据信号驱动，用 [`commit_mode`](Self::commit_mode)
    /// 切到 `Callback`，由应用在回调里更新数据。
    ///
    /// 拖动中按 `Esc` 取消，行动画回到原位且不触发回调。
    ///
    /// # 示例
    /// ```ignore
    /// use windui::prelude::*;
    /// let order = signal(vec![0usize, 1, 2]);
    /// Element::reorder_list(vec![
    ///     form_row("拼音方案", enabled_a),
    ///     form_row("五笔方案", enabled_b),
    /// ])
    /// .on_reorder(move |_ctx, from, to| {
    ///     order.update(|v| { let x = v.remove(from); v.insert(to, x); });
    /// })
    /// ```
    pub fn reorder_list(rows: Vec<Element>) -> Self {
        let ctl = reorder::new_ctl();
        let mut container = Self::col().width_match();
        for row in rows {
            let handle = reorder::handle_element(&ctl);
            container = container.child(
                Self::row()
                    .width_match()
                    .cross(Align::Center)
                    // 手柄在前：与 macOS 设置、VS Code 等一致，也让整列手柄对齐成一条竖线。
                    .child(handle)
                    .child(row.weight(1.0)),
            );
        }
        container.set_widget(Box::new(reorder::ReorderList::new(
            ctl,
            reorder::CommitMode::Children,
        )));
        container.reactive = true;
        container
    }

    /// **数据驱动的可手动排序列表**：行由 `Signal<Vec<T>>` 生成，信号变化即整体重建。
    ///
    /// 与 [`reorder_list`](Self::reorder_list) 的分工：
    ///
    /// - `reorder_list` 面向**固定若干行**，内部直接重排子节点，顺序只活在节点树里；
    ///   应用无法把顺序**推回**控件（「恢复默认」「重新载入配置」这类反向同步做不到）。
    /// - 本方法把顺序的真相源交给数据信号：拖拽只经 [`on_reorder`](Self::on_reorder)
    ///   上报意图，由应用改信号，控件据此重建行。因此反向同步天然成立。
    ///
    /// 手柄的位置由 `row_fn` 的第二个参数交还给调用方，**必须**把它放进返回的元素树里，
    /// 否则该行拖不动。之所以不像 `reorder_list` 那样自动前置：行若有整体选中背景/
    /// 左缘指示条，手柄并排在外会被排除在选中视觉之外；更要紧的是——
    ///
    /// > **手柄不能是 `clickable()` 容器的后代**。`Clickable` 消费 `Down`/`Up`，
    /// > 冒泡在它那里就断了，事件根本到不了列表（见 `Tree::dispatch_pointer` 的
    /// > `consumed → break`）。整行可点的列表请把手柄放进 `stack` 里当同级覆盖层，
    /// > 与可点行并列而非嵌套。
    ///
    /// 提交模式固定为 [`CommitMode::Callback`]——children 归数据管，控件不越权重排。
    ///
    /// # 示例
    /// ```ignore
    /// use windui::prelude::*;
    /// let order = signal(vec!["拼音".to_string(), "五笔".to_string()]);
    /// let o = order;
    /// Element::reorder_list_signal(order, |name, handle| {
    ///     Element::row().width_match().cross(Align::Center)
    ///         .child(handle)
    ///         .child(Element::label(name).weight(1.0))
    /// })
    /// .on_reorder(move |_ctx, from, to| {
    ///     o.update(|v| { let x = v.remove(from); v.insert(to.min(v.len()), x); });
    /// })
    /// ```
    pub fn reorder_list_signal<T: Clone + 'static>(
        data: Signal<Vec<T>>,
        row_fn: impl Fn(T, Element) -> Element + 'static,
    ) -> Self {
        let ctl = reorder::new_ctl();
        let row_fn: std::rc::Rc<dyn Fn(T, Element) -> Element> = std::rc::Rc::new(row_fn);
        let mut container = Self::col().width_match();
        for item in data.get() {
            container = container.child(row_fn(item, reorder::handle_element(&ctl)));
        }
        let source = reorder::signal_rows(data, row_fn, ctl.clone());
        let mut list = reorder::ReorderList::new(ctl, reorder::CommitMode::Callback);
        list.set_source(source);
        container.set_widget(Box::new(list));
        container.reactive = true;
        container
    }

    /// 重排完成回调：`(ctx, 原下标, 新下标)`。顺序未变化时不触发。
    /// 仅 [`Element::reorder_list`] 与 [`Element::reorder_list_signal`] 可用。
    #[track_caller]
    pub fn on_reorder(mut self, f: impl FnMut(&mut EventCtx, usize, usize) + 'static) -> Self {
        match self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<reorder::ReorderList>())
        {
            Some(rl) => rl.set_on_reorder(Box::new(f)),
            None => debug_assert!(false, "on_reorder() 只能用于 Element::reorder_list(..)"),
        }
        self
    }

    /// 提交模式（见 [`CommitMode`]）。[`Element::reorder_list`] 与
    /// [`Element::reorder_list_signal`] 都接受——两者挂的是同一个控件，故设置器不区分。
    ///
    /// 真正该调它的只有 `reorder_list`（默认 [`CommitMode::Children`]，需要改数据驱动时
    /// 换成 [`CommitMode::Callback`]）。`reorder_list_signal` 建出来就是 `Callback`，
    /// **不要**再改回 `Children`：那会让顺序被落实两遍——控件先自行重排 `children`，
    /// 回调改的数据信号紧接着又重建整批行。
    #[track_caller]
    pub fn commit_mode(mut self, mode: reorder::CommitMode) -> Self {
        match self
            .widget
            .as_any_mut()
            .and_then(|a| a.downcast_mut::<reorder::ReorderList>())
        {
            Some(rl) => rl.set_mode(mode),
            None => debug_assert!(
                false,
                "commit_mode() 只能用于 Element::reorder_list(..) / Element::reorder_list_signal(..)"
            ),
        }
        self
    }

    /// 带前置图标的单选列表：`items` 为 (标签, 图标内容) 列表。其余同 `list`。
    /// 图标用 `ImageContent`，可链 `.fit()`/状态换图等；行图标随选中/悬停状态调制。
    pub fn list_icons(
        items: Vec<(impl Into<String>, ImageContent)>,
        selected: Signal<usize>,
    ) -> Self {
        let mut scroll = Self::scroll().fill();
        for (i, (label, icon)) in items.into_iter().enumerate() {
            let row = list::ListRow::new(label.into(), selected, i).icon_content(icon);
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
    pub fn nav_row(label: impl Into<TextContent>) -> Self {
        Self::base(Layout::None)
            .widget(nav::NavRow::new(label))
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
    /// 面板间分隔线。**单开互斥**版——`selected` 共享选中面板下标，`None` = 全收起，
    /// 初值即默认展开项。点击某面板头展开它会自动收起其它面板。
    ///
    /// 与 [`Element::tabs`] 的 `Signal<usize>` 差一个 `Option`：标签页恒有一页选中，
    /// 手风琴可以全收起。
    pub fn accordion(
        selected: Signal<Option<usize>>,
        panels: Vec<(impl Into<String>, Element)>,
    ) -> Self {
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
        let mut items = Vec::new();
        let mut content = Element::stack().fill().weight(1.0);
        for (i, (title, page)) in pages.into_iter().enumerate() {
            items.push(containers::TabItem::new(title.into()));
            let sel2 = selected;
            content = content.child(page.fill().visible_when(move || sel2.get() == i));
        }
        Self::tabs_frame(items, selected, content, containers::TabStyle::Underline)
    }

    /// 带前置图标的标签页：`pages` 为 (标题, 图标内容, 页面) 列表。其余同 `tabs`。
    /// 标签图标随选中/悬停状态调制。
    pub fn tabs_icons(
        selected: Signal<usize>,
        pages: Vec<(impl Into<String>, ImageContent, Element)>,
    ) -> Self {
        let mut items = Vec::new();
        let mut content = Element::stack().fill().weight(1.0);
        for (i, (title, icon, page)) in pages.into_iter().enumerate() {
            items.push(containers::TabItem::new(title.into()).icon_content(icon));
            let sel2 = selected;
            content = content.child(page.fill().visible_when(move || sel2.get() == i));
        }
        Self::tabs_frame(items, selected, content, containers::TabStyle::Underline)
    }

    /// 胶囊式标签页：选中项为 accent 实底圆角胶囊、白字，胶囊在标签间滑动；无基线。
    /// 签名与 [`tabs`](Self::tabs) 一致，仅视觉风格不同。
    pub fn tabs_pill(selected: Signal<usize>, pages: Vec<(impl Into<String>, Element)>) -> Self {
        let mut items = Vec::new();
        let mut content = Element::stack().fill().weight(1.0);
        for (i, (title, page)) in pages.into_iter().enumerate() {
            items.push(containers::TabItem::new(title.into()));
            let sel2 = selected;
            content = content.child(page.fill().visible_when(move || sel2.get() == i));
        }
        Self::tabs_frame(items, selected, content, containers::TabStyle::Pill)
    }

    /// `tabs` / `tabs_icons` / `tabs_pill` 的共同骨架：整条标签条是**一个**
    /// [`containers::TabBar`] 自绘节点（滑动选中滑块要跨标签的布局信息，拆成多节点就
    /// 拿不到），下方是内容区。条高与（下划线风格的）贯穿基线均由 TabBar 自己按主题决定，
    /// 故这里不设固定高、也不额外加 divider。
    fn tabs_frame(
        items: Vec<containers::TabItem>,
        selected: Signal<usize>,
        content: Element,
        style: containers::TabStyle,
    ) -> Self {
        let bar = Element::base(Layout::None)
            .widget(containers::TabBar::new(items, selected).style(style))
            .width_match();
        Element::col().fill().spacing(16).child(bar).child(content)
    }

    /// 模态对话框：全窗半透明遮罩 + 居中内容，遮罩吞掉指针事件实现模态。
    /// `show` 绑定显示标志。
    ///
    /// 无边框窗口下遮罩对**窗口拖动区判定**透明（`ModalScrim::scrim_passthrough`）：
    /// 对话框弹出后自绘标题栏仍可拖窗，窗口按钮则照旧被模态屏蔽。因此 `content`
    /// 面板须自带背景（`bg_role(Role::Surface)` 等），否则面板空白区会穿透遮罩、
    /// 让其下的标题栏被误判成拖动区。
    pub fn dialog(show: Signal<bool>, content: Element) -> Self {
        // 注册到宿主的对话框信号栈，使 ESC / WM_CLOSE 能优先关闭此对话框。
        crate::app::register_modal(show);
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
                    .fg_role(crate::style::Role::Text)
                    .weight(1.0)
                    .height(26),
            )
            .child(
                Element::icon_button("\u{2715}")
                    .size(28, 28)
                    .fg_role(crate::style::Role::TextMuted)
                    .on_click(on_close),
            );
        let panel = Element::col()
            .width(width)
            .bg_role(crate::style::Role::Surface)
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

    /// **表单行**：固定宽的标签列 + 紧随其后的控件。
    ///
    /// 经典表单排布——所有标签占同一宽度的一列，控件左缘因此对齐成一条竖线。
    /// 控件想占满剩余宽度就自己 `.width_match()`（如文本框、滑块），想保持原生尺寸
    /// 就什么都不做（如开关、复选框）。
    ///
    /// 控件要贴到行右缘（设置页那种「标签……开关」的排布）用
    /// [`setting_row`](Self::setting_row)；标签下还要一行说明用
    /// [`setting_row_desc`](Self::setting_row_desc)。
    ///
    /// 行高、标签列宽、间距全部取自 [`FormTheme`]，**不进签名**：同一应用里的表单行
    /// 必须整齐划一，逐行传尺寸只会让每处各写一个近似值。要整体调紧就改主题。
    ///
    /// # 可用的修饰符
    ///
    /// 返回的是拼好的 `row` 容器，**不是挂了 widget 的控件**。因此：
    ///
    /// - **可以**链容器/样式类修饰符：`.padding()` / `.margin_xy()` / `.bg_role()` /
    ///   `.corner()` / `.width()` / `.visible_when()` / `.enabled(false)`（禁用沿父链
    ///   继承，会一并禁掉行内控件）等。
    /// - **不能**链控件专属修饰符：`.intent()` / `.small()` / `.outline()` /
    ///   `.on_click()` 之类要 downcast 到具体 widget，挂到这里在 debug 下会
    ///   `debug_assert` 失败、release 下静默无效。要改控件外观请**加在传进来的
    ///   `control` 上**，那才是真控件。
    ///
    /// # 示例
    /// ```
    /// use windui::prelude::*;
    /// let volume = signal(0.6f32);
    /// let dark = signal(false);
    /// let form = Element::col()
    ///     .child(Element::field("音量", Element::slider(volume).width_match()))
    ///     // 意图色/尺寸加在控件上，不是加在 field 上。
    ///     .child(Element::field("主题", Element::switch(dark).small()));
    /// ```
    ///
    /// [`FormTheme`]: crate::theme::FormTheme
    pub fn field(label: impl Into<String>, control: Element) -> Self {
        let th = crate::theme::current();
        let f = &th.form;
        Element::row()
            .width_match()
            .height(f.row_height())
            .cross(Align::Center)
            .spacing(f.gap())
            .child(Self::form_label(label, f, &th.metrics).width(f.label_width()))
            .child(control)
    }

    /// **设置行**：标签占住左侧，控件贴到行右缘。
    ///
    /// 设置页的主流排布（macOS 系统设置 / Windows 设置皆然）：标签左对齐、控件右对齐，
    /// 中间留白随窗宽伸缩。与 [`field`](Self::field) 的差别只有这一点——那边是
    /// 固定标签列、控件紧跟标签；这里标签块吃掉剩余宽度，把控件推到右边。
    ///
    /// 行高同样取 [`FormTheme::row_height`]——放开关的行与放下拉的行必须一样高，
    /// 否则整列参差不齐。需要两行文字请用 [`setting_row_desc`](Self::setting_row_desc)，
    /// 那一种才按内容撑高。
    ///
    /// 可用/不可用的修饰符与 [`field`](Self::field) 完全相同，见那里的说明。
    ///
    /// # 示例
    /// ```
    /// use windui::prelude::*;
    /// let hide_bar = signal(false);
    /// let row = Element::setting_row("隐藏状态栏", Element::switch(hide_bar));
    /// ```
    ///
    /// [`FormTheme::row_height`]: crate::theme::FormTheme::row_height
    pub fn setting_row(label: impl Into<String>, control: Element) -> Self {
        Self::setting_row_inner(label.into(), None, control)
    }

    /// 带副标题的[设置行](Self::setting_row)：标签下方再排一行小号弱化说明。
    ///
    /// 这一种**不定高**：高度由内容加 [`FormTheme::row_pad_y`](crate::theme::FormTheme::row_pad_y)
    /// 撑出来。副标题长短不一，定高只会把它挤出去。
    ///
    /// 做成独立构造器而非给 `setting_row` 加一个 `Option` 参数：`Option<impl Into<String>>`
    /// 的 `None` 推断不出类型（调用方得写 `None::<&str>`），而退成 `Option<&str>` 又会让
    /// 这一个参数破例不收 `String`。两个各自说得清的签名比一个别扭的签名好，
    /// 也免得没有副标题的行——占多数——为此多写一个 `None`。
    ///
    /// # 示例
    /// ```
    /// use windui::prelude::*;
    /// let fuzzy = signal(true);
    /// let row = Element::setting_row_desc(
    ///     "模糊音纠错",
    ///     "z/zh、c/ch、s/sh 不区分",
    ///     Element::switch(fuzzy),
    /// );
    /// ```
    pub fn setting_row_desc(
        label: impl Into<String>,
        desc: impl Into<String>,
        control: Element,
    ) -> Self {
        Self::setting_row_inner(label.into(), Some(desc.into()), control)
    }

    /// `setting_row` / `setting_row_desc` 的共同骨架。
    fn setting_row_inner(label: String, desc: Option<String>, control: Element) -> Self {
        let th = crate::theme::current();
        let f = &th.form;
        // 左块吃掉剩余宽度，控件因此被推到右缘——比「标签定宽 + flex_spacer」少一个
        // 节点，且长标签能在整个左区换行而不是撞上定宽边界。
        let mut left = Element::col()
            .weight(1.0)
            .spacing(2)
            .child(Self::form_label(label, f, &th.metrics).width_match());
        let mut row = Element::row()
            .width_match()
            .cross(Align::Center)
            .spacing(f.gap());
        match desc {
            // 带副标题：高度按内容撑开（副标题长短不一，定高会把它挤出去）。
            Some(d) => {
                // 取 `TextSubtle` 而非 `TextMuted`：行标题已是正文档，说明再压一档才
                // 拉得开层次；`TextMuted` 是**次级正文**的档位，用在这里会让说明与标题
                // 显得同重。四档文字色的强弱顺序由 style.rs 的单测锁着。
                let desc = Element::label(d.clone())
                    .font_size(f.desc_size(&th.metrics))
                    .fg_role(crate::style::Role::TextSubtle)
                    .width_match();
                left = left.child(Self::clamp_lines(desc, f.desc_max_lines(), &d));
                row = row.padding_xy(0, f.row_pad_y());
            }
            // 单行：定高，与 `field` 同一档，整列才对得齐。
            None => row = row.height(f.row_height()),
        }
        row.child(left).child(control)
    }

    /// 表单族共用的标签：字号/字重/文字色统一走 [`FormTheme`](crate::theme::FormTheme)。
    fn form_label(
        text: impl Into<String>,
        f: &crate::theme::FormTheme,
        m: &crate::theme::Metrics,
    ) -> Self {
        let text = text.into();
        let el = Element::label(text.clone())
            .font_size(f.label_size(m))
            .font_weight(f.label_weight())
            .fg_role(crate::style::Role::Text);
        Self::clamp_lines(el, f.label_max_lines(), &text)
    }

    /// 表单族的行数限制：`None` 原样返回；`Some(n)` 加末尾省略并挂上看全文的 tooltip。
    ///
    /// 截断与 tooltip 绑成一件事——截断意味着信息不完整，而 tooltip 是它唯一的兜底。
    /// 未真正截断时 `Tree::node_tooltip` 会按 `Label::text_truncated()` 自动不弹，
    /// 故短文本不会平白多出一个与可见文字相同的提示。
    ///
    /// 含换行的文本**跳过 tooltip**（仍然限行）：`Element::tooltip` 只支持单行、多行会
    /// `debug_assert` 拦下，而这里的 tooltip 是库替调用方加的，不该由它引爆——调用方
    /// 显式调 `.tooltip()` 传多行才是该被拦住的误用。
    fn clamp_lines(el: Element, max_lines: Option<usize>, full_text: &str) -> Self {
        let Some(n) = max_lines else { return el };
        let el = el.max_lines(n).truncate(crate::ui::Truncate::End);
        if full_text.contains('\n') {
            el
        } else {
            el.tooltip(full_text)
        }
    }

    /// **卡片**：标题 + 分隔线 + 内容，铺在 `Surface` 底色上的圆角容器。
    ///
    /// 分组内容的默认外壳。标题**不设固定高**，长标题在卡片宽度内换行、分隔线随之下移。
    ///
    /// 底色/圆角/内边距/标题字号取自 [`CardTheme`]。想要描边卡片就在返回值上链
    /// `.border_role(Role::Border, 1)`——那是样式修饰符，对这种组合容器有效
    /// （可用/不可用的修饰符同 [`field`](Self::field)）。
    ///
    /// # 示例
    /// ```
    /// use windui::prelude::*;
    /// let notify = signal(true);
    /// let ui = Element::card(
    ///     "通知",
    ///     Element::col()
    ///         .width_match()
    ///         .child(Element::field("推送", Element::switch(notify))),
    /// )
    /// .border_role(Role::Border, 1); // 样式修饰符可以链
    /// ```
    ///
    /// [`CardTheme`]: crate::theme::CardTheme
    pub fn card(title: impl Into<String>, body: Element) -> Self {
        let th = crate::theme::current();
        let c = &th.card;
        Element::col()
            .width_match()
            .bg_role(crate::style::Role::Surface)
            .corner(c.corner(&th.metrics))
            .padding(c.pad())
            .spacing(c.gap())
            .child(
                Element::label(title.into())
                    .font_size(c.title_size(&th.metrics))
                    .font_weight(c.title_weight())
                    .fg_role(crate::style::Role::Text)
                    .width_match(),
            )
            .child(Element::divider())
            .child(body)
    }

    /// 等宽网格：把 `items` 按每行 `cols` 个排布，行/列间距 `gap`，列按权重均分等宽；
    /// 末行不足时用空白补齐以保持列对齐。常用于复选框组、卡片墙。
    #[track_caller]
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
        // 颜色走主题角色延迟解析：运行期换主题自动跟随。
        use crate::style::Role;
        Element::row()
            .cross(Align::Center)
            .spacing(4)
            .padding_xy(9, 3)
            .corner(999.0)
            .bg_role_alpha(Role::Accent, 0.14)
            .child(
                Element::label(text.into())
                    .font_size(12.5)
                    .fg_role(Role::Accent)
                    .height(18),
            )
            .child(
                Element::icon_button("\u{2715}")
                    .size(16, 16)
                    .font_size(11.0)
                    .fg_role(Role::Accent)
                    .on_click(on_remove),
            )
    }

    /// 标签字段：仿输入框的带边框容器，内含一组 chip（多值展示）。`chips` 用
    /// [`Element::chip`] 生成；为空时显示 `placeholder`。新增值由 app 驱动
    /// （维护值列表 Signal，变化后重建 chips 列表）。
    pub fn tag_field(placeholder: impl Into<String>, chips: Vec<Element>) -> Self {
        // 颜色走 InputBg/InputBorder/Placeholder 角色延迟解析（换主题自动跟随）；
        // corner 为度量，构建期取值即可（换主题不改圆角，同 accordion 先例）。
        use crate::style::Role;
        let corner = {
            let th = crate::theme::current();
            th.input.corner(&th.metrics)
        };
        let mut row = Element::row()
            .width_match()
            .cross(Align::Center)
            .spacing(6)
            .padding_xy(8, 6)
            .corner(corner)
            .bg_role(Role::InputBg)
            .border_role(Role::InputBorder, 1);
        if chips.is_empty() {
            row = row.child(
                Element::label(placeholder.into())
                    .font_size(13.0)
                    .fg_role(Role::Placeholder)
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
        let cols: Vec<(String, f32)> = columns.into_iter().map(|(t, w)| (t.into(), w)).collect();
        let body: Vec<Vec<Element>> = rows
            .into_iter()
            .map(|r| {
                r.into_iter()
                    .map(|c| Self::table_cell_pad(Element::label(c.into()).font_size(13.0)))
                    .collect()
            })
            .collect();
        Self::table_custom(cols, body)
    }

    /// 表格单元格统一内边距包裹（文字内缩、单元格本身占满整格——便于整格背景/高亮）。
    /// 内边距在单元格**内部**而非行上，故可点击单元格的 hover 高亮能覆盖整格。单行盒（20px）。
    fn table_cell_pad(content: Element) -> Self {
        Self::table_cell_pad_lines(content, 1)
    }

    /// 同 [`table_cell_pad`](Self::table_cell_pad)，但允许多行：`lines <= 1` 时锁定 20px 单行盒
    /// （现状不变）；`lines > 1` 时 label 用 Wrap 高度，随文本折行长高——配合 `max_lines(lines)`
    /// 由绘制层精确裁到 `lines` 行（内容不足则更矮，超出裁切不溢出邻行）。
    fn table_cell_pad_lines(content: Element, lines: usize) -> Self {
        if lines <= 1 {
            // 单行：锁定 20px 单行盒（现状不变）。
            Element::stack()
                .padding_xy(TABLE_CELL_PAD_X, TABLE_CELL_PAD_Y)
                .child(content.width_match().height(20))
        } else {
            // 多行：行随同行最高单元格拉伸；文本竖直居中，内容不足一行时不再顶部对齐。
            // 用 row + cross(Center) 与自定义单元格（action_cell）保持一致的竖直居中。
            Element::row()
                .cross(Align::Center)
                .padding_xy(TABLE_CELL_PAD_X, TABLE_CELL_PAD_Y)
                .child(content.width_match())
        }
    }

    /// 数据表格（自定义单元格）：同 [`Element::table`]，但每个单元格是任意 `Element`
    /// （可放 `clickable`/`text_input` 等实现选中/编辑）。`columns` 为 (列标题, 权重)。
    pub fn table_custom(columns: Vec<(String, f32)>, rows: Vec<Vec<Element>>) -> Self {
        // 表头：加粗、弱化色、次级表面底。内边距在每列格内部（与正文同分布，列对齐）。
        let mut header = Element::row()
            .width_match()
            .cross(Align::Stretch)
            .bg_role(Role::SurfaceAlt);
        for (title, w) in &columns {
            header = header.child(
                Element::stack()
                    .weight(*w)
                    .padding_xy(TABLE_CELL_PAD_X, TABLE_HEADER_PAD_Y)
                    .child(
                        Element::label(title.clone())
                            .font_size(13.0)
                            .font_weight(600)
                            .fg_role(Role::TextMuted)
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
                tr = tr.bg_role(Role::SurfaceAlt);
            }
            for (ci, cell) in cells.into_iter().enumerate() {
                let w = columns.get(ci).map(|c| c.1).unwrap_or(1.0);
                tr = tr.child(cell.weight(w));
            }
            // 整行悬停轻微高亮（叠层在斑马纹之上、单元格之下；可编辑单元格的 clickable 叠层叠加其上）。
            tr.set_widget(Box::new(sortable_table::HoverRow::new()));
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
                                Element::label_signal(sig)
                                    .font_size(13.0)
                                    .fg_role(crate::style::Role::Text)
                                    .width_match()
                                    .height(20),
                            )
                    })
                    .collect()
            })
            .collect();
        Self::table_custom(cols, rows)
    }

    /// 可排序数据表格（受控排序）：点表头在 无 → 升序 → 降序 → 无 间循环，活动列显示
    /// 排序箭头（▲/▼）；正文按当前排序列重排。数值型列（两侧都能解析为 f64）按数值比较，
    /// 否则按字符串。排序状态由 `sort` 信号承载（受控——app 可读取/预置/联动服务端排序）。
    ///
    /// `columns` 为 (列标题, 权重)；`rows` 为每行的单元格文本。需在限高容器内使用（正文滚动）。
    ///
    /// # 示例
    /// ```ignore
    /// let sort = signal(Some(SortKey::asc(0)));
    /// Element::table_sortable(
    ///     vec![("名称", 2.0), ("大小", 1.0)],
    ///     vec![vec!["a.txt", "12"], vec!["b.txt", "3"]],
    ///     sort,
    /// ).height(200)
    /// ```
    pub fn table_sortable(
        columns: Vec<(impl Into<String>, f32)>,
        rows: Vec<Vec<impl Into<String>>>,
        sort: Signal<Option<SortKey>>,
    ) -> Self {
        let cols: Vec<(String, f32)> = columns.into_iter().map(|(t, w)| (t.into(), w)).collect();
        let data: Vec<Vec<String>> = rows
            .into_iter()
            .map(|r| r.into_iter().map(Into::into).collect())
            .collect();
        let weights: Vec<f32> = cols.iter().map(|c| c.1).collect();

        // 表头：响应式。单元格由 SortableHeader 首次布局时构建（见其 on_update），
        // 故 .sort_indicator(..) 覆盖能在 build 前设入并被采纳。客户端模式无回调。
        let mut header = Element::row()
            .width_match()
            .cross(Align::Stretch)
            .bg_role(Role::SurfaceAlt);
        header.set_widget(Box::new(sortable_table::SortableHeader::new(
            cols, sort, None,
        )));
        header.reactive = true;

        // 正文：滚动容器保留内置 ScrollWidget（滚轮 + 滚动条拖拽），行由 SortableBody 挂在
        // 其内部 col 上重建（不替换 scroll 的 widget，否则会丢滚轮/拖动能力）。初始行按当前排序排列。
        let order = sortable_table::sorted_order(&data, sort.get());
        let mut body = Element::col().width_match();
        for (disp, &ri) in order.iter().enumerate() {
            body = body.child(sortable_table::body_row(
                disp, ri, &data[ri], &weights, None, None, 1, None, None,
            ));
        }
        body.set_widget(Box::new(sortable_table::SortableBody::new(
            data, weights, sort,
        )));
        body.reactive = true;
        let scroll = Element::scroll().fill().child(body);

        Element::col()
            .width_match()
            .child(header)
            .child(Element::divider())
            .child(scroll.weight(1.0))
    }

    /// 服务端排序表格（受控数据 + 排序意图回调）：适合大数据集分页——**前端不排序**，
    /// 排序与分页由后端完成，前端只负责排序状态 UI（箭头）、捕获点击、触发重新拉取。
    ///
    /// - `rows` 为「当前页数据」信号：应用在 `on_sort` 里按新排序拉取数据后 `set` 写回，
    ///   正文自动重建（不做任何内部排序，后端给什么顺序就显示什么顺序）。
    /// - `sort` 承载当前排序列/方向，供表头渲染 ▲/▼；点表头先更新它、再触发 `on_sort`。
    /// - `on_sort(ctx, 新排序)` 在点表头时回调：应用据此请求「该排序 + 第一页」并 `set(rows)`。
    ///
    /// # 示例
    /// ```ignore
    /// let rows = signal(fetch_page(None, 0)); // 当前页数据
    /// let sort = signal(None);
    /// Element::table_sortable_server(
    ///     vec![("名称", 2.0), ("大小", 1.0)],
    ///     rows,
    ///     sort,
    ///     move |_ctx, new_sort| rows.set(fetch_page(new_sort, 0)),
    /// ).height(400)
    /// ```
    pub fn table_sortable_server(
        columns: Vec<(impl Into<String>, f32)>,
        rows: Signal<Vec<Vec<String>>>,
        sort: Signal<Option<SortKey>>,
        on_sort: impl FnMut(&mut EventCtx, Option<SortKey>) + 'static,
    ) -> Self {
        let cols: Vec<(String, f32)> = columns.into_iter().map(|(t, w)| (t.into(), w)).collect();
        let weights: Vec<f32> = cols.iter().map(|c| c.1).collect();
        let on_sort: sortable_table::OnSort = Rc::new(RefCell::new(on_sort));

        // 表头：响应式，单元格由 SortableHeader 首次布局构建（透传排序意图回调）。
        let mut header = Element::row()
            .width_match()
            .cross(Align::Stretch)
            .bg_role(Role::SurfaceAlt);
        header.set_widget(Box::new(sortable_table::SortableHeader::new(
            cols,
            sort,
            Some(on_sort),
        )));
        header.reactive = true;

        // 正文：滚动容器保留内置 ScrollWidget；PagedBody 挂在其内部 col 上，绑定当前页数据信号，
        // 按后端给定顺序渲染（无内部排序），数据变即重建。
        let initial = rows.get();
        let mut body = Element::col().width_match();
        for (disp, row) in initial.iter().enumerate() {
            body = body.child(sortable_table::body_row(
                disp, disp, row, &weights, None, None, 1, None, None,
            ));
        }
        body.set_widget(Box::new(sortable_table::PagedBody::new(rows, weights)));
        body.reactive = true;
        let scroll = Element::scroll().fill().child(body);

        Element::col()
            .width_match()
            .child(header)
            .child(Element::divider())
            .child(scroll.weight(1.0))
    }

    /// 可排序 + 可多选表格：首列复选框 + 表头全选（全/无/部分三态）+ 选中行高亮，
    /// 同时保留点表头排序。选择按**原始行身份**跟随（`selected[原始行下标]`），排序重排后仍正确。
    ///
    /// - `columns` 为 (列标题, 权重)；`rows` 为每行单元格文本。
    /// - `selected` 为每行一个 `Signal<bool>`（长度须等于 `rows`，按原始行下标索引）；
    ///   复选框直接绑定，勾选状态即读写这些信号，app 可随时读取选中集。
    /// - `sort` 同 [`table_sortable`](Self::table_sortable)：点数据列表头循环切换排序。
    ///
    /// # 示例
    /// ```ignore
    /// let rows = vec![vec!["a", "2"], vec!["b", "1"]];
    /// let selected: Vec<Signal<bool>> = (0..rows.len()).map(|_| signal(false)).collect();
    /// let sort = signal(None);
    /// Element::table_selectable(vec![("名称", 2.0), ("大小", 1.0)], rows, selected, sort).height(240)
    /// ```
    pub fn table_selectable(
        columns: Vec<(impl Into<String>, f32)>,
        rows: Vec<Vec<impl Into<String>>>,
        selected: Vec<Signal<bool>>,
        sort: Signal<Option<SortKey>>,
    ) -> Self {
        let cols: Vec<(String, f32)> = columns.into_iter().map(|(t, w)| (t.into(), w)).collect();
        let data: Vec<Vec<String>> = rows
            .into_iter()
            .map(|r| r.into_iter().map(Into::into).collect())
            .collect();
        let weights: Vec<f32> = cols.iter().map(|c| c.1).collect();
        let scw = sortable_table::select_col_w();

        // 表头：[全选列] + [可排序数据列子行]，与正文的 [复选框列] + [数据列] 逐列对齐
        // （子行 weight=1 占 W-scw，其内数据列按 weights 分；正文数据列在固定 scw 后同样按 weights 分）。
        let selectall = Element::leaf()
            .width(scw)
            .widget(sortable_table::SelectAllCheck::new(selected.clone()));
        let mut subrow = Element::row().weight(1.0).cross(Align::Stretch);
        subrow.set_widget(Box::new(sortable_table::SortableHeader::new(
            cols, sort, None,
        )));
        subrow.reactive = true;
        let header = Element::row()
            .width_match()
            .cross(Align::Stretch)
            .bg_role(Role::SurfaceAlt)
            .child(selectall)
            .child(subrow);

        // 正文：滚动容器保留内置 ScrollWidget；SelectableBody 挂在其内部 col 上，首次布局构建行。
        let mut body = Element::col().width_match();
        body.set_widget(Box::new(sortable_table::SelectableBody::new(
            data, weights, selected, sort,
        )));
        body.reactive = true;
        let scroll = Element::scroll().fill().child(body);

        Element::col()
            .width_match()
            .child(header)
            .child(Element::divider())
            .child(scroll.weight(1.0))
    }

    /// 覆盖排序表格的排序指示器样式（字形/字号/颜色/槽宽/间距/位置）。仅对
    /// [`table_sortable`](Self::table_sortable) / [`table_sortable_server`](Self::table_sortable_server)
    /// 返回的元素有效——它会定位表头行并设置每实例覆盖（未设字段回退主题 `TableTheme`）。
    /// 用于个别表格与全局主题不同的场景；全局统一样式请配主题。
    ///
    /// # 示例
    /// ```ignore
    /// Element::table_sortable(cols, rows, sort)
    ///     .sort_indicator(SortStyle { asc: Some("↑".into()), desc: Some("↓".into()), ..Default::default() })
    /// ```
    #[track_caller]
    pub fn sort_indicator(mut self, style: SortStyle) -> Self {
        // 排序表格结构为 col[ header, divider, scroll ]，表头为首个子节点。
        // `ok` 记录是否真的落到了 SortableHeader 上：任一层没命中都是误用（链错了元素），
        // 而按下标钻子树的写法不会自然报错，故显式断言，否则 debug 下也悄无声息地不生效。
        let mut ok = false;
        if let Some(header) = self.children.get_mut(0) {
            if let Some(a) = header.widget.as_any_mut() {
                if let Some(h) = a.downcast_mut::<sortable_table::SortableHeader>() {
                    h.set_style(style);
                    ok = true;
                }
            }
        }
        debug_assert!(
            ok,
            "sort_indicator() 只能用于 Element::table_sortable(..) / table_sortable_server(..)"
        );
        self
    }

    /// 在表格尾部追加一个**操作列**：表头显示 `title`（不可排序），每行单元格由
    /// `build(行下标)` 生成任意控件（如 查看/编辑/删除 按钮组），列宽按 `weight` 参与分配。
    /// 仅对 [`table_sortable`](Self::table_sortable) / [`table_selectable`](Self::table_selectable) /
    /// [`table_sortable_server`](Self::table_sortable_server) 返回的元素有效。
    ///
    /// 传给 `build` 的行下标：客户端表格为**原始行下标**（排序后仍锁定同一数据行，与选择语义
    /// 一致，可直接用作 `cells[row]` / `selected[row]` 索引）；服务端表格为当前页内**显示下标**。
    /// 在 `build` 内 `move` 捕获该下标即可为每行绑定独立回调。
    ///
    /// 性能：操作列不改变重建触发条件——排序/换页才重建，悬停/选择不重建；`build` 只在重建时
    /// 按行调用一次。大数据集请配合 [`table_sortable_server`](Self::table_sortable_server) 分页。
    ///
    /// `build` 是**生成器**（每次重建按行产出控件），不是事件回调，故无 `on_` 前缀、
    /// 无 `ctx`，且必须是 `Fn`（每行各调一次、跨重建反复调用）。要响应交互的是它
    /// 生成的控件自己的 `on_click`。
    ///
    /// # 示例
    /// ```ignore
    /// Element::table_sortable(cols, rows, sort).actions("操作", 1.6, move |row| {
    ///     Element::row().spacing(6)
    ///         .child(Element::button("查看").on_click(move |ctx| ctx.toast(format!("查看 {row}"))))
    ///         .child(Element::button("删除").outline().on_click(move |ctx| ctx.toast(format!("删除 {row}"))))
    /// })
    /// ```
    #[track_caller]
    pub fn actions(
        mut self,
        title: impl Into<String>,
        weight: f32,
        build: impl Fn(usize) -> Element + 'static,
    ) -> Self {
        const WHO: &str = "actions() 只能用于 Element::table_sortable(..) / \
             table_sortable_server(..) / table_selectable(..)";
        let ac = sortable_table::action_col(title.into(), weight, build);
        // 结构 col[ header, divider, scroll ]。表头行可能直接挂 SortableHeader
        // （table_sortable/server），或其子行 subrow 挂 SortableHeader（table_selectable 的全选列在前）。
        let mut header_ok = false;
        if let Some(header) = self.children.get_mut(0) {
            header_ok = sortable_table::set_header_actions(header, &ac);
            if !header_ok {
                if let Some(sub) = header.children.get_mut(1) {
                    header_ok = sortable_table::set_header_actions(sub, &ac);
                }
            }
        }
        debug_assert!(header_ok, "{WHO}（未定位到表头）");
        // 正文：scroll 为末子，其首个子节点（内层 col）挂响应式正文 widget。
        let mut body_ok = false;
        if let Some(scroll) = self.children.last_mut() {
            if let Some(body) = scroll.children.get_mut(0) {
                body_ok = sortable_table::set_body_actions(body, &ac);
            }
        }
        debug_assert!(body_ok, "{WHO}（未定位到正文）");
        self
    }

    /// 自定义**数据单元格**渲染：`build(行下标, 列下标, 单元格文本)` 返回 `Some` 时该格
    /// 用自定义控件（徽章/彩色标签/图标等），返回 `None` 回退默认文本渲染。仅对
    /// [`table_sortable`](Self::table_sortable) / [`table_selectable`](Self::table_selectable) /
    /// [`table_sortable_server`](Self::table_sortable_server) 返回的元素有效。
    ///
    /// 排序仍基于单元格**文本**（渲染与排序键解耦）；自定义格与操作列同款包裹
    /// （水平内边距 + 垂直居中，不强制 20px 行高，较高控件不被压扁）。行下标语义同
    /// [`actions`](Self::actions)：客户端表格为原始行下标，服务端表格为页内显示下标。
    ///
    /// `build` 与 [`actions`](Self::actions) 同属**生成器**：无 `on_` 前缀、无 `ctx`、
    /// 必须是 `Fn`（每格各调一次）。
    ///
    /// # 示例
    /// ```ignore
    /// Element::table_sortable(cols, rows, sort).cell_render(|_row, col, text| match col {
    ///     // 编码列渲染为边框徽章；其余列默认文本。
    ///     0 => Some(Element::label(text).font_size(12.5).padding_xy(6, 2).corner(4.0)
    ///         .border_role(Role::Border, 1)),
    ///     _ => None,
    /// })
    /// ```
    #[track_caller]
    pub fn cell_render(
        mut self,
        build: impl Fn(usize, usize, &str) -> Option<Element> + 'static,
    ) -> Self {
        let render: sortable_table::CellRender = Rc::new(build);
        // 结构 col[ header, divider, scroll ]：scroll 为末子，其首个子节点挂响应式正文 widget。
        let mut ok = false;
        if let Some(scroll) = self.children.last_mut() {
            if let Some(body) = scroll.children.get_mut(0) {
                ok = sortable_table::set_body_cell_render(body, &render);
            }
        }
        debug_assert!(
            ok,
            "cell_render() 只能用于 Element::table_sortable(..) / \
             table_sortable_server(..) / table_selectable(..)"
        );
        self
    }

    /// 设置**默认文本单元格**最多显示行数（`lines >= 1`，默认 1）：文本按列宽折行，行随内容
    /// 长高至多 `lines` 行、内容不足则更矮，超出部分精确裁切（不再溢出到相邻行）。仅影响走默认
    /// 文本渲染的格；自定义渲染格（`cell_render` 返回 `Some`）与操作列不受影响。仅对
    /// [`table_sortable`](Self::table_sortable) / [`table_selectable`](Self::table_selectable) /
    /// [`table_sortable_server`](Self::table_sortable_server) 返回的元素有效。
    ///
    /// # 示例
    /// ```ignore
    /// Element::table_sortable_server(cols, rows, sort, on_sort).cell_lines(2)
    /// ```
    #[track_caller]
    pub fn cell_lines(mut self, lines: usize) -> Self {
        // 结构 col[ header, divider, scroll ]：scroll 为末子，其首个子节点挂响应式正文 widget。
        let mut ok = false;
        if let Some(scroll) = self.children.last_mut() {
            if let Some(body) = scroll.children.get_mut(0) {
                ok = sortable_table::set_body_cell_lines(body, lines);
            }
        }
        debug_assert!(
            ok,
            "cell_lines() 只能用于 Element::table_sortable(..) / \
             table_sortable_server(..) / table_selectable(..)"
        );
        self
    }

    /// 整行**双击激活**回调：双击某数据行（落在数据/自定义单元格上，非操作列按钮）触发
    /// `on_activate(ctx, 行下标)`，常用于「双击进入编辑」。行下标语义同 [`actions`](Self::actions)：
    /// 客户端表格（[`table_sortable`](Self::table_sortable)）为原始行下标，服务端表格
    /// （[`table_sortable_server`](Self::table_sortable_server)）为页内显示下标。
    /// 仅对上述两类（HoverRow 型正文）有效；可多选表格（[`table_selectable`](Self::table_selectable)）
    /// 因首列复选框语义冲突不支持。
    ///
    /// # 示例
    /// ```ignore
    /// Element::table_sortable_server(cols, rows, sort, on_sort)
    ///     .on_row_activate(move |ctx, disp| open_edit(disp))
    /// ```
    #[track_caller]
    pub fn on_row_activate(
        mut self,
        on_activate: impl FnMut(&mut EventCtx, usize) + 'static,
    ) -> Self {
        let cb: sortable_table::OnRowActivate = Rc::new(RefCell::new(on_activate));
        // 结构 col[ header, divider, scroll ]：scroll 为末子，其首个子节点挂响应式正文 widget。
        let mut ok = false;
        if let Some(scroll) = self.children.last_mut() {
            if let Some(body) = scroll.children.get_mut(0) {
                ok = sortable_table::set_body_activate(body, &cb);
            }
        }
        debug_assert!(
            ok,
            "on_row_activate() 只能用于 Element::table_sortable(..) / table_sortable_server(..)；\
             table_selectable(..) 因首列复选框语义冲突不支持整行激活"
        );
        self
    }

    /// 整行**右键菜单**：右击某数据行时调用 `build(行下标)` 取菜单项并弹出级联浮层，
    /// 返回空 `Vec` 则不弹。行下标语义同 [`actions`](Self::actions)：客户端表格
    /// （[`table_sortable`](Self::table_sortable) / [`table_selectable`](Self::table_selectable)）
    /// 为原始行下标，服务端表格（[`table_sortable_server`](Self::table_sortable_server)）
    /// 为页内显示下标。三类表格均支持——右键不与首列复选框争语义（复选框只吃左键）。
    ///
    /// 菜单项**每次右击现取现建**：这样勾选态（`check`）、禁用态（`enabled`）
    /// 都反映右击当刻的数据。回调挂在行容器上，右击行内任何位置（含空白、自定义单元格、
    /// 操作列）都能弹。
    ///
    /// `build` 是**生成器**（每次右击重跑一遍产出项），不是事件回调，故无 `ctx`
    /// 参数、也必须是 `Fn`（要留存起来重复调用）。项的**动作**照常拿得到
    /// `&mut EventCtx`（见 [`MenuItem::run`](crate::event::MenuItem::run)），弹原生
    /// 对话框写 `ctx.defer_blocking(..)`。
    ///
    /// # 示例
    /// ```ignore
    /// Element::table_sortable_server(cols, rows, sort, on_sort)
    ///     .on_row_context_menu(move |disp| vec![
    ///         MenuItem::run("编辑…", move |_ctx| open_edit(disp), false),
    ///         MenuItem::separator(),
    ///         MenuItem::run("删除", move |_ctx| delete(disp), false),
    ///     ])
    /// ```
    #[track_caller]
    pub fn on_row_context_menu(
        mut self,
        build: impl Fn(usize) -> Vec<crate::event::MenuItem> + 'static,
    ) -> Self {
        let cb: sortable_table::OnRowMenu = Rc::new(build);
        // 结构 col[ header, divider, scroll ]：scroll 为末子，其首个子节点挂响应式正文 widget。
        let mut ok = false;
        if let Some(scroll) = self.children.last_mut() {
            if let Some(body) = scroll.children.get_mut(0) {
                ok = sortable_table::set_body_menu(body, &cb);
            }
        }
        debug_assert!(
            ok,
            "on_row_context_menu() 只能用于 Element::table_sortable(..) / \
             table_sortable_server(..) / table_selectable(..)；\
             普通容器/控件请用 on_context_menu()"
        );
        self
    }

    /// 设置自定义内容控件：把实现了 [`Widget`] 的类型挂到**还没有控件的**节点上，
    /// 即 `Element::leaf()` 或任意容器（`col`/`row`/`stack`）。
    ///
    /// 不能挂到已经是控件的节点上（`button`/`label`/`slider` … 以及 `table_*`、
    /// `list_signal` 这类内部已挂了 widget 的组合构造器）——一个节点只有一个
    /// widget 槽，那样做等于把原控件**静默丢掉**：按钮不再是按钮，却既不报错也
    /// 没有任何迹象。要在控件旁边加东西，用容器把两者并排放（`Element::row()`
    /// `.child(按钮).child(Element::leaf().widget(自定义))`）。
    #[track_caller]
    pub fn widget(mut self, w: impl Widget + 'static) -> Self {
        debug_assert!(
            !self.has_widget,
            "widget() 只能挂到没有控件的节点（leaf / col / row / stack）上；\
             该节点已有控件，再挂会把它静默替换掉"
        );
        self.set_widget(Box::new(w));
        self
    }

    /// 内部挂载入口：与 `has_widget` 标志同步，绕过 [`Element::widget`] 的守卫。
    /// 组合构造器把 widget 挂到自己刚建的容器上时用它——那是构造器自己的节点，
    /// 不是调用方误挂。
    fn set_widget(&mut self, w: Box<dyn Widget>) {
        self.widget = w;
        self.has_widget = true;
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
    /// 最小宽度（下界，逻辑 dp）：控件按内容自适应（保持默认 `Wrap` 宽）但不小于 `px`。
    /// 用于下拉/选择等——短选项对齐到统一基线宽，长选项自动加宽避免文本换行。
    /// 与固定宽 [`Element::width`] 互斥：若同时设了固定宽，则以固定宽为准、下界不生效。
    pub fn min_width(mut self, px: i32) -> Self {
        self.min_width = Some(px);
        self
    }
    /// 最大宽度（逻辑像素）。内容在此宽度内换行，不是排好版再裁。
    ///
    /// 典型用途是长正文限宽：行太长会让眼睛在回到行首时找错行，可读性显著下降。
    /// 配合 `width_match()` 使用——先撑满可用宽，再由本上界收住。
    ///
    /// 与 `min_width` 同时设定且冲突时**以本上界为准**。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// Element::label("很长的正文……").width_match().max_width(640);
    /// ```
    pub fn max_width(mut self, px: i32) -> Self {
        self.max_width = Some(px);
        self
    }
    /// 最大高度（逻辑像素）。节点占位封顶于此，内容仍按完整高度测量。
    ///
    /// 与滚动容器是天生一对：`Element::scroll()` 的高度默认按内容自适应，加上本上界即得
    /// 「内容短则对话框自然收缩、内容长则封顶并可滚动」——不必为了给长内容留余地而
    /// 写死一个高度，让短内容那边空出一大片。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// Element::scroll().width_match().max_height(220).child(Element::label("很长的说明……"));
    /// ```
    pub fn max_height(mut self, px: i32) -> Self {
        self.max_height = Some(px);
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
    /// 背景 = 主题角色色 × 透明度（badge/chip 的"意图色淡底"模式）。
    /// paint 期解析，运行期换主题自动跟随。
    pub fn bg_role_alpha(mut self, role: crate::style::Role, alpha: f32) -> Self {
        self.style.bg = Some(crate::style::Brush::RoleAlpha(role, alpha));
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
    /// 限定边框只画某几条边。须与 `border` / `border_role` 连用，单独调用无效果。
    ///
    /// 用于页签下划线、分区底线、栏间竖线这类**单边**装饰。此前只能拿 1px 高的色块
    /// 拼——那会多占一个布局位置，容器一改间距就跟着错位；作为边框则完全不参与布局。
    ///
    /// 缺边时 `corner()` 不生效：一条底边没有「圆角」可言。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// # use windui::style::Edges;
    /// // 页签栏底线
    /// Element::row().border_role(Role::Divider, 1).border_edges(Edges::BOTTOM);
    /// // 上下双线
    /// Element::row().border_role(Role::Divider, 1).border_edges(Edges::TOP | Edges::BOTTOM);
    /// ```
    pub fn border_edges(mut self, edges: crate::style::Edges) -> Self {
        self.style.border_edges = edges;
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
    /// 字体族名（如 `"Newsreader"`、`"Microsoft YaHei"`）。未设 = 系统默认。
    ///
    /// 字体**未安装时不报错也不 panic**：DirectWrite 按名匹配失败即回退系统默认字体。
    /// 这是刻意的——少一个装饰性字体不该让界面崩掉，何况字体是否存在取决于用户机器，
    /// 调用方无从保证。需要确保效果时应自行随程序分发字体。
    pub fn font_family(mut self, name: impl Into<String>) -> Self {
        self.style.font_family = Some(name.into());
        self
    }
    /// 行高倍数（相对字号）。不设则用字体自带行距。
    ///
    /// 主要影响**多行文字**的行间距；单行文字只影响其占位高度。取倍数而非绝对像素，
    /// 使行距随字号与 DPI 一同缩放——写死像素会在换字号时失调。
    ///
    /// 经验值：中文正文 1.6–1.7，西文 1.4–1.5，标题 1.1–1.25。字号越大行距倍数应越小。
    ///
    /// ```no_run
    /// # use windui::prelude::*;
    /// Element::label("很长的一段中文释义……").line_height(1.7).width_match();
    /// ```
    pub fn line_height(mut self, multiple: f32) -> Self {
        self.style.line_height = Some(multiple);
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
            min_width: self.min_width.unwrap_or(0),
            max_width: self.max_width.unwrap_or(0),
            max_height: self.max_height.unwrap_or(0),
            padding: self.padding,
            margin: self.margin,
            align: self.align,
            layout: self.layout,
            widget,
            style: self.style,
            visible: self.visible,
            vis_signal: self.vis_signal,
            vis_cond: self.vis_cond,
            enabled_static: self.enabled_static,
            enabled: self.enabled,
            en_cond: self.en_cond,
            on_drop: self.on_drop,
            context_menu: self.context_menu,
            window_drag: self.window_drag,
            focusable: self.focusable,
            tooltip: self.tooltip,
            focused: false,
            clip_children: self.clip_children,
            scroll_y: 0,
            content_h: 0,
            over_scroll: 0,
            prev_visible: Cell::new(true),
            offset: Point::new(0, 0),
            raised: false,
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
    use crate::event::{KeyEvent, MouseButton, PointerEvent};
    use crate::geometry::Point;
    use crate::signal::signal;
    use std::path::PathBuf;
    use std::rc::Rc;

    #[test]
    fn badge_and_chip_colors_are_theme_roles() {
        use crate::style::{Brush, Role};
        // badge/chip 底色须为角色延迟解析（RoleAlpha）——运行期换主题自动跟随；
        // 此前构建期固化颜色，热切换后徽章仍是旧主题色。
        let b = Element::badge("v1");
        assert!(
            matches!(b.style.bg, Some(Brush::RoleAlpha(Role::Accent, _))),
            "badge(Primary) 底色应为 Accent 角色淡化"
        );
        let d = Element::badge_intent("废弃", crate::theme::Intent::Danger);
        assert!(
            matches!(d.style.bg, Some(Brush::RoleAlpha(Role::Danger, _))),
            "badge(Danger) 底色应为 Danger 角色淡化"
        );
        // Neutral 前景须为 text_muted（够深可读），不得用浅灰 border 当字色。
        let n = Element::badge_intent("中性", crate::theme::Intent::Neutral);
        assert!(
            matches!(n.style.bg, Some(Brush::RoleAlpha(Role::TextMuted, _))),
            "badge(Neutral) 底色应为 TextMuted 角色淡化"
        );
        assert_eq!(
            n.children[0].style.fg_role,
            Some(Role::TextMuted),
            "badge(Neutral) 文字应为 TextMuted（灰字灰底不可读）"
        );
        let c = Element::chip("x", |_| {});
        assert!(
            matches!(c.style.bg, Some(Brush::RoleAlpha(Role::Accent, _))),
            "chip 底色应为 Accent 角色淡化"
        );
        let t = Element::tag_field("占位", vec![]);
        assert!(
            matches!(t.style.bg, Some(Brush::Role(Role::InputBg))),
            "tag_field 底色应为 InputBg 角色"
        );
    }

    /// 在 200×200 窗口里布局并返回 (tree, root)。
    fn layout(el: Element) -> Tree {
        let mut tree = Tree::new();
        let root = el.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        tree
    }

    // ---- 拖拽重排（reorder_list）：全部经真实 dispatch_pointer / layout_root 驱动 ----

    /// 关闭动画的作用域守卫：`Drop` 时复位。
    ///
    /// 必须是守卫而不是"函数末尾调 `set_enabled(true)`"：`anim` 的开关是 thread_local，
    /// 而 `cargo test -- --test-threads=1` 下所有测试跑在同一线程上——一旦某个断言 panic
    /// 就再也复位不了，同线程后续测试全部在动画关闭状态下运行，依赖补间活跃的断言会被
    /// 静默弱化。
    struct AnimOff;
    impl AnimOff {
        fn new() -> Self {
            crate::anim::set_enabled(false);
            AnimOff
        }
    }
    impl Drop for AnimOff {
        fn drop(&mut self) {
            crate::anim::set_enabled(true);
        }
    }

    /// 行高可指定的可排序列表（`heights` 决定行数与各行高度）。`hook` 收到 (from, to)。
    /// 动画全局关闭，使补间瞬时收敛——否则回落阶段要等真实时钟，测试不确定。
    fn reorder_tree_with(
        hook: Rc<RefCell<Vec<(usize, usize)>>>,
        mode: reorder::CommitMode,
        heights: &[i32],
    ) -> (Tree, Vec<NodeId>, AnimOff) {
        let guard = AnimOff::new();
        let rows: Vec<Element> = heights
            .iter()
            .map(|&h| Element::leaf().width_match().height(h))
            .collect();
        let el = Element::reorder_list(rows)
            .commit_mode(mode)
            .on_reorder(move |_ctx, from, to| hook.borrow_mut().push((from, to)));
        let tree = layout(el);
        let root = tree.root.unwrap();
        let kids = tree.get(root).unwrap().children.clone();
        (tree, kids, guard)
    }

    /// 三行等高 40 的可排序列表。
    fn reorder_tree(
        hook: Rc<RefCell<Vec<(usize, usize)>>>,
        mode: reorder::CommitMode,
    ) -> (Tree, Vec<NodeId>, AnimOff) {
        reorder_tree_with(hook, mode, &[40, 40, 40])
    }

    /// 第 `row` 行手柄的中心点（手柄是行的首个子节点）。
    fn handle_center(tree: &Tree, rows: &[NodeId], row: usize) -> Point {
        let handle = tree.get(rows[row]).unwrap().children[0];
        let b = tree.abs_bounds(handle);
        Point::new(b.x + b.w / 2, b.y + b.h / 2)
    }

    fn relayout(tree: &mut Tree) {
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
    }

    #[test]
    fn drag_handle_reorders_children_and_reports_indices() {
        let hook = Rc::new(RefCell::new(Vec::new()));
        let (mut tree, rows, _anim) = reorder_tree(hook.clone(), reorder::CommitMode::Children);
        let (mut hover, mut cap) = (None, None);
        let start = handle_center(&tree, &rows, 0);

        // 按下手柄：捕获落在手柄自身（它 capture 后返回 false，逻辑由列表接管）。
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        assert!(cap.is_some(), "按下手柄应捕获指针");

        // 拖到 y=110：首行视觉中心 110 越过第三行中心 100 → 目标位 2。
        let to_pos = Point::new(start.x, 110);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, to_pos, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);
        // 让位：后两行各上移一个行高，首行跟指针下移。
        assert_eq!(tree.get(rows[1]).unwrap().offset.y, -40, "第二行应上移让位");
        assert_eq!(tree.get(rows[2]).unwrap().offset.y, -40, "第三行应上移让位");
        assert!(tree.get(rows[0]).unwrap().raised, "被拖行应提为浮起层");
        // 被拖行不走补间、直接跟指针（补间会带来橡皮筋般的滞后手感）：
        // 按下点 y=20 拖到 y=110，位移应精确等于 90。
        assert_eq!(
            tree.get(rows[0]).unwrap().offset.y,
            110 - start.y,
            "被拖行应精确跟随指针位移"
        );

        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, to_pos, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree); // 回落动画（已禁用→瞬时）结束并提交

        assert_eq!(*hook.borrow(), vec![(0, 2)], "应上报 (0, 2)");
        let after = tree.get(tree.root.unwrap()).unwrap().children.clone();
        assert_eq!(after, vec![rows[1], rows[2], rows[0]], "首行应移到末位");
        assert!(
            after.iter().all(|&r| tree.get(r).unwrap().offset.y == 0),
            "提交后所有视觉偏移必须清零，否则会与新布局叠加成双倍位移"
        );
        assert!(!tree.get(rows[0]).unwrap().raised, "提交后应取消浮起");
    }

    #[test]
    fn tap_below_threshold_does_not_reorder() {
        // 按下手柄后只抖动 2px：是一次点击，不该变成微小重排。
        let hook = Rc::new(RefCell::new(Vec::new()));
        let (mut tree, rows, _anim) = reorder_tree(hook.clone(), reorder::CommitMode::Children);
        let (mut hover, mut cap) = (None, None);
        let start = handle_center(&tree, &rows, 0);

        for (kind, y) in [
            (PointerKind::Down, start.y),
            (PointerKind::Move, start.y + 2),
            (PointerKind::Up, start.y + 2),
        ] {
            tree.dispatch_pointer(
                PointerEvent::single(kind, Point::new(start.x, y), MouseButton::Left),
                &mut hover,
                &mut cap,
            );
        }
        relayout(&mut tree);

        assert!(hook.borrow().is_empty(), "未超阈值不应触发重排回调");
        assert_eq!(
            tree.get(tree.root.unwrap()).unwrap().children,
            rows,
            "顺序不应变化"
        );
    }

    #[test]
    fn escape_cancels_drag_without_committing() {
        let hook = Rc::new(RefCell::new(Vec::new()));
        let (mut tree, rows, _anim) = reorder_tree(hook.clone(), reorder::CommitMode::Children);
        let (mut hover, mut cap) = (None, None);
        let start = handle_center(&tree, &rows, 0);
        let handle_id = tree.get(rows[0]).unwrap().children[0];

        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Move,
                Point::new(start.x, 110),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);
        assert_ne!(tree.get(rows[1]).unwrap().offset.y, 0, "拖动中应已让位");

        // Esc 送到持有焦点的手柄（按下时 request_focus 已把焦点交给它）。
        tree.dispatch_key(
            KeyEvent {
                key: Key::Escape,
                pressed: true,
                shift: false,
                ctrl: false,
            },
            Some(handle_id),
        );
        relayout(&mut tree);

        assert!(hook.borrow().is_empty(), "取消不应触发回调");
        assert_eq!(
            tree.get(tree.root.unwrap()).unwrap().children,
            rows,
            "取消后顺序不变"
        );
        assert!(
            rows.iter().all(|&r| tree.get(r).unwrap().offset.y == 0),
            "取消后所有行应归位"
        );
        assert!(
            !tree.get(rows[0]).unwrap().raised,
            "取消后应取消浮起，否则该行会一直盖住兄弟并抢命中"
        );

        // 用户随后松手：捕获必须在这里归还。键盘路径不传播 capture 副作用
        // （`DispatchResult` 无 capture 字段），所以只能靠 Up 的兜底臂——漏掉它
        // 会让指针捕获永久泄漏，整个窗口失去响应。
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, Point::new(start.x, 110), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        assert!(cap.is_none(), "取消后松手必须释放指针捕获");
    }

    #[test]
    fn escape_before_threshold_does_not_wedge_state_machine() {
        // 按下手柄但还没起拖就按 Esc：状态机必须干净收尾。曾因 on_update 里读的是
        // 未更新的局部 phase 而从早退分支直接返回——cancel 标志一直挂着，
        // 之后每次拖动都会被立刻取消，整个列表从此拖不动。
        let hook = Rc::new(RefCell::new(Vec::new()));
        let (mut tree, rows, _anim) = reorder_tree(hook.clone(), reorder::CommitMode::Children);
        let (mut hover, mut cap) = (None, None);
        let start = handle_center(&tree, &rows, 0);
        let handle_id = tree.get(rows[0]).unwrap().children[0];
        let esc = KeyEvent {
            key: Key::Escape,
            pressed: true,
            shift: false,
            ctrl: false,
        };

        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_key(esc, Some(handle_id));
        relayout(&mut tree);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        assert!(cap.is_none(), "取消后松手必须释放指针捕获");

        // 关键回归点：紧接着再拖一次，必须正常生效。
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Move,
                Point::new(start.x, 110),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, Point::new(start.x, 110), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);

        assert_eq!(*hook.borrow(), vec![(0, 2)], "上一次取消不得污染后续拖动");
    }

    #[test]
    fn capture_lost_cancels_instead_of_committing() {
        // Alt+Tab / 别的窗口夺走捕获时，宿主补发一个远处坐标的合成 Up。既有约定是
        // "收尾/复位"而非"确认"（Slider 借它复位拖动），顺序不该被悄悄改掉。
        let hook = Rc::new(RefCell::new(Vec::new()));
        let (mut tree, rows, _anim) = reorder_tree(hook.clone(), reorder::CommitMode::Children);
        let (mut hover, mut cap) = (None, None);
        let start = handle_center(&tree, &rows, 0);

        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Move,
                Point::new(start.x, 110),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);
        // UiHost::on_capture_lost 合成的事件（坐标 -1_000_000）。
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Up,
                Point::new(-1_000_000, -1_000_000),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);

        assert!(hook.borrow().is_empty(), "捕获丢失应取消而非提交");
        assert_eq!(
            tree.get(tree.root.unwrap()).unwrap().children,
            rows,
            "捕获丢失后顺序不变"
        );
        assert!(cap.is_none(), "捕获丢失后不应残留逻辑捕获");
    }

    #[test]
    fn unequal_row_heights_stack_correctly_through_real_dispatch() {
        // 表单行常带副标题/徽章，高度天然不齐。这条走真实事件路径验证重堆叠让位，
        // 而不只是测 stack_offsets 纯函数——布局给出的槽位与算法假设是否吻合，
        // 只有经 layout_root 才能验证。
        let hook = Rc::new(RefCell::new(Vec::new()));
        let (mut tree, rows, _anim) =
            reorder_tree_with(hook.clone(), reorder::CommitMode::Children, &[40, 60, 40]);
        let (mut hover, mut cap) = (None, None);
        let start = handle_center(&tree, &rows, 0);

        // 槽位应为 y=0/40/100。首行拖到末位：需越过末行中心 120。
        assert_eq!(tree.get(rows[1]).unwrap().bounds.y, 40);
        assert_eq!(tree.get(rows[2]).unwrap().bounds.y, 100);

        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Move,
                Point::new(start.x, 130),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);

        // 抽掉 40 高的首行后，后两行各上移 40；首行则要落到 y=100（下移 100）。
        assert_eq!(tree.get(rows[1]).unwrap().offset.y, -40, "60 高行应上移 40");
        assert_eq!(tree.get(rows[2]).unwrap().offset.y, -40, "末行应上移 40");

        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, Point::new(start.x, 130), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);
        assert_eq!(*hook.borrow(), vec![(0, 2)], "不等高列表也应算出正确目标位");
        assert_eq!(
            tree.get(tree.root.unwrap()).unwrap().children,
            vec![rows[1], rows[2], rows[0]],
            "首行应移到末位"
        );
    }

    #[test]
    fn callback_mode_reports_without_touching_children() {
        // 数据驱动场景：children 由上游重建负责，控件只上报意图。
        let hook = Rc::new(RefCell::new(Vec::new()));
        let (mut tree, rows, _anim) = reorder_tree(hook.clone(), reorder::CommitMode::Callback);
        let (mut hover, mut cap) = (None, None);
        let start = handle_center(&tree, &rows, 0);

        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Move,
                Point::new(start.x, 110),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, Point::new(start.x, 110), MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);

        assert_eq!(*hook.borrow(), vec![(0, 2)], "应上报 (0, 2)");
        assert_eq!(
            tree.get(tree.root.unwrap()).unwrap().children,
            rows,
            "Callback 模式下控件不得自行重排 children"
        );
    }

    // ---- 数据驱动重排（reorder_list_signal） ----

    /// 数据信号驱动的三行等高列表；`hook` 收到 (from, to) 且**在回调里按实参改数据**
    /// ——这正是数据驱动模式约定的用法，也是「同帧重建」断言的前提。
    fn reorder_signal_tree(data: Signal<Vec<u32>>) -> (Tree, AnimOff) {
        let guard = AnimOff::new();
        let d = data;
        // 行内第二个子节点的高度编码数据值（`v * 2`），供断言反查是哪一项——
        // 比从节点树里挖标签文本稳，也不依赖文本引擎。
        let el = Element::reorder_list_signal(data, |v, handle| {
            Element::row()
                .width_match()
                .height(40)
                .child(handle)
                .child(Element::leaf().weight(1.0).height(v as i32 * 2))
        })
        .on_reorder(move |_ctx, from, to| {
            d.update(|v| {
                let x = v.remove(from);
                v.insert(to.min(v.len()), x);
            })
        });
        (layout(el), guard)
    }

    /// 各行第一个子节点就是手柄（本组测试的行构建函数把它放在行首）。
    fn signal_handle_center(tree: &Tree, row: usize) -> Point {
        let rows = tree.get(tree.root.unwrap()).unwrap().children.clone();
        let handle = tree.get(rows[row]).unwrap().children[0];
        let b = tree.abs_bounds(handle);
        Point::new(b.x + b.w / 2, b.y + b.h / 2)
    }

    /// 各行当前承载的数据值（从行内第二个子节点的高度反解），用来断言「重建结果
    /// 跟着数据走」而不只是节点数对得上。
    fn signal_row_values(tree: &Tree) -> Vec<u32> {
        tree.get(tree.root.unwrap())
            .unwrap()
            .children
            .iter()
            .map(|&r| {
                let body = tree.get(r).unwrap().children[1];
                (tree.get(body).unwrap().bounds.h / 2) as u32
            })
            .collect()
    }

    #[test]
    fn reorder_signal_rebuilds_rows_when_data_changes() {
        // 反向同步：应用改数据（「恢复默认」「重新载入配置」）→ 行随之重建。
        // 这正是 reorder_list 的 Children 模式做不到、非引入本构造器不可的那件事。
        let data = signal(vec![1u32, 2, 3]);
        let (mut tree, _anim) = reorder_signal_tree(data);
        assert_eq!(signal_row_values(&tree), vec![1, 2, 3]);

        data.set(vec![3, 1]);
        relayout(&mut tree);
        assert_eq!(
            signal_row_values(&tree),
            vec![3, 1],
            "行必须跟着数据重建（含行数变化）"
        );
    }

    #[test]
    fn reorder_signal_commits_and_rebuilds_in_same_frame() {
        let data = signal(vec![1u32, 2, 3]);
        let (mut tree, _anim) = reorder_signal_tree(data);
        let (mut hover, mut cap) = (None, None);
        let start = signal_handle_center(&tree, 0);

        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        let to_pos = Point::new(start.x, 110);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Move, to_pos, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, to_pos, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        // 只跑一次 relayout：回落提交与按新数据重建必须发生在**同一帧**，否则会
        // 闪回一帧旧顺序（偏移已清零、children 还没换）。
        relayout(&mut tree);

        assert_eq!(data.get(), vec![2, 3, 1], "回调应把首项挪到末位");
        assert_eq!(
            signal_row_values(&tree),
            vec![2, 3, 1],
            "提交同帧就应重建出新顺序"
        );
        assert!(
            tree.get(tree.root.unwrap())
                .unwrap()
                .children
                .iter()
                .all(|&r| tree.get(r).unwrap().offset.y == 0 && !tree.get(r).unwrap().raised),
            "重建后的新行不得残留偏移或浮起态"
        );
    }

    #[test]
    fn reorder_signal_defers_rebuild_while_dragging() {
        // 拖动中重建会把槽位快照、补间下标与浮起样式指向的节点整批换掉，让位当场失准。
        // 故拖动期间的数据变更必须压到落定之后再落地。
        let data = signal(vec![1u32, 2, 3]);
        let (mut tree, _anim) = reorder_signal_tree(data);
        let (mut hover, mut cap) = (None, None);
        let start = signal_handle_center(&tree, 0);

        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, start, MouseButton::Left),
            &mut hover,
            &mut cap,
        );
        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Move,
                Point::new(start.x, start.y + 20),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);

        // 拖动中来了一次外部数据变更（如后台刷新）。
        data.set(vec![7, 8]);
        relayout(&mut tree);
        assert_eq!(signal_row_values(&tree), vec![1, 2, 3], "拖动中不得重建行");

        tree.dispatch_pointer(
            PointerEvent::single(
                PointerKind::Up,
                Point::new(start.x, start.y + 20),
                MouseButton::Left,
            ),
            &mut hover,
            &mut cap,
        );
        relayout(&mut tree);
        assert_eq!(
            signal_row_values(&tree),
            vec![7, 8],
            "落定后应补上拖动期间积压的数据变更"
        );
    }

    #[test]
    fn list_signal_rebuild_reclaims_row_signals() {
        // 根因回归：`row_fn` 里创建的信号曾随每次数据变化永久累积（运行时 arena 只增不减）。
        // 现在 DynList 持有一个 SignalScope，重建时先整批回收上一轮，再收集新一轮。
        use crate::signal::stats;

        let data = signal(vec![1u8, 2, 3]);
        let mut tree = Tree::new();
        // 每行现造一个信号（放宽后的动态文案 API 让这种写法变得自然）。
        let list = Element::list_signal(
            data,
            |_| (),
            |n: u8| {
                let caption = signal(format!("第 {n} 行"));
                Element::label(caption)
            },
        );
        let root = list.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);

        // 先跑几轮让 arena 达到稳态（首批行的槽位也归 widget 的作用域管）。
        for i in 0..3u8 {
            data.set(vec![i, i + 1, i + 2]);
            tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        }
        let steady = stats();

        for i in 0..20u8 {
            data.set(vec![i, i + 1, i + 2]);
            tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        }
        let after = stats();

        assert_eq!(
            tree.get(root).unwrap().children.len(),
            3,
            "重建后行数仍正确"
        );
        assert_eq!(
            after.live, steady.live,
            "反复重建后活跃槽位数不应增长（每轮 3 个行信号曾在此永久累积）"
        );
        assert_eq!(
            after.capacity, steady.capacity,
            "槽位应被复用，arena 容量不应增长"
        );
    }

    #[test]
    fn host_signal_gives_weight_children_real_height_and_rebuilds() {
        // 回归：list_signal 容器是滚动布局（子元素按无限高度测量），内含 weight 正文的
        // 表格会高度崩塌为 0。host_signal 用普通 col 容器，weight 子元素应拿到确定高度。
        let data = signal(vec![1u8]);
        let mut tree = Tree::new();
        let host = Element::host_signal(data, |_| {
            Element::col()
                .width_match()
                .fill()
                .child(Element::label("头").height(20))
                .child(Element::leaf().weight(1.0)) // 模拟表格正文（weight 撑满剩余）
        });
        let root = host.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        let inner = tree.get(root).unwrap().children[0];
        let body = tree.get(inner).unwrap().children[1];
        assert_eq!(
            tree.get(body).unwrap().bounds.h,
            180,
            "weight 正文应拿到剩余高度（200 - 20 表头），而非崩塌为 0"
        );
        // 信号版本变化 → 下次布局整体重建（子节点替换为新数据的构建结果）。
        data.set(vec![1, 2]);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        assert_eq!(
            tree.get(root).unwrap().children.len(),
            2,
            "信号变化后应按新数据重建子元素"
        );
    }

    #[test]
    fn host_signal_rebuilt_reactive_children_receive_on_update() {
        // 回归：dispatch_reactive_updates 曾用广播快照的存活集覆盖注册列表，把广播期间
        //（宿主 on_update 重建子树时）新注册的响应式节点抹掉——重建出的响应式表头/正文
        // 永远收不到 on_update，表格在宿主重建（如切换类别）后空白。
        let epoch = signal(vec![0u64]);
        let rows = signal(vec![vec!["a".to_string()]]);
        let host = Element::host_signal(epoch, move |_| {
            Element::table_sortable_server(vec![("列", 1.0)], rows, signal(None), |_, _| {})
        });
        let mut tree = Tree::new();
        let root = host.build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);

        // 表格结构 col[header, divider, scroll]；表头单元格由 SortableHeader on_update 构建。
        let header_cells = |tree: &Tree| {
            let table = tree.get(tree.root.unwrap()).unwrap().children[0];
            let header = tree.get(table).unwrap().children[0];
            tree.get(header).unwrap().children.len()
        };
        assert_eq!(header_cells(&tree), 1, "初始表头应有单元格");

        // 触发宿主重建（模拟切换类别）：新表头应在同一帧收到 on_update 并构建单元格。
        epoch.set(vec![1]);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        assert_eq!(
            header_cells(&tree),
            1,
            "重建后的表头应构建出单元格（注册不被抹掉）"
        );

        // 再次重建仍工作（前一轮的注册清理不误伤新注册）。
        epoch.set(vec![2]);
        tree.layout_root(Size::new(200, 200), &mut crate::text::NullTextEngine);
        assert_eq!(header_cells(&tree), 1, "再次重建仍应正常构建");
    }

    #[test]
    fn disabled_text_uses_text_disabled_color() {
        let theme = crate::theme::Theme::default();
        let style = Style {
            fg: Color::hex(0x123456),
            fg_role: None, // 显式 fg 覆盖（不走主题角色）
            ..Style::default()
        };
        // 启用：fg_role 为 None 时取样式自身前景色。
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
    #[derive(Default)]
    struct CaptureCanvas {
        last_text_color: std::cell::Cell<Option<Color>>,
        /// 本次绘制画过的全部文本：供"文案随信号变"的断言读取（有些控件除了文案
        /// 还会画装饰字形，如 NavRow 右侧的 chevron，故不能只留最后一条）。
        texts: RefCell<Vec<String>>,
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
            text: &str,
            _rect: Rect,
            color: Color,
            _align: crate::spec::Align,
            _ts: &crate::text::TextStyle,
        ) {
            self.last_text_color.set(Some(color));
            self.texts.borrow_mut().push(text.to_string());
        }
        fn measure_text(&mut self, _: &str, _: &crate::text::TextStyle) -> Size {
            Size::ZERO
        }
        fn push_layer(&mut self, _: f32) {}
        fn pop_layer(&mut self) {}
        fn save(&mut self) {}
        fn restore(&mut self) {}
        fn clip_rect(&mut self, _: Rect) {}
    }

    /// 按字符数估算宽度的 Canvas：用于可控地触发/规避 Label 的单行省略截断。
    struct WidthCanvas;
    impl crate::render::Canvas for WidthCanvas {
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
            _: &str,
            _: Rect,
            _: Color,
            _: crate::spec::Align,
            _: &crate::text::TextStyle,
        ) {
        }
        fn measure_text(&mut self, text: &str, ts: &crate::text::TextStyle) -> Size {
            Size::new(
                (text.chars().count() as f32 * ts.size).ceil() as i32,
                ts.line_height_px().unwrap_or(ts.size).ceil() as i32,
            )
        }
        fn push_layer(&mut self, _: f32) {}
        fn pop_layer(&mut self) {}
        fn save(&mut self) {}
        fn restore(&mut self) {}
        fn clip_rect(&mut self, _: Rect) {}
    }

    #[test]
    fn label_text_truncated_reflects_actual_overflow_and_gates_tooltip() {
        use crate::core::Widget;
        let mut label = Label::new("这是一段用来测试截断状态的说明文字");
        label.max_lines = Some(1);
        label.truncate = Truncate::End;
        let style = Style::default();
        let mut cv = WidthCanvas;

        // 未绘制过：尚不知道是否溢出，但功能已启用 → Some(false)（保守不弹）。
        assert_eq!(label.text_truncated(), Some(false));

        // 足够宽：完整显示，未截断。
        let wide = Rect::new(0, 0, 1000, 20);
        label.paint(wide, wide, false, true, &mut cv, &style);
        assert_eq!(label.text_truncated(), Some(false));

        // 变窄后重绘（不同 content.w → 缓存 miss 重新计算）：应判定为已截断。
        let narrow = Rect::new(0, 0, 40, 20);
        label.paint(narrow, narrow, false, true, &mut cv, &style);
        assert_eq!(label.text_truncated(), Some(true));

        // 未配置 truncate/max_lines(1) 的普通 Label：截断概念不适用 → None。
        let plain = Label::new("短文本");
        assert_eq!(plain.text_truncated(), None);
    }

    /// **多行**限行的截断状态同样要如实反映，否则 tooltip 会无条件弹。
    ///
    /// 多行路径只做高度裁剪、不重排文本，paint 期拿不到"完整排版有多高"，故判断落在
    /// measure：那里已经有完整排版高度与 `max_lines` 封顶值。此前这一支恒返回 `None`，
    /// 于是 `Tree::node_tooltip` 的门控失效——两行说明哪怕一个字都没被裁，悬停也会弹出
    /// 一个与可见文字一模一样的提示（下游的多行设置行正踩这个）。
    #[test]
    fn multiline_label_reports_truncation_from_measure() {
        use crate::core::Widget;
        use crate::text::LineAwareTextEngine;
        let mut te = LineAwareTextEngine;
        let style = Style::default();
        let mut label = Label::new("这是一段需要折成好几行才放得下的较长说明文字");
        label.max_lines = Some(2);

        // 窄到必须折成三行以上 → 两行封顶必然裁掉内容。
        label.measure(Size::new(60, 1000), &style, &mut te);
        assert_eq!(label.text_truncated(), Some(true), "折行数超过上限即为截断");

        // 给足宽度：一行就放得下，没有任何内容被裁。
        label.measure(Size::new(4000, 1000), &style, &mut te);
        assert_eq!(
            label.text_truncated(),
            Some(false),
            "没裁到内容就不该报截断"
        );

        // 不限行的 Label：截断概念不适用。
        let free = Label::new("同样的文字，但不限行");
        free.measure(Size::new(60, 1000), &style, &mut te);
        assert_eq!(free.text_truncated(), None);
    }

    #[test]
    fn label_paint_wires_enabled_to_text_color() {
        use crate::core::Widget;
        let style = Style {
            fg: Color::hex(0x123456),
            fg_role: None, // 显式 fg 覆盖（不走主题角色）
            ..Style::default()
        };
        let r = Rect::new(0, 0, 100, 20);
        let disabled_col = crate::theme::current().palette.text_disabled;

        let paint_color = |draw: &dyn Fn(&mut CaptureCanvas)| {
            let mut cv = CaptureCanvas::default();
            draw(&mut cv);
            cv.last_text_color.get()
        };

        // Label：启用取 style.fg，禁用取 text_disabled。
        let label = Label::new("x");
        assert_eq!(
            paint_color(&|cv| label.paint(r, r, false, true, cv, &style)),
            Some(Color::hex(0x123456))
        );
        assert_eq!(
            paint_color(&|cv| label.paint(r, r, false, false, cv, &style)),
            Some(disabled_col)
        );

        // 绑信号的 label：独立覆盖（不依赖"共用同一函数"的隐含推理）。
        let dl = Label::new(crate::signal::signal(String::from("y")));
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

    /// `field` 的标签列宽与行高必须来自主题，且改主题真的改得动——这正是把尺寸
    /// 从签名里拿掉的理由，接不上主题就等于换了个地方硬编码。
    #[test]
    fn field_lays_out_label_column_from_theme() {
        let th = crate::theme::current();
        let (lw, gap, h) = (th.form.label_width(), th.form.gap(), th.form.row_height());
        // 套一层填充容器：`layout` 会把根拉满窗口，直接量根就量不到行的固有高度。
        let row_of = |el: Element| {
            let tree = layout(Element::col().fill().child(el));
            let row = tree.get(tree.root.unwrap()).unwrap().children[0];
            (tree.get(row).unwrap().bounds, {
                let kids = tree.get(row).unwrap().children.clone();
                kids.iter()
                    .map(|k| tree.get(*k).unwrap().bounds)
                    .collect::<Vec<_>>()
            })
        };

        let (row, kids) = row_of(Element::field("音量", Element::leaf().width(40).height(20)));
        assert_eq!(kids.len(), 2, "field = 标签 + 控件");
        assert_eq!(row.h, h, "行高应取主题");
        assert_eq!(kids[0].w, lw, "标签列宽应取主题");
        assert_eq!(kids[1].x, lw + gap, "控件紧跟标签列（列宽 + 间距）");

        // 换主题后重建，尺寸随之改变。
        let mut t2 = crate::theme::Theme::default();
        t2.form.label_width = Some(60);
        t2.form.row_height = Some(28);
        crate::theme::set_current(std::rc::Rc::new(t2));
        let (row, kids) = row_of(Element::field("音量", Element::leaf().width(40).height(20)));
        assert_eq!(row.h, 28);
        assert_eq!(kids[0].w, 60);
        crate::theme::set_current(std::rc::Rc::new(crate::theme::Theme::default()));
    }

    /// `setting_row` 的立身之本是「控件贴右缘」。左块靠 `weight(1.0)` 吃掉剩余宽度，
    /// 一旦丢了这个权重控件就会缩回标签旁边，与 `field` 再无区别。
    #[test]
    fn setting_row_pins_control_to_trailing_edge() {
        let tree = layout(Element::col().fill().child(Element::setting_row(
            "隐藏状态栏",
            Element::leaf().width(40).height(20),
        )));
        let row = tree.get(tree.root.unwrap()).unwrap().children[0];
        let kids = tree.get(row).unwrap().children.clone();
        assert_eq!(kids.len(), 2, "setting_row = 左块 + 控件");
        let ctl = tree.get(kids[1]).unwrap().bounds;
        assert_eq!(ctl.x + ctl.w, 200, "控件右缘应贴住 200px 宽的行右缘");
        // 单行设置行定高，与 field 同一档：一列表单行必须等高。
        let f = crate::theme::current();
        assert_eq!(
            tree.get(row).unwrap().bounds.h,
            f.form.row_height(),
            "单行 setting_row 应取主题行高"
        );

        // 带副标题的行反过来按内容撑高，否则副标题会被定高挤掉。
        let tree = layout(Element::col().fill().child(Element::setting_row_desc(
            "隐藏状态栏",
            "启动后自动隐藏",
            Element::leaf().width(40).height(20),
        )));
        let row = tree.get(tree.root.unwrap()).unwrap().children[0];
        let left = tree.get(row).unwrap().children[0];
        let (h, left_h) = (
            tree.get(row).unwrap().bounds.h,
            tree.get(left).unwrap().bounds.h,
        );
        assert_eq!(
            h,
            left_h + 2 * f.form.row_pad_y(),
            "副标题行 = 内容 + 内边距"
        );
        assert!(h > f.form.row_height(), "两行文字应撑得比单行行高更高");
    }

    /// 带副标题的行：说明排在标签**下方**（同一左块内），且用弱化文字色——
    /// 副标题若与标签同色同号，两行字会读成两个并列标签。
    ///
    /// 取 `TextSubtle` 这**第三档**而非 `TextMuted`：标题已是正文档，说明再压一档
    /// 才拉得开层次；`TextMuted` 是次级正文的档位，用在这里两行字仍显得同重。
    #[test]
    fn setting_row_desc_stacks_subtle_description_under_label() {
        let el = Element::setting_row_desc("模糊音纠错", "z/zh 不区分", Element::leaf().width(40));
        let left = &el.children[0];
        assert_eq!(left.children.len(), 2, "左块 = 标签 + 副标题");
        assert_eq!(
            left.children[1].style.fg_role,
            Some(crate::style::Role::TextSubtle),
            "副标题应为三级弱化文字色"
        );
        assert!(
            left.children[1].style.font_size < left.children[0].style.font_size,
            "副标题字号应小于标签"
        );
        // 无副标题时左块只有标签，不留空节点占位。
        let plain = Element::setting_row("模糊音纠错", Element::leaf().width(40));
        assert_eq!(plain.children[0].children.len(), 1);
    }

    /// 表单行的限行开关在主题上：不设即不限（保持"按内容换行"的既有默认），设了则
    /// 末尾省略与看全文的 tooltip **一并**到位——截断意味着信息不完整，只配前一半
    /// 等于把说明文字直接丢掉。说明文字长度常由后端数据决定，而 `setting_row_desc`
    /// 返回的是拼好的容器、调用方够不到内部 label，这条路只能由主题给。
    #[test]
    fn form_rows_clamp_lines_only_when_theme_asks() {
        fn label_at(el: &mut Element, path: &[usize]) -> (Option<usize>, Truncate, Option<String>) {
            let mut cur = el;
            for i in path {
                cur = &mut cur.children[*i];
            }
            let tip = cur.tooltip.clone();
            let l = cur
                .widget
                .as_any_mut()
                .and_then(|a| a.downcast_mut::<Label>())
                .expect("该位置应是 label");
            (l.max_lines, l.truncate, tip)
        }
        let long = "第一次按键直接上屏符号，再次按键改为另一形态，所见即所得";
        let row = || Element::setting_row_desc("智能符号", long, Element::leaf().width(40));

        // 默认主题：不限行、不省略、不挂提示。
        let (ml, tr, tip) = label_at(&mut row(), &[0, 1]);
        assert_eq!(
            (ml, tr, tip),
            (None, Truncate::None, None),
            "默认应保持不限行"
        );

        let mut t = crate::theme::Theme::default();
        t.form.label_max_lines = Some(1);
        t.form.desc_max_lines = Some(1);
        crate::theme::set_current(std::rc::Rc::new(t));

        let (ml, tr, tip) = label_at(&mut row(), &[0, 1]);
        assert_eq!(ml, Some(1), "副标题应按主题限行");
        assert_eq!(tr, Truncate::End, "限行须配末尾省略，否则是硬裁");
        assert_eq!(tip.as_deref(), Some(long), "截断后须能悬浮看全文");
        // 标签走同一条路（`field` 的标签列同样会被长文本撑破）。
        let (ml, _, tip) = label_at(&mut row(), &[0, 0]);
        assert_eq!(ml, Some(1));
        assert_eq!(tip.as_deref(), Some("智能符号"));

        // 含换行的说明：仍限行，但不挂 tooltip——`Element::tooltip` 只收单行，
        // 库替调用方加的提示不该把 debug_assert 引爆在人家头上。
        let mut multi = Element::setting_row_desc("标题", "第一行\n第二行", Element::leaf());
        let (ml, _, tip) = label_at(&mut multi, &[0, 1]);
        assert_eq!(ml, Some(1), "多行文本同样限行");
        assert_eq!(tip, None, "含换行时跳过 tooltip");

        crate::theme::set_current(std::rc::Rc::new(crate::theme::Theme::default()));
    }

    /// 卡片 = 标题 + 分隔线 + 内容，底色走角色延迟解析（运行期换主题跟随）。
    #[test]
    fn card_stacks_title_divider_and_body() {
        use crate::style::{Brush, Role};
        let el = Element::card("通知", Element::label("正文"));
        assert_eq!(el.children.len(), 3, "卡片 = 标题 + 分隔线 + 内容");
        assert!(
            matches!(el.style.bg, Some(Brush::Role(Role::Surface))),
            "卡片底色应为 Surface 角色（换主题自动跟随）"
        );
        assert!(
            matches!(el.children[1].style.bg, Some(Brush::Role(Role::Divider))),
            "第二个子节点应是分隔线"
        );
        // 标题不设死高：长标题要能在卡片宽度内换行，分隔线随之下移。
        assert_eq!(el.children[0].height, crate::spec::Dimension::Wrap);
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

    /// 在 200×200 窗口里布局「顶部标题栏 + 模态对话框」，面板尺寸由参数给定。
    fn layout_titlebar_with_modal(panel_w: i32, panel_h: i32) -> Tree {
        let show = crate::signal::signal(true);
        let titlebar = Element::row()
            .width_match()
            .height(40)
            .window_drag()
            .child(Element::label("标题").width(120).height(40))
            .child(
                Element::window_button(WindowButtonKind::Close)
                    .width(46)
                    .height(40),
            );
        layout(
            Element::stack()
                .fill()
                .child(Element::col().fill().child(titlebar))
                .child(Element::dialog(
                    show,
                    Element::col()
                        .width(panel_w)
                        .height(panel_h)
                        .bg_role(crate::style::Role::Surface),
                )),
        )
    }

    #[test]
    fn window_drag_survives_modal_scrim() {
        // 回归：模态遮罩全窗覆盖，普通命中会停在遮罩上，导致无边框窗口弹出对话框后
        // 标题栏拿不到 HTCAPTION、整窗拖不动。拖动判定须穿透遮罩（scrim_passthrough）。
        let tree = layout_titlebar_with_modal(120, 80); // 居中面板 y 60..140，不压标题栏
        assert!(
            tree.drag_hit_at(Point::new(40, 20)),
            "对话框弹出时，遮罩下的标题栏文字区仍应可拖窗"
        );
        assert!(
            !tree.drag_hit_at(Point::new(130, 20)),
            "窗口按钮区仍不拖窗（交按钮处理）"
        );
        // 模态语义不变：遮罩照旧屏蔽标题栏窗口按钮（交互判定走普通命中，不穿透）。
        assert!(
            !tree.interactive_hit_at(Point::new(130, 20)),
            "模态期间窗口按钮应被遮罩屏蔽，不判为交互控件"
        );
    }

    #[test]
    fn modal_panel_over_titlebar_blocks_drag() {
        // 面板自身压住标题栏的部分不可拖——按的是对话框内容，不是标题栏。
        let tree = layout_titlebar_with_modal(120, 200); // 居中面板 x 40..160，y 0..200
        assert!(
            !tree.drag_hit_at(Point::new(100, 20)),
            "被对话框面板压住的标题栏区域不应拖窗"
        );
        assert!(
            tree.drag_hit_at(Point::new(20, 20)),
            "面板左侧露出的标题栏区域仍可拖窗"
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
    fn window_button_space_requests_op() {
        // 回归：窗口按钮 focusable=true（供 drag_hit_at 判定），Tab 能停上去，
        // 但此前 on_event 只有 Pointer 分支——按空格没反应，成了键盘死角。
        let mut tree = layout(
            Element::window_button(WindowButtonKind::Minimize)
                .width(46)
                .height(40),
        );
        let btn = tree.focusable_order()[0];
        let k = |key| crate::event::KeyEvent {
            key,
            pressed: true,
            shift: false,
            ctrl: false,
        };
        let res = tree.dispatch_key(k(Key::Space), Some(btn));
        assert_eq!(
            res.window_op,
            Some(crate::event::WindowOp::Minimize),
            "空格应等同点击，请求 Minimize"
        );
        let res = tree.dispatch_key(k(Key::Enter), Some(btn));
        assert_eq!(
            res.window_op,
            Some(crate::event::WindowOp::Minimize),
            "回车同理"
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

    /// 自绘控件可按当前命中项动态给出提示，且优先于节点静态文本；
    /// 返回 None 时回退到静态文本。
    #[test]
    fn widget_tooltip_overrides_static_and_falls_back() {
        struct Chart {
            hit: std::rc::Rc<std::cell::Cell<Option<u8>>>,
        }
        impl crate::core::Widget for Chart {
            fn measure(
                &self,
                _a: crate::geometry::Size,
                _s: &crate::style::Style,
                _t: &mut dyn crate::text::TextEngine,
            ) -> crate::geometry::Size {
                crate::geometry::Size::new(100, 30)
            }
            fn tooltip(&self) -> Option<String> {
                self.hit.get().map(|i| format!("第 {i} 项"))
            }
        }

        let hit = std::rc::Rc::new(std::cell::Cell::new(None));
        let tree = layout(
            Element::col().fill().child(
                Element::leaf()
                    .widget(Chart { hit: hit.clone() })
                    .width(100)
                    .height(30)
                    .tooltip("静态说明"),
            ),
        );
        let node = tree.hit_test(Point::new(20, 15)).expect("应命中图表");

        // 未命中数据点 → 回退到节点静态文本。
        assert_eq!(tree.node_tooltip(node).as_deref(), Some("静态说明"));

        // 命中某项 → 控件的动态文案胜出。
        hit.set(Some(3));
        assert_eq!(tree.node_tooltip(node).as_deref(), Some("第 3 项"));
    }

    /// 测试用最小自绘控件。
    struct Probe;
    impl crate::core::Widget for Probe {
        fn measure(
            &self,
            _a: crate::geometry::Size,
            _s: &crate::style::Style,
            _t: &mut dyn crate::text::TextEngine,
        ) -> crate::geometry::Size {
            crate::geometry::Size::new(10, 10)
        }
    }

    /// `widget()` 的正常用法：空槽位（leaf 与三种容器）都能挂。
    #[test]
    fn widget_mounts_on_empty_slots() {
        for e in [
            Element::leaf(),
            Element::col(),
            Element::row(),
            Element::stack(),
        ] {
            let _ = e.widget(Probe);
        }
    }

    /// 守卫回归：挂到已经是控件的节点上，等于把原控件静默丢掉——按钮不再是
    /// 按钮，却既不报错也没有任何迹象。debug 下必须当场炸出来。
    #[test]
    #[should_panic(expected = "该节点已有控件")]
    fn widget_rejects_replacing_a_control() {
        let _ = Element::button("确定").widget(Probe);
    }

    /// 同一守卫覆盖组合构造器：`scroll()` 是容器，但它内部已挂了滚动控件，
    /// 盖掉就没有滚动了。
    #[test]
    #[should_panic(expected = "该节点已有控件")]
    fn widget_rejects_replacing_a_composed_container_widget() {
        let _ = Element::scroll().widget(Probe);
    }

    /// 无静态 tooltip 的自绘控件，未命中时不应弹出空提示。
    #[test]
    fn widget_tooltip_absent_yields_none() {
        struct Silent;
        impl crate::core::Widget for Silent {
            fn measure(
                &self,
                _a: crate::geometry::Size,
                _s: &crate::style::Style,
                _t: &mut dyn crate::text::TextEngine,
            ) -> crate::geometry::Size {
                crate::geometry::Size::new(50, 20)
            }
        }
        let tree = layout(
            Element::col()
                .fill()
                .child(Element::leaf().widget(Silent).width(50).height(20)),
        );
        let node = tree.hit_test(Point::new(10, 10)).expect("应命中控件");
        assert_eq!(tree.node_tooltip(node), None);
    }

    /// 启用/可见两轴的三形态（静态 / 信号 / 闭包）各自能独立关掉节点，且互为取与。
    #[test]
    fn enabled_and_visible_three_forms_each_gate() {
        let en = signal(true);
        let vis = signal(true);
        let cond_en = signal(true);
        let cond_vis = signal(true);
        let tree = layout(
            Element::col()
                .fill()
                .child(Element::button("静态").enabled(false).visible(false))
                .child(
                    Element::button("信号")
                        .enabled_signal(en)
                        .visible_signal(vis),
                )
                .child(
                    Element::button("闭包")
                        .enabled_when(move || cond_en.get())
                        .visible_when(move || cond_vis.get()),
                ),
        );
        let kids = tree.get(tree.root.unwrap()).unwrap().children.clone();
        let node = |i: usize| tree.get(kids[i]).unwrap();
        assert!(!node(0).own_enabled(), "enabled(false) 应禁用");
        assert!(!node(0).effective_visible(), "visible(false) 应隐藏");
        // 信号/闭包形态默认放行，翻转后各自关掉自己那一路。
        assert!(node(1).own_enabled() && node(1).effective_visible());
        assert!(node(2).own_enabled() && node(2).effective_visible());
        en.set(false);
        vis.set(false);
        assert!(!node(1).own_enabled(), "enabled_signal(false) 应禁用");
        assert!(!node(1).effective_visible(), "visible_signal(false) 应隐藏");
        cond_en.set(false);
        cond_vis.set(false);
        assert!(!node(2).own_enabled(), "enabled_when 假值应禁用");
        assert!(!node(2).effective_visible(), "visible_when 假值应隐藏");
    }

    /// 常量禁用只落静态位，不得为此分配信号——signal 槽尚未回收，
    /// 每个 `.disabled(true)` 占一个永不释放的槽是纯泄漏。
    #[test]
    fn static_disable_allocates_no_signal() {
        for el in [
            Element::button("A").disabled(true),
            Element::button("B").enabled(false),
        ] {
            let mut tree = Tree::new();
            let id = el.build(&mut tree);
            let n = tree.get(id).unwrap();
            assert!(n.enabled.is_none(), "静态禁用不应挂信号");
            assert!(!n.enabled_static, "静态禁用应落在 enabled_static 上");
            assert!(!n.own_enabled());
        }
        // disabled(false) 是显式启用，与默认一致。
        let mut tree = Tree::new();
        let id = Element::button("C").disabled(false).build(&mut tree);
        assert!(tree.get(id).unwrap().own_enabled());
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

    // ---- 动态文案（TextContent 绑 Signal<String>）----

    /// 绑信号的控件在测量时现取文案：改信号 + 重排 → 节点宽度跟着变。
    /// 覆盖 button / link / label / checkbox 四个控件（各自独立断言，不靠"共用同一
    /// 载体类型"的隐含推理——真正共用的是 `TextContent`，但每个控件都得自己在
    /// measure 里 resolve 一次，漏掉哪个都只有该控件不跟随）。
    fn measured_width_after(el: Element, caption: Signal<String>, next: &str) -> (i32, i32) {
        // 控件挂在行容器里：根节点会被拉伸到窗口宽，只有非根的 Wrap 宽度节点才反映测量值。
        let mut tree = Tree::new();
        let root = Element::row().fill().child(el).build(&mut tree);
        tree.root = Some(root);
        let relayout = |tree: &mut Tree| {
            tree.layout_root(Size::new(400, 200), &mut crate::text::NullTextEngine)
        };
        relayout(&mut tree);
        let child = tree.get(root).unwrap().children[0];
        let before = tree.get(child).unwrap().bounds.w;
        caption.set(String::from(next));
        relayout(&mut tree);
        let after = tree.get(child).unwrap().bounds.w;
        (before, after)
    }

    #[test]
    fn button_caption_signal_changes_measured_width() {
        let caption = signal(String::from("播放"));
        let (before, after) = measured_width_after(Element::button(caption), caption, "暂停播放");
        assert!(
            after > before,
            "按钮文案变长后应重新测量出更大的宽度（{before} → {after}）"
        );
    }

    #[test]
    fn link_caption_signal_changes_measured_width() {
        let caption = signal(String::from("展开"));
        let (before, after) = measured_width_after(
            Element::link(caption).url("https://example.com"),
            caption,
            "收起全部细节",
        );
        assert!(
            after > before,
            "链接文案变长后应重新测量（{before} → {after}）"
        );
    }

    #[test]
    fn label_caption_signal_changes_measured_width() {
        let caption = signal(String::from("就绪"));
        let (before, after) =
            measured_width_after(Element::label(caption), caption, "同步中，请稍候");
        assert!(
            after > before,
            "标签文案变长后应重新测量（{before} → {after}）"
        );
    }

    #[test]
    fn checkbox_caption_signal_changes_measured_width() {
        let caption = signal(String::from("启用"));
        let state = signal(false);
        let (before, after) =
            measured_width_after(Element::checkbox(caption, state), caption, "启用后台同步");
        assert!(
            after > before,
            "复选框文案变长后应重新测量（{before} → {after}）"
        );
    }

    #[test]
    fn painted_text_follows_signal_in_every_bound_widget() {
        // 测量之外还得确认真的画的是新串（只测宽度的话，把文案缓存进截断结果里的实现
        // 也能蒙混过关），且**每个**控件都得自己在 paint 里 resolve 一次——漏掉哪个
        // 就只有那个控件不跟随，故逐个独立断言而非抽样。
        let caption = signal(String::from("播放"));
        let style = Style::default();
        let r = Rect::new(0, 0, 200, 30);
        let group = signal(0usize);
        let checked = signal(false);
        let widgets: Vec<Box<dyn Widget>> = vec![
            Box::new(Label::new(caption)),
            Box::new(Button::new(caption)),
            Box::new(link::Link::new(caption)),
            Box::new(CheckBox::new(caption, checked)),
            Box::new(RadioButton::new(caption, group, 0)),
            Box::new(nav::NavRow::new(caption)),
            Box::new(containers::IconButton::glyph(caption)),
        ];
        let painted = |w: &dyn Widget| {
            let mut cv = CaptureCanvas::default();
            w.paint(r, r, false, true, &mut cv, &style);
            let t = cv.texts.borrow().clone();
            t
        };
        for w in &widgets {
            let got = painted(w.as_ref());
            assert!(
                got.iter().any(|t| t == "播放"),
                "初始文案应画出信号当前值，实得 {got:?}"
            );
        }
        caption.set(String::from("暂停"));
        for w in &widgets {
            let got = painted(w.as_ref());
            assert!(
                got.iter().any(|t| t == "暂停"),
                "改信号后应画出新文案，实得 {got:?}"
            );
        }
    }

    #[test]
    fn truncation_cache_keyed_by_text_not_stale() {
        // 截断缓存的 key 必须含文案：否则绑信号的 label 换了文案仍画上一次的截断串。
        let caption = signal(String::from("AAAAAAAAAAAAAAAAAAAA"));
        let label = Label {
            text: TextContent::from(caption),
            max_lines: Some(1),
            truncate: Truncate::End,
            trunc_cache: RefCell::new(None),
            multiline_overflow: Cell::new(false),
        };
        let style = Style::default();
        let r = Rect::new(0, 0, 40, 20);
        let painted = |draw: &dyn Fn(&mut WidthCanvas)| {
            let mut cv = WidthCanvas;
            draw(&mut cv);
        };
        // 先画一次把截断结果写进缓存。
        painted(&|cv| label.paint(r, r, false, true, cv, &style));
        let first = label.trunc_cache.borrow().clone().unwrap();
        caption.set(String::from("BBBBBBBBBBBBBBBBBBBB"));
        painted(&|cv| label.paint(r, r, false, true, cv, &style));
        let second = label.trunc_cache.borrow().clone().unwrap();
        assert_ne!(first.0, second.0, "缓存键里的文案应随信号更新");
        assert!(
            second.3.starts_with('B'),
            "换文案后应重算截断串，实得 {}",
            second.3
        );
    }

    #[test]
    fn click_writing_signal_requests_relayout_not_just_repaint() {
        // 文案变化会改变测量宽度，光重绘不够。点击回调里写信号时，核心把失效强度
        // 升到 `DamageReq::Layout`（宿主据此置 needs_full → 整窗帧必先 layout_root
        // 重新 measure）。这条链是动态文案能"改宽度"的前提，故在此锁住。
        let caption = signal(String::from("播放"));
        let mut tree = Tree::new();
        let root = Element::button(caption)
            .width(120)
            .height(30)
            .on_click(move |_| caption.set(String::from("暂停")))
            .build(&mut tree);
        tree.root = Some(root);
        tree.layout_root(Size::new(200, 60), &mut crate::text::NullTextEngine);
        let mut hover = None;
        let mut capture = None;
        let at = Point::new(10, 10);
        tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Down, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        let res = tree.dispatch_pointer(
            PointerEvent::single(PointerKind::Up, at, MouseButton::Left),
            &mut hover,
            &mut capture,
        );
        assert!(
            matches!(res.damage, crate::core::DamageReq::Layout(_)),
            "点击回调里写信号应升级为 Layout 级失效（仅 Rect 会导致文案变了却不重排）"
        );
        assert_eq!(caption.get(), "暂停");
    }
}
