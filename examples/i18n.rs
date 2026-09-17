//! 多语言：内置译文 + 外部覆盖 + 运行期换语言 + 位置格式化 + 复数 + 数组。
//!
//! 交互窗口：cargo run --example i18n
//! 截屏：    cargo run --example i18n -- --screenshot artifacts/i18n.png
//!          右键文本框看框架自带菜单：... --rclick 300 250
//!
//! # 这个示例同时是一次**库外视角**的验收
//!
//! 它只 `use windui::prelude::*;`，一行深路径 import 都没有。crate 内部的测试看不见
//! "pub 了但下游够不着"这类问题（构造器在 prelude 里、参数类型却不在，那条路就没人走），
//! 只有从外面写一遍才发现。
//!
//! # 外部覆盖怎么试
//!
//! 仓库根目录的 `i18n/` 就是被 `load_dir` 扫的那个目录。往里丢一个 `fr.toml`：
//!
//! ```toml
//! [meta]
//! locale = "fr"
//! name = "Français"
//! [app]
//! title = "Démo i18n"
//! ```
//!
//! 重新运行（**不用重新编译**），语言按钮里就多一个 Français。写了哪条覆盖哪条，
//! 没写的自动回落到内置译文与回退语言。

use windui::prelude::*;

/// 内置译文：`include_str!` 进二进制，不带任何外部文件也能发多语言版本。
/// 这里为了让示例自包含而写成常量，真实项目里应当是 `include_str!("../i18n/zh-CN.toml")`。
const ZH: &str = r#"
[meta]
locale = "zh-CN"
name = "简体中文"

[app]
title = "多语言演示"
heading = "windui 多语言"
greeting = "你好，{name}！欢迎使用 {product}。"
ratio = "第 {0} 页，共 {1} 页"
input_hint = "在这里右键，菜单跟着语言走"
sec_named = "命名占位 {{name}} / {{product}}"
sec_pos = "位置占位 {{0}} / {{1}}"
sec_plural = "复数：count 选类别"
sec_list = "数组：index 取下标"
external = "把 fr.toml 丢进 ./i18n 就能不重编译地加一种语言"
weekdays = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]

# 复数：值是表不是串。中文只有 other，写一条就够。
[app.files]
other = "已选 {count} 个文件"
"#;

const EN: &str = r#"
[meta]
locale = "en"
name = "English"

[app]
title = "i18n Demo"
heading = "windui internationalization"
greeting = "Hello {name}! Welcome to {product}."
ratio = "Page {0} of {1}"
input_hint = "Right-click here — the menu follows the language"
sec_named = "Named placeholders {{name}} / {{product}}"
sec_pos = "Positional placeholders {{0}} / {{1}}"
sec_plural = "Plurals: count picks the category"
sec_list = "Lists: index picks the item"
external = "Drop fr.toml into ./i18n to add a language without recompiling"
weekdays = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"]

# 英语要区分单复数，于是同一条 key 给两个类别。
[app.files]
one = "{count} file selected"
other = "{count} files selected"
"#;

const JA: &str = r#"
[meta]
locale = "ja"
name = "日本語"

[app]
title = "多言語デモ"
heading = "windui の多言語対応"
greeting = "こんにちは、{name}さん！{product} へようこそ。"
ratio = "{1} ページ中 {0} ページ目"
input_hint = "ここを右クリック — メニューも言語に追従します"
sec_named = "名前付きプレースホルダ {{name}} / {{product}}"
sec_pos = "位置プレースホルダ {{0}} / {{1}}"
sec_plural = "複数形：count が分類を選ぶ"
sec_list = "配列：index で取り出す"
external = "./i18n に fr.toml を置けば再コンパイルなしで言語が増えます"
weekdays = ["月", "火", "水", "木", "金", "土", "日"]

[app.files]
other = "{count} 件のファイルを選択中"
"#;

/// 小标题 + 内容的一栏。
fn section(title: Element, body: Element) -> Element {
    Element::col()
        .width_match()
        .spacing(6)
        .child(title.font_size(12.0).fg_role(Role::TextMuted).width_match())
        .child(body.font_size(15.0).fg_role(Role::Text).width_match())
}

fn main() {
    let locales = Locales::builder()
        .embed(ZH)
        .embed(EN)
        .embed(JA)
        // 外部层：同 key 压过内置，目录不存在也不是错误。
        .load_dir("i18n")
        .fallback("en")
        // 默认就是 Fixed("zh-CN")；写出来是为了指明「跟随系统」要显式换成 Initial::System。
        .initial(Initial::Fixed("zh-CN".into()))
        .build();

    let mut app = App::new("占位标题", 560, 520)
        .locales(locales)
        // 标题也跟着语言走：它是平台持有的字符串，不经 paint，靠这条来源每帧比对重发。
        .title(t!("app.title"));
    let lang = app.locale_handle();

    // 语言按钮从**译文自己报的显示名**生成（`[meta] name`），不是写死一张语言名表——
    // 加一种语言只该动一个文件。
    let mut switcher = Element::row().spacing(8).width_match();
    for info in lang.available() {
        let id = info.id.clone();
        let handle = lang;
        switcher = switcher.child(Element::button(info.name).small().outline().on_click(
            move |_| {
                handle.set(&id);
            },
        ));
    }

    // 复数的数量轴用信号：计数变了自动刷，换语言也自动刷——一条路，不是两条。
    let count = signal(1i64);
    let counter = Element::row()
        .spacing(8)
        .cross(Align::Center)
        .width_match()
        .child(Element::button("−").small().on_click(move |_| {
            count.set((count.get() - 1).max(0));
        }))
        .child(Element::button("+").small().on_click(move |_| {
            count.set(count.get() + 1);
        }))
        .child(Element::label(t!("app.files", count = count)).weight(1.0));

    let text = signal(String::new());

    let body = Element::col()
        .fill()
        .padding(24)
        .spacing(18)
        .bg_role(Role::Bg)
        .child(
            Element::label(t!("app.heading"))
                .font_size(22.0)
                .font_weight(700)
                .fg_role(Role::Text)
                .width_match(),
        )
        .child(switcher)
        .child(Element::divider())
        .child(section(
            Element::label(t!("app.sec_named")),
            Element::label(t!("app.greeting", name = "Ada", product = "windui")),
        ))
        .child(section(
            Element::label(t!("app.sec_pos")),
            Element::label(t!("app.ratio", 3, 12)),
        ))
        .child(section(Element::label(t!("app.sec_plural")), counter))
        .child(section(
            Element::label(t!("app.sec_list")),
            Element::label(t!("app.weekdays", index = 2)),
        ))
        .child(section(
            Element::label(t!("app.input_hint")),
            Element::text_input(text, "..."),
        ))
        .child(
            Element::label(t!("app.external"))
                .font_size(12.0)
                .fg_role(Role::TextSubtle)
                .width_match(),
        );

    app.screenshot_from_args().content(body).run();
}
