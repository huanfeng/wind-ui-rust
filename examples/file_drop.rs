//! 文件拖放示例：把文件从资源管理器拖到窗口，列出收到的路径。
//!
//! 运行：cargo run --release --example file_drop
//!
//! `.on_drop_files(...)` 可挂到任意元素；这里挂在占满窗口的根容器上＝全窗接收拖放。
//! 落点会路由到落点下的元素，再沿父链冒泡到首个设了回调的节点。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windui::prelude::*;

const FG: u32 = 0x2D3436;
const SUB: u32 = 0x636E72;

fn main() {
    // 拖放结果绑定到动态标签：回调写入，下一帧显示。
    let report = Rc::new(RefCell::new(String::from("把任意文件从资源管理器拖到这里…")));
    let count = Rc::new(Cell::new(0u32));
    let (r, c) = (report.clone(), count.clone());

    let ui = Element::col()
        .fill()
        .bg(Color::hex(0xFFFFFF))
        .padding(24)
        .spacing(12)
        .on_drop_files(move |ctx, paths| {
            c.set(c.get() + paths.len() as u32);
            let list: Vec<String> = paths.iter().map(|p| format!("• {}", p.display())).collect();
            *r.borrow_mut() = format!("本次收到 {} 个（累计 {}）：\n{}", paths.len(), c.get(), list.join("\n"));
            ctx.mark_dirty(); // 请求重绘以显示新内容
        })
        .child(Element::label("文件拖放").font_size(22.0).fg(Color::hex(FG)).height(30).width_match())
        .child(
            Element::label("把任意文件从资源管理器拖到本窗口任意位置").font_size(13.0).fg(Color::hex(SUB)).height(18).width_match(),
        )
        .child(Element::divider())
        .child(Element::label_rc(report.clone()).font_size(14.0).fg(Color::hex(FG)).width_match().weight(1.0));

    App::new("windui — 文件拖放", 480, 380).bg(Color::hex(0xFFFFFF)).screenshot_from_args().content(ui).run();
}
