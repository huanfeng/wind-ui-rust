//! 多行 / 密码 文本框聚焦示例。
//!
//! 运行：cargo run --release --example multiline
//! 截屏：cargo run --example multiline -- --screenshot artifacts/multiline.png

use std::cell::RefCell;
use std::rc::Rc;

use windui::prelude::*;

const FG: u32 = 0x2D3436;
const CARD: u32 = 0xFFFFFF;
const BG: u32 = 0xEEF1F5;

fn label(t: &str) -> Element {
    Element::label(t).font_size(13.0).fg(Color::hex(0x636E72)).height(20).width_match()
}

fn main() {
    let wrap_txt = Rc::new(RefCell::new(String::from(
        "软换行模式：超过文本框宽度的长行会自动折到下一视觉行，不需要手动断行。\n这是第二个段落（按 Enter 产生的硬换行）。",
    )));
    let code_txt = Rc::new(RefCell::new(String::from(
        "fn main() {\n    println!(\"不换行模式：长行水平滚动\");\n}",
    )));
    let pwd = Rc::new(RefCell::new(String::from("s3cr3t-pass")));

    let ui = Element::col()
        .fill()
        .bg(Color::hex(BG))
        .padding(18)
        .spacing(12)
        .child(Element::label("多行 / 密码 文本框").font_size(22.0).fg(Color::hex(0x1A1A2E)).height(30).width_match())
        .child(label("软换行多行（默认）"))
        .child(
            Element::text_input(wrap_txt, "输入多行文本")
                .multiline()
                .width_match()
                .height(96)
                .bg(Color::hex(CARD)),
        )
        .child(label("不换行多行（长行水平滚动）"))
        .child(
            Element::text_input(code_txt, "输入代码")
                .multiline()
                .wrap(false)
                .width_match()
                .height(72)
                .fg(Color::hex(FG)),
        )
        .child(label("密码"))
        .child(Element::text_input(pwd, "输入密码").password().width_match());

    App::new("windui — 多行/密码", 420, 360)
        .bg(Color::hex(BG))
        .screenshot_from_args()
        .content(ui)
        .run();
}
