//! The 54 migrations of `apps/server/src/persistence/Migrations/`, one function each.
//!
//! Generated once from the TypeScript files by extracting every `sql` template literal
//! verbatim (whitespace included: SQLite keeps the CREATE TABLE text in `sqlite_master`,
//! and ALTER TABLE ADD COLUMN splices the column definition text into it), then kept in
//! sync by hand. The control flow follows the TS (`PRAGMA table_info` checks, swallowed
//! errors in 023). 050 runs JavaScript in TS and is written in `m050.rs`.

use super::{exec, exec_ignoring_error, has_column};
use crate::conn::Conn;

/// 001_OrchestrationEvents
pub(super) fn m001(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS orchestration_events (
      sequence INTEGER PRIMARY KEY AUTOINCREMENT,
      event_id TEXT NOT NULL UNIQUE,
      aggregate_kind TEXT NOT NULL,
      stream_id TEXT NOT NULL,
      stream_version INTEGER NOT NULL,
      event_type TEXT NOT NULL,
      occurred_at TEXT NOT NULL,
      command_id TEXT,
      causation_event_id TEXT,
      correlation_id TEXT,
      actor_kind TEXT NOT NULL,
      payload_json TEXT NOT NULL,
      metadata_json TEXT NOT NULL
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE UNIQUE INDEX IF NOT EXISTS idx_orch_events_stream_version
    ON orchestration_events(aggregate_kind, stream_id, stream_version)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_orch_events_stream_sequence
    ON orchestration_events(aggregate_kind, stream_id, sequence)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_orch_events_command_id
    ON orchestration_events(command_id)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_orch_events_correlation_id
    ON orchestration_events(correlation_id)
  "#,
    )?;
    Ok(())
}

/// 002_OrchestrationCommandReceipts
pub(super) fn m002(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS orchestration_command_receipts (
      command_id TEXT PRIMARY KEY,
      aggregate_kind TEXT NOT NULL,
      aggregate_id TEXT NOT NULL,
      accepted_at TEXT NOT NULL,
      result_sequence INTEGER NOT NULL,
      status TEXT NOT NULL,
      error TEXT
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_orch_command_receipts_aggregate
    ON orchestration_command_receipts(aggregate_kind, aggregate_id)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_orch_command_receipts_sequence
    ON orchestration_command_receipts(result_sequence)
  "#,
    )?;
    Ok(())
}

/// 003_CheckpointDiffBlobs
pub(super) fn m003(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS checkpoint_diff_blobs (
      thread_id TEXT NOT NULL,
      from_turn_count INTEGER NOT NULL,
      to_turn_count INTEGER NOT NULL,
      diff TEXT NOT NULL,
      created_at TEXT NOT NULL,
      UNIQUE (thread_id, from_turn_count, to_turn_count)
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_checkpoint_diff_blobs_thread_to_turn
    ON checkpoint_diff_blobs(thread_id, to_turn_count)
  "#,
    )?;
    Ok(())
}

/// 004_ProviderSessionRuntime
pub(super) fn m004(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS provider_session_runtime (
      thread_id TEXT PRIMARY KEY,
      provider_name TEXT NOT NULL,
      adapter_key TEXT NOT NULL,
      runtime_mode TEXT NOT NULL DEFAULT 'full-access',
      status TEXT NOT NULL,
      last_seen_at TEXT NOT NULL,
      resume_cursor_json TEXT,
      runtime_payload_json TEXT
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_provider_session_runtime_status
    ON provider_session_runtime(status)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_provider_session_runtime_provider
    ON provider_session_runtime(provider_name)
  "#,
    )?;
    Ok(())
}

/// 005_Projections
pub(super) fn m005(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_projects (
      project_id TEXT PRIMARY KEY,
      title TEXT NOT NULL,
      workspace_root TEXT NOT NULL,
      default_model TEXT,
      scripts_json TEXT NOT NULL,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      deleted_at TEXT
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_threads (
      thread_id TEXT PRIMARY KEY,
      project_id TEXT NOT NULL,
      title TEXT NOT NULL,
      model TEXT NOT NULL,
      branch TEXT,
      worktree_path TEXT,
      latest_turn_id TEXT,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      deleted_at TEXT
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_thread_messages (
      message_id TEXT PRIMARY KEY,
      thread_id TEXT NOT NULL,
      turn_id TEXT,
      role TEXT NOT NULL,
      text TEXT NOT NULL,
      is_streaming INTEGER NOT NULL,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_thread_activities (
      activity_id TEXT PRIMARY KEY,
      thread_id TEXT NOT NULL,
      turn_id TEXT,
      tone TEXT NOT NULL,
      kind TEXT NOT NULL,
      summary TEXT NOT NULL,
      payload_json TEXT NOT NULL,
      created_at TEXT NOT NULL
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_thread_sessions (
      thread_id TEXT PRIMARY KEY,
      status TEXT NOT NULL,
      provider_name TEXT,
      provider_session_id TEXT,
      provider_thread_id TEXT,
      active_turn_id TEXT,
      last_error TEXT,
      updated_at TEXT NOT NULL
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_turns (
      row_id INTEGER PRIMARY KEY AUTOINCREMENT,
      thread_id TEXT NOT NULL,
      turn_id TEXT,
      pending_message_id TEXT,
      assistant_message_id TEXT,
      state TEXT NOT NULL,
      requested_at TEXT NOT NULL,
      started_at TEXT,
      completed_at TEXT,
      checkpoint_turn_count INTEGER,
      checkpoint_ref TEXT,
      checkpoint_status TEXT,
      checkpoint_files_json TEXT NOT NULL,
      UNIQUE (thread_id, turn_id),
      UNIQUE (thread_id, checkpoint_turn_count)
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_pending_approvals (
      request_id TEXT PRIMARY KEY,
      thread_id TEXT NOT NULL,
      turn_id TEXT,
      status TEXT NOT NULL,
      decision TEXT,
      created_at TEXT NOT NULL,
      resolved_at TEXT
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_state (
      projector TEXT PRIMARY KEY,
      last_applied_sequence INTEGER NOT NULL,
      updated_at TEXT NOT NULL
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_projects_updated_at
    ON projection_projects(updated_at)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_threads_project_id
    ON projection_threads(project_id)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_thread_messages_thread_created
    ON projection_thread_messages(thread_id, created_at)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_thread_activities_thread_created
    ON projection_thread_activities(thread_id, created_at)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_thread_sessions_provider_session
    ON projection_thread_sessions(provider_session_id)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_turns_thread_requested
    ON projection_turns(thread_id, requested_at)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_turns_thread_checkpoint_completed
    ON projection_turns(thread_id, checkpoint_turn_count, completed_at)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_pending_approvals_thread_status
    ON projection_pending_approvals(thread_id, status)
  "#,
    )?;
    Ok(())
}

/// 006_ProjectionThreadSessionRuntimeModeColumns
pub(super) fn m006(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
      ALTER TABLE projection_thread_sessions
      ADD COLUMN runtime_mode TEXT NOT NULL DEFAULT 'full-access'
    "#,
    )?;
    conn.execute(
        r#"
    UPDATE projection_thread_sessions
    SET runtime_mode = ?1
    WHERE runtime_mode IS NULL
  "#,
        ["full-access"],
    )?;
    Ok(())
}

/// 007_ProjectionThreadMessageAttachments
pub(super) fn m007(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    ALTER TABLE projection_thread_messages
    ADD COLUMN attachments_json TEXT
  "#,
    )?;
    Ok(())
}

/// 008_ProjectionThreadActivitySequence
pub(super) fn m008(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    ALTER TABLE projection_thread_activities
    ADD COLUMN sequence INTEGER
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_thread_activities_thread_sequence
    ON projection_thread_activities(thread_id, sequence)
  "#,
    )?;
    Ok(())
}

/// 009_ProviderSessionRuntimeMode: a no-op (`Effect.asVoid(SqlClient.SqlClient)`).
pub(super) fn m009(_conn: &Conn) -> rusqlite::Result<()> {
    Ok(())
}

/// 010_ProjectionThreadsRuntimeMode
pub(super) fn m010(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    ALTER TABLE projection_threads
    ADD COLUMN runtime_mode TEXT NOT NULL DEFAULT 'full-access'
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE projection_threads
    SET runtime_mode = 'full-access'
    WHERE runtime_mode IS NULL
  "#,
    )?;
    Ok(())
}

/// 011_OrchestrationThreadCreatedRuntimeMode
pub(super) fn m011(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    UPDATE orchestration_events
    SET payload_json = json_set(payload_json, '$.runtimeMode', 'full-access')
    WHERE event_type = 'thread.created'
      AND json_type(payload_json, '$.runtimeMode') IS NULL
  "#,
    )?;
    Ok(())
}

/// 012_ProjectionThreadsInteractionMode
pub(super) fn m012(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    ALTER TABLE projection_threads
    ADD COLUMN interaction_mode TEXT NOT NULL DEFAULT 'default'
  "#,
    )?;
    Ok(())
}

/// 013_ProjectionThreadProposedPlans
pub(super) fn m013(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS projection_thread_proposed_plans (
      plan_id TEXT PRIMARY KEY,
      thread_id TEXT NOT NULL,
      turn_id TEXT,
      plan_markdown TEXT NOT NULL,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_thread_proposed_plans_thread_created
    ON projection_thread_proposed_plans(thread_id, created_at)
  "#,
    )?;
    Ok(())
}

/// 014_ProjectionThreadProposedPlanImplementation
pub(super) fn m014(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    ALTER TABLE projection_thread_proposed_plans
    ADD COLUMN implemented_at TEXT
  "#,
    )?;
    exec(
        conn,
        r#"
    ALTER TABLE projection_thread_proposed_plans
    ADD COLUMN implementation_thread_id TEXT
  "#,
    )?;
    Ok(())
}

/// 015_ProjectionTurnsSourceProposedPlan
pub(super) fn m015(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    ALTER TABLE projection_turns
    ADD COLUMN source_proposed_plan_thread_id TEXT
  "#,
    )?;
    exec(
        conn,
        r#"
    ALTER TABLE projection_turns
    ADD COLUMN source_proposed_plan_id TEXT
  "#,
    )?;
    Ok(())
}

/// 016_CanonicalizeModelSelections
pub(super) fn m016(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    ALTER TABLE projection_projects
    ADD COLUMN default_model_selection_json TEXT
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE projection_projects
    SET default_model_selection_json = CASE
      WHEN default_model IS NULL THEN NULL
      ELSE json_object(
        'provider',
        CASE
          WHEN lower(default_model) LIKE '%claude%' THEN 'claudeAgent'
          ELSE 'codex'
        END,
        'model',
        default_model
      )
    END
    WHERE default_model_selection_json IS NULL
  "#,
    )?;
    exec(
        conn,
        r#"
    ALTER TABLE projection_threads
    ADD COLUMN model_selection_json TEXT
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE projection_threads
    SET model_selection_json = json_object(
      'provider',
      COALESCE(
        (
          SELECT provider_name
          FROM projection_thread_sessions
          WHERE projection_thread_sessions.thread_id = projection_threads.thread_id
        ),
        CASE
          WHEN lower(model) LIKE '%claude%' THEN 'claudeAgent'
          ELSE 'codex'
        END,
        'codex'
      ),
      'model',
      model
    )
    WHERE model_selection_json IS NULL
  "#,
    )?;
    exec(
        conn,
        r#"
    ALTER TABLE projection_projects
    DROP COLUMN default_model
  "#,
    )?;
    exec(
        conn,
        r#"
    ALTER TABLE projection_threads
    DROP COLUMN model
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE orchestration_events
    SET payload_json = CASE
      WHEN json_type(payload_json, '$.defaultModel') = 'null' THEN json_remove(
        json_set(payload_json, '$.defaultModelSelection', json('null')),
        '$.defaultProvider',
        '$.defaultModel',
        '$.defaultModelOptions'
      )
      ELSE json_remove(
        json_set(
          payload_json,
          '$.defaultModelSelection',
          json_patch(
            json_object(
              'provider',
              CASE
                WHEN json_extract(payload_json, '$.defaultProvider') IS NOT NULL
                THEN json_extract(payload_json, '$.defaultProvider')
                WHEN lower(json_extract(payload_json, '$.defaultModel')) LIKE '%claude%'
                THEN 'claudeAgent'
                ELSE 'codex'
              END,
              'model',
              json_extract(payload_json, '$.defaultModel')
            ),
              CASE
                WHEN json_type(payload_json, '$.defaultModelOptions') IS NULL THEN '{}'
                WHEN json_type(payload_json, '$.defaultModelOptions.codex') IS NOT NULL
                  OR json_type(payload_json, '$.defaultModelOptions.claudeAgent') IS NOT NULL
                THEN CASE
                  WHEN (
                  CASE
                    WHEN json_extract(payload_json, '$.defaultProvider') IS NOT NULL
                    THEN json_extract(payload_json, '$.defaultProvider')
                    WHEN lower(json_extract(payload_json, '$.defaultModel')) LIKE '%claude%'
                    THEN 'claudeAgent'
                    ELSE 'codex'
                    END
                  ) = 'claudeAgent'
                  THEN CASE
                    WHEN json_type(payload_json, '$.defaultModelOptions.claudeAgent') IS NOT NULL
                    THEN json_object(
                      'options',
                      json(json_extract(payload_json, '$.defaultModelOptions.claudeAgent'))
                    )
                    WHEN json_type(payload_json, '$.defaultModelOptions.codex') IS NOT NULL
                    THEN json_object(
                      'options',
                      json(json_extract(payload_json, '$.defaultModelOptions.codex'))
                    )
                    ELSE '{}'
                  END
                  ELSE CASE
                    WHEN json_type(payload_json, '$.defaultModelOptions.codex') IS NOT NULL
                    THEN json_object(
                      'options',
                      json(json_extract(payload_json, '$.defaultModelOptions.codex'))
                    )
                    WHEN json_type(payload_json, '$.defaultModelOptions.claudeAgent') IS NOT NULL
                    THEN json_object(
                      'options',
                      json(json_extract(payload_json, '$.defaultModelOptions.claudeAgent'))
                    )
                    ELSE '{}'
                  END
                END
              ELSE json_object(
                'options',
                json(json_extract(payload_json, '$.defaultModelOptions'))
              )
            END
          )
        ),
        '$.defaultProvider',
        '$.defaultModel',
        '$.defaultModelOptions'
      )
    END
    WHERE event_type IN ('project.created', 'project.meta-updated')
      AND json_type(payload_json, '$.defaultModelSelection') IS NULL
      AND json_type(payload_json, '$.defaultModel') IS NOT NULL
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE orchestration_events
    SET payload_json = json_remove(
      json_set(
        payload_json,
        '$.modelSelection',
        json_patch(
          json_object(
            'provider',
            CASE
              WHEN json_extract(payload_json, '$.provider') IS NOT NULL
              THEN json_extract(payload_json, '$.provider')
              WHEN lower(json_extract(payload_json, '$.model')) LIKE '%claude%'
              THEN 'claudeAgent'
              ELSE 'codex'
            END,
            'model',
            json_extract(payload_json, '$.model')
          ),
          CASE
            WHEN json_type(payload_json, '$.modelOptions') IS NULL THEN '{}'
            WHEN json_type(payload_json, '$.modelOptions.codex') IS NOT NULL
              OR json_type(payload_json, '$.modelOptions.claudeAgent') IS NOT NULL
            THEN CASE
              WHEN (
                CASE
                  WHEN json_extract(payload_json, '$.provider') IS NOT NULL
                  THEN json_extract(payload_json, '$.provider')
                  WHEN lower(json_extract(payload_json, '$.model')) LIKE '%claude%'
                  THEN 'claudeAgent'
                  ELSE 'codex'
                  END
              ) = 'claudeAgent'
              THEN CASE
                WHEN json_type(payload_json, '$.modelOptions.claudeAgent') IS NOT NULL
                THEN json_object(
                  'options',
                  json(json_extract(payload_json, '$.modelOptions.claudeAgent'))
                )
                WHEN json_type(payload_json, '$.modelOptions.codex') IS NOT NULL
                THEN json_object(
                  'options',
                  json(json_extract(payload_json, '$.modelOptions.codex'))
                )
                ELSE '{}'
              END
              ELSE CASE
                WHEN json_type(payload_json, '$.modelOptions.codex') IS NOT NULL
                THEN json_object(
                  'options',
                  json(json_extract(payload_json, '$.modelOptions.codex'))
                )
                WHEN json_type(payload_json, '$.modelOptions.claudeAgent') IS NOT NULL
                THEN json_object(
                  'options',
                  json(json_extract(payload_json, '$.modelOptions.claudeAgent'))
                )
                ELSE '{}'
              END
            END
            ELSE json_object('options', json(json_extract(payload_json, '$.modelOptions')))
          END
        )
      ),
      '$.provider',
      '$.model',
      '$.modelOptions'
    )
    WHERE event_type IN ('thread.created', 'thread.meta-updated', 'thread.turn-start-requested')
      AND json_type(payload_json, '$.modelSelection') IS NULL
      AND json_type(payload_json, '$.model') IS NOT NULL
  "#,
    )?;
    // Backfill thread.created events that predate the model field entirely
    exec(
        conn,
        r#"
    UPDATE orchestration_events
    SET payload_json = json_set(
      payload_json,
      '$.modelSelection',
      json(json_object('provider', 'codex', 'model', 'gpt-5.4'))
    )
    WHERE event_type = 'thread.created'
      AND json_type(payload_json, '$.modelSelection') IS NULL
      AND json_type(payload_json, '$.model') IS NULL
  "#,
    )?;
    Ok(())
}

/// 017_ProjectionThreadsArchivedAt
pub(super) fn m017(conn: &Conn) -> rusqlite::Result<()> {
    if has_column(conn, "projection_threads", "archived_at")? {
        return Ok(());
    }
    exec(
        conn,
        r#"
    ALTER TABLE projection_threads
    ADD COLUMN archived_at TEXT
  "#,
    )?;
    Ok(())
}

/// 018_ProjectionThreadsArchivedAtIndex
pub(super) fn m018(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_threads_project_archived_at
    ON projection_threads(project_id, archived_at)
  "#,
    )?;
    Ok(())
}

/// 019_ProjectionSnapshotLookupIndexes
pub(super) fn m019(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_projects_workspace_root_deleted_at
    ON projection_projects(workspace_root, deleted_at)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_threads_project_deleted_created
    ON projection_threads(project_id, deleted_at, created_at)
  "#,
    )?;
    Ok(())
}

/// 020_AuthAccessManagement
pub(super) fn m020(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS auth_pairing_links (
      id TEXT PRIMARY KEY,
      credential TEXT NOT NULL UNIQUE,
      method TEXT NOT NULL,
      role TEXT NOT NULL,
      subject TEXT NOT NULL,
      created_at TEXT NOT NULL,
      expires_at TEXT NOT NULL,
      consumed_at TEXT,
      revoked_at TEXT
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_auth_pairing_links_active
    ON auth_pairing_links(revoked_at, consumed_at, expires_at)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS auth_sessions (
      session_id TEXT PRIMARY KEY,
      subject TEXT NOT NULL,
      role TEXT NOT NULL,
      method TEXT NOT NULL,
      issued_at TEXT NOT NULL,
      expires_at TEXT NOT NULL,
      revoked_at TEXT
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_auth_sessions_active
    ON auth_sessions(revoked_at, expires_at, issued_at)
  "#,
    )?;
    Ok(())
}

/// 021_AuthSessionClientMetadata
pub(super) fn m021(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "auth_pairing_links", "label")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_pairing_links
      ADD COLUMN label TEXT
    "#,
        )?;
    }
    if !has_column(conn, "auth_sessions", "client_label")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_sessions
      ADD COLUMN client_label TEXT
    "#,
        )?;
    }
    if !has_column(conn, "auth_sessions", "client_ip_address")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_sessions
      ADD COLUMN client_ip_address TEXT
    "#,
        )?;
    }
    if !has_column(conn, "auth_sessions", "client_user_agent")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_sessions
      ADD COLUMN client_user_agent TEXT
    "#,
        )?;
    }
    if !has_column(conn, "auth_sessions", "client_device_type")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_sessions
      ADD COLUMN client_device_type TEXT NOT NULL DEFAULT 'unknown'
    "#,
        )?;
    }
    if !has_column(conn, "auth_sessions", "client_os")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_sessions
      ADD COLUMN client_os TEXT
    "#,
        )?;
    }
    if !has_column(conn, "auth_sessions", "client_browser")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_sessions
      ADD COLUMN client_browser TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 022_AuthSessionLastConnectedAt
pub(super) fn m022(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "auth_sessions", "last_connected_at")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_sessions
      ADD COLUMN last_connected_at TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 023_ProjectionThreadShellSummary
pub(super) fn m023(conn: &Conn) -> rusqlite::Result<()> {
    exec_ignoring_error(
        conn,
        r#"
    ALTER TABLE projection_threads
    ADD COLUMN latest_user_message_at TEXT
  "#,
    );
    exec_ignoring_error(
        conn,
        r#"
    ALTER TABLE projection_threads
    ADD COLUMN pending_approval_count INTEGER NOT NULL DEFAULT 0
  "#,
    );
    exec_ignoring_error(
        conn,
        r#"
    ALTER TABLE projection_threads
    ADD COLUMN pending_user_input_count INTEGER NOT NULL DEFAULT 0
  "#,
    );
    exec_ignoring_error(
        conn,
        r#"
    ALTER TABLE projection_threads
    ADD COLUMN has_actionable_proposed_plan INTEGER NOT NULL DEFAULT 0
  "#,
    );
    Ok(())
}

/// 024_BackfillProjectionThreadShellSummary
pub(super) fn m024(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    INSERT OR IGNORE INTO projection_pending_approvals (
      request_id,
      thread_id,
      turn_id,
      status,
      decision,
      created_at,
      resolved_at
    )
    SELECT
      requested.request_id,
      requested.thread_id,
      requested.turn_id,
      'pending',
      NULL,
      requested.created_at,
      NULL
    FROM (
      SELECT
        json_extract(payload_json, '$.requestId') AS request_id,
        thread_id,
        turn_id,
        created_at,
        ROW_NUMBER() OVER (
          PARTITION BY json_extract(payload_json, '$.requestId')
          ORDER BY created_at ASC, activity_id ASC
        ) AS row_number
      FROM projection_thread_activities
      WHERE kind = 'approval.requested'
        AND json_extract(payload_json, '$.requestId') IS NOT NULL
    ) AS requested
    WHERE requested.row_number = 1
  "#,
    )?;
    exec(
        conn,
        r#"
    WITH latest_resolutions AS (
      SELECT
        resolved.request_id,
        resolved.resolved_at,
        resolved.decision
      FROM (
        SELECT
          json_extract(payload_json, '$.requestId') AS request_id,
          created_at AS resolved_at,
          CASE
            WHEN json_extract(payload_json, '$.decision') IN (
              'accept',
              'acceptForSession',
              'decline',
              'cancel'
            )
            THEN json_extract(payload_json, '$.decision')
            ELSE NULL
          END AS decision,
          ROW_NUMBER() OVER (
            PARTITION BY json_extract(payload_json, '$.requestId')
            ORDER BY created_at DESC, activity_id DESC
          ) AS row_number
        FROM projection_thread_activities
        WHERE kind = 'approval.resolved'
          AND json_extract(payload_json, '$.requestId') IS NOT NULL
      ) AS resolved
      WHERE resolved.row_number = 1
    )
    UPDATE projection_pending_approvals
    SET
      status = 'resolved',
      decision = (
        SELECT latest_resolutions.decision
        FROM latest_resolutions
        WHERE latest_resolutions.request_id = projection_pending_approvals.request_id
      ),
      resolved_at = (
        SELECT latest_resolutions.resolved_at
        FROM latest_resolutions
        WHERE latest_resolutions.request_id = projection_pending_approvals.request_id
      )
    WHERE EXISTS (
      SELECT 1
      FROM latest_resolutions
      WHERE latest_resolutions.request_id = projection_pending_approvals.request_id
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    WITH latest_response_events AS (
      SELECT
        response.request_id,
        response.resolved_at,
        response.decision
      FROM (
        SELECT
          json_extract(payload_json, '$.requestId') AS request_id,
          occurred_at AS resolved_at,
          CASE
            WHEN json_extract(payload_json, '$.decision') IN (
              'accept',
              'acceptForSession',
              'decline',
              'cancel'
            )
            THEN json_extract(payload_json, '$.decision')
            ELSE NULL
          END AS decision,
          ROW_NUMBER() OVER (
            PARTITION BY json_extract(payload_json, '$.requestId')
            ORDER BY occurred_at DESC, sequence DESC
          ) AS row_number
        FROM orchestration_events
        WHERE event_type = 'thread.approval-response-requested'
          AND json_extract(payload_json, '$.requestId') IS NOT NULL
      ) AS response
      WHERE response.row_number = 1
    )
    UPDATE projection_pending_approvals
    SET
      status = 'resolved',
      decision = (
        SELECT latest_response_events.decision
        FROM latest_response_events
        WHERE latest_response_events.request_id = projection_pending_approvals.request_id
      ),
      resolved_at = (
        SELECT latest_response_events.resolved_at
        FROM latest_response_events
        WHERE latest_response_events.request_id = projection_pending_approvals.request_id
      )
    WHERE EXISTS (
      SELECT 1
      FROM latest_response_events
      WHERE latest_response_events.request_id = projection_pending_approvals.request_id
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    WITH latest_stale_failures AS (
      SELECT
        failure.request_id,
        failure.resolved_at
      FROM (
        SELECT
          json_extract(payload_json, '$.requestId') AS request_id,
          created_at AS resolved_at,
          ROW_NUMBER() OVER (
            PARTITION BY json_extract(payload_json, '$.requestId')
            ORDER BY created_at DESC, activity_id DESC
          ) AS row_number
        FROM projection_thread_activities
        WHERE kind = 'provider.approval.respond.failed'
          AND json_extract(payload_json, '$.requestId') IS NOT NULL
          AND (
            lower(COALESCE(json_extract(payload_json, '$.detail'), ''))
              LIKE '%stale pending approval request%'
            OR lower(COALESCE(json_extract(payload_json, '$.detail'), ''))
              LIKE '%unknown pending approval request%'
            OR lower(COALESCE(json_extract(payload_json, '$.detail'), ''))
              LIKE '%unknown pending permission request%'
          )
      ) AS failure
      WHERE failure.row_number = 1
    )
    UPDATE projection_pending_approvals
    SET
      status = 'resolved',
      decision = NULL,
      resolved_at = (
        SELECT latest_stale_failures.resolved_at
        FROM latest_stale_failures
        WHERE latest_stale_failures.request_id = projection_pending_approvals.request_id
      )
    WHERE status = 'pending'
      AND EXISTS (
        SELECT 1
        FROM latest_stale_failures
        WHERE latest_stale_failures.request_id = projection_pending_approvals.request_id
      )
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE projection_threads
    SET
      latest_user_message_at = (
        SELECT MAX(message.created_at)
        FROM projection_thread_messages AS message
        WHERE message.thread_id = projection_threads.thread_id
          AND message.role = 'user'
      ),
      pending_approval_count = COALESCE((
        SELECT COUNT(*)
        FROM projection_pending_approvals
        WHERE projection_pending_approvals.thread_id = projection_threads.thread_id
          AND projection_pending_approvals.status = 'pending'
      ), 0),
      pending_user_input_count = COALESCE((
        WITH latest_user_input_states AS (
          SELECT
            latest.request_id,
            latest.kind,
            latest.detail
          FROM (
            SELECT
              json_extract(activity.payload_json, '$.requestId') AS request_id,
              activity.kind,
              lower(COALESCE(json_extract(activity.payload_json, '$.detail'), '')) AS detail,
              ROW_NUMBER() OVER (
                PARTITION BY json_extract(activity.payload_json, '$.requestId')
                ORDER BY activity.created_at DESC, activity.activity_id DESC
              ) AS row_number
            FROM projection_thread_activities AS activity
            WHERE activity.thread_id = projection_threads.thread_id
              AND json_extract(activity.payload_json, '$.requestId') IS NOT NULL
              AND activity.kind IN (
                'user-input.requested',
                'user-input.resolved',
                'provider.user-input.respond.failed'
              )
          ) AS latest
          WHERE latest.row_number = 1
        )
        SELECT COUNT(*)
        FROM latest_user_input_states
        WHERE latest_user_input_states.kind = 'user-input.requested'
          OR (
            latest_user_input_states.kind = 'provider.user-input.respond.failed'
            AND latest_user_input_states.detail NOT LIKE '%stale pending user-input request%'
            AND latest_user_input_states.detail NOT LIKE '%unknown pending user-input request%'
          )
      ), 0),
      has_actionable_proposed_plan = COALESCE((
        SELECT CASE
          WHEN projection_threads.latest_turn_id IS NOT NULL
            AND EXISTS (
              SELECT 1
              FROM projection_thread_proposed_plans AS latest_turn_plan_exists
              WHERE latest_turn_plan_exists.thread_id = projection_threads.thread_id
                AND latest_turn_plan_exists.turn_id = projection_threads.latest_turn_id
            )
            THEN CASE
              WHEN (
                SELECT latest_turn_plan.implemented_at
                FROM projection_thread_proposed_plans AS latest_turn_plan
                WHERE latest_turn_plan.thread_id = projection_threads.thread_id
                  AND latest_turn_plan.turn_id = projection_threads.latest_turn_id
                ORDER BY latest_turn_plan.updated_at DESC, latest_turn_plan.plan_id DESC
                LIMIT 1
              ) IS NULL
                THEN 1
                ELSE 0
              END
          WHEN EXISTS (
            SELECT 1
            FROM projection_thread_proposed_plans AS any_plan
            WHERE any_plan.thread_id = projection_threads.thread_id
          )
            THEN CASE
              WHEN (
                SELECT latest_plan.implemented_at
                FROM projection_thread_proposed_plans AS latest_plan
                WHERE latest_plan.thread_id = projection_threads.thread_id
                ORDER BY latest_plan.updated_at DESC, latest_plan.plan_id DESC
                LIMIT 1
              ) IS NULL
                THEN 1
                ELSE 0
              END
          ELSE 0
        END
      ), 0)
  "#,
    )?;
    Ok(())
}

/// 025_CleanupInvalidProjectionPendingApprovals
pub(super) fn m025(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    DELETE FROM projection_pending_approvals
    WHERE NOT EXISTS (
      SELECT 1
      FROM projection_thread_activities AS activity
      WHERE activity.kind = 'approval.requested'
        AND json_extract(activity.payload_json, '$.requestId')
          = projection_pending_approvals.request_id
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE projection_threads
    SET pending_approval_count = COALESCE((
      SELECT COUNT(*)
      FROM projection_pending_approvals
      WHERE projection_pending_approvals.thread_id = projection_threads.thread_id
        AND projection_pending_approvals.status = 'pending'
    ), 0)
  "#,
    )?;
    Ok(())
}

/// 026_CanonicalizeModelSelectionOptions
pub(super) fn m026(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    UPDATE projection_threads
    SET model_selection_json = json_set(
      model_selection_json,
      '$.options',
      (
        SELECT json_group_array(
          json_object(
            'id', key,
            'value',
            CASE type
              WHEN 'true' THEN json('true')
              WHEN 'false' THEN json('false')
              ELSE atom
            END
          )
        )
        FROM json_each(json_extract(model_selection_json, '$.options'))
        WHERE (type = 'text' AND trim(coalesce(atom, '')) != '')
           OR type IN ('true', 'false')
      )
    )
    WHERE model_selection_json IS NOT NULL
      AND json_type(model_selection_json, '$.options') = 'object'
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE projection_projects
    SET default_model_selection_json = json_set(
      default_model_selection_json,
      '$.options',
      (
        SELECT json_group_array(
          json_object(
            'id', key,
            'value',
            CASE type
              WHEN 'true' THEN json('true')
              WHEN 'false' THEN json('false')
              ELSE atom
            END
          )
        )
        FROM json_each(json_extract(default_model_selection_json, '$.options'))
        WHERE (type = 'text' AND trim(coalesce(atom, '')) != '')
           OR type IN ('true', 'false')
      )
    )
    WHERE default_model_selection_json IS NOT NULL
      AND json_type(default_model_selection_json, '$.options') = 'object'
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE orchestration_events
    SET payload_json = json_set(
      payload_json,
      '$.modelSelection.options',
      (
        SELECT json_group_array(
          json_object(
            'id', key,
            'value',
            CASE type
              WHEN 'true' THEN json('true')
              WHEN 'false' THEN json('false')
              ELSE atom
            END
          )
        )
        FROM json_each(json_extract(payload_json, '$.modelSelection.options'))
        WHERE (type = 'text' AND trim(coalesce(atom, '')) != '')
           OR type IN ('true', 'false')
      )
    )
    WHERE event_type IN (
      'thread.created',
      'thread.meta-updated',
      'thread.turn-start-requested'
    )
      AND json_type(payload_json, '$.modelSelection.options') = 'object'
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE orchestration_events
    SET payload_json = json_set(
      payload_json,
      '$.defaultModelSelection.options',
      (
        SELECT json_group_array(
          json_object(
            'id', key,
            'value',
            CASE type
              WHEN 'true' THEN json('true')
              WHEN 'false' THEN json('false')
              ELSE atom
            END
          )
        )
        FROM json_each(json_extract(payload_json, '$.defaultModelSelection.options'))
        WHERE (type = 'text' AND trim(coalesce(atom, '')) != '')
           OR type IN ('true', 'false')
      )
    )
    WHERE event_type IN ('project.created', 'project.meta-updated')
      AND json_type(payload_json, '$.defaultModelSelection.options') = 'object'
  "#,
    )?;
    Ok(())
}

/// 027_ProviderSessionRuntimeInstanceId
pub(super) fn m027(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "provider_session_runtime", "provider_instance_id")? {
        exec(
            conn,
            r#"
      ALTER TABLE provider_session_runtime
      ADD COLUMN provider_instance_id TEXT
    "#,
        )?;
    }
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_provider_session_runtime_instance
    ON provider_session_runtime(provider_instance_id)
  "#,
    )?;
    Ok(())
}

/// 028_ProjectionThreadSessionInstanceId
pub(super) fn m028(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_thread_sessions", "provider_instance_id")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_thread_sessions
      ADD COLUMN provider_instance_id TEXT
    "#,
        )?;
    }
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_thread_sessions_instance
    ON projection_thread_sessions(provider_instance_id)
  "#,
    )?;
    Ok(())
}

/// 029_ProjectionThreadDetailOrderingIndexes
pub(super) fn m029(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_thread_activities_thread_sequence_created_id
    ON projection_thread_activities(thread_id, sequence, created_at, activity_id)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_thread_messages_thread_created_id
    ON projection_thread_messages(thread_id, created_at, message_id)
  "#,
    )?;
    Ok(())
}

/// 030_ProjectionThreadShellArchiveIndexes
pub(super) fn m030(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_threads_shell_active
    ON projection_threads(deleted_at, archived_at, project_id, created_at, thread_id)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_threads_shell_archived
    ON projection_threads(deleted_at, archived_at, project_id, thread_id)
  "#,
    )?;
    Ok(())
}

/// 031_AuthAuthorizationScopes
pub(super) fn m031(conn: &Conn) -> rusqlite::Result<()> {
    exec(conn, r#"DROP TABLE IF EXISTS auth_pairing_links"#)?;
    exec(conn, r#"DROP TABLE IF EXISTS auth_sessions"#)?;
    exec(
        conn,
        r#"
    CREATE TABLE auth_pairing_links (
      id TEXT PRIMARY KEY,
      credential TEXT NOT NULL UNIQUE,
      method TEXT NOT NULL,
      scopes TEXT NOT NULL,
      subject TEXT NOT NULL,
      label TEXT,
      created_at TEXT NOT NULL,
      expires_at TEXT NOT NULL,
      consumed_at TEXT,
      revoked_at TEXT
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX idx_auth_pairing_links_active
    ON auth_pairing_links(revoked_at, consumed_at, expires_at)
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE TABLE auth_sessions (
      session_id TEXT PRIMARY KEY,
      subject TEXT NOT NULL,
      scopes TEXT NOT NULL,
      method TEXT NOT NULL,
      client_label TEXT,
      client_ip_address TEXT,
      client_user_agent TEXT,
      client_device_type TEXT NOT NULL DEFAULT 'unknown',
      client_os TEXT,
      client_browser TEXT,
      issued_at TEXT NOT NULL,
      expires_at TEXT NOT NULL,
      last_connected_at TEXT,
      revoked_at TEXT
    )
  "#,
    )?;
    exec(
        conn,
        r#"
    CREATE INDEX idx_auth_sessions_active
    ON auth_sessions(revoked_at, expires_at, issued_at)
  "#,
    )?;
    Ok(())
}

/// 032_AuthPairingProofKeyThumbprint
pub(super) fn m032(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "auth_pairing_links", "proof_key_thumbprint")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_pairing_links
      ADD COLUMN proof_key_thumbprint TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 033_ProjectionThreadsSettled
pub(super) fn m033(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "settled_override")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN settled_override TEXT
    "#,
        )?;
    }
    if !has_column(conn, "projection_threads", "settled_at")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN settled_at TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 034_ProjectionThreadsSnoozed
pub(super) fn m034(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "snoozed_until")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN snoozed_until TEXT
    "#,
        )?;
    }
    if !has_column(conn, "projection_threads", "snoozed_at")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN snoozed_at TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 035_ProjectionThreadTitleRegeneration
pub(super) fn m035(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "title_regeneration_request_id")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN title_regeneration_request_id TEXT
    "#,
        )?;
    }
    if !has_column(conn, "projection_threads", "title_regeneration_started_at")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN title_regeneration_started_at TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 036_ProjectionThreadsPinned
pub(super) fn m036(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "pinned_at")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN pinned_at TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 037_ProjectionTurnsKeysetIndex
pub(super) fn m037(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    CREATE INDEX IF NOT EXISTS idx_projection_turns_thread_keyset
    ON projection_turns(thread_id, requested_at, turn_id)
  "#,
    )?;
    Ok(())
}

/// 038_ProjectionThreadsPinOrderKey
pub(super) fn m038(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "pin_order_key")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN pin_order_key TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 039_ProjectionProjectsDefaultThreadEnvMode
pub(super) fn m039(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_projects", "default_thread_env_mode")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_projects
      ADD COLUMN default_thread_env_mode TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 040_ProjectionProjectFaviconPath
pub(super) fn m040(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_projects", "favicon_path")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_projects
      ADD COLUMN favicon_path TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 041_AuthSessionClientConnection
pub(super) fn m041(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "auth_sessions", "client_surface")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_sessions
      ADD COLUMN client_surface TEXT
    "#,
        )?;
    }
    if !has_column(conn, "auth_sessions", "client_app_version")? {
        exec(
            conn,
            r#"
      ALTER TABLE auth_sessions
      ADD COLUMN client_app_version TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 042_ProjectionThreadLinkedPullRequest
pub(super) fn m042(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "linked_pull_request_json")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN linked_pull_request_json TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 043_ProjectionThreadsUnsettledAt
pub(super) fn m043(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "unsettled_at")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN unsettled_at TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 044_ClearAutomaticProjectModelDefaults
pub(super) fn m044(conn: &Conn) -> rusqlite::Result<()> {
    // Project creation never exposed a model choice. A later metadata event
    // containing this field is the evidence that the user set or reset one.
    exec(
        conn,
        r#"
    WITH automatically_seeded_projects AS (
      SELECT created.stream_id AS project_id
      FROM orchestration_events AS created
      WHERE created.aggregate_kind = 'project'
        AND created.event_type = 'project.created'
        AND json_type(created.payload_json, '$.defaultModelSelection') IS NOT NULL
        AND json_type(created.payload_json, '$.defaultModelSelection') <> 'null'
        AND NOT EXISTS (
          SELECT 1
          FROM orchestration_events AS configured
          WHERE configured.aggregate_kind = 'project'
            AND configured.stream_id = created.stream_id
            AND configured.event_type = 'project.meta-updated'
            AND json_type(configured.payload_json, '$.defaultModelSelection') IS NOT NULL
        )
    )
    UPDATE projection_projects
    SET default_model_selection_json = NULL
    WHERE project_id IN (SELECT project_id FROM automatically_seeded_projects)
  "#,
    )?;
    exec(
        conn,
        r#"
    UPDATE orchestration_events AS created
    SET payload_json = json_set(
      created.payload_json,
      '$.defaultModelSelection',
      json('null')
    )
    WHERE created.aggregate_kind = 'project'
      AND created.event_type = 'project.created'
      AND json_type(created.payload_json, '$.defaultModelSelection') IS NOT NULL
      AND json_type(created.payload_json, '$.defaultModelSelection') <> 'null'
      AND NOT EXISTS (
        SELECT 1
        FROM orchestration_events AS configured
        WHERE configured.aggregate_kind = 'project'
          AND configured.stream_id = created.stream_id
          AND configured.event_type = 'project.meta-updated'
          AND json_type(configured.payload_json, '$.defaultModelSelection') IS NOT NULL
      )
  "#,
    )?;
    Ok(())
}

/// 045_ProjectionProjectsAutoPull
pub(super) fn m045(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_projects", "auto_pull")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_projects
      ADD COLUMN auto_pull INTEGER NOT NULL DEFAULT 0
    "#,
        )?;
    }
    Ok(())
}

/// 046_RepairAutomaticSettlementTimestamps
pub(super) fn m046(conn: &Conn) -> rusqlite::Result<()> {
    exec(
        conn,
        r#"
    WITH activity_timestamps AS (
      SELECT thread_id, created_at AS activity_at
      FROM projection_thread_messages
      WHERE role = 'user'
      UNION ALL
      SELECT thread_id, requested_at
      FROM projection_turns
      UNION ALL
      SELECT thread_id, started_at
      FROM projection_turns
      WHERE started_at IS NOT NULL
      UNION ALL
      SELECT thread_id, completed_at
      FROM projection_turns
      WHERE completed_at IS NOT NULL
    ),
    automatic_settlements AS (
      SELECT
        stream_id AS thread_id,
        occurred_at,
        json_extract(payload_json, '$.settledAt') AS settled_at
      FROM orchestration_events
      WHERE aggregate_kind = 'thread'
        AND event_type = 'thread.settled'
        AND actor_kind = 'server'
        AND command_id LIKE 'server:auto-settle:%'
        AND json_type(payload_json, '$.settledAt') = 'text'
        AND json_extract(payload_json, '$.settledAt') = occurred_at
    )
    UPDATE projection_threads AS thread
    SET settled_at = (
      SELECT COALESCE(
        (
          SELECT activity.activity_at
          FROM activity_timestamps AS activity
          WHERE activity.thread_id = thread.thread_id
            AND julianday(activity.activity_at) IS NOT NULL
            AND julianday(activity.activity_at) <= julianday(automatic.occurred_at)
          ORDER BY julianday(activity.activity_at) DESC
          LIMIT 1
        ),
        thread.created_at
      )
      FROM automatic_settlements AS automatic
      WHERE automatic.thread_id = thread.thread_id
        AND automatic.settled_at = thread.settled_at
      LIMIT 1
    )
    WHERE thread.settled_override = 'settled'
      AND EXISTS (
        SELECT 1
        FROM automatic_settlements AS automatic
        WHERE automatic.thread_id = thread.thread_id
          AND automatic.settled_at = thread.settled_at
      )
  "#,
    )?;
    Ok(())
}

/// 047_ProjectionProjectIcon
pub(super) fn m047(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_projects", "project_icon_json")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_projects
      ADD COLUMN project_icon_json TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 048_ProjectionThreadBranchPullRequest
pub(super) fn m048(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "branch_pull_request_json")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN branch_pull_request_json TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 049_ProjectionThreadsActiveOrderKey
pub(super) fn m049(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "active_order_key")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN active_order_key TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 051_ProjectionThreadMessageContext
pub(super) fn m051(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_thread_messages", "context_json")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_thread_messages
      ADD COLUMN context_json TEXT
    "#,
        )?;
    }
    Ok(())
}

/// 052_ProjectionThreadTitleState
pub(super) fn m052(conn: &Conn) -> rusqlite::Result<()> {
    exec(conn, r#"ALTER TABLE projection_threads ADD COLUMN title_state_json TEXT"#)?;
    Ok(())
}

/// 053_PullRequestFilesViewed
pub(super) fn m053(conn: &Conn) -> rusqlite::Result<()> {
    // One row per file a reader has cleared on a host that keeps no record of its own. `revision`
    // is what the file was when it was cleared, so a push that changes it is reported as changed
    // rather than silently left ticked, and it is nullable for the reason `PullRequestFileViewedMark`
    // gives. Unticking deletes the row: absent is the resting state, and a table of "not viewed"
    // rows would grow with every diff anybody scrolled past.
    exec(
        conn,
        r#"
    CREATE TABLE IF NOT EXISTS pull_request_files_viewed (
      provider TEXT NOT NULL,
      host TEXT NOT NULL,
      repository TEXT NOT NULL,
      number INTEGER NOT NULL,
      viewer TEXT NOT NULL,
      path TEXT NOT NULL,
      revision TEXT,
      viewed_at TEXT NOT NULL,
      PRIMARY KEY (provider, host, repository, number, viewer, path)
    ) WITHOUT ROWID
  "#,
    )?;
    Ok(())
}

/// 054_ProjectionThreadsAutoSettleDisabledAt
pub(super) fn m054(conn: &Conn) -> rusqlite::Result<()> {
    if !has_column(conn, "projection_threads", "auto_settle_disabled_at")? {
        exec(
            conn,
            r#"
      ALTER TABLE projection_threads
      ADD COLUMN auto_settle_disabled_at TEXT
    "#,
        )?;
    }
    Ok(())
}
