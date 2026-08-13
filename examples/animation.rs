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

use windui::prelude::*;

fn main() {
    // 运行期动画开关：Button 点击翻转并调 anim::set_enabled，状态写入动态标签。
    let anim_on = signal(true);
    let anim_label = signal(String::from("动画：开（点击关闭）"));

    let toggle = {
        let (flag, lbl) = (anim_on, anim_label);
        Element::button("切换动画").on_click(move |_| {
            let v = !flag.get();
            flag.set(v);
            windui::anim::set_enabled(v);
            lbl.set(if v {
                "动画：开（点击关闭）".into()
            } else {
                "动画：关（点击开启）".into()
            });
        })
    };

    // 各控件状态绑定。
    let sw1 = signal(true);
    let sw2 = signal(false);
    let chk1 = signal(true);
    let chk2 = signal(false);
    let radio = signal(0usize);
    let seg = signal(0usize);
    let dd = signal(0usize);
    let step = signal(3.0f64);
    let listsel = signal(0usize);
    let tab = signal(0usize);
    let acc = signal(Some(0usize));

    let toggles = Element::card(
        "开关 / 勾选 / 单选（点击看过渡）",
        Element::col()
            .width_match()
            .spacing(8)
            .child(Element::field("Switch A", Element::switch(sw1)))
            .child(Element::field("Switch B", Element::switch(sw2)))
            .child(Element::field(
                "CheckBox",
                Element::row()
                    .spacing(16)
                    .child(Element::checkbox("自动更新", chk1))
                    .child(Element::checkbox("Beta", chk2)),
            ))
            .child(Element::field(
                "Radio",
                Element::row()
                    .spacing(16)
                    .child(Element::radio("低", radio, 0))
                    .child(Element::radio("中", radio, 1))
                    .child(Element::radio("高", radio, 2)),
            )),
    );

    let selects = Element::card(
        "分段 / 下拉 / 步进（选中切换看滑动）",
        Element::col()
            .width_match()
            .spacing(10)
            .child(Element::field(
                "分段",
                Element::segmented(vec!["简体", "繁体", "其它"], seg).height(32),
            ))
            .child(Element::field(
                "下拉",
                Element::dropdown(vec!["北京", "上海", "广州"], dd)
                    .width(140)
                    .height(32),
            ))
            .child(Element::field(
                "步进",
                Element::stepper(step, 0.0, 10.0, 1.0).width(120),
            )),
    );

    let buttons = Element::card(
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
                .fg_role(Role::TextMuted)
                .height(20),
        )
    };
    let tabs = Element::card(
        "标签页（切换看指示条展宽滑动）",
        Element::tabs(
            tab,
            vec![
                ("常规", page("常规设置内容")),
                ("外观", page("外观设置内容")),
                ("高级", page("高级设置内容")),
            ],
        )
        .width_match()
        .height(96),
    );

    let list = Element::card(
        "列表（选中看底色 + 左缘条淡入）",
        Element::list(vec!["收件箱", "已发送", "草稿箱", "垃圾箱"], listsel)
            .width_match()
            .height(150)
            .bg_role(Role::SurfaceAlt)
            .corner(8.0),
    );

    let accordion = Element::card(
        "手风琴（展开仍为瞬时，属 Phase C 待做）",
        Element::accordion(
            acc,
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
                .fg_role(Role::Text)
                .height(34)
                .weight(1.0),
        )
        .child(toggle)
        .child(
            Element::label_signal(anim_label)
                .font_size(13.0)
                .fg_role(Role::TextMuted)
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

    let ui = Element::stack().fill().bg_role(Role::Bg).child(
        Element::col()
            .fill()
            .padding(18)
            .child(Element::scroll().fill().child(body)),
    );

    App::new("windui — 动画总览", 520, 820)
        .animations(true) // 强制开启：无视系统"显示动画"设置
        .screenshot_from_args()
        .content(ui)
        .run();
}
