//! `GitManager.test.ts`, the status / PR lookup / branch PR lookup / head matching tests
//! (from "status includes draft PR metadata…" through "status keeps the last known PR when the
//! current remote URL can't be resolved").

#![allow(clippy::result_large_err)]

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use serde_json::{json, Value};
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner};
use zc_core::vcs_process::VcsProcess;
use zc_git::helpers::{pr_lookup_failure_ttl, pull_request_repository_key};
use zc_git::{FixedProvider, GitManager, GitManagerDeps, SettingsSources};
use zc_ports::git::GitBranchPullRequest;
use zc_sourcecontrol::gitlab::{GitLabCli, GitLabSourceControlProvider};
use zc_sourcecontrol::SourceControlProvider;
use zc_vcs::git_exec::{ExecuteGitInput, ExecuteGitResult, GitExecutor, GitInterceptor};
use zc_vcs::status::RemoteStatusOptions;
use zc_vcs::{GitCommandError, GitManagerServiceError, GitVcsDriver};

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `initRepo` + `checkout -b <branch>` + an `origin` bare remote + `push -u origin <branch>`.
fn repo_with_pushed_branch(branch: &str) -> (Tmp, Tmp) {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", branch]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", branch]);
    (repo, remote)
}

fn list(entries: Value) -> String {
    entries.to_string()
}

fn sequence(entries: &[Value]) -> GhScenario {
    GhScenario {
        pr_list_sequence: entries.iter().map(Value::to_string).collect(),
        ..GhScenario::default()
    }
}

fn by_head(entries: &[(&str, Value)]) -> GhScenario {
    GhScenario {
        pr_list_by_head_selector: entries.iter().map(|(head, value)| ((*head).to_owned(), value.to_string())).collect(),
        ..GhScenario::default()
    }
}

/// `GitHubCliUnavailableError` (gh missing from PATH).
fn unavailable_after(calls: usize) -> GhScenario {
    GhScenario {
        fail_with: Some(GhFailure::Missing),
        fail_after_calls: calls,
        ..GhScenario::default()
    }
}

fn pr_list_calls(h: &Harness) -> Vec<String> {
    h.gh_calls().into_iter().filter(|call| call.starts_with("pr list ")).collect()
}

async fn status(h: &Harness, cwd: &str) -> Value {
    serde_json::to_value(h.manager.status(cwd).await.unwrap()).unwrap()
}

async fn remote_status(h: &Harness, cwd: &str, refresh_upstream: bool, refresh_missing_pull_request: bool) -> Value {
    let result = h
        .manager
        .remote_status(
            cwd,
            RemoteStatusOptions {
                refresh_upstream,
                refresh_missing_pull_request,
            },
        )
        .await
        .unwrap();
    serde_json::to_value(result).unwrap()
}

/// The TS `GitBranchPullRequest` JSON: the status PR plus `closedAt`, `mergedAt`,
/// `repositoryKey`.
fn branch_pr_json(found: &GitBranchPullRequest) -> Value {
    let mut value = found.pull_request.0.clone();
    let object = value.as_object_mut().expect("a PR object");
    object.insert("closedAt".into(), json!(found.closed_at.clone().flatten()));
    object.insert("mergedAt".into(), json!(found.merged_at.clone().flatten()));
    object.insert("repositoryKey".into(), json!(found.repository_key));
    value
}

async fn branch_pr(h: &Harness, cwd: &str, branch: &str, refresh: bool) -> Result<Option<Value>, GitManagerServiceError> {
    Ok(h.manager.branch_pull_request(cwd, branch, refresh).await?.as_ref().map(branch_pr_json))
}

/// `expect(actual).toMatchObject(expected)` for flat objects.
#[track_caller]
fn assert_match_object(actual: &Value, expected: Value) {
    for (key, value) in expected.as_object().expect("expected an object") {
        assert_eq!(actual.get(key), Some(value), "key {key} of {actual}");
    }
}

fn error_tag(error: &GitManagerServiceError) -> String {
    match error {
        GitManagerServiceError::Manager(_) => "GitManagerError".into(),
        GitManagerServiceError::Command(_) => "GitCommandError".into(),
        GitManagerServiceError::Other(error) => error.tag.clone(),
    }
}

/// `makeManager({gitConfigReads})`: a manager whose git driver records `config --get <key>`.
struct ConfigReads(Mutex<Vec<String>>);

#[async_trait]
impl GitInterceptor for ConfigReads {
    async fn intercept(&self, input: &ExecuteGitInput) -> Option<Result<ExecuteGitResult, GitCommandError>> {
        if input.args.len() >= 3 && input.args[0] == "config" && input.args[1] == "--get" {
            self.0.lock().unwrap().push(input.args[2].clone());
        }
        None
    }
}

fn manager_recording_config_reads(reads: Arc<ConfigReads>) -> (GitManager, Tmp) {
    let gh = FakeGh::new(GhScenario::default());
    let temp = Tmp::new("t3-git-manager-test-");
    let manager = GitManager::new(GitManagerDeps {
        git: GitVcsDriver::with_executor(temp.path.join("worktrees"), GitExecutor::new().with_interceptor(reads)),
        providers: Arc::new(FixedProvider(github_provider(&gh))),
        text_generation: Arc::new(FakeTextGeneration::default()),
        settings: SettingsSources {
            settings: MemorySettings::new(json!({})),
            provider_status: Arc::new(FixedProviders(Vec::new())),
            projections: None,
        },
        setup_scripts: None,
        temp_dir: temp.path.clone(),
        uuids: Arc::new(zc_core::uuid_v4),
    });
    (manager, temp)
}

/// `Layer.mock(VcsProcess)({run})`: every process answers `output` and is recorded.
struct RecordingRunner {
    output: String,
    calls: Mutex<Vec<(String, Vec<String>)>>,
}

#[async_trait]
impl ProcessRunner for RecordingRunner {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        self.calls.lock().unwrap().push((input.command.clone(), input.args.clone()));
        Ok(ProcessRunOutput {
            stdout: self.output.clone(),
            code: Some(0),
            ..ProcessRunOutput::default()
        })
    }
}

type CapturedEvent = (std::thread::ThreadId, String, BTreeMap<String, String>);

/// The TS `Logger.make` capture: every tracing event's thread, message and fields.
///
/// It is the global subscriber, not a scoped one: with a single scoped dispatcher, tracing
/// computes a callsite's interest from the default of whichever test thread hits it first, so a
/// concurrent test without a subscriber could disable the warning for good. Each test reads the
/// events of its own thread (`#[tokio::test]` runs on the test's thread).
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<CapturedEvent>>>);

impl LogCapture {
    fn global() -> &'static LogCapture {
        static CAPTURE: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
        CAPTURE.get_or_init(|| {
            let capture = LogCapture::default();
            tracing::subscriber::set_global_default(capture.clone()).expect("no other global subscriber in this test binary");
            capture
        })
    }

    fn events_of_this_thread(&self) -> Vec<(String, BTreeMap<String, String>)> {
        let current = std::thread::current().id();
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(thread, _, _)| *thread == current)
            .map(|(_, message, fields)| (message.clone(), fields.clone()))
            .collect()
    }
}

struct FieldVisitor<'a> {
    message: &'a mut String,
    fields: &'a mut BTreeMap<String, String>,
}

impl tracing::field::Visit for FieldVisitor<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            *self.message = value.to_owned();
        } else {
            self.fields.insert(field.name().to_owned(), value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let text = format!("{value:?}");
        if field.name() == "message" {
            *self.message = text;
        } else {
            self.fields.insert(field.name().to_owned(), text);
        }
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut message = String::new();
        let mut fields = BTreeMap::new();
        event.record(&mut FieldVisitor {
            message: &mut message,
            fields: &mut fields,
        });
        self.0.lock().unwrap().push((std::thread::current().id(), message, fields));
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

fn non_repo_status() -> Value {
    json!({
        "isRepo": false,
        "hasPrimaryRemote": false,
        "isDefaultRef": false,
        "refName": null,
        "hasWorkingTreeChanges": false,
        "workingTree": {"files": [], "insertions": 0, "deletions": 0},
        "hasUpstream": false,
        "aheadCount": 0,
        "behindCount": 0,
        "aheadOfDefaultCount": 0,
        "pr": null,
    })
}

// ---------------------------------------------------------------------------------------------
// Status PR metadata
// ---------------------------------------------------------------------------------------------

// TS: "status includes draft PR metadata when branch already has a draft PR"
#[tokio::test]
async fn status_includes_draft_pr_metadata_when_branch_already_has_a_draft_pr() {
    let (repo, _remote) = repo_with_pushed_branch("feature/status-open-pr");
    let h = manager_with(sequence(&[json!([{
        "number": 13,
        "title": "Existing PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/13",
        "baseRefName": "main",
        "headRefName": "feature/status-open-pr",
        "isDraft": true,
    }])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["isRepo"], json!(true));
    assert_eq!(status["hasPrimaryRemote"], json!(true));
    assert_eq!(status["isDefaultRef"], json!(false));
    assert_eq!(status["refName"], json!("feature/status-open-pr"));
    assert_eq!(
        status["pr"],
        json!({
            "number": 13,
            "title": "Existing PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/13",
            "baseRef": "main",
            "headRef": "feature/status-open-pr",
            "state": "open",
            "isDraft": true,
            "updatedAt": null,
        })
    );
}

// TS: "status trims PR metadata returned by gh before publishing it"
#[tokio::test]
async fn status_trims_pr_metadata_returned_by_gh_before_publishing_it() {
    let (repo, _remote) = repo_with_pushed_branch("feature/status-trimmed-pr");
    let h = manager_with(sequence(&[json!([{
        "number": 14,
        "title": "  Existing PR title  \n",
        "url": " https://github.com/pingdotgg/codething-mvp/pull/14 ",
        "baseRefName": " main ",
        "headRefName": "\tfeature/status-trimmed-pr\t",
    }])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(
        status["pr"],
        json!({
            "number": 14,
            "title": "Existing PR title",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/14",
            "baseRef": "main",
            "headRef": "feature/status-trimmed-pr",
            "state": "open",
            "updatedAt": null,
        })
    );
}

// TS: "status ignores invalid gh pr list entries and keeps valid ones"
#[tokio::test]
async fn status_ignores_invalid_gh_pr_list_entries_and_keeps_valid_ones() {
    let (repo, _remote) = repo_with_pushed_branch("feature/status-valid-pr-entry");
    let h = manager_with(sequence(&[json!([
        {
            "number": 0,
            "title": "invalid",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/0",
            "baseRefName": "main",
            "headRefName": "feature/invalid",
        },
        {
            "number": 15,
            "title": "  Valid PR title  ",
            "url": " https://github.com/pingdotgg/codething-mvp/pull/15 ",
            "baseRefName": " main ",
            "headRefName": "\tfeature/status-valid-pr-entry\t",
            "headRepository": {"nameWithOwner": "   "},
            "headRepositoryOwner": {"login": "   "},
        },
    ])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(
        status["pr"],
        json!({
            "number": 15,
            "title": "Valid PR title",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/15",
            "baseRef": "main",
            "headRef": "feature/status-valid-pr-entry",
            "state": "open",
            "updatedAt": null,
        })
    );
}

// TS: "status preserves lowercase merged and closed PR states from gh json"
#[tokio::test]
async fn status_preserves_lowercase_merged_and_closed_pr_states_from_gh_json() {
    let (repo, _remote) = repo_with_pushed_branch("feature/status-lowercase-state");
    let h = manager_with(sequence(&[json!([
        {
            "number": 16,
            "title": "Closed PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/16",
            "baseRefName": "main",
            "headRefName": "feature/status-lowercase-state",
            "state": "closed",
            "updatedAt": "2026-01-01T00:00:00.000Z",
        },
        {
            "number": 17,
            "title": "Merged PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/17",
            "baseRefName": "main",
            "headRefName": "feature/status-lowercase-state",
            "state": "merged",
            "updatedAt": "2026-01-02T00:00:00.000Z",
        },
    ])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(
        status["pr"],
        json!({
            "number": 17,
            "title": "Merged PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/17",
            "baseRef": "main",
            "headRef": "feature/status-lowercase-state",
            "state": "merged",
            "updatedAt": "2026-01-02T00:00:00.000Z",
        })
    );
}

// TS: "status returns an explicit non-repo result for non-git directories"
#[tokio::test]
async fn status_returns_an_explicit_non_repo_result_for_non_git_directories() {
    let cwd = Tmp::new("t3code-git-manager-non-repo-");
    let h = make_manager(ManagerOptions::default());
    assert_eq!(status(&h, cwd.str()).await, non_repo_status());
}

// TS: "status returns an explicit non-repo result for deleted directories"
#[tokio::test]
async fn status_returns_an_explicit_non_repo_result_for_deleted_directories() {
    let root = Tmp::new("t3code-git-manager-missing-dir-");
    let cwd = root.join("deleted-repo");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::remove_dir_all(&cwd).unwrap();
    let h = make_manager(ManagerOptions::default());
    assert_eq!(status(&h, &cwd).await, non_repo_status());
}

// ---------------------------------------------------------------------------------------------
// Status caching
// ---------------------------------------------------------------------------------------------

// TS: "status briefly caches repeated lookups for the same cwd"
#[tokio::test]
async fn status_briefly_caches_repeated_lookups_for_the_same_cwd() {
    let (repo, _remote) = repo_with_pushed_branch("feature/status-cache");
    let existing = json!([{
        "number": 113,
        "title": "Cached PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/113",
        "baseRefName": "main",
        "headRefName": "feature/status-cache",
    }]);
    let h = manager_with(sequence(&[existing.clone(), existing]));

    let first = status(&h, repo.str()).await;
    let second = status(&h, repo.str()).await;
    assert_eq!(first["pr"]["number"], json!(113));
    assert_eq!(second["pr"]["number"], json!(113));
    assert_eq!(pr_list_calls(&h).len(), 1);
}

// TS: "a warm PR cache does not reread repository identity for status"
#[tokio::test]
async fn a_warm_pr_cache_does_not_reread_repository_identity_for_status() {
    let (repo, _remote) = repo_with_pushed_branch("feature/status-identity-cache");
    let reads = Arc::new(ConfigReads(Mutex::new(Vec::new())));
    let (manager, _temp) = manager_recording_config_reads(reads.clone());
    let options = || RemoteStatusOptions {
        refresh_upstream: false,
        refresh_missing_pull_request: false,
    };

    manager.remote_status(repo.str(), options()).await.unwrap();
    // Not in the TS test: the recorder does see the cold lookup's identity reads.
    assert!(reads.0.lock().unwrap().iter().any(|key| key == "remote.origin.url"));
    reads.0.lock().unwrap().clear();
    manager.remote_status(repo.str(), options()).await.unwrap();

    let identity_reads: Vec<String> = reads
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|key| key.as_str() == "branch.feature/status-identity-cache.remote" || key.as_str() == "remote.origin.url")
        .cloned()
        .collect();
    assert!(identity_reads.is_empty(), "{identity_reads:?}");
}

// TS: "turn-end refresh finds a new PR and keeps known PRs cached"
#[tokio::test]
async fn turn_end_refresh_finds_a_new_pr_and_keeps_known_prs_cached() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/turn-refresh", "origin/main"]);
    git(&repo.path, &["push", "origin", "feature/turn-refresh"]);
    let h = manager_with(GhScenario {
        pr_list_sequence: vec![
            "[]".into(),
            list(json!([{
                "number": 114,
                "title": "Opened during the turn",
                "url": "https://github.com/pingdotgg/codething-mvp/pull/114",
                "baseRefName": "main",
                "headRefName": "feature/turn-refresh",
            }])),
        ],
        ..GhScenario::default()
    });

    assert_eq!(remote_status(&h, repo.str(), true, false).await["pr"], Value::Null);
    assert_eq!(remote_status(&h, repo.str(), false, false).await["pr"], Value::Null);

    let refreshed = remote_status(&h, repo.str(), false, true).await;
    assert_eq!(refreshed["pr"]["number"], json!(114));
    remote_status(&h, repo.str(), false, true).await;
    assert_eq!(pr_list_calls(&h).len(), 2);
}

// TS: "turn-end refresh preserves failed PR lookup backoff"
#[tokio::test]
async fn turn_end_refresh_preserves_failed_pr_lookup_backoff() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["checkout", "-b", "feature/rate-limited"]);
    git(&repo.path, &["push", "-u", "origin", "feature/rate-limited"]);
    let h = manager_with(unavailable_after(0));

    remote_status(&h, repo.str(), true, false).await;
    let calls_after_failure = h.gh_calls().len();
    remote_status(&h, repo.str(), false, true).await;
    assert!(calls_after_failure > 0);
    assert_eq!(h.gh_calls().len(), calls_after_failure);
}

// TS: "status skips the provider lookup for a branch that was never pushed"
#[tokio::test]
async fn status_skips_the_provider_lookup_for_a_branch_that_was_never_pushed() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/never-pushed"]);
    let h = make_manager(ManagerOptions::default());

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("feature/never-pushed"));
    assert_eq!(status["pr"], Value::Null);
    assert_eq!(pr_list_calls(&h).len(), 0);
}

// ---------------------------------------------------------------------------------------------
// Branch PR lookup
// ---------------------------------------------------------------------------------------------

// TS: "branch PR lookup returns null when the repository has no remotes"
#[tokio::test]
async fn branch_pr_lookup_returns_null_when_the_repository_has_no_remotes() {
    let repo = repo();
    let h = make_manager(ManagerOptions::default());
    assert_eq!(branch_pr(&h, repo.str(), "main", false).await.unwrap(), None);
    assert!(h.gh_calls().is_empty());
}

// TS: "branch PR lookup uses a saved tracked branch without changing checkout"
#[tokio::test]
async fn branch_pr_lookup_uses_a_saved_tracked_branch_without_changing_checkout() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/saved-branch"]);
    git(&repo.path, &["push", "-u", "origin", "feature/saved-branch"]);
    git(&repo.path, &["checkout", "main"]);
    let h = manager_with(sequence(&[json!([{
        "number": 216,
        "title": "Saved branch PR",
        "url": "https://github.com/pingdotgg/t3code/pull/216",
        "baseRefName": "main",
        "headRefName": "feature/saved-branch",
        "state": "OPEN",
        "updatedAt": "2026-04-03T15:00:00Z",
    }])]));

    let pull_request = branch_pr(&h, repo.str(), "feature/saved-branch", false).await.unwrap().unwrap();
    assert_match_object(
        &pull_request,
        json!({
            "number": 216,
            "title": "Saved branch PR",
            "url": "https://github.com/pingdotgg/t3code/pull/216",
            "baseRef": "main",
            "headRef": "feature/saved-branch",
            "state": "open",
            "closedAt": null,
            "mergedAt": null,
            "updatedAt": "2026-04-03T15:00:00.000Z",
        }),
    );
    assert_eq!(git(&repo.path, &["branch", "--show-current"]), "main");
}

// TS: "branch PR lookup uses the default branch from a non-origin remote"
#[tokio::test]
async fn branch_pr_lookup_uses_the_default_branch_from_a_non_origin_remote() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "upstream", remote.str()]);
    git(&repo.path, &["push", "-u", "upstream", "main"]);
    git(&repo.path, &["checkout", "-b", "develop"]);
    git(&repo.path, &["push", "-u", "upstream", "develop"]);
    git(&remote.path, &["symbolic-ref", "HEAD", "refs/heads/develop"]);
    git(&repo.path, &["remote", "set-head", "upstream", "develop"]);
    let h = manager_with(sequence(&[json!([{
        "number": 221,
        "title": "Merged main PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/221",
        "baseRefName": "develop",
        "headRefName": "main",
        "state": "MERGED",
        "mergedAt": "2026-04-07T15:00:00Z",
        "updatedAt": "2026-04-08T15:00:00Z",
    }])]));

    let pull_request = branch_pr(&h, repo.str(), "main", false).await.unwrap().unwrap();
    assert_match_object(
        &pull_request,
        json!({
            "state": "merged",
            "closedAt": null,
            "mergedAt": "2026-04-07T15:00:00Z",
            "updatedAt": "2026-04-08T15:00:00.000Z",
        }),
    );
}

// TS: "branch PR lookup uses the saved name after the local branch is deleted"
#[tokio::test]
async fn branch_pr_lookup_uses_the_saved_name_after_the_local_branch_is_deleted() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/deleted-local-branch"]);
    git(&repo.path, &["push", "-u", "origin", "feature/deleted-local-branch"]);
    git(&repo.path, &["checkout", "main"]);
    git(&repo.path, &["branch", "-D", "feature/deleted-local-branch"]);
    git(&repo.path, &["branch", "feature/deleted-local-branch/child"]);
    git(
        &repo.path,
        &["branch", "--set-upstream-to", "origin/main", "feature/deleted-local-branch/child"],
    );
    let h = manager_with(sequence(&[json!([{
        "number": 217,
        "title": "Deleted local branch PR",
        "url": "https://github.com/pingdotgg/t3code/pull/217",
        "baseRefName": "main",
        "headRefName": "feature/deleted-local-branch",
        "state": "MERGED",
        "updatedAt": "2026-04-04T15:00:00Z",
    }])]));

    let pull_request = branch_pr(&h, repo.str(), "feature/deleted-local-branch", false).await.unwrap().unwrap();
    assert_match_object(
        &pull_request,
        json!({
            "state": "merged",
            "closedAt": null,
            "mergedAt": null,
            "updatedAt": "2026-04-04T15:00:00.000Z",
        }),
    );
    assert!(h.gh_calls().iter().any(|call| call.contains("--head feature/deleted-local-branch")));
}

// TS: "branch PR lookup recovers a deleted fork branch from its remote-tracking ref"
#[tokio::test]
async fn branch_pr_lookup_recovers_a_deleted_fork_branch_from_its_remote_tracking_ref() {
    let repo = repo();
    let origin = bare_remote();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "origin", origin.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    configure_remote(&repo.path, "team/fork", fork.str(), "team/fork");
    git(&repo.path, &["checkout", "-b", "feature/deleted-fork-branch"]);
    git(&repo.path, &["push", "-u", "team/fork", "feature/deleted-fork-branch"]);
    git(&repo.path, &["checkout", "main"]);
    git(&repo.path, &["branch", "-D", "feature/deleted-fork-branch"]);
    configure_visible_remote(&repo.path, "origin", "git@github.com:pingdotgg/codething-mvp.git", origin.str());
    configure_visible_remote(&repo.path, "team/fork", "git@github.com:contributor/codething-mvp.git", fork.str());
    let h = manager_with(by_head(&[(
        "feature/deleted-fork-branch",
        json!([{
            "number": 218,
            "title": "Deleted fork branch PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/218",
            "baseRefName": "main",
            "headRefName": "feature/deleted-fork-branch",
            "state": "MERGED",
            "updatedAt": "2026-04-05T15:00:00Z",
            "isCrossRepository": true,
            "headRepository": {"nameWithOwner": "contributor/codething-mvp"},
            "headRepositoryOwner": {"login": "contributor"},
        }]),
    )]));

    let pull_request = branch_pr(&h, repo.str(), "feature/deleted-fork-branch", false).await.unwrap().unwrap();
    assert_match_object(
        &pull_request,
        json!({
            "state": "merged",
            "closedAt": null,
            "mergedAt": null,
            "updatedAt": "2026-04-05T15:00:00.000Z",
        }),
    );
    let calls = h.gh_calls();
    assert!(
        calls.iter().any(|call| call.contains("--head feature/deleted-fork-branch --state all")),
        "{calls:?}"
    );
    assert!(!calls.iter().any(|call| call.contains("--head contributor:")), "{calls:?}");
}

// TS: "branch PR lookup rejects ambiguous deleted-branch remote refs"
#[tokio::test]
async fn branch_pr_lookup_rejects_ambiguous_deleted_branch_remote_refs() {
    let repo = repo();
    let origin = bare_remote();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "origin", origin.str()]);
    git(&repo.path, &["remote", "add", "fork", fork.str()]);
    git(&repo.path, &["checkout", "-b", "feature/ambiguous-remote"]);
    git(&repo.path, &["push", "origin", "feature/ambiguous-remote"]);
    git(&repo.path, &["push", "fork", "feature/ambiguous-remote"]);
    git(&repo.path, &["checkout", "main"]);
    git(&repo.path, &["branch", "-D", "feature/ambiguous-remote"]);
    let h = make_manager(ManagerOptions::default());

    let error = branch_pr(&h, repo.str(), "feature/ambiguous-remote", false).await.unwrap_err();
    match &error {
        GitManagerServiceError::Manager(error) => {
            assert_eq!(error.detail, "Multiple remotes track feature/ambiguous-remote. Its pull request is ambiguous.");
        }
        other => panic!("expected a GitManagerError, got {other:?}"),
    }
    assert!(h.gh_calls().is_empty());
}

// TS: "branch PR lookup does not reuse a cached PR after the remote is repointed"
#[tokio::test]
async fn branch_pr_lookup_does_not_reuse_a_cached_pr_after_the_remote_is_repointed() {
    let repo = repo();
    let original = bare_remote();
    git(&repo.path, &["remote", "add", "origin", original.str()]);
    git(&repo.path, &["checkout", "-b", "feature/repointed-lookup"]);
    git(&repo.path, &["push", "-u", "origin", "feature/repointed-lookup"]);
    configure_visible_remote(&repo.path, "origin", "git@github.com:old-owner/old-repository.git", original.str());
    let h = manager_with(GhScenario {
        pr_list_sequence: vec![
            list(json!([{
                "number": 219,
                "title": "Old repository PR",
                "url": "https://github.com/old-owner/old-repository/pull/219",
                "baseRefName": "main",
                "headRefName": "feature/repointed-lookup",
                "state": "MERGED",
                "updatedAt": "2026-04-06T15:00:00Z",
            }])),
            "[]".into(),
        ],
        ..GhScenario::default()
    });

    let first = branch_pr(&h, repo.str(), "feature/repointed-lookup", false).await.unwrap().unwrap();
    assert_eq!(first["state"], json!("merged"));

    let replacement = bare_remote();
    configure_visible_remote(&repo.path, "origin", "git@github.com:new-owner/new-repository.git", replacement.str());

    let second = branch_pr(&h, repo.str(), "feature/repointed-lookup", false).await.unwrap();
    assert_eq!(second, None);
    assert_eq!(pr_list_calls(&h).len(), 2);
}

// TS: "branch PR lookup shares the status cache for the same repository identity"
#[tokio::test]
async fn branch_pr_lookup_shares_the_status_cache_for_the_same_repository_identity() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["checkout", "-b", "feature/shared-pr-cache"]);
    git(&repo.path, &["push", "-u", "origin", "feature/shared-pr-cache"]);
    let h = manager_with(sequence(&[
        json!([{
            "number": 220,
            "title": "Shared cache PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/220",
            "baseRefName": "main",
            "headRefName": "feature/shared-pr-cache",
            "state": "MERGED",
            "updatedAt": "2026-04-07T15:00:00Z",
        }]),
        json!([{
            "number": 221,
            "title": "New PR on the same branch",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/221",
            "baseRefName": "main",
            "headRefName": "feature/shared-pr-cache",
            "state": "OPEN",
            "updatedAt": "2026-04-08T15:00:00Z",
        }]),
    ]));

    let status = status(&h, repo.str()).await;
    let pull_request = branch_pr(&h, repo.str(), "feature/shared-pr-cache", false).await.unwrap().unwrap();
    assert_eq!(status["pr"]["state"], json!("merged"));
    assert_eq!(pull_request["state"], json!("merged"));
    assert_eq!(pr_list_calls(&h).len(), 1);

    let refreshed = branch_pr(&h, repo.str(), "feature/shared-pr-cache", true).await.unwrap().unwrap();
    assert_match_object(
        &refreshed,
        json!({
            "number": 221,
            "state": "open",
            "repositoryKey": "github.com/pingdotgg/codething-mvp",
        }),
    );
    assert_eq!(pr_list_calls(&h).len(), 2);
}

// TS: "branch PR lookup rechecks open PRs every minute and settled answers less often"
//
// The TS test drives a TestClock. The lookup cache runs on `tokio::time::Instant`, so time is
// paused and advanced like the TestClock. The lookups run real git processes under tokio
// timeouts, which a paused clock would fire at once (the runtime auto-advances when it idles
// while waiting on a child): a task that keeps yielding keeps the runtime from idling, so the
// clock only moves through `advance` (and real time spent in git never counts).
#[tokio::test]
async fn branch_pr_lookup_rechecks_open_prs_every_minute_and_settled_answers_less_often() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    let branches = ["feature/open-pr", "feature/merged-pr", "feature/no-pr"];
    for branch in branches {
        git(&repo.path, &["checkout", "-b", branch, "main"]);
        git(&repo.path, &["push", "-u", "origin", branch]);
    }
    let pull_request = |number: u64, head: &str, state: &str| {
        json!([{
            "number": number,
            "title": head,
            "url": format!("https://github.com/pingdotgg/codething-mvp/pull/{number}"),
            "baseRefName": "main",
            "headRefName": head,
            "state": state,
            "updatedAt": "2026-04-07T15:00:00Z",
        }])
    };
    let h = manager_with(by_head(&[
        ("feature/open-pr", pull_request(301, "feature/open-pr", "OPEN")),
        ("feature/merged-pr", pull_request(302, "feature/merged-pr", "MERGED")),
    ]));
    let lookup_all = || async {
        for branch in branches {
            h.manager.branch_pull_request(repo.str(), branch, false).await.unwrap();
        }
    };
    let adjust = |seconds: u64| tokio::time::advance(Duration::from_secs(seconds));
    tokio::time::pause();
    let keep_busy = tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    });

    lookup_all().await;
    assert_eq!(pr_list_calls(&h).len(), 3);

    adjust(61).await;
    lookup_all().await;
    assert_eq!(pr_list_calls(&h).len(), 4);
    assert!(pr_list_calls(&h).last().unwrap().contains("--head feature/open-pr"));

    // Just inside the 5-minute window only the open PR is asked again.
    adjust(238).await;
    lookup_all().await;
    assert_eq!(pr_list_calls(&h).len(), 5);
    assert!(pr_list_calls(&h).last().unwrap().contains("--head feature/open-pr"));

    // Just past it the settled answers expire too.
    adjust(2).await;
    lookup_all().await;
    let calls = pr_list_calls(&h);
    assert_eq!(calls.len(), 7);
    assert!(!calls[calls.len() - 2..].join("\n").contains("--head feature/open-pr"));
    keep_busy.abort();
}

// TS: "branch PR lookup propagates provider failures"
#[tokio::test]
async fn branch_pr_lookup_propagates_provider_failures() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/lookup-failure"]);
    git(&repo.path, &["push", "-u", "origin", "feature/lookup-failure"]);
    git(&repo.path, &["checkout", "main"]);
    let h = manager_with(unavailable_after(0));

    let error = branch_pr(&h, repo.str(), "feature/lookup-failure", false).await.unwrap_err();
    assert_eq!(error_tag(&error), "SourceControlProviderError");
    let refresh_error = branch_pr(&h, repo.str(), "feature/lookup-failure", true).await.unwrap_err();
    assert_eq!(error_tag(&refresh_error), "SourceControlProviderError");
    assert_eq!(pr_list_calls(&h).len(), 1);
}

// ---------------------------------------------------------------------------------------------
// Published and unpublished branches
// ---------------------------------------------------------------------------------------------

// TS: "status finds a merged PR after its remote branch was deleted"
#[tokio::test]
async fn status_finds_a_merged_pr_after_its_remote_branch_was_deleted() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/merged-branch-deleted"]);
    git(&repo.path, &["push", "-u", "origin", "feature/merged-branch-deleted"]);

    // GitHub commonly deletes a pull request's head branch after merge. Git removes the
    // remote-tracking ref, but preserves the local branch's remote and merge configuration as
    // evidence that it was published.
    git(&repo.path, &["push", "origin", "--delete", "feature/merged-branch-deleted"]);
    assert_eq!(git(&repo.path, &["config", "--get", "branch.feature/merged-branch-deleted.remote"]), "origin");
    assert_eq!(
        git(&repo.path, &["config", "--get", "branch.feature/merged-branch-deleted.merge"]),
        "refs/heads/feature/merged-branch-deleted"
    );
    assert_eq!(
        git(
            &repo.path,
            &["for-each-ref", "--format=%(refname)", "refs/remotes/origin/feature/merged-branch-deleted"]
        ),
        ""
    );

    let h = manager_with(sequence(&[json!([{
        "number": 215,
        "title": "Merged branch was deleted",
        "url": "https://github.com/pingdotgg/t3code/pull/215",
        "baseRefName": "main",
        "headRefName": "feature/merged-branch-deleted",
        "state": "MERGED",
        "mergedAt": "2026-04-02T15:00:00Z",
        "updatedAt": "2026-04-02T15:00:00Z",
    }])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["hasUpstream"], json!(false));
    assert_eq!(
        status["pr"],
        json!({
            "number": 215,
            "title": "Merged branch was deleted",
            "url": "https://github.com/pingdotgg/t3code/pull/215",
            "baseRef": "main",
            "headRef": "feature/merged-branch-deleted",
            "state": "merged",
            "updatedAt": "2026-04-02T15:00:00.000Z",
        })
    );
    assert!(!pr_list_calls(&h).is_empty());
}

// TS: "status still looks up PRs for a branch pushed without --set-upstream"
#[tokio::test]
async fn status_still_looks_up_prs_for_a_branch_pushed_without_set_upstream() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/pushed-no-upstream"]);
    // No `-u`, so the remote-tracking ref exists but branch.<name>.merge does not. Most
    // terminal and agent pushes land this way, and they can still have a PR, so the skip must
    // not trigger here.
    git(&repo.path, &["push", "origin", "feature/pushed-no-upstream"]);
    let h = manager_with(sequence(&[json!([{
        "number": 214,
        "title": "Pushed without upstream",
        "url": "https://github.com/pingdotgg/t3code/pull/214",
        "baseRefName": "main",
        "headRefName": "feature/pushed-no-upstream",
        "state": "OPEN",
        "updatedAt": "2026-04-01T15:00:00Z",
    }])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["pr"]["number"], json!(214));
    assert!(!pr_list_calls(&h).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

// TS: "backs off repeated PR lookup failures past the healthy refresh cadence"
#[test]
fn backs_off_repeated_pr_lookup_failures_past_the_healthy_refresh_cadence() {
    assert_eq!(pr_lookup_failure_ttl(1).as_millis(), 20_000);
    assert_eq!(pr_lookup_failure_ttl(2).as_millis(), 40_000);
    // The point of the backoff: by the third retry a failing branch must not be asking more
    // often than a healthy one, which refreshes every 2 minutes.
    assert!(pr_lookup_failure_ttl(4).as_millis() > 120_000);
    assert_eq!(pr_lookup_failure_ttl(20).as_millis(), 900_000);
}

// TS: "reads the repository from the returned PR URL %s" (it.each)
#[test]
fn reads_the_repository_from_the_returned_pr_url() {
    let cases: [(&str, Option<&str>); 7] = [
        (
            "https://github.example.com/team/repository/pull/42?tab=files",
            Some("github.example.com/team/repository"),
        ),
        (
            "https://gitlab.example.com/group/subgroup/repository/-/merge_requests/42",
            Some("gitlab.example.com/group/subgroup/repository"),
        ),
        ("https://bitbucket.org/team/repository/pull-requests/42", Some("bitbucket.org/team/repository")),
        (
            "https://dev.azure.com/org/project/_git/repository/pullrequest/42",
            Some("dev.azure.com/org/project/_git/repository"),
        ),
        (
            "https://org.visualstudio.com/project/_git/repository/pullrequest/42",
            Some("org.visualstudio.com/project/_git/repository"),
        ),
        (
            "https://gitlab.example/group/pull/123/repository/-/merge_requests/42",
            Some("gitlab.example/group/pull/123/repository"),
        ),
        ("https://github.example.com/team/repository/issues/42", None),
    ];
    for (url, expected) in cases {
        assert_eq!(pull_request_repository_key(url).as_deref(), expected, "{url}");
    }
}

// ---------------------------------------------------------------------------------------------
// Head matching
// ---------------------------------------------------------------------------------------------

// TS: "distinguishes Enterprise forks with the same head branch"
#[tokio::test]
async fn distinguishes_enterprise_forks_with_the_same_head_branch() {
    let repo = repo();
    let origin = bare_remote();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "origin", origin.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["remote", "add", "fork", fork.str()]);
    git(&repo.path, &["checkout", "-b", "feature"]);
    git(&repo.path, &["push", "-u", "fork", "feature"]);
    configure_visible_remote(&repo.path, "origin", "git@github.example.com:team/repository.git", origin.str());
    configure_visible_remote(&repo.path, "fork", "git@github.example.com:alice/repository.git", fork.str());
    let h = manager_with(by_head(&[(
        "feature",
        json!([
            {
                "number": 2,
                "title": "Another fork",
                "url": "https://github.example.com/team/repository/pull/2",
                "baseRefName": "main",
                "headRefName": "feature",
                "state": "OPEN",
                "updatedAt": "2026-04-08T15:00:00Z",
                "isCrossRepository": true,
                "headRepository": {"nameWithOwner": "bob/repository"},
                "headRepositoryOwner": {"login": "bob"},
            },
            {
                "number": 1,
                "title": "This fork",
                "url": "https://github.example.com/team/repository/pull/1",
                "baseRefName": "main",
                "headRefName": "feature",
                "state": "OPEN",
                "updatedAt": "2026-04-07T15:00:00Z",
                "isCrossRepository": true,
                "headRepository": {"nameWithOwner": "alice/repository"},
                "headRepositoryOwner": {"login": "alice"},
            },
        ]),
    )]));

    let pull_request = branch_pr(&h, repo.str(), "feature", false).await.unwrap().unwrap();
    assert_match_object(
        &pull_request,
        json!({
            "number": 1,
            "repositoryKey": "github.example.com/team/repository",
        }),
    );
}

// TS: "matches nested GitLab forks through the adapter for %s" (it.effect.each)
#[tokio::test]
async fn matches_nested_gitlab_forks_through_the_adapter() {
    for remote_url in ["git@gitlab.com:Group/Subgroup/Fork.git", "https://gitlab.com/Group/Subgroup/Fork.git"] {
        let repo = repo();
        let origin = bare_remote();
        let fork = bare_remote();
        let branch = "feature/NestedGroups";
        git(&repo.path, &["remote", "add", "origin", origin.str()]);
        git(&repo.path, &["push", "-u", "origin", "main"]);
        git(&repo.path, &["remote", "add", "fork", fork.str()]);
        git(&repo.path, &["checkout", "-b", branch]);
        git(&repo.path, &["push", "-u", "fork", branch]);
        configure_visible_remote(&repo.path, "origin", "git@gitlab.com:Group/Upstream/Repository.git", origin.str());
        configure_visible_remote(&repo.path, "fork", remote_url, fork.str());
        let output = json!([
            {
                "iid": 2,
                "title": "Another subgroup's fork",
                "web_url": "https://gitlab.com/Group/Upstream/Repository/-/merge_requests/2",
                "target_branch": "main",
                "source_branch": branch,
                "state": "opened",
                "updated_at": "2026-04-08T15:00:00Z",
                "source_project_id": 102,
                "target_project_id": 100,
                "source_project": {"path_with_namespace": "Group/Other/Fork"},
            },
            {
                "iid": 1,
                "title": "This subgroup's fork",
                "web_url": "https://gitlab.com/Group/Upstream/Repository/-/merge_requests/1",
                "target_branch": "main",
                "source_branch": branch,
                "state": "opened",
                "updated_at": "2026-04-07T15:00:00Z",
                "source_project_id": 101,
                "target_project_id": 100,
                "source_project": {"path_with_namespace": "Group/Subgroup/Fork"},
            },
        ])
        .to_string();
        let runner = Arc::new(RecordingRunner {
            output,
            calls: Mutex::new(Vec::new()),
        });
        let provider: Arc<dyn SourceControlProvider> = Arc::new(GitLabSourceControlProvider::new(GitLabCli::new(VcsProcess::new(runner.clone()))));
        let h = make_manager(ManagerOptions {
            provider: Some(provider),
            ..ManagerOptions::default()
        });

        let pull_request = branch_pr(&h, repo.str(), branch, false).await.unwrap().unwrap();
        assert_match_object(
            &pull_request,
            json!({
                "number": 1,
                "repositoryKey": "gitlab.com/group/upstream/repository",
            }),
        );
        let calls = runner.calls.lock().unwrap().clone();
        assert!(!calls.is_empty(), "{remote_url}");
        for (command, args) in calls {
            assert_eq!(command, "glab", "{remote_url}");
            assert_eq!(
                args,
                ["mr", "list", "--source-branch", branch, "--all", "--per-page", "20", "--output", "json"],
                "{remote_url}"
            );
        }
    }
}

// TS: "status ignores unrelated fork PRs when the current branch tracks the same repository"
#[tokio::test]
async fn status_ignores_unrelated_fork_prs_when_the_current_branch_tracks_the_same_repository() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    let h = manager_with(sequence(&[json!([{
        "number": 1661,
        "title": "Fork PR from main",
        "url": "https://github.com/pingdotgg/t3code/pull/1661",
        "baseRefName": "main",
        "headRefName": "main",
        "state": "OPEN",
        "updatedAt": "2026-04-01T15:00:00Z",
        "isCrossRepository": true,
        "headRepository": {"nameWithOwner": "lnieuwenhuis/t3code"},
        "headRepositoryOwner": {"login": "lnieuwenhuis"},
    }])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("main"));
    assert_eq!(status["pr"], Value::Null);
}

// TS: "status detects cross-repo PRs from the upstream remote URL owner"
#[tokio::test]
async fn status_detects_cross_repo_prs_from_the_upstream_remote_url_owner() {
    let repo = repo();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["checkout", "-b", "statemachine"]);
    write(&repo.path, "fork-pr.txt", "fork pr\n");
    git(&repo.path, &["add", "fork-pr.txt"]);
    git(&repo.path, &["commit", "-m", "Fork PR branch"]);
    git(&repo.path, &["push", "-u", "fork-seed", "statemachine"]);
    git(&repo.path, &["checkout", "-b", "t3code/pr-488/statemachine"]);
    git(&repo.path, &["branch", "--set-upstream-to", "fork-seed/statemachine"]);
    configure_visible_remote(&repo.path, "fork-seed", "git@github.com:jasonLaster/codething-mvp.git", fork.str());
    let h = manager_with(sequence(&[json!([{
        "number": 488,
        "title": "Rebase this PR on latest main",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/488",
        "baseRefName": "main",
        "headRefName": "statemachine",
        "state": "OPEN",
        "updatedAt": "2026-03-10T07:00:00Z",
        "isCrossRepository": true,
        "headRepository": {"nameWithOwner": "jasonLaster/codething-mvp"},
        "headRepositoryOwner": {"login": "jasonLaster"},
    }])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("t3code/pr-488/statemachine"));
    assert_eq!(
        status["pr"],
        json!({
            "number": 488,
            "title": "Rebase this PR on latest main",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/488",
            "baseRef": "main",
            "headRef": "statemachine",
            "state": "open",
            "updatedAt": "2026-03-10T07:00:00.000Z",
        })
    );
    let calls = h.gh_calls();
    assert!(
        calls.contains(&"pr list --head statemachine --state all --limit 100 --json number,title,url,baseRefName,headRefName,state,isDraft,mergedAt,closedAt,updatedAt,isCrossRepository,headRepository,headRepositoryOwner".to_owned()),
        "{calls:?}"
    );
}

// TS: "status preserves a fork PR whose head is named after the default branch"
#[tokio::test]
async fn status_preserves_a_fork_pr_whose_head_is_named_after_the_default_branch() {
    let repo = repo();
    let origin = bare_remote();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "origin", origin.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["remote", "set-head", "origin", "main"]);
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["push", "fork-seed", "main"]);
    git(&repo.path, &["checkout", "-b", "t3code/pr-777/main"]);
    git(&repo.path, &["branch", "--set-upstream-to", "fork-seed/main"]);
    configure_visible_remote(&repo.path, "fork-seed", "git@github.com:contributor/codething-mvp.git", fork.str());
    let h = manager_with(by_head(&[(
        "main",
        json!([{
            "number": 777,
            "title": "Fork PR from main",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/777",
            "baseRefName": "main",
            "headRefName": "main",
            "state": "OPEN",
            "updatedAt": "2026-03-10T07:00:00Z",
            "isCrossRepository": true,
            "headRepository": {"nameWithOwner": "contributor/codething-mvp"},
            "headRepositoryOwner": {"login": "contributor"},
        }]),
    )]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("t3code/pr-777/main"));
    assert_eq!(
        status["pr"],
        json!({
            "number": 777,
            "title": "Fork PR from main",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/777",
            "baseRef": "main",
            "headRef": "main",
            "state": "open",
            "updatedAt": "2026-03-10T07:00:00.000Z",
        })
    );
    let calls = h.gh_calls();
    assert!(
        calls.contains(&"pr list --head main --state all --limit 100 --json number,title,url,baseRefName,headRefName,state,isDraft,mergedAt,closedAt,updatedAt,isCrossRepository,headRepository,headRepositoryOwner".to_owned()),
        "{calls:?}"
    );
}

// TS: "status ignores synthetic local branch aliases when the upstream remote name contains slashes"
#[tokio::test]
async fn status_ignores_synthetic_local_branch_aliases_when_the_upstream_remote_name_contains_slashes() {
    let repo = repo();
    let origin = bare_remote();
    let upstream = bare_remote();
    configure_remote(&repo.path, "origin", origin.str(), "origin");
    configure_remote(&repo.path, "my-org/upstream", upstream.str(), "my-org/upstream");

    git(&repo.path, &["checkout", "-b", "effect-atom"]);
    git(&repo.path, &["push", "-u", "origin", "effect-atom"]);
    git(&repo.path, &["push", "-u", "my-org/upstream", "effect-atom"]);
    configure_visible_remote(&repo.path, "origin", "git@github.com:pingdotgg/codething-mvp.git", origin.str());
    git(&repo.path, &["config", "remote.origin.pushurl", origin.str()]);
    configure_visible_remote(
        &repo.path,
        "my-org/upstream",
        "ssh://git@github.com/pingdotgg/codething-mvp.git",
        upstream.str(),
    );
    git(&repo.path, &["config", "remote.my-org/upstream.pushurl", upstream.str()]);
    git(&repo.path, &["checkout", "main"]);
    git(&repo.path, &["branch", "-D", "effect-atom"]);
    git(&repo.path, &["checkout", "--track", "my-org/upstream/effect-atom"]);

    let h = manager_with(by_head(&[
        (
            "effect-atom",
            json!([{
                "number": 1618,
                "title": "Correct PR",
                "url": "https://github.com/pingdotgg/t3code/pull/1618",
                "baseRefName": "main",
                "headRefName": "effect-atom",
                "state": "OPEN",
                "updatedAt": "2026-03-01T10:00:00Z",
            }]),
        ),
        (
            "upstream/effect-atom",
            json!([{
                "number": 1518,
                "title": "Wrong PR",
                "url": "https://github.com/pingdotgg/t3code/pull/1518",
                "baseRefName": "main",
                "headRefName": "upstream/effect-atom",
                "state": "OPEN",
                "updatedAt": "2026-04-01T10:00:00Z",
            }]),
        ),
    ]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("upstream/effect-atom"));
    assert_eq!(
        status["pr"],
        json!({
            "number": 1618,
            "title": "Correct PR",
            "url": "https://github.com/pingdotgg/t3code/pull/1618",
            "baseRef": "main",
            "headRef": "effect-atom",
            "state": "open",
            "updatedAt": "2026-03-01T10:00:00.000Z",
        })
    );
    let calls = h.gh_calls();
    assert!(!calls.iter().any(|call| call.contains("pr list --head upstream/effect-atom ")), "{calls:?}");
    assert!(
        !calls.iter().any(|call| call.contains("pr list --head pingdotgg:upstream/effect-atom ")),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|call| call.contains("pr list --head my-org/upstream:upstream/effect-atom ")),
        "{calls:?}"
    );
}

// TS: "status returns merged PR state when latest PR was merged"
#[tokio::test]
async fn status_returns_merged_pr_state_when_latest_pr_was_merged() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/status-merged-pr"]);
    let h = manager_with(sequence(&[json!([{
        "number": 22,
        "title": "Merged PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/22",
        "baseRefName": "main",
        "headRefName": "feature/status-merged-pr",
        "state": "MERGED",
        "mergedAt": "2026-01-30T10:00:00Z",
        "updatedAt": "2026-01-30T10:00:00Z",
    }])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("feature/status-merged-pr"));
    assert_eq!(
        status["pr"],
        json!({
            "number": 22,
            "title": "Merged PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/22",
            "baseRef": "main",
            "headRef": "feature/status-merged-pr",
            "state": "merged",
            "updatedAt": "2026-01-30T10:00:00.000Z",
        })
    );
}

// TS: "status hides merged PRs on the default branch"
#[tokio::test]
async fn status_hides_merged_prs_on_the_default_branch() {
    let repo = repo();
    let h = manager_with(sequence(&[json!([{
        "number": 23,
        "title": "Merged PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/23",
        "baseRefName": "feature/status-default-branch-target",
        "headRefName": "main",
        "state": "MERGED",
        "mergedAt": "2026-01-30T10:00:00Z",
        "updatedAt": "2026-01-30T10:00:00Z",
    }])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("main"));
    assert_eq!(status["pr"], Value::Null);
}

// TS: "status does not inherit a merged PR from a feature branch's default upstream"
#[tokio::test]
async fn status_does_not_inherit_a_merged_pr_from_a_feature_branchs_default_upstream() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["remote", "set-head", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/from-main", "origin/main"]);
    let h = manager_with(sequence(&[json!([{
        "number": 54,
        "title": "Reverse merge from main",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/54",
        "baseRefName": "je-filter-list",
        "headRefName": "main",
        "state": "MERGED",
        "mergedAt": "2023-09-28T03:21:10Z",
        "updatedAt": "2023-09-28T03:21:10Z",
    }])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("feature/from-main"));
    assert_eq!(status["pr"], Value::Null);
    assert!(!h.gh_calls().iter().any(|call| call.contains("pr list")));
}

// TS: "status finds a PR pushed under the branch's own name despite a default upstream"
#[tokio::test]
async fn status_finds_a_pr_pushed_under_the_branchs_own_name_despite_a_default_upstream() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["remote", "set-head", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/pushed-plain", "origin/main"]);
    // A plain push (no -u) leaves the upstream on origin/main.
    git(&repo.path, &["push", "origin", "feature/pushed-plain"]);
    let h = manager_with(by_head(&[(
        "feature/pushed-plain",
        json!([{
            "number": 88,
            "title": "Pushed without -u",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/88",
            "baseRefName": "main",
            "headRefName": "feature/pushed-plain",
            "state": "OPEN",
            "updatedAt": "2026-05-01T10:00:00Z",
        }]),
    )]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("feature/pushed-plain"));
    assert_eq!(status["pr"]["number"], json!(88));
    assert!(!h.gh_calls().iter().any(|call| call.contains("--head main")));
}

// TS: "status finds a fork PR pushed under the branch's own name despite a default upstream"
#[tokio::test]
async fn status_finds_a_fork_pr_pushed_under_the_branchs_own_name_despite_a_default_upstream() {
    let repo = repo();
    let origin = bare_remote();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "origin", origin.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["remote", "set-head", "origin", "main"]);
    configure_remote(&repo.path, "team/fork", fork.str(), "team/fork");
    git(&repo.path, &["checkout", "-b", "feature/fork-plain", "origin/main"]);
    // Pushed to the fork without -u: upstream stays origin/main.
    git(&repo.path, &["push", "team/fork", "feature/fork-plain"]);
    configure_visible_remote(&repo.path, "origin", "git@github.com:pingdotgg/codething-mvp.git", origin.str());
    configure_visible_remote(&repo.path, "team/fork", "git@github.com:contributor/codething-mvp.git", fork.str());
    let h = manager_with(by_head(&[(
        "feature/fork-plain",
        json!([{
            "number": 89,
            "title": "Fork PR pushed without -u",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/89",
            "baseRefName": "main",
            "headRefName": "feature/fork-plain",
            "state": "OPEN",
            "updatedAt": "2026-05-01T10:00:00Z",
            "isCrossRepository": true,
            "headRepository": {"nameWithOwner": "contributor/codething-mvp"},
            "headRepositoryOwner": {"login": "contributor"},
        }]),
    )]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["pr"]["number"], json!(89));
    let calls = h.gh_calls();
    assert!(calls.iter().any(|call| call.contains("--head feature/fork-plain")), "{calls:?}");
    assert!(!calls.iter().any(|call| call.contains("--head contributor:")), "{calls:?}");
    assert!(!calls.iter().any(|call| call.contains("--head main")), "{calls:?}");
}

// TS: "branch PR lookup verifies identity on the fork that holds the own-name ref"
#[tokio::test]
async fn branch_pr_lookup_verifies_identity_on_the_fork_that_holds_the_own_name_ref() {
    let repo = repo();
    let origin = bare_remote();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "origin", origin.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["remote", "set-head", "origin", "main"]);
    configure_remote(&repo.path, "team/fork", fork.str(), "team/fork");
    git(&repo.path, &["checkout", "-b", "feature/fork-settle", "origin/main"]);
    git(&repo.path, &["push", "team/fork", "feature/fork-settle"]);
    configure_visible_remote(&repo.path, "origin", "git@github.com:pingdotgg/codething-mvp.git", origin.str());
    configure_visible_remote(&repo.path, "team/fork", "git@github.com:contributor/codething-mvp.git", fork.str());
    let h = manager_with(by_head(&[(
        "feature/fork-settle",
        json!([{
            "number": 91,
            "title": "Fork PR to settle",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/91",
            "baseRefName": "main",
            "headRefName": "feature/fork-settle",
            "state": "MERGED",
            "updatedAt": "2026-05-02T10:00:00Z",
            "isCrossRepository": true,
            "headRepository": {"nameWithOwner": "contributor/codething-mvp"},
            "headRepositoryOwner": {"login": "contributor"},
        }]),
    )]));

    let pull_request = branch_pr(&h, repo.str(), "feature/fork-settle", false).await.unwrap().unwrap();
    assert_match_object(
        &pull_request,
        json!({
            "state": "merged",
            "closedAt": null,
            "mergedAt": null,
            "updatedAt": "2026-05-02T10:00:00.000Z",
        }),
    );
}

// ---------------------------------------------------------------------------------------------
// Failures and the last known PR
// ---------------------------------------------------------------------------------------------

// TS: "status keeps an own-name PR when a later lookup fails on a default upstream"
#[tokio::test]
async fn status_keeps_an_own_name_pr_when_a_later_lookup_fails_on_a_default_upstream() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&repo.path, &["remote", "set-head", "origin", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/sticky-plain", "origin/main"]);
    git(&repo.path, &["push", "origin", "feature/sticky-plain"]);
    let mut scenario = by_head(&[(
        "feature/sticky-plain",
        json!([{
            "number": 90,
            "title": "Sticky own-name PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/90",
            "baseRefName": "main",
            "headRefName": "feature/sticky-plain",
            "state": "OPEN",
            "updatedAt": "2026-05-01T10:00:00Z",
        }]),
    )]);
    scenario.fail_with = Some(GhFailure::Missing);
    scenario.fail_after_calls = 1;
    let h = manager_with(scenario);

    let first = status(&h, repo.str()).await;
    assert_eq!(first["pr"]["number"], json!(90));

    h.manager.invalidate_status(repo.str()).await;
    let second = status(&h, repo.str()).await;
    assert_eq!(second["pr"]["number"], json!(90));
}

// TS: "status prefers open PR when merged PR has newer updatedAt"
#[tokio::test]
async fn status_prefers_open_pr_when_merged_pr_has_newer_updated_at() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/status-open-over-merged"]);
    let h = manager_with(sequence(&[json!([
        {
            "number": 45,
            "title": "Merged PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/45",
            "baseRefName": "main",
            "headRefName": "feature/status-open-over-merged",
            "state": "MERGED",
            "mergedAt": "2026-01-31T10:00:00Z",
            "updatedAt": "2026-02-01T10:00:00Z",
        },
        {
            "number": 46,
            "title": "Open PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/46",
            "baseRefName": "main",
            "headRefName": "feature/status-open-over-merged",
            "state": "OPEN",
            "updatedAt": "2026-01-30T10:00:00Z",
        },
    ])]));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("feature/status-open-over-merged"));
    assert_eq!(
        status["pr"],
        json!({
            "number": 46,
            "title": "Open PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/46",
            "baseRef": "main",
            "headRef": "feature/status-open-over-merged",
            "state": "open",
            "updatedAt": "2026-01-30T10:00:00.000Z",
        })
    );
}

// TS: "status is resilient to gh lookup failures and returns pr null"
#[tokio::test]
async fn status_is_resilient_to_gh_lookup_failures_and_returns_pr_null() {
    let (repo, _remote) = repo_with_pushed_branch("feature/status-no-gh");
    let h = manager_with(unavailable_after(0));

    let status = status(&h, repo.str()).await;
    assert_eq!(status["refName"], json!("feature/status-no-gh"));
    assert_eq!(status["pr"], Value::Null);
}

// TS: "status logs actionable provider detail without exposing the upstream cause"
//
// Rust logs through `tracing` with snake_case field names (`error_tag`, `provider_operation`,
// …) where the TS log annotations are camelCase. gh's stderr has to name the rate limit for
// the CLI to classify the failure (the TS test builds the rate-limit error directly).
#[tokio::test]
async fn status_logs_actionable_provider_detail_without_exposing_the_upstream_cause() {
    let (repo, _remote) = repo_with_pushed_branch("feature/status-rate-limited");
    let upstream_cause = "GraphQL rate limit for user ID 51714798 and token secret-value";
    let h = manager_with(GhScenario {
        fail_with: Some(GhFailure::Exit {
            code: 1,
            stderr: format!("API rate limit exceeded: {upstream_cause}"),
        }),
        ..GhScenario::default()
    });
    let capture = LogCapture::global();
    let status = status(&h, repo.str()).await;

    assert_eq!(status["pr"], Value::Null);
    let logs = capture.events_of_this_thread();
    let (message, fields) = logs
        .iter()
        .find(|(message, _)| message.contains("PR lookup failed"))
        .unwrap_or_else(|| panic!("no PR lookup warning in {logs:?}"));
    let expected = [
        ("operation", "lookupStatusPr"),
        ("branch", "feature/status-rate-limited"),
        ("error_tag", "SourceControlProviderError"),
        ("provider", "github"),
        ("provider_operation", "listChangeRequests"),
        ("provider_command", "gh"),
        (
            "error_detail",
            "GitHub API rate limit exceeded. Run `gh api rate_limit` to inspect the quota and reset time.",
        ),
    ];
    for (key, value) in expected {
        assert_eq!(fields.get(key).map(String::as_str), Some(value), "field {key} of {fields:?}");
    }
    let logged_text = std::iter::once(message.clone()).chain(fields.values().cloned()).collect::<Vec<_>>().join("\n");
    assert!(!logged_text.contains(upstream_cause), "{logged_text}");
    assert!(!logged_text.contains("secret-value"), "{logged_text}");
}

// TS: "status keeps the last known PR when a later lookup fails"
#[tokio::test]
async fn status_keeps_the_last_known_pr_when_a_later_lookup_fails() {
    let (repo, _remote) = repo_with_pushed_branch("feature/pr-sticky");
    let mut scenario = sequence(&[json!([{
        "number": 214,
        "title": "Sticky PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/214",
        "baseRefName": "main",
        "headRefName": "feature/pr-sticky",
    }])]);
    scenario.fail_with = Some(GhFailure::Missing);
    scenario.fail_after_calls = 1;
    let h = manager_with(scenario);

    let first = status(&h, repo.str()).await;
    assert_eq!(first["pr"]["number"], json!(214));

    // An explicit invalidation (user refresh, git action) bypasses the PR cache and forces a
    // live lookup — which now fails. The badge must keep the last known PR instead of blanking
    // out.
    h.manager.invalidate_status(repo.str()).await;
    let second = status(&h, repo.str()).await;
    assert_eq!(second["pr"]["number"], json!(214));
}

// TS: "status does not reuse a stale PR after the branch is retargeted to a different upstream"
#[tokio::test]
async fn status_does_not_reuse_a_stale_pr_after_the_branch_is_retargeted_to_a_different_upstream() {
    let (repo, _origin) = repo_with_pushed_branch("feature/pr-retarget");
    let mut scenario = sequence(&[json!([{
        "number": 214,
        "title": "Sticky PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/214",
        "baseRefName": "main",
        "headRefName": "feature/pr-retarget",
    }])]);
    scenario.fail_with = Some(GhFailure::Missing);
    scenario.fail_after_calls = 1;
    let h = manager_with(scenario);

    let first = status(&h, repo.str()).await;
    assert_eq!(first["pr"]["number"], json!(214));

    // Retarget the branch to a different remote/upstream (e.g. the PR was reopened against a
    // fork). The previously cached PR belonged to the old upstream and must not be shown
    // against the new one.
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork", fork.str()]);
    git(&repo.path, &["push", "fork", "feature/pr-retarget"]);
    git(&repo.path, &["branch", "--set-upstream-to=fork/feature/pr-retarget", "feature/pr-retarget"]);

    h.manager.invalidate_status(repo.str()).await;
    let second = status(&h, repo.str()).await;
    assert_eq!(second["pr"], Value::Null);
}

// TS: "status keeps the last known PR when the branch gains its first upstream"
#[tokio::test]
async fn status_keeps_the_last_known_pr_when_the_branch_gains_its_first_upstream() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/pr-sticky-first-push"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    let mut scenario = sequence(&[json!([{
        "number": 215,
        "title": "Sticky first-push PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/215",
        "baseRefName": "main",
        "headRefName": "feature/pr-sticky-first-push",
    }])]);
    scenario.fail_with = Some(GhFailure::Missing);
    scenario.fail_after_calls = 1;
    let h = manager_with(scenario);

    let first = status(&h, repo.str()).await;
    assert_eq!(first["pr"]["number"], json!(215));

    git(&repo.path, &["push", "-u", "origin", "feature/pr-sticky-first-push"]);
    h.manager.invalidate_status(repo.str()).await;

    let second = status(&h, repo.str()).await;
    assert_eq!(second["pr"]["number"], json!(215));
}

// TS: "status drops the last known PR when the tracked remote is repointed"
#[tokio::test]
async fn status_drops_the_last_known_pr_when_the_tracked_remote_is_repointed() {
    let (repo, _original) = repo_with_pushed_branch("feature/pr-repointed");
    let mut scenario = sequence(&[json!([{
        "number": 216,
        "title": "Old remote PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/216",
        "baseRefName": "main",
        "headRefName": "feature/pr-repointed",
    }])]);
    scenario.fail_with = Some(GhFailure::Missing);
    scenario.fail_after_calls = 1;
    let h = manager_with(scenario);

    let first = status(&h, repo.str()).await;
    assert_eq!(first["pr"]["number"], json!(216));

    let replacement = bare_remote();
    git(&repo.path, &["remote", "add", "replacement", replacement.str()]);
    git(&repo.path, &["push", "replacement", "feature/pr-repointed"]);
    git(&repo.path, &["remote", "set-url", "origin", replacement.str()]);
    h.manager.invalidate_status(repo.str()).await;

    let second = status(&h, repo.str()).await;
    assert_eq!(second["pr"], Value::Null);
}

// TS: "status keeps the last known PR when the current remote URL can't be resolved"
#[tokio::test]
async fn status_keeps_the_last_known_pr_when_the_current_remote_url_cant_be_resolved() {
    let (repo, _remote) = repo_with_pushed_branch("feature/pr-config-hiccup");
    let mut scenario = sequence(&[json!([{
        "number": 217,
        "title": "Config hiccup PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/217",
        "baseRefName": "main",
        "headRefName": "feature/pr-config-hiccup",
    }])]);
    scenario.fail_with = Some(GhFailure::Missing);
    scenario.fail_after_calls = 1;
    let h = manager_with(scenario);

    let first = status(&h, repo.str()).await;
    assert_eq!(first["pr"]["number"], json!(217));

    // `remote.origin.url` reads map any failed read (a real "no remote configured" state or a
    // transient git-config hiccup) to null the same way. Unsetting the key reproduces that
    // ambiguity without touching branch tracking (refs/remotes/origin/* and
    // branch.<b>.remote are untouched): the remote identity has not actually changed, so the
    // sticky PR must survive even though the current lookup can no longer resolve a remote URL
    // to compare against.
    git(&repo.path, &["config", "--unset", "remote.origin.url"]);
    h.manager.invalidate_status(repo.str()).await;

    let second = status(&h, repo.str()).await;
    assert_eq!(second["pr"]["number"], json!(217));
}
