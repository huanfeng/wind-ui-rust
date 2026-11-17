//! 真机验证合集：一次覆盖本轮改动里「单测与截图碰不到、只能真窗口看」的几项。
//!
//! 运行：
//! ```text
//! verify_batch.exe                       # 直角 + 无外框 + ESC 不关窗（默认，验证新行为）
//! verify_batch.exe --esc true            # ESC 恢复默认（没有浮层时关窗）
//! verify_batch.exe --corner round --outline auto   # 对照：与改动前一致
//! ```
//! 参数：`--corner round|small|square`、`--outline off|auto`、`--esc true|false`。
//! 圆角、外框、ESC 都是**建窗时**定下的，要换组合就重新启动。
//!
//! 每一节标题下写着「应该看到什么」。看完告诉我哪几节不符合。

use std::time::Duration;
use windui::prelude::*;

const TITLE_BG: u32 = 0x2D3436;
const TICK_MS: u64 = 100;
const STEPS: i32 = 30;

fn arg_value(name: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

/// 窗口状态 → 一行可读文本。UNKNOWN = 事件外快照读数（纪律使然）。
fn fmt_state(s: &WindowState) -> String {
    if *s == WindowState::UNKNOWN {
        return "UNKNOWN（事件外快照不给读数）".into();
    }
    let f = |b: bool| if b { "是" } else { "否" };
    format!(
        "max={} min={} visible={} maxable={} minable={}",
        f(s.maximized),
        f(s.minimized),
        f(s.visible),
        f(s.maximizable),
        f(s.minimizable),
    )
}

/// 一节：标题 + 「应该看到什么」+ 内容。
fn section(title: &str, expect: &str, body: Element) -> Element {
    Element::col()
        .width_match()
        .spacing(6)
        .padding(12)
        .bg_role(Role::SurfaceAlt)
        .corner(8.0)
        .child(
            Element::label(title)
                .font_size(14.0)
                .fg_role(Role::Text)
                .width_match(),
        )
        .child(
            Element::label(format!("应该看到：{expect}"))
                .font_size(12.0)
                .fg_role(Role::TextMuted)
                .width_match(),
        )
        .child(body)
}

fn main() {
    let corner_name = arg_value("--corner").unwrap_or_else(|| "square".into());
    let corner = match corner_name.as_str() {
        "round" => WindowCorner::Round,
        "small" => WindowCorner::Small,
        _ => WindowCorner::Square,
    };
    let outline_name = arg_value("--outline").unwrap_or_else(|| "off".into());
    let outline = if outline_name == "auto" {
        WindowOutline::Auto
    } else {
        WindowOutline::Off
    };
    let esc_closes = arg_value("--esc").as_deref() == Some("true");

    // —— 光标隐藏（定时器路径）——
    let countdown = signal(0i32);
    let hide_left = signal(0i32);
    let hidden = signal(false);
    let status = signal(String::from("空闲"));

    // —— 数值框 ——
    let width = signal(1280.0);
    let alpha = signal(85.5);
    let count = signal(3.0);
    let echo = signal(String::from("width=1280  alpha=85.5  count=3"));

    // —— 浮层点外面 ——
    let open_swallow = signal(false);
    let open_pass = signal(false);
    let probe = signal(String::from("hello world foo bar"));
    let sw = signal(false);

    // —— Slider ——
    let volume = signal(0.4f32);
    let grabbed = signal(false);

    // —— 主窗状态（⑦ 快照 vs 同步查询）——
    let st_query = signal(String::from("…"));
    let st_snap = signal(String::from("…"));
    let st_event = signal(String::from("还没读过"));
    let tick_seq = std::cell::Cell::new(0u32);

    let title_bar = Element::row()
        .width_match()
        .height(36)
        .cross(Align::Stretch)
        .bg(Color::hex(TITLE_BG))
        .window_drag()
        .child(
            Element::label(format!(
                "   verify_batch — corner={corner_name} outline={outline_name} esc={esc_closes}"
            ))
            .fg(Color::WHITE)
            .weight(1.0),
        )
        .child(Element::window_button(WindowButtonKind::Minimize).fg(Color::WHITE))
        .child(Element::window_button(WindowButtonKind::Maximize).fg(Color::WHITE))
        .child(Element::window_button(WindowButtonKind::Close).fg(Color::WHITE));

    // ① 光标：普通状态下的各种形状（回归 sync_cursor）
    let s1 = section(
        "① 光标形状没被搞坏",
        "悬停输入框是 I 形、悬停按钮是手形；点一下之后光标不闪、不跳回箭头；窗口四边四角是缩放光标。",
        Element::row()
            .spacing(10)
            .child(Element::text_input(signal(String::from("悬停我（I 形）")), "").width(180))
            .child(
                // 手形不是框架按钮的默认（原生按钮惯例是箭头，见 Element::cursor 文档），
                // 要网页手感得显式挂上——上一版示例漏挂，标签却写着「悬停我（手形）」，
                // 真机上表现为「手形没生效」。这里补上，顺带把 .cursor() 与 sync_cursor
                //（指针静止时光标形状变化需要补发 WM_SETCURSOR）的链路一起验掉。
                Element::button("悬停我（手形）")
                    .cursor(CursorShape::Hand)
                    .on_click(|ctx| ctx.toast("点到了")),
            ),
    );

    // ② 光标隐藏
    let s2 = section(
        "② 光标隐藏",
        "点按钮后手别动：3 秒后指针自己消失，再过 3 秒恢复。深色区域内指针不可见，移出恢复。",
        Element::col()
            .spacing(8)
            .child(
                Element::row()
                    .spacing(10)
                    .cross(Align::Center)
                    .child(Element::button("3 秒后隐藏指针").on_click(move |_| {
                        countdown.set(STEPS);
                        status.set("倒计时中…手别动".into());
                    }))
                    .child(Element::label(status).fg_role(Role::TextMuted)),
            )
            .child(
                Element::col()
                    .width_match()
                    .height(48)
                    .bg(Color::hex(0x636E72))
                    .cursor(CursorShape::Hidden)
                    .child(
                        Element::label("指针在此区域应不可见")
                            .fg(Color::WHITE)
                            .padding(12),
                    ),
            ),
    );

    // ③ number_input
    let s3 = section(
        "③ 纯数字输入框 number_input",
        "框内右侧有 px / % 单位；用 Tab 在三个框与普通输入框间切换，边框应正确变色、无残影；中文输入法下只能输入数字；↑↓ 步进；越界值失焦后钳回。",
        Element::col()
            .spacing(8)
            .child(
                Element::row()
                    .spacing(10)
                    .cross(Align::Center)
                    .child(Element::label("宽度").width(48))
                    .child(
                        Element::number_input(width, 100.0, 7680.0, 1.0)
                            .unit("px")
                            .text_align(Align::End)
                            .width(150),
                    )
                    .child(Element::label("不透明度").width(60))
                    .child(
                        Element::number_input(alpha, 0.0, 100.0, 1.0)
                            .decimals(1)
                            .unit("%")
                            .width(130),
                    ),
            )
            .child(
                Element::row()
                    .spacing(10)
                    .cross(Align::Center)
                    .child(Element::label("数量").width(48))
                    .child(Element::number_input(count, 0.0, 9.0, 1.0).width(150))
                    .child(Element::label("对照").width(60))
                    .child(Element::text_input(signal(String::new()), "普通输入框").width(130)),
            )
            .child(
                Element::button("读取当前值").on_click(move |_| {
                    echo.set(format!(
                        "width={}  alpha={}  count={}",
                        width.get(),
                        alpha.get(),
                        count.get()
                    ));
                }),
            )
            .child(Element::label(echo).font_size(12.0).fg_role(Role::TextMuted)),
    );

    // ④ 浮层点外面不穿透
    let panel = |swallow: bool| {
        Element::col()
            .width(150)
            .padding(10)
            .bg_role(Role::Surface)
            .corner(8.0)
            .child(Element::label("浮层面板"))
            .dismiss_swallows_click(swallow)
    };
    let s4 = section(
        "④ 浮层点外面：吞掉 vs 穿透",
        "先点左侧按钮打开面板，再去点右侧的开关 / 在输入框上双击。【吞掉】：这一下只关面板，开关不翻转；双击只算一次单击（不选中单词）。【穿透】：面板关闭的同时开关也翻转；双击会选中单词。",
        Element::row()
            .spacing(14)
            .child(
                Element::col()
                    .spacing(8)
                    .child(
                        Element::button("打开面板（吞掉）")
                            .on_click(move |_| open_swallow.set(!open_swallow.get()))
                            .popup(open_swallow, panel(true)),
                    )
                    .child(
                        Element::button("打开面板（穿透）")
                            .on_click(move |_| open_pass.set(!open_pass.get()))
                            .popup(open_pass, panel(false)),
                    ),
            )
            .child(
                Element::col()
                    .spacing(8)
                    .child(
                        Element::row()
                            .spacing(8)
                            .cross(Align::Center)
                            .child(Element::switch(sw))
                            .child(Element::label(sw.map(|b| {
                                if *b { "开关：开".to_string() } else { "开关：关".to_string() }
                            }))),
                    )
                    .child(Element::text_input(probe, "").width(220)),
            ),
    );

    // ⑤ Slider 拖动态
    let s5 = section(
        "⑤ Slider 拖动时才显示百分比",
        "按住滑块拖动时右侧出现百分比，松手后消失；拖动过程中旋钮不应相对指针跳动。",
        Element::row()
            .width(300)
            .cross(Align::Center)
            .spacing(8)
            .child(Element::slider(volume).dragging_signal(grabbed).weight(1.0))
            .child(
                // 标签位置常驻，只显隐里面的文字，避免按下时滑块被缩窄。
                Element::row().width(44).child(
                    Element::label(volume.map(|v| format!("{:.0}%", v * 100.0)))
                        .visible_signal(grabbed),
                ),
            ),
    );

    // ⑥ ESC
    let s6 = section(
        "⑥ ESC 开关",
        if esc_closes {
            "（--esc true）没有浮层时按 ESC 会关闭窗口——与改动前一致。"
        } else {
            "（默认）没有浮层时按 ESC 窗口不应关闭；先打开 ④ 的面板再按 ESC，应只收起面板。点右上角 × 仍可关闭。"
        },
        Element::label("在窗口里按 ESC 试试").fg_role(Role::TextMuted),
    );
    // ⑦ 主窗状态：快照 vs 同步查询
    // 真机核对清单：
    // - 查询行：各转换后 ≤500ms 刷新（修复前隐藏后 visible 恒「是」）。
    // - 快照行：事件外恒 UNKNOWN（纪律使然，不是缺陷）。
    // - F9 热键每次按下都有效（修复前 visible 陈旧 → 第二次按无反应）。
    // - 「事件期快照」读到的值 = 查询行。
    let s7 = section(
        "⑦ 主窗状态：快照 vs 同步查询",
        "查询行随每个转换在半秒内刷新；快照行恒 UNKNOWN（事件外快照不给读数，纪律使然）。",
        Element::col()
            .spacing(8)
            .child(
                Element::row()
                    .spacing(10)
                    .cross(Align::Center)
                    .child(Element::button("隐藏").on_click(|ctx| {
                        ctx.hide_window();
                    }))
                    .child(Element::button("显示").on_click(|ctx| {
                        ctx.show_window();
                    }))
                    .child(Element::button("最小化").on_click(|ctx| {
                        ctx.minimize();
                    }))
                    .child(Element::button("还原").on_click(|ctx| {
                        ctx.restore();
                    }))
                    .child(Element::button("事件期快照").on_click(move |_ctx| {
                        st_event.set(fmt_state(&window_state()));
                    })),
            )
            .child(Element::label(st_query).fg_role(Role::Text).width_match())
            .child(
                Element::label(st_snap)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            )
            .child(
                Element::label(st_event)
                    .fg_role(Role::TextMuted)
                    .width_match(),
            ),
    );

    let body = Element::scroll().fill().bg_role(Role::Surface).child(
        Element::col()
            .width_match()
            .padding(14)
            .spacing(12)
            .child(s1)
            .child(s2)
            .child(s3)
            .child(s4)
            .child(s5)
            .child(s6)
            .child(s7),
    );

    // 隐藏期间盖一层全窗覆盖，光标形状取 Hidden（`clickable()` 让它参与命中）。
    let cover = Element::col()
        .fill()
        .clickable()
        .cursor(CursorShape::Hidden)
        .visible_signal(hidden);

    let ui = Element::stack()
        .fill()
        .child(
            Element::col()
                .fill()
                .child(title_bar)
                .child(body.weight(1.0)),
        )
        .child(cover);

    App::new("windui — verify_batch", 680, 760)
        .icon(brand_icon())
        .frameless()
        .window_corner(corner)
        .window_outline(outline)
        .escape_closes_window(esc_closes)
        .hotkey(Hotkey::new(Key::F(9)), move |ctx| {
            // 热键回调里用仲裁读法判向（快照在窗口隐藏后停在旧值——正是要验的）。
            let show = main_window_state().is_some_and(|s| !s.visible);
            if show {
                ctx.show_window();
            } else {
                ctx.hide_window();
            }
        })
        .on_interval(Duration::from_millis(TICK_MS), move |_| {
            // ⑦ 状态行 500ms 刷新：值不变不写（空闲纪律）
            let seq = tick_seq.get() + 1;
            tick_seq.set(seq);
            if seq.is_multiple_of(5) {
                let q = main_window_state()
                    .map(|s| fmt_state(&s))
                    .unwrap_or_else(|| "None（无主窗）".into());
                if st_query.get() != q {
                    st_query.set(q);
                }
                let s = window_state();
                let txt = if s == WindowState::UNKNOWN {
                    "UNKNOWN（事件外快照不给读数）".to_string()
                } else {
                    fmt_state(&s)
                };
                if st_snap.get() != txt {
                    st_snap.set(txt);
                }
            }
            let c = countdown.get();
            if c > 0 {
                countdown.set(c - 1);
                if c == 1 {
                    hidden.set(true);
                    hide_left.set(STEPS);
                    status.set("指针应已隐藏（3 秒后恢复）".into());
                }
            } else if hide_left.get() > 0 {
                let l = hide_left.get() - 1;
                hide_left.set(l);
                if l == 0 {
                    hidden.set(false);
                    status.set("已恢复".into());
                }
            }
        })
        .screenshot_from_args()
        .content(ui)
        .run();
}
