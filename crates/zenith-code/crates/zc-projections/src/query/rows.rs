//! The SQL of `ProjectionSnapshotQuery.ts`, verbatim (template parameters become `?`
//! placeholders in the same order), and the rows it reads. JSON columns are parsed into
//! `serde_json::Value`; the typed decode happens when the result is built, as the TS row and
//! result schemas do between them.

use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, Row};
use serde_json::Value;
use zc_db::{Conn, DbError};

/// A row mapping failure: SQL (column access) or decode (a JSON column, a literal).
pub(crate) enum RowError {
    Sql(rusqlite::Error),
    Decode(String),
}

impl From<rusqlite::Error> for RowError {
    fn from(error: rusqlite::Error) -> Self {
        RowError::Sql(error)
    }
}

pub(crate) type RowResult<T> = Result<T, RowError>;

/// Runs `sql` with `params` and maps every row; errors carry the TS operation names.
pub(crate) fn query_all<T>(
    conn: &Conn,
    sql: &str,
    params: Vec<SqlValue>,
    sql_op: &str,
    decode_op: &str,
    map: impl Fn(&Row<'_>) -> RowResult<T>,
) -> Result<Vec<T>, DbError> {
    let mut statement = conn.prepare(sql).map_err(|error| DbError::sql(sql_op, error))?;
    let mut rows = statement.query(params_from_iter(params.iter())).map_err(|error| DbError::sql(sql_op, error))?;
    let mut out = Vec::new();
    loop {
        match rows.next() {
            Ok(Some(row)) => match map(row) {
                Ok(value) => out.push(value),
                Err(RowError::Sql(error)) => return Err(DbError::decode(decode_op, error.to_string())),
                Err(RowError::Decode(issue)) => return Err(DbError::decode(decode_op, issue)),
            },
            Ok(None) => return Ok(out),
            Err(error) => return Err(DbError::sql(sql_op, error)),
        }
    }
}

/// [`query_all`], first row only (`findOneOption`).
pub(crate) fn query_one<T>(
    conn: &Conn,
    sql: &str,
    params: Vec<SqlValue>,
    sql_op: &str,
    decode_op: &str,
    map: impl Fn(&Row<'_>) -> RowResult<T>,
) -> Result<Option<T>, DbError> {
    Ok(query_all(conn, sql, params, sql_op, decode_op, map)?.into_iter().next())
}

pub(crate) fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_string())
}

pub(crate) fn int(value: i64) -> SqlValue {
    SqlValue::Integer(value)
}

fn json(field: &str, text: Option<String>) -> RowResult<Option<Value>> {
    text.map(|text| serde_json::from_str(&text).map_err(|_| RowError::Decode(format!("{field}: Encoding(InvalidValue)"))))
        .transpose()
}

fn json_required(row: &Row<'_>, field: &str) -> RowResult<Value> {
    let text: Option<String> = row.get(field)?;
    json(field, text)?.ok_or_else(|| RowError::Decode(format!("{field}: MissingKey")))
}

fn json_opt(row: &Row<'_>, field: &str) -> RowResult<Option<Value>> {
    let text: Option<String> = row.get(field)?;
    json(field, text)
}

fn literal(field: &str, value: String, allowed: &[&str]) -> RowResult<String> {
    if allowed.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(RowError::Decode(format!("{field}: InvalidValue")))
    }
}

fn non_negative(field: &str, value: i64) -> RowResult<i64> {
    if value >= 0 {
        Ok(value)
    } else {
        Err(RowError::Decode(format!("{field}: Filter(InvalidValue)")))
    }
}

/// `sql.in(column, values)`: `"column" IN (?,…)`, or `1=0` for an empty list.
pub(crate) fn sql_in(column: &str, count: usize) -> String {
    if count == 0 {
        "1=0".to_string()
    } else {
        format!("\"{column}\" IN ({})", vec!["?"; count].join(","))
    }
}

// ---------------------------------------------------------------------------------------------
// projects
// ---------------------------------------------------------------------------------------------

pub(crate) const PROJECT_COLUMNS: &str = r#"
          project_id AS "projectId",
          title,
          workspace_root AS "workspaceRoot",
          default_model_selection_json AS "defaultModelSelection",
          default_thread_env_mode AS "defaultThreadEnvMode",
          auto_pull AS "autoPull",
          favicon_path AS "faviconPath",
          project_icon_json AS "projectIcon",
          scripts_json AS "scripts",
          created_at AS "createdAt",
          updated_at AS "updatedAt",
          deleted_at AS "deletedAt""#;

#[derive(Debug, Clone)]
pub(crate) struct ProjectRow {
    pub project_id: String,
    pub title: String,
    pub workspace_root: String,
    pub default_model_selection: Option<Value>,
    pub default_thread_env_mode: Option<String>,
    pub auto_pull: f64,
    pub favicon_path: Option<String>,
    pub project_icon: Option<Value>,
    pub scripts: Value,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

pub(crate) fn project_row(row: &Row<'_>) -> RowResult<ProjectRow> {
    Ok(ProjectRow {
        project_id: row.get("projectId")?,
        title: row.get("title")?,
        workspace_root: row.get("workspaceRoot")?,
        default_model_selection: json_opt(row, "defaultModelSelection")?,
        default_thread_env_mode: row.get("defaultThreadEnvMode")?,
        auto_pull: row.get::<_, f64>("autoPull")?,
        favicon_path: row.get("faviconPath")?,
        project_icon: json_opt(row, "projectIcon")?,
        scripts: json_required(row, "scripts")?,
        created_at: row.get("createdAt")?,
        updated_at: row.get("updatedAt")?,
        deleted_at: row.get("deletedAt")?,
    })
}

// ---------------------------------------------------------------------------------------------
// threads
// ---------------------------------------------------------------------------------------------

pub(crate) const THREAD_COLUMNS: &str = r#"
          thread_id AS "threadId",
          project_id AS "projectId",
          title,
          title_state_json AS "titleState",
          model_selection_json AS "modelSelection",
          runtime_mode AS "runtimeMode",
          interaction_mode AS "interactionMode",
          branch,
          worktree_path AS "worktreePath",
          linked_pull_request_json AS "linkedPullRequest",
          branch_pull_request_json AS "branchPullRequest",
          latest_turn_id AS "latestTurnId",
          created_at AS "createdAt",
          updated_at AS "updatedAt",
          archived_at AS "archivedAt",
          settled_override AS "settledOverride",
          settled_at AS "settledAt",
          unsettled_at AS "unsettledAt",
          snoozed_until AS "snoozedUntil",
          snoozed_at AS "snoozedAt",
          pinned_at AS "pinnedAt",
          pin_order_key AS "pinOrderKey",
          active_order_key AS "activeOrderKey",
          auto_settle_disabled_at AS "autoSettleDisabledAt",
          title_regeneration_request_id AS "titleRegenerationRequestId",
          title_regeneration_started_at AS "titleRegenerationStartedAt",
          latest_user_message_at AS "latestUserMessageAt",
          pending_approval_count AS "pendingApprovalCount",
          pending_user_input_count AS "pendingUserInputCount",
          has_actionable_proposed_plan AS "hasActionableProposedPlan",
          deleted_at AS "deletedAt""#;

#[derive(Debug, Clone)]
pub(crate) struct ThreadRow {
    pub thread_id: String,
    pub project_id: String,
    pub title: String,
    pub title_state: Option<Value>,
    pub model_selection: Value,
    pub runtime_mode: String,
    pub interaction_mode: String,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    pub branch_pull_request: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
    pub settled_override: Option<String>,
    pub settled_at: Option<String>,
    pub unsettled_at: Option<String>,
    pub snoozed_until: Option<String>,
    pub snoozed_at: Option<String>,
    pub pinned_at: Option<String>,
    pub pin_order_key: Option<String>,
    pub active_order_key: Option<String>,
    pub auto_settle_disabled_at: Option<String>,
    pub title_regeneration_request_id: Option<String>,
    pub title_regeneration_started_at: Option<String>,
    pub latest_user_message_at: Option<String>,
    pub pending_approval_count: i64,
    pub pending_user_input_count: i64,
    pub has_actionable_proposed_plan: i64,
    pub deleted_at: Option<String>,
}

const RUNTIME_MODES: &[&str] = &["approval-required", "auto-accept-edits", "auto", "full-access"];
const INTERACTION_MODES: &[&str] = &["default", "plan"];

pub(crate) fn thread_row(row: &Row<'_>) -> RowResult<ThreadRow> {
    // Decoded but unused, like the TS row schema (the legacy column is superseded by links).
    let _linked: Option<Value> = json_opt(row, "linkedPullRequest")?;
    let settled_override: Option<String> = row.get("settledOverride")?;
    Ok(ThreadRow {
        thread_id: row.get("threadId")?,
        project_id: row.get("projectId")?,
        title: row.get("title")?,
        title_state: json_opt(row, "titleState")?,
        model_selection: json_required(row, "modelSelection")?,
        runtime_mode: literal("runtimeMode", row.get("runtimeMode")?, RUNTIME_MODES)?,
        interaction_mode: literal("interactionMode", row.get("interactionMode")?, INTERACTION_MODES)?,
        branch: row.get("branch")?,
        worktree_path: row.get("worktreePath")?,
        branch_pull_request: json_opt(row, "branchPullRequest")?,
        created_at: row.get("createdAt")?,
        updated_at: row.get("updatedAt")?,
        archived_at: row.get("archivedAt")?,
        settled_override: settled_override
            .map(|value| literal("settledOverride", value, &["settled", "active"]))
            .transpose()?,
        settled_at: row.get("settledAt")?,
        unsettled_at: row.get("unsettledAt")?,
        snoozed_until: row.get("snoozedUntil")?,
        snoozed_at: row.get("snoozedAt")?,
        pinned_at: row.get("pinnedAt")?,
        pin_order_key: row.get("pinOrderKey")?,
        active_order_key: row.get("activeOrderKey")?,
        auto_settle_disabled_at: row.get("autoSettleDisabledAt")?,
        title_regeneration_request_id: row.get("titleRegenerationRequestId")?,
        title_regeneration_started_at: row.get("titleRegenerationStartedAt")?,
        latest_user_message_at: row.get("latestUserMessageAt")?,
        pending_approval_count: non_negative("pendingApprovalCount", row.get("pendingApprovalCount")?)?,
        pending_user_input_count: non_negative("pendingUserInputCount", row.get("pendingUserInputCount")?)?,
        has_actionable_proposed_plan: non_negative("hasActionableProposedPlan", row.get("hasActionableProposedPlan")?)?,
        deleted_at: row.get("deletedAt")?,
    })
}

// ---------------------------------------------------------------------------------------------
// messages, plans, links, activities, sessions, checkpoints, latest turns, state
// ---------------------------------------------------------------------------------------------

pub(crate) const MESSAGE_COLUMNS: &str = r#"
          message_id AS "messageId",
          thread_id AS "threadId",
          turn_id AS "turnId",
          role,
          text,
          attachments_json AS "attachments",
          context_json AS "context",
          is_streaming AS "isStreaming",
          created_at AS "createdAt",
          updated_at AS "updatedAt""#;

#[derive(Debug, Clone)]
pub(crate) struct MessageRow {
    pub message_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub role: String,
    pub text: String,
    pub attachments: Option<Value>,
    pub context: Option<Value>,
    pub is_streaming: f64,
    pub created_at: String,
    pub updated_at: String,
}

pub(crate) fn message_row(row: &Row<'_>) -> RowResult<MessageRow> {
    Ok(MessageRow {
        message_id: row.get("messageId")?,
        thread_id: row.get("threadId")?,
        turn_id: row.get("turnId")?,
        role: literal("role", row.get("role")?, &["user", "assistant", "system", "reasoning"])?,
        text: row.get("text")?,
        attachments: json_opt(row, "attachments")?,
        context: json_opt(row, "context")?,
        is_streaming: row.get::<_, f64>("isStreaming")?,
        created_at: row.get("createdAt")?,
        updated_at: row.get("updatedAt")?,
    })
}

pub(crate) const PLAN_COLUMNS: &str = r#"
          plan_id AS "planId",
          thread_id AS "threadId",
          turn_id AS "turnId",
          plan_markdown AS "planMarkdown",
          implemented_at AS "implementedAt",
          implementation_thread_id AS "implementationThreadId",
          created_at AS "createdAt",
          updated_at AS "updatedAt""#;

#[derive(Debug, Clone)]
pub(crate) struct PlanRow {
    pub plan_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub plan_markdown: String,
    pub implemented_at: Option<String>,
    pub implementation_thread_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub(crate) fn plan_row(row: &Row<'_>) -> RowResult<PlanRow> {
    let markdown: String = row.get("planMarkdown")?;
    let markdown = crate::js::trim(&markdown).to_string();
    if markdown.is_empty() {
        return Err(RowError::Decode("planMarkdown: Filter(InvalidValue)".into()));
    }
    Ok(PlanRow {
        plan_id: row.get("planId")?,
        thread_id: row.get("threadId")?,
        turn_id: row.get("turnId")?,
        plan_markdown: markdown,
        implemented_at: row.get("implementedAt")?,
        implementation_thread_id: row.get("implementationThreadId")?,
        created_at: row.get("createdAt")?,
        updated_at: row.get("updatedAt")?,
    })
}

#[derive(Debug, Clone)]
pub(crate) struct PullRequestRow {
    pub thread_id: String,
    pub host: String,
    pub repository: String,
    pub number: i64,
    pub url: String,
    pub source: String,
    pub linked_at: String,
    pub snapshot: Option<Value>,
    pub stack: Option<Value>,
    /// Only `listActiveThreadPullRequestSyncRows` reads these three.
    pub project_id: Option<String>,
    pub settled_override: Option<String>,
    pub settled_at: Option<String>,
}

pub(crate) fn pull_request_row(row: &Row<'_>) -> RowResult<PullRequestRow> {
    Ok(PullRequestRow {
        thread_id: row.get("threadId")?,
        host: row.get("host")?,
        repository: row.get("repository")?,
        number: row.get("number")?,
        url: row.get("url")?,
        source: literal("source", row.get("source")?, &["manual", "created", "agent", "stack", "stack-dismissed"])?,
        linked_at: row.get("linkedAt")?,
        snapshot: json_opt(row, "snapshot")?,
        stack: json_opt(row, "stack")?,
        project_id: None,
        settled_override: None,
        settled_at: None,
    })
}

pub(crate) fn pull_request_sync_row(row: &Row<'_>) -> RowResult<PullRequestRow> {
    let mut out = pull_request_row(row)?;
    out.project_id = Some(row.get("projectId")?);
    let settled_override: Option<String> = row.get("settledOverride")?;
    out.settled_override = settled_override
        .map(|value| literal("settledOverride", value, &["settled", "active"]))
        .transpose()?;
    out.settled_at = row.get("settledAt")?;
    Ok(out)
}

#[derive(Debug, Clone)]
pub(crate) struct ActivityRow {
    pub activity_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub tone: String,
    pub kind: String,
    pub summary: String,
    pub payload: Value,
    pub sequence: Option<i64>,
    pub created_at: String,
}

pub(crate) fn activity_row(row: &Row<'_>) -> RowResult<ActivityRow> {
    let sequence: Option<i64> = row.get("sequence")?;
    Ok(ActivityRow {
        activity_id: row.get("activityId")?,
        thread_id: row.get("threadId")?,
        turn_id: row.get("turnId")?,
        tone: literal("tone", row.get("tone")?, &["info", "tool", "approval", "error"])?,
        kind: row.get("kind")?,
        summary: row.get("summary")?,
        payload: json_required(row, "payload")?,
        sequence: sequence.map(|value| non_negative("sequence", value)).transpose()?,
        created_at: row.get("createdAt")?,
    })
}

pub(crate) fn activity_id_row(row: &Row<'_>) -> RowResult<String> {
    Ok(row.get("activityId")?)
}

#[derive(Debug, Clone)]
pub(crate) struct SessionRow {
    pub thread_id: String,
    pub status: String,
    pub provider_name: Option<String>,
    pub provider_instance_id: Option<String>,
    pub runtime_mode: String,
    pub active_turn_id: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

const SESSION_STATUSES: &[&str] = &["idle", "starting", "running", "ready", "interrupted", "stopped", "error"];

pub(crate) fn session_row(row: &Row<'_>) -> RowResult<SessionRow> {
    Ok(SessionRow {
        thread_id: row.get("threadId")?,
        status: literal("status", row.get("status")?, SESSION_STATUSES)?,
        provider_name: row.get("providerName")?,
        provider_instance_id: row.get("providerInstanceId")?,
        runtime_mode: literal("runtimeMode", row.get("runtimeMode")?, RUNTIME_MODES)?,
        active_turn_id: row.get("activeTurnId")?,
        last_error: row.get("lastError")?,
        updated_at: row.get("updatedAt")?,
    })
}

#[derive(Debug, Clone)]
pub(crate) struct CheckpointRow {
    pub thread_id: String,
    pub turn_id: String,
    pub checkpoint_turn_count: i64,
    pub checkpoint_ref: String,
    pub status: String,
    pub files: Value,
    pub assistant_message_id: Option<String>,
    pub completed_at: String,
}

pub(crate) fn checkpoint_row(row: &Row<'_>) -> RowResult<CheckpointRow> {
    let turn_id: Option<String> = row.get("turnId")?;
    let checkpoint_ref: Option<String> = row.get("checkpointRef")?;
    let status: Option<String> = row.get("status")?;
    let completed_at: Option<String> = row.get("completedAt")?;
    let missing = |field: &str| RowError::Decode(format!("{field}: Expected string, got null"));
    Ok(CheckpointRow {
        thread_id: row.get("threadId")?,
        turn_id: turn_id.ok_or_else(|| missing("turnId"))?,
        checkpoint_turn_count: non_negative("checkpointTurnCount", row.get("checkpointTurnCount")?)?,
        checkpoint_ref: checkpoint_ref.ok_or_else(|| missing("checkpointRef"))?,
        status: literal("status", status.ok_or_else(|| missing("status"))?, &["ready", "missing", "error"])?,
        files: json_required(row, "files")?,
        assistant_message_id: row.get("assistantMessageId")?,
        completed_at: completed_at.ok_or_else(|| missing("completedAt"))?,
    })
}

pub(crate) const CHECKPOINT_COLUMNS: &str = r#"
          thread_id AS "threadId",
          turn_id AS "turnId",
          checkpoint_turn_count AS "checkpointTurnCount",
          checkpoint_ref AS "checkpointRef",
          checkpoint_status AS "status",
          checkpoint_files_json AS "files",
          assistant_message_id AS "assistantMessageId",
          completed_at AS "completedAt""#;

#[derive(Debug, Clone)]
pub(crate) struct LatestTurnRow {
    pub thread_id: String,
    pub turn_id: String,
    pub state: String,
    pub requested_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub assistant_message_id: Option<String>,
    pub source_proposed_plan_thread_id: Option<String>,
    pub source_proposed_plan_id: Option<String>,
}

pub(crate) const LATEST_TURN_COLUMNS: &str = r#"
          turns.thread_id AS "threadId",
          turns.turn_id AS "turnId",
          turns.state,
          turns.requested_at AS "requestedAt",
          turns.started_at AS "startedAt",
          turns.completed_at AS "completedAt",
          turns.assistant_message_id AS "assistantMessageId",
          turns.source_proposed_plan_thread_id AS "sourceProposedPlanThreadId",
          turns.source_proposed_plan_id AS "sourceProposedPlanId""#;

pub(crate) fn latest_turn_row(row: &Row<'_>) -> RowResult<LatestTurnRow> {
    Ok(LatestTurnRow {
        thread_id: row.get("threadId")?,
        turn_id: row.get("turnId")?,
        state: row.get("state")?,
        requested_at: row.get("requestedAt")?,
        started_at: row.get("startedAt")?,
        completed_at: row.get("completedAt")?,
        assistant_message_id: row.get("assistantMessageId")?,
        source_proposed_plan_thread_id: row.get("sourceProposedPlanThreadId")?,
        source_proposed_plan_id: row.get("sourceProposedPlanId")?,
    })
}

#[derive(Debug, Clone)]
pub(crate) struct StateRow {
    pub projector: String,
    pub last_applied_sequence: i64,
    pub updated_at: String,
}

pub(crate) fn state_row(row: &Row<'_>) -> RowResult<StateRow> {
    Ok(StateRow {
        projector: row.get("projector")?,
        last_applied_sequence: non_negative("lastAppliedSequence", row.get("lastAppliedSequence")?)?,
        updated_at: row.get("updatedAt")?,
    })
}
