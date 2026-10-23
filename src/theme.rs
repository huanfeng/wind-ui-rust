//! 主题系统：集中定义颜色 / 间距 / 字体，避免控件硬编码。
//!
//! 两层模型：`Palette`+`Metrics` 是全局 base；每个控件主题用 `Option` 字段做覆盖层
//! （`None` 回退到 base，`Some` 即覆盖）。整体可与 TOML 互转（serde），为外部主题文件打底。
//!
//! 控件经 `theme::current()` 读取当前主题（thread_local，未设置时为默认主题——
//! 故单元测试无需显式设置）。宿主在每帧布局/绘制前 `set_current`。

use std::cell::RefCell;
use std::rc::Rc;

use serde::{Deserialize, Serialize};

use crate::anim::{Easing, Spring};
use crate::geometry::Color;
use crate::style::Shadow;
use crate::ui::caret::{self, CaretStyle};

/// 全局基础调色板。
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Palette {
    pub accent: Color,
    pub accent_hover: Color,
    pub accent_active: Color,
    /// 强调色之上的前景（按钮文字等）。
    pub on_accent: Color,
    /// 强调色**当前景**用时的取值：选中项文字、链接、描边按钮的字与边、徽章字。
    ///
    /// 与 `accent` 分开，是因为两种用法对明度的要求相反：`accent` 要当白字的底
    /// （够深才读得清白字），而当字用时要在 `bg`/`surface` 上读得清。亮色主题二者
    /// 可以同值；暗色主题下同一个蓝压在藏青底上只有约 3:1，必须提亮一档。
    ///
    /// `None`（默认）= 由 `accent` 推导（见 [`Palette::accent_text`]）：只改 `accent`
    /// 的主题，链接、焦点环、选中文字会一起换色，不会出现两种蓝。需要精调时再显式给值。
    pub accent_text: Option<Color>,
    /// 窗口背景。
    pub bg: Color,
    /// 卡片 / 输入框等表面。
    pub surface: Color,
    /// 次级表面（斑马纹等）。
    pub surface_alt: Color,
    /// 反色表面：与当前主题**明暗相反**的实底条块（深色标题栏、浅底上的深色横幅）。
    /// 取值即本主题的 `text`——正文色天生就是"在 bg 上对比最强的那一档"，
    /// 用它当反色底能保证与页面其余部分同属一套色相，不像另起一个中性灰那样显得外来。
    pub surface_inverse: Color,
    /// 反色表面之上的前景。取值即本主题的 `bg`，与 `surface_inverse` 互为对方的底。
    pub on_surface_inverse: Color,
    pub text: Color,
    pub text_muted: Color,
    /// 三级弱化文字：比 `text_muted` 更淡，但**不是**禁用也**不是**占位——
    /// 版权行、脚注、时间戳这类"在场即可、不参与阅读"的字。
    /// 借用 `text_disabled` 会把可读内容说成不可交互，借用 `placeholder` 则暗示"待填写"，
    /// 二者语义都不对；没有这一档时正文的视觉层级只有两级，长页面里分不出主次。
    /// 亮度介于 `text_muted` 与 `text_disabled` 之间（见 `text_tiers_are_ordered` 测试）。
    pub text_subtle: Color,
    pub text_disabled: Color,
    pub border: Color,
    /// 关闭态轨道（开关 / 滑块）。
    pub track: Color,
    pub placeholder: Color,
    pub divider: Color,
    pub danger: Color,
    /// 成功 / 已完成语义色（状态圆点、通过标记、低优先级）。
    /// 与 `danger` 同为语义色槽，[`Intent::Success`] 由它派生整组视觉。
    pub success: Color,
    /// 警告 / 需注意语义色（非阻断提示、中优先级标记）。
    ///
    /// 取值刻意比"品牌琥珀"暗一档：警告色经常直接当**前景**用（标签文字、边框），
    /// 而饱和的亮黄在浅色表面上只有 1.9:1，字一上去就糊。亮色取 amber-700，对白底约 5:1，
    /// 与 `danger` 的 4.8:1 同量级，既保住琥珀色相又能当字用。
    pub warning: Color,
}

impl Default for Palette {
    /// 亮色：Tailwind slate 灰阶 + blue-600 强调色（shadcn 一路的现代观感）。
    /// 所有灰出自同一条 slate 色阶，避免"背景纯灰、正文偏青、边框偏蓝"三套色相混用。
    fn default() -> Self {
        Self {
            accent: Color::hex(0x2563EB),
            // 亮色下 hover/按下都**变深**：变亮会把白字的对比度从 5.2 拉到 3 以下。
            accent_hover: Color::hex(0x1D4ED8),
            accent_active: Color::hex(0x1E40AF),
            on_accent: Color::WHITE,
            accent_text: None,
            bg: Color::hex(0xF8FAFC),
            surface: Color::WHITE,
            surface_alt: Color::hex(0xF1F5F9),
            surface_inverse: Color::hex(0x0F172A),
            on_surface_inverse: Color::hex(0xF8FAFC),
            text: Color::hex(0x0F172A),
            text_muted: Color::hex(0x475569),
            text_subtle: Color::hex(0x64748B),
            text_disabled: Color::hex(0x94A3B8),
            border: Color::hex(0xCBD5E1),
            track: Color::hex(0xCBD5E1),
            placeholder: Color::hex(0x94A3B8),
            divider: Color::hex(0xE2E8F0),
            danger: Color::hex(0xDC2626),
            // green-700 而非 green-600：后者当字用对白底只有 3.3:1。
            success: Color::hex(0x15803D),
            warning: Color::hex(0xB45309),
        }
    }
}

/// 交互态叠层的不透明度：hover / 按下 / 选中淡底。全库的悬停、按下、选中反馈都从这三档取，
/// 不再各控件各配一个 alpha（此前有 0x12 / 0x18 / 0x22 / 5% / 6% / 8% 七种）。
pub const HOVER_ALPHA: f32 = 0.06;
/// 按下叠层不透明度，见 [`HOVER_ALPHA`]。
pub const PRESS_ALPHA: f32 = 0.10;
/// 选中淡底不透明度（强调色），见 [`HOVER_ALPHA`]。
pub const SELECTED_ALPHA: f32 = 0.12;

impl Palette {
    /// 悬停叠层：文字色 × [`HOVER_ALPHA`]。叠在任何底色上都读得出来（亮底变灰、暗底变亮），
    /// 不必为卡片 / 窗口底 / 斑马纹各配一个实色。
    pub fn hover_overlay(&self) -> Color {
        self.text.scale_alpha(HOVER_ALPHA)
    }
    /// 按下叠层：文字色 × [`PRESS_ALPHA`]。
    pub fn press_overlay(&self) -> Color {
        self.text.scale_alpha(PRESS_ALPHA)
    }
    /// 选中淡底：强调色 × [`SELECTED_ALPHA`]。列表、表格、菜单这类"一列里选一项"的选中态用它
    /// 配 `accent_text` 字；分段控件、胶囊标签这类"几个里选一个"的强选择用 accent 实底。
    pub fn selected_soft(&self) -> Color {
        self.accent.scale_alpha(SELECTED_ALPHA)
    }

    /// 强调色当前景用的取值：显式设了 `accent_text` 用它；否则亮色主题即 `accent`，
    /// 暗色主题把 `accent` 向白提亮 35%（blue-600 → 约 blue-400，在 slate-800 上 ≥ 4.5:1）。
    pub fn accent_text(&self) -> Color {
        self.accent_text.unwrap_or_else(|| {
            if is_dark(self) {
                self.accent.lighten(0.35)
            } else {
                self.accent
            }
        })
    }

    /// 暗色调色板预设（与亮色 Default 语义角色一一对应，仅取暗色值）。
    pub fn dark() -> Self {
        Self {
            // 按钮底沿用亮色同一个蓝（白字 5.2:1）；hover 在暗底上习惯**变亮**。
            accent: Color::hex(0x2563EB),
            accent_hover: Color::hex(0x3B82F6),
            accent_active: Color::hex(0x1D4ED8),
            on_accent: Color::WHITE,
            // 当前景用时由 accent 推导提亮一档（blue-600 压在 slate-800 上只有约 3:1）。
            accent_text: None,
            bg: Color::hex(0x0F172A),
            surface: Color::hex(0x1E293B),
            surface_alt: Color::hex(0x273449),
            // 暗色下的"反色"是**浅色**条块，故与亮色主题互为镜像：底取 text、字取 bg。
            surface_inverse: Color::hex(0xF1F5F9),
            on_surface_inverse: Color::hex(0x0F172A),
            text: Color::hex(0xF1F5F9),
            text_muted: Color::hex(0x94A3B8),
            text_subtle: Color::hex(0x7C8BA1),
            text_disabled: Color::hex(0x56657B),
            border: Color::hex(0x3B4A61),
            track: Color::hex(0x475569),
            placeholder: Color::hex(0x64748B),
            divider: Color::hex(0x2E3B50),
            // danger 仍取 red-600：它同时要当实底配白字（4.8:1），再亮就只能配深字了。
            // success / warning 取亮的一档拉开与暗底的对比；当实底时自动配深字。
            danger: Color::hex(0xDC2626),
            success: Color::hex(0x22C55E),
            warning: Color::hex(0xF59E0B),
        }
    }
}

/// 控件语义意图色。内置三核心 + 开放扩展点 `Custom`——使用者传任意基色即可，
/// 框架派生整组视觉（hover/active 变亮变暗、fg 按亮度自适应），零新增 palette 色槽。
///
/// `#[non_exhaustive]`：理由同 [`crate::style::Role`]——语义色持续演进，本版就补了
/// `Success` / `Warning`。下游对它做穷尽 `match` 须留 `_` 兜底；`Custom(Color)` 已经
/// 让"把所有 intent 一一列举"这件事失去意义（基色是无穷的），标注只是把这一点说清楚。
#[derive(Clone, Copy)]
#[non_exhaustive]
pub enum Intent {
    /// 主操作：accent 家族（控件默认）。
    Primary,
    /// 次要操作：中性灰。
    Neutral,
    /// 危险操作：palette.danger。
    Danger,
    /// 成功 / 已完成：palette.success。
    Success,
    /// 警告 / 需注意：palette.warning。
    Warning,
    /// 扩展点：任意基色，派生整组视觉。基色是**定色**，运行期换主题不跟随。
    Custom(Color),
    /// 扩展点：任意主题角色作基色，派生整组视觉。与 [`Intent::Custom`] 的唯一差别是
    /// 基色**延迟到解析时**才从当前主题取（同 [`crate::style::Brush::Role`]），
    /// 因而运行期换主题自动跟随。适合借用内置意图之外的 palette 色槽
    /// （`Role::AccentHover`、`Role::TextMuted` 等）。
    CustomRole(crate::style::Role),
}

/// `Intent` 解析出的一组语义色。控件各取所需（Button 用全部，CheckBox 取 bg+fg）。
#[derive(Clone, Copy, Debug)]
pub struct IntentColors {
    pub bg: Color,
    pub hover: Color,
    pub active: Color,
    /// 对比自适应前景：在 bg 上始终可读（Button 文字 / CheckBox 对勾共用）。
    pub fg: Color,
}

impl Intent {
    /// 解析为一组语义色。`Primary` 用 palette 精调的 accent 家族；其余 intent 由基色派生。
    ///
    /// [`Intent::CustomRole`] 的基色不取自参数 `p`，而是取自**当前线程活动主题**
    /// （`theme::current()`，同 `Brush::Role` 的解析口径）——角色可能落在 palette 之外的
    /// 覆盖层上（`Role::InputBg` 等），只有整个 `Theme` 才解得出。故本方法须在主题
    /// 已就位的时机调用（布局/绘制期），这也是控件唯一调它的地方。
    pub fn colors(self, p: &Palette) -> IntentColors {
        const L: f32 = 0.12; // 暗色主题 hover 变亮系数
        const D: f32 = 0.12; // hover 变深 / active 再深的系数
                             // hover 的方向跟随主题明暗：亮色下变深（变亮会把白字对比度拉低、按钮发白），
                             // 暗色下变亮（暗底上"更亮"才读作"被指着"）。按下恒为变深。
        let dark = is_dark(p);
        // 前景优先白字（`on_accent`）：饱和的中间色按钮配白字是主流观感，哪怕深字的对比度
        // 数值更高（`#2E9E5B` 上白字 3.4、深字 5.2，白字看着反而更清楚）。白字低于 3:1
        // （WCAG 对大字号 / UI 组件的门槛）才换深字——暗色主题的亮绿、亮琥珀属于这一种。
        // 深字候选不能直接用 `p.text`：暗色主题的 text 是浅色，两个候选会都是浅的。
        let ink = if dark { p.on_surface_inverse } else { p.text };
        let derive = |c: Color| IntentColors {
            bg: c,
            hover: if dark { c.lighten(L) } else { c.darken(D) },
            active: if dark { c.darken(D) } else { c.darken(D * 2.0) },
            fg: if contrast_ratio(p.on_accent, c) >= 3.0 {
                p.on_accent
            } else {
                ink
            },
        };
        match self {
            Intent::Primary => IntentColors {
                bg: p.accent,
                hover: p.accent_hover,
                active: p.accent_active,
                fg: p.on_accent,
            },
            // 次要操作用**文字色的半透明叠层**而非某个实色灰：叠在白卡片、灰底、斑马纹行、
            // 暗色表面上都能读出"一块按钮"，不必为每种底色各配一个灰。
            Intent::Neutral => IntentColors {
                bg: p.text.scale_alpha(0.08),
                hover: p.text.scale_alpha(0.13),
                active: p.text.scale_alpha(0.18),
                fg: p.text,
            },
            Intent::Danger => derive(p.danger),
            Intent::Success => derive(p.success),
            Intent::Warning => derive(p.warning),
            Intent::Custom(c) => derive(c),
            Intent::CustomRole(r) => derive(r.resolve(&current())),
        }
    }

    /// 徽章胶囊配色（收起态下拉当前项 / 展开态菜单项的尾随徽章通用）：返回 `(填充色, 文字色)`。
    /// 淡色底 + **可读**的同色系前景。Neutral 用 `text_muted`（够深可读），不再用浅灰 `border`
    /// 当字色——否则灰字灰底几乎看不清。
    pub fn badge_colors(self, p: &Palette) -> (Color, Color) {
        let fg = match self {
            Intent::Primary => p.accent_text(),
            Intent::Neutral => p.text_muted,
            Intent::Danger => p.danger,
            Intent::Success => p.success,
            Intent::Warning => p.warning,
            Intent::Custom(c) => c,
            Intent::CustomRole(r) => r.resolve(&current()),
        };
        (fg.scale_alpha(0.15), fg)
    }
}

/// 尺寸单位：支持随 DPI 缩放的逻辑像素和固定物理像素两种模式。
///
/// TOML 写法：
/// - `1.0`           → `Dp(1.0)`（向后兼容，随 DPI 等比缩放）
/// - `{ dp = 1.5 }`  → `Dp(1.5)`（显式逻辑像素）
/// - `{ px = 1 }`    → `Px(1.0)`（精确物理像素，任意 DPI 下清晰无模糊）
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Len {
    /// 逻辑像素（dp），随 DPI scale 等比放大。125%/150% 时映射到亚像素，
    /// 渲染器以抗锯齿混合（视觉略模糊）。
    Dp(f32),
    /// 精确物理像素。内部转换为 `px / scale` 的逻辑值，使 D2D/Skia 变换后
    /// 落在整数光栅，任意 DPI 下清晰无模糊。
    Physical { px: f32 },
}

impl Len {
    /// 将此尺寸换算为逻辑坐标值（传给 `Canvas::stroke_round_rect` 等图元的 `width` 参数）。
    /// `scale` 来自 `Canvas::dpi_scale()`。
    pub fn to_logical(self, scale: f32) -> f32 {
        match self {
            Len::Dp(v) => v,
            Len::Physical { px } => {
                if scale > 0.0 {
                    px / scale
                } else {
                    px
                }
            }
        }
    }
}

/// 全局基础度量（间距 / 控件尺寸 / 圆角 / 字号）。
///
/// 这是一套**有档位的 token**：控件与组合构造器的默认尺寸都从这里取，而不是各写各的
/// 数字——同一行里的按钮、输入框、下拉框因此天然等高，改一处全局生效。档位取值对齐
/// Tailwind 的 4px 栅格（4/8/12/16/24），与默认配色同一路数。
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Metrics {
    pub corner_sm: f32,
    pub corner_md: f32,
    pub corner_lg: f32,
    /// 控件边框（输入框 / 描边按钮 / 分段控件等）的线宽。
    /// 使用 `{ px = 1 }` 可在任意 DPI 下获得清晰的单像素边框。
    pub border_width: Len,
    /// 聚焦状态下的边框线宽（Dropdown 等有聚焦加粗行为的控件使用）。
    /// 默认比 `border_width` 略粗以突出焦点。
    pub border_width_focus: Len,
    /// 间距档位：紧贴的图标与文字、徽章内距。
    pub space_xs: i32,
    /// 间距档位：同组控件之间（按钮组、选项组）。
    pub space_sm: i32,
    /// 间距档位：卡片内各块之间、表单行之间。
    pub space_md: i32,
    /// 间距档位：卡片 / 面板内边距、卡片之间。
    pub space_lg: i32,
    /// 间距档位：页面边距、对话框内边距。
    pub space_xl: i32,
    /// 标准控件高度：按钮、输入框、下拉框、分段控件、步进器共用，同一行里天然等高。
    /// 字号调大时控件按文字自然增高，此值只是下限。
    pub control_h: i32,
    /// 小号控件高度（`ButtonSize::Small` 等密集工具栏用）。
    pub control_h_sm: i32,
    /// 列表类行高：list、nav_row、折叠标题行。
    pub row_h: i32,
    /// 字号档位：徽章、角标。
    pub font_xs: f32,
    /// 字号档位：说明文字、表头、tooltip。
    pub font_sm: f32,
    /// 字号档位：正文（所有没显式设字号的文字的默认值）。
    pub font_md: f32,
    /// 字号档位：卡片标题、对话框标题。
    pub font_lg: f32,
    /// 字号档位：页面标题。
    pub font_xl: f32,
}

impl Metrics {
    /// 标准控件的高度：`max(control_h, 字号 + 16)`。默认字号下即 `control_h`；
    /// 字号调大时按文字自然增高，不会把字挤出框。
    pub fn control_height(&self, font_size: f32) -> i32 {
        self.control_h.max(font_size.round() as i32 + 16)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            corner_sm: 4.0,
            corner_md: 6.0,
            corner_lg: 10.0,
            border_width: Len::Dp(1.0),
            border_width_focus: Len::Dp(1.8),
            space_xs: 4,
            space_sm: 8,
            space_md: 12,
            space_lg: 16,
            space_xl: 24,
            control_h: 32,
            control_h_sm: 28,
            row_h: 36,
            font_xs: 12.0,
            font_sm: 13.0,
            font_md: 14.0,
            font_lg: 16.0,
            font_xl: 20.0,
        }
    }
}

/// 按钮覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ButtonTheme {
    pub bg: Option<Color>,
    pub hover: Option<Color>,
    pub active: Option<Color>,
    /// 禁用态背景（默认回退 palette.track 灰）。
    pub disabled: Option<Color>,
    pub fg: Option<Color>,
    pub corner: Option<f32>,
}

impl ButtonTheme {
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.accent)
    }
    pub fn hover(&self, p: &Palette) -> Color {
        self.hover.unwrap_or(p.accent_hover)
    }
    pub fn active(&self, p: &Palette) -> Color {
        self.active.unwrap_or(p.accent_active)
    }
    /// 禁用态底色：`palette.track` 实色灰，配 `text_disabled` 字。不用半透明叠层——
    /// 那与 Neutral 按钮常态（文字色 8%）几乎一样淡，禁用与可用只剩字色之差。
    pub fn disabled(&self, p: &Palette) -> Color {
        self.disabled.unwrap_or(p.track)
    }
    pub fn fg(&self, p: &Palette) -> Color {
        self.fg.unwrap_or(p.on_accent)
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_md)
    }
}

/// 文本输入覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct InputTheme {
    pub bg: Option<Color>,
    /// 只读输入框（[`Element::read_only`](crate::ui::Element::read_only)）的底色，
    /// 默认 `palette.surface_alt`。正文仍用常规色，与禁用（同底、正文置灰）区分。
    pub bg_readonly: Option<Color>,
    pub border: Option<Color>,
    pub border_focus: Option<Color>,
    /// ⚠ **当前不生效**：`TextInput` 的正文色走节点 `Style` 的 `fg` / `fg_role`
    /// （`.fg(..)` / `.fg_role(..)`），不读这里。既有状况，非本版引入。
    pub text: Option<Color>,
    pub placeholder: Option<Color>,
    /// 选区高亮（含 alpha）。
    pub selection: Option<Color>,
    /// 区间底色标记（[`Element::marks`](crate::ui::Element::marks)，查找命中涂黄）的默认底色，
    /// 含 alpha。默认 `palette.warning` 的 40% alpha（琥珀色）：叠在明暗两种底上字都读得清，
    /// 且与选区的强调色分得开；自定义调色板时跟着变。
    pub mark: Option<Color>,
    pub cursor: Option<Color>,
    /// 插入光标闪烁风格（默认 [`CaretStyle::Blink`] 经典方波）。
    pub caret_style: Option<CaretStyle>,
    /// 插入光标宽度（默认 `2.0` 逻辑 px）。可写 `{ px = 2 }` 固定物理像素；
    /// 两种写法最终都会向下吸附到整数物理像素，非整数 DPI 下同样清晰。
    pub caret_width: Option<Len>,
    /// 插入光标两端是否半圆（默认 true）。
    pub caret_rounded: Option<bool>,
    /// 光标位置变化时是否在同一视觉行内滑行过去（默认 true）。
    pub caret_smooth_move: Option<bool>,
    /// 输入法合成串（未上屏的拼音等）下方那条下划线的颜色。
    /// 默认取正文色的 60% alpha——要能看出「这段还没定下来」，又不能抢正文的注意力。
    pub preedit_underline: Option<Color>,
    pub corner: Option<f32>,
}

impl InputTheme {
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.surface)
    }
    pub fn bg_readonly(&self, p: &Palette) -> Color {
        self.bg_readonly.unwrap_or(p.surface_alt)
    }
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn border_focus(&self, p: &Palette) -> Color {
        self.border_focus.unwrap_or(p.accent_text())
    }
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(p.text)
    }
    pub fn placeholder(&self, p: &Palette) -> Color {
        self.placeholder.unwrap_or(p.placeholder)
    }
    pub fn selection(&self, p: &Palette) -> Color {
        self.selection
            .unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x55))
    }
    pub fn mark(&self, p: &Palette) -> Color {
        self.mark
            .unwrap_or(Color::rgba(p.warning.r, p.warning.g, p.warning.b, 0x66))
    }
    pub fn cursor(&self, p: &Palette) -> Color {
        self.cursor.unwrap_or(p.text)
    }
    pub fn caret_style(&self) -> CaretStyle {
        self.caret_style.unwrap_or_default()
    }
    pub fn caret_width(&self) -> Len {
        self.caret_width.unwrap_or(Len::Dp(caret::DEFAULT_WIDTH))
    }
    pub fn caret_rounded(&self) -> bool {
        self.caret_rounded.unwrap_or(true)
    }
    pub fn caret_smooth_move(&self) -> bool {
        self.caret_smooth_move.unwrap_or(true)
    }
    /// 合成串下划线色。回退到正文色的 60% alpha。
    pub fn preedit_underline(&self, p: &Palette) -> Color {
        self.preedit_underline.unwrap_or_else(|| {
            let t = self.text(p);
            Color::rgba(t.r, t.g, t.b, (t.a as u16 * 60 / 100) as u8)
        })
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_md)
    }
}

/// 勾选/开关/单选/滑块共享覆盖层（强调色 + 关闭态轨道）。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToggleTheme {
    pub accent: Option<Color>,
    pub track: Option<Color>,
    /// 旋钮/勾的前景。
    pub knob: Option<Color>,
}

impl ToggleTheme {
    pub fn accent(&self, p: &Palette) -> Color {
        self.accent.unwrap_or(p.accent)
    }
    pub fn track(&self, p: &Palette) -> Color {
        self.track.unwrap_or(p.track)
    }
    pub fn knob(&self, p: &Palette) -> Color {
        self.knob.unwrap_or(p.surface)
    }
}

/// 下拉覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DropdownTheme {
    pub bg: Option<Color>,
    pub border: Option<Color>,
    pub border_focus: Option<Color>,
    /// 悬停边框。默认把 `border` 压深一档（暗色主题提亮一档）。
    pub border_hover: Option<Color>,
    pub text: Option<Color>,
    pub chevron: Option<Color>,
    pub corner: Option<f32>,
}

impl DropdownTheme {
    pub fn border_hover(&self, p: &Palette) -> Color {
        self.border_hover.unwrap_or_else(|| {
            let b = self.border(p);
            if is_dark(p) {
                b.lighten(0.15)
            } else {
                b.darken(0.15)
            }
        })
    }
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.surface)
    }
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn border_focus(&self, p: &Palette) -> Color {
        self.border_focus.unwrap_or(p.accent_text())
    }
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(p.text)
    }
    pub fn chevron(&self, p: &Palette) -> Color {
        self.chevron.unwrap_or(p.text_muted)
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_md)
    }
}

/// 浮层菜单覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MenuTheme {
    pub bg: Option<Color>,
    pub border: Option<Color>,
    pub text: Option<Color>,
    pub text_disabled: Option<Color>,
    pub hover: Option<Color>,
    pub accent: Option<Color>,
    /// 浮层投影：向下偏移 / 模糊半径 / 颜色（含 alpha）。任一为 `None` 取内置默认。
    ///
    /// 默认值刻意克制（偏移 3 / 模糊 9 / 黑 22%）：投影是用来把浮层从背景里"托起来"的，
    /// 它自己不该成为画面里最显眼的东西。大半径 + 高不透明度的投影在远程桌面这类有损
    /// 压缩通道上尤其难看——大片低对比渐变正是压缩最先牺牲的部分，会糊成块状。
    pub shadow_dy: Option<f32>,
    pub shadow_blur: Option<f32>,
    pub shadow_color: Option<Color>,
}

/// 菜单浮层投影的内置默认（取值理由见 [`MenuTheme::shadow_dy`]）。
const MENU_SHADOW_DY: f32 = 3.0;
const MENU_SHADOW_BLUR: f32 = 9.0;
const MENU_SHADOW_ALPHA: u8 = 56;

impl MenuTheme {
    /// 组装浮层投影。返回 [`Shadow`] 而非三元组：同名的 [`ReorderTheme::shadow`] 早已
    /// 返回 `Shadow`，两个 `shadow()` 同名不同型时，调用方拿到哪个全凭记忆。
    /// `dx`/`spread` 恒为 0——菜单投影只需"正下方托一层"，两个分量留在类型里
    /// 是为了与节点样式的 `style.shadow` 用同一个结构，浮层与普通节点的投影可互相搬。
    pub fn shadow(&self) -> Shadow {
        Shadow {
            dx: 0.0,
            dy: self.shadow_dy.unwrap_or(MENU_SHADOW_DY),
            blur: self.shadow_blur.unwrap_or(MENU_SHADOW_BLUR),
            spread: 0.0,
            color: self
                .shadow_color
                .unwrap_or(Color::rgba(0, 0, 0, MENU_SHADOW_ALPHA)),
        }
    }
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.surface)
    }
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(p.text)
    }
    pub fn text_disabled(&self, p: &Palette) -> Color {
        self.text_disabled.unwrap_or(p.text_disabled)
    }
    /// 悬停 / 键盘当前项的底。取按下档（[`PRESS_ALPHA`]）而非 hover 档：菜单项悬停只换底、
    /// 不换字色，键盘导航时"当前项"全靠这层底，太淡在暗色主题下几乎看不出。
    pub fn hover(&self, p: &Palette) -> Color {
        self.hover.unwrap_or(p.press_overlay())
    }
    /// 勾选标记色（强调色当前景用）。悬停项不换字色，只换底（[`MenuTheme::hover`]）。
    pub fn accent(&self, p: &Palette) -> Color {
        self.accent.unwrap_or(p.accent_text())
    }
}

/// 标签页覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TabTheme {
    pub accent: Option<Color>,
    pub inactive: Option<Color>,
    pub hover: Option<Color>,
    /// 标签条底部贯穿基线色。
    pub baseline: Option<Color>,
    /// 悬停标签的淡底色（含 alpha）。
    pub hover_bg: Option<Color>,
    /// 选中指示条高度（px）。
    pub indicator_h: Option<f32>,
    /// 标签条整体高度（逻辑 px）。
    pub height: Option<i32>,
    /// 选中标签的字重（measure 恒按此字重，避免选中态改变布局）。
    pub selected_weight: Option<u16>,
}

impl TabTheme {
    /// 胶囊式选中块的**填充**色。
    pub fn accent(&self, p: &Palette) -> Color {
        self.accent.unwrap_or(p.accent)
    }
    /// 下划线式选中项的文字与指示条色（强调色当前景用）。设了 `accent` 覆盖时两处同色。
    pub fn accent_text(&self, p: &Palette) -> Color {
        self.accent.unwrap_or(p.accent_text())
    }
    pub fn inactive(&self, p: &Palette) -> Color {
        self.inactive.unwrap_or(p.text_muted)
    }
    pub fn hover(&self, p: &Palette) -> Color {
        self.hover.unwrap_or(p.text)
    }
    pub fn baseline(&self, p: &Palette) -> Color {
        self.baseline.unwrap_or(p.divider)
    }
    /// 悬停淡底：默认取**主题文字色**的低 alpha（与 `Clickable` 叠层同范式，明暗主题自适应）。
    /// 刻意不取 accent——accent 已是选中态语义，hover 是临时态，用它会抢过选中项、
    /// 造成视觉层级倒置。
    pub fn hover_bg(&self, p: &Palette) -> Color {
        self.hover_bg.unwrap_or(p.hover_overlay())
    }
    pub fn indicator_h(&self) -> f32 {
        self.indicator_h.unwrap_or(2.0)
    }
    pub fn height(&self) -> i32 {
        self.height.unwrap_or(40)
    }
    pub fn selected_weight(&self) -> u16 {
        self.selected_weight.unwrap_or(600)
    }
}

/// 进度条覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProgressTheme {
    pub track: Option<Color>,
    pub fill: Option<Color>,
}

impl ProgressTheme {
    pub fn track(&self, p: &Palette) -> Color {
        self.track.unwrap_or(p.track)
    }
    pub fn fill(&self, p: &Palette) -> Color {
        self.fill.unwrap_or(p.accent)
    }
}

/// 分栏分隔条覆盖层（[`Element::split`](crate::ui::Element::split)）。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SplitTheme {
    /// 静态时居中那条 1px 细线。
    pub handle: Option<Color>,
    /// 悬停 / 拖动时整条分隔条的填充。
    pub handle_active: Option<Color>,
}

impl SplitTheme {
    pub fn handle(&self, p: &Palette) -> Color {
        self.handle.unwrap_or(p.divider)
    }
    pub fn handle_active(&self, p: &Palette) -> Color {
        self.handle_active.unwrap_or(p.accent_text())
    }
}

/// 数字步进覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StepperTheme {
    pub bg: Option<Color>,
    pub border: Option<Color>,
    /// ⚠ **当前不生效**。中部数值自 0.14 起由内嵌的 `TextInput` 绘制，正文色与
    /// `Element::text_input` 同源——走节点 `Style` 的 `fg` / `fg_role`
    /// （即 `.fg(..)` / `.fg_role(..)`），不读这里。`InputTheme::text` 同样如此。
    /// 保留字段是为了不破坏既有 TOML 主题文件的解析。
    pub text: Option<Color>,
    /// +/- 按钮区前景。
    pub button: Option<Color>,
    /// +/- 按钮悬停底色。
    pub button_hover: Option<Color>,
}

impl StepperTheme {
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.surface)
    }
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(p.text)
    }
    pub fn button(&self, p: &Palette) -> Color {
        self.button.unwrap_or(p.accent_text())
    }
    pub fn button_hover(&self, p: &Palette) -> Color {
        self.button_hover.unwrap_or(p.hover_overlay())
    }
}

/// 列表覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ListTheme {
    pub text: Option<Color>,
    pub selected_bg: Option<Color>,
    pub selected_text: Option<Color>,
    pub hover_bg: Option<Color>,
}

impl ListTheme {
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(p.text)
    }
    pub fn selected_bg(&self, p: &Palette) -> Color {
        self.selected_bg.unwrap_or(p.selected_soft())
    }
    pub fn selected_text(&self, p: &Palette) -> Color {
        self.selected_text.unwrap_or(p.accent_text())
    }
    pub fn hover_bg(&self, p: &Palette) -> Color {
        self.hover_bg.unwrap_or(p.hover_overlay())
    }
}

/// 链接覆盖层（链接色三态，回退到 accent 家族）。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LinkTheme {
    pub color: Option<Color>,
    pub hover: Option<Color>,
    pub pressed: Option<Color>,
}

impl LinkTheme {
    pub fn color(&self, p: &Palette) -> Color {
        self.color.unwrap_or(p.accent_text())
    }
    /// 悬停：亮色主题沿用按钮的 hover 蓝（更深）；暗色主题链接本就是调亮的那一档，
    /// 再取 accent_hover 反而变暗，故在 accent_text 上再提亮一点。
    pub fn hover(&self, p: &Palette) -> Color {
        self.hover.unwrap_or(if is_dark(p) {
            p.accent_text().lighten(0.2)
        } else {
            p.accent_hover
        })
    }
    pub fn pressed(&self, p: &Palette) -> Color {
        self.pressed.unwrap_or(if is_dark(p) {
            p.accent_text().darken(0.1)
        } else {
            p.accent_active
        })
    }
}

/// 富文本控件覆盖层（`Element::rich`）。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RichTheme {
    /// 折叠箭头色。
    pub chevron: Option<Color>,
    /// 分隔线色。
    pub divider: Option<Color>,
    /// 胶囊（chip）默认底色（span 未指定 bg 时）。
    pub chip_bg: Option<Color>,
    /// 胶囊默认文字色（span 未指定 fg 时）。
    pub chip_fg: Option<Color>,
    /// 划选选区底色（含 alpha；与输入框选区同默认）。
    pub selection: Option<Color>,
    /// 段前间距（逻辑 px）。
    pub para_spacing: Option<i32>,
    /// 折叠区子内容缩进（逻辑 px）。
    pub section_indent: Option<i32>,
}

/// WCAG 相对亮度（sRGB → 线性化加权）。
fn rel_luminance(c: Color) -> f32 {
    fn lin(u: u8) -> f32 {
        let s = u as f32 / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * lin(c.r) + 0.7152 * lin(c.g) + 0.0722 * lin(c.b)
}

/// WCAG 对比度（1..21）。
pub(crate) fn contrast_ratio(a: Color, b: Color) -> f32 {
    let (la, lb) = (rel_luminance(a), rel_luminance(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// 半透明前景 `over` 合成到不透明底色上（求淡底 chip 的实际视觉底色）。
fn composite_over(fg: Color, bg: Color) -> Color {
    let a = fg.a as f32 / 255.0;
    let ch = |f: u8, b: u8| (f as f32 * a + b as f32 * (1.0 - a)).round() as u8;
    Color::rgba(ch(fg.r, bg.r), ch(fg.g, bg.g), ch(fg.b, bg.b), 0xFF)
}

/// 通道线性插值（t∈0..=1，从 a 到 b）。
fn mix(a: Color, b: Color, t: f32) -> Color {
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::rgba(ch(a.r, b.r), ch(a.g, b.g), ch(a.b, b.b), 0xFF)
}

impl RichTheme {
    pub fn chevron(&self, p: &Palette) -> Color {
        self.chevron.unwrap_or(p.text_muted)
    }
    pub fn divider(&self, p: &Palette) -> Color {
        self.divider.unwrap_or(p.divider)
    }
    pub fn chip_bg(&self, p: &Palette) -> Color {
        // 与 badge 同族：强调色 15% 淡底。
        self.chip_bg.unwrap_or(p.accent.scale_alpha(0.15))
    }
    /// chip 默认前景：从 accent 向正文色插值，直到对**实际视觉底色**（淡底合成到
    /// surface 上）达到 WCAG AA（4.5:1）。「同色淡底 + 同色前景」的直觉搭配实测只有
    /// 约 3:1（淡底把背景亮度拉向中间，恰好吃掉前景的对比余量），必须派生修正。
    /// 只要主题满足「正文色对 surface 可读」这一基本前提，插值就必然收敛达标。
    pub fn chip_fg(&self, p: &Palette) -> Color {
        if let Some(c) = self.chip_fg {
            return c;
        }
        let bg = composite_over(self.chip_bg(p), p.surface);
        for i in 0..=8 {
            let c = mix(p.accent_text(), p.text, i as f32 / 8.0);
            if contrast_ratio(c, bg) >= 4.5 {
                return c;
            }
        }
        p.text
    }
    pub fn selection(&self, p: &Palette) -> Color {
        self.selection
            .unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x55))
    }
    pub fn para_spacing(&self) -> i32 {
        self.para_spacing.unwrap_or(6)
    }
    pub fn section_indent(&self) -> i32 {
        self.section_indent.unwrap_or(14)
    }
}

/// 分段控制器覆盖层（连体多段单选）。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SegmentTheme {
    /// 容器底色。
    pub bg: Option<Color>,
    pub border: Option<Color>,
    /// 聚焦时边框（回退 accent）。
    pub border_focus: Option<Color>,
    /// 选中段底色（含 alpha）。
    pub selected_bg: Option<Color>,
    /// 选中段文字色。
    pub selected_text: Option<Color>,
    /// 未选中段文字色。
    pub text: Option<Color>,
    /// 悬停段浅底（含 alpha）。
    pub hover_bg: Option<Color>,
    /// 段间分隔线。
    pub divider: Option<Color>,
    pub corner: Option<f32>,
    /// 选中胶囊 / 悬停浅底与外框之间的留白（px，默认 2）。
    ///
    /// 设为 0 则贴边：选中段铺满整段高度，只在贴着外框左右两端时带圆角（与外框圆角
    /// 吻合），中间段是直角——像一排相连的按钮被按下了一个，而不是框里浮着一颗胶囊。
    pub pill_inset: Option<f32>,
}

impl SegmentTheme {
    pub fn pill_inset(&self) -> f32 {
        self.pill_inset.unwrap_or(2.0).max(0.0)
    }
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.surface)
    }
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn border_focus(&self, p: &Palette) -> Color {
        self.border_focus.unwrap_or(p.accent_text())
    }
    pub fn selected_bg(&self, p: &Palette) -> Color {
        // 选中段为实心强调色（参考设计：高亮色 + 文字反色），无半透明渐变感。
        self.selected_bg.unwrap_or(p.accent)
    }
    pub fn selected_text(&self, p: &Palette) -> Color {
        // 选中段文字反色（强调色底上的对比前景）。
        self.selected_text.unwrap_or(p.on_accent)
    }
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(p.text_muted)
    }
    pub fn hover_bg(&self, p: &Palette) -> Color {
        self.hover_bg.unwrap_or(p.hover_overlay())
    }
    pub fn divider(&self, p: &Palette) -> Color {
        self.divider.unwrap_or(p.divider)
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_md)
    }
}

/// 表格覆盖层：排序指示器（箭头）的字形 / 字号 / 颜色 / 槽宽 / 间距 / 位置。
/// 每实例可用 `Element::sort_indicator(SortStyle)` 覆盖，未覆盖字段回退到本主题、再回退到内置默认。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TableTheme {
    /// 升序箭头字形（默认 ▲）。
    pub sort_asc: Option<String>,
    /// 降序箭头字形（默认 ▼）。
    pub sort_desc: Option<String>,
    /// 箭头字号 px（默认 10）。
    pub sort_size: Option<f32>,
    /// 箭头颜色（None 时用 `text_muted` 并随主题热切换；Some 为定死色）。
    pub sort_color: Option<Color>,
    /// 箭头槽宽度 px（默认 14；始终预留避免排序切换时标题宽度跳动）。
    pub sort_slot: Option<i32>,
    /// 标题与箭头间距 px（默认 2）。
    pub sort_gap: Option<i32>,
    /// 箭头置于标题左侧（默认 false = 右侧）。
    pub sort_leading: Option<bool>,
}

impl TableTheme {
    pub fn sort_asc(&self) -> &str {
        self.sort_asc.as_deref().unwrap_or("\u{25B2}")
    }
    pub fn sort_desc(&self) -> &str {
        self.sort_desc.as_deref().unwrap_or("\u{25BC}")
    }
    pub fn sort_size(&self) -> f32 {
        self.sort_size.unwrap_or(10.0)
    }
    pub fn sort_slot(&self) -> i32 {
        self.sort_slot.unwrap_or(14)
    }
    pub fn sort_gap(&self) -> i32 {
        self.sort_gap.unwrap_or(2)
    }
    pub fn sort_leading(&self) -> bool {
        self.sort_leading.unwrap_or(false)
    }
}

/// 导航覆盖层（NavRow 钻入行 + CollapsibleHeader 折叠头共用）。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NavTheme {
    pub text: Option<Color>,
    /// 悬停/按下底色。
    pub hover_bg: Option<Color>,
    /// 右侧箭头色。
    pub chevron: Option<Color>,
}

impl NavTheme {
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(p.text)
    }
    pub fn hover_bg(&self, p: &Palette) -> Color {
        self.hover_bg.unwrap_or(p.hover_overlay())
    }
    pub fn chevron(&self, p: &Palette) -> Color {
        self.chevron.unwrap_or(p.text_muted)
    }
}

/// 动效覆盖层：时长（毫秒，按语义档位取，回退 120/200/300ms）+ 标准曲线 + 标准弹簧。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimTheme {
    /// 快档（hover/press 等轻量反馈）。
    pub fast: Option<u32>,
    /// 常规档（开关滑块、展开等）。
    pub normal: Option<u32>,
    /// 慢档（大块过渡）。
    pub slow: Option<u32>,
    /// 标准缓动曲线（进场、展开、状态切换），回退 [`Easing::STANDARD`]。
    pub easing: Option<Easing>,
    /// 标准弹簧（高频打断的反馈：hover、开关、滑块），回退 [`Spring::SMOOTH`]。
    pub spring: Option<Spring>,
}

impl AnimTheme {
    pub fn fast(&self) -> u32 {
        self.fast.unwrap_or(120)
    }
    pub fn normal(&self) -> u32 {
        self.normal.unwrap_or(200)
    }
    pub fn slow(&self) -> u32 {
        self.slow.unwrap_or(300)
    }
    pub fn easing(&self) -> Easing {
        self.easing.unwrap_or(Easing::STANDARD)
    }
    pub fn spring(&self) -> Spring {
        self.spring.unwrap_or(Spring::SMOOTH)
    }
}

/// 手风琴覆盖层（Accordion 卡片外框 + 面板头背景）。chevron/text/hover 复用 [`NavTheme`]。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AccordionTheme {
    /// 卡片外框边框色。
    pub border: Option<Color>,
    /// 卡片圆角。
    pub corner: Option<f32>,
    /// 面板头背景（区别于内容区，营造卡片分层）。
    pub header_bg: Option<Color>,
    /// 面板间分隔线色。
    pub divider: Option<Color>,
}

impl AccordionTheme {
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_md)
    }
    pub fn header_bg(&self, p: &Palette) -> Color {
        self.header_bg.unwrap_or(p.surface_alt)
    }
    pub fn divider(&self, p: &Palette) -> Color {
        self.divider.unwrap_or(p.divider)
    }
}

/// 拖拽重排列表覆盖层（`Element::reorder_list`）。
///
/// 投影拆成 `shadow_color` + `shadow_blur` 两个标量而非直接放 `Shadow`——后者
/// 不实现 `Serialize`，拆开才能进 TOML。控件内部据此组装出 `Shadow`。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReorderTheme {
    /// 拖动手柄常态色。
    pub handle: Option<Color>,
    /// 拖动手柄悬停/拖动中色。
    pub handle_hover: Option<Color>,
    /// 被拖行浮起时的底色（须不透明，否则会透出下方让位中的行）。
    pub dragging_bg: Option<Color>,
    /// 被拖行浮起时的投影色（含 alpha）。
    pub shadow_color: Option<Color>,
    /// 被拖行浮起时的投影模糊半径（逻辑 px）。
    pub shadow_blur: Option<f32>,
    /// 插入指示线色。
    pub indicator: Option<Color>,
    /// 手柄槽宽（逻辑 px）。
    pub handle_w: Option<i32>,
    /// 被拖行浮起时的圆角。
    pub corner: Option<f32>,
}

impl ReorderTheme {
    pub fn handle(&self, p: &Palette) -> Color {
        self.handle.unwrap_or(p.text_muted)
    }
    pub fn handle_hover(&self, p: &Palette) -> Color {
        self.handle_hover.unwrap_or(p.text)
    }
    pub fn dragging_bg(&self, p: &Palette) -> Color {
        self.dragging_bg.unwrap_or(p.surface)
    }
    pub fn shadow_color(&self) -> Color {
        self.shadow_color.unwrap_or(Color::rgba(0, 0, 0, 56))
    }
    pub fn shadow_blur(&self) -> f32 {
        self.shadow_blur.unwrap_or(12.0)
    }
    pub fn indicator(&self, p: &Palette) -> Color {
        self.indicator.unwrap_or(p.accent_text())
    }
    pub fn handle_w(&self) -> i32 {
        self.handle_w.unwrap_or(20)
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_md)
    }
    /// 组装被拖行的浮起投影（略向下偏移，模拟光从上方来）。
    pub fn shadow(&self) -> Shadow {
        Shadow {
            dx: 0.0,
            dy: 2.0,
            blur: self.shadow_blur(),
            spread: 0.0,
            color: self.shadow_color(),
        }
    }
}

/// 表单行覆盖层（[`Element::field`] / [`Element::setting_row`] /
/// [`Element::setting_row_desc`]）。
///
/// 行高、标签列宽、间距这些**刻意不进构造器签名**：一个应用里的表单行必须整齐划一，
/// 而逐行传尺寸的 API 只会让每处调用各写一个近似值，最终对不齐。放进主题后
/// 「把所有表单行调紧一点」是改一处，不是改四十处。
///
/// [`Element::field`]: crate::ui::Element::field
/// [`Element::setting_row`]: crate::ui::Element::setting_row
/// [`Element::setting_row_desc`]: crate::ui::Element::setting_row_desc
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct FormTheme {
    /// `field` 的标签列宽（逻辑 px）。默认 110。
    pub label_width: Option<i32>,
    /// 单行表单行（`field` / `setting_row`）的行高（逻辑 px）。默认 40。
    ///
    /// 单行行**定高**而非按内容撑开：一列表单行必须等高才成其为"一列"，
    /// 按内容撑开会让放开关的行比放下拉的行矮一截，整列参差不齐。
    pub row_height: Option<i32>,
    /// 标签与控件之间的水平间距（回退 `metrics.space_md`，默认 12）。
    pub gap: Option<i32>,
    /// `setting_row_desc` 的上下内边距（回退 `metrics.space_md`，默认 12）。
    ///
    /// 带副标题的行是唯一**不定高**的一种——副标题让内容高度本就因行而异，
    /// 定高只会把它挤出去。故这一种用内边距把高度撑出来，与上面的 `row_height` 分工。
    pub row_pad_y: Option<i32>,
    /// 标签字号（回退 `metrics.font_md`）。
    pub label_size: Option<f32>,
    /// 标签字重。默认 400（库默认字重）；设置类界面常用 500 让标签压过控件文字。
    pub label_weight: Option<u16>,
    /// 副标题字号（回退 `metrics.font_xs`，即比正文小两档）。
    pub desc_size: Option<f32>,
    /// 标签最多显示几行（`None` = 不限，按内容换行）。
    ///
    /// 设了就同时启用**末尾省略**与**悬浮看全文**：截断意味着信息不完整，tooltip 是它
    /// 唯一的兜底，两者拆开只会让人漏配后一半。tooltip 由 `Tree::node_tooltip` 按
    /// `Label::text_truncated()` 自动门控——文字没被截断时不弹，故短标签不会因此多出
    /// 一个与可见文字一模一样的提示（该门控仅对 `Some(1)` 精确，多行取保守策略恒弹）。
    pub label_max_lines: Option<usize>,
    /// 副标题最多显示几行（`None` = 不限，按内容换行）。语义同
    /// [`label_max_lines`](Self::label_max_lines)。
    ///
    /// 设置页的说明文字长度由后端数据决定时尤其需要它：不限行数则长说明会把行撑成
    /// 三四行，同一列里的行高从此参差；而 `setting_row_desc` 返回的是拼好的容器，
    /// 调用方够不到内部那个 label，只能由主题这一侧给。
    pub desc_max_lines: Option<usize>,
}

impl FormTheme {
    pub fn label_width(&self) -> i32 {
        self.label_width.unwrap_or(110)
    }
    pub fn row_height(&self) -> i32 {
        self.row_height.unwrap_or(40)
    }
    pub fn gap(&self, m: &Metrics) -> i32 {
        self.gap.unwrap_or(m.space_md)
    }
    pub fn row_pad_y(&self, m: &Metrics) -> i32 {
        self.row_pad_y.unwrap_or(m.space_md)
    }
    pub fn label_size(&self, m: &Metrics) -> f32 {
        self.label_size.unwrap_or(m.font_md)
    }
    pub fn label_weight(&self) -> u16 {
        self.label_weight.unwrap_or(crate::text::WEIGHT_NORMAL)
    }
    pub fn desc_size(&self, m: &Metrics) -> f32 {
        self.desc_size.unwrap_or(m.font_xs)
    }
    pub fn label_max_lines(&self) -> Option<usize> {
        self.label_max_lines
    }
    pub fn desc_max_lines(&self) -> Option<usize> {
        self.desc_max_lines
    }
}

/// 卡片容器覆盖层（[`Element::card`]）。
///
/// 刻意**不含底色**：卡片底就是 `palette.surface`，走 [`Role::Surface`] 延迟解析
/// 才能在运行期换主题时跟着变。个别卡片要换底色，在返回的元素上链 `.bg_role(..)`
/// 即可——那是每实例的一次性微调，不该为它开一个全局主题槽。
///
/// [`Element::card`]: crate::ui::Element::card
/// [`Role::Surface`]: crate::style::Role::Surface
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CardTheme {
    /// 卡片圆角（回退 `metrics.corner_lg`）。
    pub corner: Option<f32>,
    /// 卡片内边距（回退 `metrics.space_lg`，默认 16）。
    pub pad: Option<i32>,
    /// 卡片内元素间距（标题/分隔线/内容之间，回退 `metrics.space_md`，默认 12）。
    pub gap: Option<i32>,
    /// 标题字号（回退 `metrics.font_lg`）。
    pub title_size: Option<f32>,
    /// 标题字重。默认 600（半粗）：标题与正文同字重时层级拉不开。
    pub title_weight: Option<u16>,
    /// 是否画 1px 描边（`palette.divider`）。默认 `true`：卡片底（白）与窗口底
    /// （slate-50）明度只差一点，没有描边时卡片边界几乎看不出来。
    ///
    /// 默认不加投影：投影按尺寸缓存整张模糊位图，每种卡片尺寸约多占 1MB，与本库的低内存
    /// 目标相悖。需要浮起感时对单张卡片 `.shadow(...)`。
    pub bordered: Option<bool>,
}

impl CardTheme {
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_lg)
    }
    pub fn pad(&self, m: &Metrics) -> i32 {
        self.pad.unwrap_or(m.space_lg)
    }
    pub fn gap(&self, m: &Metrics) -> i32 {
        self.gap.unwrap_or(m.space_md)
    }
    pub fn bordered(&self) -> bool {
        self.bordered.unwrap_or(true)
    }
    pub fn title_size(&self, m: &Metrics) -> f32 {
        self.title_size.unwrap_or(m.font_lg)
    }
    pub fn title_weight(&self) -> u16 {
        self.title_weight.unwrap_or(crate::text::WEIGHT_SEMIBOLD)
    }
}

/// 颜色选择器覆盖层。
///
/// 棋盘格两色是这里独有的槽：透明度要靠"底下透出什么"来表达，而调色板里没有一个
/// 现成的 token 是为"表示无内容"而设的——借 `surface`/`surface_alt` 会让棋盘格在
/// 深色主题里跟面板底融成一片，透明与不透明就看不出区别了。
///
/// 但也**不能钉死成一对浅灰**：深色面板（`surface` ≈ #1B2333）里铺一块纯白棋盘格，
/// 会成为整个面板最亮的东西，喧宾夺主。故默认值按 `surface` 的明暗分两套，
/// 亮度差保持一致（见 `checker_contrasts_with_the_panel_in_both_themes`）。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorPickerTheme {
    /// 触发器底色。
    pub bg: Option<Color>,
    /// 色块/条/面板的描边。
    pub border: Option<Color>,
    /// 拖拽手柄的亮环。
    pub handle: Option<Color>,
    /// 拖拽手柄的暗描边（浅色底上靠它显形）。
    pub handle_shade: Option<Color>,
    /// 棋盘格浅格。
    pub checker_light: Option<Color>,
    /// 棋盘格深格。
    pub checker_dark: Option<Color>,
    /// 色块与触发器的圆角。
    pub corner: Option<f32>,
}

/// tooltip / toast 这类"浮在内容上的深色小块"的 `(底, 字)`。
///
/// 两套主题下都是**深底浅字**：亮色主题取反色表面（slate-900），与页面拉开最大反差；
/// 暗色主题若也取反色就成了刺眼的白块，故改取比卡片表面再亮一档的深灰，靠明度差浮起来。
fn floating_inverse(p: &Palette) -> (Color, Color) {
    if is_dark(p) {
        (p.surface_alt.lighten(0.08), p.text)
    } else {
        (p.surface_inverse, p.on_surface_inverse)
    }
}

/// 主题整体是否为暗色：看窗口底色 `bg`。决定 hover 方向、浮层配色、强调前景提亮这些
/// "跟着主题明暗走"的取舍。不看 `surface`：深窗口配浅卡片这类局部反色的主题，整体仍是暗色。
fn is_dark(p: &Palette) -> bool {
    let s = p.bg;
    0.299 * s.r as f32 + 0.587 * s.g as f32 + 0.114 * s.b as f32 <= 153.0
}

/// 面板底色是否偏暗。阈值与 [`Color::pick_fg`] 同源（感知亮度 153），
/// 免得同一份主题在"该配什么前景"与"该配什么棋盘格"两处得出相反的判断。
fn surface_is_dark(p: &Palette) -> bool {
    let s = p.surface;
    0.299 * s.r as f32 + 0.587 * s.g as f32 + 0.114 * s.b as f32 <= 153.0
}

impl ColorPickerTheme {
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.surface)
    }
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn handle(&self, _p: &Palette) -> Color {
        self.handle.unwrap_or(Color::WHITE)
    }
    pub fn handle_shade(&self, _p: &Palette) -> Color {
        self.handle_shade.unwrap_or(Color::rgba(0, 0, 0, 0x66))
    }
    pub fn checker_light(&self, p: &Palette) -> Color {
        self.checker_light.unwrap_or(if surface_is_dark(p) {
            Color::hex(0x757C87)
        } else {
            Color::hex(0xFFFFFF)
        })
    }
    pub fn checker_dark(&self, p: &Palette) -> Color {
        self.checker_dark.unwrap_or(if surface_is_dark(p) {
            Color::hex(0x4A515B)
        } else {
            Color::hex(0xCBCED4)
        })
    }
    pub fn corner(&self, _p: &Palette) -> f32 {
        self.corner.unwrap_or_else(|| current().metrics.corner_sm)
    }
}

/// 悬停提示浮层覆盖层（深底浅字）。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TooltipTheme {
    pub bg: Option<Color>,
    pub text: Option<Color>,
    pub corner: Option<f32>,
    /// 单行超过此宽度（逻辑 px）自动换行；未设置时用库内默认值。
    pub max_width: Option<f32>,
    /// 描边色。`None`（默认）不描——浅色提示层浮在同样接近白的界面上时，靠它看出是个浮层。
    pub border: Option<Color>,
    /// 描边宽度，默认 `{ px = 1 }`：1 逻辑像素在 125% 下是 1.25 物理像素，会被抗锯齿摊成
    /// 两行灰边。
    pub border_width: Option<Len>,
    /// 指针静止多久（ms）后弹出，默认 500。
    pub delay_ms: Option<u64>,
}

impl TooltipTheme {
    /// 悬停到弹出的延时（ms），默认 500。
    pub fn delay_ms(&self) -> u64 {
        self.delay_ms.unwrap_or(500)
    }
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(floating_inverse(p).0)
    }
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(floating_inverse(p).1)
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_sm)
    }
    /// 换行宽度上限（逻辑 px）。默认 280，宿主可按窗口宽度/文案长度自行覆盖。
    pub fn max_width(&self) -> f32 {
        self.max_width.unwrap_or(280.0)
    }
    pub fn border_width(&self) -> Len {
        self.border_width.unwrap_or(Len::Physical { px: 1.0 })
    }
}

/// 滚动条覆盖层。`None` 回退到 palette：常态取 `border`、拖动态取 `text_muted`。
///
/// 单开一槽是因为那两位在宿主那边同时是全站边框色与弱化文字色——只想让滑块深一点，
/// 改 `palette.border` 会连带改掉输入框描边、`Role::Border`、窗口外框一串不相干的东西。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrollbarTheme {
    /// 滑块常态色。
    pub thumb: Option<Color>,
    /// 滑块拖动态色（加深一档，给出"抓住了"的反馈）。
    pub thumb_active: Option<Color>,
    /// 轨道底衬色。`None`（默认）不画轨道，只露滑块——理由见 `core::scrollbar::track`。
    pub track: Option<Color>,
    /// 滑块（与轨道）的视觉宽度，逻辑 px，默认 7。
    ///
    /// 只改视觉：命中区不小于 `core::scrollbar::HIT_W`（16），细滚动条照样好抓。
    pub width: Option<f32>,
    /// 滑块距容器右缘（以及多行框上下缘）的留白，逻辑 px，默认 3。
    pub margin: Option<f32>,
}

impl ScrollbarTheme {
    pub fn width(&self) -> f32 {
        self.width
            .unwrap_or(crate::core::scrollbar::TRACK_W)
            .max(1.0)
    }
    pub fn margin(&self) -> f32 {
        self.margin
            .unwrap_or(crate::core::scrollbar::MARGIN)
            .max(0.0)
    }
    pub fn thumb(&self, p: &Palette) -> Color {
        self.thumb.unwrap_or(p.border)
    }
    pub fn thumb_active(&self, p: &Palette) -> Color {
        self.thumb_active.unwrap_or(p.text_muted)
    }
}

/// 模态对话框覆盖层（[`Element::dialog`](crate::ui::Element::dialog) 一族）。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModalTheme {
    /// 对话框背后整窗遮罩的颜色（含 alpha）。默认 `rgba(0,0,0,120)`。
    ///
    /// 不从 palette 推导：遮罩要压暗的是**任意**内容，与界面底色无关；亮暗主题想用不同
    /// 浓度时各自在主题里写一份（暗色界面上同样的黑罩显得更重，通常要更淡）。
    pub scrim: Option<Color>,
}

impl ModalTheme {
    pub fn scrim(&self, _p: &Palette) -> Color {
        self.scrim.unwrap_or(Color::rgba(0, 0, 0, 120))
    }
}

/// 轻提示（Toast）浮层主题。始终为深色半透明面板（不随明暗主题翻转），
/// 与 tooltip 同源；`success`/`error` 为成功/失败图标色。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ToastTheme {
    pub bg: Option<Color>,
    pub text: Option<Color>,
    pub success: Option<Color>,
    pub error: Option<Color>,
    pub corner: Option<f32>,
    /// 浮层投影：向下偏移 / 模糊半径 / 颜色（含 alpha）。任一为 `None` 取内置默认。
    ///
    /// 默认值与 [`MenuTheme::shadow`] 一致——两者都是浮在内容之上的临时面板，
    /// 投影只负责把它们从背景里托起来，没有理由一个比另一个重。
    pub shadow_dy: Option<f32>,
    pub shadow_blur: Option<f32>,
    pub shadow_color: Option<Color>,
}

impl ToastTheme {
    /// 组装浮层投影（类型与理由同 [`MenuTheme::shadow`]）。默认值也与之相同。
    pub fn shadow(&self) -> Shadow {
        Shadow {
            dx: 0.0,
            dy: self.shadow_dy.unwrap_or(MENU_SHADOW_DY),
            blur: self.shadow_blur.unwrap_or(MENU_SHADOW_BLUR),
            spread: 0.0,
            color: self
                .shadow_color
                .unwrap_or(Color::rgba(0, 0, 0, MENU_SHADOW_ALPHA)),
        }
    }
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg
            .unwrap_or(floating_inverse(p).0.scale_alpha(240.0 / 255.0))
    }
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(floating_inverse(p).1)
    }
    /// 信息图标色（中性，跟随文字色）。
    pub fn info(&self, p: &Palette) -> Color {
        self.text(p)
    }
    /// 成功图标色。toast 底恒为深色（见 [`floating_inverse`]），亮色主题的
    /// `palette.success`（green-600）压在深底上发闷，故取 green-400；暗色主题的
    /// success 本就是亮的那一档，直接用。
    pub fn success(&self, p: &Palette) -> Color {
        self.success.unwrap_or(if is_dark(p) {
            p.success
        } else {
            Color::hex(0x4ADE80)
        })
    }
    pub fn error(&self, p: &Palette) -> Color {
        self.error.unwrap_or(p.danger)
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_md)
    }
}

/// 完整主题：base（palette/metrics）+ 各控件覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    /// 界面默认字族：所有**没显式设** `font_family` 的文字都用它，含框架自绘的菜单、
    /// tooltip、toast。`None`（默认）用平台默认：Windows「Microsoft YaHei UI」、
    /// macOS「PingFang SC」、Linux 交给 fontconfig 的 `sans-serif`。
    ///
    /// 是**兜底**而非覆盖：`Element::font_family` 设过的节点照旧用自己的。名字解析与
    /// `font_family` 相同（Windows 上带字重后缀的 GDI 名也认，见 `text::family_available`）。
    /// 经 `ThemeHandle::set` / `update` 运行期更换，下一帧全部重排重绘。
    ///
    /// 放在最前：TOML 里标量须写在各 `[表]` 之前。
    pub font_family: Option<String>,
    pub palette: Palette,
    pub metrics: Metrics,
    pub button: ButtonTheme,
    pub input: InputTheme,
    pub toggle: ToggleTheme,
    pub dropdown: DropdownTheme,
    pub menu: MenuTheme,
    pub tab: TabTheme,
    pub progress: ProgressTheme,
    pub split: SplitTheme,
    pub stepper: StepperTheme,
    pub list: ListTheme,
    pub link: LinkTheme,
    pub rich: RichTheme,
    pub segment: SegmentTheme,
    pub table: TableTheme,
    pub nav: NavTheme,
    pub form: FormTheme,
    pub card: CardTheme,
    pub accordion: AccordionTheme,
    pub reorder: ReorderTheme,
    pub anim: AnimTheme,
    pub tooltip: TooltipTheme,
    pub toast: ToastTheme,
    pub modal: ModalTheme,
    pub color_picker: ColorPickerTheme,
    pub scrollbar: ScrollbarTheme,
}

impl Theme {
    /// 暗色主题（暗色 palette + 默认 metrics/控件覆盖层）。
    pub fn dark() -> Self {
        Self {
            palette: Palette::dark(),
            ..Theme::default()
        }
    }
    /// 从 TOML 字符串解析（缺省字段回退到默认，支持部分覆盖）。
    pub fn from_toml(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(s)
    }
    /// 序列化为 TOML 字符串。
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }
}

thread_local! {
    static CURRENT: RefCell<Rc<Theme>> = RefCell::new(Rc::new(Theme::default()));
}

/// 当前线程的活动主题（未设置时为默认主题）。
pub fn current() -> Rc<Theme> {
    CURRENT.with(|c| c.borrow().clone())
}

/// 设置当前线程的活动主题（宿主在布局/绘制前调用）。
pub fn set_current(theme: Rc<Theme>) {
    CURRENT.with(|c| *c.borrow_mut() = theme);
}

#[cfg(test)]
mod tests {
    /// `font_family` 是 `Theme` 唯一的顶层标量：TOML 要求标量写在各 `[表]` 之前，字段若排在
    /// 表后面，`to_toml` 会直接报错。往返一次钉住两头。
    #[test]
    fn font_family_round_trips_through_toml() {
        let t = super::Theme {
            font_family: Some("霞鹜文楷".into()),
            ..Default::default()
        };
        let s = t.to_toml().expect("含 font_family 的主题应能序列化");
        let back = super::Theme::from_toml(&s).expect("应能读回");
        assert_eq!(back.font_family.as_deref(), Some("霞鹜文楷"));
        let partial = super::Theme::from_toml("font_family = \"思源宋体\"\n").unwrap();
        assert_eq!(partial.font_family.as_deref(), Some("思源宋体"));
        assert_eq!(super::Theme::default().font_family, None);
    }

    /// 棋盘格必须在**两套主题**下都：① 两格分得出来；② 与面板底分得开（否则透明
    /// 与不透明看着一样）；③ 不比面板底亮太多（深色面板里一块纯白会成为最扎眼的东西）。
    ///
    /// 用亮度差断言而非比对具体色值：换配色时该跟着变的是色值，不该变的是这三条关系。
    /// 此前 checker 钉死为纯白 + 浅灰，深色主题下第 ③ 条不成立，而截图跑的是浅色主题、
    /// 一路全绿——两套主题各核验一次的惯例正是为这种情形立的。
    #[test]
    fn checker_contrasts_with_the_panel_in_both_themes() {
        use super::{Palette, Theme};
        fn luma(c: crate::geometry::Color) -> f32 {
            0.299 * c.r as f32 + 0.587 * c.g as f32 + 0.114 * c.b as f32
        }
        for (name, pal) in [("浅色", Palette::default()), ("深色", Palette::dark())] {
            let cp = Theme::default().color_picker;
            let (l, d, surface) = (
                luma(cp.checker_light(&pal)),
                luma(cp.checker_dark(&pal)),
                luma(pal.surface),
            );
            assert!(
                (15.0..=90.0).contains(&(l - d)),
                "{name}：两格亮度差 {} 应落在 15..=90（太小看不出格子，太大成斑马纹）",
                l - d
            );
            // 「至少一格」而非「浅格」：浅色主题下浅格本就等于面板底（白底白格），
            // 棋盘格靠**深格**显形——这是 Photoshop/Figma 一路的标准做法。要求浅格
            // 也与底色分开，等于逼着浅色主题去用一对灰格，反而更脏。
            let apart = (l - surface).abs().max((d - surface).abs());
            assert!(
                apart >= 25.0,
                "{name}：两格与面板底的最大亮度差只有 {apart}，透明与不透明会看着一样"
            );
            assert!(
                l - surface <= 110.0,
                "{name}：浅格比面板底亮 {}，会成为面板里最扎眼的东西",
                l - surface
            );
        }
    }

    use super::*;

    #[test]
    fn chip_default_fg_meets_wcag_aa() {
        // 下游实测教训：`(色.15%淡底, 同色前景)` 只有约 3:1。锁住亮/暗两套默认
        // palette 的 chip 组合达到 WCAG AA（4.5:1），防止未来调色回归。
        for (name, th) in [("light", Theme::default()), ("dark", Theme::dark())] {
            let p = &th.palette;
            let bg = composite_over(th.rich.chip_bg(p), p.surface);
            let fg = th.rich.chip_fg(p);
            let ratio = contrast_ratio(fg, bg);
            assert!(ratio >= 4.5, "{name} 主题 chip 对比度 {ratio:.2} < 4.5");
        }
    }

    #[test]
    fn dark_theme_has_dark_bg_and_light_text() {
        let d = Theme::dark();
        let lum = |c: Color| c.r as u32 + c.g as u32 + c.b as u32;
        assert!(lum(d.palette.bg) < 300, "暗色背景亮度应低");
        assert!(lum(d.palette.text) > 500, "暗色文字应亮");
        // metrics 沿用默认。
        assert_eq!(d.metrics.corner_md, Metrics::default().corner_md);
    }

    /// 四档文字色必须**单调**地越来越弱（对背景的对比度递减）。这条不成立时
    /// `text_subtle` 就没有存在意义——它的全部价值就是在 muted 与 disabled 之间
    /// 插进一档，调色时手一滑把它调到 muted 之外，视觉层级会反过来。
    #[test]
    fn text_tiers_are_ordered_from_strong_to_weak() {
        for (name, th) in [("light", Theme::default()), ("dark", Theme::dark())] {
            let p = &th.palette;
            let c = |x: Color| contrast_ratio(x, p.bg);
            let tiers = [
                ("text", c(p.text)),
                ("text_muted", c(p.text_muted)),
                ("text_subtle", c(p.text_subtle)),
                ("text_disabled", c(p.text_disabled)),
            ];
            for w in tiers.windows(2) {
                assert!(
                    w[0].1 > w[1].1,
                    "{name} 主题 {} ({:.2}) 应比 {} ({:.2}) 对比更强",
                    w[0].0,
                    w[0].1,
                    w[1].0,
                    w[1].1
                );
            }
        }
    }

    /// 反色表面与它的前景是成对的，必须自身可读——这对色的唯一用途就是
    /// "深色标题栏配白字"，配错了整条标题栏都看不清。
    #[test]
    fn inverse_surface_pair_is_readable() {
        for (name, th) in [("light", Theme::default()), ("dark", Theme::dark())] {
            let p = &th.palette;
            let ratio = contrast_ratio(p.on_surface_inverse, p.surface_inverse);
            assert!(ratio >= 4.5, "{name} 主题反色对比度 {ratio:.2} < 4.5");
            // 反色表面还得真的与常规表面相反，否则它只是第三种灰。
            assert!(
                contrast_ratio(p.surface_inverse, p.surface) >= 4.5,
                "{name} 主题 surface_inverse 与 surface 不够对立"
            );
        }
    }

    /// 语义色经常直接当前景用（状态文字、标签边框），故须对表面达到
    /// WCAG 图形/大字阈值 3:1。饱和亮黄当 warning 是最常见的踩雷点。
    #[test]
    fn semantic_colors_are_usable_as_foreground() {
        for (name, th) in [("light", Theme::default()), ("dark", Theme::dark())] {
            let p = &th.palette;
            for (which, c) in [
                ("danger", p.danger),
                ("success", p.success),
                ("warning", p.warning),
            ] {
                let ratio = contrast_ratio(c, p.surface);
                assert!(ratio >= 3.0, "{name} 主题 {which} 对表面 {ratio:.2} < 3.0");
            }
        }
    }

    #[test]
    fn toml_roundtrip_preserves_palette() {
        let mut t = Theme::default();
        t.palette.accent = Color::hex(0xFF8800);
        let s = t.to_toml().expect("序列化");
        let back = Theme::from_toml(&s).expect("反序列化");
        assert_eq!(back.palette.accent, Color::hex(0xFF8800));
        assert_eq!(back.metrics.corner_md, t.metrics.corner_md);
    }

    /// 新增的语义色槽必须一并进 TOML：`#[serde(default)]` 让漏写字段静默回退默认，
    /// 于是"忘了加字段"和"用户没配这一项"长得一模一样，只能靠往返测试钉住。
    #[test]
    fn toml_roundtrip_covers_new_semantic_slots() {
        let mut t = Theme::default();
        t.palette.success = Color::hex(0x00FF01);
        t.palette.warning = Color::hex(0x00FF02);
        t.palette.text_subtle = Color::hex(0x00FF03);
        t.palette.surface_inverse = Color::hex(0x00FF04);
        t.palette.on_surface_inverse = Color::hex(0x00FF05);
        let back = Theme::from_toml(&t.to_toml().expect("序列化")).expect("反序列化");
        assert_eq!(back.palette.success, Color::hex(0x00FF01));
        assert_eq!(back.palette.warning, Color::hex(0x00FF02));
        assert_eq!(back.palette.text_subtle, Color::hex(0x00FF03));
        assert_eq!(back.palette.surface_inverse, Color::hex(0x00FF04));
        assert_eq!(back.palette.on_surface_inverse, Color::hex(0x00FF05));

        // 部分覆盖：只写 success，其余回退默认。
        let p = Theme::from_toml("[palette]\nsuccess = \"#123456\"\n")
            .expect("部分 TOML")
            .palette;
        assert_eq!(p.success, Color::hex(0x123456));
        assert_eq!(p.warning, Palette::default().warning);
        assert_eq!(p.text_subtle, Palette::default().text_subtle);
    }

    #[test]
    fn partial_toml_falls_back_to_defaults() {
        // 仅覆盖强调色，其余回退默认。
        let t = Theme::from_toml("[palette]\naccent = \"#112233\"\n").expect("部分 TOML");
        assert_eq!(t.palette.accent, Color::hex(0x112233));
        assert_eq!(
            t.palette.text,
            Palette::default().text,
            "未指定字段回退默认"
        );
        assert!(t.button.bg.is_none(), "控件覆盖默认 None");
    }

    /// 滚动条单开一槽：TOML 可配，未配的那一档回退 palette；滑块取色走主题槽而非 palette。
    #[test]
    fn scrollbar_slot_parses_and_drives_thumb_color() {
        let t = Theme::from_toml("[scrollbar]\nthumb = \"#ff0000\"\n").expect("应可解析");
        assert_eq!(t.scrollbar.thumb, Some(Color::rgb(255, 0, 0)));
        assert_eq!(t.scrollbar.thumb_active(&t.palette), t.palette.text_muted);
        let back = Theme::from_toml(&t.to_toml().unwrap()).unwrap();
        assert_eq!(
            back.scrollbar.thumb,
            Some(Color::rgb(255, 0, 0)),
            "回环不丢"
        );
        set_current(Rc::new(t.clone()));
        assert_eq!(crate::core::scrollbar::thumb(false), Color::rgb(255, 0, 0));
        assert_eq!(crate::core::scrollbar::thumb(true), t.palette.text_muted);
        assert_ne!(
            t.palette.border,
            Color::rgb(255, 0, 0),
            "前提：没碰 palette.border"
        );
        set_current(Rc::new(Theme::default()));
    }

    /// 动效 token 的 TOML 写法（API_GUIDE 里给的就是这两种）：命名曲线走 snake_case，
    /// 贝塞尔是四元数组，弹簧是两个字段的表。
    #[test]
    fn anim_tokens_toml_parse() {
        let t = Theme::from_toml(
            "[anim]
fast = 90
easing = { cubic_bezier = [0.4, 0.0, 0.2, 1.0] }
spring = { response_ms = 260.0, damping = 0.9 }
",
        )
        .expect("应可解析");
        assert_eq!(t.anim.fast(), 90);
        assert_eq!(t.anim.easing(), Easing::CubicBezier(0.4, 0.0, 0.2, 1.0));
        assert_eq!(
            t.anim.spring(),
            Spring {
                response_ms: 260.0,
                damping: 0.9
            }
        );
        let named = Theme::from_toml("[anim]\neasing = \"ease_out\"\n").expect("应可解析");
        assert_eq!(named.anim.easing(), Easing::EaseOut);
        let d = Theme::default();
        assert_eq!(
            (d.anim.easing(), d.anim.spring()),
            (Easing::STANDARD, Spring::SMOOTH)
        );
    }

    #[test]
    fn tooltip_border_toml_parses() {
        let t = Theme::from_toml("[tooltip]\nborder = \"#cccccc\"\nborder_width = { px = 2 }\n")
            .expect("应可解析");
        assert_eq!(t.tooltip.border, Some(Color::rgb(0xcc, 0xcc, 0xcc)));
        assert_eq!(t.tooltip.border_width(), Len::Physical { px: 2.0 });
        assert_eq!(Theme::default().tooltip.border, None, "默认不描");
        assert_eq!(
            Theme::default().tooltip.border_width(),
            Len::Physical { px: 1.0 }
        );
    }

    /// 文档（API_GUIDE「文本输入」）里给出的光标 TOML 必须真能解析：
    /// 风格名走 snake_case，改了枚举名而不改 `rename_all` 会让文档里的配置静默失效
    /// （`from_toml` 报错、或回退默认），使用方只会看到"配了没反应"。
    #[test]
    fn caret_toml_parses_and_roundtrips() {
        let t = Theme::from_toml(
            "[input]
caret_style = \"blink\"
caret_width = 3.0
             caret_rounded = false
caret_smooth_move = false
",
        )
        .expect("光标 TOML 应可解析");
        assert_eq!(t.input.caret_style(), CaretStyle::Blink);
        assert_eq!(t.input.caret_width(), Len::Dp(3.0));
        assert!(!t.input.caret_rounded());
        assert!(!t.input.caret_smooth_move());
        // 四个风格名逐个验，且回环后不变。
        for (name, want) in [
            ("smooth", CaretStyle::Smooth),
            ("blink", CaretStyle::Blink),
            ("phase", CaretStyle::Phase),
            ("solid", CaretStyle::Solid),
        ] {
            let t = Theme::from_toml(&format!(
                "[input]
caret_style = \"{name}\"
"
            ))
            .unwrap_or_else(|e| panic!("{name} 应可解析：{e}"));
            assert_eq!(t.input.caret_style(), want);
            let back = Theme::from_toml(&t.to_toml().expect("序列化")).expect("回环解析");
            assert_eq!(back.input.caret_style(), want, "{name} 回环后应不变");
        }
        // 未配置时回退默认：Blink、2px、圆角、平滑移动。
        let d = Theme::default();
        assert_eq!(d.input.caret_style(), CaretStyle::Blink);
        assert_eq!(d.input.caret_width(), Len::Dp(caret::DEFAULT_WIDTH));
        // 物理像素写法（非整数 DPI 下最清晰的选项）同样要能配。
        let px = Theme::from_toml(
            "[input]
caret_width = { px = 2 }
",
        )
        .expect("px 写法");
        assert_eq!(px.input.caret_width(), Len::Physical { px: 2.0 });
        assert!(d.input.caret_rounded() && d.input.caret_smooth_move());
    }

    #[test]
    fn override_layer_resolves_or_falls_back() {
        let p = Palette::default();
        let mut bt = ButtonTheme::default();
        assert_eq!(bt.bg(&p), p.accent, "无覆盖回退 palette.accent");
        bt.bg = Some(Color::hex(0x010203));
        assert_eq!(bt.bg(&p), Color::hex(0x010203), "有覆盖取覆盖值");
    }

    /// 两套默认调色板的对比度底线。断言关系而非色值：换配色时色值该变，这几条不该变。
    /// 阈值：正文级（主按钮白字、正文、次要文字、强调色当字）≥ 4.5（WCAG AA）；
    /// 三级弱化文字与语义色当字 ≥ 3（AA 大字 / 非文本 UI 的门槛）。
    #[test]
    fn default_palettes_meet_contrast_floors() {
        for (name, p) in [("亮色", Palette::default()), ("暗色", Palette::dark())] {
            let need = |what: &str, fg: Color, bg: Color, min: f32| {
                let r = contrast_ratio(fg, bg);
                assert!(r >= min, "{name}：{what} 对比度 {r:.2} < {min}");
            };
            need("主按钮白字", p.on_accent, p.accent, 4.5);
            for (iname, i) in [
                ("Primary", Intent::Primary),
                ("Danger", Intent::Danger),
                ("Success", Intent::Success),
                ("Warning", Intent::Warning),
            ] {
                let c = i.colors(&p);
                need(&format!("{iname} 实底文字"), c.fg, c.bg, 4.5);
                need(&format!("{iname} hover 文字"), c.fg, c.hover, 3.0);
            }
            for (bg_name, bg) in [("bg", p.bg), ("surface", p.surface)] {
                need(
                    &format!("accent_text 在 {bg_name}"),
                    p.accent_text(),
                    bg,
                    4.5,
                );
                need(&format!("text 在 {bg_name}"), p.text, bg, 4.5);
                need(&format!("text_muted 在 {bg_name}"), p.text_muted, bg, 4.5);
                need(&format!("text_subtle 在 {bg_name}"), p.text_subtle, bg, 3.0);
                need(&format!("danger 在 {bg_name}"), p.danger, bg, 3.0);
                need(&format!("success 在 {bg_name}"), p.success, bg, 3.0);
                need(&format!("warning 在 {bg_name}"), p.warning, bg, 3.0);
            }
        }
    }

    /// 只改 `accent` 的主题（文档示例、旧 TOML 都是这么写的），当前景用的那一档必须跟着变，
    /// 否则按钮换了色、链接 / 焦点环 / 选中文字还是默认蓝。暗色下推导值须提亮且够读。
    #[test]
    fn accent_text_follows_accent_unless_overridden() {
        let light = Palette {
            accent: Color::hex(0x2E9E5B),
            ..Palette::default()
        };
        assert_eq!(light.accent_text(), light.accent, "亮色主题推导值即 accent");
        let mut dark = Palette {
            accent: Color::hex(0x7C3AED),
            ..Palette::dark()
        };
        let at = dark.accent_text();
        assert_ne!(at, dark.accent, "暗色主题须提亮");
        assert!(contrast_ratio(at, dark.surface) > contrast_ratio(dark.accent, dark.surface));
        dark.accent_text = Some(Color::hex(0x123456));
        assert_eq!(dark.accent_text(), Color::hex(0x123456), "显式值优先");
        // 旧 TOML 不写 accent_text：缺省为 None，走推导，而不是回落到亮色主题的定值。
        let t = Theme::from_toml("[palette]\naccent = \"#7C3AED\"\n").unwrap();
        assert_eq!(t.palette.accent_text, None);
    }

    /// hover 的方向跟随主题明暗：亮色变深（变亮会把白字对比度拉低），暗色变亮。
    #[test]
    fn intent_hover_direction_follows_theme_brightness() {
        fn luma(c: Color) -> f32 {
            0.299 * c.r as f32 + 0.587 * c.g as f32 + 0.114 * c.b as f32
        }
        let light = Palette::default();
        let dark = Palette::dark();
        for i in [Intent::Primary, Intent::Danger, Intent::Success] {
            let c = i.colors(&light);
            assert!(luma(c.hover) < luma(c.bg), "亮色主题 hover 应变深");
            assert!(
                luma(c.active) < luma(c.hover),
                "亮色主题按下应比 hover 更深"
            );
        }
        for i in [Intent::Primary, Intent::Danger, Intent::Success] {
            let c = i.colors(&dark);
            assert!(luma(c.hover) > luma(c.bg), "暗色主题 hover 应变亮");
            assert!(luma(c.active) < luma(c.bg), "暗色主题按下应变深");
        }
    }

    #[test]
    fn intent_colors_maps_palette_and_derives_fg() {
        let p = Palette::default();
        assert_eq!(Intent::Primary.colors(&p).bg, p.accent);
        assert_eq!(Intent::Primary.colors(&p).fg, p.on_accent);
        assert_eq!(Intent::Neutral.colors(&p).bg, p.text.scale_alpha(0.08));
        assert_eq!(Intent::Danger.colors(&p).bg, p.danger);
        // Success/Warning 与 Danger 同源于 palette 语义色槽——不是另起一套并行体系。
        assert_eq!(Intent::Success.colors(&p).bg, p.success);
        assert_eq!(Intent::Warning.colors(&p).bg, p.warning);
        assert_eq!(Intent::Success.badge_colors(&p).1, p.success);
        assert_eq!(Intent::Warning.badge_colors(&p).1, p.warning);
        let custom = Color::hex(0x2E9E5B);
        assert_eq!(
            Intent::Custom(custom).colors(&p).bg,
            custom,
            "Custom 基色直用"
        );
        // 饱和的中间色（品牌绿）配白字，不因深字对比度数值更高就换成深字。
        assert_eq!(
            Intent::Custom(Color::hex(0x2E9E5B)).colors(&p).fg,
            p.on_accent,
            "中间色实底应是白字"
        );
        // Custom 浅基色 → fg 取深色(text) 保证对比。
        assert_eq!(Intent::Custom(Color::hex(0xFFF0A0)).colors(&p).fg, p.text);
    }

    /// `CustomRole` 与 `Custom` 的唯一差别是基色何时确定：前者在解析时从**当前活动主题**
    /// 取角色，故换主题后同一个 intent 解出不同的色；后者是定色，永远不动。
    #[test]
    fn custom_role_intent_follows_active_theme() {
        use crate::style::Role;
        let light = Theme::default();
        let dark = Theme::dark();
        // 用 `Role::Text` 作探针：它在两套主题下必然不同（深字 / 浅字），
        // 角色解析换没换主题从结果上一眼可辨。
        let fixed = Intent::Custom(light.palette.text);

        set_current(Rc::new(light.clone()));
        let p = &light.palette;
        assert_eq!(
            Intent::CustomRole(Role::Text).colors(p).bg,
            p.text,
            "亮色主题下解析出亮色主题的正文色"
        );
        // 派生规则与 Custom 共用：同一基色下两条路径必须给出同一组色。
        assert_eq!(
            Intent::CustomRole(Role::Text).colors(p).fg,
            fixed.colors(p).fg
        );
        assert_eq!(Intent::CustomRole(Role::Text).badge_colors(p).1, p.text);

        set_current(Rc::new(dark.clone()));
        let dp = &dark.palette;
        assert_ne!(dp.text, p.text, "前提：两套主题的正文色取值不同");
        assert_eq!(
            Intent::CustomRole(Role::Text).colors(dp).bg,
            dp.text,
            "换到暗色主题后同一个 intent 解出暗色主题的正文色"
        );
        assert_eq!(
            fixed.colors(dp).bg,
            p.text,
            "对照：Custom 是定色，换主题后纹丝不动"
        );

        // 角色可以落在 palette 之外的覆盖层上——这正是解析要整个 Theme 而非 Palette 的原因。
        assert_eq!(
            Intent::CustomRole(Role::InputBg).colors(dp).bg,
            dark.input.bg(dp)
        );

        set_current(Rc::new(Theme::default()));
    }
}
