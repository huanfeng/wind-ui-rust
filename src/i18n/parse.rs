//! 译文文件（TOML）→ 扁平 key 表。
//!
//! 格式规范见 `docs/i18n-design.md` §4。这里只讲两条**不看代码就想不到**的规则：
//!
//! 1. **表什么时候是命名空间、什么时候是变体表**：含 `other` 子键**且**所有值都是字符串
//!    的表，判为变体表（复数/select）；否则是命名空间，继续往下扁平。依据是 CLDR 规定
//!    `other` 恒存在。代价写在明处：命名空间里不能有名叫 `other` 的叶子——真要显示"其它"
//!    这个词，key 起作 `other_label`。
//! 2. **`[meta]` 不进 key 空间**：它是文件自身的元数据（语言 id / 回退 / 显示名），
//!    不是译文。

use std::collections::BTreeMap;

use super::format::Pattern;
use super::Entry;

/// 文件头 `[meta]`。
#[derive(Debug, Clone, Default)]
pub struct Meta {
    /// BCP-47 语言标签。**以此为准**，文件名只是提示。
    pub locale: Option<String>,
    /// 本语言的回退语言（覆盖全局 fallback）。
    pub fallback: Option<String>,
    /// 给设置页下拉直接用的显示名（"简体中文" / "English"）。
    /// 放在文件里而不是让应用写死一张语言名表——加一种语言只该动一个文件。
    pub name: Option<String>,
    /// 从右向左书写。**当前仅作标记，渲染层不消费**（见 `docs/i18n-design.md` §9）。
    pub rtl: bool,
}

/// 解析好的一份译文文件。
#[derive(Debug, Clone, Default)]
pub struct LangFile {
    pub meta: Meta,
    pub entries: BTreeMap<String, Entry>,
}

/// 解析失败的原因。整文件级失败只有一种（TOML 语法错）；条目级的怪东西一律跳过 + warn，
/// 不让一条坏值拖垮整个语言。
#[derive(Debug)]
pub struct ParseError(pub String);

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ParseError {}

pub fn parse(src: &str) -> Result<LangFile, ParseError> {
    let val: toml::Value = toml::from_str(src).map_err(|e| ParseError(e.to_string()))?;
    let table = match val {
        toml::Value::Table(t) => t,
        _ => return Err(ParseError("译文文件的顶层必须是表".into())),
    };
    let mut out = LangFile::default();
    for (k, v) in table {
        if k == "meta" {
            out.meta = parse_meta(&v);
            continue;
        }
        flatten(&k, &v, &mut out.entries);
    }
    Ok(out)
}

fn parse_meta(v: &toml::Value) -> Meta {
    let t = match v.as_table() {
        Some(t) => t,
        None => return Meta::default(),
    };
    Meta {
        locale: t.get("locale").and_then(|v| v.as_str()).map(str::to_string),
        fallback: t
            .get("fallback")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        name: t.get("name").and_then(|v| v.as_str()).map(str::to_string),
        rtl: t.get("rtl").and_then(|v| v.as_bool()).unwrap_or(false),
    }
}

/// 表是否为「变体表」（复数类别 / select 变体），判据见本模块头注。
fn is_variants(t: &toml::map::Map<String, toml::Value>) -> bool {
    t.contains_key("other") && t.values().all(|v| v.is_str())
}

fn flatten(path: &str, v: &toml::Value, out: &mut BTreeMap<String, Entry>) {
    match v {
        toml::Value::String(s) => {
            out.insert(path.to_string(), Entry::One(Pattern::compile(s)));
        }
        toml::Value::Array(items) => {
            let mut list = Vec::with_capacity(items.len());
            for it in items {
                match it.as_str() {
                    Some(s) => list.push(Pattern::compile(s)),
                    None => {
                        log::warn!("i18n: `{path}` 的数组含非字符串项，已跳过该项");
                    }
                }
            }
            out.insert(path.to_string(), Entry::List(list));
        }
        toml::Value::Table(t) if is_variants(t) => {
            let mut map = BTreeMap::new();
            for (k, v) in t {
                if let Some(s) = v.as_str() {
                    map.insert(k.clone().into_boxed_str(), Pattern::compile(s));
                }
            }
            out.insert(path.to_string(), Entry::Variants(map));
        }
        toml::Value::Table(t) => {
            for (k, v) in t {
                flatten(&format!("{path}.{k}"), v, out);
            }
        }
        // 数字/布尔/日期：译文里出现它们多半是写错了（漏了引号）。按字面量收下并 warn,
        // 比静默丢弃好——丢弃的表现是"这条文案怎么是空的"，查起来毫无线索。
        other => {
            log::warn!("i18n: `{path}` 的值不是字符串（{other}），已按字面量收下");
            out.insert(
                path.to_string(),
                Entry::One(Pattern::compile(&other.to_string())),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_tables_flatten_to_dotted_keys() {
        let f = parse("[menu]\ncut = \"剪切\"\ncopy = \"复制\"\n").expect("解析");
        assert_eq!(
            f.entries.keys().collect::<Vec<_>>(),
            vec!["menu.copy", "menu.cut"]
        );
        assert!(matches!(f.entries["menu.cut"], Entry::One(_)));
    }

    #[test]
    fn meta_is_read_and_kept_out_of_the_key_space() {
        let f = parse(
            "[meta]\nlocale = \"zh-CN\"\nname = \"简体中文\"\nfallback = \"en\"\n[a]\nb = \"c\"\n",
        )
        .expect("解析");
        assert_eq!(f.meta.locale.as_deref(), Some("zh-CN"));
        assert_eq!(f.meta.name.as_deref(), Some("简体中文"));
        assert_eq!(f.meta.fallback.as_deref(), Some("en"));
        assert!(!f.meta.rtl);
        assert_eq!(f.entries.keys().collect::<Vec<_>>(), vec!["a.b"]);
    }

    #[test]
    fn table_with_other_is_variants_not_a_namespace() {
        let f =
            parse("[file.selected]\none = \"1 file\"\nother = \"{count} files\"\n").expect("解析");
        match &f.entries["file.selected"] {
            Entry::Variants(m) => {
                assert_eq!(m.len(), 2);
                assert_eq!(m["other"].raw(), "{count} files");
            }
            e => panic!("应是变体表，得到 {e:?}"),
        }
        assert!(
            !f.entries.contains_key("file.selected.other"),
            "变体表不再逐条扁平，否则 key 集合会随语言的复数类别数而不同"
        );
    }

    #[test]
    fn table_without_other_stays_a_namespace() {
        // `[menu]` 只是命名空间——判据是"有没有 other"，不是"值是不是都为串"。
        let f = parse("[menu]\none = \"一\"\ntwo = \"二\"\n").expect("解析");
        assert!(f.entries.contains_key("menu.one"));
        assert!(f.entries.contains_key("menu.two"));
    }

    #[test]
    fn nested_table_inside_a_namespace_is_still_walked() {
        let f = parse("[a.b.c]\nd = \"x\"\n").expect("解析");
        assert!(f.entries.contains_key("a.b.c.d"));
    }

    #[test]
    fn string_arrays_become_lists() {
        let f = parse("[cal]\nweekdays = [\"一\", \"二\"]\n").expect("解析");
        match &f.entries["cal.weekdays"] {
            Entry::List(v) => assert_eq!(v.len(), 2),
            e => panic!("应是数组，得到 {e:?}"),
        }
    }

    #[test]
    fn syntax_error_fails_the_whole_file() {
        assert!(parse("这不是 TOML = = =").is_err());
    }

    #[test]
    fn non_string_scalar_is_kept_with_a_warning() {
        let f = parse("count = 3\n").expect("解析");
        assert_eq!(
            match &f.entries["count"] {
                Entry::One(p) => p.raw(),
                _ => "",
            },
            "3"
        );
    }
}
