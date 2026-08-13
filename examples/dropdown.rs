//! 下拉选择 Dropdown 示例。
//!
//! 运行：cargo run --release --example dropdown
//! 闭合截屏：cargo run --example dropdown -- --screenshot artifacts/dropdown.png
//! 展开截屏（纯文本）：cargo run --example dropdown -- --screenshot artifacts/dropdown_open.png --click 120 96
//! 展开截屏（富内容：副标题 + 徽章 + 可点击尾随图标）：
//!   cargo run --example dropdown -- --screenshot artifacts/dropdown_open_rich.png --click 140 245
//! 展开截屏（复选菜单：开关 + 禁用项 + 分隔线 + 动作项）：
//!   cargo run --example dropdown -- --screenshot artifacts/check_menu_open.png --click 120 317
//! 连点截屏（.stay_open()：展开后连点两个开关，菜单不关）：
//!   cargo run --example dropdown -- --screenshot artifacts/check_menu_sticky.png --click 120 397 --click 120 422 --click 120 451

use windui::prelude::*;

fn label(t: &str) -> Element {
    Element::label(t)
        .font_size(13.0)
        .fg_role(Role::TextMuted)
        .height(20)
        .width_match()
}

fn main() {
    let theme = signal(1usize);
    let quality = signal(0usize);
    let plan = signal(0usize);
    let (hide_disabled, show_special, compact) = (signal(true), signal(false), signal(false));
    let (exp_dict, exp_keys, exp_ui) = (signal(true), signal(false), signal(false));

    let plan_items = vec![
        DropdownItem::new("免费版").badge("当前", Intent::Neutral),
        DropdownItem::new("专业版")
            .subtitle("解锁全部导出格式")
            .badge("推荐", Intent::Primary),
        DropdownItem::new("团队版")
            .subtitle("多人协作 + 权限管理")
            .badge("New", Intent::Danger)
            .trailing_icon("🗑", |_ctx| {
                println!("点击了团队版的尾随图标（未选中该项）")
            }),
    ];

    let ui = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .padding(20)
        .spacing(10)
        .child(
            Element::label("下拉选择")
                .font_size(22.0)
                .fg_role(Role::Text)
                .height(30)
                .width_match(),
        )
        .child(label("主题"))
        .child(Element::dropdown(vec!["跟随系统", "浅色", "深色"], theme).width(220))
        .child(label("渲染质量"))
        .child(Element::dropdown(vec!["低", "中", "高", "极致"], quality).width(220))
        .child(label("方案（富内容：副标题 + 徽章 + 可点击尾随图标）"))
        .child(Element::dropdown_items(plan_items, plan).width(260))
        .child(label("列表显示（复选菜单：默认点击即关）"))
        .child(
            Element::check_menu(
                "列表显示",
                vec![
                    CheckMenuItem::check("隐藏未启用", hide_disabled)
                        .on_change(|_ctx, v| println!("隐藏未启用 → {v}")),
                    CheckMenuItem::check("显示特殊方案", show_special),
                    CheckMenuItem::check("紧凑行高", compact).enabled(false),
                    CheckMenuItem::separator(),
                    CheckMenuItem::action("全部展开", |_ctx| {
                        println!("执行「全部展开」并关闭菜单")
                    }),
                ],
            )
            .summary(|on| match on.len() {
                0 => "列表显示".to_string(),
                n => format!("列表显示 ({n})"),
            })
            .width(220),
        )
        .child(label("导出内容（.stay_open()：开关点了不关，可连点）"))
        .child(
            Element::check_menu(
                "导出内容",
                vec![
                    CheckMenuItem::check("用户词库", exp_dict),
                    CheckMenuItem::check("按键配置", exp_keys),
                    CheckMenuItem::check("界面偏好", exp_ui),
                ],
            )
            .stay_open()
            .summary(|on| match on.len() {
                0 => "导出内容".to_string(),
                n => format!("导出内容 ({n})"),
            })
            .width(220),
        );

    App::new("windui — 下拉选择", 320, 540)
        .screenshot_from_args()
        .content(ui)
        .run();
}
