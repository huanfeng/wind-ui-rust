//! Phase 3 验证：按钮 + 事件 + 焦点。
//!
//! 交互窗口：cargo run --example phase3_button
//! 截屏：    cargo run --example phase3_button -- --screenshot artifacts/phase3.png
//!
//! 交互正确性（点击/捕获/hover）由 core.rs 的单元测试覆盖。

use windui::prelude::*;

fn main() {
    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(16)
        .bg(Color::hex(0xF5F6FA))
        .child(
            Element::label("点击按钮 / Tab 切换焦点 / 回车激活")
                .font_size(18.0)
                .fg(Color::hex(0x2D3436))
                .width_match()
                .height(28),
        )
        .child(
            Element::row()
                .width_match()
                .height(44)
                .spacing(12)
                .child(Element::button("确定").on_click(|_| println!("确定 clicked")))
                .child(Element::button("取消").on_click(|ctx| ctx.request_close()))
                .child(Element::button("应用").on_click(|_| println!("应用 clicked"))),
        )
        .child(
            Element::col()
                .width_match()
                .weight(1.0)
                .bg(Color::WHITE)
                .corner(10.0)
                .border(Color::hex(0xDFE6E9), 1)
                .padding(20)
                .spacing(12)
                .child(
                    Element::label("卡片区域")
                        .font_size(20.0)
                        .fg(Color::hex(0x0984E3))
                        .width_match()
                        .height(28),
                )
                .child(
                    Element::label(
                        "按钮具备 normal / hover / pressed 三态与焦点环，事件经命中测试冒泡分发。",
                    )
                    .font_size(14.0)
                    .fg(Color::hex(0x636E72))
                    .width_match()
                    .height(22),
                )
                .child(
                    Element::row()
                        .width_match()
                        .height(40)
                        .spacing(10)
                        .child(Element::button("保存").on_click(|_| println!("保存")))
                        .child(Element::button("删除").on_click(|_| println!("删除"))),
                ),
        );

    App::new("Phase 3 — 按钮与事件", 560, 360)
        .bg(Color::hex(0xF5F6FA))
        .screenshot_from_args()
        .content(ui)
        .run();
}
