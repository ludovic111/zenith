//! Ports of `vcs/GitVcsDriverCore.test.ts`: status, refs, worktrees, execution and caches.

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use zc_ports::contracts::WorktreeSubmodules;
use zc_ports::git::{CreateWorktreeOptions, CreateWorktreeProgress, SubmodulesDisabledSource};
use zc_vcs::contracts::*;
use zc_vcs::errors::GitCommandError;
use zc_vcs::git_exec::{available_git_permits, ExecuteGitInput, ExecuteGitProgress, ExecuteGitResult, GitInterceptor, GitTimeout};
use zc_vcs::GitVcsDriver;

fn list_input(cwd: &str) -> VcsListRefsInput {
    VcsListRefsInput {
        cwd: cwd.to_owned(),
        ..VcsListRefsInput::default()
    }
}

fn worktree_input(cwd: &str, path: &str, ref_name: &str, new_ref: &str) -> VcsCreateWorktreeInput {
    VcsCreateWorktreeInput {
        cwd: cwd.to_owned(),
        ref_name: ref_name.to_owned(),
        new_ref_name: Some(new_ref.to_owned()),
        base_ref_name: None,
        path: Some(path.to_owned()),
    }
}

// ---------------------------------------------------------------------------------------------
// repository status
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reports_non_repository_directories_without_failing() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    let refs = driver.list_refs(&list_input(cwd.str())).await.unwrap();
    assert!(!refs.is_repo);
    assert!(refs.refs.is_empty());
    let status = driver.status_details(cwd.str()).await.unwrap();
    assert!(!status.is_repo);
}

#[tokio::test]
async fn reports_ref_name_and_dirty_state() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    write(&cwd.path, "feature.ts", "export const value = 1;\n");
    let (driver, _w) = driver();
    let status = driver.status_details(cwd.str()).await.unwrap();
    assert!(status.is_repo);
    assert_eq!(status.branch.as_deref(), Some(branch.as_str()));
    assert!(status.has_working_tree_changes);
    assert!(status.working_tree.files.iter().any(|f| f.path == "feature.ts"));
}

#[tokio::test]
async fn reports_changes_to_a_file_named_head() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, "HEAD", "first line\n");
    git(&cwd.path, &["add", "HEAD"]);
    git(&cwd.path, &["commit", "-m", "add HEAD file"]);
    write(&cwd.path, "HEAD", "first line\nsecond line\n");
    let (driver, _w) = driver();
    let status = driver.status_details(cwd.str()).await.unwrap();
    assert!(status.has_working_tree_changes);
    assert!(status.working_tree.files.contains(&WorkingTreeFile {
        path: "HEAD".into(),
        insertions: 1,
        deletions: 0
    }));
}

fn repo_with_origin() -> (Tmp, Tmp, String) {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = Tmp::new("git-vcs-driver-remote-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&remote.path, &["init", "--bare"]);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    git(&cwd.path, &["push", "-u", "origin", &branch]);
    (cwd, remote, branch)
}

#[tokio::test]
async fn reports_default_branch_delta_separately_from_upstream_delta() {
    let (cwd, _remote, _branch) = repo_with_origin();
    git(&cwd.path, &["checkout", "-b", "feature/synced"]);
    write(&cwd.path, "feature.txt", "feature\n");
    commit_all(&cwd.path, "feature commit");
    git(&cwd.path, &["push", "-u", "origin", "feature/synced"]);
    let (driver, _w) = driver();
    let status = driver.status_details(cwd.str()).await.unwrap();
    assert!(status.has_upstream);
    assert_eq!((status.ahead_count, status.behind_count), (0, 0));
    assert_eq!(status.ahead_of_default_count, 1);
}

#[tokio::test]
async fn reports_remote_divergence_without_reading_working_tree_details() {
    let (cwd, _remote, _branch) = repo_with_origin();
    git(&cwd.path, &["checkout", "-b", "feature/remote-status"]);
    write(&cwd.path, "feature.txt", "feature\n");
    commit_all(&cwd.path, "feature commit");
    git(&cwd.path, &["push", "-u", "origin", "feature/remote-status"]);
    write(&cwd.path, "untracked.txt", "local-only\n");
    let (driver, _w) = driver();
    let status = driver.status_details_remote(cwd.str(), true).await.unwrap();
    assert!(status.is_repo);
    assert_eq!(status.branch.as_deref(), Some("feature/remote-status"));
    assert!(status.has_upstream);
    assert_eq!((status.ahead_count, status.behind_count), (0, 0));
    assert_eq!(status.ahead_of_default_count, 1);
}

#[tokio::test]
async fn reports_remote_status_on_unborn_head() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    driver.init_repo(cwd.str()).await.unwrap();
    let branch = git(&cwd.path, &["symbolic-ref", "--short", "HEAD"]);
    let status = driver.status_details_remote(cwd.str(), false).await.unwrap();
    assert!(status.is_repo);
    assert_eq!(status.branch.as_deref(), Some(branch.as_str()));
    assert!(!status.has_upstream);
    assert_eq!((status.ahead_count, status.behind_count), (0, 0));
}

#[tokio::test]
async fn can_read_cached_remote_divergence_without_fetching_upstream() {
    let (cwd, remote, branch) = repo_with_origin();
    let updater = Tmp::new("git-vcs-driver-updater-");
    git(&updater.path, &["clone", remote.str(), "."]);
    git(&updater.path, &["config", "user.email", "test@test.com"]);
    git(&updater.path, &["config", "user.name", "Test"]);
    write(&updater.path, "remote.txt", "remote\n");
    commit_all(&updater.path, "remote commit");
    git(&updater.path, &["push", "origin", &branch]);
    let (driver, _w) = driver();
    let cached = driver.status_details_remote(cwd.str(), false).await.unwrap();
    let refreshed = driver.status_details_remote(cwd.str(), true).await.unwrap();
    assert_eq!(cached.behind_count, 0);
    assert_eq!(refreshed.behind_count, 1);
}

#[tokio::test]
async fn background_upstream_fetches_skip_auto_maintenance() {
    let (cwd, _remote, _branch) = repo_with_origin();
    git(&cwd.path, &["repack", "-d"]);
    write(&cwd.path, "second.txt", "second\n");
    commit_all(&cwd.path, "second commit");
    git(&cwd.path, &["push"]);
    git(&cwd.path, &["repack", "-d"]);
    git(&cwd.path, &["config", "gc.autoPackLimit", "1"]);
    git(&cwd.path, &["config", "gc.autoDetach", "false"]);
    git(&cwd.path, &["config", "maintenance.autoDetach", "false"]);
    let packs = || {
        git(&cwd.path, &["count-objects", "-v"])
            .lines()
            .find_map(|l| l.strip_prefix("packs: ").map(str::to_owned))
            .unwrap()
    };
    assert_eq!(packs(), "2");
    let recorder = Recorder::new();
    let (driver, _w) = driver_with(recorder.clone());
    driver.status_details_remote(cwd.str(), true).await.unwrap();
    assert_eq!(packs(), "2");
    let fetches = recorder.args().into_iter().filter(|a| has(a, "fetch")).collect::<Vec<_>>();
    assert_eq!(fetches.len(), 1);
    assert!(has(&fetches[0], "--no-auto-gc"));
    assert!(has(&fetches[0], "--git-dir"));
}

#[tokio::test]
async fn uses_origin_head_for_default_branch_with_a_non_origin_upstream() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let origin = Tmp::new("git-vcs-driver-origin-");
    let upstream = Tmp::new("git-vcs-driver-upstream-");
    init_repo_with_commit(&cwd.path);
    git(&origin.path, &["init", "--bare"]);
    git(&upstream.path, &["init", "--bare"]);
    git(&cwd.path, &["branch", "-M", "main"]);
    git(&cwd.path, &["remote", "add", "origin", origin.str()]);
    git(&cwd.path, &["remote", "add", "upstream", upstream.str()]);
    git(&cwd.path, &["push", "origin", "main"]);
    git(&cwd.path, &["push", "upstream", "main"]);
    git(&cwd.path, &["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/main"]);
    git(&cwd.path, &["checkout", "-b", "release"]);
    write(&cwd.path, "release.txt", "release\n");
    commit_all(&cwd.path, "release commit");
    git(&cwd.path, &["push", "-u", "upstream", "release"]);
    git(&cwd.path, &["symbolic-ref", "refs/remotes/upstream/HEAD", "refs/remotes/upstream/release"]);
    let (driver, _w) = driver();
    let status = driver.status_details_remote(cwd.str(), true).await.unwrap();
    assert_eq!(status.branch.as_deref(), Some("release"));
    assert_eq!(status.upstream_ref.as_deref(), Some("upstream/release"));
    assert!(!status.is_default_branch);
}

#[tokio::test]
async fn makes_background_upstream_status_fetches_non_interactive() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let temp = Tmp::new("git-vcs-driver-ssh-env-");
    let branch = init_repo_with_commit(&cwd.path);
    let log = temp.join("ssh-env.txt");
    let wrapper = temp.join("ssh-wrapper.sh");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\n\
             printf \"GCM_INTERACTIVE=%s\\n\" \"${{GCM_INTERACTIVE:-}}\" > '{log}'\n\
             printf \"GIT_ASKPASS=%s\\n\" \"${{GIT_ASKPASS:-}}\" >> '{log}'\n\
             printf \"GIT_TERMINAL_PROMPT=%s\\n\" \"${{GIT_TERMINAL_PROMPT:-}}\" >> '{log}'\n\
             printf \"SSH_ASKPASS=%s\\n\" \"${{SSH_ASKPASS:-}}\" >> '{log}'\n\
             printf \"SSH_ASKPASS_REQUIRE=%s\\n\" \"${{SSH_ASKPASS_REQUIRE:-}}\" >> '{log}'\n\
             exit 1\n"
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The process environment is shared by the parallel tests, so the wrapper comes from the
    // repository's `core.sshCommand` instead of `GIT_SSH`.
    git(&cwd.path, &["config", "core.sshCommand", &wrapper]);
    git(&cwd.path, &["remote", "add", "origin", "ssh://example.invalid/repo.git"]);
    git(&cwd.path, &["update-ref", &format!("refs/remotes/origin/{branch}"), "HEAD"]);
    git(&cwd.path, &["branch", "--set-upstream-to", &format!("origin/{branch}")]);
    let (driver, _w) = driver();
    driver.status_details(cwd.str()).await.unwrap();
    let logged = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        logged.trim().lines().collect::<Vec<_>>(),
        vec![
            "GCM_INTERACTIVE=never",
            "GIT_ASKPASS=",
            "GIT_TERMINAL_PROMPT=0",
            "SSH_ASKPASS=",
            "SSH_ASKPASS_REQUIRE=never",
        ]
    );
}

#[tokio::test]
async fn reuses_the_no_upstream_fallback_ahead_count() {
    let (cwd, _remote, _branch) = repo_with_origin();
    git(&cwd.path, &["checkout", "-b", "feature/no-upstream"]);
    write(&cwd.path, "feature.txt", "feature\n");
    commit_all(&cwd.path, "feature commit");
    let (driver, _w) = driver();
    let status = driver.status_details(cwd.str()).await.unwrap();
    assert!(!status.has_upstream);
    assert_eq!((status.ahead_count, status.behind_count), (1, 0));
    assert_eq!(status.ahead_of_default_count, 1);
}

#[tokio::test]
async fn reports_combined_staged_and_unstaged_edits() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, "feature.ts", "// line one\n");
    commit_all(&cwd.path, "add feature");
    write(&cwd.path, "feature.ts", "// line one\n// line two\n");
    git(&cwd.path, &["add", "feature.ts"]);
    write(&cwd.path, "feature.ts", "// line one\n// line two\n// line three\n");
    let (driver, _w) = driver();
    let status = driver.status_details(cwd.str()).await.unwrap();
    let file = status.working_tree.files.iter().find(|f| f.path == "feature.ts").unwrap();
    assert_eq!((file.insertions, file.deletions), (2, 0));
}

#[tokio::test]
async fn reports_staged_file_counts_on_unborn_head() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    driver.init_repo(cwd.str()).await.unwrap();
    write(&cwd.path, "initial.ts", "// first file\n");
    git(&cwd.path, &["add", "initial.ts"]);
    let status = driver.status_details(cwd.str()).await.unwrap();
    assert!(status.is_repo);
    assert_eq!(
        status.working_tree.files,
        vec![WorkingTreeFile {
            path: "initial.ts".into(),
            insertions: 1,
            deletions: 0
        }]
    );
}

#[tokio::test]
async fn sorts_status_files_like_locale_compare() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    for name in ["b.txt", "B2.txt", "a.txt", "_x.txt", "Z.txt"] {
        write(&cwd.path, name, "x\n");
    }
    let (driver, _w) = driver();
    let status = driver.status_details(cwd.str()).await.unwrap();
    let names: Vec<&str> = status.working_tree.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(names, vec!["_x.txt", "a.txt", "b.txt", "B2.txt", "Z.txt"]);
}

#[tokio::test]
async fn skips_clean_filters_while_the_index_is_locked() {
    for location in ["root", "nested", "worktree"] {
        let repository = Tmp::new("git-vcs-driver-test-");
        init_repo_with_commit(&repository.path);
        let linked = Tmp::new("git-vcs-driver-linked-");
        let cwd = if location == "worktree" {
            std::fs::remove_dir(&linked.path).unwrap();
            git(&repository.path, &["worktree", "add", "--detach", linked.str()]);
            linked.path.clone()
        } else {
            repository.path.clone()
        };
        git(&cwd, &["config", "filter.probe.clean", "echo clean >> .filter-runs; cat"]);
        write(&cwd, ".gitattributes", "asset.bin filter=probe\n");
        write(&cwd, ".gitignore", ".filter-runs\n");
        write(&cwd, "asset.bin", "original\n");
        commit_all(&cwd, "filtered asset");
        let asset = std::fs::File::options().write(true).open(cwd.join("asset.bin")).unwrap();
        let epoch = std::time::UNIX_EPOCH + Duration::from_secs(1);
        asset.set_times(std::fs::FileTimes::new().set_accessed(epoch).set_modified(epoch)).unwrap();
        let runs = cwd.join(".filter-runs");
        let _ = std::fs::remove_file(&runs);
        let index = git(&cwd, &["rev-parse", "--git-path", "index"]);
        let lock = format!("{}.lock", cwd.join(index).to_string_lossy());
        std::fs::write(&lock, "").unwrap();
        let status_cwd = if location == "nested" { cwd.join("nested") } else { cwd.clone() };
        std::fs::create_dir_all(&status_cwd).unwrap();
        let (driver, _w) = driver();
        for _ in 0..3 {
            let error = driver.status_details_local(status_cwd.to_str().unwrap()).await.unwrap_err();
            assert!(error.detail.contains("index is locked"), "{location}");
        }
        assert!(!runs.exists(), "{location}");
        std::fs::remove_file(&lock).unwrap();
        let status = driver.status_details_local(status_cwd.to_str().unwrap()).await.unwrap();
        assert!(!status.has_working_tree_changes, "{location}");
        assert!(std::fs::read_to_string(&runs).unwrap().contains("clean"));
    }
}

#[tokio::test]
async fn uses_stable_diagnostics_for_every_parsed_non_repository_command() {
    let recorder = Recorder::responding(|_| not_a_repository());
    let (driver, _w) = driver_with(recorder.clone());
    let cwd = "/repo";
    driver.status_details_local(cwd).await.unwrap();
    driver.status_details_remote(cwd, false).await.unwrap();
    driver.list_refs(&list_input(cwd)).await.unwrap();
    let calls: Vec<(Vec<String>, Option<String>)> = recorder
        .calls
        .lock()
        .unwrap()
        .iter()
        .map(|c| (c.args.clone(), c.env.as_ref().and_then(|e| e.get("LC_ALL").cloned().flatten())))
        .collect();
    let c = |args: &[&str]| (args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>(), Some("C".to_owned()));
    assert_eq!(
        calls,
        vec![
            c(&["rev-parse", "--git-path", "index"]),
            c(&["status", "--porcelain=2", "--branch"]),
            c(&["rev-parse", "--abbrev-ref", "HEAD"]),
            c(&["rev-parse", "--git-common-dir"]),
        ]
    );
}

#[tokio::test]
async fn invalidates_the_origin_cache_when_a_driver_mutation_adds_origin() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = Tmp::new("git-vcs-driver-remote-");
    init_repo_with_commit(&cwd.path);
    git(&remote.path, &["init", "--bare"]);
    let (driver, _w) = driver();
    assert!(!driver.status_details_local(cwd.str()).await.unwrap().has_origin_remote);
    driver.ensure_remote(cwd.str(), "origin", remote.str()).await.unwrap();
    assert!(driver.status_details_local(cwd.str()).await.unwrap().has_origin_remote);
}

#[tokio::test]
async fn keeps_the_origin_cache_when_git_changes_behind_its_back() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = Tmp::new("git-vcs-driver-remote-");
    init_repo_with_commit(&cwd.path);
    git(&remote.path, &["init", "--bare"]);
    let (driver, _w) = driver();
    assert!(!driver.status_details_local(cwd.str()).await.unwrap().has_origin_remote);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    // 5-minute TTL: still the cached `false`.
    assert!(!driver.status_details_local(cwd.str()).await.unwrap().has_origin_remote);
}

// ---------------------------------------------------------------------------------------------
// git queue
// ---------------------------------------------------------------------------------------------

/// Holds the intercepted commands it gates until released; counts the concurrent ones.
struct Gate {
    active: AtomicUsize,
    peak: AtomicUsize,
    started: AtomicUsize,
    release: tokio::sync::Notify,
    open: std::sync::atomic::AtomicBool,
    only: Option<&'static str>,
}

impl Gate {
    fn new(only: Option<&'static str>) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            started: AtomicUsize::new(0),
            release: tokio::sync::Notify::new(),
            open: std::sync::atomic::AtomicBool::new(false),
            only,
        })
    }

    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }

    fn gates(&self, input: &ExecuteGitInput) -> bool {
        match self.only {
            None => true,
            Some(only) => input.args.first().map(String::as_str) == Some(only),
        }
    }
}

/// Routes each command to the gate that claims it.
struct Gates(Vec<Arc<Gate>>);

#[async_trait]
impl GitInterceptor for Gates {
    async fn intercept(&self, input: &ExecuteGitInput) -> Option<Result<ExecuteGitResult, GitCommandError>> {
        let gate = self.0.iter().find(|g| g.gates(input))?;
        let active = gate.active.fetch_add(1, Ordering::SeqCst) + 1;
        gate.peak.fetch_max(active, Ordering::SeqCst);
        gate.started.fetch_add(1, Ordering::SeqCst);
        while !gate.open.load(Ordering::SeqCst) {
            let notified = gate.release.notified();
            if gate.open.load(Ordering::SeqCst) {
                break;
            }
            notified.await;
        }
        gate.active.fetch_sub(1, Ordering::SeqCst);
        succeed_with("ok")
    }
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !condition() {
        assert!(std::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn spawn_execute(
    driver: &GitVcsDriver,
    operation: &'static str,
    args: &'static [&'static str],
    timeout: GitTimeout,
) -> tokio::task::JoinHandle<Result<ExecuteGitResult, GitCommandError>> {
    let driver = driver.clone();
    tokio::spawn(async move {
        driver
            .execute(ExecuteGitInput {
                timeout,
                ..ExecuteGitInput::new(operation, "/repo", args.iter().copied())
            })
            .await
    })
}

/// Both queue scenarios in one test: the queue is process-wide, so they must not overlap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn git_queue_bounds_bursts_and_skips_long_commands() {
    // Bursts across drivers: at most 8 run, and queued commands never time out while queued
    // (the last 8 have a 1 s timeout and wait well over a second).
    let gate = Gate::new(None);
    let interceptor = Arc::new(Gates(vec![gate.clone()]));
    let drivers: Vec<GitVcsDriver> = (0..16)
        .map(|_| GitVcsDriver::with_executor("/tmp", zc_vcs::git_exec::GitExecutor::new().with_interceptor(interceptor.clone())))
        .collect();
    let mut tasks = Vec::new();
    for (index, driver) in drivers.iter().enumerate().take(8) {
        let timeout = if index < 4 { GitTimeout::Default } else { GitTimeout::Millis(30_000) };
        tasks.push(spawn_execute(driver, "test.gitBurst", &["rev-parse", "HEAD"], timeout));
    }
    wait_until("8 running commands", || gate.started.load(Ordering::SeqCst) == 8).await;
    for driver in drivers.iter().skip(8) {
        tasks.push(spawn_execute(driver, "test.gitBurst", &["rev-parse", "HEAD"], GitTimeout::Millis(1_000)));
    }
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(gate.started.load(Ordering::SeqCst), 8);
    assert_eq!(gate.peak.load(Ordering::SeqCst), 8);
    gate.open();
    for task in tasks {
        let result = task.await.unwrap().unwrap();
        assert_eq!((result.stdout.as_str(), result.exit_code), ("ok", 0));
    }
    assert_eq!(gate.peak.load(Ordering::SeqCst), 8);
    assert_eq!(gate.started.load(Ordering::SeqCst), 16);
    assert_eq!(gate.active.load(Ordering::SeqCst), 0);

    // A pending command without a timeout (or above 30 s) holds no slot: 8 more still run.
    for timeout in [GitTimeout::Unbounded, GitTimeout::Millis(30_001)] {
        let slow = Gate::new(Some("push"));
        let fast = Gate::new(Some("status"));
        let driver = GitVcsDriver::with_executor(
            "/tmp",
            zc_vcs::git_exec::GitExecutor::new().with_interceptor(Arc::new(Gates(vec![slow.clone(), fast.clone()]))),
        );
        let pending = spawn_execute(&driver, "test.slowGit", &["push"], timeout);
        wait_until("the slow command", || slow.started.load(Ordering::SeqCst) == 1).await;
        let burst: Vec<_> = (0..8)
            .map(|_| spawn_execute(&driver, "test.fastGit", &["status"], GitTimeout::Default))
            .collect();
        wait_until("9 concurrent commands", || {
            fast.active.load(Ordering::SeqCst) == 8 && slow.active.load(Ordering::SeqCst) == 1
        })
        .await;
        fast.open();
        for task in burst {
            assert_eq!(task.await.unwrap().unwrap().stdout, "ok");
        }
        assert_eq!(slow.active.load(Ordering::SeqCst), 1);
        slow.open();
        assert_eq!(pending.await.unwrap().unwrap().stdout, "ok");
    }
    assert!(available_git_permits() <= 8);
}

// ---------------------------------------------------------------------------------------------
// refs
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn optionally_includes_remote_refs_that_match_local_branches() {
    let (cwd, _remote, branch) = repo_with_origin();
    let (driver, _w) = driver();
    let deduplicated = driver.list_refs(&list_input(cwd.str())).await.unwrap();
    assert!(!deduplicated.refs.iter().any(|r| r.name == format!("origin/{branch}")));
    let complete = driver
        .list_refs(&VcsListRefsInput {
            include_matching_remote_refs: Some(true),
            ..list_input(cwd.str())
        })
        .await
        .unwrap();
    assert!(complete.refs.iter().any(|r| r.name == branch));
    assert!(complete.refs.iter().any(|r| r.name == format!("origin/{branch}")));
    let remote_only = driver
        .list_refs(&VcsListRefsInput {
            include_matching_remote_refs: Some(true),
            ref_kind: Some(VcsRefKind::Remote),
            limit: Some(1),
            ..list_input(cwd.str())
        })
        .await
        .unwrap();
    assert_eq!(remote_only.refs.len(), 1);
    assert_eq!(remote_only.refs[0].name, format!("origin/{branch}"));
    assert_eq!(remote_only.refs[0].is_remote, Some(true));
    assert_eq!(remote_only.refs[0].remote_name.as_deref(), Some("origin"));
}

#[tokio::test]
async fn marks_the_origin_default_ref_as_default_without_a_local_copy() {
    let (cwd, _remote, branch) = repo_with_origin();
    git(&cwd.path, &["remote", "set-head", "origin", &branch]);
    git(&cwd.path, &["checkout", "-b", "feature/only-local"]);
    git(&cwd.path, &["branch", "-D", &branch]);
    let (driver, _w) = driver();
    let refs = driver.list_refs(&list_input(cwd.str())).await.unwrap();
    let default = refs.refs.iter().find(|r| r.name == format!("origin/{branch}")).unwrap();
    assert_eq!(default.is_remote, Some(true));
    assert!(default.is_default);
    // Current first, then the default, then the rest.
    assert_eq!(refs.refs[0].name, "feature/only-local");
    assert_eq!(refs.refs[1].name, format!("origin/{branch}"));
}

#[tokio::test]
async fn creates_checks_out_renames_and_lists_refs() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let (driver, _w) = driver();
    driver
        .create_ref(&VcsCreateRefInput {
            cwd: cwd.str().into(),
            ref_name: "feature/original".into(),
            switch_ref: None,
        })
        .await
        .unwrap();
    let switched = driver
        .switch_ref(&VcsSwitchRefInput {
            cwd: cwd.str().into(),
            ref_name: "feature/original".into(),
        })
        .await
        .unwrap();
    assert_eq!(switched.ref_name.as_deref(), Some("feature/original"));
    let renamed = driver.rename_branch(cwd.str(), "feature/original", "feature/renamed").await.unwrap();
    assert_eq!(renamed, "feature/renamed");
    assert_eq!(git(&cwd.path, &["branch", "--show-current"]), "feature/renamed");
    let refs = driver.list_refs(&list_input(cwd.str())).await.unwrap();
    assert!(refs.refs.iter().find(|r| r.name == "feature/renamed").unwrap().current);
}

#[tokio::test]
async fn renames_to_a_free_suffixed_name_and_keeps_identical_names() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let current = init_repo_with_commit(&cwd.path);
    let (driver, _w) = driver();
    assert_eq!(driver.rename_branch(cwd.str(), &current, &current).await.unwrap(), current);
    git(&cwd.path, &["branch", "taken"]);
    git(&cwd.path, &["branch", "taken-1"]);
    assert_eq!(driver.rename_branch(cwd.str(), &current, "taken").await.unwrap(), "taken-2");
}

#[tokio::test]
async fn rejects_a_missing_branch_without_restoring_a_matching_dirty_file() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    write(&cwd.path, "obsolete-branch", "original\n");
    git(&cwd.path, &["add", "obsolete-branch"]);
    git(&cwd.path, &["commit", "-m", "tracked file"]);
    git(&cwd.path, &["branch", "obsolete-branch"]);
    git(&cwd.path, &["branch", "-D", "obsolete-branch"]);
    write(&cwd.path, "obsolete-branch", "uncommitted work\n");
    let (driver, _w) = driver();
    let result = driver
        .switch_ref(&VcsSwitchRefInput {
            cwd: cwd.str().into(),
            ref_name: "obsolete-branch".into(),
        })
        .await;
    let error = result.unwrap_err();
    assert_eq!(error.detail, "git checkout failed");
    assert_eq!(read(&cwd.path, "obsolete-branch"), "uncommitted work\n");
    assert_eq!(git(&cwd.path, &["branch", "--show-current"]), branch);
}

#[tokio::test]
async fn creates_and_reuses_remote_tracking_branches_and_allows_detached_refs() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = Tmp::new("git-remote-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&remote.path, &["init", "--bare"]);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    git(&cwd.path, &["push", "origin", "HEAD:refs/heads/remote-only"]);
    git(&cwd.path, &["fetch", "origin"]);
    let (driver, _w) = driver();
    let switch = |name: &str| VcsSwitchRefInput {
        cwd: cwd.str().into(),
        ref_name: name.into(),
    };
    for _ in 0..2 {
        let result = driver.switch_ref(&switch("origin/remote-only")).await.unwrap();
        assert_eq!(result.ref_name.as_deref(), Some("remote-only"));
        assert_eq!(git(&cwd.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/remote-only");
        driver.switch_ref(&switch(&branch)).await.unwrap();
    }
    let commit = git(&cwd.path, &["rev-parse", "HEAD"]);
    let detached = driver.switch_ref(&switch(&commit)).await.unwrap();
    assert_eq!(detached.ref_name, None);
    assert_eq!(git(&cwd.path, &["rev-parse", "HEAD"]), commit);
}

#[tokio::test]
async fn coalesces_concurrent_ref_pages_into_one_snapshot() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let recorder = Recorder::new();
    let (driver, _w) = driver_with(recorder.clone());
    let pages = (0..30).map(|index| {
        let driver = driver.clone();
        let cwd = cwd.str().to_owned();
        async move {
            driver
                .list_refs(&VcsListRefsInput {
                    refresh: Some(true),
                    query: Some(format!("missing-{index}")),
                    limit: Some(100),
                    ..list_input(&cwd)
                })
                .await
                .unwrap()
        }
    });
    futures::future::join_all(pages).await;
    driver
        .list_refs(&VcsListRefsInput {
            cursor: Some(1),
            limit: Some(100),
            ..list_input(cwd.str())
        })
        .await
        .unwrap();
    let ref_scans = |r: &Recorder| r.count(|a| has(a, "for-each-ref") && has(a, "refs/remotes"));
    assert_eq!(ref_scans(&recorder), 1);
    assert_eq!(recorder.count(|a| has(a, "worktree") && has(a, "--porcelain")), 1);

    driver
        .create_ref(&VcsCreateRefInput {
            cwd: cwd.str().into(),
            ref_name: "feature/cache-invalidation".into(),
            switch_ref: None,
        })
        .await
        .unwrap();
    let refreshed = driver
        .list_refs(&VcsListRefsInput {
            limit: Some(100),
            ..list_input(cwd.str())
        })
        .await
        .unwrap();
    assert!(refreshed.refs.iter().any(|r| r.name == "feature/cache-invalidation"));
    assert_eq!(ref_scans(&recorder), 2);
}

/// Delays the first worktree scan until released, recording ref scans.
struct HoldFirstWorktreeScan {
    held: std::sync::atomic::AtomicBool,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    released: std::sync::atomic::AtomicBool,
    ref_scans: AtomicUsize,
}

#[async_trait]
impl GitInterceptor for HoldFirstWorktreeScan {
    async fn intercept(&self, input: &ExecuteGitInput) -> Option<Result<ExecuteGitResult, GitCommandError>> {
        if has(&input.args, "for-each-ref") && has(&input.args, "refs/remotes") {
            self.ref_scans.fetch_add(1, Ordering::SeqCst);
        }
        if has(&input.args, "worktree") && has(&input.args, "--porcelain") && !self.held.swap(true, Ordering::SeqCst) {
            self.started.notify_one();
            while !self.released.load(Ordering::SeqCst) {
                let notified = self.release.notified();
                if self.released.load(Ordering::SeqCst) {
                    break;
                }
                notified.await;
            }
        }
        None
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn retries_an_in_flight_ref_snapshot_invalidated_by_a_mutation() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let hold = Arc::new(HoldFirstWorktreeScan {
        held: Default::default(),
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        released: Default::default(),
        ref_scans: AtomicUsize::new(0),
    });
    let (driver, _w) = driver_with(hold.clone());
    let started = hold.started.notified();
    let in_flight = {
        let driver = driver.clone();
        let cwd = cwd.str().to_owned();
        tokio::spawn(async move {
            driver
                .list_refs(&VcsListRefsInput {
                    refresh: Some(true),
                    limit: Some(100),
                    ..list_input(&cwd)
                })
                .await
        })
    };
    started.await;
    // Let the ref scan of the first snapshot finish before the mutation.
    while hold.ref_scans.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    driver
        .create_ref(&VcsCreateRefInput {
            cwd: cwd.str().into(),
            ref_name: "feature/during-refresh".into(),
            switch_ref: None,
        })
        .await
        .unwrap();
    hold.released.store(true, Ordering::SeqCst);
    hold.release.notify_waiters();
    let refs = in_flight.await.unwrap().unwrap();
    assert!(refs.refs.iter().any(|r| r.name == "feature/during-refresh"));
    assert_eq!(hold.ref_scans.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn invalidates_a_ref_snapshot_when_a_mutation_fails_after_changing_git() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let repo = cwd.path.clone();
    let recorder = Recorder::responding(move |input| {
        if input.args == ["branch", "feature/partial-failure"] {
            git(&repo, &["branch", "feature/partial-failure"]);
            return not_a_repository();
        }
        None
    });
    let (driver, _w) = driver_with(recorder);
    driver
        .list_refs(&VcsListRefsInput {
            refresh: Some(true),
            ..list_input(cwd.str())
        })
        .await
        .unwrap();
    driver
        .create_ref(&VcsCreateRefInput {
            cwd: cwd.str().into(),
            ref_name: "feature/partial-failure".into(),
            switch_ref: None,
        })
        .await
        .unwrap_err();
    let refs = driver.list_refs(&list_input(cwd.str())).await.unwrap();
    assert!(refs.refs.iter().any(|r| r.name == "feature/partial-failure"));
}

#[tokio::test]
async fn fails_a_ref_snapshot_when_for_each_ref_exits_unsuccessfully() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let recorder = Recorder::responding(move |input| {
        if has(&input.args, "for-each-ref") {
            counter.fetch_add(1, Ordering::SeqCst);
            return not_a_repository();
        }
        None
    });
    let (driver, _w) = driver_with(recorder);
    let error = driver
        .list_refs(&VcsListRefsInput {
            refresh: Some(true),
            ..list_input(cwd.str())
        })
        .await
        .unwrap_err();
    assert_eq!(error.operation, "GitVcsDriver.listRefs.snapshotRefs");
    assert_eq!(error.detail, "Git ref snapshot enumeration failed.");
    assert_eq!(error.exit_code, Some(128));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn marks_the_current_branch_when_worktree_metadata_is_unavailable() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    let recorder = Recorder::responding(|input| {
        let root = has(&input.args, "rev-parse") && has(&input.args, "--show-toplevel");
        let list = has(&input.args, "worktree") && has(&input.args, "--porcelain");
        if root || list {
            return not_a_repository();
        }
        None
    });
    let (driver, _w) = driver_with(recorder);
    let refs = driver
        .list_refs(&VcsListRefsInput {
            refresh: Some(true),
            ..list_input(cwd.str())
        })
        .await
        .unwrap();
    assert!(refs.is_repo);
    assert!(refs.refs.iter().find(|r| r.name == branch).unwrap().current);
}

#[tokio::test]
async fn ignores_worktree_metadata_for_directories_that_no_longer_exist() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["branch", "stale-worktree"]);
    let recorder = Recorder::responding(|input| {
        if has(&input.args, "worktree") && has(&input.args, "--porcelain") {
            return succeed_with("worktree /missing/deleted-worktree\0HEAD deadbeef\0branch refs/heads/stale-worktree\0\0");
        }
        None
    });
    let (driver, _w) = driver_with(recorder);
    let refs = driver
        .list_refs(&VcsListRefsInput {
            refresh: Some(true),
            ..list_input(cwd.str())
        })
        .await
        .unwrap();
    assert_eq!(refs.refs.iter().find(|r| r.name == "stale-worktree").unwrap().worktree_path, None);
}

#[tokio::test]
async fn refreshes_the_current_branch_after_an_external_checkout() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["branch", "external-checkout"]);
    let (driver, _w) = driver();
    let refresh = || VcsListRefsInput {
        refresh: Some(true),
        ..list_input(cwd.str())
    };
    let initial = driver.list_refs(&refresh()).await.unwrap();
    assert!(initial.refs.iter().find(|r| r.name == branch).unwrap().current);
    git(&cwd.path, &["checkout", "external-checkout"]);
    // The refresh coalesces for 5 s.
    tokio::time::sleep(Duration::from_millis(5_200)).await;
    let refreshed = driver.list_refs(&refresh()).await.unwrap();
    assert!(refreshed.refs.iter().find(|r| r.name == "external-checkout").unwrap().current);
    assert!(!refreshed.refs.iter().find(|r| r.name == branch).unwrap().current);
}

#[tokio::test]
async fn lists_worktree_paths_and_marks_the_linked_worktree_current() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    let worktrees = Tmp::new("git-vcs-driver-worktrees-");
    let linked = worktrees.join("linked\nworktree");
    git(&cwd.path, &["worktree", "add", "-b", "feature/newline-path", &linked]);
    let (driver, _w) = driver();
    let refs = driver
        .list_refs(&VcsListRefsInput {
            refresh: Some(true),
            ..list_input(cwd.str())
        })
        .await
        .unwrap();
    let listed = refs
        .refs
        .iter()
        .find(|r| r.name == "feature/newline-path")
        .unwrap()
        .worktree_path
        .clone()
        .unwrap();
    assert_eq!(std::fs::canonicalize(&listed).unwrap(), std::fs::canonicalize(&linked).unwrap());
    let from_linked = driver.list_refs(&list_input(&linked)).await.unwrap();
    assert!(from_linked.refs.iter().find(|r| r.name == "feature/newline-path").unwrap().current);
    assert!(!from_linked.refs.iter().find(|r| r.name == branch).unwrap().current);
}

#[tokio::test]
async fn backs_off_failed_background_fetches_across_linked_worktrees() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let remote = Tmp::new("git-vcs-driver-remote-");
    let worktrees = Tmp::new("git-vcs-driver-worktrees-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&remote.path, &["init", "--bare"]);
    git(&cwd.path, &["remote", "add", "origin", remote.str()]);
    git(&cwd.path, &["push", "-u", "origin", &branch]);
    let linked = worktrees.join("linked");
    git(&cwd.path, &["worktree", "add", "-b", "feature/linked", &linked]);
    git(&linked, &["branch", "--set-upstream-to", &format!("origin/{branch}"), "feature/linked"]);
    let fetches = Arc::new(AtomicUsize::new(0));
    let counter = fetches.clone();
    let recorder = Recorder::responding(move |input| {
        if has(&input.args, "fetch") && has(&input.args, "--quiet") {
            counter.fetch_add(1, Ordering::SeqCst);
            return not_a_repository();
        }
        None
    });
    let (driver, _w) = driver_with(recorder);
    driver.status_details_remote(cwd.str(), true).await.unwrap();
    driver.status_details_remote(&linked, true).await.unwrap();
    driver.status_details_remote(&linked, true).await.unwrap();
    // One fetch for the shared common dir; the failure is cached with a 30 s cooldown.
    assert_eq!(fetches.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------------------------
// worktrees
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn uses_parallel_checkout_without_skipping_filters_or_hooks() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["config", "filter.test.smudge", "sed s/original/filtered/g"]);
    write(&cwd.path, ".gitattributes", "asset.txt filter=test\n");
    write(&cwd.path, "asset.txt", "original\n");
    commit_all(&cwd.path, "filtered asset");
    write(
        &cwd.path,
        ".git/hooks/post-checkout",
        "#!/bin/sh\ngit config checkout.workers > checkout-workers\nexit 0\n",
    );
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(cwd.path.join(".git/hooks/post-checkout"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let parent = Tmp::new("git-worktrees-");
    let worktree = parent.join("parallel");
    let (driver, _w) = driver();
    driver
        .create_worktree(
            &VcsCreateWorktreeInput {
                base_ref_name: Some(branch.clone()),
                ..worktree_input(cwd.str(), &worktree, &branch, "feature/parallel")
            },
            &CreateWorktreeOptions::default(),
        )
        .await
        .unwrap();
    assert!(!git(&cwd.path, &["worktree", "list", "--porcelain"]).contains("locked"));
    assert_eq!(read(&worktree, "checkout-workers"), "0\n");
    assert_eq!(read(&worktree, "asset.txt"), "filtered\n");
    assert_eq!(git(&worktree, &["rev-parse", "HEAD"]), git(&cwd.path, &["rev-parse", "HEAD"]));
    assert_eq!(git(&cwd.path, &["config", "branch.feature/parallel.gh-merge-base"]), branch);
    for (configured, expected) in [("1", "1"), ("", "0")] {
        git(&cwd.path, &["config", "checkout.workers", configured]);
        let parent = Tmp::new("git-worktrees-");
        let path = parent.join("configured");
        driver
            .create_worktree(
                &worktree_input(cwd.str(), &path, &branch, &format!("feature/configured-{expected}")),
                &CreateWorktreeOptions::default(),
            )
            .await
            .unwrap();
        assert_eq!(read(&path, "checkout-workers"), format!("{expected}\n"));
    }
}

#[tokio::test]
async fn defaults_worktree_paths_under_the_worktrees_dir() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    let (driver, worktrees) = driver();
    let created = driver
        .create_worktree(
            &VcsCreateWorktreeInput {
                path: None,
                ..worktree_input(cwd.str(), "", &branch, "feature/a/b")
            },
            &CreateWorktreeOptions::default(),
        )
        .await
        .unwrap();
    let repo_name = cwd.path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(created.worktree.path, worktrees.path.join(repo_name).join("feature-a-b").to_string_lossy());
    assert_eq!(created.worktree.ref_name, "feature/a/b");
    assert_eq!(git(&created.worktree.path, &["branch", "--show-current"]), "feature/a/b");
}

#[tokio::test]
async fn checks_out_submodules_in_a_new_worktree() {
    let submodule = Tmp::new("git-submodule-");
    init_repo_with_commit(&submodule.path);
    write(&submodule.path, "SHARED.md", "# shared\n");
    commit_all(&submodule.path, "shared");
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["submodule", "add", submodule.str(), "shared"]);
    git(&cwd.path, &["commit", "-m", "add submodule"]);
    let parent = Tmp::new("git-worktrees-");
    let worktree = parent.join("submodule-worktree");
    let (driver, _w) = driver();
    driver
        .create_worktree(
            &worktree_input(cwd.str(), &worktree, &branch, "feature/submodules"),
            &CreateWorktreeOptions::default(),
        )
        .await
        .unwrap();
    assert!(std::path::Path::new(&worktree).join("shared/SHARED.md").exists());
}

#[tokio::test]
async fn still_creates_the_worktree_when_submodule_checkout_fails() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    write(
        &cwd.path,
        ".gitmodules",
        "[submodule \"missing\"]\n\tpath = missing\n\turl = /nonexistent/repo.git\n",
    );
    commit_all(&cwd.path, "add unreachable submodule");
    let parent = Tmp::new("git-worktrees-");
    let worktree = parent.join("broken-submodule-worktree");
    let finished: Arc<Mutex<Vec<(bool, Option<String>)>>> = Default::default();
    let sink = finished.clone();
    let (driver, _w) = driver();
    let created = driver
        .create_worktree(
            &worktree_input(cwd.str(), &worktree, &branch, "feature/broken-submodules"),
            &CreateWorktreeOptions {
                progress: CreateWorktreeProgress {
                    on_submodules_finished: Some(Arc::new(move |ok, detail| sink.lock().unwrap().push((ok, detail.map(str::to_owned))))),
                    ..Default::default()
                },
                submodules: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(created.worktree.path, worktree);
    assert!(std::path::Path::new(&worktree).exists());
    // `.gitmodules` without a gitlink: git has nothing to update, which is a success.
    assert_eq!(finished.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn resolves_the_submodule_mode_from_the_option_then_t3_json() {
    let nested = Tmp::new("git-nested-");
    init_repo_with_commit(&nested.path);
    write(&nested.path, "NESTED.md", "# nested\n");
    commit_all(&nested.path, "nested");
    let inner = Tmp::new("git-inner-");
    init_repo_with_commit(&inner.path);
    write(&inner.path, "INNER.md", "# inner\n");
    git(&inner.path, &["submodule", "add", nested.str(), "nested"]);
    commit_all(&inner.path, "inner");
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    git(&cwd.path, &["submodule", "add", inner.str(), "inner"]);
    git(&cwd.path, &["commit", "-m", "add submodule"]);
    let (driver, _w) = driver();
    let worktrees = Tmp::new("git-worktrees-");

    let create = |file_mode: &'static str, name: &'static str, submodules: Option<WorktreeSubmodules>| {
        let driver = driver.clone();
        let cwd = cwd.path.clone();
        let branch = branch.clone();
        let path = worktrees.join(name);
        async move {
            write(&cwd, "t3.json", &format!("{{ \"worktreeSubmodules\": \"{file_mode}\" }}"));
            git(&cwd, &["add", "t3.json"]);
            git(&cwd, &["commit", "--allow-empty", "-m", &format!("submodules: {file_mode}")]);
            let disabled: Arc<Mutex<Option<SubmodulesDisabledSource>>> = Default::default();
            let sink = disabled.clone();
            driver
                .create_worktree(
                    &worktree_input(cwd.to_str().unwrap(), &path, &branch, name),
                    &CreateWorktreeOptions {
                        progress: CreateWorktreeProgress {
                            on_submodules_disabled: Some(Arc::new(move |source| *sink.lock().unwrap() = Some(source))),
                            ..Default::default()
                        },
                        submodules,
                    },
                )
                .await
                .unwrap();
            let root = std::path::Path::new(&path);
            let disabled = *disabled.lock().unwrap();
            (disabled, root.join("inner/INNER.md").exists(), root.join("inner/nested/NESTED.md").exists())
        }
    };

    assert_eq!(create("recursive", "recursive", None).await, (None, true, true));
    assert_eq!(create("top-level", "top-level", None).await, (None, true, false));
    assert_eq!(
        create("recursive", "setting-none", Some(WorktreeSubmodules::None)).await,
        (Some(SubmodulesDisabledSource::Settings), false, false)
    );
    assert_eq!(create("none", "setting-wins", Some(WorktreeSubmodules::TopLevel)).await, (None, true, false));
    assert_eq!(create("none", "none", None).await, (Some(SubmodulesDisabledSource::T3Json), false, false));
}

#[tokio::test]
async fn reports_checkout_progress_during_worktree_creation() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    for index in 0..200 {
        write(&cwd.path, &format!("file-{index}.txt"), &format!("{index}\n"));
    }
    commit_all(&cwd.path, "add files");
    let parent = Tmp::new("git-worktrees-");
    let worktree = parent.join("progress-worktree");
    let seen: Arc<Mutex<Vec<(f64, u64, u64)>>> = Default::default();
    let claimed: Arc<Mutex<Option<(String, bool)>>> = Default::default();
    let (seen_sink, claimed_sink) = (seen.clone(), claimed.clone());
    let (driver, _w) = driver();
    driver
        .create_worktree(
            &worktree_input(cwd.str(), &worktree, &branch, "feature/progress"),
            &CreateWorktreeOptions {
                progress: CreateWorktreeProgress {
                    on_worktree_claimed: Some(Arc::new(move |path| {
                        *claimed_sink.lock().unwrap() = Some((path.to_owned(), std::path::Path::new(path).exists()))
                    })),
                    on_checkout_progress: Some(Arc::new(move |p| seen_sink.lock().unwrap().push((p.percent, p.completed, p.total)))),
                    ..Default::default()
                },
                submodules: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(*claimed.lock().unwrap(), Some((worktree.clone(), true)));
    let updates = seen.lock().unwrap().clone();
    assert!(updates.len() > 1, "{updates:?}");
    let last = updates.last().unwrap();
    assert_eq!((last.0, last.2), (100.0, 201));
    let completed: Vec<u64> = updates.iter().map(|u| u.1).collect();
    let mut sorted = completed.clone();
    sorted.sort();
    assert_eq!(completed, sorted);
}

#[tokio::test]
async fn creates_and_removes_a_worktree_twice_without_failing() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    let parent = Tmp::new("git-worktrees-");
    let worktree = parent.join("feature-worktree");
    let (driver, _w) = driver();
    let created = driver
        .create_worktree(
            &worktree_input(cwd.str(), &worktree, &branch, "feature/worktree"),
            &CreateWorktreeOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(created.worktree.path, worktree);
    assert_eq!(created.worktree.ref_name, "feature/worktree");
    assert_eq!(git(&worktree, &["branch", "--show-current"]), "feature/worktree");
    let remove = VcsRemoveWorktreeInput {
        cwd: cwd.str().into(),
        path: worktree.clone(),
        force: None,
    };
    driver.remove_worktree(&remove).await.unwrap();
    assert!(!std::path::Path::new(&worktree).exists());
    driver.remove_worktree(&remove).await.unwrap();
}

#[tokio::test]
async fn prunes_stale_registrations_when_removing_an_already_gone_worktree() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let branch = init_repo_with_commit(&cwd.path);
    let parent = Tmp::new("git-worktrees-");
    let stale = parent.join("stale");
    let (driver, _w) = driver();
    driver
        .create_worktree(&worktree_input(cwd.str(), &stale, &branch, "feature/stale"), &CreateWorktreeOptions::default())
        .await
        .unwrap();
    std::fs::remove_dir_all(&stale).unwrap();
    driver
        .remove_worktree(&VcsRemoveWorktreeInput {
            cwd: cwd.str().into(),
            path: parent.join("never-registered"),
            force: None,
        })
        .await
        .unwrap();
    assert!(!git(&cwd.path, &["worktree", "list", "--porcelain"]).contains("stale"));
}

#[tokio::test]
async fn does_not_wrap_a_remove_worktree_failure() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let not_a_worktree = cwd.join("not-a-worktree");
    std::fs::create_dir(&not_a_worktree).unwrap();
    let (driver, _w) = driver();
    driver.init_repo(cwd.str()).await.unwrap();
    let error = driver
        .remove_worktree(&VcsRemoveWorktreeInput {
            cwd: cwd.str().into(),
            path: not_a_worktree,
            force: None,
        })
        .await
        .unwrap_err();
    assert_eq!(error.operation, "GitVcsDriver.removeWorktree");
    assert_eq!(error.command, "git");
    assert_eq!(error.argument_count, Some(3));
    assert!(error.cause.is_none());
    assert!(!error.detail.contains("Git command failed in"));
}

// ---------------------------------------------------------------------------------------------
// execution and structured errors
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn preserves_the_caller_locale_for_general_git_subprocesses() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    let result = driver
        .execute(ExecuteGitInput {
            env: Some(zc_vcs::git_exec::env([("LC_ALL", "zh_CN.UTF-8")])),
            ..ExecuteGitInput::new(
                "GitVcsDriver.test.git",
                cwd.str(),
                ["-c", "alias.print-locale=!printf \"%s\" \"$LC_ALL\"", "print-locale"],
            )
        })
        .await
        .unwrap();
    assert_eq!(result.stdout, "zh_CN.UTF-8");
}

#[tokio::test]
async fn reports_a_missing_cwd_as_a_structured_spawn_failure() {
    let parent = Tmp::new("git-vcs-driver-test-");
    let cwd = parent.join("missing");
    let (driver, _w) = driver();
    let error = driver
        .execute(ExecuteGitInput::new("GitVcsDriver.test.missingCwd", &cwd, ["status", "--short"]))
        .await
        .unwrap_err();
    assert_eq!(error.operation, "GitVcsDriver.test.missingCwd");
    assert_eq!(error.command, "git");
    assert_eq!(error.argument_count, Some(2));
    assert_eq!(error.cwd, cwd);
    assert_eq!(error.detail, "Failed to spawn Git process.");
    assert!(error.missing_cwd);
    assert!(error.cause.is_some());
}

#[tokio::test]
async fn does_not_retain_git_arguments_or_stderr_in_command_failures() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    driver.init_repo(cwd.str()).await.unwrap();
    let secret = "secret-token-value";
    let error = driver
        .execute(ExecuteGitInput::new(
            "GitVcsDriver.test.redactedFailure",
            cwd.str(),
            ["status".to_owned(), format!("--unknown-option={secret}")],
        ))
        .await
        .unwrap_err();
    assert_eq!(error.argument_count, Some(2));
    assert!(error.exit_code.is_some());
    assert!(error.stderr_length.unwrap() > 0);
    let encoded = serde_json::to_string(&error).unwrap();
    assert!(!encoded.contains(secret));
    assert!(!error.to_string().contains(secret));
}

#[tokio::test]
async fn keeps_line_callbacks_flowing_past_the_output_cap() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    let lines: Arc<Mutex<Vec<String>>> = Default::default();
    let sink = lines.clone();
    let result = driver
        .execute(ExecuteGitInput {
            max_output_bytes: Some(512),
            append_truncation_marker: true,
            keep_line_callbacks_after_truncation: true,
            progress: Some(ExecuteGitProgress {
                on_stderr_line: Some(Arc::new(move |line| sink.lock().unwrap().push(line.to_owned()))),
                ..Default::default()
            }),
            ..ExecuteGitInput::new(
                "GitVcsDriver.test.callbacksPastCap",
                cwd.str(),
                [
                    "-c",
                    "alias.spew=!for i in $(seq 1 128); do printf \"é%03d\\n\" $i >&2; done; echo fatal: last line >&2",
                    "spew",
                ],
            )
        })
        .await
        .unwrap();
    assert!(result.stderr_truncated);
    assert!(result.stderr.encode_utf16().count() <= 600);
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len(), 129);
    assert_eq!(lines[0], "é001");
    assert_eq!(lines[127], "é128");
    assert_eq!(lines.last().unwrap(), "fatal: last line");
    assert!(!lines.iter().any(|l| l.contains('\u{FFFD}')));
}

#[tokio::test]
async fn fails_past_the_output_cap_without_a_truncation_marker() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    let error = driver
        .execute(ExecuteGitInput {
            max_output_bytes: Some(16),
            ..ExecuteGitInput::new("GitVcsDriver.test.cap", cwd.str(), ["-c", "alias.spew=!printf '%0100d' 0", "spew"])
        })
        .await
        .unwrap_err();
    assert_eq!(error.detail, "Git output exceeded 16 bytes and was truncated.");
    assert!(error.output_length.unwrap() > 16);
}

#[tokio::test]
async fn times_out_hung_git_commands() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    let (driver, _w) = driver();
    let error = driver
        .execute(ExecuteGitInput {
            timeout: GitTimeout::Millis(300),
            ..ExecuteGitInput::new("GitVcsDriver.test.timeout", cwd.str(), ["-c", "alias.hang=!sleep 5", "hang"])
        })
        .await
        .unwrap_err();
    assert_eq!(error.detail, "Git command timed out.");
}

#[tokio::test]
async fn recovers_a_missing_cwd_as_a_non_repository() {
    let parent = Tmp::new("git-vcs-driver-test-");
    let cwd = parent.join("missing");
    let (driver, _w) = driver();
    assert!(!driver.status_details(&cwd).await.unwrap().is_repo);
    assert!(!driver.status_details_remote(&cwd, false).await.unwrap().is_repo);
    let refs = driver.list_refs(&list_input(&cwd)).await.unwrap();
    assert!(!refs.is_repo);
    assert!(refs.refs.is_empty());
}

#[tokio::test]
async fn reports_hook_progress_through_trace2() {
    let cwd = Tmp::new("git-vcs-driver-test-");
    init_repo_with_commit(&cwd.path);
    write(&cwd.path, ".git/hooks/pre-commit", "#!/bin/sh\necho checking >&2\nexit 0\n");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(cwd.path.join(".git/hooks/pre-commit"), std::fs::Permissions::from_mode(0o755)).unwrap();
    write(&cwd.path, "hooked.txt", "x\n");
    git(&cwd.path, &["add", "hooked.txt"]);
    let started: Arc<Mutex<Vec<String>>> = Default::default();
    let finished: Arc<Mutex<Vec<zc_vcs::git_exec::HookFinished>>> = Default::default();
    let output: Arc<Mutex<Vec<(zc_vcs::driver_core::OutputStream, String)>>> = Default::default();
    let (s, f, o) = (started.clone(), finished.clone(), output.clone());
    let (driver, _w) = driver();
    let sha = driver
        .commit(
            cwd.str(),
            "hooked commit",
            "",
            zc_vcs::driver_core::GitCommitOptions {
                timeout_ms: None,
                progress: Some(zc_vcs::driver_core::GitCommitProgress {
                    on_output_line: Some(Arc::new(move |stream, line| o.lock().unwrap().push((stream, line.to_owned())))),
                    on_hook_started: Some(Arc::new(move |name| s.lock().unwrap().push(name.to_owned()))),
                    on_hook_finished: Some(Arc::new(move |done| f.lock().unwrap().push(done))),
                }),
            },
        )
        .await
        .unwrap();
    assert_eq!(sha.len(), 40);
    assert_eq!(*started.lock().unwrap(), vec!["pre-commit"]);
    // Parity with TS: git's `child_exit` events carry no `child_class`, which the TS monitor
    // filters on, so `onHookFinished` never fires from trace2 (a TS bug kept on purpose).
    assert!(finished.lock().unwrap().is_empty());
    assert!(output
        .lock()
        .unwrap()
        .iter()
        .any(|(stream, line)| *stream == zc_vcs::driver_core::OutputStream::Stderr && line == "checking"));
}
