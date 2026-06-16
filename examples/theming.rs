//! 主题系统示例：用 TOML 部分覆盖默认主题（换强调色/表面/背景），其余回退默认。
//!
//! 运行：cargo run --release --example theming
//! 截屏：cargo run --example theming -- --screenshot artifacts/theming.png

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windui::prelude::*;

/// 仅覆盖部分 token，其余字段由 serde default 回退到内置默认。
const THEME_TOML: &str = r##"
[palette]
accent = "#7C5CFC"
accent_hover = "#9277FF"
accent_active = "#5E3FD6"
bg = "#F4F2FB"
surface = "#FFFFFF"
text = "#241B45"

[metrics]
corner_md = 10.0
"##;

fn row(label: &str, control: Element) -> Element {
    Element::row()
        .width_match()
        .height(40)
        .cross(Align::Center)
        .spacing(12)
        .child(Element::label(label).font_size(14.0).fg(Color::hex(0x241B45)).width(96))
        .child(control)
}

fn main() {
    let theme = Theme::from_toml(THEME_TOML).expect("解析主题 TOML");

    let name = Rc::new(RefCell::new(String::from("紫色主题")));
    let on = Rc::new(Cell::new(true));
    let check = Rc::new(Cell::new(true));
    let vol = Rc::new(Cell::new(0.6f32));
    let mode = Rc::new(Cell::new(1usize));

    let card = Element::col()
        .width_match()
        .background(Color::hex(0xFFFFFF))
        .corner(12.0)
        .padding(16)
        .spacing(8)
        .child(row("名称", Element::text_input(name, "输入").width_match()))
        .child(row("开关", Element::switch(on)))
        .child(row("复选", Element::checkbox("启用功能", check)))
        .child(row("音量", Element::slider(vol).width_match()))
        .child(row("模式", Element::dropdown(vec!["A", "B", "C"], mode).width(160)))
        .child(
            Element::row()
                .width_match()
                .spacing(10)
                .child(Element::button("主操作"))
                .child(Element::button("次操作")),
        );

    let ui = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .child(Element::label("自定义主题（TOML 覆盖）").font_size(22.0).fg(Color::hex(0x241B45)).height(30).width_match())
        .child(card);

    App::new("windui — 主题", 380, 360)
        .theme(theme)
        .screenshot_from_args()
        .content(ui)
        .run();
}
