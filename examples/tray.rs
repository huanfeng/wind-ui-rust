//! 系统托盘示例：托盘图标 + 提示 + 左键显示/双击 + 原生右键菜单（勾选项 + 分隔线 + 气泡）。
//!
//! 运行：cargo run --release --example tray
//! - 左键单击托盘图标：显示并前置窗口；双击同。
//! - 右键托盘图标：弹原生菜单（显示/隐藏、启用通知[勾选]、弹气泡[通知关时灰显]、退出）。
//! - 关闭窗口即退出（托盘图标随之清理）。

use std::cell::Cell;
use std::rc::Rc;

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
    // TrayMenuItem::check 的 checked 参数仍为 Rc<Cell<bool>>（驱动菜单对勾显示）。
    let notify_on = Rc::new(Cell::new(true));
    let n2 = notify_on.clone();
    // TrayMenuItem::enabled 参数自 0.4.1 起改为 Signal<bool>（驱动菜单项灰显）。
    let notify_sig = signal(true);

    let tray = Tray::new()
        .tooltip("windui 托盘示例")
        .icon_rgba(16, 16, &solid(16, 0x4C8BF5))
        .on_left_click(|ctx| ctx.show_window())
        .on_double_click(|ctx| ctx.show_window())
        .menu(vec![
            TrayMenuItem::item("显示窗口", |ctx| ctx.show_window()),
            TrayMenuItem::item("隐藏到托盘", |ctx| ctx.hide_window()),
            TrayMenuItem::separator(),
            // 勾选项：菜单弹出时按 notify_on 当前值显示对勾；点击时同步翻转 Rc 与 Signal。
            TrayMenuItem::check("启用通知", notify_on.clone(), move |ctx| {
                let next = !n2.get();
                n2.set(next);
                notify_sig.set(next);
                if next {
                    ctx.notify("通知已开启", "右键菜单可再次切换");
                }
            }),
            // 禁用态演示：通知关闭时该项灰显不可点（enabled 绑定 notify_sig，弹出时读当前值）。
            TrayMenuItem::item("弹个气泡", |ctx| {
                ctx.notify("你好", "这是来自托盘的气泡通知")
            })
            .enabled(notify_sig),
            TrayMenuItem::separator(),
            TrayMenuItem::item("退出", |ctx| ctx.quit()),
        ]);

    let ui = Element::col()
        .fill()
        .bg(Color::hex(0xFFFFFF))
        .padding(24)
        .spacing(10)
        .child(
            Element::label("系统托盘")
                .font_size(22.0)
                .fg(Color::hex(0x2D3436))
                .height(30)
                .width_match(),
        )
        .child(
            Element::label("左键托盘图标显示窗口；右键弹原生菜单（含勾选项、分隔线、气泡通知）。")
                .font_size(13.0)
                .fg(Color::hex(0x636E72))
                .width_match()
                .weight(1.0),
        );

    App::new("windui — 托盘", 420, 240)
        .bg(Color::hex(0xFFFFFF))
        .tray(tray)
        .content(ui)
        .run();
}
