//! `GitHubCli.test.ts` and `GitHubSourceControlProvider.test.ts`.

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use serde_json::{json, Value};
use zc_contracts::{SourceControlProviderAuthStatus as Auth, SourceControlProviderInfo, SourceControlProviderKind, SourceControlRepositoryVisibility};
use zc_core::process::ProcessRunInput;
use zc_sourcecontrol::discovery::AuthProbeInput;
use zc_sourcecontrol::github::auth_status::parse_github_auth_status;
use zc_sourcecontrol::github::provider::{discovery, GitHubSourceControlProvider};
use zc_sourcecontrol::github::{with_github_reserve, with_pinned_github_credential, GitHubCli, GitHubCliErrorKind, GitHubExecuteInput, PinnedGitHubCredential};
use zc_sourcecontrol::graphql_budget::GitHubGraphQlBudget;
use zc_sourcecontrol::provider::*;
use zc_sourcecontrol::rate_limit::SourceControlRateLimit;
use zc_sourcecontrol::util::{date_parse_millis, ManualClock, SharedClock};

fn quota(remaining: i64, reset_at: &str) -> String {
    json!({"data": {"rateLimit": {"cost": 1, "limit": 5000, "remaining": remaining, "resetAt": reset_at}}}).to_string()
}

fn is_quota(input: &ProcessRunInput) -> bool {
    input.args.get(1).map(String::as_str) == Some("rate_limit")
}

fn cli(runner: &Arc<ScriptedRunner>, clock: SharedClock) -> GitHubCli {
    GitHubCli::new(runner.process(), clock)
}

/// A runner answering quota probes with plenty of quota and everything else with `respond`.
fn with_quota(
    respond: impl Fn(&ProcessRunInput) -> Result<zc_core::process::ProcessRunOutput, zc_core::process::ProcessRunError> + Send + Sync + 'static,
) -> Arc<ScriptedRunner> {
    ScriptedRunner::new(move |input| {
        if is_quota(input) {
            ok(&quota(5000, "2099-01-01T00:00:00Z"))
        } else {
            respond(input)
        }
    })
}

fn non_quota_calls(runner: &ScriptedRunner) -> Vec<ProcessRunInput> {
    runner.calls().into_iter().filter(|c| !is_quota(c)).collect()
}

fn credential(host: &str, token: &str, fingerprint: &str) -> PinnedGitHubCredential {
    PinnedGitHubCredential {
        host: host.into(),
        token: token.into(),
        credential_fingerprint: fingerprint.into(),
    }
}

#[tokio::test]
async fn shares_quota_checks_preserves_the_reserve_and_resumes_after_reset() {
    let clock = ManualClock::new(0);
    let state = Arc::new(Mutex::new((501_i64, "1970-01-01T00:01:00.000Z".to_owned())));
    let probes = Arc::new(Mutex::new(0));
    let (s, p) = (state.clone(), probes.clone());
    let runner = ScriptedRunner::new(move |input| {
        if is_quota(input) {
            *p.lock().unwrap() += 1;
            assert_eq!(input.args[3], "enterprise.test");
            let (remaining, reset) = s.lock().unwrap().clone();
            return ok(&quota(remaining, &reset));
        }
        ok("[]")
    });
    let gh = cli(&runner, clock.clone());
    let read = |command: &str| {
        let args: Vec<String> = if command == "repo" {
            ["repo", "view", "enterprise.test/acme/web", "--json", "name"].map(String::from).to_vec()
        } else {
            vec![
                "pr".into(),
                command.into(),
                "--repo=enterprise.test/acme/web".into(),
                "--json".into(),
                "number".into(),
            ]
        };
        gh.execute(GitHubExecuteInput::new("/repo", args))
    };
    read("list").await.unwrap();
    let failure = read("view").await.unwrap_err();
    assert_eq!(failure.tag(), "GitHubCliRateLimitError");
    assert_eq!(*probes.lock().unwrap(), 1);
    let commands = |r: &ScriptedRunner| non_quota_calls(r).iter().map(|c| c.args[..2].join(" ")).collect::<Vec<_>>();
    assert_eq!(commands(&runner), ["pr list"]);
    with_github_reserve(read("view")).await.unwrap();
    gh.execute(GitHubExecuteInput::new("/repo", ["pr", "merge", "1"])).await.unwrap();
    assert_eq!(commands(&runner), ["pr list", "pr view", "pr merge"]);
    state.lock().unwrap().0 = 0;
    clock.advance(30_000);
    read("repo").await.unwrap_err();
    assert_eq!(*probes.lock().unwrap(), 2);
    clock.advance(30_000);
    *state.lock().unwrap() = (5000, "1970-01-01T00:02:00.000Z".into());
    let (a, b) = tokio::join!(read("list"), read("repo"));
    a.unwrap();
    b.unwrap();
    assert_eq!(*probes.lock().unwrap(), 3);
    let mut tail = commands(&runner)[3..].to_vec();
    tail.sort();
    assert_eq!(tail, ["pr list", "repo view"]);
}

#[tokio::test]
async fn shares_the_registry_budget_with_cli_reads() {
    let clock: SharedClock = ManualClock::new(0);
    let runner = with_quota(|_| ok("[]"));
    let budget = GitHubGraphQlBudget::new(clock.clone());
    let gh = GitHubCli::with_limits(runner.process(), budget.clone(), SourceControlRateLimit::new(clock.clone()), clock);
    budget.observe("github.com", &quota(0, "2099-01-01T00:00:00Z"));
    let error = gh.execute(GitHubExecuteInput::new("/repo", ["pr", "list"])).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubCliRateLimitError");
    assert!(non_quota_calls(&runner).is_empty());
}

#[tokio::test]
async fn keeps_quota_snapshots_separate_for_verified_credentials_on_the_same_host() {
    let runner = ScriptedRunner::new(|input| {
        if is_quota(input) {
            let remaining = if env_of(input, "GH_TOKEN").as_deref() == Some("empty") { 0 } else { 5000 };
            return ok(&quota(remaining, "2099-01-01T00:00:00Z"));
        }
        ok("[]")
    });
    let gh = cli(&runner, ManualClock::new(0));
    let read = |token: &'static str| {
        with_pinned_github_credential(
            credential("github.com", token, token),
            gh.execute(GitHubExecuteInput::new("/repo", ["pr", "list", "--repo", "github.com/acme/web"])),
        )
    };
    read("empty").await.unwrap_err();
    read("healthy").await.unwrap();
    read("empty").await.unwrap_err();
    assert_eq!(non_quota_calls(&runner).len(), 1);
}

#[tokio::test]
async fn pins_concurrent_commands_to_their_own_verified_credentials() {
    let runner = with_quota(|input| ok(&env_of(input, "GH_TOKEN").unwrap_or_else(|| "ambient".into())));
    let gh = cli(&runner, ManualClock::new(0));
    let call = |host: &'static str, index: usize| {
        let mut input = GitHubExecuteInput::new("/repo", ["api", "user", "--hostname", host]);
        input.env = Some(
            [
                ("GH_DEBUG".to_owned(), "api".to_owned()),
                ("GH_TOKEN".to_owned(), "changed-after-verification".to_owned()),
            ]
            .into(),
        );
        with_pinned_github_credential(
            credential(host, &format!("credential-{index}"), &format!("fingerprint-{index}")),
            gh.execute(input),
        )
    };
    let (a, b) = tokio::join!(call("github.com", 0), call("github.example.test", 1));
    assert_eq!([a.unwrap().stdout, b.unwrap().stdout], ["credential-0", "credential-1"]);
    for input in runner.calls() {
        assert_eq!(env_of(&input, "GH_HOST").as_deref(), Some(input.args[3].as_str()));
        assert_eq!(env_of(&input, "GH_DEBUG").as_deref(), Some(""));
        let token = env_of(&input, "GH_TOKEN");
        for key in ["GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"] {
            assert_eq!(env_of(&input, key), token);
        }
    }
    assert_eq!(gh.execute(GitHubExecuteInput::new("/repo", ["api", "user"])).await.unwrap().stdout, "ambient");
}

#[tokio::test]
async fn refuses_other_or_implicit_hosts_before_exposing_a_scoped_credential() {
    let runner = with_quota(|_| ok(""));
    let gh = cli(&runner, ManualClock::new(0));
    for args in [
        vec!["api", "user", "--hostname", "other.example.test"],
        vec!["api", "user", "--hostname=other.example.test"],
        vec!["pr", "view", "1", "--repo", "other.example.test/owner/repo"],
        vec!["repo", "view", "other.example.test/owner/repo", "--json", "name"],
        vec!["api", "https://other.example.test/user", "--hostname", "github.com"],
        vec!["api", "user"],
    ] {
        let failure = with_pinned_github_credential(
            credential("github.com", "secret-credential", "fingerprint"),
            gh.execute(GitHubExecuteInput::new("/repo", args)),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.tag(), "GitHubCliCommandError");
        assert!(!serde_json::to_string(&failure).unwrap().contains("secret-credential"));
    }
    assert!(runner.calls().is_empty());
}

#[tokio::test]
async fn pins_repository_targeted_writes_on_enterprise_hosts() {
    let runner = with_quota(|_| ok(""));
    let gh = cli(&runner, ManualClock::new(0));
    let pinned = || credential("github.example.test", "enterprise-credential", "fingerprint");
    with_pinned_github_credential(
        pinned(),
        gh.execute(GitHubExecuteInput::new(
            "/repo",
            ["pr", "merge", "1", "--repo", "github.example.test/owner/repo"],
        )),
    )
    .await
    .unwrap();
    with_pinned_github_credential(
        pinned(),
        gh.execute(GitHubExecuteInput::new(
            "/repo",
            ["repo", "view", "github.example.test/owner/repo", "--json", "name"],
        )),
    )
    .await
    .unwrap();
    let first = &runner.calls()[0];
    assert_eq!(env_of(first, "GH_HOST").as_deref(), Some("github.example.test"));
    assert_eq!(env_of(first, "GH_ENTERPRISE_TOKEN").as_deref(), Some("enterprise-credential"));
    assert_eq!(env_of(first, "GH_DEBUG").as_deref(), Some(""));
}

#[tokio::test]
async fn does_not_classify_a_missing_cwd_as_an_unavailable_gh() {
    let runner = ScriptedRunner::new(missing);
    let gh = cli(&runner, ManualClock::new(0));
    let error = gh.execute(GitHubExecuteInput::new("/zc-missing-cwd/repo", ["api", "user"])).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubCliCommandError");
    assert_eq!(error.cause.name().as_deref(), Some("VcsProcessSpawnError"));
    let cwd = Tmp::new("zc-gh-cwd-");
    let error = gh.execute(GitHubExecuteInput::new(cwd.str(), ["api", "user"])).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubCliUnavailableError");
    assert_eq!(error.detail(), "GitHub CLI (`gh`) is required but not available on PATH.");
}

#[tokio::test]
async fn parses_pull_request_view_output() {
    let runner = with_quota(|_| {
        ok(&json!({
            "number": 42,
            "title": "Add PR thread creation",
            "url": "https://github.com/octocat/demo/pull/42",
            "baseRefName": "main",
            "headRefName": "feature/pr-threads",
            "state": "OPEN",
            "isDraft": true,
            "mergedAt": null,
            "updatedAt": "2026-08-24T12:34:56Z",
            "isCrossRepository": true,
            "headRepository": {"nameWithOwner": "someone/demo"},
            "headRepositoryOwner": {"login": "someone"},
        })
        .to_string())
    });
    let gh = cli(&runner, ManualClock::new(0));
    let result = gh.get_pull_request("/repo", "#42", None).await.unwrap();
    assert_eq!(
        result.to_summary_json(),
        json!({
            "number": 42,
            "title": "Add PR thread creation",
            "url": "https://github.com/octocat/demo/pull/42",
            "baseRefName": "main",
            "headRefName": "feature/pr-threads",
            "state": "open",
            "closedAt": null,
            "mergedAt": null,
            "isDraft": true,
            "updatedAt": "2026-08-24T12:34:56.000Z",
            "isCrossRepository": true,
            "headRepositoryNameWithOwner": "someone/demo",
            "headRepositoryOwnerLogin": "someone",
        })
    );
    let call = &non_quota_calls(&runner)[0];
    assert_eq!(call.command, "gh");
    assert_eq!(
        call.args,
        [
            "pr",
            "view",
            "#42",
            "--json",
            "number,title,url,baseRefName,headRefName,state,isDraft,mergedAt,closedAt,updatedAt,isCrossRepository,headRepository,headRepositoryOwner"
        ]
    );
    assert_eq!(call.cwd.as_deref(), Some(std::path::Path::new("/repo")));
    assert_eq!(call.timeout, Some(std::time::Duration::from_secs(30)));
}

#[tokio::test]
async fn trims_pull_request_fields_decoded_from_gh_json() {
    let runner = with_quota(|_| {
        ok(&json!({
            "number": 42,
            "title": "  Add PR thread creation  \n",
            "url": " https://github.com/octocat/demo/pull/42 ",
            "baseRefName": " main ",
            "headRefName": "\tfeature/pr-threads\t",
            "state": "OPEN",
            "mergedAt": null,
            "isCrossRepository": true,
            "headRepository": {"nameWithOwner": " someone/demo "},
            "headRepositoryOwner": {"login": " someone "},
        })
        .to_string())
    });
    let result = cli(&runner, ManualClock::new(0)).get_pull_request("/repo", "#42", None).await.unwrap();
    assert_eq!(
        result.to_summary_json(),
        json!({
            "number": 42,
            "title": "Add PR thread creation",
            "url": "https://github.com/octocat/demo/pull/42",
            "baseRefName": "main",
            "headRefName": "feature/pr-threads",
            "state": "open",
            "closedAt": null,
            "mergedAt": null,
            "isCrossRepository": true,
            "headRepositoryNameWithOwner": "someone/demo",
            "headRepositoryOwnerLogin": "someone",
        })
    );
}

#[tokio::test]
async fn skips_invalid_entries_when_parsing_pr_lists() {
    let runner = with_quota(|_| {
        ok(&json!([
            {"number": 0, "title": "invalid", "url": "https://github.com/octocat/demo/pull/0", "baseRefName": "main", "headRefName": "feature/invalid"},
            {"number": 43, "title": "  Valid PR  ", "url": " https://github.com/octocat/demo/pull/43 ", "baseRefName": " main ", "headRefName": " feature/pr-list ",
             "headRepository": {"nameWithOwner": "   "}, "headRepositoryOwner": {"login": "   "}},
        ])
        .to_string())
    });
    let result = cli(&runner, ManualClock::new(0))
        .list_open_pull_requests("/repo", "feature/pr-list", None, None)
        .await
        .unwrap();
    assert_eq!(
        result.iter().map(|r| r.to_summary_json()).collect::<Vec<_>>(),
        [json!({
            "number": 43,
            "title": "Valid PR",
            "url": "https://github.com/octocat/demo/pull/43",
            "baseRefName": "main",
            "headRefName": "feature/pr-list",
            "state": "open",
            "closedAt": null,
            "mergedAt": null,
        })]
    );
}

#[tokio::test]
async fn keeps_pull_requests_from_gh_versions_without_name_with_owner() {
    let runner = with_quota(|_| {
        ok(&json!([{
            "number": 2829, "title": "Codex turn mapping", "url": "https://github.com/octocat/demo/pull/2829",
            "baseRefName": "main", "headRefName": "t3code/codex-turn-mapping", "state": "OPEN", "mergedAt": null,
            "isCrossRepository": false, "headRepository": {"id": "R_1", "name": "demo"}, "headRepositoryOwner": {"id": "O_1", "login": "octocat"},
        }])
        .to_string())
    });
    let result = cli(&runner, ManualClock::new(0))
        .list_open_pull_requests("/repo", "t3code/codex-turn-mapping", None, None)
        .await
        .unwrap();
    assert_eq!(result[0].to_summary_json()["headRepositoryNameWithOwner"], "octocat/demo");
    assert_eq!(result[0].to_summary_json()["headRepositoryOwnerLogin"], "octocat");
    assert_eq!(result[0].to_summary_json()["isCrossRepository"], false);
}

#[tokio::test]
async fn reads_repository_clone_urls() {
    let runner = with_quota(|_| ok(r#"{"nameWithOwner":"octocat/demo","url":"https://github.com/octocat/demo","sshUrl":"git@github.com:octocat/demo.git"}"#));
    let urls = cli(&runner, ManualClock::new(0))
        .get_repository_clone_urls("/repo", "octocat/demo")
        .await
        .unwrap();
    assert_eq!(urls.ssh_url, "git@github.com:octocat/demo.git");
    assert_eq!(
        non_quota_calls(&runner)[0].args,
        ["repo", "view", "octocat/demo", "--json", "nameWithOwner,url,sshUrl"]
    );
}

#[tokio::test]
async fn creates_repositories_and_parses_clone_urls_from_create_output() {
    let runner = with_quota(|_| ok("✓ Created repository octocat/demo on github.com\nhttps://github.com/octocat/demo\n"));
    let urls = cli(&runner, ManualClock::new(0))
        .create_repository("/repo", "octocat/demo", SourceControlRepositoryVisibility::Private)
        .await
        .unwrap();
    assert_eq!(
        (urls.name_with_owner.as_str(), urls.url.as_str(), urls.ssh_url.as_str()),
        ("octocat/demo", "https://github.com/octocat/demo", "git@github.com:octocat/demo.git")
    );
    assert_eq!(runner.lines(), ["gh repo create octocat/demo --private"]);
    let runner = with_quota(|_| ok(""));
    let urls = cli(&runner, ManualClock::new(0))
        .create_repository("/repo", "octocat/demo", SourceControlRepositoryVisibility::Private)
        .await
        .unwrap();
    assert_eq!(urls.url, "https://github.com/octocat/demo");
}

#[tokio::test]
async fn surfaces_a_friendly_error_when_the_pull_request_is_not_found() {
    let runner = with_quota(|_| {
        exit(
            1,
            "",
            "GraphQL: Could not resolve to a PullRequest with the number of 4888. (repository.pullRequest)",
        )
    });
    let error = cli(&runner, ManualClock::new(0)).get_pull_request("/repo", "4888", None).await.unwrap_err();
    assert!(error.message().contains("Pull request not found"));
    assert_eq!(error.tag(), "GitHubPullRequestNotFoundError");
    assert_eq!(error.command(), "gh");
    assert_eq!(error.cwd, "/repo");
    assert_eq!(error.cause.name().as_deref(), Some("VcsProcessExitError"));
    assert!(!error.message().contains("4888"));
}

#[tokio::test]
async fn surfaces_a_rate_limit_error_and_pauses_the_host() {
    let clock = ManualClock::new(0);
    let fail = Arc::new(Mutex::new(true));
    let f = fail.clone();
    let runner = with_quota(move |_| {
        if *f.lock().unwrap() {
            exit(1, "", "API rate limit exceeded for user ID 1.")
        } else {
            ok("[]")
        }
    });
    let gh = cli(&runner, clock.clone());
    let error = gh.list_open_pull_requests("/repo", "feature/rate-limited", None, None).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubCliRateLimitError");
    assert!(error.detail().contains("GitHub API rate limit exceeded"));
    assert!(error.detail().contains("gh api rate_limit"));
    assert!(!error.message().contains("user ID"));
    let paused = gh.execute(GitHubExecuteInput::new("/other-repo", ["pr", "list"])).await.unwrap_err();
    assert!(matches!(paused.kind, GitHubCliErrorKind::RateLimit { retry_at: Some(30_000) }));
    assert_eq!(non_quota_calls(&runner).len(), 1);
    clock.advance(30_000);
    *fail.lock().unwrap() = false;
    gh.execute(GitHubExecuteInput::new("/other-repo", ["pr", "list"])).await.unwrap();
    assert_eq!(non_quota_calls(&runner).len(), 2);
}

// GitHubSourceControlProvider.test.ts

fn provider(runner: &Arc<ScriptedRunner>) -> GitHubSourceControlProvider {
    GitHubSourceControlProvider::new(cli(runner, ManualClock::new(0)))
}

#[tokio::test]
async fn uses_the_enterprise_quota_for_a_current_repository_default_branch_read() {
    let runner = ScriptedRunner::new(|input| {
        if !is_quota(input) {
            return ok("main");
        }
        assert_eq!(input.args[3], "enterprise.test");
        ok(&quota(5000, "2099-01-01T00:00:00Z"))
    });
    let branch = provider(&runner)
        .get_default_branch(DefaultBranchInput {
            cwd: "/enterprise-repo".into(),
            context: Some(SourceControlProviderContext {
                provider: SourceControlProviderInfo {
                    kind: SourceControlProviderKind::Github,
                    name: "GitHub Enterprise".into(),
                    base_url: "https://enterprise.test".into(),
                },
                remote_name: "origin".into(),
                remote_url: "https://enterprise.test/acme/web.git".into(),
                requested_host: None,
            }),
        })
        .await
        .unwrap();
    assert_eq!(branch.as_deref(), Some("main"));
}

#[tokio::test]
async fn maps_github_pr_summaries_into_change_requests() {
    let runner = with_quota(|_| {
        ok(
            &json!({"number": 42, "title": "Add GitHub provider", "url": "https://github.com/octocat/demo/pull/42", "baseRefName": "main",
            "headRefName": "feature/source-control", "state": "OPEN", "isCrossRepository": true,
            "headRepository": {"nameWithOwner": "fork/demo"}, "headRepositoryOwner": {"login": "fork"}})
            .to_string(),
        )
    });
    let change = provider(&runner)
        .get_change_request(GetChangeRequestInput {
            cwd: "/repo".into(),
            context: None,
            reference: "42".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&change).unwrap(),
        json!({
            "provider": "github", "number": 42, "title": "Add GitHub provider", "url": "https://github.com/octocat/demo/pull/42",
            "baseRefName": "main", "headRefName": "feature/source-control", "state": "open", "closedAt": null, "mergedAt": null,
            "updatedAt": {"_tag": "None"}, "isCrossRepository": true, "headRepositoryNameWithOwner": "fork/demo", "headRepositoryOwnerLogin": "fork",
        })
    );
}

#[tokio::test]
async fn adds_safe_request_context_while_retaining_cli_causes() {
    let runner = with_quota(|_| exit(1, "", "no pull requests found for branch: raw upstream detail"));
    let error = provider(&runner)
        .get_change_request(GetChangeRequestInput {
            cwd: "/repo".into(),
            context: None,
            reference: "https://user:secret@github.com/octocat/demo/pull/42?token=secret#diff".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.provider, SourceControlProviderKind::Github);
    assert_eq!(error.operation, "getChangeRequest");
    assert_eq!(error.command.as_deref(), Some("gh"));
    assert_eq!(error.cwd, "/repo");
    assert_eq!(error.reference.as_deref(), Some("https://github.com/octocat/demo/pull/42"));
    assert_eq!(error.detail, "Pull request not found. Check the PR number or URL and try again.");
    assert_eq!(error.cause.as_ref().and_then(|c| c.name()).as_deref(), Some("GitHubPullRequestNotFoundError"));
    assert!(!error.message().contains("raw upstream detail"));
    assert!(!serde_json::to_string(&error).unwrap().contains("secret"));
}

#[tokio::test]
async fn uses_gh_json_listing_for_non_open_state_queries() {
    let runner = with_quota(|_| {
        ok(
            &json!([{"number": 7, "title": "Merged work", "url": "https://github.com/octocat/demo/pull/7", "baseRefName": "main",
            "headRefName": "feature/merged", "state": "merged", "mergedAt": "2026-01-01T00:00:00Z", "updatedAt": "2026-01-02T00:00:00.000Z"}])
            .to_string(),
        )
    });
    let changes = provider(&runner)
        .list_change_requests(ListChangeRequestsInput {
            cwd: "/repo".into(),
            context: None,
            source: None,
            head_selector: "feature/merged".into(),
            state: ChangeRequestStateFilter::All,
            limit: Some(10),
        })
        .await
        .unwrap();
    assert_eq!(
        non_quota_calls(&runner)[0].args,
        [
            "pr",
            "list",
            "--head",
            "feature/merged",
            "--state",
            "all",
            "--limit",
            "10",
            "--json",
            "number,title,url,baseRefName,headRefName,state,isDraft,mergedAt,closedAt,updatedAt,isCrossRepository,headRepository,headRepositoryOwner"
        ]
    );
    let change = serde_json::to_value(&changes[0]).unwrap();
    assert_eq!(change["provider"], "github");
    assert_eq!(change["state"], "merged");
    assert_eq!(change["mergedAt"], "2026-01-01T00:00:00Z");
    assert_eq!(change["updatedAt"], json!({"_tag": "Some", "value": "2026-01-02T00:00:00.000Z"}));

    let runner = with_quota(|_| ok(""));
    let changes = provider(&runner)
        .list_change_requests(ListChangeRequestsInput {
            cwd: "/repo".into(),
            context: None,
            source: None,
            head_selector: "feature/empty".into(),
            state: ChangeRequestStateFilter::All,
            limit: Some(10),
        })
        .await
        .unwrap();
    assert!(changes.is_empty());
}

#[tokio::test]
async fn creates_github_prs_through_provider_neutral_input_names() {
    let runner = with_quota(|_| ok(""));
    provider(&runner)
        .create_change_request(CreateChangeRequestInput {
            cwd: "/repo".into(),
            context: None,
            source: None,
            target: None,
            base_ref_name: "main".into(),
            head_selector: "owner:feature/provider".into(),
            title: "Provider PR".into(),
            body_file: "/tmp/body.md".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        runner.lines(),
        ["gh pr create --base main --head owner:feature/provider --title Provider PR --body-file /tmp/body.md"]
    );
}

fn auth(stdout: &str, stderr: &str, exit_code: i32) -> zc_contracts::SourceControlProviderAuth {
    (discovery().parse_auth)(&AuthProbeInput {
        stdout: stdout.into(),
        stderr: stderr.into(),
        exit_code,
    })
}

fn account(state: &str, active: bool, host: &str, login: &str, error: Option<&str>) -> Value {
    let mut value = json!({"state": state, "active": active, "host": host, "login": login, "tokenSource": "keyring", "gitProtocol": "ssh"});
    if let Some(error) = error {
        value["error"] = json!(error);
    }
    value
}

#[test]
fn accepts_active_authenticated_accounts_when_another_fails() {
    let stdout = json!({"hosts": {"github.com": [
        account("success", true, "github.com", "active-user", None),
        account("error", false, "github.com", "stale-user", Some("The token in keyring is invalid.")),
    ]}})
    .to_string();
    let result = auth(&stdout, "", 0);
    assert_eq!(result.status, Auth::Authenticated);
    assert_eq!(result.account.0.as_deref(), Some("active-user"));
    assert_eq!(result.host.0.as_deref(), Some("github.com"));
    let with_warnings = auth(
        &json!({"hosts": {"github.com": [account("success", true, "github.com", "active-user", None)]}}).to_string(),
        "warning: ignored diagnostic from gh\n",
        0,
    );
    assert_eq!(with_warnings.status, Auth::Authenticated);
}

#[test]
fn parses_github_auth_status_accounts_by_host_and_active_state() {
    let status = parse_github_auth_status(
        &json!({"hosts": {
            "github.com": [account("success", true, "github.com", "active-user", None), account("error", false, "github.com", "stale-user", None)],
            "github.example.test": [account("success", false, "github.example.test", "enterprise-user", None)],
        }})
        .to_string(),
    );
    let summary: Vec<(String, String, bool, bool)> = status
        .accounts
        .iter()
        .map(|a| (a.host.clone(), a.account.clone(), a.authenticated, a.active))
        .collect();
    assert_eq!(
        summary,
        [
            ("github.com".to_owned(), "active-user".to_owned(), true, true),
            ("github.com".to_owned(), "stale-user".to_owned(), false, false),
            ("github.example.test".to_owned(), "enterprise-user".to_owned(), true, false),
        ]
    );
}

#[test]
fn reports_unauthenticated_and_old_gh_states() {
    let invalid = auth(
        &json!({"hosts": {"github.com": [account("error", true, "github.com", "stale-user", Some("The token in keyring is invalid."))]}}).to_string(),
        "",
        0,
    );
    assert_eq!(invalid.status, Auth::Unauthenticated);
    assert_eq!(invalid.host.0.as_deref(), Some("github.com"));
    assert_eq!(invalid.detail.0.as_deref(), Some("The token in keyring is invalid."));
    let old = auth("", "unknown flag: --json\n\nUsage:  gh auth status [flags]\n", 1);
    assert_eq!(old.status, Auth::Unknown);
    assert!(old.detail.0.unwrap().contains("2.81.0"));
}

#[tokio::test]
async fn resolves_link_subjects_on_the_linked_host_without_using_the_checkout() {
    for kind in ["pull", "issues"] {
        let runner = ScriptedRunner::new(|input| {
            assert_eq!(
                input.args,
                ["api", "--hostname", "github.com", "repos/owner/repo/issues/42", "--jq", "{title, body}"]
            );
            assert_eq!(input.max_output_bytes, Some(32_000));
            assert_eq!(input.timeout, Some(std::time::Duration::from_millis(3_000)));
            ok(r#"{"title":"Pairing expiry","body":"Preserve remote access"}"#)
        });
        let provider = provider(&runner);
        let url = url::Url::parse(&format!("https://github.com/owner/repo/{kind}/42")).unwrap();
        let subject = provider.resolve_link("/unrelated", &url).unwrap().await.unwrap();
        assert_eq!(subject.title, "Pairing expiry");
        assert_eq!(subject.body.as_deref(), Some("Preserve remote access"));
        assert!(provider
            .resolve_link("/unrelated", &url::Url::parse("https://github.com/owner/repo").unwrap())
            .is_none());
    }
}

#[tokio::test]
async fn retains_link_failures_without_exposing_raw_contents() {
    for stage in ["read", "decode"] {
        let runner = ScriptedRunner::new(move |_| {
            if stage == "read" {
                exit(1, "private response text", "boom")
            } else {
                ok("private response text")
            }
        });
        let url = url::Url::parse("https://github.com/owner/repo/issues/42").unwrap();
        let error = provider(&runner).resolve_link("/repo", &url).unwrap().await.unwrap_err();
        assert_eq!(error.operation, if stage == "read" { "resolveLink" } else { "resolveLink.decode" });
        assert_eq!(error.detail, "The linked subject could not be read.");
        assert!(!error.message().contains("private response text"));
        let cause = error.cause.unwrap().name();
        assert_eq!(cause.as_deref(), Some(if stage == "read" { "GitHubCliCommandError" } else { "SchemaError" }));
    }
}

#[tokio::test]
async fn runs_a_fake_gh_on_the_path() {
    let clis = FakeClis::new();
    clis.respond("gh", "api rate_limit --hostname github.com --jq .resources.graphql | {data:{rateLimit:{cost:1,limit:.limit,remaining:.remaining,resetAt:(.reset|todateiso8601)}}}", &quota(5000, "2099-01-01T00:00:00Z"), "", 0);
    clis.respond(
        "gh",
        "pr view 5 --json number,title,url,baseRefName,headRefName,state,isDraft,mergedAt,closedAt,updatedAt,isCrossRepository,headRepository,headRepositoryOwner",
        r#"{"number":5,"title":"Fake","url":"https://github.com/octocat/demo/pull/5","baseRefName":"main","headRefName":"x","state":"CLOSED"}"#,
        "",
        0,
    );
    clis.respond(
        "gh",
        "pr checkout 7",
        "",
        "GraphQL: Could not resolve to a PullRequest with the number of 7.",
        1,
    );
    let cwd = Tmp::new("zc-gh-fake-");
    let gh = GitHubCli::new(clis.process(), ManualClock::new(date_parse_millis("2026-01-01T00:00:00Z").unwrap()));
    let record = gh.get_pull_request(cwd.str(), "5", None).await.unwrap();
    assert_eq!(record.to_summary_json()["state"], "closed");
    let error = gh.checkout_pull_request(cwd.str(), "7", false).await.unwrap_err();
    assert_eq!(error.tag(), "GitHubPullRequestNotFoundError");
    assert_eq!(clis.log("gh").len(), 3);
    let missing = GitHubCli::new(FakeClis::new().process(), ManualClock::new(0))
        .execute(GitHubExecuteInput::new(cwd.str(), ["api", "user"]))
        .await
        .unwrap_err();
    assert_eq!(missing.tag(), "GitHubCliUnavailableError");
}
