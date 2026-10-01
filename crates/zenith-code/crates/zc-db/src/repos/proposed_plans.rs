//! `ProjectionThreadProposedPlanRepository`
//! (`persistence/Layers/ProjectionThreadProposedPlans.ts`).

use std::cmp::Ordering;

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};

use crate::collate::locale_compare;
use crate::conn::Conn;
use crate::error::{named_sql, Raw, RawResult, Result};
use crate::pr_keys::js_trim;

/// `ProjectionThreadProposedPlan`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionThreadProposedPlan {
    pub plan_id: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    /// `TrimmedNonEmptyString`: trimmed on encode and decode, never empty.
    pub plan_markdown: String,
    pub implemented_at: Option<String>,
    pub implementation_thread_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// `TrimmedNonEmptyString` on the way in or out.
fn trimmed_non_empty(field: &str, value: &str) -> RawResult<String> {
    let trimmed = js_trim(value);
    if trimmed.is_empty() {
        return Err(Raw::Decode(format!("{field}: Filter(InvalidValue)")));
    }
    Ok(trimmed.to_string())
}

/// `upsert`.
pub fn upsert(conn: &Conn, row: &ProjectionThreadProposedPlan) -> Result<()> {
    let result = (|| -> RawResult<()> {
        let markdown = trimmed_non_empty("planMarkdown", &row.plan_markdown)?;
        conn.execute(
            r#"
      INSERT INTO projection_thread_proposed_plans (
        plan_id,
        thread_id,
        turn_id,
        plan_markdown,
        implemented_at,
        implementation_thread_id,
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
        ?8
      )
      ON CONFLICT (plan_id)
      DO UPDATE SET
        thread_id = excluded.thread_id,
        turn_id = excluded.turn_id,
        plan_markdown = excluded.plan_markdown,
        implemented_at = excluded.implemented_at,
        implementation_thread_id = excluded.implementation_thread_id,
        created_at = excluded.created_at,
        updated_at = excluded.updated_at
    "#,
            params![
                row.plan_id,
                row.thread_id,
                row.turn_id,
                markdown,
                row.implemented_at,
                row.implementation_thread_id,
                row.created_at,
                row.updated_at,
            ],
        )?;
        Ok(())
    })();
    named_sql(result, "ProjectionThreadProposedPlanRepository.upsert:query")
}

const PLAN_COLUMNS: &str = r#"
        plan_id AS "planId",
        thread_id AS "threadId",
        turn_id AS "turnId",
        plan_markdown AS "planMarkdown",
        implemented_at AS "implementedAt",
        implementation_thread_id AS "implementationThreadId",
        created_at AS "createdAt",
        updated_at AS "updatedAt""#;

fn plan_from_row(row: &Row<'_>) -> RawResult<ProjectionThreadProposedPlan> {
    let markdown: String = row.get("planMarkdown")?;
    Ok(ProjectionThreadProposedPlan {
        plan_id: row.get("planId")?,
        thread_id: row.get("threadId")?,
        turn_id: row.get("turnId")?,
        plan_markdown: trimmed_non_empty("planMarkdown", &markdown)?,
        implemented_at: row.get("implementedAt")?,
        implementation_thread_id: row.get("implementationThreadId")?,
        created_at: row.get("createdAt")?,
        updated_at: row.get("updatedAt")?,
    })
}

/// `getByPlanId`: only within its thread.
pub fn get_by_plan_id(conn: &Conn, thread_id: &str, plan_id: &str) -> Result<Option<ProjectionThreadProposedPlan>> {
    let sql = format!(
        r#"
      SELECT{PLAN_COLUMNS}
      FROM projection_thread_proposed_plans
      WHERE thread_id = ?1 AND plan_id = ?2
    "#
    );
    let result = (|| -> RawResult<Option<ProjectionThreadProposedPlan>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![thread_id, plan_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(plan_from_row(row)?)),
            None => Ok(None),
        }
    })();
    named_sql(result, "ProjectionThreadProposedPlanRepository.getByPlanId:query")
}

/// `listByThreadId`: oldest first.
pub fn list_by_thread_id(conn: &Conn, thread_id: &str) -> Result<Vec<ProjectionThreadProposedPlan>> {
    let sql = format!(
        r#"
      SELECT{PLAN_COLUMNS}
      FROM projection_thread_proposed_plans
      WHERE thread_id = ?1
      ORDER BY created_at ASC, plan_id ASC
    "#
    );
    let result = (|| -> RawResult<Vec<ProjectionThreadProposedPlan>> {
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params![thread_id])?;
        let mut plans = Vec::new();
        while let Some(row) = rows.next()? {
            plans.push(plan_from_row(row)?);
        }
        Ok(plans)
    })();
    named_sql(result, "ProjectionThreadProposedPlanRepository.listByThreadId:query")
}

/// `hasActionableByThreadId`: whether the plan that counts is not implemented yet. The plan
/// that counts is the latest of the latest turn's plans, or of the thread's plans when that
/// turn has none; "latest" by `updatedAt` then `planId` with `localeCompare` (not SQLite byte
/// order), later rows winning ties. Only status columns are read.
pub fn has_actionable_by_thread_id(conn: &Conn, thread_id: &str, latest_turn_id: Option<&str>) -> Result<bool> {
    let result = (|| -> RawResult<bool> {
        let mut statement = conn.prepare(
            r#"
      SELECT
        plan_id AS "planId",
        implemented_at AS "implementedAt",
        updated_at AS "updatedAt"
      FROM projection_thread_proposed_plans
      WHERE thread_id = ?1
        AND (
          turn_id = ?2
          OR NOT EXISTS (
            SELECT 1 FROM projection_thread_proposed_plans
            WHERE thread_id = ?1 AND turn_id = ?2
          )
        )
      ORDER BY created_at ASC, plan_id ASC
    "#,
        )?;
        let candidates = statement
            .query_map(params![thread_id, latest_turn_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut selected: Option<&(String, Option<String>, String)> = None;
        for candidate in &candidates {
            let replace = match selected {
                None => true,
                Some(current) => {
                    let order = match locale_compare(&candidate.2, &current.2) {
                        Ordering::Equal => locale_compare(&candidate.0, &current.0),
                        other => other,
                    };
                    order != Ordering::Less
                }
            };
            if replace {
                selected = Some(candidate);
            }
        }
        Ok(selected.is_some_and(|(_, implemented_at, _)| implemented_at.is_none()))
    })();
    named_sql(result, "ProjectionThreadProposedPlanRepository.hasActionableByThreadId:query")
}

/// `deleteByThreadId`.
pub fn delete_by_thread_id(conn: &Conn, thread_id: &str) -> Result<()> {
    let result = conn
        .execute(
            r#"
      DELETE FROM projection_thread_proposed_plans
      WHERE thread_id = ?1
    "#,
            params![thread_id],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionThreadProposedPlanRepository.deleteByThreadId:query")
}
