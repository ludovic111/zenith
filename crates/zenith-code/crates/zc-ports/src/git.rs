//! Git ports (`apps/server/src/git/GitWorkflowService.ts`, `git/GitManager.ts`,
//! `vcs/GitVcsDriver.ts` option types, `vcs/VcsStatusBroadcaster.ts`).
//!
//! Implemented by zc-vcs (driver, workflow, stacked actions, broadcaster); consumed by the
//! orchestration reactors and the worktree bootstrap in the RPC handlers (create, rename, prune
//! and remove worktrees, fetch and resolve bases), storage cleanup, the settlement and PR
//! reactors (`branchPullRequest`) and zc-project (clone, setup).

use std::sync::Arc;

use async_trait::async_trait;

use crate::contracts::{
    GitActionProgressEvent, GitCommandError, GitManagerServiceError, GitPreparePullRequestThreadInput, GitPreparePullRequestThreadResult,
    GitPullRequestRefInput, GitResolvePullRequestResult, GitRunStackedActionInput, GitRunStackedActionResult, VcsCreateRefInput, VcsCreateRefResult,
    VcsCreateWorktreeInput, VcsCreateWorktreeResult, VcsListRefsInput, VcsListRefsResult, VcsPullResult, VcsRemoveWorktreeInput, VcsStatusInput,
    VcsStatusLocalResult, VcsStatusPullRequest, VcsStatusRemoteResult, VcsStatusResult, VcsSwitchRefInput, VcsSwitchRefResult, WorktreeSubmodules,
};

/// `GitRemoteStatusOptions` (GitManager's, which extends the driver's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitRemoteStatusOptions {
    /// Fetch the upstream before computing ahead/behind.
    pub refresh_upstream: bool,
    /// Retry a cached missing PR without clearing known PRs or failed-lookup backoff.
    pub refresh_missing_pull_request: bool,
}

/// `GitActionProgressReporter`: receives stacked-action progress events.
pub type GitActionProgressReporter = Arc<dyn Fn(GitActionProgressEvent) + Send + Sync>;

/// `GitRunStackedActionOptions`.
#[derive(Clone, Default)]
pub struct GitRunStackedActionOptions {
    /// Reuse a client-chosen id instead of a fresh UUID.
    pub action_id: Option<String>,
    pub progress_reporter: Option<GitActionProgressReporter>,
}

/// `git worktree add` checkout progress (`onCheckoutProgress`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CheckoutProgress {
    pub percent: f64,
    pub completed: u64,
    pub total: u64,
}

/// Why submodules were skipped (`onSubmodulesDisabled.source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmodulesDisabledSource {
    /// The project-over-environment `worktreeSubmodules` setting.
    Settings,
    /// The checkout's own `t3.json`.
    T3Json,
}

/// `CreateWorktreeProgress`: callbacks during `createWorktree`. All optional, all infallible.
#[derive(Clone, Default)]
#[allow(clippy::type_complexity)]
pub struct CreateWorktreeProgress {
    /// Fires once git created and registered the directory, before submodules. A path reported
    /// here belongs to this call and is safe to remove on cancel.
    pub on_worktree_claimed: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    pub on_checkout_progress: Option<Arc<dyn Fn(CheckoutProgress) + Send + Sync>>,
    pub on_submodules_started: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Fires when `.gitmodules` exists but the resolved submodule mode is `none`.
    pub on_submodules_disabled: Option<Arc<dyn Fn(SubmodulesDisabledSource) + Send + Sync>>,
    pub on_submodule_line: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    /// `{ok, detail}`.
    pub on_submodules_finished: Option<Arc<dyn Fn(bool, Option<&str>) + Send + Sync>>,
}

/// `CreateWorktreeOptions`.
#[derive(Clone, Default)]
pub struct CreateWorktreeOptions {
    pub progress: CreateWorktreeProgress,
    /// The `worktreeSubmodules` setting; `None` defers to the checkout's own `t3.json`.
    pub submodules: Option<WorktreeSubmodules>,
}

/// `resolveRemoteTrackingCommit` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTrackingCommit {
    pub commit_sha: String,
    pub remote_ref_name: String,
}

/// `GitBranchPullRequest`: the branch's change request plus what the settlement and PR-sync
/// reactors need to tell whether it is the same one.
#[derive(Debug, Clone, PartialEq)]
pub struct GitBranchPullRequest {
    /// `NonNullable<VcsStatusResult["pr"]>`.
    pub pull_request: VcsStatusPullRequest,
    pub repository_key: Option<String>,
    pub updated_at: Option<String>,
    /// `closedAt?: string | null`: `None` = absent, `Some(None)` = null.
    pub closed_at: Option<Option<String>>,
    /// `mergedAt?: string | null`.
    pub merged_at: Option<Option<String>>,
}

/// `GitWorkflowService` plus `GitManager.branchPullRequest`.
#[async_trait]
pub trait GitWorkflow: Send + Sync {
    /// `isRepository(cwd)`.
    async fn is_repository(&self, cwd: &str) -> Result<bool, GitManagerServiceError>;

    /// `hasCommit({cwd, refName})`.
    async fn has_commit(&self, cwd: &str, ref_name: &str) -> Result<bool, GitCommandError>;

    /// `status(input)`: local and remote status (cached about 1 s).
    async fn status(&self, input: VcsStatusInput) -> Result<VcsStatusResult, GitManagerServiceError>;

    /// `localStatus(input)`.
    async fn local_status(&self, input: VcsStatusInput) -> Result<VcsStatusLocalResult, GitManagerServiceError>;

    /// `remoteStatus(input, options?)`: `None` when there is nothing remote to report.
    async fn remote_status(&self, input: VcsStatusInput, options: GitRemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError>;

    /// `branchPullRequest({cwd, branch}, {refresh?})`: the PR of a saved branch, without
    /// touching the current checkout.
    async fn branch_pull_request(&self, cwd: &str, branch: &str, refresh: bool) -> Result<Option<GitBranchPullRequest>, GitManagerServiceError>;

    /// `invalidateLocalStatus(cwd)`.
    async fn invalidate_local_status(&self, cwd: &str);

    /// `invalidateRemoteStatus(cwd)`.
    async fn invalidate_remote_status(&self, cwd: &str);

    /// `invalidateStatus(cwd)`.
    async fn invalidate_status(&self, cwd: &str);

    /// `pullCurrentBranch(cwd)`.
    async fn pull_current_branch(&self, cwd: &str) -> Result<VcsPullResult, GitCommandError>;

    /// `runStackedAction(input, options?)`: commit / push / create PR, with progress.
    async fn run_stacked_action(
        &self,
        input: GitRunStackedActionInput,
        options: GitRunStackedActionOptions,
    ) -> Result<GitRunStackedActionResult, GitManagerServiceError>;

    /// `resolvePullRequest(input)`.
    async fn resolve_pull_request(&self, input: GitPullRequestRefInput) -> Result<GitResolvePullRequestResult, GitManagerServiceError>;

    /// `preparePullRequestThread(input)`.
    async fn prepare_pull_request_thread(&self, input: GitPreparePullRequestThreadInput) -> Result<GitPreparePullRequestThreadResult, GitManagerServiceError>;

    /// `listRefs(input)`.
    async fn list_refs(&self, input: VcsListRefsInput) -> Result<VcsListRefsResult, GitCommandError>;

    /// `createWorktree(input, options?)`.
    async fn create_worktree(&self, input: VcsCreateWorktreeInput, options: CreateWorktreeOptions) -> Result<VcsCreateWorktreeResult, GitCommandError>;

    /// `fetchRemote({cwd, remoteName, refName?})`.
    async fn fetch_remote(&self, cwd: &str, remote_name: &str, ref_name: Option<&str>) -> Result<(), GitCommandError>;

    /// `remoteExists({cwd, remoteName})`.
    async fn remote_exists(&self, cwd: &str, remote_name: &str) -> Result<bool, GitCommandError>;

    /// `remoteBranchExists({cwd, remoteName, refName})`.
    async fn remote_branch_exists(&self, cwd: &str, remote_name: &str, ref_name: &str) -> Result<bool, GitCommandError>;

    /// `resolveRemoteTrackingCommit({cwd, refName, fallbackRemoteName})`.
    async fn resolve_remote_tracking_commit(&self, cwd: &str, ref_name: &str, fallback_remote_name: &str) -> Result<RemoteTrackingCommit, GitCommandError>;

    /// `removeWorktree(input)`.
    async fn remove_worktree(&self, input: VcsRemoveWorktreeInput) -> Result<(), GitCommandError>;

    /// `pruneWorktrees({cwd})`.
    async fn prune_worktrees(&self, cwd: &str) -> Result<(), GitCommandError>;

    /// `createRef(input)`.
    async fn create_ref(&self, input: VcsCreateRefInput) -> Result<VcsCreateRefResult, GitCommandError>;

    /// `switchRef(input)`.
    async fn switch_ref(&self, input: VcsSwitchRefInput) -> Result<VcsSwitchRefResult, GitCommandError>;

    /// `renameBranch({cwd, oldBranch, newBranch})`: returns the branch name actually used.
    async fn rename_branch(&self, cwd: &str, old_branch: &str, new_branch: &str) -> Result<String, GitManagerServiceError>;
}

/// The refresh half of `VcsStatusBroadcaster`, which the checkpoint and provider-command
/// reactors poke after a turn.
#[async_trait]
pub trait VcsStatusRefresher: Send + Sync {
    /// `refreshLocalStatus(cwd)`: invalidate, recompute and publish the local status.
    async fn refresh_local_status(&self, cwd: &str) -> Result<VcsStatusLocalResult, GitManagerServiceError>;

    /// `refreshStatus(cwd)`: local and remote.
    async fn refresh_status(&self, cwd: &str) -> Result<VcsStatusResult, GitManagerServiceError>;

    /// `refreshPullRequestStatus(cwd)`: after a turn, if background policy allows it.
    async fn refresh_pull_request_status(&self, cwd: &str) -> Result<Option<VcsStatusRemoteResult>, GitManagerServiceError>;
}
