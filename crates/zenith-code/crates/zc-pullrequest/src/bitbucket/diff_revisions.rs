//! `pullRequest/bitbucketDiffRevisions.ts`: what the head of a pull request has of each file,
//! read off its unified patch (Bitbucket exposes no blob id for a file anywhere else).

use std::collections::HashMap;

use super::git_patch_path::unquote_git_patch_path;

const ENTRY: &str = "diff --git ";
const QUOTE: u8 = b'"';

/// A `Map<string, string>`: insertion-ordered, a repeated key keeps its first place.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OrderedRevisions {
    entries: Vec<(String, String)>,
    index: HashMap<String, usize>,
}

impl OrderedRevisions {
    /// `map.set(key, value)`.
    pub fn set(&mut self, key: String, value: String) {
        match self.index.get(&key) {
            Some(&at) => self.entries[at].1 = value,
            None => {
                self.index.insert(key.clone(), self.entries.len());
                self.entries.push((key, value));
            }
        }
    }

    /// `map.has(key)`.
    pub fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[(String, String)] {
        &self.entries
    }

    pub fn into_entries(self) -> Vec<(String, String)> {
        self.entries
    }
}

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
            b'\\' => at += 1,
            QUOTE => return Some(at),
            _ => {}
        }
        at += 1;
    }
    None
}

/// `a/x`/`b/x` on a `---`/`+++` line; `/dev/null` marks the side that has no file. Git ends the
/// name with a tab when it holds a space, and a name with a tab of its own arrives quoted with
/// that tab escaped, so the first literal tab is never part of what the file is called.
fn side_path(rest: &str, prefix: &str) -> Option<String> {
    let token = rest.split_once('\t').map_or(rest, |(token, _)| token);
    if token == "/dev/null" {
        return None;
    }
    let path = unquote_git_patch_path(token);
    Some(match path.strip_prefix(prefix) {
        Some(stripped) => stripped.to_owned(),
        None => path,
    })
}

fn header_side(token: &str, prefix: &str) -> Option<String> {
    unquote_git_patch_path(token).strip_prefix(prefix).map(str::to_owned)
}

/// The two names on a `diff --git` line, written with no delimiter between them. `a/one two
/// b/one two` can split in more than one place, so the split leaving both sides equal wins; a
/// rename (the only case where sides differ) states its names on separate lines instead. A
/// quoted name ends at its own closing quote and needs none of that guessing.
fn header_paths(rest: &str) -> (Option<String>, Option<String>) {
    let bytes = rest.as_bytes();
    if rest.starts_with('"') {
        return match quoted_end(rest) {
            Some(end) if bytes.get(end + 1) == Some(&b' ') => (header_side(&rest[..=end], "a/"), header_side(&rest[end + 2..], "b/")),
            _ => (None, None),
        };
    }
    if rest.ends_with('"') {
        return match rest.find('"') {
            Some(opens) if opens >= 1 && bytes[opens - 1] == b' ' => (header_side(&rest[..opens - 1], "a/"), header_side(&rest[opens..], "b/")),
            _ => (None, None),
        };
    }
    if !rest.starts_with("a/") {
        return (None, None);
    }
    let splits: Vec<usize> = rest.match_indices(" b/").map(|(at, _)| at).collect();
    let chosen = splits
        .iter()
        .copied()
        .find(|&at| rest.get(2..at) == rest.get(at + 3..))
        .or(if splits.len() == 1 { Some(splits[0]) } else { None });
    match chosen {
        Some(at) => (Some(rest.get(2..at).unwrap_or_default().to_owned()), Some(rest[at + 3..].to_owned())),
        None => (None, None),
    }
}

/// The right-hand id of `index <before>..<after> <mode>`.
fn head_revision(rest: &str) -> Option<String> {
    let (_, after) = rest.split_once("..")?;
    let head = after.split_once(' ').map_or(after, |(head, _)| head);
    (!head.is_empty()).then(|| head.to_owned())
}

/// `parseDiffFileRevisions`: what the head has of each file, as the blob ids from a unified
/// patch's `index <before>..<after>` line. Keyed by the head's name, except for a deletion where
/// only the old name exists. An entry with no `index` line (most often one Bitbucket excluded by
/// pattern) is left out.
pub fn parse_diff_file_revisions(patch: &str) -> OrderedRevisions {
    let mut revisions = OrderedRevisions::default();
    let mut entry: Option<Entry> = None;

    let close = |entry: &mut Option<Entry>, revisions: &mut OrderedRevisions| {
        let Some(closing) = entry.take() else { return };
        let path = if closing.deleted {
            closing.old_path
        } else {
            closing.new_path.or(closing.old_path)
        };
        if let (Some(path), Some(revision)) = (path, closing.revision) {
            if !path.is_empty() {
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
        let Some(current) = entry.as_mut().filter(|current| !current.in_body) else {
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

    fn pairs(revisions: &OrderedRevisions) -> Vec<(&str, &str)> {
        revisions.entries().iter().map(|(path, revision)| (path.as_str(), revision.as_str())).collect()
    }

    #[test]
    fn reads_the_head_id_of_a_changed_file_off_its_index_line() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git a/src/a.ts b/src/a.ts",
            "index 7f2aa0ab6..b4a2a7c9a 100644",
            "--- a/src/a.ts",
            "+++ b/src/a.ts",
            "@@ -1 +1 @@",
            "-a",
            "+b",
        ]));
        assert_eq!(pairs(&revisions), vec![("src/a.ts", "b4a2a7c9a")]);
    }

    #[test]
    fn names_a_deletion_by_the_path_it_had() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git a/gone.ts b/gone.ts",
            "deleted file mode 100644",
            "index 1111111..0000000",
            "--- a/gone.ts",
            "+++ /dev/null",
            "@@ -1 +0,0 @@",
            "-x",
        ]));
        assert_eq!(pairs(&revisions), vec![("gone.ts", "0000000")]);
    }

    #[test]
    fn names_a_rename_by_where_it_moved_to() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
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
        ]));
        assert_eq!(pairs(&revisions), vec![("new.ts", "3333333")]);
    }

    #[test]
    fn names_a_rename_that_changed_nothing() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git a/old.ts b/new.ts",
            "similarity index 100%",
            "rename from old.ts",
            "rename to new.ts",
            "index 2222222..2222222 100644",
        ]));
        assert_eq!(pairs(&revisions), vec![("new.ts", "2222222")]);
    }

    #[test]
    fn leaves_out_a_file_bitbucket_excluded() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git a/package.json b/package.json",
            "index 7f2aa0ab6..b4a2a7c9a 100644",
            "--- a/package.json",
            "+++ b/package.json",
            "@@ -1 +1 @@",
            "-  \"x\": \"1\"",
            "+  \"x\": \"2\"",
            "diff --git a/yarn.lock b/yarn.lock",
            "File excluded by pattern \"yarn.lock\"",
        ]));
        assert_eq!(pairs(&revisions), vec![("package.json", "b4a2a7c9a")]);
    }

    #[test]
    fn stops_reading_headers_at_the_first_hunk() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git a/notes.md b/notes.md",
            "index aaaaaaa..bbbbbbb 100644",
            "--- a/notes.md",
            "+++ b/notes.md",
            "@@ -1,2 +1,2 @@",
            "--- a/decoy.ts",
            "+++ b/decoy.ts",
            "+index ccccccc..ddddddd 100644",
        ]));
        assert_eq!(pairs(&revisions), vec![("notes.md", "bbbbbbb")]);
    }

    #[test]
    fn splits_a_header_whose_paths_contain_the_separator() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git a/one b/two.ts b/one b/two.ts",
            "index eeeeeee..fffffff 100644",
            "@@ -1 +1 @@",
        ]));
        assert_eq!(pairs(&revisions), vec![("one b/two.ts", "fffffff")]);
    }

    #[test]
    fn leaves_out_an_added_file_with_no_index_line() {
        let revisions = parse_diff_file_revisions(&patch_of(&["diff --git a/added.ts b/added.ts", "new file mode 100644", "--- /dev/null"]));
        assert_eq!(revisions.len(), 0);
    }

    #[test]
    fn reads_nothing_out_of_an_empty_patch() {
        assert_eq!(parse_diff_file_revisions("").len(), 0);
    }

    #[test]
    fn reads_a_name_git_had_to_quote() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            r#"diff --git "a/we\tird.ts" "b/we\tird.ts""#,
            "index 4444444..5555555 100644",
            r#"--- "a/we\tird.ts""#,
            r#"+++ "b/we\tird.ts""#,
            "@@ -1 +1 @@",
            "-a",
            "+b",
        ]));
        assert_eq!(pairs(&revisions), vec![("we\tird.ts", "5555555")]);
    }

    #[test]
    fn rejoins_the_octal_bytes_git_writes_for_a_name_outside_ascii() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            r#"diff --git "a/caf\303\251/r\303\251sum\303\251.ts" "b/caf\303\251/r\303\251sum\303\251.ts""#,
            "index 6666666..7777777 100644",
            "@@ -1 +1 @@",
        ]));
        assert_eq!(pairs(&revisions), vec![("café/résumé.ts", "7777777")]);
    }

    #[test]
    fn splits_a_rename_header_where_git_quoted_only_one_side() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
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
        ]));
        assert_eq!(pairs(&revisions), vec![("new\tname.ts", "9999999")]);
    }

    #[test]
    fn names_a_quoted_rename_that_changed_nothing() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            r#"diff --git "a/old\tname.ts" "b/new\tname.ts""#,
            "similarity index 100%",
            r#"rename from "old\tname.ts""#,
            r#"rename to "new\tname.ts""#,
            "index abcabca..abcabca 100644",
        ]));
        assert_eq!(pairs(&revisions), vec![("new\tname.ts", "abcabca")]);
    }

    #[test]
    fn keeps_a_character_from_outside_the_basic_plane() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git \"a/we\\tird-\u{1f680}.ts\" \"b/we\\tird-\u{1f680}.ts\"",
            "index aaaaaaa..bbbbbbb 100644",
            "@@ -1 +1 @@",
        ]));
        assert_eq!(pairs(&revisions), vec![("we\tird-\u{1f680}.ts", "bbbbbbb")]);
    }

    #[test]
    fn drops_the_tab_git_ends_a_name_holding_a_space_with() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git a/with space.txt b/with space.txt",
            "index 1111111..6178079 100644",
            "--- a/with space.txt\t",
            "+++ b/with space.txt\t",
            "@@ -1 +1 @@",
            "-a",
            "+b",
        ]));
        assert_eq!(pairs(&revisions), vec![("with space.txt", "6178079")]);
    }

    #[test]
    fn drops_that_tab_from_a_quoted_name_too() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            r#"diff --git "a/we ird\tname.ts" "b/we ird\tname.ts""#,
            "index 2222222..7777777 100644",
            "--- \"a/we ird\\tname.ts\"\t",
            "+++ \"b/we ird\\tname.ts\"\t",
            "@@ -1 +1 @@",
            "-a",
            "+b",
        ]));
        assert_eq!(pairs(&revisions), vec![("we ird\tname.ts", "7777777")]);
    }

    #[test]
    fn reads_a_deletion_whose_name_holds_a_space() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git a/with space.txt b/with space.txt",
            "deleted file mode 100644",
            "index 3333333..0000000",
            "--- a/with space.txt\t",
            "+++ /dev/null",
            "@@ -1 +0,0 @@",
            "-a",
        ]));
        assert_eq!(pairs(&revisions), vec![("with space.txt", "0000000")]);
    }

    #[test]
    fn drops_the_timestamp_other_producers_write_past_that_tab() {
        let revisions = parse_diff_file_revisions(&patch_of(&[
            "diff --git a/stamped.ts b/stamped.ts",
            "index 4444444..5555555 100644",
            "--- a/stamped.ts\t2024-01-01 00:00:00.000000000 +0000",
            "+++ b/stamped.ts\t2024-01-02 00:00:00.000000000 +0000",
            "@@ -1 +1 @@",
            "-a",
            "+b",
        ]));
        assert_eq!(pairs(&revisions), vec![("stamped.ts", "5555555")]);
    }

    #[test]
    fn splits_an_unquoted_header_whose_names_hold_a_space() {
        let revisions = parse_diff_file_revisions(&patch_of(&["diff --git a/one two b/one two", "index ddddddd..eeeeeee 100644", "@@ -1 +1 @@"]));
        assert_eq!(pairs(&revisions), vec![("one two", "eeeeeee")]);
    }
}
