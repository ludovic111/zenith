//! `orchestration/Layers/CheckpointReactor.ts`: git checkpoints around agent turns.
//!
//! - **Baseline** at turn start (`thread.turn-start-requested`, a plain user
//!   `thread.message-sent`, or the provider's `turn.started`): capture
//!   `refs/t3/checkpoints/<thread>/turn/<n>` for the current turn count when it is missing.
//! - **Completion** on `turn.completed` / `turn.aborted`: capture turn `n + 1` (or the
//!   placeholder's count), compute the numstat summary against the baseline, dispatch
//!   `thread.turn.diff.complete`, publish the receipts and append `checkpoint.captured`. Git
//!   status (and the thread's PR, and branch drift adoption) refresh on their own worker.
//! - **Revert** on `thread.checkpoint-revert-requested`: restore the files (isolated worktrees
//!   only), roll the provider conversation back, delete the stale refs, dispatch
//!   `thread.revert.complete`; any failure becomes a `checkpoint.revert.failed` activity.
//!
//! Domain and provider events are funnelled into one sequential worker, like the TS
//! `DrainableWorker`, so checkpoints of one thread never interleave.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use tokio::task::JoinHandle;
use zc_contracts::{
    CheckpointRef, MessageId, OrchestrationCheckpointStatus, OrchestrationEvent, OrchestrationMessageRole, OrchestrationThread, ProjectId, ThreadId, TurnId,
};
use zc_ports::contracts::{ProviderRuntimeEvent, VcsStatusLocalResult};
use zc_ports::orchestration::ThreadDetailQuery;
use zc_ports::{OrchestrationDispatch, ProjectionReads, ProviderService, PullRequests, TaggedError, VcsStatusRefresher};

use crate::diffs::parse_turn_diff_files_from_numstat;
use crate::receipts::{OrchestrationRuntimeReceipt, RuntimeReceiptBus};
use crate::store::{CheckpointStore, DiffCheckpointsInput, DiffFormat};
use crate::support::{
    decode_command, is_temporary_worktree_branch, is_within, now_iso, same_id, server_command_id, str_field, uuid, ProviderRuntimeEventExt, ProviderSessionExt,
};
use crate::utils::{checkpoint_ref_for_thread_turn, resolve_thread_workspace_cwd, WorkspaceProject};
use crate::worker::DrainableWorker;

/// `WorkspaceEntries.refresh(cwd)`: re-index the @-mention file picker after files changed.
#[async_trait]
pub trait WorkspaceEntriesRefresher: Send + Sync {
    async fn refresh(&self, cwd: &str) -> std::result::Result<(), String>;
}

/// No file index to refresh.
pub struct NoWorkspaceEntries;

#[async_trait]
impl WorkspaceEntriesRefresher for NoWorkspaceEntries {
    async fn refresh(&self, _cwd: &str) -> std::result::Result<(), String> {
        Ok(())
    }
}

/// What the reactor is built from.
#[derive(Clone)]
pub struct CheckpointReactorDeps {
    pub engine: Arc<dyn OrchestrationDispatch>,
    pub projections: Arc<dyn ProjectionReads>,
    pub providers: Arc<dyn ProviderService>,
    pub store: Arc<dyn CheckpointStore>,
    pub receipts: RuntimeReceiptBus,
    pub workspace_entries: Arc<dyn WorkspaceEntriesRefresher>,
    pub vcs_status: Arc<dyn VcsStatusRefresher>,
    pub pull_requests: Arc<dyn PullRequests>,
}

/// A failure inside one reactor step: its message is what activities record (`error.message`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReactorError(pub String);

impl std::fmt::Display for ReactorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<zc_vcs::VcsError> for ReactorError {
    fn from(error: zc_vcs::VcsError) -> Self {
        Self(error.message())
    }
}

impl From<TaggedError> for ReactorError {
    fn from(error: TaggedError) -> Self {
        Self(error.to_string())
    }
}

impl From<String> for ReactorError {
    fn from(message: String) -> Self {
        Self(message)
    }
}

type Result<T> = std::result::Result<T, ReactorError>;

enum ReactorInput {
    Runtime(ProviderRuntimeEvent),
    Domain(Box<OrchestrationEvent>),
}

struct Inner {
    deps: CheckpointReactorDeps,
    started_turns: Mutex<HashMap<String, String>>,
    pending: Mutex<HashSet<String>>,
    queued_entry_refreshes: Mutex<HashSet<String>>,
    entry_refresh_worker: DrainableWorker<String>,
    status_refresh_worker: DrainableWorker<ProviderRuntimeEvent>,
}

/// `CheckpointReactor`. Cloning shares it.
#[derive(Clone)]
pub struct CheckpointReactor {
    inner: Arc<Inner>,
    worker: DrainableWorker<ReactorInput>,
}

/// The running subscriptions of [`CheckpointReactor::start`]; dropping it stops them (the TS
/// scope closing).
pub struct ReactorTasks(Vec<JoinHandle<()>>);

impl ReactorTasks {
    pub fn new(tasks: Vec<JoinHandle<()>>) -> Self {
        Self(tasks)
    }
}

impl Drop for ReactorTasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

fn checkpoint_status_from_runtime(status: Option<&str>) -> OrchestrationCheckpointStatus {
    match status {
        Some("failed") => OrchestrationCheckpointStatus::Error,
        Some("cancelled") | Some("interrupted") => OrchestrationCheckpointStatus::Missing,
        _ => OrchestrationCheckpointStatus::Ready,
    }
}

fn current_turn_count(thread: &OrchestrationThread) -> i64 {
    thread.checkpoints.iter().map(|c| c.checkpoint_turn_count).fold(0, i64::max)
}

impl CheckpointReactor {
    /// `make`. Needs a Tokio runtime (it starts its workers).
    pub fn new(deps: CheckpointReactorDeps) -> Self {
        let inner = Arc::new_cyclic(|weak: &std::sync::Weak<Inner>| {
            let for_entries = weak.clone();
            let entry_refresh_worker = DrainableWorker::new(move |cwd: String| {
                let inner = for_entries.upgrade();
                async move {
                    let Some(inner) = inner else { return };
                    inner.queued_entry_refreshes.lock().unwrap().remove(&cwd);
                    if let Err(error) = inner.deps.workspace_entries.refresh(&cwd).await {
                        tracing::warn!(cwd, error, "failed to refresh checkpoint workspace entries");
                    }
                }
            });
            let for_status = weak.clone();
            let status_refresh_worker = DrainableWorker::new(move |event: ProviderRuntimeEvent| {
                let inner = for_status.upgrade();
                async move {
                    let Some(inner) = inner else { return };
                    if let Err(error) = inner.refresh_local_git_status_from_turn_completion(&event).await {
                        tracing::warn!(thread_id = event.thread_id(), error = %error, "failed to refresh git status after turn completion");
                    }
                }
            });
            Inner {
                deps,
                started_turns: Mutex::new(HashMap::new()),
                pending: Mutex::new(HashSet::new()),
                queued_entry_refreshes: Mutex::new(HashSet::new()),
                entry_refresh_worker,
                status_refresh_worker,
            }
        });
        let for_worker = inner.clone();
        let worker = DrainableWorker::new(move |input: ReactorInput| {
            let inner = for_worker.clone();
            async move { inner.process_input_safely(input).await }
        });
        Self { inner, worker }
    }

    /// `start()`: subscribe to domain events (turn starts, plain user messages, revert
    /// requests) and provider events (turn start/end, session exit). Both subscriptions exist
    /// when this returns.
    pub fn start(&self) -> ReactorTasks {
        let mut domain = self.inner.deps.engine.subscribe_domain_events();
        let mut runtime = self.inner.deps.providers.subscribe_events();
        let worker = self.worker.clone();
        let domain_task = tokio::spawn(async move {
            while let Some(event) = domain.next().await {
                if matches!(
                    event,
                    OrchestrationEvent::ThreadTurnStartRequested(_)
                        | OrchestrationEvent::ThreadMessageSent(_)
                        | OrchestrationEvent::ThreadCheckpointRevertRequested(_)
                ) {
                    worker.enqueue(ReactorInput::Domain(Box::new(event)));
                }
            }
        });
        let worker = self.worker.clone();
        let runtime_task = tokio::spawn(async move {
            while let Some(event) = runtime.next().await {
                if matches!(event.event_type(), "turn.started" | "turn.completed" | "turn.aborted" | "session.exited") {
                    worker.enqueue(ReactorInput::Runtime(event));
                }
            }
        });
        ReactorTasks(vec![domain_task, runtime_task])
    }

    /// `drain`: the event worker, then the status refreshes, then the entry refreshes.
    pub async fn drain(&self) {
        self.worker.drain().await;
        self.inner.status_refresh_worker.drain().await;
        self.inner.entry_refresh_worker.drain().await;
    }
}

impl Inner {
    async fn process_input_safely(&self, input: ReactorInput) {
        let (source, event_type) = match &input {
            ReactorInput::Runtime(event) => ("runtime", event.event_type().to_owned()),
            ReactorInput::Domain(event) => ("domain", domain_event_type(event).to_owned()),
        };
        let result = match input {
            ReactorInput::Domain(event) => self.process_domain_event(*event).await,
            ReactorInput::Runtime(event) => self.process_runtime_event(event).await,
        };
        if let Err(error) = result {
            tracing::warn!(source, event_type, cause = %error, "checkpoint reactor failed to process input");
        }
    }

    fn refresh_workspace_entries(&self, cwd: &str) {
        if !self.queued_entry_refreshes.lock().unwrap().insert(cwd.to_owned()) {
            return;
        }
        self.entry_refresh_worker.enqueue(cwd.to_owned());
    }

    async fn dispatch(&self, command: serde_json::Value) -> Result<i64> {
        let command = decode_command(command)?;
        Ok(self.deps.engine.dispatch(command, None).await?.sequence)
    }

    async fn append_revert_failure_activity(&self, thread_id: &ThreadId, turn_count: i64, detail: &str, created_at: &str) -> Result<()> {
        self.dispatch(json!({
            "type": "thread.activity.append",
            "commandId": server_command_id("checkpoint-revert-failure"),
            "threadId": thread_id,
            "activity": {
                "id": uuid(),
                "tone": "error",
                "kind": "checkpoint.revert.failed",
                "summary": "Checkpoint revert failed",
                "payload": {"turnCount": turn_count, "detail": detail},
                "turnId": null,
                "createdAt": created_at,
            },
            "createdAt": created_at,
        }))
        .await
        .map(drop)
    }

    async fn append_capture_failure_activity(&self, thread_id: &str, turn_id: Option<&str>, detail: &str, created_at: &str) -> Result<()> {
        self.dispatch(json!({
            "type": "thread.activity.append",
            "commandId": server_command_id("checkpoint-capture-failure"),
            "threadId": thread_id,
            "activity": {
                "id": uuid(),
                "tone": "error",
                "kind": "checkpoint.capture.failed",
                "summary": "Checkpoint capture failed",
                "payload": {"detail": detail},
                "turnId": turn_id,
                "createdAt": created_at,
            },
            "createdAt": created_at,
        }))
        .await
        .map(drop)
    }

    /// `resolveSessionRuntimeForThread`: the live session's cwd.
    async fn session_cwd(&self, thread_id: &str) -> Option<String> {
        self.deps
            .providers
            .list_sessions()
            .await
            .into_iter()
            .find(|session| session.thread_id() == Some(thread_id))
            .and_then(|session| session.cwd().filter(|cwd| !cwd.is_empty()).map(str::to_owned))
    }

    async fn resolve_thread_detail(&self, thread_id: &str) -> Result<Option<OrchestrationThread>> {
        Ok(self
            .deps
            .projections
            .get_thread_detail_by_id(&ThreadId::new(thread_id), ThreadDetailQuery { activity_kinds: Some(vec![]) })
            .await?)
    }

    async fn resolve_thread_projects(&self, project_id: &ProjectId) -> Result<Vec<WorkspaceProject>> {
        Ok(self
            .deps
            .projections
            .get_project_shell_by_id(project_id)
            .await?
            .map(|project| WorkspaceProject {
                id: project.id,
                workspace_root: project.workspace_root,
            })
            .into_iter()
            .collect())
    }

    /// `resolveCheckpointCwd`: the session cwd or the thread/project workspace (in the
    /// requested order of preference), and only when it is a git repository.
    async fn resolve_checkpoint_cwd(
        &self,
        thread: &OrchestrationThread,
        projects: &[WorkspaceProject],
        prefer_session_runtime: bool,
    ) -> Result<Option<String>> {
        let from_session = self.session_cwd(thread.id.as_str()).await;
        let from_thread = resolve_thread_workspace_cwd(&thread.project_id, thread.worktree_path.as_deref(), projects);
        let cwd = if prefer_session_runtime {
            from_session.or(from_thread)
        } else {
            from_thread.or(from_session)
        };
        let Some(cwd) = cwd else { return Ok(None) };
        if !self.deps.store.is_git_repository(&cwd).await? {
            return Ok(None);
        }
        Ok(Some(cwd))
    }

    /// `captureAndDispatchCheckpoint`.
    #[allow(clippy::too_many_arguments)]
    async fn capture_and_dispatch_checkpoint(
        &self,
        thread: &OrchestrationThread,
        turn_id: &str,
        cwd: &str,
        turn_count: i64,
        status: OrchestrationCheckpointStatus,
        assistant_message_id: Option<MessageId>,
        created_at: &str,
    ) -> Result<()> {
        let thread_id = &thread.id;
        let from_turn_count = (turn_count - 1).max(0);
        let from_checkpoint_ref = checkpoint_ref_for_thread_turn(thread_id, from_turn_count);
        let target_checkpoint_ref = checkpoint_ref_for_thread_turn(thread_id, turn_count);

        let from_exists = match self.deps.store.has_checkpoint_ref(cwd, &from_checkpoint_ref).await {
            Ok(exists) => exists,
            Err(error) => {
                tracing::warn!(thread_id = %thread_id, checkpoint_ref = %from_checkpoint_ref.as_str(), category = error.tag(), "checkpoint capture previous ref lookup failed");
                false
            }
        };
        if !from_exists {
            tracing::warn!(thread_id = %thread_id, turn_id, from_turn_count, "checkpoint capture missing pre-turn baseline");
        }

        self.deps.store.capture_checkpoint(cwd, &target_checkpoint_ref).await?;
        self.refresh_workspace_entries(cwd);

        // Git may have been initialized during this turn: keep the completion checkpoint for
        // later turns, but do not diff against a baseline that does not exist.
        let files = if from_exists {
            match self
                .deps
                .store
                .diff_checkpoints(&DiffCheckpointsInput {
                    cwd: cwd.to_owned(),
                    from_checkpoint_ref: from_checkpoint_ref.clone(),
                    to_checkpoint_ref: target_checkpoint_ref.clone(),
                    fallback_from_to_head: false,
                    ignore_whitespace: false,
                    format: DiffFormat::Numstat,
                })
                .await
            {
                Ok(numstat) => parse_turn_diff_files_from_numstat(&numstat),
                Err(error) => {
                    let detail = format!("Checkpoint captured, but turn diff summary is unavailable: {}", error.message());
                    let failure = self
                        .append_capture_failure_activity(thread_id.as_str(), Some(turn_id), &detail, created_at)
                        .await;
                    let logged = failure.err().map_or(error.message(), |e| e.0);
                    tracing::warn!(thread_id = %thread_id, turn_id, turn_count, detail = logged, "failed to derive checkpoint file summary");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };
        let files: Vec<serde_json::Value> = files
            .into_iter()
            .map(|file| json!({"path": file.path, "kind": "modified", "additions": file.additions, "deletions": file.deletions}))
            .collect();

        let assistant_message_id = assistant_message_id
            .or_else(|| {
                thread
                    .messages
                    .iter()
                    .rev()
                    .find(|message| message.role == OrchestrationMessageRole::Assistant && message.turn_id.as_ref().map(|t| t.as_str()) == Some(turn_id))
                    .map(|message| message.id.clone())
            })
            .unwrap_or_else(|| MessageId::new(format!("assistant:{turn_id}")));

        self.dispatch(json!({
            "type": "thread.turn.diff.complete",
            "commandId": server_command_id("checkpoint-turn-diff-complete"),
            "threadId": thread_id,
            "turnId": turn_id,
            "completedAt": created_at,
            "checkpointRef": target_checkpoint_ref,
            "status": status,
            "files": files,
            "assistantMessageId": assistant_message_id,
            "checkpointTurnCount": turn_count,
            "createdAt": created_at,
        }))
        .await?;
        self.deps.receipts.publish(OrchestrationRuntimeReceipt::CheckpointDiffFinalized {
            thread_id: thread_id.clone(),
            turn_id: TurnId::new(turn_id),
            checkpoint_turn_count: turn_count,
            checkpoint_ref: target_checkpoint_ref.clone(),
            status,
            created_at: created_at.to_owned(),
        });
        self.deps.receipts.publish(OrchestrationRuntimeReceipt::TurnProcessingQuiesced {
            thread_id: thread_id.clone(),
            turn_id: TurnId::new(turn_id),
            checkpoint_turn_count: turn_count,
            created_at: created_at.to_owned(),
        });

        self.dispatch(json!({
            "type": "thread.activity.append",
            "commandId": server_command_id("checkpoint-captured-activity"),
            "threadId": thread_id,
            "activity": {
                "id": uuid(),
                "tone": "info",
                "kind": "checkpoint.captured",
                "summary": "Checkpoint captured",
                "payload": {"turnCount": turn_count, "status": status},
                "turnId": turn_id,
                "createdAt": created_at,
            },
            "createdAt": created_at,
        }))
        .await?;
        Ok(())
    }

    /// `captureCheckpointFromTurnCompletion`.
    async fn capture_checkpoint_from_turn_completion(&self, event: &ProviderRuntimeEvent) -> Result<()> {
        let Some(turn_id) = event.turn_id() else { return Ok(()) };
        let Some(thread) = self.resolve_thread_detail(event.thread_id()).await? else {
            return Ok(());
        };

        // While a primary turn is active, only that turn produces completion checkpoints.
        if let Some(active) = thread.session.as_ref().and_then(|s| s.active_turn_id.as_ref()) {
            if active.as_str() != turn_id {
                return Ok(());
            }
        }
        // Placeholders ("missing") inserted by runtime ingestion must not block real capture.
        if thread
            .checkpoints
            .iter()
            .any(|c| c.turn_id.as_str() == turn_id && c.status != OrchestrationCheckpointStatus::Missing)
        {
            return Ok(());
        }

        let projects = self.resolve_thread_projects(&thread.project_id).await?;
        let Some(cwd) = self.resolve_checkpoint_cwd(&thread, &projects, true).await? else {
            return Ok(());
        };

        let placeholder = thread
            .checkpoints
            .iter()
            .find(|c| c.turn_id.as_str() == turn_id && c.status == OrchestrationCheckpointStatus::Missing);
        let next_turn_count = placeholder.map_or_else(|| current_turn_count(&thread) + 1, |p| p.checkpoint_turn_count);
        let status = if event.event_type() == "turn.aborted" {
            OrchestrationCheckpointStatus::Ready
        } else {
            checkpoint_status_from_runtime(event.payload_state())
        };
        let assistant_message_id = placeholder.and_then(|p| p.assistant_message_id.clone());
        self.capture_and_dispatch_checkpoint(&thread, &turn_id, &cwd, next_turn_count, status, assistant_message_id, event.created_at())
            .await
    }

    /// The baseline capture shared by `ensurePreTurnBaselineFromTurnStart` and
    /// `ensurePreTurnBaselineFromDomainTurnStart`.
    async fn ensure_pre_turn_baseline(&self, thread_id: &str, created_at: &str) -> Result<()> {
        let Some(thread) = self.resolve_thread_detail(thread_id).await? else {
            return Ok(());
        };
        let projects = self.resolve_thread_projects(&thread.project_id).await?;
        let Some(cwd) = self.resolve_checkpoint_cwd(&thread, &projects, false).await? else {
            return Ok(());
        };
        let turn_count = current_turn_count(&thread);
        let baseline: CheckpointRef = checkpoint_ref_for_thread_turn(&thread.id, turn_count);
        if self.deps.store.has_checkpoint_ref(&cwd, &baseline).await? {
            return Ok(());
        }
        self.deps.store.capture_checkpoint(&cwd, &baseline).await?;
        self.deps.receipts.publish(OrchestrationRuntimeReceipt::CheckpointBaselineCaptured {
            thread_id: thread.id.clone(),
            checkpoint_turn_count: turn_count,
            checkpoint_ref: baseline,
            created_at: created_at.to_owned(),
        });
        Ok(())
    }

    /// `refreshLocalGitStatusFromTurnCompletion` (on the status worker).
    async fn refresh_local_git_status_from_turn_completion(&self, event: &ProviderRuntimeEvent) -> Result<()> {
        let Some(cwd) = self.session_cwd(event.thread_id()).await else {
            return Ok(());
        };
        let local = match self.deps.vcs_status.refresh_local_status(&cwd).await {
            Ok(local) => local,
            Err(error) => {
                tracing::warn!(thread_id = event.thread_id(), turn_id = ?event.turn_id(), cwd, detail = %error, "failed to refresh local git status after turn completion");
                return Ok(());
            }
        };
        let thread_id = ThreadId::new(event.thread_id());
        self.follow_worktree_branch_drift(&thread_id, &cwd, &local).await;
        self.refresh_pull_request_after_turn(&thread_id, event.turn_id().as_deref(), &cwd, &local).await
    }

    /// `refreshPullRequestAfterTurn`: retry a missing PR after the agent pushed, but only for
    /// the thread whose recorded branch is checked out here.
    async fn refresh_pull_request_after_turn(&self, thread_id: &ThreadId, turn_id: Option<&str>, cwd: &str, local: &VcsStatusLocalResult) -> Result<()> {
        let Some(checked_out) = str_field(&local.0, "refName") else { return Ok(()) };
        if local.0.get("isDefaultRef").and_then(serde_json::Value::as_bool) == Some(true) {
            return Ok(());
        }
        let Some(thread) = self.deps.projections.get_thread_shell_by_id(thread_id).await? else {
            return Ok(());
        };
        if thread.branch.as_deref() != Some(checked_out) {
            return Ok(());
        }
        if let Some(active) = thread.session.as_ref().and_then(|s| s.active_turn_id.as_ref()) {
            if !same_id(Some(active.as_str()), turn_id) {
                return Ok(());
            }
        }
        if let Err(error) = self.deps.vcs_status.refresh_pull_request_status(cwd).await {
            tracing::warn!(thread_id = %thread_id, cwd, detail = %error, "failed to refresh pull request status after turn completion");
        }
        Ok(())
    }

    /// `followWorktreeBranchDrift`: adopt a branch checked out by hand (or by the agent) in a
    /// worktree that belongs to this thread alone.
    async fn follow_worktree_branch_drift(&self, thread_id: &ThreadId, cwd: &str, local: &VcsStatusLocalResult) {
        let Some(checked_out) = str_field(&local.0, "refName") else { return };
        if is_temporary_worktree_branch(checked_out) {
            return;
        }
        let result: Result<()> = async {
            let Some(thread) = self.deps.projections.get_thread_shell_by_id(thread_id).await? else {
                return Ok(());
            };
            let (Some(branch), Some(worktree_path)) = (thread.branch.as_deref(), thread.worktree_path.as_deref()) else {
                return Ok(());
            };
            if branch == checked_out || worktree_path != cwd {
                return Ok(());
            }
            let shell = self.deps.projections.get_shell_snapshot(false).await?;
            if shell
                .threads
                .iter()
                .any(|other| other.id != thread.id && other.worktree_path.as_deref() == Some(worktree_path))
            {
                return Ok(());
            }
            // `expectedBranch` makes this a compare-and-swap in the decider.
            self.dispatch(json!({
                "type": "thread.meta.update",
                "commandId": server_command_id("worktree-branch-drift"),
                "threadId": thread.id,
                "branch": checked_out,
                "expectedBranch": branch,
            }))
            .await?;
            tracing::info!(thread_id = %thread.id, previous_branch = branch, branch = checked_out, "thread branch followed worktree checkout");
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(thread_id = %thread_id, cause = %error, "failed to follow worktree branch drift");
        }
    }

    /// `isRestoreWorkspaceIsolated`: checkpoints contain the whole checkout, so restoring a
    /// shared cwd could erase a sibling thread's work.
    async fn is_restore_workspace_isolated(&self, thread: &OrchestrationThread, cwd: &str) -> Result<bool> {
        let Some(worktree_path) = thread.worktree_path.as_deref() else {
            return Ok(false);
        };
        let realpath = |path: &str| std::fs::canonicalize(path).map_err(|error| ReactorError(format!("{error}: {path}")));
        let canonical_cwd = realpath(cwd)?;
        if realpath(worktree_path)? != canonical_cwd {
            return Ok(false);
        }
        let active = self.deps.projections.get_shell_snapshot(false).await?;
        let archived = self.deps.projections.get_archived_shell_snapshot().await?;
        let projects: Vec<_> = active.projects.iter().chain(archived.projects.iter()).collect();
        let mut paths: HashSet<String> = HashSet::new();
        for other in active.threads.iter().chain(archived.threads.iter()) {
            if other.id == thread.id {
                continue;
            }
            let candidate = other
                .worktree_path
                .clone()
                .or_else(|| projects.iter().find(|p| p.id == other.project_id).map(|p| p.workspace_root.clone()));
            if let Some(candidate) = candidate {
                paths.insert(candidate);
            }
        }
        for session in self.deps.providers.list_sessions().await {
            if session.thread_id() != Some(thread.id.as_str()) && session.status() != Some("closed") {
                if let Some(cwd) = session.cwd() {
                    paths.insert(cwd.to_owned());
                }
            }
        }
        for candidate in paths {
            let other: PathBuf = match std::fs::canonicalize(&candidate) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(ReactorError(format!("{error}: {candidate}"))),
            };
            // Parent and nested owners can both have files inside the restore target.
            if is_within(&canonical_cwd, &other) || is_within(&other, &canonical_cwd) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `handleRevertRequested`.
    async fn handle_revert_requested(&self, thread_id: &ThreadId, turn_count: i64, restore_files: bool) -> Result<()> {
        let now = now_iso();
        let Some(thread) = self.resolve_thread_detail(thread_id.as_str()).await? else {
            let _ = self
                .append_revert_failure_activity(thread_id, turn_count, "Thread was not found in read model.", &now)
                .await;
            return Ok(());
        };

        let projects = self.resolve_thread_projects(&thread.project_id).await?;
        let checkpoint_cwd = match self.resolve_checkpoint_cwd(&thread, &projects, true).await {
            Ok(cwd) => cwd,
            Err(_) if !restore_files => None,
            Err(error) => return Err(error),
        };
        let current = current_turn_count(&thread);
        if turn_count > current {
            let detail = format!("Checkpoint turn count {turn_count} exceeds current turn count {current}.");
            let _ = self.append_revert_failure_activity(thread_id, turn_count, &detail, &now).await;
            return Ok(());
        }

        self.deps.providers.assert_conversation_rollback_supported(thread_id).await?;

        if restore_files {
            let Some(cwd) = checkpoint_cwd.as_deref() else {
                let _ = self
                    .append_revert_failure_activity(thread_id, turn_count, "Checkpoint workspace is unavailable or is not a git repository.", &now)
                    .await;
                return Ok(());
            };
            if !self.is_restore_workspace_isolated(&thread, cwd).await? {
                let _ = self
                    .append_revert_failure_activity(
                        &thread.id,
                        turn_count,
                        "File restore requires an isolated worktree. This workspace may contain changes from another thread. Rewind the conversation without restoring files instead.",
                        &now,
                    )
                    .await;
                return Ok(());
            }
            let target = if turn_count == 0 {
                Some(checkpoint_ref_for_thread_turn(thread_id, 0))
            } else {
                thread
                    .checkpoints
                    .iter()
                    .find(|c| c.checkpoint_turn_count == turn_count)
                    .map(|c| c.checkpoint_ref.clone())
            };
            let Some(target) = target else {
                let detail = format!("Checkpoint ref for turn {turn_count} is unavailable in read model.");
                let _ = self.append_revert_failure_activity(thread_id, turn_count, &detail, &now).await;
                return Ok(());
            };
            if !self.deps.store.restore_checkpoint(cwd, &target, turn_count == 0).await? {
                let detail = format!("Filesystem checkpoint is unavailable for turn {turn_count}.");
                let _ = self.append_revert_failure_activity(thread_id, turn_count, &detail, &now).await;
                return Ok(());
            }
            self.refresh_workspace_entries(cwd);
        }

        let rolled_back_turns = (current - turn_count).max(0);
        if rolled_back_turns > 0 {
            self.deps
                .providers
                .rollback_conversation(thread_id, u32::try_from(rolled_back_turns).unwrap_or(u32::MAX))
                .await?;
        }

        let stale: Vec<CheckpointRef> = thread
            .checkpoints
            .iter()
            .filter(|c| c.checkpoint_turn_count > turn_count)
            .map(|c| c.checkpoint_ref.clone())
            .collect();
        if let Some(cwd) = checkpoint_cwd.as_deref() {
            if !stale.is_empty() {
                self.deps.store.delete_checkpoint_refs(cwd, &stale).await?;
            }
        }

        let completed = self
            .dispatch(json!({
                "type": "thread.revert.complete",
                "commandId": server_command_id("checkpoint-revert-complete"),
                "threadId": thread_id,
                "turnCount": turn_count,
                "createdAt": now,
            }))
            .await;
        if let Err(error) = completed {
            self.append_revert_failure_activity(thread_id, turn_count, &error.0, &now).await?;
        }
        Ok(())
    }

    /// `processDomainEvent`.
    async fn process_domain_event(&self, event: OrchestrationEvent) -> Result<()> {
        match event {
            OrchestrationEvent::ThreadTurnStartRequested(event) => {
                self.pending.lock().unwrap().insert(event.payload.thread_id.0.clone());
                self.ensure_pre_turn_baseline(event.payload.thread_id.as_str(), &event.occurred_at).await
            }
            OrchestrationEvent::ThreadMessageSent(event) => {
                // A bootstrap message lands before the worktree exists; the turn-start event
                // that follows captures against the right cwd.
                let metadata = &event.metadata;
                let payload = &event.payload;
                if metadata.history_import == Some(true)
                    || metadata.deferred_turn == Some(true)
                    || payload.role != OrchestrationMessageRole::User
                    || payload.streaming
                    || payload.turn_id.is_some()
                {
                    return Ok(());
                }
                self.ensure_pre_turn_baseline(payload.thread_id.as_str(), &event.occurred_at).await
            }
            OrchestrationEvent::ThreadCheckpointRevertRequested(event) => {
                let payload = event.payload;
                if let Err(error) = self
                    .handle_revert_requested(&payload.thread_id, payload.turn_count, payload.restore_files != Some(false))
                    .await
                {
                    self.append_revert_failure_activity(&payload.thread_id, payload.turn_count, &error.0, &now_iso())
                        .await?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// `processRuntimeEvent`.
    async fn process_runtime_event(&self, event: ProviderRuntimeEvent) -> Result<()> {
        let thread_id = event.thread_id().to_owned();
        match event.event_type() {
            "session.exited" => {
                self.started_turns.lock().unwrap().remove(&thread_id);
                self.pending.lock().unwrap().remove(&thread_id);
                Ok(())
            }
            "turn.started" => {
                let turn_id = event.turn_id();
                let active_turn_id = self
                    .deps
                    .providers
                    .list_sessions()
                    .await
                    .into_iter()
                    .find(|session| session.thread_id() == Some(thread_id.as_str()))
                    .and_then(|session| session.active_turn_id().map(str::to_owned));
                let may_replace = self.pending.lock().unwrap().contains(&thread_id) && same_id(active_turn_id.as_deref(), turn_id.as_deref());
                if let Some(turn_id) = turn_id {
                    let mut started = self.started_turns.lock().unwrap();
                    if !started.contains_key(&thread_id) || may_replace {
                        started.insert(thread_id.clone(), turn_id);
                        self.pending.lock().unwrap().remove(&thread_id);
                    }
                }
                self.ensure_pre_turn_baseline(&thread_id, event.created_at()).await
            }
            "turn.completed" | "turn.aborted" => {
                let turn_id = event.turn_id();
                let thread = self.resolve_thread_detail(&thread_id).await?;
                let started_turn_id = self.started_turns.lock().unwrap().get(&thread_id).cloned();
                let is_tracked = same_id(started_turn_id.as_deref(), turn_id.as_deref());
                if is_tracked {
                    self.started_turns.lock().unwrap().remove(&thread_id);
                }
                if event.event_type() == "turn.completed" {
                    self.status_refresh_worker.enqueue(event.clone());
                }
                let active_turn_id = thread
                    .as_ref()
                    .and_then(|t| t.session.as_ref())
                    .and_then(|s| s.active_turn_id.as_ref())
                    .map(|t| t.as_str().to_owned());
                if let (Some(_), Some(thread)) = (&turn_id, &thread) {
                    if is_tracked || same_id(active_turn_id.as_deref(), turn_id.as_deref()) || (started_turn_id.is_none() && active_turn_id.is_none()) {
                        self.pending.lock().unwrap().remove(&thread_id);
                        self.deps.pull_requests.refresh_after_turn(&thread.project_id).await;
                    }
                }
                if event.event_type() == "turn.aborted" && !is_tracked && !same_id(active_turn_id.as_deref(), turn_id.as_deref()) {
                    return Ok(());
                }
                if let Err(error) = self.capture_checkpoint_from_turn_completion(&event).await {
                    let _ = self.append_capture_failure_activity(&thread_id, turn_id.as_deref(), &error.0, &now_iso()).await;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn domain_event_type(event: &OrchestrationEvent) -> &'static str {
    match event {
        OrchestrationEvent::ThreadTurnStartRequested(_) => "thread.turn-start-requested",
        OrchestrationEvent::ThreadMessageSent(_) => "thread.message-sent",
        OrchestrationEvent::ThreadCheckpointRevertRequested(_) => "thread.checkpoint-revert-requested",
        _ => "other",
    }
}
