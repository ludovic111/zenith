//! `ProjectionThreadMessageRepository` (`persistence/Layers/ProjectionThreadMessages.ts`).
//! Streaming deltas are appended in SQL (`text || excluded.text`), so a delta never needs the
//! current text in memory.

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{literal, parse_json_opt, to_json, MESSAGE_ROLES};
use crate::conn::Conn;
use crate::error::{named_sql, Raw, RawResult, Result};

/// `ProjectionThreadMessage`. `attachments` and `context` are `optional` in TS: `None` means
/// absent, and an upsert without them keeps the stored ones (`COALESCE`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionThreadMessage {
    pub message_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    /// `OrchestrationMessageRole`.
    pub role: String,
    pub text: String,
    /// `Array<ChatAttachment>`, encoded. `Some([])` clears them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Value>,
    /// `OrchestrationMessageContext`, encoded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    pub is_streaming: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// `AppendStreamingProjectionThreadMessage`: a message without `isStreaming` (always 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendStreamingMessage {
    pub message_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub role: String,
    /// The delta to append.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    pub created_at: String,
    pub updated_at: String,
}

/// `upsert`.
pub fn upsert(conn: &Conn, row: &ProjectionThreadMessage) -> Result<()> {
    let result = conn
        .execute(
            r#"
        INSERT INTO projection_thread_messages (
          message_id,
          thread_id,
          turn_id,
          role,
          text,
          attachments_json,
          context_json,
          is_streaming,
          created_at,
          updated_at
        )
        VALUES (
          ?1,
          ?2,
          ?3,
          ?4,
          ?5,
          COALESCE(
            ?6,
            (
              SELECT attachments_json
              FROM projection_thread_messages
              WHERE message_id = ?1
            )
          ),
          COALESCE(
            ?7,
            (
              SELECT context_json
              FROM projection_thread_messages
              WHERE message_id = ?1
            )
          ),
          ?8,
          ?9,
          ?10
        )
        ON CONFLICT (message_id)
        DO UPDATE SET
          thread_id = excluded.thread_id,
          turn_id = excluded.turn_id,
          role = excluded.role,
          text = excluded.text,
          attachments_json = COALESCE(
            excluded.attachments_json,
            projection_thread_messages.attachments_json
          ),
          context_json = COALESCE(
            excluded.context_json,
            projection_thread_messages.context_json
          ),
          is_streaming = excluded.is_streaming,
          created_at = excluded.created_at,
          updated_at = excluded.updated_at
      "#,
            params![
                row.message_id,
                row.thread_id,
                row.turn_id,
                row.role,
                row.text,
                row.attachments.as_ref().map(to_json),
                row.context.as_ref().map(to_json),
                if row.is_streaming { 1 } else { 0 },
                row.created_at,
                row.updated_at,
            ],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadMessageRepository.upsert:query")
}

/// `appendStreaming`: inserts the message, or appends `text` to the stored text, keeping
/// `created_at` and marking it streaming.
pub fn append_streaming(conn: &Conn, row: &AppendStreamingMessage) -> Result<()> {
    let result = conn
        .execute(
            r#"
        INSERT INTO projection_thread_messages (
          message_id,
          thread_id,
          turn_id,
          role,
          text,
          attachments_json,
          context_json,
          is_streaming,
          created_at,
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
          1,
          ?8,
          ?9
        )
        ON CONFLICT (message_id)
        DO UPDATE SET
          thread_id = excluded.thread_id,
          turn_id = excluded.turn_id,
          role = excluded.role,
          text = projection_thread_messages.text || excluded.text,
          attachments_json = COALESCE(
            excluded.attachments_json,
            projection_thread_messages.attachments_json
          ),
          context_json = COALESCE(
            excluded.context_json,
            projection_thread_messages.context_json
          ),
          is_streaming = 1,
          updated_at = excluded.updated_at
      "#,
            params![
                row.message_id,
                row.thread_id,
                row.turn_id,
                row.role,
                row.text,
                row.attachments.as_ref().map(to_json),
                row.context.as_ref().map(to_json),
                row.created_at,
                row.updated_at,
            ],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadMessageRepository.appendStreaming:query")
}

const MESSAGE_COLUMNS: &str = r#"
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

fn message_from_row(row: &Row<'_>) -> RawResult<ProjectionThreadMessage> {
    let attachments = parse_json_opt("attachments", row.get("attachments")?)?;
    if attachments.as_ref().is_some_and(|value| !value.is_array()) {
        return Err(Raw::Decode("attachments: Encoding(InvalidType)".into()));
    }
    let is_streaming: i64 = row.get("isStreaming")?;
    Ok(ProjectionThreadMessage {
        message_id: row.get("messageId")?,
        thread_id: row.get("threadId")?,
        turn_id: row.get("turnId")?,
        role: literal("role", row.get("role")?, MESSAGE_ROLES)?,
        text: row.get("text")?,
        attachments,
        context: parse_json_opt("context", row.get("context")?)?,
        is_streaming: is_streaming == 1,
        created_at: row.get("createdAt")?,
        updated_at: row.get("updatedAt")?,
    })
}

/// `getByMessageId`.
pub fn get_by_message_id(conn: &Conn, message_id: &str) -> Result<Option<ProjectionThreadMessage>> {
    let sql = format!(
        r#"
        SELECT{MESSAGE_COLUMNS}
        FROM projection_thread_messages
        WHERE message_id = ?1
        LIMIT 1
      "#
    );
    let result = (|| -> RawResult<Option<ProjectionThreadMessage>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![message_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(message_from_row(row)?)),
            None => Ok(None),
        }
    })();
    named_sql(result, "ProjectionThreadMessageRepository.getByMessageId:query")
}

/// `hasAssistantMessageForTurn`: an assistant message exists for the turn (only a streaming one
/// when `streaming_only`), without reading any text.
pub fn has_assistant_message_for_turn(conn: &Conn, thread_id: &str, turn_id: &str, streaming_only: bool) -> Result<bool> {
    let result = (|| -> RawResult<bool> {
        let exists: i64 = conn
            .prepare(
                r#"
        SELECT EXISTS (
          SELECT 1
          FROM projection_thread_messages
          WHERE thread_id = ?1
            AND turn_id = ?2
            AND role = 'assistant'
            AND (?3 = 0 OR is_streaming = 1)
          LIMIT 1
        ) AS "exists"
      "#,
            )?
            .query_row(params![thread_id, turn_id, if streaming_only { 1 } else { 0 }], |row| row.get(0))?;
        Ok(exists == 1)
    })();
    named_sql(result, "ProjectionThreadMessageRepository.hasAssistantMessageForTurn:query")
}

/// `listByThreadId`: oldest first.
pub fn list_by_thread_id(conn: &Conn, thread_id: &str) -> Result<Vec<ProjectionThreadMessage>> {
    let sql = format!(
        r#"
        SELECT{MESSAGE_COLUMNS}
        FROM projection_thread_messages
        WHERE thread_id = ?1
        ORDER BY created_at ASC, message_id ASC
      "#
    );
    let result = (|| -> RawResult<Vec<ProjectionThreadMessage>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![thread_id])?;
        let mut messages = Vec::new();
        while let Some(row) = rows.next()? {
            messages.push(message_from_row(row)?);
        }
        Ok(messages)
    })();
    named_sql(result, "ProjectionThreadMessageRepository.listByThreadId:query")
}

/// `getLatestUserMessageAt`: the latest live user message (`import:*` messages excluded).
pub fn get_latest_user_message_at(conn: &Conn, thread_id: &str) -> Result<Option<String>> {
    let result = (|| -> RawResult<Option<String>> {
        Ok(conn
            .prepare(
                r#"
      SELECT MAX(created_at) AS "latestUserMessageAt"
      FROM projection_thread_messages
      WHERE thread_id = ?1 AND role = 'user'
        AND message_id NOT GLOB 'import:*'
    "#,
            )?
            .query_row(params![thread_id], |row| row.get(0))?)
    })();
    named_sql(result, "ProjectionThreadMessageRepository.getLatestUserMessageAt:query")
}

/// `deleteByThreadId`.
pub fn delete_by_thread_id(conn: &Conn, thread_id: &str) -> Result<()> {
    let result = conn
        .execute(
            r#"
        DELETE FROM projection_thread_messages
        WHERE thread_id = ?1
      "#,
            params![thread_id],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadMessageRepository.deleteByThreadId:query")
}
