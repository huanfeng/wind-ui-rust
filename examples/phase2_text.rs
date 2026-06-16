//! Phase 2 验证：DirectWrite 中英文混排文字渲染。
//!
//! 截屏：cargo run --example phase2_text -- --screenshot artifacts/phase2.png

use windui::prelude::*;

fn row_label(text: &str, size: f32, color: u32) -> Element {
    Element::label(text)
        .font_size(size)
        .fg(Color::hex(color))
        .height_match()
}

fn main() {
    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(14)
        .background(Color::hex(0xFFFFFF))
        .child(
            Element::label("windui 文字渲染")
                .font_size(28.0)
                .fg(Color::hex(0x1A1A2E))
                .width_match()
                .height(40),
        )
        .child(
            Element::label("基于 DirectWrite 的高质量中文排版")
                .font_size(16.0)
                .fg(Color::hex(0x555555))
                .width_match()
                .height(26),
        )
        // 不同字号
        .child(row_label("16px：快速的棕色狐狸 Quick Fox 0123", 16.0, 0x333333).height(26))
        .child(row_label("20px：轻量级桌面 GUI 框架", 20.0, 0x0066CC).height(32))
        // 彩色块上的白字（验证任意背景合成）
        .child(
            Element::col()
                .width_match()
                .height(64)
                .background(Color::hex(0x6C5CE7))
                .corner(8.0)
                .padding_xy(16, 0)
                .child(
                    Element::label("彩色背景上的白色文字 — 灰度抗锯齿合成")
                        .font_size(18.0)
                        .fg(Color::WHITE)
                        .fill(),
                ),
        )
        // 对齐演示
        .child(
            Element::row()
                .width_match()
                .height(40)
                .spacing(8)
                .child(
                    Element::label("左对齐")
                        .text_align(Align::Start)
                        .background(Color::hex(0xF0F0F0))
                        .weight(1.0)
                        .height_match(),
                )
                .child(
                    Element::label("居中")
                        .text_align(Align::Center)
                        .background(Color::hex(0xF0F0F0))
                        .weight(1.0)
                        .height_match(),
                )
                .child(
                    Element::label("右对齐")
                        .text_align(Align::End)
                        .background(Color::hex(0xF0F0F0))
                        .weight(1.0)
                        .height_match(),
                ),
        );

    App::new("Phase 2 — 文字", 600, 420)
        .background(Color::hex(0xFFFFFF))
        .screenshot_from_args()
        .content(ui)
        .run();
}
