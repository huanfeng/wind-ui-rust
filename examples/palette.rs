//! **纯键盘通路**：唤起 → 打字 → ↑↓ 选候选 → Enter 确认。命令面板 / 查词 / 快速打开
//! / 地址栏共用的同一条路。
//!
//! 运行：`cargo run --release --example palette`
//!
//! - 启动**不显示窗口**，只在托盘留一个图标（常驻工具的常态）。
//! - 按 **Ctrl+Alt+P** 唤起：焦点自动落进查询框**并全选**上次的词——直接覆盖打字，
//!   不用先删。
//! - 打字过筛候选，**↑↓** 移动游标，**Tab** 接受当前候选（补全进输入框），
//!   **Enter** 确认，**Esc** 收回窗口。**Shift+Tab** 仍是反向焦点导航——刻意放过，
//!   否则吞掉 Tab 的输入框会让用户失去离开它的唯一键盘途径。
//! - 全程不碰鼠标。退出走托盘右键。
//!
//! ## 这个例子为什么必须存在
//!
//! 这条通路由三条独立的能力**合起来**才成立，分开看每一条都像已经能用了：
//!
//! | 能力 | 缺了它的表现 |
//! |---|---|
//! | [`Element::autofocus_select_all`] | `focus == None`，第一次按键**消失**（`Tree::dispatch_key` 的目标是 `Option<NodeId>`，为 `None` 时整个事件丢弃） |
//! | [`Element::on_submit`] | Enter 无出口。按键分发**不冒泡**，故「外层容器接 Enter」编译通过、逻辑正确、永远不触发 |
//! | [`Element::on_nav_key`] | ↑↓ 同样就地消失，游标动不了；Tab 到不了控件（曾被宿主抢先截去做焦点导航） |
//!
//! ## 选中态是应用自己画的
//!
//! 框架只负责把 ↑↓ 送到 [`Element::on_nav_key`]；「哪一项高亮」是应用状态
//! （这里是 `cursor: Signal<usize>`）。高亮条是一个 `bg_role_alpha` 的兄弟节点，
//! 靠 `visible_when` 跟随游标——用角色色而非写死颜色，故运行期换主题自动跟随。
//!
//! ## 已知盲区
//!
//! 本例的键盘交互目前**截不到图**：截图路径（`--screenshot`）支持 `--click` /
//! `--drag` / `--hover`，但还没有合成键盘输入的 `--type` / `--key`。所以「焦点落在
//! 哪」「游标停在第几项」这两件事只能靠人眼在真机上看，进不了视觉回归。首帧的
//! autofocus + 全选状态是唯一截得到的部分。

use windui::core::EventCtx;
use windui::event::Key;
use windui::prelude::*;

/// 候选词库（真实应用里来自词典/命令注册表）。
const WORDS: &[(&str, &str)] = &[
    ("palette", "调色板；命令面板"),
    ("parade", "游行；一系列"),
    ("paradigm", "范式；典范"),
    ("parallel", "平行的；并行"),
    ("paranoid", "偏执的"),
    ("parcel", "包裹；一批"),
    ("pardon", "原谅；请再说一遍"),
    ("parse", "解析；剖析"),
];

/// 生成 size×size 纯色 RGBA8（演示图标，免捆绑资源）。
fn solid(size: u32, hex: u32) -> Vec<u8> {
    let (r, g, b) = (
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    );
    [r, g, b, 255].repeat((size * size) as usize)
}

/// 命中查询的候选**下标**（按 WORDS 原序）。空查询给全部。
///
/// 游标索引的是这个列表而不是 `WORDS`——过筛后「第 0 项」指的是屏幕上第一行，
/// 与用户看到的一致。
fn matches(query: &str) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    WORDS
        .iter()
        .enumerate()
        .filter(|(_, (w, _))| q.is_empty() || w.contains(&q))
        .map(|(i, _)| i)
        .collect()
}

fn main() {
    let query = signal(String::from("pa"));
    // 游标：命中列表内的位置。
    let cursor = signal(0usize);
    let result = signal(String::from("等待确认…"));

    // ---- 候选行：结构静态，显隐/高亮由信号驱动 ----
    let mut list = Element::col();
    for (i, (word, gloss)) in WORDS.iter().enumerate() {
        // 高亮条：当前游标指向本行时出现。用角色色 + 低透明度，换主题自动跟随。
        let highlight = Element::leaf()
            .fill()
            .corner(6.0)
            .bg_role_alpha(Role::Accent, 0.18)
            .visible_when(move || matches(&query.get()).get(cursor.get()) == Some(&i));
        let row = Element::row()
            .fill()
            .padding_xy(10, 6)
            .cross(Align::Center)
            .child(Element::label(*word).font_size(15.0))
            .child(Element::flex_spacer())
            .child(Element::label(*gloss).fg_role(Role::TextMuted));
        list = list.child(
            Element::stack()
                .height(30)
                // 整行随过筛结果显隐。不占位（visible 的语义）——落选的行不留空档。
                .visible_when(move || matches(&query.get()).contains(&i))
                .child(highlight)
                .child(row),
        );
    }

    // ---- 确认当前项：Enter 与鼠标点击共用同一段逻辑 ----
    let confirm = move |ctx: &mut EventCtx| {
        let hits = matches(&query.get());
        match hits.get(cursor.get()) {
            Some(&i) => {
                let (word, gloss) = WORDS[i];
                result.set(format!("已确认：{word} — {gloss}"));
                ctx.toast_ok(format!("查 {word}"));
            }
            // 无候选时 Enter 应当什么都不做，而不是 panic 或确认一个空项。
            None => result.set(String::from("没有候选可确认")),
        }
    };

    let ui = Element::col()
        .fill()
        .padding(16)
        .spacing(10)
        .child(
            Element::text_input(query, "输入以过筛…")
                .height(34)
                .leading_icon('\u{1F50D}')
                // 三条缺口的第一条：焦点有归属，且全选旧内容供覆盖打字。
                .autofocus_select_all()
                // 第二条：Enter 有出口。
                .on_submit(confirm)
                // 第三条：↑↓ 与 Tab 有出口。返回值是「我消费了吗」——宿主的 Tab
                // 焦点导航是兜底，只在这里返回 false 时才轮到。
                .on_nav_key(move |_ctx, ev| {
                    let hits = matches(&query.get());
                    let n = hits.len();
                    if n == 0 {
                        return false;
                    }
                    // 游标钳到命中数内：过筛后候选变少时，停在旧位置会指向一个已经
                    // 不在屏幕上的行。
                    let cur = cursor.get().min(n - 1);
                    match ev.key {
                        Key::Down => {
                            cursor.set((cur + 1) % n);
                            true
                        }
                        Key::Up => {
                            cursor.set((cur + n - 1) % n);
                            true
                        }
                        // Tab 接受当前候选（shell 补全语义）：把词填进输入框，不确认。
                        //
                        // **Shift+Tab 刻意放过**，交回宿主做反向焦点导航——这是用户
                        // 离开查询框的唯一键盘途径。无脑 `true` 会把它一起吃掉。
                        Key::Tab if !ev.shift => {
                            query.set(String::from(WORDS[hits[cur]].0));
                            // 补全后候选只剩一条，游标归零才不会指向空处。
                            cursor.set(0);
                            true
                        }
                        _ => false,
                    }
                }),
        )
        .child(
            Element::label("↑↓ 选择 · Enter 确认 · Esc 收回窗口")
                .fg_role(Role::TextSubtle)
                .font_size(12.0),
        )
        .child(Element::divider())
        .child(list)
        .child(Element::flex_spacer())
        .child(Element::label_signal(result).fg_role(Role::Accent));

    let tray = Tray::new()
        .tooltip("windui 命令面板示例（Ctrl+Alt+P 唤起）")
        .icon_rgba(16, 16, &solid(16, 0x6C5CE7))
        .on_left_click(|ctx| ctx.show_window())
        .menu(vec![
            TrayMenuItem::item("唤起面板", |ctx| ctx.show_window()),
            TrayMenuItem::separator(),
            TrayMenuItem::item("退出", |ctx| ctx.quit()),
        ]);

    App::new("命令面板", 460, 420)
        .start_hidden()
        // Esc / 标题栏 × 收回窗口而不退出——常驻工具的退出只走托盘。
        .hide_on_close()
        .tray(tray)
        .hotkey(Hotkey::new(Key::Char('P')).ctrl().alt(), |ctx| {
            ctx.show_window()
        })
        .content(ui)
        .screenshot_from_args()
        .run();
}
