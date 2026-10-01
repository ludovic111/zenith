//! `ProjectionThreadRepository` (`persistence/Layers/ProjectionThreads.ts`): every column of
//! `projection_threads`.

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{literal, non_negative, parse_json, parse_json_opt, to_json, INTERACTION_MODES, RUNTIME_MODES};
use crate::conn::Conn;
use crate::error::{named_sql, Raw, RawResult, Result};

/// `ProjectionThread`. The `optional(NullOr(…))` fields of the TS struct are `Option` here:
/// absent and null both store SQL NULL, and a read returns null.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionThread {
    pub thread_id: String,
    pub project_id: String,
    pub title: String,
    /// `ThreadTitleState | null`, encoded.
    pub title_state: Option<Value>,
    /// `ModelSelection`, encoded.
    pub model_selection: Value,
    /// `RuntimeMode`.
    pub runtime_mode: String,
    /// `ProviderInteractionMode`.
    pub interaction_mode: String,
    pub branch: Option<String>,
    pub worktree_path: Option<String>,
    /// `ThreadLinkedPullRequest | null` (legacy column), encoded.
    pub linked_pull_request: Option<Value>,
    /// `ThreadLinkedPullRequest | null`, encoded.
    pub branch_pull_request: Option<Value>,
    pub latest_turn_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub archived_at: Option<String>,
    /// `"settled" | "active" | null`.
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

/// `upsert`: insert or replace by `thread_id`.
pub fn upsert(conn: &Conn, row: &ProjectionThread) -> Result<()> {
    let json_or_null = |value: &Option<Value>| -> Option<String> { value.as_ref().filter(|value| !value.is_null()).map(to_json) };
    let result = conn
        .execute(
            r#"
        INSERT INTO projection_threads (
          thread_id,
          project_id,
          title,
          title_state_json,
          model_selection_json,
          runtime_mode,
          interaction_mode,
          branch,
          worktree_path,
          linked_pull_request_json,
          branch_pull_request_json,
          latest_turn_id,
          created_at,
          updated_at,
          archived_at,
          settled_override,
          settled_at,
          unsettled_at,
          snoozed_until,
          snoozed_at,
          pinned_at,
          pin_order_key,
          active_order_key,
          auto_settle_disabled_at,
          title_regeneration_request_id,
          title_regeneration_started_at,
          latest_user_message_at,
          pending_approval_count,
          pending_user_input_count,
          has_actionable_proposed_plan,
          deleted_at
        )
        VALUES (
          ?1,
          ?2,
          ?3,
          ?4,
          ?5,
          ?6,
          ?7,
          ?8,
          ?9,
          ?10,
          ?11,
          ?12,
          ?13,
          ?14,
          ?15,
          ?16,
          ?17,
          ?18,
          ?19,
          ?20,
          ?21,
          ?22,
          ?23,
          ?24,
          ?25,
          ?26,
          ?27,
          ?28,
          ?29,
          ?30,
          ?31
        )
        ON CONFLICT (thread_id)
        DO UPDATE SET
          project_id = excluded.project_id,
          title = excluded.title,
          title_state_json = excluded.title_state_json,
          model_selection_json = excluded.model_selection_json,
          runtime_mode = excluded.runtime_mode,
          interaction_mode = excluded.interaction_mode,
          branch = excluded.branch,
          worktree_path = excluded.worktree_path,
          linked_pull_request_json = excluded.linked_pull_request_json,
          branch_pull_request_json = excluded.branch_pull_request_json,
          latest_turn_id = excluded.latest_turn_id,
          created_at = excluded.created_at,
          updated_at = excluded.updated_at,
          archived_at = excluded.archived_at,
          settled_override = excluded.settled_override,
          settled_at = excluded.settled_at,
          unsettled_at = excluded.unsettled_at,
          snoozed_until = excluded.snoozed_until,
          snoozed_at = excluded.snoozed_at,
          pinned_at = excluded.pinned_at,
          pin_order_key = excluded.pin_order_key,
          active_order_key = excluded.active_order_key,
          auto_settle_disabled_at = excluded.auto_settle_disabled_at,
          title_regeneration_request_id = excluded.title_regeneration_request_id,
          title_regeneration_started_at = excluded.title_regeneration_started_at,
          latest_user_message_at = excluded.latest_user_message_at,
          pending_approval_count = excluded.pending_approval_count,
          pending_user_input_count = excluded.pending_user_input_count,
          has_actionable_proposed_plan = excluded.has_actionable_proposed_plan,
          deleted_at = excluded.deleted_at
      "#,
            params![
                row.thread_id,
                row.project_id,
                row.title,
                json_or_null(&row.title_state),
                to_json(&row.model_selection),
                row.runtime_mode,
                row.interaction_mode,
                row.branch,
                row.worktree_path,
                json_or_null(&row.linked_pull_request),
                json_or_null(&row.branch_pull_request),
                row.latest_turn_id,
                row.created_at,
                row.updated_at,
                row.archived_at,
                row.settled_override,
                row.settled_at,
                row.unsettled_at,
                row.snoozed_until,
                row.snoozed_at,
                row.pinned_at,
                row.pin_order_key,
                row.active_order_key,
                row.auto_settle_disabled_at,
                row.title_regeneration_request_id,
                row.title_regeneration_started_at,
                row.latest_user_message_at,
                row.pending_approval_count,
                row.pending_user_input_count,
                row.has_actionable_proposed_plan,
                row.deleted_at,
            ],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadRepository.upsert:query")
}

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

pub(crate) fn thread_from_row(row: &Row<'_>) -> RawResult<ProjectionThread> {
    let model_selection: Option<String> = row.get("modelSelection")?;
    let model_selection = model_selection
        .ok_or_else(|| Raw::Decode("modelSelection: MissingKey".into()))
        .and_then(|text| parse_json("modelSelection", &text))?;
    let settled_override: Option<String> = row.get("settledOverride")?;
    Ok(ProjectionThread {
        thread_id: row.get("threadId")?,
        project_id: row.get("projectId")?,
        title: row.get("title")?,
        title_state: parse_json_opt("titleState", row.get("titleState")?)?,
        model_selection,
        runtime_mode: literal("runtimeMode", row.get("runtimeMode")?, RUNTIME_MODES)?,
        interaction_mode: literal("interactionMode", row.get("interactionMode")?, INTERACTION_MODES)?,
        branch: row.get("branch")?,
        worktree_path: row.get("worktreePath")?,
        linked_pull_request: parse_json_opt("linkedPullRequest", row.get("linkedPullRequest")?)?,
        branch_pull_request: parse_json_opt("branchPullRequest", row.get("branchPullRequest")?)?,
        latest_turn_id: row.get("latestTurnId")?,
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

/// `getById`.
pub fn get_by_id(conn: &Conn, thread_id: &str) -> Result<Option<ProjectionThread>> {
    let sql = format!(
        r#"
        SELECT{THREAD_COLUMNS}
        FROM projection_threads
        WHERE thread_id = ?1
      "#
    );
    let result = (|| -> RawResult<Option<ProjectionThread>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![thread_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(thread_from_row(row)?)),
            None => Ok(None),
        }
    })();
    named_sql(result, "ProjectionThreadRepository.getById:query")
}
