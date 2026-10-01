//! `String.prototype.localeCompare` with the default (root / en-US) ICU collation, which the
//! TS driver uses to sort status files and same-age branches. Byte order would put `B` before
//! `a` and `/` after `-`, so the Rust driver sorts with the same UCA collator Node uses.

use std::cmp::Ordering;
use std::sync::OnceLock;

use icu_collator::options::CollatorOptions;
use icu_collator::{Collator, CollatorBorrowed};

fn collator() -> &'static CollatorBorrowed<'static> {
    static COLLATOR: OnceLock<CollatorBorrowed<'static>> = OnceLock::new();
    COLLATOR.get_or_init(|| Collator::try_new(Default::default(), CollatorOptions::default()).expect("the compiled root collation data is always available"))
}

/// `left.localeCompare(right)`.
pub fn locale_compare(left: &str, right: &str) -> Ordering {
    collator().compare(left, right)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_node_default_collation() {
        let mut names = vec!["b", "A", "a", "_x", "-x", "a-b", "A-a", "feature/x", "feature-x", "10", "9", "B", ".z", "Z"];
        names.sort_by(|a, b| locale_compare(a, b));
        // Node 26: [...].sort((a, b) => a.localeCompare(b))
        assert_eq!(
            names,
            vec!["_x", "-x", ".z", "10", "9", "a", "A", "A-a", "a-b", "b", "B", "feature-x", "feature/x", "Z"]
        );
    }
}
