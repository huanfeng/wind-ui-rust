//! 聊天式滚动示例：跟随底部、头部插入锚定、边缘回调。
//!
//! 运行：cargo run --release --example chat_scroll
//! 截屏：cargo run --example chat_scroll -- --screenshot artifacts/chat_scroll.png

use windui::prelude::*;

fn main() {
    // 消息 = (序号, 文本)；序号既是键，也决定先后。
    let msgs = signal(
        (100..130u32)
            .map(|i| (i, format!("消息 #{i}")))
            .collect::<Vec<_>>(),
    );
    let next_new = signal(130u32);
    let next_old = signal(99u32);
    let status = signal(String::from("尚未触发边缘"));

    let list = Element::list_signal(
        msgs,
        |m| m.0,
        |m| {
            Element::label(m.1)
                .width_match()
                .height(40)
                .padding(8)
                .fg_role(Role::Text)
        },
    )
    .stick_to_bottom(true)
    .preserve_on_prepend(true)
    .on_scroll_edge(move |_ctx, edge| {
        status.set(match edge {
            ScrollEdge::Top => "到达顶部".to_string(),
            ScrollEdge::Bottom => "到达底部".to_string(),
        });
    })
    .width_match()
    .weight(1.0)
    .bg_role(Role::Surface)
    .corner(10.0)
    .border_role(Role::Border, 1);

    let ui = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .padding(20)
        .spacing(10)
        .child(
            Element::row()
                .width_match()
                .height(36)
                .spacing(10)
                .child(Element::button("追加消息").on_click(move |_| {
                    let n = next_new.get();
                    next_new.set(n + 1);
                    let mut v = msgs.get();
                    v.push((n, format!("消息 #{n}")));
                    msgs.set(v);
                }))
                .child(Element::button("加载更早").on_click(move |_| {
                    let end = next_old.get();
                    let start = end.saturating_sub(4);
                    next_old.set(start.saturating_sub(1));
                    let mut v: Vec<(u32, String)> = (start..=end)
                        .map(|i| (i, format!("更早的消息 #{i}")))
                        .collect();
                    v.extend(msgs.get());
                    msgs.set(v);
                }))
                .child(Element::label_signal(status).fg_role(Role::TextMuted)),
        )
        .child(list);

    App::new("windui — 聊天滚动", 360, 480)
        .icon(brand_icon())
        .screenshot_from_args()
        .content(ui)
        .run();
}
