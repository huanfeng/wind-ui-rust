//! 目标场景示例：一个"输入法设置"形状的界面，集中演示新控件如何组合成
//! 主从布局的设置页（侧栏导航 + 内容区分组）。
//!
//! 运行： cargo run --release --example ime_settings
//! 截屏： cargo run --example ime_settings -- --screenshot temp/ime.png
//!
//! 用到的控件：SegmentedControl（简/繁等二三选一）、Switch（开关项）、
//! Collapsible（侧栏可折叠分组）、list（侧栏选中高亮）、NavRow（钻入子页 >）。
//! 设置行走库里的 `Element::setting_row`（标签占左、控件贴右）；`section_header`
//! 仍是本示例的局部便捷器（各家分组小标题的设计差异大，未入库）。

use windui::prelude::*;

/// 分组小标题（灰色小号，左对齐）。
fn section_header(text: &str) -> Element {
    Element::label(text)
        .font_size(12.0)
        .fg_role(Role::TextMuted)
        .height(22)
        .width_match()
}

fn main() {
    // 设置行比库默认略高（44 而非 40），给分段控件留出呼吸感。行高走主题而非逐行传参，
    // 故这一处改动对全窗所有 setting_row 一起生效。
    // 主题必须在**建树之前**装好：setting_row 在构造期读主题定行高。
    let mut theme = Theme::default();
    theme.form.row_height = Some(44);
    let app = App::new("输入法设置 — windui 示例", 720, 520).theme(theme);

    // —— 状态 ——
    let nav_sel = signal(0usize); // 侧栏选中项（常用）
    let attr_expand = signal(true); // 侧栏"属性设置"展开
    let zh_form = signal(0usize); // 简体/繁体
    let width_mode = signal(0usize); // 半角/全角
    let cn_en = signal(0usize); // 中文/英文
    let pinyin = signal(0usize); // 全拼/双拼/笔画
    let hide_bar = signal(false);
    let fullscreen_hide = signal(true);
    let fuzzy = signal(true);
    let status = signal(String::from("提示：点击带 > 的行可钻入子页"));

    // —— 侧栏：可折叠分组 + 选中高亮列表 ——
    let sidebar = Element::col()
        .width(170)
        .height_match()
        .bg_role(Role::SurfaceAlt)
        .padding(10)
        .spacing(4)
        .child(
            Element::label("输入法设置")
                .font_size(15.0)
                .fg_role(Role::Text)
                .height(34)
                .width_match(),
        )
        .child(Element::divider())
        .child(Element::collapsible(
            "属性设置",
            attr_expand,
            Element::list(
                vec!["常用", "外观", "词库", "账户", "按键", "高级"],
                nav_sel,
            )
            .width_match()
            .height(6 * 36),
        ));

    // —— 内容区：分组设置项 ——
    let (s1, s2, s3) = (status, status, status);
    let content = Element::scroll().fill().weight(1.0).child(
        Element::col()
            .width_match()
            .padding(22)
            .spacing(6)
            .child(
                Element::label("常用")
                    .font_size(20.0)
                    .fg_role(Role::Text)
                    .height(34)
                    .width_match(),
            )
            .child(section_header("默认状态"))
            .child(Element::setting_row(
                "简体 / 繁体",
                Element::segmented(vec!["简体", "繁体"], zh_form),
            ))
            .child(Element::setting_row(
                "半角 / 全角",
                Element::segmented(vec!["半角", "全角"], width_mode),
            ))
            .child(Element::setting_row(
                "中文 / 英文",
                Element::segmented(vec!["中文", "英文"], cn_en),
            ))
            .child(Element::setting_row(
                "隐藏状态栏",
                Element::switch(hide_bar),
            ))
            // 带 (?) 悬停提示的禁用子项：还原原界面的帮助图标。
            .child(
                Element::row()
                    .width_match()
                    .height(44)
                    .cross(Align::Center)
                    .child(
                        Element::label("显示输入指示器")
                            .font_size(14.0)
                            .fg_role(Role::Text)
                            .margin_xy(12, 0),
                    )
                    .child(
                        Element::label("(?)")
                            .font_size(13.0)
                            .fg_role(Role::TextMuted)
                            .height(20)
                            .tooltip(
                                "开启后，输入时在光标附近显示当前输入状态（中/英、全/半角等）",
                            ),
                    )
                    .child(Element::label("").weight(1.0))
                    .child(Element::switch(signal(false)).disabled(true)),
            )
            .child(Element::setting_row(
                "全屏隐藏状态栏",
                Element::switch(fullscreen_hide),
            ))
            .child(Element::divider())
            .child(section_header("输入习惯"))
            .child(Element::setting_row(
                "输入方案",
                Element::segmented(vec!["全拼", "双拼", "笔画"], pinyin),
            ))
            .child(
                Element::nav_row("双拼方案设定")
                    .on_click(move |_| s1.set("已进入：双拼方案设定".into())),
            )
            .child(Element::setting_row("拼音纠错", Element::switch(fuzzy)))
            .child(
                Element::nav_row("拼音纠错设置")
                    .on_click(move |_| s2.set("已进入：拼音纠错设置".into())),
            )
            .child(
                Element::nav_row("模糊音设置")
                    .on_click(move |_| s3.set("已进入：模糊音设置".into())),
            )
            .child(Element::divider())
            .child(
                Element::label_signal(status)
                    .font_size(13.0)
                    .fg_role(Role::TextMuted)
                    .height(20)
                    .width_match(),
            ),
    );

    let ui = Element::row()
        .fill()
        .bg_role(Role::Surface)
        .child(sidebar)
        .child(content);

    app.screenshot_from_args().content(ui).run();
}
