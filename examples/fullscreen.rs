//! 全屏切换示例：`ctx.toggle_fullscreen()` / `ctx.restore()` 与 `window_state().fullscreen`。
//!
//! 运行：cargo run --release --example fullscreen
//!
//! 真窗口核对清单（截图路径验不到窗口级行为）：
//! - Windows：进入后覆盖整个显示器、标题栏与任务栏隐藏；退出回到进入前的位置与大小，
//!   **最大化着进入的，退出后仍是最大化**。
//! - macOS：进入后切到独立 Space（有系统动画），退出回到原 Space。
//! - X11 / Wayland：进入后由窗口管理器 / 合成器铺满所在显示器，退出恢复。
//! - 「还原」在全屏中等价于退出全屏。

use std::time::Duration;
use windui::prelude::*;

fn main() {
    let status = signal(String::from("当前状态：普通"));

    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(12)
        .child(Element::label("全屏切换").font_size(22.0).width_match())
        .child(
            Element::label("全屏覆盖所在显示器；退出后回到进入前的位置、大小与最大化状态。")
                .fg_role(Role::TextMuted)
                .width_match(),
        )
        .child(
            Element::row()
                .spacing(8)
                .child(Element::button("切换全屏").on_click(|ctx| ctx.toggle_fullscreen()))
                .child(
                    Element::button("最大化")
                        .outline()
                        .on_click(|ctx| ctx.maximize()),
                )
                .child(
                    Element::button("还原")
                        .outline()
                        .on_click(|ctx| ctx.restore()),
                ),
        )
        .child(
            Element::label_signal(status)
                .fg_role(Role::Accent)
                .width_match(),
        );

    App::new("全屏切换", 520, 260)
        // 状态快照只在事件回调里有效，借定时器周期性刷新显示（全屏也可能由系统快捷键进入）。
        .on_interval(Duration::from_millis(200), move |ctx| {
            let st = ctx.window_state();
            let mode = if st.fullscreen {
                "全屏"
            } else if st.maximized {
                "最大化"
            } else {
                "普通"
            };
            status.set(format!(
                "当前状态：{mode}（fullscreen={} maximized={}）",
                st.fullscreen, st.maximized
            ));
        })
        .content(ui)
        .screenshot_from_args()
        .run();
}
