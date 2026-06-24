//! 动画总览示例：集中展示所有带过渡动画的控件，并提供运行期「动画开关」对照。
//!
//! 运行：cargo run --release --example animation
//!
//! 说明：本示例用 `App::animations(true)` **强制开启**动画——无视系统「显示动画」设置，
//! 故即使你在 Windows 里关掉了动画，这里也能看到效果。点顶部「切换动画」按钮可运行期
//! 开/关（调 `windui::anim::set_enabled`）对照：关闭后所有过渡瞬时收敛、开启后平滑过渡。
//!
//! 动画是交互触发的（hover / 点击 / 切换 / 选中），**截图看不到**，请实跑后用鼠标交互：
//! - 开关 Switch：滑块平移 + 轨道色渐变      - CheckBox：方框填充 + 对勾淡入
//! - RadioButton：环加粗 + 中心点放大        - 分段控制器：选中胶囊跨段滑动
//! - 标签页：底部指示条展宽滑动              - 列表行：底色 + 左缘条淡入
//! - 下拉/步进/按钮/链接：hover/press 颜色淡变

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windui::prelude::*;

const FG: u32 = 0x2D3436;
const SUB: u32 = 0x636E72;
const CARD: u32 = 0xFFFFFF;
const BG: u32 = 0xEEF1F5;

fn card(title: &str, body: Element) -> Element {
    Element::col()
        .width_match()
        .bg(Color::hex(CARD))
        .corner(10.0)
        .padding(16)
        .spacing(10)
        .child(
            Element::label(title)
                .font_size(15.0)
                .fg(Color::hex(FG))
                .height(22)
                .width_match(),
        )
        .child(Element::divider())
        .child(body)
}

fn row(label: &str, control: Element) -> Element {
    Element::row()
        .width_match()
        .height(40)
        .cross(Align::Center)
        .child(
            Element::label(label)
                .font_size(14.0)
                .fg(Color::hex(FG))
                .width(96)
                .height(20),
        )
        .child(control)
}

fn main() {
    // 运行期动画开关：Button 点击翻转并调 anim::set_enabled，状态写入动态标签。
    let anim_on = Rc::new(Cell::new(true));
    let anim_label = Rc::new(RefCell::new(String::from("动画：开（点击关闭）")));

    let toggle = {
        let (flag, lbl) = (anim_on.clone(), anim_label.clone());
        Element::button("切换动画").on_click(move |_| {
            let v = !flag.get();
            flag.set(v);
            windui::anim::set_enabled(v);
            *lbl.borrow_mut() = if v {
                "动画：开（点击关闭）".into()
            } else {
                "动画：关（点击开启）".into()
            };
        })
    };

    // 各控件状态绑定。
    let sw1 = Rc::new(Cell::new(true));
    let sw2 = Rc::new(Cell::new(false));
    let chk1 = Rc::new(Cell::new(true));
    let chk2 = Rc::new(Cell::new(false));
    let radio = Rc::new(Cell::new(0usize));
    let seg = Rc::new(Cell::new(0usize));
    let dd = Rc::new(Cell::new(0usize));
    let step = Rc::new(Cell::new(3.0f64));
    let listsel = Rc::new(Cell::new(0usize));
    let tab = Rc::new(Cell::new(0usize));
    let acc = Rc::new(Cell::new(0i32));

    let toggles = card(
        "开关 / 勾选 / 单选（点击看过渡）",
        Element::col()
            .width_match()
            .spacing(8)
            .child(row("Switch A", Element::switch(sw1.clone())))
            .child(row("Switch B", Element::switch(sw2.clone())))
            .child(row(
                "CheckBox",
                Element::row()
                    .spacing(16)
                    .child(Element::checkbox("自动更新", chk1.clone()))
                    .child(Element::checkbox("Beta", chk2.clone())),
            ))
            .child(row(
                "Radio",
                Element::row()
                    .spacing(16)
                    .child(Element::radio("低", radio.clone(), 0))
                    .child(Element::radio("中", radio.clone(), 1))
                    .child(Element::radio("高", radio.clone(), 2)),
            )),
    );

    let selects = card(
        "分段 / 下拉 / 步进（选中切换看滑动）",
        Element::col()
            .width_match()
            .spacing(10)
            .child(row(
                "分段",
                Element::segmented(vec!["简体", "繁体", "其它"], seg.clone()).height(32),
            ))
            .child(row(
                "下拉",
                Element::dropdown(vec!["北京", "上海", "广州"], dd.clone())
                    .width(140)
                    .height(32),
            ))
            .child(row(
                "步进",
                Element::stepper(step.clone(), 0.0, 10.0, 1.0).width(120),
            )),
    );

    let buttons = card(
        "按钮 / 链接（hover/press 看淡变）",
        Element::row()
            .spacing(12)
            .cross(Align::Center)
            .child(Element::button("主要按钮"))
            .child(Element::button("禁用").disabled(true))
            .child(Element::link("一个链接").url("https://example.com")),
    );

    // 标签页（底部指示条滑动）。
    let page = |s: &str| {
        Element::col().padding(12).child(
            Element::label(s)
                .font_size(13.0)
                .fg(Color::hex(SUB))
                .height(20),
        )
    };
    let tabs = card(
        "标签页（切换看指示条展宽滑动）",
        Element::tabs(
            tab.clone(),
            vec![
                ("常规", page("常规设置内容")),
                ("外观", page("外观设置内容")),
                ("高级", page("高级设置内容")),
            ],
        )
        .width_match()
        .height(96),
    );

    let list = card(
        "列表（选中看底色 + 左缘条淡入）",
        Element::list(
            vec!["收件箱", "已发送", "草稿箱", "垃圾箱"],
            listsel.clone(),
        )
        .width_match()
        .height(150)
        .bg(Color::hex(0xF6F8FA))
        .corner(8.0),
    );

    let accordion = card(
        "手风琴（展开仍为瞬时，属 Phase C 待做）",
        Element::accordion(
            acc.clone(),
            vec![
                (
                    "面板一",
                    Element::label("内容一……")
                        .width_match()
                        .height(28)
                        .padding_xy(12, 0),
                ),
                (
                    "面板二",
                    Element::label("内容二……")
                        .width_match()
                        .height(28)
                        .padding_xy(12, 0),
                ),
            ],
        ),
    );

    let header = Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(12)
        .child(
            Element::label("动画总览")
                .font_size(24.0)
                .fg(Color::hex(0x1A1A2E))
                .height(34)
                .weight(1.0),
        )
        .child(toggle)
        .child(
            Element::label_rc(anim_label.clone())
                .font_size(13.0)
                .fg(Color::hex(SUB))
                .height(18)
                .width(150),
        );

    let body = Element::col()
        .width_match()
        .spacing(14)
        .child(header)
        .child(toggles)
        .child(selects)
        .child(buttons)
        .child(tabs)
        .child(list)
        .child(accordion);

    let ui = Element::stack().fill().bg(Color::hex(BG)).child(
        Element::col()
            .fill()
            .padding(18)
            .child(Element::scroll().fill().child(body)),
    );

    App::new("windui — 动画总览", 520, 820)
        .bg(Color::hex(BG))
        .animations(true) // 强制开启：无视系统"显示动画"设置
        .screenshot_from_args()
        .content(ui)
        .run();
}
