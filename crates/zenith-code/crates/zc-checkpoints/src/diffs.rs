//! `checkpointing/Diffs.ts`: the file summary of a turn from `git diff --numstat -z`.

use std::sync::OnceLock;

use regex::Regex;

/// `TurnDiffFileSummary`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnDiffFileSummary {
    pub path: String,
    pub additions: u64,
    pub deletions: u64,
}

fn counts() -> &'static Regex {
    static COUNTS: OnceLock<Regex> = OnceLock::new();
    COUNTS.get_or_init(|| Regex::new(r"^(\d+|-)\t(\d+|-)\t").expect("valid regex"))
}

fn count(value: &str) -> u64 {
    // `Number("0123")` is 123; numstat never prints more than u64 lines.
    if value == "-" {
        0
    } else {
        value.parse().unwrap_or(0)
    }
}

/// `parseTurnDiffFilesFromNumstat(numstat)`: reads git's NUL-delimited numstat output without
/// decoding display paths, sorted by `localeCompare`.
pub fn parse_turn_diff_files_from_numstat(numstat: &str) -> Vec<TurnDiffFileSummary> {
    let records: Vec<&str> = numstat.split('\0').collect();
    let mut files = Vec::new();
    let mut index = 0;
    while index < records.len() {
        let record = records[index];
        if let Some(captures) = counts().captures(record) {
            let prefix_len = captures.get(0).map_or(0, |m| m.end());
            let mut path = record[prefix_len..].to_owned();
            if path.is_empty() {
                // Renames and copies use two more records: the source and destination.
                path = records.get(index + 2).map(|p| (*p).to_owned()).unwrap_or_default();
                index += 2;
            }
            if !path.is_empty() {
                files.push(TurnDiffFileSummary {
                    path,
                    additions: count(&captures[1]),
                    deletions: count(&captures[2]),
                });
            }
        }
        index += 1;
    }
    files.sort_by(|left, right| zc_vcs::collate::locale_compare(&left.path, &right.path));
    files
}

#[cfg(test)]
mod tests {
    //! Port of `checkpointing/Diffs.test.ts`.
    use super::*;

    fn file(path: &str, additions: u64, deletions: u64) -> TurnDiffFileSummary {
        TurnDiffFileSummary {
            path: path.into(),
            additions,
            deletions,
        }
    }

    #[test]
    fn returns_an_empty_list_when_no_files_changed() {
        assert_eq!(parse_turn_diff_files_from_numstat(""), vec![]);
    }

    #[test]
    fn sorts_files_and_preserves_addition_and_deletion_counts() {
        let numstat = ["0\t2\tsrc/b.ts", "2\t1\ta.txt", ""].join("\0");
        assert_eq!(parse_turn_diff_files_from_numstat(&numstat), vec![file("a.txt", 2, 1), file("src/b.ts", 0, 2)]);
    }

    #[test]
    fn uses_destination_paths_for_renames_and_copies() {
        let numstat = [
            "0\t0\t",
            "src/old.ts",
            "src/new.ts",
            "2\t1\t",
            "src/source.ts",
            "src/copied.ts",
            "1\t0\tother.ts",
            "",
        ]
        .join("\0");
        assert_eq!(
            parse_turn_diff_files_from_numstat(&numstat),
            vec![file("other.ts", 1, 0), file("src/copied.ts", 2, 1), file("src/new.ts", 0, 0)]
        );
    }

    #[test]
    fn keeps_binary_files_and_empty_files_with_zero_line_changes() {
        let numstat = ["-\t-\timage.png", "0\t0\tempty.txt", ""].join("\0");
        assert_eq!(
            parse_turn_diff_files_from_numstat(&numstat),
            vec![file("empty.txt", 0, 0), file("image.png", 0, 0)]
        );
    }

    #[test]
    fn preserves_unicode_tabs_line_endings_and_spaces_in_paths() {
        let path = " café\tline\r\nname.txt ";
        let numstat = format!("3\t2\t\0old\tname\n.txt\0{path}\0");
        assert_eq!(parse_turn_diff_files_from_numstat(&numstat), vec![file(path, 3, 2)]);
        assert_eq!(parse_turn_diff_files_from_numstat(&format!("1\t0\t{path}\0")), vec![file(path, 1, 0)]);
    }
}
