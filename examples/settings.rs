//! 设置界面：综合验证 IconButton / grid / Chip / TagField / 搜索框 / Dialog 脚手架 / Table
//! 七项新能力，复刻输入法设置窗（侧栏 + 内容 + 标点表格对话框 + 中文配对对话框）。
//!
//! 交互窗口：cargo run --example settings
//! 截屏主窗：cargo run --example settings -- --screenshot artifacts/settings.png
//! 两个对话框由右上「标点表格 / 中文配对」按钮打开（运行时点击），或把对应 show_* 初值改为 true 截屏。

use windui::prelude::*;

const FG: u32 = 0x1F2328;
const MUTED: u32 = 0x8A9099;
const ACCENT: u32 = 0x4C8BF5;

/// 彩色圆角图标方块 + 居中白色字形。
fn icon_box(bg: Color, glyph: &str, size: i32) -> Element {
    Element::stack().size(size, size).corner(10.0).bg(bg).child(
        Element::label(glyph)
            .font_size((size as f32) * 0.42)
            .fg(Color::WHITE)
            .align(Align::Center),
    )
}

/// 左侧竖色条 + 标题的小节头。
fn section_title(title: &str) -> Element {
    Element::row()
        .cross(Align::Center)
        .spacing(10)
        .child(
            Element::leaf()
                .size(4, 18)
                .corner(2.0)
                .bg(Color::hex(ACCENT)),
        )
        .child(
            Element::label(title)
                .font_size(16.0)
                .font_weight(700)
                .fg(Color::hex(FG))
                .height(22),
        )
}

/// 卡片容器。
fn card(body: Element) -> Element {
    Element::col()
        .width_match()
        .bg(Color::WHITE)
        .corner(12.0)
        .border(Color::hex(0xEAECEF), 1)
        .padding(20)
        .spacing(14)
        .child(body)
}

/// 一行输入方案：调序箭头 + 名称/标签/版本 + 描述 + 信息 + 状态 + 设置。
fn scheme_row(name: &str, tag: &str, current: bool, desc: &str) -> Element {
    let status = if current {
        Element::badge("当前方案")
    } else {
        Element::button("设为当前").small().outline().neutral()
    };
    Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(12)
        .padding_xy(4, 10)
        .child(
            Element::col()
                .spacing(2)
                .child(
                    Element::icon_button("\u{25B2}")
                        .size(22, 18)
                        .fg(Color::hex(MUTED)),
                )
                .child(
                    Element::icon_button("\u{25BC}")
                        .size(22, 18)
                        .fg(Color::hex(MUTED)),
                ),
        )
        .child(
            Element::col()
                .weight(1.0)
                .spacing(4)
                .child(
                    Element::row()
                        .cross(Align::Center)
                        .spacing(8)
                        .child(
                            Element::label(name)
                                .font_size(15.0)
                                .font_weight(600)
                                .fg(Color::hex(FG))
                                .height(20),
                        )
                        .child(Element::badge_intent(tag, Intent::Neutral))
                        .child(
                            Element::label("v1.0")
                                .font_size(12.0)
                                .fg(Color::hex(MUTED))
                                .height(18),
                        ),
                )
                .child(
                    Element::label(desc)
                        .font_size(12.5)
                        .fg(Color::hex(MUTED))
                        .height(18),
                ),
        )
        .child(
            Element::icon_button("\u{24D8}")
                .size(26, 26)
                .fg(Color::hex(MUTED)),
        )
        .child(status)
        .child(Element::button("方案设置").small().outline().neutral())
}

/// 主方案设置行：标题/描述 + 右侧下拉。
fn dropdown_row(title: &str, desc: &str, options: Vec<&str>, sel: Signal<usize>) -> Element {
    Element::row()
        .width_match()
        .cross(Align::Center)
        .child(
            Element::col()
                .weight(1.0)
                .spacing(3)
                .child(
                    Element::label(title)
                        .font_size(15.0)
                        .font_weight(600)
                        .fg(Color::hex(FG))
                        .height(20),
                )
                .child(
                    Element::label(desc)
                        .font_size(12.5)
                        .fg(Color::hex(MUTED))
                        .height(18),
                ),
        )
        .child(Element::dropdown(options, sel).width(180))
}

/// 导航占位页（演示左侧栏切换内容）。
fn nav_placeholder(title: &str) -> Element {
    Element::scroll().fill().child(
        Element::col()
            .width_match()
            .padding(24)
            .spacing(16)
            .child(
                Element::label(title)
                    .font_size(24.0)
                    .font_weight(700)
                    .fg(Color::hex(FG))
                    .height(32),
            )
            .child(card(
                Element::label("（此页为导航切换占位演示）")
                    .font_size(14.0)
                    .fg(Color::hex(MUTED))
                    .width_match()
                    .height(22),
            )),
    )
}

fn main() {
    let nav = signal(0usize);
    let main_scheme = signal(0usize);
    let pinyin_scheme = signal(0usize);
    let show_table = signal(false);
    let show_pairs = signal(false);
    // 标点表格的可编辑数据 + 编辑子对话框状态。
    let edit_show = signal(false);
    let edit_buf = signal(String::new());
    let edit_pos = signal((0usize, 0usize));
    // 中文配对：8 个开关状态。
    let pairs: Vec<(Signal<bool>, &str, &str)> = vec![
        (signal(true), "（ ）", "圆括号"),
        (signal(true), "【 】", "方括号"),
        (signal(true), "{ }", "花括号"),
        (signal(true), "《 》", "书名号"),
        (signal(true), "〈 〉", "尖括号"),
        (signal(false), "‘ ’", "单引号"),
        (signal(false), "“ ”", "双引号"),
    ];

    // ── 左侧栏 ──
    let sidebar = Element::col()
        .width(210)
        .height_match()
        .bg(Color::WHITE)
        .border(Color::hex(0xEAECEF), 1)
        .padding(14)
        .spacing(14)
        .child(
            Element::row()
                .cross(Align::Center)
                .spacing(10)
                .child(icon_box(Color::hex(ACCENT), "风", 40))
                .child(
                    Element::col()
                        .weight(1.0)
                        .spacing(2)
                        .child(
                            Element::label("清风输入法")
                                .font_size(15.0)
                                .font_weight(700)
                                .fg(Color::hex(FG))
                                .height(20),
                        )
                        .child(
                            Element::label("v0.0.0-alpha")
                                .font_size(11.5)
                                .fg(Color::hex(MUTED))
                                .height(16),
                        ),
                )
                .child(
                    Element::leaf()
                        .size(8, 8)
                        .corner(4.0)
                        .bg(Color::hex(0x2EA043)),
                ),
        )
        .child(
            Element::text_input(signal(String::new()), "搜索设置…")
                .leading_icon('\u{1F50D}')
                .width_match(),
        )
        .child(
            Element::list_pill(
                vec![
                    "方案", "输入", "按键", "外观", "词库", "高级", "统计", "关于",
                ],
                nav,
            )
            .weight(1.0),
        )
        .child(
            Element::row()
                .width_match()
                .spacing(8)
                .child(
                    Element::button("恢复本页")
                        .small()
                        .outline()
                        .neutral()
                        .weight(1.0),
                )
                .child(
                    Element::button("重新加载")
                        .small()
                        .outline()
                        .neutral()
                        .weight(1.0),
                ),
        )
        .child(Element::button("保存设置").width_match());

    // ── 右侧内容：方案页（横向占剩余空间用 weight，不能用 width_match/fill 否则溢出父宽）──
    let scheme_page = Element::scroll().fill().child(
        Element::col()
            .width_match()
            .padding(24)
            .spacing(20)
            .child(
                Element::row()
                    .cross(Align::Center)
                    .spacing(12)
                    .child(
                        Element::label("方案设置")
                            .font_size(24.0)
                            .font_weight(700)
                            .fg(Color::hex(FG))
                            .height(32),
                    )
                    .child(
                        Element::label("启用、排序与方案专属设置")
                            .font_size(13.0)
                            .fg(Color::hex(MUTED))
                            .height(20),
                    )
                    .child(Element::flex_spacer())
                    .child(
                        Element::button("标点表格")
                            .small()
                            .on_click(move |_| show_table.set(true)),
                    )
                    .child(
                        Element::button("中文配对")
                            .small()
                            .neutral()
                            .on_click(move |_| show_pairs.set(true)),
                    ),
            )
            .child(card(
                Element::col()
                    .width_match()
                    .spacing(12)
                    .child(
                        Element::row()
                            .width_match()
                            .cross(Align::Center)
                            .child(section_title("输入方案").weight(1.0))
                            .child(Element::button("方案管理").small()),
                    )
                    .child(
                        Element::label("使用箭头调整顺序，快捷键切换时按此顺序循环")
                            .font_size(12.5)
                            .fg(Color::hex(MUTED))
                            .width_match()
                            .height(18),
                    )
                    .child(Element::divider())
                    .child(scheme_row("五笔", "码表", true, "内置 · 五笔86版输入方案"))
                    .child(Element::divider())
                    .child(scheme_row(
                        "五笔拼音",
                        "混输",
                        false,
                        "内置 · 五笔86+拼音混合，五笔优先",
                    )),
            ))
            .child(card(
                Element::col()
                    .width_match()
                    .spacing(16)
                    .child(section_title("主方案设置"))
                    .child(dropdown_row(
                        "主码表方案",
                        "拼音方案的\"反查/编码提示\"基于此方案的码表",
                        vec!["五笔", "仓颉"],
                        main_scheme,
                    ))
                    .child(dropdown_row(
                        "主拼音方案",
                        "码表方案的\"临时拼音\"使用此方案",
                        vec!["全拼", "双拼"],
                        pinyin_scheme,
                    )),
            )),
    );

    // 内容区 = 按 nav 切换的页面栈（visible_when 显隐，点侧栏即换页）。
    let pages = [
        "",
        "输入设置",
        "按键设置",
        "外观设置",
        "词库设置",
        "高级设置",
        "统计",
        "关于",
    ];
    let mut content = Element::stack()
        .height_match()
        .weight(1.0)
        .child(scheme_page.visible_when(move || nav.get() == 0));
    for (i, title) in pages.iter().enumerate().skip(1) {
        content = content.child(nav_placeholder(title).visible_when(move || nav.get() == i));
    }

    // ── 标点表格对话框（可编辑：点单元格 → 编辑框 → 写回，表格自动刷新）──
    let table_cols = vec![
        ("原字符", 1.0f32),
        ("英文半角", 1.0),
        ("英文全角", 1.0),
        ("中文半角", 1.0),
        ("中文全角", 1.0),
    ];
    let init: Vec<[&str; 5]> = vec![
        ["空格", "—", "", "—", ""],
        ["!", "!", "！", "!", "！"],
        ["@", "@", "＠", "@", "＠"],
        ["#", "#", "＃", "#", "＃"],
        ["$", "$", "＄", "￥", "￥"],
        ["%", "%", "％", "%", "％"],
        ["^", "^", "＾", "……", "……"],
        ["&", "&", "＆", "&", "＆"],
    ];
    let cells: Vec<Vec<Signal<String>>> = init
        .iter()
        .map(|r| r.iter().map(|s| signal(s.to_string())).collect())
        .collect();
    // 点单元格：载入当前值到编辑缓冲、记录坐标、弹编辑子对话框。
    let cells_edit = cells.clone();
    let table_w = Element::table_editable(table_cols, cells.clone(), move |_ctx, r, c| {
        edit_buf.set(cells_edit[r][c].get());
        edit_pos.set((r, c));
        edit_show.set(true);
    })
    .height(360);
    let table_dialog = Element::dialog_panel(
        show_table,
        "自定义标点设置",
        720,
        move |_| show_table.set(false),
        Element::col()
            .width_match()
            .spacing(10)
            .child(
                Element::label("点单元格编辑，长度 1–8 个字符")
                    .font_size(12.5)
                    .fg(Color::hex(MUTED))
                    .width_match()
                    .height(18),
            )
            .child(table_w),
        Element::row()
            .width_match()
            .child(Element::button("恢复默认").small().outline().neutral())
            .child(Element::flex_spacer())
            .child(
                Element::button("取消")
                    .small()
                    .outline()
                    .neutral()
                    .on_click(move |_| show_table.set(false)),
            )
            .child(
                Element::button("确定")
                    .small()
                    .on_click(move |_| show_table.set(false)),
            ),
    );

    // ── 单元格编辑子对话框 ──
    let cells_ok = cells.clone();
    let edit_dialog = Element::dialog_panel(
        edit_show,
        "编辑单元格",
        340,
        move |_| edit_show.set(false),
        Element::text_input(edit_buf, "输入字符…").width_match(),
        Element::row()
            .width_match()
            .child(Element::flex_spacer())
            .child(
                Element::button("取消")
                    .small()
                    .outline()
                    .neutral()
                    .on_click(move |_| edit_show.set(false)),
            )
            .child(Element::button("确定").small().on_click(move |_| {
                let (r, c) = edit_pos.get();
                cells_ok[r][c].set(edit_buf.get());
                edit_show.set(false);
            })),
    );

    // ── 中文配对对话框（复选框 2 列网格）──
    let checks: Vec<Element> = pairs
        .iter()
        .map(|(sig, sym, label)| Element::checkbox(format!("{sym}  {label}"), *sig))
        .collect();
    let pairs_dialog = Element::dialog_panel(
        show_pairs,
        "中文配对配置",
        520,
        move |_| show_pairs.set(false),
        Element::grid(2, 14, checks).width_match(),
        Element::row()
            .width_match()
            .child(Element::flex_spacer())
            .child(Element::button("全选").small().outline().neutral())
            .child(Element::button("全不选").small().outline().neutral())
            .child(Element::button("确定").small()),
    );

    let root = Element::stack()
        .fill()
        .bg(Color::hex(0xF0F2F4))
        .child(Element::row().fill().child(sidebar).child(content))
        .child(table_dialog)
        .child(pairs_dialog)
        .child(edit_dialog);

    App::new("清风输入法 设置", 1000, 680)
        .bg(Color::hex(0xF0F2F4))
        .screenshot_from_args()
        .content(root)
        .run();
}
