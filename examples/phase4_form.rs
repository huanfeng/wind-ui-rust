//! Phase 4 验证：基础输入控件综合表单。
//!
//! 交互窗口：cargo run --example phase4_form
//! 截屏：    cargo run --example phase4_form -- --screenshot artifacts/phase4.png

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windui::prelude::*;

fn field(label: &str, control: Element) -> Element {
    Element::row()
        .width_match()
        .height(36)
        .cross(Align::Center)
        .spacing(12)
        .child(
            Element::label(label)
                .font_size(14.0)
                .fg(Color::hex(0x2D3436))
                .width(96),
        )
        .child(control)
}

fn main() {
    let name = Rc::new(RefCell::new(String::from("windui")));
    let enabled = Rc::new(Cell::new(true));
    let dark = Rc::new(Cell::new(false));
    let mode = Rc::new(Cell::new(1usize));
    let volume = Rc::new(Cell::new(0.65f32));

    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(14)
        .bg(Color::hex(0xF5F6FA))
        .child(
            Element::label("设置面板")
                .font_size(22.0)
                .fg(Color::hex(0x1A1A2E))
                .width_match()
                .height(32),
        )
        .child(field(
            "名称",
            Element::text_input(name.clone(), "请输入名称").width(220),
        ))
        .child(field(
            "启用功能",
            Element::checkbox("开启高级模式", enabled.clone()),
        ))
        .child(field("深色主题", Element::switch(dark.clone())))
        .child(field(
            "渲染模式",
            Element::row()
                .spacing(16)
                .child(Element::radio("快速", mode.clone(), 0))
                .child(Element::radio("均衡", mode.clone(), 1))
                .child(Element::radio("高质量", mode.clone(), 2)),
        ))
        .child(field("音量", Element::slider(volume.clone()).width(220)))
        .child(
            Element::row()
                .width_match()
                .height(44)
                .cross(Align::Center)
                .spacing(12)
                .child(Element::button("保存").on_click(|_| println!("saved")))
                .child(Element::button("重置").on_click(|_| println!("reset"))),
        );

    App::new("Phase 4 — 输入控件", 480, 420)
        .bg(Color::hex(0xF5F6FA))
        .screenshot_from_args()
        .content(ui)
        .run();
}
