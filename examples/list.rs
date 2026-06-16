//! 列表 ListView 示例：单选 + 选中/悬停高亮 + 滚动。
//!
//! 运行：cargo run --release --example list
//! 截屏：cargo run --example list -- --screenshot artifacts/list.png

use std::cell::Cell;
use std::rc::Rc;

use windui::prelude::*;

const BG: u32 = 0xEEF1F5;

fn main() {
    let sel = Rc::new(Cell::new(2usize));
    let items = vec![
        "收件箱", "已发送", "草稿箱", "垃圾邮件", "归档",
        "重要", "已加星标", "全部邮件", "未读", "已删除",
    ];

    let ui = Element::col()
        .fill()
        .background(Color::hex(BG))
        .padding(20)
        .spacing(10)
        .child(Element::label("列表（单选）").font_size(22.0).fg(Color::hex(0x1A1A2E)).height(30).width_match())
        .child(
            Element::list(items, sel)
                .width_match()
                .weight(1.0)
                .background(Color::WHITE)
                .corner(10.0)
                .border(Color::hex(0xE2E6EA), 1),
        );

    App::new("windui — 列表", 300, 360)
        .background(Color::hex(BG))
        .screenshot_from_args()
        .content(ui)
        .run();
}
