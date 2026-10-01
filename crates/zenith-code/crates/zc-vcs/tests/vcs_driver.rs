//! Ports of `vcs/testing/VcsDriverContractHarness.ts` (run against the git driver) and of the
//! checkpoint tests of `vcs/GitVcsDriver.test.ts`.

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use std::path::Path;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner, SystemProcessRunner};
use zc_core::vcs_process::{VcsProcess, VcsProcessInput};
use zc_vcs::contracts::*;
use zc_vcs::vcs_driver::{DiffCheckpointsInput, GitVcsProcessDriver, VcsDriver};

fn live() -> GitVcsProcessDriver {
    isolate_git();
    GitVcsProcessDriver::new(VcsProcess::default())
}

/// Wraps the real runner: records every input, lets a hook rewrite inputs and outputs.
struct Wrapped {
    calls: Mutex<Vec<ProcessRunInput>>,
    rewrite_input: Box<dyn Fn(ProcessRunInput) -> ProcessRunInput + Send + Sync>,
    rewrite_output: Box<dyn Fn(&ProcessRunInput, ProcessRunOutput) -> ProcessRunOutput + Send + Sync>,
}

impl Wrapped {
    fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            rewrite_input: Box::new(|input| input),
            rewrite_output: Box::new(|_, output| output),
        }
    }
}

#[async_trait]
impl ProcessRunner for Wrapped {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        let input = (self.rewrite_input)(input);
        self.calls.lock().unwrap().push(input.clone());
        let output = SystemProcessRunner.run(input.clone()).await?;
        Ok((self.rewrite_output)(&input, output))
    }
}

/// A fake runner answering from a closure, recording every input.
struct Fake {
    calls: Mutex<Vec<ProcessRunInput>>,
    answer: Box<dyn Fn(&ProcessRunInput) -> String + Send + Sync>,
}

#[async_trait]
impl ProcessRunner for Fake {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        let stdout = (self.answer)(&input);
        self.calls.lock().unwrap().push(input);
        Ok(ProcessRunOutput {
            stdout,
            code: Some(0),
            ..ProcessRunOutput::default()
        })
    }
}

// ---------------------------------------------------------------------------------------------
// contract harness
// ---------------------------------------------------------------------------------------------

fn create_repo(cwd: &Path) {
    git(cwd, &["init"]);
    git(cwd, &["config", "user.email", "test@test.com"]);
    git(cwd, &["config", "user.name", "Test"]);
}

#[tokio::test]
async fn contract_returns_null_outside_a_repository() {
    let cwd = Tmp::new("t3-git-vcs-contract-");
    let driver = live();
    assert_eq!(driver.detect_repository(cwd.str()).await.unwrap(), None);
    assert!(!driver.is_inside_work_tree(cwd.str()).await.unwrap());
    // A missing directory is not a repository either (git -C fails).
    assert!(!driver.is_inside_work_tree(&cwd.join("missing")).await.unwrap());
}

#[tokio::test]
async fn contract_detects_repository_identity_in_nested_directories() {
    let cwd = Tmp::new("t3-git-vcs-contract-");
    let driver = live();
    create_repo(&cwd.path);
    write(&cwd.path, "src/index.ts", "export const value = 1;\n");
    let identity = driver.detect_repository(cwd.str()).await.unwrap().unwrap();
    assert_eq!(identity.kind, VcsDriverKind::Git);
    assert!(identity.root_path.ends_with(cwd.str()));
    assert_eq!(identity.metadata_path.as_deref(), Some(".git"));
    assert_eq!(identity.freshness.source, VcsFreshnessSource::LiveLocal);
    assert_eq!(identity.freshness.expires_at, TaggedOption::None);
    assert_eq!(identity.freshness.observed_at.len(), 24);
    assert!(driver.is_inside_work_tree(cwd.str()).await.unwrap());
    let nested = cwd.join("src");
    let nested_identity = driver.detect_repository(&nested).await.unwrap().unwrap();
    assert_eq!(nested_identity.root_path, identity.root_path);
    assert!(driver.is_inside_work_tree(&nested).await.unwrap());
    let encoded = serde_json::to_value(&identity).unwrap();
    assert_eq!(encoded["freshness"]["expiresAt"], serde_json::json!({"_tag": "None"}));
}

#[tokio::test]
async fn contract_lists_tracked_and_untracked_non_ignored_files() {
    let cwd = Tmp::new("t3-git-vcs-contract-");
    let driver = live();
    create_repo(&cwd.path);
    write(&cwd.path, "tracked.ts", "export const tracked = true;\n");
    git(&cwd.path, &["add", "tracked.ts"]);
    git(&cwd.path, &["commit", "-m", "Track file"]);
    write(&cwd.path, "untracked.ts", "export const untracked = true;\n");
    let result = driver.list_workspace_files(cwd.str()).await.unwrap();
    assert!(result.paths.contains(&"tracked.ts".to_owned()));
    assert!(result.paths.contains(&"untracked.ts".to_owned()));
    assert!(!result.truncated);
    assert_eq!(result.freshness.source, VcsFreshnessSource::LiveLocal);
}

#[tokio::test]
async fn contract_excludes_ignored_files_from_workspace_listing() {
    let cwd = Tmp::new("t3-git-vcs-contract-");
    let driver = live();
    create_repo(&cwd.path);
    write(&cwd.path, ".gitignore", "*.log\n");
    write(&cwd.path, "included.ts", "export const included = true;\n");
    write(&cwd.path, "debug.log", "ignore me\n");
    write(&cwd.path, "nested/error.log", "ignore me too\n");
    let result = driver.list_workspace_files(cwd.str()).await.unwrap();
    assert!(result.paths.contains(&"included.ts".to_owned()));
    assert!(!result.paths.contains(&"debug.log".to_owned()));
    assert!(!result.paths.contains(&"nested/error.log".to_owned()));
}

#[tokio::test]
async fn contract_filters_ignored_paths_and_keeps_empty_input() {
    let cwd = Tmp::new("t3-git-vcs-contract-");
    let driver = live();
    create_repo(&cwd.path);
    write(&cwd.path, ".gitignore", "*.log\n");
    let paths = vec!["keep.ts".to_owned(), "debug.log".to_owned(), "nested/error.log".to_owned()];
    assert_eq!(driver.filter_ignored_paths(cwd.str(), &paths).await.unwrap(), vec!["keep.ts"]);
    assert!(driver.filter_ignored_paths(cwd.str(), &[]).await.unwrap().is_empty());
    // Nothing ignored: the input comes back unchanged.
    let kept = vec!["a.ts".to_owned(), "b.ts".to_owned()];
    assert_eq!(driver.filter_ignored_paths(cwd.str(), &kept).await.unwrap(), kept);
}

#[tokio::test]
async fn lists_remotes_with_push_urls_and_the_primary_flag() {
    let cwd = Tmp::new("t3-git-vcs-contract-");
    let driver = live();
    create_repo(&cwd.path);
    git(&cwd.path, &["remote", "add", "origin", "https://github.com/o/r.git"]);
    git(&cwd.path, &["remote", "add", "fork", "git@github.com:me/r.git"]);
    git(&cwd.path, &["remote", "set-url", "--push", "fork", "git@github.com:me/push.git"]);
    let result = driver.list_remotes(cwd.str()).await.unwrap();
    let origin = result.remotes.iter().find(|r| r.name == "origin").unwrap();
    assert!(origin.is_primary);
    assert_eq!(
        origin.push_url,
        TaggedOption::Some {
            value: "https://github.com/o/r.git".into()
        }
    );
    let fork = result.remotes.iter().find(|r| r.name == "fork").unwrap();
    assert!(!fork.is_primary);
    assert_eq!(
        fork.push_url,
        TaggedOption::Some {
            value: "git@github.com:me/push.git".into()
        }
    );
}

#[tokio::test]
async fn init_repository_and_capabilities() {
    let cwd = Tmp::new("t3-git-vcs-contract-");
    let driver = live();
    driver
        .init_repository(&VcsInitInput {
            cwd: cwd.str().into(),
            kind: None,
        })
        .await
        .unwrap();
    assert!(cwd.path.join(".git").exists());
    let capabilities = serde_json::to_value(driver.capabilities()).unwrap();
    assert_eq!(
        capabilities,
        serde_json::json!({
            "kind": "git",
            "supportsWorktrees": true,
            "supportsBookmarks": false,
            "supportsAtomicSnapshot": false,
            "supportsPushDefaultRemote": true,
            "ignoreClassifier": "native"
        })
    );
}

// ---------------------------------------------------------------------------------------------
// checkpoints
// ---------------------------------------------------------------------------------------------

const REF: &str = "refs/t3/checkpoints/test";

/// `makeCheckpointFixture`: a commit, then `staged` in the index and `unstaged` on disk.
fn checkpoint_fixture(cwd: &Path) {
    create_repo(cwd);
    write(cwd, "file.txt", "initial\n");
    commit_all(cwd, "initial");
    write(cwd, "file.txt", "staged\n");
    git(cwd, &["add", "."]);
    write(cwd, "file.txt", "unstaged\n");
}

fn show(cwd: &Path, spec: &str) -> String {
    let output = std::process::Command::new("git").args(["show", spec]).current_dir(cwd).output().unwrap();
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[tokio::test]
async fn capture_records_the_working_tree_without_touching_the_index() {
    let cwd = Tmp::new("t3-checkpoint-");
    checkpoint_fixture(&cwd.path);
    write(&cwd.path, "untracked.txt", "new\n");
    let index_before = std::fs::read(cwd.path.join(".git/index")).unwrap();
    let driver = live();
    let checkpoints = driver.checkpoints().unwrap();
    checkpoints.capture_checkpoint(cwd.str(), REF).await.unwrap();
    assert_eq!(show(&cwd.path, &format!("{REF}:file.txt")), "unstaged\n");
    assert_eq!(show(&cwd.path, &format!("{REF}:untracked.txt")), "new\n");
    assert_eq!(std::fs::read(cwd.path.join(".git/index")).unwrap(), index_before);
    assert!(checkpoints.has_checkpoint_ref(cwd.str(), REF).await.unwrap());
    let log = git(&cwd.path, &["log", "-1", "--format=%an <%ae>|%s", REF]);
    assert_eq!(log, format!("T3 Code <t3code@users.noreply.github.com>|t3 checkpoint ref={REF}"));
    // The private index is gone.
    let leftovers: Vec<String> = std::fs::read_dir(cwd.path.join(".git"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("t3-checkpoint-index-"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[tokio::test]
async fn capture_skips_untracked_nested_repositories_without_a_commit() {
    let cwd = Tmp::new("t3-checkpoint-unborn-");
    checkpoint_fixture(&cwd.path);
    let nested = "scratch/empty [repo]";
    git(&cwd.path, &["init", nested]);
    git(&cwd.path, &["init", "another empty"]);
    write(cwd.path.join(nested), "private.txt", "nested\n");
    git(&cwd.path, &["init", "committed"]);
    git(
        &cwd.path,
        &[
            "-C",
            "committed",
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@test.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
    let nested_head = git(&cwd.path, &["-C", "committed", "rev-parse", "HEAD"]);
    write(&cwd.path, "untracked.txt", "new\n");
    let index_before = std::fs::read(cwd.path.join(".git/index")).unwrap();
    let driver = live();
    driver.checkpoints().unwrap().capture_checkpoint(cwd.str(), REF).await.unwrap();
    assert_eq!(show(&cwd.path, &format!("{REF}:file.txt")), "unstaged\n");
    assert_eq!(show(&cwd.path, &format!("{REF}:untracked.txt")), "new\n");
    assert_eq!(git(&cwd.path, &["ls-tree", "-r", REF, "--", nested]), "");
    assert_eq!(git(&cwd.path, &["ls-tree", REF, "--", "another empty"]), "");
    assert_eq!(
        git(&cwd.path, &["ls-tree", REF, "--", "committed"]),
        format!("160000 commit {nested_head}\tcommitted")
    );
    assert_eq!(std::fs::read(cwd.path.join(".git/index")).unwrap(), index_before);
    assert_eq!(read(cwd.path.join(nested), "private.txt"), "nested\n");
}

#[tokio::test]
async fn recovery_discovers_nested_head_independently_of_an_inherited_git_dir() {
    let cwd = Tmp::new("t3-checkpoint-git-dir-");
    checkpoint_fixture(&cwd.path);
    git(&cwd.path, &["init", "empty"]);
    let index_before = std::fs::read(cwd.path.join(".git/index")).unwrap();
    // Simulate a server started with GIT_DIR set: every command inherits it unless the
    // driver unsets it (the process env is shared by parallel tests, so inject it here).
    let git_dir = cwd.join(".git");
    let mut runner = Wrapped::new();
    runner.rewrite_input = Box::new(move |mut input| {
        let env = input.env.get_or_insert_with(Default::default);
        env.entry("GIT_DIR".into()).or_insert_with(|| Some(git_dir.clone()));
        input
    });
    let runner = Arc::new(runner);
    let driver = GitVcsProcessDriver::new(VcsProcess::new(runner.clone()));
    driver.checkpoints().unwrap().capture_checkpoint(cwd.str(), REF).await.unwrap();
    assert_eq!(show(&cwd.path, &format!("{REF}:file.txt")), "unstaged\n");
    assert_eq!(git(&cwd.path, &["ls-tree", "-r", REF, "--", "empty"]), "");
    assert_eq!(std::fs::read(cwd.path.join(".git/index")).unwrap(), index_before);
    // The nested probe ran with the git bindings removed.
    let unbound_probes = runner
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|c| c.env.as_ref().is_some_and(|env| env.get("GIT_DIR") == Some(&None)))
        .count();
    assert_eq!(unbound_probes, 1);
}

#[tokio::test]
async fn capture_still_fails_when_a_clean_filter_rejects_a_file() {
    let cwd = Tmp::new("t3-checkpoint-filter-failure-");
    checkpoint_fixture(&cwd.path);
    write(&cwd.path, ".gitattributes", "file.txt filter=reject\n");
    git(&cwd.path, &["config", "filter.reject.clean", "false"]);
    git(&cwd.path, &["config", "filter.reject.required", "true"]);
    let index_before = std::fs::read(cwd.path.join(".git/index")).unwrap();
    let driver = live();
    let checkpoints = driver.checkpoints().unwrap();
    let error = checkpoints.capture_checkpoint(cwd.str(), REF).await.unwrap_err();
    assert_eq!(error.tag(), "VcsProcessExitError");
    assert_eq!(std::fs::read(cwd.path.join(".git/index")).unwrap(), index_before);
    assert!(!checkpoints.has_checkpoint_ref(cwd.str(), REF).await.unwrap());
}

#[tokio::test]
async fn capture_refuses_a_truncated_nested_repository_listing() {
    let cwd = Tmp::new("t3-checkpoint-truncated-");
    checkpoint_fixture(&cwd.path);
    git(&cwd.path, &["init", "empty"]);
    let mut runner = Wrapped::new();
    runner.rewrite_output = Box::new(|input, mut output| {
        if input.args.iter().any(|a| a == "--others") {
            output.stdout_truncated = true;
        }
        output
    });
    let driver = GitVcsProcessDriver::new(VcsProcess::new(Arc::new(runner)));
    driver.checkpoints().unwrap().capture_checkpoint(cwd.str(), REF).await.unwrap_err();
    assert!(!live().checkpoints().unwrap().has_checkpoint_ref(cwd.str(), REF).await.unwrap());
}

#[tokio::test]
async fn recovery_refuses_excessive_candidates_before_probing() {
    let cwd = Tmp::new("t3-checkpoint-recovery-cap-");
    checkpoint_fixture(&cwd.path);
    for i in 0..65 {
        git(&cwd.path, &["init", &format!("empty{i}")]);
    }
    let index_before = std::fs::read(cwd.path.join(".git/index")).unwrap();
    let runner = Arc::new(Wrapped::new());
    let driver = GitVcsProcessDriver::new(VcsProcess::new(runner.clone()));
    let error = driver.checkpoints().unwrap().capture_checkpoint(cwd.str(), REF).await.unwrap_err();
    assert_eq!(error.tag(), "VcsProcessExitError");
    let calls = runner.calls.lock().unwrap();
    let probes = calls
        .iter()
        .filter(|c| c.args.get(1).is_some_and(|dir| dir != cwd.str()) && c.args.iter().any(|a| a == "rev-parse"))
        .count();
    assert_eq!(probes, 0);
    let stage_attempts = calls
        .iter()
        .filter(|c| c.args.iter().any(|a| a == "add") && c.args.iter().any(|a| a == "-A"))
        .count();
    assert_eq!(stage_attempts, 1);
    assert_eq!(std::fs::read(cwd.path.join(".git/index")).unwrap(), index_before);
}

#[tokio::test]
async fn capture_falls_back_when_the_user_index_is_missing_or_invalid() {
    for state in ["missing", "invalid"] {
        let cwd = Tmp::new("t3-checkpoint-index-");
        checkpoint_fixture(&cwd.path);
        let index = cwd.path.join(".git/index");
        if state == "missing" {
            std::fs::remove_file(&index).unwrap();
        } else {
            std::fs::write(&index, "invalid index").unwrap();
        }
        live().checkpoints().unwrap().capture_checkpoint(cwd.str(), REF).await.unwrap();
        assert_eq!(show(&cwd.path, &format!("{REF}:file.txt")), "unstaged\n", "{state}");
        if state == "missing" {
            assert!(!index.exists());
        } else {
            assert_eq!(std::fs::read_to_string(&index).unwrap(), "invalid index");
        }
    }
}

#[tokio::test]
async fn capture_keeps_sparse_cone_exclusions() {
    let cwd = Tmp::new("t3-checkpoint-sparse-");
    create_repo(&cwd.path);
    write(&cwd.path, "included/a.txt", "a\n");
    write(&cwd.path, "excluded/b.txt", "b\n");
    write(&cwd.path, "file.txt", "root\n");
    commit_all(&cwd.path, "sparse fixture");
    git(&cwd.path, &["sparse-checkout", "set", "--cone", "included"]);
    assert!(!cwd.path.join("excluded/b.txt").exists());
    write(&cwd.path, "included/a.txt", "changed\n");
    live().checkpoints().unwrap().capture_checkpoint(cwd.str(), REF).await.unwrap();
    assert_eq!(show(&cwd.path, &format!("{REF}:included/a.txt")), "changed\n");
    // The excluded file is not recorded as deleted.
    assert_eq!(show(&cwd.path, &format!("{REF}:excluded/b.txt")), "b\n");
}

#[tokio::test]
async fn restores_checkpoints_and_cleans_untracked_files() {
    let cwd = Tmp::new("t3-checkpoint-restore-");
    checkpoint_fixture(&cwd.path);
    let driver = live();
    let checkpoints = driver.checkpoints().unwrap();
    checkpoints.capture_checkpoint(cwd.str(), REF).await.unwrap();
    write(&cwd.path, "file.txt", "later\n");
    write(&cwd.path, "scratch/new.txt", "scratch\n");
    assert!(checkpoints.restore_checkpoint(cwd.str(), REF, false).await.unwrap());
    assert_eq!(read(&cwd.path, "file.txt"), "unstaged\n");
    assert!(!cwd.path.join("scratch").exists());
    // `reset`: nothing staged afterwards.
    assert_eq!(git(&cwd.path, &["diff", "--cached", "--name-only"]), "");
    assert!(!checkpoints.restore_checkpoint(cwd.str(), "refs/t3/checkpoints/missing", false).await.unwrap());
    assert!(checkpoints.restore_checkpoint(cwd.str(), "refs/t3/checkpoints/missing", true).await.unwrap());
    assert_eq!(read(&cwd.path, "file.txt"), "initial\n");
}

#[tokio::test]
async fn restores_empty_checkpoints_without_changing_paths_outside_the_workspace() {
    for nested in [false, true] {
        let root = Tmp::new("t3-empty-checkpoint-");
        create_repo(&root.path);
        if nested {
            write(&root.path, "outside.txt", "original\n");
            git(&root.path, &["add", "."]);
        }
        git(&root.path, &["commit", "--allow-empty", "-m", "initial"]);
        let cwd = if nested { root.path.join("nested") } else { root.path.clone() };
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd_str = cwd.to_str().unwrap();
        let driver = live();
        let checkpoints = driver.checkpoints().unwrap();
        let empty_ref = "refs/t3/checkpoints/empty";
        checkpoints.capture_checkpoint(cwd_str, empty_ref).await.unwrap();
        if nested {
            write(&root.path, "outside.txt", "changed\n");
            git(&root.path, &["add", "outside.txt"]);
        }
        for staged in [false, true] {
            write(&cwd, "added.txt", "new\n");
            if staged {
                git(&cwd, &["add", "added.txt"]);
            }
            assert!(checkpoints.restore_checkpoint(cwd_str, empty_ref, false).await.unwrap());
            assert!(!cwd.join("added.txt").exists(), "nested={nested} staged={staged}");
        }
        write(&root.path, ".git/info/exclude", "ignored.txt\n");
        write(&cwd, "ignored.txt", "keep\n");
        write(&cwd, "untracked/file.txt", "remove\n");
        assert!(checkpoints.restore_checkpoint(cwd_str, empty_ref, false).await.unwrap());
        assert_eq!(read(&cwd, "ignored.txt"), "keep\n");
        assert!(!cwd.join("untracked").exists());
        if nested {
            assert_eq!(read(&root.path, "outside.txt"), "changed\n");
            assert_eq!(git(&root.path, &["diff", "--cached", "--name-only"]), "outside.txt");
        }
    }
}

#[tokio::test]
async fn diffs_checkpoints_as_patches_and_numstat() {
    let cwd = Tmp::new("t3-checkpoint-diff-");
    checkpoint_fixture(&cwd.path);
    let driver = live();
    let checkpoints = driver.checkpoints().unwrap();
    let first = "refs/t3/checkpoints/thread/turn/1";
    let second = "refs/t3/checkpoints/thread/turn/2";
    checkpoints.capture_checkpoint(cwd.str(), first).await.unwrap();
    write(&cwd.path, "file.txt", "second turn\n");
    write(&cwd.path, "added.txt", "added\n");
    checkpoints.capture_checkpoint(cwd.str(), second).await.unwrap();
    let input = DiffCheckpointsInput {
        cwd: cwd.str().into(),
        from_checkpoint_ref: first.into(),
        to_checkpoint_ref: second.into(),
        fallback_from_to_head: false,
        ignore_whitespace: false,
        numstat: false,
    };
    let patch = checkpoints.diff_checkpoints(&input).await.unwrap();
    assert!(patch.contains("diff --git a/file.txt b/file.txt"));
    assert!(patch.contains("-unstaged\n+second turn"));
    assert!(patch.contains("+++ b/added.txt"));
    let numstat = checkpoints
        .diff_checkpoints(&DiffCheckpointsInput {
            numstat: true,
            ..input.clone()
        })
        .await
        .unwrap();
    assert_eq!(numstat, "1\t0\tadded.txt\x001\t1\tfile.txt\0");
    // A missing "from" falls back to HEAD only when asked to.
    let missing = DiffCheckpointsInput {
        from_checkpoint_ref: "refs/t3/checkpoints/thread/turn/0".into(),
        ..input.clone()
    };
    let error = checkpoints.diff_checkpoints(&missing).await.unwrap_err();
    assert_eq!(error.tag(), "VcsProcessExitError");
    let from_head = checkpoints
        .diff_checkpoints(&DiffCheckpointsInput {
            fallback_from_to_head: true,
            ..missing
        })
        .await
        .unwrap();
    assert!(from_head.contains("-initial\n+second turn"));
    checkpoints
        .delete_checkpoint_refs(cwd.str(), &[first.to_owned(), "refs/t3/checkpoints/never".to_owned()])
        .await
        .unwrap();
    assert!(!checkpoints.has_checkpoint_ref(cwd.str(), first).await.unwrap());
    assert!(checkpoints.has_checkpoint_ref(cwd.str(), second).await.unwrap());
}

#[tokio::test]
async fn forwards_execute_env_and_options_to_the_vcs_process() {
    let runner = Arc::new(Fake {
        calls: Mutex::new(Vec::new()),
        answer: Box::new(|_| String::new()),
    });
    let driver = GitVcsProcessDriver::new(VcsProcess::new(runner.clone()));
    let mut env = std::collections::BTreeMap::new();
    env.insert("GIT_INDEX_FILE".to_owned(), Some("/tmp/t3-index".to_owned()));
    driver
        .execute(VcsProcessInput {
            env: Some(env.clone()),
            append_truncation_marker: true,
            output_mode: Some(zc_core::process::OutputMode::Error),
            ..VcsProcessInput::new("GitVcsDriver.test.env", "ignored", ["status"], "/repo")
        })
        .await
        .unwrap();
    let calls = runner.calls.lock().unwrap();
    assert_eq!(calls[0].env, Some(env));
    assert_eq!(calls[0].output_mode, zc_core::process::OutputMode::Error);
    assert_eq!(calls[0].command, "git");
    assert_eq!(calls[0].args, ["-C", "/repo", "status"]);
    assert_eq!(calls[0].truncated_marker.as_deref(), Some("\n\n[truncated]"));
}

#[tokio::test]
async fn flushes_checkpoint_objects_and_refs_before_publishing_them() {
    let runner = Arc::new(Fake {
        calls: Mutex::new(Vec::new()),
        answer: Box::new(|input| {
            if input.args.iter().any(|a| a == "write-tree") {
                "tree0000\n".into()
            } else if input.args.iter().any(|a| a == "commit-tree") {
                "commit0000\n".into()
            } else if input.args.iter().any(|a| a == "--git-common-dir") {
                ".git\n".into()
            } else {
                String::new()
            }
        }),
    });
    let driver = GitVcsProcessDriver::new(VcsProcess::new(runner.clone()));
    driver
        .checkpoints()
        .unwrap()
        .capture_checkpoint("/repo", "refs/t3/checkpoints/thread/turn/1")
        .await
        .unwrap();
    let calls = runner.calls.lock().unwrap();
    let writes: Vec<&Vec<String>> = calls
        .iter()
        .map(|c| &c.args)
        .filter(|args| ["add", "write-tree", "commit-tree", "update-ref"].iter().any(|w| args.iter().any(|a| a == w)))
        .collect();
    assert_eq!(writes.len(), 4);
    for args in &writes {
        let command = args
            .iter()
            .position(|a| ["add", "write-tree", "commit-tree", "update-ref"].contains(&a.as_str()))
            .unwrap();
        for setting in ["core.fsync=objects,reference", "core.fsyncMethod=fsync"] {
            let index = args.iter().position(|a| a == setting).unwrap();
            assert_eq!(args[index - 1], "-c");
            assert!(index < command);
        }
    }
    assert_eq!(
        calls.last().unwrap().args,
        [
            "-C",
            "/repo",
            "-c",
            "core.fsync=objects,reference",
            "-c",
            "core.fsyncMethod=fsync",
            "update-ref",
            "refs/t3/checkpoints/thread/turn/1",
            "commit0000"
        ]
    );
    // The commit is written as T3 Code through a private index.
    let commit = calls.iter().find(|c| c.args.iter().any(|a| a == "commit-tree")).unwrap();
    let env = commit.env.as_ref().unwrap();
    assert_eq!(env["GIT_AUTHOR_NAME"].as_deref(), Some("T3 Code"));
    assert!(env["GIT_INDEX_FILE"].as_deref().unwrap().starts_with("/repo/.git/t3-checkpoint-index-"));
}
