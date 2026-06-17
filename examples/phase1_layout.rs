//! Phase 1 验证：嵌套 Row/Column + 彩色 Panel + weight + 对齐。
//!
//! 截屏：cargo run --example phase1_layout -- --screenshot artifacts/phase1.png

use windui::prelude::*;

fn card(bg: u32) -> Element {
    Element::leaf().bg(Color::hex(bg)).corner(8.0).fill()
}

fn main() {
    let ui = Element::col()
        .fill()
        .padding(16)
        .spacing(12)
        .bg(Color::hex(0x1E1E2E))
        // 顶部条：固定高度，水平排列三块等宽（weight）色卡
        .child(
            Element::row()
                .width_match()
                .height(80)
                .spacing(12)
                .child(card(0xF38BA8).weight(1.0))
                .child(card(0xA6E3A1).weight(1.0))
                .child(card(0x89B4FA).weight(2.0)),
        )
        // 中部：左侧栏 + 右侧主区，按 weight 分配，撑满剩余高度
        .child(
            Element::row()
                .fill()
                .weight(1.0)
                .spacing(12)
                .child(card(0xFAB387).width(120))
                .child(
                    Element::col()
                        .fill()
                        .weight(1.0)
                        .spacing(12)
                        .bg(Color::hex(0x313244))
                        .corner(8.0)
                        .padding(12)
                        .child(card(0xF9E2AF).height(40).width_match())
                        // 居中的小色块（交叉轴 Center + 自身 align）
                        .child(
                            Element::leaf()
                                .size(80, 80)
                                .bg(Color::hex(0xCBA6F7))
                                .corner(40.0)
                                .align(Align::Center),
                        )
                        .child(card(0x94E2D5).height(40).width_match()),
                ),
        )
        // 底部条
        .child(
            Element::row()
                .width_match()
                .height(48)
                .spacing(12)
                .cross(Align::Stretch)
                .child(card(0xF5C2E7).weight(3.0))
                .child(card(0xEBA0AC).weight(1.0).border(Color::WHITE, 2)),
        );

    App::new("Phase 1 — 布局", 560, 420)
        .bg(Color::hex(0x1E1E2E))
        .screenshot_from_args()
        .content(ui)
        .run();
}
