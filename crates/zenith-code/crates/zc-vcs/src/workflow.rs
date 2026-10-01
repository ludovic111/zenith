//! `git/GitWorkflowService.ts`: routes each git workflow through the VCS registry (only git
//! repositories get git operations; a non-repository gets empty results where TS gives them)
//! and implements the [`zc_ports::GitWorkflow`] port.
//!
//! The GitManager half (status with PRs, stacked actions, PR resolve/prepare, a saved branch's
//! PR) sits behind [`GitManagerBackend`], which zc-git's `GitManager` implements (WP-19).
//! [`StatusOnlyGitManager`] is the stand-in for tests that need status only: it reads status
//! through [`GitStatusService`] and answers the rest with a `GitManagerError`.

use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use zc_core::defect::Defect;
use zc_ports::contracts as ports;
use zc_ports::git::{
    CreateWorktreeOptions, GitBranchPullRequest, GitRemoteStatusOptions, GitRunStackedActionOptions, RemoteTrackingCommit as PortRemoteTrackingCommit,
};
use zc_ports::TaggedError;

use crate::contracts::*;
use crate::driver_core::GitVcsDriver;
use crate::errors::{service_error_from_tagged, GitCommandError, GitManagerError, GitManagerServiceError, IntoTagged};
use crate::git_exec::ExecuteGitInput;
use crate::registry::VcsDriverRegistry;
use crate::status::{GitStatusService, RemoteStatusOptions};

/// GitManager as the workflow needs it (zc-git's `GitManager` implements the whole of it).
#[async_trait]
pub trait GitManagerBackend: Send + Sync {
    async fn status(&self, cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError>;
    async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError>;
    async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError>;
    async fn invalidate_local_status(&self, cwd: &str);
    async fn invalidate_remote_status(&self, cwd: &str);
    async fn invalidate_status(&self, cwd: &str);
    /// `runStackedAction(input, options)` (`GitRunStackedActionInput` /
    /// `GitRunStackedActionResult` as wire JSON until zc-contracts lands).
    async fn run_stacked_action(&self, input: serde_json::Value, options: GitRunStackedActionOptions) -> Result<serde_json::Value, GitManagerServiceError>;
    /// `resolvePullRequest(input)`.
    async fn resolve_pull_request(&self, input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError>;
    /// `preparePullRequestThread(input)`.
    async fn prepare_pull_request_thread(&self, input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError>;
    /// `branchPullRequest({cwd, branch}, {refresh})`.
    async fn branch_pull_request(&self, cwd: &str, branch: &str, refresh: bool) -> Result<Option<GitBranchPullRequest>, GitManagerServiceError>;
}

/// A status-only [`GitManagerBackend`] (tests): real status (PRs from the status service's
/// source), and a clear "not available" for the stacked-action and pull-request flows.
#[derive(Clone)]
pub struct StatusOnlyGitManager {
    status: GitStatusService,
}

impl StatusOnlyGitManager {
    pub fn new(status: GitStatusService) -> Self {
        Self { status }
    }

    fn not_yet(operation: &str, cwd: &str) -> GitManagerServiceError {
        GitManagerServiceError::Manager(GitManagerError::new(
            operation,
            cwd,
            "This git workflow is not available without a GitManager (stacked actions and pull request flows live in zc-git).",
        ))
    }
}

fn cwd_of(value: &serde_json::Value) -> String {
    value.get("cwd").and_then(|cwd| cwd.as_str()).unwrap_or_default().to_owned()
}

#[async_trait]
impl GitManagerBackend for StatusOnlyGitManager {
    async fn status(&self, cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError> {
        self.status.status(cwd).await
    }

    async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        self.status.local_status(cwd).await
    }

    async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        self.status.remote_status(cwd, options).await
    }

    async fn invalidate_local_status(&self, cwd: &str) {
        self.status.invalidate_local_status(cwd).await
    }

    async fn invalidate_remote_status(&self, cwd: &str) {
        self.status.invalidate_remote_status(cwd).await
    }

    async fn invalidate_status(&self, cwd: &str) {
        self.status.invalidate_status(cwd).await
    }

    async fn run_stacked_action(&self, input: serde_json::Value, _options: GitRunStackedActionOptions) -> Result<serde_json::Value, GitManagerServiceError> {
        Err(Self::not_yet("GitManager.runStackedAction", &cwd_of(&input)))
    }

    async fn resolve_pull_request(&self, input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError> {
        Err(Self::not_yet("GitManager.resolvePullRequest", &cwd_of(&input)))
    }

    async fn prepare_pull_request_thread(&self, input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError> {
        Err(Self::not_yet("GitManager.preparePullRequestThread", &cwd_of(&input)))
    }

    async fn branch_pull_request(&self, cwd: &str, _branch: &str, _refresh: bool) -> Result<Option<GitBranchPullRequest>, GitManagerServiceError> {
        Err(Self::not_yet("GitManager.branchPullRequest", cwd))
    }
}

/// `GitWorkflowService`.
#[derive(Clone)]
pub struct GitWorkflowService {
    registry: VcsDriverRegistry,
    git: GitVcsDriver,
    manager: Arc<dyn GitManagerBackend>,
}

impl GitWorkflowService {
    pub fn new(registry: VcsDriverRegistry, git: GitVcsDriver, manager: Arc<dyn GitManagerBackend>) -> Self {
        Self { registry, git, manager }
    }

    pub fn driver(&self) -> &GitVcsDriver {
        &self.git
    }

    pub fn manager(&self) -> &Arc<dyn GitManagerBackend> {
        &self.manager
    }

    /// `ensureGit`: resolve a driver (failing outside a repository) that must be git.
    async fn ensure_git(&self, operation: &str, cwd: &str) -> Result<(), GitManagerServiceError> {
        let handle =
            self.registry.resolve(cwd, None).await.map_err(|cause| {
                GitManagerError::new(operation, cwd, "Failed to resolve the VCS driver for this Git workflow.").with_cause(cause.as_defect())
            })?;
        if handle.kind != VcsDriverKind::Git {
            return Err(GitManagerError::new(
                operation,
                cwd,
                format!(
                    "The {operation} workflow currently supports Git repositories only; detected {}. ({cwd})",
                    handle.kind
                ),
            )
            .into());
        }
        Ok(())
    }

    /// `ensureGitCommand`.
    async fn ensure_git_command(&self, operation: &str, cwd: &str) -> Result<(), GitCommandError> {
        let handle = self.registry.resolve(cwd, None).await.map_err(|cause| {
            GitCommandError::new(operation, "vcs-route", cwd, "Failed to resolve the VCS driver for this Git command.").with_cause(cause.as_defect())
        })?;
        if handle.kind != VcsDriverKind::Git {
            return Err(GitCommandError::new(
                operation,
                "vcs-route",
                cwd,
                format!("The {operation} command currently supports Git repositories only; detected {}.", handle.kind),
            ));
        }
        Ok(())
    }

    /// `detectGitRepositoryForStatus`.
    async fn detect_for_status(&self, operation: &str, cwd: &str) -> Result<bool, GitManagerServiceError> {
        let handle =
            self.registry.detect(cwd, None).await.map_err(|cause| {
                GitManagerError::new(operation, cwd, "Failed to detect a VCS repository for this Git workflow.").with_cause(cause.as_defect())
            })?;
        match handle {
            None => Ok(false),
            Some(handle) if handle.kind != VcsDriverKind::Git => Err(GitManagerError::new(
                operation,
                cwd,
                format!(
                    "The {operation} workflow currently supports Git repositories only; detected {}. ({cwd})",
                    handle.kind
                ),
            )
            .into()),
            Some(_) => Ok(true),
        }
    }

    /// `detectGitRepositoryForCommand`.
    async fn detect_for_command(&self, operation: &str, cwd: &str) -> Result<bool, GitCommandError> {
        let handle = self.registry.detect(cwd, None).await.map_err(|cause| {
            GitCommandError::new(operation, "vcs-route", cwd, "Failed to detect a VCS repository for this Git command.").with_cause(cause.as_defect())
        })?;
        match handle {
            None => Ok(false),
            Some(handle) if handle.kind != VcsDriverKind::Git => Err(GitCommandError::new(
                operation,
                "vcs-route",
                cwd,
                format!("The {operation} command currently supports Git repositories only; detected {}.", handle.kind),
            )),
            Some(_) => Ok(true),
        }
    }

    /// `isRepository(cwd)`.
    pub async fn is_repository(&self, cwd: &str) -> Result<bool, GitManagerServiceError> {
        let handle = self.registry.detect(cwd, None).await.map_err(|cause| {
            GitManagerError::new(
                "GitWorkflowService.isRepository",
                cwd,
                "Failed to detect a VCS repository for this Git workflow.",
            )
            .with_cause(cause.as_defect())
        })?;
        Ok(handle.is_some_and(|h| h.kind == VcsDriverKind::Git))
    }

    /// `hasCommit({cwd, refName})`.
    pub async fn has_commit(&self, cwd: &str, ref_name: &str) -> Result<bool, GitCommandError> {
        self.ensure_git_command("GitWorkflowService.hasCommit", cwd).await?;
        let result = self
            .git
            .execute(ExecuteGitInput {
                allow_non_zero_exit: true,
                ..ExecuteGitInput::new(
                    "GitWorkflowService.hasCommit",
                    cwd,
                    ["rev-parse".to_owned(), "--verify".to_owned(), format!("{ref_name}^{{commit}}")],
                )
            })
            .await?;
        Ok(result.exit_code == 0)
    }

    /// `status(input)`.
    pub async fn status(&self, cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError> {
        if self.detect_for_status("GitWorkflowService.status", cwd).await? {
            self.manager.status(cwd).await
        } else {
            Ok(VcsStatusResult::non_repository())
        }
    }

    /// `localStatus(input)`.
    pub async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        if self.detect_for_status("GitWorkflowService.localStatus", cwd).await? {
            self.manager.local_status(cwd).await
        } else {
            Ok(VcsStatusLocalResult::non_repository())
        }
    }

    /// `remoteStatus(input, options?)`.
    pub async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        if self.detect_for_status("GitWorkflowService.remoteStatus", cwd).await? {
            self.manager.remote_status(cwd, options).await
        } else {
            Ok(None)
        }
    }

    pub async fn invalidate_local_status(&self, cwd: &str) {
        self.manager.invalidate_local_status(cwd).await
    }

    pub async fn invalidate_remote_status(&self, cwd: &str) {
        self.manager.invalidate_remote_status(cwd).await
    }

    pub async fn invalidate_status(&self, cwd: &str) {
        self.manager.invalidate_status(cwd).await
    }

    /// `pullCurrentBranch(cwd)`.
    pub async fn pull_current_branch(&self, cwd: &str) -> Result<VcsPullResult, GitCommandError> {
        self.ensure_git_command("GitWorkflowService.pullCurrentBranch", cwd).await?;
        self.git.pull_current_branch(cwd).await
    }

    /// `listRefs(input)`.
    pub async fn list_refs(&self, input: &VcsListRefsInput) -> Result<VcsListRefsResult, GitCommandError> {
        if self.detect_for_command("GitWorkflowService.listRefs", &input.cwd).await? {
            self.git.list_refs(input).await
        } else {
            Ok(VcsListRefsResult::non_repository())
        }
    }

    /// `createWorktree(input, options?)`.
    pub async fn create_worktree(&self, input: &VcsCreateWorktreeInput, options: &CreateWorktreeOptions) -> Result<VcsCreateWorktreeResult, GitCommandError> {
        self.ensure_git_command("GitWorkflowService.createWorktree", &input.cwd).await?;
        self.git.create_worktree(input, options).await
    }

    /// `fetchRemote({cwd, remoteName, refName?})`.
    pub async fn fetch_remote(&self, cwd: &str, remote_name: &str, ref_name: Option<&str>) -> Result<(), GitCommandError> {
        self.ensure_git_command("GitWorkflowService.fetchRemote", cwd).await?;
        self.git.fetch_remote(cwd, remote_name, ref_name).await
    }

    /// `remoteExists({cwd, remoteName})`.
    pub async fn remote_exists(&self, cwd: &str, remote_name: &str) -> Result<bool, GitCommandError> {
        self.ensure_git_command("GitWorkflowService.remoteExists", cwd).await?;
        self.git.remote_exists(cwd, remote_name).await
    }

    /// `remoteBranchExists({cwd, remoteName, refName})`.
    pub async fn remote_branch_exists(&self, cwd: &str, remote_name: &str, ref_name: &str) -> Result<bool, GitCommandError> {
        self.ensure_git_command("GitWorkflowService.remoteBranchExists", cwd).await?;
        self.git.remote_branch_exists(cwd, remote_name, ref_name).await
    }

    /// `resolveRemoteTrackingCommit({cwd, refName, fallbackRemoteName})`.
    pub async fn resolve_remote_tracking_commit(
        &self,
        cwd: &str,
        ref_name: &str,
        fallback_remote_name: &str,
    ) -> Result<crate::driver_core::RemoteTrackingCommit, GitCommandError> {
        self.ensure_git_command("GitWorkflowService.resolveRemoteTrackingCommit", cwd).await?;
        self.git.resolve_remote_tracking_commit(cwd, ref_name, fallback_remote_name).await
    }

    /// `removeWorktree(input)`.
    pub async fn remove_worktree(&self, input: &VcsRemoveWorktreeInput) -> Result<(), GitCommandError> {
        self.ensure_git_command("GitWorkflowService.removeWorktree", &input.cwd).await?;
        self.git.remove_worktree(input).await
    }

    /// `pruneWorktrees({cwd})`.
    pub async fn prune_worktrees(&self, cwd: &str) -> Result<(), GitCommandError> {
        self.ensure_git_command("GitWorkflowService.pruneWorktrees", cwd).await?;
        self.git.prune_worktrees(cwd).await
    }

    /// `createRef(input)`.
    pub async fn create_ref(&self, input: &VcsCreateRefInput) -> Result<VcsCreateRefResult, GitCommandError> {
        self.ensure_git_command("GitWorkflowService.createRef", &input.cwd).await?;
        self.git.create_ref(input).await
    }

    /// `switchRef(input)`.
    pub async fn switch_ref(&self, input: &VcsSwitchRefInput) -> Result<VcsSwitchRefResult, GitCommandError> {
        self.ensure_git_command("GitWorkflowService.switchRef", &input.cwd).await?;
        self.git.switch_ref(input).await
    }

    /// `renameBranch({cwd, oldBranch, newBranch})`.
    pub async fn rename_branch(&self, cwd: &str, old_branch: &str, new_branch: &str) -> Result<String, GitManagerServiceError> {
        self.ensure_git("GitWorkflowService.renameBranch", cwd).await?;
        Ok(self.git.rename_branch(cwd, old_branch, new_branch).await?)
    }

    /// `runStackedAction(input, options?)`.
    pub async fn run_stacked_action(&self, input: serde_json::Value, options: GitRunStackedActionOptions) -> Result<serde_json::Value, GitManagerServiceError> {
        self.ensure_git("GitWorkflowService.runStackedAction", &cwd_of(&input)).await?;
        self.manager.run_stacked_action(input, options).await
    }

    /// `resolvePullRequest(input)`.
    pub async fn resolve_pull_request(&self, input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError> {
        self.ensure_git("GitWorkflowService.resolvePullRequest", &cwd_of(&input)).await?;
        self.manager.resolve_pull_request(input).await
    }

    /// `preparePullRequestThread(input)`.
    pub async fn prepare_pull_request_thread(&self, input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError> {
        self.ensure_git("GitWorkflowService.preparePullRequestThread", &cwd_of(&input)).await?;
        self.manager.prepare_pull_request_thread(input).await
    }
}

// ---------------------------------------------------------------------------------------------
// The zc-ports trait
// ---------------------------------------------------------------------------------------------

fn decode<T: DeserializeOwned>(operation: &str, value: serde_json::Value) -> Result<T, GitCommandError> {
    let cwd = cwd_of(&value);
    serde_json::from_value(value)
        .map_err(|error| GitCommandError::new(operation, "decode", cwd, "Invalid input.").with_cause(Defect::error("Error", error.to_string())))
}

fn encode<T: Serialize, P: From<serde_json::Value>>(value: &T) -> P {
    P::from(serde_json::to_value(value).unwrap_or(serde_json::Value::Null))
}

fn port_options(options: GitRemoteStatusOptions) -> RemoteStatusOptions {
    RemoteStatusOptions {
        refresh_upstream: options.refresh_upstream,
        refresh_missing_pull_request: options.refresh_missing_pull_request,
    }
}

#[async_trait]
impl zc_ports::GitWorkflow for GitWorkflowService {
    async fn is_repository(&self, cwd: &str) -> Result<bool, TaggedError> {
        GitWorkflowService::is_repository(self, cwd).await.map_err(IntoTagged::into_tagged)
    }

    async fn has_commit(&self, cwd: &str, ref_name: &str) -> Result<bool, TaggedError> {
        GitWorkflowService::has_commit(self, cwd, ref_name).await.map_err(IntoTagged::into_tagged)
    }

    async fn status(&self, input: ports::VcsStatusInput) -> Result<ports::VcsStatusResult, TaggedError> {
        let input: VcsStatusInput = decode("GitWorkflowService.status", input.0).map_err(IntoTagged::into_tagged)?;
        GitWorkflowService::status(self, &input.cwd)
            .await
            .map(|r| encode(&r))
            .map_err(IntoTagged::into_tagged)
    }

    async fn local_status(&self, input: ports::VcsStatusInput) -> Result<ports::VcsStatusLocalResult, TaggedError> {
        let input: VcsStatusInput = decode("GitWorkflowService.localStatus", input.0).map_err(IntoTagged::into_tagged)?;
        GitWorkflowService::local_status(self, &input.cwd)
            .await
            .map(|r| encode(&r))
            .map_err(IntoTagged::into_tagged)
    }

    async fn remote_status(&self, input: ports::VcsStatusInput, options: GitRemoteStatusOptions) -> Result<Option<ports::VcsStatusRemoteResult>, TaggedError> {
        let input: VcsStatusInput = decode("GitWorkflowService.remoteStatus", input.0).map_err(IntoTagged::into_tagged)?;
        GitWorkflowService::remote_status(self, &input.cwd, port_options(options))
            .await
            .map(|r| r.map(|r| encode(&r)))
            .map_err(IntoTagged::into_tagged)
    }

    async fn branch_pull_request(&self, cwd: &str, branch: &str, refresh: bool) -> Result<Option<GitBranchPullRequest>, TaggedError> {
        self.manager.branch_pull_request(cwd, branch, refresh).await.map_err(IntoTagged::into_tagged)
    }

    async fn invalidate_local_status(&self, cwd: &str) {
        GitWorkflowService::invalidate_local_status(self, cwd).await
    }

    async fn invalidate_remote_status(&self, cwd: &str) {
        GitWorkflowService::invalidate_remote_status(self, cwd).await
    }

    async fn invalidate_status(&self, cwd: &str) {
        GitWorkflowService::invalidate_status(self, cwd).await
    }

    async fn pull_current_branch(&self, cwd: &str) -> Result<ports::VcsPullResult, TaggedError> {
        GitWorkflowService::pull_current_branch(self, cwd)
            .await
            .map(|r| encode(&r))
            .map_err(IntoTagged::into_tagged)
    }

    async fn run_stacked_action(
        &self,
        input: ports::GitRunStackedActionInput,
        options: GitRunStackedActionOptions,
    ) -> Result<ports::GitRunStackedActionResult, TaggedError> {
        GitWorkflowService::run_stacked_action(self, input.0, options)
            .await
            .map(ports::GitRunStackedActionResult)
            .map_err(IntoTagged::into_tagged)
    }

    async fn resolve_pull_request(&self, input: ports::GitPullRequestRefInput) -> Result<ports::GitResolvePullRequestResult, TaggedError> {
        GitWorkflowService::resolve_pull_request(self, input.0)
            .await
            .map(ports::GitResolvePullRequestResult)
            .map_err(IntoTagged::into_tagged)
    }

    async fn prepare_pull_request_thread(
        &self,
        input: ports::GitPreparePullRequestThreadInput,
    ) -> Result<ports::GitPreparePullRequestThreadResult, TaggedError> {
        GitWorkflowService::prepare_pull_request_thread(self, input.0)
            .await
            .map(ports::GitPreparePullRequestThreadResult)
            .map_err(IntoTagged::into_tagged)
    }

    async fn list_refs(&self, input: ports::VcsListRefsInput) -> Result<ports::VcsListRefsResult, TaggedError> {
        let input: VcsListRefsInput = decode("GitWorkflowService.listRefs", input.0).map_err(IntoTagged::into_tagged)?;
        GitWorkflowService::list_refs(self, &input)
            .await
            .map(|r| encode(&r))
            .map_err(IntoTagged::into_tagged)
    }

    async fn create_worktree(
        &self,
        input: ports::VcsCreateWorktreeInput,
        options: CreateWorktreeOptions,
    ) -> Result<ports::VcsCreateWorktreeResult, TaggedError> {
        let input: VcsCreateWorktreeInput = decode("GitWorkflowService.createWorktree", input.0).map_err(IntoTagged::into_tagged)?;
        GitWorkflowService::create_worktree(self, &input, &options)
            .await
            .map(|r| encode(&r))
            .map_err(IntoTagged::into_tagged)
    }

    async fn fetch_remote(&self, cwd: &str, remote_name: &str, ref_name: Option<&str>) -> Result<(), TaggedError> {
        GitWorkflowService::fetch_remote(self, cwd, remote_name, ref_name)
            .await
            .map_err(IntoTagged::into_tagged)
    }

    async fn remote_exists(&self, cwd: &str, remote_name: &str) -> Result<bool, TaggedError> {
        GitWorkflowService::remote_exists(self, cwd, remote_name).await.map_err(IntoTagged::into_tagged)
    }

    async fn remote_branch_exists(&self, cwd: &str, remote_name: &str, ref_name: &str) -> Result<bool, TaggedError> {
        GitWorkflowService::remote_branch_exists(self, cwd, remote_name, ref_name)
            .await
            .map_err(IntoTagged::into_tagged)
    }

    async fn resolve_remote_tracking_commit(&self, cwd: &str, ref_name: &str, fallback_remote_name: &str) -> Result<PortRemoteTrackingCommit, TaggedError> {
        GitWorkflowService::resolve_remote_tracking_commit(self, cwd, ref_name, fallback_remote_name)
            .await
            .map(|c| PortRemoteTrackingCommit {
                commit_sha: c.commit_sha,
                remote_ref_name: c.remote_ref_name,
            })
            .map_err(IntoTagged::into_tagged)
    }

    async fn remove_worktree(&self, input: ports::VcsRemoveWorktreeInput) -> Result<(), TaggedError> {
        let input: VcsRemoveWorktreeInput = decode("GitWorkflowService.removeWorktree", input.0).map_err(IntoTagged::into_tagged)?;
        GitWorkflowService::remove_worktree(self, &input).await.map_err(IntoTagged::into_tagged)
    }

    async fn prune_worktrees(&self, cwd: &str) -> Result<(), TaggedError> {
        GitWorkflowService::prune_worktrees(self, cwd).await.map_err(IntoTagged::into_tagged)
    }

    async fn create_ref(&self, input: ports::VcsCreateRefInput) -> Result<ports::VcsCreateRefResult, TaggedError> {
        let input: VcsCreateRefInput = decode("GitWorkflowService.createRef", input.0).map_err(IntoTagged::into_tagged)?;
        GitWorkflowService::create_ref(self, &input)
            .await
            .map(|r| encode(&r))
            .map_err(IntoTagged::into_tagged)
    }

    async fn switch_ref(&self, input: ports::VcsSwitchRefInput) -> Result<ports::VcsSwitchRefResult, TaggedError> {
        let input: VcsSwitchRefInput = decode("GitWorkflowService.switchRef", input.0).map_err(IntoTagged::into_tagged)?;
        GitWorkflowService::switch_ref(self, &input)
            .await
            .map(|r| encode(&r))
            .map_err(IntoTagged::into_tagged)
    }

    async fn rename_branch(&self, cwd: &str, old_branch: &str, new_branch: &str) -> Result<String, TaggedError> {
        GitWorkflowService::rename_branch(self, cwd, old_branch, new_branch)
            .await
            .map_err(IntoTagged::into_tagged)
    }
}

/// A [`GitManagerBackend`] backed by a [`zc_ports::GitWorkflow`] implemented elsewhere (so a
/// WP-19 GitManager exposed only through the port can still drive the broadcaster).
pub struct PortGitManager(pub Arc<dyn zc_ports::GitWorkflow>);

#[async_trait]
impl GitManagerBackend for PortGitManager {
    async fn status(&self, cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError> {
        let value = self
            .0
            .status(ports::VcsStatusInput(serde_json::json!({ "cwd": cwd })))
            .await
            .map_err(service_error_from_tagged)?;
        serde_json::from_value(value.0).map_err(|e| GitManagerError::new("GitManager.status", cwd, format!("Invalid status: {e}")).into())
    }

    async fn local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError> {
        let value = self
            .0
            .local_status(ports::VcsStatusInput(serde_json::json!({ "cwd": cwd })))
            .await
            .map_err(service_error_from_tagged)?;
        serde_json::from_value(value.0).map_err(|e| GitManagerError::new("GitManager.localStatus", cwd, format!("Invalid status: {e}")).into())
    }

    async fn remote_status(&self, cwd: &str, options: RemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError> {
        let value = self
            .0
            .remote_status(
                ports::VcsStatusInput(serde_json::json!({ "cwd": cwd })),
                GitRemoteStatusOptions {
                    refresh_upstream: options.refresh_upstream,
                    refresh_missing_pull_request: options.refresh_missing_pull_request,
                },
            )
            .await
            .map_err(service_error_from_tagged)?;
        match value {
            None => Ok(None),
            Some(value) => serde_json::from_value(value.0)
                .map(Some)
                .map_err(|e| GitManagerError::new("GitManager.remoteStatus", cwd, format!("Invalid status: {e}")).into()),
        }
    }

    async fn invalidate_local_status(&self, cwd: &str) {
        self.0.invalidate_local_status(cwd).await
    }

    async fn invalidate_remote_status(&self, cwd: &str) {
        self.0.invalidate_remote_status(cwd).await
    }

    async fn invalidate_status(&self, cwd: &str) {
        self.0.invalidate_status(cwd).await
    }

    async fn run_stacked_action(&self, input: serde_json::Value, options: GitRunStackedActionOptions) -> Result<serde_json::Value, GitManagerServiceError> {
        self.0
            .run_stacked_action(ports::GitRunStackedActionInput(input), options)
            .await
            .map(|r| r.0)
            .map_err(service_error_from_tagged)
    }

    async fn resolve_pull_request(&self, input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError> {
        self.0
            .resolve_pull_request(ports::GitPullRequestRefInput(input))
            .await
            .map(|r| r.0)
            .map_err(service_error_from_tagged)
    }

    async fn prepare_pull_request_thread(&self, input: serde_json::Value) -> Result<serde_json::Value, GitManagerServiceError> {
        self.0
            .prepare_pull_request_thread(ports::GitPreparePullRequestThreadInput(input))
            .await
            .map(|r| r.0)
            .map_err(service_error_from_tagged)
    }

    async fn branch_pull_request(&self, cwd: &str, branch: &str, refresh: bool) -> Result<Option<GitBranchPullRequest>, GitManagerServiceError> {
        self.0.branch_pull_request(cwd, branch, refresh).await.map_err(service_error_from_tagged)
    }
}
