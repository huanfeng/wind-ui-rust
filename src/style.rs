//! 节点视觉样式。

use crate::geometry::Color;
use crate::render::{Gradient, Paint};
use crate::spec::Align;
use crate::theme::Theme;

/// 主题角色：背景/边框/文字延迟解析到当前主题的对应颜色。
/// 用 Role 而非写死颜色的节点，在运行期换主题时会自动跟随刷新（paint 期解析）。
///
/// `#[non_exhaustive]`：语义色是会持续补齐的一组，本版就加了五个（`SurfaceInverse` /
/// `OnSurfaceInverse` / `TextSubtle` / `Success` / `Warning`）。没有这个标注的话，
/// 每补一个角色都是下游的破坏性变更；标上之后下游的 `match` 必须留 `_` 兜底分支，
/// 新角色便只是新增。本 crate 内部的 `match` 不受影响，仍须穷尽——
/// 忘记给新角色接上 `resolve` 会当场编译失败，正是想要的。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Role {
    Bg,
    Surface,
    SurfaceAlt,
    /// 与当前主题明暗相反的实底条块（深色标题栏、浅底上的深色横幅）。
    SurfaceInverse,
    /// [`Role::SurfaceInverse`] 之上的前景，二者成对使用。
    OnSurfaceInverse,
    Border,
    Divider,
    Track,
    Text,
    TextMuted,
    /// 三级弱化文字（版权行、脚注），比 `TextMuted` 更淡但仍是可读正文——
    /// 既非禁用（`TextDisabled`）也非待填写（`Placeholder`）。
    TextSubtle,
    TextDisabled,
    Placeholder,
    Accent,
    AccentHover,
    AccentActive,
    OnAccent,
    Danger,
    /// 成功 / 已完成语义色。与 [`crate::theme::Intent::Success`] 同源于 `palette.success`。
    Success,
    /// 警告 / 需注意语义色。与 [`crate::theme::Intent::Warning`] 同源于 `palette.warning`。
    Warning,
    /// 手风琴卡片边框（含控件覆盖层回退）。
    AccordionBorder,
    /// 手风琴面板头背景。
    AccordionHeaderBg,
    /// 输入框类底色（含 InputTheme 覆盖层回退；tag_field 等仿输入框容器用）。
    InputBg,
    /// 输入框类边框（含 InputTheme 覆盖层回退）。
    InputBorder,
}

impl Role {
    /// 解析为当前主题下的具体颜色。
    pub fn resolve(self, t: &Theme) -> Color {
        let p = &t.palette;
        match self {
            Role::Bg => p.bg,
            Role::Surface => p.surface,
            Role::SurfaceAlt => p.surface_alt,
            Role::SurfaceInverse => p.surface_inverse,
            Role::OnSurfaceInverse => p.on_surface_inverse,
            Role::Border => p.border,
            Role::Divider => p.divider,
            Role::Track => p.track,
            Role::Text => p.text,
            Role::TextMuted => p.text_muted,
            Role::TextSubtle => p.text_subtle,
            Role::TextDisabled => p.text_disabled,
            Role::Placeholder => p.placeholder,
            Role::Accent => p.accent,
            Role::AccentHover => p.accent_hover,
            Role::AccentActive => p.accent_active,
            Role::OnAccent => p.on_accent,
            Role::Danger => p.danger,
            Role::Success => p.success,
            Role::Warning => p.warning,
            Role::AccordionBorder => t.accordion.border(p),
            Role::AccordionHeaderBg => t.accordion.header_bg(p),
            Role::InputBg => t.input.bg(p),
            Role::InputBorder => t.input.border(p),
        }
    }
}

/// 背景/边框画刷：纯色、渐变，或延迟解析的主题角色。
#[derive(Debug, Clone)]
pub enum Brush {
    Solid(Color),
    Gradient(Gradient),
    Role(Role),
    /// 角色色 × 透明度调制（badge/chip 的"意图色 15% 淡底"模式）：
    /// paint 期解析，运行期换主题自动跟随——比为每个角色加 XxxSoft 变体正交。
    RoleAlpha(Role, f32),
}

impl Brush {
    /// 解析为 render 层 `Paint`（Role 经 theme 取色 → 纯色 fill；Gradient → 渐变）。
    pub fn resolve_paint(&self, t: &Theme) -> Paint {
        match self {
            Brush::Solid(c) => Paint::fill(*c),
            Brush::Gradient(g) => Paint::gradient(g.clone()),
            Brush::Role(r) => Paint::fill(r.resolve(t)),
            Brush::RoleAlpha(r, a) => Paint::fill(r.resolve(t).scale_alpha(*a)),
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

/// 边框作用于哪几条边。
///
/// 存在的理由是设计里大量使用**单边**边框——页签的下划线、分区的底线、侧栏的
/// 右边线。此前只能用「1px 高的色块」拼出来：那既要多一个节点，又会占据布局位置，
/// 把它当分隔元素而非装饰，容器一改间距就跟着错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edges {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
}

impl Edges {
    pub const NONE: Edges = Edges {
        top: false,
        right: false,
        bottom: false,
        left: false,
    };
    pub const ALL: Edges = Edges {
        top: true,
        right: true,
        bottom: true,
        left: true,
    };
    pub const TOP: Edges = Edges {
        top: true,
        ..Edges::NONE
    };
    pub const RIGHT: Edges = Edges {
        right: true,
        ..Edges::NONE
    };
    pub const BOTTOM: Edges = Edges {
        bottom: true,
        ..Edges::NONE
    };
    pub const LEFT: Edges = Edges {
        left: true,
        ..Edges::NONE
    };

    /// 是否四边齐全。齐全时走圆角描边路径，缺边时逐边画直线段。
    pub fn is_all(self) -> bool {
        self == Edges::ALL
    }
}

impl std::ops::BitOr for Edges {
    type Output = Edges;
    /// 合并两组边，用于 `Edges::TOP | Edges::BOTTOM` 这类写法。
    fn bitor(self, o: Edges) -> Edges {
        Edges {
            top: self.top || o.top,
            right: self.right || o.right,
            bottom: self.bottom || o.bottom,
            left: self.left || o.left,
        }
    }
}

impl Default for Edges {
    fn default() -> Self {
        Edges::ALL
    }
}

/// 背景/边框/文字等视觉属性。核心层统一绘制投影、背景与边框，widget 绘制内容。
#[derive(Debug, Clone)]
pub struct Style {
    /// 背景画刷（None = 透明）。
    pub bg: Option<Brush>,
    /// 边框（画刷, 线宽 px）。
    pub border: Option<(Brush, i32)>,
    /// 边框作用于哪几条边。默认四边。仅在 `border` 有值时有意义。
    pub border_edges: Edges,
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
    /// 行高倍数（相对字号）。`None` 用字体自带行距。
    ///
    /// 影响**多行文字的行间距**，单行文字只影响其占位高度。取倍数而非绝对像素，
    /// 使行距随字号与 DPI 一同缩放。中文正文通常 1.6–1.7，西文 1.4–1.5。
    pub line_height: Option<f32>,
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
            border_edges: Edges::ALL,
            corner_radius: 0.0,
            fg: Color::hex(0x1A1A1A),
            fg_role: Some(Role::Text),
            font_size: 14.0,
            font_weight: crate::text::WEIGHT_NORMAL,
            font_family: None,
            line_height: None,
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

    /// 补齐的语义角色必须真的接到 palette 上：漏接一条 `resolve` 分支不会报错，
    /// 只会让该角色悄悄取到别的颜色。亮暗两套主题各验一遍。
    #[test]
    fn new_semantic_roles_resolve_in_both_themes() {
        for t in [Theme::default(), Theme::dark()] {
            let p = &t.palette;
            assert_eq!(Role::Success.resolve(&t), p.success);
            assert_eq!(Role::Warning.resolve(&t), p.warning);
            assert_eq!(Role::TextSubtle.resolve(&t), p.text_subtle);
            assert_eq!(Role::SurfaceInverse.resolve(&t), p.surface_inverse);
            assert_eq!(Role::OnSurfaceInverse.resolve(&t), p.on_surface_inverse);
        }
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
