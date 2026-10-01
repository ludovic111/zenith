//! `PullRequestFilesViewedRepository` (`persistence/PullRequestFilesViewed.ts`): the files a
//! reader cleared on hosts that keep no record of their own. Only cleared files are rows;
//! unticking deletes.

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::conn::Conn;
use crate::error::{DbError, Raw, RawResult, Result};

/// How many marks one read carries (`MAX_FILES_VIEWED_ROWS`).
pub const MAX_FILES_VIEWED_ROWS: i64 = 500;

/// `PullRequestFilesViewedScope`: which change request, on which host, for which reader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesViewedScope {
    /// `SourceControlProviderKind`.
    pub provider: String,
    pub host: String,
    pub repository: String,
    pub number: i64,
    /// Empty when the host will not say who the reader is.
    pub viewer: String,
}

/// `PullRequestFileViewedMark`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileViewedMark {
    pub path: String,
    /// The host's name for that version of the file; empty is an answer, null is none.
    pub revision: Option<String>,
}

/// One file of a `set` batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileViewedChange {
    pub path: String,
    pub revision: Option<String>,
    pub viewed: bool,
}

/// `PullRequestFilesViewedPage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesViewedPage {
    pub files: Vec<FileViewedMark>,
    /// The store had more marks than it carried.
    pub truncated: bool,
}

fn sql_error(operation: &str, raw: Raw) -> DbError {
    match raw {
        // `new PersistenceSqlError({ operation, cause })`.
        Raw::Sql(error) => match DbError::sql(operation, error) {
            DbError::Sql { operation, kind, cause, .. } => DbError::Sql {
                operation,
                detail: None,
                kind,
                correlation: None,
                cause,
            },
            other => other,
        },
        Raw::Decode(issue) => DbError::decode("PullRequestFileViewed", issue),
        Raw::Db(error) => error,
    }
}

/// `list`: the marks by path, at most [`MAX_FILES_VIEWED_ROWS`] (one more row is read to know
/// whether the list was truncated).
pub fn list(conn: &Conn, scope: &FilesViewedScope) -> Result<FilesViewedPage> {
    let result = (|| -> RawResult<FilesViewedPage> {
        let mut rows = conn
            .prepare(
                r#"
        SELECT
          path AS "path",
          revision AS "revision"
        FROM pull_request_files_viewed
        WHERE provider = ?1
          AND host = ?2
          AND repository = ?3
          AND number = ?4
          AND viewer = ?5
        ORDER BY path
        LIMIT ?6
      "#,
            )?
            .query_map(
                params![
                    scope.provider,
                    scope.host,
                    scope.repository,
                    scope.number,
                    scope.viewer,
                    MAX_FILES_VIEWED_ROWS + 1
                ],
                |row| {
                    Ok(FileViewedMark {
                        path: row.get(0)?,
                        revision: row.get(1)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let truncated = rows.len() as i64 > MAX_FILES_VIEWED_ROWS;
        rows.truncate(MAX_FILES_VIEWED_ROWS as usize);
        Ok(FilesViewedPage { files: rows, truncated })
    })();
    result.map_err(|raw| sql_error("listPullRequestFilesViewed", raw))
}

/// `set`: one transaction for the batch; viewed files are upserted, the others deleted.
pub fn set(conn: &Conn, scope: &FilesViewedScope, files: &[FileViewedChange], viewed_at: &str) -> Result<()> {
    let result = conn.transaction(|conn| -> RawResult<()> {
        for file in files {
            if file.viewed {
                conn.execute(
                    r#"
                INSERT INTO pull_request_files_viewed (
                  provider,
                  host,
                  repository,
                  number,
                  viewer,
                  path,
                  revision,
                  viewed_at
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
                ON CONFLICT (provider, host, repository, number, viewer, path)
                DO UPDATE SET revision = excluded.revision, viewed_at = excluded.viewed_at
              "#,
                    params![
                        scope.provider,
                        scope.host,
                        scope.repository,
                        scope.number,
                        scope.viewer,
                        file.path,
                        file.revision,
                        viewed_at
                    ],
                )?;
            } else {
                conn.execute(
                    r#"
                DELETE FROM pull_request_files_viewed
                WHERE provider = ?1
                  AND host = ?2
                  AND repository = ?3
                  AND number = ?4
                  AND viewer = ?5
                  AND path = ?6
              "#,
                    params![scope.provider, scope.host, scope.repository, scope.number, scope.viewer, file.path],
                )?;
            }
        }
        Ok(())
    });
    result.map_err(|raw| sql_error("setPullRequestFilesViewed", raw))
}
