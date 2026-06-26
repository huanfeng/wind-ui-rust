//! 轻提示 Toast 验证：居中浮层 + 语义图标 + 淡入淡出 + 定时消失。
//!
//! 交互窗口：cargo run --example toast
//! 截屏：    cargo run --example toast -- --screenshot artifacts/toast.png --click 79 92
//!          （--click 落在「成功提示」按钮上，捕获淡入完成后的提示浮层）
//!
//! 任意控件回调内 `ctx.toast* ` 即可弹出，无需绑定节点——浮层由宿主接管渲染与计时。

use windui::prelude::*;

fn main() {
    let buttons = Element::row()
        .width_match()
        .height(48)
        .spacing(12)
        .child(
            Element::button("成功提示")
                .width(110)
                .height(40)
                .on_click(|ctx| ctx.toast_ok("已添加到剪贴板")),
        )
        .child(
            Element::button("普通提示")
                .neutral()
                .width(110)
                .height(40)
                .on_click(|ctx| ctx.toast("已保存设置")),
        )
        .child(
            Element::button("错误提示")
                .danger()
                .width(110)
                .height(40)
                .on_click(|ctx| ctx.toast_err("操作失败，请重试")),
        );

    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(16)
        .bg(Color::hex(0xF3F3F3))
        .child(
            Element::label("点击按钮弹出轻提示（Toast）")
                .font_size(18.0)
                .font_weight(600)
                .fg(Color::hex(0x2D3436))
                .width_match()
                .height(28),
        )
        .child(buttons)
        .child(
            Element::label(
                "Toast 居中显示、自动淡入淡出并定时消失；支持 Info / Success / Error 三种语义。",
            )
            .font_size(13.0)
            .fg(Color::hex(0x636E72))
            .width_match()
            .height(40),
        );

    App::new("Toast — 轻提示", 480, 300)
        .bg(Color::hex(0xF3F3F3))
        .screenshot_from_args()
        .content(ui)
        .run();
}
