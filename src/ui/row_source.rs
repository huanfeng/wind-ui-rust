//! 服务端行源：整份数据在后端，前端只握着「总行数 + 已到货的分段 + 当前排序」。
//!
//! [`Element::table_virtual`](super::Element::table_virtual) 要求整份数据在本地
//! （`Signal<Vec<Vec<String>>>`）。十万行文本在本地就是十几 MB，而本库的立身指标之一是
//! 约 3.6MB 常驻内存——数据真在后端时，把它整份拉下来正是虚拟滚动想避开的那件事。
//!
//! 本模块给的是另一套数据模型：
//!
//! ```text
//! 总行数 100000  ← 滚动条按它撑高，第一帧就是对的
//! ┌──────────────────────────────────────────┐
//! │ 段 0      段 1      段 2   …   段 999     │  每段 100 行
//! │  ✗         ✓         ✓             ✗      │  ✓=已到货  ✗=画骨架灰条
//! └──────────────────────────────────────────┘
//!            └──视口──┘                        视口进到空洞就发一次请求
//! ```
//!
//! 三条设计上的取舍，都不是随手定的：
//!
//! - **按固定分段拉取**，不按视口的实际区间。区间随滚动位置漂移，滚 3 像素就是一个新区间，
//!   既没法去重也没法缓存；对齐到 `chunk` 的倍数之后，后端拿到的就是 `LIMIT n OFFSET m×n`，
//!   同一段滚过去再滚回来不会重复请求。
//! - **缓存有上限**（默认 [`ROW_CACHE_SEGMENTS`] 段）。不淘汰的话，把十万行滚一遍就等于
//!   把整份数据搬进了内存——那正是这套东西要避开的。淘汰绕开当前视口覆盖的段，否则会
//!   把正在看的那一段淘汰掉，画面当场闪成骨架。
//! - **台账与数据分成两个信号**。信号一写就请求重绘（见 `signal::notify_changed`），
//!   而在途请求、LRU 这些每帧都要翻看的字段若混在数据信号里，光是翻看就会把
//!   「空闲零 CPU」写死。这里的规矩是：**只有真的变了才写信号**，有测试钉着。

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use crate::signal::{signal, Signal};

use super::SortKey;

/// 默认分段大小（行）。
pub const ROW_CHUNK: usize = 100;

/// 默认缓存上限（段数）。默认值下常驻约 3200 行。
pub const ROW_CACHE_SEGMENTS: usize = 32;

/// 已到货的数据，以及一切会影响画面的东西。写它就该重画。
struct Rows {
    total: usize,
    /// 总行数是否已由后端确认过。
    ///
    /// 与 `total == 0` **不是**一回事：前者是"还不知道"，后者可能是"确实一行都没有"。
    /// 两者混为一谈的话，空结果集会永远画着一屏骨架。
    known: bool,
    chunk: usize,
    /// 段起点 → 该段的行。段起点一定是 `chunk` 的整数倍。
    seg: HashMap<usize, Vec<Vec<String>>>,
    /// 代次：排序变化或 `invalidate` 时 +1，用来丢弃过期响应。
    gen: u64,
}

/// 台账：只决定"下一次向后端要什么"，不影响画面。
///
/// 单独一个信号是有意的，理由见模块文档最后一条。
struct Book {
    cap: usize,
    /// 已发出、尚未到货的段起点。
    inflight: HashSet<usize>,
    /// 最近用过的排在末尾；淘汰从头取。
    lru: Vec<usize>,
    /// 当前渲染窗口覆盖的段起点，淘汰时绕开它们。
    window: Vec<usize>,
    /// `total == 0` 时是否已发过引导请求（见 [`RowSource::new`]）。
    bootstrapped: bool,
}

/// 一次取数请求：要哪个区间、按什么排序。
///
/// 拿到之后向后端取数，再用 [`RowSource::fill`] 写回——**回填要带上这个请求**，
/// 而不只是起始下标：它同时带着代次，慢响应因此不会盖掉新排序的数据（异步取数里
/// 最经典的一个 bug）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RowRequest {
    /// 要取的行区间（已对齐到分段边界，末段按总行数收尾）。
    pub rows: Range<usize>,
    /// 当前排序；后端应按它排好序之后再取 `rows` 这一段。
    pub sort: Option<SortKey>,
    /// 代次。私有：应用只该原样带回，不该自己造。
    pub(super) gen: u64,
}

/// 稀疏行源：总行数撑起滚动条，数据按段到货，未到货的行画骨架占位。
///
/// 供 [`Element::table_virtual_server`](super::Element::table_virtual_server) 使用。
/// 与 [`Signal`] 一样是 `Copy` 句柄，可以随手 `move` 进多个闭包。
///
/// # 示例
///
/// ```ignore
/// let src = RowSource::new(total_from_backend);
/// Element::table_virtual_server(cols, src, TABLE_ROW_H, move |_ctx, req| {
///     // 同步取数就地写回；异步取数把 req 一起搬进回调，到货时再 fill
///     let rows = db.query(req.sort, req.rows.clone());
///     src.fill(&req, rows);
/// })
/// ```
#[derive(Clone, Copy)]
pub struct RowSource {
    rows: Signal<Rows>,
    book: Signal<Book>,
    sort: Signal<Option<SortKey>>,
}

impl RowSource {
    /// 已知总行数的行源。分段大小 [`ROW_CHUNK`]、缓存上限 [`ROW_CACHE_SEGMENTS`]。
    ///
    /// `total` 可以先给 `0`：此时会**先发一次引导请求**（`0..chunk`），应用在回调里
    /// 拿到首段数据的同时调 [`set_total`](Self::set_total) 补上总数。没有这条引导，
    /// `total == 0` 的表格什么都不会请求，界面永远空着且不报错。
    pub fn new(total: usize) -> Self {
        Self {
            rows: signal(Rows {
                total,
                known: total != 0,
                chunk: ROW_CHUNK,
                seg: HashMap::new(),
                gen: 0,
            }),
            book: signal(Book {
                cap: ROW_CACHE_SEGMENTS,
                inflight: HashSet::new(),
                lru: Vec::new(),
                window: Vec::new(),
                bootstrapped: false,
            }),
            sort: signal(None),
        }
    }

    /// 改分段大小（行）。应与后端一次能舒服返回的量对齐。
    ///
    /// 只在建表之前调用：改它会作废已到货的数据（旧段的边界对不上新的分段）。
    pub fn chunk(self, chunk: usize) -> Self {
        let chunk = chunk.max(1);
        self.rows.update(|r| {
            r.chunk = chunk;
            r.seg.clear();
            r.gen = r.gen.wrapping_add(1);
        });
        self.book.update(Book::reset);
        self
    }

    /// 改缓存上限（保留多少段）。下限 4 段——低于视口能覆盖的段数就会反复淘汰再重拉。
    pub fn cache_segments(self, n: usize) -> Self {
        self.book.update(|b| b.cap = n.max(4));
        self
    }

    /// 总行数。
    pub fn total(&self) -> usize {
        self.rows.with(|r| r.total)
    }

    /// 设置总行数（滚动条据此撑高），并标记为"后端已确认"。
    ///
    /// 值没变且已确认过则不写信号，故可以在每次响应里无脑调用。
    ///
    /// 用 `RowSource::new(0)` 起步时**必须**调它，哪怕结果就是 0 行：在确认之前，正文
    /// 按引导请求那一段的高度画骨架（否则首屏全空，"还没到货"就看不见了），确认为 0
    /// 之后才会收成空表。
    pub fn set_total(&self, total: usize) {
        if self.rows.with(|r| r.total == total && r.known) {
            return;
        }
        self.rows.update(|r| {
            r.total = total;
            r.known = true;
        });
    }

    /// 排序状态信号：表头绑定它显示 ▲/▼。应用可以预设初始排序，也可以自己读。
    ///
    /// 它变化时行源会**自动作废**（见 [`invalidate`](Self::invalidate)）——排序变了旧
    /// 顺序的缓存就是错的，这一步不该由应用记得去做。
    pub fn sort(&self) -> Signal<Option<SortKey>> {
        self.sort
    }

    /// 回填一段数据。
    ///
    /// `req` 必须是回调里收到的那一个：它带着代次，若排序在请求发出后变过，本次写入会被
    /// **丢弃**而不是盖掉新数据。行数多于该段容量时截断，少于时按实到长度记（尾段正常
    /// 就比整段短）。
    pub fn fill(&self, req: &RowRequest, rows: Vec<Vec<String>>) {
        let start = req.rows.start;
        let (gen, chunk) = self.rows.with(|r| (r.gen, r.chunk));
        if req.gen != gen {
            // 过期响应：排序或数据在这次请求发出之后变过了。
            self.book.update(|b| {
                b.inflight.remove(&start);
            });
            return;
        }
        let mut rows = rows;
        rows.truncate(chunk);
        self.rows.update(|r| {
            r.seg.insert(start, rows);
        });
        self.book.update(|b| {
            b.inflight.remove(&start);
            b.lru.retain(|&c| c != start);
            b.lru.push(start);
        });
        self.evict();
    }

    /// 把某次请求标记回"未请求"，使它下次进入视口时会被重新发出。
    ///
    /// 用于取数失败：不这么做的话那一段会永远停在"在途"，骨架条再也不会变成数据，
    /// 且不会有任何报错。
    pub fn retry(&self, req: &RowRequest) {
        let start = req.rows.start;
        let armed = self.book.with(|b| b.inflight.contains(&start));
        if !armed {
            return;
        }
        self.book.update(|b| {
            b.inflight.remove(&start);
        });
        // 顺带把数据版本推一格：正文只在"区间或数据版本变了"时才重新发请求，少了这一下，
        // 重试要等用户滚动一次才生效——而失败时用户多半正盯着那几行骨架不动。
        self.rows.update(|_| {});
    }

    /// 作废全部已到货数据与在途请求，代次 +1（在途响应回来会被丢弃）。总行数保留。
    ///
    /// 排序变化时会自动调用；数据在后端被改动时由应用自己调。
    pub fn invalidate(&self) {
        self.rows.update(|r| {
            r.seg.clear();
            r.gen = r.gen.wrapping_add(1);
        });
        self.book.update(Book::reset);
    }

    /// 当前缓存了多少行（诊断/测试用）。
    pub fn loaded_rows(&self) -> usize {
        self.rows.with(|r| r.seg.values().map(Vec::len).sum())
    }

    // ---- 以下供正文 widget 使用 ----

    /// 撑起滚动条的行数：总数已确认就用它，还没确认就先按引导请求那一段撑着。
    ///
    /// 先撑一段是为了**首屏就能看见骨架**。留白的话，慢后端下的第一眼是一张空表，
    /// 与"查询结果为空"无从区分——那正是选骨架而不是留白要避开的事。
    pub(super) fn span(&self) -> usize {
        self.rows.with(|r| if r.known { r.total } else { r.chunk })
    }

    pub(super) fn version(&self) -> u64 {
        self.rows.version()
    }

    pub(super) fn sort_version(&self) -> u64 {
        self.sort.version()
    }

    /// `[range)` 各行的数据，未到货的位置为 `None`。一次借用取完，不逐行 `with`。
    pub(super) fn visible(&self, range: Range<usize>) -> Vec<Option<Vec<String>>> {
        self.rows.with(|r| {
            range
                .map(|i| {
                    let start = i / r.chunk * r.chunk;
                    r.seg.get(&start).and_then(|s| s.get(i - start)).cloned()
                })
                .collect()
        })
    }

    /// 记下当前渲染窗口覆盖的段（淘汰时绕开），并把它们提到 LRU 末尾。
    ///
    /// **窗口没变就不写信号**：每帧都写等于每帧都请求重绘，「空闲零 CPU」当场作废。
    pub(super) fn set_window(&self, range: Range<usize>) {
        let starts = self.rows.with(|r| chunk_starts(&range, r.chunk));
        if self.book.with(|b| b.window == starts) {
            return;
        }
        self.book.update(|b| {
            for &c in &starts {
                b.lru.retain(|&x| x != c);
                b.lru.push(c);
            }
            b.window = starts;
        });
    }

    /// 取出 `[range)` 里还缺、且不在途的那些段，标记为在途并返回对应请求。
    ///
    /// **没有要发的就不写信号**，理由同 [`set_window`](Self::set_window)。
    pub(super) fn take_requests(&self, range: Range<usize>) -> Vec<RowRequest> {
        let (total, chunk, gen) = self.rows.with(|r| (r.total, r.chunk, r.gen));
        let sort = self.sort.get();

        // 总行数还不知道：发一次引导请求，应用回填时顺带 set_total。
        if total == 0 {
            if self.book.with(|b| b.bootstrapped) {
                return Vec::new();
            }
            self.book.update(|b| {
                b.bootstrapped = true;
                b.inflight.insert(0);
            });
            return vec![RowRequest {
                rows: 0..chunk,
                sort,
                gen,
            }];
        }

        let starts = chunk_starts(&range, chunk);
        let want: Vec<usize> = self.rows.with(|r| {
            self.book.with(|b| {
                starts
                    .into_iter()
                    .filter(|c| !r.seg.contains_key(c) && !b.inflight.contains(c))
                    .collect()
            })
        });
        if want.is_empty() {
            return Vec::new();
        }
        self.book.update(|b| {
            for &c in &want {
                b.inflight.insert(c);
            }
        });
        want.into_iter()
            .map(|c| RowRequest {
                rows: c..(c + chunk).min(total),
                sort,
                gen,
            })
            .collect()
    }

    /// 超出上限时淘汰最久没用过的段，**绕开当前窗口覆盖的那几段**。
    fn evict(&self) {
        let victims: Vec<usize> = self.book.with(|b| {
            let over = self
                .rows
                .with(|r| r.seg.len())
                .saturating_sub(b.cap.max(b.window.len()));
            b.lru
                .iter()
                .copied()
                .filter(|c| !b.window.contains(c))
                .take(over)
                .collect()
        });
        if victims.is_empty() {
            return;
        }
        self.rows.update(|r| {
            for c in &victims {
                r.seg.remove(c);
            }
        });
        self.book.update(|b| {
            b.lru.retain(|c| !victims.contains(c));
        });
    }
}

impl Book {
    fn reset(&mut self) {
        self.inflight.clear();
        self.lru.clear();
        self.window.clear();
        self.bootstrapped = false;
    }
}

/// `[range)` 覆盖到的全部段起点（升序）。
fn chunk_starts(range: &Range<usize>, chunk: usize) -> Vec<usize> {
    if range.start >= range.end {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut c = range.start / chunk * chunk;
    while c < range.end {
        out.push(c);
        c += chunk;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::SortOrder;

    /// 一段假数据：第 `i` 行的首格就是 `r{i}`，供"下标与数据对得上吗"这类断言取真相。
    fn rows_for(range: &Range<usize>) -> Vec<Vec<String>> {
        range
            .clone()
            .map(|i| vec![format!("r{i}"), format!("{}", i * 3)])
            .collect()
    }

    /// 发请求 + 立刻回填，模拟同步取数。
    fn serve(src: &RowSource, range: Range<usize>) -> usize {
        let reqs = src.take_requests(range);
        let n = reqs.len();
        for req in reqs {
            let rows = rows_for(&req.rows);
            src.fill(&req, rows);
        }
        n
    }

    #[test]
    fn requests_align_to_chunk_boundaries() {
        // 对齐是这套设计的地基：不对齐就没法去重、没法缓存，后端也拿不到可复用的分页参数。
        let src = RowSource::new(1_000);
        let got: Vec<_> = src
            .take_requests(150..260)
            .into_iter()
            .map(|r| r.rows)
            .collect();
        assert_eq!(got, vec![100..200, 200..300], "请求应对齐到分段边界");
    }

    #[test]
    fn last_chunk_stops_at_the_total() {
        // 末段按总行数收尾，否则应用会照着越界的区间去查库。
        let src = RowSource::new(250);
        let got: Vec<_> = src
            .take_requests(210..250)
            .into_iter()
            .map(|r| r.rows)
            .collect();
        assert_eq!(got, vec![200..250], "末段不应越过总行数");
    }

    #[test]
    fn never_asks_twice_for_the_same_chunk() {
        // 去重是"按需拉取"能用的前提：滚动时每帧都会问一次要不要拉，没有台账就是每帧一发。
        let src = RowSource::new(1_000);
        let reqs = src.take_requests(0..50);
        assert_eq!(reqs.len(), 1);
        assert!(src.take_requests(0..50).is_empty(), "在途的段不该再问一次");
        src.fill(&reqs[0], rows_for(&reqs[0].rows));
        assert!(
            src.take_requests(0..50).is_empty(),
            "已到货的段不该再问一次"
        );
    }

    #[test]
    fn idle_never_writes_the_signals() {
        // 「空闲零 CPU」的守卫。信号一写就请求重绘（见 signal::notify_changed），而这些
        // 台账每帧都要翻看——翻看绝不能变成写。这条测试盯着的退化不会有任何视觉表现：
        // 界面一切正常，只是 CPU 再也回不到零。
        let src = RowSource::new(1_000);
        serve(&src, 0..300);
        src.set_window(0..300);
        let before = (src.rows.version(), src.book.version());
        for _ in 0..10 {
            src.take_requests(0..300); // 都已到货，无事可做
            src.set_window(0..300); // 窗口没变
            src.set_total(1_000); // 值没变
            src.visible(0..300); // 只读
        }
        assert_eq!(
            (src.rows.version(), src.book.version()),
            before,
            "没有实质变化时一个信号都不该写"
        );
    }

    #[test]
    fn stale_response_is_dropped() {
        // 异步取数最经典的一个 bug：慢响应盖掉新排序的数据。表现是"排了序，内容却是旧的"，
        // 而且只在网络慢的时候偶发。
        let src = RowSource::new(1_000);
        let req = src.take_requests(0..100).remove(0);
        src.sort().set(Some(SortKey::new(0, SortOrder::Asc)));
        src.invalidate(); // 正文在排序变化时会替应用做这一步
        src.fill(&req, rows_for(&req.rows));
        assert_eq!(src.loaded_rows(), 0, "旧代次的响应必须丢弃");
        // 丢弃之后那一段要能重新请求，否则它会永远停在骨架态。
        assert_eq!(src.take_requests(0..100).len(), 1, "丢弃后应可重新请求");
    }

    #[test]
    fn eviction_spares_the_visible_segments() {
        // 淘汰若不绕开当前窗口，正在看的那一段会被淘汰掉——画面当场闪成骨架，然后重拉，
        // 再闪回来，滚动时无限循环。
        let src = RowSource::new(10_000).cache_segments(4);
        src.set_window(0..200); // 段 0、1 是"正在看的"
        for c in 0..8usize {
            serve(&src, c * 100..c * 100 + 1);
        }
        assert!(src.visible(0..1)[0].is_some(), "视口内的段 0 不得被淘汰");
        assert!(
            src.visible(100..101)[0].is_some(),
            "视口内的段 1 不得被淘汰"
        );
        assert_eq!(src.loaded_rows(), 4 * 100, "缓存应被上限压住");
    }

    #[test]
    fn bootstraps_once_when_the_total_is_unknown() {
        // total==0 时若不发引导请求，表格会永远空着、一次请求都不发，且不报错——
        // 那种"什么都没发生"最难查。
        let src = RowSource::new(0);
        let reqs = src.take_requests(0..0);
        assert_eq!(reqs.len(), 1, "总数未知时应先发一次引导请求");
        assert_eq!(reqs[0].rows, 0..ROW_CHUNK);
        assert!(
            src.take_requests(0..0).is_empty(),
            "引导请求只该发一次，否则每帧一发"
        );
    }

    #[test]
    fn retry_re_arms_a_failed_chunk_right_away() {
        // 取数失败后不重新武装，那一段会永远停在"在途"，骨架条再也不会变成数据。
        let src = RowSource::new(1_000);
        let req = src.take_requests(0..100).remove(0);
        assert!(src.take_requests(0..100).is_empty());

        let ver = src.rows.version();
        src.retry(&req);
        assert_eq!(src.take_requests(0..100).len(), 1, "重试后应能重新发出");
        assert_ne!(
            src.rows.version(),
            ver,
            "重试要顺带推一格数据版本，否则正文要等用户滚动一次才会重发——\
             而失败时用户多半正盯着那几行骨架不动"
        );
    }

    #[test]
    fn sort_travels_with_the_request() {
        // 后端要按当前排序取那一段。排序没随请求带出去的话，应用只能自己去读全局状态，
        // 而那个状态可能已经又变了。
        let src = RowSource::new(1_000);
        src.sort().set(Some(SortKey::new(1, SortOrder::Desc)));
        let req = src.take_requests(0..10).remove(0);
        assert_eq!(req.sort, Some(SortKey::new(1, SortOrder::Desc)));
    }
}
