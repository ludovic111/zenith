//! Orchestration ports (`apps/server/src/orchestration/Services/OrchestrationEngine.ts`,
//! `ProjectionSnapshotQuery.ts`).
//!
//! Implemented by zc-orchestration; consumed by the RPC handlers, the HTTP API, the MCP
//! toolkits, git workflows (`thread.pull-request.link` after creating a PR), project clone and
//! agent-session import, and the reactors themselves.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::contracts::{
    AgentSessionImportSource, ApprovalRequestId, CheckpointRef, MessageId, OrchestrationCheckpointSummary, OrchestrationClientOrigin, OrchestrationCommand,
    OrchestrationDispatchError, OrchestrationEvent, OrchestrationMessage, OrchestrationProject, OrchestrationProjectShell, OrchestrationReadModel,
    OrchestrationSearchThreadsInput, OrchestrationSearchThreadsResult, OrchestrationShellSnapshot, OrchestrationThread, OrchestrationThreadActivity,
    OrchestrationThreadDetailSnapshot, OrchestrationThreadDetailWindow, OrchestrationThreadShell, PersistenceError, ProjectId, ThreadId,
};
use crate::EventStream;
use zc_contracts::{OrchestrationSession, ThreadPullRequestLink, ThreadTitleState};

/// `{sequence}` (`DispatchResult`): the sequence of the last event the command persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DispatchResult {
    pub sequence: i64,
}

/// `OrchestrationThreadReplayRange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadReplayRange {
    pub thread_id: ThreadId,
    pub from_sequence_exclusive: i64,
    pub to_sequence_inclusive: i64,
}

/// `OrchestrationAggregateReplayStats` / `ProjectionEventReplayStats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayStats {
    pub event_count: u64,
    pub payload_bytes: u64,
}

/// `OrchestrationAggregateReplayStats` as `getThreadReplayStats` returns it: also whether the
/// range re-creates the thread (`thread.created`), which makes `subscribeThread` fall back to a
/// snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadReplayStats {
    pub event_count: u64,
    pub payload_bytes: u64,
    pub has_create_event: bool,
}

/// `OrchestrationEngineService`: the single serialized writer of the event store.
#[async_trait]
pub trait OrchestrationDispatch: Send + Sync {
    /// `dispatch(command, {origin})`: validate, decide, persist and project in one transaction,
    /// deduplicated by command receipt. `origin` is stamped into every produced event's
    /// `metadata.origin`.
    async fn dispatch(&self, command: OrchestrationCommand, origin: Option<OrchestrationClientOrigin>) -> Result<DispatchResult, OrchestrationDispatchError>;

    /// `subscribeDomainEvents`: every event persisted after this call returns, in dispatch
    /// order, unbounded. (`streamDomainEvents` is the same with lazy subscription; Rust only
    /// offers the eager form.)
    fn subscribe_domain_events(&self) -> EventStream<OrchestrationEvent>;

    /// `latestSequence`: the latest sequence in the engine's authoritative command read model
    /// (0 when empty).
    async fn latest_sequence(&self) -> i64;

    /// `readEvents(fromSequenceExclusive, limit?)`: historical replay in sequence order.
    fn read_events(&self, from_sequence_exclusive: i64, limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, PersistenceError>>;

    /// `readThreadEvents({threadId, fromSequenceExclusive, toSequenceInclusive, limit?})`.
    fn read_thread_events(&self, range: ThreadReplayRange, limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, PersistenceError>>;

    /// `getThreadReplayStats({…range, maxEvents})`: count and payload bytes without decoding.
    async fn get_thread_replay_stats(&self, range: ThreadReplayRange, max_events: u32) -> Result<ThreadReplayStats, PersistenceError>;
}

/// `ProjectionThreadCheckpointContext`.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadCheckpointContext {
    pub thread_id: ThreadId,
    pub project_id: ProjectId,
    pub workspace_root: String,
    pub worktree_path: Option<String>,
    pub checkpoints: Vec<OrchestrationCheckpointSummary>,
}

/// `ProjectionFullThreadDiffContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullThreadDiffContext {
    pub thread_id: ThreadId,
    pub project_id: ProjectId,
    pub workspace_root: String,
    pub worktree_path: Option<String>,
    pub latest_checkpoint_turn_count: i64,
    pub to_checkpoint_ref: Option<CheckpointRef>,
}

/// One row of `getDeletedWorktreeThreads`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedWorktreeThread {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub branch: String,
    pub worktree_path: String,
    pub workspace_root: String,
    pub deleted_at: String,
}

/// One row of `getImportedAgentSessionSources`.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedAgentSessionSource {
    pub thread_id: ThreadId,
    pub source: AgentSessionImportSource,
}

/// `getTurnStartMessage` result.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStartMessage {
    pub message: OrchestrationMessage,
    pub has_other_user_messages: bool,
}

/// `ProjectionSnapshotCounts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapshotCounts {
    pub project_count: u64,
    pub thread_count: u64,
}

/// `ProjectionThreadPullRequests`: `Pick<OrchestrationThreadShell, "id" | "projectId" |
/// "settledOverride" | "settledAt" | "pullRequests">`, for a thread with at least one link.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadPullRequests {
    pub id: ThreadId,
    pub project_id: ProjectId,
    /// `"settled" | "active"`.
    pub settled_override: Option<String>,
    pub settled_at: Option<String>,
    pub pull_requests: Vec<ThreadPullRequestLink>,
}

/// `getThreadRuntimeContext`: `Pick<OrchestrationThreadShell, "id" | "projectId" | "title" |
/// "titleState" | "session">`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRuntimeContext {
    pub id: ThreadId,
    pub project_id: ProjectId,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_state: Option<ThreadTitleState>,
    pub session: Option<OrchestrationSession>,
}

/// `ProjectionThreadDetailQuery`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThreadDetailQuery {
    /// Only these activity kinds (all when `None`).
    pub activity_kinds: Option<Vec<String>>,
}

/// `ProjectionSnapshotQuery`: read-only queries over the projection tables (WAL readers, never
/// the writer). Every method of the TS service is here because the RPC handlers, the HTTP API,
/// MCP, storage cleanup and the reactors between them use all of them.
///
/// The `Pick<OrchestrationThreadShell, …>` results are [`ThreadPullRequests`] and
/// [`ThreadRuntimeContext`]. Implemented by `zc_projections::ProjectionSnapshotQuery`.
#[async_trait]
pub trait ProjectionReads: Send + Sync {
    /// `getUserInputActivity({threadId, requestId})`.
    async fn get_user_input_activity(
        &self,
        thread_id: &ThreadId,
        request_id: &ApprovalRequestId,
    ) -> Result<Option<OrchestrationThreadActivity>, PersistenceError>;

    /// `listActivitiesByKind(kind)`.
    async fn list_activities_by_kind(&self, kind: &str) -> Result<Vec<OrchestrationThreadActivity>, PersistenceError>;

    /// `getCommandReadModel()`: the lightweight model the decider works on.
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, PersistenceError>;

    /// `getSnapshot()`: the full read model (`GET /api/orchestration/snapshot`).
    async fn get_snapshot(&self) -> Result<OrchestrationReadModel, PersistenceError>;

    /// `getShellSnapshot({unsettledOnly?})`.
    async fn get_shell_snapshot(&self, unsettled_only: bool) -> Result<OrchestrationShellSnapshot, PersistenceError>;

    /// `getArchivedShellSnapshot()`.
    async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot, PersistenceError>;

    /// `listThreadsWithPullRequests()`.
    async fn list_threads_with_pull_requests(&self) -> Result<Vec<ThreadPullRequests>, PersistenceError>;

    /// `getDeletedWorktreeThreads()`.
    async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>, PersistenceError>;

    /// `searchThreads(input)`.
    async fn search_threads(&self, input: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, PersistenceError>;

    /// `getSnapshotSequence()`: the minimum cursor over the required projectors.
    async fn get_snapshot_sequence(&self) -> Result<i64, PersistenceError>;

    /// `getCounts()`.
    async fn get_counts(&self) -> Result<SnapshotCounts, PersistenceError>;

    /// `getEventReplayStats({fromSequenceExclusive, toSequenceInclusive})`.
    async fn get_event_replay_stats(&self, from_sequence_exclusive: i64, to_sequence_inclusive: i64) -> Result<ReplayStats, PersistenceError>;

    /// `getActiveProjectByWorkspaceRoot(workspaceRoot)`.
    async fn get_active_project_by_workspace_root(&self, workspace_root: &str) -> Result<Option<OrchestrationProject>, PersistenceError>;

    /// `getProjectShellById(projectId)`.
    async fn get_project_shell_by_id(&self, project_id: &ProjectId) -> Result<Option<OrchestrationProjectShell>, PersistenceError>;

    /// `getProjectShells(projectIds?)`.
    async fn get_project_shells(&self, project_ids: Option<Vec<ProjectId>>) -> Result<Vec<OrchestrationProjectShell>, PersistenceError>;

    /// `getFirstActiveThreadIdByProjectId(projectId)`.
    async fn get_first_active_thread_id_by_project_id(&self, project_id: &ProjectId) -> Result<Option<ThreadId>, PersistenceError>;

    /// `getImportedAgentSessionSources(projectId)`.
    async fn get_imported_agent_session_sources(&self, project_id: &ProjectId) -> Result<Vec<ImportedAgentSessionSource>, PersistenceError>;

    /// `getThreadCheckpointContext(threadId)`.
    async fn get_thread_checkpoint_context(&self, thread_id: &ThreadId) -> Result<Option<ThreadCheckpointContext>, PersistenceError>;

    /// `getFullThreadDiffContext(threadId, toTurnCount)`.
    async fn get_full_thread_diff_context(&self, thread_id: &ThreadId, to_turn_count: i64) -> Result<Option<FullThreadDiffContext>, PersistenceError>;

    /// `getThreadShellById(threadId)`.
    async fn get_thread_shell_by_id(&self, thread_id: &ThreadId) -> Result<Option<OrchestrationThreadShell>, PersistenceError>;

    /// `getThreadRuntimeContext(threadId)`: `{id, projectId, title, titleState, session}`.
    async fn get_thread_runtime_context(&self, thread_id: &ThreadId) -> Result<Option<ThreadRuntimeContext>, PersistenceError>;

    /// `getTurnStartMessage({threadId, messageId})`.
    async fn get_turn_start_message(&self, thread_id: &ThreadId, message_id: &MessageId) -> Result<Option<TurnStartMessage>, PersistenceError>;

    /// `getThreadDetailById(threadId, query?)`.
    async fn get_thread_detail_by_id(&self, thread_id: &ThreadId, query: ThreadDetailQuery) -> Result<Option<OrchestrationThread>, PersistenceError>;

    /// `getThreadDetailSnapshot(threadId, window?)`.
    async fn get_thread_detail_snapshot(
        &self,
        thread_id: &ThreadId,
        window: Option<OrchestrationThreadDetailWindow>,
    ) -> Result<Option<OrchestrationThreadDetailSnapshot>, PersistenceError>;
}
