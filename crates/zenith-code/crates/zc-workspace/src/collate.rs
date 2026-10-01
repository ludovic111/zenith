//! `String.prototype.localeCompare` with Node's default (root) ICU collation, used to sort the
//! index listing and browse results. Same as `zc_vcs::collate` (kept local so this crate does
//! not depend on the git driver).

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
    fn sorts_paths_like_node() {
        let mut paths = vec!["src/b.ts", "README.md", "src", "src/A.ts", ".github", "src-old", "_x", "10", "9"];
        paths.sort_by(|a, b| locale_compare(a, b));
        // Node 26: [...].sort((a, b) => a.localeCompare(b))
        assert_eq!(paths, vec!["_x", ".github", "10", "9", "README.md", "src", "src-old", "src/A.ts", "src/b.ts"]);
    }
}
