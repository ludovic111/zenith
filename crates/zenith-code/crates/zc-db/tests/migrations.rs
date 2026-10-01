//! The migration protocol (Effect's Migrator) and the ported migration tests of
//! `persistence/Migrations/*.test.ts`.

use rusqlite::params;
use serde_json::json;
use zc_db::migrations::{self, run_through, LATEST_MIGRATION_ID, MIGRATIONS};
use zc_db::{Conn, Db, DbError, DbOptions, MigrationErrorKind};

fn memory() -> Conn {
    Conn::open_in_memory().expect("in-memory database")
}

fn exec(conn: &Conn, sql: &str) {
    conn.execute_batch(sql).expect(sql);
}

fn rows<T: rusqlite::types::FromSql>(conn: &Conn, sql: &str) -> Vec<Vec<T>> {
    let mut statement = conn.raw().prepare(sql).unwrap();
    let count = statement.column_count();
    statement
        .query_map([], |row| (0..count).map(|index| row.get::<_, T>(index)).collect())
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

fn column_names(conn: &Conn, table: &str) -> Vec<String> {
    rows::<rusqlite::types::Value>(conn, &format!("PRAGMA table_info({table})"))
        .into_iter()
        .map(|row| match &row[1] {
            rusqlite::types::Value::Text(name) => name.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

fn index_names(conn: &Conn, table: &str) -> Vec<String> {
    rows::<rusqlite::types::Value>(conn, &format!("PRAGMA index_list({table})"))
        .into_iter()
        .map(|row| match &row[1] {
            rusqlite::types::Value::Text(name) => name.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect()
}

#[test]
fn registry_matches_the_typescript_manifest() {
    assert_eq!(MIGRATIONS.len(), 54);
    for (index, (id, _, _)) in MIGRATIONS.iter().enumerate() {
        assert_eq!(*id, index as i64 + 1);
    }
    assert_eq!(MIGRATIONS.last().unwrap().0, LATEST_MIGRATION_ID);
    assert_eq!(MIGRATIONS[49].1, "ProjectionThreadPullRequests");
}

#[test]
fn runs_everything_once_then_nothing() {
    let conn = memory();
    let first = migrations::run(&conn).unwrap();
    assert_eq!(first.ran.len(), 54);
    assert_eq!(first.ran[0], (1, "OrchestrationEvents".to_string()));
    assert_eq!(first.previous_latest, 0);
    let second = migrations::run(&conn).unwrap();
    assert!(second.ran.is_empty());
    assert_eq!(second.previous_latest, 54);
    let recorded = rows::<String>(&conn, "SELECT migration_id || '_' || name FROM effect_sql_migrations ORDER BY migration_id");
    assert_eq!(recorded.len(), 54);
    assert_eq!(recorded[53][0], "54_ProjectionThreadsAutoSettleDisabledAt");
}

#[test]
fn runs_in_steps_like_to_migration_inclusive() {
    let conn = memory();
    let first = run_through(&conn, Some(10)).unwrap();
    assert_eq!(first.ran.len(), 10);
    let second = run_through(&conn, Some(10)).unwrap();
    assert!(second.ran.is_empty());
    let rest = migrations::run(&conn).unwrap();
    assert_eq!(rest.ran.first().map(|(id, _)| *id), Some(11));
    assert_eq!(rest.ran.len(), 44);
}

#[test]
fn refuses_a_database_from_a_newer_server() {
    let conn = memory();
    migrations::run(&conn).unwrap();
    exec(&conn, "INSERT INTO effect_sql_migrations (migration_id, name) VALUES (55, 'FromTheFuture')");
    let error = migrations::run(&conn).unwrap_err();
    match error {
        DbError::Migration { kind, .. } => assert_eq!(kind, MigrationErrorKind::NewerSchema),
        other => panic!("expected NewerSchema, got {other:?}"),
    }
    // Nothing was left open.
    assert!(conn.raw().is_autocommit());

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    drop(Db::open(&path).unwrap());
    {
        let raw = rusqlite::Connection::open(&path).unwrap();
        raw.execute("INSERT INTO effect_sql_migrations (migration_id, name) VALUES (60, 'Newer')", [])
            .unwrap();
    }
    assert!(matches!(
        Db::open(&path),
        Err(DbError::Migration {
            kind: MigrationErrorKind::NewerSchema,
            ..
        })
    ));
}

#[test]
fn a_failing_migration_rolls_everything_back() {
    let conn = memory();
    run_through(&conn, Some(51)).unwrap();
    // 052 adds title_state_json unguarded: pre-adding the column makes it fail.
    exec(&conn, "ALTER TABLE projection_threads ADD COLUMN title_state_json TEXT");
    let error = migrations::run(&conn).unwrap_err();
    match &error {
        DbError::Migration { kind, message, .. } => {
            assert_eq!(*kind, MigrationErrorKind::Failed);
            assert_eq!(message, "Migration \"52_ProjectionThreadTitleState\" failed");
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    // The bookkeeping rows of 52..54 were rolled back with the transaction.
    assert_eq!(migrations::latest_migration_id(&conn).unwrap(), 51);
    assert!(column_names(&conn, "pull_request_files_viewed").is_empty());
    assert!(conn.raw().is_autocommit());
}

#[test]
fn a_conflicting_bookkeeping_insert_means_locked() {
    // When another process is migrating, inserting the pending rows hits its rows: Effect's
    // "Locked". Nothing runs, the transaction is rolled back, and opening carries on. The
    // conflict is simulated with a trigger that fails the insert with SQLITE_CONSTRAINT.
    let conn = memory();
    run_through(&conn, Some(52)).unwrap();
    exec(
        &conn,
        "CREATE TEMP TRIGGER lock_probe BEFORE INSERT ON effect_sql_migrations \
         WHEN NEW.migration_id = 53 BEGIN SELECT RAISE(ABORT, 'constraint failed'); END",
    );
    let outcome = migrations::run(&conn).unwrap();
    assert!(outcome.locked);
    assert!(outcome.ran.is_empty());
    assert_eq!(migrations::latest_migration_id(&conn).unwrap(), 52);
    assert!(column_names(&conn, "pull_request_files_viewed").is_empty());
    assert!(conn.raw().is_autocommit());
}

#[test]
fn migration_027_028_continue_after_a_partial_migration() {
    let conn = memory();
    run_through(&conn, Some(26)).unwrap();
    exec(&conn, "ALTER TABLE provider_session_runtime ADD COLUMN provider_instance_id TEXT");
    run_through(&conn, Some(28)).unwrap();
    let recorded = rows::<rusqlite::types::Value>(
        &conn,
        "SELECT migration_id, name FROM effect_sql_migrations WHERE migration_id IN (27, 28) ORDER BY migration_id",
    );
    assert_eq!(recorded.len(), 2);
    assert!(column_names(&conn, "provider_session_runtime").contains(&"provider_instance_id".into()));
    assert!(column_names(&conn, "projection_thread_sessions").contains(&"provider_instance_id".into()));
    assert!(index_names(&conn, "provider_session_runtime").contains(&"idx_provider_session_runtime_instance".into()));
    assert!(index_names(&conn, "projection_thread_sessions").contains(&"idx_projection_thread_sessions_instance".into()));
}

#[test]
fn migration_051_accepts_a_context_column_from_a_development_migration() {
    let conn = memory();
    run_through(&conn, Some(50)).unwrap();
    exec(&conn, "ALTER TABLE projection_thread_messages ADD COLUMN context_json TEXT");
    run_through(&conn, Some(51)).unwrap();
    assert!(column_names(&conn, "projection_thread_messages").contains(&"context_json".into()));
    assert_eq!(migrations::latest_migration_id(&conn).unwrap(), 51);
}

#[test]
fn migration_024_backfills_shell_summary_and_clears_stale_approvals() {
    let conn = memory();
    run_through(&conn, Some(23)).unwrap();
    exec(
        &conn,
        r#"
        INSERT INTO projection_threads (
          thread_id, project_id, title, model_selection_json, runtime_mode, interaction_mode,
          branch, worktree_path, latest_turn_id, created_at, updated_at, archived_at,
          latest_user_message_at, pending_approval_count, pending_user_input_count,
          has_actionable_proposed_plan, deleted_at
        ) VALUES (
          'thread-1', 'project-1', 'Thread 1', '{"provider":"codex","model":"gpt-5-codex"}',
          'approval-required', 'plan', NULL, NULL, 'turn-1', '2026-02-24T00:00:00.000Z',
          '2026-02-24T00:00:00.000Z', NULL, NULL, 0, 0, 0, NULL
        );
        INSERT INTO projection_thread_messages (
          message_id, thread_id, turn_id, role, text, attachments_json, is_streaming, created_at, updated_at
        ) VALUES (
          'message-user-1', 'thread-1', 'turn-1', 'user', 'Need help', NULL, 0,
          '2026-02-24T00:01:00.000Z', '2026-02-24T00:01:00.000Z'
        );
        INSERT INTO projection_thread_activities (
          activity_id, thread_id, turn_id, tone, kind, summary, payload_json, sequence, created_at
        ) VALUES
          ('activity-approval-requested', 'thread-1', 'turn-1', 'approval', 'approval.requested',
            'Command approval requested', '{"requestId":"approval-1","requestKind":"command"}', NULL,
            '2026-02-24T00:02:00.000Z'),
          ('activity-approval-stale', 'thread-1', 'turn-1', 'error', 'provider.approval.respond.failed',
            'Provider approval response failed',
            '{"requestId":"approval-1","detail":"Unknown pending permission request: approval-1"}', NULL,
            '2026-02-24T00:03:00.000Z'),
          ('activity-user-input-requested', 'thread-1', 'turn-1', 'info', 'user-input.requested',
            'User input requested', '{"requestId":"input-1","questions":[]}', NULL,
            '2026-02-24T00:04:00.000Z');
        INSERT INTO projection_thread_proposed_plans (
          plan_id, thread_id, turn_id, plan_markdown, implemented_at, implementation_thread_id,
          created_at, updated_at
        ) VALUES (
          'plan-1', 'thread-1', 'turn-1', '# Do the thing', NULL, NULL,
          '2026-02-24T00:05:00.000Z', '2026-02-24T00:05:00.000Z'
        );
        INSERT INTO projection_pending_approvals (
          request_id, thread_id, turn_id, status, decision, created_at, resolved_at
        ) VALUES (
          'approval-1', 'thread-1', 'turn-1', 'pending', NULL, '2026-02-24T00:02:00.000Z', NULL
        );
        "#,
    );
    run_through(&conn, Some(24)).unwrap();
    let thread = rows::<rusqlite::types::Value>(
        &conn,
        "SELECT latest_user_message_at, pending_approval_count, pending_user_input_count, has_actionable_proposed_plan FROM projection_threads WHERE thread_id = 'thread-1'",
    );
    use rusqlite::types::Value::{Integer, Text};
    assert_eq!(thread, vec![vec![Text("2026-02-24T00:01:00.000Z".into()), Integer(0), Integer(1), Integer(1)]]);
    let approval = rows::<rusqlite::types::Value>(
        &conn,
        "SELECT status, resolved_at FROM projection_pending_approvals WHERE request_id = 'approval-1'",
    );
    assert_eq!(approval, vec![vec![Text("resolved".into()), Text("2026-02-24T00:03:00.000Z".into())]]);
}

#[test]
fn migration_046_repairs_automatic_settlement_stamps_only() {
    let conn = memory();
    run_through(&conn, Some(45)).unwrap();
    let model = r#"{"instanceId":"codex","model":"gpt-5.6-sol"}"#;
    let threads = [
        (
            "thread-auto",
            "Automatic",
            Some("turn-auto"),
            "2026-05-01T00:00:00.000Z",
            "2026-09-01T00:00:00.000Z",
            Some("2026-06-01T00:00:00.000Z"),
            "2026-09-01T00:00:00.000Z",
        ),
        (
            "thread-auto-no-activity",
            "Automatic without activity",
            None,
            "2026-05-02T00:00:00.000Z",
            "2026-09-01T00:00:00.000Z",
            None,
            "2026-09-01T00:00:00.000Z",
        ),
        (
            "thread-auto-later-activity",
            "Automatic then active",
            None,
            "2026-05-03T00:00:00.000Z",
            "2026-09-03T00:00:00.000Z",
            Some("2026-09-03T00:00:00.000Z"),
            "2026-09-01T00:00:00.000Z",
        ),
        (
            "thread-manual",
            "Manual",
            None,
            "2026-05-01T00:00:00.000Z",
            "2026-08-10T00:00:00.000Z",
            Some("2026-06-10T00:00:00.000Z"),
            "2026-08-10T00:00:00.000Z",
        ),
        (
            "thread-resettled",
            "Manually re-settled",
            None,
            "2026-05-01T00:00:00.000Z",
            "2026-09-02T00:00:00.000Z",
            Some("2026-06-05T00:00:00.000Z"),
            "2026-09-02T00:00:00.000Z",
        ),
    ];
    for (id, title, turn, created, updated, latest_user, settled) in threads {
        conn.execute(
            "INSERT INTO projection_threads (thread_id, project_id, title, model_selection_json, latest_turn_id, created_at, updated_at, latest_user_message_at, settled_override, settled_at, deleted_at) VALUES (?1, 'project-1', ?2, ?3, ?4, ?5, ?6, ?7, 'settled', ?8, NULL)",
            params![id, title, model, turn, created, updated, latest_user, settled],
        )
        .unwrap();
    }
    exec(
        &conn,
        r#"
        INSERT INTO projection_thread_messages (message_id, thread_id, turn_id, role, text, is_streaming, created_at, updated_at) VALUES
          ('message-auto', 'thread-auto', 'turn-auto', 'user', 'Prompt', 0, '2026-06-01T00:00:00.000Z', '2026-06-01T00:00:00.000Z'),
          ('message-later-old', 'thread-auto-later-activity', NULL, 'user', 'Prompt', 0, '2026-06-20T00:00:00.000Z', '2026-06-20T00:00:00.000Z'),
          ('message-later-new', 'thread-auto-later-activity', NULL, 'user', 'Prompt', 0, '2026-09-03T00:00:00.000Z', '2026-09-03T00:00:00.000Z'),
          ('message-manual', 'thread-manual', NULL, 'user', 'Prompt', 0, '2026-06-10T00:00:00.000Z', '2026-06-10T00:00:00.000Z'),
          ('message-resettled', 'thread-resettled', NULL, 'user', 'Prompt', 0, '2026-06-05T00:00:00.000Z', '2026-06-05T00:00:00.000Z');
        INSERT INTO projection_turns (thread_id, turn_id, state, requested_at, started_at, completed_at, checkpoint_files_json)
        VALUES ('thread-auto', 'turn-auto', 'completed', '2026-06-02T00:00:00.000Z', '2026-06-02T00:01:00.000Z', '2026-06-03T00:00:00.000Z', '[]');
        "#,
    );
    let swept = "2026-09-01T00:00:00.000Z";
    let automatic = |thread: &str| format!("server:auto-settle:{thread}:uuid");
    let events: Vec<(&str, &str, i64, &str, String, &str)> = vec![
        ("event-auto", "thread-auto", 0, swept, automatic("thread-auto"), swept),
        (
            "event-auto-repeat",
            "thread-auto",
            1,
            "2026-09-01T00:00:05.000Z",
            "command-repeat".into(),
            swept,
        ),
        (
            "event-auto-no-activity",
            "thread-auto-no-activity",
            0,
            swept,
            automatic("thread-auto-no-activity"),
            swept,
        ),
        (
            "event-auto-later-activity",
            "thread-auto-later-activity",
            0,
            swept,
            automatic("thread-auto-later-activity"),
            swept,
        ),
        (
            "event-manual",
            "thread-manual",
            0,
            "2026-08-10T00:00:00.000Z",
            "command-manual".into(),
            "2026-08-10T00:00:00.000Z",
        ),
        ("event-resettled-auto", "thread-resettled", 0, swept, automatic("thread-resettled"), swept),
        (
            "event-resettled-manual",
            "thread-resettled",
            1,
            "2026-09-02T00:00:00.000Z",
            "command-resettled-manual".into(),
            "2026-09-02T00:00:00.000Z",
        ),
    ];
    for (event_id, thread_id, version, occurred, command, settled_at) in &events {
        let actor = if command.starts_with("server:") { "server" } else { "client" };
        let payload = json!({ "threadId": thread_id, "settledAt": settled_at, "updatedAt": occurred }).to_string();
        conn.execute(
            "INSERT INTO orchestration_events (event_id, aggregate_kind, stream_id, stream_version, event_type, occurred_at, command_id, causation_event_id, correlation_id, actor_kind, payload_json, metadata_json) VALUES (?1, 'thread', ?2, ?3, 'thread.settled', ?4, ?5, NULL, ?5, ?6, ?7, '{}')",
            params![event_id, thread_id, version, occurred, command, actor, payload],
        )
        .unwrap();
    }
    let before = rows::<String>(&conn, "SELECT payload_json FROM orchestration_events ORDER BY event_id");
    run_through(&conn, Some(46)).unwrap();
    let threads = rows::<String>(&conn, "SELECT thread_id, settled_at, updated_at FROM projection_threads ORDER BY thread_id");
    let expect = |a: &str, b: &str, c: &str| vec![a.to_string(), b.to_string(), c.to_string()];
    assert_eq!(
        threads,
        vec![
            expect("thread-auto", "2026-06-03T00:00:00.000Z", "2026-09-01T00:00:00.000Z"),
            expect("thread-auto-later-activity", "2026-06-20T00:00:00.000Z", "2026-09-03T00:00:00.000Z"),
            expect("thread-auto-no-activity", "2026-05-02T00:00:00.000Z", "2026-09-01T00:00:00.000Z"),
            expect("thread-manual", "2026-08-10T00:00:00.000Z", "2026-08-10T00:00:00.000Z"),
            expect("thread-resettled", "2026-09-02T00:00:00.000Z", "2026-09-02T00:00:00.000Z"),
        ]
    );
    let after = rows::<String>(&conn, "SELECT payload_json FROM orchestration_events ORDER BY event_id");
    assert_eq!(before, after);
}

#[test]
fn migration_050_backfills_legacy_single_links() {
    let conn = memory();
    run_through(&conn, Some(49)).unwrap();
    exec(
        &conn,
        r#"
        INSERT INTO projection_projects (project_id, title, workspace_root, scripts_json, created_at, updated_at, deleted_at)
        VALUES ('project-1', 'Project 1', '/tmp/project-1', '[]', '2026-03-01T00:00:00.000Z', '2026-03-01T00:00:00.000Z', NULL);
        INSERT INTO projection_threads (thread_id, project_id, title, model_selection_json, linked_pull_request_json, created_at, updated_at) VALUES
          ('thread-github', 'project-1', 'GitHub link', '{"instanceId":"codex","model":"gpt-5.4"}',
            '{"projectId":"project-1","repository":"PingDotGG/T3Code","number":42,"url":"https://GitHub.com/pingdotgg/t3code/pull/42"}',
            '2026-03-01T00:00:01.000Z', '2026-03-02T00:00:00.000Z'),
          ('thread-bad-url', 'project-1', 'Unparseable URL', '{"instanceId":"codex","model":"gpt-5.4"}',
            '{"projectId":"project-1","repository":"acme/widgets","number":7,"url":"not a url"}',
            '2026-03-01T00:00:02.000Z', '2026-03-03T00:00:00.000Z'),
          ('thread-malformed', 'project-1', 'Malformed JSON', '{"instanceId":"codex","model":"gpt-5.4"}',
            '{"repository":"acme/widgets"}', '2026-03-01T00:00:03.000Z', '2026-03-04T00:00:00.000Z'),
          ('thread-unlinked', 'project-1', 'No link', '{"instanceId":"codex","model":"gpt-5.4"}',
            NULL, '2026-03-01T00:00:04.000Z', '2026-03-05T00:00:00.000Z');
        "#,
    );
    run_through(&conn, Some(50)).unwrap();
    let links = rows::<rusqlite::types::Value>(
        &conn,
        "SELECT thread_id, host, repository, number, url, source, linked_at, snapshot_json, stack_json FROM projection_thread_pull_requests ORDER BY thread_id ASC",
    );
    use rusqlite::types::Value::{Integer, Null, Text};
    let t = |s: &str| Text(s.to_string());
    assert_eq!(
        links,
        vec![
            vec![
                t("thread-bad-url"),
                t("unknown"),
                t("acme/widgets"),
                Integer(7),
                t("not a url"),
                t("manual"),
                t("2026-03-03T00:00:00.000Z"),
                Null,
                Null
            ],
            vec![
                t("thread-github"),
                t("github.com"),
                t("pingdotgg/t3code"),
                Integer(42),
                t("https://GitHub.com/pingdotgg/t3code/pull/42"),
                t("manual"),
                t("2026-03-02T00:00:00.000Z"),
                Null,
                Null
            ],
        ]
    );
    // The legacy column stays so a rollback keeps its data.
    assert!(column_names(&conn, "projection_threads").contains(&"linked_pull_request_json".into()));
    assert!(index_names(&conn, "projection_thread_pull_requests").contains(&"idx_projection_thread_pull_requests_pr".into()));
}

#[test]
fn migration_050_keeps_legacy_azure_repositories_distinct_across_organizations() {
    let conn = memory();
    run_through(&conn, Some(49)).unwrap();
    for organization in ["org-a", "org-b"] {
        let linked = json!({
            "projectId": organization,
            "repository": "web",
            "number": 7,
            "url": format!("https://dev.azure.com/{organization}/project/_git/web/pullrequest/7"),
        })
        .to_string();
        conn.execute(
            "INSERT INTO projection_threads (thread_id, project_id, title, model_selection_json, linked_pull_request_json, created_at, updated_at) VALUES (?1, ?1, 'Azure', '{\"instanceId\":\"codex\",\"model\":\"gpt-5.4\"}', ?2, '2026-03-01T00:00:00.000Z', '2026-03-01T00:00:00.000Z')",
            params![organization, linked],
        )
        .unwrap();
    }
    run_through(&conn, Some(50)).unwrap();
    let links = rows::<rusqlite::types::Value>(
        &conn,
        "SELECT host, repository, number FROM projection_thread_pull_requests ORDER BY repository",
    );
    use rusqlite::types::Value::{Integer, Text};
    assert_eq!(
        links,
        vec![
            vec![Text("dev.azure.com".into()), Text("org-a/project/_git/web".into()), Integer(7)],
            vec![Text("dev.azure.com".into()), Text("org-b/project/_git/web".into()), Integer(7)],
        ]
    );
}

#[test]
fn migration_054_adds_auto_settle_disabled_at_left_off() {
    let conn = memory();
    run_through(&conn, Some(53)).unwrap();
    exec(
        &conn,
        "INSERT INTO projection_threads (thread_id, project_id, title, model_selection_json, created_at, updated_at) VALUES ('t', 'p', 'T', '{}', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
    );
    run_through(&conn, Some(54)).unwrap();
    let values = rows::<Option<String>>(&conn, "SELECT auto_settle_disabled_at FROM projection_threads");
    assert_eq!(values, vec![vec![None]]);
}

#[test]
fn opening_twice_is_a_no_op_and_db_open_migrates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested/dir/state.sqlite");
    let db = Db::open_with(&path, DbOptions { migrate: true, readers: 0 }).unwrap();
    assert_eq!(db.migrations().ran.len(), 54);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert!(db.migrations().ran.is_empty());
    assert_eq!(db.migrations().previous_latest, 54);
}
