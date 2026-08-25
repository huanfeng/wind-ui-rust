//! 标签条风格演示：下划线式（默认）与胶囊式并排对比。
//!
//! 窗口运行（可点击切换、观察选中滑块的横向滑动动画）：
//!   cargo run --release --example tabs_pill
//!
//! 离屏截图（静态首帧）：
//!   cargo run --release --example tabs_pill -- --screenshot artifacts/tabs_pill.png

use windui::prelude::*;

/// 一个演示内容页：标题 + 说明。
fn page(title: &str, note: &str) -> Element {
    Element::col()
        .fill()
        .padding(16)
        .spacing(8)
        .child(Element::label(title.to_string()).font_size(16.0))
        .child(
            Element::label(note.to_string())
                .font_size(13.0)
                .fg(Color::hex(0x636E72)),
        )
}

fn main() {
    // 两组各自独立的选中信号（互不影响，方便对照）。
    let underline_sel = signal(0usize);
    let pill_sel = signal(0usize);

    let underline = Element::tabs(
        underline_sel,
        vec![
            (
                "列表",
                page("下划线式", "选中项整格宽下划线，无圆角；底部有贯穿基线。"),
            ),
            ("关于", page("关于", "点其它标签，看下划线横向滑过去。")),
            (
                "设置",
                page("设置", "默认风格：无选中框、无悬停高亮，干净。"),
            ),
        ],
    );

    let pill = Element::tabs_pill(
        pill_sel,
        vec![
            (
                "列表",
                page("胶囊式", "选中项是 accent 实底圆角胶囊、白字；无基线。"),
            ),
            ("关于", page("关于", "点其它标签，看胶囊滑过去。")),
            ("设置", page("设置", "适合选项少（2~4 个）的场景。")),
        ],
    );

    let ui = Element::col()
        .fill()
        .bg(Color::hex(0xF3F3F3))
        .padding(20)
        .spacing(20)
        .child(
            Element::label("下划线式（默认）".to_string())
                .font_size(13.0)
                .fg(Color::hex(0x8A94A6)),
        )
        .child(underline.height(150))
        .child(Element::divider())
        .child(
            Element::label("胶囊式（tabs_pill）".to_string())
                .font_size(13.0)
                .fg(Color::hex(0x8A94A6)),
        )
        .child(pill.height(150));

    App::new("标签条风格演示", 540, 460)
        .icon(brand_icon())
        .bg(Color::hex(0xF3F3F3))
        .screenshot_from_args()
        .content(ui)
        .run();
}
