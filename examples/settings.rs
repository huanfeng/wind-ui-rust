//! 设置界面：复刻桌面应用设置窗的完整外壳 —— 自绘标题栏 + 图标侧栏 + 内容区 + 底部操作栏，
//! 外加标点表格 / 中文配对两个对话框。
//!
//! 交互窗口：cargo run --example settings
//! 截屏主窗：cargo run --example settings -- --screenshot artifacts/settings.png
//! 两个对话框由右上「标点表格 / 中文配对」按钮打开（运行时点击），或把对应 show_* 初值改为 true 截屏。
//!
//! 版式取自真实项目（`wind-setting`）的四段式：**标题栏 → [侧栏 | 内容] → 底部操作栏**。
//! 比"侧栏 + 内容"两段多出的那两条横带不是装饰：标题栏承载窗口身份（图标 / 名称 / 版本），
//! 底部操作栏承载**全局**动作（保存 / 重载）——把保存按钮塞在侧栏底部会让它看起来
//! 只对侧栏当前选中项负责，而它实际上是整窗提交。

use windui::prelude::*;

#[path = "common/mod.rs"]
mod common;
use common::{theme_toggle, Shell};

/// 左侧竖色条 + 标题的小节头。
fn section_title(title: &str) -> Element {
    Element::row()
        .cross(Align::Center)
        .spacing(10)
        .child(
            Element::leaf()
                .size(4, 18)
                .corner(2.0)
                .bg_role(Role::Accent),
        )
        .child(
            Element::label(title)
                .font_size(16.0)
                .font_weight(700)
                .fg_role(Role::Text),
        )
}

/// 卡片容器。
fn card(body: Element) -> Element {
    Element::col()
        .width_match()
        .bg_role(Role::Surface)
        .corner(12.0)
        .border_role(Role::Border, 1)
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
                        .fg_role(Role::TextMuted),
                )
                .child(
                    Element::icon_button("\u{25BC}")
                        .size(22, 18)
                        .fg_role(Role::TextMuted),
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
                                .fg_role(Role::Text),
                        )
                        .child(Element::badge_intent(tag, Intent::Neutral))
                        .child(
                            Element::label("v1.0")
                                .font_size(12.0)
                                .fg_role(Role::TextMuted),
                        ),
                )
                .child(
                    Element::label(desc)
                        .font_size(12.5)
                        .fg_role(Role::TextMuted),
                ),
        )
        .child(
            Element::icon_button("\u{24D8}")
                .size(26, 26)
                .fg_role(Role::TextMuted),
        )
        .child(status)
        .child(Element::button("方案设置").small().outline().neutral())
}

/// 主方案设置行：标题/描述 + 右侧下拉。库里的 `setting_row_desc` 加一个定宽下拉即可，
/// 字号/字重走本例注入的 `FormTheme`。
fn dropdown_row(title: &str, desc: &str, options: Vec<&str>, sel: Signal<usize>) -> Element {
    Element::setting_row_desc(title, desc, Element::dropdown(options, sel).width(180))
}

/// 快捷键一行：名称 + 右侧键位胶囊。用 `SurfaceAlt` 淡底 + 等宽感字号，
/// 让键位与正文区分开。
fn shortcut_row(name: &str, keys: &str) -> Element {
    Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(10)
        .child(
            Element::label(name)
                .font_size(13.0)
                .fg_role(Role::Text)
                .weight(1.0),
        )
        .child(
            Element::stack()
                .corner(6.0)
                .bg_role(Role::SurfaceAlt)
                .border_role(Role::Border, 1)
                .padding_xy(10, 5)
                .child(
                    Element::label(keys)
                        .font_size(12.0)
                        .fg_role(Role::TextMuted),
                ),
        )
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
                    .fg_role(Role::Text),
            )
            .child(card(
                Element::label("（此页为导航切换占位演示）")
                    .font_size(14.0)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            )),
    )
}

/// 明 / 暗两套主题，**共用同一份 `FormTheme` 覆盖**。
///
/// 这一步容易漏：`ThemeHandle::set(Theme::dark())` 换的是整个 `Theme`，本例注入的
/// 字号/字重覆盖会一并被换掉——切到暗色后设置行的标题会悄悄缩回库默认字号。
/// 覆盖集中在这里，两个分支就不可能走偏。
fn theme_for(dark: bool) -> Theme {
    let mut theme = if dark {
        Theme::dark()
    } else {
        Theme::default()
    };
    // 本稿的设置行标题比库默认更重更大，且行间距由外层 col 统一给，故行本身不留上下内边距。
    theme.form.label_size = Some(15.0);
    theme.form.label_weight = Some(600);
    theme.form.desc_size = Some(12.5);
    theme.form.row_pad_y = Some(0);
    theme
}

/// 侧栏的一个导航项：图标块 + 名称，选中/未选中两棵子树叠放互斥显示，
/// 左缘指示条作为**覆盖层**贴在行左沿。
///
/// 不用改 padding / border 来表达选中：那样切换时图标与文字会横向跳一下。
/// 两态的内边距完全一致，动的只有底色与字重。
fn nav_item(name: &'static str, glyph: &'static str, i: usize, sel: Signal<usize>) -> Element {
    let chip = |selected: bool| {
        Element::stack()
            .size(26, 26)
            .corner(7.0)
            .bg_role(if selected {
                Role::Accent
            } else {
                Role::Surface
            })
            .child(
                Element::label(glyph)
                    .font_size(14.0)
                    .fg_role(if selected {
                        Role::OnAccent
                    } else {
                        Role::TextMuted
                    })
                    .align(Align::Center),
            )
    };

    let on = Element::row()
        .width_match()
        .height(38)
        .corner(9.0)
        .cross(Align::Center)
        .spacing(10)
        .padding_xy(10, 0)
        // 淡底而非实底：实底 accent 会把整条侧栏拉成一块高饱和色斑，压过右侧内容。
        .bg_role_alpha(Role::Accent, 0.12)
        .child(chip(true))
        .child(
            Element::label(name)
                .font_size(13.0)
                .font_weight(600)
                .fg_role(Role::Accent)
                .weight(1.0)
                .max_lines(1),
        )
        .visible_when(move || sel.get() == i);

    let off = Element::row()
        .clickable()
        .on_click(move |_| sel.set(i))
        .width_match()
        .height(38)
        .corner(9.0)
        .cross(Align::Center)
        .spacing(10)
        .padding_xy(10, 0)
        .child(chip(false))
        .child(
            Element::label(name)
                .font_size(13.0)
                .font_weight(500)
                .fg_role(Role::TextMuted)
                .weight(1.0)
                .max_lines(1),
        )
        .visible_when(move || sel.get() != i);

    // 左缘指示条：不占行内宽度，故选中/未选中时图标与文字位置完全一致。
    let indicator = Element::row()
        .width_match()
        .height(38)
        .cross(Align::Center)
        .child(
            Element::leaf()
                .width(3)
                .height(16)
                .corner(1.5)
                .bg_role(Role::Accent),
        )
        .visible_when(move || sel.get() == i);

    Element::stack()
        .width_match()
        .height(38)
        .child(on)
        .child(off)
        .child(indicator)
}

fn main() {
    // 主题必须在**建树之前**装好：setting_row_desc 在构造期读主题定字号。
    let mut app = App::new("应用设置 — windui 示例", 1040, 700)
        .icon(brand_icon())
        .frameless()
        .theme(theme_for(false));
    let th = app.theme_handle();
    let dark = signal(false);

    let nav = signal(0usize);
    let main_scheme = signal(0usize);
    let pinyin_scheme = signal(0usize);
    // 候选窗口卡片
    let cand_dir = signal(0usize);
    let cand_count = signal(5.0f64);
    let show_code = signal(true);
    let follow_caret = signal(true);
    let cand_font = signal(0usize);
    let opacity = signal(0.92f32);
    // 外观页
    let theme_mode = signal(0usize);
    let accent_pick = signal(0usize);
    let win_shadow = signal(true);
    let ui_font_size = signal(14.0f64);
    let ui_scale = signal(0.5f32);
    let compact = signal(false);
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

    // ── 左侧栏：搜索 + 图标导航 ──
    // 品牌区（图标 / 名称 / 版本）已由标题栏承担，侧栏不再重复一遍——两处都摆一次
    // 会让 1040px 宽的窗口里出现两个"这是什么应用"的答案。
    // 字形取自几何 / 杂项符号区：这几段在 Windows 与 macOS 的中文字体里覆盖都稳，
    // 不会退化成豆腐块。彩色 emoji 反而不合适——它们自带配色，压不住也跟不了主题。
    const NAV: [(&str, &str); 8] = [
        ("方案", "\u{25C8}"),
        ("输入", "\u{270E}"),
        ("按键", "\u{2328}"),
        ("外观", "\u{25D0}"),
        ("词库", "\u{25A4}"),
        ("高级", "\u{2699}"),
        ("统计", "\u{25A6}"),
        ("关于", "\u{24D8}"),
    ];
    let mut nav_col = Element::col().width_match().spacing(3);
    for (i, (name, glyph)) in NAV.iter().enumerate() {
        nav_col = nav_col.child(nav_item(name, glyph, i, nav));
    }

    let sidebar = Element::col()
        .width(196)
        .height_match()
        .bg_role(Role::Bg)
        .padding_xy(10, 12)
        .spacing(12)
        .child(
            Element::text_input(signal(String::new()), "搜索设置…")
                .leading_icon('\u{1F50D}')
                .width_match(),
        )
        .child(Element::scroll().weight(1.0).child(nav_col));

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
                            .fg_role(Role::Text),
                    )
                    .child(
                        Element::label("启用、排序与方案专属设置")
                            .font_size(13.0)
                            .fg_role(Role::TextMuted),
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
                            .fg_role(Role::TextMuted)
                            .width_match(),
                    )
                    .child(Element::divider())
                    .child(scheme_row("五笔", "码表", true, "内置 · 五笔86版输入方案"))
                    .child(Element::divider())
                    .child(scheme_row(
                        "五笔拼音",
                        "混输",
                        false,
                        "内置 · 五笔86+拼音混合，五笔优先",
                    ))
                    .child(Element::divider())
                    .child(scheme_row(
                        "全拼",
                        "拼音",
                        false,
                        "内置 · 全拼，支持模糊音与简拼",
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
            ))
            .child(card(
                Element::col()
                    .width_match()
                    .spacing(16)
                    .child(section_title("方案文件"))
                    .child(Element::setting_row_desc(
                        "配置目录",
                        "%APPDATA%\\windui\\schemes",
                        Element::button("打开目录").small().outline().neutral(),
                    ))
                    .child(Element::setting_row_desc(
                        "用户词库",
                        "%APPDATA%\\windui\\user.dict — 3.2 MB · 12 480 词条",
                        Element::button("导出").small().outline().neutral(),
                    ))
                    .child(Element::divider())
                    .child(
                        Element::row()
                            .width_match()
                            .cross(Align::Center)
                            .spacing(8)
                            .child(Element::badge_intent("已同步", Intent::Success))
                            .child(
                                Element::label("上次同步：今天 09:41")
                                    .font_size(12.5)
                                    .fg_role(Role::TextMuted)
                                    .weight(1.0),
                            )
                            .child(Element::button("立即同步").small()),
                    ),
            )),
    );

    // ── 输入页：候选窗口 + 快捷键 ──
    // 这两组原本挂在「方案」页下。按归属重排过：它们描述的是**打字时**的行为，
    // 与"启用哪几套方案、谁是主方案"不是一回事，混在一页会让方案页读起来像杂物抽屉。
    let input_page = Element::scroll().fill().child(
        Element::col()
            .width_match()
            .padding(24)
            .spacing(20)
            .child(
                Element::row()
                    .cross(Align::Center)
                    .spacing(12)
                    .child(
                        Element::label("输入设置")
                            .font_size(24.0)
                            .font_weight(700)
                            .fg_role(Role::Text),
                    )
                    .child(
                        Element::label("候选窗口与快捷键")
                            .font_size(13.0)
                            .fg_role(Role::TextMuted),
                    ),
            )
            .child(card(
                Element::col()
                    .width_match()
                    .spacing(16)
                    .child(section_title("候选窗口"))
                    .child(Element::setting_row_desc(
                        "排列方向",
                        "横排省纵向空间，竖排长词更好读",
                        Element::segmented(vec!["横排", "竖排"], cand_dir),
                    ))
                    .child(Element::setting_row_desc(
                        "候选个数",
                        "一屏显示的候选词数量（1–9）",
                        Element::stepper(cand_count, 1.0, 9.0, 1.0),
                    ))
                    .child(Element::setting_row_desc(
                        "候选字体",
                        "候选窗内的字体，留默认则跟随系统",
                        Element::dropdown(vec!["跟随系统", "微软雅黑", "思源黑体"], cand_font)
                            .width(180),
                    ))
                    .child(Element::setting_row_desc(
                        "窗口不透明度",
                        "低于 100% 时候选窗半透明",
                        Element::slider(opacity).width(180),
                    ))
                    .child(Element::setting_row("显示编码", Element::switch(show_code)))
                    .child(Element::setting_row(
                        "跟随光标",
                        Element::switch(follow_caret),
                    )),
            ))
            .child(card(
                Element::col()
                    .width_match()
                    .spacing(14)
                    .child(section_title("快捷键"))
                    .child(
                        Element::label("点击标签右侧的 × 移除；输入框内回车追加新键位")
                            .font_size(12.5)
                            .fg_role(Role::TextMuted)
                            .width_match(),
                    )
                    // tag_field 收的是**已建好的 chip**，故追加/移除由 app 自己管数据源。
                    // 这里演示静态一组：真实项目里把 chips 换成由 Signal<Vec<String>> 映射出来的。
                    .child(Element::tag_field(
                        "添加键位…",
                        vec![
                            Element::chip("Ctrl+Space", |ctx| ctx.toast("移除 Ctrl+Space")),
                            Element::chip("Shift", |ctx| ctx.toast("移除 Shift")),
                            Element::chip("Ctrl+.", |ctx| ctx.toast("移除 Ctrl+.")),
                        ],
                    ))
                    .child(Element::divider())
                    .child(
                        Element::grid(
                            2,
                            12,
                            vec![
                                shortcut_row("中英切换", "Shift"),
                                shortcut_row("简繁切换", "Ctrl+Shift+F"),
                                shortcut_row("全半角", "Shift+Space"),
                                shortcut_row("标点切换", "Ctrl+."),
                            ],
                        )
                        .width_match(),
                    ),
            )),
    );

    // ── 外观页：主题与排版 ──
    let appearance_page = Element::scroll().fill().child(
        Element::col()
            .width_match()
            .padding(24)
            .spacing(20)
            .child(
                Element::row()
                    .cross(Align::Center)
                    .spacing(12)
                    .child(
                        Element::label("外观设置")
                            .font_size(24.0)
                            .font_weight(700)
                            .fg_role(Role::Text),
                    )
                    .child(
                        Element::label("主题、排版与界面缩放")
                            .font_size(13.0)
                            .fg_role(Role::TextMuted),
                    ),
            )
            .child(card(
                Element::col()
                    .width_match()
                    .spacing(16)
                    .child(section_title("主题"))
                    .child(Element::setting_row_desc(
                        "外观模式",
                        "跟随系统时随系统的浅色/深色设置自动切换",
                        Element::segmented(vec!["跟随系统", "浅色", "深色"], theme_mode),
                    ))
                    .child(Element::setting_row_desc(
                        "强调色",
                        "用于选中态、主按钮与进度条",
                        Element::dropdown(
                            vec!["经典蓝", "海洋青", "日落橙", "森林绿"],
                            accent_pick,
                        )
                        .width(180),
                    ))
                    .child(Element::setting_row(
                        "窗口投影",
                        Element::switch(win_shadow),
                    )),
            ))
            .child(card(
                Element::col()
                    .width_match()
                    .spacing(16)
                    .child(section_title("排版"))
                    .child(Element::setting_row_desc(
                        "界面字号",
                        "影响设置窗与候选窗的正文字号",
                        Element::stepper(ui_font_size, 11.0, 20.0, 1.0),
                    ))
                    .child(Element::setting_row_desc(
                        "界面缩放",
                        "在高 DPI 屏上整体放大界面",
                        Element::slider(ui_scale).width(180),
                    ))
                    .child(Element::setting_row("紧凑模式", Element::switch(compact))),
            )),
    );

    // 内容区 = 按 nav 切换的页面栈（visible_when 显隐，点侧栏即换页）。
    // 前三页是做实的，其余五页留占位——示例要演示的是外壳与控件，不是把一个
    // 真设置窗从头填到尾。
    let placeholders = [
        (2usize, "按键设置"),
        (4, "词库设置"),
        (5, "高级设置"),
        (6, "统计"),
        (7, "关于"),
    ];
    let mut content = Element::stack()
        .height_match()
        .weight(1.0)
        .child(scheme_page.visible_when(move || nav.get() == 0))
        .child(input_page.visible_when(move || nav.get() == 1))
        .child(appearance_page.visible_when(move || nav.get() == 3));
    for (i, title) in placeholders {
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
                    .fg_role(Role::TextMuted)
                    .width_match(),
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

    // ── 底部操作栏：左=服务状态 + 明暗切换；右=全局动作 ──
    // 保存/重载是对**整窗**的提交，故横贯全窗放在底部，而不是塞进侧栏底部——
    // 后者会让人以为它只对当前选中的那一页负责。
    let footer = Element::row()
        .width_match()
        .height(54)
        .cross(Align::Center)
        .padding_xy(16, 0)
        .spacing(10)
        .bg_role(Role::SurfaceAlt)
        .child(
            Element::stack()
                .size(14, 14)
                .corner(7.0)
                .bg_role_alpha(Role::Success, 0.22)
                .child(
                    Element::leaf()
                        .size(8, 8)
                        .corner(4.0)
                        .bg_role(Role::Success)
                        .align(Align::Center),
                ),
        )
        .child(
            Element::label("配置已就绪")
                .font_size(12.5)
                .fg_role(Role::TextMuted),
        )
        .child(theme_toggle(th, dark))
        .child(Element::flex_spacer())
        .child(Element::button("恢复本页").small().outline().neutral())
        .child(Element::button("重新加载").small().outline().neutral())
        .child(Element::button("保存设置").small());

    let body = Element::col()
        .fill()
        .child(
            Element::row()
                .fill()
                .weight(1.0)
                .child(sidebar)
                .child(
                    Element::leaf()
                        .width(1)
                        .height_match()
                        .bg_role(Role::Divider),
                )
                .child(content),
        )
        .child(Element::divider())
        .child(footer);

    let root = Element::stack()
        .fill()
        .bg_role(Role::Bg)
        .child(Shell::new("设置").wrap(body))
        .child(table_dialog)
        .child(pairs_dialog)
        .child(edit_dialog);

    app.screenshot_from_args().content(root).run();
}
