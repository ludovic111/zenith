//! Port of `orchestration/Layers/OrchestrationEngine.test.ts`.
//!
//! The TS tests read the SQL projection (`ProjectionSnapshotQuery.getSnapshot()` /
//! `getThreadDetailById`), which WP-09 ports; here the same facts are asserted on the engine's
//! command read model (`command_read_model()`, the event log folded through the projector) or on
//! the stored events. Where the TS test stubs the event store, a SQLite trigger, a failing
//! [`ProjectionPipeline`] or a second writer on the same database stands in for the stub; each
//! adaptation is explained above its test.
//!
//! Not ported: "records command ack duration using the first committed event type" and
//! "records failed command dispatches as metric failures" assert on Effect `Metric` snapshots,
//! which the Rust engine does not keep.

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::*;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{ApprovalRequestId, OrchestrationEvent, OrchestrationReadModel, OrchestrationThreadActivity, ProjectId, RepositoryIdentity, ThreadId};
use zc_db::repos::command_receipts;
use zc_db::{Conn, Db, DbError, SqlErrorKind};
use zc_orchestration::decider::{DeciderEnv, SystemEnv};
use zc_orchestration::engine::{BackgroundLiveness, EngineConfig, EngineReads, EventLogReads, NoBackgroundLiveness, OrchestrationEngine};
use zc_orchestration::errors::OrchestrationDispatchError;
use zc_orchestration::event::OrchestrationEventExt;
use zc_orchestration::pipeline::{DeferredCleanup, NoopProjectionPipeline, ProjectionPipeline};
use zc_ports::{DispatchResult, TaggedError};

/// `now()` in the TS test.
const NOW: &str = "2026-01-01T00:00:00.000Z";

// ---------------------------------------------------------------------------------------------
// Engine set-up

/// A standalone engine config with the decider clock pinned to [`NOW`].
fn config(db: &Db) -> EngineConfig {
    config_with_env(db, TestEnv::at(NOW))
}

fn config_with_env(db: &Db, env: impl DeciderEnv + 'static) -> EngineConfig {
    EngineConfig {
        db: db.clone(),
        reads: Arc::new(EventLogReads::new(db.clone())),
        pipeline: Arc::new(NoopProjectionPipeline),
        liveness: Arc::new(NoBackgroundLiveness),
        env: Arc::new(env),
    }
}

async fn start(config: EngineConfig) -> OrchestrationEngine {
    OrchestrationEngine::start(config).await.expect("start the engine")
}

/// `createOrchestrationSystem()`: an in-memory database and a standalone engine.
async fn system() -> (Db, OrchestrationEngine) {
    let db = Db::open_in_memory().unwrap();
    let engine = start(config(&db)).await;
    (db, engine)
}

async fn dispatch(engine: &OrchestrationEngine, command_value: Value) -> Result<DispatchResult, OrchestrationDispatchError> {
    engine.dispatch(command(command_value), None).await
}

/// Dispatches a command that must be accepted; returns its sequence.
async fn ok(engine: &OrchestrationEngine, command_value: Value) -> i64 {
    let text = command_value.to_string();
    match dispatch(engine, command_value).await {
        Ok(result) => result.sequence,
        Err(error) => panic!("dispatch of {text} failed: [{}] {error}", error.tag()),
    }
}

/// Dispatches a command that must fail; returns the error.
async fn fails(engine: &OrchestrationEngine, command_value: Value) -> OrchestrationDispatchError {
    let text = command_value.to_string();
    match dispatch(engine, command_value).await {
        Ok(result) => panic!("dispatch of {text} succeeded with sequence {}", result.sequence),
        Err(error) => error,
    }
}

/// The command read model as wire JSON (stands in for `readModel()`).
async fn model(engine: &OrchestrationEngine) -> Value {
    to_json(&engine.command_read_model().await.unwrap())
}

fn thread<'a>(model: &'a Value, thread_id: &str) -> &'a Value {
    model["threads"]
        .as_array()
        .unwrap()
        .iter()
        .find(|thread| thread["id"] == thread_id)
        .unwrap_or_else(|| panic!("no thread {thread_id} in the model"))
}

/// `Stream.runCollect(engine.readEvents(0))`, as wire JSON.
async fn all_events(engine: &OrchestrationEngine) -> Vec<Value> {
    engine.read_events(0, None).map(|event| to_json(&event.expect("read an event"))).collect().await
}

fn types(events: &[Value]) -> Vec<&str> {
    events.iter().map(|event| event["type"].as_str().unwrap()).collect()
}

fn null_or_absent(value: &Value) -> bool {
    value.is_null()
}

// ---------------------------------------------------------------------------------------------
// Command literals

fn project_create(command_id: &str, project_id: &str, title: &str, workspace_root: &str) -> Value {
    json!({
        "type": "project.create",
        "commandId": command_id,
        "projectId": project_id,
        "title": title,
        "workspaceRoot": workspace_root,
        "defaultModelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
        "createdAt": NOW,
    })
}

fn thread_create(command_id: &str, thread_id: &str, project_id: &str, title: &str, runtime_mode: &str) -> Value {
    json!({
        "type": "thread.create",
        "commandId": command_id,
        "threadId": thread_id,
        "projectId": project_id,
        "title": title,
        "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
        "interactionMode": "default",
        "runtimeMode": runtime_mode,
        "branch": null,
        "worktreePath": null,
        "createdAt": NOW,
    })
}

fn turn_start(command_id: &str, thread_id: &str, message_id: &str, text: &str) -> Value {
    json!({
        "type": "thread.turn.start",
        "commandId": command_id,
        "threadId": thread_id,
        "message": {"messageId": message_id, "role": "user", "text": text, "attachments": []},
        "interactionMode": "default",
        "runtimeMode": "approval-required",
        "createdAt": NOW,
    })
}

fn user_messages(thread: &Value) -> Vec<&Value> {
    thread["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| message["role"] == "user")
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Test doubles

/// A [`ProjectionPipeline`] that fails (once) on the first event matching a predicate, with the
/// `PersistenceSqlError` the TS stub returns.
struct FailingPipeline {
    armed: AtomicBool,
    matches: fn(&OrchestrationEvent) -> bool,
}

impl FailingPipeline {
    fn new(matches: fn(&OrchestrationEvent) -> bool) -> Self {
        Self {
            armed: AtomicBool::new(true),
            matches,
        }
    }
}

#[async_trait]
impl ProjectionPipeline for FailingPipeline {
    async fn bootstrap(&self) -> Result<(), DbError> {
        Ok(())
    }

    fn project_event_deferred(&self, _conn: &Conn, event: &OrchestrationEvent) -> Result<DeferredCleanup, DbError> {
        if (self.matches)(event) && self.armed.swap(false, Ordering::SeqCst) {
            return Err(DbError::Sql {
                operation: "test.projection".into(),
                detail: Some("projection failed".into()),
                kind: SqlErrorKind::Unknown,
                correlation: None,
                cause: None,
            });
        }
        Ok(DeferredCleanup::none())
    }
}

fn command_id_is(event: &OrchestrationEvent, command_id: &str) -> bool {
    event.command_id().is_some_and(|id| id.as_str() == command_id)
}

/// `ThreadBackgroundLiveness` with threads marked live by hand
/// (`recordTaskLiveness` / `clearThreadLiveness`).
#[derive(Default)]
struct SettableLiveness(Mutex<HashSet<String>>);

impl SettableLiveness {
    fn set_live(&self, thread_id: &str) {
        self.0.lock().unwrap().insert(thread_id.to_owned());
    }
    fn clear(&self, thread_id: &str) {
        self.0.lock().unwrap().remove(thread_id);
    }
}

impl BackgroundLiveness for SettableLiveness {
    fn has_live_background_work(&self, thread_id: &ThreadId) -> bool {
        self.0.lock().unwrap().contains(thread_id.as_str())
    }
}

/// The event log as the command model, with one resolved repository identity for every project
/// (the TS test's stub `RepositoryIdentityResolver`, seen through `getProjectShellById`).
struct IdentityReads {
    log: EventLogReads,
    identity: RepositoryIdentity,
}

#[async_trait]
impl EngineReads for IdentityReads {
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
        self.log.get_command_read_model().await
    }
    async fn get_project_repository_identity(&self, _project_id: &ProjectId) -> Result<Option<Option<RepositoryIdentity>>, TaggedError> {
        Ok(Some(Some(self.identity.clone())))
    }
    async fn get_user_input_activity(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
        self.log.get_user_input_activity(thread_id, request_id).await
    }
}

/// A fixed bootstrap command model (the TS stub `ProjectionSnapshotQuery`), counting reads.
struct BootstrapReads {
    model: OrchestrationReadModel,
    command_model_reads: AtomicUsize,
}

#[async_trait]
impl EngineReads for BootstrapReads {
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
        self.command_model_reads.fetch_add(1, Ordering::SeqCst);
        Ok(self.model.clone())
    }
    async fn get_project_repository_identity(&self, _project_id: &ProjectId) -> Result<Option<Option<RepositoryIdentity>>, TaggedError> {
        Ok(None)
    }
    async fn get_user_input_activity(
        &self,
        _thread_id: &ThreadId,
        _request_id: &ApprovalRequestId,
    ) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
        panic!("unused")
    }
}

// ---------------------------------------------------------------------------------------------
// "sends async answers with a %s session and rejects old duplicate replies"
//
// Adapted: `readModel()` / `readThread()` become the command read model, which applies the same
// activity retention (500 most recent + pending questions). "Disposing" the system drops the
// engine and reopens the database file, so the restarted engine rebuilds its command model from
// the event log and finds the request through `EventLogReads::get_user_input_activity`. The
// TS test runs on the real clock with random event ids; so does this one (`SystemEnv`: a
// restarted `TestEnv` would number its event ids from 1 again and collide).

async fn open_file_engine(path: &Path) -> OrchestrationEngine {
    let db = Db::open(path).unwrap();
    start(config_with_env(&db, SystemEnv)).await
}

async fn append_work(engine: &OrchestrationEngine, thread_id: &str, prefix: &str, created_at: &str) {
    for index in 0..501 {
        ok(
            engine,
            json!({
                "type": "thread.activity.append",
                "commandId": format!("{prefix}-{index}"),
                "threadId": thread_id,
                "createdAt": created_at,
                "activity": {
                    "id": format!("{prefix}-{index}"),
                    "kind": "tool.completed",
                    "summary": "Work continued",
                    "payload": {},
                    "tone": "info",
                    "turnId": "turn-1",
                    "createdAt": created_at,
                },
            }),
        )
        .await;
    }
}

async fn async_answers_case(status: &str) {
    let directory = tempfile::tempdir().unwrap();
    let database_path = directory.path().join("state.sqlite");
    let mut engine = open_file_engine(&database_path).await;
    let thread_id = "async-thread";
    let project_id = "async-project";
    let request_id = "codex-async:question-1";

    ok(
        &engine,
        json!({
            "type": "project.create",
            "commandId": "async-project",
            "projectId": project_id,
            "title": "Async questions",
            "workspaceRoot": "/tmp/async-questions",
            "createdAt": NOW,
        }),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "thread.create",
            "commandId": "async-thread",
            "threadId": thread_id,
            "projectId": project_id,
            "title": "Async questions",
            "modelSelection": {"instanceId": "codex", "model": "gpt-5.4"},
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "branch": null,
            "worktreePath": null,
            "createdAt": NOW,
        }),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "thread.session.set",
            "commandId": "async-session",
            "threadId": thread_id,
            "createdAt": NOW,
            "session": {
                "threadId": thread_id,
                "status": status,
                "providerName": "codex",
                "runtimeMode": "full-access",
                "activeTurnId": if status == "running" { json!("turn-1") } else { Value::Null },
                "lastError": null,
                "updatedAt": NOW,
            },
        }),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "thread.activity.append",
            "commandId": "async-question",
            "threadId": thread_id,
            "createdAt": NOW,
            "activity": {
                "id": "async-question",
                "kind": "user-input.requested",
                "summary": "User input requested",
                "tone": "info",
                "turnId": "turn-1",
                "createdAt": NOW,
                "payload": {
                    "requestId": request_id,
                    "responseMode": "message",
                    "questions": [
                        {
                            "id": "0",
                            "header": "Question",
                            "question": "Which package manager?",
                            "options": [{"label": "pnpm", "description": ""}],
                        },
                        {
                            "id": "1",
                            "header": "Question",
                            "question": "What should it be named?",
                            "options": [],
                        },
                    ],
                },
            },
        }),
    )
    .await;
    append_work(&engine, thread_id, "work", "2026-01-01T00:00:01.000Z").await;
    let before = model(&engine).await;
    assert!(
        before["threads"][0]["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|activity| activity["id"] == "async-question"),
        "the pending question survives the activity cap"
    );
    if status == "stopped" {
        drop(engine);
        engine = open_file_engine(&database_path).await;
    }

    let attachments = json!([{
        "type": "file",
        "id": "thread-1-00000000-0000-4000-8000-0000000000aa-txt",
        "name": "spec.txt",
        "mimeType": "text/plain",
        "sizeBytes": 4,
    }]);
    let answers = json!({"0": "pnpm", "1": "Example"});
    let response = |command_id: &str, answers: &Value| {
        json!({
            "type": "thread.user-input.respond",
            "commandId": command_id,
            "threadId": thread_id,
            "requestId": request_id,
            "answers": answers,
            "attachmentsByQuestionId": {"1": attachments.clone()},
            "createdAt": "2026-01-01T00:00:02.000Z",
        })
    };

    let incomplete = fails(&engine, response("incomplete-answer", &json!({"0": "pnpm"}))).await;
    assert!(incomplete.to_string().contains("Answer each question before sending."), "{incomplete}");
    ok(&engine, response("async-response", &answers)).await;

    let after = model(&engine).await;
    let thread_after = thread(&after, thread_id);
    let messages = user_messages(thread_after);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0]["attachments"], attachments);
    assert_eq!(
        messages[0]["text"],
        "Which package manager?\npnpm\n\nWhat should it be named?\nExample\nAttached file: spec.txt (thread-1-00000000-0000-4000-8000-0000000000aa-txt)"
    );
    let resolved = thread_after["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|activity| activity["kind"] == "user-input.resolved")
        .expect("a resolved activity");
    // `toMatchObject({requestId, responseMode, answers})`.
    assert_eq!(resolved["payload"]["requestId"], request_id);
    assert_eq!(resolved["payload"]["responseMode"], "message");
    assert_eq!(resolved["payload"]["answers"], answers);

    let events = all_events(&engine).await;
    let response_types: Vec<&str> = events
        .iter()
        .filter(|event| event["commandId"] == "async-response")
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        response_types,
        ["thread.activity-appended", "thread.message-sent", "thread.turn-start-requested"]
    );

    let duplicate = fails(&engine, response("second-client-reply", &answers)).await;
    assert!(duplicate.to_string().contains("This question has already been answered."), "{duplicate}");

    append_work(&engine, thread_id, "later-work", "2026-01-01T00:00:03.000Z").await;
    let after_eviction = model(&engine).await;
    assert!(
        !thread(&after_eviction, thread_id)["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|activity| activity["kind"] == "user-input.resolved"),
        "the resolved activity is evicted from the capped thread"
    );
    if status == "stopped" {
        drop(engine);
        engine = open_file_engine(&database_path).await;
    }
    let after_eviction_error = fails(&engine, response("reply-after-eviction", &answers)).await;
    assert!(
        after_eviction_error.to_string().contains("This question has already been answered."),
        "{after_eviction_error}"
    );
    drop(engine);
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_async_answers_with_a_running_session_and_rejects_old_duplicate_replies() {
    async_answers_case("running").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sends_async_answers_with_a_stopped_session_and_rejects_old_duplicate_replies() {
    async_answers_case("stopped").await;
}

// ---------------------------------------------------------------------------------------------
// "bootstraps command handling from persisted projections without reading the full snapshot"
//
// Adapted: the TS stub event store numbers appends from 8 and fails `readAll`; here the engine
// gets a fixed command model (sequence 7) from its `EngineReads` and never sees the event log
// (`EventLogReads` is not configured, so the log cannot be replayed), and the real store is
// seeded so its next sequence is 8. "No full snapshot read" becomes: the bootstrap model was
// read exactly once, at start.

#[tokio::test]
async fn bootstraps_command_handling_from_persisted_projections_without_reading_the_full_snapshot() {
    let db = Db::open_in_memory().unwrap();
    db.call(|conn| {
        conn.execute_batch("INSERT INTO sqlite_sequence (name, seq) VALUES ('orchestration_events', 7);")
            .map_err(|error| DbError::sql("test.seed", error))
    })
    .await
    .unwrap();
    let bootstrap_model = read_model(json!({
        "snapshotSequence": 7,
        "updatedAt": "2026-03-03T00:00:04.000Z",
        "projects": [{
            "id": "project-bootstrap",
            "title": "Bootstrap Project",
            "workspaceRoot": "/tmp/project-bootstrap",
            "defaultModelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
            "scripts": [],
            "createdAt": "2026-03-03T00:00:00.000Z",
            "updatedAt": "2026-03-03T00:00:01.000Z",
            "deletedAt": null,
        }],
        "threads": [thread_json(
            "thread-bootstrap",
            "project-bootstrap",
            "2026-03-03T00:00:02.000Z",
            json!({
                "title": "Bootstrap Thread",
                "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                "updatedAt": "2026-03-03T00:00:03.000Z",
            }),
        )],
    }));
    let reads = Arc::new(BootstrapReads {
        model: bootstrap_model,
        command_model_reads: AtomicUsize::new(0),
    });
    let engine = start(EngineConfig {
        db: db.clone(),
        reads: reads.clone(),
        pipeline: Arc::new(NoopProjectionPipeline),
        liveness: Arc::new(NoBackgroundLiveness),
        env: Arc::new(TestEnv::now()),
    })
    .await;

    assert_eq!(engine.latest_sequence(), 7);
    let sequence = ok(
        &engine,
        json!({
            "type": "thread.meta.update",
            "commandId": "cmd-bootstrap-thread-update",
            "threadId": "thread-bootstrap",
            "title": "Updated Bootstrap Thread",
        }),
    )
    .await;
    assert_eq!(sequence, 8);
    assert_eq!(engine.latest_sequence(), 8);
    assert_eq!(reads.command_model_reads.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------------------------
// "preserves the blocked-settle error and persists its rejected receipt"

#[tokio::test]
async fn preserves_the_blocked_settle_error_and_persists_its_rejected_receipt() {
    let (db, engine) = system().await;
    let project_id = "project-blocked-settle";
    let thread_id = "thread-blocked-settle";
    let command_id = "cmd-blocked-settle";

    ok(
        &engine,
        project_create("cmd-blocked-settle-project-create", project_id, "Project", "/tmp/project-blocked-settle"),
    )
    .await;
    ok(
        &engine,
        thread_create("cmd-blocked-settle-thread-create", thread_id, project_id, "Thread", "full-access"),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "thread.session.set",
            "commandId": "cmd-blocked-settle-session-set",
            "threadId": thread_id,
            "createdAt": NOW,
            "session": {
                "threadId": thread_id,
                "status": "running",
                "providerName": "codex",
                "runtimeMode": "full-access",
                "activeTurnId": null,
                "lastError": null,
                "updatedAt": NOW,
            },
        }),
    )
    .await;

    let sequence = engine.latest_sequence();
    let error = fails(&engine, json!({"type": "thread.settle", "commandId": command_id, "threadId": thread_id})).await;
    let message = "This thread still needs attention. Resolve or interrupt it first, then try again.";
    assert_eq!(error.tag(), "OrchestrationThreadSettleBlockedError");
    assert_eq!(error.to_string(), message);
    let tagged = to_json(&error.to_tagged());
    assert_eq!(tagged["threadId"], thread_id);

    let receipt = db
        .call(move |conn| command_receipts::get_by_command_id(conn, command_id))
        .await
        .unwrap()
        .expect("a rejected receipt");
    assert_eq!(receipt.command_id, command_id);
    assert_eq!(receipt.aggregate_kind, "thread");
    assert_eq!(receipt.aggregate_id, thread_id);
    assert_eq!(receipt.status, "rejected");
    assert_eq!(receipt.error.as_deref(), Some(message));
    assert_eq!(receipt.result_sequence, sequence);
    assert_eq!(engine.latest_sequence(), sequence);
}

// ---------------------------------------------------------------------------------------------
// "rejects persisted changes and live background work without blocking unrelated threads"
//
// Adapted: `ThreadBackgroundLiveness.recordTaskLiveness` (a `subagent` task → "working", a
// `local_bash` task → "monitoring") becomes a [`BackgroundLiveness`] whose threads are marked
// live by hand; the engine only asks whether a thread has live work. Snapshot reads become the
// command model. The decider clock is pinned to `now()` (`TestClock.setTime`).

#[tokio::test]
async fn rejects_persisted_changes_and_live_background_work_without_blocking_unrelated_threads() {
    let db = Db::open_in_memory().unwrap();
    let liveness = Arc::new(SettableLiveness::default());
    let engine = start(EngineConfig {
        liveness: liveness.clone(),
        ..config(&db)
    })
    .await;
    let project_id = "project-auto-settle-guard";
    let guarded = "thread-auto-settle-guarded";
    let unrelated = "thread-auto-settle-unrelated";
    let live = "thread-auto-settle-live";

    ok(
        &engine,
        project_create("cmd-auto-settle-guard-project", project_id, "Project", "/tmp/project-auto-settle-guard"),
    )
    .await;
    for thread_id in [guarded, unrelated, live] {
        ok(
            &engine,
            thread_create(&format!("cmd-create-{thread_id}"), thread_id, project_id, "Thread", "full-access"),
        )
        .await;
    }

    let before_update = engine.command_read_model().await.unwrap();
    let snapshot_sequence = before_update.snapshot_sequence;
    let original_updated_at = thread(&to_json(&before_update), guarded)["updatedAt"].clone();
    ok(
        &engine,
        json!({
            "type": "thread.meta.update",
            "commandId": "cmd-auto-settle-guard-meta",
            "threadId": guarded,
            "branch": "new-branch",
        }),
    )
    .await;
    let after_update = model(&engine).await;
    assert_eq!(thread(&after_update, guarded)["updatedAt"], original_updated_at);

    // Automatic settlement stamps the last activity, never the sweep time.
    let last_activity_at = "2025-12-20T00:00:00.000Z";
    let auto_settle = |command_id: &str, thread_id: &str, snapshot_sequence: i64| {
        json!({
            "type": "thread.auto-settle",
            "commandId": command_id,
            "threadId": thread_id,
            "snapshotSequence": snapshot_sequence,
            "settledAt": last_activity_at,
        })
    };
    let stale = fails(&engine, auto_settle("cmd-auto-settle-stale-snapshot", guarded, snapshot_sequence)).await;
    assert_eq!(stale.tag(), "OrchestrationCommandInvariantError");

    let liveness_snapshot_sequence = engine.latest_sequence();
    for expected_liveness in ["working", "monitoring"] {
        liveness.set_live(live);
        assert!(liveness.has_live_background_work(&ThreadId::new(live)));
        assert_eq!(engine.latest_sequence(), liveness_snapshot_sequence);

        let error = fails(
            &engine,
            auto_settle(&format!("cmd-auto-settle-{expected_liveness}"), live, liveness_snapshot_sequence),
        )
        .await;
        assert_eq!(error.tag(), "OrchestrationCommandInvariantError");
        assert_eq!(engine.latest_sequence(), liveness_snapshot_sequence);
        liveness.clear(live);
    }

    ok(&engine, auto_settle("cmd-auto-settle-after-liveness-cleared", live, liveness_snapshot_sequence)).await;

    let fresh_snapshot_sequence = engine.latest_sequence();
    ok(
        &engine,
        json!({
            "type": "thread.meta.update",
            "commandId": "cmd-auto-settle-unrelated-meta",
            "threadId": unrelated,
            "title": "Unrelated update",
        }),
    )
    .await;
    ok(&engine, auto_settle("cmd-auto-settle-after-unrelated-update", guarded, fresh_snapshot_sequence)).await;

    let settled = model(&engine).await;
    for thread_id in [guarded, live] {
        let thread = thread(&settled, thread_id);
        assert_eq!(thread["settledOverride"], "settled", "{thread_id}");
        assert_eq!(thread["settledAt"], last_activity_at, "{thread_id}");
        assert_eq!(thread["updatedAt"], NOW, "{thread_id}");
    }
}

// ---------------------------------------------------------------------------------------------
// "persists deterministic read models for repeated snapshot reads"
//
// Adapted: two reads of the command model instead of two SQL snapshots.

#[tokio::test]
async fn persists_deterministic_read_models_for_repeated_snapshot_reads() {
    let (_db, engine) = system().await;
    ok(&engine, project_create("cmd-project-1-create", "project-1", "Project 1", "/tmp/project-1")).await;
    ok(
        &engine,
        thread_create("cmd-thread-1-create", "thread-1", "project-1", "Thread", "approval-required"),
    )
    .await;
    ok(&engine, turn_start("cmd-turn-start-1", "thread-1", "msg-1", "hello")).await;

    let read_model_a = model(&engine).await;
    let read_model_b = model(&engine).await;
    assert_eq!(read_model_b, read_model_a);
}

// ---------------------------------------------------------------------------------------------
// "archives and unarchives threads through orchestration commands"

#[tokio::test]
async fn archives_and_unarchives_threads_through_orchestration_commands() {
    let (_db, engine) = system().await;
    let thread_id = "thread-archive";
    ok(
        &engine,
        project_create("cmd-project-archive-create", "project-archive", "Project Archive", "/tmp/project-archive"),
    )
    .await;
    ok(
        &engine,
        thread_create("cmd-thread-archive-create", thread_id, "project-archive", "Archive me", "full-access"),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "thread.meta.update",
            "commandId": "cmd-thread-archive-title-regeneration",
            "threadId": thread_id,
            "regenerateTitle": true,
        }),
    )
    .await;
    ok(
        &engine,
        json!({"type": "thread.archive", "commandId": "cmd-thread-archive", "threadId": thread_id}),
    )
    .await;
    let archived = model(&engine).await;
    assert!(!thread(&archived, thread_id)["archivedAt"].is_null());
    assert!(null_or_absent(&thread(&archived, thread_id)["titleRegeneration"]));

    ok(
        &engine,
        json!({"type": "thread.unarchive", "commandId": "cmd-thread-unarchive", "threadId": thread_id}),
    )
    .await;
    let unarchived = model(&engine).await;
    assert!(thread(&unarchived, thread_id)["archivedAt"].is_null());
    assert!(null_or_absent(&thread(&unarchived, thread_id)["titleRegeneration"]));

    ok(
        &engine,
        json!({
            "type": "thread.title.regeneration.complete",
            "commandId": "cmd-thread-archive-stale-title-completion",
            "threadId": thread_id,
            "requestId": "cmd-thread-archive-title-regeneration",
            "title": "Stale generated title",
        }),
    )
    .await;
    assert_eq!(thread(&model(&engine).await, thread_id)["title"], "Archive me");
}

// ---------------------------------------------------------------------------------------------
// "replays append-only events from sequence"

#[tokio::test]
async fn replays_append_only_events_from_sequence() {
    let (_db, engine) = system().await;
    ok(
        &engine,
        project_create("cmd-project-replay-create", "project-replay", "Replay Project", "/tmp/project-replay"),
    )
    .await;
    ok(
        &engine,
        thread_create("cmd-thread-replay-create", "thread-replay", "project-replay", "replay", "approval-required"),
    )
    .await;
    ok(
        &engine,
        json!({"type": "thread.delete", "commandId": "cmd-thread-replay-delete", "threadId": "thread-replay"}),
    )
    .await;

    let events = all_events(&engine).await;
    assert_eq!(types(&events), ["project.created", "thread.created", "thread.deleted"]);
}

// ---------------------------------------------------------------------------------------------
// "streams persisted domain events in order"

#[tokio::test]
async fn streams_persisted_domain_events_in_order() {
    let (_db, engine) = system().await;
    ok(
        &engine,
        project_create("cmd-project-stream-create", "project-stream", "Stream Project", "/tmp/project-stream"),
    )
    .await;

    let mut subscription = engine.subscribe();
    ok(
        &engine,
        thread_create(
            "cmd-stream-thread-create",
            "thread-stream",
            "project-stream",
            "domain-stream",
            "approval-required",
        ),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "thread.meta.update",
            "commandId": "cmd-stream-thread-update",
            "threadId": "thread-stream",
            "title": "domain-stream-updated",
        }),
    )
    .await;
    let mut event_types = Vec::new();
    for _ in 0..2 {
        let event = tokio::time::timeout(Duration::from_secs(5), subscription.recv())
            .await
            .expect("an event in time")
            .expect("the stream is open");
        event_types.push(event.event_type());
    }
    assert_eq!(event_types, ["thread.created", "thread.meta-updated"]);
}

// ---------------------------------------------------------------------------------------------
// "does not regress a generated branch to a stale temporary worktree branch"

#[tokio::test]
async fn does_not_regress_a_generated_branch_to_a_stale_temporary_worktree_branch() {
    let (_db, engine) = system().await;
    ok(
        &engine,
        project_create(
            "cmd-branch-race-project-create",
            "project-branch-race",
            "Branch Race Project",
            "/tmp/project-branch-race",
        ),
    )
    .await;
    let mut create = thread_create(
        "cmd-branch-race-thread-create",
        "thread-branch-race",
        "project-branch-race",
        "Branch Race Thread",
        "approval-required",
    );
    merge(
        &mut create,
        json!({"branch": "t3code/generated-branch-name", "worktreePath": "/tmp/project-branch-race-worktree"}),
    );
    ok(&engine, create).await;
    ok(
        &engine,
        json!({
            "type": "thread.meta.update",
            "commandId": "cmd-stale-temporary-branch-sync",
            "threadId": "thread-branch-race",
            "branch": "t3code/1234abcd",
            "expectedBranch": "t3code/1234abcd",
        }),
    )
    .await;

    let snapshot = model(&engine).await;
    assert_eq!(snapshot["threads"][0]["branch"], "t3code/generated-branch-name");
}

// ---------------------------------------------------------------------------------------------
// "rejects PR discovery completed after a newer %s command"
//
// Adapted: the stub `RepositoryIdentityResolver` becomes [`IdentityReads`], which hands the
// engine the project's repository identity when a legacy `linkedPullRequest` edit needs it
// (`getProjectShellById` in TS). `Date.now` is pinned by the decider clock.

fn example_identity(workspace_root: &str) -> RepositoryIdentity {
    decode(json!({
        "canonicalKey": "example.test/owner/repository",
        "provider": "github",
        "displayName": "owner/repository",
        "rootPath": workspace_root,
        "locator": {
            "source": "git-remote",
            "remoteName": "origin",
            "remoteUrl": "https://example.test/owner/repository.git",
        },
    }))
}

async fn pr_race_case(change: &str) {
    let db = Db::open_in_memory().unwrap();
    let engine = start(EngineConfig {
        reads: Arc::new(IdentityReads {
            log: EventLogReads::new(db.clone()),
            identity: example_identity("/tmp/pr-race-project"),
        }),
        ..config(&db)
    })
    .await;
    let project_id = "pr-race-project";
    let thread_id = "pr-race-thread";
    let previous = json!({
        "projectId": project_id,
        "repository": "owner/repository",
        "number": 1,
        "url": "https://example.test/owner/repository/pull/1",
    });
    let mut replacement = previous.clone();
    merge(&mut replacement, json!({"number": 2, "url": "https://example.test/owner/repository/pull/2"}));
    let mut relinked = previous.clone();
    merge(&mut relinked, json!({"number": 3, "url": "https://example.test/owner/repository/pull/3"}));

    ok(
        &engine,
        json!({
            "type": "project.create",
            "commandId": "pr-race-project-create",
            "projectId": project_id,
            "title": "PR race project",
            "workspaceRoot": "/tmp/pr-race-project",
            "defaultModelSelection": null,
            "createdAt": NOW,
        }),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "thread.create",
            "commandId": "pr-race-thread-create",
            "threadId": thread_id,
            "projectId": project_id,
            "title": "PR race thread",
            "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "branch": "feature",
            "worktreePath": null,
            "createdAt": NOW,
        }),
    )
    .await;
    let observed = ok(
        &engine,
        json!({
            "type": "thread.meta.update",
            "commandId": "pr-race-link",
            "threadId": thread_id,
            "linkedPullRequest": previous,
        }),
    )
    .await;
    assert_eq!(model(&engine).await["threads"][0]["linkedPullRequest"], previous);

    let change_command = match change {
        "project" => json!({
            "type": "project.meta.update",
            "commandId": "pr-race-project-move",
            "projectId": project_id,
            "workspaceRoot": "/tmp/another-project-root",
        }),
        "delete" => json!({"type": "thread.delete", "commandId": "pr-race-delete", "threadId": thread_id}),
        _ => {
            let mut update = json!({
                "type": "thread.meta.update",
                "commandId": format!("pr-race-{change}"),
                "threadId": thread_id,
            });
            merge(
                &mut update,
                match change {
                    "unlink" => json!({"linkedPullRequest": null}),
                    "relink" => json!({"linkedPullRequest": relinked}),
                    "branch" => json!({"branch": "another-feature"}),
                    "worktree" => json!({"worktreePath": "/tmp/another-worktree"}),
                    other => panic!("unknown change {other}"),
                },
            );
            update
        }
    };
    ok(&engine, change_command).await;

    let error = fails(
        &engine,
        json!({
            "type": "thread.pull-request.sync",
            "commandId": "pr-race-stale-sync",
            "threadId": thread_id,
            "projectId": project_id,
            "snapshotSequence": observed,
            "expected": {
                "workspaceRoot": "/tmp/pr-race-project",
                "branch": "feature",
                "worktreePath": null,
                "linkedPullRequest": previous,
                "branchPullRequest": null,
            },
            "branchPullRequest": replacement,
            "linkedPullRequest": replacement,
        }),
    )
    .await;
    assert_eq!(error.tag(), "OrchestrationCommandInvariantError", "{change}: {error}");
    if change == "delete" {
        return;
    }
    let current = model(&engine).await;
    let current = &current["threads"][0];
    assert!(null_or_absent(&current["branchPullRequest"]), "{change}: {current}");
    let numbers: Vec<i64> = current["pullRequests"]
        .as_array()
        .unwrap()
        .iter()
        .map(|link| link["number"].as_i64().unwrap())
        .collect();
    let expected_numbers: Vec<i64> = match change {
        "unlink" => vec![],
        "relink" => vec![3],
        _ => vec![1],
    };
    assert_eq!(numbers, expected_numbers, "{change}");
    let expected_linked = match change {
        "unlink" => Value::Null,
        "relink" => relinked,
        _ => previous,
    };
    assert_eq!(current["linkedPullRequest"], expected_linked, "{change}");
}

#[tokio::test]
async fn rejects_pr_discovery_completed_after_a_newer_unlink_command() {
    pr_race_case("unlink").await;
}

#[tokio::test]
async fn rejects_pr_discovery_completed_after_a_newer_relink_command() {
    pr_race_case("relink").await;
}

#[tokio::test]
async fn rejects_pr_discovery_completed_after_a_newer_branch_command() {
    pr_race_case("branch").await;
}

#[tokio::test]
async fn rejects_pr_discovery_completed_after_a_newer_worktree_command() {
    pr_race_case("worktree").await;
}

#[tokio::test]
async fn rejects_pr_discovery_completed_after_a_newer_project_command() {
    pr_race_case("project").await;
}

#[tokio::test]
async fn rejects_pr_discovery_completed_after_a_newer_delete_command() {
    pr_race_case("delete").await;
}

// ---------------------------------------------------------------------------------------------
// "saves PR associations through streaming and unrelated metadata edits"

#[tokio::test]
async fn saves_pr_associations_through_streaming_and_unrelated_metadata_edits() {
    let (_db, engine) = system().await;
    let project_id = "pr-sync-project";
    let thread_id = "pr-sync-thread";
    ok(
        &engine,
        json!({
            "type": "project.create",
            "commandId": "pr-sync-project-create",
            "projectId": project_id,
            "title": "PR sync project",
            "workspaceRoot": "/tmp/pr-sync-project",
            "defaultModelSelection": null,
            "createdAt": NOW,
        }),
    )
    .await;
    let created = ok(
        &engine,
        json!({
            "type": "thread.create",
            "commandId": "pr-sync-thread-create",
            "threadId": thread_id,
            "projectId": project_id,
            "title": "PR sync thread",
            "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
            "runtimeMode": "full-access",
            "interactionMode": "default",
            "branch": "feature",
            "worktreePath": null,
            "createdAt": NOW,
        }),
    )
    .await;
    let reference = json!({
        "projectId": project_id,
        "repository": "owner/repository",
        "number": 42,
        "url": "https://example.test/owner/repository/pull/42",
    });
    let activity_at = "2026-01-01T01:00:00.000Z";
    ok(
        &engine,
        json!({
            "type": "thread.message.assistant.delta",
            "commandId": "pr-sync-streaming-message",
            "threadId": thread_id,
            "messageId": "pr-sync-message",
            "delta": "The PR is ready.",
            "createdAt": activity_at,
        }),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "thread.meta.update",
            "commandId": "pr-sync-title-and-model",
            "threadId": thread_id,
            "title": "Renamed thread",
            "modelSelection": {"instanceId": "codex", "model": "gpt-5.4"},
        }),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "project.meta.update",
            "commandId": "pr-sync-project-title",
            "projectId": project_id,
            "title": "Renamed project",
        }),
    )
    .await;
    let before_sync = model(&engine).await["threads"][0].clone();
    ok(
        &engine,
        json!({
            "type": "thread.pull-request.sync",
            "commandId": "pr-sync-discovery",
            "projectId": project_id,
            "threadId": thread_id,
            "snapshotSequence": created,
            "expected": {
                "workspaceRoot": "/tmp/pr-sync-project",
                "branch": "feature",
                "worktreePath": null,
                "linkedPullRequest": null,
                "branchPullRequest": null,
            },
            "branchPullRequest": reference,
        }),
    )
    .await;
    let current = model(&engine).await["threads"][0].clone();
    assert_eq!(current["branchPullRequest"], reference);
    assert!(null_or_absent(&current["linkedPullRequest"]));
    assert_eq!(current["updatedAt"], before_sync["updatedAt"]);
}

// ---------------------------------------------------------------------------------------------
// "allows authoritative worktree bootstrap to assign a temporary branch"

#[tokio::test]
async fn allows_authoritative_worktree_bootstrap_to_assign_a_temporary_branch() {
    let (_db, engine) = system().await;
    ok(
        &engine,
        project_create(
            "cmd-worktree-bootstrap-project-create",
            "project-worktree-bootstrap",
            "Worktree Bootstrap Project",
            "/tmp/project-worktree-bootstrap",
        ),
    )
    .await;
    let mut create = thread_create(
        "cmd-worktree-bootstrap-thread-create",
        "thread-worktree-bootstrap",
        "project-worktree-bootstrap",
        "Worktree Bootstrap Thread",
        "approval-required",
    );
    merge(&mut create, json!({"branch": "main"}));
    ok(&engine, create).await;
    ok(
        &engine,
        json!({
            "type": "thread.meta.update",
            "commandId": "cmd-authoritative-worktree-bootstrap",
            "threadId": "thread-worktree-bootstrap",
            "branch": "t3code/1234abcd",
            "worktreePath": "/tmp/project-worktree-bootstrap-worktree",
        }),
    )
    .await;

    let snapshot = model(&engine).await;
    assert_eq!(snapshot["threads"][0]["branch"], "t3code/1234abcd");
    assert_eq!(snapshot["threads"][0]["worktreePath"], "/tmp/project-worktree-bootstrap-worktree");
}

// ---------------------------------------------------------------------------------------------
// "records command ack duration using the first committed event type": not ported (metrics).
// "records failed command dispatches as metric failures": not ported (metrics).

// ---------------------------------------------------------------------------------------------
// "stores completed checkpoint summaries even when no files changed"

#[tokio::test]
async fn stores_completed_checkpoint_summaries_even_when_no_files_changed() {
    let (_db, engine) = system().await;
    ok(
        &engine,
        project_create(
            "cmd-project-turn-diff-create",
            "project-turn-diff",
            "Turn Diff Project",
            "/tmp/project-turn-diff",
        ),
    )
    .await;
    ok(
        &engine,
        thread_create(
            "cmd-thread-turn-diff-create",
            "thread-turn-diff",
            "project-turn-diff",
            "Turn diff thread",
            "approval-required",
        ),
    )
    .await;
    ok(
        &engine,
        json!({
            "type": "thread.turn.diff.complete",
            "commandId": "cmd-turn-diff-complete",
            "threadId": "thread-turn-diff",
            "turnId": "turn-1",
            "completedAt": NOW,
            "checkpointRef": "refs/t3/checkpoints/thread-turn-diff/turn/1",
            "status": "ready",
            "files": [],
            "checkpointTurnCount": 1,
            "createdAt": NOW,
        }),
    )
    .await;

    let snapshot = model(&engine).await;
    assert_eq!(
        thread(&snapshot, "thread-turn-diff")["checkpoints"],
        json!([{
            "turnId": "turn-1",
            "checkpointTurnCount": 1,
            "checkpointRef": "refs/t3/checkpoints/thread-turn-diff/turn/1",
            "status": "ready",
            "files": [],
            "assistantMessageId": null,
            "completedAt": NOW,
        }])
    );
}

// ---------------------------------------------------------------------------------------------
// "keeps processing queued commands after a storage failure"
//
// Adapted: the TS stub store fails the first append of `cmd-flaky-1` with
// `PersistenceSqlError("append failed")`; here a trigger aborts that command's insert. The Rust
// error carries the SQLite condition (never the driver message), so the assertion is on the
// `PersistenceSqlError` tag instead of the stub's detail text.

#[tokio::test]
async fn keeps_processing_queued_commands_after_a_storage_failure() {
    let db = Db::open_in_memory().unwrap();
    db.call(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER fail_flaky_append BEFORE INSERT ON orchestration_events
             WHEN NEW.command_id = 'cmd-flaky-1'
             BEGIN SELECT RAISE(ABORT, 'append failed'); END;",
        )
        .map_err(|error| DbError::sql("test.trigger", error))
    })
    .await
    .unwrap();
    let engine = start(config(&db)).await;

    ok(
        &engine,
        project_create("cmd-project-flaky-create", "project-flaky", "Flaky Project", "/tmp/project-flaky"),
    )
    .await;
    let error = fails(
        &engine,
        thread_create("cmd-flaky-1", "thread-flaky-fail", "project-flaky", "flaky-fail", "approval-required"),
    )
    .await;
    assert_eq!(error.tag(), "PersistenceSqlError", "{error}");

    let sequence = ok(
        &engine,
        thread_create("cmd-flaky-2", "thread-flaky-ok", "project-flaky", "flaky-ok", "approval-required"),
    )
    .await;
    assert_eq!(sequence, 2);
    let events = all_events(&engine).await;
    assert_eq!(types(&events), ["project.created", "thread.created"]);
}

// ---------------------------------------------------------------------------------------------
// "rolls back all events for a multi-event command when projection fails mid-dispatch"

#[tokio::test]
async fn rolls_back_all_events_for_a_multi_event_command_when_projection_fails_mid_dispatch() {
    let db = Db::open_in_memory().unwrap();
    let engine = start(EngineConfig {
        pipeline: Arc::new(FailingPipeline::new(|event| {
            command_id_is(event, "cmd-turn-start-atomic") && event.event_type() == "thread.turn-start-requested"
        })),
        ..config(&db)
    })
    .await;
    ok(
        &engine,
        project_create("cmd-project-atomic-create", "project-atomic", "Atomic Project", "/tmp/project-atomic"),
    )
    .await;
    ok(
        &engine,
        thread_create("cmd-thread-atomic-create", "thread-atomic", "project-atomic", "atomic", "approval-required"),
    )
    .await;

    let turn_start_command = turn_start("cmd-turn-start-atomic", "thread-atomic", "msg-atomic-1", "hello");
    let error = fails(&engine, turn_start_command.clone()).await;
    assert!(error.to_string().contains("projection failed"), "{error}");

    let events_after_failure = all_events(&engine).await;
    assert_eq!(types(&events_after_failure), ["project.created", "thread.created"]);

    let retry_sequence = ok(&engine, turn_start_command).await;
    assert_eq!(retry_sequence, 4);
    let events_after_retry = all_events(&engine).await;
    assert_eq!(
        types(&events_after_retry),
        ["project.created", "thread.created", "thread.message-sent", "thread.turn-start-requested",]
    );
    assert_eq!(
        events_after_retry.iter().filter(|event| event["commandId"] == "cmd-turn-start-atomic").count(),
        2
    );
}

// ---------------------------------------------------------------------------------------------
// "reconciles command state when append persists but projection fails"
//
// Adapted: the TS test uses a non-transactional stub store, so the failing dispatch's own event
// stays persisted and the engine must fold it in when it reconciles. The Rust engine appends
// inside its transaction, so a projection failure always rolls its own events back; the case it
// reconciles is events persisted by another writer since the dispatch started. A second engine
// on the same database archives the thread behind the first engine's back; the first engine's
// failing dispatch then reconciles that event, so its retry is rejected as "already archived"
// exactly as in TS, and the reconciled event is published to its subscribers.

#[tokio::test]
async fn reconciles_command_state_when_append_persists_but_projection_fails() {
    let db = Db::open_in_memory().unwrap();
    let engine = start(EngineConfig {
        pipeline: Arc::new(FailingPipeline::new(|event| command_id_is(event, "cmd-thread-archive-sync-fail"))),
        ..config(&db)
    })
    .await;
    ok(
        &engine,
        project_create("cmd-project-sync-create", "project-sync", "Sync Project", "/tmp/project-sync"),
    )
    .await;
    ok(
        &engine,
        thread_create("cmd-thread-sync-create", "thread-sync", "project-sync", "sync-before", "approval-required"),
    )
    .await;

    // Random event ids: a second `TestEnv` would reuse `event-1`, `event-2`, …
    let other_writer = start(config_with_env(&db, SystemEnv)).await;
    ok(
        &other_writer,
        json!({"type": "thread.archive", "commandId": "cmd-thread-archive-other-writer", "threadId": "thread-sync"}),
    )
    .await;
    drop(other_writer);
    assert!(
        thread(&model(&engine).await, "thread-sync")["archivedAt"].is_null(),
        "the first engine has not seen the other writer's event yet"
    );

    let mut subscription = engine.subscribe();
    let error = fails(
        &engine,
        json!({"type": "thread.archive", "commandId": "cmd-thread-archive-sync-fail", "threadId": "thread-sync"}),
    )
    .await;
    assert!(error.to_string().contains("projection failed"), "{error}");
    let reconciled = subscription.try_recv().expect("the reconciled event is published");
    assert_eq!(reconciled.event_type(), "thread.archived");
    assert_eq!(engine.latest_sequence(), 3);

    let retry = fails(
        &engine,
        json!({"type": "thread.archive", "commandId": "cmd-thread-archive-sync-retry", "threadId": "thread-sync"}),
    )
    .await;
    assert!(retry.to_string().contains("already archived"), "{retry}");
}

// ---------------------------------------------------------------------------------------------
// "fails command dispatch when command invariants are violated"

#[tokio::test]
async fn fails_command_dispatch_when_command_invariants_are_violated() {
    let (_db, engine) = system().await;
    let error = fails(&engine, turn_start("cmd-invariant-missing-thread", "thread-missing", "msg-missing", "hello")).await;
    assert!(error.to_string().contains("Thread 'thread-missing' does not exist"), "{error}");
}

// ---------------------------------------------------------------------------------------------
// "rejects duplicate thread creation"

#[tokio::test]
async fn rejects_duplicate_thread_creation() {
    let (_db, engine) = system().await;
    ok(
        &engine,
        project_create(
            "cmd-project-duplicate-create",
            "project-duplicate",
            "Duplicate Project",
            "/tmp/project-duplicate",
        ),
    )
    .await;
    ok(
        &engine,
        thread_create(
            "cmd-thread-duplicate-1",
            "thread-duplicate",
            "project-duplicate",
            "duplicate",
            "approval-required",
        ),
    )
    .await;
    let error = fails(
        &engine,
        thread_create(
            "cmd-thread-duplicate-2",
            "thread-duplicate",
            "project-duplicate",
            "duplicate",
            "approval-required",
        ),
    )
    .await;
    assert!(error.to_string().contains("already exists"), "{error}");
}

// ---------------------------------------------------------------------------------------------
// "replays the accepted receipt for a genuine retry of the same command"

#[tokio::test]
async fn replays_the_accepted_receipt_for_a_genuine_retry_of_the_same_command() {
    let (_db, engine) = system().await;
    ok(
        &engine,
        project_create("cmd-retry-project-create", "project-retry", "Retry Project", "/tmp/project-retry"),
    )
    .await;
    ok(
        &engine,
        thread_create("cmd-retry-thread-create", "thread-retry", "project-retry", "retry", "approval-required"),
    )
    .await;

    let command = turn_start("cmd-retry-turn-start", "thread-retry", "msg-retry", "hello");
    let first = ok(&engine, command.clone()).await;
    let second = ok(&engine, command).await;
    assert_eq!(second, first);

    let snapshot = model(&engine).await;
    assert_eq!(user_messages(thread(&snapshot, "thread-retry")).len(), 1);
}

// ---------------------------------------------------------------------------------------------
// "rejects reusing an accepted command id for a different aggregate"

#[tokio::test]
async fn rejects_reusing_an_accepted_command_id_for_a_different_aggregate() {
    let (_db, engine) = system().await;
    ok(
        &engine,
        project_create("cmd-conflict-project-create", "project-conflict", "Conflict Project", "/tmp/project-conflict"),
    )
    .await;
    for thread_id in ["thread-conflict-a", "thread-conflict-b"] {
        ok(
            &engine,
            thread_create(
                &format!("cmd-{thread_id}-create"),
                thread_id,
                "project-conflict",
                thread_id,
                "approval-required",
            ),
        )
        .await;
    }
    ok(&engine, turn_start("cmd-conflict-turn-start", "thread-conflict-a", "msg-conflict-a", "hello")).await;

    let error = fails(
        &engine,
        turn_start("cmd-conflict-turn-start", "thread-conflict-b", "msg-conflict-b", "hello again"),
    )
    .await;
    assert_eq!(error.tag(), "OrchestrationCommandIdConflictError");
    assert!(error.to_string().contains("already used for thread 'thread-conflict-a'"), "{error}");

    let snapshot = model(&engine).await;
    assert!(user_messages(thread(&snapshot, "thread-conflict-b")).is_empty());
}

// ---------------------------------------------------------------------------------------------
// "stamps the dispatching client's origin onto persisted event metadata"

#[tokio::test]
async fn stamps_the_dispatching_clients_origin_onto_persisted_event_metadata() {
    let (_db, engine) = system().await;
    engine
        .dispatch(
            command(project_create(
                "cmd-origin-project-create",
                "project-origin",
                "Origin Project",
                "/tmp/project-origin",
            )),
            Some(decode(json!({"surface": "mobile", "appVersion": "1.2.3"}))),
        )
        .await
        .unwrap();
    ok(
        &engine,
        project_create(
            "cmd-no-origin-project-create",
            "project-no-origin",
            "No Origin Project",
            "/tmp/project-no-origin",
        ),
    )
    .await;

    let events = all_events(&engine).await;
    let with_origin = events.iter().find(|event| event["commandId"] == "cmd-origin-project-create").unwrap();
    let without_origin = events.iter().find(|event| event["commandId"] == "cmd-no-origin-project-create").unwrap();
    assert_eq!(with_origin["metadata"]["origin"], json!({"surface": "mobile", "appVersion": "1.2.3"}));
    assert!(without_origin["metadata"].get("origin").is_none(), "{}", without_origin["metadata"]);
}
