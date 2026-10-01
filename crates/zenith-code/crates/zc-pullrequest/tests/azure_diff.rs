//! Port of `azureDevOpsDiff.test.ts`.

use zc_pullrequest::azure::diff::*;
use zc_pullrequest::azure::json::{AzureDevOpsChangeEntry, AzureDevOpsChangeKind};
use zc_pullrequest::azure::util::unquote_git_patch_path;

fn change(path: &str, old_path: &str, kind: AzureDevOpsChangeKind) -> AzureDevOpsChangeEntry {
    AzureDevOpsChangeEntry {
        path: path.into(),
        old_path: old_path.into(),
        change_kind: kind,
        object_id: Some("8f80".into()),
        original_object_id: Some("0ca4".into()),
    }
}

fn readme() -> AzureDevOpsChangeEntry {
    change("README.md", "README.md", AzureDevOpsChangeKind::Change)
}

fn texts(old: &str, new: &str) -> AzureDevOpsFileTexts {
    AzureDevOpsFileTexts {
        old_contents: old.into(),
        new_contents: new.into(),
        binary: false,
    }
}

fn lines(parts: &[&str]) -> String {
    parts.join("\n")
}

fn count_line_starts(section: &str, prefix: &str) -> usize {
    section.split('\n').filter(|line| line.starts_with(prefix)).count()
}

#[test]
fn writes_a_changed_file_as_a_unified_patch() {
    let patch = azure_devops_file_patch(&readme(), &texts("one\ntwo\nthree\n", "one\ntwo again\nthree\n"));
    assert!(!patch.truncated);
    assert_eq!(
        patch.section,
        lines(&[
            "diff --git a/README.md b/README.md",
            "--- a/README.md",
            "+++ b/README.md",
            "@@ -1,3 +1,3 @@",
            " one",
            "-two",
            "+two again",
            " three",
            ""
        ])
    );
}

#[test]
fn names_the_side_a_new_file_does_not_have_as_dev_null() {
    let patch = azure_devops_file_patch(&change("DEMO.md", "DEMO.md", AzureDevOpsChangeKind::New), &texts("", "hello\n"));
    assert!(patch.section.contains("new file mode 100644"));
    assert!(patch.section.contains("--- /dev/null"));
    assert!(patch.section.contains("+++ b/DEMO.md"));
    // Git points the range a new file does not have at line zero.
    assert!(patch.section.contains("@@ -0,0 +1 @@"));
    assert!(patch.section.contains("+hello"));
}

#[test]
fn names_the_side_a_deleted_file_no_longer_has_as_dev_null() {
    let patch = azure_devops_file_patch(&change("OLD.md", "OLD.md", AzureDevOpsChangeKind::Deleted), &texts("gone\n", ""));
    assert!(patch.section.contains("deleted file mode 100644"));
    assert!(patch.section.contains("--- a/OLD.md"));
    assert!(patch.section.contains("+++ /dev/null"));
    assert!(patch.section.contains("@@ -1 +0,0 @@"));
    assert!(patch.section.contains("-gone"));
}

#[test]
fn keeps_the_carriage_returns_of_windows_line_endings() {
    let patch = azure_devops_file_patch(&readme(), &texts("one\r\ntwo\r\n", "one\r\ntwo again\r\n"));
    assert!(patch.section.contains("-two\r"));
    assert!(patch.section.contains("+two again\r"));
}

#[test]
fn keeps_a_file_that_only_moved() {
    let patch = azure_devops_file_patch(
        &change("docs/new.md", "docs/old.md", AzureDevOpsChangeKind::RenamePure),
        &texts("same\n", "same\n"),
    );
    assert!(!patch.truncated);
    assert_eq!(
        patch.section,
        lines(&[
            "diff --git a/docs/old.md b/docs/new.md",
            "rename from docs/old.md",
            "rename to docs/new.md",
            "--- a/docs/old.md",
            "+++ b/docs/new.md",
            ""
        ])
    );
}

#[test]
fn reports_a_binary_file_as_changed() {
    let patch = azure_devops_file_patch(&change("logo.png", "logo.png", AzureDevOpsChangeKind::Change), &texts("PNG\0old", "PNG\0new"));
    assert!(patch.truncated);
    assert!(patch.section.contains("Binary files a/logo.png and b/logo.png differ"));
}

#[test]
fn shows_an_overlong_file_as_changed_without_its_hunks() {
    let patch = azure_devops_file_patch(
        &change("bundle.js", "bundle.js", AzureDevOpsChangeKind::Change),
        &texts(&"a\n".repeat(400_000), &"b\n".repeat(400_000)),
    );
    assert!(patch.truncated);
    assert_eq!(
        patch.section,
        lines(&["diff --git a/bundle.js b/bundle.js", "--- a/bundle.js", "+++ b/bundle.js", ""])
    );
}

#[test]
fn takes_the_hosts_word_that_a_file_is_binary() {
    let mut pair = texts("b2xk", "bmV3");
    pair.binary = true;
    let patch = azure_devops_file_patch(&change("logo.png", "logo.png", AzureDevOpsChangeKind::Change), &pair);
    assert!(patch.truncated);
    assert!(patch.section.contains("Binary files a/logo.png and b/logo.png differ"));
}

#[test]
fn counts_an_overlong_file_in_bytes() {
    let patch = azure_devops_file_patch(
        &change("notes.md", "notes.md", AzureDevOpsChangeKind::Change),
        &texts(&"\u{4e00}".repeat(200_000), &"\u{4e8c}".repeat(200_000)),
    );
    assert!(patch.truncated);
    assert_eq!(
        patch.section,
        lines(&["diff --git a/notes.md b/notes.md", "--- a/notes.md", "+++ b/notes.md", ""])
    );
}

fn line_range(count: usize, prefix: &str) -> String {
    (0..count).map(|line| format!("{prefix} {line}")).collect::<Vec<_>>().join("\n")
}

#[test]
fn lists_a_file_too_far_apart_to_diff_without_its_hunks() {
    let patch = azure_devops_file_patch(
        &change("generated.ts", "generated.ts", AzureDevOpsChangeKind::Change),
        &texts(
            &format!("{}\n", line_range(MAX_FILE_DIFF_EDITS, "old")),
            &format!("{}\n", line_range(MAX_FILE_DIFF_EDITS, "new")),
        ),
    );
    assert!(patch.truncated);
    // The search was given up on, so a reader of a run of files stops.
    assert!(patch.abandoned);
    assert_eq!(
        patch.section,
        lines(&["diff --git a/generated.ts b/generated.ts", "--- a/generated.ts", "+++ b/generated.ts", ""])
    );
}

#[test]
fn writes_out_a_wholly_new_file_however_many_lines_it_has() {
    let contents = format!("{}\n", vec!["x".repeat(29); 15_000].join("\n"));
    let patch = azure_devops_file_patch(&change("DEMO.md", "DEMO.md", AzureDevOpsChangeKind::New), &texts("", &contents));
    assert!(byte_length(&contents) < 512 * 1024);
    assert!(!patch.truncated);
    assert!(!patch.abandoned);
    assert_eq!(patch.edits, 15_000);
    assert!(patch.section.contains("@@ -0,0 +1,15000 @@"));
    assert_eq!(count_line_starts(&patch.section, "+"), 15_001);
}

#[test]
fn keeps_a_wholly_new_file_too_heavy_to_write_out_listed_without_its_hunks() {
    let contents = format!("{}\n", vec!["x"; 200_000].join("\n"));
    let patch = azure_devops_file_patch(&change("bundle.min.js", "bundle.min.js", AzureDevOpsChangeKind::New), &texts("", &contents));
    assert!(byte_length(&contents) < 512 * 1024);
    assert!(patch.truncated);
    assert!(!patch.abandoned);
    // It still cost the walk over its lines.
    assert_eq!(patch.edits, 200_000);
    assert_eq!(
        patch.section,
        lines(&[
            "diff --git a/bundle.min.js b/bundle.min.js",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/bundle.min.js",
            ""
        ])
    );
}

#[test]
fn writes_out_a_wholly_deleted_file_however_many_lines_it_had() {
    let contents = format!("{}\n", line_range(20_000, "line"));
    let patch = azure_devops_file_patch(&change("OLD.md", "OLD.md", AzureDevOpsChangeKind::Deleted), &texts(&contents, ""));
    assert!(!patch.truncated);
    assert!(!patch.abandoned);
    assert_eq!(patch.edits, 20_000);
    assert!(patch.section.contains("@@ -1,20000 +0,0 @@"));
    assert_eq!(count_line_starts(&patch.section, "-line "), 20_000);
    assert_eq!(count_line_starts(&patch.section, "-"), 20_001);
}

#[test]
fn gives_an_empty_new_file_no_hunk() {
    let patch = azure_devops_file_patch(&change("EMPTY.md", "EMPTY.md", AzureDevOpsChangeKind::New), &texts("", ""));
    assert_eq!(patch.edits, 0);
    assert!(!patch.section.contains("@@"));
}

#[test]
fn marks_a_wholly_new_file_whose_last_line_has_no_newline() {
    let patch = azure_devops_file_patch(&change("NOTES.md", "NOTES.md", AzureDevOpsChangeKind::New), &texts("", "one\ntwo"));
    assert_eq!(
        patch.section,
        lines(&[
            "diff --git a/NOTES.md b/NOTES.md",
            "new file mode 100644",
            "--- /dev/null",
            "+++ b/NOTES.md",
            "@@ -0,0 +1,2 @@",
            "+one",
            "+two",
            "\\ No newline at end of file",
            ""
        ])
    );
}

#[test]
fn keeps_a_file_it_did_diff_out_of_the_giving_up() {
    assert!(!azure_devops_file_patch(&readme(), &texts("one\ntwo\n", "one\ntwo again\n")).abandoned);
}

#[test]
fn counts_what_the_diff_worked_out() {
    assert_eq!(
        azure_devops_file_patch(&readme(), &texts("one\ntwo\nthree\nfour\n", "one\ntwo again\nthree\nfour\n")).edits,
        2
    );
}

#[test]
fn counts_nothing_for_a_file_it_never_diffed() {
    assert_eq!(
        azure_devops_file_patch(&change("logo.png", "logo.png", AzureDevOpsChangeKind::Change), &texts("PNG\0old", "PNG\0new")).edits,
        0
    );
}

#[test]
fn lists_a_file_whose_hunks_outweigh_its_sides_without_them() {
    let line = format!("{}\n", "a".repeat(400 * 1024));
    let patch = azure_devops_file_patch(
        &change("min.js", "min.js", AzureDevOpsChangeKind::Change),
        &texts(&line, &format!("{}\n", "b".repeat(400 * 1024))),
    );
    assert!(patch.edits < MAX_FILE_DIFF_EDITS);
    assert!(patch.truncated);
    assert_eq!(patch.section, lines(&["diff --git a/min.js b/min.js", "--- a/min.js", "+++ b/min.js", ""]));
    assert!(byte_length(&patch.section) < byte_length(&line));
}

#[test]
fn marks_a_file_that_does_not_end_in_a_newline() {
    assert!(azure_devops_file_patch(&readme(), &texts("one\n", "two"))
        .section
        .contains("\\ No newline at end of file"));
}

#[test]
fn keeps_a_file_the_host_would_not_hand_over_listed_without_its_hunks() {
    let patch = azure_devops_unreadable_file_patch(&change("huge.bin", "huge.bin", AzureDevOpsChangeKind::Change));
    assert!(patch.truncated);
    assert_eq!(
        patch.section,
        lines(&["diff --git a/huge.bin b/huge.bin", "--- a/huge.bin", "+++ b/huge.bin", ""])
    );
}

// A file Azure names something a patch header cannot carry plainly.

#[test]
fn writes_each_side_as_gits_quoted_form() {
    let patch = azure_devops_file_patch(
        &change("notes\treadme.md", "notes\treadme.md", AzureDevOpsChangeKind::Change),
        &texts("one\n", "two\n"),
    );
    assert_eq!(
        patch.section,
        lines(&[
            "diff --git \"a/notes\\treadme.md\" \"b/notes\\treadme.md\"",
            "--- \"a/notes\\treadme.md\"",
            "+++ \"b/notes\\treadme.md\"",
            "@@ -1 +1 @@",
            "-one",
            "+two",
            ""
        ])
    );
}

#[test]
fn keeps_a_name_holding_a_newline_on_its_header_line() {
    let patch = azure_devops_file_patch(
        &change("line\nfile.txt", "line\nfile.txt", AzureDevOpsChangeKind::Change),
        &texts("one\n", "two\n"),
    );
    assert_eq!(
        patch.section.split('\n').take(3).collect::<Vec<_>>(),
        [
            "diff --git \"a/line\\nfile.txt\" \"b/line\\nfile.txt\"",
            "--- \"a/line\\nfile.txt\"",
            "+++ \"b/line\\nfile.txt\""
        ]
    );
}

#[test]
fn quotes_the_names_a_rename_states() {
    let patch = azure_devops_file_patch(
        &change("docs/new\tname.md", "docs/old\tname.md", AzureDevOpsChangeKind::RenamePure),
        &texts("same\n", "same\n"),
    );
    assert_eq!(
        patch.section,
        lines(&[
            "diff --git \"a/docs/old\\tname.md\" \"b/docs/new\\tname.md\"",
            "rename from \"docs/old\\tname.md\"",
            "rename to \"docs/new\\tname.md\"",
            "--- \"a/docs/old\\tname.md\"",
            "+++ \"b/docs/new\\tname.md\"",
            ""
        ])
    );
}

#[test]
fn quotes_the_sides_of_the_binary_line() {
    let patch = azure_devops_file_patch(
        &change("logo\tmark.png", "logo\tmark.png", AzureDevOpsChangeKind::Change),
        &texts("PNG\0old", "PNG\0new"),
    );
    assert!(patch.section.contains("Binary files \"a/logo\\tmark.png\" and \"b/logo\\tmark.png\" differ"));
}

#[test]
fn hands_a_reader_of_the_header_back_the_name_azure_gave() {
    let path = "every\t\n\"kind\"\\of.md";
    let patch = azure_devops_file_patch(&change(path, path, AzureDevOpsChangeKind::Change), &texts("one\n", "two\n"));
    let parts: Vec<&str> = patch.section.split('\n').collect();
    assert!(!parts[0].contains('\t'));
    assert_eq!(unquote_git_patch_path(&parts[1][4..]), format!("a/{path}"));
    assert_eq!(unquote_git_patch_path(&parts[2][4..]), format!("b/{path}"));
}

// A diff cursor.

#[test]
fn carries_the_push_back_to_the_next_slice() {
    let cursor = format_azure_devops_diff_cursor(AzureDevOpsDiffCursor {
        iteration_id: 3,
        file_index: 12,
    });
    assert_eq!(
        parse_azure_devops_diff_cursor(Some(&cursor)),
        Some(AzureDevOpsDiffCursor {
            iteration_id: 3,
            file_index: 12
        })
    );
}

#[test]
fn reads_anything_it_did_not_write_as_no_position() {
    assert_eq!(parse_azure_devops_diff_cursor(None), None);
    for raw in ["", "abc", "1", "0:4", "1:-2", "1:2:3"] {
        assert_eq!(parse_azure_devops_diff_cursor(Some(raw)), None, "{raw}");
    }
}

#[test]
fn refuses_a_half_it_did_not_write() {
    for raw in ["1:", ":4", "1: ", " 1:4", "1:0x2", "0x1:2", "1e2:0", "1:4.0"] {
        assert_eq!(parse_azure_devops_diff_cursor(Some(raw)), None, "{raw}");
    }
}
