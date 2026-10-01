//! `pullRequest/linkedThreads.ts`: the threads linked to one pull request
//! (`pullRequests.linkedThreads`), read from `projection_thread_pull_requests`.

use std::sync::OnceLock;

use regex::Regex;
use rusqlite::params;
use zc_contracts::{PullRequestLinkedThreadsResult, PullRequestLinkedThreadsResultThreadsItem, ThreadPullRequestKey};
use zc_db::pr_keys::normalize_thread_pull_request_key;
use zc_db::{Db, DbError};
use zc_projections::pull_requests::thread_pull_request_keys_equal;

use crate::error::{Cause, PullRequestError};

struct Row {
    id: String,
    project_id: String,
    title: String,
    archived_at: Option<String>,
    host: String,
    repository: String,
    number: i64,
    url: String,
}

/// `listLinkedPullRequestThreads(key)`: active and archived (not deleted) threads linked to
/// exactly this pull request, most recently updated first. Dismissed stack links do not count.
/// Rows stored under the bare hostname (older Forgejo links) are matched through their URL.
pub async fn list_linked_pull_request_threads(db: &Db, input: &ThreadPullRequestKey) -> Result<PullRequestLinkedThreadsResult, PullRequestError> {
    static PORT: OnceLock<Regex> = OnceLock::new();
    let key = normalize_thread_pull_request_key(&input.host, &input.repository, input.number, None, None);
    let hostname = PORT
        .get_or_init(|| Regex::new(r":\d+$").expect("static regex"))
        .replace(&key.host, "")
        .into_owned();
    let (host, repository, number) = (key.host.clone(), key.repository.to_lowercase(), key.number);
    let rows = db
        .read(move |conn| {
            let operation = "listLinkedPullRequestThreads:query";
            let mut statement = conn
                .prepare(
                    r#"
      SELECT t.thread_id AS id, t.project_id AS "projectId", t.title,
        t.archived_at AS "archivedAt", link.host, link.repository, link.number, link.url
      FROM projection_thread_pull_requests AS link
      JOIN projection_threads AS t ON t.thread_id = link.thread_id
      WHERE (link.host = ?1 OR link.host = ?2)
        AND link.repository = ?3
        AND link.number = ?4
        AND link.source != 'stack-dismissed'
        AND t.deleted_at IS NULL
      ORDER BY t.updated_at DESC, t.thread_id ASC
    "#,
                )
                .map_err(|error| DbError::sql(operation, error))?;
            let rows = statement
                .query_map(params![host, hostname, repository, number], |row| {
                    Ok(Row {
                        id: row.get(0)?,
                        project_id: row.get(1)?,
                        title: row.get(2)?,
                        archived_at: row.get(3)?,
                        host: row.get(4)?,
                        repository: row.get(5)?,
                        number: row.get(6)?,
                        url: row.get(7)?,
                    })
                })
                .and_then(|rows| rows.collect::<rusqlite::Result<Vec<Row>>>())
                .map_err(|error| DbError::sql(operation, error))?;
            Ok(rows)
        })
        .await
        .map_err(|error| PullRequestError::operation("linkedThreads", "Could not load linked threads.").with_cause(Cause::message(error.to_string())))?;
    let threads = rows
        .into_iter()
        .filter(|row| {
            thread_pull_request_keys_equal(
                (&row.host, &row.repository, row.number, Some(&row.url)),
                (&key.host, &key.repository, key.number, None),
            )
        })
        .map(|row| PullRequestLinkedThreadsResultThreadsItem {
            id: row.id.into(),
            project_id: row.project_id.into(),
            title: row.title,
            archived_at: row.archived_at,
        })
        .collect();
    Ok(PullRequestLinkedThreadsResult { threads })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(host: &str, repository: &str, number: i64) -> ThreadPullRequestKey {
        ThreadPullRequestKey {
            host: host.into(),
            repository: repository.into(),
            number,
        }
    }

    async fn ids(db: &Db, host: &str, repository: &str, number: i64) -> Vec<String> {
        list_linked_pull_request_threads(db, &key(host, repository, number))
            .await
            .unwrap()
            .threads
            .into_iter()
            .map(|thread| thread.id.to_string())
            .collect()
    }

    #[tokio::test]
    async fn finds_active_and_archived_threads_for_exactly_one_pull_request_excluding_deleted_and_dismissed_links() {
        let db = Db::open_in_memory().unwrap();
        let created_at = "2026-09-01T00:00:00.000Z";
        let archived_at = "2026-09-03T00:00:00.000Z";
        /// `(id, host, repository, number, source, url)`.
        type Fixture = (&'static str, &'static str, &'static str, i64, &'static str, Option<&'static str>);
        let fixtures: Vec<Fixture> = vec![
            (
                "forgejo-old",
                "forge.example",
                "acme/web",
                7,
                "manual",
                Some("http://forge.example:3000/acme/web/pulls/7"),
            ),
            (
                "forgejo-other-port",
                "forge.example:4000",
                "acme/web",
                7,
                "manual",
                Some("http://forge.example:4000/acme/web/pulls/7"),
            ),
            ("azure", "dev.azure.com", "org/project/_git/web", 7, "manual", None),
            ("other-org", "dev.azure.com", "other/project/_git/web", 7, "manual", None),
            ("active", "github.com", "acme/web", 7, "manual", None),
            ("archived", "github.com", "acme/web", 7, "created", None),
            ("deleted", "github.com", "acme/web", 7, "manual", None),
            ("dismissed", "github.com", "acme/web", 7, "stack-dismissed", None),
            ("other-host", "github.example.com", "acme/web", 7, "manual", None),
            ("other-repository", "github.com", "acme/api", 7, "manual", None),
            ("other-number", "github.com", "acme/web", 8, "manual", None),
        ];
        db.call(move |conn| {
            let sql = |error| DbError::sql("test", error);
            conn.execute(
                "INSERT INTO projection_projects (project_id, title, workspace_root, scripts_json, created_at, updated_at)
                 VALUES ('project-1', 'Project', '/tmp/project', '[]', ?1, ?1)",
                params![created_at],
            )
            .map_err(sql)?;
            for (id, host, repository, number, source, url) in fixtures {
                let updated_at = if id == "archived" { archived_at } else { created_at };
                conn.execute(
                    r#"INSERT INTO projection_threads (thread_id, project_id, title, model_selection_json, created_at, updated_at, archived_at, deleted_at)
                       VALUES (?1, 'project-1', ?1, '{"instanceId":"codex","model":"gpt-5.4"}', ?2, ?3, ?4, ?5)"#,
                    params![
                        id,
                        created_at,
                        updated_at,
                        (id == "archived").then_some(archived_at),
                        (id == "deleted").then_some(archived_at)
                    ],
                )
                .map_err(sql)?;
                conn.execute(
                    "INSERT INTO projection_thread_pull_requests (thread_id, host, repository, number, url, source, linked_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        id,
                        host,
                        repository,
                        number,
                        url.unwrap_or("https://github.com/acme/web/pull/7"),
                        source,
                        created_at
                    ],
                )
                .map_err(sql)?;
            }
            Ok(())
        })
        .await
        .unwrap();

        assert_eq!(ids(&db, "forge.example:3000", "acme/web", 7).await, ["forgejo-old"]);
        assert_eq!(ids(&db, "forge.example:4000", "acme/web", 7).await, ["forgejo-other-port"]);
        assert_eq!(ids(&db, "org.visualstudio.com", "project/_git/web", 7).await, ["azure"]);
        let result = list_linked_pull_request_threads(&db, &key("GitHub.Com", "ACME/WEB", 7)).await.unwrap();
        assert_eq!(
            serde_json::to_value(&result).unwrap(),
            serde_json::json!({"threads": [
                {"id": "archived", "projectId": "project-1", "title": "archived", "archivedAt": archived_at},
                {"id": "active", "projectId": "project-1", "title": "active", "archivedAt": null},
            ]})
        );
        assert!(ids(&db, "github.com", "acme/web", 99).await.is_empty());
    }
}
