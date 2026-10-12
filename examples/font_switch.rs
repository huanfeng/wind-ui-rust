//! 运行期换界面默认字族（`Theme::font_family`）的真窗口核对。
//!
//! 运行：cargo run --example font_switch
//!       cargo run --example font_switch -- --renderer gpu   （强制 GPU 后端，Windows 上是 D2D）
//!
//! 字族名按 Windows 准备；其他平台多数没装，会静默回退到系统默认（底部那行字体查询也仅 Windows）。
//!
//! 核对点：
//! - 顶部按钮换默认字族：所有没设字族的文字（含按钮、tooltip、toast、右键菜单）跟着换；
//!   「钉死宋体」那行设了 `font_family`，不受影响。
//! - 右键空白处 →「字体」子菜单的项是粘滞的：菜单不关、点一下就换，面板宽度随字体重算。
//! - 输入框里用输入法打字：系统画的组合串字体跟随（焦点控件没设字族时用主题默认）。
//! - 底部一行：当前字族装没装、对一段含生僻字的文本缺哪些字（`text::font_coverage`，仅 Windows）。

use windui::prelude::*;

const FAMILIES: &[&str] = &[
    "Microsoft YaHei UI",
    "SimSun",
    "KaiTi",
    "FangSong",
    "Segoe UI",
    "Consolas",
    "Comic Sans MS",
];

#[cfg(windows)]
fn coverage_line(family: &str) -> String {
    /// 用来查覆盖的样本：常用字 + 扩展 A 的「㐀」+ 扩展 B 的「𠀀」。
    const SAMPLE: &str = "永和九年㐀𠀀";
    use windui::text::{family_available, font_coverage};
    if !family_available(family) {
        return format!("「{family}」本机未安装");
    }
    match font_coverage(family) {
        Some(c) => {
            let miss: String = c.missing(SAMPLE).into_iter().collect();
            if miss.is_empty() {
                format!("「{family}」已安装，「{SAMPLE}」全部覆盖")
            } else {
                format!("「{family}」已安装，「{SAMPLE}」缺：{miss}")
            }
        }
        None => format!("「{family}」已安装，覆盖表取不到"),
    }
}

#[cfg(not(windows))]
fn coverage_line(family: &str) -> String {
    format!("「{family}」（字体查询仅 Windows）")
}

fn status_text(family: &Option<String>) -> String {
    match family {
        Some(f) => coverage_line(f),
        None => "默认字族：平台默认（font_family = None）".into(),
    }
}

fn main() {
    let mut app = App::new("换字族核对", 640, 460);
    let theme = app.theme_handle();
    let status = signal(status_text(&None));
    let input = signal(String::new());

    // 换字族的唯一入口：只改 font_family，别的主题字段原样保留（`set` 会整体替换）。
    let apply = {
        let theme = theme.clone();
        move |family: Option<String>| {
            status.set(status_text(&family));
            theme.update(move |t| t.font_family = family);
        }
    };

    // 两行放按钮：字体名换宽后一行放不下。
    let button = |label: &str, family: Option<&'static str>| {
        let apply = apply.clone();
        Element::button(label)
            .small()
            .tooltip(format!(
                "把界面默认字族换成 {}",
                family.unwrap_or("平台默认")
            ))
            .on_click(move |_| apply(family.map(str::to_string)))
    };
    let (first, rest) = FAMILIES.split_at(3);
    let row_a = first.iter().fold(
        Element::row().spacing(6).child(button("平台默认", None)),
        |r, &f| r.child(button(f, Some(f))),
    );
    let row_b = rest.iter().fold(Element::row().spacing(6), |r, &f| {
        r.child(button(f, Some(f)))
    });
    let buttons = Element::col().spacing(6).child(row_a).child(row_b);

    let menu_theme = theme.clone();
    let menu = move || {
        let current = menu_theme.current().font_family.clone();
        let mut fonts = vec![MenuItem::run(
            "平台默认",
            {
                let apply = apply.clone();
                move |_| apply(None)
            },
            false,
        )
        .check(current.is_none())
        .stay_open()];
        for &f in FAMILIES {
            let apply = apply.clone();
            fonts.push(
                MenuItem::run(f, move |_| apply(Some(f.to_string())), false)
                    .check(current.as_deref() == Some(f))
                    .stay_open(),
            );
        }
        vec![
            MenuItem::submenu("字体", fonts),
            MenuItem::separator(),
            MenuItem::run(
                "复制",
                |ctx| ctx.toast("已复制（toast 也跟着换字体）"),
                false,
            )
            .shortcut("Ctrl+C"),
            MenuItem::run("一段很长的菜单项文字，用来看宽度变化", |_| {}, false)
                .shortcut("Ctrl+Shift+L"),
        ]
    };

    app = app.content(
        Element::col()
            .width_match()
            .height_match()
            .padding(16)
            .spacing(12)
            .bg_role(Role::Surface)
            .on_context_menu(menu)
            .child(Element::label("点按钮或右键空白处 →「字体」换界面默认字族"))
            .child(buttons)
            .child(Element::label(
                "没设字族的文字：天地玄黄 宇宙洪荒 The quick brown fox 0123456789",
            ))
            .child(
                Element::label(
                    "钉死宋体（font_family = SimSun）：天地玄黄 宇宙洪荒 The quick brown fox",
                )
                .font_family("SimSun"),
            )
            .child(
                Element::row()
                    .spacing(8)
                    .child(
                        Element::button("弹一个 toast")
                            .tooltip("悬停这里看 tooltip 的字体")
                            .on_click(|ctx| ctx.toast("toast：天地玄黄 The quick brown fox")),
                    )
                    .child(
                        Element::text_input(input, "在这里用输入法打字，看组合串字体").width(320),
                    ),
            )
            .child(Element::label(status).fg_role(Role::TextMuted)),
    );
    app.screenshot_from_args().run();
}
