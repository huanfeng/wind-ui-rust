//! 动画总览示例：集中展示所有带过渡动画的控件，并提供运行期「动画开关」对照。
//!
//! 运行：cargo run --release --example animation
//!
//! 说明：本示例用 `App::animations(true)` **强制开启**动画——无视系统「显示动画」/「减弱
//! 动态效果」设置，故即使系统里关掉了动画，这里也能看到效果。顶部三个按钮运行期切换动效
//! 档位（调 `windui::anim::set_motion_level`）对照：
//! - 全开：所有过渡平滑；
//! - 减弱：位移类（开关滑块、分段胶囊、标签页指示条、光标滑行）瞬时到位，颜色渐变保留
//!   （开关的轨道色由滑块位置推出，随滑块一起瞬变）；
//! - 关闭：所有过渡瞬时收敛。
//!
//! 想看清曲线，可用 `WINDUI_ANIM_SCALE=5` 把所有补间放慢 5 倍。
//!
//! 动画是交互触发的（hover / 点击 / 切换 / 选中），**截图看不到**，请实跑后用鼠标交互：
//! - 开关 Switch：滑块平移 + 轨道色渐变      - CheckBox：方框填充 + 对勾淡入
//! - RadioButton：环加粗 + 中心点放大        - 分段控制器：选中胶囊跨段滑动
//! - 标签页：底部指示条展宽滑动              - 列表行：底色 + 左缘条淡入
//! - 下拉/步进/按钮/链接：hover/press 颜色淡变
//! - 手风琴 / 折叠面板：高度卷帘展开 + 淡入、箭头旋转；`animate_height`：内容变高变矮时平滑过渡
//! - 状态卡片：`bg_when` + `animate_style`，背景色随状态平滑过渡
//! - 进出场（`Element::transition`）：提示条淡入下落/淡出上收、对话框遮罩淡入 + 面板放大、
//!   颜色选择浮层淡入轻落、下拉菜单与悬停提示淡入

use windui::prelude::*;

fn main() {
    // 运行期动效档位：三个按钮各设一档，当前档写入动态标签。
    let anim_label = signal(String::from("当前：全开"));
    let level_button = |text: &'static str, level: MotionLevel| {
        Element::button(text).on_click(move |_| {
            windui::anim::set_motion_level(level);
            anim_label.set(format!("当前：{text}"));
        })
    };
    let toggle = Element::row()
        .spacing(6)
        .child(level_button("全开", MotionLevel::Full))
        .child(level_button("减弱", MotionLevel::Reduced))
        .child(level_button("关闭", MotionLevel::Off));

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

    // 进出场：提示条显隐（离场期间继续占位，播完才让出空间）、对话框、锚定浮层。
    let hint_on = signal(false);
    let more = signal(false);
    let alert = signal(false);
    let show_dialog = signal(false);
    let color = signal(Color::hex(0x4C8BF5));
    let presence = Element::card(
        "进出场（Element::transition）",
        Element::col()
            .width_match()
            .spacing(8)
            // 卡片高度也跟着平滑伸缩，下方区块随之让位（里面的行仍各自滑动）。
            .animate_height()
            .child(
                Element::row()
                    .spacing(8)
                    .child(
                        Element::button("显示/隐藏提示")
                            .on_click(move |_| hint_on.set(!hint_on.get())),
                    )
                    .child(Element::button("打开对话框").on_click(move |_| show_dialog.set(true)))
                    .child(Element::color_picker(color).tooltip("点开是锚定浮层：默认淡入轻落")),
            )
            .child(
                Element::label("✓ 设置已保存（离场时继续占位，播完才让出空间）")
                    .width_match()
                    .height(32)
                    .padding_xy(12, 0)
                    .bg_role(Role::SurfaceAlt)
                    .corner(6.0)
                    .visible_signal(hint_on)
                    .transition(Presence::fade().slide(0, -6)),
            )
            .child(
                // 布局过渡：提示条显隐改变了下面几行的布局位置，它们滑过去而不是跳过去。
                Element::label("↑ 提示条显隐时，下面这几行平滑让位（animate_layout）")
                    .font_size(12.0)
                    .fg_role(Role::TextMuted)
                    .height(18)
                    .animate_layout(),
            )
            .child(
                // 隐式样式过渡：背景色随状态变（bg_when），变化平滑过渡（animate_style）。
                Element::row()
                    .width_match()
                    .cross(Align::Center)
                    .spacing(8)
                    .padding_xy(12, 8)
                    .corner(6.0)
                    .bg_when(move || {
                        if alert.get() {
                            Color::hex(0xFDE2E1)
                        } else {
                            Color::hex(0xE7F0FD)
                        }
                    })
                    .animate_style()
                    .animate_layout()
                    .child(
                        Element::label("状态卡片：背景色随状态平滑过渡（bg_when + animate_style）")
                            .weight(1.0)
                            .height(22),
                    )
                    .child(
                        Element::button("切换告警")
                            .small()
                            .on_click(move |_| alert.set(!alert.get())),
                    ),
            )
            .child(
                Element::button("展开/收起详情（animate_height）")
                    .outline()
                    .on_click(move |_| more.set(!more.get()))
                    .animate_layout(),
            )
            .child(
                // 内容的自然高度变化时，占位高度平滑跟过去。
                Element::col()
                    .width_match()
                    .spacing(4)
                    .padding_xy(12, 8)
                    .bg_role(Role::SurfaceAlt)
                    .corner(6.0)
                    .animate_height()
                    .animate_layout()
                    .child(Element::label("摘要：本卡片的高度随内容平滑变化。").height(22))
                    .child(
                        Element::col()
                            .visible_signal(more)
                            .spacing(4)
                            .child(Element::label("详情第一行……").height(22))
                            .child(Element::label("详情第二行……").height(22))
                            .child(Element::label("详情第三行……").height(22)),
                    ),
            ),
    );

    let dialog = Element::dialog_panel(
        show_dialog,
        "对话框",
        320,
        move |_| show_dialog.set(false),
        Element::label("遮罩淡入，面板从 0.96 倍放大；关闭时反向。减弱档下只淡不缩放。")
            .width_match(),
        Element::row()
            .width_match()
            .child(Element::flex_spacer())
            .child(Element::button("关闭").on_click(move |_| show_dialog.set(false))),
    );

    let accordion = Element::card(
        "手风琴（展开：高度卷帘 + 淡入，箭头旋转）",
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
                .width(90),
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
        .child(presence)
        .child(accordion);

    let ui = Element::stack()
        .fill()
        .bg_role(Role::Bg)
        .child(
            Element::col()
                .fill()
                .padding(18)
                .child(Element::scroll().fill().child(body)),
        )
        .child(dialog);

    App::new("windui — 动画总览", 520, 820)
        .icon(brand_icon())
        .animations(true) // 强制开启：无视系统"显示动画"设置
        .screenshot_from_args()
        .content(ui)
        .run();
}
