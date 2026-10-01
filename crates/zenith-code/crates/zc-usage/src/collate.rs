//! `String.prototype.localeCompare` as V8 runs it (ICU root collation), for the strings the
//! usage code sorts: model names, days, file names.
//!
//! Exact for printable ASCII: punctuation and symbols sort before digits, digits before
//! letters, letters compare case-insensitively first and lowercase-first on a tie, and
//! control characters are ignorable. Other characters sort after `z` by code point (no
//! accent folding), an approximation that never applies to the ids involved here.

use std::cmp::Ordering;

/// ICU root order of printable ASCII (`" _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0-9aAbB…zZ"`).
const ASCII_ORDER: &[u8] = b" _-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$0123456789aAbBcCdDeEfFgGhHiIjJkKlLmMnNoOpPqQrRsStTuUvVwWxXyYzZ";

const fn ascii_weights() -> [(u16, u8); 128] {
    let mut table = [(0u16, 0u8); 128];
    let mut index = 0;
    let mut primary = 1u16;
    while index < ASCII_ORDER.len() {
        let byte = ASCII_ORDER[index];
        let upper = byte.is_ascii_uppercase();
        if upper {
            // Same primary as the lowercase letter just before it, tertiary 1.
            table[byte as usize] = (primary - 1, 1);
        } else {
            table[byte as usize] = (primary, 0);
            primary += 1;
        }
        index += 1;
    }
    table
}

const WEIGHTS: [(u16, u8); 128] = ascii_weights();

/// (primary, tertiary), or `None` for an ignorable character.
fn weight(ch: char) -> Option<(u32, u8)> {
    let code = ch as u32;
    if code < 128 {
        let (primary, tertiary) = WEIGHTS[code as usize];
        return (primary != 0).then_some((u32::from(primary), tertiary));
    }
    if ch.is_control() {
        return None;
    }
    Some((1000 + code, 0))
}

/// `a.localeCompare(b)` as an ordering.
pub fn locale_compare(a: &str, b: &str) -> Ordering {
    let mut tertiary = Ordering::Equal;
    let mut left = a.chars().filter_map(weight);
    let mut right = b.chars().filter_map(weight);
    loop {
        match (left.next(), right.next()) {
            (None, None) => return tertiary,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some((p1, t1)), Some((p2, t2))) => match p1.cmp(&p2) {
                Ordering::Equal => {
                    if tertiary == Ordering::Equal {
                        tertiary = t1.cmp(&t2);
                    }
                }
                other => return other,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_icu_order() {
        let mut values = vec!["ab-", "Ab", "aB", "ab", "a-b", "a_b", "a b", "ab1", "f", "E", "e"];
        values.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(values, ["a b", "a_b", "a-b", "ab", "aB", "Ab", "ab-", "ab1", "e", "E", "f"]);
        let mut ascii: Vec<String> = (32u8..127).map(|b| (b as char).to_string()).collect();
        ascii.sort_by(|a, b| locale_compare(a, b));
        assert_eq!(ascii.concat().as_bytes(), ASCII_ORDER);
        assert_eq!(locale_compare("a\u{0}b", "ab"), Ordering::Equal);
        assert_eq!(locale_compare("claude-opus-4-5", "claude-opus-4.5"), Ordering::Less);
    }
}
