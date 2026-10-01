//! `parseDiffFileRevisions` of `pullRequest/bitbucketDiffRevisions.ts`, which the Forgejo
//! provider uses too: the head blob id of each file from a unified patch's `index` lines.
//!
//! Ported here because the Forgejo provider needs it and `bitbucket/diff_revisions.rs` is owned
//! by the Bitbucket port; one of the two copies can go once both have landed.

use super::super::gitlab::util::{unquote_git_patch_path, OrderedMap};

const ENTRY: &str = "diff --git ";

struct Entry {
    old_path: Option<String>,
    new_path: Option<String>,
    deleted: bool,
    revision: Option<String>,
    /// Past the first hunk header every line is content, and content can start like a header.
    in_body: bool,
}

/// Where a quoted name closes, given git escapes every quote the name itself holds.
fn quoted_end(rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut at = 1;
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            b'"' => return Some(at),
            _ => at += 1,
        }
    }
    None
}

/// `a/x`/`b/x` on a `---`/`+++` line; `/dev/null` marks the side with no file. The first literal
/// tab ends the name (git ends a name holding a space with one, and escapes a tab of its own).
fn side_path(rest: &str, prefix: &str) -> Option<String> {
    let token = rest.split('\t').next().unwrap_or(rest);
    if token == "/dev/null" {
        return None;
    }
    let path = unquote_git_patch_path(token);
    Some(path.strip_prefix(prefix).map(str::to_owned).unwrap_or(path))
}

fn header_side(token: &str, prefix: &str) -> Option<String> {
    unquote_git_patch_path(token).strip_prefix(prefix).map(str::to_owned)
}

/// The two names on a `diff --git` line. Unquoted names have no delimiter, so the split leaving
/// both sides equal wins (a rename states its names on separate lines); a quoted name ends at its
/// own closing quote.
fn header_paths(rest: &str) -> (Option<String>, Option<String>) {
    if rest.starts_with('"') {
        let Some(end) = quoted_end(rest) else { return (None, None) };
        if rest.as_bytes().get(end + 1) != Some(&b' ') {
            return (None, None);
        }
        return (header_side(&rest[..=end], "a/"), header_side(&rest[end + 2..], "b/"));
    }
    if rest.ends_with('"') {
        let opens = rest.find('"').unwrap_or(0);
        if opens < 1 || rest.as_bytes()[opens - 1] != b' ' {
            return (None, None);
        }
        return (header_side(&rest[..opens - 1], "a/"), header_side(&rest[opens..], "b/"));
    }
    if !rest.starts_with("a/") {
        return (None, None);
    }
    let splits: Vec<usize> = rest.match_indices(" b/").map(|(at, _)| at).collect();
    let chosen = splits
        .iter()
        .copied()
        .find(|&at| rest[2..at] == rest[at + 3..])
        .or_else(|| (splits.len() == 1).then(|| splits[0]));
    match chosen {
        Some(at) => (Some(rest[2..at].to_owned()), Some(rest[at + 3..].to_owned())),
        None => (None, None),
    }
}

/// The right-hand id of `index <before>..<after> <mode>`.
fn head_revision(rest: &str) -> Option<String> {
    let gap = rest.find("..")?;
    let after = &rest[gap + 2..];
    let head = after.split(' ').next().unwrap_or(after);
    (!head.is_empty()).then(|| head.to_owned())
}

/// `parseDiffFileRevisions`: what the head has of each file, keyed by the head's name (the old
/// one for a deletion). An entry with no `index` line is left out.
pub fn parse_diff_file_revisions(patch: &str) -> OrderedMap<String> {
    let mut revisions = OrderedMap::new();
    let mut entry: Option<Entry> = None;
    let close = |entry: &mut Option<Entry>, revisions: &mut OrderedMap<String>| {
        if let Some(entry) = entry.take() {
            let path = if entry.deleted { entry.old_path } else { entry.new_path.or(entry.old_path) };
            if let (Some(path), Some(revision)) = (path.filter(|path| !path.is_empty()), entry.revision) {
                revisions.set(path, revision);
            }
        }
    };
    for line in patch.split('\n') {
        if let Some(rest) = line.strip_prefix(ENTRY) {
            close(&mut entry, &mut revisions);
            let (old_path, new_path) = header_paths(rest);
            entry = Some(Entry {
                old_path,
                new_path,
                deleted: false,
                revision: None,
                in_body: false,
            });
            continue;
        }
        let Some(current) = entry.as_mut().filter(|entry| !entry.in_body) else {
            continue;
        };
        if line.starts_with("@@") {
            current.in_body = true;
        } else if let Some(rest) = line.strip_prefix("index ") {
            current.revision = head_revision(rest);
        } else if line.starts_with("deleted file mode") {
            current.deleted = true;
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            current.old_path = Some(unquote_git_patch_path(rest));
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            current.new_path = Some(unquote_git_patch_path(rest));
        } else if let Some(rest) = line.strip_prefix("--- ") {
            current.old_path = side_path(rest, "a/");
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let side = side_path(rest, "b/");
            if side.is_none() {
                current.deleted = true;
            }
            current.new_path = side;
        }
    }
    close(&mut entry, &mut revisions);
    revisions
}

#[cfg(test)]
mod tests {
    //! `bitbucketDiffRevisions.test.ts`.

    use super::*;

    fn patch_of(lines: &[&str]) -> String {
        format!("{}\n", lines.join("\n"))
    }

    fn revisions(lines: &[&str]) -> Vec<(String, String)> {
        parse_diff_file_revisions(&patch_of(lines)).into_entries()
    }

    fn one(path: &str, revision: &str) -> Vec<(String, String)> {
        vec![(path.to_owned(), revision.to_owned())]
    }

    #[test]
    fn reads_the_head_id_of_a_changed_file_off_its_index_line() {
        assert_eq!(
            revisions(&[
                "diff --git a/src/a.ts b/src/a.ts",
                "index 7f2aa0ab6..b4a2a7c9a 100644",
                "--- a/src/a.ts",
                "+++ b/src/a.ts",
                "@@ -1 +1 @@",
                "-a",
                "+b"
            ]),
            one("src/a.ts", "b4a2a7c9a")
        );
    }

    #[test]
    fn names_a_deletion_by_the_path_it_had() {
        assert_eq!(
            revisions(&[
                "diff --git a/gone.ts b/gone.ts",
                "deleted file mode 100644",
                "index 1111111..0000000",
                "--- a/gone.ts",
                "+++ /dev/null",
                "@@ -1 +0,0 @@",
                "-x"
            ]),
            one("gone.ts", "0000000")
        );
    }

    #[test]
    fn names_a_rename_by_where_it_moved_to() {
        assert_eq!(
            revisions(&[
                "diff --git a/old.ts b/new.ts",
                "similarity index 90%",
                "rename from old.ts",
                "rename to new.ts",
                "index 2222222..3333333 100644",
                "--- a/old.ts",
                "+++ b/new.ts",
                "@@ -1 +1 @@",
                "-a",
                "+b",
            ]),
            one("new.ts", "3333333")
        );
    }

    #[test]
    fn names_a_rename_that_changed_nothing() {
        assert_eq!(
            revisions(&[
                "diff --git a/old.ts b/new.ts",
                "similarity index 100%",
                "rename from old.ts",
                "rename to new.ts",
                "index 2222222..2222222 100644"
            ]),
            one("new.ts", "2222222")
        );
    }

    #[test]
    fn leaves_out_a_file_given_no_index_line() {
        assert_eq!(
            revisions(&[
                "diff --git a/package.json b/package.json",
                "index 7f2aa0ab6..b4a2a7c9a 100644",
                "--- a/package.json",
                "+++ b/package.json",
                "@@ -1 +1 @@",
                r#"-  "x": "1""#,
                r#"+  "x": "2""#,
                "diff --git a/yarn.lock b/yarn.lock",
                r#"File excluded by pattern "yarn.lock""#,
            ]),
            one("package.json", "b4a2a7c9a")
        );
    }

    #[test]
    fn stops_reading_headers_at_the_first_hunk() {
        assert_eq!(
            revisions(&[
                "diff --git a/notes.md b/notes.md",
                "index aaaaaaa..bbbbbbb 100644",
                "--- a/notes.md",
                "+++ b/notes.md",
                "@@ -1,2 +1,2 @@",
                "--- a/decoy.ts",
                "+++ b/decoy.ts",
                "+index ccccccc..ddddddd 100644",
            ]),
            one("notes.md", "bbbbbbb")
        );
    }

    #[test]
    fn splits_a_header_whose_paths_contain_the_separator_by_the_sides_agreeing() {
        assert_eq!(
            revisions(&["diff --git a/one b/two.ts b/one b/two.ts", "index eeeeeee..fffffff 100644", "@@ -1 +1 @@"]),
            one("one b/two.ts", "fffffff")
        );
    }

    #[test]
    fn leaves_out_an_added_file_sent_no_index_line() {
        assert!(revisions(&["diff --git a/added.ts b/added.ts", "new file mode 100644", "--- /dev/null"]).is_empty());
    }

    #[test]
    fn reads_nothing_out_of_an_empty_patch() {
        assert!(parse_diff_file_revisions("").is_empty());
    }

    #[test]
    fn reads_a_name_git_had_to_quote() {
        assert_eq!(
            revisions(&[
                r#"diff --git "a/we\tird.ts" "b/we\tird.ts""#,
                "index 4444444..5555555 100644",
                r#"--- "a/we\tird.ts""#,
                r#"+++ "b/we\tird.ts""#,
                "@@ -1 +1 @@",
                "-a",
                "+b",
            ]),
            one("we\tird.ts", "5555555")
        );
    }

    #[test]
    fn rejoins_the_octal_bytes_git_writes_for_a_name_outside_ascii() {
        assert_eq!(
            revisions(&[
                r#"diff --git "a/caf\303\251/r\303\251sum\303\251.ts" "b/caf\303\251/r\303\251sum\303\251.ts""#,
                "index 6666666..7777777 100644",
                "@@ -1 +1 @@",
            ]),
            one("café/résumé.ts", "7777777")
        );
    }

    #[test]
    fn splits_a_rename_header_where_git_quoted_only_one_side() {
        assert_eq!(
            revisions(&[
                r#"diff --git a/old.ts "b/new\tname.ts""#,
                "similarity index 90%",
                "rename from old.ts",
                r#"rename to "new\tname.ts""#,
                "index 8888888..9999999 100644",
                "--- a/old.ts",
                r#"+++ "b/new\tname.ts""#,
                "@@ -1 +1 @@",
                "-a",
                "+b",
            ]),
            one("new\tname.ts", "9999999")
        );
    }

    #[test]
    fn names_a_quoted_rename_that_changed_nothing() {
        assert_eq!(
            revisions(&[
                r#"diff --git "a/old\tname.ts" "b/new\tname.ts""#,
                "similarity index 100%",
                r#"rename from "old\tname.ts""#,
                r#"rename to "new\tname.ts""#,
                "index abcabca..abcabca 100644",
            ]),
            one("new\tname.ts", "abcabca")
        );
    }

    #[test]
    fn keeps_a_character_from_outside_the_basic_plane_left_unescaped() {
        assert_eq!(
            revisions(&[
                "diff --git \"a/we\\tird-\u{1f680}.ts\" \"b/we\\tird-\u{1f680}.ts\"",
                "index aaaaaaa..bbbbbbb 100644",
                "@@ -1 +1 @@"
            ]),
            one("we\tird-\u{1f680}.ts", "bbbbbbb")
        );
    }

    #[test]
    fn drops_the_tab_git_ends_a_name_holding_a_space_with() {
        assert_eq!(
            revisions(&[
                "diff --git a/with space.txt b/with space.txt",
                "index 1111111..6178079 100644",
                "--- a/with space.txt\t",
                "+++ b/with space.txt\t",
                "@@ -1 +1 @@",
                "-a",
                "+b",
            ]),
            one("with space.txt", "6178079")
        );
    }

    #[test]
    fn drops_that_tab_from_a_quoted_name_too() {
        assert_eq!(
            revisions(&[
                r#"diff --git "a/we ird\tname.ts" "b/we ird\tname.ts""#,
                "index 2222222..7777777 100644",
                "--- \"a/we ird\\tname.ts\"\t",
                "+++ \"b/we ird\\tname.ts\"\t",
                "@@ -1 +1 @@",
                "-a",
                "+b",
            ]),
            one("we ird\tname.ts", "7777777")
        );
    }

    #[test]
    fn reads_a_deletion_whose_name_holds_a_space() {
        assert_eq!(
            revisions(&[
                "diff --git a/with space.txt b/with space.txt",
                "deleted file mode 100644",
                "index 3333333..0000000",
                "--- a/with space.txt\t",
                "+++ /dev/null",
                "@@ -1 +0,0 @@",
                "-a",
            ]),
            one("with space.txt", "0000000")
        );
    }

    #[test]
    fn drops_the_timestamp_other_producers_write_past_that_tab() {
        assert_eq!(
            revisions(&[
                "diff --git a/stamped.ts b/stamped.ts",
                "index 4444444..5555555 100644",
                "--- a/stamped.ts\t2024-01-01 00:00:00.000000000 +0000",
                "+++ b/stamped.ts\t2024-01-02 00:00:00.000000000 +0000",
                "@@ -1 +1 @@",
                "-a",
                "+b",
            ]),
            one("stamped.ts", "5555555")
        );
    }

    #[test]
    fn splits_an_unquoted_header_whose_names_hold_a_space() {
        assert_eq!(
            revisions(&["diff --git a/one two b/one two", "index ddddddd..eeeeeee 100644", "@@ -1 +1 @@"]),
            one("one two", "eeeeeee")
        );
    }
}
