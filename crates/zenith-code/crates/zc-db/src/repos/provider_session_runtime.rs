//! `ProviderSessionRuntimeRepository` (`persistence/ProviderSessionRuntime.ts`): provider
//! runtime metadata and resume cursors per thread. `runtime_payload_json.importedTranscripts`
//! is owned by [`record_imported_transcript`]; an upsert never changes it.

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use super::RUNTIME_MODES;
use super::{literal, parse_json_opt, to_json};
use crate::conn::Conn;
use crate::error::{Correlation, DbError, Raw, RawResult, Result};

/// `ProviderSessionRuntimeStatus`.
pub const RUNTIME_STATUSES: &[&str] = &["starting", "running", "stopped", "error"];

/// `ProviderSessionRuntime`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderSessionRuntime {
    pub thread_id: String,
    pub provider_name: String,
    /// Null only for rows from before the driver/instance split; consumers must materialize a
    /// concrete instance id before routing.
    pub provider_instance_id: Option<String>,
    pub adapter_key: String,
    /// [`RUNTIME_MODES`].
    pub runtime_mode: String,
    /// [`RUNTIME_STATUSES`].
    pub status: String,
    pub last_seen_at: String,
    /// `Schema.Unknown | null`.
    pub resume_cursor: Option<Value>,
    /// `Schema.Unknown | null`.
    pub runtime_payload: Option<Value>,
}

/// `ProviderSessionRuntimeUpsertOptions.onConflict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnConflict {
    #[default]
    Update,
    Ignore,
}

fn sql_error(operation: &str, thread_id: &str, raw: Raw) -> DbError {
    let correlation = Correlation::ThreadId(thread_id.to_string());
    match raw {
        // `new PersistenceSqlError({ operation, correlation, cause })`: no detail.
        Raw::Sql(error) => match DbError::sql(operation, error) {
            DbError::Sql { operation, kind, cause, .. } => DbError::Sql {
                operation,
                detail: None,
                kind,
                correlation: Some(correlation),
                cause,
            },
            other => other,
        },
        Raw::Decode(issue) => DbError::decode(operation, issue).with_correlation(correlation),
        Raw::Db(error) => error,
    }
}

const UPSERT_VALUES: &str = r#"
        INSERT INTO provider_session_runtime (
          thread_id,
          provider_name,
          provider_instance_id,
          adapter_key,
          runtime_mode,
          status,
          last_seen_at,
          resume_cursor_json,
          runtime_payload_json
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
          CASE
            WHEN json_type(?9) = 'object'
            THEN json_remove(?9, '$.importedTranscripts')
            ELSE ?9
          END
        )"#;

/// `upsert(runtime, { onConflict })`. With `Update`, the stored `importedTranscripts` survive
/// the new payload; with `Ignore`, an existing row is left alone.
pub fn upsert(conn: &Conn, runtime: &ProviderSessionRuntime, on_conflict: OnConflict) -> Result<()> {
    let conflict = match on_conflict {
        OnConflict::Ignore => "\n        ON CONFLICT (thread_id) DO NOTHING\n      ",
        OnConflict::Update => {
            r#"
        ON CONFLICT (thread_id)
        DO UPDATE SET
          provider_name = excluded.provider_name,
          provider_instance_id = excluded.provider_instance_id,
          adapter_key = excluded.adapter_key,
          runtime_mode = excluded.runtime_mode,
          status = excluded.status,
          last_seen_at = excluded.last_seen_at,
          resume_cursor_json = excluded.resume_cursor_json,
          runtime_payload_json = CASE
            WHEN json_type(
              CASE
                WHEN json_valid(provider_session_runtime.runtime_payload_json)
                THEN provider_session_runtime.runtime_payload_json
                ELSE '{}'
              END,
              '$.importedTranscripts'
            ) IS NOT NULL
            THEN json_set(
              CASE
                WHEN json_type(excluded.runtime_payload_json) = 'object'
                THEN excluded.runtime_payload_json
                ELSE '{}'
              END,
              '$.importedTranscripts',
              json_extract(provider_session_runtime.runtime_payload_json, '$.importedTranscripts')
            )
            ELSE excluded.runtime_payload_json
          END
      "#
        }
    };
    let sql = format!("{UPSERT_VALUES}{conflict}");
    let result = conn
        .execute(
            &sql,
            params![
                runtime.thread_id,
                runtime.provider_name,
                runtime.provider_instance_id,
                runtime.adapter_key,
                runtime.runtime_mode,
                runtime.status,
                runtime.last_seen_at,
                runtime.resume_cursor.as_ref().map(to_json),
                runtime.runtime_payload.as_ref().map(to_json),
            ],
        )
        .map(|_| ())
        .map_err(Raw::from);
    result.map_err(|raw| sql_error("ProviderSessionRuntimeRepository.upsert:query", &runtime.thread_id, raw))
}

/// `recordImportedTranscript`: adds (or replaces, by `providerInstanceId` + `filePath`) one
/// `AgentSessionImportSource` (encoded) in `runtime_payload_json.importedTranscripts`,
/// without touching the rest of the session state. No row, no change.
pub fn record_imported_transcript(conn: &Conn, thread_id: &str, source: &Value) -> Result<()> {
    let result = conn
        .execute(
            r#"
        WITH current_runtime AS (
          SELECT CASE
            WHEN json_valid(runtime_payload_json) THEN CASE
              WHEN json_type(runtime_payload_json) = 'object' THEN runtime_payload_json
              ELSE '{}'
            END
            ELSE '{}'
          END AS payload
          FROM provider_session_runtime
          WHERE thread_id = ?1
        )
        UPDATE provider_session_runtime
        SET runtime_payload_json = (
          SELECT json_set(
            payload,
            '$.importedTranscripts',
            json((
              SELECT json_group_array(json(value))
              FROM (
                SELECT value
                FROM json_each(CASE
                  WHEN json_type(payload, '$.importedTranscripts') = 'array'
                  THEN json_extract(payload, '$.importedTranscripts')
                  ELSE '[]'
                END)
                WHERE CASE
                  WHEN type = 'object' THEN
                    json_extract(value, '$.providerInstanceId')
                      IS NOT json_extract(?2, '$.providerInstanceId')
                    OR json_extract(value, '$.filePath') IS NOT json_extract(?2, '$.filePath')
                  ELSE 0
                END
                UNION ALL
                SELECT ?2 AS value
              )
            ))
          )
          FROM current_runtime
        )
        WHERE thread_id = ?1
      "#,
            params![thread_id, to_json(source)],
        )
        .map(|_| ())
        .map_err(Raw::from);
    result.map_err(|raw| sql_error("ProviderSessionRuntimeRepository.recordImportedTranscript:query", thread_id, raw))
}

const RUNTIME_COLUMNS: &str = r#"
          thread_id AS "threadId",
          provider_name AS "providerName",
          provider_instance_id AS "providerInstanceId",
          adapter_key AS "adapterKey",
          runtime_mode AS "runtimeMode",
          status,
          last_seen_at AS "lastSeenAt",
          resume_cursor_json AS "resumeCursor",
          runtime_payload_json AS "runtimePayload""#;

/// Raw row: read as SQL values first so a bad row can be skipped by thread id.
struct RawRuntimeRow {
    thread_id: String,
    provider_name: rusqlite::types::Value,
    provider_instance_id: rusqlite::types::Value,
    adapter_key: rusqlite::types::Value,
    runtime_mode: rusqlite::types::Value,
    status: rusqlite::types::Value,
    last_seen_at: rusqlite::types::Value,
    resume_cursor: rusqlite::types::Value,
    runtime_payload: rusqlite::types::Value,
}

fn raw_row(row: &Row<'_>) -> rusqlite::Result<RawRuntimeRow> {
    Ok(RawRuntimeRow {
        thread_id: row.get("threadId")?,
        provider_name: row.get("providerName")?,
        provider_instance_id: row.get("providerInstanceId")?,
        adapter_key: row.get("adapterKey")?,
        runtime_mode: row.get("runtimeMode")?,
        status: row.get("status")?,
        last_seen_at: row.get("lastSeenAt")?,
        resume_cursor: row.get("resumeCursor")?,
        runtime_payload: row.get("runtimePayload")?,
    })
}

fn text(field: &str, value: rusqlite::types::Value) -> RawResult<String> {
    match value {
        rusqlite::types::Value::Text(text) => Ok(text),
        _ => Err(Raw::Decode(format!("{field}: InvalidType"))),
    }
}

fn nullable_text(field: &str, value: rusqlite::types::Value) -> RawResult<Option<String>> {
    match value {
        rusqlite::types::Value::Null => Ok(None),
        other => text(field, other).map(Some),
    }
}

/// `ProviderSessionRuntimeDbRowSchema` decode.
fn decode(row: RawRuntimeRow) -> RawResult<ProviderSessionRuntime> {
    Ok(ProviderSessionRuntime {
        provider_name: text("providerName", row.provider_name)?,
        provider_instance_id: nullable_text("providerInstanceId", row.provider_instance_id)?,
        adapter_key: text("adapterKey", row.adapter_key)?,
        runtime_mode: literal("runtimeMode", text("runtimeMode", row.runtime_mode)?, RUNTIME_MODES)?,
        status: literal("status", text("status", row.status)?, RUNTIME_STATUSES)?,
        last_seen_at: text("lastSeenAt", row.last_seen_at)?,
        resume_cursor: parse_json_opt("resumeCursor", nullable_text("resumeCursor", row.resume_cursor)?)?,
        runtime_payload: parse_json_opt("runtimePayload", nullable_text("runtimePayload", row.runtime_payload)?)?,
        thread_id: row.thread_id,
    })
}

/// `getByThreadId`.
pub fn get_by_thread_id(conn: &Conn, thread_id: &str) -> Result<Option<ProviderSessionRuntime>> {
    let sql = format!(
        r#"
        SELECT{RUNTIME_COLUMNS}
        FROM provider_session_runtime
        WHERE thread_id = ?1
      "#
    );
    let row = (|| -> RawResult<Option<RawRuntimeRow>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![thread_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(raw_row(row)?)),
            None => Ok(None),
        }
    })()
    .map_err(|raw| sql_error("ProviderSessionRuntimeRepository.getByThreadId:query", thread_id, raw))?;
    row.map(decode)
        .transpose()
        .map_err(|raw| sql_error("ProviderSessionRuntimeRepository.getByThreadId:decodeRow", thread_id, raw))
}

/// `list({ excludeStopped })`: by last-seen time. Rows that do not decode (written by an older
/// build) are skipped with a warning instead of failing the list.
pub fn list(conn: &Conn, exclude_stopped: bool) -> Result<Vec<ProviderSessionRuntime>> {
    let filter = if exclude_stopped { "WHERE status != 'stopped'" } else { "" };
    let sql = format!(
        r#"
        SELECT{RUNTIME_COLUMNS}
        FROM provider_session_runtime
        {filter}
        ORDER BY last_seen_at ASC, thread_id ASC
      "#
    );
    let rows = (|| -> RawResult<Vec<RawRuntimeRow>> {
        let mut statement = conn.prepare(&sql)?;
        let rows = statement.query_map([], raw_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })()
    .map_err(|raw| match raw {
        Raw::Sql(error) => match DbError::sql("ProviderSessionRuntimeRepository.list:query", error) {
            DbError::Sql {
                operation,
                kind,
                cause,
                correlation,
                ..
            } => DbError::Sql {
                operation,
                detail: None,
                kind,
                correlation,
                cause,
            },
            other => other,
        },
        Raw::Decode(issue) => DbError::decode("ProviderSessionRuntimeRepository.list:decodeRows", issue),
        Raw::Db(error) => error,
    })?;
    let mut decoded = Vec::with_capacity(rows.len());
    for row in rows {
        let thread_id = row.thread_id.clone();
        match decode(row) {
            Ok(runtime) => decoded.push(runtime),
            Err(raw) => {
                let error = DbError::decode("ProviderSessionRuntimeRepository.list:decodeRows", raw.to_string());
                tracing::warn!(thread_id = %thread_id, error = %error, "provider.session.runtime.row-skipped");
            }
        }
    }
    Ok(decoded)
}

/// `deleteByThreadId`.
pub fn delete_by_thread_id(conn: &Conn, thread_id: &str) -> Result<()> {
    let result = conn
        .execute(
            r#"
        DELETE FROM provider_session_runtime
        WHERE thread_id = ?1
      "#,
            params![thread_id],
        )
        .map(|_| ())
        .map_err(Raw::from);
    result.map_err(|raw| sql_error("ProviderSessionRuntimeRepository.deleteByThreadId:query", thread_id, raw))
}
