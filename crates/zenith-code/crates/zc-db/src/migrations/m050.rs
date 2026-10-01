//! 050_ProjectionThreadPullRequests: the link table, plus the JavaScript backfill of the
//! legacy single `linked_pull_request_json` link (kept so a rollback keeps its data).

use rusqlite::params;
use serde_json::Value;

use super::exec;
use crate::conn::Conn;
use crate::pr_keys::{js_trim, legacy_thread_pull_request_key};

struct LegacyLinkedPullRequest {
    repository: String,
    number: i64,
    url: String,
}

/// `parseLegacyLinkedPullRequest`.
fn parse_legacy_linked_pull_request(json: &str) -> Option<LegacyLinkedPullRequest> {
    let value: Value = serde_json::from_str(json).ok()?;
    let object = value.as_object()?;
    let repository = object.get("repository")?.as_str()?;
    if js_trim(repository).is_empty() {
        return None;
    }
    // `typeof number === "number" && Number.isInteger(number) && number >= 1`.
    let number = object.get("number")?;
    let number = match (number.as_i64(), number.as_f64()) {
        (Some(n), _) => n,
        (None, Some(f)) if f.fract() == 0.0 && f >= 1.0 && f <= i64::MAX as f64 => f as i64,
        _ => return None,
    };
    if number < 1 {
        return None;
    }
    let url = object.get("url")?.as_str()?;
    if js_trim(url).is_empty() {
        return None;
    }
    Some(LegacyLinkedPullRequest {
        repository: repository.to_string(),
        number,
        url: url.to_string(),
    })
}

pub(super) fn m050(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_thread_pull_requests (
      thread_id TEXT NOT NULL,
      host TEXT NOT NULL,
      repository TEXT NOT NULL,
      number INTEGER NOT NULL,
      url TEXT NOT NULL,
      source TEXT NOT NULL,
      linked_at TEXT NOT NULL,
      snapshot_json TEXT,
      stack_json TEXT,
      PRIMARY KEY (thread_id, host, repository, number)
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_thread_pull_requests_pr
    ON projection_thread_pull_requests(host, repository, number)
  "#,
    )?;

    let legacy_rows: Vec<(String, String, String)> = {
        let mut statement = conn.raw().prepare(
            r#"
    SELECT
      thread_id AS "threadId",
      updated_at AS "updatedAt",
      linked_pull_request_json AS "linkedPullRequestJson"
    FROM projection_threads
    WHERE linked_pull_request_json IS NOT NULL
  "#,
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };

    for (thread_id, updated_at, linked_json) in legacy_rows {
        let Some(linked) = parse_legacy_linked_pull_request(&linked_json) else {
            continue;
        };
        let key = legacy_thread_pull_request_key(&linked.repository, linked.number, &linked.url, None);
        conn.execute(
            r#"
      INSERT OR IGNORE INTO projection_thread_pull_requests (
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
        'manual',
        ?6,
        NULL,
        NULL
      )
    "#,
            params![thread_id, key.host, key.repository, linked.number, linked.url, updated_at],
        )?;
    }
    Ok(())
}
