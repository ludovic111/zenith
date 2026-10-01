//! `ProjectionThreadPullRequestRepository` (`persistence/ProjectionThreadPullRequests.ts`):
//! the pull requests linked to a thread, keyed by normalized `(host, repository, number)`.

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{literal, parse_json_opt, to_json};
use crate::conn::Conn;
use crate::error::{named_sql, Raw, RawResult, Result};
use crate::pr_keys::{js_trim, normalize_thread_pull_request_key};

pub const LINK_SOURCES: &[&str] = &["manual", "created", "agent", "stack", "stack-dismissed"];

/// `ProjectionThreadPullRequest`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionThreadPullRequest {
    pub thread_id: String,
    pub host: String,
    pub repository: String,
    pub number: i64,
    pub url: String,
    /// `ThreadPullRequestLinkSource`: [`LINK_SOURCES`].
    pub source: String,
    pub linked_at: String,
    /// `ThreadPullRequestSnapshot | null`, encoded.
    pub snapshot: Option<Value>,
    /// `ThreadPullRequestStack | null`, encoded.
    pub stack: Option<Value>,
}

/// `upsert`: the key is normalized first (lower case, canonical Azure path, Forgejo authority
/// recovered from `url`).
pub fn upsert(conn: &Conn, row: &ProjectionThreadPullRequest) -> Result<()> {
    let key = normalize_thread_pull_request_key(&row.host, &row.repository, row.number, None, Some(&row.url));
    let result = conn
        .execute(
            r#"
      INSERT INTO projection_thread_pull_requests (
        thread_id,
        host,
        repository,
        number,
        url,
        source,
        linked_at,
        snapshot_json,
        stack_json
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
      ON CONFLICT (thread_id, host, repository, number)
      DO UPDATE SET
        url = excluded.url,
        source = excluded.source,
        linked_at = excluded.linked_at,
        snapshot_json = excluded.snapshot_json,
        stack_json = excluded.stack_json
    "#,
            params![
                row.thread_id,
                key.host,
                key.repository,
                key.number,
                js_trim(&row.url),
                row.source,
                row.linked_at,
                row.snapshot.as_ref().filter(|value| !value.is_null()).map(to_json),
                row.stack.as_ref().filter(|value| !value.is_null()).map(to_json),
            ],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadPullRequestRepository.upsert:query")
}

const LINK_COLUMNS: &str = r#"
        thread_id AS "threadId",
        host,
        repository,
        number,
        url,
        source,
        linked_at AS "linkedAt",
        snapshot_json AS "snapshot",
        stack_json AS "stack""#;

fn link_from_row(row: &Row<'_>) -> RawResult<ProjectionThreadPullRequest> {
    let number: i64 = row.get("number")?;
    if number < 1 {
        return Err(Raw::Decode("number: Filter(InvalidValue)".into()));
    }
    Ok(ProjectionThreadPullRequest {
        thread_id: row.get("threadId")?,
        host: row.get("host")?,
        repository: row.get("repository")?,
        number,
        url: row.get("url")?,
        source: literal("source", row.get("source")?, LINK_SOURCES)?,
        linked_at: row.get("linkedAt")?,
        snapshot: parse_json_opt("snapshot", row.get("snapshot")?)?,
        stack: parse_json_opt("stack", row.get("stack")?)?,
    })
}

fn collect(conn: &Conn, sql: &str, params: impl rusqlite::Params) -> RawResult<Vec<ProjectionThreadPullRequest>> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query(params)?;
    let mut links = Vec::new();
    while let Some(row) = rows.next()? {
        links.push(link_from_row(row)?);
    }
    Ok(links)
}

/// `listByThreadId`: by link time.
pub fn list_by_thread_id(conn: &Conn, thread_id: &str) -> Result<Vec<ProjectionThreadPullRequest>> {
    let sql = format!(
        r#"
      SELECT{LINK_COLUMNS}
      FROM projection_thread_pull_requests
      WHERE thread_id = ?1
      ORDER BY linked_at ASC, number ASC
    "#
    );
    named_sql(
        collect(conn, &sql, params![thread_id]),
        "ProjectionThreadPullRequestRepository.listByThreadId:query",
    )
}

/// `listByPullRequest`: every thread linked to one pull request (key normalized first).
pub fn list_by_pull_request(conn: &Conn, host: &str, repository: &str, number: i64) -> Result<Vec<ProjectionThreadPullRequest>> {
    let key = normalize_thread_pull_request_key(host, repository, number, None, None);
    let sql = format!(
        r#"
      SELECT{LINK_COLUMNS}
      FROM projection_thread_pull_requests
      WHERE host = ?1
        AND repository = ?2
        AND number = ?3
      ORDER BY linked_at ASC, thread_id ASC
    "#
    );
    named_sql(
        collect(conn, &sql, params![key.host, key.repository, key.number]),
        "ProjectionThreadPullRequestRepository.listByPullRequest:query",
    )
}

/// `delete`: one link (key normalized first).
pub fn delete(conn: &Conn, thread_id: &str, host: &str, repository: &str, number: i64) -> Result<()> {
    let key = normalize_thread_pull_request_key(host, repository, number, None, None);
    let result = conn
        .execute(
            r#"
      DELETE FROM projection_thread_pull_requests
      WHERE thread_id = ?1
        AND host = ?2
        AND repository = ?3
        AND number = ?4
    "#,
            params![thread_id, key.host, key.repository, key.number],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadPullRequestRepository.delete:query")
}

/// `deleteByThreadId`.
pub fn delete_by_thread_id(conn: &Conn, thread_id: &str) -> Result<()> {
    let result = conn
        .execute(
            r#"
      DELETE FROM projection_thread_pull_requests
      WHERE thread_id = ?1
    "#,
            params![thread_id],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadPullRequestRepository.deleteByThreadId:query")
}

/// `deleteByThreadIdAndSource`.
pub fn delete_by_thread_id_and_source(conn: &Conn, thread_id: &str, source: &str) -> Result<()> {
    let result = conn
        .execute(
            r#"
      DELETE FROM projection_thread_pull_requests
      WHERE thread_id = ?1
        AND source = ?2
    "#,
            params![thread_id, source],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadPullRequestRepository.deleteByThreadIdAndSource:query")
}
