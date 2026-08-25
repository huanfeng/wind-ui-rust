//! 文本光标（caret）风格对比：四种闪烁风格 + 平滑移动 / 圆角 / 宽度开关。
//!
//! 运行：cargo run --release --example caret
//! 截屏：cargo run --example caret -- --screenshot artifacts/caret.png
//!
//! 光标风格是**全局主题**的一部分（`theme.input.caret_*`），所以这里用运行期换主题
//! （`App::theme_handle`）整窗切换，而不是逐个输入框设置——一个应用里的插入符必须处处
//! 一致，逐控件可调只会让不同页面的光标各闪各的。
//!
//! 截图路径下光标恒为实心（见 `ui::caret::set_animated`），故那张图看的是静态外观；
//! 闪烁与滑行请真跑窗口看。

use windui::prelude::*;

const STYLES: [(CaretStyle, &str, &str); 4] = [
    (
        CaretStyle::Blink,
        "Blink",
        "亮/灭各半周期硬切换，与系统插入符一致（默认，最省）",
    ),
    (
        CaretStyle::Smooth,
        "Smooth",
        "缓入缓出地淡入淡出，两端各驻留一小段；每帧都在变，代价最高",
    ),
    (
        CaretStyle::Phase,
        "Phase",
        "alpha 在 1.0↔0.35 间起伏，从不熄灭，识别度最高",
    ),
    (CaretStyle::Solid, "Solid", "静态实心，不闪（零续帧开销）"),
];

/// 按当前选择拼一份主题：只覆盖 `input` 的光标四项，其余留默认。
fn theme_for(style: CaretStyle, smooth_move: bool, rounded: bool, width: Len) -> Theme {
    let mut t = Theme::default();
    t.palette.bg = Color::hex(0xF5F7FA);
    t.input.caret_style = Some(style);
    t.input.caret_smooth_move = Some(smooth_move);
    t.input.caret_rounded = Some(rounded);
    t.input.caret_width = Some(width);
    t
}

fn main() {
    let single = signal(String::from(
        "单行：点击定位、Home/End、拖选都会重置闪烁相位",
    ));
    let multi = signal(String::from(
        "多行：换行时光标瞬移，同一行内左右移动才滑行。\n打字期间光标保持实心，停手 0.5 秒后才开始闪。",
    ));
    let style_idx = signal(0usize);
    let smooth_move = signal(true);
    let rounded = signal(true);
    let wide = signal(true);
    let phys = signal(false);
    let desc = signal(String::from(STYLES[0].2));

    let mut app = App::new("windui — 光标风格", 560, 470)
        .icon(brand_icon())
        .theme(theme_for(CaretStyle::Blink, true, true, Len::Dp(2.0)));
    let handle = app.theme_handle();

    // 四个开关共用一条应用路径：任一变化都重算整份主题再灌进去。
    let apply = move |h: &ThemeHandle| {
        let (style, _, text) = STYLES[style_idx.get().min(STYLES.len() - 1)];
        // Dp 随 DPI 等比放大，Physical 恒为固定物理像素；两者都会向下吸附到整数
        // 物理像素，所以 125%/150% 下都不糊——差别只在"要不要随 DPI 变粗"。
        let w = if wide.get() { 2.0 } else { 1.0 };
        h.set(theme_for(
            style,
            smooth_move.get(),
            rounded.get(),
            if phys.get() {
                Len::Physical { px: w }
            } else {
                Len::Dp(w)
            },
        ));
        desc.set(String::from(text));
    };

    // 风格选择：段控件没有变更回调，故用按钮各自 set 索引后重灌主题。
    let mut picker = Element::row().width_match().spacing(8);
    for (i, (_, name, _)) in STYLES.iter().enumerate() {
        let h = handle.clone();
        picker = picker.child(
            Element::button(*name)
                .outline()
                .neutral()
                .weight(1.0)
                .on_click(move |_| {
                    style_idx.set(i);
                    apply(&h);
                }),
        );
    }

    // `on_toggle` 是受控点击：勾选框不再自动翻转，由回调决定——正好用来「翻转 + 重灌主题」。
    let toggle = |label: &str, flag: Signal<bool>, h: ThemeHandle| {
        Element::checkbox(label, flag).on_toggle(move |_| {
            flag.set(!flag.get());
            apply(&h);
        })
    };

    let ui = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .child(
            Element::label("光标风格")
                .font_size(17.0)
                .fg(Color::hex(0x1A2035)),
        )
        .child(picker)
        .child(
            Element::label_signal(desc)
                .font_size(12.0)
                .fg(Color::hex(0x666680)),
        )
        .child(
            Element::col()
                .width_match()
                .bg(Color::hex(0xFFFFFF))
                .corner(10.0)
                .padding(16)
                .spacing(10)
                .child(Element::text_input(single, "单行…").width_match())
                .child(
                    Element::text_input(multi, "多行…")
                        .multiline()
                        .width_match()
                        .height(120),
                ),
        )
        .child(
            Element::row()
                .width_match()
                .spacing(16)
                .child(toggle("平滑移动", smooth_move, handle.clone()))
                .child(toggle("圆角端", rounded, handle.clone()))
                .child(toggle("2px 宽", wide, handle.clone()))
                .child(toggle("物理像素", phys, handle.clone())),
        )
        .child(
            Element::label(
                "闪烁周期跟随系统「插入符」设置；只重绘光标那一条，静止界面回到零 CPU 空闲。",
            )
            .font_size(11.0)
            .fg(Color::hex(0x8A8AA0)),
        );

    app.content(ui).screenshot_from_args().run();
}
