//! Ports of `ThreadDeletionReactor.test.ts` and `OrchestrationReactor.test.ts`, plus the
//! deletion reactor against the real engine.

mod common;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use common::command::*;
use common::*;
use futures::StreamExt;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use zc_contracts::{OrchestrationCommand, OrchestrationEvent};
use zc_ports::contracts as ports;
use zc_ports::orchestration::{ThreadReplayRange, ThreadReplayStats};
use zc_ports::provider::SessionModelSwitchMode;
use zc_ports::{DispatchResult, EventStream, OrchestrationDispatch, TaggedError};
use zc_reactors::reactor::{ExternalReactor, ReactorSlot};
use zc_reactors::registries::ThreadBackgroundLivenessRegistry;
use zc_reactors::{OrchestrationReactor, ThreadDeletionReactor};

const DELETION_NOW: &str = "2026-01-01T00:00:00.000Z";
const THREAD: &str = "thread-deletion-reactor-drain";

fn deleted_event(sequence: i64) -> OrchestrationEvent {
    serde_json::from_value(json!({
        "sequence": sequence, "eventId": format!("evt-deleted-{sequence}"), "aggregateKind": "thread", "aggregateId": THREAD,
        "type": "thread.deleted", "occurredAt": DELETION_NOW, "commandId": format!("cmd-deleted-{sequence}"), "causationEventId": null,
        "correlationId": format!("cmd-deleted-{sequence}"), "metadata": {}, "payload": {"threadId": THREAD, "deletedAt": DELETION_NOW},
    }))
    .expect("deleted event")
}

/// The TS test's engine stub: `latestSequence` from a ref, domain events from a scripted stream.
struct StreamEngine {
    latest: AtomicI64,
    events: Mutex<Option<mpsc::UnboundedReceiver<OrchestrationEvent>>>,
}

#[async_trait]
impl OrchestrationDispatch for StreamEngine {
    async fn dispatch(&self, _command: OrchestrationCommand, _origin: Option<ports::OrchestrationClientOrigin>) -> Result<DispatchResult, TaggedError> {
        Err(TaggedError::new("Defect", "unexpected dispatch"))
    }
    fn subscribe_domain_events(&self) -> EventStream<OrchestrationEvent> {
        let receiver = self.events.lock().unwrap().take().expect("one subscriber");
        futures::stream::unfold(receiver, |mut receiver| async move { receiver.recv().await.map(|event| (event, receiver)) }).boxed()
    }
    async fn latest_sequence(&self) -> i64 {
        self.latest.load(Ordering::SeqCst)
    }
    fn read_events(&self, _from: i64, _limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        futures::stream::empty().boxed()
    }
    fn read_thread_events(&self, _range: ThreadReplayRange, _limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        futures::stream::empty().boxed()
    }
    async fn get_thread_replay_stats(&self, _range: ThreadReplayRange, _max: u32) -> Result<ThreadReplayStats, TaggedError> {
        Ok(ThreadReplayStats::default())
    }
}

fn providers() -> Arc<CommandProviders> {
    CommandProviders::new(json!({"instanceId": "codex", "model": "gpt-5-codex"}), SessionModelSwitchMode::InSession)
}

#[tokio::test]
async fn waits_for_a_published_deletion_the_subscriber_has_not_consumed_yet() {
    let (sender, receiver) = mpsc::unbounded_channel();
    let engine = Arc::new(StreamEngine {
        latest: AtomicI64::new(0),
        events: Mutex::new(Some(receiver)),
    });
    let providers = providers();
    let first_cleanup = Latch::new();
    let signal = first_cleanup.clone();
    *providers.stop_hook.lock().unwrap() = Some(hook(move |_| {
        let signal = signal.clone();
        async move {
            signal.open();
            Ok(())
        }
    }));
    let terminals = Arc::new(FakeTerminals::default());
    let reactor = Arc::new(ThreadDeletionReactor::new(
        engine.clone(),
        providers.clone(),
        terminals.clone(),
        CancellationToken::new(),
    ));
    reactor.start().await;
    sender.send(deleted_event(1)).unwrap();
    first_cleanup.wait().await;

    // Sequence 1 is cleaned up and the worker is idle; sequence 2 is committed but still in
    // flight to the subscriber.
    engine.latest.store(2, Ordering::SeqCst);
    let drained = tokio::spawn({
        let reactor = reactor.clone();
        async move { reactor.drain_through(2).await }
    });
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(providers.stop_session.count(), 1);
    assert!(!drained.is_finished());

    sender.send(deleted_event(2)).unwrap();
    drained.await.unwrap();
    assert_eq!(providers.stop_session.count(), 2);
    reactor.stop();
}

/// Not in TS (its server never dispatches before `reactors.start`): without a running
/// subscriber nothing can be pending, so `drainThrough` returns instead of waiting forever,
/// both before `start` and after `stop`.
#[tokio::test]
async fn drain_through_returns_when_the_subscriber_is_not_running() {
    let (sender, receiver) = mpsc::unbounded_channel();
    let engine = Arc::new(StreamEngine {
        latest: AtomicI64::new(0),
        events: Mutex::new(Some(receiver)),
    });
    let reactor = Arc::new(ThreadDeletionReactor::new(
        engine.clone(),
        providers(),
        Arc::new(FakeTerminals::default()),
        CancellationToken::new(),
    ));
    tokio::time::timeout(Duration::from_secs(5), reactor.drain_through(7))
        .await
        .expect("drainThrough before start returns");

    reactor.start().await;
    let waiting = tokio::spawn({
        let reactor = reactor.clone();
        async move { reactor.drain_through(7).await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!waiting.is_finished());
    reactor.stop();
    tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .expect("drainThrough returns once stopped")
        .unwrap();
    drop(sender);
}

/// `logCleanupCauseUnlessInterrupted` "swallows ordinary cleanup failures": a failing stop
/// and a failing terminal close neither stop the other step nor the next deletion.
#[tokio::test]
async fn swallows_ordinary_cleanup_failures() {
    let (sender, receiver) = mpsc::unbounded_channel();
    let engine = Arc::new(StreamEngine {
        latest: AtomicI64::new(0),
        events: Mutex::new(Some(receiver)),
    });
    let providers = providers();
    *providers.stop_hook.lock().unwrap() = Some(hook(|_| async { Err(TaggedError::new("ProviderSessionNotFoundError", "cleanup failed")) }));
    let terminals = Arc::new(FakeTerminals::default());
    *terminals.close_hook.lock().unwrap() = Some(hook(|_| async { Err(TaggedError::new("TerminalError", "cleanup failed")) }));
    let reactor = ThreadDeletionReactor::new(engine.clone(), providers.clone(), terminals.clone(), CancellationToken::new());
    reactor.start().await;
    engine.latest.store(2, Ordering::SeqCst);
    sender.send(deleted_event(1)).unwrap();
    sender.send(deleted_event(2)).unwrap();
    reactor.drain_through(2).await;
    assert_eq!(providers.stop_session.count(), 2);
    assert_eq!(terminals.close.calls(), vec![json!({"threadId": THREAD, "deleteHistory": true}); 2]);
    reactor.stop();
}

/// The reactor wired to the real engine: deleting a thread stops its session and closes its
/// terminals; `drainThrough` a later `thread.created` covers the deletion.
#[tokio::test]
async fn deleting_a_thread_on_the_engine_stops_its_session_and_closes_its_terminals() {
    let (_db, engine) = engine(Arc::new(ThreadBackgroundLivenessRegistry::new())).await;
    let engine: Arc<dyn OrchestrationDispatch> = Arc::new(engine);
    let providers = providers();
    let terminals = Arc::new(FakeTerminals::default());
    let reactor = ThreadDeletionReactor::new(engine.clone(), providers.clone(), terminals.clone(), CancellationToken::new());
    reactor.start().await;
    let selection = json!({"instanceId": "codex", "model": "gpt-5-codex"});
    ok(dispatch(
        &*engine,
        json!({
            "type": "project.create", "commandId": "cmd-project", "projectId": "project-1", "title": "Project",
            "workspaceRoot": "/tmp/deletion-project", "defaultModelSelection": selection, "createdAt": NOW,
        }),
    )
    .await);
    for thread in ["thread-1", "thread-2"] {
        ok(dispatch(
            &*engine,
            json!({
                "type": "thread.create", "commandId": format!("cmd-create-{thread}"), "threadId": thread, "projectId": "project-1",
                "title": "Thread", "modelSelection": selection, "interactionMode": "default", "runtimeMode": "full-access",
                "branch": null, "worktreePath": null, "createdAt": NOW,
            }),
        )
        .await);
    }
    ok(dispatch(&*engine, json!({"type": "thread.delete", "commandId": "cmd-delete", "threadId": "thread-1"})).await);
    let created = ok(dispatch(
        &*engine,
        json!({
            "type": "thread.create", "commandId": "cmd-create-thread-3", "threadId": "thread-3", "projectId": "project-1",
            "title": "Thread", "modelSelection": selection, "interactionMode": "default", "runtimeMode": "full-access",
            "branch": null, "worktreePath": null, "createdAt": NOW,
        }),
    )
    .await);
    reactor.drain_through(created).await;
    assert_eq!(providers.stop_session.calls(), vec![json!({"threadId": "thread-1"})]);
    assert_eq!(terminals.close.calls(), vec![json!({"threadId": "thread-1", "deleteHistory": true})]);
    reactor.stop();
}

struct Named(&'static str, Arc<Mutex<Vec<&'static str>>>);

#[async_trait]
impl ExternalReactor for Named {
    async fn start(&self) {
        self.1.lock().unwrap().push(self.0);
    }
}

#[tokio::test]
async fn starts_every_orchestration_reactor() {
    let started = Arc::new(Mutex::new(Vec::new()));
    let slot = |slot: ReactorSlot, name: &'static str| -> (ReactorSlot, Arc<dyn ExternalReactor>) { (slot, Arc::new(Named(name, started.clone()))) };
    // Registered out of order on purpose: the start order comes from the slots.
    let reactor = OrchestrationReactor {
        external: vec![
            slot(ReactorSlot::StorageCleanup, "storage-cleanup"),
            slot(ReactorSlot::AgentAwarenessRelay, "agent-awareness-relay"),
            slot(ReactorSlot::PullRequestSyncReactor, "pull-request-sync-reactor"),
            slot(ReactorSlot::ThreadSettlementReactor, "thread-settlement-reactor"),
            slot(ReactorSlot::ThreadPullRequestReactor, "thread-pull-request-reactor"),
            slot(ReactorSlot::ThreadDeletionReactor, "thread-deletion-reactor"),
            slot(ReactorSlot::CheckpointReactor, "checkpoint-reactor"),
            slot(ReactorSlot::ProviderCommandReactor, "provider-command-reactor"),
            slot(ReactorSlot::ProviderRuntimeIngestion, "provider-runtime-ingestion"),
        ],
        ..Default::default()
    };
    let slots = reactor.start().await;
    assert_eq!(slots, ReactorSlot::ORDER.to_vec());
    assert_eq!(
        *started.lock().unwrap(),
        vec![
            "provider-runtime-ingestion",
            "provider-command-reactor",
            "checkpoint-reactor",
            "thread-deletion-reactor",
            "thread-pull-request-reactor",
            "thread-settlement-reactor",
            "pull-request-sync-reactor",
            "agent-awareness-relay",
            "storage-cleanup",
        ]
    );
}

#[tokio::test]
async fn owned_reactors_take_their_slots_between_external_ones() {
    let (_db, engine) = engine(Arc::new(ThreadBackgroundLivenessRegistry::new())).await;
    let engine: Arc<dyn OrchestrationDispatch> = Arc::new(engine);
    let started = Arc::new(Mutex::new(Vec::new()));
    let deletion = Arc::new(ThreadDeletionReactor::new(
        engine,
        providers(),
        Arc::new(FakeTerminals::default()),
        CancellationToken::new(),
    ));
    let reactor = OrchestrationReactor {
        deletion: Some(deletion),
        external: vec![
            (
                ReactorSlot::StorageCleanup,
                Arc::new(Named("storage-cleanup", started.clone())) as Arc<dyn ExternalReactor>,
            ),
            (ReactorSlot::CheckpointReactor, Arc::new(Named("checkpoint-reactor", started.clone()))),
        ],
        ..Default::default()
    };
    let slots = reactor.start().await;
    assert_eq!(
        slots,
        vec![ReactorSlot::CheckpointReactor, ReactorSlot::ThreadDeletionReactor, ReactorSlot::StorageCleanup]
    );
    assert_eq!(*started.lock().unwrap(), vec!["checkpoint-reactor", "storage-cleanup"]);
    reactor.stop();
}
