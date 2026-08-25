//! 轻提示 Toast 验证：顶部居中堆叠 + 语义图标 + 悬停暂停 + 手动关闭 + 右键复制。
//!
//! 交互窗口：cargo run --example toast
//! 截屏：    cargo run --example toast -- --screenshot artifacts/toast.png --click 79 92
//!          （--click 落在「成功提示」按钮上，捕获淡入完成后的提示浮层）
//!
//! 任意控件回调内 `ctx.toast* ` 即可弹出，无需绑定节点——浮层由宿主接管渲染与计时。
//! 连点多次可见多条 toast 顶部居中垂直堆叠（上限 4 条，超出丢最旧）；
//! 鼠标悬停在某条上会暂停其倒计时；点击每条右侧 ✕ 可手动关闭；右键可复制文字内容。

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
                "顶部居中堆叠展示，最多 4 条；悬停暂停倒计时，✕ 手动关闭，右键复制内容。连点按钮可观察堆叠效果。",
            )
            .font_size(13.0)
            .fg(Color::hex(0x636E72))
            .width_match()
            .height(40),
        );

    App::new("Toast — 轻提示", 480, 300)
        .icon(brand_icon())
        .bg(Color::hex(0xF3F3F3))
        .screenshot_from_args()
        .content(ui)
        .run();
}
