//! 全屏测试 Demo：验证 WindowOp::ToggleFullscreen 四平台实现。
//!
//! 运行：cargo run --example fullscreen_demo
//! 截屏：cargo run --example fullscreen_demo -- --screenshot artifacts/fullscreen.png
//!
//! 功能：
//! - 点击「切换全屏」按钮：进入/退出全屏
//! - 按 Esc：退出全屏（仅在全屏时）
//! - 显示当前窗口状态（全屏、最大化、普通）
//!
//! 真机测试清单：
//! 1. Windows：进入后窗口覆盖整个显示器，标题栏与任务栏隐藏；退出后恢复原位置和样式
//! 2. macOS：进入后进入独立 Space，退出后回到原 Space
//! 3. X11：进入后窗口无边框覆盖所在显示器；退出后恢复
//! 4. Wayland：进入后由合成器全屏；退出后恢复

use std::time::Duration;
use windui::prelude::*;

fn main() {
    let state_text = signal(String::from("当前状态：普通"));
    let hint_text = signal(String::from(""));

    let ui = Element::col()
        .fill()
        .padding(30)
        .spacing(20)
        .bg(Color::hex(0x1E1E2E))
        .child(
            Element::label("全屏测试 Demo")
                .font_size(24.0)
                .fg(Color::hex(0xCDD6F4))
                .width_match(),
        )
        .child(
            Element::col()
                .width_match()
                .padding(20)
                .spacing(12)
                .bg_role(Role::SurfaceAlt)
                .border_role(Role::Border, 1)
                .child(
                    Element::label("操作说明")
                        .font_size(15.0)
                        .fg_role(Role::Accent),
                )
                .child(
                    Element::label("• 点击「切换全屏」按钮进入/退出全屏")
                        .fg_role(Role::TextMuted)
                        .width_match(),
                )
                .child(
                    Element::label("• 全屏时按 Esc 退出全屏")
                        .fg_role(Role::TextMuted)
                        .width_match(),
                )
                .child(
                    Element::label("• 进入后应覆盖整个显示器，隐藏标题栏与任务栏")
                        .fg_role(Role::TextMuted)
                        .width_match(),
                )
                .child(
                    Element::label("• 退出后应恢复进入前的窗口位置与样式")
                        .fg_role(Role::TextMuted)
                        .width_match(),
                ),
        )
        .child(
            Element::row()
                .width_match()
                .spacing(12)
                .cross(Align::Center)
                .child(
                    Element::button("切换全屏")
                        .on_click(move |ctx| {
                            ctx.toggle_fullscreen();
                            hint_text.set("已发送全屏切换请求".to_string());
                        })
                        .font_size(14.0),
                )
                .child(
                    Element::button("最大化")
                        .on_click(move |ctx| {
                            ctx.maximize();
                            hint_text.set("已发送最大化请求".to_string());
                        })
                        .font_size(14.0),
                )
                .child(
                    Element::button("还原")
                        .on_click(move |ctx| {
                            ctx.restore();
                            hint_text.set("已发送还原请求".to_string());
                        })
                        .font_size(14.0),
                ),
        )
        .child(
            Element::label_signal(state_text)
                .font_size(14.0)
                .fg_role(Role::TextMuted)
                .width_match(),
        )
        .child(
            Element::label_signal(hint_text)
                .font_size(13.0)
                .fg_role(Role::Accent)
                .width_match(),
        )
        .child(
            Element::col()
                .width_match()
                .padding(16)
                .spacing(8)
                .bg_role(Role::SurfaceAlt)
                .border_role(Role::Border, 1)
                .child(
                    Element::label("平台测试清单")
                        .font_size(15.0)
                        .fg_role(Role::Accent),
                )
                .child(
                    Element::label("Windows：进入后覆盖整个显示器，标题栏与任务栏隐藏；退出后恢复原位置和样式")
                        .fg_role(Role::TextMuted)
                        .width_match(),
                )
                .child(
                    Element::label("macOS：进入后进入独立 Space（顶部动画）；退出后回到原 Space")
                        .fg_role(Role::TextMuted)
                        .width_match(),
                )
                .child(
                    Element::label("X11：进入后窗口无边框覆盖所在显示器；退出后恢复")
                        .fg_role(Role::TextMuted)
                        .width_match(),
                )
                .child(
                    Element::label("Wayland：进入后由合成器全屏；退出后恢复")
                        .fg_role(Role::TextMuted)
                        .width_match(),
                ),
        );

    let mut app = App::new("全屏测试 Demo", 560, 480)
        .icon(brand_icon())
        .bg(Color::hex(0x1E1E2E))
        .on_interval(Duration::from_millis(200), move |ctx| {
            let st = ctx.window_state();
            let status = if st.fullscreen {
                "全屏"
            } else if st.maximized {
                "最大化"
            } else {
                "普通"
            };
            state_text.set(format!(
                "当前状态：{}（maximized={} minimized={} visible={}）",
                status, st.maximized, st.minimized, st.visible
            ));
        })
        .content(ui);

    // Esc 键退出全屏
    app.hotkey(Hotkey::new(Key::Escape), move |ctx| {
        ctx.restore_window();
        hint_text.set("Esc：已发送还原请求".to_string());
    });

    app.run();
}
