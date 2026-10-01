//! Ports of the review-diff tests of `GitVcsDriverCore.test.ts` and of
//! `review/ReviewService.test.ts`.

mod common;

use std::sync::Arc;

use common::*;
use zc_core::vcs_process::VcsProcess;
use zc_vcs::contracts::*;
use zc_vcs::errors::{ReviewDiffPreviewError, VcsError};
use zc_vcs::parse::split_null_separated_paths;
use zc_vcs::registry::{VcsDriverRegistry, VcsProjectConfig};
use zc_vcs::vcs_driver::GitVcsProcessDriver;
use zc_vcs::ReviewService;

fn preview(cwd: &str) -> ReviewDiffPreviewInput {
    ReviewDiffPreviewInput {
        cwd: cwd.to_owned(),
        base_ref: None,
        ignore_whitespace: None,
        file: None,
    }
}

fn source(result: &ReviewDiffPreviewResult, kind: ReviewDiffPreviewSourceKind) -> &ReviewDiffPreviewSource {
    result.sources.iter().find(|s| s.kind == kind).unwrap()
}

const WT: ReviewDiffPreviewSourceKind = ReviewDiffPreviewSourceKind::WorkingTree;
const BR: ReviewDiffPreviewSourceKind = ReviewDiffPreviewSourceKind::BranchRange;

fn stat(path: &str, previous: Option<&str>, additions: u64, deletions: u64) -> ReviewDiffFileStat {
    ReviewDiffFileStat {
        path: path.into(),
        previous_path: previous.map(Into::into),
        additions,
        deletions,
    }
}

#[tokio::test]
async fn loads_repository_relative_files_from_a_nested_project_directory() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["checkout", "-b", "feature/nested"]);
    write(&cwd.path, "nested/tracked.txt", "committed\n");
    commit_all(&cwd.path, "nested file");
    write(&cwd.path, "nested/tracked.txt", "changed\n");
    write(&cwd.path, "untracked.txt", "new\n");
    let nested = cwd.join("nested");
    let (driver, _w) = driver();
    let result = driver
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            base_ref: Some(branch.clone()),
            ..preview(&nested)
        })
        .await
        .unwrap();
    assert_eq!(result.cwd, nested);
    assert_eq!(source(&result, WT).files.as_ref().unwrap().len(), 2);
    for kind in [WT, BR] {
        for file in source(&result, kind).files.clone().unwrap_or_default() {
            let scoped = driver
                .get_review_diff_preview(&ReviewDiffPreviewInput {
                    base_ref: Some(branch.clone()),
                    file: Some(ReviewDiffPreviewFile {
                        path: file.path.clone(),
                        previous_path: file.previous_path.clone(),
                        source_kind: kind,
                    }),
                    ..preview(&nested)
                })
                .await
                .unwrap();
            let patch = source(&scoped, kind);
            assert_eq!(patch.files.as_ref().unwrap(), &vec![file.clone()]);
            assert!(patch.diff.contains(&format!("b/{}", file.path)));
        }
    }
}

#[tokio::test]
async fn reads_complete_manifests_beyond_one_megabyte() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    write(&cwd.path, "untracked.txt", "untracked content\n");
    let paths: Vec<String> = (0..5000).map(|i| format!("{}-{i}.txt", "a".repeat(220))).collect();
    let stats: String = paths.iter().map(|p| format!("1\t0\t{p}\0")).collect();
    assert!(stats.len() > 1024 * 1024);
    let range = format!("{branch}...HEAD");
    let recorder = Recorder::responding(move |input| {
        if has(&input.args, "--numstat") && has(&input.args, &range) {
            return succeed_with(&stats);
        }
        None
    });
    let (driver, _w) = driver_with(recorder);
    let result = driver
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            base_ref: Some(branch.clone()),
            ..preview(cwd.str())
        })
        .await
        .unwrap();
    let files = source(&result, BR).files.clone().unwrap();
    assert_eq!(files.len(), paths.len());
    assert_eq!(files.last().unwrap().path, *paths.last().unwrap());
    assert_eq!(files.iter().map(|f| f.additions).sum::<u64>(), paths.len() as u64);
}

#[tokio::test]
async fn propagates_patch_failures_instead_of_an_empty_complete_diff() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, "README.md", "changed\n");
    let recorder = Recorder::responding(|input| {
        if has(&input.args, "--patch") {
            return not_a_repository();
        }
        None
    });
    let (driver, _w) = driver_with(recorder);
    let error = driver.get_review_diff_preview(&preview(cwd.str())).await.unwrap_err();
    assert_eq!(error.operation, "GitVcsDriver.getReviewDiffPreview.patch");
}

#[test]
fn drops_an_unterminated_path_from_truncated_output() {
    assert_eq!(split_null_separated_paths("complete.txt\0partial", true), vec!["complete.txt"]);
    assert_eq!(split_null_separated_paths("complete.txt\0final.txt", false), vec!["complete.txt", "final.txt"]);
}

#[tokio::test]
async fn honors_whitespace_filtering_for_both_sources() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["checkout", "-b", "feature/whitespace"]);
    write(&cwd.path, "README.md", "#  test\n");
    git(&cwd.path, &["add", "README.md"]);
    git(&cwd.path, &["commit", "-m", "change whitespace"]);
    write(&cwd.path, "README.md", "#   test\n");
    let (driver, _w) = driver();
    let with = |ignore: bool| ReviewDiffPreviewInput {
        base_ref: Some(branch.clone()),
        ignore_whitespace: Some(ignore),
        ..preview(cwd.str())
    };
    let included = driver.get_review_diff_preview(&with(false)).await.unwrap();
    let ignored = driver.get_review_diff_preview(&with(true)).await.unwrap();
    assert!(!source(&included, WT).diff.is_empty());
    assert!(!source(&included, BR).diff.is_empty());
    assert_eq!(source(&ignored, WT).diff, "");
    assert_eq!(source(&ignored, WT).files, Some(vec![]));
    assert_eq!(source(&ignored, BR).files, Some(vec![]));
    assert_eq!(source(&ignored, BR).diff, "");
}

#[tokio::test]
async fn keeps_a_and_b_patch_prefixes_when_the_repository_disables_them() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["config", "diff.noprefix", "true"]);
    git(&cwd.path, &["config", "diff.mnemonicPrefix", "true"]);
    git(&cwd.path, &["checkout", "-b", "feature/noprefix"]);
    write(&cwd.path, "README.md", "# committed change\n");
    git(&cwd.path, &["add", "README.md"]);
    git(&cwd.path, &["commit", "-m", "committed change"]);
    write(&cwd.path, "README.md", "# dirty change\n");
    write(&cwd.path, "untracked.txt", "untracked\n");
    let (driver, _w) = driver();
    let result = driver
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            base_ref: Some(branch),
            ignore_whitespace: Some(false),
            ..preview(cwd.str())
        })
        .await
        .unwrap();
    assert!(source(&result, WT).diff.contains("diff --git a/README.md b/README.md"));
    assert!(source(&result, WT).diff.contains("+++ b/untracked.txt"));
    assert!(source(&result, BR).diff.contains("diff --git a/README.md b/README.md"));
}

#[tokio::test]
async fn keeps_untracked_filenames_with_pathspec_magic() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, ":(exclude)after.ts", "literal pathspec contents\n");
    write(&cwd.path, "ordinary.ts", "ordinary contents\n");
    let index_before = git(&cwd.path, &["ls-files", "--stage"]);
    let (driver, _w) = driver();
    let result = driver.get_review_diff_preview(&preview(cwd.str())).await.unwrap();
    let diff = &source(&result, WT).diff;
    assert!(diff.contains("+literal pathspec contents"));
    assert!(diff.contains("+ordinary contents"));
    let scoped = driver
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            file: Some(ReviewDiffPreviewFile {
                path: ":(exclude)after.ts".into(),
                previous_path: None,
                source_kind: WT,
            }),
            ..preview(cwd.str())
        })
        .await
        .unwrap();
    let scoped = source(&scoped, WT);
    assert_eq!(scoped.files, Some(vec![stat(":(exclude)after.ts", None, 1, 0)]));
    assert!(scoped.diff.contains("+literal pathspec contents"));
    assert!(!scoped.diff.contains("ordinary.ts"));
    assert_eq!(git(&cwd.path, &["ls-files", "--stage"]), index_before);
}

#[tokio::test]
async fn detects_an_unstaged_rename_with_edits_without_mutating_a_split_index() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, "before.ts", "one\ntwo\nthree\nfour\nfive\n");
    git(&cwd.path, &["add", "before.ts"]);
    git(&cwd.path, &["commit", "-m", "add source file"]);
    git(&cwd.path, &["config", "core.splitIndex", "true"]);
    git(&cwd.path, &["config", "splitIndex.sharedIndexExpire", "now"]);
    git(&cwd.path, &["update-index", "--split-index"]);
    let index = git(&cwd.path, &["rev-parse", "--git-path", "index"]);
    let hash_before = git(&cwd.path, &["hash-object", &index]);
    let shared = |dir: &std::path::Path| {
        let mut entries: Vec<String> = std::fs::read_dir(dir.join(".git"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("sharedindex."))
            .collect();
        entries.sort();
        entries
    };
    let shared_before = shared(&cwd.path);
    std::fs::rename(cwd.path.join("before.ts"), cwd.path.join("after.ts")).unwrap();
    write(&cwd.path, "after.ts", "one\ntwo\nTHREE\nfour\nfive\n");
    let (driver, _w) = driver();
    let result = driver.get_review_diff_preview(&preview(cwd.str())).await.unwrap();
    let wt = source(&result, WT);
    assert_eq!(wt.files, Some(vec![stat("after.ts", Some("before.ts"), 1, 1)]));
    let scoped = driver
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            file: Some(ReviewDiffPreviewFile {
                path: "after.ts".into(),
                previous_path: Some("before.ts".into()),
                source_kind: WT,
            }),
            ..preview(cwd.str())
        })
        .await
        .unwrap();
    assert_eq!(source(&scoped, WT).files, wt.files);
    assert_eq!(source(&scoped, WT).diff, wt.diff);
    assert!(wt.diff.contains("rename from before.ts"));
    assert!(wt.diff.contains("rename to after.ts"));
    assert!(wt.diff.contains("-three") && wt.diff.contains("+THREE"));
    assert_eq!(wt.diff.matches("diff --git ").count(), 1);
    assert_eq!(git(&cwd.path, &["hash-object", &index]), hash_before);
    assert_eq!(shared(&cwd.path), shared_before);
}

#[tokio::test]
async fn keeps_tracked_changes_visible_when_untracked_discovery_fails() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, "README.md", "# tracked change\n");
    let recorder = Recorder::responding(|input| {
        if input.args.first().map(String::as_str) == Some("ls-files") && input.args.get(1).map(String::as_str) == Some("--others") {
            return not_a_repository();
        }
        None
    });
    let (driver, _w) = driver_with(recorder);
    let result = driver.get_review_diff_preview(&preview(cwd.str())).await.unwrap();
    let wt = source(&result, WT);
    assert!(wt.diff.contains("-# test") && wt.diff.contains("+# tracked change"));
    // Without the untracked manifest the statistics are incomplete: no `files`, truncated.
    assert!(wt.files.is_none());
    assert!(wt.truncated);
}

#[tokio::test]
async fn preserves_a_staged_deletion_when_the_removed_path_still_exists() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, "removed.txt", "remove me\n");
    git(&cwd.path, &["add", "removed.txt"]);
    git(&cwd.path, &["commit", "-m", "add removable file"]);
    git(&cwd.path, &["rm", "--cached", "removed.txt"]);
    let (driver, _w) = driver();
    let result = driver.get_review_diff_preview(&preview(cwd.str())).await.unwrap();
    let diff = &source(&result, WT).diff;
    assert!(diff.contains("deleted file mode"));
    assert!(diff.contains("-remove me"));
    assert!(!diff.contains("new file mode"));
}

#[tokio::test]
async fn keeps_untracked_files_visible_before_the_first_commit() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    driver.init_repo(cwd.str()).await.unwrap();
    write(&cwd.path, "untracked.txt", "visible before HEAD\n");
    let result = driver.get_review_diff_preview(&preview(cwd.str())).await.unwrap();
    let wt = source(&result, WT);
    assert!(wt.diff.contains("visible before HEAD"));
    assert!(!wt.truncated);
}

#[tokio::test]
async fn keeps_complete_stats_for_files_beyond_the_patch_limit() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    let large = "a long line of changed content for the diff preview\n".repeat(4000);
    git(&cwd.path, &["checkout", "-b", "feature/large"]);
    write(&cwd.path, "a-large.txt", &large);
    write(&cwd.path, "z-last.txt", "last file\n");
    commit_all(&cwd.path, "large change");
    write(&cwd.path, "a-large.txt", &large.replace("changed", "updated"));
    write(&cwd.path, "z-last.txt", "last file updated\n");
    write(&cwd.path, "untracked.txt", &large);
    let (driver, _w) = driver();
    let base = || ReviewDiffPreviewInput {
        base_ref: Some(branch.clone()),
        ..preview(cwd.str())
    };
    let result = driver.get_review_diff_preview(&base()).await.unwrap();
    let (branch_source, dirty) = (source(&result, BR), source(&result, WT));
    assert!(branch_source.truncated && dirty.truncated);
    assert!(!branch_source.diff.contains("z-last.txt"));
    assert!(branch_source.diff.ends_with("\n\n[truncated]") || branch_source.diff.len() <= 120_000);
    for (kind, src) in [(BR, branch_source), (WT, dirty)] {
        for file in src.files.clone().unwrap() {
            let individual = driver
                .get_review_diff_preview(&ReviewDiffPreviewInput {
                    file: Some(ReviewDiffPreviewFile {
                        path: file.path.clone(),
                        previous_path: file.previous_path.clone(),
                        source_kind: kind,
                    }),
                    ..base()
                })
                .await
                .unwrap();
            let patch = source(&individual, kind);
            assert!(!patch.truncated);
            assert_eq!(patch.files.as_ref().unwrap(), &vec![file.clone()]);
            assert!(patch.diff.contains(&format!("b/{}", file.path)));
            let other = individual.sources.iter().find(|s| s.kind != kind).unwrap();
            assert!(other.diff.is_empty());
        }
    }
    assert_eq!(
        branch_source.files,
        Some(vec![stat("a-large.txt", None, 4000, 0), stat("z-last.txt", None, 1, 0)])
    );
    assert_eq!(
        dirty.files,
        Some(vec![
            stat("a-large.txt", None, 4000, 4000),
            stat("untracked.txt", None, 4000, 0),
            stat("z-last.txt", None, 1, 1),
        ])
    );
}

#[tokio::test]
async fn preserves_renames_unusual_paths_modes_and_binary_statistics() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    write(&cwd.path, "mode-only.sh", "echo unchanged\n");
    git(&cwd.path, &["add", "mode-only.sh"]);
    git(&cwd.path, &["commit", "-m", "add executable candidate"]);
    git(&cwd.path, &["checkout", "-b", "feature/paths"]);
    git(&cwd.path, &["mv", "README.md", "renamed.md"]);
    write(&cwd.path, "[literal].txt", "literal\n");
    write(&cwd.path, " leading.txt", "whitespace path\n");
    write(&cwd.path, "l.txt", "other\n");
    std::fs::write(cwd.path.join("binary.dat"), b"binary\0data").unwrap();
    write(&cwd.path, "tab\tand\nnewline.txt", "unusual path\n");
    git(&cwd.path, &["add", "."]);
    git(&cwd.path, &["update-index", "--chmod=+x", "mode-only.sh"]);
    git(&cwd.path, &["commit", "-m", "rename and add files"]);
    let (driver, _w) = driver();
    let result = driver
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            base_ref: Some(branch.clone()),
            ..preview(cwd.str())
        })
        .await
        .unwrap();
    let br = source(&result, BR);
    let files = br.files.clone().unwrap();
    for path in ["renamed.md", "[literal].txt", " leading.txt", "mode-only.sh"] {
        let file = files.iter().find(|f| f.path == path).unwrap().clone();
        // The RPC decodes `file.path` as a NonEmptyString (not trimmed), so " leading.txt"
        // keeps its space.
        let request: ReviewDiffPreviewInput = serde_json::from_value(serde_json::json!({
            "cwd": cwd.str(),
            "baseRef": branch,
            "file": {"path": path, "previousPath": file.previous_path, "sourceKind": "branch-range"}
        }))
        .unwrap();
        let scoped = driver.get_review_diff_preview(&request).await.unwrap();
        let scoped = source(&scoped, BR);
        assert_eq!(scoped.files.as_ref().unwrap(), &vec![file.clone()]);
        assert!(!scoped.diff.contains("b/l.txt"));
        if path == "renamed.md" {
            assert!(scoped.diff.contains("rename from README.md"));
        }
        if path == "mode-only.sh" {
            assert!(scoped.diff.contains("old mode 100644") && scoped.diff.contains("new mode 100755"));
        }
    }
    assert!(br.diff.contains("rename from README.md") && br.diff.contains("rename to renamed.md"));
    assert!(files.contains(&stat("renamed.md", Some("README.md"), 0, 0)));
    assert!(files.contains(&stat("binary.dat", None, 0, 0)));
    assert!(files.contains(&stat("tab\tand\nnewline.txt", None, 1, 0)));
}

#[tokio::test]
async fn reports_staged_and_untracked_changes_before_the_first_commit() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    git(&cwd.path, &["init"]);
    write(&cwd.path, "staged.txt", "staged\n");
    git(&cwd.path, &["add", "staged.txt"]);
    write(&cwd.path, "untracked.txt", "untracked\n");
    let (driver, _w) = driver();
    let result = driver.get_review_diff_preview(&preview(cwd.str())).await.unwrap();
    let wt = source(&result, WT);
    assert_eq!(wt.files, Some(vec![stat("staged.txt", None, 1, 0), stat("untracked.txt", None, 1, 0)]));
    assert!(wt.diff.contains("b/staged.txt") && wt.diff.contains("b/untracked.txt"));
    // No branch: the branch-range source has no base and is empty.
    let br = source(&result, BR);
    assert_eq!(br.title, "Against base branch");
    assert!(br.diff.is_empty());
}

#[tokio::test]
async fn hashes_diffs_like_ts_and_reports_titles() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    let (driver, _w) = driver();
    let result = driver
        .get_review_diff_preview(&ReviewDiffPreviewInput {
            base_ref: Some(branch.clone()),
            ..preview(cwd.str())
        })
        .await
        .unwrap();
    use sha2::Digest;
    let empty_hash: String = sha2::Sha256::digest(b"[\"\",[]]").iter().map(|b| format!("{b:02x}")).collect();
    let wt = source(&result, WT);
    assert_eq!(wt.diff_hash, empty_hash);
    assert_eq!(wt.title, "Dirty worktree");
    assert_eq!(wt.base_ref.as_deref(), Some("HEAD"));
    assert_eq!(wt.head_ref, None);
    let br = source(&result, BR);
    assert_eq!(br.title, format!("Against {branch}"));
    assert_eq!(br.head_ref.as_deref(), Some(branch.as_str()));
    assert!(result.generated_at.ends_with('Z') && result.generated_at.len() == 24);
}

#[tokio::test]
async fn returns_no_sources_outside_a_repository() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    let result = driver.get_review_diff_preview(&preview(cwd.str())).await.unwrap();
    assert!(result.sources.is_empty());
    let missing = cwd.join("missing");
    assert!(driver.get_review_diff_preview(&preview(&missing)).await.unwrap().sources.is_empty());
}

// ---------------------------------------------------------------------------------------------
// file contents
// ---------------------------------------------------------------------------------------------

fn contents_input(cwd: &str) -> ReviewDiffFileContentsInput {
    ReviewDiffFileContentsInput {
        cwd: cwd.to_owned(),
        source_kind: WT,
        change_type: ReviewDiffChangeType::Change,
        base_ref: Some("HEAD".into()),
        head_ref: None,
        old_path: "README.md".into(),
        new_path: "README.md".into(),
    }
}

#[tokio::test]
async fn loads_full_file_contents_for_working_tree_expansion() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, "nested/.keep", "");
    write(&cwd.path, "README.md", "# changed\nunchanged context\n");
    let (driver, _w) = driver();
    let contents = driver.get_review_diff_file_contents(&contents_input(&cwd.join("nested"))).await.unwrap();
    assert_eq!(contents.old_contents, "# test\n");
    assert_eq!(contents.new_contents, "# changed\nunchanged context\n");
}

#[tokio::test]
async fn attributes_working_tree_filesystem_failures_to_the_failing_operation() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let (driver, _w) = driver();
    let error = driver
        .get_review_diff_file_contents(&ReviewDiffFileContentsInput {
            change_type: ReviewDiffChangeType::New,
            old_path: "missing.ts".into(),
            new_path: "missing.ts".into(),
            ..contents_input(cwd.str())
        })
        .await
        .unwrap_err();
    assert_eq!(error.operation, "GitVcsDriver.getReviewDiffFileContents.workingTree.fs.realPath");
    assert_eq!(error.command, "fs.realPath");
    assert_eq!(error.cwd, cwd.str());
    assert_eq!(error.detail, "Could not resolve diff file 'missing.ts'.");
}

#[tokio::test]
async fn refuses_paths_outside_the_repository_and_binary_files() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    std::fs::write(cwd.path.join("blob.bin"), b"a\0b").unwrap();
    let (driver, _w) = driver();
    let outside = driver
        .get_review_diff_file_contents(&ReviewDiffFileContentsInput {
            change_type: ReviewDiffChangeType::New,
            new_path: "../escape.txt".into(),
            ..contents_input(cwd.str())
        })
        .await
        .unwrap_err();
    assert_eq!(outside.command, "path.resolve");
    assert!(outside.detail.contains("resolves outside the review workspace"));
    let binary = driver
        .get_review_diff_file_contents(&ReviewDiffFileContentsInput {
            change_type: ReviewDiffChangeType::New,
            new_path: "blob.bin".into(),
            ..contents_input(cwd.str())
        })
        .await
        .unwrap_err();
    assert_eq!(binary.detail, "Cannot expand binary file 'blob.bin'.");
}

#[tokio::test]
async fn loads_new_and_deleted_files_without_reading_their_missing_side() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, "added.ts", "export const added = true;\n");
    std::fs::remove_file(cwd.path.join("README.md")).unwrap();
    let (driver, _w) = driver();
    let added = driver
        .get_review_diff_file_contents(&ReviewDiffFileContentsInput {
            change_type: ReviewDiffChangeType::New,
            old_path: "added.ts".into(),
            new_path: "added.ts".into(),
            ..contents_input(cwd.str())
        })
        .await
        .unwrap();
    let deleted = driver
        .get_review_diff_file_contents(&ReviewDiffFileContentsInput {
            change_type: ReviewDiffChangeType::Deleted,
            ..contents_input(cwd.str())
        })
        .await
        .unwrap();
    assert_eq!((added.old_contents.as_str(), added.new_contents.as_str()), ("", "export const added = true;\n"));
    assert_eq!((deleted.old_contents.as_str(), deleted.new_contents.as_str()), ("# test\n", ""));
}

#[tokio::test]
async fn loads_merge_base_and_head_contents_for_branch_expansion() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["checkout", "-b", "feature/context"]);
    write(&cwd.path, "README.md", "# branch change\nunchanged context\n");
    git(&cwd.path, &["add", "README.md"]);
    git(&cwd.path, &["commit", "-m", "change readme"]);
    let (driver, _w) = driver();
    let contents = driver
        .get_review_diff_file_contents(&ReviewDiffFileContentsInput {
            source_kind: BR,
            base_ref: Some(branch),
            head_ref: Some("feature/context".into()),
            ..contents_input(cwd.str())
        })
        .await
        .unwrap();
    assert_eq!(contents.old_contents, "# test\n");
    assert_eq!(contents.new_contents, "# branch change\nunchanged context\n");
    let missing_head = driver
        .get_review_diff_file_contents(&ReviewDiffFileContentsInput {
            source_kind: BR,
            head_ref: None,
            ..contents_input(cwd.str())
        })
        .await
        .unwrap_err();
    assert_eq!(missing_head.detail, "Branch diff file expansion requires both base and head refs.");
}

// ---------------------------------------------------------------------------------------------
// ReviewService
// ---------------------------------------------------------------------------------------------

fn review_service(workspace: &Tmp, base: &Tmp) -> ReviewService {
    let (driver, _w) = driver();
    let registry = VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::new(VcsProcess::default())));
    ReviewService::new(&workspace.path, base.path.join("worktrees"), registry, driver)
}

#[tokio::test]
async fn rejects_diff_preview_cwds_outside_the_workspace_roots() {
    let workspace = Tmp::new("t3-review-workspace-");
    let outside = Tmp::new("t3-review-outside-");
    let base = Tmp::new("t3-review-base-");
    let review = review_service(&workspace, &base);
    let error = review.get_diff_preview(&preview(outside.str())).await.unwrap_err();
    let ReviewDiffPreviewError::Vcs(VcsError::RepositoryDetection(error)) = error else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(error.operation, "ReviewService.getDiffPreview");
    assert!(error.detail.contains("must stay within the configured workspace root"));
    let encoded = serde_json::to_value(ReviewDiffPreviewError::Vcs(VcsError::RepositoryDetection(error))).unwrap();
    assert_eq!(encoded["_tag"], "VcsRepositoryDetectionError");
}

#[tokio::test]
async fn attributes_file_content_violations_to_the_file_content_operation() {
    let workspace = Tmp::new("t3-review-workspace-");
    let outside = Tmp::new("t3-review-outside-");
    let base = Tmp::new("t3-review-base-");
    let review = review_service(&workspace, &base);
    let error = review
        .get_diff_file_contents(&ReviewDiffFileContentsInput {
            old_path: "file.ts".into(),
            new_path: "file.ts".into(),
            ..contents_input(outside.str())
        })
        .await
        .unwrap_err();
    let ReviewDiffPreviewError::Vcs(VcsError::RepositoryDetection(error)) = error else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(error.operation, "ReviewService.getDiffFileContents");
    assert!(error.detail.contains("must stay within the configured workspace root"));
}

#[tokio::test]
async fn allows_diff_previews_inside_the_workspace_root_and_worktrees_dir() {
    let workspace = Tmp::new("t3-review-workspace-");
    let base = Tmp::new("t3-review-base-");
    let review = review_service(&workspace, &base);
    let result = review.get_diff_preview(&preview(workspace.str())).await.unwrap();
    assert_eq!(result.cwd, workspace.str());
    assert!(result.sources.is_empty());

    let repo = workspace.join("repo");
    std::fs::create_dir(&repo).unwrap();
    init_repo_with_commit(&repo);
    write(&repo, "README.md", "# changed\n");
    let result = review.get_diff_preview(&preview(&repo)).await.unwrap();
    assert_eq!(result.sources.len(), 2);
    let contents = review.get_diff_file_contents(&contents_input(&repo)).await.unwrap();
    assert_eq!(contents.new_contents, "# changed\n");

    // A directory inside the workspace but outside any repository cannot expand files.
    let plain = workspace.join("plain");
    std::fs::create_dir(&plain).unwrap();
    let error = review.get_diff_file_contents(&contents_input(&plain)).await.unwrap_err();
    let ReviewDiffPreviewError::Vcs(VcsError::UnsupportedOperation(error)) = error else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(error.kind, VcsDriverKind::Unknown);
}

#[tokio::test]
async fn preserves_unexpected_path_resolution_failures() {
    let workspace = Tmp::new("t3-review-workspace-");
    let base = Tmp::new("t3-review-base-");
    let review = review_service(&workspace, &base);
    let invalid = format!("{}\0invalid", workspace.str());
    let error = review.get_diff_preview(&preview(&invalid)).await.unwrap_err();
    let ReviewDiffPreviewError::Vcs(VcsError::RepositoryDetection(error)) = error else {
        panic!("unexpected error {error:?}");
    };
    assert_eq!(error.operation, "ReviewService.assertWorkspaceBoundCwd.canonicalizePath");
    assert_eq!(error.cwd, invalid);
    assert!(error.detail.contains("Failed to resolve a path"));
    assert!(error.cause.is_some());
}
