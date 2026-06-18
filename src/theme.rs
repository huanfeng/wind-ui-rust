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
    pub text: Color,
    pub text_muted: Color,
    pub text_disabled: Color,
    pub border: Color,
    /// 关闭态轨道（开关 / 滑块）。
    pub track: Color,
    pub placeholder: Color,
    pub divider: Color,
    pub danger: Color,
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
            text: Color::hex(0x2D3436),
            text_muted: Color::hex(0x636E72),
            text_disabled: Color::hex(0xB0B6BD),
            border: Color::hex(0xCFD4DC),
            track: Color::hex(0xCFD4DC),
            placeholder: Color::hex(0xAAB0B8),
            divider: Color::hex(0xE2E6EA),
            danger: Color::hex(0xE5484D),
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
    pub border_width: f32,
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
            border_width: 1.5,
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
        self.selection.unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x55))
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
}

impl MenuTheme {
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
        self.hover.unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x22))
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
        self.button_hover.unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x18))
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
        self.selected_bg.unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x22))
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
        self.selected_bg.unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x22))
    }
    pub fn selected_text(&self, p: &Palette) -> Color {
        self.selected_text.unwrap_or(p.accent)
    }
    pub fn text(&self, p: &Palette) -> Color {
        self.text.unwrap_or(p.text_muted)
    }
    pub fn hover_bg(&self, p: &Palette) -> Color {
        self.hover_bg.unwrap_or(Color::rgba(p.accent.r, p.accent.g, p.accent.b, 0x12))
    }
    pub fn divider(&self, p: &Palette) -> Color {
        self.divider.unwrap_or(p.divider)
    }
    pub fn corner(&self, m: &Metrics) -> f32 {
        self.corner.unwrap_or(m.corner_md)
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
    pub segment: SegmentTheme,
    pub nav: NavTheme,
}

impl Theme {
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
    fn toml_roundtrip_preserves_palette() {
        let mut t = Theme::default();
        t.palette.accent = Color::hex(0xFF8800);
        let s = t.to_toml().expect("序列化");
        let back = Theme::from_toml(&s).expect("反序列化");
        assert_eq!(back.palette.accent, Color::hex(0xFF8800));
        assert_eq!(back.metrics.corner_md, t.metrics.corner_md);
    }

    #[test]
    fn partial_toml_falls_back_to_defaults() {
        // 仅覆盖强调色，其余回退默认。
        let t = Theme::from_toml("[palette]\naccent = \"#112233\"\n").expect("部分 TOML");
        assert_eq!(t.palette.accent, Color::hex(0x112233));
        assert_eq!(t.palette.text, Palette::default().text, "未指定字段回退默认");
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
}
