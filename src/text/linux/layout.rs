//! 折行：纯函数，输入字符与各自的前进宽度，输出每行的字符区间。
//!
//! 规则刻意从简（不是 UAX #14 的完整实现），覆盖 UI 文本的两类主流：
//! - 西文按词折：空白之后可断；一个词比整行还宽时退化为逐字符硬折，保证不溢出。
//! - CJK 逐字可断：汉字/假名/谚文前后都是断点；避头尾只做最常见的一层——闭合标点
//!   （，。、）」…）不落行首，开启标点（（「《…）不落行尾。

use std::ops::Range;

/// 按 `max` 宽折行。`adv[i]` 是第 i 个字符的前进宽度（已含与下一字的字距调整）。
///
/// 行首的空格被吃掉（续行不以空格开头）；行尾空格保留在区间里但不计宽（见 [`line_width`]）。
pub(crate) fn wrap(chars: &[char], adv: &[f32], max: f32) -> Vec<Range<usize>> {
    debug_assert_eq!(chars.len(), adv.len());
    let n = chars.len();
    let mut lines = Vec::new();
    let mut start = 0;
    if n == 0 {
        lines.push(0..0);
        return lines;
    }
    while start < n {
        let mut x = 0.0f32;
        let mut brk: Option<usize> = None;
        let mut i = start;
        while i < n {
            if i > start && can_break_before(chars[i - 1], chars[i]) {
                brk = Some(i);
            }
            let nx = x + adv[i];
            // 空白不触发折行：行尾空白本就不计宽，让它挂在行尾即可。
            if !chars[i].is_whitespace() && nx > max + 0.01 && i > start {
                break;
            }
            x = nx;
            i += 1;
        }
        if i >= n {
            lines.push(start..n);
            break;
        }
        let b = match brk {
            Some(b) if b > start => b,
            _ => i,
        };
        lines.push(start..b);
        start = b;
        while start < n && chars[start] == ' ' {
            start += 1;
        }
    }
    lines
}

/// 一行的可见宽度：不计行尾空白。
pub(crate) fn line_width(chars: &[char], adv: &[f32], r: Range<usize>) -> f32 {
    let mut end = r.end;
    while end > r.start && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    adv[r.start..end].iter().sum()
}

fn can_break_before(prev: char, cur: char) -> bool {
    if is_closing(cur) || is_opening(prev) {
        return false;
    }
    prev.is_whitespace() || is_cjk(prev) || is_cjk(cur)
}

/// 宽字符（逐字可断）的粗判：CJK 统一表意文字及扩展、假名、谚文、全角形式与 CJK 标点。
pub(crate) fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11FF
        | 0x2E80..=0x303F
        | 0x3040..=0x30FF
        | 0x3100..=0x31FF
        | 0x3200..=0x9FFF
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7FF
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFFEF
        | 0x20000..=0x3FFFF)
}

fn is_closing(c: char) -> bool {
    matches!(
        c,
        ',' | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | ')'
            | ']'
            | '}'
            | '，'
            | '。'
            | '、'
            | '；'
            | '：'
            | '！'
            | '？'
            | '）'
            | '」'
            | '』'
            | '》'
            | '〉'
            | '】'
            | '”'
            | '’'
            | '…'
            | '～'
    )
}

fn is_opening(c: char) -> bool {
    matches!(
        c,
        '(' | '[' | '{' | '（' | '「' | '『' | '《' | '〈' | '【' | '“' | '‘'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(s: &str, max: f32) -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        let adv = vec![10.0; chars.len()];
        wrap(&chars, &adv, max)
            .into_iter()
            .map(|r| chars[r].iter().collect())
            .collect()
    }

    #[test]
    fn latin_breaks_after_spaces_and_drops_leading_space() {
        assert_eq!(run("aa bb cc", 50.0), vec!["aa bb ", "cc"]);
    }

    #[test]
    fn overlong_word_is_hard_broken() {
        assert_eq!(run("abcdefg", 30.0), vec!["abc", "def", "g"]);
    }

    #[test]
    fn cjk_breaks_between_any_two_ideographs() {
        assert_eq!(run("一二三四五", 30.0), vec!["一二三", "四五"]);
    }

    #[test]
    fn closing_punctuation_never_starts_a_line() {
        // 「三」之后本可断，但下一个是「，」——断点退到「三」之前。
        assert_eq!(run("一二三，四", 30.0), vec!["一二", "三，四"]);
    }

    #[test]
    fn fits_on_one_line_without_breaking() {
        assert_eq!(run("abc", 30.0), vec!["abc"]);
        assert_eq!(run("", 30.0), vec![""]);
    }

    #[test]
    fn trailing_space_does_not_count_toward_width() {
        let chars: Vec<char> = "ab  ".chars().collect();
        let adv = vec![10.0; 4];
        assert_eq!(line_width(&chars, &adv, 0..4), 20.0);
    }
}
