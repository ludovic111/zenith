//! `ProjectionProjectRepository` (`persistence/Layers/ProjectionProjects.ts`).

use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{parse_json, parse_json_opt, to_json};
use crate::conn::Conn;
use crate::error::{named_sql, Raw, RawResult, Result};

/// `ProjectionProject`. `repositoryIdentity` is not stored; it is resolved from git at read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectionProject {
    pub project_id: String,
    pub title: String,
    pub workspace_root: String,
    /// `ModelSelection | null`, encoded.
    pub default_model_selection: Option<Value>,
    /// `ThreadEnvMode | null`.
    pub default_thread_env_mode: Option<String>,
    pub auto_pull: bool,
    /// `optional(NullOr(String))`; `None` covers both absent and null (both store NULL).
    pub favicon_path: Option<String>,
    /// `optional(NullOr(ProjectIconOverride))`, encoded.
    pub project_icon: Option<Value>,
    /// `Array<ProjectScript>`, encoded.
    pub scripts: Value,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
}

/// `upsert`: insert or replace by `project_id`.
pub fn upsert(conn: &Conn, row: &ProjectionProject) -> Result<()> {
    let result = conn
        .execute(
            r#"
        INSERT INTO projection_projects (
          project_id,
          title,
          workspace_root,
          default_model_selection_json,
          default_thread_env_mode,
          auto_pull,
          favicon_path,
          project_icon_json,
          scripts_json,
          created_at,
          updated_at,
          deleted_at
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
          ?9,
          ?10,
          ?11,
          ?12
        )
        ON CONFLICT (project_id)
        DO UPDATE SET
          title = excluded.title,
          workspace_root = excluded.workspace_root,
          default_model_selection_json = excluded.default_model_selection_json,
          default_thread_env_mode = excluded.default_thread_env_mode,
          auto_pull = excluded.auto_pull,
          favicon_path = excluded.favicon_path,
          project_icon_json = excluded.project_icon_json,
          scripts_json = excluded.scripts_json,
          created_at = excluded.created_at,
          updated_at = excluded.updated_at,
          deleted_at = excluded.deleted_at
      "#,
            params![
                row.project_id,
                row.title,
                row.workspace_root,
                // `defaultModelSelection !== null ? JSON.stringify(…) : null`
                row.default_model_selection.as_ref().map(to_json),
                row.default_thread_env_mode,
                if row.auto_pull { 1 } else { 0 },
                row.favicon_path,
                // `projectIcon ? JSON.stringify(projectIcon) : null`
                row.project_icon.as_ref().filter(|icon| !icon.is_null()).map(to_json),
                to_json(&row.scripts),
                row.created_at,
                row.updated_at,
                row.deleted_at,
            ],
        )
        .map(|_| ())
        .map_err(Raw::from);
    named_sql(result, "ProjectionProjectRepository.upsert:query")
}

fn project_from_row(row: &Row<'_>) -> RawResult<ProjectionProject> {
    let scripts: String = row.get("scripts")?;
    let scripts = parse_json("scripts", &scripts)?;
    if !scripts.is_array() {
        return Err(Raw::Decode("scripts: Encoding(InvalidType)".into()));
    }
    let auto_pull: i64 = row.get("autoPull")?;
    Ok(ProjectionProject {
        project_id: row.get("projectId")?,
        title: row.get("title")?,
        workspace_root: row.get("workspaceRoot")?,
        default_model_selection: parse_json_opt("defaultModelSelection", row.get("defaultModelSelection")?)?,
        default_thread_env_mode: row.get("defaultThreadEnvMode")?,
        // `autoPull: row.autoPull === 1`
        auto_pull: auto_pull == 1,
        favicon_path: row.get("faviconPath")?,
        project_icon: parse_json_opt("projectIcon", row.get("projectIcon")?)?,
        scripts,
        created_at: row.get("createdAt")?,
        updated_at: row.get("updatedAt")?,
        deleted_at: row.get("deletedAt")?,
    })
}

/// `getById`.
pub fn get_by_id(conn: &Conn, project_id: &str) -> Result<Option<ProjectionProject>> {
    let result = (|| -> RawResult<Option<ProjectionProject>> {
        let mut statement = conn.prepare(
            r#"
        SELECT
          project_id AS "projectId",
          title,
          workspace_root AS "workspaceRoot",
          default_model_selection_json AS "defaultModelSelection",
          default_thread_env_mode AS "defaultThreadEnvMode",
          auto_pull AS "autoPull",
          favicon_path AS "faviconPath",
          project_icon_json AS "projectIcon",
          scripts_json AS "scripts",
          created_at AS "createdAt",
          updated_at AS "updatedAt",
          deleted_at AS "deletedAt"
        FROM projection_projects
        WHERE project_id = ?1
      "#,
        )?;
        let mut rows = statement.query(params![project_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(project_from_row(row)?)),
            None => Ok(None),
        }
    })();
    named_sql(result, "ProjectionProjectRepository.getById:query")
}
