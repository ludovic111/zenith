//! `SourceControlProviderRegistry.test.ts` and the discovery cases of
//! `SourceControlDiscovery.test.ts`, on real repositories with fake forge CLIs on the PATH, plus
//! the zc-vcs status hook.

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use std::sync::Arc;

use common::*;
use serde_json::json;
use zc_contracts::{SourceControlProviderInfo, SourceControlProviderKind};
use zc_core::vcs_process::VcsProcess;
use zc_sourcecontrol::bitbucket::{BitbucketApiConfig, StaticBitbucketSettings};
use zc_sourcecontrol::forgejo::ForgejoEnvironment;
use zc_sourcecontrol::provider::*;
use zc_sourcecontrol::util::ManualClock;
use zc_sourcecontrol::{SourceControl, SourceControlDeps, WithSourceControlProviders};
use zc_vcs::registry::{VcsDriverRegistry, VcsProjectConfig};
use zc_vcs::status::{GitStatusService, NoPullRequests};
use zc_vcs::vcs_driver::GitVcsProcessDriver;
use zc_vcs::GitVcsDriver;

struct World {
    source_control: SourceControl,
    clis: Arc<FakeClis>,
    _home: Tmp,
    _worktrees: Tmp,
    git: GitVcsDriver,
}

fn make_world(clis: Arc<FakeClis>) -> World {
    let home = Tmp::new("zc-registry-home-");
    let worktrees = Tmp::new("zc-registry-worktrees-");
    let git = GitVcsDriver::new(&worktrees.path);
    let process: VcsProcess = clis.process();
    let source_control = SourceControl::new(SourceControlDeps {
        cwd: home.str().into(),
        process: process.clone(),
        git: git.clone(),
        vcs: VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::new(process))),
        bitbucket_credentials: Arc::new(StaticBitbucketSettings(None)),
        bitbucket_config: BitbucketApiConfig {
            base_url: "http://127.0.0.1:9/2.0".into(),
            ..BitbucketApiConfig::default()
        },
        forgejo_environment: ForgejoEnvironment {
            platform: "linux".into(),
            home: home.path.clone(),
            data_home: None,
            app_data: None,
        },
        clock: ManualClock::new(0),
    });
    World {
        source_control,
        clis,
        _home: home,
        _worktrees: worktrees,
        git,
    }
}

fn repo_with_remotes(remotes: &[(&str, &str)]) -> Tmp {
    let repo = Tmp::new("zc-registry-repo-");
    init_repo_with_commit(&repo.path);
    for (name, url) in remotes {
        git(&repo.path, &["remote", "add", name, url]);
    }
    repo
}

async fn resolved_kind(world: &World, remotes: &[(&str, &str)]) -> SourceControlProviderKind {
    let repo = repo_with_remotes(remotes);
    world.source_control.registry.resolve(repo.str()).await.unwrap().kind()
}

#[tokio::test]
async fn routes_remotes_to_their_providers() {
    let world = make_world(FakeClis::new());
    assert_eq!(
        resolved_kind(&world, &[("origin", "git@github.com:octocat/demo.git")]).await,
        SourceControlProviderKind::Github
    );
    assert_eq!(
        resolved_kind(&world, &[("origin", "git@gitlab.com:group/project.git")]).await,
        SourceControlProviderKind::Gitlab
    );
    assert_eq!(
        resolved_kind(&world, &[("origin", "git@bitbucket.org:team/demo.git")]).await,
        SourceControlProviderKind::Bitbucket
    );
    assert_eq!(
        resolved_kind(&world, &[("origin", "https://dev.azure.com/acme/project/_git/repo")]).await,
        SourceControlProviderKind::AzureDevops
    );
    // Without origin, the first recognized forge wins.
    assert_eq!(
        resolved_kind(&world, &[("upstream", "https://dev.azure.com/acme/project/_git/repo")]).await,
        SourceControlProviderKind::AzureDevops
    );
    assert_eq!(
        world.source_control.registry.get(SourceControlProviderKind::Github).kind(),
        SourceControlProviderKind::Github
    );
}

#[tokio::test]
async fn includes_the_request_cwd_when_an_unregistered_provider_is_used() {
    let world = make_world(FakeClis::new());
    let error = world
        .source_control
        .registry
        .get(SourceControlProviderKind::Unknown)
        .get_change_request(GetChangeRequestInput {
            cwd: "/repo".into(),
            context: None,
            reference: "#42".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(
        (error.provider, error.operation.as_str(), error.cwd.as_str(), error.reference.as_deref()),
        (SourceControlProviderKind::Unknown, "getChangeRequest", "/repo", Some("#42"))
    );
    assert_eq!(error.detail, "No unknown source control provider is registered.");
}

#[tokio::test]
async fn retains_vcs_detection_failures_with_structured_cwd_context() {
    let world = make_world(FakeClis::new());
    let not_a_repo = Tmp::new("zc-registry-plain-");
    let error = world.source_control.registry.resolve(not_a_repo.str()).await.err().unwrap();
    assert_eq!(error.provider, SourceControlProviderKind::Unknown);
    assert_eq!(error.operation, "detectProvider");
    assert_eq!(error.cwd, not_a_repo.str());
    assert_eq!(error.detail, "Failed to detect source control provider.");
    assert_eq!(error.cause.unwrap().name().as_deref(), Some("VcsUnsupportedOperationError"));
}

const GLAB_STATUS: &str = "gitlab.com\n  x gitlab.com: API call failed: 401 Unauthorized\n  ! No token found\nself-hosted.example.test\n  ✓ Logged in to self-hosted.example.test as gitlab-user\n  ✓ Token found: ******\n";

#[tokio::test]
async fn routes_authenticated_self_hosted_gitlab_remotes() {
    let clis = FakeClis::new();
    clis.respond("glab", "auth status", GLAB_STATUS, "", 1);
    let world = make_world(clis);
    assert_eq!(
        resolved_kind(&world, &[("origin", "https://self-hosted.example.test/group/project.git")]).await,
        SourceControlProviderKind::Gitlab
    );
    // An unrecognized host nobody is signed in to stays unknown.
    assert_eq!(
        resolved_kind(&world, &[("origin", "https://git.example.test/group/project.git")]).await,
        SourceControlProviderKind::Unknown
    );
    assert!(world.clis.log("glab").iter().all(|line| line == "auth status"));

    let clis = FakeClis::new();
    clis.respond(
        "glab",
        "auth status",
        "self-hosted.example.test:8443\n  ✓ Logged in to self-hosted.example.test:8443 as gitlab-user\n",
        "",
        0,
    );
    let world = make_world(clis);
    assert_eq!(
        resolved_kind(&world, &[("origin", "https://self-hosted.example.test:8443/group/project.git")]).await,
        SourceControlProviderKind::Gitlab
    );
}

#[tokio::test]
async fn refines_the_caller_selected_remote_instead_of_another_configured_remote() {
    let clis = FakeClis::new();
    clis.respond(
        "glab",
        "auth status",
        "self-hosted.example.test\n  ✓ Logged in to self-hosted.example.test as gitlab-user\n",
        "",
        0,
    );
    let world = make_world(clis);
    let repo = repo_with_remotes(&[("origin", "git@github.com:fork/project.git")]);
    let handle = world
        .source_control
        .registry
        .resolve_handle(
            repo.str(),
            Some(SourceControlProviderContext {
                provider: SourceControlProviderInfo {
                    kind: SourceControlProviderKind::Unknown,
                    name: "self-hosted.example.test".into(),
                    base_url: "https://self-hosted.example.test".into(),
                },
                remote_name: "upstream".into(),
                remote_url: "https://self-hosted.example.test/group/project.git".into(),
                requested_host: None,
            }),
        )
        .await
        .unwrap();
    let context = handle.context.unwrap();
    assert_eq!(
        (context.provider.kind, context.remote_name.as_str()),
        (SourceControlProviderKind::Gitlab, "upstream")
    );
    assert_eq!(handle.provider.kind(), SourceControlProviderKind::Gitlab);
}

#[tokio::test]
async fn routes_linked_subjects_by_url_and_skips_unsupported_links() {
    let clis = FakeClis::new();
    clis.respond(
        "gh",
        "api --hostname github.com repos/team/project/issues/1 --jq {title, body}",
        r#"{"title":"GitHub issue","body":null}"#,
        "",
        0,
    );
    clis.respond(
        "glab",
        "api --hostname gitlab.com projects/team%2Fsub%2Fproject/merge_requests/2",
        r#"{"title":"GitLab MR","description":"Nested project"}"#,
        "",
        0,
    );
    let world = make_world(clis);
    let cwd = Tmp::new("zc-registry-links-");
    let registry = &world.source_control.registry;
    let github = registry
        .resolve_link_str(cwd.str(), "https://github.com/team/project/issues/1")
        .unwrap()
        .await
        .unwrap();
    assert_eq!((github.title.as_str(), github.body), ("GitHub issue", None));
    let gitlab = registry
        .resolve_link_str(cwd.str(), "https://gitlab.com/team/sub/project/-/merge_requests/2")
        .unwrap()
        .await
        .unwrap();
    assert_eq!((gitlab.title.as_str(), gitlab.body.as_deref()), ("GitLab MR", Some("Nested project")));
    for url in [
        "https://example.test/team/project/issues/1",
        "https://github.attacker.test/team/project/issues/1",
        "https://gitlab.attacker.test/team/project/-/issues/1",
        "https://github.com/team/project",
        "https://codeberg.org/team/project/issues/1",
        "https://bitbucket.org/team/project/pull-requests/1",
        "https://dev.azure.com/org/project/_git/repo/pullrequest/1",
        "http://github.com/team/project/issues/1",
        "https://user:secret@github.com/team/project/issues/1",
    ] {
        assert!(registry.resolve_link_str(cwd.str(), url).is_none(), "{url}");
    }
}

#[tokio::test]
async fn reports_implemented_tools_separately_from_available_executables() {
    let clis = FakeClis::new();
    clis.respond("gh", "--version", "gh version 2.83.0\n", "", 0);
    clis.respond(
        "gh",
        "auth status --json hosts",
        &json!({"hosts": {"github.com": [{"state": "success", "active": true, "host": "github.com", "login": "octocat", "tokenSource": "keyring", "gitProtocol": "ssh"}]}}).to_string(),
        "",
        0,
    );
    let world = make_world(clis);
    let result = serde_json::to_value(world.source_control.discovery.discover().await).unwrap();
    let vcs: Vec<(String, bool, String)> = result["versionControlSystems"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["kind"].as_str().unwrap().into(),
                item["implemented"].as_bool().unwrap(),
                item["status"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        vcs,
        [("git".to_owned(), true, "available".to_owned()), ("jj".to_owned(), false, "missing".to_owned())]
    );
    let providers: Vec<(String, String, String, serde_json::Value)> = result["sourceControlProviders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["kind"].as_str().unwrap().into(),
                item["status"].as_str().unwrap().into(),
                item["auth"]["status"].as_str().unwrap().into(),
                item["auth"]["account"].clone(),
            )
        })
        .collect();
    let none = json!({"_tag": "None"});
    assert_eq!(
        providers,
        [
            (
                "github".to_owned(),
                "available".to_owned(),
                "authenticated".to_owned(),
                json!({"_tag": "Some", "value": "octocat"})
            ),
            ("gitlab".to_owned(), "missing".to_owned(), "unknown".to_owned(), none.clone()),
            ("azure-devops".to_owned(), "missing".to_owned(), "unknown".to_owned(), none.clone()),
            ("bitbucket".to_owned(), "available".to_owned(), "unauthenticated".to_owned(), none.clone()),
            ("forgejo".to_owned(), "missing".to_owned(), "unknown".to_owned(), none),
        ]
    );
    let bitbucket = &result["sourceControlProviders"][3];
    assert!(bitbucket.get("executable").is_none());
    assert_eq!(
        result["sourceControlProviders"][0]["version"],
        json!({"_tag": "Some", "value": "gh version 2.83.0"})
    );
    assert_eq!(
        result["sourceControlProviders"][1]["auth"]["detail"],
        json!({"_tag": "Some", "value": "Hosting integration command was not found on the server PATH."})
    );
}

#[tokio::test]
async fn probes_provider_authentication_without_exposing_token_details() {
    let clis = FakeClis::new();
    for name in ["gh", "glab", "az", "tea"] {
        clis.respond(name, "--version", &format!("{name} version test\n"), "", 0);
    }
    clis.respond(
        "gh",
        "auth status --json hosts",
        &json!({"hosts": {"github.com": [{"state": "success", "active": true, "host": "github.com", "login": "octocat", "tokenSource": "keyring", "gitProtocol": "ssh"}]}}).to_string(),
        "",
        0,
    );
    clis.respond(
        "glab",
        "auth status",
        "gitlab.com\nLogged in to gitlab.com as gitlab-user\n  ✓ Token: glpat-secret\n",
        "",
        0,
    );
    clis.respond("az", "account show --query user.name -o tsv", "azure-user@example.test\n", "", 0);
    clis.respond(
        "tea",
        "login status --output json",
        &json!([{"name": "forgejo", "url": "https://forgejo.example.test", "ssh_host": "forgejo.example.test", "user": "forgejo-user", "valid": "true", "default": "true"}]).to_string(),
        "",
        0,
    );
    let world = make_world(clis);
    let result = serde_json::to_value(world.source_control.discovery.discover().await).unwrap();
    let auth: Vec<(String, String, serde_json::Value, serde_json::Value)> = result["sourceControlProviders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["kind"].as_str().unwrap().into(),
                item["auth"]["status"].as_str().unwrap().into(),
                item["auth"]["account"].clone(),
                item["auth"]["detail"].clone(),
            )
        })
        .collect();
    let some = |v: &str| json!({"_tag": "Some", "value": v});
    let none = json!({"_tag": "None"});
    assert_eq!(auth[0], ("github".into(), "authenticated".into(), some("octocat"), none.clone()));
    assert_eq!(auth[1], ("gitlab".into(), "authenticated".into(), some("gitlab-user"), none.clone()));
    assert_eq!(
        auth[2],
        ("azure-devops".into(), "authenticated".into(), some("azure-user@example.test"), none.clone())
    );
    assert_eq!(auth[4], ("forgejo".into(), "authenticated".into(), some("forgejo-user"), none));
    assert!(!result.to_string().contains("glpat-secret"));
}

#[tokio::test]
async fn resolves_unknown_status_providers_through_the_registry_hook() {
    let clis = FakeClis::new();
    clis.respond(
        "glab",
        "auth status",
        "self-hosted.example.test\n  ✓ Logged in to self-hosted.example.test as gitlab-user\n",
        "",
        0,
    );
    let world = make_world(clis);
    let repo = repo_with_remotes(&[("origin", "https://self-hosted.example.test/group/project.git")]);
    let hooked = Arc::new(WithSourceControlProviders::new(Arc::new(NoPullRequests), world.source_control.registry.clone()));
    let status = GitStatusService::new(world.git.clone(), hooked).local_status(repo.str()).await.unwrap();
    let provider = status.source_control_provider.unwrap();
    assert_eq!(
        serde_json::to_value(&provider).unwrap(),
        json!({"kind": "gitlab", "name": "GitLab Self-Hosted", "baseUrl": "https://self-hosted.example.test"})
    );
    let plain = GitStatusService::new(world.git.clone(), Arc::new(NoPullRequests))
        .local_status(repo.str())
        .await
        .unwrap();
    assert_eq!(serde_json::to_value(plain.source_control_provider.unwrap()).unwrap()["kind"], "unknown");
}
