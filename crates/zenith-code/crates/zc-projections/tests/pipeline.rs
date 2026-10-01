//! Ports of `ProjectionPipeline.test.ts` and `ProjectionSnapshotQuery.test.ts` cases that run
//! without node (the differential suite in `scenarios.rs` covers the rest against TS itself).

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};
use zc_contracts::OrchestrationThreadDetailWindow;
use zc_db::repos::event_store::{self, NewEvent, PersistedEvent};
use zc_db::{Conn, Db, DbOptions};
use zc_projections::{projector_names, NoRepositoryIdentities, NoThreadLiveState, ProjectionPipeline, ProjectionSnapshotQuery};

struct Harness {
    _dir: tempfile::TempDir,
    db: Db,
    pipeline: ProjectionPipeline,
    attachments: PathBuf,
    n: usize,
}

const NOW: &str = "2026-01-01T00:00:00.000Z";

fn ts(seconds: usize) -> String {
    format!("2026-01-01T00:{:02}:{:02}.000Z", seconds / 60, seconds % 60)
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let attachments = dir.path().join("attachments");
        std::fs::create_dir_all(&attachments).unwrap();
        let db = Db::open_with(dir.path().join("state.sqlite"), DbOptions { migrate: true, readers: 1 }).unwrap();
        Self {
            pipeline: ProjectionPipeline::new(&attachments),
            attachments,
            db,
            _dir: dir,
            n: 0,
        }
    }

    fn event(&mut self, event_type: &str, kind: &str, id: &str, command: Option<&str>, payload: Value) -> NewEvent {
        self.n += 1;
        serde_json::from_value(json!({
            "type": event_type,
            "eventId": format!("evt-{}", self.n),
            "aggregateKind": kind,
            "aggregateId": id,
            "occurredAt": ts(self.n),
            "commandId": command.map(str::to_owned).unwrap_or_else(|| format!("cmd-{}", self.n)),
            "causationEventId": null,
            "correlationId": null,
            "metadata": {},
            "payload": payload,
        }))
        .unwrap()
    }

    /// Appends without projecting (a backlog for bootstrap).
    fn append(&mut self, event_type: &str, kind: &str, id: &str, payload: Value) -> PersistedEvent {
        let event = self.event(event_type, kind, id, None, payload);
        self.db.call_blocking(move |conn| event_store::append(conn, &event)).unwrap()
    }

    /// Appends and projects like the engine; returns whether a cleanup was needed.
    fn dispatch_with(&mut self, event_type: &str, kind: &str, id: &str, command: Option<&str>, payload: Value) -> bool {
        let event = self.event(event_type, kind, id, command, payload);
        let pipeline = self.pipeline.clone();
        self.db
            .call_blocking(move |conn| {
                let cleanup = conn.transaction(|conn| {
                    let stored = event_store::append(conn, &event)?;
                    pipeline.project_persisted_deferred(conn, &stored)
                })?;
                let needed = !cleanup.is_empty();
                cleanup.run(conn);
                Ok(needed)
            })
            .unwrap()
    }

    fn dispatch(&mut self, event_type: &str, id: &str, payload: Value) -> bool {
        let kind = if event_type.starts_with("project.") { "project" } else { "thread" };
        self.dispatch_with(event_type, kind, id, None, payload)
    }

    fn bootstrap(&self) {
        let pipeline = self.pipeline.clone();
        self.db.call_blocking(move |conn| pipeline.bootstrap(conn)).unwrap();
    }

    fn rows(&self, sql: &str) -> Vec<Vec<Value>> {
        let sql = sql.to_string();
        self.db
            .call_blocking(move |conn: &Conn| {
                let mut statement = conn.raw().prepare(&sql).unwrap();
                let count = statement.column_count();
                let rows = statement
                    .query_map([], |row| {
                        Ok((0..count)
                            .map(|index| match row.get::<_, rusqlite::types::Value>(index).unwrap() {
                                rusqlite::types::Value::Null => Value::Null,
                                rusqlite::types::Value::Integer(n) => json!(n),
                                rusqlite::types::Value::Real(n) => json!(n),
                                rusqlite::types::Value::Text(text) => json!(text),
                                rusqlite::types::Value::Blob(_) => json!("<blob>"),
                            })
                            .collect::<Vec<_>>())
                    })
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                Ok(rows)
            })
            .unwrap()
    }

    fn query(&self) -> ProjectionSnapshotQuery {
        ProjectionSnapshotQuery::new(self.db.clone(), Arc::new(NoRepositoryIdentities), Arc::new(NoThreadLiveState))
    }

    fn project(&mut self, id: &str) {
        self.dispatch(
            "project.created",
            id,
            json!({
                "projectId": id, "title": format!("Project {id}"), "workspaceRoot": format!("/work/{id}"),
                "defaultModelSelection": null, "scripts": [], "createdAt": NOW, "updatedAt": NOW,
            }),
        );
    }

    fn thread(&mut self, id: &str, project: &str) {
        self.dispatch(
            "thread.created",
            id,
            json!({
                "threadId": id, "projectId": project, "title": format!("Thread {id}"),
                "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"},
                "runtimeMode": "full-access", "branch": null, "worktreePath": null,
                "createdAt": NOW, "updatedAt": NOW,
            }),
        );
    }

    fn message(&mut self, thread: &str, id: &str, role: &str, text: &str, turn: Option<&str>, streaming: bool) -> bool {
        let at = ts(self.n + 1);
        self.dispatch(
            "thread.message-sent",
            thread,
            json!({
                "threadId": thread, "messageId": id, "role": role, "text": text, "turnId": turn,
                "streaming": streaming, "createdAt": at, "updatedAt": at,
            }),
        )
    }

    fn session(&mut self, thread: &str, status: &str, turn: Option<&str>) {
        let at = ts(self.n + 1);
        self.dispatch(
            "thread.session-set",
            thread,
            json!({"threadId": thread, "session": {
                "threadId": thread, "status": status, "providerName": "codex", "runtimeMode": "full-access",
                "activeTurnId": turn, "lastError": null, "updatedAt": at,
            }}),
        );
    }

    fn activity(&mut self, thread: &str, id: &str, kind: &str, payload: Value) {
        let at = ts(self.n + 1);
        self.dispatch(
            "thread.activity-appended",
            thread,
            json!({"threadId": thread, "activity": {
                "id": id, "tone": "approval", "kind": kind, "summary": kind, "payload": payload,
                "turnId": null, "createdAt": at,
            }}),
        );
    }
}

// "writes a project and all projector cursors in two statements"
#[test]
fn live_projection_advances_every_cursor_together() {
    let mut h = Harness::new();
    h.project("project-1");
    let states = h.rows("SELECT projector, last_applied_sequence FROM projection_state ORDER BY projector");
    assert_eq!(states.len(), 9);
    assert!(states.iter().all(|row| row[1] == json!(1)));
    assert!(!states.iter().any(|row| row[0] == json!(projector_names::ATTACHMENT_CLEANUP)));
}

// "runs attachment cleanup only for events that remove attachments"
#[test]
fn only_removal_events_carry_a_cleanup() {
    let mut h = Harness::new();
    h.project("p1");
    h.thread("t1", "p1");
    assert!(!h.message("t1", "m1", "user", "hi", None, false));
    let file = h.attachments.join("t1-0f0e0d0c-0b0a-4908-8706-050403020100.png");
    std::fs::write(&file, b"x").unwrap();
    assert!(h.dispatch("thread.deleted", "t1", json!({"threadId": "t1", "deletedAt": NOW})));
    assert!(!file.exists(), "the deleted thread's files are removed");
}

// "bootstraps all projection states and writes projection rows", "replays a bootstrap backlog
// larger than the event store default limit", "resumes from projector last_applied_sequence"
#[test]
fn bootstrap_replays_a_backlog_and_resumes_from_cursors() {
    let mut h = Harness::new();
    h.append(
        "project.created",
        "project",
        "p1",
        json!({
            "projectId": "p1", "title": "P", "workspaceRoot": "/w", "defaultModelSelection": null,
            "scripts": [], "createdAt": NOW, "updatedAt": NOW,
        }),
    );
    h.append(
        "thread.created",
        "thread",
        "t1",
        json!({
            "threadId": "t1", "projectId": "p1", "title": "T", "modelSelection": {"instanceId": "codex", "model": "m"},
            "runtimeMode": "full-access", "branch": null, "worktreePath": null, "createdAt": NOW, "updatedAt": NOW,
        }),
    );
    for n in 0..1_105 {
        h.append(
            "thread.message-sent",
            "thread",
            "t1",
            json!({
                "threadId": "t1", "messageId": format!("m{n}"), "role": "assistant", "text": format!("text {n}"),
                "turnId": null, "streaming": false, "createdAt": NOW, "updatedAt": NOW,
            }),
        );
    }
    h.bootstrap();
    assert_eq!(h.rows("SELECT COUNT(*) FROM projection_thread_messages")[0][0], json!(1_105));
    let states = h.rows("SELECT projector, last_applied_sequence FROM projection_state");
    assert_eq!(states.len(), 10);
    assert!(states.iter().all(|row| row[1] == json!(1_107)));

    // A projector reset to an older cursor replays only what it is missing.
    h.db.call_blocking(|conn| {
        conn.execute("DELETE FROM projection_thread_messages WHERE message_id = 'm1104'", [])?;
        conn.execute(
            "UPDATE projection_state SET last_applied_sequence = 1106 WHERE projector = 'projection.thread-messages'",
            [],
        )?;
        conn.execute("DELETE FROM projection_thread_messages WHERE message_id = 'm0'", [])?;
        Ok(())
    })
    .unwrap();
    h.bootstrap();
    let ids = h.rows("SELECT message_id FROM projection_thread_messages WHERE message_id IN ('m0', 'm1104')");
    assert_eq!(ids, vec![vec![json!("m1104")]], "only the event after the cursor is replayed");
}

// "keeps accumulated assistant text when completion payload text is empty", streaming deltas
#[test]
fn streaming_deltas_accumulate_and_an_empty_completion_keeps_the_text() {
    let mut h = Harness::new();
    h.project("p1");
    h.thread("t1", "p1");
    h.message("t1", "a1", "assistant", "Hel", Some("turn-1"), true);
    h.message("t1", "a1", "assistant", "lo", Some("turn-1"), true);
    assert_eq!(
        h.rows("SELECT text, is_streaming FROM projection_thread_messages"),
        vec![vec![json!("Hello"), json!(1)]]
    );
    h.message("t1", "a1", "assistant", "", Some("turn-1"), false);
    assert_eq!(
        h.rows("SELECT text, is_streaming FROM projection_thread_messages"),
        vec![vec![json!("Hello"), json!(0)]]
    );
}

// "does not mark imported user messages as queued work in thread shells"
#[test]
fn imported_user_messages_do_not_count_as_latest_user_message() {
    let mut h = Harness::new();
    h.project("p1");
    h.thread("t1", "p1");
    h.message("t1", "import:codex:s:0", "user", "old", None, false);
    assert_eq!(h.rows("SELECT latest_user_message_at FROM projection_threads"), vec![vec![Value::Null]]);
    h.message("t1", "u1", "user", "new", None, false);
    assert_ne!(h.rows("SELECT latest_user_message_at FROM projection_threads")[0][0], Value::Null);
}

// "keeps the turn running across interim assistant messages until the session ends",
// "settles a superseded running turn when a new turn becomes active"
#[test]
fn turns_settle_when_the_session_leaves_running() {
    let mut h = Harness::new();
    h.project("p1");
    h.thread("t1", "p1");
    h.session("t1", "running", Some("turn-1"));
    h.message("t1", "a1", "assistant", "interim", Some("turn-1"), false);
    assert_eq!(
        h.rows("SELECT state FROM projection_turns WHERE turn_id = 'turn-1'"),
        vec![vec![json!("running")]]
    );
    h.session("t1", "running", Some("turn-2"));
    assert_eq!(
        h.rows("SELECT state FROM projection_turns WHERE turn_id = 'turn-1'"),
        vec![vec![json!("completed")]]
    );
    h.session("t1", "interrupted", None);
    assert_eq!(
        h.rows("SELECT state FROM projection_turns WHERE turn_id = 'turn-2'"),
        vec![vec![json!("interrupted")]]
    );
    assert_eq!(h.rows("SELECT latest_turn_id FROM projection_threads"), vec![vec![json!("turn-2")]]);
}

// "does not let a later missing placeholder clobber a ready checkpoint"
#[test]
fn a_missing_placeholder_does_not_clobber_a_ready_checkpoint() {
    let mut h = Harness::new();
    h.project("p1");
    h.thread("t1", "p1");
    for status in ["ready", "missing"] {
        h.dispatch(
            "thread.turn-diff-completed",
            "t1",
            json!({
                "threadId": "t1", "turnId": "turn-1", "checkpointTurnCount": 1,
                "checkpointRef": "refs/t3/checkpoints/x/turn/1", "status": status, "files": [],
                "assistantMessageId": null, "completedAt": NOW,
            }),
        );
    }
    assert_eq!(h.rows("SELECT checkpoint_status FROM projection_turns"), vec![vec![json!("ready")]]);
}

// "restores pending approvals when a provider reply fails", "clears stale pending approvals"
#[test]
fn failed_approval_replies_restore_the_request() {
    let mut h = Harness::new();
    h.project("p1");
    h.thread("t1", "p1");
    h.activity("t1", "req", "approval.requested", json!({"requestId": "r1"}));
    assert_eq!(h.rows("SELECT pending_approval_count FROM projection_threads"), vec![vec![json!(1)]]);
    h.dispatch(
        "thread.approval-response-requested",
        "t1",
        json!({"threadId": "t1", "requestId": "r1", "decision": "accept", "createdAt": NOW}),
    );
    assert_eq!(h.rows("SELECT pending_approval_count FROM projection_threads"), vec![vec![json!(0)]]);
    h.activity(
        "t1",
        "fail",
        "provider.approval.respond.failed",
        json!({"requestId": "r1", "detail": "socket closed"}),
    );
    assert_eq!(h.rows("SELECT status FROM projection_pending_approvals"), vec![vec![json!("pending")]]);
    assert_eq!(h.rows("SELECT pending_approval_count FROM projection_threads"), vec![vec![json!(1)]]);
    h.activity(
        "t1",
        "stale",
        "provider.approval.respond.failed",
        json!({"requestId": "r1", "detail": "Unknown pending approval request"}),
    );
    assert_eq!(
        h.rows("SELECT status, decision FROM projection_pending_approvals"),
        vec![vec![json!("resolved"), Value::Null]]
    );
}

// "clears pending turn starts when startup reaches a terminal session state", "only clears the
// compact request that produced the compaction activity"
#[test]
fn pending_turn_starts_clear_on_terminal_states_and_their_compaction() {
    let mut h = Harness::new();
    h.project("p1");
    h.thread("t1", "p1");
    h.message("t1", "c1", "user", "/compact", None, false);
    h.dispatch(
        "thread.turn-start-requested",
        "t1",
        json!({"threadId": "t1", "messageId": "c1", "createdAt": NOW}),
    );
    // A turn start behind a pending /compact is not recorded.
    h.dispatch(
        "thread.turn-start-requested",
        "t1",
        json!({"threadId": "t1", "messageId": "u9", "createdAt": NOW}),
    );
    assert_eq!(h.rows("SELECT pending_message_id FROM projection_turns"), vec![vec![json!("c1")]]);
    h.activity("t1", "other", "context-compaction", json!({"requestId": "u9"}));
    assert_eq!(h.rows("SELECT COUNT(*) FROM projection_turns")[0][0], json!(1));
    h.activity("t1", "mine", "context-compaction", json!({"requestId": "c1"}));
    assert_eq!(h.rows("SELECT COUNT(*) FROM projection_turns")[0][0], json!(0));
    h.dispatch(
        "thread.turn-start-requested",
        "t1",
        json!({"threadId": "t1", "messageId": "u2", "createdAt": NOW}),
    );
    h.session("t1", "error", None);
    assert_eq!(h.rows("SELECT COUNT(*) FROM projection_turns")[0][0], json!(0));
}

// "does not fallback-retain messages whose turnId is removed by revert"
#[test]
fn revert_drops_later_turns_and_their_messages() {
    let mut h = Harness::new();
    h.project("p1");
    h.thread("t1", "p1");
    for n in 1..=2 {
        let (user, turn, reply) = (format!("u{n}"), format!("turn-{n}"), format!("a{n}"));
        h.message("t1", &user, "user", "q", None, false);
        h.dispatch(
            "thread.turn-start-requested",
            "t1",
            json!({"threadId": "t1", "messageId": user, "createdAt": ts(h.n + 1)}),
        );
        h.session("t1", "running", Some(&turn));
        h.message("t1", &reply, "assistant", "a", Some(&turn), false);
        h.dispatch(
            "thread.turn-diff-completed",
            "t1",
            json!({
                "threadId": "t1", "turnId": turn, "checkpointTurnCount": n,
                "checkpointRef": format!("refs/t3/checkpoints/x/turn/{n}"), "status": "ready", "files": [],
                "assistantMessageId": reply, "completedAt": ts(h.n + 1),
            }),
        );
        h.session("t1", "ready", None);
    }
    h.message("t1", "stray", "assistant", "late", Some("turn-2"), false);
    assert!(h.dispatch("thread.reverted", "t1", json!({"threadId": "t1", "turnCount": 1})));
    assert_eq!(
        h.rows("SELECT message_id FROM projection_thread_messages ORDER BY message_id"),
        vec![vec![json!("a1")], vec![json!("u1")]]
    );
    assert_eq!(h.rows("SELECT turn_id FROM projection_turns"), vec![vec![json!("turn-1")]]);
    assert_eq!(h.rows("SELECT latest_turn_id FROM projection_threads"), vec![vec![json!("turn-1")]]);
}

// "re-creating a deleted thread id starts from an empty projection", "replaying a superseded
// thread.deleted keeps the re-created thread's files"
#[test]
fn a_recreated_thread_starts_empty_and_keeps_its_files_on_replay() {
    let mut h = Harness::new();
    h.project("p1");
    h.thread("t1", "p1");
    h.message("t1", "old", "user", "old", None, false);
    h.dispatch("thread.deleted", "t1", json!({"threadId": "t1", "deletedAt": NOW}));
    h.thread("t1", "p1");
    assert_eq!(h.rows("SELECT COUNT(*) FROM projection_thread_messages")[0][0], json!(0));
    assert_eq!(h.rows("SELECT deleted_at FROM projection_threads"), vec![vec![Value::Null]]);

    let file = h.attachments.join("t1-0f0e0d0c-0b0a-4908-8706-050403020100.png");
    std::fs::write(&file, b"x").unwrap();
    h.db.call_blocking(|conn| {
        conn.execute("DELETE FROM projection_state", [])?;
        Ok(())
    })
    .unwrap();
    h.bootstrap();
    assert!(file.exists(), "a superseded deletion does not remove the new incarnation's files");
}

// ProjectionSnapshotQuery: windows, cursors and the thread watermark.
#[tokio::test(flavor = "multi_thread")]
async fn thread_detail_windows_page_backwards_with_cursors() {
    let mut h = tokio::task::block_in_place(Harness::new);
    tokio::task::block_in_place(|| {
        h.project("p1");
        h.thread("t1", "p1");
        for n in 1..=5 {
            let (user, turn) = (format!("u{n}"), format!("turn-{n}"));
            // The decider stamps a turn start with its message's time: the page anchor.
            let sent_at = ts(h.n + 1);
            h.message("t1", &user, "user", "q", None, false);
            h.dispatch(
                "thread.turn-start-requested",
                "t1",
                json!({"threadId": "t1", "messageId": user, "createdAt": sent_at}),
            );
            h.session("t1", "running", Some(&turn));
            h.message("t1", &format!("a{n}"), "assistant", "a", Some(&turn), false);
            h.session("t1", "ready", None);
        }
    });
    let query = h.query();
    let window = |cursor: Option<String>| OrchestrationThreadDetailWindow {
        turn_limit: Some(2),
        before_cursor: cursor,
    };
    let mut cursor = None;
    let mut pages = Vec::new();
    loop {
        let snapshot = query.get_thread_detail_snapshot("t1", Some(window(cursor.clone()))).await.unwrap().unwrap();
        let page = snapshot.page.clone().unwrap();
        pages.push(
            snapshot
                .thread
                .messages
                .iter()
                .map(|message| message.id.as_str().to_string())
                .collect::<Vec<_>>(),
        );
        assert_eq!(page.snapshot_sequence, snapshot.snapshot_sequence);
        assert!(page.thread_sequence.unwrap() > 0);
        match page.before_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, vec![vec!["u4", "a4", "u5", "a5"], vec!["u2", "a2", "u3", "a3"], vec!["u1", "a1"],]);
    // A malformed cursor degrades to the first page.
    let first = query
        .get_thread_detail_snapshot("t1", Some(window(Some("garbage".into()))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.thread.messages.len(), 4);
    // Search escapes LIKE wildcards.
    let input = serde_json::from_value(json!({"query": "%"})).unwrap();
    assert!(query.search_threads(&input).await.unwrap().matches.is_empty());
}
