//! 渲染性能探针：GPU 档与软件路径对账用（**不是**使用示例）。
//!
//! 形状是刻意选的——**脏区极小而绘制树很大**，正是局部重绘该赢、而"恒全窗"该输的那类
//! 帧：160 个标签 + 1 个自动聚焦的输入框，每帧真正变的只有光标那一条竖线。
//!
//! 两档场景各对应一类帧：
//!
//! - `PROBE_CARET=blink`（默认）：光标亮/灭各半周期硬切换，**帧率应落在 ~1.9fps**
//!   （530ms 半周期）。这一档量的是"闲着的时候白付了多少"。落不到 1.9fps 那一组数据
//!   作废——鼠标停在窗口上会把它抬到几十 fps，实测踩过。
//! - `PROBE_CARET=phase`：光标 alpha 每帧起伏，60fps 连续动画，脏区同样极小。
//!   这一档量的是"连续动画的每帧成本"。
//! - `PROBE_CARET=solid`：光标不闪，零续帧。用来确认"静止界面真的不出帧"。
//!
//! 另有 `PROBE_FULL=1`：加一个每 16ms 换一次文本的标签。文本换了排版就变，宿主据结构
//! 签名把这一帧升成**整窗**——于是能量到「稳态整窗帧」，那正是 glyph atlas 的收益场景
//! （160 条文字每帧全部重画，但字形早已在 atlas 里）。整段光栅粒度下这一档同样是每帧
//! 全部重新光栅，两者的差就是 atlas 值多少。
//!
//! 跑法（macOS，必须 release）：
//!
//! ```sh
//! caffeinate -u -t 30 &                       # 显示器睡着时 wgpu 恒报 Occluded → 零帧
//! WINDUI_GPU=1 WINDUI_PROF=1 \
//!   cargo run --release --features gpu --example perfprobe
//! ```

use windui::prelude::*;

/// 每列的标签数 × 列数 = 绘制树里的控件数。分列是为了让它们**都落在窗口内**——
/// 挤到窗口外的节点会被 pixmap 边界/`cull_rect` 廉价剔除，那样"160 控件"就是虚的。
const COLS: usize = 4;
const ROWS: usize = 40;

fn main() {
    let style = match std::env::var("PROBE_CARET").as_deref() {
        Ok("phase") => CaretStyle::Phase,
        Ok("solid") => CaretStyle::Solid,
        _ => CaretStyle::Blink,
    };
    let mut theme = Theme::default();
    theme.palette.bg = Color::hex(0xF5F7FA);
    theme.input.caret_style = Some(style);

    let text = signal(String::from("光标闪烁：每帧真正变的只有这一条竖线"));
    // 整窗档：一个每帧换文本的标签。文本变 → 排版变 → 结构签名变 → 宿主升整窗帧。
    let full_mode = std::env::var("PROBE_FULL").is_ok_and(|v| v != "0");
    let tick = signal(0u64);

    let mut grid = Element::row().width_match().spacing(8);
    for c in 0..COLS {
        let mut col = Element::col().weight(1.0).spacing(1);
        for r in 0..ROWS {
            col = col.child(
                Element::label(format!("控件 {:03}", c * ROWS + r))
                    .font_size(11.0)
                    .fg(Color::hex(0x44485C)),
            );
        }
        grid = grid.child(col);
    }

    let mut ui = Element::col()
        .fill()
        .padding(10)
        .spacing(8)
        .child(Element::text_input(text, "输入…").width_match().autofocus());
    if full_mode {
        ui = ui.child(
            Element::label_signal(tick.map(|n| format!("整窗档 tick {n}")))
                .font_size(12.0)
                .fg(Color::hex(0x1A2035)),
        );
    }
    let ui = ui.child(grid);

    let mut app = App::new("windui perf probe", 640, 760)
        .icon(brand_icon())
        .theme(theme);
    if full_mode {
        app = app.on_interval(std::time::Duration::from_millis(16), move |_| {
            tick.set(tick.get() + 1);
        });
    }
    app.content(ui).screenshot_from_args().run();
}
