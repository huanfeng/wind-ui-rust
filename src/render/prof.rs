//! 绘制热点计时（仅诊断）。环境变量 `WINDUI_PROF` 非空时启用：各绘制图元把耗时
//! 累加到对应桶，宿主每帧打印拆分。禁用时 [`start`] 不取时钟，零额外开销。

use std::cell::RefCell;
use std::time::Instant;

/// 计时桶下标。
pub const FILL: usize = 0;
pub const STROKE: usize = 1;
pub const TEXT: usize = 2;
pub const SHADOW: usize = 3;
pub const IMAGE: usize = 4;
pub const CLIP: usize = 5;
pub const N: usize = 6;

const LABELS: [&str; N] = ["fill", "stroke", "text", "shadow", "image", "clip"];

thread_local! {
    static NANOS: RefCell<[u64; N]> = const { RefCell::new([0; N]) };
    static COUNTS: RefCell<[u32; N]> = const { RefCell::new([0; N]) };
}

/// 是否启用绘制计时（读一次缓存）。
pub fn enabled() -> bool {
    use std::sync::OnceLock;
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::env::var("WINDUI_PROF").is_ok_and(|v| v != "0" && !v.is_empty()))
}

/// 开始计时；禁用时返回 None（不取时钟）。
pub fn start() -> Option<Instant> {
    if enabled() {
        Some(Instant::now())
    } else {
        None
    }
}

/// 结束计时并累加到 `bucket`。
pub fn end(bucket: usize, t: Option<Instant>) {
    if let Some(t) = t {
        let ns = t.elapsed().as_nanos() as u64;
        NANOS.with(|b| b.borrow_mut()[bucket] += ns);
        COUNTS.with(|c| c.borrow_mut()[bucket] += 1);
    }
}

/// 作用域计时哨兵：drop 时把存活期累加到 `bucket`，覆盖含提前 return 的所有路径。
/// 禁用时不取时钟。用法：`let _g = prof::scope(prof::TEXT);`
pub struct Guard(usize, Option<Instant>);
impl Drop for Guard {
    fn drop(&mut self) {
        end(self.0, self.1);
    }
}
pub fn scope(bucket: usize) -> Guard {
    Guard(bucket, start())
}

/// 取出并清空本帧各桶，格式化为按耗时降序的单行摘要（仅非零桶）。无数据返回空串。
pub fn take_summary() -> String {
    let (nanos, counts) = NANOS.with(|b| {
        COUNTS.with(|c| {
            let mut b = b.borrow_mut();
            let mut c = c.borrow_mut();
            let out = (*b, *c);
            *b = [0; N];
            *c = [0; N];
            out
        })
    });
    let mut items: Vec<(usize, u64, u32)> = (0..N)
        .map(|i| (i, nanos[i], counts[i]))
        .filter(|x| x.1 > 0)
        .collect();
    items.sort_by_key(|x| std::cmp::Reverse(x.1));
    items
        .iter()
        .map(|(i, ns, cnt)| format!("{} {:.2}ms({})", LABELS[*i], *ns as f64 / 1e6, cnt))
        .collect::<Vec<_>>()
        .join("  ")
}
