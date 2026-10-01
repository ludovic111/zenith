//! `orchestration/Layers/ProjectionSnapshotQuery.ts`: read-model snapshots over the projection
//! tables. Every query is the TS SQL verbatim; reads that TS runs in one transaction run in one
//! read transaction here (on a WAL reader when the [`Db`] has one).
//!
//! Results are built as the TS object literals (JSON) and decoded into the generated contract
//! types, which is what the TS row schemas, `decodeThread`/`decodeReadModel` and the RPC/HTTP
//! encoders amount to.

mod rows;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};
use zc_contracts::{
    OrchestrationProject, OrchestrationProjectShell, OrchestrationReadModel, OrchestrationSearchThreadsInput, OrchestrationSearchThreadsResult,
    OrchestrationShellSnapshot, OrchestrationThread, OrchestrationThreadActivity, OrchestrationThreadDetailSnapshot, OrchestrationThreadDetailWindow,
    OrchestrationThreadShell, RepositoryIdentity,
};
use zc_db::{Conn, Db, DbError};

use crate::activity_payload::project_activity_payload;
use crate::cursor::{decode_thread_detail_page_cursor, encode_thread_detail_page_cursor, ThreadDetailPageCursor};
use crate::js;
use crate::pipeline::projector_names;
use crate::pull_requests::legacy_linked_pull_request_of;

use rows::*;

/// `RepositoryIdentityResolver.resolve(cwd)`: the git identity of a workspace (owned by the
/// project crate; it shells out to git behind a cache).
#[async_trait]
pub trait RepositoryIdentityResolver: Send + Sync {
    async fn resolve(&self, cwd: &str) -> Option<RepositoryIdentity>;

    /// `resolve(cwd, {refresh: true})`: drop the cached entries of `cwd` first (after a clone,
    /// a publish, a pull request link). Defaults to a plain resolve.
    async fn refresh(&self, cwd: &str) -> Option<RepositoryIdentity> {
        self.resolve(cwd).await
    }
}

/// No repository identities (tests, or before the project crate is wired).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRepositoryIdentities;

#[async_trait]
impl RepositoryIdentityResolver for NoRepositoryIdentities {
    async fn resolve(&self, _cwd: &str) -> Option<RepositoryIdentity> {
        None
    }
}

/// A fixed `workspaceRoot → identity` table.
#[derive(Debug, Default, Clone)]
pub struct FixedRepositoryIdentities(pub HashMap<String, Option<RepositoryIdentity>>);

#[async_trait]
impl RepositoryIdentityResolver for FixedRepositoryIdentities {
    async fn resolve(&self, cwd: &str) -> Option<RepositoryIdentity> {
        self.0.get(cwd).cloned().flatten()
    }
}

/// The in-memory per-thread state thread shells carry (`ThreadBackgroundLivenessService`,
/// `ThreadPlanProgressService`), owned by the reactors.
pub trait ThreadLiveState: Send + Sync {
    /// `"working" | "monitoring"`, or `None`.
    fn background_liveness(&self, thread_id: &str) -> Option<String>;
    /// `{step, completedSteps, totalSteps}` (encoded), or `None`.
    fn plan_progress(&self, thread_id: &str) -> Option<Value>;
}

/// No background work and no plan progress anywhere (a fresh process).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoThreadLiveState;

impl ThreadLiveState for NoThreadLiveState {
    fn background_liveness(&self, _thread_id: &str) -> Option<String> {
        None
    }
    fn plan_progress(&self, _thread_id: &str) -> Option<Value> {
        None
    }
}

/// `ProjectionSnapshotCounts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SnapshotCounts {
    pub project_count: i64,
    pub thread_count: i64,
}

/// `ProjectionEventReplayStats`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EventReplayStats {
    pub event_count: i64,
    pub payload_bytes: i64,
}

/// `ProjectionThreadCheckpointContext` (checkpoints encoded as `OrchestrationCheckpointSummary`).
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadCheckpointContext {
    pub thread_id: String,
    pub project_id: String,
    pub workspace_root: String,
    pub worktree_path: Option<String>,
    pub checkpoints: Vec<zc_contracts::OrchestrationCheckpointSummary>,
}

/// `ProjectionFullThreadDiffContext`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FullThreadDiffContext {
    pub thread_id: String,
    pub project_id: String,
    pub workspace_root: String,
    pub worktree_path: Option<String>,
    pub latest_checkpoint_turn_count: i64,
    pub to_checkpoint_ref: Option<String>,
}

/// One row of `getDeletedWorktreeThreads`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedWorktreeThread {
    pub id: String,
    pub project_id: String,
    pub branch: String,
    pub worktree_path: String,
    pub workspace_root: String,
    pub deleted_at: String,
}

/// `ProjectionThreadPullRequests`: `Pick<OrchestrationThreadShell, "id" | "projectId" |
/// "settledOverride" | "settledAt" | "pullRequests">`, links encoded.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadPullRequests {
    pub id: String,
    pub project_id: String,
    pub settled_override: Option<String>,
    pub settled_at: Option<String>,
    pub pull_requests: Vec<zc_contracts::ThreadPullRequestLink>,
}

/// `getThreadRuntimeContext`: `{id, projectId, title, titleState, session}`.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadRuntimeContext {
    pub id: String,
    pub project_id: String,
    pub title: String,
    pub title_state: Option<zc_contracts::ThreadTitleState>,
    pub session: Option<zc_contracts::OrchestrationSession>,
}

/// `getTurnStartMessage`.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStartMessage {
    pub message: zc_contracts::OrchestrationMessage,
    pub has_other_user_messages: bool,
}

/// One `getImportedAgentSessionSources` row.
#[derive(Debug, Clone, PartialEq)]
pub struct ImportedAgentSessionSource {
    pub thread_id: String,
    pub source: zc_contracts::AgentSessionImportSource,
}

/// `ProjectionThreadDetailQuery`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ThreadDetailQuery {
    /// Only these activity kinds (and no pinned-request reads); `Some([])` skips activities.
    pub activity_kinds: Option<Vec<String>>,
}

// Keep detail reads consistent with the in-memory projector's retained activity window.
const THREAD_DETAIL_ACTIVITY_LIMIT: i64 = 500;
// Snapshot payloads are decoded and projected in small sequential batches.
const THREAD_DETAIL_ACTIVITY_PAYLOAD_BATCH_SIZE: usize = 25;
// SQLite trim defaults to spaces. Match the whitespace removed by String.trim.
const MESSAGE_TRIM_WHITESPACE: &str = "\t\n\u{0b}\u{0c}\r \u{a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";
// Bounds pathological fan-out of one user turn into hundreds of subagent turns.
const THREAD_DETAIL_MAX_RAW_TURNS_PER_PAGE: i64 = 150;
// Sentinel for an unbounded keyset end; "~" sorts after any ISO timestamp.
const ANCHOR_UNBOUNDED: &str = "~";
const EPOCH: &str = "1970-01-01T00:00:00.000Z";

const REQUIRED_SNAPSHOT_PROJECTORS: &[&str] = &[
    projector_names::PROJECTS,
    projector_names::THREADS,
    projector_names::THREAD_MESSAGES,
    projector_names::THREAD_PROPOSED_PLANS,
    projector_names::THREAD_ACTIVITIES,
    projector_names::THREAD_SESSIONS,
    projector_names::CHECKPOINTS,
];

fn max_iso(left: Option<String>, right: &str) -> Option<String> {
    match left {
        // `left > right` on JS strings: UTF-16 order.
        Some(left) if left.encode_utf16().cmp(right.encode_utf16()) == std::cmp::Ordering::Greater => Some(left),
        _ => Some(right.to_string()),
    }
}

/// `escapeLikePattern`.
fn escape_like_pattern(value: &str) -> String {
    value.replace('!', "!!").replace('%', "!%").replace('_', "!_")
}

fn fold_ascii_case(value: &str) -> String {
    value.chars().map(|c| if c.is_ascii_uppercase() { c.to_ascii_lowercase() } else { c }).collect()
}

/// `buildSearchSnippet`.
pub fn build_search_snippet(text: &str, query: &str) -> String {
    let collapsed = js::collapse_whitespace(text);
    let normalized = js::trim(&collapsed);
    let length = js::utf16_len(normalized);
    if length <= 240 {
        return normalized.to_string();
    }
    let collapsed_query = js::collapse_whitespace(query);
    let normalized_query = fold_ascii_case(js::trim(&collapsed_query));
    let match_index = js::utf16_index_of(&fold_ascii_case(normalized), &normalized_query);
    let body_length: i64 = 236;
    let ideal_start = (match_index - 72).max(0);
    let start = ideal_start.min(length as i64 - body_length);
    let end = (length as i64).min(start + body_length);
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        js::utf16_slice(normalized, start as usize, end as usize),
        if end < length as i64 { "…" } else { "" }
    )
}

/// `computeSnapshotSequence`: the minimum cursor over the required projectors, 0 when any is
/// missing.
fn compute_snapshot_sequence(states: &[StateRow]) -> i64 {
    if states.is_empty() {
        return 0;
    }
    let mut min = i64::MAX;
    for projector in REQUIRED_SNAPSHOT_PROJECTORS {
        let Some(state) = states.iter().rev().find(|state| state.projector == *projector) else {
            return 0;
        };
        min = min.min(state.last_applied_sequence);
    }
    if min == i64::MAX {
        0
    } else {
        min
    }
}

fn opt_str(value: &Option<String>) -> Value {
    value.as_ref().map(|value| Value::String(value.clone())).unwrap_or(Value::Null)
}

fn opt_json(value: &Option<Value>) -> Value {
    value.clone().unwrap_or(Value::Null)
}

fn map_latest_turn(row: &LatestTurnRow) -> Value {
    let state = match row.state.as_str() {
        "error" => "error",
        "interrupted" => "interrupted",
        "completed" => "completed",
        _ => "running",
    };
    let mut out = json!({
        "turnId": row.turn_id,
        "state": state,
        "requestedAt": row.requested_at,
        "startedAt": opt_str(&row.started_at),
        "completedAt": opt_str(&row.completed_at),
        "assistantMessageId": opt_str(&row.assistant_message_id),
    });
    if let (Some(thread_id), Some(plan_id)) = (&row.source_proposed_plan_thread_id, &row.source_proposed_plan_id) {
        out["sourceProposedPlan"] = json!({"threadId": thread_id, "planId": plan_id});
    }
    out
}

fn map_title_regeneration(row: &ThreadRow) -> Value {
    match (&row.title_regeneration_request_id, &row.title_regeneration_started_at) {
        (Some(request_id), Some(started_at)) => {
            json!({"requestId": request_id, "startedAt": started_at})
        }
        _ => Value::Null,
    }
}

fn map_session_row(row: &SessionRow) -> Value {
    let mut out = Map::new();
    out.insert("threadId".into(), Value::from(row.thread_id.clone()));
    out.insert("status".into(), Value::from(row.status.clone()));
    out.insert("providerName".into(), opt_str(&row.provider_name));
    if let Some(instance) = &row.provider_instance_id {
        out.insert("providerInstanceId".into(), Value::from(instance.clone()));
    }
    out.insert("runtimeMode".into(), Value::from(row.runtime_mode.clone()));
    out.insert("activeTurnId".into(), opt_str(&row.active_turn_id));
    out.insert("lastError".into(), opt_str(&row.last_error));
    out.insert("updatedAt".into(), Value::from(row.updated_at.clone()));
    Value::Object(out)
}

fn map_project_fields(row: &ProjectRow, identity: Value) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("id".into(), Value::from(row.project_id.clone()));
    out.insert("title".into(), Value::from(row.title.clone()));
    out.insert("workspaceRoot".into(), Value::from(row.workspace_root.clone()));
    out.insert("repositoryIdentity".into(), identity);
    out.insert("defaultModelSelection".into(), opt_json(&row.default_model_selection));
    out.insert("defaultThreadEnvMode".into(), opt_str(&row.default_thread_env_mode));
    out.insert("autoPull".into(), Value::Bool(row.auto_pull == 1.0));
    out.insert("faviconPath".into(), opt_str(&row.favicon_path));
    out.insert("projectIcon".into(), opt_json(&row.project_icon));
    out.insert("scripts".into(), row.scripts.clone());
    out.insert("createdAt".into(), Value::from(row.created_at.clone()));
    out.insert("updatedAt".into(), Value::from(row.updated_at.clone()));
    out
}

fn map_project_shell_row(row: &ProjectRow, identity: Value) -> Value {
    Value::Object(map_project_fields(row, identity))
}

fn map_project_row(row: &ProjectRow, identity: Value) -> Value {
    let mut out = map_project_fields(row, identity);
    out.insert("deletedAt".into(), opt_str(&row.deleted_at));
    Value::Object(out)
}

fn map_proposed_plan_row(row: &PlanRow) -> Value {
    json!({
        "id": row.plan_id,
        "turnId": opt_str(&row.turn_id),
        "planMarkdown": row.plan_markdown,
        "implementedAt": opt_str(&row.implemented_at),
        "implementationThreadId": opt_str(&row.implementation_thread_id),
        "createdAt": row.created_at,
        "updatedAt": row.updated_at,
    })
}

fn map_pull_request_row(row: &PullRequestRow) -> Value {
    json!({
        "host": row.host,
        "repository": row.repository,
        "number": row.number,
        "url": row.url,
        "source": row.source,
        "linkedAt": row.linked_at,
        "snapshot": opt_json(&row.snapshot),
        "stack": opt_json(&row.stack),
    })
}

fn group_pull_requests_by_thread(rows: &[PullRequestRow]) -> HashMap<String, Vec<Value>> {
    let mut by_thread: HashMap<String, Vec<Value>> = HashMap::new();
    for row in rows {
        by_thread.entry(row.thread_id.clone()).or_default().push(map_pull_request_row(row));
    }
    by_thread
}

/// `mapThreadPullRequests`: `pullRequests` plus the legacy `linkedPullRequest` when one
/// resolves.
fn apply_thread_pull_requests(out: &mut Map<String, Value>, links: Vec<Value>, project_id: &str, identity: Option<&Value>) {
    let linked = legacy_linked_pull_request_of(&links, project_id, identity);
    out.insert("pullRequests".into(), Value::Array(links));
    if let Some(linked) = linked {
        out.insert("linkedPullRequest".into(), linked);
    }
}

fn map_thread_activity_row(row: &ActivityRow) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), Value::from(row.activity_id.clone()));
    out.insert("tone".into(), Value::from(row.tone.clone()));
    out.insert("kind".into(), Value::from(row.kind.clone()));
    out.insert("summary".into(), Value::from(row.summary.clone()));
    out.insert("payload".into(), row.payload.clone());
    out.insert("turnId".into(), opt_str(&row.turn_id));
    out.insert("createdAt".into(), Value::from(row.created_at.clone()));
    if let Some(sequence) = row.sequence {
        out.insert("sequence".into(), Value::from(sequence));
    }
    Value::Object(out)
}

fn map_message_row(row: &MessageRow) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), Value::from(row.message_id.clone()));
    out.insert("role".into(), Value::from(row.role.clone()));
    out.insert("text".into(), Value::from(row.text.clone()));
    if let Some(attachments) = &row.attachments {
        out.insert("attachments".into(), attachments.clone());
    }
    if let Some(context) = &row.context {
        out.insert("context".into(), context.clone());
    }
    out.insert("turnId".into(), opt_str(&row.turn_id));
    out.insert("streaming".into(), Value::Bool(row.is_streaming == 1.0));
    out.insert("createdAt".into(), Value::from(row.created_at.clone()));
    out.insert("updatedAt".into(), Value::from(row.updated_at.clone()));
    Value::Object(out)
}

fn map_checkpoint_row(row: &CheckpointRow) -> Value {
    json!({
        "turnId": row.turn_id,
        "checkpointTurnCount": row.checkpoint_turn_count,
        "checkpointRef": row.checkpoint_ref,
        "status": row.status,
        "files": row.files,
        "assistantMessageId": opt_str(&row.assistant_message_id),
        "completedAt": row.completed_at,
    })
}

/// The thread fields every thread object shares (`OrchestrationThread` and the shell).
fn thread_base_fields(row: &ThreadRow, links: Vec<Value>, identity: Option<&Value>, latest_turn: Value) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("id".into(), Value::from(row.thread_id.clone()));
    out.insert("projectId".into(), Value::from(row.project_id.clone()));
    out.insert("title".into(), Value::from(row.title.clone()));
    out.insert("modelSelection".into(), row.model_selection.clone());
    out.insert("runtimeMode".into(), Value::from(row.runtime_mode.clone()));
    out.insert("interactionMode".into(), Value::from(row.interaction_mode.clone()));
    out.insert("branch".into(), opt_str(&row.branch));
    out.insert("worktreePath".into(), opt_str(&row.worktree_path));
    apply_thread_pull_requests(&mut out, links, &row.project_id, identity);
    out.insert("branchPullRequest".into(), opt_json(&row.branch_pull_request));
    out.insert("latestTurn".into(), latest_turn);
    out.insert("createdAt".into(), Value::from(row.created_at.clone()));
    out.insert("updatedAt".into(), Value::from(row.updated_at.clone()));
    out.insert("archivedAt".into(), opt_str(&row.archived_at));
    out.insert("settledOverride".into(), opt_str(&row.settled_override));
    out.insert("settledAt".into(), opt_str(&row.settled_at));
    out.insert("unsettledAt".into(), opt_str(&row.unsettled_at));
    out.insert("snoozedUntil".into(), opt_str(&row.snoozed_until));
    out.insert("snoozedAt".into(), opt_str(&row.snoozed_at));
    out.insert("pinnedAt".into(), opt_str(&row.pinned_at));
    out.insert("pinOrderKey".into(), opt_str(&row.pin_order_key));
    out.insert("activeOrderKey".into(), opt_str(&row.active_order_key));
    out.insert("autoSettleDisabledAt".into(), opt_str(&row.auto_settle_disabled_at));
    out.insert("titleRegeneration".into(), map_title_regeneration(row));
    out.insert("titleState".into(), opt_json(&row.title_state));
    out
}

fn decode<T: serde::de::DeserializeOwned>(value: Value, operation: &str) -> Result<T, DbError> {
    serde_json::from_value(value).map_err(|error| DbError::decode(operation, error.to_string()))
}

/// `ProjectionSnapshotQuery`.
#[derive(Clone)]
pub struct ProjectionSnapshotQuery {
    db: Db,
    identities: Arc<dyn RepositoryIdentityResolver>,
    live: Arc<dyn ThreadLiveState>,
}

/// A window resolved for a detail read: turn-linked rows in the keyset range
/// `[min, before)`, turnless rows in the matching anchor time range.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ThreadDetailBounds {
    min_anchor_at: String,
    min_turn_key: String,
    before_anchor_at: String,
    before_turn_key: String,
}

impl ThreadDetailBounds {
    fn params(&self, thread_id: &str) -> Vec<SqlValue> {
        vec![
            text(thread_id),
            text(thread_id),
            text(&self.min_anchor_at),
            text(&self.min_anchor_at),
            text(&self.min_turn_key),
            text(&self.before_anchor_at),
            text(&self.before_anchor_at),
            text(&self.before_turn_key),
            text(&self.min_anchor_at),
            text(&self.before_anchor_at),
        ]
    }
}

#[derive(Debug, Clone)]
enum ActivityRead {
    Raw(ThreadDetailQuery),
    Client,
}

/// The rows of one thread detail read, before identities are resolved.
struct ThreadDetailRows {
    thread: ThreadRow,
    messages: Vec<MessageRow>,
    plans: Vec<PlanRow>,
    links: Vec<PullRequestRow>,
    activities: Vec<Value>,
    checkpoints: Vec<CheckpointRow>,
    latest_turn: Option<LatestTurnRow>,
    session: Option<SessionRow>,
    project: Option<ProjectRow>,
}

impl ProjectionSnapshotQuery {
    pub fn new(db: Db, identities: Arc<dyn RepositoryIdentityResolver>, live: Arc<dyn ThreadLiveState>) -> Self {
        Self { db, identities, live }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Runs `f` in one read transaction.
    async fn read<R, F>(&self, f: F) -> Result<R, DbError>
    where
        F: FnOnce(&Conn) -> Result<R, DbError> + Send + 'static,
        R: Send + 'static,
    {
        self.db.read(move |conn| conn.transaction(f)).await
    }

    /// `resolveRepositoryIdentitiesForProjects`: unique workspace roots, active projects only
    /// unless `include_deleted`.
    async fn resolve_identities(&self, projects: &[&ProjectRow], include_deleted: bool) -> HashMap<String, Value> {
        let filtered: Vec<&&ProjectRow> = projects.iter().filter(|row| include_deleted || row.deleted_at.is_none()).collect();
        let mut by_root: HashMap<String, Value> = HashMap::new();
        for row in &filtered {
            if by_root.contains_key(&row.workspace_root) {
                continue;
            }
            let identity = self.identities.resolve(&row.workspace_root).await;
            by_root.insert(
                row.workspace_root.clone(),
                identity
                    .map(|identity| serde_json::to_value(identity).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null),
            );
        }
        filtered
            .iter()
            .map(|row| (row.project_id.clone(), by_root.get(&row.workspace_root).cloned().unwrap_or(Value::Null)))
            .collect()
    }

    async fn resolve_identity(&self, workspace_root: &str) -> Value {
        self.identities
            .resolve(workspace_root)
            .await
            .map(|identity| serde_json::to_value(identity).unwrap_or(Value::Null))
            .unwrap_or(Value::Null)
    }

    fn shell_live_fields(&self, out: &mut Map<String, Value>, thread_id: &str) {
        out.insert(
            "backgroundLiveness".into(),
            self.live.background_liveness(thread_id).map(Value::String).unwrap_or(Value::Null),
        );
        out.insert("planProgress".into(), self.live.plan_progress(thread_id).unwrap_or(Value::Null));
    }

    fn thread_shell(&self, row: &ThreadRow, links: Vec<Value>, identity: Option<&Value>, latest_turn: Value, session: Value) -> Value {
        let mut out = thread_base_fields(row, links, identity, latest_turn);
        out.insert("session".into(), session);
        out.insert("latestUserMessageAt".into(), opt_str(&row.latest_user_message_at));
        out.insert("hasPendingApprovals".into(), Value::Bool(row.pending_approval_count > 0));
        out.insert("hasPendingUserInput".into(), Value::Bool(row.pending_user_input_count > 0));
        out.insert("hasActionableProposedPlan".into(), Value::Bool(row.has_actionable_proposed_plan > 0));
        self.shell_live_fields(&mut out, &row.thread_id);
        Value::Object(out)
    }

    // -----------------------------------------------------------------------------------------
    // Activities by kind / user input
    // -----------------------------------------------------------------------------------------

    /// `getUserInputActivity({threadId, requestId})`.
    pub async fn get_user_input_activity(&self, thread_id: &str, request_id: &str) -> Result<Option<OrchestrationThreadActivity>, DbError> {
        let params = vec![text(thread_id), text(request_id)];
        let row = self
            .read(move |conn| {
                query_one(
                    conn,
                    r#"
      SELECT
        activity_id AS "activityId",
        thread_id AS "threadId",
        turn_id AS "turnId",
        tone,
        kind,
        summary,
        payload_json AS "payload",
        sequence,
        created_at AS "createdAt"
      FROM projection_thread_activities
      WHERE thread_id = ?
        AND kind IN ('user-input.requested', 'user-input.resolved')
        AND json_extract(payload_json, '$.requestId') = ?
      ORDER BY sequence DESC, created_at DESC, activity_id DESC
      LIMIT 1
    "#,
                    params,
                    "ProjectionSnapshotQuery.getUserInputActivity:query",
                    "ProjectionSnapshotQuery.getUserInputActivity:decodeRow",
                    activity_row,
                )
            })
            .await?;
        row.map(|row| decode(map_thread_activity_row(&row), "ProjectionSnapshotQuery.getUserInputActivity:decodeRow"))
            .transpose()
    }

    /// `listActivitiesByKind(kind)`: across active threads.
    pub async fn list_activities_by_kind(&self, kind: &str) -> Result<Vec<OrchestrationThreadActivity>, DbError> {
        let params = vec![text(kind)];
        let rows = self
            .read(move |conn| {
                query_all(
                    conn,
                    r#"
      SELECT
        a.activity_id AS "activityId",
        a.thread_id AS "threadId",
        a.turn_id AS "turnId",
        a.tone,
        a.kind,
        a.summary,
        a.payload_json AS "payload",
        a.sequence,
        a.created_at AS "createdAt"
      FROM projection_thread_activities a
      JOIN projection_threads t ON t.thread_id = a.thread_id
      WHERE a.kind = ?
        AND t.deleted_at IS NULL
        AND t.archived_at IS NULL
      ORDER BY a.created_at ASC, a.activity_id ASC
    "#,
                    params,
                    "ProjectionSnapshotQuery.listActivitiesByKind:query",
                    "ProjectionSnapshotQuery.listActivitiesByKind:decodeRow",
                    activity_row,
                )
            })
            .await?;
        rows.iter()
            .map(|row| decode(map_thread_activity_row(row), "ProjectionSnapshotQuery.listActivitiesByKind:decodeRow"))
            .collect()
    }

    // -----------------------------------------------------------------------------------------
    // Full read models
    // -----------------------------------------------------------------------------------------

    /// `getSnapshot()`: every project and thread, fully hydrated.
    pub async fn get_snapshot(&self) -> Result<OrchestrationReadModel, DbError> {
        let op = |name: &str, kind: &str| format!("ProjectionSnapshotQuery.getSnapshot:{name}:{kind}");
        let (projects, threads, messages, plans, links, activities, sessions, checkpoints, latest, states) =
            self.read(move |conn| {
                Ok((
                    list_project_rows(conn, false, None, &op("listProjects", "query"), &op("listProjects", "decodeRows"))?,
                    query_all(conn, &format!("\n        SELECT{THREAD_COLUMNS}\n        FROM projection_threads\n        ORDER BY created_at ASC, thread_id ASC\n      "), vec![], &op("listThreads", "query"), &op("listThreads", "decodeRows"), thread_row)?,
                    query_all(conn, &format!("\n        SELECT{MESSAGE_COLUMNS}\n        FROM projection_thread_messages\n        ORDER BY thread_id ASC, created_at ASC, message_id ASC\n      "), vec![], &op("listThreadMessages", "query"), &op("listThreadMessages", "decodeRows"), message_row)?,
                    list_plan_rows(conn, None, &op("listThreadProposedPlans", "query"), &op("listThreadProposedPlans", "decodeRows"))?,
                    query_all(conn, r#"
        SELECT
          thread_id AS "threadId",
          host,
          repository,
          number,
          url,
          source,
          linked_at AS "linkedAt",
          snapshot_json AS "snapshot",
          stack_json AS "stack"
        FROM projection_thread_pull_requests
        ORDER BY thread_id ASC, linked_at ASC, number ASC
      "#, vec![], &op("listThreadPullRequests", "query"), &op("listThreadPullRequests", "decodeRows"), pull_request_row)?,
                    query_all(conn, r#"
        SELECT
          activity_id AS "activityId",
          thread_id AS "threadId",
          turn_id AS "turnId",
          tone,
          kind,
          summary,
          payload_json AS "payload",
          sequence,
          created_at AS "createdAt"
        FROM projection_thread_activities
        ORDER BY
          thread_id ASC,
          sequence ASC,
          created_at ASC,
          activity_id ASC
      "#, vec![], &op("listThreadActivities", "query"), &op("listThreadActivities", "decodeRows"), activity_row)?,
                    list_session_rows(conn, &op("listThreadSessions", "query"), &op("listThreadSessions", "decodeRows"))?,
                    query_all(conn, &format!("\n        SELECT{CHECKPOINT_COLUMNS}\n        FROM projection_turns\n        WHERE checkpoint_turn_count IS NOT NULL\n        ORDER BY thread_id ASC, checkpoint_turn_count ASC\n      "), vec![], &op("listCheckpoints", "query"), &op("listCheckpoints", "decodeRows"), checkpoint_row)?,
                    list_latest_turn_rows(conn, LatestTurnScope::All, &op("listLatestTurns", "query"), &op("listLatestTurns", "decodeRows"))?,
                    list_state_rows(conn, &op("listProjectionState", "query"), &op("listProjectionState", "decodeRows"))?,
                ))
            })
            .await?;

        let mut updated_at: Option<String> = None;
        for row in &projects {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        for row in &threads {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        for row in &states {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        let mut messages_by_thread: HashMap<String, Vec<Value>> = HashMap::new();
        for row in &messages {
            updated_at = max_iso(updated_at, &row.updated_at);
            messages_by_thread.entry(row.thread_id.clone()).or_default().push(map_message_row(row));
        }
        let mut plans_by_thread: HashMap<String, Vec<Value>> = HashMap::new();
        for row in &plans {
            updated_at = max_iso(updated_at, &row.updated_at);
            plans_by_thread.entry(row.thread_id.clone()).or_default().push(map_proposed_plan_row(row));
        }
        let mut activities_by_thread: HashMap<String, Vec<Value>> = HashMap::new();
        for row in &activities {
            updated_at = max_iso(updated_at, &row.created_at);
            activities_by_thread
                .entry(row.thread_id.clone())
                .or_default()
                .push(map_thread_activity_row(row));
        }
        let mut checkpoints_by_thread: HashMap<String, Vec<Value>> = HashMap::new();
        for row in &checkpoints {
            updated_at = max_iso(updated_at, &row.completed_at);
            checkpoints_by_thread.entry(row.thread_id.clone()).or_default().push(map_checkpoint_row(row));
        }
        let mut latest_by_thread: HashMap<String, Value> = HashMap::new();
        for row in &latest {
            updated_at = max_iso(updated_at, &row.requested_at);
            if let Some(started) = &row.started_at {
                updated_at = max_iso(updated_at, started);
            }
            if let Some(completed) = &row.completed_at {
                updated_at = max_iso(updated_at, completed);
            }
            latest_by_thread.entry(row.thread_id.clone()).or_insert_with(|| map_latest_turn(row));
        }
        let mut sessions_by_thread: HashMap<String, Value> = HashMap::new();
        for row in &sessions {
            updated_at = max_iso(updated_at, &row.updated_at);
            sessions_by_thread.insert(row.thread_id.clone(), map_session_row(row));
        }
        let mut links_by_thread = group_pull_requests_by_thread(&links);

        let project_refs: Vec<&ProjectRow> = projects.iter().collect();
        let identities = self.resolve_identities(&project_refs, true).await;
        let projects_out: Vec<Value> = projects
            .iter()
            .map(|row| map_project_row(row, identities.get(&row.project_id).cloned().unwrap_or(Value::Null)))
            .collect();
        let threads_out: Vec<Value> = threads
            .iter()
            .map(|row| {
                let identity = identities.get(&row.project_id);
                let mut out = thread_base_fields(
                    row,
                    links_by_thread.remove(&row.thread_id).unwrap_or_default(),
                    identity,
                    latest_by_thread.get(&row.thread_id).cloned().unwrap_or(Value::Null),
                );
                out.insert("deletedAt".into(), opt_str(&row.deleted_at));
                out.insert("messages".into(), Value::Array(messages_by_thread.remove(&row.thread_id).unwrap_or_default()));
                out.insert("proposedPlans".into(), Value::Array(plans_by_thread.remove(&row.thread_id).unwrap_or_default()));
                out.insert(
                    "activities".into(),
                    Value::Array(activities_by_thread.remove(&row.thread_id).unwrap_or_default()),
                );
                out.insert(
                    "checkpoints".into(),
                    Value::Array(checkpoints_by_thread.remove(&row.thread_id).unwrap_or_default()),
                );
                out.insert("session".into(), sessions_by_thread.get(&row.thread_id).cloned().unwrap_or(Value::Null));
                Value::Object(out)
            })
            .collect();
        decode(
            json!({
                "snapshotSequence": compute_snapshot_sequence(&states),
                "projects": projects_out,
                "threads": threads_out,
                "updatedAt": updated_at.unwrap_or_else(|| EPOCH.to_string()),
            }),
            "ProjectionSnapshotQuery.getSnapshot:decodeReadModel",
        )
    }

    /// `getCommandReadModel()`: projects and thread metadata without message, activity or
    /// checkpoint bodies (the engine's in-memory model, and `GET /api/orchestration/snapshot`).
    pub async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, DbError> {
        let op = |name: &str, kind: &str| format!("ProjectionSnapshotQuery.getCommandReadModel:{name}:{kind}");
        let (projects, threads, plans, links, sessions, latest, states) = self
            .read(move |conn| {
                Ok((
                    list_project_rows(conn, false, None, &op("listProjects", "query"), &op("listProjects", "decodeRows"))?,
                    query_all(
                        conn,
                        &format!("\n        SELECT{THREAD_COLUMNS}\n        FROM projection_threads\n        ORDER BY created_at ASC, thread_id ASC\n      "),
                        vec![],
                        &op("listThreads", "query"),
                        &op("listThreads", "decodeRows"),
                        thread_row,
                    )?,
                    list_plan_rows(
                        conn,
                        None,
                        &op("listThreadProposedPlans", "query"),
                        &op("listThreadProposedPlans", "decodeRows"),
                    )?,
                    query_all(
                        conn,
                        r#"
        SELECT
          thread_id AS "threadId",
          host,
          repository,
          number,
          url,
          source,
          linked_at AS "linkedAt",
          snapshot_json AS "snapshot",
          stack_json AS "stack"
        FROM projection_thread_pull_requests
        ORDER BY thread_id ASC, linked_at ASC, number ASC
      "#,
                        vec![],
                        &op("listThreadPullRequests", "query"),
                        &op("listThreadPullRequests", "decodeRows"),
                        pull_request_row,
                    )?,
                    list_session_rows(conn, &op("listThreadSessions", "query"), &op("listThreadSessions", "decodeRows"))?,
                    list_latest_turn_rows(
                        conn,
                        LatestTurnScope::All,
                        &op("listLatestTurns", "query"),
                        &op("listLatestTurns", "decodeRows"),
                    )?,
                    list_state_rows(conn, &op("listProjectionState", "query"), &op("listProjectionState", "decodeRows"))?,
                ))
            })
            .await?;
        let linked_thread_ids: HashSet<&str> = links.iter().map(|row| row.thread_id.as_str()).collect();
        let linked_project_ids: HashSet<&str> = threads
            .iter()
            .filter(|row| linked_thread_ids.contains(row.thread_id.as_str()))
            .map(|row| row.project_id.as_str())
            .collect();
        let linked_projects: Vec<&ProjectRow> = projects.iter().filter(|row| linked_project_ids.contains(row.project_id.as_str())).collect();
        let identities = self.resolve_identities(&linked_projects, false).await;

        let mut updated_at: Option<String> = None;
        for row in &projects {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        for row in &threads {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        for row in &plans {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        for row in &sessions {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        for row in &latest {
            updated_at = max_iso(updated_at, &row.requested_at);
            if let Some(started) = &row.started_at {
                updated_at = max_iso(updated_at, started);
            }
            if let Some(completed) = &row.completed_at {
                updated_at = max_iso(updated_at, completed);
            }
        }
        for row in &states {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        let projects_out: Vec<Value> = projects
            .iter()
            .map(|row| map_project_row(row, identities.get(&row.project_id).cloned().unwrap_or(Value::Null)))
            .collect();
        // `new Map(rows.map(...))`: the last row per thread wins.
        let latest_by_thread: HashMap<String, Value> = latest.iter().map(|row| (row.thread_id.clone(), map_latest_turn(row))).collect();
        let sessions_by_thread: HashMap<String, Value> = sessions.iter().map(|row| (row.thread_id.clone(), map_session_row(row))).collect();
        let mut plans_by_thread: HashMap<String, Vec<Value>> = HashMap::new();
        for row in &plans {
            plans_by_thread.entry(row.thread_id.clone()).or_default().push(map_proposed_plan_row(row));
        }
        let mut links_by_thread = group_pull_requests_by_thread(&links);
        let threads_out: Vec<Value> = threads
            .iter()
            .map(|row| {
                let mut out = thread_base_fields(
                    row,
                    links_by_thread.remove(&row.thread_id).unwrap_or_default(),
                    identities.get(&row.project_id),
                    latest_by_thread.get(&row.thread_id).cloned().unwrap_or(Value::Null),
                );
                out.insert("deletedAt".into(), opt_str(&row.deleted_at));
                out.insert("messages".into(), Value::Array(vec![]));
                out.insert("proposedPlans".into(), Value::Array(plans_by_thread.remove(&row.thread_id).unwrap_or_default()));
                out.insert("activities".into(), Value::Array(vec![]));
                out.insert("checkpoints".into(), Value::Array(vec![]));
                out.insert("session".into(), sessions_by_thread.get(&row.thread_id).cloned().unwrap_or(Value::Null));
                Value::Object(out)
            })
            .collect();
        decode(
            json!({
                "snapshotSequence": compute_snapshot_sequence(&states),
                "projects": projects_out,
                "threads": threads_out,
                "updatedAt": updated_at.unwrap_or_else(|| EPOCH.to_string()),
            }),
            "ProjectionSnapshotQuery.getCommandReadModel:query",
        )
    }

    // -----------------------------------------------------------------------------------------
    // Shell snapshots
    // -----------------------------------------------------------------------------------------

    /// `getShellSnapshot({unsettledOnly})`: projects and active thread shells.
    pub async fn get_shell_snapshot(&self, unsettled_only: bool) -> Result<OrchestrationShellSnapshot, DbError> {
        let op = |name: &str, kind: &str| format!("ProjectionSnapshotQuery.getShellSnapshot:{name}:{kind}");
        let filter = if unsettled_only {
            "AND threads.settled_at IS NULL AND threads.settled_override IS NOT 'settled'"
        } else {
            ""
        };
        let (projects, threads, sessions, links, latest, states) = self
            .read(move |conn| {
                Ok((
                    list_project_rows(conn, false, None, &op("listProjects", "query"), &op("listProjects", "decodeRows"))?,
                    query_all(conn, &format!("\n        SELECT{THREAD_COLUMNS}\n        FROM projection_threads threads\n        WHERE deleted_at IS NULL\n          AND archived_at IS NULL\n          {filter}\n        ORDER BY project_id ASC, created_at ASC, thread_id ASC\n      "), vec![], &op("listThreads", "query"), &op("listThreads", "decodeRows"), thread_row)?,
                    query_all(conn, &format!(r#"
        SELECT
          sessions.thread_id AS "threadId",
          sessions.status,
          sessions.provider_name AS "providerName",
          sessions.provider_instance_id AS "providerInstanceId",
          sessions.provider_session_id AS "providerSessionId",
          sessions.provider_thread_id AS "providerThreadId",
          sessions.runtime_mode AS "runtimeMode",
          sessions.active_turn_id AS "activeTurnId",
          sessions.last_error AS "lastError",
          sessions.updated_at AS "updatedAt"
        FROM projection_thread_sessions sessions
        INNER JOIN projection_threads threads
          ON threads.thread_id = sessions.thread_id
        WHERE threads.deleted_at IS NULL
          AND threads.archived_at IS NULL
          {filter}
        ORDER BY sessions.thread_id ASC
      "#), vec![], &op("listThreadSessions", "query"), &op("listThreadSessions", "decodeRows"), session_row)?,
                    query_all(conn, &format!(r#"
        SELECT
          links.thread_id AS "threadId",
          links.host,
          links.repository,
          links.number,
          links.url,
          links.source,
          links.linked_at AS "linkedAt",
          links.snapshot_json AS "snapshot",
          links.stack_json AS "stack"
        FROM projection_thread_pull_requests links
        INNER JOIN projection_threads threads
          ON threads.thread_id = links.thread_id
        WHERE threads.deleted_at IS NULL
          AND threads.archived_at IS NULL
          {filter}
        ORDER BY links.thread_id ASC, links.linked_at ASC, links.number ASC
      "#), vec![], &op("listThreadPullRequests", "query"), &op("listThreadPullRequests", "decodeRows"), pull_request_row)?,
                    list_latest_turn_rows(conn, LatestTurnScope::Active(filter), &op("listLatestTurns", "query"), &op("listLatestTurns", "decodeRows"))?,
                    list_state_rows(conn, &op("listProjectionState", "query"), &op("listProjectionState", "decodeRows"))?,
                ))
            })
            .await?;
        let snapshot = self
            .build_shell_snapshot(projects, threads, sessions, links, latest, states, ShellKind::Active)
            .await;
        decode(snapshot, "ProjectionSnapshotQuery.getShellSnapshot:query")
    }

    /// `getArchivedShellSnapshot()`: archived thread shells and their projects.
    pub async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot, DbError> {
        let op = |name: &str, kind: &str| format!("ProjectionSnapshotQuery.getArchivedShellSnapshot:{name}:{kind}");
        let (projects, threads, sessions, links, latest, states) = self
            .read(move |conn| {
                Ok((
                    list_project_rows(conn, false, None, &op("listProjects", "query"), &op("listProjects", "decodeRows"))?,
                    query_all(conn, &format!("\n        SELECT{THREAD_COLUMNS}\n        FROM projection_threads\n        WHERE deleted_at IS NULL\n          AND archived_at IS NOT NULL\n        ORDER BY project_id ASC, archived_at DESC, thread_id DESC\n      "), vec![], &op("listThreads", "query"), &op("listThreads", "decodeRows"), thread_row)?,
                    query_all(conn, r#"
        SELECT
          sessions.thread_id AS "threadId",
          sessions.status,
          sessions.provider_name AS "providerName",
          sessions.provider_instance_id AS "providerInstanceId",
          sessions.provider_session_id AS "providerSessionId",
          sessions.provider_thread_id AS "providerThreadId",
          sessions.runtime_mode AS "runtimeMode",
          sessions.active_turn_id AS "activeTurnId",
          sessions.last_error AS "lastError",
          sessions.updated_at AS "updatedAt"
        FROM projection_thread_sessions sessions
        INNER JOIN projection_threads threads
          ON threads.thread_id = sessions.thread_id
        WHERE threads.deleted_at IS NULL
          AND threads.archived_at IS NOT NULL
        ORDER BY sessions.thread_id ASC
      "#, vec![], &op("listThreadSessions", "query"), &op("listThreadSessions", "decodeRows"), session_row)?,
                    query_all(conn, r#"
        SELECT
          links.thread_id AS "threadId",
          links.host,
          links.repository,
          links.number,
          links.url,
          links.source,
          links.linked_at AS "linkedAt",
          links.snapshot_json AS "snapshot",
          links.stack_json AS "stack"
        FROM projection_thread_pull_requests links
        INNER JOIN projection_threads threads
          ON threads.thread_id = links.thread_id
        WHERE threads.deleted_at IS NULL
          AND threads.archived_at IS NOT NULL
        ORDER BY links.thread_id ASC, links.linked_at ASC, links.number ASC
      "#, vec![], &op("listThreadPullRequests", "query"), &op("listThreadPullRequests", "decodeRows"), pull_request_row)?,
                    list_latest_turn_rows(conn, LatestTurnScope::Archived, &op("listLatestTurns", "query"), &op("listLatestTurns", "decodeRows"))?,
                    list_state_rows(conn, &op("listProjectionState", "query"), &op("listProjectionState", "decodeRows"))?,
                ))
            })
            .await?;
        let snapshot = self
            .build_shell_snapshot(projects, threads, sessions, links, latest, states, ShellKind::Archived)
            .await;
        decode(snapshot, "ProjectionSnapshotQuery.getArchivedShellSnapshot:query")
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_shell_snapshot(
        &self,
        projects: Vec<ProjectRow>,
        threads: Vec<ThreadRow>,
        sessions: Vec<SessionRow>,
        links: Vec<PullRequestRow>,
        latest: Vec<LatestTurnRow>,
        states: Vec<StateRow>,
        kind: ShellKind,
    ) -> Value {
        let mut updated_at: Option<String> = None;
        for row in &projects {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        for row in &threads {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        for row in &sessions {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        for row in &latest {
            updated_at = max_iso(updated_at, &row.requested_at);
            if let Some(started) = &row.started_at {
                updated_at = max_iso(updated_at, started);
            }
            if let Some(completed) = &row.completed_at {
                updated_at = max_iso(updated_at, completed);
            }
        }
        for row in &states {
            updated_at = max_iso(updated_at, &row.updated_at);
        }
        let active_project_ids: HashSet<&str> = threads.iter().map(|row| row.project_id.as_str()).collect();
        let identity_projects: Vec<&ProjectRow> = match kind {
            ShellKind::Active => projects.iter().collect(),
            ShellKind::Archived => projects.iter().filter(|row| active_project_ids.contains(row.project_id.as_str())).collect(),
        };
        let identities = self.resolve_identities(&identity_projects, false).await;
        let latest_by_thread: HashMap<&str, Value> = latest.iter().map(|row| (row.thread_id.as_str(), map_latest_turn(row))).collect();
        let sessions_by_thread: HashMap<&str, Value> = sessions.iter().map(|row| (row.thread_id.as_str(), map_session_row(row))).collect();
        let mut links_by_thread = group_pull_requests_by_thread(&links);
        let projects_out: Vec<Value> = projects
            .iter()
            .filter(|row| row.deleted_at.is_none() && (kind == ShellKind::Active || active_project_ids.contains(row.project_id.as_str())))
            .map(|row| map_project_shell_row(row, identities.get(&row.project_id).cloned().unwrap_or(Value::Null)))
            .collect();
        let threads_out: Vec<Value> = threads
            .iter()
            .filter(|row| kind == ShellKind::Archived || row.deleted_at.is_none())
            .map(|row| {
                self.thread_shell(
                    row,
                    links_by_thread.remove(&row.thread_id).unwrap_or_default(),
                    identities.get(&row.project_id),
                    latest_by_thread.get(row.thread_id.as_str()).cloned().unwrap_or(Value::Null),
                    sessions_by_thread.get(row.thread_id.as_str()).cloned().unwrap_or(Value::Null),
                )
            })
            .collect();
        json!({
            "snapshotSequence": compute_snapshot_sequence(&states),
            "projects": projects_out,
            "threads": threads_out,
            "updatedAt": updated_at.unwrap_or_else(|| EPOCH.to_string()),
        })
    }

    /// `listThreadsWithPullRequests()`: active threads with at least one link, in shell order.
    pub async fn list_threads_with_pull_requests(&self) -> Result<Vec<ThreadPullRequests>, DbError> {
        let rows = self
            .read(move |conn| {
                query_all(
                    conn,
                    r#"
        SELECT
          links.thread_id AS "threadId",
          threads.project_id AS "projectId",
          threads.settled_override AS "settledOverride",
          threads.settled_at AS "settledAt",
          links.host,
          links.repository,
          links.number,
          links.url,
          links.source,
          links.linked_at AS "linkedAt",
          links.snapshot_json AS "snapshot",
          links.stack_json AS "stack"
        FROM projection_thread_pull_requests links
        INNER JOIN projection_threads threads
          ON threads.thread_id = links.thread_id
        WHERE threads.deleted_at IS NULL
          AND threads.archived_at IS NULL
        ORDER BY threads.project_id ASC, threads.created_at ASC, threads.thread_id ASC,
          links.linked_at ASC, links.number ASC
      "#,
                    vec![],
                    "ProjectionSnapshotQuery.listThreadsWithPullRequests:query",
                    "ProjectionSnapshotQuery.listThreadsWithPullRequests:decodeRows",
                    pull_request_sync_row,
                )
            })
            .await?;
        let mut threads: Vec<ThreadPullRequests> = Vec::new();
        for row in &rows {
            let link: zc_contracts::ThreadPullRequestLink =
                decode(map_pull_request_row(row), "ProjectionSnapshotQuery.listThreadsWithPullRequests:decodeRows")?;
            match threads.iter_mut().find(|thread| thread.id == row.thread_id) {
                Some(thread) => thread.pull_requests.push(link),
                None => threads.push(ThreadPullRequests {
                    id: row.thread_id.clone(),
                    project_id: row.project_id.clone().unwrap_or_default(),
                    settled_override: row.settled_override.clone(),
                    settled_at: row.settled_at.clone(),
                    pull_requests: vec![link],
                }),
            }
        }
        Ok(threads)
    }

    /// `getDeletedWorktreeThreads()`.
    pub async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>, DbError> {
        self.read(move |conn| {
            query_all(
                conn,
                r#"
      SELECT t.thread_id AS "id", t.project_id AS "projectId", t.branch,
        t.worktree_path AS "worktreePath", p.workspace_root AS "workspaceRoot",
        t.deleted_at AS "deletedAt"
      FROM projection_threads t
      JOIN projection_projects p ON p.project_id = t.project_id
      WHERE t.deleted_at IS NOT NULL AND t.worktree_path IS NOT NULL AND t.branch IS NOT NULL
      ORDER BY t.deleted_at DESC, t.thread_id ASC
    "#,
                vec![],
                "ProjectionSnapshotQuery.getDeletedWorktreeThreads:query",
                "ProjectionSnapshotQuery.getDeletedWorktreeThreads:decodeRows",
                |row| {
                    Ok(DeletedWorktreeThread {
                        id: row.get("id")?,
                        project_id: row.get("projectId")?,
                        branch: row.get("branch")?,
                        worktree_path: row.get("worktreePath")?,
                        workspace_root: row.get("workspaceRoot")?,
                        deleted_at: row.get("deletedAt")?,
                    })
                },
            )
        })
        .await
    }

    /// `searchThreads({query, limit?})`.
    pub async fn search_threads(&self, input: &OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, DbError> {
        let pattern = format!("%{}%", escape_like_pattern(&input.query));
        let limit = input.limit.unwrap_or(50);
        let rows = self
            .read(move |conn| {
                query_all(
                    conn,
                    r#"
        WITH ranked AS (
          SELECT
            threads.thread_id AS thread_id,
            threads.project_id AS project_id,
            CASE messages.role
              WHEN 'user' THEN 'user'
              ELSE 'assistant'
            END AS source,
            messages.text AS match_text,
            messages.created_at AS message_created_at,
            CASE messages.role
              WHEN 'user' THEN 0
              ELSE 1
            END AS match_rank,
            threads.updated_at AS thread_updated_at,
            ROW_NUMBER() OVER (
              PARTITION BY threads.thread_id
              ORDER BY
                CASE messages.role
                  WHEN 'user' THEN 0
                  ELSE 1
                END ASC,
                messages.created_at DESC,
                messages.message_id ASC
            ) AS thread_match_rank
          FROM projection_thread_messages AS messages
          INNER JOIN projection_threads AS threads
            ON threads.thread_id = messages.thread_id
          INNER JOIN projection_projects AS projects
            ON projects.project_id = threads.project_id
          WHERE threads.deleted_at IS NULL
            AND threads.archived_at IS NULL
            AND projects.deleted_at IS NULL
            AND messages.is_streaming = 0
            -- Only these two roles are searchable, and the CASE above depends
            -- on it: reasoning is deliberately excluded so a thinking trace
            -- cannot surface in the command palette, and widening this filter
            -- would label it 'assistant' rather than adding a source.
            AND (
              messages.role = 'user'
              OR (
                messages.role = 'assistant'
                AND messages.message_id IN (
                  SELECT turns.assistant_message_id
                  FROM projection_turns AS turns
                  WHERE turns.assistant_message_id IS NOT NULL
                )
              )
            )
            AND messages.text LIKE ? ESCAPE '!'
        )
        SELECT
          thread_id AS "threadId",
          project_id AS "projectId",
          source,
          match_text AS "matchText",
          message_created_at AS "messageCreatedAt"
        FROM ranked
        WHERE thread_match_rank = 1
        ORDER BY
          match_rank ASC,
          thread_updated_at DESC,
          thread_id ASC
        LIMIT ?
      "#,
                    vec![text(&pattern), int(limit)],
                    "ProjectionSnapshotQuery.searchThreads:query",
                    "ProjectionSnapshotQuery.searchThreads:decodeRows",
                    |row| {
                        Ok((
                            row.get::<_, String>("threadId")?,
                            row.get::<_, String>("projectId")?,
                            row.get::<_, String>("source")?,
                            row.get::<_, String>("matchText")?,
                            row.get::<_, Option<String>>("messageCreatedAt")?,
                        ))
                    },
                )
            })
            .await?;
        let matches: Vec<Value> = rows
            .into_iter()
            .map(|(thread_id, project_id, source, match_text, created_at)| {
                json!({
                    "threadId": thread_id,
                    "projectId": project_id,
                    "source": source,
                    "snippet": build_search_snippet(&match_text, &input.query),
                    "messageCreatedAt": created_at,
                })
            })
            .collect();
        decode(json!({"matches": matches}), "ProjectionSnapshotQuery.searchThreads:decodeRows")
    }

    // -----------------------------------------------------------------------------------------
    // Small reads
    // -----------------------------------------------------------------------------------------

    /// `getSnapshotSequence()`.
    pub async fn get_snapshot_sequence(&self) -> Result<i64, DbError> {
        self.read(|conn| snapshot_sequence_on(conn, "ProjectionSnapshotQuery.getSnapshotSequence"))
            .await
    }

    /// `getCounts()`.
    pub async fn get_counts(&self) -> Result<SnapshotCounts, DbError> {
        self.read(|conn| {
            query_one(
                conn,
                r#"
        SELECT
          (SELECT COUNT(*) FROM projection_projects) AS "projectCount",
          (SELECT COUNT(*) FROM projection_threads) AS "threadCount"
      "#,
                vec![],
                "ProjectionSnapshotQuery.getCounts:query",
                "ProjectionSnapshotQuery.getCounts:decodeRow",
                |row| {
                    Ok(SnapshotCounts {
                        project_count: row.get("projectCount")?,
                        thread_count: row.get("threadCount")?,
                    })
                },
            )
            .map(Option::unwrap_or_default)
        })
        .await
    }

    /// `getEventReplayStats({fromSequenceExclusive, toSequenceInclusive})`.
    pub async fn get_event_replay_stats(&self, from_sequence_exclusive: i64, to_sequence_inclusive: i64) -> Result<EventReplayStats, DbError> {
        self.read(move |conn| {
            query_one(
                conn,
                r#"
        SELECT
          COUNT(*) AS "eventCount",
          COALESCE(SUM(octet_length(payload_json)), 0) AS "payloadBytes"
        FROM orchestration_events
        WHERE sequence > ?
          AND sequence <= ?
      "#,
                vec![int(from_sequence_exclusive), int(to_sequence_inclusive)],
                "ProjectionSnapshotQuery.getEventReplayStats:query",
                "ProjectionSnapshotQuery.getEventReplayStats:decodeRow",
                |row| {
                    Ok(EventReplayStats {
                        event_count: row.get("eventCount")?,
                        payload_bytes: row.get("payloadBytes")?,
                    })
                },
            )
            .map(Option::unwrap_or_default)
        })
        .await
    }

    /// `getActiveProjectByWorkspaceRoot(workspaceRoot)`.
    pub async fn get_active_project_by_workspace_root(&self, workspace_root: &str) -> Result<Option<OrchestrationProject>, DbError> {
        let params = vec![text(workspace_root)];
        let row = self
            .read(move |conn| {
                query_one(
                    conn,
                    &format!("\n        SELECT{PROJECT_COLUMNS}\n        FROM projection_projects\n        WHERE workspace_root = ?\n          AND deleted_at IS NULL\n        ORDER BY created_at ASC, project_id ASC\n        LIMIT 1\n      "),
                    params,
                    "ProjectionSnapshotQuery.getActiveProjectByWorkspaceRoot:query",
                    "ProjectionSnapshotQuery.getActiveProjectByWorkspaceRoot:decodeRow",
                    project_row,
                )
            })
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let identity = self.resolve_identity(&row.workspace_root).await;
        decode(
            map_project_row(&row, identity),
            "ProjectionSnapshotQuery.getActiveProjectByWorkspaceRoot:decodeRow",
        )
        .map(Some)
    }

    /// `getProjectShells(projectIds?)`: active projects, all or the given ones.
    pub async fn get_project_shells(&self, project_ids: Option<Vec<String>>) -> Result<Vec<OrchestrationProjectShell>, DbError> {
        if project_ids.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        let rows = self
            .read(move |conn| {
                list_project_rows(
                    conn,
                    true,
                    project_ids.as_deref(),
                    "ProjectionSnapshotQuery.getProjectShells:query",
                    "ProjectionSnapshotQuery.getProjectShells:decodeRows",
                )
            })
            .await?;
        let refs: Vec<&ProjectRow> = rows.iter().collect();
        let identities = self.resolve_identities(&refs, false).await;
        rows.iter()
            .map(|row| {
                decode(
                    map_project_shell_row(row, identities.get(&row.project_id).cloned().unwrap_or(Value::Null)),
                    "ProjectionSnapshotQuery.getProjectShells:decodeRows",
                )
            })
            .collect()
    }

    /// `getProjectShellById(projectId)`: an active project.
    pub async fn get_project_shell_by_id(&self, project_id: &str) -> Result<Option<OrchestrationProjectShell>, DbError> {
        let Some(row) = self.active_project_row(project_id).await? else {
            return Ok(None);
        };
        let identity = self.resolve_identity(&row.workspace_root).await;
        decode(map_project_shell_row(&row, identity), "ProjectionSnapshotQuery.getProjectShellById:decodeRow").map(Some)
    }

    async fn active_project_row(&self, project_id: &str) -> Result<Option<ProjectRow>, DbError> {
        let params = vec![text(project_id)];
        self.read(move |conn| active_project_row_on(conn, params)).await
    }

    /// `getFirstActiveThreadIdByProjectId(projectId)`.
    pub async fn get_first_active_thread_id_by_project_id(&self, project_id: &str) -> Result<Option<String>, DbError> {
        let params = vec![text(project_id)];
        self.read(move |conn| {
            query_one(
                conn,
                r#"
        SELECT
          thread_id AS "threadId"
        FROM projection_threads
        WHERE project_id = ?
          AND deleted_at IS NULL
          AND archived_at IS NULL
        ORDER BY created_at ASC, thread_id ASC
        LIMIT 1
      "#,
                params,
                "ProjectionSnapshotQuery.getFirstActiveThreadIdByProjectId:query",
                "ProjectionSnapshotQuery.getFirstActiveThreadIdByProjectId:decodeRow",
                |row| Ok(row.get::<_, String>("threadId")?),
            )
        })
        .await
    }

    /// `getImportedAgentSessionSources(projectId)`: completed imports, without history.
    pub async fn get_imported_agent_session_sources(&self, project_id: &str) -> Result<Vec<ImportedAgentSessionSource>, DbError> {
        let params = vec![text(project_id)];
        let rows = self
            .read(move |conn| {
                query_all(
                    conn,
                    r#"
        SELECT
          threads.thread_id AS "threadId",
          runtime.runtime_payload_json AS "runtimePayload"
        FROM projection_threads AS threads
        INNER JOIN projection_projects AS projects
          ON projects.project_id = threads.project_id
        INNER JOIN provider_session_runtime AS runtime
          ON runtime.thread_id = threads.thread_id
        WHERE threads.project_id = ?
          AND threads.deleted_at IS NULL
          AND threads.archived_at IS NULL
          AND projects.deleted_at IS NULL
          AND EXISTS (
            SELECT 1
            FROM projection_thread_messages AS messages
            WHERE messages.thread_id = threads.thread_id
              AND messages.message_id GLOB 'import:*'
          )
        ORDER BY threads.thread_id ASC
      "#,
                    params,
                    "ProjectionSnapshotQuery.getImportedAgentSessionSources:query",
                    "ProjectionSnapshotQuery.getImportedAgentSessionSources:decodeRows",
                    |row| Ok((row.get::<_, String>("threadId")?, row.get::<_, Option<String>>("runtimePayload")?)),
                )
            })
            .await?;
        let mut out = Vec::new();
        for (thread_id, payload) in rows {
            // `decodeImportedTranscriptsPayload`: `fromJsonString({importedTranscripts: Array})`.
            let Some(entries) = payload
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|value| value.get("importedTranscripts").cloned())
                .and_then(|value| value.as_array().cloned())
            else {
                continue;
            };
            for entry in entries {
                let Ok(source) = serde_json::from_value::<zc_contracts::AgentSessionImportSource>(entry.clone()) else {
                    continue;
                };
                let expected = format!(
                    "import:{}:{}",
                    entry.get("providerInstanceId").and_then(Value::as_str).unwrap_or(""),
                    entry.get("providerSessionId").and_then(Value::as_str).unwrap_or("")
                );
                if thread_id != expected {
                    continue;
                }
                out.push(ImportedAgentSessionSource {
                    thread_id: thread_id.clone(),
                    source,
                });
            }
        }
        Ok(out)
    }

    /// `getThreadCheckpointContext(threadId)`.
    pub async fn get_thread_checkpoint_context(&self, thread_id: &str) -> Result<Option<ThreadCheckpointContext>, DbError> {
        let id = thread_id.to_string();
        let result = self
            .read(move |conn| {
                let thread = query_one(
                    conn,
                    r#"
        SELECT
          threads.thread_id AS "threadId",
          threads.project_id AS "projectId",
          projects.workspace_root AS "workspaceRoot",
          threads.worktree_path AS "worktreePath"
        FROM projection_threads AS threads
        INNER JOIN projection_projects AS projects
          ON projects.project_id = threads.project_id
        WHERE threads.thread_id = ?
          AND threads.deleted_at IS NULL
        LIMIT 1
      "#,
                    vec![text(&id)],
                    "ProjectionSnapshotQuery.getThreadCheckpointContext:getThread:query",
                    "ProjectionSnapshotQuery.getThreadCheckpointContext:getThread:decodeRow",
                    |row| {
                        Ok((
                            row.get::<_, String>("threadId")?,
                            row.get::<_, String>("projectId")?,
                            row.get::<_, String>("workspaceRoot")?,
                            row.get::<_, Option<String>>("worktreePath")?,
                        ))
                    },
                )?;
                let Some(thread) = thread else {
                    return Ok(None);
                };
                let checkpoints = list_checkpoint_rows_by_thread(
                    conn,
                    &id,
                    "ProjectionSnapshotQuery.getThreadCheckpointContext:listCheckpoints:query",
                    "ProjectionSnapshotQuery.getThreadCheckpointContext:listCheckpoints:decodeRows",
                )?;
                Ok(Some((thread, checkpoints)))
            })
            .await?;
        let Some(((thread_id, project_id, workspace_root, worktree_path), checkpoints)) = result else {
            return Ok(None);
        };
        let checkpoints = checkpoints
            .iter()
            .map(|row| {
                decode(
                    map_checkpoint_row(row),
                    "ProjectionSnapshotQuery.getThreadCheckpointContext:listCheckpoints:decodeRows",
                )
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(ThreadCheckpointContext {
            thread_id,
            project_id,
            workspace_root,
            worktree_path,
            checkpoints,
        }))
    }

    /// `getFullThreadDiffContext(threadId, toTurnCount)`.
    pub async fn get_full_thread_diff_context(&self, thread_id: &str, to_turn_count: i64) -> Result<Option<FullThreadDiffContext>, DbError> {
        let params = vec![int(to_turn_count), text(thread_id)];
        self.read(move |conn| {
            query_one(
                conn,
                r#"
        SELECT
          threads.thread_id AS "threadId",
          threads.project_id AS "projectId",
          projects.workspace_root AS "workspaceRoot",
          threads.worktree_path AS "worktreePath",
          (
            SELECT MAX(turns.checkpoint_turn_count)
            FROM projection_turns AS turns
            WHERE turns.thread_id = threads.thread_id
              AND turns.checkpoint_turn_count IS NOT NULL
          ) AS "latestCheckpointTurnCount",
          (
            SELECT turns.checkpoint_ref
            FROM projection_turns AS turns
            WHERE turns.thread_id = threads.thread_id
              AND turns.checkpoint_turn_count = ?
            LIMIT 1
          ) AS "toCheckpointRef"
        FROM projection_threads AS threads
        INNER JOIN projection_projects AS projects
          ON projects.project_id = threads.project_id
        WHERE threads.thread_id = ?
          AND threads.deleted_at IS NULL
        LIMIT 1
      "#,
                params,
                "ProjectionSnapshotQuery.getFullThreadDiffContext:query",
                "ProjectionSnapshotQuery.getFullThreadDiffContext:decodeRow",
                |row| {
                    Ok(FullThreadDiffContext {
                        thread_id: row.get("threadId")?,
                        project_id: row.get("projectId")?,
                        workspace_root: row.get("workspaceRoot")?,
                        worktree_path: row.get("worktreePath")?,
                        latest_checkpoint_turn_count: row.get::<_, Option<i64>>("latestCheckpointTurnCount")?.unwrap_or(0),
                        to_checkpoint_ref: row.get("toCheckpointRef")?,
                    })
                },
            )
        })
        .await
    }

    /// `getThreadShellById(threadId)`: an active thread's shell.
    pub async fn get_thread_shell_by_id(&self, thread_id: &str) -> Result<Option<OrchestrationThreadShell>, DbError> {
        let id = thread_id.to_string();
        let op = |name: &str, kind: &str| format!("ProjectionSnapshotQuery.getThreadShellById:{name}:{kind}");
        let rows = self
            .read(move |conn| {
                let thread = active_thread_row_on(conn, &id, &op("getThread", "query"), &op("getThread", "decodeRow"))?;
                let latest = latest_turn_row_by_thread(conn, &id, &op("getLatestTurn", "query"), &op("getLatestTurn", "decodeRow"))?;
                let session = session_row_by_thread(conn, &id, &op("getSession", "query"), &op("getSession", "decodeRow"))?;
                let links = pull_request_rows_by_thread(conn, &id, &op("listPullRequests", "query"), &op("listPullRequests", "decodeRows"))?;
                let Some(thread) = thread else {
                    return Ok(None);
                };
                let project = if links.is_empty() {
                    None
                } else {
                    active_project_row_on(conn, vec![text(&thread.project_id)])?
                };
                Ok(Some((thread, latest, session, links, project)))
            })
            .await?;
        let Some((thread, latest, session, links, project)) = rows else {
            return Ok(None);
        };
        let identity = match &project {
            Some(project) => Some(self.resolve_identity(&project.workspace_root).await),
            None => None,
        };
        let shell = self.thread_shell(
            &thread,
            links.iter().map(map_pull_request_row).collect(),
            identity.as_ref(),
            latest.as_ref().map(map_latest_turn).unwrap_or(Value::Null),
            session.as_ref().map(map_session_row).unwrap_or(Value::Null),
        );
        decode(shell, "ProjectionSnapshotQuery.getThreadShellById:getThread:decodeRow").map(Some)
    }

    /// `getThreadRuntimeContext(threadId)`: `{id, projectId, title, titleState, session}`.
    pub async fn get_thread_runtime_context(&self, thread_id: &str) -> Result<Option<ThreadRuntimeContext>, DbError> {
        let params = vec![text(thread_id)];
        let row = self
            .read(move |conn| {
                query_one(
                    conn,
                    r#"
        SELECT
          threads.thread_id AS id,
          threads.project_id AS "projectId",
          threads.title,
          threads.title_state_json AS "titleState",
          sessions.thread_id AS "threadId",
          sessions.status,
          sessions.provider_name AS "providerName",
          sessions.provider_instance_id AS "providerInstanceId",
          sessions.runtime_mode AS "runtimeMode",
          sessions.active_turn_id AS "activeTurnId",
          sessions.last_error AS "lastError",
          sessions.updated_at AS "updatedAt"
        FROM projection_threads AS threads
        LEFT JOIN projection_thread_sessions AS sessions
          ON sessions.thread_id = threads.thread_id
        WHERE threads.thread_id = ?
          AND threads.deleted_at IS NULL
          AND threads.archived_at IS NULL
        LIMIT 1
      "#,
                    params,
                    "ProjectionSnapshotQuery.getThreadRuntimeContext:query",
                    "ProjectionSnapshotQuery.getThreadRuntimeContext:decodeRow",
                    |row| {
                        let title_state: Option<String> = row.get("titleState")?;
                        let session_thread: Option<String> = row.get("threadId")?;
                        let session = match session_thread {
                            Some(_) => Some(session_row(row)?),
                            None => None,
                        };
                        Ok((
                            row.get::<_, String>("id")?,
                            row.get::<_, String>("projectId")?,
                            row.get::<_, String>("title")?,
                            title_state,
                            session,
                        ))
                    },
                )
            })
            .await?;
        let Some((id, project_id, title, title_state, session)) = row else {
            return Ok(None);
        };
        let op = "ProjectionSnapshotQuery.getThreadRuntimeContext:decodeRow";
        let title_state = title_state
            .map(|text| {
                serde_json::from_str::<Value>(&text)
                    .map_err(|error| DbError::decode(op, error.to_string()))
                    .and_then(|value| decode(value, op))
            })
            .transpose()?;
        let session = session.map(|row| decode(map_session_row(&row), op)).transpose()?;
        Ok(Some(ThreadRuntimeContext {
            id,
            project_id,
            title,
            title_state,
            session,
        }))
    }

    /// `getTurnStartMessage({threadId, messageId})`.
    pub async fn get_turn_start_message(&self, thread_id: &str, message_id: &str) -> Result<Option<TurnStartMessage>, DbError> {
        let params = vec![
            text(thread_id),
            text(message_id),
            text(MESSAGE_TRIM_WHITESPACE),
            text(thread_id),
            text(message_id),
        ];
        let row = self
            .read(move |conn| {
                query_one(
                    conn,
                    r#"
      SELECT
        message_id AS "messageId",
        thread_id AS "threadId",
        turn_id AS "turnId",
        role,
        text,
        attachments_json AS "attachments",
        context_json AS "context",
        is_streaming AS "isStreaming",
        created_at AS "createdAt",
        updated_at AS "updatedAt",
        EXISTS (
          SELECT 1
          FROM projection_thread_messages AS other
          WHERE other.thread_id = ?
            AND other.message_id != ?
            AND other.role = 'user'
            AND (
              LOWER(TRIM(other.text, ?)) != '/compact'
              OR COALESCE(json_array_length(other.attachments_json), 0) > 0
            )
        ) AS "hasOtherUserMessages"
      FROM projection_thread_messages
      WHERE thread_id = ? AND message_id = ?
      LIMIT 1
    "#,
                    params,
                    "ProjectionSnapshotQuery.getTurnStartMessage:query",
                    "ProjectionSnapshotQuery.getTurnStartMessage:decodeRow",
                    |row| Ok((message_row(row)?, row.get::<_, f64>("hasOtherUserMessages")?)),
                )
            })
            .await?;
        let Some((message, has_other)) = row else {
            return Ok(None);
        };
        Ok(Some(TurnStartMessage {
            message: decode(map_message_row(&message), "ProjectionSnapshotQuery.getTurnStartMessage:decodeRow")?,
            has_other_user_messages: has_other == 1.0,
        }))
    }

    // -----------------------------------------------------------------------------------------
    // Thread detail
    // -----------------------------------------------------------------------------------------

    /// `getThreadDetailById(threadId, query?)`: the full active thread, raw activity payloads.
    pub async fn get_thread_detail_by_id(&self, thread_id: &str, query: Option<ThreadDetailQuery>) -> Result<Option<OrchestrationThread>, DbError> {
        let id = thread_id.to_string();
        let read = ActivityRead::Raw(query.unwrap_or_default());
        let rows = self.read(move |conn| thread_detail_rows(conn, &id, None, &read)).await?;
        match rows {
            None => Ok(None),
            Some(rows) => self.build_thread_detail(rows).await.map(Some),
        }
    }

    async fn build_thread_detail(&self, rows: ThreadDetailRows) -> Result<OrchestrationThread, DbError> {
        let identity = match &rows.project {
            Some(project) => Some(self.resolve_identity(&project.workspace_root).await),
            None => None,
        };
        let thread = &rows.thread;
        let mut out = thread_base_fields(
            thread,
            rows.links.iter().map(map_pull_request_row).collect(),
            identity.as_ref(),
            rows.latest_turn.as_ref().map(map_latest_turn).unwrap_or(Value::Null),
        );
        out.insert("deletedAt".into(), Value::Null);
        out.insert("messages".into(), Value::Array(rows.messages.iter().map(map_message_row).collect()));
        out.insert("proposedPlans".into(), Value::Array(rows.plans.iter().map(map_proposed_plan_row).collect()));
        out.insert("activities".into(), Value::Array(rows.activities));
        out.insert("checkpoints".into(), Value::Array(rows.checkpoints.iter().map(map_checkpoint_row).collect()));
        out.insert("session".into(), rows.session.as_ref().map(map_session_row).unwrap_or(Value::Null));
        decode(Value::Object(out), "ProjectionSnapshotQuery.getThreadDetailById:decodeThread")
    }

    /// `getThreadDetailSnapshot(threadId, window?)`: the thread and the snapshot sequence read
    /// in one transaction. With `window.turnLimit` the turn-linked collections are bounded to a
    /// page of recent turns and `page` says how to fetch older ones.
    pub async fn get_thread_detail_snapshot(
        &self,
        thread_id: &str,
        window: Option<OrchestrationThreadDetailWindow>,
    ) -> Result<Option<OrchestrationThreadDetailSnapshot>, DbError> {
        let id = thread_id.to_string();
        let result = self
            .read(move |conn| -> Result<Option<(ThreadDetailRows, i64, Option<Value>)>, DbError> {
                let turn_limit = window.as_ref().and_then(|window| window.turn_limit);
                let Some(turn_limit) = turn_limit else {
                    let Some(rows) = thread_detail_rows(conn, &id, None, &ActivityRead::Client)? else {
                        return Ok(None);
                    };
                    let sequence = snapshot_sequence_on(conn, "ProjectionSnapshotQuery.getSnapshotSequence")?;
                    return Ok(Some((rows, sequence, None)));
                };
                // A malformed or foreign-thread cursor falls back to the first page.
                let cursor = window
                    .as_ref()
                    .and_then(|window| window.before_cursor.as_deref())
                    .and_then(decode_thread_detail_page_cursor)
                    .filter(|cursor| cursor.thread_id == id);
                let before_anchor = cursor
                    .as_ref()
                    .map(|cursor| cursor.before_anchor_at.clone())
                    .unwrap_or_else(|| ANCHOR_UNBOUNDED.to_string());
                let before_key = cursor.as_ref().map(|cursor| cursor.before_turn_id.clone()).unwrap_or_default();
                let window_rows = list_turn_window_rows(
                    conn,
                    &id,
                    &before_anchor,
                    &before_key,
                    turn_limit,
                    THREAD_DETAIL_MAX_RAW_TURNS_PER_PAGE,
                    "ProjectionSnapshotQuery.getThreadDetailSnapshot:listTurnWindow:query",
                    "ProjectionSnapshotQuery.getThreadDetailSnapshot:listTurnWindow:decodeRows",
                )?;
                let oldest = window_rows.first().cloned();
                let has_more = match &oldest {
                    None => false,
                    Some((anchor, key)) => !list_turn_window_rows(
                        conn,
                        &id,
                        anchor,
                        key,
                        1,
                        1,
                        "ProjectionSnapshotQuery.getThreadDetailSnapshot:probeOlder:query",
                        "ProjectionSnapshotQuery.getThreadDetailSnapshot:probeOlder:decodeRows",
                    )?
                    .is_empty(),
                };
                let bounds = if oldest.is_none() && cursor.is_none() {
                    None
                } else {
                    Some(ThreadDetailBounds {
                        min_anchor_at: if has_more {
                            oldest.as_ref().map(|(anchor, _)| anchor.clone()).unwrap_or_default()
                        } else {
                            String::new()
                        },
                        min_turn_key: if has_more {
                            oldest.as_ref().map(|(_, key)| key.clone()).unwrap_or_default()
                        } else {
                            String::new()
                        },
                        before_anchor_at: before_anchor.clone(),
                        before_turn_key: before_key.clone(),
                    })
                };
                // Empty window behind a cursor: nothing older remains.
                let empty_bounds = (oldest.is_none() && cursor.is_some()).then(|| ThreadDetailBounds {
                    min_anchor_at: String::new(),
                    min_turn_key: String::new(),
                    before_anchor_at: String::new(),
                    before_turn_key: String::new(),
                });
                let Some(rows) = thread_detail_rows(conn, &id, empty_bounds.or(bounds).as_ref(), &ActivityRead::Client)? else {
                    return Ok(None);
                };
                let snapshot_sequence = snapshot_sequence_on(conn, "ProjectionSnapshotQuery.getSnapshotSequence")?;
                let thread_sequence = query_one(
                    conn,
                    r#"
        SELECT MAX(sequence) AS "threadSequence"
        FROM orchestration_events
        WHERE aggregate_kind = 'thread'
          AND stream_id = ?
          AND sequence <= ?
          AND event_type IN (
            'thread.message-sent',
            'thread.proposed-plan-upserted',
            'thread.activity-appended',
            'thread.turn-diff-completed',
            'thread.reverted',
            'thread.session-set'
          )
      "#,
                    vec![text(&id), int(snapshot_sequence)],
                    "ProjectionSnapshotQuery.getThreadDetailSnapshot:threadWatermark:query",
                    "ProjectionSnapshotQuery.getThreadDetailSnapshot:threadWatermark:decodeRow",
                    |row| Ok(row.get::<_, Option<i64>>("threadSequence")?),
                )?
                .flatten()
                .unwrap_or(0);
                let before_cursor = match (&oldest, has_more) {
                    (Some((anchor, key)), true) => Value::String(encode_thread_detail_page_cursor(&ThreadDetailPageCursor {
                        thread_id: id.clone(),
                        before_anchor_at: anchor.clone(),
                        before_turn_id: key.clone(),
                    })),
                    _ => Value::Null,
                };
                let page = json!({
                    "beforeCursor": before_cursor,
                    "hasMore": has_more,
                    "snapshotSequence": snapshot_sequence,
                    "threadSequence": thread_sequence,
                });
                Ok(Some((rows, snapshot_sequence, Some(page))))
            })
            .await?;
        let Some((rows, snapshot_sequence, page)) = result else {
            return Ok(None);
        };
        let thread = self.build_thread_detail(rows).await?;
        let mut snapshot = json!({
            "snapshotSequence": snapshot_sequence,
            "thread": serde_json::to_value(&thread).map_err(|error| {
                DbError::decode("ProjectionSnapshotQuery.getThreadDetailSnapshot:transaction", error.to_string())
            })?,
        });
        if let Some(page) = page {
            snapshot["page"] = page;
        }
        decode(snapshot, "ProjectionSnapshotQuery.getThreadDetailSnapshot:transaction").map(Some)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShellKind {
    Active,
    Archived,
}

enum LatestTurnScope {
    All,
    Active(&'static str),
    Archived,
}

fn list_project_rows(conn: &Conn, active_only: bool, project_ids: Option<&[String]>, sql_op: &str, decode_op: &str) -> Result<Vec<ProjectRow>, DbError> {
    let active = if active_only { "deleted_at IS NULL" } else { "1 = 1" };
    let (ids, params) = match project_ids {
        None => ("1 = 1".to_string(), vec![]),
        Some(ids) => (sql_in("project_id", ids.len()), ids.iter().map(|id| text(id)).collect()),
    };
    query_all(
        conn,
        &format!("\n        SELECT{PROJECT_COLUMNS}\n        FROM projection_projects\n        WHERE {active}\n          AND {ids}\n        ORDER BY created_at ASC, project_id ASC\n      "),
        params,
        sql_op,
        decode_op,
        project_row,
    )
}

fn active_project_row_on(conn: &Conn, params: Vec<SqlValue>) -> Result<Option<ProjectRow>, DbError> {
    query_one(
        conn,
        &format!("\n        SELECT{PROJECT_COLUMNS}\n        FROM projection_projects\n        WHERE project_id = ?\n          AND deleted_at IS NULL\n        LIMIT 1\n      "),
        params,
        "ProjectionSnapshotQuery.getProjectShellById:query",
        "ProjectionSnapshotQuery.getProjectShellById:decodeRow",
        project_row,
    )
}

fn list_plan_rows(conn: &Conn, thread_id: Option<&str>, sql_op: &str, decode_op: &str) -> Result<Vec<PlanRow>, DbError> {
    match thread_id {
        None => query_all(
            conn,
            &format!("\n        SELECT{PLAN_COLUMNS}\n        FROM projection_thread_proposed_plans\n        ORDER BY thread_id ASC, created_at ASC, plan_id ASC\n      "),
            vec![],
            sql_op,
            decode_op,
            plan_row,
        ),
        Some(thread_id) => query_all(
            conn,
            &format!("\n        SELECT{PLAN_COLUMNS}\n        FROM projection_thread_proposed_plans\n        WHERE thread_id = ?\n        ORDER BY created_at ASC, plan_id ASC\n      "),
            vec![text(thread_id)],
            sql_op,
            decode_op,
            plan_row,
        ),
    }
}

fn list_session_rows(conn: &Conn, sql_op: &str, decode_op: &str) -> Result<Vec<SessionRow>, DbError> {
    query_all(
        conn,
        r#"
        SELECT
          thread_id AS "threadId",
          status,
          provider_name AS "providerName",
          provider_instance_id AS "providerInstanceId",
          provider_session_id AS "providerSessionId",
          provider_thread_id AS "providerThreadId",
          runtime_mode AS "runtimeMode",
          active_turn_id AS "activeTurnId",
          last_error AS "lastError",
          updated_at AS "updatedAt"
        FROM projection_thread_sessions
        ORDER BY thread_id ASC
      "#,
        vec![],
        sql_op,
        decode_op,
        session_row,
    )
}

fn list_latest_turn_rows(conn: &Conn, scope: LatestTurnScope, sql_op: &str, decode_op: &str) -> Result<Vec<LatestTurnRow>, DbError> {
    let sql = match scope {
        LatestTurnScope::All => format!("\n        SELECT{LATEST_TURN_COLUMNS}\n        FROM projection_threads threads\n        JOIN projection_turns turns\n          ON turns.thread_id = threads.thread_id\n          AND turns.turn_id = threads.latest_turn_id\n        WHERE threads.latest_turn_id IS NOT NULL\n        ORDER BY turns.thread_id ASC\n      "),
        LatestTurnScope::Active(filter) => format!("\n        SELECT{LATEST_TURN_COLUMNS}\n        FROM projection_threads threads\n        JOIN projection_turns turns\n          ON turns.thread_id = threads.thread_id\n          AND turns.turn_id = threads.latest_turn_id\n        WHERE threads.deleted_at IS NULL\n          AND threads.archived_at IS NULL\n          AND threads.latest_turn_id IS NOT NULL\n          {filter}\n        ORDER BY turns.thread_id ASC\n      "),
        LatestTurnScope::Archived => format!("\n        SELECT{LATEST_TURN_COLUMNS}\n        FROM projection_threads threads\n        JOIN projection_turns turns\n          ON turns.thread_id = threads.thread_id\n          AND turns.turn_id = threads.latest_turn_id\n        WHERE threads.deleted_at IS NULL\n          AND threads.archived_at IS NOT NULL\n          AND threads.latest_turn_id IS NOT NULL\n        ORDER BY turns.thread_id ASC\n      "),
    };
    query_all(conn, &sql, vec![], sql_op, decode_op, latest_turn_row)
}

fn list_state_rows(conn: &Conn, sql_op: &str, decode_op: &str) -> Result<Vec<StateRow>, DbError> {
    query_all(
        conn,
        r#"
        SELECT
          projector,
          last_applied_sequence AS "lastAppliedSequence",
          updated_at AS "updatedAt"
        FROM projection_state
      "#,
        vec![],
        sql_op,
        decode_op,
        state_row,
    )
}

fn snapshot_sequence_on(conn: &Conn, operation: &str) -> Result<i64, DbError> {
    let states = list_state_rows(conn, &format!("{operation}:query"), &format!("{operation}:decodeRows"))?;
    Ok(compute_snapshot_sequence(&states))
}

fn active_thread_row_on(conn: &Conn, thread_id: &str, sql_op: &str, decode_op: &str) -> Result<Option<ThreadRow>, DbError> {
    query_one(
        conn,
        &format!("\n        SELECT{THREAD_COLUMNS}\n        FROM projection_threads\n        WHERE thread_id = ?\n          AND deleted_at IS NULL\n          AND archived_at IS NULL\n        LIMIT 1\n      "),
        vec![text(thread_id)],
        sql_op,
        decode_op,
        thread_row,
    )
}

fn latest_turn_row_by_thread(conn: &Conn, thread_id: &str, sql_op: &str, decode_op: &str) -> Result<Option<LatestTurnRow>, DbError> {
    query_one(
        conn,
        &format!("\n        SELECT{LATEST_TURN_COLUMNS}\n        FROM projection_threads threads\n        JOIN projection_turns turns\n          ON turns.thread_id = threads.thread_id\n          AND turns.turn_id = threads.latest_turn_id\n        WHERE threads.thread_id = ?\n          AND threads.deleted_at IS NULL\n          AND threads.archived_at IS NULL\n        LIMIT 1\n      "),
        vec![text(thread_id)],
        sql_op,
        decode_op,
        latest_turn_row,
    )
}

fn session_row_by_thread(conn: &Conn, thread_id: &str, sql_op: &str, decode_op: &str) -> Result<Option<SessionRow>, DbError> {
    query_one(
        conn,
        r#"
        SELECT
          thread_id AS "threadId",
          status,
          provider_name AS "providerName",
          provider_instance_id AS "providerInstanceId",
          runtime_mode AS "runtimeMode",
          active_turn_id AS "activeTurnId",
          last_error AS "lastError",
          updated_at AS "updatedAt"
        FROM projection_thread_sessions
        WHERE thread_id = ?
        LIMIT 1
      "#,
        vec![text(thread_id)],
        sql_op,
        decode_op,
        session_row,
    )
}

fn pull_request_rows_by_thread(conn: &Conn, thread_id: &str, sql_op: &str, decode_op: &str) -> Result<Vec<PullRequestRow>, DbError> {
    query_all(
        conn,
        r#"
        SELECT
          thread_id AS "threadId",
          host,
          repository,
          number,
          url,
          source,
          linked_at AS "linkedAt",
          snapshot_json AS "snapshot",
          stack_json AS "stack"
        FROM projection_thread_pull_requests
        WHERE thread_id = ?
        ORDER BY linked_at ASC, number ASC
      "#,
        vec![text(thread_id)],
        sql_op,
        decode_op,
        pull_request_row,
    )
}

fn list_checkpoint_rows_by_thread(conn: &Conn, thread_id: &str, sql_op: &str, decode_op: &str) -> Result<Vec<CheckpointRow>, DbError> {
    query_all(
        conn,
        &format!("\n        SELECT{CHECKPOINT_COLUMNS}\n        FROM projection_turns\n        WHERE thread_id = ?\n          AND checkpoint_turn_count IS NOT NULL\n        ORDER BY checkpoint_turn_count ASC\n      "),
        vec![text(thread_id)],
        sql_op,
        decode_op,
        checkpoint_row,
    )
}

const ACTIVITY_SELECT: &str = r#"
          activity_id AS "activityId",
          thread_id AS "threadId",
          turn_id AS "turnId",
          tone,
          kind,
          summary,
          payload_json AS "payload",
          sequence,
          created_at AS "createdAt""#;

const ACTIVITY_INNER_SELECT: &str = r#"
            activity_id,
            thread_id,
            turn_id,
            tone,
            kind,
            summary,
            payload_json,
            sequence,
            created_at"#;

/// The turn-window predicate of the windowed reads (10 parameters, see
/// [`ThreadDetailBounds::params`]).
const WINDOW_PREDICATE: &str = r#"
            AND (
              turn_id IN (
                SELECT turn_id FROM projection_turns
                WHERE thread_id = ?
                  AND turn_id IS NOT NULL
                  AND (
                    requested_at > ?
                    OR (
                      requested_at = ?
                      AND turn_id >= ?
                    )
                  )
                  AND (
                    requested_at < ?
                    OR (
                      requested_at = ?
                      AND turn_id < ?
                    )
                  )
              )
              OR (
                turn_id IS NULL
                AND created_at >= ?
                AND created_at < ?
              )
            )"#;

const PINNED_ACTIVITY_IDS_CTE: &str = r#"
pending_approval_requests AS (
          SELECT request_id, thread_id
          FROM projection_pending_approvals
          WHERE thread_id = ?
            AND status = 'pending'
        ),
        pending_approval_activities AS (
          SELECT
            activity.activity_id,
            ROW_NUMBER() OVER (
              PARTITION BY pending.request_id
              ORDER BY activity.created_at DESC, activity.activity_id DESC
            ) AS request_order
          FROM pending_approval_requests AS pending
          CROSS JOIN projection_thread_activities AS activity
          WHERE activity.thread_id = pending.thread_id
            AND activity.kind = 'approval.requested'
            AND json_extract(activity.payload_json, '$.requestId') = pending.request_id
        ),
        pending_user_input_thread AS (
          SELECT thread_id
          FROM projection_threads
          WHERE thread_id = ?
            AND pending_user_input_count > 0
        ),
        user_input_lifecycle AS (
          SELECT
            activity.activity_id,
            activity.kind,
            ROW_NUMBER() OVER (
              PARTITION BY json_extract(activity.payload_json, '$.requestId')
              ORDER BY activity.created_at DESC, activity.activity_id DESC
            ) AS request_order
          FROM pending_user_input_thread AS pending
          CROSS JOIN projection_thread_activities AS activity
          WHERE activity.thread_id = pending.thread_id
            AND (
              activity.kind IN ('user-input.requested', 'user-input.resolved')
              OR (
                activity.kind = 'provider.user-input.respond.failed'
                AND (
                  lower(COALESCE(json_extract(activity.payload_json, '$.detail'), ''))
                    LIKE '%stale pending user-input request%'
                  OR lower(COALESCE(json_extract(activity.payload_json, '$.detail'), ''))
                    LIKE '%unknown pending user-input request%'
                  OR lower(COALESCE(json_extract(activity.payload_json, '$.detail'), ''))
                    LIKE '%unknown pending user input request%'
                  OR lower(COALESCE(json_extract(activity.payload_json, '$.detail'), ''))
                    LIKE '%unknown pending codex user input request%'
                )
              )
            )
            AND json_extract(activity.payload_json, '$.requestId') IS NOT NULL
        ),
        pinned_activity_ids AS (
          SELECT activity_id
          FROM pending_approval_activities
          WHERE request_order = 1
          UNION ALL
          SELECT activity_id
          FROM user_input_lifecycle
          WHERE request_order = 1
            AND kind = 'user-input.requested'
        )
  "#;

/// `left.sequence ?? -1 - …  || createdAt.localeCompare || id.localeCompare`.
fn activity_order(left: (Option<i64>, &str, &str), right: (Option<i64>, &str, &str)) -> std::cmp::Ordering {
    left.0
        .unwrap_or(-1)
        .cmp(&right.0.unwrap_or(-1))
        .then_with(|| js::locale_compare(left.1, right.1))
        .then_with(|| js::locale_compare(left.2, right.2))
}

fn thread_activities(conn: &Conn, thread_id: &str, bounds: Option<&ThreadDetailBounds>, read: &ActivityRead) -> Result<Vec<Value>, DbError> {
    match read {
        ActivityRead::Client => {
            let op = |name: &str, kind: &str| format!("ProjectionSnapshotQuery.getThreadDetailById:{name}:{kind}");
            let ids = match bounds {
                None => query_all(
                    conn,
                    r#"
        SELECT activity_id AS "activityId"
        FROM projection_thread_activities
        WHERE thread_id = ?
        ORDER BY
          sequence DESC,
          created_at DESC,
          activity_id DESC
        LIMIT ?
      "#,
                    vec![text(thread_id), int(THREAD_DETAIL_ACTIVITY_LIMIT)],
                    &op("listActivityIds", "query"),
                    &op("listActivityIds", "decodeRows"),
                    activity_id_row,
                )?,
                Some(bounds) => {
                    let mut params = vec![text(thread_id)];
                    params.extend(bounds.params(thread_id).into_iter().skip(1));
                    params.push(int(THREAD_DETAIL_ACTIVITY_LIMIT));
                    query_all(
                        conn,
                        &format!("\n        SELECT activity_id AS \"activityId\"\n        FROM projection_thread_activities\n        WHERE thread_id = ?{WINDOW_PREDICATE}\n        ORDER BY\n          sequence DESC,\n          created_at DESC,\n          activity_id DESC\n        LIMIT ?\n      "),
                        params,
                        &op("listActivityIds", "query"),
                        &op("listActivityIds", "decodeRows"),
                        activity_id_row,
                    )?
                }
            };
            let pinned = query_all(
                conn,
                &format!("\n        WITH {PINNED_ACTIVITY_IDS_CTE}\n        SELECT activity_id AS \"activityId\"\n        FROM pinned_activity_ids\n      "),
                vec![text(thread_id), text(thread_id)],
                &op("listPinnedActivityIds", "query"),
                &op("listPinnedActivityIds", "decodeRows"),
                activity_id_row,
            )?;
            let mut seen = HashSet::new();
            let unique: Vec<String> = ids.into_iter().chain(pinned).filter(|id| seen.insert(id.clone())).collect();
            let mut activities: Vec<OrchestrationThreadActivity> = Vec::new();
            for batch in unique.chunks(THREAD_DETAIL_ACTIVITY_PAYLOAD_BATCH_SIZE) {
                let rows = query_all(
                    conn,
                    &format!("\n        SELECT{ACTIVITY_SELECT}\n        FROM projection_thread_activities\n        -- The selectors already scoped these globally unique ids to the\n        -- thread inside this transaction. Keep this as a primary-key lookup.\n        WHERE {}\n      ", sql_in("activity_id", batch.len())),
                    batch.iter().map(|id| text(id)).collect(),
                    &op("listActivityPayloadBatch", "query"),
                    &op("listActivityPayloadBatch", "decodeRows"),
                    activity_row,
                )?;
                for row in rows {
                    let activity: OrchestrationThreadActivity = decode(map_thread_activity_row(&row), &op("listActivityPayloadBatch", "decodeRows"))?;
                    activities.push(project_activity_payload(&activity));
                }
            }
            activities.sort_by(|left, right| {
                activity_order(
                    (left.sequence, &left.created_at, left.id.as_str()),
                    (right.sequence, &right.created_at, right.id.as_str()),
                )
            });
            activities
                .iter()
                .map(|activity| {
                    serde_json::to_value(activity).map_err(|error| DbError::decode(op("listActivityPayloadBatch", "decodeRows"), error.to_string()))
                })
                .collect()
        }
        ActivityRead::Raw(query) => {
            let op = |kind: &str| format!("ProjectionSnapshotQuery.getThreadDetailById:listActivities:{kind}");
            let rows: Vec<ActivityRow> = match &query.activity_kinds {
                None => match bounds {
                    None => query_all(
                        conn,
                        &format!("\n        SELECT{ACTIVITY_SELECT}\n        FROM (\n          SELECT{ACTIVITY_INNER_SELECT}\n          FROM projection_thread_activities\n          WHERE thread_id = ?\n          ORDER BY\n            sequence DESC,\n            created_at DESC,\n            activity_id DESC\n          LIMIT ?\n        ) AS recent_activities\n        ORDER BY\n          sequence ASC,\n          created_at ASC,\n          activity_id ASC\n      "),
                        vec![text(thread_id), int(THREAD_DETAIL_ACTIVITY_LIMIT)],
                        &op("query"),
                        &op("decodeRows"),
                        activity_row,
                    )?,
                    Some(bounds) => {
                        let mut params = vec![text(thread_id)];
                        params.extend(bounds.params(thread_id).into_iter().skip(1));
                        params.push(int(THREAD_DETAIL_ACTIVITY_LIMIT));
                        query_all(
                            conn,
                            &format!("\n        SELECT{ACTIVITY_SELECT}\n        FROM (\n          SELECT{ACTIVITY_INNER_SELECT}\n          FROM projection_thread_activities\n          WHERE thread_id = ?{WINDOW_PREDICATE}\n          ORDER BY\n            sequence DESC,\n            created_at DESC,\n            activity_id DESC\n          LIMIT ?\n        ) AS recent_activities\n        ORDER BY\n          sequence ASC,\n          created_at ASC,\n          activity_id ASC\n      "),
                            params,
                            &op("query"),
                            &op("decodeRows"),
                            activity_row,
                        )?
                    }
                },
                Some(kinds) if kinds.is_empty() => Vec::new(),
                Some(kinds) => {
                    let mut params = vec![text(thread_id)];
                    params.extend(kinds.iter().map(|kind| text(kind)));
                    params.push(int(THREAD_DETAIL_ACTIVITY_LIMIT));
                    query_all(
                        conn,
                        &format!("\n        SELECT{ACTIVITY_SELECT}\n        FROM (\n          SELECT{ACTIVITY_INNER_SELECT}\n          FROM projection_thread_activities\n          WHERE thread_id = ?\n            AND {}\n          ORDER BY\n            sequence DESC,\n            created_at DESC,\n            activity_id DESC\n          LIMIT ?\n        ) AS recent_activities\n        ORDER BY\n          sequence ASC,\n          created_at ASC,\n          activity_id ASC\n      ", sql_in("kind", kinds.len())),
                        params,
                        &op("query"),
                        &op("decodeRows"),
                        activity_row,
                    )?
                }
            };
            let pinned = if query.activity_kinds.is_none() {
                query_all(
                    conn,
                    &format!("\n        WITH {PINNED_ACTIVITY_IDS_CTE}\n        SELECT\n          activity.activity_id AS \"activityId\",\n          activity.thread_id AS \"threadId\",\n          activity.turn_id AS \"turnId\",\n          activity.tone,\n          activity.kind,\n          activity.summary,\n          activity.payload_json AS \"payload\",\n          activity.sequence,\n          activity.created_at AS \"createdAt\"\n        FROM pinned_activity_ids AS pinned\n        INNER JOIN projection_thread_activities AS activity\n          ON activity.activity_id = pinned.activity_id\n        ORDER BY activity.created_at ASC, activity.activity_id ASC\n      "),
                    vec![text(thread_id), text(thread_id)],
                    "ProjectionSnapshotQuery.getThreadDetailById:listPinnedActivities:query",
                    "ProjectionSnapshotQuery.getThreadDetailById:listPinnedActivities:decodeRows",
                    activity_row,
                )?
            } else {
                Vec::new()
            };
            // `new Map(rows.map(row => [id, row]))`: first position, last value.
            let mut merged: Vec<ActivityRow> = Vec::new();
            for row in rows.into_iter().chain(pinned) {
                match merged.iter_mut().find(|existing| existing.activity_id == row.activity_id) {
                    Some(existing) => *existing = row,
                    None => merged.push(row),
                }
            }
            merged.sort_by(|left, right| {
                activity_order(
                    (left.sequence, &left.created_at, &left.activity_id),
                    (right.sequence, &right.created_at, &right.activity_id),
                )
            });
            Ok(merged.iter().map(map_thread_activity_row).collect())
        }
    }
}

/// `getThreadDetailByIdBounded`: every row of one active thread detail read.
fn thread_detail_rows(conn: &Conn, thread_id: &str, bounds: Option<&ThreadDetailBounds>, read: &ActivityRead) -> Result<Option<ThreadDetailRows>, DbError> {
    let op = |name: &str, kind: &str| format!("ProjectionSnapshotQuery.getThreadDetailById:{name}:{kind}");
    let thread = active_thread_row_on(conn, thread_id, &op("getThread", "query"), &op("getThread", "decodeRow"))?;
    let messages = match bounds {
        None => query_all(
            conn,
            &format!("\n        SELECT{MESSAGE_COLUMNS}\n        FROM projection_thread_messages\n        WHERE thread_id = ?\n        ORDER BY created_at ASC, message_id ASC\n      "),
            vec![text(thread_id)],
            &op("listMessages", "query"),
            &op("listMessages", "decodeRows"),
            message_row,
        )?,
        Some(bounds) => {
            let mut params = vec![text(thread_id)];
            params.extend(bounds.params(thread_id).into_iter().skip(1));
            // The message window has its predicate inline (no LIMIT).
            query_all(
                conn,
                &format!("\n        SELECT{MESSAGE_COLUMNS}\n        FROM projection_thread_messages\n        WHERE thread_id = ?\n          AND (\n            turn_id IN (\n              SELECT turn_id FROM projection_turns\n              WHERE thread_id = ?\n                AND turn_id IS NOT NULL\n                AND (\n                  requested_at > ?\n                  OR (\n                    requested_at = ?\n                    AND turn_id >= ?\n                  )\n                )\n                AND (\n                  requested_at < ?\n                  OR (\n                    requested_at = ?\n                    AND turn_id < ?\n                  )\n                )\n            )\n            OR (\n              turn_id IS NULL\n              AND created_at >= ?\n              AND created_at < ?\n            )\n          )\n        ORDER BY created_at ASC, message_id ASC\n      "),
                params,
                &op("listMessages", "query"),
                &op("listMessages", "decodeRows"),
                message_row,
            )?
        }
    };
    let plans = list_plan_rows(conn, Some(thread_id), &op("listPlans", "query"), &op("listPlans", "decodeRows"))?;
    let links = pull_request_rows_by_thread(conn, thread_id, &op("listPullRequests", "query"), &op("listPullRequests", "decodeRows"))?;
    let activities = thread_activities(conn, thread_id, bounds, read)?;
    let checkpoints = list_checkpoint_rows_by_thread(conn, thread_id, &op("listCheckpoints", "query"), &op("listCheckpoints", "decodeRows"))?;
    let latest_turn = latest_turn_row_by_thread(conn, thread_id, &op("getLatestTurn", "query"), &op("getLatestTurn", "decodeRow"))?;
    let session = session_row_by_thread(conn, thread_id, &op("getSession", "query"), &op("getSession", "decodeRow"))?;
    let Some(thread) = thread else {
        return Ok(None);
    };
    let project = if links.is_empty() {
        None
    } else {
        active_project_row_on(conn, vec![text(&thread.project_id)])?
    };
    Ok(Some(ThreadDetailRows {
        thread,
        messages,
        plans,
        links,
        activities,
        checkpoints,
        latest_turn,
        session,
        project,
    }))
}

/// `listTurnWindowRows`: `(anchorAt, turnKey)` of the page, oldest first.
#[allow(clippy::too_many_arguments)]
fn list_turn_window_rows(
    conn: &Conn,
    thread_id: &str,
    before_anchor_at: &str,
    before_turn_key: &str,
    user_turn_limit: i64,
    max_raw_turns: i64,
    sql_op: &str,
    decode_op: &str,
) -> Result<Vec<(String, String)>, DbError> {
    query_all(
        conn,
        r#"
        WITH candidates AS (
          SELECT
            turns.requested_at AS anchor_at,
            COALESCE(turns.turn_id, '') AS turn_key,
            turns.pending_message_id
          FROM projection_turns AS turns
          WHERE turns.thread_id = ?
            AND (
              turns.requested_at < ?
              OR (
                turns.requested_at = ?
                AND COALESCE(turns.turn_id, '') < ?
              )
            )
          ORDER BY turns.requested_at DESC, turns.turn_id DESC
          LIMIT ?
        ),
        walked AS (
          SELECT
            candidates.anchor_at,
            candidates.turn_key,
            CASE WHEN messages.role = 'user' THEN 1 ELSE 0 END AS is_user_turn,
            SUM(CASE WHEN messages.role = 'user' THEN 1 ELSE 0 END) OVER (
              ORDER BY candidates.anchor_at DESC, candidates.turn_key DESC
            ) AS user_turns_seen
          FROM candidates
          LEFT JOIN projection_thread_messages AS messages
            ON messages.message_id = candidates.pending_message_id
        )
        SELECT
          anchor_at AS "anchorAt",
          turn_key AS "turnKey"
        FROM walked
        WHERE user_turns_seen < ?
          OR (user_turns_seen = ? AND is_user_turn = 1)
        ORDER BY anchor_at ASC, turn_key ASC
      "#,
        vec![
            text(thread_id),
            text(before_anchor_at),
            text(before_anchor_at),
            text(before_turn_key),
            int(max_raw_turns),
            int(user_turn_limit),
            int(user_turn_limit),
        ],
        sql_op,
        decode_op,
        |row| Ok((row.get::<_, String>("anchorAt")?, row.get::<_, String>("turnKey")?)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippets_center_on_the_match() {
        assert_eq!(build_search_snippet("  a \n b  ", "a"), "a b");
        let long = format!("{}needle{}", "x ".repeat(200), " y".repeat(200));
        let snippet = build_search_snippet(&long, "NEEDLE");
        assert!(snippet.starts_with('…') && snippet.ends_with('…'));
        assert!(snippet.contains("needle"));
        assert_eq!(js::utf16_len(&snippet), 238);
    }

    #[test]
    fn snapshot_sequence_is_the_minimum_required_cursor() {
        let state = |projector: &str, sequence: i64| StateRow {
            projector: projector.into(),
            last_applied_sequence: sequence,
            updated_at: EPOCH.into(),
        };
        assert_eq!(compute_snapshot_sequence(&[]), 0);
        let mut states: Vec<StateRow> = REQUIRED_SNAPSHOT_PROJECTORS.iter().map(|name| state(name, 10)).collect();
        states.push(state(projector_names::PENDING_APPROVALS, 2));
        assert_eq!(compute_snapshot_sequence(&states), 10);
        states[1].last_applied_sequence = 7;
        assert_eq!(compute_snapshot_sequence(&states), 7);
        states.remove(0);
        assert_eq!(compute_snapshot_sequence(&states), 0);
        assert_eq!(escape_like_pattern("50%_off!"), "50!%!_off!!");
    }
}
