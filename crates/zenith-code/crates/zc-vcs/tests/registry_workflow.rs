//! Ports of `vcs/VcsDriverRegistry.test.ts`, `vcs/VcsProjectConfig.test.ts`,
//! `vcs/VcsProvisioningService.test.ts` and `git/GitWorkflowService.test.ts`.

#![allow(clippy::result_large_err, clippy::type_complexity)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use zc_core::process::{ProcessRunError, ProcessRunInput, ProcessRunOutput, ProcessRunner};
use zc_core::vcs_process::{VcsProcess, VcsProcessInput, VcsProcessOutput};
use zc_ports::git::GitRunStackedActionOptions;
use zc_vcs::contracts::*;
use zc_vcs::errors::{GitManagerServiceError, VcsError, VcsRepositoryDetectionError};
use zc_vcs::registry::{VcsDriverRegistry, VcsProjectConfig, VcsProjectConfigError, VcsProjectConfigOperation, VcsProvisioningService};
use zc_vcs::status::{GitStatusService, NoPullRequests, RemoteStatusOptions};
use zc_vcs::vcs_driver::{GitVcsProcessDriver, VcsDriver};
use zc_vcs::workflow::{GitManagerBackend, GitWorkflowService, StatusOnlyGitManager};

/// Answers git commands from a closure (`Layer.mock(VcsProcess.VcsProcess)`).
struct ScriptedRunner {
    calls: Mutex<Vec<String>>,
    answer: Box<dyn Fn(&str) -> (i32, String) + Send + Sync>,
}

#[async_trait]
impl ProcessRunner for ScriptedRunner {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        // Drop the `-C <cwd>` prefix like `normalizeGitArgs`.
        let args = if input.args.first().map(String::as_str) == Some("-C") {
            &input.args[2..]
        } else {
            &input.args[..]
        };
        let command = args.join(" ");
        self.calls.lock().unwrap().push(command.clone());
        let (code, stdout) = (self.answer)(&command);
        Ok(ProcessRunOutput {
            stdout,
            stderr: if code == 0 { String::new() } else { "fatal: not a git repository".into() },
            code: Some(code),
            ..ProcessRunOutput::default()
        })
    }
}

fn scripted(answer: impl Fn(&str) -> (i32, String) + Send + Sync + 'static) -> (Arc<ScriptedRunner>, Arc<dyn VcsDriver>) {
    let runner = Arc::new(ScriptedRunner {
        calls: Mutex::new(Vec::new()),
        answer: Box::new(answer),
    });
    let driver = Arc::new(GitVcsProcessDriver::new(VcsProcess::new(runner.clone())));
    (runner, driver)
}

fn repo_answers(command: &str) -> (i32, String) {
    match command {
        "rev-parse --is-inside-work-tree" => (0, "true\n".into()),
        "rev-parse --show-toplevel" => (0, "/repo\n".into()),
        "rev-parse --git-common-dir" => (0, "/repo/.git\n".into()),
        _ => (0, String::new()),
    }
}

// ---------------------------------------------------------------------------------------------
// registry
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn routes_directly_by_vcs_driver_kind() {
    let (_runner, git) = scripted(|_| (0, String::new()));
    let registry = VcsDriverRegistry::new(VcsProjectConfig::new(), git);
    assert_eq!(registry.get(VcsDriverKind::Git).unwrap().capabilities().kind, VcsDriverKind::Git);
    let Err(VcsError::UnsupportedOperation(error)) = registry.get(VcsDriverKind::Jj) else {
        panic!("expected an unsupported-operation error");
    };
    assert_eq!(error.operation, "VcsDriverRegistry.get");
    assert_eq!(error.detail, "No jj VCS driver is registered.");
}

#[tokio::test]
async fn caches_detection_for_repeated_resolves_in_the_same_cwd_and_kind() {
    let (runner, git) = scripted(repo_answers);
    let registry = VcsDriverRegistry::new(VcsProjectConfig::new(), git);
    let git_kind = Some(RequestedVcsKind::Kind(VcsDriverKind::Git));
    let first = registry.resolve("/repo", git_kind).await.unwrap();
    let second = registry.resolve("/repo", git_kind).await.unwrap();
    assert_eq!(first.repository.root_path, "/repo");
    assert_eq!(second.repository.root_path, "/repo");
    assert_eq!(first.repository.metadata_path.as_deref(), Some("/repo/.git"));
    assert_eq!(
        *runner.calls.lock().unwrap(),
        vec!["rev-parse --is-inside-work-tree", "rev-parse --show-toplevel", "rev-parse --git-common-dir"]
    );
}

#[tokio::test]
async fn detects_a_repository_created_after_a_negative_lookup() {
    let checks = Arc::new(AtomicUsize::new(0));
    let counter = checks.clone();
    let (_runner, git) = scripted(move |command| {
        if command == "rev-parse --is-inside-work-tree" {
            return if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                (128, String::new())
            } else {
                (0, "true\n".into())
            };
        }
        repo_answers(command)
    });
    let registry = VcsDriverRegistry::new(VcsProjectConfig::new(), git);
    assert!(registry.detect("/repo", None).await.unwrap().is_none());
    assert_eq!(registry.detect("/repo", None).await.unwrap().unwrap().repository.root_path, "/repo");
    assert_eq!(checks.load(Ordering::SeqCst), 2);
    let error = VcsDriverRegistry::new(VcsProjectConfig::new(), scripted(|_| (128, String::new())).1)
        .resolve("/nowhere", None)
        .await
        .unwrap_err();
    let VcsError::UnsupportedOperation(error) = error else { panic!() };
    assert_eq!(error.kind, VcsDriverKind::Unknown);
    assert_eq!(error.detail, "No supported VCS repository was detected at /nowhere.");
}

// ---------------------------------------------------------------------------------------------
// project config
// ---------------------------------------------------------------------------------------------

fn capturing_config() -> (VcsProjectConfig, Arc<Mutex<Vec<VcsProjectConfigError>>>) {
    let errors: Arc<Mutex<Vec<VcsProjectConfigError>>> = Default::default();
    let sink = errors.clone();
    (
        VcsProjectConfig::with_logger(Arc::new(move |error| sink.lock().unwrap().push(error.clone()))),
        errors,
    )
}

#[tokio::test]
async fn returns_the_requested_kind_before_config() {
    let config = VcsProjectConfig::new();
    assert_eq!(
        config.resolve_kind("/repo", Some(RequestedVcsKind::Kind(VcsDriverKind::Jj))).await,
        RequestedVcsKind::Kind(VcsDriverKind::Jj)
    );
}

#[tokio::test]
async fn discovers_vcs_json_from_nested_workspaces() {
    let root = Tmp::new("t3-vcs-config-test-");
    write(&root.path, ".t3code/vcs.json", r#"{"vcs":{"kind":"jj"}}"#);
    let nested = root.path.join("packages/app");
    std::fs::create_dir_all(&nested).unwrap();
    let config = VcsProjectConfig::new();
    assert_eq!(
        config.resolve_kind(nested.to_str().unwrap(), None).await,
        RequestedVcsKind::Kind(VcsDriverKind::Jj)
    );
    write(&root.path, ".t3code/vcs.json", r#"{ "vcsKind": "git", /* lenient */ }"#);
    assert_eq!(
        config.resolve_kind(root.str(), Some(RequestedVcsKind::Auto)).await,
        RequestedVcsKind::Kind(VcsDriverKind::Git)
    );
}

#[tokio::test]
async fn continues_to_parent_configs_after_a_candidate_inspect_failure() {
    let root = Tmp::new("t3-vcs-config-test-");
    write(&root.path, ".t3code/vcs.json", r#"{"vcs":{"kind":"jj"}}"#);
    let cwd = format!("{}/invalid\0child", root.str());
    let (config, errors) = capturing_config();
    assert_eq!(config.resolve_kind(&cwd, None).await, RequestedVcsKind::Kind(VcsDriverKind::Jj));
    let errors = errors.lock().unwrap();
    let failed = format!("{cwd}/.t3code/vcs.json");
    assert_eq!(errors[0].operation, VcsProjectConfigOperation::Inspect);
    assert_eq!(errors[0].cwd, cwd);
    assert_eq!(errors[0].config_path, failed);
    assert_eq!(errors[0].message(), format!("Failed to inspect VCS project config at {failed}."));
}

#[tokio::test]
async fn falls_back_to_auto_without_config_or_with_a_bad_one() {
    let root = Tmp::new("t3-vcs-config-test-");
    let (config, errors) = capturing_config();
    assert_eq!(config.resolve_kind(root.str(), None).await, RequestedVcsKind::Auto);
    assert!(errors.lock().unwrap().is_empty());

    write(&root.path, ".t3code/vcs.json", "{not json");
    assert_eq!(config.resolve_kind(root.str(), None).await, RequestedVcsKind::Auto);
    {
        let errors = errors.lock().unwrap();
        assert_eq!(errors[0].operation, VcsProjectConfigOperation::Decode);
        assert_eq!(errors[0].config_path, root.join(".t3code/vcs.json"));
        assert_eq!(
            errors[0].message(),
            format!("Failed to decode VCS project config at {}.", root.join(".t3code/vcs.json"))
        );
    }

    write(&root.path, ".t3code/vcs.json", r#"{"vcs":{"kind":"svn"}}"#);
    assert_eq!(config.resolve_kind(root.str(), None).await, RequestedVcsKind::Auto);

    let unreadable = Tmp::new("t3-vcs-config-test-");
    std::fs::create_dir_all(unreadable.path.join(".t3code/vcs.json")).unwrap();
    let (config, errors) = capturing_config();
    assert_eq!(config.resolve_kind(unreadable.str(), None).await, RequestedVcsKind::Auto);
    let errors = errors.lock().unwrap();
    assert_eq!(errors[0].operation, VcsProjectConfigOperation::Read);
    assert_eq!(errors[0].config_path, unreadable.join(".t3code/vcs.json"));
}

// ---------------------------------------------------------------------------------------------
// provisioning
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn provisions_git_explicitly_by_default_and_refuses_unknown() {
    let (runner, git) = scripted(|_| (0, String::new()));
    let provisioning = VcsProvisioningService::new(VcsDriverRegistry::new(VcsProjectConfig::new(), git));
    provisioning
        .init_repository(&VcsInitInput {
            cwd: "/repo".into(),
            kind: Some(VcsDriverKind::Git),
        })
        .await
        .unwrap();
    provisioning
        .init_repository(&VcsInitInput {
            cwd: "/other".into(),
            kind: None,
        })
        .await
        .unwrap();
    assert_eq!(*runner.calls.lock().unwrap(), vec!["init", "init"]);
    let error = provisioning
        .init_repository(&VcsInitInput {
            cwd: "/repo".into(),
            kind: Some(VcsDriverKind::Unknown),
        })
        .await
        .unwrap_err();
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        serde_json::json!({
            "_tag": "VcsUnsupportedOperationError",
            "operation": "VcsProvisioningService.resolveRequestedKind",
            "kind": "unknown",
            "detail": "A concrete VCS driver kind is required for repository provisioning."
        })
    );
    let jj = provisioning
        .init_repository(&VcsInitInput {
            cwd: "/repo".into(),
            kind: Some(VcsDriverKind::Jj),
        })
        .await
        .unwrap_err();
    assert_eq!(jj.tag(), "VcsUnsupportedOperationError");
}

// ---------------------------------------------------------------------------------------------
// workflow
// ---------------------------------------------------------------------------------------------

/// A driver whose detection is scripted (`Layer.mock(VcsDriverRegistry)({detect})`).
struct DetectingDriver {
    kind: VcsDriverKind,
    detect: Box<dyn Fn() -> Result<Option<VcsRepositoryIdentity>, VcsError> + Send + Sync>,
}

#[async_trait]
impl VcsDriver for DetectingDriver {
    fn capabilities(&self) -> VcsDriverCapabilities {
        VcsDriverCapabilities {
            kind: self.kind,
            supports_worktrees: false,
            supports_bookmarks: true,
            supports_atomic_snapshot: true,
            supports_push_default_remote: false,
            ignore_classifier: "git-compatible-fallback".into(),
        }
    }
    async fn execute(&self, _input: VcsProcessInput) -> Result<VcsProcessOutput, VcsError> {
        unimplemented!()
    }
    async fn detect_repository(&self, _cwd: &str) -> Result<Option<VcsRepositoryIdentity>, VcsError> {
        (self.detect)()
    }
    async fn is_inside_work_tree(&self, _cwd: &str) -> Result<bool, VcsError> {
        unimplemented!()
    }
    async fn list_workspace_files(&self, _cwd: &str) -> Result<VcsListWorkspaceFilesResult, VcsError> {
        unimplemented!()
    }
    async fn list_remotes(&self, _cwd: &str) -> Result<VcsListRemotesResult, VcsError> {
        unimplemented!()
    }
    async fn filter_ignored_paths(&self, _cwd: &str, _paths: &[String]) -> Result<Vec<String>, VcsError> {
        unimplemented!()
    }
    async fn init_repository(&self, _input: &VcsInitInput) -> Result<(), VcsError> {
        unimplemented!()
    }
}

fn identity(kind: VcsDriverKind, root: &str) -> VcsRepositoryIdentity {
    VcsRepositoryIdentity {
        kind,
        root_path: root.into(),
        metadata_path: None,
        freshness: VcsFreshness::live_local_now(),
    }
}

/// Counts the GitManager calls (`vi.fn()` mocks).
#[derive(Default)]
struct CountingManager {
    calls: AtomicUsize,
}

#[async_trait]
impl GitManagerBackend for CountingManager {
    async fn status(&self, _cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        unimplemented!()
    }
    async fn local_status(&self, _cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        unimplemented!()
    }
    async fn remote_status(&self, _cwd: &str, _options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        unimplemented!()
    }
    async fn invalidate_local_status(&self, _cwd: &str) {}
    async fn invalidate_remote_status(&self, _cwd: &str) {}
    async fn invalidate_status(&self, _cwd: &str) {}
    async fn run_stacked_action(&self, _input: serde_json::Value, _options: GitRunStackedActionOptions) -> Result<serde_json::Value, GitManagerServiceError> {
        unimplemented!()
    }
    async fn resolve_pull_request(&self, _input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError> {
        unimplemented!()
    }
    async fn prepare_pull_request_thread(&self, _input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError> {
        unimplemented!()
    }
    async fn branch_pull_request(
        &self,
        _cwd: &str,
        _branch: &str,
        _refresh: bool,
    ) -> Result<Option<zc_ports::git::GitBranchPullRequest>, GitManagerServiceError> {
        unimplemented!()
    }
}

fn workflow_with(drivers: Vec<(VcsDriverKind, Arc<dyn VcsDriver>)>, manager: Arc<dyn GitManagerBackend>) -> GitWorkflowService {
    let (git, _w) = driver();
    GitWorkflowService::new(VcsDriverRegistry::with_drivers(VcsProjectConfig::new(), drivers), git, manager)
}

fn detecting(kind: VcsDriverKind, detect: impl Fn() -> Result<Option<VcsRepositoryIdentity>, VcsError> + Send + Sync + 'static) -> Arc<dyn VcsDriver> {
    Arc::new(DetectingDriver {
        kind,
        detect: Box::new(detect),
    })
}

#[tokio::test]
async fn reports_a_non_git_vcs_repository_as_not_a_git_repository() {
    let root = Tmp::new("t3-jj-repo-");
    write(&root.path, ".t3code/vcs.json", r#"{"vcsKind":"jj"}"#);
    let jj_root = root.str().to_owned();
    let workflow = workflow_with(
        vec![
            (VcsDriverKind::Git, detecting(VcsDriverKind::Git, || Ok(None))),
            (
                VcsDriverKind::Jj,
                detecting(VcsDriverKind::Jj, move || Ok(Some(identity(VcsDriverKind::Jj, &jj_root)))),
            ),
        ],
        Arc::new(CountingManager::default()),
    );
    assert!(!workflow.is_repository(root.str()).await.unwrap());
    let error = workflow.status(root.str()).await.unwrap_err();
    let GitManagerServiceError::Manager(error) = error else { panic!() };
    assert_eq!(
        error.detail,
        format!(
            "The GitWorkflowService.status workflow currently supports Git repositories only; detected jj. ({})",
            root.str()
        )
    );
    let command = workflow
        .list_refs(&VcsListRefsInput {
            cwd: root.str().into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(command.command, "vcs-route");
    assert_eq!(
        command.detail,
        "The GitWorkflowService.listRefs command currently supports Git repositories only; detected jj."
    );
}

#[tokio::test]
async fn returns_empty_results_without_calling_git_manager_outside_a_repository() {
    let manager = Arc::new(CountingManager::default());
    let workflow = workflow_with(vec![(VcsDriverKind::Git, detecting(VcsDriverKind::Git, || Ok(None)))], manager.clone());
    let local = workflow.local_status("/not-a-repo").await.unwrap();
    assert_eq!(
        serde_json::to_value(&local).unwrap(),
        serde_json::json!({
            "isRepo": false,
            "hasPrimaryRemote": false,
            "isDefaultRef": false,
            "refName": null,
            "hasWorkingTreeChanges": false,
            "workingTree": {"files": [], "insertions": 0, "deletions": 0}
        })
    );
    assert_eq!(workflow.remote_status("/not-a-repo", RemoteStatusOptions::default()).await.unwrap(), None);
    let status = workflow.status("/not-a-repo").await.unwrap();
    assert_eq!(
        serde_json::to_value(&status).unwrap(),
        serde_json::json!({
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
            "pr": null
        })
    );
    let refs = workflow
        .list_refs(&VcsListRefsInput {
            cwd: "/not-a-repo".into(),
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&refs).unwrap(),
        serde_json::json!({"refs": [], "isRepo": false, "hasPrimaryRemote": false, "nextCursor": null, "totalCount": 0})
    );
    assert_eq!(manager.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn structures_detection_failures_without_exposing_upstream_details() {
    let failing = || {
        Err(VcsError::RepositoryDetection(VcsRepositoryDetectionError {
            operation: "VcsDriverRegistry.detect".into(),
            cwd: "/repo".into(),
            detail: "upstream detail must stay in the cause chain".into(),
            cause: None,
        }))
    };
    let workflow = workflow_with(
        vec![(VcsDriverKind::Git, detecting(VcsDriverKind::Git, failing))],
        Arc::new(CountingManager::default()),
    );
    let error = workflow.status("/repo").await.unwrap_err();
    let encoded = serde_json::to_value(&error).unwrap();
    assert_eq!(encoded["_tag"], "GitManagerError");
    assert_eq!(encoded["operation"], "GitWorkflowService.status");
    assert_eq!(encoded["cwd"], "/repo");
    assert_eq!(encoded["detail"], "Failed to detect a VCS repository for this Git workflow.");
    assert_eq!(encoded["cause"]["name"], "VcsRepositoryDetectionError");
    assert!(!error.message().contains("upstream detail"));

    let command = workflow
        .list_refs(&VcsListRefsInput {
            cwd: "/repo".into(),
            ..Default::default()
        })
        .await
        .unwrap_err();
    let encoded = serde_json::to_value(&command).unwrap();
    assert_eq!(encoded["_tag"], "GitCommandError");
    assert_eq!(encoded["operation"], "GitWorkflowService.listRefs");
    assert_eq!(encoded["command"], "vcs-route");
    assert_eq!(encoded["detail"], "Failed to detect a VCS repository for this Git command.");
    assert!(!command.to_string().contains("upstream command detail"));
}

#[tokio::test]
async fn implements_the_git_workflow_port_on_a_real_repository() {
    let cwd = Tmp::new("git-workflow-port-");
    let branch = init_repo_with_commit(&cwd.path);
    let (git, _w) = driver();
    let registry = VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::default()));
    let manager = Arc::new(StatusOnlyGitManager::new(GitStatusService::new(git.clone(), Arc::new(NoPullRequests))));
    let workflow = GitWorkflowService::new(registry, git, manager);
    let port: &dyn zc_ports::GitWorkflow = &workflow;
    assert!(port.is_repository(cwd.str()).await.unwrap());
    assert!(port.has_commit(cwd.str(), &branch).await.unwrap());
    assert!(!port.has_commit(cwd.str(), "nope").await.unwrap());
    let status = port
        .status(zc_ports::contracts::VcsStatusInput(serde_json::json!({"cwd": cwd.str()})))
        .await
        .unwrap();
    assert_eq!(status.0["refName"], serde_json::json!(branch));
    assert_eq!(status.0["pr"], serde_json::Value::Null);
    let refs = port
        .list_refs(zc_ports::contracts::VcsListRefsInput(serde_json::json!({"cwd": cwd.str()})))
        .await
        .unwrap();
    assert_eq!(refs.0["refs"][0]["name"], serde_json::json!(branch));
    assert_eq!(port.rename_branch(cwd.str(), &branch, "renamed").await.unwrap(), "renamed");
    let not_yet = port
        .resolve_pull_request(zc_ports::contracts::GitPullRequestRefInput(
            serde_json::json!({"cwd": cwd.str(), "reference": "1"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(not_yet.tag, "GitManagerError");
    assert!(not_yet.fields["detail"].as_str().unwrap().contains("not available without a GitManager"));
    let branch_pr = port.branch_pull_request(cwd.str(), "renamed", false).await.unwrap_err();
    assert_eq!(branch_pr.tag, "GitManagerError");
}
