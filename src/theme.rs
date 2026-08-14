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

use crate::geometry::Color;
use crate::style::Shadow;

/// 全局基础调色板。
#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Palette {
    pub accent: Color,
    pub accent_hover: Color,
    pub accent_active: Color,
    /// 强调色之上的前景（按钮文字等）。
    pub on_accent: Color,
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
    /// 而饱和的亮黄在浅色表面上只有 1.9:1，字一上去就糊。这里的取值对白底约 3.2:1，
    /// 与 `danger` 的 3.8:1 同量级，既保住琥珀色相又能当字用。
    pub warning: Color,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            accent: Color::hex(0x4C8BF5),
            accent_hover: Color::hex(0x6BA3FF),
            accent_active: Color::hex(0x3A6FD0),
            on_accent: Color::WHITE,
            bg: Color::hex(0xF3F3F3),
            surface: Color::WHITE,
            surface_alt: Color::hex(0xF6F8FA),
            surface_inverse: Color::hex(0x2D3436),
            on_surface_inverse: Color::hex(0xF3F3F3),
            text: Color::hex(0x2D3436),
            text_muted: Color::hex(0x636E72),
            text_subtle: Color::hex(0x8A9099),
            text_disabled: Color::hex(0xB0B6BD),
            border: Color::hex(0xCFD4DC),
            track: Color::hex(0xCFD4DC),
            placeholder: Color::hex(0xAAB0B8),
            divider: Color::hex(0xE2E6EA),
            danger: Color::hex(0xE5484D),
            success: Color::hex(0x2EA043),
            warning: Color::hex(0xD97706),
        }
    }
}

impl Palette {
    /// 暗色调色板预设（与亮色 Default 语义角色一一对应，仅取暗色值）。
    pub fn dark() -> Self {
        Self {
            accent: Color::hex(0x4C8BF5),
            accent_hover: Color::hex(0x6BA3FF),
            accent_active: Color::hex(0x3A6FD0),
            on_accent: Color::WHITE,
            bg: Color::hex(0x111827),
            surface: Color::hex(0x1B2333),
            surface_alt: Color::hex(0x232C3E),
            // 暗色下的"反色"是**浅色**条块，故与亮色主题互为镜像：底取 text、字取 bg。
            surface_inverse: Color::hex(0xE6E8EC),
            on_surface_inverse: Color::hex(0x111827),
            text: Color::hex(0xE6E8EC),
            text_muted: Color::hex(0x9AA3B2),
            text_subtle: Color::hex(0x76808F),
            text_disabled: Color::hex(0x5A6172),
            border: Color::hex(0x2C3650),
            track: Color::hex(0x2B3344),
            placeholder: Color::hex(0x6B7280),
            divider: Color::hex(0x242C3C),
            danger: Color::hex(0xE5484D),
            // 暗底上不必压暗，取更亮的一档以拉开与背景的对比。
            success: Color::hex(0x3FB950),
            warning: Color::hex(0xD29922),
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
        const L: f32 = 0.10; // hover 变亮系数
        const D: f32 = 0.10; // active 变暗系数
        let derive = |c: Color| IntentColors {
            bg: c,
            hover: c.lighten(L),
            active: c.darken(D),
            fg: c.pick_fg(p.text, p.on_accent),
        };
        match self {
            Intent::Primary => IntentColors {
                bg: p.accent,
                hover: p.accent_hover,
                active: p.accent_active,
                fg: p.on_accent,
            },
            Intent::Neutral => IntentColors {
                bg: p.border,
                hover: p.border.darken(D),
                active: p.border.darken(D * 2.0),
                fg: p.text,
            },
            Intent::Danger => IntentColors {
                bg: p.danger,
                hover: p.danger.lighten(L),
                active: p.danger.darken(D),
                fg: p.danger.pick_fg(p.text, p.on_accent),
            },
            Intent::Success => IntentColors {
                bg: p.success,
                hover: p.success.lighten(L),
                active: p.success.darken(D),
                fg: p.success.pick_fg(p.text, p.on_accent),
            },
            Intent::Warning => IntentColors {
                bg: p.warning,
                hover: p.warning.lighten(L),
                active: p.warning.darken(D),
                fg: p.warning.pick_fg(p.text, p.on_accent),
            },
            Intent::Custom(c) => derive(c),
            Intent::CustomRole(r) => derive(r.resolve(&current())),
        }
    }

    /// 徽章胶囊配色（收起态下拉当前项 / 展开态菜单项的尾随徽章通用）：返回 `(填充色, 文字色)`。
    /// 淡色底 + **可读**的同色系前景。Neutral 用 `text_muted`（够深可读），不再用浅灰 `border`
    /// 当字色——否则灰字灰底几乎看不清。
    pub fn badge_colors(self, p: &Palette) -> (Color, Color) {
        let fg = match self {
            Intent::Primary => p.accent,
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

/// 全局基础度量（间距 / 圆角 / 字号）。
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
    /// 基础间距单位。
    pub spacing: i32,
    /// 文本控件内边距。
    pub text_pad: i32,
    pub font_sm: f32,
    pub font_md: f32,
    pub font_lg: f32,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            corner_sm: 4.0,
            corner_md: 6.0,
            corner_lg: 10.0,
            border_width: Len::Dp(1.0),
            border_width_focus: Len::Dp(1.8),
            spacing: 8,
            text_pad: 10,
            font_sm: 13.0,
            font_md: 14.0,
            font_lg: 16.0,
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
    pub border: Option<Color>,
    pub border_focus: Option<Color>,
    pub text: Option<Color>,
    pub placeholder: Option<Color>,
    /// 选区高亮（含 alpha）。
    pub selection: Option<Color>,
    pub cursor: Option<Color>,
    pub corner: Option<f32>,
}

impl InputTheme {
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.surface)
    }
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn border_focus(&self, p: &Palette) -> Color {
        self.border_focus.unwrap_or(p.accent)
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
    pub fn cursor(&self, p: &Palette) -> Color {
        self.cursor.unwrap_or(p.text)
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
    pub text: Option<Color>,
    pub chevron: Option<Color>,
    pub corner: Option<f32>,
}

impl DropdownTheme {
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.surface)
    }
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn border_focus(&self, p: &Palette) -> Color {
        self.border_focus.unwrap_or(p.accent)
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
    pub fn hover(&self, p: &Palette) -> Color {
        self.hover
            .unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x22))
    }
    pub fn accent(&self, p: &Palette) -> Color {
        self.accent.unwrap_or(p.accent)
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
    pub fn accent(&self, p: &Palette) -> Color {
        self.accent.unwrap_or(p.accent)
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
        self.hover_bg.unwrap_or(p.text.scale_alpha(0.08))
    }
    pub fn indicator_h(&self) -> f32 {
        self.indicator_h.unwrap_or(2.0)
    }
    pub fn height(&self) -> i32 {
        self.height.unwrap_or(44)
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

/// 数字步进覆盖层。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct StepperTheme {
    pub bg: Option<Color>,
    pub border: Option<Color>,
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
        self.button.unwrap_or(p.accent)
    }
    pub fn button_hover(&self, p: &Palette) -> Color {
        self.button_hover
            .unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x18))
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
        self.selected_bg
            .unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x22))
    }
    pub fn selected_text(&self, p: &Palette) -> Color {
        self.selected_text.unwrap_or(p.accent)
    }
    pub fn hover_bg(&self, p: &Palette) -> Color {
        self.hover_bg.unwrap_or(p.surface_alt)
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
        self.color.unwrap_or(p.accent)
    }
    pub fn hover(&self, p: &Palette) -> Color {
        self.hover.unwrap_or(p.accent_hover)
    }
    pub fn pressed(&self, p: &Palette) -> Color {
        self.pressed.unwrap_or(p.accent_active)
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
            let c = mix(p.accent, p.text, i as f32 / 8.0);
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
}

impl SegmentTheme {
    pub fn bg(&self, p: &Palette) -> Color {
        self.bg.unwrap_or(p.surface)
    }
    pub fn border(&self, p: &Palette) -> Color {
        self.border.unwrap_or(p.border)
    }
    pub fn border_focus(&self, p: &Palette) -> Color {
        self.border_focus.unwrap_or(p.accent)
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
        self.hover_bg
            .unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x12))
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
        self.hover_bg.unwrap_or(p.surface_alt)
    }
    pub fn chevron(&self, p: &Palette) -> Color {
        self.chevron.unwrap_or(p.text_muted)
    }
}

/// 动画时长覆盖层（毫秒）。控件按语义档位取时长，回退 120/200/300ms。
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AnimTheme {
    /// 快档（hover/press 等轻量反馈）。
    pub fast: Option<u32>,
    /// 常规档（开关滑块、展开等）。
    pub normal: Option<u32>,
    /// 慢档（大块过渡）。
    pub slow: Option<u32>,
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
        self.indicator.unwrap_or(p.accent)
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
    /// 标签与控件之间的水平间距。默认 12。
    pub gap: Option<i32>,
    /// `setting_row_desc` 的上下内边距。默认 12。
    ///
    /// 带副标题的行是唯一**不定高**的一种——副标题让内容高度本就因行而异，
    /// 定高只会把它挤出去。故这一种用内边距把高度撑出来，与上面的 `row_height` 分工。
    pub row_pad_y: Option<i32>,
    /// 标签字号（回退 `metrics.font_md`）。
    pub label_size: Option<f32>,
    /// 标签字重。默认 400（库默认字重）；设置类界面常用 500 让标签压过控件文字。
    pub label_weight: Option<u16>,
    /// 副标题字号（回退 `metrics.font_sm - 1`，即比正文小两档）。
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
    pub fn gap(&self) -> i32 {
        self.gap.unwrap_or(12)
    }
    pub fn row_pad_y(&self) -> i32 {
        self.row_pad_y.unwrap_or(12)
    }
    pub fn label_size(&self, m: &Metrics) -> f32 {
        self.label_size.unwrap_or(m.font_md)
    }
    pub fn label_weight(&self) -> u16 {
        self.label_weight.unwrap_or(crate::text::WEIGHT_NORMAL)
    }
    pub fn desc_size(&self, m: &Metrics) -> f32 {
        self.desc_size.unwrap_or(m.font_sm - 1.0)
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
    /// 卡片内边距。默认 16。
    pub pad: Option<i32>,
    /// 卡片内元素间距（标题/分隔线/内容之间）。默认 10。
    pub gap: Option<i32>,
    /// 标题字号（回退 `metrics.font_lg`）。
    pub title_size: Option<f32>,
    /// 标题字重。默认 400。
    pub title_weight: Option<u16>,
}

impl CardTheme {
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_lg)
    }
    pub fn pad(&self) -> i32 {
        self.pad.unwrap_or(16)
    }
    pub fn gap(&self) -> i32 {
        self.gap.unwrap_or(10)
    }
    pub fn title_size(&self, m: &Metrics) -> f32 {
        self.title_size.unwrap_or(m.font_lg)
    }
    pub fn title_weight(&self) -> u16 {
        self.title_weight.unwrap_or(crate::text::WEIGHT_NORMAL)
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
}

impl TooltipTheme {
    pub fn bg(&self, _p: &Palette) -> Color {
        self.bg.unwrap_or(Color::hex(0x303033))
    }
    pub fn text(&self, _p: &Palette) -> Color {
        self.text.unwrap_or(Color::WHITE)
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_sm)
    }
    /// 换行宽度上限（逻辑 px）。默认 280，宿主可按窗口宽度/文案长度自行覆盖。
    pub fn max_width(&self) -> f32 {
        self.max_width.unwrap_or(280.0)
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
    pub fn bg(&self, _p: &Palette) -> Color {
        self.bg.unwrap_or(Color::rgba(0x32, 0x32, 0x35, 235))
    }
    pub fn text(&self, _p: &Palette) -> Color {
        self.text.unwrap_or(Color::WHITE)
    }
    /// 信息图标色（中性，跟随文字白）。
    pub fn info(&self, _p: &Palette) -> Color {
        self.text.unwrap_or(Color::WHITE)
    }
    pub fn success(&self, _p: &Palette) -> Color {
        self.success.unwrap_or(Color::hex(0x4ADE80))
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
    pub palette: Palette,
    pub metrics: Metrics,
    pub button: ButtonTheme,
    pub input: InputTheme,
    pub toggle: ToggleTheme,
    pub dropdown: DropdownTheme,
    pub menu: MenuTheme,
    pub tab: TabTheme,
    pub progress: ProgressTheme,
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

    #[test]
    fn override_layer_resolves_or_falls_back() {
        let p = Palette::default();
        let mut bt = ButtonTheme::default();
        assert_eq!(bt.bg(&p), p.accent, "无覆盖回退 palette.accent");
        bt.bg = Some(Color::hex(0x010203));
        assert_eq!(bt.bg(&p), Color::hex(0x010203), "有覆盖取覆盖值");
    }

    #[test]
    fn intent_colors_maps_palette_and_derives_fg() {
        let p = Palette::default();
        assert_eq!(Intent::Primary.colors(&p).bg, p.accent);
        assert_eq!(Intent::Primary.colors(&p).fg, p.on_accent);
        assert_eq!(Intent::Neutral.colors(&p).bg, p.border);
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
        // 用 `Role::Text` 而不是 `Role::Danger` 作探针：两套主题的 danger 刻意取同值
        // （红就是红），角色解析换没换主题从结果上看不出来。
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
