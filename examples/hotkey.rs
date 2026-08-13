//! 全局热键 + 启动即隐藏 + 托盘：常驻后台小工具的完整骨架。
//!
//! 运行：`cargo run --release --example hotkey`
//!
//! - 启动**不显示窗口**，只在托盘出现一个图标。
//! - 按 **Ctrl+Alt+D**（任何程序里都行，本窗口无需焦点）唤起窗口并置前。
//! - 按 **Ctrl+Alt+H** 隐藏窗口。
//! - 窗口内可**运行期改绑**唤起热键为 Ctrl+Alt+J / 改回 D、启停热键——
//!   `App::hotkey_handle` 返回的句柄在回调里 `set`/`set_enabled`，立即生效无需重启。
//! - 「切换暗色」演示主题热切换（`ThemeHandle::update` 同样可局部改色/字号）。
//! - **ESC 或点标题栏 × 均隐藏而非退出**（`hide_on_close`）。
//! - 退出只有一条路：托盘右键 → 退出。
//!
//! 热键消息由系统投递到本窗口队列，空闲时仍阻塞在 `GetMessageW`——**零 CPU 占用**。

use windui::prelude::*;

/// 生成 size×size 纯色 RGBA8（演示图标，免捆绑资源）。
fn solid(size: u32, hex: u32) -> Vec<u8> {
    let (r, g, b) = (
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    );
    [r, g, b, 255].repeat((size * size) as usize)
}

fn main() {
    let hits = signal(0u32);
    let hits_text = signal(String::from("热键唤起次数：0"));
    let binding_text = signal(String::from("当前唤起热键：Ctrl+Alt+D"));
    let dark = signal(false);

    let tray = Tray::new()
        .tooltip("windui 全局热键示例")
        .icon_rgba(16, 16, &solid(16, 0x6C5CE7))
        .on_left_click(|ctx| ctx.show_window())
        .menu(vec![
            TrayMenuItem::item("显示窗口", |ctx| ctx.show_window()),
            TrayMenuItem::separator(),
            TrayMenuItem::item("退出", |ctx| ctx.quit()),
        ]);

    let mut app = App::new("全局热键", 420, 300)
        .tray(tray)
        .start_hidden()
        // ESC 与标题栏 × 均隐藏而非退出——常驻工具里「关闭」的意思是「收起来」。
        .hide_on_close()
        .hotkey(Hotkey::new(Key::Char('H')).ctrl().alt(), |ctx| {
            ctx.hide_window()
        });

    // 运行期可改绑的唤起热键：hotkey_handle 返回句柄，克隆进任意控件回调。
    let show_hk = app.hotkey_handle(Hotkey::new(Key::Char('D')).ctrl().alt(), move |ctx| {
        hits.set(hits.get() + 1);
        hits_text.set(format!("热键唤起次数：{}", hits.get()));
        ctx.show_window();
    });
    // 主题运行期句柄：切暗色 / 局部改强调色都走它。
    let th = app.theme_handle();

    let (hk_j, hk_d, hk_off) = (show_hk.clone(), show_hk.clone(), show_hk);
    let (bt1, bt2, bt3) = (binding_text, binding_text, binding_text);
    let (th_dark, th_accent) = (th.clone(), th);

    let ui = Element::col()
        .fill()
        .padding(24)
        .spacing(12)
        .child(
            Element::label("全局热键 · 运行期动态更新")
                .font_size(20.0)
                .fg_role(Role::Text)
                .height(28)
                .width_match(),
        )
        .child(Element::label_signal(binding_text).height(22).width_match())
        .child(Element::label_signal(hits_text).height(22).width_match())
        .child(Element::divider())
        .child(
            Element::row()
                .spacing(8)
                .child(Element::button("改绑 Ctrl+Alt+J").on_click(move |ctx| {
                    hk_j.set(Hotkey::new(Key::Char('J')).ctrl().alt());
                    bt1.set(String::from("当前唤起热键：Ctrl+Alt+J"));
                    ctx.toast_ok("已改绑，无需重启");
                }))
                .child(
                    Element::button("改回 Ctrl+Alt+D")
                        .neutral()
                        .on_click(move |ctx| {
                            hk_d.set(Hotkey::new(Key::Char('D')).ctrl().alt());
                            bt2.set(String::from("当前唤起热键：Ctrl+Alt+D"));
                            ctx.toast_ok("已改回");
                        }),
                )
                .child({
                    let enabled = signal(true);
                    Element::button("启/停热键").outline().on_click(move |ctx| {
                        enabled.set(!enabled.get());
                        hk_off.set_enabled(enabled.get());
                        bt3.set(format!(
                            "当前唤起热键：{}",
                            if enabled.get() {
                                "已启用"
                            } else {
                                "已停用（组合已归还系统）"
                            }
                        ));
                        ctx.toast(if enabled.get() {
                            "已启用"
                        } else {
                            "已停用"
                        });
                    })
                }),
        )
        .child(
            Element::row()
                .spacing(8)
                .child(
                    Element::button("切换暗色/亮色")
                        .neutral()
                        .on_click(move |_| {
                            dark.set(!dark.get());
                            th_dark.set(if dark.get() {
                                Theme::dark()
                            } else {
                                Theme::default()
                            });
                        }),
                )
                .child(Element::button("强调色 → 绿").outline().on_click(move |_| {
                    // 局部动态改主题：只动 accent，其余不变，下一帧全树跟随。
                    th_accent.update(|t| t.palette.accent = Color::hex(0x2E9E5B));
                })),
        )
        .child(Element::divider())
        .child(
            Element::label("Ctrl+Alt+H 隐藏 · ESC/× 收起 · 托盘右键退出")
                .fg_role(Role::TextMuted)
                .height(20)
                .width_match(),
        )
        // 控件回调里请求隐藏：走 EventCtx::hide_window → WindowOp::Hide。
        .child(Element::button("隐藏到托盘").on_click(|ctx| ctx.hide_window()));

    // 截屏走离屏路径，不创建窗口，故与 start_hidden 无冲突。
    app.screenshot_from_args().content(ui).run();
}
