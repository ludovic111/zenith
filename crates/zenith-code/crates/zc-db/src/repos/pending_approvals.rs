//! `ProjectionPendingApprovalRepository` (`persistence/Layers/ProjectionPendingApprovals.ts`).

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};

use super::literal;
use crate::conn::Conn;
use crate::error::{named_sql, Raw, RawResult, Result};

pub const APPROVAL_STATUSES: &[&str] = &["pending", "resolved"];
pub const APPROVAL_DECISIONS: &[&str] = &["accept", "acceptForSession", "acceptAlways", "decline", "cancel"];

/// `ProjectionPendingApproval`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionPendingApproval {
    pub request_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    /// [`APPROVAL_STATUSES`].
    pub status: String,
    /// `ProjectionPendingApprovalDecision`: one of [`APPROVAL_DECISIONS`] or null.
    pub decision: Option<String>,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

/// `upsert`.
pub fn upsert(conn: &Conn, row: &ProjectionPendingApproval) -> Result<()> {
    let result = conn
        .execute(
            r#"
        INSERT INTO projection_pending_approvals (
          request_id,
          thread_id,
          turn_id,
          status,
          decision,
          created_at,
          resolved_at
        )
        VALUES (
          ?1,
          ?2,
          ?3,
          ?4,
          ?5,
          ?6,
          ?7
        )
        ON CONFLICT (request_id)
        DO UPDATE SET
          thread_id = excluded.thread_id,
          turn_id = excluded.turn_id,
          status = excluded.status,
          decision = excluded.decision,
          created_at = excluded.created_at,
          resolved_at = excluded.resolved_at
      "#,
            params![
                row.request_id,
                row.thread_id,
                row.turn_id,
                row.status,
                row.decision,
                row.created_at,
                row.resolved_at,
            ],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionPendingApprovalRepository.upsert:query")
}

const APPROVAL_COLUMNS: &str = r#"
          request_id AS "requestId",
          thread_id AS "threadId",
          turn_id AS "turnId",
          status,
          decision,
          created_at AS "createdAt",
          resolved_at AS "resolvedAt""#;

fn approval_from_row(row: &Row<'_>) -> RawResult<ProjectionPendingApproval> {
    let decision: Option<String> = row.get("decision")?;
    Ok(ProjectionPendingApproval {
        request_id: row.get("requestId")?,
        thread_id: row.get("threadId")?,
        turn_id: row.get("turnId")?,
        status: literal("status", row.get("status")?, APPROVAL_STATUSES)?,
        decision: decision.map(|decision| literal("decision", decision, APPROVAL_DECISIONS)).transpose()?,
        created_at: row.get("createdAt")?,
        resolved_at: row.get("resolvedAt")?,
    })
}

/// `listByThreadId`: oldest first.
pub fn list_by_thread_id(conn: &Conn, thread_id: &str) -> Result<Vec<ProjectionPendingApproval>> {
    let sql = format!(
        r#"
        SELECT{APPROVAL_COLUMNS}
        FROM projection_pending_approvals
        WHERE thread_id = ?1
        ORDER BY created_at ASC, request_id ASC
      "#
    );
    let result = (|| -> RawResult<Vec<ProjectionPendingApproval>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![thread_id])?;
        let mut approvals = Vec::new();
        while let Some(row) = rows.next()? {
            approvals.push(approval_from_row(row)?);
        }
        Ok(approvals)
    })();
    named_sql(result, "ProjectionPendingApprovalRepository.listByThreadId:query")
}

/// `countPendingByThreadId`.
pub fn count_pending_by_thread_id(conn: &Conn, thread_id: &str) -> Result<i64> {
    let result = (|| -> RawResult<i64> {
        Ok(conn
            .prepare(
                r#"
      SELECT COUNT(*) AS count
      FROM projection_pending_approvals
      WHERE thread_id = ?1 AND status = 'pending'
    "#,
            )?
            .query_row(params![thread_id], |row| row.get(0))?)
    })();
    named_sql(result, "ProjectionPendingApprovalRepository.countPendingByThreadId:query")
}

/// `getByRequestId`.
pub fn get_by_request_id(conn: &Conn, request_id: &str) -> Result<Option<ProjectionPendingApproval>> {
    let sql = format!(
        r#"
        SELECT{APPROVAL_COLUMNS}
        FROM projection_pending_approvals
        WHERE request_id = ?1
      "#
    );
    let result = (|| -> RawResult<Option<ProjectionPendingApproval>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![request_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(approval_from_row(row)?)),
            None => Ok(None),
        }
    })();
    named_sql(result, "ProjectionPendingApprovalRepository.getByRequestId:query")
}

/// `deleteByThreadId`.
pub fn delete_by_thread_id(conn: &Conn, thread_id: &str) -> Result<()> {
    let result = conn
        .execute(
            r#"
        DELETE FROM projection_pending_approvals
        WHERE thread_id = ?1
      "#,
            params![thread_id],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionPendingApprovalRepository.deleteByThreadId:query")
}
