//! `ProjectionThreadActivityRepository` (`persistence/Layers/ProjectionThreadActivities.ts`).

use rusqlite::{params, params_from_iter, types::Value as SqlValue, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{literal, non_negative, parse_json, to_json};
use crate::conn::Conn;
use crate::error::{named, named_sql, Raw, RawResult, Result};

/// `ProjectionThreadActivity`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionThreadActivity {
    /// The `EventId` of the activity.
    pub activity_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    /// `OrchestrationThreadActivityTone`: `info | tool | approval | error`.
    pub tone: String,
    pub kind: String,
    pub summary: String,
    /// `Schema.Unknown`.
    pub payload: Value,
    /// `optional(NonNegativeInt)`: absent for rows from before migration 008.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<i64>,
    pub created_at: String,
}

pub const ACTIVITY_TONES: &[&str] = &["info", "tool", "approval", "error"];

/// Match `String.prototype.trim` so blank saved titles cannot hide an earlier task name.
const TASK_TITLE_WHITESPACE: &str = "\u{0009}\u{000a}\u{000b}\u{000c}\u{000d}\u{0020}\u{00a0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200a}\u{2028}\u{2029}\u{202f}\u{205f}\u{3000}\u{feff}";

/// `upsert`.
pub fn upsert(conn: &Conn, row: &ProjectionThreadActivity) -> Result<()> {
    let result = conn
        .execute(
            r#"
            INSERT INTO projection_thread_activities (
              activity_id,
              thread_id,
              turn_id,
              tone,
              kind,
              summary,
              payload_json,
              sequence,
              created_at
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
              ?9
            )
            ON CONFLICT (activity_id)
            DO UPDATE SET
              thread_id = excluded.thread_id,
              turn_id = excluded.turn_id,
              tone = excluded.tone,
              kind = excluded.kind,
              summary = excluded.summary,
              payload_json = excluded.payload_json,
              sequence = excluded.sequence,
              created_at = excluded.created_at
          "#,
            params![
                row.activity_id,
                row.thread_id,
                row.turn_id,
                row.tone,
                row.kind,
                row.summary,
                to_json(&row.payload),
                row.sequence,
                row.created_at,
            ],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named(
        result,
        "ProjectionThreadActivityRepository.upsert:query",
        "ProjectionThreadActivityRepository.upsert:encodeRequest",
    )
}

const ACTIVITY_COLUMNS: &str = r#"
          activity_id AS "activityId",
          thread_id AS "threadId",
          turn_id AS "turnId",
          tone,
          kind,
          summary,
          payload_json AS "payload",
          sequence,
          created_at AS "createdAt""#;

fn activity_from_row(row: &Row<'_>) -> RawResult<ProjectionThreadActivity> {
    let payload: String = row.get("payload")?;
    let sequence: Option<i64> = row.get("sequence")?;
    Ok(ProjectionThreadActivity {
        activity_id: row.get("activityId")?,
        thread_id: row.get("threadId")?,
        turn_id: row.get("turnId")?,
        tone: literal("tone", row.get("tone")?, ACTIVITY_TONES)?,
        kind: row.get("kind")?,
        summary: row.get("summary")?,
        payload: parse_json("payload", &payload)?,
        sequence: sequence.map(|sequence| non_negative("sequence", sequence)).transpose()?,
        created_at: row.get("createdAt")?,
    })
}

fn collect(conn: &Conn, sql: &str, values: Vec<SqlValue>) -> RawResult<Vec<ProjectionThreadActivity>> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query(params_from_iter(values.iter()))?;
    let mut activities = Vec::new();
    while let Some(row) = rows.next()? {
        activities.push(activity_from_row(row)?);
    }
    Ok(activities)
}

/// `listByThreadId`: the most recent `limit` activities (optionally of some kinds), returned
/// in replay order (rows without a sequence first, then by sequence, time and id).
pub fn list_by_thread_id(conn: &Conn, thread_id: &str, activity_kinds: Option<&[String]>, limit: Option<i64>) -> Result<Vec<ProjectionThreadActivity>> {
    let mut values: Vec<SqlValue> = vec![thread_id.to_string().into()];
    let kinds_clause = match activity_kinds {
        None => String::new(),
        // `sql.in("kind", [])` is `1=0`.
        Some([]) => "AND 1=0".to_string(),
        Some(kinds) => {
            values.extend(kinds.iter().map(|kind| SqlValue::from(kind.clone())));
            format!("AND \"kind\" IN ({})", vec!["?"; kinds.len()].join(","))
        }
    };
    let limit_clause = match limit {
        None => String::new(),
        Some(limit) => {
            values.push(limit.into());
            "LIMIT ?".to_string()
        }
    };
    let sql = format!(
        r#"
        SELECT{ACTIVITY_COLUMNS}
        FROM (
          SELECT *
          FROM projection_thread_activities
          WHERE thread_id = ?
            {kinds_clause}
          ORDER BY sequence DESC, created_at DESC, activity_id DESC
          {limit_clause}
        ) AS recent_activities
        ORDER BY
          CASE WHEN sequence IS NULL THEN 0 ELSE 1 END ASC,
          sequence ASC,
          created_at ASC,
          activity_id ASC
      "#
    );
    named(
        collect(conn, &sql, values),
        "ProjectionThreadActivityRepository.listByThreadId:query",
        "ProjectionThreadActivityRepository.listByThreadId:decodeRows",
    )
}

/// `listUserInputLifecycleByThreadId`: the user-input request/resolve/failure activities.
pub fn list_user_input_lifecycle_by_thread_id(conn: &Conn, thread_id: &str) -> Result<Vec<ProjectionThreadActivity>> {
    let sql = format!(
        r#"
        SELECT{ACTIVITY_COLUMNS}
        FROM projection_thread_activities
        WHERE thread_id = ?
          AND kind IN (
            'user-input.requested',
            'user-input.resolved',
            'provider.user-input.respond.failed'
          )
        ORDER BY
          CASE WHEN sequence IS NULL THEN 0 ELSE 1 END ASC,
          sequence ASC,
          created_at ASC,
          activity_id ASC
      "#
    );
    named(
        collect(conn, &sql, vec![thread_id.to_string().into()]),
        "ProjectionThreadActivityRepository.listUserInputLifecycleByThreadId:query",
        "ProjectionThreadActivityRepository.listUserInputLifecycleByThreadId:decodeRows",
    )
}

/// `getLatestTaskActivity`: the latest `task.started`/`task.progress` of a task that names it
/// (a non-blank title, or a started detail).
pub fn get_latest_task_activity(conn: &Conn, thread_id: &str, task_id: &str) -> Result<Option<ProjectionThreadActivity>> {
    let sql = format!(
        r#"
        SELECT{ACTIVITY_COLUMNS}
        FROM projection_thread_activities
        WHERE thread_id = ?
          AND kind IN ('task.started', 'task.progress')
          AND json_extract(payload_json, '$.taskId') = ?
          AND length(trim(
            CASE
              WHEN json_type(payload_json, '$.title') = 'text'
                THEN json_extract(payload_json, '$.title')
              WHEN kind = 'task.started' AND json_type(payload_json, '$.detail') = 'text'
                THEN json_extract(payload_json, '$.detail')
              ELSE ''
            END,
            ?
          )) > 0
        ORDER BY sequence DESC, created_at DESC, activity_id DESC
        LIMIT 1
      "#
    );
    let result = collect(
        conn,
        &sql,
        vec![
            thread_id.to_string().into(),
            task_id.to_string().into(),
            TASK_TITLE_WHITESPACE.to_string().into(),
        ],
    )
    .map(|mut rows| rows.pop());
    named(
        result,
        "ProjectionThreadActivityRepository.getLatestTaskActivity:query",
        "ProjectionThreadActivityRepository.getLatestTaskActivity:decodeRow",
    )
}

/// `deleteByThreadId`.
pub fn delete_by_thread_id(conn: &Conn, thread_id: &str) -> Result<()> {
    let result = conn
        .execute(
            r#"
        DELETE FROM projection_thread_activities
        WHERE thread_id = ?1
      "#,
            params![thread_id],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadActivityRepository.deleteByThreadId:query")
}
