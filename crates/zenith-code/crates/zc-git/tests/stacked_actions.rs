//! `GitManager.test.ts`, the stacked actions: commit (writer model, writing style, custom
//! messages, selected files), feature branches, push with upstream setup, `create_pr` base
//! branch resolution, existing PR matching (cross-repo, forks, slash remotes, Forgejo heads),
//! PR creation with templates, and the errors (detached HEAD, missing gh, gh auth).

#![allow(clippy::result_large_err)]

mod common;

use common::*;
use serde_json::{json, Value};
use zc_contracts::ChangeRequestState;
use zc_git::helpers::{matches_branch_head_context, parse_repository_name_with_owner_from_remote_url};
use zc_git::types::{BranchHeadContext, GitRunStackedActionInput, GitRunStackedActionResult, PullRequestInfo};
use zc_ports::git::GitRunStackedActionOptions;
use zc_ports::text_generation::{CommitMessageGenerationResult, PrContentGenerationResult};
use zc_vcs::GitManagerServiceError;

async fn run(h: &Harness, input: GitRunStackedActionInput) -> Result<GitRunStackedActionResult, GitManagerServiceError> {
    h.manager.run_stacked_action(input, GitRunStackedActionOptions::default()).await
}

fn input(cwd: &Tmp, action: &str) -> GitRunStackedActionInput {
    action_input(cwd.str(), action)
}

fn toast_json(result: &GitRunStackedActionResult) -> Value {
    serde_json::to_value(&result.toast).unwrap()
}

fn commit_returning(subject: &'static str) -> FakeTextGeneration {
    FakeTextGeneration {
        commit: Box::new(move |_| {
            Ok(CommitMessageGenerationResult {
                subject: subject.into(),
                body: String::new(),
                branch: None,
            })
        }),
        ..FakeTextGeneration::default()
    }
}

fn pr_entry(number: i64, title: &str, url: &str, base: &str, head: &str) -> Value {
    json!({ "number": number, "title": title, "url": url, "baseRefName": base, "headRefName": head })
}

fn fork_pr_entry(number: i64, title: &str, head: &str) -> Value {
    json!({
        "number": number,
        "title": title,
        "url": format!("https://github.com/pingdotgg/codething-mvp/pull/{number}"),
        "baseRefName": "main",
        "headRefName": head,
        "state": "OPEN",
        "isCrossRepository": true,
        "headRepository": { "nameWithOwner": "octocat/codething-mvp" },
        "headRepositoryOwner": { "login": "octocat" },
    })
}

fn list(entries: &[Value]) -> String {
    Value::Array(entries.to_vec()).to_string()
}

fn has_call(h: &Harness, needle: &str) -> bool {
    h.gh_calls().iter().any(|call| call.contains(needle))
}

fn head_context(head_branch: &str, repository: Option<&str>, owner: Option<&str>, cross: bool) -> BranchHeadContext {
    BranchHeadContext {
        head_branch: head_branch.into(),
        head_repository_name_with_owner: repository.map(str::to_owned),
        head_repository_owner_login: owner.map(str::to_owned),
        is_cross_repository: cross,
        ..BranchHeadContext::default()
    }
}

fn pr_info(number: i64, title: &str, url: &str, head: &str, cross: bool, repository: &str, owner: &str) -> PullRequestInfo {
    PullRequestInfo {
        number,
        title: title.into(),
        url: url.into(),
        base_ref_name: "main".into(),
        head_ref_name: head.into(),
        state: ChangeRequestState::Open,
        is_draft: false,
        closed_at: None,
        merged_at: None,
        updated_at: None,
        is_cross_repository: Some(cross),
        head_repository_name_with_owner: Some(repository.into()),
        head_repository_owner_login: Some(owner.into()),
    }
}

/// The fork checkout of the fork tests: `statemachine` pushed to `fork-seed` (a GitHub URL
/// rewritten to a local bare repository), optionally checked out as `t3code/pr-<n>/statemachine`.
fn fork_checkout(local_alias: Option<&str>) -> (Tmp, Tmp) {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "statemachine"]);
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["push", "-u", "fork-seed", "statemachine"]);
    if let Some(alias) = local_alias {
        git(&repo.path, &["checkout", "-b", alias]);
        git(&repo.path, &["branch", "--set-upstream-to", "fork-seed/statemachine"]);
    }
    configure_visible_remote(&repo.path, "fork-seed", "git@github.com:octocat/codething-mvp.git", fork.str());
    (repo, fork)
}

// TS: "creates a commit when working tree is dirty"
#[tokio::test]
async fn creates_a_commit_when_working_tree_is_dirty() {
    let repo = repo();
    write(&repo.path, "README.md", "hello\nworld\n");
    let h = make_manager(ManagerOptions {
        settings: json!({"sourceControlWritingStyle": {"mode": "custom", "customInstructions": "Use a direct tone."}}),
        text_generation: commit_returning("Implement stacked git actions"),
        ..ManagerOptions::default()
    });
    let result = run(&h, input(&repo, "commit")).await.unwrap();

    assert_eq!(result.branch.status, "skipped_not_requested");
    assert_eq!(result.commit.status, "created");
    assert_eq!(result.push.status, "skipped_not_requested");
    assert_eq!(result.pr.status, "skipped_not_requested");
    let policy = h.text.commit_inputs.lock().unwrap()[0].policy.clone().unwrap();
    assert_eq!(policy.commit_instructions.as_deref(), Some("Use a direct tone."));
    let toast = toast_json(&result);
    assert_eq!(toast["description"], json!("Implement stacked git actions"));
    assert_eq!(toast["cta"], json!({"kind": "run_action", "label": "Push", "action": {"kind": "push"}}));
    let title = toast["title"].as_str().unwrap();
    assert!(regex::Regex::new(r"^Committed [0-9a-f]{7}$").unwrap().is_match(title), "{title}");
    assert_eq!(git(&repo.path, &["log", "-1", "--pretty=%s"]), "Implement stacked git actions");
}

// TS: "preserves custom style when instructions are empty"
#[tokio::test]
async fn preserves_custom_style_when_instructions_are_empty() {
    let repo = repo();
    write(&repo.path, "README.md", "hello\nworld\n");
    let h = make_manager(ManagerOptions {
        settings: json!({"sourceControlWritingStyle": {"mode": "custom", "customInstructions": ""}}),
        text_generation: commit_returning("Preserve custom style"),
        ..ManagerOptions::default()
    });
    run(&h, input(&repo, "commit")).await.unwrap();

    let policy = h.text.commit_inputs.lock().unwrap()[0].policy.clone();
    assert_eq!(
        serde_json::to_value(policy).unwrap(),
        json!({"kind": "custom", "inferRepositoryConventions": false})
    );
}

// TS: "falls back when the dedicated source control writer is unavailable"
#[tokio::test]
async fn falls_back_when_the_dedicated_source_control_writer_is_unavailable() {
    let repo = repo();
    write(&repo.path, "README.md", "hello\nworld\n");
    let h = make_manager(ManagerOptions {
        settings: json!({
            "providerInstances": {"missing_writer": {"driver": "missing-driver", "config": {}}},
            "sourceControlWriterModelSelection": {"instanceId": "missing_writer", "model": "missing-model"},
        }),
        text_generation: commit_returning("Use the available writer"),
        ..ManagerOptions::default()
    });
    run(&h, input(&repo, "commit")).await.unwrap();

    let default_selection = zc_settings::settings::test_settings(&json!({}))["textGenerationModelSelection"].clone();
    assert!(!default_selection.is_null());
    let model_selection = h.text.commit_inputs.lock().unwrap()[0].model_selection.0.clone();
    assert_eq!(model_selection, default_selection);
}

// TS: "includes local agent instructions when recent history is empty"
#[tokio::test]
async fn includes_local_agent_instructions_when_recent_history_is_empty() {
    let repo = Tmp::new("t3code-git-manager-");
    git(&repo.path, &["init", "--initial-branch=main"]);
    git(&repo.path, &["config", "user.email", "test@example.com"]);
    git(&repo.path, &["config", "user.name", "Test User"]);
    let agent_instructions = "Use lowercase source control text.";
    let claude_instructions = "Keep pull request bodies brief.";
    write(&repo.path, "AGENTS.md", agent_instructions);
    write(&repo.path, "CLAUDE.md", claude_instructions);
    write(&repo.path, "README.md", "hello\n");
    git(&repo.path, &["add", "README.md"]);
    let h = make_manager(ManagerOptions {
        settings: json!({
            "textGenerationModelSelection": {"instanceId": "claudeAgent", "model": "claude-sonnet-4-6"},
            "sourceControlWritingStyle": {"mode": "repo_conventions"},
        }),
        text_generation: commit_returning("Create initial commit"),
        ..ManagerOptions::default()
    });
    run(&h, input(&repo, "commit")).await.unwrap();

    let policy = h.text.commit_inputs.lock().unwrap()[0].policy.clone();
    assert_eq!(
        serde_json::to_value(policy).unwrap(),
        json!({
            "kind": "repo_conventions",
            "commitInstructions": format!("Follow the repository's established commit message style when examples are available.\n\nLocal AGENTS.md:\n{agent_instructions}\n\nLocal CLAUDE.md:\n{claude_instructions}"),
            "changeRequestInstructions": format!("Follow the repository's established change request title and body style when examples are available.\n\nLocal AGENTS.md:\n{agent_instructions}\n\nLocal CLAUDE.md:\n{claude_instructions}"),
            "inferRepositoryConventions": true,
        })
    );
}

/// A text generation counting its commit messages, answering `subject` (and `branch` when
/// asked for one).
fn counting_commit(subject: &'static str, branch: &'static str) -> FakeTextGeneration {
    FakeTextGeneration {
        commit: Box::new(move |input| {
            Ok(CommitMessageGenerationResult {
                subject: subject.into(),
                body: String::new(),
                branch: input.include_branch.then(|| branch.into()),
            })
        }),
        ..FakeTextGeneration::default()
    }
}

// TS: "uses custom commit message when provided"
#[tokio::test]
async fn uses_custom_commit_message_when_provided() {
    let repo = repo();
    write(&repo.path, "README.md", "hello\ncustom\n");
    let h = make_manager(ManagerOptions {
        text_generation: counting_commit("this should not be used", "feature/unused"),
        ..ManagerOptions::default()
    });
    let mut action = input(&repo, "commit");
    action.commit_message = Some("feat: custom summary line\n\n- details from user".into());
    let result = run(&h, action).await.unwrap();

    assert_eq!(result.branch.status, "skipped_not_requested");
    assert_eq!(result.commit.status, "created");
    assert_eq!(result.commit.subject.as_deref(), Some("feat: custom summary line"));
    assert_eq!(h.text.commit_inputs.lock().unwrap().len(), 0);
    assert_eq!(git(&repo.path, &["log", "-1", "--pretty=%s"]), "feat: custom summary line");
    assert!(git(&repo.path, &["log", "-1", "--pretty=%b"]).contains("- details from user"));
}

// TS: "commits only selected files when filePaths is provided"
#[tokio::test]
async fn commits_only_selected_files_when_file_paths_is_provided() {
    let repo = repo();
    write(&repo.path, "a.txt", "file a\n");
    write(&repo.path, "b.txt", "file b\n");
    let h = make_manager(ManagerOptions::default());
    let mut action = input(&repo, "commit");
    action.file_paths = Some(vec!["a.txt".into()]);
    let result = run(&h, action).await.unwrap();

    assert_eq!(result.commit.status, "created");
    // b.txt should remain in the working tree
    let status = git(&repo.path, &["status", "--porcelain"]);
    assert!(status.contains("b.txt"), "{status}");
    assert!(!status.contains("a.txt"), "{status}");
}

// TS: "creates feature branch, commits, and pushes with featureBranch option"
#[tokio::test]
async fn creates_feature_branch_commits_and_pushes_with_feature_branch_option() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    write(&repo.path, "README.md", "hello\nfeature-branch\n");
    let h = make_manager(ManagerOptions {
        text_generation: counting_commit("Implement stacked git actions", "feature/implement-stacked-git-actions"),
        ..ManagerOptions::default()
    });
    let mut action = input(&repo, "commit_push");
    action.feature_branch = Some(true);
    let result = run(&h, action).await.unwrap();

    assert_eq!(result.branch.status, "created");
    assert_eq!(result.branch.name.as_deref(), Some("feature/implement-stacked-git-actions"));
    assert_eq!(result.commit.status, "created");
    assert_eq!(result.push.status, "pushed");
    let toast = toast_json(&result);
    assert_eq!(toast["description"], json!("Implement stacked git actions"));
    assert_eq!(
        toast["cta"],
        json!({"kind": "run_action", "label": "Create PR", "action": {"kind": "create_pr"}})
    );
    let title = toast["title"].as_str().unwrap();
    assert!(
        regex::Regex::new(r"^Pushed [0-9a-f]{7} to origin/feature/implement-stacked-git-actions$")
            .unwrap()
            .is_match(title),
        "{title}"
    );
    assert_eq!(git(&repo.path, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature/implement-stacked-git-actions");
    let main_sha = git(&repo.path, &["rev-parse", "main"]);
    assert_eq!(git(&repo.path, &["merge-base", "main", "HEAD"]), main_sha);
    assert_eq!(h.text.commit_inputs.lock().unwrap().len(), 1);
}

// TS: "featureBranch uses custom commit message and derives branch name"
#[tokio::test]
async fn feature_branch_uses_custom_commit_message_and_derives_branch_name() {
    let repo = repo();
    write(&repo.path, "README.md", "hello\ncustom-feature\n");
    let h = make_manager(ManagerOptions {
        text_generation: counting_commit("unused", "feature/unused"),
        ..ManagerOptions::default()
    });
    let mut action = input(&repo, "commit");
    action.feature_branch = Some(true);
    action.commit_message = Some("feat: custom summary line\n\n- details from user".into());
    let result = run(&h, action).await.unwrap();

    assert_eq!(result.branch.status, "created");
    assert_eq!(result.branch.name.as_deref(), Some("feature/feat-custom-summary-line"));
    assert_eq!(result.commit.status, "created");
    assert_eq!(result.commit.subject.as_deref(), Some("feat: custom summary line"));
    assert_eq!(h.text.commit_inputs.lock().unwrap().len(), 0);
    let main_sha = git(&repo.path, &["rev-parse", "main"]);
    assert_eq!(git(&repo.path, &["merge-base", "main", result.branch.name.as_deref().unwrap()]), main_sha);
}

// TS: "skips commit when there are no uncommitted changes"
#[tokio::test]
async fn skips_commit_when_there_are_no_uncommitted_changes() {
    let repo = repo();
    let h = make_manager(ManagerOptions::default());
    let result = run(&h, input(&repo, "commit")).await.unwrap();

    assert_eq!(result.branch.status, "skipped_not_requested");
    assert_eq!(result.commit.status, "skipped_no_changes");
    assert_eq!(result.push.status, "skipped_not_requested");
    assert_eq!(result.pr.status, "skipped_not_requested");
}

// TS: "featureBranch returns error when worktree is clean"
#[tokio::test]
async fn feature_branch_returns_error_when_worktree_is_clean() {
    let repo = repo();
    let h = make_manager(ManagerOptions::default());
    let mut action = input(&repo, "commit");
    action.feature_branch = Some(true);
    let error = run(&h, action).await.unwrap_err();

    match &error {
        GitManagerServiceError::Manager(manager) => {
            assert_eq!(manager.operation, "runFeatureBranchStep");
            assert_eq!(manager.cwd, repo.str());
        }
        other => panic!("expected a GitManagerError, got {other:?}"),
    }
    assert!(error.message().contains("no changes to commit"), "{}", error.message());
}

// TS: "commits and pushes with upstream auto-setup when needed"
#[tokio::test]
async fn commits_and_pushes_with_upstream_auto_setup_when_needed() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/stacked-flow"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    write(&repo.path, "feature.txt", "feature\n");
    let h = make_manager(ManagerOptions::default());
    let result = run(&h, input(&repo, "commit_push")).await.unwrap();

    assert_eq!(result.branch.status, "skipped_not_requested");
    assert_eq!(result.commit.status, "created");
    assert_eq!(result.push.status, "pushed");
    assert_eq!(result.push.set_upstream, Some(true));
    assert_eq!(git(&repo.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/feature/stacked-flow");
}

// TS: "pushes and creates PR from a no-upstream branch when local commits are ahead of base"
#[tokio::test]
async fn pushes_and_creates_pr_from_a_no_upstream_branch_when_local_commits_are_ahead_of_base() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/no-upstream-pr"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    write(&repo.path, "feature.txt", "feature\n");
    let h = manager_with(GhScenario {
        pr_list_sequence: vec![
            "[]".into(),
            list(&[pr_entry(
                77,
                "Add no-upstream PR flow",
                "https://github.com/pingdotgg/codething-mvp/pull/77",
                "main",
                "feature/no-upstream-pr",
            )]),
        ],
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.branch.status, "skipped_not_requested");
    assert_eq!(result.commit.status, "created");
    assert_eq!(result.push.status, "pushed");
    assert_eq!(result.push.set_upstream, Some(true));
    assert_eq!(result.pr.status, "created");
    assert_eq!(git(&repo.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/feature/no-upstream-pr");
    assert!(has_call(&h, "pr create --base main --head feature/no-upstream-pr"), "{:?}", h.gh_calls());
}

// TS: "skips push when branch is already up to date"
#[tokio::test]
async fn skips_push_when_branch_is_already_up_to_date() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/up-to-date"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "feature/up-to-date"]);
    let h = make_manager(ManagerOptions::default());
    let result = run(&h, input(&repo, "commit_push")).await.unwrap();

    assert_eq!(result.branch.status, "skipped_not_requested");
    assert_eq!(result.commit.status, "skipped_no_changes");
    assert_eq!(result.push.status, "skipped_up_to_date");
}

// TS: "pushes existing clean commits without rerunning commit logic"
#[tokio::test]
async fn pushes_existing_clean_commits_without_rerunning_commit_logic() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/push-only"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    write(&repo.path, "push-only.txt", "push only\n");
    git(&repo.path, &["add", "push-only.txt"]);
    git(&repo.path, &["commit", "-m", "Push only branch"]);
    let h = make_manager(ManagerOptions::default());
    let result = run(&h, input(&repo, "push")).await.unwrap();

    assert_eq!(result.commit.status, "skipped_not_requested");
    assert_eq!(result.push.status, "pushed");
    assert_eq!(result.pr.status, "skipped_not_requested");
    assert_eq!(git(&repo.path, &["rev-parse", "--abbrev-ref", "@{upstream}"]), "origin/feature/push-only");
}

// TS: "pushes existing commits without committing dirty worktree changes"
#[tokio::test]
async fn pushes_existing_commits_without_committing_dirty_worktree_changes() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/push-dirty"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    write(&repo.path, "push-dirty.txt", "push dirty\n");
    git(&repo.path, &["add", "push-dirty.txt"]);
    git(&repo.path, &["commit", "-m", "Push dirty branch"]);
    write(&repo.path, ".vercel/project.json", "{}\n");
    let h = make_manager(ManagerOptions::default());
    let result = run(&h, input(&repo, "push")).await.unwrap();

    assert_eq!(result.commit.status, "skipped_not_requested");
    assert_eq!(result.push.status, "pushed");
    assert_eq!(result.pr.status, "skipped_not_requested");
    assert!(git(&repo.path, &["status", "--porcelain"]).contains("?? .vercel/"));
    assert_eq!(git(&remote.path, &["log", "-1", "--pretty=%s", "feature/push-dirty"]), "Push dirty branch");
}

// TS: "create_pr pushes a clean branch before creating the PR when needed"
#[tokio::test]
async fn create_pr_pushes_a_clean_branch_before_creating_the_pr_when_needed() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/create-pr-only"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    write(&repo.path, "create-pr-only.txt", "create pr\n");
    git(&repo.path, &["add", "create-pr-only.txt"]);
    git(&repo.path, &["commit", "-m", "Create PR only branch"]);
    let h = manager_with(GhScenario {
        pr_list_sequence: vec![
            "[]".into(),
            list(&[pr_entry(
                303,
                "Create PR only branch",
                "https://github.com/pingdotgg/codething-mvp/pull/303",
                "main",
                "feature/create-pr-only",
            )]),
        ],
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "create_pr")).await.unwrap();

    assert_eq!(result.commit.status, "skipped_not_requested");
    assert_eq!(result.push.status, "pushed");
    assert_eq!(result.push.set_upstream, Some(true));
    assert_eq!(result.pr.status, "created");
    assert_eq!(result.pr.number, Some(303));
    assert!(has_call(&h, "pr create --base main --head feature/create-pr-only"), "{:?}", h.gh_calls());
}

// TS: "create_pr falls back to main when source control provider detection fails"
#[tokio::test]
async fn create_pr_falls_back_to_main_when_source_control_provider_detection_fails() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/provider-fallback"]);
    write(&repo.path, "provider-fallback.txt", "fallback\n");
    git(&repo.path, &["add", "provider-fallback.txt"]);
    git(&repo.path, &["commit", "-m", "Provider fallback"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    let h = manager_with(GhScenario {
        pr_list_sequence: vec![
            "[]".into(),
            list(&[pr_entry(
                404,
                "Provider fallback",
                "https://github.com/pingdotgg/codething-mvp/pull/404",
                "main",
                "feature/provider-fallback",
            )]),
        ],
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "create_pr")).await.unwrap();

    assert_eq!(result.pr.status, "created");
    assert_eq!(result.pr.number, Some(404));
    assert!(has_call(&h, "pr create --base main --head feature/provider-fallback"), "{:?}", h.gh_calls());
}

// TS: "create_pr targets the remote default branch when it is not main"
#[tokio::test]
async fn create_pr_targets_the_remote_default_branch_when_it_is_not_main() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    // A repository whose default branch is master, with no main anywhere.
    git(&repo.path, &["push", "origin", "HEAD:master"]);
    git(&repo.path, &["fetch", "origin"]);
    git(&repo.path, &["remote", "set-head", "origin", "master"]);
    git(&repo.path, &["checkout", "-b", "feature/master-default"]);
    write(&repo.path, "master-default.txt", "master default\n");
    git(&repo.path, &["add", "master-default.txt"]);
    git(&repo.path, &["commit", "-m", "Master default"]);
    let h = manager_with(GhScenario {
        // Mirrors a provider that cannot report a default branch, as the Azure DevOps CLI
        // does when it cannot detect the repository.
        default_branch: Some(String::new()),
        pr_list_sequence: vec![
            "[]".into(),
            list(&[pr_entry(
                505,
                "Master default",
                "https://github.com/pingdotgg/codething-mvp/pull/505",
                "master",
                "feature/master-default",
            )]),
        ],
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "create_pr")).await.unwrap();

    assert_eq!(result.pr.status, "created");
    assert!(has_call(&h, "pr create --base master --head feature/master-default"), "{:?}", h.gh_calls());
}

// TS: "returns existing PR metadata for commit/push/pr action"
#[tokio::test]
async fn returns_existing_pr_metadata_for_commit_push_pr_action() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/existing-pr"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "feature/existing-pr"]);
    let h = manager_with(GhScenario {
        pr_list_sequence: vec![list(&[pr_entry(
            42,
            "Existing PR",
            "https://github.com/pingdotgg/codething-mvp/pull/42",
            "main",
            "feature/existing-pr",
        )])],
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.branch.status, "skipped_not_requested");
    assert_eq!(result.pr.status, "opened_existing");
    assert_eq!(result.pr.number, Some(42));
    assert_eq!(
        toast_json(&result),
        json!({
            "title": "Opened PR #42",
            "description": "Existing PR",
            "cta": {"kind": "open_pr", "label": "View PR", "url": "https://github.com/pingdotgg/codething-mvp/pull/42"},
        })
    );
    assert!(!h.gh_calls().iter().any(|call| call.starts_with("pr view ")));
}

// TS: "returns existing cross-repo PR metadata found under the bare branch name"
#[tokio::test]
async fn returns_existing_cross_repo_pr_metadata_found_under_the_bare_branch_name() {
    let (repo, _fork) = fork_checkout(None);
    let h = manager_with(GhScenario {
        pr_list_sequence: vec![list(&[fork_pr_entry(142, "Existing fork PR", "statemachine")])],
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.pr.status, "opened_existing");
    assert_eq!(result.pr.number, Some(142));
    assert!(has_call(&h, "pr list --head statemachine --state open --limit 100"), "{:?}", h.gh_calls());
    assert!(!h.gh_calls().iter().any(|call| call.starts_with("pr create ")));
}

// TS: "returns the correct existing PR when a slash remote checks out to a synthetic local alias"
#[tokio::test]
async fn returns_the_correct_existing_pr_when_a_slash_remote_checks_out_to_a_synthetic_local_alias() {
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
    write(&repo.path, "changes.txt", "change\n");
    git(&repo.path, &["add", "changes.txt"]);
    git(&repo.path, &["commit", "-m", "Feature commit"]);
    let h = manager_with(GhScenario {
        pr_list_by_head_selector: [
            (
                "effect-atom".to_owned(),
                list(&[pr_entry(
                    1618,
                    "Correct PR",
                    "https://github.com/pingdotgg/t3code/pull/1618",
                    "main",
                    "effect-atom",
                )]),
            ),
            (
                "upstream/effect-atom".to_owned(),
                list(&[pr_entry(
                    1518,
                    "Wrong PR",
                    "https://github.com/pingdotgg/t3code/pull/1518",
                    "main",
                    "upstream/effect-atom",
                )]),
            ),
        ]
        .into_iter()
        .collect(),
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.pr.status, "opened_existing");
    assert_eq!(result.pr.number, Some(1618));
    assert!(!has_call(&h, "pr list --head upstream/effect-atom "), "{:?}", h.gh_calls());
}

// TS: "picks the fork PR among same-named branches from other repositories"
#[tokio::test]
async fn picks_the_fork_pr_among_same_named_branches_from_other_repositories() {
    let (repo, _fork) = fork_checkout(Some("t3code/pr-142/statemachine"));
    let h = manager_with(GhScenario {
        pr_list_by_head_selector: [
            ("t3code/pr-142/statemachine".to_owned(), "[]".to_owned()),
            (
                "statemachine".to_owned(),
                list(&[
                    pr_entry(
                        41,
                        "Unrelated same-repo PR",
                        "https://github.com/pingdotgg/codething-mvp/pull/41",
                        "main",
                        "statemachine",
                    ),
                    fork_pr_entry(142, "Existing fork PR", "statemachine"),
                ]),
            ),
        ]
        .into_iter()
        .collect(),
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.pr.status, "opened_existing");
    assert_eq!(result.pr.number, Some(142));
    let owner_selector = regex::Regex::new(r"--head [^ ]*:").unwrap();
    let calls = h.gh_calls();
    assert!(
        !calls.iter().any(|call| owner_selector.is_match(call) && call.starts_with("pr list")),
        "{calls:?}"
    );
    assert!(!calls.iter().any(|call| call.starts_with("pr create ")));
}

// TS: "stops probing head selectors after finding an existing PR"
#[tokio::test]
async fn stops_probing_head_selectors_after_finding_an_existing_pr() {
    let (repo, _fork) = fork_checkout(Some("t3code/pr-142/statemachine"));
    let h = manager_with(GhScenario {
        pr_list_by_head_selector: [
            ("statemachine".to_owned(), list(&[fork_pr_entry(142, "Existing fork PR", "statemachine")])),
            ("t3code/pr-142/statemachine".to_owned(), "[]".to_owned()),
        ]
        .into_iter()
        .collect(),
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.pr.status, "opened_existing");
    assert_eq!(result.pr.number, Some(142));
    let open_lookups: Vec<String> = h.gh_calls().into_iter().filter(|call| call.contains("--state open --limit 100")).collect();
    assert_eq!(open_lookups.len(), 1, "{open_lookups:?}");
    assert!(
        open_lookups[0].contains("pr list --head statemachine --state open --limit 100"),
        "{open_lookups:?}"
    );
}

// TS: "does not reuse a cross-repo PR when GitHub omits head identity metadata"
#[tokio::test]
async fn does_not_reuse_a_cross_repo_pr_when_github_omits_head_identity_metadata() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "statemachine"]);
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["push", "-u", "fork-seed", "statemachine"]);
    git(&repo.path, &["config", "remote.fork-seed.url", "git@github.com:octocat/codething-mvp.git"]);
    let h = manager_with(GhScenario {
        pr_list_sequence_by_head_selector: [(
            "statemachine".to_owned(),
            vec![
                r#"[{"number":41,"title":"Ambiguous fork PR","url":"https://github.com/pingdotgg/codething-mvp/pull/41","baseRefName":"main","headRefName":"statemachine","state":"OPEN"}]"#.to_owned(),
                r#"[{"number":142,"title":"Add stacked git actions","url":"https://github.com/pingdotgg/codething-mvp/pull/142","baseRefName":"main","headRefName":"statemachine","state":"OPEN","isCrossRepository":true,"headRepository":{"nameWithOwner":"octocat/codething-mvp"},"headRepositoryOwner":{"login":"octocat"}}]"#.to_owned(),
            ],
        )]
        .into_iter()
        .collect(),
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.pr.status, "created");
    assert_eq!(result.pr.number, Some(142));
    assert!(h.gh_calls().iter().any(|call| call.starts_with("pr create ")), "{:?}", h.gh_calls());
}

// TS: "matches mounted Forgejo heads without confusing forks sharing a branch"
#[tokio::test]
async fn matches_mounted_forgejo_heads_without_confusing_forks_sharing_a_branch() {
    use zc_sourcecontrol::forgejo::pull_requests::{decode_forgejo_pull_request, to_forgejo_change_request};
    for owner in ["maria", "reviewer"] {
        let raw = decode_forgejo_pull_request(&json!({
            "number": 42,
            "title": "Greeting",
            "html_url": "https://forgejo.example/forgejo/maria/project/pulls/42",
            "state": "open",
            "merged": false,
            "base": {"ref": "main", "sha": "base", "repo": {"full_name": "maria/project", "owner": {"login": "maria"}}},
            "head": {"ref": "greeting", "sha": "head", "repo": {"full_name": format!("{owner}/project"), "owner": {"login": owner}}},
        }))
        .expect("a valid Forgejo pull request");
        let pr = PullRequestInfo::from_change_request(&to_forgejo_change_request(&raw));
        let repository =
            parse_repository_name_with_owner_from_remote_url(Some(&format!("https://forgejo.example/forgejo/{owner}/project.git")), Some("forgejo"));
        assert_eq!(repository, Some(format!("{owner}/project")));
        let repository_owner = repository.as_deref().and_then(|r| r.split('/').next());
        let context = head_context("greeting", repository.as_deref(), repository_owner, owner != "maria");
        assert!(matches_branch_head_context(&pr, &context), "{owner}");
        assert!(
            !matches_branch_head_context(
                &pr,
                &BranchHeadContext {
                    head_repository_name_with_owner: Some("other/project".into()),
                    head_repository_owner_login: Some("other".into()),
                    ..context.clone()
                }
            ),
            "{owner}"
        );
    }
    assert_eq!(
        parse_repository_name_with_owner_from_remote_url(Some("git@forgejo.example:maria/project.git"), Some("forgejo")).as_deref(),
        Some("maria/project")
    );
    assert_eq!(
        parse_repository_name_with_owner_from_remote_url(Some("https://gitlab.example/group/maria/project.git"), Some("gitlab")).as_deref(),
        Some("group/maria/project")
    );
}

// TS: "rejects same-repo PR metadata when matching a cross-repo head context"
#[test]
fn rejects_same_repo_pr_metadata_when_matching_a_cross_repo_head_context() {
    let head = head_context("statemachine", Some("pingdotgg/codething-mvp"), Some("pingdotgg"), true);
    assert!(!matches_branch_head_context(
        &pr_info(
            41,
            "Same-repo PR",
            "https://github.com/pingdotgg/codething-mvp/pull/41",
            "statemachine",
            false,
            "pingdotgg/codething-mvp",
            "pingdotgg",
        ),
        &head
    ));
    assert!(matches_branch_head_context(
        &pr_info(
            142,
            "Fork PR",
            "https://github.com/pingdotgg/codething-mvp/pull/142",
            "statemachine",
            true,
            "pingdotgg/codething-mvp",
            "pingdotgg",
        ),
        &head
    ));
}

// TS: "accepts fork PR metadata when origin is the fork checkout remote"
#[test]
fn accepts_fork_pr_metadata_when_origin_is_the_fork_checkout_remote() {
    let head = head_context("t3code/git-audit-stability", Some("justsomelegs/t3code"), Some("justsomelegs"), false);
    assert!(matches_branch_head_context(
        &pr_info(
            2284,
            "Improve branch mismatch warnings",
            "https://github.com/pingdotgg/t3code/pull/2284",
            "t3code/git-audit-stability",
            true,
            "justsomelegs/t3code",
            "justsomelegs",
        ),
        &head
    ));
}

// TS: "creates PR when one does not already exist"
#[tokio::test]
async fn creates_pr_when_one_does_not_already_exist() {
    let repo = repo();
    write(&repo.path, ".github/pull_request_template.md", "## What changed?\n\n## Verification");
    git(&repo.path, &["add", ".github/pull_request_template.md"]);
    git(&repo.path, &["commit", "-m", "Add pull request template"]);
    git(&repo.path, &["checkout", "-b", "feature-create-pr"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    write(&repo.path, "changes.txt", "change\n");
    git(&repo.path, &["add", "changes.txt"]);
    git(&repo.path, &["commit", "-m", "Feature commit"]);
    git(&repo.path, &["push", "-u", "origin", "feature-create-pr"]);
    git(&repo.path, &["config", "branch.feature-create-pr.gh-merge-base", "main"]);
    let text = FakeTextGeneration {
        pr: Box::new(|_| {
            Ok(PrContentGenerationResult {
                title: "Add stacked git actions".into(),
                body: "## What changed?\nAdded stacked git actions.".into(),
            })
        }),
        ..FakeTextGeneration::default()
    };
    let h = make_manager(ManagerOptions {
        settings: json!({"sourceControlWritingStyle": {"mode": "custom", "customInstructions": "Lead with user impact."}}),
        text_generation: text,
        gh: GhScenario {
            pr_list_sequence: vec![
                "[]".into(),
                list(&[pr_entry(
                    88,
                    "Add stacked git actions",
                    "https://github.com/pingdotgg/codething-mvp/pull/88",
                    "main",
                    "feature-create-pr",
                )]),
            ],
            ..GhScenario::default()
        },
        ..ManagerOptions::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.branch.status, "skipped_not_requested");
    assert_eq!(result.pr.status, "created");
    assert_eq!(result.pr.number, Some(88));
    let pr_input = h.text.pr_inputs.lock().unwrap()[0].clone();
    assert_eq!(
        pr_input.policy.and_then(|policy| policy.change_request_instructions).as_deref(),
        Some("Lead with user impact.")
    );
    assert_eq!(pr_input.change_request_template.as_deref(), Some("## What changed?\n\n## Verification"));
    let calls = h.gh_calls();
    assert_eq!(calls.iter().filter(|call| call.starts_with("pr list ")).count(), 2, "{calls:?}");
    assert!(has_call(&h, "pr create --base main --head feature-create-pr"), "{calls:?}");
    assert!(!calls.iter().any(|call| call.starts_with("pr view ")));
}

// TS: "generates PR content from branch changes when the remote base advances"
#[tokio::test]
async fn generates_pr_content_from_branch_changes_when_the_remote_base_advances() {
    let repo = repo();
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", "main"]);
    git(&remote.path, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let peer = Tmp::new("t3code-git-peer-");
    git(&peer.path, &["clone", remote.str(), "."]);
    git(&peer.path, &["config", "user.email", "peer@example.com"]);
    git(&peer.path, &["config", "user.name", "Peer User"]);
    write(&peer.path, "remote.txt", "remote\n");
    git(&peer.path, &["add", "remote.txt"]);
    git(&peer.path, &["commit", "-m", "Remote base commit"]);
    git(&peer.path, &["push", "origin", "main"]);

    git(&repo.path, &["fetch", "origin"]);
    git(&repo.path, &["checkout", "--no-track", "-b", "feature/remote-base", "origin/main"]);
    write(&repo.path, "feature.txt", "feature\n");
    git(&repo.path, &["add", "feature.txt"]);
    git(&repo.path, &["commit", "-m", "Feature commit"]);
    git(&repo.path, &["push", "-u", "origin", "feature/remote-base"]);
    git(&repo.path, &["config", "branch.feature/remote-base.gh-merge-base", "main"]);

    write(&peer.path, "later-main.txt", "unrelated\n");
    git(&peer.path, &["add", "later-main.txt"]);
    git(&peer.path, &["commit", "-m", "Later main commit"]);
    git(&peer.path, &["push", "origin", "main"]);
    git(&repo.path, &["fetch", "origin"]);

    let text = FakeTextGeneration {
        pr: Box::new(|_| {
            Ok(PrContentGenerationResult {
                title: "Feature PR".into(),
                body: "Feature body".into(),
            })
        }),
        ..FakeTextGeneration::default()
    };
    let h = make_manager(ManagerOptions {
        gh: GhScenario {
            pr_list_sequence: vec!["[]".into(), "[]".into()],
            ..GhScenario::default()
        },
        text_generation: text,
        ..ManagerOptions::default()
    });
    let result = run(&h, input(&repo, "create_pr")).await.unwrap();

    assert_eq!(result.pr.status, "created");
    let pr_input = h.text.pr_inputs.lock().unwrap()[0].clone();
    assert!(pr_input.commit_summary.contains("Feature commit"), "{}", pr_input.commit_summary);
    assert!(!pr_input.commit_summary.contains("Remote base commit"), "{}", pr_input.commit_summary);
    assert!(!pr_input.commit_summary.contains("Later main commit"), "{}", pr_input.commit_summary);
    assert!(pr_input.diff_summary.contains("feature.txt"), "{}", pr_input.diff_summary);
    assert!(!pr_input.diff_summary.contains("later-main.txt"), "{}", pr_input.diff_summary);
    assert!(pr_input.diff_patch.contains("feature.txt"));
    assert!(!pr_input.diff_patch.contains("later-main.txt"));
}

// TS: "creates a new PR instead of reusing an unrelated fork PR with the same head branch"
#[tokio::test]
async fn creates_a_new_pr_instead_of_reusing_an_unrelated_fork_pr_with_the_same_head_branch() {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", "feature/no-fork-match"]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    write(&repo.path, "changes.txt", "change\n");
    git(&repo.path, &["add", "changes.txt"]);
    git(&repo.path, &["commit", "-m", "Feature commit"]);
    git(&repo.path, &["push", "-u", "origin", "feature/no-fork-match"]);
    let h = manager_with(GhScenario {
        pr_list_sequence: vec![
            list(&[json!({
                "number": 1661,
                "title": "Fork PR with same branch name",
                "url": "https://github.com/pingdotgg/t3code/pull/1661",
                "baseRefName": "main",
                "headRefName": "feature/no-fork-match",
                "state": "OPEN",
                "isCrossRepository": true,
                "headRepository": {"nameWithOwner": "lnieuwenhuis/t3code"},
                "headRepositoryOwner": {"login": "lnieuwenhuis"},
            })]),
            list(&[json!({
                "number": 188,
                "title": "Add stacked git actions",
                "url": "https://github.com/pingdotgg/codething-mvp/pull/188",
                "baseRefName": "main",
                "headRefName": "feature/no-fork-match",
                "state": "OPEN",
                "isCrossRepository": false,
            })]),
        ],
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.pr.status, "created");
    assert_eq!(result.pr.number, Some(188));
    assert_eq!(
        toast_json(&result),
        json!({
            "title": "Created PR #188",
            "description": "Add stacked git actions",
            "cta": {"kind": "open_pr", "label": "View PR", "url": "https://github.com/pingdotgg/codething-mvp/pull/188"},
        })
    );
    assert!(has_call(&h, "pr create --base main --head feature/no-fork-match"), "{:?}", h.gh_calls());
}

// TS: "creates cross-repo PRs with the fork owner selector and default base branch"
#[tokio::test]
async fn creates_cross_repo_prs_with_the_fork_owner_selector_and_default_base_branch() {
    let repo = repo();
    let fork = bare_remote();
    git(&repo.path, &["remote", "add", "fork-seed", fork.str()]);
    git(&repo.path, &["checkout", "-b", "statemachine"]);
    write(&repo.path, "changes.txt", "change\n");
    git(&repo.path, &["add", "changes.txt"]);
    git(&repo.path, &["commit", "-m", "Feature commit"]);
    git(&repo.path, &["push", "-u", "fork-seed", "statemachine"]);
    git(&repo.path, &["checkout", "-b", "t3code/pr-91/statemachine"]);
    git(&repo.path, &["branch", "--set-upstream-to", "fork-seed/statemachine"]);
    configure_visible_remote(&repo.path, "fork-seed", "git@github.com:octocat/codething-mvp.git", fork.str());
    let h = manager_with(GhScenario {
        pr_list_sequence_by_head_selector: [(
            "statemachine".to_owned(),
            vec!["[]".to_owned(), list(&[fork_pr_entry(188, "Add stacked git actions", "statemachine")])],
        )]
        .into_iter()
        .collect(),
        ..GhScenario::default()
    });
    let result = run(&h, input(&repo, "commit_push_pr")).await.unwrap();

    assert_eq!(result.pr.status, "created");
    assert_eq!(result.pr.number, Some(188));
    assert!(has_call(&h, "pr create --base main --head octocat:statemachine"), "{:?}", h.gh_calls());
    assert!(!has_call(&h, "pr create --base statemachine --head octocat:statemachine"), "{:?}", h.gh_calls());
}

// TS: "rejects push/pr actions from detached HEAD"
#[tokio::test]
async fn rejects_push_pr_actions_from_detached_head() {
    let repo = repo();
    git(&repo.path, &["checkout", "--detach", "HEAD"]);
    let h = make_manager(ManagerOptions::default());
    let error = run(&h, input(&repo, "commit_push")).await.unwrap_err();
    assert!(error.message().contains("detached HEAD"), "{}", error.message());
}

/// A pushed, clean `branch` with a gh that fails every call.
async fn run_with_failing_gh(branch: &str, failure: GhFailure) -> GitManagerServiceError {
    let repo = repo();
    git(&repo.path, &["checkout", "-b", branch]);
    let remote = bare_remote();
    git(&repo.path, &["remote", "add", "origin", remote.str()]);
    git(&repo.path, &["push", "-u", "origin", branch]);
    let h = manager_with(GhScenario {
        fail_with: Some(failure),
        ..GhScenario::default()
    });
    run(&h, input(&repo, "commit_push_pr")).await.unwrap_err()
}

// TS: "surfaces missing gh binary errors"
#[tokio::test]
async fn surfaces_missing_gh_binary_errors() {
    let error = run_with_failing_gh("feature/gh-missing", GhFailure::Missing).await;
    assert!(error.message().contains("GitHub CLI (`gh`) is required"), "{}", error.message());
}

// TS: "surfaces gh auth errors with guidance"
#[tokio::test]
async fn surfaces_gh_auth_errors_with_guidance() {
    // gh's own words when no host is logged in (classified as an authentication failure;
    // the guidance must come from the error, not from this stderr).
    let error = run_with_failing_gh(
        "feature/gh-auth",
        GhFailure::Exit {
            code: 1,
            stderr: "You are not logged into any GitHub hosts.".into(),
        },
    )
    .await;
    assert!(error.message().contains("gh auth login"), "{}", error.message());
}
