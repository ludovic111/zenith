//! `String.prototype.localeCompare` without arguments, as Node's ICU root collation orders
//! the strings this crate compares (ISO timestamps and ids: ASCII letters, digits and a little
//! punctuation). `ProjectionThreadProposedPlanRepository.hasActionableByThreadId` picks the
//! latest plan with it, deliberately not in SQLite byte order.
//!
//! The approximation: both strings are decomposed (NFD, so `é` and `e\u{301}` are equal, as
//! ICU says); compare the primary keys of the whole strings first (white space, then
//! punctuation and symbols in CLDR root order, then digits, then letters case-insensitively,
//! combining marks ignored), then the accents, then case (lower before upper), then code
//! points. Other characters sort after ASCII letters by code point.

use std::cmp::Ordering;

use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};

/// CLDR root order of the ASCII punctuation and symbols (they all sort before digits).
const PUNCTUATION_ORDER: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";

fn primary(c: char) -> (u8, u32) {
    if c.is_whitespace() {
        return (0, c as u32);
    }
    if let Some(index) = PUNCTUATION_ORDER.find(c) {
        return (1, index as u32);
    }
    if c.is_ascii_digit() {
        return (2, c as u32);
    }
    if c.is_ascii_alphabetic() {
        return (3, c.to_ascii_lowercase() as u32);
    }
    (4, c as u32)
}

fn tertiary(c: char) -> u8 {
    // Lower case first, as ICU's default tertiary order.
    if c.is_ascii_uppercase() {
        1
    } else {
        0
    }
}

pub fn locale_compare(left: &str, right: &str) -> Ordering {
    let left: Vec<char> = left.nfd().collect();
    let right: Vec<char> = right.nfd().collect();
    let base = |chars: &Vec<char>| -> Vec<char> { chars.iter().copied().filter(|c| !is_combining_mark(*c)).collect() };
    let (left_base, right_base) = (base(&left), base(&right));
    let primary_order = left_base.iter().map(|c| primary(*c)).cmp(right_base.iter().map(|c| primary(*c)));
    if primary_order != Ordering::Equal {
        return primary_order;
    }
    let marks = |chars: &Vec<char>| -> Vec<char> { chars.iter().copied().filter(|c| is_combining_mark(*c)).collect() };
    let secondary_order = marks(&left).cmp(&marks(&right));
    if secondary_order != Ordering::Equal {
        return secondary_order;
    }
    let tertiary_order = left_base.iter().map(|c| tertiary(*c)).cmp(right_base.iter().map(|c| tertiary(*c)));
    if tertiary_order != Ordering::Equal {
        return tertiary_order;
    }
    // Canonically equivalent strings are equal, as ICU says.
    left.cmp(&right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orders_like_icu_root() {
        assert_eq!(locale_compare("a", "B"), Ordering::Less);
        assert_eq!(locale_compare("a", "A"), Ordering::Less);
        assert_eq!(locale_compare("plan-2", "plan_2"), Ordering::Greater);
        assert_eq!(locale_compare("plan-10", "plan-9"), Ordering::Less);
        assert_eq!(locale_compare("Z", "a"), Ordering::Greater);
        assert_eq!(locale_compare("2026-01-01T00:00:00.000Z", "2026-01-01T00:00:00.001Z"), Ordering::Less);
        assert_eq!(locale_compare("same", "same"), Ordering::Equal);
        assert_eq!(locale_compare("plan-\u{e9}", "plan-e\u{301}"), Ordering::Equal);
        assert_eq!(locale_compare("plan-e", "plan-\u{e9}"), Ordering::Less);
        assert_eq!(locale_compare("2026-03-24T00:00:00+00:00", "2026-03-24T00:00:00-01:00"), Ordering::Greater);
    }
}
