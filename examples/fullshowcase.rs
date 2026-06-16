//! 综合示例：一个"偏好设置"小工具，集中展示 windui 全部控件。
//!
//! 运行：    cargo run --release --example fullshowcase
//! 截屏：    cargo run --example fullshowcase -- --screenshot artifacts/showcase.png
//! 对话框：  cargo run --example fullshowcase -- --dialog --screenshot artifacts/showcase_dialog.png

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windui::prelude::*;

const FG: u32 = 0x2D3436;
const SUB: u32 = 0x636E72;
const CARD: u32 = 0xFFFFFF;
const BG: u32 = 0xEEF1F5;

/// 一行设置项：左标签 + 右控件。
fn row(label: &str, control: Element) -> Element {
    Element::row()
        .width_match()
        .height(40)
        .cross(Align::Center)
        .spacing(12)
        .child(Element::label(label).font_size(14.0).fg(Color::hex(FG)).width(110))
        .child(control)
}

fn card(title: &str, body: Element) -> Element {
    Element::col()
        .width_match()
        .background(Color::hex(CARD))
        .corner(10.0)
        .padding(16)
        .spacing(8)
        .child(Element::label(title).font_size(16.0).fg(Color::hex(FG)).height(24).width_match())
        .child(Element::divider())
        .child(body)
}

fn main() {
    let name = Rc::new(RefCell::new(String::from("我的设备")));
    let pwd = Rc::new(RefCell::new(String::from("hunter2")));
    let dark = Rc::new(Cell::new(true));
    let notify = Rc::new(Cell::new(true));
    let beta = Rc::new(Cell::new(false));
    let quality = Rc::new(Cell::new(1usize));
    let volume = Rc::new(Cell::new(0.7f32));
    let show_about = Rc::new(Cell::new(std::env::args().any(|a| a == "--dialog")));

    // 设置页
    let settings = Element::col()
        .fill()
        .spacing(14)
        .child(card(
            "常规",
            Element::col()
                .width_match()
                .spacing(6)
                .child(row("设备名称", Element::text_input(name.clone(), "输入名称").width_match()))
                .child(row("访问密码", Element::text_input(pwd.clone(), "输入密码").password().width_match()))
                .child(row("深色主题", Element::switch(dark.clone())))
                .child(row("接收通知", Element::checkbox("启用推送通知", notify.clone())))
                .child(row("测试版", Element::checkbox("加入 Beta 通道", beta.clone()))),
        ))
        .child(card(
            "渲染",
            Element::col()
                .width_match()
                .spacing(6)
                .child(row("音量", Element::slider(volume.clone()).width_match()))
                .child(row(
                    "质量",
                    Element::row()
                        .spacing(16)
                        .child(Element::radio("低", quality.clone(), 0))
                        .child(Element::radio("中", quality.clone(), 1))
                        .child(Element::radio("高", quality.clone(), 2)),
                )),
        ));

    // 列表页（滚动）
    let mut list = Element::scroll().fill().background(Color::hex(CARD)).corner(10.0);
    for i in 0u32..24 {
        list = list.child(
            Element::row()
                .width_match()
                .height(38)
                .cross(Align::Center)
                .padding_xy(14, 0)
                .background(if i.is_multiple_of(2) { Color::hex(CARD) } else { Color::hex(0xF6F8FA) })
                .child(Element::label(format!("历史记录 {i:02}")).font_size(14.0).fg(Color::hex(FG)).weight(1.0))
                .child(Element::label("查看").font_size(13.0).fg(Color::hex(0x4C8BF5))),
        );
    }

    let about_show = show_about.clone();
    let about = Element::col()
        .fill()
        .spacing(12)
        .child(card(
            "关于 windui",
            Element::col()
                .width_match()
                .spacing(8)
                .child(Element::label("轻量 Windows 桌面 GUI 框架").font_size(15.0).fg(Color::hex(FG)).height(22).width_match())
                .child(Element::label("Win32 窗口 + GDI 呈现 + tiny-skia 图形 + DirectWrite 文字").font_size(13.0).fg(Color::hex(SUB)).height(20).width_match())
                .child(Element::label("目标内存占用 2–5MB，无运行时、无 GC。").font_size(13.0).fg(Color::hex(SUB)).height(20).width_match())
                .child(Element::button("打开对话框").on_click(move |_| about_show.set(true))),
        ));

    let tab = Rc::new(Cell::new(0usize));
    let tabs = Element::tabs(
        tab.clone(),
        vec![("设置", settings), ("历史", Element::col().fill().child(list)), ("关于", about)],
    );

    // 关于对话框
    let close = show_about.clone();
    let dialog = Element::dialog(
        show_about.clone(),
        Element::col()
            .width(320)
            .background(Color::hex(CARD))
            .corner(14.0)
            .padding(22)
            .spacing(14)
            .child(Element::label("windui v0.1").font_size(20.0).fg(Color::hex(FG)).height(28).width_match())
            .child(Element::label("一个用 Rust 编写的轻量桌面 GUI 框架，适合做内存友好的小工具。").font_size(14.0).fg(Color::hex(SUB)).height(44).width_match())
            .child(
                Element::row()
                    .width_match()
                    .height(40)
                    .child(Element::label("").weight(1.0))
                    .child(Element::button("知道了").on_click(move |_| close.set(false))),
            ),
    );

    let ui = Element::stack()
        .fill()
        .background(Color::hex(BG))
        .child(
            Element::col()
                .fill()
                .padding(18)
                .spacing(12)
                .child(Element::label("偏好设置").font_size(24.0).fg(Color::hex(0x1A1A2E)).height(34).width_match())
                .child(tabs),
        )
        .child(dialog);

    App::new("windui — 综合示例", 520, 560)
        .background(Color::hex(BG))
        .screenshot_from_args()
        .content(ui)
        .run();
}
