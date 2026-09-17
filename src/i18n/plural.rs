//! 复数/量词类别选择（CLDR 规则子集，整数部分）。
//!
//! # 为什么手写规则表而不是引 `icu`
//!
//! `icu_plurals` 会带进整套 CLDR 数据加载机制（provider / 数据包 / 版本对齐），而本库
//! 的全部需求是「给定语言和一个整数，回答 one/few/many/other」。手写表约 100 行、零依赖，
//! 且能逐语言用 CLDR 官方样例数字写断言——正确性有独立信源，不靠"跑一遍看着对"。
//!
//! # 覆盖范围与退化行为
//!
//! 收录下面这些语言族。**未收录的语言退化为 `n == 1 → One，否则 Other`**——那是最常见
//! 的形态，且译文缺该类别时还会再落到 `other`（见 `Catalog::format` 的变体选择），所以最坏
//! 情况是"复数没选对"，不是"显示不出来"。
//!
//! 只看整数：`count` 参数的类型是整数（[`ArgValue::Int`](super::ArgValue::Int)），
//! 小数复数（英语 `1.0 files`）不在射程内。

/// CLDR 复数类别。变体表的子键名即其小写形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Zero,
    One,
    Two,
    Few,
    Many,
    Other,
}

impl Category {
    /// 变体表里的子键名。
    pub fn key(self) -> &'static str {
        match self {
            Category::Zero => "zero",
            Category::One => "one",
            Category::Two => "two",
            Category::Few => "few",
            Category::Many => "many",
            Category::Other => "other",
        }
    }
}

/// 取语言标签的基础语言（`zh-Hans-CN` → `zh`），并转小写。
fn base(lang: &str) -> String {
    lang.split(['-', '_'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 按语言与数量选类别。
///
/// 负数按其绝对值判定：CLDR 的操作数 `n` 定义为绝对值。
pub fn select(lang: &str, count: i64) -> Category {
    let n = count.unsigned_abs();
    match base(lang).as_str() {
        // ---- 无复数变化：一条 other 就够 ----
        "zh" | "ja" | "ko" | "vi" | "th" | "id" | "ms" | "my" | "km" | "lo" | "bo" | "dz"
        | "ii" | "yo" | "to" => Category::Other,

        // ---- 法语/葡萄牙语：0 与 1 同属 one ----
        "fr" | "pt" | "hy" | "ff" | "kab" => {
            if n <= 1 {
                Category::One
            } else {
                Category::Other
            }
        }

        // ---- 俄语族：按个位/末两位分 one/few/many ----
        "ru" | "uk" | "be" => {
            let (d1, d2) = (n % 10, n % 100);
            if d1 == 1 && d2 != 11 {
                Category::One
            } else if (2..=4).contains(&d1) && !(12..=14).contains(&d2) {
                Category::Few
            } else {
                Category::Many
            }
        }

        // ---- 波兰语：one 只给 1，其余同俄语族 ----
        "pl" => {
            let (d1, d2) = (n % 10, n % 100);
            if n == 1 {
                Category::One
            } else if (2..=4).contains(&d1) && !(12..=14).contains(&d2) {
                Category::Few
            } else {
                Category::Many
            }
        }

        // ---- 捷克/斯洛伐克：1 / 2-4 / 其余（many 只用于小数，本表不产出）----
        "cs" | "sk" => {
            if n == 1 {
                Category::One
            } else if (2..=4).contains(&n) {
                Category::Few
            } else {
                Category::Other
            }
        }

        // ---- 阿拉伯语：六类全用 ----
        "ar" => {
            let d2 = n % 100;
            match n {
                0 => Category::Zero,
                1 => Category::One,
                2 => Category::Two,
                _ if (3..=10).contains(&d2) => Category::Few,
                _ if (11..=99).contains(&d2) => Category::Many,
                _ => Category::Other,
            }
        }

        // ---- 立陶宛语 ----
        "lt" => {
            let (d1, d2) = (n % 10, n % 100);
            if d1 == 1 && !(11..=19).contains(&d2) {
                Category::One
            } else if (2..=9).contains(&d1) && !(11..=19).contains(&d2) {
                Category::Few
            } else {
                Category::Other
            }
        }

        // ---- 其余（含 en/de/es/it/nl/sv/tr/… 以及未收录语言）：n == 1 ----
        _ => {
            if n == 1 {
                Category::One
            } else {
                Category::Other
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Category::*;

    /// 断言一批"语言 + 数字 → 类别"。样例数字取自 CLDR 规则文档里各语言的示例集合，
    /// 不是从本文件的实现反推的——这是这张表唯一的独立信源。
    fn check(lang: &str, cases: &[(i64, Category)]) {
        for (n, want) in cases {
            assert_eq!(
                select(lang, *n),
                *want,
                "{lang} 的 {n} 应属 {:?}",
                want.key()
            );
        }
    }

    #[test]
    fn chinese_family_has_only_other() {
        check("zh-CN", &[(0, Other), (1, Other), (2, Other), (11, Other)]);
        check("ja", &[(1, Other)]);
    }

    #[test]
    fn english_splits_one_and_other() {
        check("en", &[(0, Other), (1, One), (2, Other), (21, Other)]);
        check("de-DE", &[(1, One), (2, Other)]);
    }

    #[test]
    fn french_counts_zero_as_one() {
        check("fr", &[(0, One), (1, One), (2, Other)]);
        check("pt-BR", &[(0, One), (1, One), (2, Other)]);
    }

    #[test]
    fn russian_uses_one_few_many() {
        // CLDR ru 样例：one 1,21,31…；few 2-4,22-24…；many 0,5-20,25-30…
        check(
            "ru",
            &[
                (1, One),
                (21, One),
                (101, One),
                (11, Many),
                (2, Few),
                (4, Few),
                (22, Few),
                (12, Many),
                (14, Many),
                (0, Many),
                (5, Many),
                (25, Many),
            ],
        );
    }

    #[test]
    fn polish_gives_one_only_to_exactly_one() {
        check(
            "pl",
            &[
                (1, One),
                (21, Many),
                (2, Few),
                (22, Few),
                (12, Many),
                (5, Many),
            ],
        );
    }

    #[test]
    fn czech_has_few_for_two_to_four() {
        check(
            "cs",
            &[(1, One), (2, Few), (4, Few), (5, Other), (0, Other)],
        );
    }

    #[test]
    fn arabic_uses_all_six() {
        check(
            "ar",
            &[
                (0, Zero),
                (1, One),
                (2, Two),
                (3, Few),
                (10, Few),
                (103, Few),
                (11, Many),
                (99, Many),
                (100, Other),
                (102, Other),
            ],
        );
    }

    #[test]
    fn lithuanian_excludes_the_teens() {
        check(
            "lt",
            &[
                (1, One),
                (21, One),
                (11, Other),
                (2, Few),
                (9, Few),
                (19, Other),
                (10, Other),
            ],
        );
    }

    #[test]
    fn unknown_language_degrades_to_one_other() {
        check("xx-YY", &[(1, One), (0, Other), (2, Other)]);
    }

    #[test]
    fn negative_counts_use_absolute_value() {
        assert_eq!(select("en", -1), One);
        assert_eq!(select("ru", -2), Few);
    }

    #[test]
    fn script_and_region_subtags_are_ignored() {
        assert_eq!(select("zh-Hans-CN", 1), Other);
        assert_eq!(select("en_US", 1), One);
        assert_eq!(select("EN", 1), One, "大小写不敏感");
    }
}
