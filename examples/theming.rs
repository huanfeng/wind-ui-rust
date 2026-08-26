//! 主题与换肤：TOML 部分覆盖 + `Role` 角色着色 + 运行期整树热切换。
//!
//! 运行：cargo run --release --example theming
//! 截屏：cargo run --example theming -- --screenshot artifacts/theming.png
//!
//! 顶部四枚按钮切换主题：亮色 / 暗色是内置的，海洋 / 日落由 TOML **部分覆盖**得来
//! （只写要改的键，其余从默认调色板继承）。切换走 `ThemeHandle::set`，整棵控件树
//! 就地换色、不重建；右侧色卡与左侧控件都没有一行"我是什么颜色"的代码——
//! 它们用的是 `bg_role(Role::Accent)` 这类**角色**，具体色值由当前主题解析。
//!
//! 反过来说：写死的 `Color::hex(...)` 不跟随换肤。示例里除了 TOML 字符串本身，
//! 一处硬编码颜色都没有，正是为了让"切一下全变"这件事可信。

use windui::prelude::*;

#[path = "common/mod.rs"]
mod common;
use common::{page_title, section_title, Shell};

/// 海洋：青蓝色调，冷色低饱和。只覆盖 8 个键，其余（text_disabled / divider /
/// placeholder …）继承默认——这正是"部分覆盖"的意思。
const THEME_OCEAN: &str = r##"
[palette]
accent       = "#0E9AA7"
accent_hover = "#17B4C2"
bg           = "#EAF4F5"
surface      = "#FFFFFF"
surface_alt  = "#DCEDEF"
text         = "#123A40"
text_muted   = "#4A7178"
border       = "#B6D6DA"

[metrics]
corner_md = 10.0
"##;

/// 日落：暖橙色调。同样只覆盖必要的键。
const THEME_SUNSET: &str = r##"
[palette]
accent       = "#E4572E"
accent_hover = "#F4703F"
bg           = "#FBF1EA"
surface      = "#FFFFFF"
surface_alt  = "#F6E4D8"
text         = "#3A2418"
text_muted   = "#7A5745"
border       = "#E4C9B4"

[metrics]
corner_md = 4.0
"##;

/// 主题切换按钮：选中态实心、未选中态描边，两棵子树用 `visible_when` 互斥显示。
///
/// 不做成 `segmented` 是因为它只绑 `Signal<usize>`、不给变更回调，而换肤要在
/// 点击当刻调 `ThemeHandle::set`。两态叠放而非改样式，也顺带保证切换时按钮
/// 宽度不跳（实心与描边的内padding 一致）。
fn theme_chip(
    name: &'static str,
    i: usize,
    sel: Signal<usize>,
    th: ThemeHandle,
    t: Theme,
) -> Element {
    let (th_on, th_off) = (th.clone(), th);
    let (t_on, t_off) = (t.clone(), t);
    Element::stack()
        .child(
            Element::button(name)
                .small()
                .on_click(move |_| {
                    sel.set(i);
                    th_on.set(t_on.clone());
                })
                .visible_when(move || sel.get() == i),
        )
        .child(
            Element::button(name)
                .small()
                .outline()
                .neutral()
                .on_click(move |_| {
                    sel.set(i);
                    th_off.set(t_off.clone());
                })
                .visible_when(move || sel.get() != i),
        )
}

/// 一格色卡：角色色块 + 角色名。色块用 `bg_role`，故换肤时自动跟着变——
/// 这一格本身就是"角色着色"的演示对象。
fn swatch(role: Role, name: &'static str) -> Element {
    Element::col()
        .weight(1.0)
        .spacing(6)
        .child(
            Element::leaf()
                .width_match()
                .height(38)
                .corner(8.0)
                .bg_role(role)
                .border_role(Role::Border, 1),
        )
        .child(
            Element::label(name)
                .font_size(11.0)
                .fg_role(Role::TextMuted)
                .width_match(),
        )
}

/// 一行色卡（每行三格，末行不足时补空位保持等宽）。
fn swatch_row(items: [(Role, &'static str); 3]) -> Element {
    let mut row = Element::row().width_match().spacing(10);
    for (role, name) in items {
        row = row.child(swatch(role, name));
    }
    row
}

/// 左列的表单预览行：定宽标签 + 控件。
fn field_row(label: &'static str, ctrl: Element) -> Element {
    Element::row()
        .width_match()
        .height(36)
        .cross(Align::Center)
        .spacing(12)
        .child(
            Element::label(label)
                .font_size(13.0)
                .fg_role(Role::TextMuted)
                .width(52),
        )
        .child(ctrl)
}

fn main() {
    let theme_light = Theme::default();
    let theme_dark = Theme::dark();
    let theme_ocean = Theme::from_toml(THEME_OCEAN).expect("海洋主题解析失败");
    let theme_sunset = Theme::from_toml(THEME_SUNSET).expect("日落主题解析失败");

    let name = signal(String::from("windui"));
    let on = signal(true);
    let check = signal(true);
    let vol = signal(0.62f32);
    let mode = signal(1usize);
    let sel = signal(0usize);
    let prog = signal(0.66f32);

    let mut app = App::new("windui — 主题与换肤", 880, 700).icon(brand_icon());
    let th = app.theme_handle();

    // ── 顶部：标题 + 四枚主题按钮 ──
    let picker = Element::row()
        .cross(Align::Center)
        .spacing(8)
        .child(theme_chip("亮色", 0, sel, th.clone(), theme_light.clone()))
        .child(theme_chip("暗色", 1, sel, th.clone(), theme_dark))
        .child(theme_chip("海洋", 2, sel, th.clone(), theme_ocean))
        .child(theme_chip("日落", 3, sel, th.clone(), theme_sunset));

    let header = Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(16)
        .child(page_title("主题与换肤", "TOML 部分覆盖 · Role 角色着色 · 运行期热切换").weight(1.0))
        .child(picker);

    // ── 左列：控件在当前主题下的样子 ──
    let controls = Element::col()
        .width_match()
        .bg_role(Role::Surface)
        .corner(12.0)
        .border_role(Role::Border, 1)
        .padding(18)
        .spacing(10)
        .child(section_title("控件预览"))
        .child(field_row(
            "文本框",
            Element::text_input(name, "点击聚焦…").width_match(),
        ))
        .child(field_row(
            "下拉",
            Element::dropdown(vec!["选项 A", "选项 B", "选项 C"], mode).width_match(),
        ))
        .child(field_row("开关", Element::switch(on)))
        .child(field_row("复选", Element::checkbox("启用功能", check)))
        .child(field_row("滑块", Element::slider(vol).width_match()))
        .child(
            Element::row()
                .width_match()
                .spacing(8)
                .child(Element::button("主操作"))
                .child(Element::button("次操作").outline().neutral())
                .child(Element::button("删除").outline().danger()),
        );

    // ── 左列下：语义色（Intent）不随色相走，永远表达"这件事的性质" ──
    let intents = Element::col()
        .width_match()
        .bg_role(Role::Surface)
        .corner(12.0)
        .border_role(Role::Border, 1)
        .padding(18)
        .spacing(12)
        .child(section_title("语义色"))
        .child(
            Element::row()
                .width_match()
                .spacing(8)
                .cross(Align::Center)
                .child(Element::badge_intent("成功", Intent::Success))
                .child(Element::badge_intent("警告", Intent::Warning))
                .child(Element::badge_intent("危险", Intent::Danger))
                .child(Element::badge("中性"))
                .child(Element::leaf().weight(1.0)),
        )
        .child(Element::progress(prog).width_match());

    let left = Element::col()
        .weight(1.0)
        .height_match()
        .spacing(14)
        .child(controls)
        .child(intents)
        .child(Element::leaf().weight(1.0));

    // ── 右列：调色板色卡 + TOML 源码 ──
    let palette = Element::col()
        .width_match()
        .bg_role(Role::Surface)
        .corner(12.0)
        .border_role(Role::Border, 1)
        .padding(18)
        .spacing(12)
        .child(section_title("当前调色板"))
        .child(swatch_row([
            (Role::Accent, "Accent"),
            (Role::Bg, "Bg"),
            (Role::Surface, "Surface"),
        ]))
        .child(swatch_row([
            (Role::SurfaceAlt, "SurfaceAlt"),
            (Role::Border, "Border"),
            (Role::Text, "Text"),
        ]))
        .child(swatch_row([
            (Role::Success, "Success"),
            (Role::Warning, "Warning"),
            (Role::Danger, "Danger"),
        ]));

    let toml_card = Element::col()
        .width_match()
        .weight(1.0)
        .bg_role(Role::Surface)
        .corner(12.0)
        .border_role(Role::Border, 1)
        .padding(18)
        .spacing(10)
        .child(section_title("「海洋」的全部改动"))
        .child(
            Element::label(THEME_OCEAN.trim())
                .font_size(11.5)
                .line_height(1.55)
                .fg_role(Role::TextMuted)
                .width_match()
                .weight(1.0),
        );

    let right = Element::col()
        .width(348)
        .height_match()
        .spacing(14)
        .child(palette)
        .child(toml_card);

    let body = Element::col()
        .fill()
        .padding(20)
        .spacing(16)
        .child(header)
        .child(
            Element::row()
                .fill()
                .weight(1.0)
                .cross(Align::Stretch)
                .spacing(14)
                .child(left)
                .child(right),
        );

    app.screenshot_from_args()
        .frameless()
        .theme(theme_light)
        .content(Shell::new("主题").wrap(body))
        .run();
}
