//! Emoji 渲染演示：DirectWrite 彩色字形（COLR/CPAL）逐层着色。
//! 组合序列（ZWJ 家庭、彩虹旗）与肤色修饰均正确合成彩色。
//! 截屏：cargo run --example emoji -- --screenshot artifacts/emoji.png

use windui::prelude::*;

fn line(text: &str) -> Element {
    Element::label(text)
        .font_size(28.0)
        .fg_role(Role::Text)
        .width_match()
        .height(44)
}

fn main() {
    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(10)
        .child(line("纯 emoji: 😀 😎 🎉 ❤️ 👍 🚀"))
        .child(line("中英混排: Hello 世界 🌍 windui 🦀"))
        .child(line("符号: ★ ☂ ♻ ✓ → ©"))
        .child(line("组合序列: 👨‍👩‍👧 👋🏽 🏳️‍🌈"))
        .child(line("更多: 🔥 💧 🌳 ⚡ 🎨 🍕"));

    App::new("windui — Emoji", 640, 360)
        .icon(brand_icon())
        .screenshot_from_args()
        .content(ui)
        .run();
}
