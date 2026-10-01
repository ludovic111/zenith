//! Subscription semantics of `orchestration.subscribeShell` / `subscribeThread` (`ws.ts`):
//! subscribe-before-snapshot, resume with `afterSequence` within and over the replay budgets,
//! completion markers, thread re-creation, coalescing, and the live buffer budget. The engine
//! is a small fake over the real event store, pipeline and queries.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{
    ApprovalRequestId, MessageId, OrchestrationEvent, OrchestrationGetSnapshotError, OrchestrationProject, OrchestrationProjectShell, OrchestrationReadModel,
    OrchestrationSearchThreadsInput, OrchestrationSearchThreadsResult, OrchestrationShellSnapshot, OrchestrationShellStreamEvent, OrchestrationShellStreamItem,
    OrchestrationThread, OrchestrationThreadActivity, OrchestrationThreadDetailSnapshot, OrchestrationThreadDetailWindow, OrchestrationThreadShell,
    OrchestrationThreadStreamItem, ProjectId, ThreadId,
};
use zc_core::pubsub::PubSub;
use zc_db::repos::event_store::{self, AggregateRange, EventPager, NewEvent};
use zc_db::{Db, DbOptions};
use zc_ports::contracts::{OrchestrationClientOrigin, OrchestrationCommand, OrchestrationDispatchError, PersistenceError};
use zc_ports::orchestration::{
    DeletedWorktreeThread, DispatchResult, FullThreadDiffContext, ImportedAgentSessionSource, ReplayStats, SnapshotCounts, ThreadCheckpointContext,
    ThreadDetailQuery, ThreadPullRequests, ThreadReplayRange, ThreadReplayStats, ThreadRuntimeContext, TurnStartMessage,
};
use zc_ports::{EventStream, OrchestrationDispatch, ProjectionReads};
use zc_projections::event::decode_persisted;
use zc_projections::reads::persistence_error;
use zc_projections::subscriptions::OrchestrationSubscriptions;
use zc_projections::{NoRepositoryIdentities, NoThreadLiveState, ProjectionPipeline, ProjectionSnapshotQuery};

const NOW: &str = "2026-01-01T00:00:00.000Z";

/// The engine side the subscriptions use: append + project + publish, and the replay reads.
struct FakeEngine {
    db: Db,
    pipeline: ProjectionPipeline,
    events: PubSub<OrchestrationEvent>,
    counter: Mutex<usize>,
    _dir: tempfile::TempDir,
}

impl FakeEngine {
    fn new() -> Arc<Self> {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_with(dir.path().join("state.sqlite"), DbOptions { migrate: true, readers: 1 }).unwrap();
        Arc::new(Self {
            pipeline: ProjectionPipeline::new(dir.path().join("attachments")),
            db,
            events: PubSub::new(),
            counter: Mutex::new(0),
            _dir: dir,
        })
    }

    async fn emit(&self, event_type: &str, id: &str, payload: Value) -> OrchestrationEvent {
        let n = {
            let mut counter = self.counter.lock().unwrap();
            *counter += 1;
            *counter
        };
        let kind = if event_type.starts_with("project.") { "project" } else { "thread" };
        let event: NewEvent = serde_json::from_value(json!({
            "type": event_type, "eventId": format!("evt-{n}"), "aggregateKind": kind, "aggregateId": id,
            "occurredAt": NOW, "commandId": format!("cmd-{n}"), "causationEventId": null,
            "correlationId": null, "metadata": {}, "payload": payload,
        }))
        .unwrap();
        let pipeline = self.pipeline.clone();
        let typed = self
            .db
            .call(move |conn| {
                let stored = conn.transaction(|conn| -> Result<_, zc_db::DbError> {
                    let stored = event_store::append(conn, &event)?;
                    pipeline.project_persisted_deferred(conn, &stored)?;
                    Ok(stored)
                })?;
                decode_persisted(&stored)
            })
            .await
            .unwrap();
        self.events.publish(typed.clone());
        typed
    }

    async fn project(&self, id: &str) {
        self.emit(
            "project.created",
            id,
            json!({
                "projectId": id, "title": "Project", "workspaceRoot": format!("/work/{id}"),
                "defaultModelSelection": null, "scripts": [], "createdAt": NOW, "updatedAt": NOW,
            }),
        )
        .await;
    }

    async fn thread(&self, id: &str) {
        self.emit(
            "thread.created",
            id,
            json!({
                "threadId": id, "projectId": "p1", "title": format!("Thread {id}"),
                "modelSelection": {"instanceId": "codex", "model": "gpt-5-codex"}, "runtimeMode": "full-access",
                "branch": null, "worktreePath": null, "createdAt": NOW, "updatedAt": NOW,
            }),
        )
        .await;
    }

    async fn message(&self, thread: &str, id: &str, text: &str) -> i64 {
        let event = self
            .emit(
                "thread.message-sent",
                thread,
                json!({
                    "threadId": thread, "messageId": id, "role": "assistant", "text": text, "turnId": null,
                    "streaming": false, "createdAt": NOW, "updatedAt": NOW,
                }),
            )
            .await;
        sequence(&event)
    }

    async fn tool_update(&self, thread: &str, id: &str, call: &str) {
        self.emit(
            "thread.activity-appended",
            thread,
            json!({"threadId": thread, "activity": {
                "id": id, "tone": "tool", "kind": "tool.updated", "summary": "Editing",
                "payload": {"itemType": "file_change", "title": "Editing", "data": {"toolCallId": call}},
                "turnId": "turn-1", "createdAt": NOW,
            }}),
        )
        .await;
    }

    async fn head(&self) -> i64 {
        self.db.call(event_store::latest_sequence).await.unwrap()
    }
}

fn sequence(event: &OrchestrationEvent) -> i64 {
    serde_json::to_value(event).unwrap()["sequence"].as_i64().unwrap()
}

fn stream_of(db: Db, pager: EventPager) -> EventStream<Result<OrchestrationEvent, PersistenceError>> {
    pager
        .into_stream(db)
        .map(|row| row.and_then(|row| decode_persisted(&row)).map_err(persistence_error))
        .boxed()
}

#[async_trait]
impl OrchestrationDispatch for FakeEngine {
    async fn dispatch(&self, _command: OrchestrationCommand, _origin: Option<OrchestrationClientOrigin>) -> Result<DispatchResult, OrchestrationDispatchError> {
        unimplemented!("not used by the subscriptions")
    }

    fn subscribe_domain_events(&self) -> EventStream<OrchestrationEvent> {
        self.events.subscribe().boxed()
    }

    async fn latest_sequence(&self) -> i64 {
        self.head().await
    }

    fn read_events(&self, from_sequence_exclusive: i64, limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, PersistenceError>> {
        stream_of(self.db.clone(), EventPager::from_sequence(from_sequence_exclusive, limit.map(i64::from)))
    }

    fn read_thread_events(&self, range: ThreadReplayRange, limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, PersistenceError>> {
        let range = AggregateRange {
            aggregate_kind: "thread".into(),
            aggregate_id: range.thread_id.0,
            from_sequence_exclusive: range.from_sequence_exclusive,
            to_sequence_inclusive: range.to_sequence_inclusive,
        };
        stream_of(self.db.clone(), EventPager::aggregate_range(range, limit.map(i64::from)))
    }

    async fn get_thread_replay_stats(&self, range: ThreadReplayRange, max_events: u32) -> Result<ThreadReplayStats, PersistenceError> {
        let range = AggregateRange {
            aggregate_kind: "thread".into(),
            aggregate_id: range.thread_id.0,
            from_sequence_exclusive: range.from_sequence_exclusive,
            to_sequence_inclusive: range.to_sequence_inclusive,
        };
        let stats = self
            .db
            .call(move |conn| event_store::get_aggregate_replay_stats(conn, &range, i64::from(max_events)))
            .await
            .map_err(persistence_error)?;
        Ok(ThreadReplayStats {
            event_count: stats.event_count as u64,
            payload_bytes: stats.payload_bytes as u64,
            has_create_event: stats.has_create_event,
        })
    }
}

/// The real queries, with a hook run while the shell snapshot is being read.
struct HookedReads {
    inner: ProjectionSnapshotQuery,
    engine: Arc<FakeEngine>,
    during_snapshot: AtomicBool,
}

#[async_trait]
impl ProjectionReads for HookedReads {
    async fn get_user_input_activity(&self, a: &ThreadId, b: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, PersistenceError> {
        ProjectionReads::get_user_input_activity(&self.inner, a, b).await
    }
    async fn list_activities_by_kind(&self, kind: &str) -> Result<Vec<OrchestrationThreadActivity>, PersistenceError> {
        ProjectionReads::list_activities_by_kind(&self.inner, kind).await
    }
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, PersistenceError> {
        ProjectionReads::get_command_read_model(&self.inner).await
    }
    async fn get_snapshot(&self) -> Result<OrchestrationReadModel, PersistenceError> {
        ProjectionReads::get_snapshot(&self.inner).await
    }
    async fn get_shell_snapshot(&self, unsettled_only: bool) -> Result<OrchestrationShellSnapshot, PersistenceError> {
        let snapshot = ProjectionReads::get_shell_snapshot(&self.inner, unsettled_only).await;
        if self.during_snapshot.swap(false, Ordering::SeqCst) {
            // Published after the snapshot was read but before the stream is returned.
            self.engine.message("t1", "m-during-snapshot", "late").await;
        }
        snapshot
    }
    async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot, PersistenceError> {
        ProjectionReads::get_archived_shell_snapshot(&self.inner).await
    }
    async fn list_threads_with_pull_requests(&self) -> Result<Vec<ThreadPullRequests>, PersistenceError> {
        ProjectionReads::list_threads_with_pull_requests(&self.inner).await
    }
    async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>, PersistenceError> {
        ProjectionReads::get_deleted_worktree_threads(&self.inner).await
    }
    async fn search_threads(&self, input: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, PersistenceError> {
        ProjectionReads::search_threads(&self.inner, input).await
    }
    async fn get_snapshot_sequence(&self) -> Result<i64, PersistenceError> {
        ProjectionReads::get_snapshot_sequence(&self.inner).await
    }
    async fn get_counts(&self) -> Result<SnapshotCounts, PersistenceError> {
        ProjectionReads::get_counts(&self.inner).await
    }
    async fn get_event_replay_stats(&self, a: i64, b: i64) -> Result<ReplayStats, PersistenceError> {
        ProjectionReads::get_event_replay_stats(&self.inner, a, b).await
    }
    async fn get_active_project_by_workspace_root(&self, root: &str) -> Result<Option<OrchestrationProject>, PersistenceError> {
        ProjectionReads::get_active_project_by_workspace_root(&self.inner, root).await
    }
    async fn get_project_shell_by_id(&self, id: &ProjectId) -> Result<Option<OrchestrationProjectShell>, PersistenceError> {
        ProjectionReads::get_project_shell_by_id(&self.inner, id).await
    }
    async fn get_project_shells(&self, ids: Option<Vec<ProjectId>>) -> Result<Vec<OrchestrationProjectShell>, PersistenceError> {
        ProjectionReads::get_project_shells(&self.inner, ids).await
    }
    async fn get_first_active_thread_id_by_project_id(&self, id: &ProjectId) -> Result<Option<ThreadId>, PersistenceError> {
        ProjectionReads::get_first_active_thread_id_by_project_id(&self.inner, id).await
    }
    async fn get_imported_agent_session_sources(&self, id: &ProjectId) -> Result<Vec<ImportedAgentSessionSource>, PersistenceError> {
        ProjectionReads::get_imported_agent_session_sources(&self.inner, id).await
    }
    async fn get_thread_checkpoint_context(&self, id: &ThreadId) -> Result<Option<ThreadCheckpointContext>, PersistenceError> {
        ProjectionReads::get_thread_checkpoint_context(&self.inner, id).await
    }
    async fn get_full_thread_diff_context(&self, id: &ThreadId, count: i64) -> Result<Option<FullThreadDiffContext>, PersistenceError> {
        ProjectionReads::get_full_thread_diff_context(&self.inner, id, count).await
    }
    async fn get_thread_shell_by_id(&self, id: &ThreadId) -> Result<Option<OrchestrationThreadShell>, PersistenceError> {
        ProjectionReads::get_thread_shell_by_id(&self.inner, id).await
    }
    async fn get_thread_runtime_context(&self, id: &ThreadId) -> Result<Option<ThreadRuntimeContext>, PersistenceError> {
        ProjectionReads::get_thread_runtime_context(&self.inner, id).await
    }
    async fn get_turn_start_message(&self, a: &ThreadId, b: &MessageId) -> Result<Option<TurnStartMessage>, PersistenceError> {
        ProjectionReads::get_turn_start_message(&self.inner, a, b).await
    }
    async fn get_thread_detail_by_id(&self, id: &ThreadId, query: ThreadDetailQuery) -> Result<Option<OrchestrationThread>, PersistenceError> {
        ProjectionReads::get_thread_detail_by_id(&self.inner, id, query).await
    }
    async fn get_thread_detail_snapshot(
        &self,
        id: &ThreadId,
        window: Option<OrchestrationThreadDetailWindow>,
    ) -> Result<Option<OrchestrationThreadDetailSnapshot>, PersistenceError> {
        ProjectionReads::get_thread_detail_snapshot(&self.inner, id, window).await
    }
}

async fn setup() -> (Arc<FakeEngine>, Arc<HookedReads>, OrchestrationSubscriptions) {
    let engine = FakeEngine::new();
    engine.project("p1").await;
    engine.thread("t1").await;
    engine.thread("t2").await;
    let reads = Arc::new(HookedReads {
        inner: ProjectionSnapshotQuery::new(engine.db.clone(), Arc::new(NoRepositoryIdentities), Arc::new(NoThreadLiveState)),
        engine: Arc::clone(&engine),
        during_snapshot: AtomicBool::new(false),
    });
    let subscriptions = OrchestrationSubscriptions::new(engine.clone(), reads.clone());
    (engine, reads, subscriptions)
}

fn shell_label(item: &OrchestrationShellStreamItem) -> String {
    match item {
        OrchestrationShellStreamItem::Snapshot(snapshot) => format!("snapshot@{}", snapshot.snapshot.snapshot_sequence),
        OrchestrationShellStreamItem::Synchronized(_) => "synchronized".into(),
        OrchestrationShellStreamItem::OrchestrationShellStreamEvent(event) => match event {
            OrchestrationShellStreamEvent::ThreadUpserted(e) => format!("thread-upserted:{}@{}", e.thread.id, e.sequence),
            OrchestrationShellStreamEvent::ThreadRemoved(e) => format!("thread-removed:{}@{}", e.thread_id, e.sequence),
            OrchestrationShellStreamEvent::ProjectUpserted(e) => format!("project-upserted:{}@{}", e.project.id, e.sequence),
            OrchestrationShellStreamEvent::ProjectRemoved(e) => format!("project-removed:{}@{}", e.project_id, e.sequence),
        },
    }
}

fn thread_label(item: &OrchestrationThreadStreamItem) -> String {
    match item {
        OrchestrationThreadStreamItem::Snapshot(snapshot) => format!("snapshot@{}", snapshot.snapshot.snapshot_sequence),
        OrchestrationThreadStreamItem::Synchronized(_) => "synchronized".into(),
        OrchestrationThreadStreamItem::Event(event) => {
            let value = serde_json::to_value(&event.event).unwrap();
            format!("{}@{}", value["type"].as_str().unwrap(), value["sequence"])
        }
    }
}

async fn next<T>(stream: &mut BoxStream<'static, Result<T, OrchestrationGetSnapshotError>>) -> Result<T, OrchestrationGetSnapshotError> {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("an item within 5 s")
        .expect("the stream continues")
}

fn shell_input(value: Value) -> zc_contracts::OrchestrationSubscribeShellInput {
    serde_json::from_value(value).unwrap()
}

fn thread_input(value: Value) -> zc_contracts::OrchestrationSubscribeThreadInput {
    serde_json::from_value(value).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_sends_a_snapshot_then_the_marker_then_coalesced_live_updates() {
    let (engine, reads, subscriptions) = setup().await;
    let head = engine.head().await;
    // An event published while the snapshot is read is delivered after it, not lost.
    reads.during_snapshot.store(true, Ordering::SeqCst);
    let mut stream = subscriptions
        .subscribe_shell(shell_input(json!({"requestCompletionMarker": true})))
        .await
        .unwrap();
    assert_eq!(shell_label(&next(&mut stream).await.unwrap()), format!("snapshot@{head}"));
    assert_eq!(shell_label(&next(&mut stream).await.unwrap()), format!("thread-upserted:t1@{}", head + 1));
    assert_eq!(shell_label(&next(&mut stream).await.unwrap()), "synchronized");

    // A burst for one thread collapses into one refetch at the latest sequence.
    for n in 0..20 {
        engine.message("t2", &format!("burst-{n}"), "x").await;
    }
    engine
        .emit("thread.archived", "t1", json!({"threadId": "t1", "archivedAt": NOW, "updatedAt": NOW}))
        .await;
    let mut labels = Vec::new();
    while labels.len() < 2 {
        labels.push(shell_label(&next(&mut stream).await.unwrap()));
    }
    assert_eq!(
        labels,
        vec![format!("thread-upserted:t2@{}", head + 21), format!("thread-removed:t1@{}", head + 22)]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_resumes_within_the_gap_and_snapshots_past_it() {
    let (engine, _reads, subscriptions) = setup().await;
    let after = engine.head().await;
    engine.message("t1", "m1", "one").await;
    engine.message("t1", "m2", "two").await;
    engine.message("t2", "m3", "three").await;
    let head = engine.head().await;

    // Within the gap: replay, coalesced per aggregate, no snapshot.
    let mut stream = subscriptions
        .subscribe_shell(shell_input(json!({"afterSequence": after, "requestCompletionMarker": true})))
        .await
        .unwrap();
    assert_eq!(shell_label(&next(&mut stream).await.unwrap()), format!("thread-upserted:t1@{}", head - 1));
    assert_eq!(shell_label(&next(&mut stream).await.unwrap()), format!("thread-upserted:t2@{head}"));
    assert_eq!(shell_label(&next(&mut stream).await.unwrap()), "synchronized");

    // A cursor ahead of the head is invalid: snapshot.
    let mut stream = subscriptions.subscribe_shell(shell_input(json!({"afterSequence": head + 5}))).await.unwrap();
    assert_eq!(shell_label(&next(&mut stream).await.unwrap()), format!("snapshot@{head}"));

    // More than 1,000 events behind: snapshot.
    for n in 0..1_001 {
        engine.message("t1", &format!("bulk-{n}"), "x").await;
    }
    let head = engine.head().await;
    let mut stream = subscriptions.subscribe_shell(shell_input(json!({"afterSequence": after}))).await.unwrap();
    assert_eq!(shell_label(&next(&mut stream).await.unwrap()), format!("snapshot@{head}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn shell_resume_over_the_payload_budget_sends_a_snapshot() {
    let (engine, _reads, subscriptions) = setup().await;
    let after = engine.head().await;
    engine.message("t1", "huge", &"x".repeat(9 * 1024 * 1024)).await;
    let head = engine.head().await;
    let mut stream = subscriptions.subscribe_shell(shell_input(json!({"afterSequence": after}))).await.unwrap();
    assert_eq!(shell_label(&next(&mut stream).await.unwrap()), format!("snapshot@{head}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_replays_only_its_detail_events_within_budget() {
    let (engine, _reads, subscriptions) = setup().await;
    let after = engine.head().await;
    let first = engine.message("t1", "m1", "one").await;
    engine.message("t2", "other", "elsewhere").await;
    engine
        .emit("thread.pinned", "t1", json!({"threadId": "t1", "pinnedAt": NOW, "updatedAt": NOW}))
        .await;
    let second = engine.message("t1", "m2", "two").await;
    let mut stream = subscriptions
        .subscribe_thread(thread_input(json!({"threadId": "t1", "afterSequence": after, "requestCompletionMarker": true})))
        .await
        .unwrap();
    assert_eq!(thread_label(&next(&mut stream).await.unwrap()), format!("thread.message-sent@{first}"));
    assert_eq!(thread_label(&next(&mut stream).await.unwrap()), format!("thread.message-sent@{second}"));
    assert_eq!(thread_label(&next(&mut stream).await.unwrap()), "synchronized");
    // Live events follow, filtered to this thread's detail events.
    engine.message("t2", "other-2", "elsewhere").await;
    let live = engine.message("t1", "m3", "three").await;
    assert_eq!(thread_label(&next(&mut stream).await.unwrap()), format!("thread.message-sent@{live}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_resume_snapshots_when_the_range_recreates_the_thread_or_is_too_big() {
    let (engine, _reads, subscriptions) = setup().await;
    let after = engine.head().await;
    engine.emit("thread.deleted", "t1", json!({"threadId": "t1", "deletedAt": NOW})).await;
    engine.thread("t1").await;
    engine.message("t1", "fresh", "new life").await;
    let head = engine.head().await;
    let mut stream = subscriptions
        .subscribe_thread(thread_input(json!({"threadId": "t1", "afterSequence": after})))
        .await
        .unwrap();
    assert_eq!(thread_label(&next(&mut stream).await.unwrap()), format!("snapshot@{head}"));

    // Re-created, then deleted again: no snapshot exists, the bounded replay is kept.
    engine.emit("thread.deleted", "t1", json!({"threadId": "t1", "deletedAt": NOW})).await;
    let mut stream = subscriptions
        .subscribe_thread(thread_input(json!({"threadId": "t1", "afterSequence": after})))
        .await
        .unwrap();
    assert_eq!(thread_label(&next(&mut stream).await.unwrap()), format!("thread.message-sent@{head}"));

    // Over the 8 MiB payload budget: snapshot (t2 still exists).
    let after = engine.head().await;
    engine.message("t2", "huge", &"y".repeat(9 * 1024 * 1024)).await;
    let head = engine.head().await;
    let mut stream = subscriptions
        .subscribe_thread(thread_input(json!({"threadId": "t2", "afterSequence": after, "turnLimit": 2})))
        .await
        .unwrap();
    match next(&mut stream).await.unwrap() {
        OrchestrationThreadStreamItem::Snapshot(snapshot) => {
            assert_eq!(snapshot.snapshot.snapshot_sequence, head);
            assert!(snapshot.snapshot.page.is_some(), "turnLimit windows the fallback snapshot");
        }
        other => panic!("expected a snapshot, got {}", thread_label(&other)),
    }

    // A thread that does not exist fails the subscription.
    let error = subscriptions.subscribe_thread(thread_input(json!({"threadId": "nope"}))).await.err().unwrap();
    assert_eq!(error.message, "Thread nope was not found");
}

#[tokio::test(flavor = "multi_thread")]
async fn thread_live_tool_updates_coalesce_until_another_event_flushes_them() {
    let (engine, _reads, subscriptions) = setup().await;
    let mut stream = subscriptions.subscribe_thread(thread_input(json!({"threadId": "t1"}))).await.unwrap();
    assert!(thread_label(&next(&mut stream).await.unwrap()).starts_with("snapshot@"));
    for n in 0..5 {
        engine.tool_update("t1", &format!("u{n}"), "call-1").await;
    }
    let flush = engine.message("t1", "done", "finished").await;
    let first = next(&mut stream).await.unwrap();
    let OrchestrationThreadStreamItem::Event(event) = &first else {
        panic!("expected an event")
    };
    let value = serde_json::to_value(&event.event).unwrap();
    assert_eq!(value["payload"]["activity"]["id"], "u4", "only the latest update of the run survives");
    assert_eq!(thread_label(&next(&mut stream).await.unwrap()), format!("thread.message-sent@{flush}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_live_tail_past_the_budget_fails_with_resume_from_last_sequence() {
    let (engine, _reads, subscriptions) = setup().await;
    let mut stream = subscriptions.subscribe_shell(shell_input(json!({}))).await.unwrap();
    assert!(shell_label(&next(&mut stream).await.unwrap()).starts_with("snapshot@"));
    // The client stops reading: the buffer fills.
    for n in 0..1_001 {
        engine.message("t1", &format!("flood-{n}"), "x").await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let error = loop {
        match next(&mut stream).await {
            Ok(_) => continue,
            Err(error) => break error,
        }
    };
    assert_eq!(error.message, "The live event buffer is full. Resume from the last received sequence.");
    assert!(tokio::time::timeout(Duration::from_secs(1), stream.next()).await.unwrap().is_none());
}
