//! 节点视觉样式。

use crate::geometry::Color;
use crate::render::{Gradient, Paint};
use crate::spec::Align;
use crate::theme::Theme;

/// 主题角色：背景/边框/文字延迟解析到当前主题的对应颜色。
/// 用 Role 而非写死颜色的节点，在运行期换主题时会自动跟随刷新（paint 期解析）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Bg,
    Surface,
    SurfaceAlt,
    Border,
    Divider,
    Track,
    Text,
    TextMuted,
    TextDisabled,
    Placeholder,
    Accent,
    AccentHover,
    AccentActive,
    OnAccent,
    Danger,
    /// 手风琴卡片边框（含控件覆盖层回退）。
    AccordionBorder,
    /// 手风琴面板头背景。
    AccordionHeaderBg,
}

impl Role {
    /// 解析为当前主题下的具体颜色。
    pub fn resolve(self, t: &Theme) -> Color {
        let p = &t.palette;
        match self {
            Role::Bg => p.bg,
            Role::Surface => p.surface,
            Role::SurfaceAlt => p.surface_alt,
            Role::Border => p.border,
            Role::Divider => p.divider,
            Role::Track => p.track,
            Role::Text => p.text,
            Role::TextMuted => p.text_muted,
            Role::TextDisabled => p.text_disabled,
            Role::Placeholder => p.placeholder,
            Role::Accent => p.accent,
            Role::AccentHover => p.accent_hover,
            Role::AccentActive => p.accent_active,
            Role::OnAccent => p.on_accent,
            Role::Danger => p.danger,
            Role::AccordionBorder => t.accordion.border(p),
            Role::AccordionHeaderBg => t.accordion.header_bg(p),
        }
    }
}

/// 背景/边框画刷：纯色、渐变，或延迟解析的主题角色。
#[derive(Debug, Clone)]
pub enum Brush {
    Solid(Color),
    Gradient(Gradient),
    Role(Role),
}

impl Brush {
    /// 解析为 render 层 `Paint`（Role 经 theme 取色 → 纯色 fill；Gradient → 渐变）。
    pub fn resolve_paint(&self, t: &Theme) -> Paint {
        match self {
            Brush::Solid(c) => Paint::fill(*c),
            Brush::Gradient(g) => Paint::gradient(g.clone()),
            Brush::Role(r) => Paint::fill(r.resolve(t)),
        }
    }
    /// 解析出的纯色用色（Gradient 取首个 stop，用于边框 stroke）。
    pub fn solid_color(&self, t: &Theme) -> Color {
        self.resolve_paint(t).color
    }
}

/// 浮层投影（drop shadow）。`blur` 为模糊半径（逻辑 px），`spread` 为正向外扩、
/// 负向内收的外扩量；`color` 含 alpha。绘制在背景之前、节点矩形之下。
#[derive(Debug, Clone, Copy)]
pub struct Shadow {
    pub dx: f32,
    pub dy: f32,
    pub blur: f32,
    pub spread: f32,
    pub color: Color,
}

impl Shadow {
    /// 常用投影：偏移 + 模糊 + 颜色（spread=0）。
    pub fn new(dx: f32, dy: f32, blur: f32, color: Color) -> Self {
        Self {
            dx,
            dy,
            blur,
            spread: 0.0,
            color,
        }
    }
}

/// 背景/边框/文字等视觉属性。核心层统一绘制投影、背景与边框，widget 绘制内容。
#[derive(Debug, Clone)]
pub struct Style {
    /// 背景画刷（None = 透明）。
    pub bg: Option<Brush>,
    /// 边框（画刷, 线宽 px）。
    pub border: Option<(Brush, i32)>,
    /// 圆角半径 px。
    pub corner_radius: f32,
    /// 前景/文字色（当 `fg_role` 为 None 时生效）。
    pub fg: Color,
    /// 前景主题角色（Some 时优先于 `fg`，运行期换主题跟随）。
    pub fg_role: Option<Role>,
    /// 字号 px。
    pub font_size: f32,
    /// 字重（DirectWrite 数值：400=Normal、500=Medium、600=SemiBold、700=Bold）。
    pub font_weight: u16,
    /// 字体族（None = 系统默认）。
    pub font_family: Option<String>,
    /// 文字水平对齐。
    pub text_align: Align,
    /// 浮层投影（None = 无）。
    pub shadow: Option<Shadow>,
    /// 子树整体不透明度（1.0 = 不透明；<1 时核心层入离屏层合成）。
    pub opacity: f32,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            bg: None,
            border: None,
            corner_radius: 0.0,
            fg: Color::hex(0x1A1A1A),
            fg_role: None,
            font_size: 14.0,
            font_weight: crate::text::WEIGHT_NORMAL,
            font_family: None,
            text_align: Align::Start,
            shadow: None,
            opacity: 1.0,
        }
    }
}

impl Style {
    /// 解析最终文字色：有 `fg_role` 时按主题解析，否则用 `fg`。
    pub fn resolved_fg(&self, t: &Theme) -> Color {
        match self.fg_role {
            Some(r) => r.resolve(t),
            None => self.fg,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Color;
    use crate::theme::Theme;

    #[test]
    fn role_resolves_against_theme_palette() {
        let light = Theme::default();
        assert_eq!(Role::Divider.resolve(&light), light.palette.divider);
        assert_eq!(Role::Accent.resolve(&light), light.palette.accent);
        assert_eq!(Role::Text.resolve(&light), light.palette.text);
    }

    #[test]
    fn brush_solid_resolves_to_fill_paint() {
        let t = Theme::default();
        let p = Brush::Solid(Color::hex(0x123456)).resolve_paint(&t);
        assert_eq!(p.color, Color::hex(0x123456));
        assert!(p.gradient.is_none());
    }

    #[test]
    fn brush_role_tracks_theme() {
        let t = Theme::default();
        let p = Brush::Role(Role::Surface).resolve_paint(&t);
        assert_eq!(p.color, t.palette.surface);
    }

    #[test]
    fn resolved_fg_prefers_role() {
        let t = Theme::default();
        let s = Style {
            fg: Color::hex(0x000000),
            fg_role: Some(Role::TextMuted),
            ..Style::default()
        };
        assert_eq!(s.resolved_fg(&t), t.palette.text_muted);
        let s2 = Style {
            fg: Color::hex(0x010203),
            fg_role: None,
            ..Style::default()
        };
        assert_eq!(s2.resolved_fg(&t), Color::hex(0x010203));
    }
}
