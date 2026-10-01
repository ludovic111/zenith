//! `SourceControlRepositoryService.test.ts`, `PrTemplateDetection.test.ts` and the RPC
//! handlers, on real repositories (clones and pushes go to local bare repositories).

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use futures::FutureExt;
use serde_json::{json, Value};
use zc_contracts::{
    ChangeRequest, SourceControlCloneProtocol, SourceControlCloneRepositoryInput, SourceControlProviderAuth, SourceControlProviderAuthStatus,
    SourceControlProviderKind, SourceControlPublishRepositoryInput, SourceControlPublishStatus, SourceControlRepositoryCloneUrls,
    SourceControlRepositoryLookupInput, SourceControlRepositoryVisibility,
};
use zc_core::vcs_process::VcsProcess;
use zc_sourcecontrol::clone_progress::GitCloneProgressLine;
use zc_sourcecontrol::discovery::{ApiDiscoverySpec, DiscoverySpec};
use zc_sourcecontrol::errors::SourceControlProviderError;
use zc_sourcecontrol::pr_template::detect_pr_template;
use zc_sourcecontrol::provider::*;
use zc_sourcecontrol::registry::{SourceControlProviderRegistration, SourceControlProviderRegistry};
use zc_sourcecontrol::repository::{SourceControlCloneOptions, SourceControlRepositoryService};
use zc_sourcecontrol::rpc::{self, SourceControlRpcServices};
use zc_sourcecontrol::source_control_discovery::SourceControlDiscovery;
use zc_sourcecontrol::util::ManualClock;
use zc_vcs::registry::{VcsDriverRegistry, VcsProjectConfig};
use zc_vcs::vcs_driver::GitVcsProcessDriver;
use zc_vcs::GitVcsDriver;

/// A GitHub-kind provider answering lookups and creations with fixed URLs.
struct FakeProvider {
    urls: SourceControlRepositoryCloneUrls,
    fail_with: Option<SourceControlProviderError>,
    calls: Mutex<Vec<(String, String, String)>>,
}

#[async_trait]
impl SourceControlProvider for FakeProvider {
    fn kind(&self) -> SourceControlProviderKind {
        SourceControlProviderKind::Github
    }
    async fn list_change_requests(&self, _: ListChangeRequestsInput) -> Result<Vec<ChangeRequest>, SourceControlProviderError> {
        unreachable!()
    }
    async fn get_change_request(&self, _: GetChangeRequestInput) -> Result<ChangeRequest, SourceControlProviderError> {
        unreachable!()
    }
    async fn create_change_request(&self, _: CreateChangeRequestInput) -> Result<(), SourceControlProviderError> {
        unreachable!()
    }
    async fn get_repository_clone_urls(&self, input: RepositoryCloneUrlsInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.calls.lock().unwrap().push(("lookup".into(), input.cwd, input.repository));
        match &self.fail_with {
            Some(error) => Err(error.clone()),
            None => Ok(self.urls.clone()),
        }
    }
    async fn create_repository(&self, input: CreateRepositoryInput) -> Result<SourceControlRepositoryCloneUrls, SourceControlProviderError> {
        self.calls
            .lock()
            .unwrap()
            .push(("create".into(), input.cwd, format!("{} {}", input.repository, input.visibility.as_str())));
        Ok(self.urls.clone())
    }
    async fn get_default_branch(&self, _: DefaultBranchInput) -> Result<Option<String>, SourceControlProviderError> {
        Ok(None)
    }
    async fn checkout_change_request(&self, _: CheckoutChangeRequestInput) -> Result<(), SourceControlProviderError> {
        unreachable!()
    }
}

struct Setup {
    service: SourceControlRepositoryService,
    provider: Arc<FakeProvider>,
    registry: SourceControlProviderRegistry,
    _worktrees: Tmp,
}

fn urls(url: &str, ssh_url: &str) -> SourceControlRepositoryCloneUrls {
    SourceControlRepositoryCloneUrls {
        name_with_owner: "octocat/demo".into(),
        url: url.into(),
        ssh_url: ssh_url.into(),
    }
}

fn setup(urls: SourceControlRepositoryCloneUrls, fail_with: Option<SourceControlProviderError>) -> Setup {
    let worktrees = Tmp::new("zc-repo-worktrees-");
    let git = GitVcsDriver::new(&worktrees.path);
    let provider = Arc::new(FakeProvider {
        urls,
        fail_with,
        calls: Mutex::default(),
    });
    let process = VcsProcess::default();
    let registry = SourceControlProviderRegistry::new(
        vec![SourceControlProviderRegistration {
            kind: SourceControlProviderKind::Github,
            provider: provider.clone(),
            discovery: DiscoverySpec::Api(ApiDiscoverySpec {
                kind: SourceControlProviderKind::Github,
                label: "GitHub".into(),
                install_hint: "none".into(),
                probe_auth: Arc::new(|| {
                    async {
                        SourceControlProviderAuth {
                            status: SourceControlProviderAuthStatus::Unknown,
                            account: Default::default(),
                            host: Default::default(),
                            detail: Default::default(),
                        }
                    }
                    .boxed()
                }),
            }),
        }],
        process.clone(),
        VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::new(process))),
        "/server-cwd",
        ManualClock::new(0),
    );
    Setup {
        service: SourceControlRepositoryService::new("/server-cwd", git, registry.clone()),
        provider,
        registry,
        _worktrees: worktrees,
    }
}

/// A bare repository with one commit on `main`, and its path.
fn bare_repository() -> (Tmp, Tmp) {
    let source = Tmp::new("zc-repo-source-");
    init_repo_with_commit(&source.path);
    let bare = Tmp::new("zc-repo-bare-");
    git(&bare.path, &["clone", "--bare", "-q", source.str(), "."]);
    (source, bare)
}

fn clone_input(destination: &str) -> SourceControlCloneRepositoryInput {
    SourceControlCloneRepositoryInput {
        provider: None,
        repository: None,
        remote_url: None,
        destination_path: destination.into(),
        protocol: None,
    }
}

#[tokio::test]
async fn looks_up_repositories_through_the_requested_provider() {
    let s = setup(urls("https://github.com/octocat/demo", "git@github.com:octocat/demo.git"), None);
    let info = s
        .service
        .lookup_repository(&SourceControlRepositoryLookupInput {
            provider: SourceControlProviderKind::Github,
            repository: " octocat/demo ".into(),
            cwd: Some("/workspace".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&info).unwrap(),
        json!({"provider": "github", "nameWithOwner": "octocat/demo", "url": "https://github.com/octocat/demo", "sshUrl": "git@github.com:octocat/demo.git"})
    );
    assert_eq!(
        s.provider.calls.lock().unwrap().clone(),
        [("lookup".to_owned(), "/workspace".to_owned(), "octocat/demo".to_owned())]
    );
    let unknown = s
        .service
        .lookup_repository(&SourceControlRepositoryLookupInput {
            provider: SourceControlProviderKind::Unknown,
            repository: "octocat/demo".into(),
            cwd: None,
        })
        .await
        .unwrap_err();
    assert_eq!(unknown.detail, "Choose a source control provider before continuing.");
    assert!(unknown.cause.is_none());
}

#[tokio::test]
async fn preserves_provider_failures_without_deriving_the_message_from_them() {
    let cause = SourceControlProviderError::new(
        SourceControlProviderKind::Github,
        "getRepositoryCloneUrls",
        "/workspace",
        "credential token abc123 was rejected",
    )
    .with_repository("octocat/demo");
    let s = setup(urls("u", "s"), Some(cause));
    let error = s
        .service
        .lookup_repository(&SourceControlRepositoryLookupInput {
            provider: SourceControlProviderKind::Github,
            repository: "octocat/demo".into(),
            cwd: Some("/workspace".into()),
        })
        .await
        .unwrap_err();
    assert_eq!(
        (error.provider, error.operation.as_str()),
        (SourceControlProviderKind::Github, "lookupRepository")
    );
    assert_eq!(error.detail, "The source control operation could not be completed.");
    assert_eq!(
        error.message(),
        "Source control repository operation lookupRepository failed for github: The source control operation could not be completed."
    );
    let encoded = serde_json::to_value(&error).unwrap();
    assert_eq!(encoded["cause"]["name"], "SourceControlProviderError");
    assert_eq!(
        encoded["cause"]["message"],
        "Source control provider github failed in getRepositoryCloneUrls: credential token abc123 was rejected"
    );
}

#[tokio::test]
async fn clones_a_looked_up_repository_into_the_requested_destination() {
    let (_source, bare) = bare_repository();
    let s = setup(urls(bare.str(), "git@github.com:octocat/demo.git"), None);
    let parent = Tmp::new("zc-repo-clone-parent-");
    let destination = parent.join("demo");
    let result = s
        .service
        .clone_repository(
            &SourceControlCloneRepositoryInput {
                provider: Some(SourceControlProviderKind::Github),
                repository: Some("octocat/demo".into()),
                protocol: Some(SourceControlCloneProtocol::Https),
                ..clone_input(&destination)
            },
            &SourceControlCloneOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({"cwd": destination, "remoteUrl": bare.str(), "repository": {"provider": "github", "nameWithOwner": "octocat/demo", "url": bare.str(), "sshUrl": "git@github.com:octocat/demo.git"}})
    );
    assert_eq!(git(&destination, &["config", "remote.origin.url"]), bare.str());
    // The lookup ran in the destination's parent.
    assert_eq!(s.provider.calls.lock().unwrap()[0].1, parent.str());
    // A non-empty destination is refused before git runs.
    let error = s
        .service
        .clone_repository(&clone_input(&destination), &SourceControlCloneOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.detail, "Destination path already exists and is not empty.");
}

#[tokio::test]
async fn reports_clone_progress_and_keeps_git_error_text_on_failure() {
    let (_source, bare) = bare_repository();
    let s = setup(urls("u", "s"), None);
    let parent = Tmp::new("zc-repo-progress-");
    let progress: Arc<Mutex<Vec<GitCloneProgressLine>>> = Arc::default();
    let sink = progress.clone();
    let options = SourceControlCloneOptions {
        on_progress: Some(Arc::new(move |line| sink.lock().unwrap().push(line))),
        ..SourceControlCloneOptions::default()
    };
    let mut input = clone_input(&parent.join("demo"));
    input.remote_url = Some(format!("file://{}", bare.str()));
    s.service.clone_repository(&input, &options).await.unwrap();
    let lines = progress.lock().unwrap().clone();
    assert!(
        lines
            .iter()
            .any(|l| l.stage == zc_contracts::ProjectCloneStage::Receiving && l.percent == Some(100)),
        "{lines:?}"
    );

    let mut failing = clone_input(&parent.join("missing"));
    failing.remote_url = Some(format!("file://user:s3c@ret@{}/does-not-exist.git", "localhost"));
    let error = s.service.clone_repository(&failing, &SourceControlCloneOptions::default()).await.unwrap_err();
    assert_eq!(error.operation, "cloneRepository");
    assert!(!error.detail.contains("s3c"), "{}", error.detail);
    assert!(
        error.detail.starts_with("fatal:") || error.detail.contains("does-not-exist"),
        "{}",
        error.detail
    );
    assert_eq!(serde_json::to_value(&error).unwrap()["cause"]["name"], "GitCommandError");
}

#[tokio::test]
async fn strips_embedded_credentials_and_query_tokens_from_reported_urls() {
    let s = setup(urls("u", "s"), None);
    let parent = Tmp::new("zc-repo-redact-");
    let mut input = clone_input(&parent.join("a"));
    input.remote_url = Some("https://user:s3cret@github.com/octocat/demo.git".into());
    let prepared = s.service.prepare_clone(&input).await.unwrap();
    assert_eq!(prepared.remote_url, "https://github.com/octocat/demo.git");
    assert_eq!(prepared.clone_url, "https://user:s3cret@github.com/octocat/demo.git");
    input.remote_url = Some("https://github.com/octocat/demo.git?access_token=s3cret".into());
    assert_eq!(s.service.prepare_clone(&input).await.unwrap().remote_url, "https://github.com/octocat/demo.git");
    input.destination_path = parent.join("b");
    input.remote_url = Some("https://user:pa@rt@github.com/octocat/demo.git".into());
    assert_eq!(s.service.prepare_clone(&input).await.unwrap().remote_url, "https://github.com/octocat/demo.git");
    input.remote_url = None;
    let error = s.service.prepare_clone(&input).await.unwrap_err();
    assert_eq!(error.detail, "Enter a repository path or clone URL before cloning.");
    let error = s.service.prepare_clone(&clone_input("  ")).await.unwrap_err();
    assert_eq!(error.detail, "Choose a destination path before cloning.");
}

#[tokio::test]
async fn discards_only_a_directory_git_wrote_to() {
    let s = setup(urls("u", "s"), None);
    let parent = Tmp::new("zc-repo-discard-");
    write(&parent.path, "partial/.git/HEAD", "ref: refs/heads/main\n");
    write(&parent.path, "partial/README.md", "half");
    write(&parent.path, "foreign/notes.txt", "mine");
    write(&parent.path, "replaced", "not a directory");
    s.service.discard_clone(&parent.join("partial")).await.unwrap();
    let error = s.service.discard_clone(&parent.join("foreign")).await.unwrap_err();
    assert!(error.detail.contains("not from the clone"));
    let error = s.service.discard_clone(&parent.join("replaced")).await.unwrap_err();
    assert!(error.detail.contains("could not be inspected"));
    s.service.discard_clone(&parent.join("missing")).await.unwrap();
    assert_eq!(std::fs::read_dir(parent.path.join("partial")).unwrap().count(), 0);
    assert_eq!(std::fs::read_to_string(parent.path.join("foreign/notes.txt")).unwrap(), "mine");
    assert_eq!(std::fs::read_to_string(parent.path.join("replaced")).unwrap(), "not a directory");
}

#[tokio::test]
async fn preserves_destination_probe_failures() {
    use std::os::unix::fs::PermissionsExt;
    let s = setup(urls("u", "s"), None);
    let parent = Tmp::new("zc-repo-restricted-");
    let restricted = parent.path.join("restricted");
    std::fs::create_dir(&restricted).unwrap();
    std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut input = clone_input(&restricted.join("demo").to_string_lossy());
    input.remote_url = Some("git@github.com:octocat/demo.git".into());
    let result = s.service.clone_repository(&input, &SourceControlCloneOptions::default()).await;
    std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = result.unwrap_err();
    assert_eq!(
        (error.provider, error.operation.as_str()),
        (SourceControlProviderKind::Unknown, "cloneRepository")
    );
    assert_eq!(error.detail, "The source control operation could not be completed.");
    assert!(error.cause.is_some());
}

#[tokio::test]
async fn publishes_by_creating_the_repository_adding_a_remote_and_pushing_upstream() {
    let bare = Tmp::new("zc-repo-publish-bare-");
    git(&bare.path, &["init", "--bare", "-q"]);
    let s = setup(urls("https://github.com/octocat/demo", bare.str()), None);
    let local = Tmp::new("zc-repo-publish-");
    init_repo_with_commit(&local.path);
    let result = s
        .service
        .publish_repository(&SourceControlPublishRepositoryInput {
            cwd: local.str().into(),
            provider: SourceControlProviderKind::Github,
            repository: "octocat/demo".into(),
            visibility: SourceControlRepositoryVisibility::Private,
            remote_name: Some("origin".into()),
            protocol: Some(SourceControlCloneProtocol::Ssh),
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        json!({"repository": {"provider": "github", "nameWithOwner": "octocat/demo", "url": "https://github.com/octocat/demo", "sshUrl": bare.str()},
               "remoteName": "origin", "remoteUrl": bare.str(), "branch": "main", "upstreamBranch": "origin/main", "status": "pushed"})
    );
    assert_eq!(
        s.provider.calls.lock().unwrap().clone(),
        [("create".to_owned(), local.str().to_owned(), "octocat/demo private".to_owned())]
    );
    assert_eq!(git(&bare.path, &["rev-parse", "main"]), git(&local.path, &["rev-parse", "HEAD"]));

    // An origin pointing elsewhere: the remote name ensureRemote picks is the one pushed to.
    let other_bare = Tmp::new("zc-repo-publish-other-bare-");
    git(&other_bare.path, &["init", "--bare", "-q"]);
    let s = setup(urls("https://github.com/octocat/demo", other_bare.str()), None);
    let other = Tmp::new("zc-repo-publish-other-");
    init_repo_with_commit(&other.path);
    git(&other.path, &["remote", "add", "origin", "https://example.test/other.git"]);
    let result = s
        .service
        .publish_repository(&SourceControlPublishRepositoryInput {
            cwd: other.str().into(),
            provider: SourceControlProviderKind::Github,
            repository: "octocat/demo".into(),
            visibility: SourceControlRepositoryVisibility::Private,
            remote_name: Some("origin".into()),
            protocol: Some(SourceControlCloneProtocol::Ssh),
        })
        .await
        .unwrap();
    assert_ne!(result.remote_name, "origin");
    assert_eq!(result.upstream_branch.as_deref(), Some(format!("{}/main", result.remote_name).as_str()));
    assert_eq!(result.status, SourceControlPublishStatus::Pushed);
}

#[tokio::test]
async fn publish_succeeds_with_remote_added_when_the_local_repo_has_no_commits() {
    let (_source, bare) = bare_repository();
    let s = setup(urls("https://github.com/octocat/demo", bare.str()), None);
    let empty = Tmp::new("zc-repo-empty-");
    git(&empty.path, &["init", "-b", "main"]);
    let result = s
        .service
        .publish_repository(&SourceControlPublishRepositoryInput {
            cwd: empty.str().into(),
            provider: SourceControlProviderKind::Github,
            repository: "octocat/demo".into(),
            visibility: SourceControlRepositoryVisibility::Private,
            remote_name: Some("origin".into()),
            protocol: Some(SourceControlCloneProtocol::Ssh),
        })
        .await
        .unwrap();
    assert_eq!(
        (result.branch.as_str(), result.status, result.upstream_branch),
        ("main", SourceControlPublishStatus::RemoteAdded, None)
    );
    assert_eq!(git(&empty.path, &["config", "remote.origin.url"]), bare.str());
}

#[tokio::test]
async fn serves_the_rpc_methods_with_wire_json() {
    let (_source, bare) = bare_repository();
    let s = setup(urls(bare.str(), bare.str()), None);
    let published: Arc<Mutex<Vec<String>>> = Arc::default();
    let sink = published.clone();
    let services = SourceControlRpcServices {
        discovery: SourceControlDiscovery::new(VcsProcess::default(), s.registry.clone(), "/"),
        repositories: s.service.clone(),
        after_publish: Some(Arc::new(move |cwd| {
            sink.lock().unwrap().push(cwd);
            async {}.boxed()
        })),
    };
    let router = rpc::register(zc_rpc::RpcRouter::builder(), services.clone()).build().unwrap();
    for tag in [
        "server.discoverSourceControl",
        "sourceControl.lookupRepository",
        "sourceControl.cloneRepository",
        "sourceControl.publishRepository",
    ] {
        assert_eq!(router.is_stream(tag), Some(false), "{tag}");
    }
    let discovered = rpc::discover_source_control(&services).await;
    assert_eq!(serde_json::to_value(&discovered).unwrap()["versionControlSystems"][0]["kind"], "git");

    let parent = Tmp::new("zc-repo-rpc-");
    let payload: SourceControlCloneRepositoryInput =
        serde_json::from_value(json!({"remoteUrl": format!(" {} ", bare.str()), "destinationPath": parent.join("demo")})).unwrap();
    let cloned = rpc::clone_repository(&services, payload).await.ok().unwrap();
    assert_eq!(
        serde_json::to_value(&cloned).unwrap(),
        json!({"cwd": parent.join("demo"), "remoteUrl": bare.str(), "repository": null})
    );

    let failure = rpc::lookup_repository(
        &services,
        serde_json::from_value(json!({"provider": "unknown", "repository": "octocat/demo"})).unwrap(),
    )
    .await
    .err()
    .unwrap();
    let encoded = match failure.into_rpc_error() {
        zc_rpc::RpcError::Fail(value) => value,
        other => panic!("unexpected {other:?}"),
    };
    assert_eq!(
        encoded,
        json!({"_tag": "SourceControlRepositoryError", "provider": "unknown", "operation": "lookupRepository", "detail": "Choose a source control provider before continuing."})
    );
    let empty = rpc::lookup_repository(&services, serde_json::from_value(json!({"provider": "github", "repository": "   "})).unwrap())
        .await
        .err()
        .unwrap();
    assert!(matches!(empty.into_rpc_error(), zc_rpc::RpcError::Die(Value::String(_))));

    // A clone of the remote, so the push is a fast-forward whatever the commit times.
    let local = Tmp::new("zc-repo-rpc-publish-");
    git(&local.path, &["clone", "-q", bare.str(), "."]);
    let payload: SourceControlPublishRepositoryInput =
        serde_json::from_value(json!({"cwd": local.str(), "provider": "github", "repository": "octocat/demo", "visibility": "public"})).unwrap();
    let result = rpc::publish_repository(&services, payload).await.ok().unwrap();
    assert_eq!(result.status, SourceControlPublishStatus::Pushed);
    assert_eq!(published.lock().unwrap().clone(), [local.str().to_owned()]);
}

// PrTemplateDetection.test.ts

fn template_repo() -> (Tmp, GitVcsDriver, Tmp) {
    let repo = Tmp::new("zc-pr-template-");
    git(&repo.path, &["init", "-b", "main"]);
    let worktrees = Tmp::new("zc-pr-template-worktrees-");
    (repo, GitVcsDriver::new(&worktrees.path), worktrees)
}

fn commit(repo: &Tmp) {
    git(&repo.path, &["add", "-A"]);
    git(&repo.path, &["commit", "--allow-empty", "-q", "-m", "Add pull request templates"]);
}

#[tokio::test]
async fn recognizes_every_single_template_path() {
    for path in zc_sourcecontrol::pr_template::TEMPLATE_PATHS {
        let (repo, driver, _w) = template_repo();
        write(&repo.path, path, &format!("template from {path}"));
        commit(&repo);
        assert_eq!(
            detect_pr_template(repo.str(), "HEAD", &driver).await.as_deref(),
            Some(format!("template from {path}").as_str()),
            "{path}"
        );
    }
    for directory in zc_sourcecontrol::pr_template::TEMPLATE_DIRECTORIES {
        let (repo, driver, _w) = template_repo();
        write(&repo.path, &format!("{directory}/template.MD"), "directory template");
        commit(&repo);
        assert_eq!(
            detect_pr_template(repo.str(), "HEAD", &driver).await.as_deref(),
            Some("directory template"),
            "{directory}"
        );
    }
}

#[tokio::test]
async fn reads_templates_from_the_requested_base_tree_in_path_order() {
    let (repo, driver, _w) = template_repo();
    write(&repo.path, "README.md", "initial\n");
    commit(&repo);
    git(&repo.path, &["branch", "feature"]);
    write(&repo.path, ".github/pull_request_template.md", " \n");
    write(&repo.path, ".github/PULL_REQUEST_TEMPLATE.md", "  ## Preferred template  \n");
    write(&repo.path, "pull_request_template.md", "## Later template");
    commit(&repo);
    git(&repo.path, &["checkout", "-q", "feature"]);
    assert_eq!(detect_pr_template(repo.str(), "HEAD", &driver).await, None);
    assert_eq!(detect_pr_template(repo.str(), "main", &driver).await.as_deref(), Some("## Preferred template"));
}

#[tokio::test]
async fn does_not_guess_between_directory_templates_and_skips_unusable_entries() {
    let (repo, driver, _w) = template_repo();
    write(&repo.path, ".github/PULL_REQUEST_TEMPLATE/a.md", "first");
    write(&repo.path, ".github/PULL_REQUEST_TEMPLATE/b.md", "second");
    write(&repo.path, "PULL_REQUEST_TEMPLATE/fallback.md", "fallback");
    commit(&repo);
    assert_eq!(detect_pr_template(repo.str(), "HEAD", &driver).await, None);

    let (repo, driver, _w) = template_repo();
    let directory = repo.path.join(".github/PULL_REQUEST_TEMPLATE");
    write(&repo.path, ".github/PULL_REQUEST_TEMPLATE/b-directory.md/inner.txt", "x");
    write(&repo.path, ".github/PULL_REQUEST_TEMPLATE/a-empty.md", " \n");
    std::os::unix::fs::symlink(directory.join("missing.md"), directory.join("c-broken.md")).unwrap();
    write(&repo.path, ".github/PULL_REQUEST_TEMPLATE/z-valid.md", "valid");
    commit(&repo);
    assert_eq!(detect_pr_template(repo.str(), "HEAD", &driver).await.as_deref(), Some("valid"));
}

#[tokio::test]
async fn never_reads_through_symlinks_and_bounds_template_reads() {
    let (repo, driver, _w) = template_repo();
    let outside = Tmp::new("zc-pr-template-outside-");
    write(&outside.path, "secret.md", "LOCAL_SECRET_SENTINEL");
    std::fs::create_dir_all(repo.path.join(".github")).unwrap();
    std::os::unix::fs::symlink(outside.path.join("secret.md"), repo.path.join(".github/pull_request_template.md")).unwrap();
    write(&repo.path, "pull_request_template.md", "safe template");
    commit(&repo);
    assert_eq!(detect_pr_template(repo.str(), "HEAD", &driver).await.as_deref(), Some("safe template"));

    let (repo, driver, _w) = template_repo();
    let prefix = "a".repeat(8_000);
    write(&repo.path, ".github/pull_request_template.md", &format!("{prefix}SECRET_SENTINEL"));
    commit(&repo);
    let template = detect_pr_template(repo.str(), "HEAD", &driver).await.unwrap();
    assert_eq!(template, format!("{prefix}\n\n[truncated]"));
    assert_eq!(template.matches("[truncated]").count(), 1);
}
