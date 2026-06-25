//! 中文输入法界面复刻：候选窗口外的三大组件——状态工具栏、右键级联菜单、设置界面。
//! 暗/亮双主题运行期热切换；复用本次新增的渐变 / 浮层阴影 / 主题角色 / 半透明文字 / opacity 能力。
//!
//! 运行：  cargo run --release --example ime
//! 暗色截图：cargo run --example ime -- --screenshot artifacts/ime_dark.png
//! 亮色截图：cargo run --example ime -- --light --screenshot artifacts/ime_light.png
//! 切主题：窗口右上角「暗色 / 亮色」按钮，或点外观页主题卡。

use windui::prelude::*;

fn hex(v: u32) -> Color {
    Color::hex(v)
}
/// 设计主强调蓝，暗亮一致（#3b82f6）。
const ACCENT: u32 = 0x3B82F6;
/// 半透明强调色叠层：alpha<255，绘制时实时合成到下方按主题刷新的父面板上。
fn accent_a(a: u8) -> Color {
    Color::rgba(0x3B, 0x82, 0xF6, a)
}

/// 亮色主题（设计浅色稿配色）。
fn light_theme() -> Theme {
    let mut t = Theme::default();
    let p = &mut t.palette;
    p.accent = hex(ACCENT);
    p.accent_hover = hex(0x60A5FA);
    p.accent_active = hex(0x2563EB);
    p.on_accent = Color::WHITE;
    p.bg = hex(0xEEF1F5);
    p.surface = hex(0xFFFFFF);
    p.surface_alt = hex(0xF4F6F9);
    p.text = hex(0x1A1F2A);
    p.text_muted = hex(0x6B7280);
    p.text_disabled = hex(0xB7BECA);
    p.border = hex(0xE3E7EC);
    p.divider = hex(0xECEEF2);
    p.track = hex(0xD7DCE3);
    t
}

/// 暗色主题（设计深色稿配色）。
fn dark_theme() -> Theme {
    let mut t = Theme::dark();
    let p = &mut t.palette;
    p.accent = hex(ACCENT);
    p.accent_hover = hex(0x60A5FA);
    p.accent_active = hex(0x2563EB);
    p.on_accent = Color::WHITE;
    p.bg = hex(0x0F1626);
    p.surface = hex(0x141C2B);
    p.surface_alt = hex(0x1B2436);
    p.text = hex(0xE8EAED);
    p.text_muted = hex(0x8B94A7);
    p.text_disabled = hex(0x5A6377);
    p.border = hex(0x28324A);
    p.divider = hex(0x222C40);
    p.track = hex(0x2B3550);
    t
}

fn theme_for(dark: bool) -> Theme {
    if dark {
        dark_theme()
    } else {
        light_theme()
    }
}

/// 浮层面板：圆角 surface + 边框 + 投影（设计中所有浮窗的统一外观）。
fn panel() -> Element {
    flat_panel().shadow(Shadow::new(0.0, 6.0, 14.0, Color::rgba(0, 0, 0, 110)))
}

/// 无投影面板（圆角 surface + 边框）。用于密集/装饰性面板，避免大量大阴影合成拖慢帧率。
fn flat_panel() -> Element {
    Element::col()
        .bg_role(Role::Surface)
        .border_role(Role::Border, 1)
        .corner(12.0)
}

/// 小节标签（11px 弱化大写感）。
fn section(title: &str, body: Element) -> Element {
    Element::col()
        .spacing(10)
        .child(
            Element::label(title)
                .font_size(11.0)
                .fg_role(Role::TextMuted)
                .height(16),
        )
        .child(body)
}

/// 竖向 1px 分隔（工具栏用）。
fn vdivider() -> Element {
    Element::leaf().size(1, 16).bg_role(Role::Divider)
}

// ============================ 1 · 状态工具栏 ============================

fn toolbar() -> Element {
    // 工具栏按钮：active=强调底+强调字；普通=主文字；dim=弱化文字。
    let btn = |label: &str, min_w: i32, active: bool, dim: bool| {
        let mut e = Element::label(label)
            .font_size(13.0)
            .text_align(Align::Center)
            .width(min_w)
            .height(28)
            .corner(7.0);
        if active {
            e = e.bg(accent_a(48)).fg_role(Role::Accent).font_weight(600);
        } else if dim {
            e = e.fg_role(Role::TextMuted);
        } else {
            e = e.fg_role(Role::Text);
        }
        e
    };
    let bar = Element::row()
        .cross(Align::Center)
        .spacing(2)
        .padding_xy(6, 4)
        // 拖动柄（6 点近似）。
        .child(
            Element::label("⠿")
                .font_size(15.0)
                .fg_role(Role::TextMuted)
                .width(16)
                .height(28)
                .text_align(Align::Center),
        )
        .child(vdivider())
        .child(btn("中", 34, true, false))
        .child(btn("，。", 32, false, false))
        .child(btn("半", 30, false, true))
        .child(btn("简", 30, false, true))
        .child(vdivider())
        .child(btn("😊", 30, false, true))
        .child(btn("⚙", 30, false, true))
        // 扩展插件占位（虚线感用淡边框近似）。
        .child(
            Element::label("+")
                .font_size(15.0)
                .text_align(Align::Center)
                .fg_role(Role::TextMuted)
                .width(26)
                .height(26)
                .corner(6.0)
                .border_role(Role::Border, 1),
        );
    panel().padding(0).child(bar)
}

// ============================ 2 · 右键级联菜单 ============================

/// 菜单项：图标 + 文本 + 尾随（快捷键/勾/箭头）。active 高亮。
fn menu_item(icon: &str, label: &str, trailing: &str, active: bool, accent_text: bool) -> Element {
    let mut row = Element::row()
        .width_match()
        .height(32)
        .cross(Align::Center)
        .spacing(10)
        .padding_xy(12, 0)
        .margin_xy(4, 1)
        .corner(6.0)
        .child(
            Element::label(icon)
                .font_size(13.0)
                .fg_role(Role::TextMuted)
                .width(16)
                .text_align(Align::Center),
        )
        .child(
            Element::label(label)
                .font_size(13.0)
                .font_weight(if accent_text { 500 } else { 400 })
                .fg_role(if accent_text {
                    Role::Accent
                } else {
                    Role::Text
                }),
        )
        // 弹性占位把尾随推到最右（避免给标签加 weight 时挤压尾随快捷键换行）。
        .child(Element::leaf().weight(1.0))
        .child(
            Element::label(trailing)
                .font_size(11.0)
                .max_lines(1)
                .fg_role(Role::TextMuted),
        );
    if active {
        row = row.bg(accent_a(40));
    }
    row
}

fn menu_separator() -> Element {
    Element::leaf()
        .width_match()
        .height(1)
        .margin_xy(8, 3)
        .bg_role(Role::Divider)
}

/// 三级级联菜单的最终展开态：L1 + L2（在激活项右侧）+ L3 并排呈现。
fn context_menu() -> Element {
    // L3：字形风格子菜单。
    let l3 = flat_panel()
        .width(160)
        .padding_xy(0, 6)
        .corner(10.0)
        .child(menu_item("宋", "宋体风格", "", false, false))
        .child(menu_item("黑", "黑体风格", "✓", true, true))
        .child(menu_item("楷", "楷体风格", "", false, false));

    // L2：字符集与字形子菜单（末项「字形风格」激活，右挂 L3）。
    let l2 = flat_panel()
        .width(190)
        .padding_xy(0, 6)
        .corner(10.0)
        .child(menu_item("简", "简体中文", "✓", false, false))
        .child(menu_item("繁", "繁体中文（台湾）", "", false, false))
        .child(menu_item("港", "繁体中文（香港）", "", false, false))
        .child(menu_separator())
        .child(menu_item("字", "字形风格", "›", true, true));

    // L1：主菜单（「字符集与字形」激活，右挂 L2）。
    let l1 = flat_panel()
        .width(220)
        .padding_xy(0, 6)
        .corner(10.0)
        .child(menu_item("⌨", "输入方案", "›", false, false))
        .child(menu_item("文", "字符集与字形", "›", true, true))
        .child(menu_item("⌨", "标点符号", "›", false, false))
        .child(menu_separator())
        .child(menu_item("✓", "模糊音", "", false, true))
        .child(menu_item("✓", "云输入", "", false, true))
        .child(menu_item("○", "双拼模式", "", false, false))
        .child(menu_separator())
        .child(menu_item("😊", "表情与符号", "⌃⌘Space", false, false))
        .child(menu_item("📋", "剪贴板历史", "›", false, false))
        .child(menu_item("🔧", "工具箱", "›", false, false))
        .child(menu_separator())
        .child(menu_item("⚙", "输入法设置…", "", false, false));

    // 级联摆位：三面板横向并排，L2/L3 用顶部 spacer 对齐到各自父级激活项附近。
    Element::row()
        .cross(Align::Start)
        .child(l1)
        .child(Element::col().child(Element::leaf().size(2, 44)).child(l2))
        .child(Element::col().child(Element::leaf().size(2, 200)).child(l3))
}

/// 真实右键菜单项树（图标 + 分隔 + 快捷键 + 三级级联），交框架级联浮层呈现。
fn ime_menu_items() -> Vec<MenuItem> {
    vec![
        MenuItem::submenu(
            "输入方案",
            vec![
                MenuItem::run("全拼", || {}, true).with_icon("拼"),
                MenuItem::run("双拼", || {}, false).with_icon("双"),
                MenuItem::run("五笔", || {}, false).with_icon("笔"),
            ],
        )
        .with_icon("⌨"),
        MenuItem::submenu(
            "字符集与字形",
            vec![
                MenuItem::run("简体中文", || {}, true).with_icon("简"),
                MenuItem::run("繁体中文（台湾）", || {}, false).with_icon("繁"),
                MenuItem::run("繁体中文（香港）", || {}, false).with_icon("港"),
                MenuItem::separator(),
                MenuItem::submenu(
                    "字形风格",
                    vec![
                        MenuItem::run("宋体风格", || {}, false).with_icon("宋"),
                        MenuItem::run("黑体风格", || {}, true).with_icon("黑"),
                        MenuItem::run("楷体风格", || {}, false).with_icon("楷"),
                    ],
                )
                .with_icon("字"),
            ],
        )
        .with_icon("文"),
        MenuItem::submenu(
            "标点符号",
            vec![
                MenuItem::run("中文标点", || {}, true),
                MenuItem::run("英文标点", || {}, false),
            ],
        )
        .with_icon("⌨"),
        MenuItem::separator(),
        MenuItem::run("模糊音", || {}, false).with_icon("✓"),
        MenuItem::run("云输入", || {}, false).with_icon("✓"),
        MenuItem::run("双拼模式", || {}, false).with_icon("○"),
        MenuItem::separator(),
        MenuItem::run("表情与符号", || {}, false)
            .with_icon("😊")
            .with_shortcut("⌃⌘Space"),
        MenuItem::submenu("剪贴板历史", vec![MenuItem::run("清空历史", || {}, false)])
            .with_icon("📋"),
        MenuItem::submenu("工具箱", vec![MenuItem::run("截图取字", || {}, false)]).with_icon("🔧"),
        MenuItem::separator(),
        MenuItem::run("输入法设置…", || {}, false).with_icon("⚙"),
    ]
}

// ============================ 3 · 设置界面 ============================

/// 设置行：左标签（+可选副标题）+ 右控件。
fn setting_row(label: &str, sub: Option<&str>, control: Element) -> Element {
    let mut left = Element::col().weight(1.0).spacing(2).child(
        Element::label(label)
            .font_size(13.0)
            .font_weight(500)
            .fg_role(Role::Text)
            .height(18),
    );
    if let Some(s) = sub {
        left = left.child(
            Element::label(s)
                .font_size(11.0)
                .fg_role(Role::TextMuted)
                .height(15),
        );
    }
    Element::row()
        .width_match()
        .cross(Align::Center)
        .padding_xy(0, 13)
        .spacing(12)
        .child(left)
        .child(control)
}

fn settings_section_header(title: &str) -> Element {
    Element::label(title)
        .font_size(11.0)
        .font_weight(600)
        .fg_role(Role::TextMuted)
        .height(16)
        .margin_xy(0, 9)
}

/// 主题预览卡（渐变填充 + 选中边框），展示 bg_gradient 能力。
fn theme_swatch(label: &str, g: Gradient, selected: bool) -> Element {
    let mut card = Element::col()
        .weight(1.0)
        .height(54)
        .corner(9.0)
        .padding(6)
        .cross(Align::Start)
        .bg_gradient(g)
        .child(Element::leaf().weight(1.0))
        .child(
            Element::label(label)
                .font_size(11.0)
                .fg(Color::WHITE)
                .height(14),
        );
    if selected {
        card = card.border(hex(ACCENT), 2);
    } else {
        card = card.border_role(Role::Border, 1);
    }
    card
}

#[allow(clippy::too_many_arguments)]
fn build_settings(
    tab: Signal<usize>,
    seg_arrange: Signal<usize>,
    cand_count: Signal<usize>,
    cand_size: Signal<usize>,
    win_corner: Signal<usize>,
    dark: Signal<bool>,
    open_add: Signal<bool>,
    open_export: Signal<bool>,
) -> Element {
    // 基本设置页。
    let general = Element::col()
        .width_match()
        .child(settings_section_header("基本设置"))
        .child(setting_row(
            "默认中文输入",
            Some("启动后自动进入中文模式"),
            Element::switch(signal(true)),
        ))
        .child(Element::divider())
        .child(setting_row(
            "模糊音纠错",
            Some("z/zh、c/ch、s/sh 不区分"),
            Element::switch(signal(false)),
        ))
        .child(Element::divider())
        .child(setting_row(
            "中英混输",
            Some("自动识别英文单词"),
            Element::switch(signal(true)),
        ))
        .child(settings_section_header("候选词"))
        .child(setting_row(
            "每页候选数量",
            None,
            Element::dropdown(vec!["5 个", "9 个"], cand_count).width(110),
        ))
        .child(Element::divider())
        .child(setting_row(
            "智能联想",
            Some("根据上下文优化排序"),
            Element::switch(signal(true)),
        ));

    // 词库页：工具行 + 表格 + 页脚。
    let dict_tool = Element::row()
        .width_match()
        .cross(Align::Center)
        .padding_xy(0, 12)
        .spacing(8)
        .child(
            Element::label("🔍 搜索词条")
                .font_size(12.0)
                .fg_role(Role::TextMuted)
                .height(30)
                .weight(1.0)
                .padding_xy(10, 0)
                .corner(7.0)
                .bg_role(Role::SurfaceAlt)
                .border_role(Role::Border, 1),
        )
        .child(
            Element::button("+ 添加")
                .small()
                .font_size(12.5)
                .accent(hex(ACCENT))
                .on_click(move |_| open_add.set(true)),
        )
        .child(Element::button("导入").small().font_size(12.5).neutral())
        .child(
            Element::button("导出")
                .small()
                .font_size(12.5)
                .neutral()
                .on_click(move |_| open_export.set(true)),
        );

    let dict_head = Element::row()
        .width_match()
        .padding_xy(0, 7)
        .border_role(Role::Border, 1)
        .child(col_label("词条", 1.6, Align::Start))
        .child(col_label("拼音", 1.4, Align::Start))
        .child(col_label("词频", 0.7, Align::End))
        .child(col_label("来源", 0.9, Align::End));
    let rows = [
        ("奥利给", "ao li gei", "1280", "自定义"),
        ("绝绝子", "jue jue zi", "964", "网络"),
        ("yyds", "y y d s", "877", "网络"),
        ("属实", "shu shi", "640", "自定义"),
        ("芭比Q", "ba bi Q", "512", "网络"),
        ("内卷", "nei juan", "433", "自定义"),
    ];
    let mut dict_table = Element::col().width_match().child(dict_head);
    for (w, py, fr, src) in rows {
        dict_table = dict_table.child(
            Element::row()
                .width_match()
                .cross(Align::Center)
                .padding_xy(0, 9)
                .child(
                    Element::label(w)
                        .font_size(13.0)
                        .font_weight(500)
                        .fg_role(Role::Text)
                        .weight(1.6),
                )
                .child(
                    Element::label(py)
                        .font_size(12.0)
                        .fg_role(Role::TextMuted)
                        .weight(1.4),
                )
                .child(
                    Element::label(fr)
                        .font_size(13.0)
                        .fg_role(Role::TextMuted)
                        .text_align(Align::End)
                        .weight(0.7),
                )
                .child(
                    Element::label(src)
                        .font_size(12.0)
                        .fg_role(Role::Accent)
                        .text_align(Align::End)
                        .weight(0.9),
                ),
        );
        dict_table = dict_table.child(Element::divider());
    }
    let dict = Element::col()
        .width_match()
        .child(dict_tool)
        .child(dict_table)
        .child(
            Element::row()
                .width_match()
                .padding_xy(0, 12)
                .child(
                    Element::label("共 1,284 条自定义词")
                        .font_size(11.0)
                        .fg_role(Role::TextMuted)
                        .weight(1.0),
                )
                .child(
                    Element::label("已同步 · 2 分钟前")
                        .font_size(11.0)
                        .fg_role(Role::TextMuted),
                ),
        );

    // 外观页：主题卡（渐变）+ 排列分段 + 字号/圆角下拉 + 毛玻璃开关。
    let dark_grad = Gradient::linear(
        (0.0, 0.0),
        (1.0, 1.0),
        vec![(0.0, hex(0x0F1626)), (1.0, hex(0x24344E))],
    );
    let light_grad = Gradient::linear(
        (0.0, 0.0),
        (1.0, 1.0),
        vec![(0.0, hex(0xFFFFFF)), (1.0, hex(0xE6EBF2))],
    );
    let sys_grad = Gradient::linear(
        (0.0, 0.0),
        (1.0, 0.0),
        vec![
            (0.0, hex(0x1E2A3E)),
            (0.5, hex(0x1E2A3E)),
            (0.5, hex(0xE6EBF2)),
            (1.0, hex(0xE6EBF2)),
        ],
    );
    let is_dark = dark.get();
    let appearance = Element::col()
        .width_match()
        .child(settings_section_header("主题"))
        .child(
            Element::row()
                .width_match()
                .padding_xy(0, 6)
                .spacing(10)
                .child(theme_swatch("深色", dark_grad, is_dark))
                .child(theme_swatch("浅色", light_grad, !is_dark))
                .child(theme_swatch("跟随系统", sys_grad, false)),
        )
        .child(settings_section_header("候选窗口排列"))
        .child(
            Element::col()
                .width_match()
                .padding_xy(0, 4)
                .child(Element::segmented(vec!["横向", "竖向", "网格"], seg_arrange).width_match()),
        )
        .child(setting_row(
            "候选字号",
            None,
            Element::dropdown(vec!["小", "中", "大"], cand_size).width(110),
        ))
        .child(Element::divider())
        .child(setting_row(
            "毛玻璃效果",
            Some("候选窗口背景模糊"),
            Element::switch(signal(true)),
        ))
        .child(Element::divider())
        .child(setting_row(
            "窗口圆角",
            None,
            Element::dropdown(vec!["直角", "圆润", "胶囊"], win_corner).width(110),
        ));

    // 快捷键页。
    let keys = [
        ("中英文切换", "Shift"),
        ("候选翻页", "= / -"),
        ("标点切换", "Ctrl + ."),
        ("简繁切换", "Ctrl + Shift + F"),
        ("表情面板", "Ctrl + ⌘ + Space"),
    ];
    let mut shortcuts = Element::col().width_match().padding_xy(0, 8);
    for (label, key) in keys {
        shortcuts = shortcuts
            .child(setting_row(label, None, key_chip(key)))
            .child(Element::divider());
    }

    // 头部：渐变 logo + 标题 + 版本。
    let logo = Element::stack()
        .size(28, 28)
        .child(
            Element::leaf()
                .fill()
                .corner(7.0)
                .bg_gradient(Gradient::linear(
                    (0.0, 0.0),
                    (1.0, 1.0),
                    vec![(0.0, hex(0x2563EB)), (1.0, hex(0x60A5FA))],
                )),
        )
        .child(
            Element::label("云")
                .fill()
                .font_size(14.0)
                .fg(Color::WHITE)
                .text_align(Align::Center),
        );
    let header = Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(10)
        .padding_xy(0, 16)
        .child(logo)
        .child(
            Element::col()
                .weight(1.0)
                .spacing(1)
                .child(
                    Element::label("云拼输入法")
                        .font_size(15.0)
                        .font_weight(600)
                        .fg_role(Role::Text)
                        .height(20),
                )
                .child(
                    Element::label("版本 3.2.1")
                        .font_size(11.0)
                        .fg_role(Role::TextMuted)
                        .height(14),
                ),
        );

    let tabs = Element::tabs(
        tab,
        vec![
            ("基本", general),
            ("词库", dict),
            ("外观", appearance),
            ("快捷键", shortcuts),
        ],
    );

    // 面板统一配水平内边距：header / 标签条 / 各行作为子节点一律被约束缩进对齐，
    // 无需各自单独 padding 或单独计算标签条位置（容器 padding 约束子控件）。
    panel()
        .width(440)
        .padding_xy(22, 0)
        .child(header)
        .child(Element::divider())
        .child(tabs.width_match().height(420))
}

fn col_label(text: &str, weight: f32, align: Align) -> Element {
    Element::label(text)
        .font_size(11.0)
        .fg_role(Role::TextMuted)
        .text_align(align)
        .weight(weight)
}

fn key_chip(key: &str) -> Element {
    Element::label(key)
        .font_size(11.0)
        .fg_role(Role::Text)
        .height(24)
        .padding_xy(8, 0)
        .corner(5.0)
        .bg_role(Role::SurfaceAlt)
        .border_role(Role::Border, 1)
}

// ============================ 对话框 ============================

fn dialog_frame(title: &str, body: Element, footer: Element) -> Element {
    Element::col()
        .width(320)
        .bg_role(Role::Surface)
        .border_role(Role::Border, 1)
        .corner(12.0)
        .shadow(Shadow::new(0.0, 16.0, 40.0, Color::rgba(0, 0, 0, 130)))
        .child(
            Element::label(title)
                .font_size(14.0)
                .font_weight(600)
                .fg_role(Role::Text)
                .height(22)
                .width_match()
                .padding_xy(16, 14),
        )
        .child(Element::divider())
        .child(body)
        .child(Element::divider())
        .child(footer)
}

fn main() {
    let start_dark = !std::env::args().any(|a| a == "--light");
    let dark = signal(start_dark);

    // 可选 `--tab N` 指定初始标签页（截图各页用）。
    let tab_init = std::env::args()
        .skip_while(|a| a != "--tab")
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    let tab = signal(tab_init);
    let seg_arrange = signal(0usize);
    let cand_count = signal(1usize);
    let cand_size = signal(1usize);
    let win_corner = signal(1usize);
    let open_add = signal(std::env::args().any(|a| a == "--add"));
    let open_export = signal(false);

    let settings = build_settings(
        tab,
        seg_arrange,
        cand_count,
        cand_size,
        win_corner,
        dark,
        open_add,
        open_export,
    );

    // 页面：标题 + 主题切换 + 三组件分节。
    let mut app = App::new("中文输入法界面 · 复刻", 960, 820).theme(theme_for(start_dark));
    let th = app.theme_handle();

    let th_d = th.clone();
    let d_d = dark;
    let th_l = th.clone();
    let d_l = dark;
    let header_bar = Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(10)
        .child(
            Element::label("中文输入法界面复刻")
                .font_size(22.0)
                .font_weight(600)
                .fg_role(Role::Text)
                .height(30)
                .weight(1.0),
        )
        .child(Element::button("暗色").neutral().on_click(move |_| {
            d_d.set(true);
            th_d.set(dark_theme());
        }))
        .child(Element::button("亮色").neutral().on_click(move |_| {
            d_l.set(false);
            th_l.set(light_theme());
        }));

    // `--only N`（0=工具栏 1=菜单 2=设置）单独渲染一个组件，便于逐组件截图验证。
    let only = std::env::args()
        .skip_while(|a| a != "--only")
        .nth(1)
        .and_then(|s| s.parse::<usize>().ok());
    let inner = match only {
        Some(0) => Element::col()
            .width_match()
            .padding(28)
            .child(section("1 · 状态工具栏", toolbar())),
        Some(1) => Element::col().width_match().padding(28).child(section(
            "2 · 右键菜单（右击区域弹出真实级联菜单 · 下方为最终态预览）",
            context_menu().on_context_menu(ime_menu_items),
        )),
        Some(2) => Element::col()
            .width_match()
            .padding(28)
            .child(section("3 · 设置界面（多标签 · 可切换）", settings)),
        _ => Element::col()
            .width_match()
            .padding(28)
            .spacing(28)
            .child(header_bar)
            .child(section("1 · 状态工具栏", toolbar()))
            .child(section(
                "2 · 右键菜单（右击区域弹出真实级联菜单 · 下方为最终态预览）",
                context_menu().on_context_menu(ime_menu_items),
            ))
            .child(section("3 · 设置界面（多标签 · 可切换）", settings)),
    };
    let page = Element::scroll().fill().child(inner);

    // 对话框：添加用户词 / 导出词库。
    let close_add = open_add;
    let close_add2 = open_add;
    let add_dialog = Element::dialog(
        open_add,
        dialog_frame(
            "添加用户词",
            Element::col()
                .width_match()
                .padding(16)
                .spacing(12)
                .child(field("词条", "奥利给", true))
                .child(field("自定义编码 · 可选", "aolg", false))
                .child(setting_row(
                    "设为高频词",
                    None,
                    Element::switch(signal(true)),
                )),
            Element::row()
                .width_match()
                .cross(Align::Center)
                .padding_xy(16, 12)
                .spacing(8)
                .child(Element::leaf().weight(1.0))
                .child(
                    Element::button("取消")
                        .neutral()
                        .on_click(move |_| close_add.set(false)),
                )
                .child(Element::button("保存").on_click(move |_| close_add2.set(false))),
        ),
    );

    let close_exp = open_export;
    let close_exp2 = open_export;
    let export_dialog = Element::dialog(
        open_export,
        dialog_frame(
            "导出词库",
            Element::col()
                .width_match()
                .padding(10)
                .child(export_row("自定义词（1,284）", true))
                .child(export_row("网络流行词（318）", true))
                .child(export_row("系统词库（只读）", false)),
            Element::row()
                .width_match()
                .cross(Align::Center)
                .padding_xy(16, 12)
                .spacing(8)
                .child(Element::leaf().weight(1.0))
                .child(
                    Element::button("取消")
                        .neutral()
                        .on_click(move |_| close_exp.set(false)),
                )
                .child(Element::button("导出为 .txt").on_click(move |_| close_exp2.set(false))),
        ),
    );

    let ui = Element::stack()
        .fill()
        .bg_role(Role::Bg)
        .child(Element::col().fill().child(header_bar_wrap(page)))
        .child(add_dialog)
        .child(export_dialog);

    app.bg(theme_for(start_dark).palette.bg)
        .screenshot_from_args()
        .content(ui)
        .run();
}

/// 包一层使 scroll 填满（与综合示例同模式）。
fn header_bar_wrap(page: Element) -> Element {
    Element::col().fill().child(page.weight(1.0))
}

/// 对话框输入框（聚焦态用强调边框近似）。
fn field(label: &str, value: &str, focused: bool) -> Element {
    let mut input = Element::label(value)
        .font_size(13.0)
        .fg_role(Role::Text)
        .height(34)
        .width_match()
        .padding_xy(10, 0)
        .corner(7.0)
        .bg_role(Role::SurfaceAlt);
    if focused {
        input = input.border(hex(ACCENT), 1);
    } else {
        input = input.border_role(Role::Border, 1);
    }
    Element::col()
        .width_match()
        .spacing(5)
        .child(
            Element::label(label)
                .font_size(11.0)
                .fg_role(Role::TextMuted)
                .height(15),
        )
        .child(input)
}

/// 导出选择行（勾选框 + 标签）。
fn export_row(label: &str, checked: bool) -> Element {
    let box_ = if checked {
        Element::label("✓")
            .font_size(11.0)
            .fg(Color::WHITE)
            .text_align(Align::Center)
            .size(16, 16)
            .corner(4.0)
            .bg(hex(ACCENT))
    } else {
        Element::leaf()
            .size(16, 16)
            .corner(4.0)
            .border_role(Role::Border, 2)
    };
    Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(10)
        .padding_xy(12, 9)
        .child(box_)
        .child(
            Element::label(label)
                .font_size(13.0)
                .fg_role(if checked { Role::Text } else { Role::TextMuted })
                .weight(1.0),
        )
}
