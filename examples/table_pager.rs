//! 翻页表格：整页替换 + 分页操作栏。
//!
//! 运行：cargo run --release --example table_pager
//! 截屏：cargo run --example table_pager -- --screenshot artifacts/pager.png
//!
//! 与 [`virtual_table_server`](virtual_table_server) 是两套并列的 UX，**不是替代关系**：
//!
//! - **翻页**（这里）：一次只有一页数据在手，用户知道自己在第几页、总共多少页，能直接跳
//!   到第 37 页。适合"结果集要被逐条看过"的场景。
//! - **虚拟滚动**（那边）：一条长长的滚动，没有"页"的概念，滚动条就是进度指示。适合
//!   "翻着找"的场景。
//!
//! 别把两者叠着用：虚拟滚动的表格上再挂一个页码栏，"当前页"没有对应的视觉锚点，滚一下
//! 页码就变，用户会以为自己点错了。
//!
//! 看点：
//! - 条目总数 / 第 P 共 T 页 / 首·上·下·末页 / 跳到第几页
//! - 边界上按钮**置灰而非隐藏**：位置不跳，也看得出"已经在第一页了"
//! - 先翻到末页（第 103 页），再勾上「只看 .log」→ 条目从 1234 掉到 247（21 页）→
//!   **越界的页码被自动钳到第 21 页并重取那一页**。少了这一步，界面会停在"第 103 / 21 页"
//!   配一张空表，而且怎么点都回不来。
//!   （这个复选框**故意不**回到第一页，就为了让这条路径看得见；换排序那条是回第一页的，
//!   那才是真实应用的常规做法。）

use windui::prelude::*;

/// 后端总条目数。
const TOTAL: usize = 1_234;
/// 每页条目数。
const PAGE_SIZE: usize = 12;

/// 第 `i` 条的三列文本。真实应用里这是一行数据库记录。
fn row_of(i: usize) -> Vec<String> {
    let ext = if i.is_multiple_of(5) { "log" } else { "dat" };
    vec![
        format!("file_{i:04}.{ext}"),
        format!("{}", i * 37 % 9999),
        format!("2026-{:02}-{:02}", i % 12 + 1, i % 28 + 1),
    ]
}

/// 假后端：按筛选与排序给出条目下标的完整顺序。
///
/// 真实应用里这是 `WHERE … ORDER BY …`，只有 `LIMIT/OFFSET` 那一段会回到前端。这里为了
/// 示例自足才在本地算——**前端不该持有整份数据**，那正是分页要避开的事。
fn backend_ids(only_log: bool, sort: Option<SortKey>) -> Vec<usize> {
    let mut ids: Vec<usize> = (0..TOTAL)
        .filter(|i| !only_log || i.is_multiple_of(5))
        .collect();
    if let Some(k) = sort {
        ids.sort_by(|&a, &b| {
            let (x, y) = (row_of(a), row_of(b));
            let (x, y) = (&x[k.column], &y[k.column]);
            // 两边都是数就按数比，否则按串比——与 table_sortable 的客户端排序同口径。
            match (x.parse::<f64>(), y.parse::<f64>()) {
                (Ok(a), Ok(b)) => a.total_cmp(&b),
                _ => x.cmp(y),
            }
        });
        if k.order == SortOrder::Desc {
            ids.reverse();
        }
    }
    ids
}

/// 按当前的筛选/排序/页码取一页，并把条目总数写回。
///
/// 总数每次都写：筛选一变它就变，而页码栏据它算总页数与边界禁用态。
fn reload(
    rows: Signal<Vec<Vec<String>>>,
    total: Signal<usize>,
    page: Signal<usize>,
    only_log: Signal<bool>,
    sort: Signal<Option<SortKey>>,
) {
    let ids = backend_ids(only_log.get(), sort.get());
    total.set(ids.len());
    let start = page.get() * PAGE_SIZE;
    rows.set(
        ids.iter()
            .skip(start)
            .take(PAGE_SIZE)
            .map(|&i| row_of(i))
            .collect(),
    );
}

fn main() {
    let rows = signal(Vec::new());
    let total = signal(0usize);
    let page = signal(0usize);
    let only_log = signal(false);
    let sort = signal(None);

    reload(rows, total, page, only_log, sort);

    let toolbar = Element::row()
        .width_match()
        .cross(Align::Center)
        .spacing(12)
        .child(
            Element::checkbox(
                "只看 .log（故意不重置页码：翻到末页再勾，看页码被钳回来）",
                only_log,
            )
            .on_click(move |_ctx| {
                // 复选框挂了 on_click 之后，**翻转要自己做**：回调是接管不是追加
                // （见 Element::checkbox 的说明）。少了这一行，勾选框点了不打勾、
                // 筛选也永远读到 false。
                only_log.set(!only_log.get());
                // 这里**故意不**回到第一页，好让"页码越界被自动钳回末页"看得见：
                // 翻到第 103 页再勾上，条目只剩 21 页，页码栏会把它钳到第 21 页并
                // 重取那一页。真实应用换筛选条件时通常还是回第一页（同下面换排序）。
                reload(rows, total, page, only_log, sort);
            }),
        );

    let table = Element::table_sortable_server(
        vec![("名称", 3.0), ("大小(KB)", 1.2), ("修改日期", 1.6)],
        rows,
        sort,
        move |_ctx, new_sort| {
            // 换排序也回到第一页：第 20 页的"新第 20 页"与用户刚才在看的东西无关。
            sort.set(new_sort);
            page.set(0);
            reload(rows, total, page, only_log, sort);
        },
    )
    .actions("操作", 1.6, |disp| {
        // 服务端表格的行下标是**页内显示下标**（本页第几行），不是全局条目号。
        Element::button("查看")
            .neutral()
            .outline()
            .small()
            .on_click(move |ctx| ctx.toast(format!("本页第 {} 行", disp + 1)))
    })
    .weight(1.0);

    let ui = Element::col()
        .fill()
        .bg_role(Role::Bg)
        .padding(18)
        .spacing(10)
        .child(
            Element::label("翻页表格")
                .font_size(22.0)
                .fg_role(Role::Text)
                .height(32)
                .width_match(),
        )
        .child(
            Element::label(
                "整页替换 + 分页操作栏：一次只有一页数据在手。点表头排序、勾选筛选都会回到第一页。",
            )
            .font_size(12.5)
            .fg_role(Role::TextMuted)
            .width_match(),
        )
        .child(toolbar)
        .child(table)
        .child(Element::pager(page, total, PAGE_SIZE, move |_ctx, _p| {
            // 页码栏已经把新页码写进 page 了，这里只管去取那一页。
            reload(rows, total, page, only_log, sort);
        }));

    App::new("windui — 翻页表格", 760, 620)
        .icon(brand_icon())
        .screenshot_from_args()
        .content(ui)
        .run();
}
