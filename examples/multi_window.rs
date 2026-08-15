//! 多窗口示例：主窗口用 `ctx.open_window` 开出设置 / 关于两种子窗。
//!
//! 运行：`cargo run --release --example multi_window`
//! 截屏主窗：`cargo run --example multi_window -- --screenshot artifacts/multi_window.png`
//!
//! 可以验证的几件事：
//! - **开子窗**：点「打开设置…」「关于…」各弹出一个独立窗口；重复点会再开一个
//!   （每个窗口有自己的交互状态，hover/焦点互不干扰）。
//! - **关子窗不退应用**：关掉任意子窗，主窗照常在；**关掉最后一个窗口**才退出进程。
//!   主窗也可以先关，留着子窗继续用。
//! - **主题联动**：在任意窗口点「切换暗色/亮色」，所有已打开的窗口一起换肤——
//!   子窗与主窗共享同一个 `ThemeHandle`，不是各拿一份快照定格。
//! - **跨窗共享状态**：设置窗里改「显示名称」，主窗的问候语同步变。`Signal` 是 `Copy`
//!   句柄，传进子窗即可，不需要任何额外的跨窗口通信机制。
//!
//! 子窗只有对自己有意义的配置（标题/尺寸/可缩放/居中/无边框/最小尺寸/背景）。托盘、
//! 全局热键、单实例、渲染后端都是**应用级**的，由 `App` 那次配置决定，子窗自动跟随。
//!
//! 平台支持：Windows 已实现；macOS 尚未实现多窗口，`open_window` 会在 debug 期 panic
//! 提示、release 期打印一行并忽略（同全局热键的处理，见 `platform/macos/hotkey.rs`）。

use windui::prelude::*;
use windui::style::Role;

fn main() {
    let mut app = App::new("多窗口示例", 520, 380);
    let theme = app.theme_handle();
    let dark = signal(false);
    // 跨窗共享的状态：主窗显示、设置窗编辑。
    let name = signal(String::from("世界"));

    let th_main = theme.clone();
    let th_child = theme.clone();

    app.screenshot_from_args()
        .content(
            Element::col()
                .fill()
                .padding(24)
                .spacing(16)
                .child(Element::label("主窗口").font_size(20.0))
                .child(Element::label(name).font_size(16.0))
                .child(
                    Element::label("↑ 在设置窗里改「显示名称」，这行会同步变")
                        .fg_role(Role::TextMuted),
                )
                .child(
                    Element::row()
                        .spacing(12)
                        .child(Element::button("打开设置…").on_click(move |ctx| {
                            ctx.open_window(
                                Window::new("设置", 420, 320)
                                    .centered(true)
                                    .min_size(320, 260)
                                    .content(settings_page(name, th_child.clone(), dark)),
                            );
                        }))
                        .child(Element::button("关于…").on_click(|ctx| {
                            ctx.open_window(
                                Window::new("关于", 360, 220)
                                    .resizable(false)
                                    .centered(true)
                                    .content(about_page()),
                            );
                        })),
                )
                .child(theme_button(th_main, dark))
                .child(Element::flex_spacer())
                .child(Element::label("关掉最后一个窗口才会退出进程").fg_role(Role::TextMuted)),
        )
        .run();
}

/// 换肤按钮。主窗与子窗各放一个，验证任意窗口触发都会让所有窗口一起换。
fn theme_button(theme: ThemeHandle, dark: Signal<bool>) -> Element {
    Element::button("切换暗色/亮色").on_click(move |_| {
        let next = !dark.get();
        dark.set(next);
        theme.set(if next {
            Theme::dark()
        } else {
            Theme::default()
        });
    })
}

/// 设置子窗内容。`name` 是主窗传进来的**同一个**信号句柄——改它，主窗跟着变。
fn settings_page(name: Signal<String>, theme: ThemeHandle, dark: Signal<bool>) -> Element {
    Element::col()
        .fill()
        .padding(20)
        .spacing(14)
        .child(Element::label("设置").font_size(18.0))
        .child(Element::divider())
        .child(Element::field(
            "显示名称",
            Element::text_input(name, "输入点什么…").weight(1.0),
        ))
        .child(theme_button(theme, dark))
        .child(Element::flex_spacer())
        .child(Element::label("独立窗口，关掉它不影响主窗").fg_role(Role::TextMuted))
}

/// 关于子窗内容：固定大小、不可缩放。
fn about_page() -> Element {
    Element::col()
        .fill()
        .padding(20)
        .spacing(10)
        .cross(windui::spec::Align::Center)
        .child(Element::label("windui").font_size(22.0))
        .child(Element::label("多窗口示例的关于窗口"))
        .child(Element::label("固定大小、不可缩放").fg_role(Role::TextMuted))
        .child(Element::flex_spacer())
        .child(Element::button("关闭本窗").on_click(|ctx| ctx.request_close()))
}
