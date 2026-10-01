//! `GitLabCli.test.ts`, `GitLabSourceControlProvider.test.ts`, `AzureDevOpsCli.test.ts`,
//! `AzureDevOpsSourceControlProvider.test.ts`.

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;
use zc_contracts::{SourceControlProviderAuthStatus as Auth, SourceControlProviderInfo, SourceControlProviderKind, SourceControlRepositoryVisibility};
use zc_sourcecontrol::azure::provider::AzureDevOpsSourceControlProvider;
use zc_sourcecontrol::azure::{AzureDevOpsCli, AzureDevOpsCliErrorKind, AzureExecuteInput};
use zc_sourcecontrol::discovery::{AuthProbeInput, RefinementInput};
use zc_sourcecontrol::gitlab::auth_status::parse_gitlab_auth_status_hosts;
use zc_sourcecontrol::gitlab::provider::{discovery, GitLabSourceControlProvider};
use zc_sourcecontrol::gitlab::{GitLabCli, GitLabExecuteInput};
use zc_sourcecontrol::provider::*;

#[tokio::test]
async fn gitlab_parses_merge_request_view_output() {
    let runner = ScriptedRunner::new(|_| {
        ok(&json!({
            "iid": 42, "title": "Add MR thread creation", "web_url": "https://gitlab.com/group/demo/-/merge_requests/42",
            "target_branch": "main", "source_branch": "feature/mr-threads", "state": "closed", "closed_at": "2026-08-23T10:00:00Z",
            "source_project_id": 101, "target_project_id": 100, "source_project": {"path_with_namespace": "someone/demo"},
        })
        .to_string())
    });
    let result = GitLabCli::new(runner.process()).get_merge_request("/repo", "42").await.unwrap();
    assert_eq!(
        result.to_summary_json(),
        json!({
            "number": 42, "title": "Add MR thread creation", "url": "https://gitlab.com/group/demo/-/merge_requests/42",
            "baseRefName": "main", "headRefName": "feature/mr-threads", "state": "closed", "closedAt": "2026-08-23T10:00:00Z",
            "mergedAt": null, "isCrossRepository": true, "headRepositoryNameWithOwner": "someone/demo", "headRepositoryOwnerLogin": "someone",
        })
    );
    assert_eq!(runner.lines(), ["glab mr view 42 --output json"]);
}

#[tokio::test]
async fn gitlab_skips_invalid_entries_when_parsing_mr_lists() {
    let runner = ScriptedRunner::new(|_| {
        ok(&json!([
            {"iid": 0, "title": "invalid", "web_url": "https://gitlab.com/group/demo/-/merge_requests/0", "target_branch": "main", "source_branch": "feature/invalid"},
            {"iid": 43, "title": "  Valid MR  ", "web_url": " https://gitlab.com/group/demo/-/merge_requests/43 ", "target_branch": " main ",
             "source_branch": " feature/mr-list ", "state": "merged", "merged_at": "2026-08-23T11:00:00Z"},
        ])
        .to_string())
    });
    let result = GitLabCli::new(runner.process())
        .list_merge_requests("/repo", "feature/mr-list", None, ChangeRequestStateFilter::All, None)
        .await
        .unwrap();
    assert_eq!(
        result.iter().map(|r| r.to_summary_json()).collect::<Vec<_>>(),
        [
            json!({"number": 43, "title": "Valid MR", "url": "https://gitlab.com/group/demo/-/merge_requests/43", "baseRefName": "main",
                "headRefName": "feature/mr-list", "state": "merged", "closedAt": null, "mergedAt": "2026-08-23T11:00:00Z"})
        ]
    );
    assert_eq!(
        runner.lines(),
        ["glab mr list --source-branch feature/mr-list --all --per-page 20 --output json"]
    );
}

#[tokio::test]
async fn gitlab_reads_clone_urls_and_creates_merge_requests_and_repositories() {
    let project = json!({"path_with_namespace": "someone/demo", "web_url": "https://gitlab.com/someone/demo",
        "http_url_to_repo": "https://gitlab.com/someone/demo.git", "ssh_url_to_repo": "git@gitlab.com:someone/demo.git"})
    .to_string();
    let project_for_runner = project.clone();
    let runner = ScriptedRunner::new(move |input| {
        if input.args.get(1).map(String::as_str) == Some("namespaces/someone") {
            ok(r#"{"id":1234}"#)
        } else {
            ok(&project_for_runner)
        }
    });
    let glab = GitLabCli::new(runner.process());
    let urls = glab.get_repository_clone_urls("/repo", "someone/demo").await.unwrap();
    assert_eq!(
        (urls.name_with_owner.as_str(), urls.url.as_str(), urls.ssh_url.as_str()),
        ("someone/demo", "https://gitlab.com/someone/demo", "git@gitlab.com:someone/demo.git")
    );
    glab.create_merge_request("/repo", "main", "owner:feature/provider", None, None, "Provider MR", "/tmp/t3-mr-body.md")
        .await
        .unwrap();
    let created = glab
        .create_repository("/repo", "someone/demo", SourceControlRepositoryVisibility::Public)
        .await
        .unwrap();
    assert_eq!(created.name_with_owner, "someone/demo");
    glab.checkout_merge_request("/repo", "42").await.unwrap();
    assert_eq!(
        runner.lines(),
        [
            "glab api projects/someone%2Fdemo",
            "glab api --method POST projects/:fullpath/merge_requests --raw-field source_branch=feature/provider --raw-field target_branch=main --raw-field title=Provider MR --field description=@/tmp/t3-mr-body.md",
            "glab api namespaces/someone",
            "glab api --method POST projects --raw-field path=demo --raw-field name=demo --raw-field visibility=public --raw-field namespace_id=1234",
            "glab mr checkout 42",
        ]
    );
}

#[tokio::test]
async fn gitlab_classifies_not_found_and_rate_limit_failures() {
    let runner = ScriptedRunner::new(|input| match input.args[0].as_str() {
        "mr" => exit(1, "", "GET 404 merge request not found"),
        "api" if input.args[1] == "projects" => exit(1, "", "API rate limit exceeded"),
        _ => exit(1, "", "GET 404 project not found"),
    });
    let glab = GitLabCli::new(runner.process());
    let error = glab.get_merge_request("/repo", "4888").await.unwrap_err();
    assert!(error.message().contains("Merge request 4888 was not found"));
    assert_eq!(error.tag(), "GitLabMergeRequestNotFoundError");
    assert_eq!((error.command(), error.cwd.as_str()), ("glab", "/repo"));
    assert_eq!(error.cause.name().as_deref(), Some("VcsProcessExitError"));
    assert!(!error.message().contains("GET 404"));
    let generic = glab.get_repository_clone_urls("/repo", "missing/project").await.unwrap_err();
    assert_eq!(generic.tag(), "GitLabCliCommandError");
    let limited = glab.execute(GitLabExecuteInput::new("/repo", ["api", "projects"])).await.unwrap_err();
    assert_eq!(limited.tag(), "GitLabCliRateLimitError");
}

#[tokio::test]
async fn gitlab_provider_maps_and_wraps() {
    let runner = ScriptedRunner::new(|input| {
        if input.args[0] == "mr" && input.args[1] == "view" {
            return exit(1, "", "404 Not Found: raw upstream detail");
        }
        ok(&json!([{"iid": 5, "title": "MR", "web_url": "https://gitlab.com/g/p/-/merge_requests/5", "target_branch": "main", "source_branch": "feature/x", "draft": true}]).to_string())
    });
    let provider = GitLabSourceControlProvider::new(GitLabCli::new(runner.process()));
    let listed = provider
        .list_change_requests(ListChangeRequestsInput {
            cwd: "/repo".into(),
            context: None,
            source: None,
            head_selector: "fork:feature/x".into(),
            state: ChangeRequestStateFilter::Open,
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&listed[0]).unwrap(),
        json!({"provider": "gitlab", "number": 5, "title": "MR", "url": "https://gitlab.com/g/p/-/merge_requests/5", "baseRefName": "main",
               "headRefName": "feature/x", "state": "open", "isDraft": true, "closedAt": null, "mergedAt": null, "updatedAt": {"_tag": "None"}})
    );
    assert_eq!(runner.lines()[0], "glab mr list --source-branch feature/x --per-page 20 --output json");
    let error = provider
        .get_change_request(GetChangeRequestInput {
            cwd: "/repo".into(),
            context: None,
            reference: "https://token@gitlab.com/g/p/-/merge_requests/9?private_token=x".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.operation, "getChangeRequest");
    assert_eq!(error.command.as_deref(), Some("glab"));
    assert_eq!(error.reference.as_deref(), Some("https://gitlab.com/g/p/-/merge_requests/9"));
    assert!(error.detail.starts_with("Merge request https://token@gitlab.com"), "{}", error.detail);
    assert!(!error.message().contains("raw upstream detail"));
}

#[test]
fn gitlab_auth_and_refinement() {
    let auth = (discovery().parse_auth)(&AuthProbeInput {
        exit_code: 1,
        stdout: "gitlab.com\n  x gitlab.com: API call failed: 401 Unauthorized\n  ! No token found\nself-hosted.example.test\n  ✓ Logged in to self-hosted.example.test as gitlab-user\n  ✓ Token found: ******\n".into(),
        stderr: String::new(),
    });
    assert_eq!(auth.status, Auth::Authenticated);
    assert_eq!(auth.account.0.as_deref(), Some("gitlab-user"));
    assert_eq!(auth.host.0.as_deref(), Some("self-hosted.example.test"));

    let refined = (discovery().refine_unknown_remote.unwrap())(&RefinementInput {
        cwd: "/repo".into(),
        context: SourceControlProviderContext {
            provider: SourceControlProviderInfo {
                kind: SourceControlProviderKind::Unknown,
                name: "Self-Hosted.Example.Test".into(),
                base_url: "https://Self-Hosted.Example.Test".into(),
            },
            remote_name: "origin".into(),
            remote_url: "https://Self-Hosted.Example.Test/group/project.git".into(),
            requested_host: None,
        },
        auth: AuthProbeInput {
            exit_code: 0,
            stdout: "self-hosted.example.test\n  ✓ Logged in to self-hosted.example.test as gitlab-user\n  ✓ Token found: ******\n".into(),
            stderr: String::new(),
        },
    })
    .unwrap();
    assert_eq!(refined.kind, SourceControlProviderKind::Gitlab);
    assert_eq!(refined.name, "GitLab Self-Hosted");
    assert_eq!(refined.base_url, "https://Self-Hosted.Example.Test");

    let hosts = parse_gitlab_auth_status_hosts(
        "localhost:8080\n  ✓ Logged in to localhost:8080 as local-user\nselfhosted\n  ✓ Logged in to selfhosted as single-label-user\n",
    );
    let pairs: Vec<(String, Option<String>)> = hosts.into_iter().map(|h| (h.host, h.account)).collect();
    assert_eq!(
        pairs,
        [
            ("localhost:8080".to_owned(), Some("local-user".to_owned())),
            ("selfhosted".to_owned(), Some("single-label-user".to_owned()))
        ]
    );
}

#[tokio::test]
async fn gitlab_resolves_link_subjects() {
    for kind in ["merge_requests", "issues"] {
        let runner = ScriptedRunner::new(move |input| {
            assert_eq!(
                input.args,
                ["api", "--hostname", "gitlab.com", &format!("projects/team%2Fsub%2Fproject/{kind}/2")]
            );
            ok(r#"{"title":"GitLab MR","description":"Nested project"}"#)
        });
        let provider = GitLabSourceControlProvider::new(GitLabCli::new(runner.process()));
        let url = url::Url::parse(&format!("https://gitlab.com/team/sub/project/-/{kind}/2")).unwrap();
        let subject = provider.resolve_link("/unrelated", &url).unwrap().await.unwrap();
        assert_eq!((subject.title.as_str(), subject.body.as_deref()), ("GitLab MR", Some("Nested project")));
        assert!(provider
            .resolve_link("/x", &url::Url::parse("https://gitlab.attacker.test/team/project/-/issues/1").unwrap())
            .is_none());
    }
}

// Azure DevOps

fn az_calls(runner: &ScriptedRunner) -> Vec<(Vec<String>, Option<Duration>, Option<usize>)> {
    runner.calls().into_iter().map(|c| (c.args, c.timeout, c.max_output_bytes)).collect()
}

#[tokio::test]
async fn azure_parses_pull_request_view_output_and_builds_web_urls() {
    let runner = ScriptedRunner::new(|input| {
        if input.args.contains(&"863".to_owned()) {
            return ok(&json!({
                "pullRequestId": 863, "title": "Fix Azure link",
                "url": "https://dev.azure.com/example-org/a8fe4088/_apis/git/repositories/16108a25/pullRequests/863",
                "repository": {"name": "CV engine", "project": {"name": "CV engine"}},
                "sourceRefName": "refs/heads/feature/azure-pr-link", "targetRefName": "refs/heads/main", "status": "active",
            })
            .to_string());
        }
        ok(&json!({
            "pullRequestId": 42, "title": "Add Azure provider", "sourceRefName": "refs/heads/feature/source-control",
            "targetRefName": "refs/heads/main", "status": "active", "creationDate": "2026-01-02T00:00:00.000Z", "closedDate": null,
            "_links": {"web": {"href": "https://dev.azure.com/acme/project/_git/repo/pullrequest/42"}},
        })
        .to_string())
    });
    let az = AzureDevOpsCli::new(runner.process());
    let result = az.get_pull_request("/repo", "#42").await.unwrap();
    assert_eq!(
        (
            result.number,
            result.title.as_str(),
            result.url.as_str(),
            result.base_ref_name.as_str(),
            result.head_ref_name.as_str()
        ),
        (
            42,
            "Add Azure provider",
            "https://dev.azure.com/acme/project/_git/repo/pullrequest/42",
            "main",
            "feature/source-control"
        )
    );
    assert!(result.updated_at.is_some());
    assert_eq!(
        az_calls(&runner)[0],
        (
            [
                "repos",
                "pr",
                "show",
                "--detect",
                "true",
                "--id",
                "42",
                "--only-show-errors",
                "--output",
                "json"
            ]
            .map(String::from)
            .to_vec(),
            Some(Duration::from_secs(30)),
            Some(1_000_000)
        )
    );
    let built = az.get_pull_request("/repo", "863").await.unwrap();
    assert_eq!(built.url, "https://dev.azure.com/example-org/CV%20engine/_git/CV%20engine/pullrequest/863");
}

#[tokio::test]
async fn azure_lists_clones_creates_and_checks_out() {
    let repo = json!({"name": "repo", "webUrl": "https://dev.azure.com/acme/project/_git/repo", "remoteUrl": "https://dev.azure.com/acme/project/_git/repo",
        "sshUrl": "git@ssh.dev.azure.com:v3/acme/project/repo", "project": {"name": "project"}})
    .to_string();
    let runner = ScriptedRunner::new(move |input| {
        match input.args[1].as_str() {
        "pr" if input.args[2] == "list" => ok(&json!([{"pullRequestId": 7, "title": "Merged work", "sourceRefName": "refs/heads/feature/merged", "targetRefName": "refs/heads/main",
            "status": "completed", "closedDate": "2026-01-03T00:00:00.000Z", "_links": {"web": {"href": "https://dev.azure.com/acme/project/_git/repo/pullrequest/7"}}}])
        .to_string()),
        "pr" => ok(""),
        _ => ok(&repo),
    }
    });
    let az = AzureDevOpsCli::new(runner.process());
    let listed = az
        .list_pull_requests("/repo", "origin:feature/merged", None, ChangeRequestStateFilter::Merged, Some(10))
        .await
        .unwrap();
    assert_eq!(listed[0].to_summary_json()["state"], "merged");
    assert_eq!(listed[0].to_summary_json()["mergedAt"], "2026-01-03T00:00:00.000Z");
    assert_eq!(listed[0].to_summary_json()["closedAt"], serde_json::Value::Null);
    let urls = az.get_repository_clone_urls("/repo", "repo").await.unwrap();
    assert_eq!(
        (urls.name_with_owner.as_str(), urls.ssh_url.as_str()),
        ("project/repo", "git@ssh.dev.azure.com:v3/acme/project/repo")
    );
    az.create_repository("/repo", "project/repo", SourceControlRepositoryVisibility::Private)
        .await
        .unwrap();
    az.create_pull_request("/repo", "main", "feature/provider", None, None, "Provider PR", "/tmp/body.md")
        .await
        .unwrap();
    az.checkout_pull_request("/repo", "42", None).await.unwrap();
    az.execute(AzureExecuteInput {
        cwd: "/repo".into(),
        args: vec!["repos".into(), "pr".into(), "list".into()],
        timeout_ms: None,
        max_output_bytes: Some(16 * 1024 * 1024),
    })
    .await
    .unwrap();
    let lines = runner.lines();
    assert_eq!(
        lines[0],
        "az repos pr list --detect true --source-branch feature/merged --status completed --top 10 --only-show-errors --output json"
    );
    assert_eq!(lines[1], "az repos show --detect true --repository repo --only-show-errors --output json");
    assert_eq!(
        lines[2],
        "az repos create --detect true --name repo --project project --only-show-errors --output json"
    );
    assert_eq!(
        lines[3],
        "az repos pr create --only-show-errors --detect true --target-branch main --source-branch feature/provider --title Provider PR --description @/tmp/body.md"
    );
    assert_eq!(lines[4], "az repos pr checkout --only-show-errors --detect true --id 42 --remote-name origin");
    assert_eq!(az_calls(&runner)[5].2, Some(16 * 1024 * 1024));
}

#[tokio::test]
async fn azure_classifies_failures_without_copying_upstream_details() {
    let runner = ScriptedRunner::new(|input| match input.args[0].as_str() {
        "limited" => exit(1, "", "API rate limit exceeded"),
        "decode" => ok("not-json"),
        "spawn" => missing(input),
        _ => exit(1, "", "sensitive-upstream-detail"),
    });
    let az = AzureDevOpsCli::new(runner.process());
    let error = az.execute(AzureExecuteInput::new("/repo", ["repos", "list"])).await.unwrap_err();
    assert_eq!(error.kind, AzureDevOpsCliErrorKind::CommandFailed { argument_count: 2 });
    assert_eq!(error.detail(), "Azure DevOps CLI command failed.");
    assert!(!error.message().contains("sensitive-upstream-detail"));
    let missing_cwd = az.execute(AzureExecuteInput::new("/missing/repo", ["spawn", "x"])).await.unwrap_err();
    assert_eq!(missing_cwd.tag(), "AzureDevOpsCommandFailedError");
    let limited = az.execute(AzureExecuteInput::new("/repo", ["limited", "x"])).await.unwrap_err();
    assert_eq!(limited.tag(), "AzureDevOpsCliRateLimitError");

    let runner = ScriptedRunner::new(|_| ok("not-json"));
    let error = AzureDevOpsCli::new(runner.process()).get_pull_request("/repo", "42").await.unwrap_err();
    assert_eq!(error.tag(), "AzureDevOpsPullRequestDecodeError");
    assert_eq!(error.kind, AzureDevOpsCliErrorKind::PullRequestDecode { output_length: 8 });
    assert_eq!(
        error.message(),
        "Azure DevOps CLI failed in getPullRequest: Azure DevOps CLI returned invalid pull request JSON."
    );
    assert_eq!(serde_json::to_value(&error).unwrap()["outputLength"], 8);
}

#[tokio::test]
async fn azure_provider_maps_and_wraps() {
    let runner = ScriptedRunner::new(|input| {
        if input.args[2] == "checkout" {
            return exit(1, "", "raw upstream detail that should remain in the cause");
        }
        ok(&json!({"pullRequestId": 42, "title": "Add Azure provider", "sourceRefName": "refs/heads/feature/source-control", "targetRefName": "refs/heads/main",
            "status": "abandoned", "closedDate": "2026-08-23T10:00:00Z", "_links": {"web": {"href": "https://dev.azure.com/acme/project/_git/repo/pullrequest/42"}}})
        .to_string())
    });
    let provider = AzureDevOpsSourceControlProvider::new(AzureDevOpsCli::new(runner.process()));
    let change = provider
        .get_change_request(GetChangeRequestInput {
            cwd: "/repo".into(),
            context: None,
            reference: "42".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&change).unwrap(),
        json!({"provider": "azure-devops", "number": 42, "title": "Add Azure provider", "url": "https://dev.azure.com/acme/project/_git/repo/pullrequest/42",
               "baseRefName": "main", "headRefName": "feature/source-control", "state": "closed", "closedAt": "2026-08-23T10:00:00.000Z", "mergedAt": null,
               "updatedAt": {"_tag": "Some", "value": "2026-08-23T10:00:00.000Z"}, "isCrossRepository": false})
    );
    let error = provider
        .checkout_change_request(CheckoutChangeRequestInput {
            cwd: "/repo".into(),
            context: None,
            reference: "#42".into(),
            force: false,
        })
        .await
        .unwrap_err();
    assert_eq!(
        (error.operation.as_str(), error.command.as_deref(), error.reference.as_deref()),
        ("checkoutChangeRequest", Some("az"), Some("#42"))
    );
    assert_eq!(error.detail, "Azure DevOps CLI command failed.");
    assert!(!error.message().contains("raw upstream detail"));
}

#[test]
fn azure_auth_parsing() {
    let parse = zc_sourcecontrol::azure::provider::discovery().parse_auth;
    let authenticated = parse(&AuthProbeInput {
        stdout: "someone@example.test\n".into(),
        stderr: String::new(),
        exit_code: 0,
    });
    assert_eq!(authenticated.status, Auth::Authenticated);
    assert_eq!(authenticated.host.0.as_deref(), Some("dev.azure.com"));
    let signed_out = parse(&AuthProbeInput {
        stdout: String::new(),
        stderr: "Please run 'az login' to setup account.".into(),
        exit_code: 1,
    });
    assert_eq!(signed_out.status, Auth::Unauthenticated);
    assert_eq!(signed_out.detail.0.as_deref(), Some("Please run 'az login' to setup account."));
}
