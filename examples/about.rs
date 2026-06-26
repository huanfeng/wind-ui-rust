//! 「关于」界面：综合验证 Toast / Badge / 描边按钮 / 可点击卡片四项新能力，
//! 复刻输入法关于页设计。
//!
//! 交互窗口：cargo run --example about
//! 截屏：    cargo run --example about -- --screenshot artifacts/about.png
//!          点击卡片截屏：... --click 300 300（落在「官方网站」卡片上弹 Toast）

use windui::prelude::*;

/// 彩色圆角图标方块 + 居中白色字形（替代真实 SVG 图标，验证布局）。
fn icon_box(bg: Color, glyph: &str, size: i32) -> Element {
    Element::stack()
        .size(size, size)
        .corner(12.0)
        .bg(bg)
        .child(
            Element::label(glyph)
                .font_size((size as f32) * 0.42)
                .fg(Color::WHITE)
                .align(Align::Center),
        )
}

/// 可点击卡片：彩色图标 + 标题 + 描述。点击弹出 Toast。
fn card(icon_bg: Color, glyph: &str, title: &str, desc: &str) -> Element {
    Element::row()
        .clickable()
        .on_click(|ctx| ctx.toast_ok("已添加到剪贴板"))
        .width_match()
        .cross(Align::Center)
        .spacing(14)
        .padding(16)
        .corner(12.0)
        .bg(Color::WHITE)
        .border(Color::hex(0xE6E8EB), 1)
        .child(icon_box(icon_bg, glyph, 40))
        .child(
            Element::col()
                .weight(1.0)
                .spacing(3)
                .child(
                    Element::label(title)
                        .font_size(15.0)
                        .font_weight(600)
                        .fg(Color::hex(0x1F2328))
                        .width_match()
                        .height(20),
                )
                .child(
                    Element::label(desc)
                        .font_size(12.5)
                        .fg(Color::hex(0x8A9099))
                        .width_match()
                        .height(18),
                ),
        )
}

fn main() {
    // 顶部：大图标 + 标题 + 徽章/检查更新 + 副标题。
    let header = Element::row()
        .width_match()
        .spacing(20)
        .child(icon_box(Color::hex(0x4C8BF5), "风", 96))
        .child(
            Element::col()
                .weight(1.0)
                .spacing(10)
                .child(
                    Element::label("清风输入法")
                        .font_size(26.0)
                        .font_weight(700)
                        .fg(Color::hex(0x1F2328))
                        .width_match()
                        .height(34),
                )
                .child(
                    Element::row()
                        .spacing(10)
                        .cross(Align::Center)
                        .child(Element::badge("v0.0.0-alpha"))
                        .child(Element::button("检查更新").small().outline()),
                )
                .child(
                    Element::label("轻量、快速、可定制的开源中文输入法")
                        .font_size(13.5)
                        .fg(Color::hex(0x8A9099))
                        .width_match()
                        .height(20),
                ),
        );

    // 2 列卡片网格。
    let grid = Element::col()
        .width_match()
        .spacing(14)
        .child(card(
            Color::hex(0x4C8BF5),
            "网",
            "官方网站",
            "文档、下载与最新动态",
        ))
        .child(
            Element::row()
                .width_match()
                .spacing(14)
                .cross(Align::Stretch)
                .child(
                    Element::stack().weight(1.0).child(card(
                        Color::hex(0x24292F),
                        "G",
                        "GitHub",
                        "源码与文档",
                    )),
                )
                .child(
                    Element::stack().weight(1.0).child(card(
                        Color::hex(0xF5A623),
                        "!",
                        "报告 Bug",
                        "报告 Bug 或建议",
                    )),
                ),
        )
        .child(
            Element::row()
                .width_match()
                .spacing(14)
                .cross(Align::Stretch)
                .child(
                    Element::stack().weight(1.0).child(card(
                        Color::hex(0x2EA043),
                        "↓",
                        "版本发布",
                        "更新日志",
                    )),
                )
                .child(
                    Element::stack().weight(1.0).child(card(
                        Color::hex(0x12B7F5),
                        "Q",
                        "QQ 交流群",
                        "1085293418",
                    )),
                ),
        );

    let panel = Element::col()
        .width_match()
        .spacing(22)
        .padding(28)
        .corner(16.0)
        .bg(Color::WHITE)
        .border(Color::hex(0xEAECEF), 1)
        .child(header)
        .child(grid)
        .child(
            Element::label("© 2026 WindInput Contributors · MIT License")
                .font_size(12.5)
                .fg(Color::hex(0xAEB4BC))
                .width_match()
                .height(18)
                .text_align(Align::Center),
        );

    let ui = Element::col()
        .fill()
        .padding(16)
        .bg(Color::hex(0xF0F2F4))
        .child(panel);

    App::new("关于 — 清风输入法", 620, 560)
        .bg(Color::hex(0xF0F2F4))
        .screenshot_from_args()
        .content(ui)
        .run();
}
