//! JavaScript string semantics the ported code depends on: `String.prototype.trim` and `\s`
//! (a different white-space set than Rust's), UTF-16 lengths and slices (`length`, `slice`,
//! `indexOf` count UTF-16 code units), `Number.prototype.toLocaleString` and
//! `String.prototype.localeCompare`.

use std::cmp::Ordering;

/// A character of JavaScript's `\s` class, which is also what `trim` removes: the ECMAScript
/// `WhiteSpace` and `LineTerminator` productions.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0b}' | '\u{0c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `String.prototype.trim`.
pub fn trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// `String.prototype.trimEnd`.
pub fn trim_end(value: &str) -> &str {
    value.trim_end_matches(is_js_whitespace)
}

/// `value.replace(/\s+/g, " ")`.
pub fn collapse_whitespace(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut in_space = false;
    for c in value.chars() {
        if is_js_whitespace(c) {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            out.push(c);
            in_space = false;
        }
    }
    out
}

/// `value.length`: UTF-16 code units.
pub fn utf16_len(value: &str) -> usize {
    value.chars().map(char::len_utf16).sum()
}

/// `value.slice(start, end)` with already clamped, non-negative UTF-16 offsets. A surrogate pair
/// cut in half cannot be represented in a Rust string: the lone half is dropped (JS keeps it
/// and `JSON.stringify` writes it as an escape).
pub fn utf16_slice(value: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut offset = 0;
    for c in value.chars() {
        let width = c.len_utf16();
        if offset >= end {
            break;
        }
        if offset >= start && offset + width <= end {
            out.push(c);
        }
        offset += width;
    }
    out
}

/// `haystack.indexOf(needle)` in UTF-16 code units, or -1.
pub fn utf16_index_of(haystack: &str, needle: &str) -> i64 {
    match haystack.find(needle) {
        Some(byte_index) => utf16_len(&haystack[..byte_index]) as i64,
        None => -1,
    }
}

/// `n.toLocaleString()` for a non-negative integer in the `en-US` locale Node defaults to.
pub fn to_locale_string(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `left.localeCompare(right)` (ICU root collation, approximated by zc-db).
pub fn locale_compare(left: &str, right: &str) -> Ordering {
    zc_db::collate::locale_compare(left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_like_javascript() {
        assert_eq!(trim("\u{feff} a b\u{3000}"), "a b");
        // NEL is white space for Rust but not for JavaScript.
        assert_eq!(trim("\u{85}a"), "\u{85}a");
        assert_eq!(collapse_whitespace("a \n\t b  c"), "a b c");
        assert_eq!(trim_end("x  "), "x");
    }

    #[test]
    fn counts_utf16() {
        assert_eq!(utf16_len("a😀b"), 4);
        assert_eq!(utf16_slice("a😀b", 0, 3), "a😀");
        assert_eq!(utf16_slice("a😀b", 0, 2), "a");
        assert_eq!(utf16_slice("a😀b", 3, 4), "b");
        assert_eq!(utf16_index_of("a😀bc", "bc"), 3);
        assert_eq!(utf16_index_of("abc", "x"), -1);
    }

    #[test]
    fn formats_like_to_locale_string() {
        assert_eq!(to_locale_string(7), "7");
        assert_eq!(to_locale_string(1234), "1,234");
        assert_eq!(to_locale_string(1234567), "1,234,567");
        assert_eq!(to_locale_string(100), "100");
    }
}
