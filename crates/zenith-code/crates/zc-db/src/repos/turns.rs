//! `ProjectionTurnRepository` (`persistence/Layers/ProjectionTurns.ts`).
//!
//! A *pending turn start* is a row with `turn_id IS NULL`, `state = 'pending'`, a
//! `pending_message_id` and no checkpoint: the turn the user asked for before the provider
//! named it. There is at most one per thread ([`replace_pending_turn_start`]).

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{literal, non_negative, parse_json, to_json, CHECKPOINT_STATUSES};
use crate::conn::Conn;
use crate::error::{named, named_sql, Raw, RawResult, Result};

pub const TURN_STATES: &[&str] = &["pending", "running", "interrupted", "completed", "error"];

/// `ProjectionTurn` (`turn_id` may be null for the pending row) and `ProjectionTurnById`
/// (`turn_id` set; use [`ProjectionTurn::turn_id`] `Some`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionTurn {
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub pending_message_id: Option<String>,
    pub source_proposed_plan_thread_id: Option<String>,
    pub source_proposed_plan_id: Option<String>,
    pub assistant_message_id: Option<String>,
    /// [`TURN_STATES`].
    pub state: String,
    pub requested_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub checkpoint_turn_count: Option<i64>,
    /// `refs/t3/checkpoints/<b64url(threadId)>/turn/<n>`.
    pub checkpoint_ref: Option<String>,
    /// `OrchestrationCheckpointStatus`: `ready | missing | error`.
    pub checkpoint_status: Option<String>,
    /// `Array<OrchestrationCheckpointFile>`, encoded.
    pub checkpoint_files: Value,
}

/// `ProjectionPendingTurnStart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingTurnStart {
    pub thread_id: String,
    pub message_id: String,
    pub source_proposed_plan_thread_id: Option<String>,
    pub source_proposed_plan_id: Option<String>,
    pub requested_at: String,
}

/// `upsertByTurnId`: insert or update by `(thread_id, turn_id)`. `row.turn_id` must be set.
pub fn upsert_by_turn_id(conn: &Conn, row: &ProjectionTurn) -> Result<()> {
    let result = (|| -> RawResult<()> {
        if row.turn_id.is_none() {
            return Err(Raw::Decode("turnId: MissingKey".into()));
        }
        conn.execute(
            r#"
        INSERT INTO projection_turns (
          thread_id,
          turn_id,
          pending_message_id,
          source_proposed_plan_thread_id,
          source_proposed_plan_id,
          assistant_message_id,
          state,
          requested_at,
          started_at,
          completed_at,
          checkpoint_turn_count,
          checkpoint_ref,
          checkpoint_status,
          checkpoint_files_json
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
          ?14
        )
        ON CONFLICT (thread_id, turn_id)
        DO UPDATE SET
          pending_message_id = excluded.pending_message_id,
          source_proposed_plan_thread_id = excluded.source_proposed_plan_thread_id,
          source_proposed_plan_id = excluded.source_proposed_plan_id,
          assistant_message_id = excluded.assistant_message_id,
          state = excluded.state,
          requested_at = excluded.requested_at,
          started_at = excluded.started_at,
          completed_at = excluded.completed_at,
          checkpoint_turn_count = excluded.checkpoint_turn_count,
          checkpoint_ref = excluded.checkpoint_ref,
          checkpoint_status = excluded.checkpoint_status,
          checkpoint_files_json = excluded.checkpoint_files_json
      "#,
            params![
                row.thread_id,
                row.turn_id,
                row.pending_message_id,
                row.source_proposed_plan_thread_id,
                row.source_proposed_plan_id,
                row.assistant_message_id,
                row.state,
                row.requested_at,
                row.started_at,
                row.completed_at,
                row.checkpoint_turn_count,
                row.checkpoint_ref,
                row.checkpoint_status,
                to_json(&row.checkpoint_files),
            ],
        )?;
        Ok(())
    })();
    named(
        result,
        "ProjectionTurnRepository.upsertByTurnId:query",
        "ProjectionTurnRepository.upsertByTurnId:encodeRequest",
    )
}

const CLEAR_PENDING_SQL: &str = r#"
        DELETE FROM projection_turns
        WHERE thread_id = ?1
          AND turn_id IS NULL
          AND state = 'pending'
          AND checkpoint_turn_count IS NULL
      "#;

/// `replacePendingTurnStart`: in one transaction (a savepoint when nested), delete the thread's
/// pending row(s) and insert this one.
pub fn replace_pending_turn_start(conn: &Conn, row: &PendingTurnStart) -> Result<()> {
    let result = conn.transaction(|conn| -> RawResult<()> {
        conn.execute(CLEAR_PENDING_SQL, params![row.thread_id])?;
        conn.execute(
            r#"
        INSERT INTO projection_turns (
          thread_id,
          turn_id,
          pending_message_id,
          source_proposed_plan_thread_id,
          source_proposed_plan_id,
          assistant_message_id,
          state,
          requested_at,
          started_at,
          completed_at,
          checkpoint_turn_count,
          checkpoint_ref,
          checkpoint_status,
          checkpoint_files_json
        )
        VALUES (
          ?1,
          NULL,
          ?2,
          ?3,
          ?4,
          NULL,
          'pending',
          ?5,
          NULL,
          NULL,
          NULL,
          NULL,
          NULL,
          '[]'
        )
      "#,
            params![
                row.thread_id,
                row.message_id,
                row.source_proposed_plan_thread_id,
                row.source_proposed_plan_id,
                row.requested_at,
            ],
        )?;
        Ok(())
    });
    named(
        result,
        "ProjectionTurnRepository.replacePendingTurnStart:query",
        "ProjectionTurnRepository.replacePendingTurnStart:encodeRequest",
    )
}

/// `getPendingTurnStartByThreadId`: the newest pending turn start.
pub fn get_pending_turn_start_by_thread_id(conn: &Conn, thread_id: &str) -> Result<Option<PendingTurnStart>> {
    let result = (|| -> RawResult<Option<PendingTurnStart>> {
        let mut statement = conn.prepare(
            r#"
        SELECT
          thread_id AS "threadId",
          pending_message_id AS "messageId",
          source_proposed_plan_thread_id AS "sourceProposedPlanThreadId",
          source_proposed_plan_id AS "sourceProposedPlanId",
          requested_at AS "requestedAt"
        FROM projection_turns
        WHERE thread_id = ?1
          AND turn_id IS NULL
          AND state = 'pending'
          AND pending_message_id IS NOT NULL
          AND checkpoint_turn_count IS NULL
        ORDER BY requested_at DESC
        LIMIT 1
      "#,
        )?;
        let mut rows = statement.query(params![thread_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(PendingTurnStart {
                thread_id: row.get("threadId")?,
                message_id: row.get("messageId")?,
                source_proposed_plan_thread_id: row.get("sourceProposedPlanThreadId")?,
                source_proposed_plan_id: row.get("sourceProposedPlanId")?,
                requested_at: row.get("requestedAt")?,
            })),
            None => Ok(None),
        }
    })();
    named_sql(result, "ProjectionTurnRepository.getPendingTurnStartByThreadId:query")
}

/// `deletePendingTurnStartByThreadId`.
pub fn delete_pending_turn_start_by_thread_id(conn: &Conn, thread_id: &str) -> Result<()> {
    let result = conn.execute(CLEAR_PENDING_SQL, params![thread_id]).map(|_| ()).map_err(Raw::from);
    named_sql(result, "ProjectionTurnRepository.deletePendingTurnStartByThreadId:query")
}

const TURN_COLUMNS: &str = r#"
          thread_id AS "threadId",
          turn_id AS "turnId",
          pending_message_id AS "pendingMessageId",
          source_proposed_plan_thread_id AS "sourceProposedPlanThreadId",
          source_proposed_plan_id AS "sourceProposedPlanId",
          assistant_message_id AS "assistantMessageId",
          state,
          requested_at AS "requestedAt",
          started_at AS "startedAt",
          completed_at AS "completedAt",
          checkpoint_turn_count AS "checkpointTurnCount",
          checkpoint_ref AS "checkpointRef",
          checkpoint_status AS "checkpointStatus",
          checkpoint_files_json AS "checkpointFiles""#;

fn turn_from_row(row: &Row<'_>) -> RawResult<ProjectionTurn> {
    let files: String = row.get("checkpointFiles")?;
    let checkpoint_files = parse_json("checkpointFiles", &files)?;
    if !checkpoint_files.is_array() {
        return Err(Raw::Decode("checkpointFiles: Encoding(InvalidType)".into()));
    }
    let count: Option<i64> = row.get("checkpointTurnCount")?;
    Ok(ProjectionTurn {
        thread_id: row.get("threadId")?,
        turn_id: row.get("turnId")?,
        pending_message_id: row.get("pendingMessageId")?,
        source_proposed_plan_thread_id: row.get("sourceProposedPlanThreadId")?,
        source_proposed_plan_id: row.get("sourceProposedPlanId")?,
        assistant_message_id: row.get("assistantMessageId")?,
        state: literal("state", row.get("state")?, TURN_STATES)?,
        requested_at: row.get("requestedAt")?,
        started_at: row.get("startedAt")?,
        completed_at: row.get("completedAt")?,
        checkpoint_turn_count: count.map(|count| non_negative("checkpointTurnCount", count)).transpose()?,
        checkpoint_ref: row.get("checkpointRef")?,
        checkpoint_status: row
            .get::<_, Option<String>>("checkpointStatus")?
            .map(|status| literal("checkpointStatus", status, CHECKPOINT_STATUSES))
            .transpose()?,
        checkpoint_files,
    })
}

/// `listByThreadId`: checkpointed turns by count, then the rest by request time.
pub fn list_by_thread_id(conn: &Conn, thread_id: &str) -> Result<Vec<ProjectionTurn>> {
    let sql = format!(
        r#"
        SELECT{TURN_COLUMNS}
        FROM projection_turns
        WHERE thread_id = ?1
        ORDER BY
          CASE
            WHEN checkpoint_turn_count IS NULL THEN 1
            ELSE 0
          END ASC,
          checkpoint_turn_count ASC,
          requested_at ASC,
          turn_id ASC
      "#
    );
    let result = (|| -> RawResult<Vec<ProjectionTurn>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![thread_id])?;
        let mut turns = Vec::new();
        while let Some(row) = rows.next()? {
            turns.push(turn_from_row(row)?);
        }
        Ok(turns)
    })();
    named(
        result,
        "ProjectionTurnRepository.listByThreadId:query",
        "ProjectionTurnRepository.listByThreadId:decodeRows",
    )
}

/// `getByTurnId`.
pub fn get_by_turn_id(conn: &Conn, thread_id: &str, turn_id: &str) -> Result<Option<ProjectionTurn>> {
    let sql = format!(
        r#"
        SELECT{TURN_COLUMNS}
        FROM projection_turns
        WHERE thread_id = ?1
          AND turn_id = ?2
        LIMIT 1
      "#
    );
    let result = (|| -> RawResult<Option<ProjectionTurn>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![thread_id, turn_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(turn_from_row(row)?)),
            None => Ok(None),
        }
    })();
    named(
        result,
        "ProjectionTurnRepository.getByTurnId:query",
        "ProjectionTurnRepository.getByTurnId:decodeRow",
    )
}

/// `clearCheckpointTurnConflict`: frees `checkpoint_turn_count` on every other turn of the
/// thread that holds it, so `turn_id` can take it (the column is UNIQUE per thread).
pub fn clear_checkpoint_turn_conflict(conn: &Conn, thread_id: &str, turn_id: &str, checkpoint_turn_count: i64) -> Result<()> {
    let result = conn
        .execute(
            r#"
        UPDATE projection_turns
        SET
          checkpoint_turn_count = NULL,
          checkpoint_ref = NULL,
          checkpoint_status = NULL,
          checkpoint_files_json = '[]'
        WHERE thread_id = ?1
          AND checkpoint_turn_count = ?3
          AND (turn_id IS NULL OR turn_id <> ?2)
      "#,
            params![thread_id, turn_id, checkpoint_turn_count],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionTurnRepository.clearCheckpointTurnConflict:query")
}

/// `deleteByThreadId`.
pub fn delete_by_thread_id(conn: &Conn, thread_id: &str) -> Result<()> {
    let result = conn
        .execute(
            r#"
        DELETE FROM projection_turns
        WHERE thread_id = ?1
      "#,
            params![thread_id],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionTurnRepository.deleteByThreadId:query")
}
