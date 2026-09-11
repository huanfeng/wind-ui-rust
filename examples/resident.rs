//! 零窗口常驻示例：启动后桌面上**什么都没有**，只有一个托盘图标与一个全局热键。
//!
//! 运行：cargo run --release --example resident
//! - 托盘右键菜单：「打开窗口」按需建窗、「退出」结束进程（零窗口时唯一的出口）。
//! - 左键单击托盘图标、或按 Ctrl+Alt+W：同样开出那个窗口。
//! - **关掉窗口应用不退出**，托盘图标仍在——这正是常驻模式与 `hide_on_close` 的分界：
//!   那一套留着一个隐藏的主窗（后备缓冲按物理像素一直挂着），这里窗口是真的销毁了，
//!   渲染资源随 `WindowState` 的 drop 归还，内存回到"只有托盘"的水平。
//!
//! 与 `examples/tray.rs` 对比着看：那个是「有主窗 + 关闭转隐藏」的常驻写法。

use windui::prelude::*;

/// 按需建出的那个窗口。
///
/// 每次都重新构建：常驻模式下窗口关掉即销毁，下一次是一棵新树、一批新信号
/// （内容闭包在 `SignalScope` 里跑，随窗口一起回收）。要让重复点击复用同一个窗口而不是
/// 越开越多，靠的是 `single("main")` —— 已经开着就把它激活到前台。
fn main_window() -> WindowRequest {
    Window::new("windui — 常驻", 460, 260)
        .single("main")
        .centered(true)
        .content(|| {
            Element::col()
                .fill()
                .bg(Color::hex(0xFFFFFF))
                .padding(24)
                .spacing(10)
                .child(
                    Element::label("零窗口常驻")
                        .font_size(22.0)
                        .fg(Color::hex(0x2D3436))
                        .height(30)
                        .width_match(),
                )
                .child(
                    Element::label(
                        "关掉这个窗口，进程不会退出：托盘图标还在，界面随之销毁。\n\
                         再从托盘（或 Ctrl+Alt+W）打开时，是新建的一棵控件树。",
                    )
                    .font_size(13.0)
                    .fg(Color::hex(0x636E72))
                    .width_match()
                    .weight(1.0),
                )
        })
}

fn main() {
    let tray = Tray::new()
        .tooltip("windui 常驻示例 — Ctrl+Alt+W 打开窗口")
        .icon_rgba(32, 32, brand_icon_at(32).rgba())
        // 左键直接开窗：常驻模式下没有窗口可 `show_window`，唤起就是"建一个出来"。
        .on_left_click(|ctx| ctx.open_window(main_window()))
        .menu(vec![
            TrayMenuItem::item("打开窗口", |ctx| ctx.open_window(main_window())),
            TrayMenuItem::separator(),
            TrayMenuItem::item("退出", |ctx| ctx.quit()),
        ]);

    App::resident("windui — 常驻")
        .icon(brand_icon())
        .tray(tray)
        .hotkey(Hotkey::new(Key::Char('W')).ctrl().alt(), |ctx| {
            ctx.open_window(main_window())
        })
        .run_resident();
}
