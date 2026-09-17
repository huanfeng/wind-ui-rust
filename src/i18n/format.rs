//! 译文值的**预编译**与格式化。
//!
//! 目录加载时把每条值从 `"已把 {name} 移到 {dest}"` 编成 `Vec<Piece>`，paint 期只做拼接。
//! 这是「每帧现取」能便宜的前提：扫描花括号是 O(字符数) 的活，不能每帧对每个控件重做一遍。

use std::borrow::Cow;

use super::ArgValue;

/// 预编译后的一段。
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Piece {
    /// 字面量。
    Lit(Box<str>),
    /// 命名占位 `{name}`。
    Named(Box<str>),
    /// 位置占位 `{0}`。
    Pos(usize),
}

/// 一条译文值（已预编译）。
#[derive(Debug, Clone)]
pub struct Pattern {
    pieces: Vec<Piece>,
    /// 原始串。给 lint / 报错用——只留 pieces 的话，错误信息里没法把原文打出来。
    raw: Box<str>,
}

impl Pattern {
    /// 编译一条值。
    ///
    /// 语法只有三条，刻意小：
    /// - `{name}` 命名占位，`{0}` 位置占位（全数字即位置）；
    /// - `{{` / `}}` 转义出字面花括号；
    /// - 其余一律字面量。**未闭合的 `{` 按字面量处理**而不是报错——译者写漏一个括号
    ///   不该让整个语言加载失败（见 `Locales` 的「单文件失败只丢那一个文件」原则，
    ///   这里更进一步：单条值写坏只影响那一条）。
    pub(crate) fn compile(raw: &str) -> Self {
        let mut pieces = Vec::new();
        let mut lit = String::new();
        let mut it = raw.char_indices().peekable();
        while let Some((i, c)) = it.next() {
            match c {
                '{' if matches!(it.peek(), Some((_, '{'))) => {
                    it.next();
                    lit.push('{');
                }
                '}' if matches!(it.peek(), Some((_, '}'))) => {
                    it.next();
                    lit.push('}');
                }
                '{' => {
                    // 找配对的 '}'。找不到就把这个 '{' 当字面量。
                    let rest = &raw[i + c.len_utf8()..];
                    match rest.find('}') {
                        Some(end) => {
                            let name = rest[..end].trim();
                            if !lit.is_empty() {
                                pieces.push(Piece::Lit(std::mem::take(&mut lit).into()));
                            }
                            if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) {
                                // 全数字 → 位置占位。溢出 usize 的下标当命名处理（必然取不到，
                                // debug 下会原样显形），不 panic。
                                match name.parse::<usize>() {
                                    Ok(n) => pieces.push(Piece::Pos(n)),
                                    Err(_) => pieces.push(Piece::Named(name.into())),
                                }
                            } else {
                                pieces.push(Piece::Named(name.into()));
                            }
                            // 跳过已消费的 name + '}'
                            for _ in 0..rest[..=end].chars().count() {
                                it.next();
                            }
                        }
                        None => lit.push('{'),
                    }
                }
                _ => lit.push(c),
            }
        }
        if !lit.is_empty() {
            pieces.push(Piece::Lit(lit.into()));
        }
        Self {
            pieces,
            raw: raw.into(),
        }
    }

    /// 原始串（未代入参数）。
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// 本条用到的占位符名字（位置占位记作 `"0"`/`"1"`），按出现顺序去重。
    ///
    /// 给一致性测试用：同一 key 在各语言里的占位符集合必须相等，否则就是
    /// 「`en` 写 `{count}`、`zh-CN` 写 `{num}`」这种跑起来才发现是空串的静默错。
    pub fn placeholders(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in &self.pieces {
            let name = match p {
                Piece::Lit(_) => continue,
                Piece::Named(n) => n.to_string(),
                Piece::Pos(i) => i.to_string(),
            };
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    /// 代入参数。
    ///
    /// 缺参数的处理分档，与 key miss 同一套哲学（见 `Catalog::lookup`）：
    /// - debug：原样保留 `{name}`——看得见才改得掉；
    /// - release：替换为空串——不拿花括号吓用户。
    pub(crate) fn format(
        &self,
        named: &[(Cow<'static, str>, ArgValue)],
        pos: &[ArgValue],
    ) -> String {
        // 无占位的快路径：绝大多数译文属此类，不必进拼接循环。
        if let [Piece::Lit(s)] = self.pieces.as_slice() {
            return s.to_string();
        }
        if self.pieces.is_empty() {
            return String::new();
        }
        let mut out = String::with_capacity(self.raw.len() + 8);
        for p in &self.pieces {
            match p {
                Piece::Lit(s) => out.push_str(s),
                Piece::Named(n) => match named.iter().find(|(k, _)| k.as_ref() == n.as_ref()) {
                    Some((_, v)) => v.render_into(&mut out),
                    None => missing(&mut out, n),
                },
                Piece::Pos(i) => match pos.get(*i) {
                    Some(v) => v.render_into(&mut out),
                    None => missing(&mut out, &i.to_string()),
                },
            }
        }
        out
    }
}

fn missing(out: &mut String, name: &str) {
    if cfg!(debug_assertions) {
        out.push('{');
        out.push_str(name);
        out.push('}');
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fmt(raw: &str, named: &[(&'static str, ArgValue)]) -> String {
        let args: Vec<(Cow<'static, str>, ArgValue)> = named
            .iter()
            .map(|(k, v)| (Cow::Borrowed(*k), v.clone()))
            .collect();
        Pattern::compile(raw).format(&args, &[])
    }

    #[test]
    fn plain_text_is_one_literal_piece() {
        let p = Pattern::compile("复制");
        assert_eq!(p.pieces, vec![Piece::Lit("复制".into())]);
        assert_eq!(p.format(&[], &[]), "复制");
    }

    #[test]
    fn named_placeholder_is_substituted() {
        assert_eq!(
            fmt(
                "已把 {name} 移到 {dest}",
                &[
                    ("name", ArgValue::from("a.txt")),
                    ("dest", ArgValue::from("下载")),
                ]
            ),
            "已把 a.txt 移到 下载"
        );
    }

    #[test]
    fn named_args_are_order_independent() {
        // 译文调换语序时不必动代码——这正是命名占位相对位置占位的全部意义。
        let args = [
            ("name", ArgValue::from("a.txt")),
            ("dest", ArgValue::from("下载")),
        ];
        assert_eq!(fmt("{dest} ← {name}", &args), "下载 ← a.txt");
    }

    #[test]
    fn positional_placeholder_is_substituted() {
        let p = Pattern::compile("{0} / {1}");
        assert_eq!(
            p.format(&[], &[ArgValue::from(3i64), ArgValue::from(10i64)]),
            "3 / 10"
        );
    }

    #[test]
    fn braces_are_escaped_by_doubling() {
        assert_eq!(fmt("{{name}} 是字面量", &[]), "{name} 是字面量");
        assert_eq!(fmt("{{{name}}}", &[("name", ArgValue::from("x"))]), "{x}");
    }

    #[test]
    fn unclosed_brace_stays_literal() {
        // 译者写漏括号不该让这条值变成解析错误。
        assert_eq!(fmt("30{% 完成", &[]), "30{% 完成");
    }

    #[test]
    fn missing_arg_is_visible_in_debug() {
        let got = fmt("已选 {count} 项", &[]);
        if cfg!(debug_assertions) {
            assert_eq!(got, "已选 {count} 项", "debug 下缺参数应原样显形");
        } else {
            assert_eq!(got, "已选  项");
        }
    }

    #[test]
    fn extra_args_are_ignored() {
        // 译文少用一个变量是译者的正当选择，不该是错误。
        assert_eq!(fmt("完成", &[("name", ArgValue::from("x"))]), "完成");
    }

    #[test]
    fn placeholders_are_listed_deduped_in_order() {
        let p = Pattern::compile("{a} {b} {a} {0}");
        assert_eq!(p.placeholders(), vec!["a", "b", "0"]);
    }

    #[test]
    fn multibyte_text_around_placeholder_does_not_split_chars() {
        // `{` 的定位用字节下标、消费用字符数——两者混用会在中文译文上切碎字符。
        assert_eq!(
            fmt("你好{name}，欢迎", &[("name", ArgValue::from("世界"))]),
            "你好世界，欢迎"
        );
    }
}
