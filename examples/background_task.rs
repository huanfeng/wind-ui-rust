//! 跨线程更新示例：后台线程发进度经 channel 驱动进度条（事件驱动）；on_interval 驱动秒表。
//!
//! 运行：cargo run --example background_task
//! 截屏：cargo run --example background_task -- --screenshot artifacts/bg.png
use std::time::Duration;
use windui::prelude::*;

fn main() {
    let progress = signal(0.0f32);
    let clock = signal(String::from("已运行 0 秒"));
    let ticks = signal(0u32);

    let mut app = App::new("后台任务", 360, 180);

    // 后台线程：每 40ms 发一次进度，channel 驱动 UI（有更新才唤醒一帧）。
    let pc = progress;
    let tx = app.channel::<f32>(move |p| pc.set(p));
    std::thread::spawn(move || {
        for i in 1..=100 {
            std::thread::sleep(Duration::from_millis(40));
            if tx.send(i as f32 / 100.0).is_err() {
                break; // 窗口已关
            }
        }
    });

    // UI 树：标签 + 进度条 + 动态秒表文本。
    let ui = Element::col()
        .fill()
        .padding(20)
        .spacing(12)
        .child(Element::label("下载进度").height(20).width_match())
        .child(Element::progress(progress).width_match())
        .child(Element::label_rc(clock).height(20).width_match());

    // on_interval：每秒更新秒表文本。
    let (tk, ck) = (ticks, clock);
    app.on_interval(Duration::from_millis(1000), move || {
        tk.set(tk.get() + 1);
        ck.set(format!("已运行 {} 秒", tk.get()));
    })
    .screenshot_from_args()
    .content(ui)
    .run();
}
