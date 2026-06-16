//! Phase 5 验证：ScrollView + Tabs + Divider + Dialog。
//!
//! 主界面截屏：cargo run --example phase5_containers -- --screenshot artifacts/phase5.png
//! 对话框截屏：cargo run --example phase5_containers -- --dialog --screenshot artifacts/phase5_dialog.png

use std::cell::Cell;
use std::rc::Rc;

use windui::prelude::*;

fn list_item(i: usize) -> Element {
    Element::row()
        .width_match()
        .height(40)
        .cross(Align::Center)
        .padding_xy(12, 0)
        .background(if i.is_multiple_of(2) { Color::WHITE } else { Color::hex(0xF7F9FB) })
        .child(
            Element::label(format!("列表项 #{i:02} — 可滚动内容"))
                .font_size(15.0)
                .fg(Color::hex(0x2D3436))
                .weight(1.0),
        )
        .child(Element::label("›").font_size(18.0).fg(Color::hex(0xAAB0B8)))
}

fn main() {
    let dialog_open = std::env::args().any(|a| a == "--dialog");
    let show = Rc::new(Cell::new(dialog_open));
    let tab = Rc::new(Cell::new(0usize));

    // 列表页：滚动容器装入 20 个条目
    let mut list = Element::scroll().fill().background(Color::WHITE).corner(8.0);
    for i in 0..20 {
        list = list.child(list_item(i));
    }
    let page_list = Element::col().fill().child(list);

    let show2 = show.clone();
    let page_about = Element::col()
        .fill()
        .padding(16)
        .spacing(12)
        .child(Element::label("windui").font_size(24.0).fg(Color::hex(0x1A1A2E)).height(32).width_match())
        .child(Element::label("轻量 Windows GUI：Win32 + tiny-skia + DirectWrite").font_size(14.0).fg(Color::hex(0x636E72)).height(22).width_match())
        .child(Element::divider())
        .child(Element::button("打开对话框").on_click(move |_| show2.set(true)));

    let tabs = Element::tabs(
        tab.clone(),
        vec![
            ("列表", page_list),
            ("关于", page_about),
        ],
    );

    let show3 = show.clone();
    let dialog = Element::dialog(
        show.clone(),
        Element::col()
            .width(300)
            .background(Color::WHITE)
            .corner(12.0)
            .padding(20)
            .spacing(14)
            .child(Element::label("提示").font_size(20.0).fg(Color::hex(0x1A1A2E)).height(28).width_match())
            .child(Element::label("这是一个模态对话框，遮罩会吞掉下层点击。").font_size(14.0).fg(Color::hex(0x636E72)).height(40).width_match())
            .child(
                Element::row()
                    .width_match()
                    .height(40)
                    .child(Element::button("关闭").on_click(move |_| show3.set(false))),
            ),
    );

    // 根：主界面 + 叠加对话框
    let ui = Element::stack()
        .fill()
        .background(Color::hex(0xEFF1F4))
        .child(Element::col().fill().padding(16).child(tabs))
        .child(dialog);

    App::new("Phase 5 — 容器与导航", 480, 440)
        .background(Color::hex(0xEFF1F4))
        .screenshot_from_args()
        .content(ui)
        .run();
}
