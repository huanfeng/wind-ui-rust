//! 无标题栏窗口示例：自定义标题栏（拖动区 + 最小化/最大化/关闭按钮）。
//!
//! 运行：cargo run --release --example frameless
//! 截屏：cargo run --example frameless -- --screenshot artifacts/frameless.png
//!
//! - 拖动深色标题栏空白处移动窗口（按钮区自动排除，仍可点击）。
//! - 右上角三个按钮：最小化 / 最大化-还原 / 关闭。
//! - 窗口四边/四角可缩放；保留 Aero 吸附与窗口投影。

use windui::prelude::*;

const TITLE_BG: u32 = 0x2D3436;
const FG: u32 = 0x2D3436;
const SUB: u32 = 0x636E72;

fn main() {
    // 标题栏：整条可拖（window_drag），按钮为可聚焦控件故落在按钮上不拖、可点。
    let title_bar = Element::row()
        .width_match()
        .height(36)
        .cross(Align::Stretch)
        .bg(Color::hex(TITLE_BG))
        .window_drag()
        .child(
            Element::label("   windui — 无边框窗口")
                .font_size(14.0)
                .fg(Color::WHITE)
                .weight(1.0),
        )
        .child(Element::window_button(WindowButtonKind::Minimize).fg(Color::WHITE))
        .child(Element::window_button(WindowButtonKind::Maximize).fg(Color::WHITE))
        .child(Element::window_button(WindowButtonKind::Close).fg(Color::WHITE));

    let body = Element::col()
        .fill()
        .bg(Color::hex(0xFFFFFF))
        .padding(24)
        .spacing(10)
        .child(
            Element::label("自定义标题栏")
                .font_size(20.0)
                .fg(Color::hex(FG))
                .height(28)
                .width_match(),
        )
        .child(
            Element::label(
                "拖动深色标题栏移动窗口；右上角按钮最小化/最大化/关闭；窗口边缘可缩放。",
            )
            .font_size(13.0)
            .fg(Color::hex(SUB))
            .width_match()
            .weight(1.0),
        );

    let ui = Element::col()
        .fill()
        .child(title_bar)
        .child(body.weight(1.0));

    App::new("windui — frameless", 520, 360)
        .frameless()
        .screenshot_from_args()
        .content(ui)
        .run();
}
