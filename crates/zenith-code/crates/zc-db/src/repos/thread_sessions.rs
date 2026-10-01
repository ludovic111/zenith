//! `ProjectionThreadSessionRepository` (`persistence/Layers/ProjectionThreadSessions.ts`).

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};

use super::{literal, RUNTIME_MODES, SESSION_STATUSES};
use crate::conn::Conn;
use crate::error::{named_sql, Raw, RawResult, Result};

/// `ProjectionThreadSession`. (`provider_session_id`/`provider_thread_id` exist in the table
/// but are no longer written or read.)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionThreadSession {
    pub thread_id: String,
    /// `OrchestrationSessionStatus`.
    pub status: String,
    pub provider_name: Option<String>,
    pub provider_instance_id: Option<String>,
    /// `RuntimeMode`.
    pub runtime_mode: String,
    pub active_turn_id: Option<String>,
    pub last_error: Option<String>,
    pub updated_at: String,
}

/// `upsert`.
pub fn upsert(conn: &Conn, row: &ProjectionThreadSession) -> Result<()> {
    let result = conn
        .execute(
            r#"
        INSERT INTO projection_thread_sessions (
          thread_id,
          status,
          provider_name,
          provider_instance_id,
          runtime_mode,
          active_turn_id,
          last_error,
          updated_at
        )
        VALUES (
          ?1,
          ?2,
          ?3,
          ?4,
          ?5,
          ?6,
          ?7,
          ?8
        )
        ON CONFLICT (thread_id)
        DO UPDATE SET
          status = excluded.status,
          provider_name = excluded.provider_name,
          provider_instance_id = excluded.provider_instance_id,
          runtime_mode = excluded.runtime_mode,
          active_turn_id = excluded.active_turn_id,
          last_error = excluded.last_error,
          updated_at = excluded.updated_at
      "#,
            params![
                row.thread_id,
                row.status,
                row.provider_name,
                row.provider_instance_id,
                row.runtime_mode,
                row.active_turn_id,
                row.last_error,
                row.updated_at,
            ],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadSessionRepository.upsert:query")
}

fn session_from_row(row: &Row<'_>) -> RawResult<ProjectionThreadSession> {
    Ok(ProjectionThreadSession {
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

/// `getByThreadId`.
pub fn get_by_thread_id(conn: &Conn, thread_id: &str) -> Result<Option<ProjectionThreadSession>> {
    let result = (|| -> RawResult<Option<ProjectionThreadSession>> {
        let mut statement = conn.prepare(
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
        WHERE thread_id = ?1
      "#,
        )?;
        let mut rows = statement.query(params![thread_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(session_from_row(row)?)),
            None => Ok(None),
        }
    })();
    named_sql(result, "ProjectionThreadSessionRepository.getByThreadId:query")
}

/// `deleteByThreadId`.
pub fn delete_by_thread_id(conn: &Conn, thread_id: &str) -> Result<()> {
    let result = conn
        .execute(
            r#"
        DELETE FROM projection_thread_sessions
        WHERE thread_id = ?1
      "#,
            params![thread_id],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadSessionRepository.deleteByThreadId:query")
}
