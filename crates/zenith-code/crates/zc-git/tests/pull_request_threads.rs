//! `GitManager.test.ts`, from "resolves pull requests from #number references" to the end:
//! `resolvePullRequest`, every `preparePullRequestThread` case (local and worktree modes, fork
//! upstream tracking, materialization failures, setup scripts, reused and refreshed worktrees,
//! force-pushed heads, dirty worktrees, unrelated branches, main-repo conflicts) and the
//! progress events of commit hooks and `create_pr`.

// The manager's error type, like `src/lib.rs`.
#![allow(clippy::result_large_err)]

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::*;
use serde_json::{json, Value};
use zc_git::types::{GitPreparePullRequestThreadInput, GitPullRequestRefInput};
use zc_vcs::errors::GitManagerServiceError;

/// The fake PR of a scenario: `{number, title, url, baseRefName: "main", headRefName, state:
/// "open"}` plus `extra`.
fn pull_request(number: i64, title: &str, head: &str, extra: Value) -> Value {
    let mut pr = json!({
        "number": number,
        "title": title,
        "url": format!("https://github.com/pingdotgg/codething-mvp/pull/{number}"),
        "baseRefName": "main",
        "headRefName": head,
        "state": "open",
    });
    if let (Value::Object(pr), Value::Object(extra)) = (&mut pr, extra) {
        pr.extend(extra);
    }
    pr
}

/// The cross-repository fields of a fork PR from `octocat/codething-mvp`.
fn octocat_fork() -> Value {
    json!({
        "isCrossRepository": true,
        "headRepositoryNameWithOwner": "octocat/codething-mvp",
        "headRepositoryOwnerLogin": "octocat",
    })
}

fn clone_urls(repository: &str, path: &str) -> HashMap<String, (String, String)> {
    HashMap::from([(repository.to_owned(), (path.to_owned(), path.to_owned()))])
}

fn scenario(pull_request: Value) -> GhScenario {
    GhScenario {
        pull_request: Some(pull_request),
        ..GhScenario::default()
    }
}

fn with_setup(gh: GhScenario, setup: &Arc<RecordingSetupScripts>) -> Harness {
    make_manager(ManagerOptions {
        gh,
        setup_scripts: Some(setup.clone()),
        ..ManagerOptions::default()
    })
}

async fn prepare(h: &Harness, cwd: &str, reference: &str, mode: &str, thread_id: Option<&str>) -> Result<Value, GitManagerServiceError> {
    let mut input = json!({ "cwd": cwd, "reference": reference, "mode": mode });
    if let Some(thread_id) = thread_id {
        input["threadId"] = json!(thread_id);
    }
    let input: GitPreparePullRequestThreadInput = serde_json::from_value(input).unwrap();
    h.manager.prepare_pull_request_thread(&input).await
}

async fn prepare_ok(h: &Harness, cwd: &str, reference: &str, mode: &str, thread_id: Option<&str>) -> Value {
    prepare(h, cwd, reference, mode, thread_id).await.unwrap()
}

fn worktree_path(result: &Value) -> &str {
    result["worktreePath"].as_str().expect("a worktree path")
}

fn realpath(path: impl AsRef<Path>) -> PathBuf {
    std::fs::canonicalize(path).unwrap()
}

fn read(path: impl AsRef<Path>) -> String {
    std::fs::read_to_string(path).unwrap()
}

/// `NodePath.join(repoDir, "..", "<prefix>-<basename(repoDir)>")`, removed with the guard (the
/// TS tests leave these siblings of the temp repository behind).
struct Sibling(PathBuf);

impl Sibling {
    fn new(repo: &Tmp, prefix: &str) -> Self {
        let name = format!("{prefix}-{}", repo.path.file_name().unwrap().to_string_lossy());
        Self(repo.path.parent().unwrap().join(name))
    }

    fn str(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for Sibling {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `git add <file> && git commit -m <message>` after writing `contents` to `file`.
fn commit_file(cwd: impl AsRef<Path>, file: &str, contents: &str, message: &str) {
    let cwd = cwd.as_ref();
    write(cwd, file, contents);
    git(cwd, &["add", file]);
    git(cwd, &["commit", "-m", message]);
}

/// `initRepo` + a bare `origin` with `main` pushed.
fn repo_with_origin() -> (Tmp, Tmp) {
    let repo = repo();
    let origin = bare_remote();
    git(&repo.path, &["remote", "add", "origin", origin.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    (repo, origin)
}

fn setup_calls(setup: &RecordingSetupScripts) -> Vec<(String, String, String)> {
    setup.calls.lock().unwrap().clone()
}

// TS: "resolves pull requests from #number references"
#[tokio::test]
async fn resolves_pull_requests_from_number_references() {
    let repo = repo();
    let h = manager_with(scenario(json!({
        "number": 42,
        "title": "Resolve PR",
        "url": "https://github.com/pingdotgg/codething-mvp/pull/42",
        "baseRefName": "main",
        "headRefName": "feature/resolve-pr",
        "state": "open",
    })));

    let input: GitPullRequestRefInput = serde_json::from_value(json!({ "cwd": repo.str(), "reference": "#42" })).unwrap();
    let result = h.manager.resolve_pull_request(&input).await.unwrap();

    assert_eq!(
        result["pullRequest"],
        json!({
            "number": 42,
            "title": "Resolve PR",
            "url": "https://github.com/pingdotgg/codething-mvp/pull/42",
            "baseBranch": "main",
            "headBranch": "feature/resolve-pr",
            "state": "open",
        })
    );
    let calls = h.gh_calls();
    assert!(calls.iter().any(|call| call.starts_with("pr view 42 ")), "{calls:?}");
}

// TS: "prepares pull request threads in local mode by checking out the PR branch"
#[tokio::test]
async fn prepares_pull_request_threads_in_local_mode_by_checking_out_the_pr_branch() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/pr-local"]);
    commit_file(&repo.path, "local.txt", "local\n", "Local PR branch");

    let h = manager_with(scenario(pull_request(64, "Local PR", "feature/pr-local", json!({}))));

    let result = prepare_ok(&h, repo.str(), "#64", "local", None).await;

    assert_eq!(result["branch"], json!("feature/pr-local"));
    assert_eq!(result["worktreePath"], Value::Null);
    assert_eq!(git(&repo.path, &["branch", "--show-current"]), "feature/pr-local");
    let calls = h.gh_calls();
    assert!(calls.contains(&"pr checkout 64 --force".to_owned()), "{calls:?}");
}

// TS: "restores same-repository upstream tracking after local PR checkout without a remote ref"
#[tokio::test]
async fn restores_same_repository_upstream_tracking_after_local_pr_checkout_without_a_remote_ref() {
    let (repo, remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-local-upstream"]);
    commit_file(&repo.path, "upstream.txt", "upstream\n", "Local upstream PR branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-local-upstream"]);
    git(&repo.path, &["checkout", "main"]);
    git(&repo.path, &["branch", "-D", "feature/pr-local-upstream"]);
    git(&repo.path, &["update-ref", "-d", "refs/remotes/origin/feature/pr-local-upstream"]);

    let h = manager_with(GhScenario {
        pull_request: Some(pull_request(
            65,
            "Local upstream PR",
            "feature/pr-local-upstream",
            json!({
                "isCrossRepository": false,
                "headRepositoryNameWithOwner": "pingdotgg/codething-mvp",
                "headRepositoryOwnerLogin": "pingdotgg",
            }),
        )),
        repository_clone_urls: clone_urls("pingdotgg/codething-mvp", remote.str()),
        ..GhScenario::default()
    });

    let result = prepare_ok(&h, repo.str(), "65", "local", None).await;

    assert_eq!(result["worktreePath"], Value::Null);
    assert_eq!(result["branch"], json!("feature/pr-local-upstream"));
    assert_eq!(
        git(&repo.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feature/pr-local-upstream"
    );
}

// TS: "restores same-repository upstream tracking when provider omits head repository metadata"
#[tokio::test]
async fn restores_same_repository_upstream_tracking_when_provider_omits_head_repository_metadata() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-local-no-head-repo"]);
    commit_file(&repo.path, "no-head-repo.txt", "upstream\n", "Local PR branch without repo metadata");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-local-no-head-repo"]);
    git(&repo.path, &["checkout", "main"]);
    git(&repo.path, &["branch", "-D", "feature/pr-local-no-head-repo"]);
    git(&repo.path, &["update-ref", "-d", "refs/remotes/origin/feature/pr-local-no-head-repo"]);

    let h = manager_with(scenario(pull_request(
        66,
        "Local upstream PR without repo metadata",
        "feature/pr-local-no-head-repo",
        json!({}),
    )));

    let result = prepare_ok(&h, repo.str(), "66", "local", None).await;

    assert_eq!(result["worktreePath"], Value::Null);
    assert_eq!(result["branch"], json!("feature/pr-local-no-head-repo"));
    assert_eq!(
        git(&repo.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "origin/feature/pr-local-no-head-repo"
    );
}

// TS: "prepares pull request threads in worktree mode on the PR head branch"
#[tokio::test]
async fn prepares_pull_request_threads_in_worktree_mode_on_the_pr_head_branch() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-worktree"]);
    commit_file(&repo.path, "worktree.txt", "worktree\n", "PR worktree branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-worktree"]);
    git(&repo.path, &["push", "origin", "HEAD:refs/pull/77/head"]);
    git(&repo.path, &["checkout", "main"]);

    let h = manager_with(scenario(pull_request(77, "Worktree PR", "feature/pr-worktree", json!({}))));

    let result = prepare_ok(&h, repo.str(), "77", "worktree", None).await;

    assert_eq!(result["branch"], json!("feature/pr-worktree"));
    let path = worktree_path(&result);
    assert!(Path::new(path).exists());
    assert_eq!(git(path, &["branch", "--show-current"]), "feature/pr-worktree");
}

// TS: "preserves both branch materialization failures when the fallback also fails"
#[tokio::test]
async fn preserves_both_branch_materialization_failures_when_the_fallback_also_fails() {
    let (repo, _origin) = repo_with_origin();
    let missing_fork = repo.join("missing-fork.git");

    let h = manager_with(GhScenario {
        pull_request: Some(pull_request(93, "Missing fork branch", "feature/missing-fork-branch", octocat_fork())),
        repository_clone_urls: clone_urls("octocat/codething-mvp", &missing_fork),
        ..GhScenario::default()
    });

    let error = prepare(&h, repo.str(), "93", "worktree", None).await.unwrap_err();

    let GitManagerServiceError::Other(error) = error else {
        panic!("expected GitPullRequestMaterializationError, got {error:?}");
    };
    assert_eq!(error.tag, "GitPullRequestMaterializationError", "{error:?}");
    assert_eq!(error.fields["cwd"], json!(repo.str()));
    assert_eq!(error.fields["pullRequestNumber"], json!(93));
    assert_eq!(error.fields["headRepository"], json!("octocat/codething-mvp"));
    assert_eq!(error.fields["headBranch"], json!("feature/missing-fork-branch"));
    assert_eq!(error.fields["localBranch"], json!("t3code/pr-93/feature/missing-fork-branch"));
    let cause = &error.fields["cause"];
    assert_eq!(cause["name"], json!("AggregateError"), "{cause}");
    let errors = cause["errors"].as_array().expect("AggregateError errors");
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0]["_tag"], json!("GitCommandError"));
    assert_eq!(errors[1]["_tag"], json!("GitCommandError"));
    assert_eq!(cause["cause"], errors[0]);
}

// TS: "launches setup when creating a new PR worktree"
#[tokio::test]
async fn launches_setup_when_creating_a_new_pr_worktree() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-worktree-setup"]);
    commit_file(&repo.path, "setup.txt", "setup\n", "PR worktree setup branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-worktree-setup"]);
    git(&repo.path, &["push", "origin", "HEAD:refs/pull/177/head"]);
    git(&repo.path, &["checkout", "main"]);

    let setup = Arc::new(RecordingSetupScripts::default());
    let h = with_setup(scenario(pull_request(177, "Worktree setup PR", "feature/pr-worktree-setup", json!({}))), &setup);

    let result = prepare_ok(&h, repo.str(), "177", "worktree", Some("thread-pr-setup")).await;

    let path = worktree_path(&result);
    assert_eq!(
        setup_calls(&setup),
        vec![("thread-pr-setup".to_owned(), repo.str().to_owned(), path.to_owned())]
    );
}

// TS: "preserves fork upstream tracking when preparing a worktree PR thread"
#[tokio::test]
async fn preserves_fork_upstream_tracking_when_preparing_a_worktree_pr_thread() {
    let (repo, _origin) = repo_with_origin();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["checkout", "-b", "feature/pr-fork"]);
    commit_file(&repo.path, "fork.txt", "fork\n", "Fork PR branch");
    git(&repo.path, &["push", "-u", "fork-seed", "feature/pr-fork"]);
    git(&repo.path, &["checkout", "main"]);

    let h = manager_with(GhScenario {
        pull_request: Some(pull_request(81, "Fork PR", "feature/pr-fork", octocat_fork())),
        repository_clone_urls: clone_urls("octocat/codething-mvp", fork.str()),
        ..GhScenario::default()
    });

    let result = prepare_ok(&h, repo.str(), "81", "worktree", None).await;

    let path = worktree_path(&result);
    let upstream = git(path, &["rev-parse", "--abbrev-ref", "@{upstream}"]);
    assert_eq!(upstream, "fork-seed/feature/pr-fork");
    assert!(!upstream.starts_with("origin/"));
    assert_eq!(git(path, &["config", "--get", "remote.fork-seed.url"]), fork.str());
}

// TS: "preserves fork upstream tracking when preparing a local PR thread"
#[tokio::test]
async fn preserves_fork_upstream_tracking_when_preparing_a_local_pr_thread() {
    let (repo, _origin) = repo_with_origin();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["checkout", "-b", "feature/pr-local-fork"]);
    commit_file(&repo.path, "local-fork.txt", "local fork\n", "Local fork PR branch");
    git(&repo.path, &["push", "-u", "fork-seed", "feature/pr-local-fork"]);
    git(&repo.path, &["checkout", "main"]);
    git(&repo.path, &["branch", "-D", "feature/pr-local-fork"]);

    let h = manager_with(GhScenario {
        pull_request: Some(pull_request(82, "Local Fork PR", "feature/pr-local-fork", octocat_fork())),
        repository_clone_urls: clone_urls("octocat/codething-mvp", fork.str()),
        ..GhScenario::default()
    });

    let result = prepare_ok(&h, repo.str(), "82", "local", None).await;

    assert_eq!(result["worktreePath"], Value::Null);
    assert_eq!(result["branch"], json!("feature/pr-local-fork"));
    assert_eq!(
        git(&repo.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "fork-seed/feature/pr-local-fork"
    );
}

// TS: "derives fork repository identity from PR URL when GitHub omits nameWithOwner"
#[tokio::test]
async fn derives_fork_repository_identity_from_pr_url_when_github_omits_name_with_owner() {
    let (repo, _origin) = repo_with_origin();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "binbandit-seed", fork.str()]);
    git(&repo.path, &["checkout", "-b", "fix/git-action-default-without-origin"]);
    commit_file(&repo.path, "derived-fork.txt", "derived fork\n", "Derived fork PR branch");
    git(&repo.path, &["push", "-u", "binbandit-seed", "fix/git-action-default-without-origin"]);
    git(&repo.path, &["checkout", "main"]);
    git(&repo.path, &["branch", "-D", "fix/git-action-default-without-origin"]);

    let h = manager_with(GhScenario {
        pull_request: Some(json!({
            "number": 642,
            "title": "fix: use commit as the default git action without origin",
            "url": "https://github.com/pingdotgg/t3code/pull/642",
            "baseRefName": "main",
            "headRefName": "fix/git-action-default-without-origin",
            "state": "open",
            "isCrossRepository": true,
            "headRepositoryOwnerLogin": "binbandit",
        })),
        repository_clone_urls: clone_urls("binbandit/t3code", fork.str()),
        ..GhScenario::default()
    });

    let result = prepare_ok(&h, repo.str(), "642", "local", None).await;

    assert_eq!(result["branch"], json!("fix/git-action-default-without-origin"));
    assert_eq!(result["worktreePath"], Value::Null);
    assert_eq!(
        git(&repo.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "binbandit-seed/fix/git-action-default-without-origin"
    );
}

// TS: "reuses an existing dedicated worktree for the PR head branch"
#[tokio::test]
async fn reuses_an_existing_dedicated_worktree_for_the_pr_head_branch() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/pr-existing-worktree"]);
    commit_file(&repo.path, "existing.txt", "existing\n", "Existing worktree branch");
    git(&repo.path, &["checkout", "main"]);
    let worktree = Sibling::new(&repo, "pr-existing");
    git(&repo.path, &["worktree", "add", worktree.str(), "feature/pr-existing-worktree"]);

    let setup = Arc::new(RecordingSetupScripts::default());
    let h = with_setup(
        scenario(pull_request(78, "Existing worktree PR", "feature/pr-existing-worktree", json!({}))),
        &setup,
    );

    let result = prepare_ok(&h, repo.str(), "78", "worktree", Some("thread-pr-existing-worktree")).await;

    assert_eq!(realpath(worktree_path(&result)), realpath(&worktree.0));
    assert_eq!(result["branch"], json!("feature/pr-existing-worktree"));
    // Nothing to fetch from, so the checkout keeps the commit it had and setup stays out of a
    // worktree another thread may be sitting in.
    assert_eq!(setup_calls(&setup).len(), 0);
    assert_eq!(result["isOnPullRequestHead"], json!(false));
}

// TS: "refreshes a reused PR worktree onto the updated pull request head"
#[tokio::test]
async fn refreshes_a_reused_pr_worktree_onto_the_updated_pull_request_head() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-reused-stale"]);
    commit_file(&repo.path, "stale.txt", "stale\n", "Reused stale PR branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-reused-stale"]);
    git(&repo.path, &["checkout", "main"]);
    let worktree = Sibling::new(&repo, "pr-reused-stale");
    git(&repo.path, &["worktree", "add", worktree.str(), "feature/pr-reused-stale"]);

    git(&repo.path, &["checkout", "-b", "author-push", "origin/feature/pr-reused-stale"]);
    commit_file(&repo.path, "authored.txt", "authored\n", "New PR head commit");
    git(&repo.path, &["push", "origin", "author-push:feature/pr-reused-stale"]);
    let updated_head = git(&repo.path, &["rev-parse", "author-push"]);
    git(&repo.path, &["checkout", "main"]);

    let h = manager_with(scenario(pull_request(84, "Reused stale PR", "feature/pr-reused-stale", json!({}))));

    let result = prepare_ok(&h, repo.str(), "84", "worktree", None).await;

    assert_eq!(realpath(worktree_path(&result)), realpath(&worktree.0));
    assert_eq!(result["branch"], json!("feature/pr-reused-stale"));
    assert_eq!(git(&worktree.0, &["rev-parse", "HEAD"]), updated_head);
}

// TS: "runs the setup script when a reused PR worktree moves onto the new head"
#[tokio::test]
async fn runs_the_setup_script_when_a_reused_pr_worktree_moves_onto_the_new_head() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-reused-setup"]);
    commit_file(&repo.path, "reused-setup.txt", "reused setup\n", "Reused setup PR branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-reused-setup"]);
    git(&repo.path, &["checkout", "main"]);
    let worktree = Sibling::new(&repo, "pr-reused-setup");
    git(&repo.path, &["worktree", "add", worktree.str(), "feature/pr-reused-setup"]);

    git(&repo.path, &["checkout", "-b", "setup-author-push", "feature/pr-reused-setup"]);
    commit_file(&repo.path, "reused-setup.txt", "reused setup again\n", "New reused setup head");
    git(&repo.path, &["push", "origin", "setup-author-push:feature/pr-reused-setup"]);
    git(&repo.path, &["checkout", "main"]);

    let setup = Arc::new(RecordingSetupScripts::default());
    let h = with_setup(scenario(pull_request(85, "Reused setup PR", "feature/pr-reused-setup", json!({}))), &setup);

    let result = prepare_ok(&h, repo.str(), "85", "worktree", Some("thread-pr-reused-setup")).await;

    assert_eq!(
        setup_calls(&setup),
        vec![("thread-pr-reused-setup".to_owned(), repo.str().to_owned(), worktree_path(&result).to_owned())]
    );
    assert_eq!(result["isOnPullRequestHead"], json!(true));
}

// TS: "leaves the setup script alone when a reused PR worktree is already on the head"
#[tokio::test]
async fn leaves_the_setup_script_alone_when_a_reused_pr_worktree_is_already_on_the_head() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-reused-current"]);
    commit_file(&repo.path, "reused-current.txt", "reused current\n", "Reused current PR branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-reused-current"]);
    git(&repo.path, &["checkout", "main"]);
    let worktree = Sibling::new(&repo, "pr-reused-current");
    git(&repo.path, &["worktree", "add", worktree.str(), "feature/pr-reused-current"]);
    let current_head = git(&worktree.0, &["rev-parse", "HEAD"]);

    let setup = Arc::new(RecordingSetupScripts::default());
    let h = with_setup(scenario(pull_request(95, "Reused current PR", "feature/pr-reused-current", json!({}))), &setup);

    let result = prepare_ok(&h, repo.str(), "95", "worktree", Some("thread-pr-reused-current")).await;

    assert_eq!(result["isOnPullRequestHead"], json!(true));
    assert_eq!(git(&worktree.0, &["rev-parse", "HEAD"]), current_head);
    assert_eq!(setup_calls(&setup).len(), 0);
}

// TS: "resets a clean reused PR worktree onto a force-pushed pull request head"
#[tokio::test]
async fn resets_a_clean_reused_pr_worktree_onto_a_force_pushed_pull_request_head() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-force-pushed"]);
    commit_file(&repo.path, "force-pushed.txt", "first\n", "Force-pushed PR branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-force-pushed"]);
    git(&repo.path, &["checkout", "main"]);
    let worktree = Sibling::new(&repo, "pr-force-pushed");
    git(&repo.path, &["worktree", "add", worktree.str(), "feature/pr-force-pushed"]);
    let stale_head = git(&worktree.0, &["rev-parse", "HEAD"]);

    git(&repo.path, &["checkout", "-b", "author-rewrite", "feature/pr-force-pushed"]);
    write(&repo.path, "force-pushed.txt", "rewritten\n");
    git(&repo.path, &["add", "force-pushed.txt"]);
    git(&repo.path, &["commit", "--amend", "-m", "Rewritten PR head"]);
    git(&repo.path, &["push", "--force", "origin", "author-rewrite:feature/pr-force-pushed"]);
    let rewritten_head = git(&repo.path, &["rev-parse", "author-rewrite"]);
    // Pushing from this clone also advanced its remote-tracking ref. A head rewritten by the
    // author leaves that ref behind, which is the state a reused worktree is really opened in.
    git(&repo.path, &["update-ref", "refs/remotes/origin/feature/pr-force-pushed", &stale_head]);
    git(&repo.path, &["checkout", "main"]);

    let h = manager_with(scenario(pull_request(86, "Force-pushed PR", "feature/pr-force-pushed", json!({}))));

    let result = prepare_ok(&h, repo.str(), "86", "worktree", None).await;

    assert_eq!(realpath(worktree_path(&result)), realpath(&worktree.0));
    assert_eq!(result["isOnPullRequestHead"], json!(true));
    assert_eq!(git(&worktree.0, &["rev-parse", "HEAD"]), rewritten_head);
    assert_eq!(read(worktree.0.join("force-pushed.txt")), "rewritten\n");
}

// TS: "keeps a reused PR worktree that carries its own commit off the rewritten head"
#[tokio::test]
async fn keeps_a_reused_pr_worktree_that_carries_its_own_commit_off_the_rewritten_head() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-local-commit"]);
    commit_file(&repo.path, "local-commit.txt", "first\n", "Local commit PR branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-local-commit"]);
    git(&repo.path, &["checkout", "main"]);
    let worktree = Sibling::new(&repo, "pr-local-commit");
    git(&repo.path, &["worktree", "add", worktree.str(), "feature/pr-local-commit"]);
    let upstream_head = git(&worktree.0, &["rev-parse", "HEAD"]);

    git(&repo.path, &["checkout", "-b", "local-commit-rewrite", "feature/pr-local-commit"]);
    write(&repo.path, "local-commit.txt", "rewritten\n");
    git(&repo.path, &["add", "local-commit.txt"]);
    git(&repo.path, &["commit", "--amend", "-m", "Rewritten local commit head"]);
    git(&repo.path, &["push", "--force", "origin", "local-commit-rewrite:feature/pr-local-commit"]);
    git(&repo.path, &["update-ref", "refs/remotes/origin/feature/pr-local-commit", &upstream_head]);
    git(&repo.path, &["checkout", "main"]);

    // The work that must survive: a commit made in the worktree, on top of the stale head.
    commit_file(&worktree.0, "thread-work.txt", "thread work\n", "Work done in the reused worktree");
    let worktree_head = git(&worktree.0, &["rev-parse", "HEAD"]);

    let setup = Arc::new(RecordingSetupScripts::default());
    let h = with_setup(scenario(pull_request(87, "Local commit PR", "feature/pr-local-commit", json!({}))), &setup);

    let result = prepare_ok(&h, repo.str(), "87", "worktree", Some("thread-pr-local-commit")).await;

    assert_eq!(result["isOnPullRequestHead"], json!(false));
    assert_eq!(git(&worktree.0, &["rev-parse", "HEAD"]), worktree_head);
    assert!(worktree.0.join("thread-work.txt").exists());
    assert_eq!(setup_calls(&setup).len(), 0);
}

// TS: "keeps a dirty reused PR worktree off the rewritten pull request head"
#[tokio::test]
async fn keeps_a_dirty_reused_pr_worktree_off_the_rewritten_pull_request_head() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-dirty-worktree"]);
    commit_file(&repo.path, "dirty.txt", "first\n", "Dirty worktree PR branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-dirty-worktree"]);
    git(&repo.path, &["checkout", "main"]);
    let worktree = Sibling::new(&repo, "pr-dirty-worktree");
    git(&repo.path, &["worktree", "add", worktree.str(), "feature/pr-dirty-worktree"]);
    let stale_head = git(&worktree.0, &["rev-parse", "HEAD"]);

    git(&repo.path, &["checkout", "-b", "dirty-rewrite", "feature/pr-dirty-worktree"]);
    write(&repo.path, "dirty.txt", "rewritten\n");
    git(&repo.path, &["add", "dirty.txt"]);
    git(&repo.path, &["commit", "--amend", "-m", "Rewritten dirty head"]);
    git(&repo.path, &["push", "--force", "origin", "dirty-rewrite:feature/pr-dirty-worktree"]);
    git(&repo.path, &["update-ref", "refs/remotes/origin/feature/pr-dirty-worktree", &stale_head]);
    git(&repo.path, &["checkout", "main"]);

    write(&worktree.0, "dirty.txt", "uncommitted edit\n");

    let setup = Arc::new(RecordingSetupScripts::default());
    let h = with_setup(scenario(pull_request(89, "Dirty worktree PR", "feature/pr-dirty-worktree", json!({}))), &setup);

    let result = prepare_ok(&h, repo.str(), "89", "worktree", Some("thread-pr-dirty-worktree")).await;

    assert_eq!(result["isOnPullRequestHead"], json!(false));
    assert_eq!(git(&worktree.0, &["rev-parse", "HEAD"]), stale_head);
    assert_eq!(read(worktree.0.join("dirty.txt")), "uncommitted edit\n");
    assert_eq!(setup_calls(&setup).len(), 0);
}

// TS: "refreshes a reused PR worktree that has no upstream from the pull request ref"
#[tokio::test]
async fn refreshes_a_reused_pr_worktree_that_has_no_upstream_from_the_pull_request_ref() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-ref-only"]);
    commit_file(&repo.path, "ref-only.txt", "ref only\n", "Pull ref only PR branch");
    // The head lives at refs/pull/90/head and nowhere else, so nothing can be tracked.
    git(&repo.path, &["push", "origin", "HEAD:refs/pull/90/head"]);
    git(&repo.path, &["checkout", "main"]);
    git(&repo.path, &["branch", "-D", "feature/pr-ref-only"]);

    let h = manager_with(scenario(pull_request(90, "Pull ref only PR", "feature/pr-ref-only", json!({}))));

    let created = prepare_ok(&h, repo.str(), "90", "worktree", None).await;
    let path = worktree_path(&created).to_owned();
    let (has_upstream, _) = git_try(&path, &["rev-parse", "--abbrev-ref", "@{upstream}"]);
    assert!(!has_upstream);

    git(&repo.path, &["fetch", "origin", "refs/pull/90/head"]);
    git(&repo.path, &["checkout", "-b", "ref-only-author", "FETCH_HEAD"]);
    commit_file(&repo.path, "ref-only.txt", "ref only again\n", "New pull ref head");
    git(&repo.path, &["push", "origin", "ref-only-author:refs/pull/90/head"]);
    let updated_head = git(&repo.path, &["rev-parse", "ref-only-author"]);
    git(&repo.path, &["checkout", "main"]);

    let result = prepare_ok(&h, repo.str(), "90", "worktree", None).await;

    assert_eq!(realpath(worktree_path(&result)), realpath(&path));
    assert_eq!(result["isOnPullRequestHead"], json!(true));
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), updated_head);
}

// TS: "never moves an unrelated local branch that shares the fork head branch name"
#[tokio::test]
async fn never_moves_an_unrelated_local_branch_that_shares_the_fork_head_branch_name() {
    let (repo, _origin) = repo_with_origin();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["checkout", "-b", "fork-main-collision"]);
    commit_file(&repo.path, "contributor.txt", "contributor\n", "Contributor commit on the fork main");
    git(&repo.path, &["push", "-u", "fork-seed", "fork-main-collision:main"]);
    // The user's own main, checked out in its own worktree and behind the fork's main: a
    // fast-forward would land the contributor's commits in it.
    git(&repo.path, &["checkout", "-b", "feature/root-work", "main"]);
    let main_worktree = Sibling::new(&repo, "local-main");
    git(&repo.path, &["worktree", "add", main_worktree.str(), "main"]);
    let local_main_before = git(&repo.path, &["rev-parse", "main"]);

    let setup = Arc::new(RecordingSetupScripts::default());
    let h = with_setup(
        GhScenario {
            pull_request: Some(pull_request(94, "Fork main collision PR", "main", octocat_fork())),
            repository_clone_urls: clone_urls("octocat/codething-mvp", fork.str()),
            ..GhScenario::default()
        },
        &setup,
    );

    let result = prepare_ok(&h, repo.str(), "94", "worktree", Some("thread-pr-fork-main-collision")).await;

    assert_eq!(git(&repo.path, &["rev-parse", "main"]), local_main_before);
    assert_eq!(git(&main_worktree.0, &["rev-parse", "HEAD"]), local_main_before);
    assert!(!main_worktree.0.join("contributor.txt").exists());
    assert_eq!(result["isOnPullRequestHead"], json!(false));
    assert_eq!(setup_calls(&setup).len(), 0);
}

// TS: "does not block fork PR worktree prep when the fork head branch collides with root main"
#[tokio::test]
async fn does_not_block_fork_pr_worktree_prep_when_the_fork_head_branch_collides_with_root_main() {
    let (repo, _origin) = repo_with_origin();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["checkout", "-b", "fork-main-source"]);
    commit_file(&repo.path, "fork-main.txt", "fork main\n", "Fork main branch");
    git(&repo.path, &["push", "-u", "fork-seed", "fork-main-source:main"]);
    git(&repo.path, &["checkout", "main"]);
    let main_before = git(&repo.path, &["rev-parse", "main"]);

    let h = manager_with(GhScenario {
        pull_request: Some(pull_request(91, "Fork main PR", "main", octocat_fork())),
        repository_clone_urls: clone_urls("octocat/codething-mvp", fork.str()),
        ..GhScenario::default()
    });

    let result = prepare_ok(&h, repo.str(), "91", "worktree", None).await;

    assert_eq!(result["branch"], json!("t3code/pr-91/main"));
    let path = worktree_path(&result);
    assert_eq!(git(&repo.path, &["branch", "--show-current"]), "main");
    assert_eq!(git(&repo.path, &["rev-parse", "main"]), main_before);
    assert_eq!(git(path, &["branch", "--show-current"]), "t3code/pr-91/main");
}

// TS: "does not overwrite an existing local main branch when preparing a fork PR worktree"
#[tokio::test]
async fn does_not_overwrite_an_existing_local_main_branch_when_preparing_a_fork_pr_worktree() {
    let (repo, _origin) = repo_with_origin();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["checkout", "-b", "fork-main-source"]);
    commit_file(&repo.path, "fork-main-second.txt", "fork main second\n", "Fork main second branch");
    git(&repo.path, &["push", "-u", "fork-seed", "fork-main-source:main"]);
    git(&repo.path, &["checkout", "main"]);
    let local_main_before = git(&repo.path, &["rev-parse", "main"]);
    git(&repo.path, &["checkout", "-b", "feature/root-branch"]);

    let h = manager_with(GhScenario {
        pull_request: Some(pull_request(92, "Fork main overwrite PR", "main", octocat_fork())),
        repository_clone_urls: clone_urls("octocat/codething-mvp", fork.str()),
        ..GhScenario::default()
    });

    let result = prepare_ok(&h, repo.str(), "92", "worktree", None).await;

    assert_eq!(result["branch"], json!("t3code/pr-92/main"));
    assert_eq!(git(&repo.path, &["rev-parse", "main"]), local_main_before);
    assert_eq!(git(worktree_path(&result), &["rev-parse", "--abbrev-ref", "@{upstream}"]), "fork-seed/main");
}

// TS: "reuses an existing PR worktree and restores fork upstream tracking"
#[tokio::test]
async fn reuses_an_existing_pr_worktree_and_restores_fork_upstream_tracking() {
    let (repo, _origin) = repo_with_origin();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["checkout", "-b", "feature/pr-reused-fork"]);
    commit_file(&repo.path, "reused-fork.txt", "reused fork\n", "Reused fork PR branch");
    git(&repo.path, &["push", "-u", "fork-seed", "feature/pr-reused-fork"]);
    git(&repo.path, &["checkout", "main"]);
    let worktree = Sibling::new(&repo, "pr-reused-fork");
    git(&repo.path, &["worktree", "add", worktree.str(), "feature/pr-reused-fork"]);
    let _ = git_try(&worktree.0, &["branch", "--unset-upstream"]);

    let h = manager_with(GhScenario {
        pull_request: Some(pull_request(83, "Reused Fork PR", "feature/pr-reused-fork", octocat_fork())),
        repository_clone_urls: clone_urls("octocat/codething-mvp", fork.str()),
        ..GhScenario::default()
    });

    let result = prepare_ok(&h, repo.str(), "83", "worktree", None).await;

    assert_eq!(realpath(worktree_path(&result)), realpath(&worktree.0));
    assert_eq!(
        git(&worktree.0, &["rev-parse", "--abbrev-ref", "@{upstream}"]),
        "fork-seed/feature/pr-reused-fork"
    );
}

// TS: "does not fail PR worktree prep when setup terminal startup fails"
#[tokio::test]
async fn does_not_fail_pr_worktree_prep_when_setup_terminal_startup_fails() {
    let (repo, _remote) = repo_with_origin();
    git(&repo.path, &["checkout", "-b", "feature/pr-setup-failure"]);
    commit_file(&repo.path, "setup-failure.txt", "setup failure\n", "PR setup failure branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-setup-failure"]);
    git(&repo.path, &["push", "origin", "HEAD:refs/pull/184/head"]);
    git(&repo.path, &["checkout", "main"]);

    let setup = Arc::new(RecordingSetupScripts {
        fail: true,
        ..RecordingSetupScripts::default()
    });
    let h = with_setup(scenario(pull_request(184, "Setup failure PR", "feature/pr-setup-failure", json!({}))), &setup);

    let result = prepare_ok(&h, repo.str(), "184", "worktree", Some("thread-pr-setup-failure")).await;

    assert_eq!(result["branch"], json!("feature/pr-setup-failure"));
    assert!(Path::new(worktree_path(&result)).exists());
}

// TS: "rejects worktree prep when the PR head branch is checked out in the main repo"
#[tokio::test]
async fn rejects_worktree_prep_when_the_pr_head_branch_is_checked_out_in_the_main_repo() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/pr-root-only"]);

    let h = manager_with(scenario(pull_request(79, "Root-only PR", "feature/pr-root-only", json!({}))));

    let error = prepare(&h, repo.str(), "79", "worktree", None).await.unwrap_err();

    assert!(error.message().contains("already checked out in the main repo"), "{}", error.message());
}

// TS: "emits ordered progress events for commit hooks"
#[tokio::test]
async fn emits_ordered_progress_events_for_commit_hooks() {
    let repo = repo();
    write(&repo.path, "hooked.txt", "hooked\n");
    write_hook(
        &repo.path,
        "pre-commit",
        "#!/bin/sh\necho \"hook: start\" >&2\nsleep 0.05\necho \"hook: end\" >&2\n",
    );

    let h = make_manager(ManagerOptions::default());
    let (options, events) = recorder();

    let result = h.manager.run_stacked_action(action_input(repo.str(), "commit"), options).await.unwrap();

    assert_eq!(result.commit.status, "created");
    let events = events.lock().unwrap().clone();
    assert!(events.iter().any(|e| e["kind"] == "action_started"), "{events:#?}");
    for expected in [
        json!({"kind": "phase_started", "phase": "commit"}),
        json!({"kind": "hook_started", "hookName": "pre-commit"}),
        json!({"kind": "hook_output", "text": "hook: start"}),
        json!({"kind": "hook_output", "text": "hook: end"}),
        json!({"kind": "hook_finished", "hookName": "pre-commit"}),
        json!({"kind": "action_finished"}),
    ] {
        assert!(events.iter().any(|e| contains(e, &expected)), "missing {expected} in {events:#?}");
    }
}

// TS: "emits action_failed when a commit hook rejects"
#[tokio::test]
async fn emits_action_failed_when_a_commit_hook_rejects() {
    let repo = repo();
    write(&repo.path, "hook-failure.txt", "broken\n");
    write_hook(&repo.path, "pre-commit", "#!/bin/sh\necho \"hook: fail\" >&2\nexit 1\n");

    let h = make_manager(ManagerOptions::default());
    let (options, events) = recorder();

    let error = h.manager.run_stacked_action(action_input(repo.str(), "commit"), options).await.unwrap_err();

    let message = error.message();
    assert!(message.contains("Git command failed in GitVcsDriver.commit.commit"), "{message}");
    assert!(!message.contains("hook: fail"), "{message}");
    let events = events.lock().unwrap().clone();
    for expected in [
        json!({"kind": "hook_started", "hookName": "pre-commit"}),
        json!({"kind": "hook_output", "text": "hook: fail"}),
        json!({"kind": "action_failed", "phase": "commit"}),
    ] {
        assert!(events.iter().any(|e| contains(e, &expected)), "missing {expected} in {events:#?}");
    }
}

// TS: "create_pr emits only the PR phase when the branch is already pushed"
#[tokio::test]
async fn create_pr_emits_only_the_pr_phase_when_the_branch_is_already_pushed() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/pr-only-follow-up"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    commit_file(&repo.path, "pr-only.txt", "pr only\n", "PR only branch");
    git(&repo.path, &["push", "-u", "origin", "feature/pr-only-follow-up"]);

    let h = manager_with(GhScenario {
        pr_list_sequence: vec![
            json!([]).to_string(),
            json!([{
                "number": 201,
                "title": "PR only branch",
                "url": "https://github.com/pingdotgg/codething-mvp/pull/201",
                "baseRefName": "main",
                "headRefName": "feature/pr-only-follow-up",
                "state": "OPEN",
                "isCrossRepository": false,
            }])
            .to_string(),
        ],
        ..GhScenario::default()
    });
    let (options, events) = recorder();

    let result = h.manager.run_stacked_action(action_input(repo.str(), "create_pr"), options).await.unwrap();

    assert_eq!(result.commit.status, "skipped_not_requested");
    assert_eq!(result.push.status, "skipped_not_requested");
    assert_eq!(result.pr.status, "created");
    let phases: Vec<Value> = events.lock().unwrap().iter().filter(|e| e["kind"] == "phase_started").cloned().collect();
    let expected = [
        json!({"kind": "phase_started", "phase": "pr", "label": "Preparing PR..."}),
        json!({"kind": "phase_started", "phase": "pr", "label": "Generating PR content..."}),
        json!({"kind": "phase_started", "phase": "pr", "label": "Creating pull request..."}),
    ];
    assert_eq!(phases.len(), expected.len(), "{phases:#?}");
    for (actual, expected) in phases.iter().zip(expected.iter()) {
        assert!(contains(actual, expected), "{actual} does not match {expected}");
    }
}

/// `expect.objectContaining(expected)`: every key of `expected` is in `actual` with that value.
fn contains(actual: &Value, expected: &Value) -> bool {
    expected.as_object().unwrap().iter().all(|(key, value)| actual.get(key) == Some(value))
}

/// An executable `.git/hooks/<name>` (mode 0o755).
fn write_hook(repo: &Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = repo.join(".git").join("hooks").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}
