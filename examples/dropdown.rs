//! 下拉选择 Dropdown 示例。
//!
//! 运行：cargo run --release --example dropdown
//! 闭合截屏：cargo run --example dropdown -- --screenshot artifacts/dropdown.png
//! 展开截屏：cargo run --example dropdown -- --screenshot artifacts/dropdown_open.png --click 120 96

use windui::prelude::*;

const BG: u32 = 0xEEF1F5;

fn label(t: &str) -> Element {
    Element::label(t)
        .font_size(13.0)
        .fg(Color::hex(0x636E72))
        .height(20)
        .width_match()
}

fn main() {
    let theme = signal(1usize);
    let quality = signal(0usize);

    let ui = Element::col()
        .fill()
        .bg(Color::hex(BG))
        .padding(20)
        .spacing(10)
        .child(
            Element::label("下拉选择")
                .font_size(22.0)
                .fg(Color::hex(0x1A1A2E))
                .height(30)
                .width_match(),
        )
        .child(label("主题"))
        .child(Element::dropdown(vec!["跟随系统", "浅色", "深色"], theme).width(220))
        .child(label("渲染质量"))
        .child(Element::dropdown(vec!["低", "中", "高", "极致"], quality).width(220));

    App::new("windui — 下拉选择", 320, 280)
        .bg(Color::hex(BG))
        .screenshot_from_args()
        .content(ui)
        .run();
}
