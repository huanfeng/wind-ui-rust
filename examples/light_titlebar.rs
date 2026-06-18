//! 浅色自定义标题栏示例：常见安装器那类风格（虚构产品，仅演示标题栏与布局）。
//!
//! 运行：cargo run --release --example light_titlebar
//! 截屏：cargo run --example light_titlebar -- --screenshot artifacts/light_titlebar.png
//!
//! 要点（无需改库，纯组合现有能力）：
//! - 无边框窗口（`App::frameless()`）+ 标题栏与正文同为浅色，按钮浮在右上角。
//! - 仅最小化 + 关闭（安装器通常不可最大化），`window_button` 图标色用 `.fg(深灰)`。
//! - 顶部整条 `window_drag()` 可拖动；按钮为可聚焦控件，落在按钮上不拖、可点。

use windui::prelude::*;

const BG: u32 = 0xF5F6F7; // 窗口浅色底
const FG: u32 = 0x1F2329; // 主文字
const SUB: u32 = 0x9AA0A6; // 次要文字
const GREEN: u32 = 0x2AAE67; // 主按钮/Logo 绿
const GLYPH: u32 = 0x60656B; // 标题栏按钮图标灰

fn main() {
    // 标题栏：与正文同色，弹性占位把按钮推到最右；只放最小化 + 关闭。
    let title_bar = Element::row()
        .width_match()
        .height(36)
        .cross(Align::Stretch)
        .bg(Color::hex(BG))
        .window_drag()
        .child(Element::row().weight(1.0)) // 弹性占位
        .child(Element::window_button(WindowButtonKind::Minimize).fg(Color::hex(GLYPH)))
        .child(Element::window_button(WindowButtonKind::Close).fg(Color::hex(GLYPH)));

    // Logo：圆角绿方块占位（真实项目可换 Element::image(...)）。
    let logo = Element::stack()
        .width(96)
        .height(96)
        .bg(Color::hex(GREEN))
        .corner(20.0)
        .align(Align::Center);

    // 底部：勾选 + 协议链接。
    let agreed = std::rc::Rc::new(std::cell::Cell::new(true));
    let agree_row = Element::row()
        .spacing(2)
        .align(Align::Center)
        .child(Element::checkbox("我已阅读并同意", agreed).fg(Color::hex(SUB)).font_size(13.0))
        .child(Element::link("《服务协议》").url("https://example.com/tos").font_size(13.0))
        .child(Element::label("和").fg(Color::hex(SUB)).font_size(13.0).width(20))
        .child(Element::link("《隐私协议》").url("https://example.com/privacy").font_size(13.0));

    let body = Element::col()
        .fill()
        .bg(Color::hex(BG))
        .padding(24)
        .cross(Align::Center)
        .child(Element::col().weight(1.0)) // 顶部弹性留白
        .child(logo)
        .child(Element::label("星尘输入法").font_size(26.0).fg(Color::hex(FG)).height(40).align(Align::Center))
        .child(
            Element::label("适用于 Windows 7 及以上版本")
                .font_size(14.0)
                .fg(Color::hex(SUB))
                .height(24)
                .align(Align::Center),
        )
        .child(Element::col().height(24)) // 间距
        .child(
            Element::button("安装")
                .bg(Color::hex(GREEN))
                .fg(Color::WHITE)
                .corner(8.0)
                .width(300)
                .height(48)
                .font_size(17.0)
                .on_click(|_| println!("开始安装")),
        )
        .child(Element::col().height(16)) // 间距
        .child(agree_row)
        .child(Element::col().weight(1.0)); // 底部弹性留白

    let ui = Element::col().fill().bg(Color::hex(BG)).child(title_bar).child(body.weight(1.0));

    App::new("星尘输入法", 600, 460)
        .frameless()
        .screenshot_from_args()
        .content(ui)
        .run();
}
