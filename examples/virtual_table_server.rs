//! 服务端分页 + 虚拟滚动：二十万行在"后端"，前端只按滚动位置按段取。
//!
//! 运行：cargo run --release --example virtual_table_server
//! 截屏：cargo run --example virtual_table_server -- --screenshot artifacts/vts.png
//!
//! 与 [`virtual_list`](virtual_list) 里的 `table_virtual` 的分界不在"数据从哪来"，而在
//! **整份数据在不在本地内存**。在本地就用 `table_virtual`（哪怕是接口一次性拉回来的）；
//! 二十万行文本在本地是几十 MB，而本库的立身指标之一是约 3.6MB 常驻——数据真在后端时
//! 用这里的 `table_virtual_server`。
//!
//! 看点：
//! - **滚动条一开始就是对的**：按总行数撑高，不随数据到货跳来跳去。
//! - **没到货的行画骨架灰条**（无动画：流光要每帧重绘，会把空闲 CPU 钉在满转）。
//! - **点表头排序**：缓存自动作废并按新排序重拉——这一步不用应用记得做。
//! - **同一段只请求一次**，滚过去再滚回来不重复问；缓存超过上限按 LRU 淘汰，且绕开
//!   正在看的那几段。
//! - **取数失败可重试**：勾上"下次请求失败"再滚动，那一段会停在骨架，`retry` 之后重新拉。
//!
//! 这里的"后端"是另一个线程 + 一段 `sleep`。真实应用把 `fetch` 换成 HTTP/DB 即可——
//! 关键是**回填要带上收到的那个 `RowRequest`**，慢响应才不会盖掉新排序的数据。

use std::time::Duration;

use windui::prelude::*;

/// 后端总行数。前端从不持有这么多行，只握着这个数字撑滚动条。
const TOTAL: usize = 200_000;

/// 行内有按钮，行高按「控件高 + 单元格上下内边距 + 分隔线」给够；`TABLE_ROW_H` 只够单行文本。
const ROW_H: i32 = 30 + 9 * 2 + 1;

/// 一次响应：请求原样带回（用于校验代次），加上这一段的行与总行数。
struct Page {
    req: RowRequest,
    rows: Option<Vec<Vec<String>>>,
    total: usize,
}

/// 假后端：按排序把显示下标映射到源下标，再按源下标生成那一行。
///
/// 真实后端这里是 `ORDER BY … LIMIT … OFFSET …`。用映射而不是真排序，是因为示例不该
/// 在内存里摆二十万行——那正是这套东西要避开的事。
fn backend_row(i: usize, sort: Option<SortKey>) -> Vec<String> {
    let src = match sort {
        None => i,
        Some(k) => match k.order {
            SortOrder::Asc => (i * 7919 + k.column * 13) % TOTAL,
            SortOrder::Desc => TOTAL - 1 - (i * 7919 + k.column * 13) % TOTAL,
        },
    };
    vec![
        format!("file_{src:06}.dat"),
        format!("{}", src * 37 % 9999),
        format!("2026-{:02}-{:02}", src % 12 + 1, src % 28 + 1),
    ]
}

fn main() {
    // 总行数先给 0：行源会先发一次引导请求（0..chunk），应用在回应里连总数一起补上。
    // 这正是真实后端"首屏一次 COUNT + 第一页"的形状。
    let src = RowSource::new(0);
    let status = signal(String::from("等待首屏…"));
    let slow = signal(true);
    let fail_next = signal(false);

    let mut app = App::new("windui — 服务端分页 + 虚拟滚动", 760, 620).icon(brand_icon());

    // 后端回应落回 UI 线程：这里才碰 RowSource（它是线程局部的信号句柄，不能跨线程）。
    let tx = app.channel::<Page>(move |ctx, page| {
        src.set_total(page.total);
        match page.rows {
            Some(rows) => src.fill(&page.req, rows),
            None => {
                // 失败：把这一段标记回"未请求"，否则它会永远停在骨架态且不报错。
                src.retry(&page.req);
                ctx.toast_err(format!("第 {}.. 段取数失败，已重试", page.req.rows.start));
            }
        }
        status.set(format!(
            "共 {} 行 · 本地缓存 {} 行",
            src.total(),
            src.loaded_rows()
        ));
    });

    let table = Element::table_virtual_server(
        vec![("名称", 3.0), ("大小(KB)", 1.2), ("修改日期", 1.6)],
        src,
        ROW_H,
        move |_ctx, req| {
            // 视口进到还没到货的段时调用一次。同一段不会重复问（行源记着在途台账）。
            let tx = tx.clone();
            let (delay, boom) = (slow.get(), fail_next.get());
            if boom {
                fail_next.set(false);
            }
            std::thread::spawn(move || {
                if delay {
                    std::thread::sleep(Duration::from_millis(450));
                }
                let rows =
                    (!boom).then(|| req.rows.clone().map(|i| backend_row(i, req.sort)).collect());
                let _ = tx.send(Page {
                    req,
                    rows,
                    total: TOTAL,
                });
            });
        },
    )
    // 行内修饰符与本地表格通用；下标是**整份数据里的真实行号**，不是段内位置。
    .actions("操作", 2.0, |row| {
        Element::row()
            .spacing(6)
            .child(
                Element::button("查看")
                    .neutral()
                    .outline()
                    .small()
                    .on_click(move |ctx| ctx.toast(format!("查看第 {} 行", row + 1))),
            )
            .child(
                Element::button("删除")
                    .danger()
                    .outline()
                    .small()
                    .on_click(move |ctx| ctx.toast_err(format!("删除第 {} 行", row + 1))),
            )
    })
    .on_row_context_menu(|row| {
        vec![MenuItem::run(
            "复制行号",
            move |ctx| ctx.clipboard_set(&format!("{}", row + 1)),
            false,
        )]
    })
    .weight(1.0);

    let toolbar = Element::row()
        .width_match()
        .height(30)
        .cross(Align::Center)
        .spacing(14)
        .child(Element::checkbox("模拟慢后端（450ms）", slow))
        .child(Element::checkbox("下次请求失败", fail_next))
        .child(
            Element::label_signal(status)
                .font_size(12.0)
                .fg_role(Role::TextMuted)
                .weight(1.0),
        );

    let ui = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .padding(18)
        .spacing(10)
        .child(
            Element::label("服务端分页 + 虚拟滚动")
                .font_size(22.0)
                .fg_role(Role::Text)
                .height(32)
                .width_match(),
        )
        .child(
            Element::label(
                "二十万行在后端。滚动条按总行数撑高，数据按 100 行一段到货，\
                 没到的画骨架；点表头排序会自动作废缓存并按新排序重拉。",
            )
            .font_size(12.5)
            .fg_role(Role::TextMuted)
            .width_match(),
        )
        .child(toolbar)
        .child(table);

    app.screenshot_from_args().content(ui).run();
}
