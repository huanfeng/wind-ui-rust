//! 多窗口示例：主窗口用 `ctx.open_window` 开出设置 / 关于两种子窗。
//!
//! 运行：`cargo run --release --example multi_window`
//! 截屏主窗：`cargo run --example multi_window -- --screenshot artifacts/multi_window.png`
//!
//! 可以验证的几件事：
//! - **开子窗**：点「打开设置…」「关于…」各弹出一个独立窗口（每个窗口有自己的交互
//!   状态，hover/焦点互不干扰）。
//! - **单例窗口**：设置与关于都设了 `.single(key)`，再点一次**不会**开第二个，已经
//!   开着的那个跳到前台（最小化的会还原）。作对照的「便签」没设，点几次开几个。
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
//! 平台支持：Windows 与 macOS 均已实现，行为一致。

use windui::prelude::*;
use windui::style::Role;

fn main() {
    let mut app = App::new("多窗口示例", 520, 380).icon(brand_icon());
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
                            let th = th_child.clone();
                            ctx.open_window(
                                Window::new("设置", 420, 320)
                                    .icon(brand_icon())
                                    .centered(true)
                                    .min_size(320, 260)
                                    // 单例：再点一次不会开第二个，已有的那个跳到前台。
                                    .single("settings")
                                    // content 收闭包：内容在窗口真正创建时才构建，
                                    // 其间创建的 Signal 归这个窗口，关窗即回收。
                                    .content(move || settings_page(name, th, dark)),
                            );
                        }))
                        .child(Element::button("关于…").on_click(|ctx| {
                            ctx.open_window(
                                Window::new("关于", 360, 220)
                                    .icon(brand_icon())
                                    .resizable(false)
                                    .centered(true)
                                    .single("about")
                                    .content(about_page),
                            );
                        }))
                        // 对照组：不设 single，点几次开几个。
                        .child(Element::button("便签（可开多个）").on_click(|ctx| {
                            ctx.open_window(
                                Window::new("便签", 300, 200)
                                    .icon(brand_icon())
                                    .centered(true)
                                    .content(note_page),
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

/// 便签子窗内容：**不设** `single`，用来和上面两个单例窗口作对照——点几次开几个，
/// 每个窗口有自己的文本状态（`content` 闭包在每次建窗时各跑一遍）。
fn note_page() -> Element {
    let text = signal(String::new());
    Element::col()
        .fill()
        .padding(16)
        .spacing(10)
        .child(Element::label("便签").font_size(16.0))
        .child(Element::text_input(text, "随手记点什么…"))
        .child(Element::flex_spacer())
        .child(Element::label("再点按钮会开出新的一个").fg_role(Role::TextMuted))
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
