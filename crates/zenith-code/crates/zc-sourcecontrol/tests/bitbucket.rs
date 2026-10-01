//! `BitbucketApi.test.ts` and `BitbucketSourceControlProvider.test.ts`, against a local mock
//! HTTP server and real git repositories.

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use common::http::*;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use common::*;
use serde_json::{json, Value};
use zc_contracts::{
    BitbucketSettings, SourceControlProviderAuthStatus as Auth, SourceControlProviderInfo, SourceControlProviderKind, SourceControlRepositoryVisibility,
};
use zc_core::vcs_process::VcsProcess;
use zc_sourcecontrol::bitbucket::api::{BitbucketApi, BitbucketApiConfig, BitbucketApiError, BitbucketCredentialSource, BitbucketRequest};
use zc_sourcecontrol::bitbucket::provider::BitbucketSourceControlProvider;
use zc_sourcecontrol::provider::*;
use zc_sourcecontrol::util::ManualClock;
use zc_vcs::registry::{VcsDriverRegistry, VcsProjectConfig};
use zc_vcs::vcs_driver::GitVcsProcessDriver;
use zc_vcs::GitVcsDriver;

/// Settings that tests can change between requests.
#[derive(Default)]
struct MutableSettings(Mutex<Option<BitbucketSettings>>);

impl MutableSettings {
    fn set(&self, email: &str, access_token: &str, api_token: &str) {
        *self.0.lock().unwrap() = Some(serde_json::from_value(json!({"email": email, "accessToken": access_token, "apiToken": api_token})).unwrap());
    }
}

#[async_trait]
impl BitbucketCredentialSource for MutableSettings {
    async fn bitbucket_settings(&self) -> Result<BitbucketSettings, String> {
        Ok(self.0.lock().unwrap().clone().unwrap_or_else(|| serde_json::from_value(json!({})).unwrap()))
    }
}

fn pull_request() -> Value {
    json!({
        "id": 42, "title": "Add Bitbucket provider", "state": "OPEN", "updated_on": "2026-01-02T00:00:00.000Z",
        "links": {"html": {"href": "https://bitbucket.org/team/demo/pull-requests/42"}},
        "source": {"branch": {"name": "feature/source-control"}, "repository": {"full_name": "someone/demo", "workspace": {"slug": "someone"}}},
        "destination": {"branch": {"name": "main"}, "repository": {"full_name": "team/demo", "workspace": {"slug": "team"}}},
    })
}

fn repository(full_name: &str, ssh: &str) -> Value {
    json!({
        "full_name": full_name,
        "links": {"html": {"href": format!("https://bitbucket.org/{full_name}")},
                  "clone": [{"name": "https", "href": format!("https://bitbucket.org/{full_name}.git")}, {"name": "ssh", "href": ssh}]},
        "mainbranch": {"name": "main"},
    })
}

struct Fixture {
    api: BitbucketApi,
    settings: Arc<MutableSettings>,
    repo: Tmp,
    clock: Arc<ManualClock>,
}

/// A checkout whose origin is a Bitbucket URL, and the API pointed at `server`.
fn fixture(server: &MockServer, with_env_credentials: bool) -> Fixture {
    let repo = Tmp::new("zc-bitbucket-repo-");
    init_repo_with_commit(&repo.path);
    git(&repo.path, &["remote", "add", "origin", "git@bitbucket.org:team/demo.git"]);
    let worktrees = Tmp::new("zc-bitbucket-worktrees-");
    let git_driver = GitVcsDriver::new(&worktrees.path);
    std::mem::forget(worktrees);
    let vcs = VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::new(VcsProcess::default())));
    let settings = Arc::new(MutableSettings::default());
    let clock = ManualClock::new(1_000);
    let config = BitbucketApiConfig {
        base_url: server.base.clone(),
        access_token: None,
        email: with_env_credentials.then(|| "user@example.test".into()),
        api_token: with_env_credentials.then(|| "token".into()),
    };
    Fixture {
        api: BitbucketApi::new(config, settings.clone(), clock.clone(), git_driver, vcs),
        settings,
        repo,
        clock,
    }
}

fn basic(user: &str, password: &str) -> String {
    format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}")))
}

#[tokio::test]
async fn parses_pull_request_responses_from_the_rest_api() {
    let server = MockServer::start(|_| reply(200, pull_request().to_string())).await;
    let f = fixture(&server, true);
    let result = f.api.get_pull_request(f.repo.str(), None, "#42").await.unwrap();
    assert_eq!(
        result.to_summary_json(),
        json!({"number": 42, "title": "Add Bitbucket provider", "url": "https://bitbucket.org/team/demo/pull-requests/42", "baseRefName": "main",
               "headRefName": "feature/source-control", "state": "open", "updatedAt": "2026-01-02T00:00:00.000Z", "isCrossRepository": true,
               "headRepositoryNameWithOwner": "someone/demo", "headRepositoryOwnerLogin": "someone"})
    );
    let seen = server.seen();
    assert_eq!(seen[0].target, "/2.0/repositories/team/demo/pullrequests/42");
    assert_eq!(seen[0].headers.get("accept").map(String::as_str), Some("application/json"));
    assert_eq!(seen[0].headers.get("authorization"), Some(&basic("user@example.test", "token")));
}

#[tokio::test]
async fn lists_pull_requests_with_state_and_source_branch_query_params() {
    let server = MockServer::start(|seen| {
        if seen.target.contains("feature%2Fmerged") {
            let mut merged = pull_request();
            merged["id"] = json!(7);
            merged["state"] = json!("MERGED");
            merged["source"] = json!({"branch": {"name": "feature/merged"}, "repository": {"full_name": "team/demo"}});
            return reply(200, json!({"values": [merged]}).to_string());
        }
        reply(200, r#"{"values":[]}"#)
    })
    .await;
    let f = fixture(&server, true);
    let merged = f
        .api
        .list_pull_requests(f.repo.str(), None, "origin:feature/merged", None, ChangeRequestStateFilter::Merged, Some(10))
        .await
        .unwrap();
    assert_eq!(merged[0].state.as_str(), "merged");
    f.api
        .list_pull_requests(f.repo.str(), None, "feature/closed", None, ChangeRequestStateFilter::Closed, Some(10))
        .await
        .unwrap();
    f.api
        .list_pull_requests(f.repo.str(), None, "feature/all", None, ChangeRequestStateFilter::All, Some(10))
        .await
        .unwrap();
    let queries: Vec<Vec<(String, String)>> = server
        .seen()
        .iter()
        .map(|s| {
            let url = url::Url::parse(&format!("http://x{}", s.target)).unwrap();
            assert_eq!(url.path(), "/2.0/repositories/team/demo/pullrequests");
            url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect()
        })
        .collect();
    let pairs = |items: &[(&str, &str)]| items.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect::<Vec<_>>();
    assert_eq!(
        queries[0],
        pairs(&[
            ("pagelen", "10"),
            ("sort", "-updated_on"),
            ("q", r#"source.branch.name = "feature/merged" AND state = "MERGED""#),
            ("state", "MERGED")
        ])
    );
    assert_eq!(
        queries[1],
        pairs(&[
            ("pagelen", "10"),
            ("sort", "-updated_on"),
            ("q", r#"source.branch.name = "feature/closed" AND (state = "DECLINED" OR state = "SUPERSEDED")"#),
            ("state", "DECLINED"),
            ("state", "SUPERSEDED"),
        ])
    );
    assert_eq!(queries[2].len(), 7);
    assert_eq!(
        queries[2][2].1,
        r#"source.branch.name = "feature/all" AND (state = "OPEN" OR state = "MERGED" OR state = "DECLINED" OR state = "SUPERSEDED")"#
    );
}

#[tokio::test]
async fn reads_clone_urls_and_the_default_branch_from_the_branching_model() {
    let model = Arc::new(Mutex::new((
        200,
        json!({"development": {"branch": {"name": "main"}, "name": "main", "use_mainbranch": true}}),
    )));
    let m = model.clone();
    let server = MockServer::start(move |seen| {
        if seen.target.ends_with("/branching-model") {
            let (status, body) = m.lock().unwrap().clone();
            return reply(status, body.to_string());
        }
        reply(200, repository("team/demo", "git@bitbucket.org:team/demo.git").to_string())
    })
    .await;
    let f = fixture(&server, true);
    let urls = f.api.get_repository_clone_urls(f.repo.str(), None, "team/demo").await.unwrap();
    assert_eq!(
        (urls.name_with_owner.as_str(), urls.url.as_str(), urls.ssh_url.as_str()),
        ("team/demo", "https://bitbucket.org/team/demo.git", "git@bitbucket.org:team/demo.git")
    );
    assert_eq!(f.api.get_default_branch(f.repo.str(), None).await.unwrap().as_deref(), Some("main"));
    *model.lock().unwrap() = (
        200,
        json!({"development": {"branch": {"name": "develop"}, "name": "develop", "use_mainbranch": false}}),
    );
    assert_eq!(f.api.get_default_branch(f.repo.str(), None).await.unwrap().as_deref(), Some("develop"));
    *model.lock().unwrap() = (200, json!({"development": {"name": "develop", "use_mainbranch": false, "is_valid": false}}));
    assert_eq!(f.api.get_default_branch(f.repo.str(), None).await.unwrap().as_deref(), Some("main"));
    *model.lock().unwrap() = (404, json!({"error": {"message": "Not found"}}));
    assert_eq!(f.api.get_default_branch(f.repo.str(), None).await.unwrap().as_deref(), Some("main"));
}

#[tokio::test]
async fn creates_repositories_and_pull_requests_with_the_rest_payloads() {
    let server = MockServer::start(|seen| {
        if seen.target.ends_with("/pullrequests") {
            reply(201, pull_request().to_string())
        } else {
            reply(200, repository("team/demo", "git@bitbucket.org:team/demo.git").to_string())
        }
    })
    .await;
    let f = fixture(&server, true);
    let urls = f.api.create_repository("team/demo", SourceControlRepositoryVisibility::Private).await.unwrap();
    assert_eq!(urls.url, "https://bitbucket.org/team/demo.git");
    let body_file = f.repo.join("body.md");
    std::fs::write(&body_file, "PR body").unwrap();
    f.api
        .create_pull_request(f.repo.str(), None, "main", "owner:feature/provider", None, None, "Provider PR", &body_file)
        .await
        .unwrap();
    let seen = server.seen();
    assert_eq!((seen[0].method.as_str(), seen[0].target.as_str()), ("POST", "/2.0/repositories/team/demo"));
    assert_eq!(serde_json::from_str::<Value>(&seen[0].body).unwrap(), json!({"scm": "git", "is_private": true}));
    assert_eq!(
        (seen[1].method.as_str(), seen[1].target.as_str()),
        ("POST", "/2.0/repositories/team/demo/pullrequests")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&seen[1].body).unwrap(),
        json!({"title": "Provider PR", "description": "PR body", "source": {"branch": {"name": "feature/provider"}, "repository": {"full_name": "owner/demo"}},
               "destination": {"branch": {"name": "main"}}})
    );
    let missing = f.api.create_repository("demo", SourceControlRepositoryVisibility::Public).await.unwrap_err();
    assert_eq!(missing.tag(), "BitbucketRepositoryLocatorError");
}

#[tokio::test]
async fn probes_auth_and_prefers_saved_credentials_without_a_restart() {
    let server = MockServer::start(|_| reply(200, r#"{"username":"bitbucket-user"}"#)).await;
    let f = fixture(&server, true);
    let last = || server.seen().last().unwrap().headers.get("authorization").cloned();
    let auth = f.api.probe_auth().await;
    assert_eq!(auth.status, Auth::Authenticated);
    assert_eq!(auth.account.0.as_deref(), Some("bitbucket-user"));
    assert_eq!(auth.host.0.as_deref(), Some("bitbucket.org"));
    assert_eq!(last(), Some(basic("user@example.test", "token")));
    f.settings.set("saved@example.test", "", "saved-api-token");
    f.api.probe_auth().await;
    assert_eq!(last(), Some(basic("saved@example.test", "saved-api-token")));
    f.settings.set("saved@example.test", "saved-access-token", "saved-api-token");
    f.api.probe_auth().await;
    assert_eq!(last().as_deref(), Some("Bearer saved-access-token"));
    f.settings.set("saved@example.test", "", "");
    f.api.probe_auth().await;
    assert_eq!(last(), Some(basic("user@example.test", "token")));
    // A token that cannot travel in a header is treated as unset.
    f.settings.set("", "saved\ntoken", "");
    f.api.probe_auth().await;
    assert_eq!(last(), Some(basic("user@example.test", "token")));
}

#[tokio::test]
async fn reports_saved_credentials_as_configured_when_bitbucket_cannot_confirm_them() {
    let server = MockServer::start(|_| reply(401, "")).await;
    let f = fixture(&server, false);
    assert_eq!(f.api.probe_auth().await.status, Auth::Unauthenticated);
    f.settings.set("", "saved-access-token", "");
    let auth = f.api.probe_auth().await;
    assert_eq!(
        serde_json::to_value(&auth).unwrap(),
        json!({"status": "unknown", "account": {"_tag": "None"}, "host": {"_tag": "Some", "value": "bitbucket.org"},
        "detail": {"_tag": "Some", "value": "An access token is configured."}})
    );
}

#[tokio::test]
async fn keeps_transport_failures_and_response_bodies_out_of_messages() {
    let f = fixture(&MockServer::start(|_| reply(200, "{}")).await, true);
    // Nothing listens on port 9 of localhost: the request itself fails.
    let unreachable = BitbucketApi::new(
        BitbucketApiConfig {
            base_url: "http://127.0.0.1:9/2.0".into(),
            ..BitbucketApiConfig::default()
        },
        f.settings.clone(),
        f.clock.clone(),
        GitVcsDriver::new(f.repo.path.join(".worktrees")),
        VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::new(VcsProcess::default()))),
    );
    let error = unreachable.get_pull_request(f.repo.str(), None, "42").await.unwrap_err();
    assert!(matches!(
        error,
        BitbucketApiError::Request {
            operation: "getPullRequest",
            ..
        }
    ));
    assert_eq!(error.message(), "Bitbucket API failed in getPullRequest: Failed to send the Bitbucket request.");

    let body = r#"{"error":{"message":"credential=secret-value"}}"#;
    let server = MockServer::start(move |_| reply(403, body)).await;
    let f = fixture(&server, true);
    let error = f.api.checkout_pull_request(f.repo.str(), None, "42", false).await.unwrap_err();
    match &error {
        BitbucketApiError::Response {
            operation,
            status,
            response_body_length,
            ..
        } => assert_eq!((*operation, *status, *response_body_length), ("getPullRequest", 403, body.len())),
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(error.message(), "Bitbucket API failed in getPullRequest: Bitbucket returned HTTP 403.");
    assert!(!serde_json::to_string(&error).unwrap().contains("secret-value"));
}

#[tokio::test]
async fn keeps_retry_after_and_body_read_failures() {
    let server = MockServer::start(|seen| {
        let mut answer = reply(if seen.target.contains("broken") { 502 } else { 429 }, "busy");
        answer.headers.push(("retry-after".into(), "120".into()));
        if seen.target.contains("broken") || seen.target.contains("truncated") {
            answer.declared_length = Some(100);
        }
        answer
    })
    .await;
    let f = fixture(&server, true);
    f.clock.set(1_000);
    let request = |url: &str| BitbucketRequest {
        method: "GET".into(),
        url: url.into(),
        body: None,
        max_bytes: None,
    };
    let error = f.api.request(request("/repositories/acme/web")).await.unwrap_err();
    assert!(
        matches!(
            error,
            BitbucketApiError::Response {
                status: 429,
                retry_at: Some(121_000),
                ..
            }
        ),
        "{error:?}"
    );
    let error = f.api.request(request("/repositories/acme/truncated")).await.unwrap_err();
    assert!(
        matches!(
            error,
            BitbucketApiError::ResponseBodyRead {
                status: 429,
                retry_at: Some(121_000),
                ..
            }
        ),
        "{error:?}"
    );
    let error = f
        .api
        .get_pull_request(f.repo.str(), Some(&context("git@bitbucket.org:team/broken.git")), "42")
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            BitbucketApiError::ResponseBodyRead {
                operation: "getPullRequest",
                status: 502,
                ..
            }
        ),
        "{error:?}"
    );
    assert_eq!(error.message(), "Bitbucket API failed in getPullRequest: Bitbucket returned HTTP 502.");
}

fn context(remote_url: &str) -> SourceControlProviderContext {
    SourceControlProviderContext {
        provider: SourceControlProviderInfo {
            kind: SourceControlProviderKind::Bitbucket,
            name: "Bitbucket".into(),
            base_url: "https://bitbucket.org".into(),
        },
        remote_name: "origin".into(),
        remote_url: remote_url.into(),
        requested_host: None,
    }
}

/// A bare repository with `branch` holding one commit.
fn bare_with_branch(branch: &str) -> (Tmp, Tmp) {
    let source = Tmp::new("zc-bitbucket-source-");
    init_repo_with_commit(&source.path);
    git(&source.path, &["checkout", "-b", branch]);
    write(&source.path, "feature.txt", "feature\n");
    git(&source.path, &["add", "."]);
    git(&source.path, &["commit", "-m", "feature"]);
    let bare = Tmp::new("zc-bitbucket-bare-");
    git(&bare.path, &["clone", "--bare", source.str(), "."]);
    (source, bare)
}

#[tokio::test]
async fn checks_out_same_repository_pull_requests_with_the_existing_remote() {
    let mut same = pull_request();
    same["source"] = json!({"branch": {"name": "feature/source-control"}, "repository": {"full_name": "team/demo", "workspace": {"slug": "team"}}});
    let server = MockServer::start(move |_| reply(200, same.to_string())).await;
    let f = fixture(&server, true);
    let (_source, bare) = bare_with_branch("feature/source-control");
    git(&f.repo.path, &["remote", "set-url", "origin", bare.str()]);
    f.api
        .checkout_pull_request(f.repo.str(), Some(&context("git@bitbucket.org:team/demo.git")), "42", true)
        .await
        .unwrap();
    assert_eq!(git(&f.repo.path, &["branch", "--show-current"]), "feature/source-control");
    assert_eq!(git(&f.repo.path, &["config", "branch.feature/source-control.remote"]), "origin");
    assert_eq!(git(&f.repo.path, &["remote"]), "origin");

    // A failing fetch keeps the git error as the cause.
    git(&f.repo.path, &["remote", "set-url", "origin", &f.repo.join("missing.git")]);
    let error = f
        .api
        .checkout_pull_request(f.repo.str(), Some(&context("git@bitbucket.org:team/demo.git")), "42", true)
        .await
        .unwrap_err();
    match &error {
        BitbucketApiError::Checkout { cwd, reference, cause } => {
            assert_eq!((cwd.as_str(), reference.as_str()), (f.repo.str(), "42"));
            assert_eq!(cause.name().as_deref(), Some("GitCommandError"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        error.message(),
        "Bitbucket API failed in checkoutPullRequest: Failed to check out the Bitbucket pull request."
    );
}

#[tokio::test]
async fn checks_out_fork_pull_requests_through_an_ensured_fork_remote() {
    let (_source, fork) = bare_with_branch("main-fork");
    git(&fork.path, &["branch", "-m", "main-fork", "feature"]);
    let fork_path = fork.str().to_owned();
    let server = MockServer::start(move |seen| {
        if seen.target.ends_with("/repositories/someone/demo") {
            return reply(200, repository("someone/demo", &fork_path).to_string());
        }
        let mut fork_pr = pull_request();
        fork_pr["source"] = json!({"branch": {"name": "feature"}, "repository": {"full_name": "someone/demo", "workspace": {"slug": "someone"}}});
        reply(200, fork_pr.to_string())
    })
    .await;
    let f = fixture(&server, true);
    f.api.checkout_pull_request(f.repo.str(), None, "42", true).await.unwrap();
    assert_eq!(git(&f.repo.path, &["branch", "--show-current"]), "t3code/pr-42/feature");
    assert_eq!(git(&f.repo.path, &["config", "remote.someone.url"]), fork.str());
    assert_eq!(git(&f.repo.path, &["config", "branch.t3code/pr-42/feature.remote"]), "someone");
}

#[tokio::test]
async fn guards_credentials_against_untrusted_urls_and_bounds_bodies() {
    let base_for_redirect = Arc::new(Mutex::new(String::new()));
    let b = base_for_redirect.clone();
    let server = MockServer::start(move |seen| {
        if seen.target.ends_with("/offsite") {
            let mut answer = reply(302, "");
            answer.headers.push(("location".into(), "https://attacker.example/stolen".into()));
            return answer;
        }
        if seen.target.ends_with("/pullrequests/1/diff") {
            let mut answer = reply(302, "");
            answer
                .headers
                .push(("location".into(), format!("{}/repositories/acme/web/diff/abc", b.lock().unwrap())));
            return answer;
        }
        reply(
            200,
            if seen.target.ends_with("/big") {
                "1234567890"
            } else {
                "diff --git a/a.ts b/a.ts"
            },
        )
    })
    .await;
    *base_for_redirect.lock().unwrap() = server.base.clone();
    let f = fixture(&server, true);
    let request = |url: &str, max_bytes: Option<usize>| BitbucketRequest {
        method: "GET".into(),
        url: url.into(),
        body: None,
        max_bytes,
    };
    let error = f
        .api
        .request(request("https://attacker.example/asset?signature=secret-token", None))
        .await
        .unwrap_err();
    assert!(matches!(&error, BitbucketApiError::UntrustedUrl { host } if host == "https://attacker.example"));
    assert!(!error.message().contains("secret-token"));
    assert!(server.seen().is_empty());
    let error = f.api.request(request("/repositories/acme/web/offsite", None)).await.unwrap_err();
    assert_eq!(error.tag(), "BitbucketUntrustedUrlError");
    let followed = f.api.request(request("/repositories/acme/web/pullrequests/1/diff", None)).await.unwrap();
    assert_eq!((followed.body.as_str(), followed.truncated), ("diff --git a/a.ts b/a.ts", false));
    let cut = f.api.request(request("/repositories/acme/web/big", Some(8))).await.unwrap();
    assert_eq!((cut.body.as_str(), cut.truncated), ("12345678", true));
}

#[tokio::test]
async fn provider_maps_and_wraps_bitbucket_results() {
    let server = MockServer::start(|seen| {
        if seen.target.contains("/pullrequests/9") {
            reply(404, "{}")
        } else {
            reply(200, pull_request().to_string())
        }
    })
    .await;
    let f = fixture(&server, true);
    let provider = BitbucketSourceControlProvider::new(f.api.clone());
    let change = provider
        .get_change_request(GetChangeRequestInput {
            cwd: f.repo.str().into(),
            context: None,
            reference: "42".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&change).unwrap(),
        json!({"provider": "bitbucket", "number": 42, "title": "Add Bitbucket provider", "url": "https://bitbucket.org/team/demo/pull-requests/42",
               "baseRefName": "main", "headRefName": "feature/source-control", "state": "open", "updatedAt": {"_tag": "Some", "value": "2026-01-02T00:00:00.000Z"},
               "isCrossRepository": true, "headRepositoryNameWithOwner": "someone/demo", "headRepositoryOwnerLogin": "someone"})
    );
    let error = provider
        .get_change_request(GetChangeRequestInput {
            cwd: f.repo.str().into(),
            context: None,
            reference: "9".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        (error.operation.as_str(), error.detail.as_str(), error.command.as_deref()),
        ("getChangeRequest", "Failed to get change request.", None)
    );
    assert_eq!(error.cause.unwrap().name().as_deref(), Some("BitbucketResponseError"));
}
