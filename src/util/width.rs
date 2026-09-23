//! 显示宽度裁剪（NFR-06 / FR-01 验收 3）。
//!
//! 全项目唯一的「按显示宽度切字符串」入口：CJK 是双宽、emoji 宽度可变的字符，
//! 用 `str::len()` 或 `chars().count()` 裁剪必然错行。这里统一走 `unicode-width`，
//! 并且**绝不切出半个字符**。

use std::borrow::Cow;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// 字符串的显示宽度（列数）。
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// 按显示宽度裁剪，超出部分舍弃（不追加省略号，调用方决定）。
pub fn clip(text: &str, max_cols: usize) -> Cow<'_, str> {
    if display_width(text) <= max_cols {
        return Cow::Borrowed(text);
    }

    let mut used = 0usize;
    let mut end = 0usize;
    for (idx, ch) in text.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max_cols {
            break;
        }
        used += w;
        end = idx + ch.len_utf8();
    }
    Cow::Borrowed(&text[..end])
}

/// 裁到 `max_cols`，若确有截断则在末尾补 `…`（省略号本身占一列）。
pub fn clip_with_ellipsis(text: &str, max_cols: usize) -> String {
    if display_width(text) <= max_cols {
        return text.to_string();
    }
    if max_cols == 0 {
        return String::new();
    }
    let mut out = clip(text, max_cols.saturating_sub(1)).into_owned();
    out.push('…');
    out
}

/// 右侧补空格到指定显示宽度；已超宽则原样返回（不截断）。
pub fn pad_right(text: &str, cols: usize) -> String {
    let w = display_width(text);
    if w >= cols {
        return text.to_string();
    }
    let mut out = text.to_string();
    out.push_str(&" ".repeat(cols - w));
    out
}

/// 把控制字符替换为 `?`。
///
/// 会话名理论上来自 `\S+` 不会含空白，但它是**用户可控输入**；
/// 一旦混入 `\t`/`\x1b` 就会破坏列对齐甚至注入转义序列（FR-22 要求无 ANSI 输出）。
pub fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_is_double_width() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("会话"), 4);
        assert_eq!(display_width("a会b"), 4);
    }

    #[test]
    fn clip_never_splits_a_character() {
        assert_eq!(clip("会话列表", 5), "会话");
        assert_eq!(clip("会话列表", 4), "会话");
        assert_eq!(clip("会话列表", 3), "会");
        assert_eq!(clip("abcdef", 3), "abc");
        assert_eq!(clip("abc", 10), "abc");
        assert_eq!(clip("会话", 0), "");
    }

    #[test]
    fn ellipsis_respects_budget() {
        assert_eq!(clip_with_ellipsis("abcdef", 4), "abc…");
        assert_eq!(clip_with_ellipsis("abc", 4), "abc");
        // 双宽字符：3 列预算 → 留 1 列放省略号，余下 2 列刚好装一个汉字。
        assert_eq!(clip_with_ellipsis("会话列表", 3), "会…");
        assert_eq!(clip_with_ellipsis("会话列表", 2), "…");
        assert_eq!(clip_with_ellipsis("会话列表", 1), "…");
        assert_eq!(clip_with_ellipsis("会话列表", 6), "会话…");
    }

    #[test]
    fn pad_right_counts_columns() {
        assert_eq!(pad_right("会话", 6), "会话  ");
        assert_eq!(pad_right("abc", 3), "abc");
        assert_eq!(pad_right("abcd", 3), "abcd");
    }

    #[test]
    fn sanitize_kills_control_and_escape_sequences() {
        assert_eq!(sanitize("a\tb\nc"), "a?b?c");
        assert_eq!(sanitize("x\x1b[31mred"), "x?[31mred");
    }
}
