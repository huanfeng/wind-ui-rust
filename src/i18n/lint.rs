//! 译文一致性检查。**给测试用的公开工具**，不是运行期代码。
//!
//! # 为什么这东西必须存在
//!
//! 多语言最容易出的不是崩溃，是**静默错**：某语言漏了一条 key（跑到回退语言，看着像
//! "这句没翻译"）、占位符名字对不上（`en` 写 `{count}`、`zh-CN` 写 `{num}`，代入时那一
//! 处变空串）、变体表漏了 `other`（落到别的类别，语法怪但不报错）。三种都要跑到那条具体
//! 路径才看得见，而那正是自测最容易漏的地方。
//!
//! 把它做成库的一部分而不是一个脚本，是因为**下游也需要**：任何用 windui 做多语言的应用
//! 都会遇到同样三种错。在自己的测试里写一行即可：
//!
//! ```
//! use windui::i18n::lint;
//!
//! let zh = "[meta]\nlocale = \"zh-CN\"\n[a]\nb = \"你好 {name}\"\n";
//! let en = "[meta]\nlocale = \"en\"\n[a]\nb = \"hi {name}\"\n";
//!
//! let problems = lint::check(&[zh, en]);
//! assert!(problems.is_empty(), "{problems:#?}");
//! ```

use std::collections::{BTreeMap, BTreeSet};

use super::{parse, Entry};

/// 一条问题的可读描述。返回 `Vec<String>` 而不是结构化错误类型：它唯一的消费者是
/// `assert!(problems.is_empty(), "{problems:#?}")`，人读的字符串比枚举更有用。
pub type Problems = Vec<String>;

/// 检查一组译文源（按 `include_str!` 的顺序传入）是否互相一致。
///
/// 以**并集**为基准而不是拿第一份当模板：漏 key 是双向的，`en` 多写了一条而 `zh-CN`
/// 没有，同样是问题。
pub fn check(sources: &[&str]) -> Problems {
    let mut problems = Problems::new();
    let mut langs: Vec<(String, BTreeMap<String, Entry>)> = Vec::new();

    for (i, src) in sources.iter().enumerate() {
        match parse::parse(src) {
            Ok(f) => {
                let id = f
                    .meta
                    .locale
                    .clone()
                    .unwrap_or_else(|| format!("#{i}（缺 [meta] locale）"));
                if f.meta.locale.is_none() {
                    problems.push(format!("译文 #{i} 缺 `[meta] locale`"));
                }
                langs.push((id, f.entries));
            }
            Err(e) => problems.push(format!("译文 #{i} 解析失败：{e}")),
        }
    }

    let all_keys: BTreeSet<String> = langs.iter().flat_map(|(_, e)| e.keys().cloned()).collect();

    for key in &all_keys {
        // 1) 谁少了这条
        let missing: Vec<&str> = langs
            .iter()
            .filter(|(_, e)| !e.contains_key(key))
            .map(|(id, _)| id.as_str())
            .collect();
        if !missing.is_empty() {
            problems.push(format!("`{key}` 缺在：{}", missing.join(", ")));
        }

        // 2) 占位符集合必须一致（跨语言比较，用集合而非顺序——译文调换语序是正当的）
        let mut seen: BTreeMap<BTreeSet<String>, Vec<&str>> = BTreeMap::new();
        for (id, entries) in &langs {
            if let Some(e) = entries.get(key) {
                seen.entry(placeholders(e)).or_default().push(id);
            }
        }
        if seen.len() > 1 {
            let detail: Vec<String> = seen
                .iter()
                .map(|(ph, ids)| format!("{:?}→{:?}", ids, ph))
                .collect();
            problems.push(format!(
                "`{key}` 的占位符各语言不一致：{}",
                detail.join(" / ")
            ));
        }

        // 3) 变体表必须有 other —— CLDR 规定它恒存在，也是本库的兜底类别
        for (id, entries) in &langs {
            if let Some(Entry::Variants(m)) = entries.get(key) {
                if !m.contains_key("other") {
                    problems.push(format!("`{key}` 在 {id} 的变体表缺 `other`"));
                }
            }
        }

        // 4) 同一条 key 在各语言里必须是同一种形态（串 / 变体表 / 数组）
        let shapes: BTreeSet<&'static str> = langs
            .iter()
            .filter_map(|(_, e)| e.get(key).map(shape))
            .collect();
        if shapes.len() > 1 {
            problems.push(format!("`{key}` 的形态各语言不一致：{shapes:?}"));
        }
    }

    problems
}

fn shape(e: &Entry) -> &'static str {
    match e {
        Entry::One(_) => "字符串",
        Entry::Variants(_) => "变体表",
        Entry::List(_) => "数组",
    }
}

/// 一条目用到的全部占位符（变体表/数组取各分支的并集）。
fn placeholders(e: &Entry) -> BTreeSet<String> {
    match e {
        Entry::One(p) => p.placeholders().into_iter().collect(),
        Entry::Variants(m) => m.values().flat_map(|p| p.placeholders()).collect(),
        Entry::List(v) => v.iter().flat_map(|p| p.placeholders()).collect(),
    }
}

/// 源码里一处 `t!` / `tr!` 的字面 key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyUse {
    pub key: String,
    /// 1-based 行号。报错要能直接跳过去，不然一个 key 在几十个文件里出现得自己找。
    pub line: usize,
}

/// 扫一段 Rust 源码，取出 `t!("…")` / `tr!("…")` 里的**字面** key（按出现顺序，不去重）。
///
/// # 为什么要自己走一遍词法，而不是一句 `contains("t!(")`
///
/// 三处都会误伤：
/// - `assert!(..)` / `format!(..)` / `print!(..)` 的名字都以 `t!` 结尾；
/// - 文档注释里的示例（本库自己的 `i18n` 模块文档就写着 `t!("menu.copy")`）；
/// - 字符串字面量里出现的 `t!("…")`（比如这段文档本身）。
///
/// 前一种靠"`t` 前面不能是标识符字符"排除，后两种只能靠真的跟踪注释与字符串状态。
/// 于是这里是个小状态机：行注释、块注释（可嵌套，Rust 如此）、普通串、原始串
/// （`r"…"` / `r#"…"#`）、字符字面量（并把生命周期 `'a` 与 `'x'` 分开）各自跳过。
///
/// # 查得到什么、查不到什么
///
/// - **只查字面 key**。`t!(key_var)` / `t!(&format!("a.{i}"))` 一律跳过——静态查不动，
///   与其猜不如明说。
/// - **不看 `cfg`**。被 `#[cfg(test)]` 关掉的代码里的 key 也会被收；宁可多报。
pub fn keys_in_source(src: &str) -> Vec<KeyUse> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;

    // 推进 n 个字节并维护行号。
    macro_rules! bump {
        ($n:expr) => {{
            let n = $n;
            for k in i..(i + n).min(b.len()) {
                if b[k] == b'\n' {
                    line += 1;
                }
            }
            i = (i + n).min(b.len());
        }};
    }

    while i < b.len() {
        // ---- 注释 ----
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            let mut depth = 1;
            bump!(2);
            while i < b.len() && depth > 0 {
                if b[i] == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
                    depth += 1;
                    bump!(2);
                } else if b[i] == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    depth -= 1;
                    bump!(2);
                } else {
                    bump!(1);
                }
            }
            continue;
        }
        // ---- 原始串 r"…" / r#"…"# ----
        if b[i] == b'r' && !prev_is_ident(b, i) {
            if let Some(end) = skip_raw_string(b, i) {
                bump!(end - i);
                continue;
            }
        }
        // ---- 普通串 ----
        if b[i] == b'"' {
            let end = skip_string(b, i);
            bump!(end - i);
            continue;
        }
        // ---- 字符字面量 vs 生命周期 ----
        if b[i] == b'\'' {
            if let Some(end) = skip_char_literal(b, i) {
                bump!(end - i);
            } else {
                bump!(1); // 生命周期：只跳这个引号，别把后面整段当字面量吞掉
            }
            continue;
        }
        // ---- t! / tr! ----
        if (b[i] == b't') && !prev_is_ident(b, i) {
            let name_len = if b[i..].starts_with(b"tr!") {
                3
            } else if b[i..].starts_with(b"t!") {
                2
            } else {
                0
            };
            if name_len > 0 {
                let at_line = line;
                bump!(name_len);
                // 宏可以写成 t!(..) / t![..] / t!{..}，都收。
                let j = skip_trivia(b, i);
                if j < b.len() && matches!(b[j], b'(' | b'[' | b'{') {
                    let k = skip_trivia(b, j + 1);
                    if let Some((key, end)) = read_string_literal(b, k) {
                        out.push(KeyUse { key, line: at_line });
                        bump!(end - i);
                        continue;
                    }
                }
                continue;
            }
        }
        bump!(1);
    }
    out
}

/// 跳过空白与注释，返回下一个"有内容"的字节下标。
///
/// 只跳不算：`t!(/* 复数 */ "k", count = n)` 这种写法里 key 前面夹着注释，只跳空白就会
/// 判定"第一个 token 不是字符串字面量"从而当成动态 key **静默跳过**——而漏报正是这个
/// lint 唯一的失效方向（报不出来 = 测试照样绿 = 少一条译文上线）。
///
/// 空白用 `is_ascii_whitespace` 而不是 `(b[j] as char).is_whitespace()`：后者把字节按
/// Latin-1 提升，UTF-8 的续字节 `0x85` 会被当成 NEL 空白。在 UTF-8 扫描器里按 Latin-1
/// 判断是形态上的错，哪怕这个位置几乎不可能出现非 ASCII。
fn skip_trivia(b: &[u8], mut j: usize) -> usize {
    loop {
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j + 1 < b.len() && b[j] == b'/' && b[j + 1] == b'/' {
            while j < b.len() && b[j] != b'\n' {
                j += 1;
            }
            continue;
        }
        if j + 1 < b.len() && b[j] == b'/' && b[j + 1] == b'*' {
            let mut depth = 1;
            j += 2;
            while j < b.len() && depth > 0 {
                if j + 1 < b.len() && b[j] == b'/' && b[j + 1] == b'*' {
                    depth += 1;
                    j += 2;
                } else if j + 1 < b.len() && b[j] == b'*' && b[j + 1] == b'/' {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            continue;
        }
        return j;
    }
}

/// 前一个字节是否属于标识符（用来把 `assert!` / `format!` 的尾巴排除掉）。
fn prev_is_ident(b: &[u8], i: usize) -> bool {
    i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')
}

/// 跳过从 `i`（那个 `"`）开始的普通串，返回收尾引号之后的下标。
fn skip_string(b: &[u8], i: usize) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return j + 1,
            _ => j += 1,
        }
    }
    b.len()
}

/// `r"…"` / `r#"…"#`：`i` 指向 `r`。不是原始串则返回 `None`。
fn skip_raw_string(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    let mut hashes = 0;
    while j < b.len() && b[j] == b'#' {
        hashes += 1;
        j += 1;
    }
    if j >= b.len() || b[j] != b'"' {
        return None;
    }
    j += 1;
    while j < b.len() {
        if b[j] == b'"' {
            let mut k = j + 1;
            let mut n = 0;
            while k < b.len() && b[k] == b'#' && n < hashes {
                k += 1;
                n += 1;
            }
            if n == hashes {
                return Some(k);
            }
        }
        j += 1;
    }
    Some(b.len())
}

/// `'x'` / `'\n'` / `'\''`：`i` 指向开引号。是生命周期（`'a`）则返回 `None`。
fn skip_char_literal(b: &[u8], i: usize) -> Option<usize> {
    if i + 1 >= b.len() {
        return None;
    }
    if b[i + 1] == b'\\' {
        // 从被转义的那个字节**之后**开始找收尾引号：`'\''` 的字节是 `' \ ' '`，
        // 若从 `i+2` 起找，找到的"收尾引号"正是转义序列自己的那个，于是少跳一格。
        // 后果不是报错而是**静默漏报**：扫描器把真正的收尾引号当成新的开引号，
        // 在 `['\'','a']` 这类紧凑写法里会瞎掉一小段，落在里面的 `t!` 就此消失。
        let mut j = i + 3;
        while j < b.len() && b[j] != b'\'' {
            j += 1;
        }
        return Some((j + 1).min(b.len()));
    }
    // 非转义：闭合引号必须紧跟一个字符（UTF-8 多字节要整体跳过）。
    let ch_len = utf8_len(b[i + 1]);
    let close = i + 1 + ch_len;
    if close < b.len() && b[close] == b'\'' {
        Some(close + 1)
    } else {
        None
    }
}

fn utf8_len(first: u8) -> usize {
    match first {
        x if x < 0x80 => 1,
        x if x >> 5 == 0b110 => 2,
        x if x >> 4 == 0b1110 => 3,
        _ => 4,
    }
}

/// 读 `k` 处的字符串字面量（普通或原始），返回 (内容, 结束下标)。不是字面量则 `None`。
fn read_string_literal(b: &[u8], k: usize) -> Option<(String, usize)> {
    if k < b.len() && b[k] == b'"' {
        let end = skip_string(b, k);
        // 未闭合（字符串一直到文件末尾）：`skip_string` 只能返回 `b.len()`，此时
        // `end - 1` 会落在开引号本身甚至之前，`&b[k+1..end-1]` 就是一段**起点大于终点**
        // 的区间——切片直接 panic。而本模块是跑在别人测试里的工具，喂给它一个改到一半
        // 的源文件不该把测试进程打崩。与原始串那一支的 `close < open` 守卫对称。
        if end < k + 2 {
            return None;
        }
        let raw = std::str::from_utf8(&b[k + 1..end - 1]).ok()?;
        return Some((unescape(raw), end));
    }
    if k < b.len() && b[k] == b'r' {
        let end = skip_raw_string(b, k)?;
        let inner = std::str::from_utf8(&b[k..end]).ok()?;
        let open = inner.find('"')? + 1;
        let close = inner.rfind('"')?;
        if close < open {
            return None;
        }
        return Some((inner[open..close].to_string(), end));
    }
    None
}

/// key 里几乎只会出现 `\"` 与 `\\`，够用即可——多的转义序列原样留着，
/// 它们本来就不该出现在一个 key 里，留着反而能在报错里看出是写歪了。
fn unescape(s: &str) -> String {
    s.replace("\\\\", "\\").replace("\\\"", "\"")
}

/// 扫一个目录下的所有 `.rs`，报告**用到了却没有任何语言提供译文**的 key。
///
/// 这是 `lint::check`（查译文文件之间对不对得上）的另一半：那个管"翻译之间一致"，
/// 这个管"代码用到的都翻了"。典型用法是在自己的测试里：
///
/// ```
/// # use windui::prelude::*;
/// # use windui::i18n::lint;
/// // 实际项目里这里是 `include_str!("../i18n/zh-CN.toml")`。
/// let locales = Locales::builder()
///     .embed("[meta]\nlocale = \"zh-CN\"\n[app]\ntitle = \"演示\"\n")
///     .build();
///
/// let problems = lint::check_usage("src", &locales);
/// // 本 crate 的 src/ 里有大量测试自带的 key，这里只示意调用形状。
/// let _ = problems;
/// ```
///
/// # 一个方向，不是两个
///
/// 只查「用了但没译文」。反过来的「译文里有、代码没用」**故意不查**：动态 key
/// （`t!(&format!("status.{s}"))`）静态看不见，那个方向必然误报，而误报的 lint
/// 很快就会被加一堆例外、然后被整个关掉。
///
/// 目录读不动（不存在、没权限）时报一条问题而不是 panic——它跑在测试里，
/// 一条清楚的失败信息比堆栈有用。
pub fn check_usage(dir: impl AsRef<std::path::Path>, locales: &super::Locales) -> Problems {
    let dir = dir.as_ref();
    let mut problems = Problems::new();
    let mut files = Vec::new();
    collect_rs(dir, &mut files, &mut problems);
    files.sort();
    for path in files {
        let src = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                problems.push(format!("读不了 {}：{e}", path.display()));
                continue;
            }
        };
        for u in keys_in_source(&src) {
            if !locales.has_key(&u.key) {
                problems.push(format!(
                    "{}:{} 用了 `{}`，但没有任何语言提供它的译文",
                    path.display(),
                    u.line,
                    u.key
                ));
            }
        }
    }
    problems
}

/// 递归深度上限。够任何正常源码树用，而超过它多半说明踩进了环。
const MAX_DEPTH: usize = 64;

fn collect_rs(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>, problems: &mut Problems) {
    collect_rs_at(dir, out, problems, 0);
}

fn collect_rs_at(
    dir: &std::path::Path,
    out: &mut Vec<std::path::PathBuf>,
    problems: &mut Problems,
    depth: usize,
) {
    if depth > MAX_DEPTH {
        problems.push(format!(
            "目录层级超过 {MAX_DEPTH} 层，停在 {}（多半是符号链接成环）",
            dir.display()
        ));
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            problems.push(format!("读不了目录 {}：{e}", dir.display()));
            return;
        }
    };
    for e in rd.flatten() {
        let p = e.path();
        // `symlink_metadata` 不跟随链接：`p.is_dir()` 会跟随，指向祖先的目录链接就是
        // 一个无界递归，最终栈溢出——那同样是 panic，同样违背本模块"报问题不 panic"的承诺。
        let Ok(md) = std::fs::symlink_metadata(&p) else {
            continue;
        };
        if md.is_dir() {
            collect_rs_at(&p, out, problems, depth + 1);
        } else if md.is_file() && p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ZH: &str = r#"
[meta]
locale = "zh-CN"
[a]
hello = "你好 {name}"
[a.files]
other = "{count} 个文件"
"#;

    #[test]
    fn identical_translations_have_no_problems() {
        let en = "[meta]\nlocale=\"en\"\n[a]\nhello=\"hi {name}\"\n[a.files]\none=\"{count} file\"\nother=\"{count} files\"\n";
        assert_eq!(check(&[ZH, en]), Vec::<String>::new());
    }

    #[test]
    fn a_missing_key_is_reported_with_the_language() {
        let en = "[meta]\nlocale=\"en\"\n[a.files]\nother=\"{count} files\"\n";
        let p = check(&[ZH, en]);
        assert_eq!(p.len(), 1, "{p:#?}");
        assert!(p[0].contains("a.hello") && p[0].contains("en"), "{p:#?}");
    }

    #[test]
    fn a_renamed_placeholder_is_reported() {
        // 最典型的静默错：跑起来那一处是空串，不报任何错。
        let en =
            "[meta]\nlocale=\"en\"\n[a]\nhello=\"hi {nom}\"\n[a.files]\nother=\"{count} files\"\n";
        let p = check(&[ZH, en]);
        assert!(
            p.iter()
                .any(|s| s.contains("a.hello") && s.contains("占位符")),
            "{p:#?}"
        );
    }

    #[test]
    fn a_variants_table_without_other_is_reported() {
        let en =
            "[meta]\nlocale=\"en\"\n[a]\nhello=\"hi {name}\"\n[a.files]\none=\"{count} file\"\n";
        let p = check(&[ZH, en]);
        // `[a.files]` 没有 other → 它根本不被判成变体表，而是命名空间，于是表现为
        // "key 形态不一致 / 缺 key"。两种报法都指向同一处，这里只断言指到了那个 key。
        assert!(p.iter().any(|s| s.contains("a.files")), "{p:#?}");
    }

    #[test]
    fn shape_mismatch_is_reported() {
        let en = "[meta]\nlocale=\"en\"\n[a]\nhello=\"hi {name}\"\nfiles=[\"x\"]\n";
        let p = check(&[ZH, en]);
        assert!(p.iter().any(|s| s.contains("形态")), "{p:#?}");
    }

    #[test]
    fn a_missing_meta_locale_is_reported() {
        let p = check(&["[a]\nb=\"c\"\n"]);
        assert!(p.iter().any(|s| s.contains("meta")), "{p:#?}");
    }

    // ---------------- keys_in_source：扫描器 ----------------

    fn keys(src: &str) -> Vec<String> {
        keys_in_source(src).into_iter().map(|u| u.key).collect()
    }

    #[test]
    fn both_macros_are_picked_up_with_line_numbers() {
        let src = "fn main() {\n    let a = t!(\"a.b\");\n    let b = tr!(\"c.d\");\n}\n";
        assert_eq!(
            keys_in_source(src),
            vec![
                KeyUse {
                    key: "a.b".into(),
                    line: 2
                },
                KeyUse {
                    key: "c.d".into(),
                    line: 3
                },
            ]
        );
    }

    #[test]
    fn macros_whose_names_end_in_t_are_not_mistaken_for_ours() {
        // 这条是"一句 contains 就够了吧"的直接反例：assert! / format! / print! 都以 t! 结尾。
        let src = r#"
            assert!("not.a.key");
            format!("also.not");
            print!("nope");
            debug_assert!("still.not");
        "#;
        assert_eq!(keys(src), Vec::<String>::new());
    }

    #[test]
    fn paths_in_front_of_the_macro_are_fine() {
        let src = "crate::t!(\"a\"); windui::tr!(\"b\"); self::t!(\"c\");";
        assert_eq!(keys(src), vec!["a", "b", "c"]);
    }

    #[test]
    fn comments_are_skipped() {
        // 本库自己的模块文档里就写着 t!("menu.copy") 这样的示例——不跳过注释，
        // 第一次拿它扫自己的 src 就会淹没在假问题里。
        let src = r#"
            // t!("line.comment")
            /// t!("doc.comment")
            /* t!("block.comment") */
            /* 嵌套 /* t!("nested") */ 仍在注释里 t!("still.comment") */
            t!("real.one");
        "#;
        assert_eq!(keys(src), vec!["real.one"]);
    }

    #[test]
    fn keys_inside_string_literals_are_not_calls() {
        let src = r####"
            let s = "t!(\"in.string\")";
            let r = r#"t!("in.raw.string")"#;
            t!("real.one");
        "####;
        assert_eq!(keys(src), vec!["real.one"]);
    }

    #[test]
    fn lifetimes_do_not_swallow_the_rest_of_the_file() {
        // `'a` 的单引号若被当成字符字面量的开头，会一路吞到下一个引号——
        // 后面所有的 t! 就此消失，而且没有任何报错。
        let src = "fn f<'a>(x: &'a str) -> &'a str { t!(\"after.lifetime\"); x }";
        assert_eq!(keys(src), vec!["after.lifetime"]);
    }

    #[test]
    fn char_literals_do_not_desync_the_scanner() {
        let src = "let q = '\"'; let e = '\\''; let u = '中'; t!(\"after.chars\");";
        assert_eq!(keys(src), vec!["after.chars"]);
    }

    #[test]
    fn an_unterminated_string_does_not_panic() {
        // 改到一半的源文件、生成器中断留下的残片都长这样。本模块跑在别人的测试里，
        // 炸掉整个测试进程比报一条问题糟得多，而且堆栈指向 lint 内部、看不出是哪个文件。
        assert_eq!(keys_in_source("t!(\""), Vec::<KeyUse>::new());
        assert_eq!(keys_in_source("t!(\"未闭合的中文"), Vec::<KeyUse>::new());
        assert_eq!(keys_in_source("let s = \"dangling"), Vec::<KeyUse>::new());
        assert_eq!(
            keys_in_source("/* 没收尾的块注释 t!(\"k\")"),
            Vec::<KeyUse>::new()
        );
    }

    #[test]
    fn char_literals_are_consumed_whole() {
        // 直接钉 `skip_char_literal` 的契约，而不是找一段"含 `'\''` 的源码"来验：
        // 少跳一格之后扫描器多半会自愈（下一次判定返回 None、当生命周期跳一格就回到
        // 正轨），我试过的几种紧凑写法都恰好healed——也就是说，用源码片段写的断言
        // **破坏了也不红**，那种测试只是看着在保护你。契约这一层是确定的。
        assert_eq!(skip_char_literal(b"'a'", 0), Some(3));
        assert_eq!(skip_char_literal(b"'\\n'", 0), Some(4));
        assert_eq!(skip_char_literal(b"'\\\\'", 0), Some(4));
        assert_eq!(
            skip_char_literal(b"'\\''", 0),
            Some(4),
            "`'\\''` 是 4 个字节 `' \\ ' '`：从被转义的那个字节之后才该找收尾引号，\
             否则找到的是转义序列自己的那个"
        );
        // 多字节字符要整体跳过，否则收尾引号会判在续字节上。
        assert_eq!(skip_char_literal("'中'".as_bytes(), 0), Some(5));
        // 生命周期不是字符字面量——判错会把后面整段当字面量吞掉。
        assert_eq!(skip_char_literal(b"'a: ", 0), None);
    }

    #[test]
    fn a_char_literal_does_not_swallow_later_calls() {
        // 上一条查契约，这条查它在真源码里的效果（两条都在，是因为契约对了不等于
        // 调用点用对了）。
        let src = "let v = ['\\'','a']; t!(\"after.tight.array\");";
        assert_eq!(keys(src), vec!["after.tight.array"]);
    }

    #[test]
    fn comments_between_the_paren_and_the_key_are_skipped() {
        // 人手写的 `t!(/* 复数 */ "k", count = n)` 是可能的；只跳空白会把它当成动态 key
        // 而静默放过——漏报是这个 lint 唯一的失效方向。
        assert_eq!(keys("t!(/*复数*/\"a.b\")"), vec!["a.b"]);
        assert_eq!(keys("t!(\n    // 说明\n    \"c.d\",\n)"), vec!["c.d"]);
        assert_eq!(keys("tr! /*奇怪但合法*/ (\"e.f\")"), vec!["e.f"]);
    }

    #[test]
    fn dynamic_keys_are_skipped_silently() {
        // 静态查不动的就明说查不到，别猜。注意第二行里 format! 的那个串也不能被误收。
        let src = r#"
            t!(key_var);
            t!(&format!("status.{i}"));
            tr!(some::CONST);
            t!("static.one");
        "#;
        assert_eq!(keys(src), vec!["static.one"]);
    }

    #[test]
    fn arguments_after_the_key_do_not_matter() {
        let src = "t!(\"a.b\", name = x, count = n); tr!(\"c.d\", 1, 2);";
        assert_eq!(keys(src), vec!["a.b", "c.d"]);
    }

    #[test]
    fn raw_string_keys_and_whitespace_are_accepted() {
        let src = "t!( r\"raw.key\" ); t!(\n    \"multi.line\"\n);";
        assert_eq!(keys(src), vec!["raw.key", "multi.line"]);
    }

    #[test]
    fn line_numbers_survive_multibyte_text() {
        // 行号按字节推进时若不跟着 UTF-8 走，中文注释会把行号算歪，报错就指不到地方。
        let src = "// 一行中文注释\n// 又一行\nt!(\"k\");\n";
        assert_eq!(keys_in_source(src)[0].line, 3);
    }

    // ---------------- check_usage：目录扫描 ----------------

    fn locales_with(keys: &[&str]) -> crate::i18n::Locales {
        // 顶层 key 必须写在任何表头**之前**，否则 `"a.b" = "x"` 会落进 `[meta]` 里
        // 被当成元数据吃掉——写反了不报错，只是那条 key 凭空消失。
        let body: String = keys.iter().map(|k| format!("\"{k}\" = \"x\"\n")).collect();
        crate::i18n::Locales::builder()
            .embed(&format!("{body}[meta]\nlocale = \"zh-CN\"\n"))
            .build()
    }

    fn temp_src_dir(tag: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "windui_lint_{tag}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("sub")).expect("建临时目录");
        for (name, body) in files {
            std::fs::write(d.join(name), body).expect("写源码");
        }
        d
    }

    #[test]
    fn a_key_without_any_translation_is_reported_with_file_and_line() {
        let dir = temp_src_dir(
            "missing",
            &[
                ("a.rs", "fn a() { let _ = t!(\"known.one\"); }\n"),
                ("sub/b.rs", "fn b() {\n    let _ = tr!(\"lost.one\");\n}\n"),
            ],
        );
        let p = check_usage(&dir, &locales_with(&["known.one"]));
        assert_eq!(p.len(), 1, "{p:#?}");
        assert!(p[0].contains("lost.one"), "{p:#?}");
        assert!(p[0].contains("b.rs"), "子目录也要扫到：{p:#?}");
        assert!(p[0].contains(":2"), "要带行号才跳得过去：{p:#?}");
    }

    #[test]
    fn any_language_providing_it_counts_as_translated() {
        // 只有 en 有的 key 也算有着落——其余语言缺的那份归 `check` 管，不是这里。
        let dir = temp_src_dir("anylang", &[("a.rs", "fn a() { t!(\"only.en\"); }\n")]);
        let locales = crate::i18n::Locales::builder()
            .embed("[meta]\nlocale = \"zh-CN\"\n[other]\nk = \"x\"\n")
            .embed("[meta]\nlocale = \"en\"\n[only]\nen = \"x\"\n")
            .build();
        assert_eq!(check_usage(&dir, &locales), Vec::<String>::new());
    }

    #[test]
    fn framework_keys_are_known_out_of_the_box() {
        // 下游若自己也用 `windui.*`（比如复用"复制"），不该被报成缺译文。
        let dir = temp_src_dir(
            "builtin",
            &[("a.rs", "fn a() { t!(\"windui.menu.copy\"); }\n")],
        );
        assert_eq!(
            check_usage(&dir, &crate::i18n::Locales::default()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_unreadable_directory_is_a_problem_not_a_panic() {
        let p = check_usage("绝对不存在的目录_windui_lint", &locales_with(&[]));
        assert_eq!(p.len(), 1, "{p:#?}");
        assert!(p[0].contains("读不了目录"), "{p:#?}");
    }
}
