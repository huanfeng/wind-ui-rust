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
}
