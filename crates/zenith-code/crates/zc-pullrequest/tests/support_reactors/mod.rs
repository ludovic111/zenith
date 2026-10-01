//! Fakes of the ports the pull request reactors use, in the style of zc-reactors' settlement
//! tests: scripted hooks, recorded calls, a read channel the tests wait on (the TS harnesses'
//! `Queue.take(reads)`), and a clock that follows tokio's paused time (`TestClock`).

#![allow(dead_code)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use zc_contracts::*;
use zc_core::PubSub;
use zc_ports::contracts as ports;
use zc_ports::git::{CreateWorktreeOptions, GitBranchPullRequest, GitRemoteStatusOptions, GitRunStackedActionOptions, RemoteTrackingCommit};
use zc_ports::orchestration::*;
use zc_ports::pull_requests::PullRequestMergeEvent;
use zc_ports::{DispatchResult, EventStream, GitWorkflow, OrchestrationDispatch, ProjectionReads, PullRequests, TaggedError};
use zc_pullrequest::reactors::{Activation, RepositoryIdentities};
use zc_reactors::common::UuidSource;
use zc_reactors::ReactorClock;

pub type Hook<A, R> = Arc<dyn Fn(A) -> BoxFuture<'static, R> + Send + Sync>;

pub fn hook<A, R, F, Fut>(f: F) -> Hook<A, R>
where
    F: Fn(A) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = R> + Send + 'static,
{
    Arc::new(move |a| f(a).boxed())
}

pub fn decode<T: serde::de::DeserializeOwned>(value: Value) -> T {
    serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("decode {value}: {error}"))
}

pub fn encode<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap()
}

/// `base` with `overrides`' keys replaced.
pub fn merged(mut base: Value, overrides: Value) -> Value {
    for (key, value) in overrides.as_object().unwrap() {
        base[key] = value.clone();
    }
    base
}

/// A `Deferred<void>`: opened once, awaited by many.
#[derive(Clone)]
pub struct Latch(Arc<tokio::sync::watch::Sender<bool>>);

impl Default for Latch {
    fn default() -> Self {
        Self::new()
    }
}

impl Latch {
    pub fn new() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(false).0))
    }
    pub fn open(&self) {
        self.0.send_replace(true);
    }
    pub async fn wait(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|open| *open).await;
    }
    pub fn activation(&self) -> Activation {
        let latch = self.clone();
        async move { latch.wait().await }.boxed()
    }
}

/// `TestClock`: `base` plus the time tokio's (paused) clock has advanced since creation.
pub struct TestClock {
    base: i64,
    origin: tokio::time::Instant,
}

impl TestClock {
    pub fn at(iso: &str) -> Arc<Self> {
        Arc::new(Self {
            base: zc_core::time::parse_iso_millis(iso).expect("iso time"),
            origin: tokio::time::Instant::now(),
        })
    }
}

impl ReactorClock for TestClock {
    fn now_millis(&self) -> i64 {
        self.base + i64::try_from(self.origin.elapsed().as_millis()).unwrap()
    }
}

/// `crypto.randomUUIDv4` from a counter.
pub fn counter_uuids() -> UuidSource {
    let counter = Arc::new(AtomicUsize::new(0));
    Arc::new(move || format!("00000000-0000-4000-8000-{:012}", counter.fetch_add(1, Ordering::SeqCst) + 1))
}

/// Waits for the next recorded read (`Queue.take(reads)`).
pub async fn take<T>(reads: &tokio::sync::Mutex<mpsc::UnboundedReceiver<T>>) -> T {
    reads.lock().await.recv().await.expect("a read")
}

fn unsupported<T>(operation: &str) -> Result<T, TaggedError> {
    Err(TaggedError::new("Defect", format!("unexpected call: {operation}")))
}

// ---------------------------------------------------------------------------------------------
// Projections

pub type ShellHook = Hook<bool, Result<OrchestrationShellSnapshot, TaggedError>>;

/// `ProjectionSnapshotQuery` over one in-memory shell snapshot. Every shell read is recorded
/// on `reads`: the thread id of a one-thread read, `None` for a full read or a
/// `listThreadsWithPullRequests`.
pub struct Projections {
    pub snapshot: Mutex<OrchestrationShellSnapshot>,
    pub reads: mpsc::UnboundedSender<Option<String>>,
    pub shell_snapshot_reads: AtomicUsize,
    /// Serves full shell reads instead of `snapshot`.
    pub shell_hook: Option<ShellHook>,
}

impl Projections {
    pub fn new(
        snapshot: OrchestrationShellSnapshot,
        shell_hook: Option<ShellHook>,
    ) -> (Arc<Self>, tokio::sync::Mutex<mpsc::UnboundedReceiver<Option<String>>>) {
        let (reads, receiver) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                snapshot: Mutex::new(snapshot),
                reads,
                shell_snapshot_reads: AtomicUsize::new(0),
                shell_hook,
            }),
            tokio::sync::Mutex::new(receiver),
        )
    }

    pub fn get(&self) -> OrchestrationShellSnapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub fn set(&self, snapshot: OrchestrationShellSnapshot) {
        *self.snapshot.lock().unwrap() = snapshot;
    }

    pub fn update(&self, f: impl FnOnce(&mut OrchestrationShellSnapshot)) {
        f(&mut self.snapshot.lock().unwrap());
    }
}

#[async_trait]
impl ProjectionReads for Projections {
    async fn get_user_input_activity(&self, _: &ThreadId, _: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
        unsupported("getUserInputActivity")
    }
    async fn list_activities_by_kind(&self, _: &str) -> Result<Vec<OrchestrationThreadActivity>, TaggedError> {
        unsupported("listActivitiesByKind")
    }
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
        unsupported("getCommandReadModel")
    }
    async fn get_snapshot(&self) -> Result<OrchestrationReadModel, TaggedError> {
        unsupported("getSnapshot")
    }
    async fn get_shell_snapshot(&self, unsettled_only: bool) -> Result<OrchestrationShellSnapshot, TaggedError> {
        self.shell_snapshot_reads.fetch_add(1, Ordering::SeqCst);
        let snapshot = match &self.shell_hook {
            Some(hook) => hook(unsettled_only).await,
            None => Ok(self.get()),
        };
        let _ = self.reads.send(None);
        snapshot
    }
    async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot, TaggedError> {
        unsupported("getArchivedShellSnapshot")
    }
    async fn list_threads_with_pull_requests(&self) -> Result<Vec<ThreadPullRequests>, TaggedError> {
        let _ = self.reads.send(None);
        Ok(self
            .get()
            .threads
            .into_iter()
            .map(|thread| ThreadPullRequests {
                id: thread.id,
                project_id: thread.project_id,
                settled_override: thread.settled_override.map(|value| value.as_str().to_owned()),
                settled_at: thread.settled_at,
                pull_requests: thread.pull_requests,
            })
            .collect())
    }
    async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>, TaggedError> {
        unsupported("getDeletedWorktreeThreads")
    }
    async fn search_threads(&self, _: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, TaggedError> {
        unsupported("searchThreads")
    }
    async fn get_snapshot_sequence(&self) -> Result<i64, TaggedError> {
        Ok(self.get().snapshot_sequence)
    }
    async fn get_counts(&self) -> Result<SnapshotCounts, TaggedError> {
        unsupported("getCounts")
    }
    async fn get_event_replay_stats(&self, _: i64, _: i64) -> Result<ReplayStats, TaggedError> {
        unsupported("getEventReplayStats")
    }
    async fn get_active_project_by_workspace_root(&self, _: &str) -> Result<Option<OrchestrationProject>, TaggedError> {
        unsupported("getActiveProjectByWorkspaceRoot")
    }
    async fn get_project_shell_by_id(&self, id: &ProjectId) -> Result<Option<OrchestrationProjectShell>, TaggedError> {
        Ok(self.get().projects.into_iter().find(|project| &project.id == id))
    }
    async fn get_project_shells(&self, ids: Option<Vec<ProjectId>>) -> Result<Vec<OrchestrationProjectShell>, TaggedError> {
        Ok(self
            .get()
            .projects
            .into_iter()
            .filter(|project| ids.as_ref().is_none_or(|ids| ids.contains(&project.id)))
            .collect())
    }
    async fn get_first_active_thread_id_by_project_id(&self, _: &ProjectId) -> Result<Option<ThreadId>, TaggedError> {
        unsupported("getFirstActiveThreadIdByProjectId")
    }
    async fn get_imported_agent_session_sources(&self, _: &ProjectId) -> Result<Vec<ImportedAgentSessionSource>, TaggedError> {
        unsupported("getImportedAgentSessionSources")
    }
    async fn get_thread_checkpoint_context(&self, _: &ThreadId) -> Result<Option<ThreadCheckpointContext>, TaggedError> {
        unsupported("getThreadCheckpointContext")
    }
    async fn get_full_thread_diff_context(&self, _: &ThreadId, _: i64) -> Result<Option<FullThreadDiffContext>, TaggedError> {
        unsupported("getFullThreadDiffContext")
    }
    async fn get_thread_shell_by_id(&self, id: &ThreadId) -> Result<Option<OrchestrationThreadShell>, TaggedError> {
        let thread = self.get().threads.into_iter().find(|thread| &thread.id == id && thread.archived_at.is_none());
        let _ = self.reads.send(Some(id.to_string()));
        Ok(thread)
    }
    async fn get_thread_runtime_context(&self, _: &ThreadId) -> Result<Option<ThreadRuntimeContext>, TaggedError> {
        unsupported("getThreadRuntimeContext")
    }
    async fn get_turn_start_message(&self, _: &ThreadId, _: &MessageId) -> Result<Option<TurnStartMessage>, TaggedError> {
        unsupported("getTurnStartMessage")
    }
    async fn get_thread_detail_by_id(&self, _: &ThreadId, _: ThreadDetailQuery) -> Result<Option<OrchestrationThread>, TaggedError> {
        unsupported("getThreadDetailById")
    }
    async fn get_thread_detail_snapshot(
        &self,
        _: &ThreadId,
        _: Option<OrchestrationThreadDetailWindow>,
    ) -> Result<Option<OrchestrationThreadDetailSnapshot>, TaggedError> {
        unsupported("getThreadDetailSnapshot")
    }
}

// ---------------------------------------------------------------------------------------------
// Engine

pub type DispatchHook = Hook<Value, Result<(), TaggedError>>;

/// `OrchestrationEngineService`: records every dispatched command as wire JSON, then runs
/// `on_dispatch` (which may reject it).
pub struct Engine {
    pub commands: Mutex<Vec<Value>>,
    pub on_dispatch: Mutex<Option<DispatchHook>>,
    pub events: PubSub<OrchestrationEvent>,
}

impl Engine {
    pub fn new(on_dispatch: Option<DispatchHook>) -> Arc<Self> {
        Arc::new(Self {
            commands: Mutex::new(Vec::new()),
            on_dispatch: Mutex::new(on_dispatch),
            events: PubSub::new(),
        })
    }

    pub fn commands(&self) -> Vec<Value> {
        self.commands.lock().unwrap().clone()
    }

    pub fn commands_of(&self, kind: &str) -> Vec<Value> {
        self.commands().into_iter().filter(|command| command["type"] == kind).collect()
    }

    pub fn publish(&self, event: Value) {
        self.events.publish(decode(event));
    }
}

#[async_trait]
impl OrchestrationDispatch for Engine {
    async fn dispatch(&self, command: OrchestrationCommand, _origin: Option<ports::OrchestrationClientOrigin>) -> Result<DispatchResult, TaggedError> {
        let value = encode(&command);
        self.commands.lock().unwrap().push(value.clone());
        let on_dispatch = self.on_dispatch.lock().unwrap().clone();
        if let Some(on_dispatch) = on_dispatch {
            on_dispatch(value).await?;
        }
        Ok(DispatchResult { sequence: 1 })
    }
    fn subscribe_domain_events(&self) -> EventStream<OrchestrationEvent> {
        self.events.subscribe().boxed()
    }
    async fn latest_sequence(&self) -> i64 {
        0
    }
    fn read_events(&self, _from: i64, _limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        futures::stream::empty().boxed()
    }
    fn read_thread_events(&self, _range: ThreadReplayRange, _limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        futures::stream::empty().boxed()
    }
    async fn get_thread_replay_stats(&self, _range: ThreadReplayRange, _max: u32) -> Result<ThreadReplayStats, TaggedError> {
        Ok(ThreadReplayStats::default())
    }
}

/// The envelope of a domain event (`sequence`, ids, `metadata: {}`) around `type` + `payload`.
pub fn event(kind: &str, sequence: i64, event_id: &str, aggregate_id: &str, payload: Value) -> Value {
    json!({
        "type": kind, "sequence": sequence, "eventId": event_id, "aggregateKind": "thread", "aggregateId": aggregate_id,
        "occurredAt": "2026-09-01T12:00:00.000Z", "commandId": null, "causationEventId": null, "correlationId": null,
        "metadata": {}, "payload": payload,
    })
}

// ---------------------------------------------------------------------------------------------
// Git

/// `(cwd, branch, refresh)`.
pub type BranchHook = Hook<(String, String, bool), Result<Option<GitBranchPullRequest>, TaggedError>>;

/// `GitManager.branchPullRequest`; everything else is unexpected.
pub struct Git {
    /// `{cwd, branch, refresh}` per call.
    pub branch_calls: Mutex<Vec<Value>>,
    pub branch_hook: Option<BranchHook>,
}

impl Git {
    pub fn new(branch_hook: Option<BranchHook>) -> Arc<Self> {
        Arc::new(Self {
            branch_calls: Mutex::new(Vec::new()),
            branch_hook,
        })
    }

    pub fn calls(&self) -> Vec<Value> {
        self.branch_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl GitWorkflow for Git {
    async fn is_repository(&self, _cwd: &str) -> Result<bool, TaggedError> {
        unsupported("isRepository")
    }
    async fn has_commit(&self, _cwd: &str, _ref_name: &str) -> Result<bool, TaggedError> {
        unsupported("hasCommit")
    }
    async fn status(&self, _input: ports::VcsStatusInput) -> Result<ports::VcsStatusResult, TaggedError> {
        unsupported("status")
    }
    async fn local_status(&self, _input: ports::VcsStatusInput) -> Result<ports::VcsStatusLocalResult, TaggedError> {
        unsupported("localStatus")
    }
    async fn remote_status(
        &self,
        _input: ports::VcsStatusInput,
        _options: GitRemoteStatusOptions,
    ) -> Result<Option<ports::VcsStatusRemoteResult>, TaggedError> {
        unsupported("remoteStatus")
    }
    async fn branch_pull_request(&self, cwd: &str, branch: &str, refresh: bool) -> Result<Option<GitBranchPullRequest>, TaggedError> {
        self.branch_calls
            .lock()
            .unwrap()
            .push(json!({"cwd": cwd, "branch": branch, "refresh": refresh}));
        match &self.branch_hook {
            Some(hook) => hook((cwd.to_owned(), branch.to_owned(), refresh)).await,
            None => Ok(None),
        }
    }
    async fn invalidate_local_status(&self, _cwd: &str) {}
    async fn invalidate_remote_status(&self, _cwd: &str) {}
    async fn invalidate_status(&self, _cwd: &str) {}
    async fn pull_current_branch(&self, _cwd: &str) -> Result<ports::VcsPullResult, TaggedError> {
        unsupported("pullCurrentBranch")
    }
    async fn run_stacked_action(
        &self,
        _input: ports::GitRunStackedActionInput,
        _options: GitRunStackedActionOptions,
    ) -> Result<ports::GitRunStackedActionResult, TaggedError> {
        unsupported("runStackedAction")
    }
    async fn resolve_pull_request(&self, _input: ports::GitPullRequestRefInput) -> Result<ports::GitResolvePullRequestResult, TaggedError> {
        unsupported("resolvePullRequest")
    }
    async fn prepare_pull_request_thread(
        &self,
        _input: ports::GitPreparePullRequestThreadInput,
    ) -> Result<ports::GitPreparePullRequestThreadResult, TaggedError> {
        unsupported("preparePullRequestThread")
    }
    async fn list_refs(&self, _input: ports::VcsListRefsInput) -> Result<ports::VcsListRefsResult, TaggedError> {
        unsupported("listRefs")
    }
    async fn create_worktree(
        &self,
        _input: ports::VcsCreateWorktreeInput,
        _options: CreateWorktreeOptions,
    ) -> Result<ports::VcsCreateWorktreeResult, TaggedError> {
        unsupported("createWorktree")
    }
    async fn fetch_remote(&self, _cwd: &str, _remote: &str, _ref_name: Option<&str>) -> Result<(), TaggedError> {
        unsupported("fetchRemote")
    }
    async fn remote_exists(&self, _cwd: &str, _remote: &str) -> Result<bool, TaggedError> {
        unsupported("remoteExists")
    }
    async fn remote_branch_exists(&self, _cwd: &str, _remote: &str, _ref_name: &str) -> Result<bool, TaggedError> {
        unsupported("remoteBranchExists")
    }
    async fn resolve_remote_tracking_commit(&self, _cwd: &str, _ref_name: &str, _fallback: &str) -> Result<RemoteTrackingCommit, TaggedError> {
        unsupported("resolveRemoteTrackingCommit")
    }
    async fn remove_worktree(&self, _input: ports::VcsRemoveWorktreeInput) -> Result<(), TaggedError> {
        unsupported("removeWorktree")
    }
    async fn prune_worktrees(&self, _cwd: &str) -> Result<(), TaggedError> {
        unsupported("pruneWorktrees")
    }
    async fn create_ref(&self, _input: ports::VcsCreateRefInput) -> Result<ports::VcsCreateRefResult, TaggedError> {
        unsupported("createRef")
    }
    async fn switch_ref(&self, _input: ports::VcsSwitchRefInput) -> Result<ports::VcsSwitchRefResult, TaggedError> {
        unsupported("switchRef")
    }
    async fn rename_branch(&self, _cwd: &str, _old: &str, _new: &str) -> Result<String, TaggedError> {
        unsupported("renameBranch")
    }
}

/// A `GitManagerError` as the git port reports it.
pub fn git_manager_error(cwd: &str, detail: &str) -> TaggedError {
    TaggedError::new("GitManagerError", format!("Git manager failed in branchPullRequest: {detail}"))
        .with("operation", "branchPullRequest")
        .with("cwd", cwd)
        .with("detail", detail)
}

// ---------------------------------------------------------------------------------------------
// Pull requests

pub type RefHook<R> = Hook<Value, Result<R, TaggedError>>;

/// The summary a reference gets when no hook answers.
pub type DefaultSummary = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

/// `PullRequestService`: `summary`, `stack` and `invalidate`, recorded.
#[derive(Default)]
pub struct PullRequestFake {
    pub summary_calls: Mutex<Vec<Value>>,
    pub stack_calls: Mutex<Vec<Value>>,
    pub invalidations: Mutex<Vec<Value>>,
    /// `recoverTransientFailure` of each summary read.
    pub recovery: Mutex<Vec<bool>>,
    /// `includeDetails` of each stack read.
    pub details: Mutex<Vec<bool>>,
    pub summary_hook: Option<RefHook<Value>>,
    pub stack_hook: Option<RefHook<Option<Value>>>,
    pub invalidate_hook: Option<Hook<Value, ()>>,
    /// Builds the default summary of a reference (`summary(input, "open")`).
    pub default_summary: Option<DefaultSummary>,
    pub merges: PubSub<PullRequestMergeEvent>,
}

impl PullRequestFake {
    pub fn summary_calls(&self) -> Vec<Value> {
        self.summary_calls.lock().unwrap().clone()
    }
    pub fn stack_calls(&self) -> Vec<Value> {
        self.stack_calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl PullRequests for PullRequestFake {
    async fn summary(&self, reference: ports::PullRequestRef, recover_transient_failure: bool) -> Result<ports::PullRequestSummary, TaggedError> {
        self.summary_calls.lock().unwrap().push(reference.0.clone());
        self.recovery.lock().unwrap().push(recover_transient_failure);
        let value = match (&self.summary_hook, &self.default_summary) {
            (Some(hook), _) => hook(reference.0).await?,
            (None, Some(default)) => default(&reference.0),
            (None, None) => return unsupported("summary"),
        };
        Ok(ports::PullRequestSummary(value))
    }
    async fn stack(&self, reference: ports::PullRequestRef, include_details: bool) -> Result<Option<ports::PullRequestStack>, TaggedError> {
        self.stack_calls.lock().unwrap().push(reference.0.clone());
        self.details.lock().unwrap().push(include_details);
        match &self.stack_hook {
            Some(hook) => Ok(hook(reference.0).await?.map(ports::PullRequestStack)),
            None => Ok(None),
        }
    }
    async fn diff(&self, _input: ports::PullRequestDiffInput) -> Result<ports::PullRequestDiffResult, TaggedError> {
        unsupported("diff")
    }
    async fn invalidate(&self, input: ports::PullRequestInvalidateInput, _notify_readers: bool) {
        self.invalidations.lock().unwrap().push(input.0.clone());
        if let Some(hook) = &self.invalidate_hook {
            hook(input.0).await;
        }
    }
    async fn refresh_after_turn(&self, _project_id: &ProjectId) {}
    fn subscribe_merges(&self) -> EventStream<PullRequestMergeEvent> {
        self.merges.subscribe().boxed()
    }
    fn subscribe_refreshes(&self) -> EventStream<u64> {
        futures::stream::empty().boxed()
    }
}

/// A `PullRequestOperationError` as the port reports it.
pub fn operation_error(operation: &str, detail: &str) -> TaggedError {
    TaggedError::new("PullRequestOperationError", format!("Pull request operation {operation} failed: {detail}"))
        .with("operation", operation)
        .with("detail", detail)
}

// ---------------------------------------------------------------------------------------------
// Repository identities

/// `(cwd, refresh)`.
pub type IdentityHook = Hook<(String, bool), Option<RepositoryIdentity>>;

pub struct Identities(pub IdentityHook);

#[async_trait]
impl RepositoryIdentities for Identities {
    async fn resolve(&self, cwd: &str, refresh: bool) -> Option<RepositoryIdentity> {
        (self.0)((cwd.to_owned(), refresh)).await
    }
}
