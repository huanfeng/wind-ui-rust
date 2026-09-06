//! 菜单栏 MenuBar 示例：原生手感的应用菜单。
//!
//! 运行：cargo run --release --example menubar
//! 展开截屏：cargo run --example menubar -- --screenshot artifacts/menubar_open.png --click 30 14
//! 滑到相邻标题切换：cargo run --example menubar -- --screenshot artifacts/menubar_switch.png --click 30 14 --hover 100 14
//! F10 激活 + → + ↓：cargo run --example menubar -- --screenshot artifacts/menubar_kbd.png --key F10 --key Right --key Down
//! 单击 Alt：cargo run --example menubar -- --screenshot artifacts/menubar_alt.png --key alt
//! Alt+E 直开「编辑」：cargo run --example menubar -- --screenshot artifacts/menubar_alt_e.png --key alt+e

use windui::prelude::*;

fn main() {
    let last = signal(String::from("（还没有执行过菜单项）"));
    let word_wrap = signal(true);
    let status_bar = signal(false);

    let file = {
        move || {
            let act = move |name: &'static str| {
                move |ctx: &mut windui::core::EventCtx| {
                    last.set(format!("执行了「{name}」"));
                    ctx.toast(name);
                }
            };
            vec![
                MenuItem::run("新建(N)", act("新建"), false)
                    .shortcut("Ctrl+N")
                    .mnemonic('N'),
                MenuItem::run("打开(O)…", act("打开"), false)
                    .shortcut("Ctrl+O")
                    .mnemonic('O'),
                MenuItem::submenu(
                    "最近打开(R)",
                    vec![
                        MenuItem::run("设计稿.md", act("设计稿.md"), false),
                        MenuItem::run("笔记.txt", act("笔记.txt"), false),
                    ],
                )
                .mnemonic('R'),
                MenuItem::separator(),
                MenuItem::run("保存(S)", act("保存"), false)
                    .shortcut("Ctrl+S")
                    .mnemonic('S'),
                MenuItem::run("另存为(A)…", act("另存为"), false)
                    .enabled(false)
                    .mnemonic('A'),
                MenuItem::separator(),
                MenuItem::run("退出(X)", |ctx| ctx.request_close(), false)
                    .shortcut("Alt+F4")
                    .mnemonic('X'),
            ]
        }
    };
    let edit = {
        move || {
            let act = move |name: &'static str| {
                move |_ctx: &mut windui::core::EventCtx| last.set(format!("执行了「{name}」"))
            };
            vec![
                MenuItem::run("撤销(U)", act("撤销"), false)
                    .shortcut("Ctrl+Z")
                    .mnemonic('U'),
                MenuItem::separator(),
                MenuItem::run("剪切(T)", act("剪切"), false)
                    .shortcut("Ctrl+X")
                    .mnemonic('T'),
                MenuItem::run("复制(C)", act("复制"), false)
                    .shortcut("Ctrl+C")
                    .mnemonic('C'),
                MenuItem::run("粘贴(P)", act("粘贴"), false)
                    .shortcut("Ctrl+V")
                    .mnemonic('P'),
                MenuItem::separator(),
                MenuItem::run("删除(D)", act("删除"), false)
                    .shortcut("Del")
                    .danger()
                    .mnemonic('D'),
            ]
        }
    };
    let view = move || {
        vec![
            MenuItem::run(
                "自动换行(W)",
                move |_ctx| word_wrap.set(!word_wrap.get()),
                word_wrap.get(),
            )
            .mnemonic('W'),
            MenuItem::run(
                "状态栏(S)",
                move |_ctx| status_bar.set(!status_bar.get()),
                status_bar.get(),
            )
            .mnemonic('S'),
        ]
    };
    let help = move || {
        vec![
            MenuItem::run("查看帮助(H)", |ctx| ctx.toast("F1"), false)
                .shortcut("F1")
                .mnemonic('H'),
            MenuItem::separator(),
            MenuItem::run("关于(A)…", |ctx| ctx.toast("windui 菜单栏示例"), false).mnemonic('A'),
        ]
    };

    let ui = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .child(
            Element::menu_bar(vec![
                MenuBarEntry::new("文件(F)", file).mnemonic('F'),
                MenuBarEntry::new("编辑(E)", edit).mnemonic('E'),
                MenuBarEntry::new("查看(V)", view).mnemonic('V'),
                MenuBarEntry::new("帮助(H)", help).mnemonic('H'),
            ])
            .height(28)
            .padding_xy(4, 0)
            .bg_role(Role::Surface),
        )
        .child(
            Element::col()
                .fill()
                .padding(20)
                .spacing(8)
                .child(
                    Element::label("点标题展开；展开后滑到相邻标题即切换；← → 跨菜单")
                        .fg_role(Role::TextMuted)
                        .height(20)
                        .width_match(),
                )
                .child(
                    Element::label(
                        "F10 或单击 Alt 激活；Alt+F / Alt+E 直接展开；按下划线字母激活项",
                    )
                    .fg_role(Role::TextMuted)
                    .height(20)
                    .width_match(),
                )
                .child(Element::label_signal(last).height(24).width_match()),
        );

    App::new("windui · 菜单栏", 560, 320)
        .content(ui)
        .screenshot_from_args()
        .run();
}
