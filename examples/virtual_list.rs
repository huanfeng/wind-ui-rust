//! 虚拟滚动示例：十万行的列表与一万行的表格，滚多远都不变慢。
//!
//! 运行：cargo run --release --example virtual_list
//! 截屏：cargo run --example virtual_list -- --screenshot artifacts/virtual_list.png
//!
//! 交互：
//! - 滚轮 / 拖滚动条 — 两块内容各自独立滚动
//! - 「跳到中间 / 回到顶部」— 直接改数据不动滚动量，演示重建与滚动解耦
//! - 「只留 20 行」— 行数骤减，滚动条与钳制立刻跟上
//! - 表格：末列按钮、双击整行、右键弹菜单 — 行内修饰符与非虚拟表格是同一套
//!
//! 与 [`dyn_list`](dyn_list) 演示的 `list_signal` 是同一件事的两种做法：那边每一行都是
//! 真实节点，几十行以内最省事；这边只构建视口内的那几十行，代价是行高必须固定。
//! **几百行以内用 `list_signal` 就好**，虚拟化的复杂度只在行数真的多起来时才划算。
//!
//! 十万行存的是 `Vec<usize>` 而不是预格式化的 `Vec<String>`：行文本在构建行时才生成。
//! 十万个 String 会平白多占十几 MB，而本库的立身之本正是极低内存。

use windui::prelude::*;

#[path = "common/mod.rs"]
mod common;
use common::{page_title, Shell};

/// 列表行高（逻辑 px）。虚拟滚动要求行高固定——索引与像素偏移靠一次乘法互换。
const ROW_H: i32 = 34;

/// 单行：序号 + 内容 + 右侧数值。行内不放可聚焦控件，故不受"Tab 焦点环只覆盖已渲染行"
/// 这条限制影响（见 `ui::virtual_list` 模块文档的「已知限制」）。
fn row(i: usize, v: usize) -> Element {
    Element::row()
        .width_match()
        .cross(Align::Center)
        .padding_xy(12, 0)
        .spacing(10)
        .child(
            Element::label(format!("{:>6}", i + 1))
                .font_size(12.5)
                .fg_role(Role::TextMuted)
                .width(56),
        )
        .child(
            Element::label(format!("数据行 {} — 内容随索引生成", i + 1))
                .font_size(13.5)
                .fg_role(Role::Text)
                .weight(1.0),
        )
        .child(
            Element::label(format!("{}", v * 7919 % 100_000))
                .font_size(12.5)
                .fg_role(Role::Success)
                .width(64),
        )
}

/// 行内放控件时的行高：`TABLE_ROW_H` 是给**纯文本行**的（单行文本盒 20px），装不下按钮。
/// 行高在虚拟滚动里是硬约束——占位撑高按它算，行也被强制成它，短了内容会溢出到邻行。
/// 这个值由截图核对得来：`.small()` 按钮约 30px + 单元格上下内边距 9×2 + 行下分隔线 1。
const ACTION_ROW_H: i32 = 30 + 9 * 2 + 1;

fn table_rows(n: usize) -> Vec<Vec<String>> {
    (0..n)
        .map(|i| {
            vec![
                format!("file_{i:05}.dat"),
                format!("{}", i * 37 % 9999),
                String::from(match i % 3 {
                    0 => "已同步",
                    1 => "待上传",
                    _ => "冲突",
                }),
            ]
        })
        .collect()
}

fn main() {
    let items = signal((0..100_000usize).collect::<Vec<_>>());
    let table = signal(table_rows(10_000));
    let status = signal(String::from("列表 100000 行 · 表格 10000 行"));

    // 三个按钮都只改数据信号，完全不碰滚动量——重建与滚动位置是两件事。
    let toolbar = Element::row()
        .width_match()
        .height(36)
        .cross(Align::Center)
        .spacing(8)
        .child(Element::button("跳到中间").small().on_click(move |_| {
            items.set((50_000..150_000).collect());
            status.set(String::from("列表已换成 50000..150000 这一段"));
        }))
        .child(
            Element::button("只留 20 行")
                .neutral()
                .outline()
                .small()
                .on_click(move |_| {
                    items.set((0..20).collect());
                    status.set(String::from("行数骤减到 20 — 滚动条与钳制立刻跟上"));
                }),
        )
        .child(
            Element::button("回到十万行")
                .neutral()
                .outline()
                .small()
                .on_click(move |_| {
                    items.set((0..100_000).collect());
                    status.set(String::from("列表 100000 行 · 表格 10000 行"));
                }),
        )
        .child(
            Element::label_signal(status)
                .font_size(12.0)
                .fg_role(Role::TextMuted)
                .weight(1.0),
        );

    let ui = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .padding(18)
        .spacing(10)
        .child(page_title("虚拟滚动", "列表 10 万行 / 表格 1 万行，只构建视口内的行").height(32))
        .child(
            Element::label(
                "两端用空占位撑出未渲染部分的高度 —— 滚动条、\
                 滚动钳制与 scroll_into_view 因此无需任何特殊处理",
            )
            .font_size(12.5)
            .fg_role(Role::TextMuted)
            .width_match(),
        )
        .child(toolbar)
        .child(
            Element::virtual_list(items, ROW_H, row)
                .bg_role(Role::Surface)
                .border_role(Role::Border, 1)
                .corner(6.0)
                .weight(1.0),
        )
        .child(
            Element::label("表格 table_virtual — 末列按钮 / 状态徽章 / 双击整行 / 右键菜单")
                .font_size(12.5)
                .fg_role(Role::TextMuted)
                .width_match(),
        )
        .child(
            Element::table_virtual(
                vec![("名称", 3.0), ("大小(KB)", 1.0), ("状态", 1.2)],
                table,
                // 注意不是 TABLE_ROW_H：行里有按钮，得给够高度（见常量说明）。
                ACTION_ROW_H,
            )
            // 末列按行生成按钮组。row 是**真实行下标**（不是它在渲染窗口里的位置），
            // 滚到哪一行、闭包拿到的就是哪一行——这点与非虚拟表格一致。
            .actions("操作", 2.2, move |row| {
                Element::row()
                    .spacing(6)
                    .child(
                        Element::button("查看")
                            .neutral()
                            .outline()
                            .small()
                            .on_click(move |ctx| ctx.toast(format!("查看第 {} 行", row + 1))),
                    )
                    .child(
                        Element::button("删除")
                            .danger()
                            .outline()
                            .small()
                            .on_click(move |ctx| ctx.toast_err(format!("删除第 {} 行", row + 1))),
                    )
            })
            // 状态列渲染成彩色徽章；返回 None 的列走默认文本。
            .cell_render(|_row, col, text| {
                if col != 2 {
                    return None;
                }
                let role = match text {
                    "已同步" => Role::Success,
                    "冲突" => Role::Danger,
                    _ => Role::TextMuted,
                };
                Some(
                    Element::label(text)
                        .font_size(11.0)
                        .fg_role(role)
                        .padding_xy(6, 2)
                        .corner(4.0)
                        .border_role(role, 1),
                )
            })
            .on_row_activate(|ctx, row| ctx.toast(format!("双击进入第 {} 行", row + 1)))
            .on_row_context_menu(|row| {
                vec![
                    MenuItem::run(
                        "复制名称",
                        move |ctx| ctx.clipboard_set(&format!("file_{row:05}.dat")),
                        false,
                    ),
                    MenuItem::separator(),
                    MenuItem::run(
                        "删除",
                        move |ctx| ctx.toast_err(format!("删除第 {} 行", row + 1)),
                        false,
                    ),
                ]
            })
            .weight(1.0),
        );

    App::new("windui — 虚拟滚动", 680, 700)
        .icon(brand_icon())
        .frameless()
        .screenshot_from_args()
        .content(Shell::new("虚拟滚动").wrap(ui))
        .run();
}
