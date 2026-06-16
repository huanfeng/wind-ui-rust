//! 进度条 ProgressBar 示例：确定（多档）+ 不确定（忙碌动画）。
//!
//! 运行：cargo run --release --example progress
//! 截屏：cargo run --example progress -- --screenshot artifacts/progress.png

use std::cell::Cell;
use std::rc::Rc;

use windui::prelude::*;

const BG: u32 = 0xEEF1F5;

fn label(t: &str) -> Element {
    Element::label(t).font_size(13.0).fg(Color::hex(0x636E72)).height(20).width_match()
}

fn main() {
    let p25 = Rc::new(Cell::new(0.25f32));
    let p60 = Rc::new(Cell::new(0.6f32));
    let p100 = Rc::new(Cell::new(1.0f32));

    let ui = Element::col()
        .fill()
        .background(Color::hex(BG))
        .padding(22)
        .spacing(10)
        .child(Element::label("进度条").font_size(22.0).fg(Color::hex(0x1A1A2E)).height(30).width_match())
        .child(label("确定 25%"))
        .child(Element::progress(p25).width_match())
        .child(label("确定 60%"))
        .child(Element::progress(p60).width_match())
        .child(label("确定 100%"))
        .child(Element::progress(p100).width_match())
        .child(label("不确定（忙碌动画）"))
        .child(Element::progress_indeterminate().width_match());

    App::new("windui — 进度条", 320, 280)
        .background(Color::hex(BG))
        .screenshot_from_args()
        .content(ui)
        .run();
}
