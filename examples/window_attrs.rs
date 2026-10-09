//! 窗口属性示例：不可缩放的无边框窗、指定位置 + 不进任务栏的浮窗、高频跨线程消息。
//!
//! 运行：cargo run --release --example window_attrs
//! 截屏：cargo run --example window_attrs -- --screenshot artifacts/window_attrs.png
//!
//! 三块各对应一处只有真窗口才看得出的行为（截图路径到不了 `WM_NC*` 与任务栏）：
//!
//! - **主窗**是不可缩放的无边框窗：最外一圈不是缩放边，贴着窗口右缘 / 下缘的两条
//!   色带点得到（计数会涨），光标也不会变成缩放箭头。窗口大小正好是请求的 460×420。
//! - **浮窗**：`position` 指定外框左上角（平台原生屏幕坐标，Windows 为物理像素），
//!   `skip_taskbar` 不进任务栏（Windows 上同时退出 Alt+Tab）。这类窗口没有最小化按钮——
//!   最小化之后任务栏上没有它，无处还原。
//! - **高频消息**：后台线程每毫秒 `send` 一次，同时一条忙碌进度条在连续动画。CPU 应与
//!   只有动画时相当，消息由帧循环按刷新率排空，而不是每条消息推一次整窗重画。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use windui::prelude::*;

/// 浮窗：无边框、不可缩放、不进任务栏，外框左上角落在 `(x, y)`。
fn floating(x: i32, y: i32) -> WindowRequest {
    Window::new("浮窗", 240, 120)
        .position(x, y)
        .skip_taskbar(true)
        .frameless(true)
        .resizable(false)
        .content(move || {
            Element::col()
                .fill()
                .bg_role(Role::Surface)
                .border_role(Role::Border, 1)
                .child(
                    Element::row()
                        .width_match()
                        .height(32)
                        .cross(Align::Stretch)
                        .window_drag()
                        .child(Element::label("  浮窗（拖这里移动）").weight(1.0))
                        .child(Element::window_button(WindowButtonKind::Close)),
                )
                .child(
                    Element::label(format!("外框左上角请求在 ({x}, {y})"))
                        .padding_xy(12, 8)
                        .fg_role(Role::TextMuted),
                )
        })
}

/// 贴边色带：点一下计数加一。放在窗口最外缘，验证那一圈不被当成缩放边吞掉。
fn edge_strip(hits: Signal<u32>, text: Signal<String>, vertical: bool) -> Element {
    let strip = if vertical {
        Element::col().width(10).height_match()
    } else {
        Element::row().height(10).width_match()
    };
    strip.bg_role(Role::Accent).clickable().on_click(move |_| {
        hits.set(hits.get() + 1);
        text.set(format!("贴边色带点击次数：{}", hits.get()));
    })
}

fn main() {
    let edge_hits = signal(0u32);
    let edge_text = signal(String::from("贴边色带点击次数：0"));
    let received = signal(0u64);
    let rate_text = signal(String::from("未开始"));
    let running = Arc::new(AtomicBool::new(false));

    let mut app = App::new("窗口属性", 460, 420)
        .frameless()
        .resizable(false)
        .centered();

    // 消息计数：每条消息只写一个数，界面文字由 on_interval 每 200ms 刷一次——
    // 测的是「消息唤醒」这条路，不让每条消息顺带格式化字符串干扰读数。
    let tx = app.channel::<()>(move |_, ()| received.set(received.get() + 1));
    let started = Arc::new(std::sync::Mutex::new(None::<Instant>));

    let title_bar = Element::row()
        .width_match()
        .height(36)
        .cross(Align::Stretch)
        .bg_role(Role::SurfaceAlt)
        .window_drag()
        .child(Element::label("   窗口属性（不可缩放的无边框窗）").weight(1.0))
        .child(Element::window_button(WindowButtonKind::Minimize))
        .child(Element::window_button(WindowButtonKind::Close));

    let run_flag = running.clone();
    let start_at = started.clone();
    let body = Element::col()
        .weight(1.0)
        .width_match()
        .padding(20)
        .spacing(12)
        .child(Element::label("1. 无边框、不可缩放").font_size(15.0))
        .child(
            Element::label("右缘与下缘的色带贴着窗口最外一圈；点得到、光标不变成缩放箭头即正确。")
                .fg_role(Role::TextMuted)
                .width_match(),
        )
        .child(Element::label_signal(edge_text).width_match())
        .child(Element::label("2. 指定位置 + 不进任务栏").font_size(15.0))
        .child(
            Element::row()
                .spacing(8)
                .child(Element::button("在 (100, 100) 开浮窗").on_click(|ctx| {
                    ctx.open_window(floating(100, 100));
                }))
                .child(Element::button("在 (600, 300) 开浮窗").on_click(|ctx| {
                    ctx.open_window(floating(600, 300));
                })),
        )
        .child(Element::label("3. 高频消息 + 连续动画").font_size(15.0))
        .child(Element::progress_indeterminate().width_match())
        .child(
            Element::button("后台每毫秒发一次，持续 10 秒").on_click(move |ctx| {
                if run_flag.swap(true, Ordering::SeqCst) {
                    ctx.toast("已在发送中");
                    return;
                }
                received.set(0);
                *start_at.lock().unwrap() = Some(Instant::now());
                let (tx, flag) = (tx.clone(), run_flag.clone());
                std::thread::spawn(move || {
                    let end = Instant::now() + Duration::from_secs(10);
                    while Instant::now() < end && tx.send(()).is_ok() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    flag.store(false, Ordering::SeqCst);
                });
            }),
        )
        .child(Element::label_signal(rate_text).width_match());

    let ui = Element::col()
        .fill()
        .bg_role(Role::Surface)
        .child(title_bar)
        .child(
            Element::row()
                .weight(1.0)
                .width_match()
                .child(body.weight(1.0))
                .child(edge_strip(edge_hits, edge_text, true)),
        )
        .child(edge_strip(edge_hits, edge_text, false));

    app.on_interval(Duration::from_millis(200), move |_| {
        if let Some(t0) = *started.lock().unwrap() {
            let secs = t0.elapsed().as_secs_f64().max(0.001);
            let n = received.get();
            let state = if running.load(Ordering::SeqCst) {
                "发送中"
            } else {
                "已结束"
            };
            rate_text.set(format!(
                "{state}：已收 {n} 条，约 {:.0} 条/秒",
                n as f64 / secs
            ));
        }
    })
    .screenshot_from_args()
    .content(ui)
    .run();
}
