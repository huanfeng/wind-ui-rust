//! Phase 5 验证：ScrollView + Tabs + Divider + Dialog。
//!
//! 主界面截屏：cargo run --example phase5_containers -- --screenshot artifacts/phase5.png
//! 对话框截屏：cargo run --example phase5_containers -- --dialog --screenshot artifacts/phase5_dialog.png

use windui::prelude::*;

fn list_item(i: usize) -> Element {
    Element::row()
        .width_match()
        .height(40)
        .cross(Align::Center)
        .padding_xy(12, 0)
        .bg_role(if i.is_multiple_of(2) {
            Role::Surface
        } else {
            Role::SurfaceAlt
        })
        .child(
            Element::label(format!("列表项 #{i:02} — 可滚动内容"))
                .fg_role(Role::Text)
                .weight(1.0),
        )
        .child(
            Element::label("›")
                .font_size(18.0)
                .fg_role(Role::TextSubtle),
        )
}

fn main() {
    let dialog_open = std::env::args().any(|a| a == "--dialog");
    let show = signal(dialog_open);
    let tab = signal(0usize);

    // 列表页：滚动容器装入 20 个条目
    let mut list = Element::scroll().fill().bg_role(Role::Surface).corner(8.0);
    for i in 0..20 {
        list = list.child(list_item(i));
    }
    let page_list = Element::col().fill().child(list);

    let show2 = show;
    let page_about = Element::col()
        .fill()
        .padding(16)
        .spacing(12)
        .child(
            Element::label("windui")
                .font_size(24.0)
                .fg_role(Role::Text)
                .height(32)
                .width_match(),
        )
        .child(
            Element::label("轻量 Windows GUI：Win32 + tiny-skia + DirectWrite")
                .fg_role(Role::TextMuted)
                .height(22)
                .width_match(),
        )
        .child(Element::divider())
        .child(Element::button("打开对话框").on_click(move |_| show2.set(true)));

    let tabs = Element::tabs(tab, vec![("列表", page_list), ("关于", page_about)]);

    let show3 = show;
    let dialog = Element::dialog(
        show,
        Element::col()
            .width(300)
            .bg_role(Role::Surface)
            .corner(12.0)
            .padding(20)
            .spacing(14)
            .child(
                Element::label("提示")
                    .font_size(20.0)
                    .fg_role(Role::Text)
                    .height(28)
                    .width_match(),
            )
            .child(
                Element::label("这是一个模态对话框，遮罩会吞掉下层点击。")
                    .fg_role(Role::TextMuted)
                    .height(40)
                    .width_match(),
            )
            .child(
                Element::hstack()
                    .width_match()
                    .height(40)
                    .child(Element::button("关闭").on_click(move |_| show3.set(false))),
            ),
    );

    // 根：主界面 + 叠加对话框
    let ui = Element::stack()
        .fill()
        .child(Element::col().fill().padding(16).child(tabs))
        .child(dialog);

    App::new("Phase 5 — 容器与导航", 480, 440)
        .icon(brand_icon())
        .screenshot_from_args()
        .content(ui)
        .run();
}
