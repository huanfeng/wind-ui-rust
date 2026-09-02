//! 颜色选择器验证：色块触发器 + 锚定下拉面板。
//!
//! 交互窗口：cargo run --example color_picker
//! 截屏：    cargo run --example color_picker -- --screenshot artifacts/color_picker.png --click 190 92
//!          （--click 落在「主题色」那一行的触发器上，捕获展开后的面板）
//!
//! 看点：
//! - 面板走 `Element::popup` 的**锚定浮层**：浮在后面的内容之上、不受祖先裁剪，
//!   点面板外或按 ESC 收起。这与 `Element::dialog` 的模态遮罩是两回事——浮层不压暗
//!   背景、不拦其余交互，只是临时借一小块地方。
//! - 面板与下面那行 HEX 文字绑的是**同一个** `Signal<Color>`，拖任意一条都立刻联动。
//! - 「画笔色」用 `trigger_text(false)`，触发器只剩一枚方色块，适合工具栏。
//! - 底部两个按钮从外部改色，验证外部改动能回流进面板（HSVA 与 HEX 框都会跟上）。

use windui::prelude::*;

fn main() {
    let brand = signal(Color::hex(0x4C8BF5));
    let pen = signal(Color::rgba(0xE0, 0x31, 0x31, 0xCC));

    let form = Element::col()
        .width_match()
        .spacing(4)
        .child(Element::field("主题色", Element::color_picker(brand)))
        .child(Element::field(
            "画笔色",
            Element::color_picker_opts(
                pen,
                ColorPickerOpts::default().trigger_text(false).presets(vec![
                    Color::hex(0x000000),
                    Color::hex(0xE03131),
                    Color::hex(0x1971C2),
                    Color::hex(0x2F9E44),
                    Color::hex(0xF5A524),
                    Color::hex(0x6741D9),
                ]),
            ),
        ));

    // 绑同一个信号的只读回显：拖面板时它实时跟着变，证明绑定是活的。
    let readout = Element::label_signal(brand.map(|c| {
        format!(
            "主题色当前值：{}（拖面板可见此处实时变化）",
            c.to_hex_string()
        )
    }))
    .font_size(13.0)
    .fg_role(Role::TextMuted)
    .width_match()
    .height(22);

    // 外部改色：面板里的色相游标与 HEX 框都应立刻跟上。
    let external = Element::row()
        .width_match()
        .height(40)
        .spacing(10)
        .child(
            Element::button("外部设为品牌蓝")
                .outline()
                .on_click(move |_| brand.set(Color::hex(0x1971C2))),
        )
        .child(
            Element::button("外部设为纯黑")
                .outline()
                .on_click(move |_| brand.set(Color::hex(0x000000))),
        );

    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(14)
        .bg_role(Role::Bg)
        .child(
            Element::label("颜色选择器")
                .font_size(18.0)
                .font_weight(600)
                .fg_role(Role::Text)
                .width_match()
                .height(28),
        )
        .child(form)
        .child(readout)
        .child(external)
        // 故意在下方压一块实底：面板展开后必须盖在它之上，
        // 这正是锚定浮层与普通子节点的分水岭。
        .child(
            Element::col()
                .width_match()
                .weight(1.0)
                .bg_role(Role::Surface)
                .corner(10.0)
                .padding(16)
                .child(
                    Element::label(
                        "这块内容排在取色器后面。面板展开时应当整块盖住它——\
                         若换成普通子节点，它会反过来把面板盖掉。",
                    )
                    .font_size(13.0)
                    .fg_role(Role::TextSubtle)
                    .width_match()
                    .max_lines(2),
                ),
        );

    App::new("颜色选择器", 560, 520)
        .icon(brand_icon())
        .screenshot_from_args()
        .content(ui)
        .run();
}
