//! `ProjectionStateRepository` (`persistence/Layers/ProjectionState.ts`): one cursor per
//! projector (`projection.projects`, `projection.threads`, …, `projection.attachment-cleanup`).

use rusqlite::{params, params_from_iter, types::Value as SqlValue, Row};
use serde::{Deserialize, Serialize};

use super::non_negative;
use crate::conn::Conn;
use crate::error::{named_sql, Raw, RawResult, Result};

/// `ProjectionState`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionState {
    pub projector: String,
    pub last_applied_sequence: i64,
    pub updated_at: String,
}

/// `upsert`.
pub fn upsert(conn: &Conn, row: &ProjectionState) -> Result<()> {
    let result = conn
        .execute(
            r#"
        INSERT INTO projection_state (
          projector,
          last_applied_sequence,
          updated_at
        )
        VALUES (
          ?1,
          ?2,
          ?3
        )
        ON CONFLICT (projector)
        DO UPDATE SET
          last_applied_sequence = excluded.last_applied_sequence,
          updated_at = excluded.updated_at
      "#,
            params![row.projector, row.last_applied_sequence, row.updated_at],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionStateRepository.upsert:query")
}

/// `upsertMany`: one multi-row INSERT (`sql.insert(rows)`); nothing for an empty slice.
pub fn upsert_many(conn: &Conn, rows: &[ProjectionState]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let values_sql = vec!["(?,?,?)"; rows.len()].join(",");
    let sql = format!(
        r#"
            INSERT INTO projection_state ("projector","last_applied_sequence","updated_at") VALUES {values_sql}
            ON CONFLICT (projector)
            DO UPDATE SET
              last_applied_sequence = excluded.last_applied_sequence,
              updated_at = excluded.updated_at
          "#
    );
    let mut values: Vec<SqlValue> = Vec::with_capacity(rows.len() * 3);
    for row in rows {
        values.push(row.projector.clone().into());
        values.push(row.last_applied_sequence.into());
        values.push(row.updated_at.clone().into());
    }
    let result = conn.execute(&sql, params_from_iter(values.iter())).map(|_| ()).map_err(Raw::from);
    named_sql(result, "ProjectionStateRepository.upsertMany:query")
}

const STATE_COLUMNS: &str = r#"
          projector,
          last_applied_sequence AS "lastAppliedSequence",
          updated_at AS "updatedAt""#;

fn state_from_row(row: &Row<'_>) -> RawResult<ProjectionState> {
    Ok(ProjectionState {
        projector: row.get("projector")?,
        last_applied_sequence: non_negative("lastAppliedSequence", row.get("lastAppliedSequence")?)?,
        updated_at: row.get("updatedAt")?,
    })
}

/// `getByProjector`.
pub fn get_by_projector(conn: &Conn, projector: &str) -> Result<Option<ProjectionState>> {
    let sql = format!(
        r#"
        SELECT{STATE_COLUMNS}
        FROM projection_state
        WHERE projector = ?1
      "#
    );
    let result = (|| -> RawResult<Option<ProjectionState>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![projector])?;
        match rows.next()? {
            Some(row) => Ok(Some(state_from_row(row)?)),
            None => Ok(None),
        }
    })();
    named_sql(result, "ProjectionStateRepository.getByProjector:query")
}

/// `listAll`: by projector name.
pub fn list_all(conn: &Conn) -> Result<Vec<ProjectionState>> {
    let sql = format!(
        r#"
        SELECT{STATE_COLUMNS}
        FROM projection_state
        ORDER BY projector ASC
      "#
    );
    let result = (|| -> RawResult<Vec<ProjectionState>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query([])?;
        let mut states = Vec::new();
        while let Some(row) = rows.next()? {
            states.push(state_from_row(row)?);
        }
        Ok(states)
    })();
    named_sql(result, "ProjectionStateRepository.listAll:query")
}
