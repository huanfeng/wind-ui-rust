//! 主题系统示例：TOML 部分覆盖 + border_width 单位对比（Dp vs Px）。
//!
//! 运行：cargo run --release --example theming
//!
//! 点击右上角"切换"按钮在 Dp（逻辑像素）和 Px（物理像素）之间切换。
//! 在 125% / 150% DPI 缩放下，Dp 边框会因亚像素映射产生模糊感；
//! Px 边框精确落在整数光栅，任意缩放比例下均清晰。

use windui::prelude::*;

const THEME_DP: &str = r##"
[palette]
accent  = "#5B8AF5"
bg      = "#F5F7FA"
surface = "#FFFFFF"
text    = "#1A2035"

[metrics]
corner_md    = 6.0
border_width = 1.0
"##;

/// 使用物理像素边框：{ px = 1 } 在任意 DPI 下清晰无模糊。
const THEME_PX: &str = r##"
[palette]
accent  = "#5B8AF5"
bg      = "#F5F7FA"
surface = "#FFFFFF"
text    = "#1A2035"

[metrics]
corner_md          = 6.0
border_width       = { px = 1 }
border_width_focus = { px = 2 }
"##;

fn main() {
    let theme_dp = Theme::from_toml(THEME_DP).expect("Dp 主题解析失败");
    let theme_px = Theme::from_toml(THEME_PX).expect("Px 主题解析失败");

    let name = signal(String::new());
    let on = signal(true);
    let check = signal(true);
    let vol = signal(0.5f32);
    let mode = signal(0usize);
    let use_px = signal(false);
    let mode_text = signal(String::from(
        "当前：border_width = 1.0（Dp）— 逻辑像素，125%/150% DPI 可见亚像素模糊",
    ));

    let mut app = App::new("windui — border_width 单位对比", 440, 400);
    let handle = app.theme_handle();

    let toggle_btn = {
        let h = handle.clone();
        let tdp = theme_dp.clone();
        let tpx = theme_px.clone();
        let mt = mode_text;
        Element::button("切换单位")
            .on_click(move |_| {
                let now = use_px.get();
                use_px.set(!now);
                if now {
                    h.set(tdp.clone());
                    mt.set(String::from(
                        "当前：border_width = 1.0（Dp）— 逻辑像素，125%/150% DPI 可见亚像素模糊",
                    ));
                } else {
                    h.set(tpx.clone());
                    mt.set(String::from(
                        "当前：border_width = { px = 1 }（Px）— 物理像素，任意 DPI 清晰无模糊",
                    ));
                }
            })
            .neutral()
            .outline()
    };

    let row = |lbl: &str, ctrl: Element| {
        Element::row()
            .width_match()
            .height(36)
            .cross(Align::Center)
            .spacing(10)
            .child(
                Element::label(lbl)
                    .font_size(13.0)
                    .fg(Color::hex(0x555577))
                    .width(56),
            )
            .child(ctrl)
    };

    let card = Element::col()
        .width_match()
        .bg(Color::hex(0xFFFFFF))
        .corner(10.0)
        .padding(16)
        .spacing(8)
        .child(row(
            "文本框",
            Element::text_input(name, "点击聚焦…").width_match(),
        ))
        .child(row(
            "下拉",
            Element::dropdown(vec!["选项 A", "选项 B", "选项 C"], mode).width_match(),
        ))
        .child(row("开关", Element::switch(on)))
        .child(row("复选", Element::checkbox("启用功能", check)))
        .child(row("滑块", Element::slider(vol).width_match()))
        .child(
            Element::row()
                .width_match()
                .spacing(8)
                .child(Element::button("主操作"))
                .child(Element::button("次操作").outline().neutral())
                .child(Element::button("删除").outline().danger()),
        );

    let ui = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .child(
            Element::row()
                .width_match()
                .height(32)
                .cross(Align::Center)
                .spacing(12)
                .child(
                    Element::label("border_width 单位对比")
                        .font_size(17.0)
                        .fg(Color::hex(0x1A2035))
                        .weight(1.0),
                )
                .child(toggle_btn),
        )
        .child(
            Element::label_rc(mode_text)
                .font_size(12.0)
                .fg(Color::hex(0x666688))
                .width_match()
                .height(20),
        )
        .child(card);

    app.theme(theme_dp).content(ui).run();
}
