//! The reactors end to end over the real provider service: the engine on an in-memory
//! database, `zc_providers::ProviderServiceImpl` routing to a scripted Codex adapter (the
//! zc-providers test double; no provider CLI runs), and the orchestration reactor starting
//! ingestion, the command reactor and the deletion reactor.

mod common;

#[allow(unused_imports)]
#[path = "../../zc-providers/tests/common/mod.rs"]
mod provider_fakes;

use std::sync::Arc;
use std::time::Duration;

use common::command::*;
use common::*;
use provider_fakes::{Call, FakeAdapter};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zc_ports::adapter::ProviderAdapter;
use zc_ports::OrchestrationDispatch;
use zc_providers::adapter_registry::StaticAdapterRegistry;
use zc_providers::directory::ProviderSessionDirectory;
use zc_providers::{ProviderServiceImpl, ProviderServiceOptions};
use zc_reactors::command_reactor::CommandReactorDeps;
use zc_reactors::common::system_uuids;
use zc_reactors::registries::{ThreadBackgroundLivenessRegistry, ThreadPlanProgressRegistry};
use zc_reactors::{
    EventLogReactorReads, FixedRepositoryProbe, IngestionDeps, ManualClock, OrchestrationReactor, ProviderCommandReactor, ProviderRuntimeIngestion,
    ReactorSlot, ThreadDeletionReactor,
};

struct Stack {
    engine: Arc<dyn OrchestrationDispatch>,
    reads: Arc<EventLogReactorReads>,
    codex: Arc<FakeAdapter>,
    reactor: OrchestrationReactor,
    _dir: tempfile::TempDir,
}

impl Stack {
    async fn thread(&self, thread_id: &str) -> Value {
        let model = self.reads.read_model().await.expect("read model");
        find(&model["threads"], |thread| s(thread, "id") == thread_id).cloned().unwrap_or(Value::Null)
    }

    async fn wait_for_thread(&self, thread_id: &str, predicate: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let thread = self.thread(thread_id).await;
            if predicate(&thread) {
                return thread;
            }
            assert!(tokio::time::Instant::now() < deadline, "timed out; last thread state: {thread:#}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn calls(&self) -> Vec<Call> {
        self.codex.calls()
    }
}

async fn stack() -> Stack {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let liveness = Arc::new(ThreadBackgroundLivenessRegistry::new());
    let (db, engine) = engine(liveness.clone()).await;
    let engine: Arc<dyn OrchestrationDispatch> = Arc::new(engine);
    let reads = Arc::new(EventLogReactorReads::new(engine.clone()));

    let codex = FakeAdapter::new("codex");
    let registry = Arc::new(StaticAdapterRegistry::new(vec![("codex".into(), codex.clone() as Arc<dyn ProviderAdapter>)]));
    let service = Arc::new(
        ProviderServiceImpl::start(
            registry,
            ProviderSessionDirectory::new(db),
            ProviderServiceOptions::new(dir.path().join("attachments")),
        )
        .await,
    );

    let settings = MemorySettings::new(json!({}));
    let terminals = Arc::new(FakeTerminals::default());
    let clock = Arc::new(ManualClock::shifted());
    let stop = CancellationToken::new();
    let ingestion = Arc::new(ProviderRuntimeIngestion::new(
        IngestionDeps {
            engine: engine.clone(),
            reads: reads.clone(),
            providers: service.clone(),
            settings: settings.clone(),
            repositories: Arc::new(FixedRepositoryProbe(false)),
            liveness,
            plan_progress: Arc::new(ThreadPlanProgressRegistry::new()),
            clock: clock.clone(),
            uuids: system_uuids(),
        },
        stop.child_token(),
    ));
    let command_reactor = Arc::new(ProviderCommandReactor::new(
        CommandReactorDeps {
            engine: engine.clone(),
            reads: reads.clone(),
            providers: service.clone(),
            provider_status: Arc::new(StatusReads(vec![json!({"instanceId": "codex"})])),
            provider_auth: Arc::new(FakeAuth {
                calls: Recorder::default(),
                hook: std::sync::Mutex::new(None),
            }),
            workspace_snapshots: None,
            git: Arc::new(FakeGit::default()),
            vcs_status: Arc::new(FakeVcsStatus::default()),
            text_generation: Arc::new(zc_reactors::NoTextGeneration),
            settings: settings.clone(),
            terminals: terminals.clone(),
            clock,
            uuids: system_uuids(),
            path_exists: Arc::new(|path: &str| std::path::Path::new(path).exists()),
            title_retry_base: Duration::from_millis(10),
        },
        stop.child_token(),
    ));
    let deletion = Arc::new(ThreadDeletionReactor::new(engine.clone(), service.clone(), terminals, stop.child_token()));
    let reactor = OrchestrationReactor {
        ingestion: Some(ingestion),
        command_reactor: Some(command_reactor),
        deletion: Some(deletion),
        ..Default::default()
    };

    let selection = json!({"instanceId": "codex", "model": "gpt-5-codex"});
    ok(dispatch(
        &*engine,
        json!({
            "type": "project.create", "commandId": "cmd-project", "projectId": "project-1", "title": "Project",
            "workspaceRoot": workspace.to_string_lossy(), "defaultModelSelection": selection, "createdAt": NOW,
        }),
    )
    .await);
    ok(dispatch(
        &*engine,
        json!({
            "type": "thread.create", "commandId": "cmd-thread", "threadId": "thread-1", "projectId": "project-1",
            "title": "Thread", "modelSelection": selection, "interactionMode": "default", "runtimeMode": "full-access",
            "branch": null, "worktreePath": null, "createdAt": NOW,
        }),
    )
    .await);
    Stack {
        engine,
        reads,
        codex,
        reactor,
        _dir: dir,
    }
}

#[tokio::test]
async fn a_turn_runs_through_the_provider_service_and_comes_back_as_domain_events() {
    let h = stack().await;
    assert_eq!(
        h.reactor.start().await,
        vec![
            ReactorSlot::ProviderRuntimeIngestion,
            ReactorSlot::ProviderCommandReactor,
            ReactorSlot::ThreadDeletionReactor
        ]
    );

    let mut turn = turn_start_for("thread-1", "cmd-turn-1", "message-user-1", "Say hello", NOW);
    turn["runtimeMode"] = json!("full-access");
    ok(dispatch(&*h.engine, turn).await);

    // The command reactor started a session through the service and sent the turn.
    wait_for(|| async { h.calls().iter().any(|call| matches!(call, Call::SendTurn(_))) }).await;
    let calls = h.calls();
    let Call::StartSession(start) = &calls[0] else {
        panic!("first call: {:?}", calls[0])
    };
    assert_eq!(start.thread_id.as_str(), "thread-1");
    assert_eq!(start.provider_instance_id.as_ref().map(|id| id.as_str()), Some("codex"));
    let Call::SendTurn(send) = calls.iter().find(|call| matches!(call, Call::SendTurn(_))).unwrap() else {
        unreachable!()
    };
    assert_eq!(send.input.as_deref(), Some("Say hello"));
    // The session waits in "starting" for the provider to report the turn.
    h.wait_for_thread("thread-1", |thread| thread["session"]["status"] == "starting").await;

    // The adapter streams the answer; ingestion turns it into domain events.
    let base = |kind: &str, id: &str, payload: Value| json!({"type": kind, "eventId": id, "provider": "codex", "providerInstanceId": "codex", "threadId": "thread-1", "turnId": "turn-thread-1", "itemId": "item-answer", "payload": payload});
    h.codex.emit_json(base("turn.started", "evt-turn-started", json!({})));
    let running = h.wait_for_thread("thread-1", |thread| thread["session"]["status"] == "running").await;
    assert_eq!(running["session"]["activeTurnId"], "turn-thread-1");
    h.codex
        .emit_json(base("content.delta", "evt-delta-1", json!({"streamKind": "assistant_text", "delta": "Hello"})));
    h.codex
        .emit_json(base("content.delta", "evt-delta-2", json!({"streamKind": "assistant_text", "delta": " there"})));
    h.codex.emit_json(base(
        "item.completed",
        "evt-item-completed",
        json!({"itemType": "assistant_message", "status": "completed"}),
    ));
    h.codex.emit_json(base("turn.completed", "evt-turn-completed", json!({"state": "completed"})));

    let done = h
        .wait_for_thread("thread-1", |thread| {
            thread["latestTurn"]["state"] == "completed"
                && thread["messages"]
                    .as_array()
                    .is_some_and(|messages| messages.iter().any(|m| m["role"] == "assistant" && m["streaming"] == false))
        })
        .await;
    let assistant = find(&done["messages"], |message| s(message, "role") == "assistant").unwrap();
    assert_eq!(assistant["text"], "Hello there");
    assert_eq!(assistant["turnId"], "turn-thread-1");
    assert_eq!(done["latestTurn"]["turnId"], "turn-thread-1");
    assert_eq!(done["session"]["status"], "ready");

    // Deleting the thread stops its provider session through the service.
    let deleted = ok(dispatch(&*h.engine, json!({"type": "thread.delete", "commandId": "cmd-delete", "threadId": "thread-1"})).await);
    h.reactor.deletion.as_ref().unwrap().drain_through(deleted).await;
    assert!(h
        .calls()
        .iter()
        .any(|call| matches!(call, Call::StopSession(thread) if thread.as_str() == "thread-1")));
    h.reactor.stop();
}

#[tokio::test]
async fn interrupt_and_approval_commands_reach_the_adapter() {
    let h = stack().await;
    h.reactor.start().await;
    let mut turn = turn_start_for("thread-1", "cmd-turn-1", "message-user-1", "Run the tests", NOW);
    turn["runtimeMode"] = json!("full-access");
    ok(dispatch(&*h.engine, turn).await);
    wait_for(|| async { h.calls().iter().any(|call| matches!(call, Call::SendTurn(_))) }).await;
    h.codex.emit_json(json!({
        "type": "turn.started", "eventId": "evt-turn-started", "provider": "codex", "providerInstanceId": "codex", "threadId": "thread-1",
        "turnId": "turn-thread-1", "payload": {},
    }));
    h.wait_for_thread("thread-1", |thread| thread["session"]["status"] == "running").await;

    h.codex.emit_json(json!({
        "type": "request.opened", "eventId": "evt-request-opened", "provider": "codex", "providerInstanceId": "codex", "threadId": "thread-1",
        "turnId": "turn-thread-1", "requestId": "request-1",
        "payload": {"requestType": "command_execution_approval", "detail": "npm test"},
    }));
    h.wait_for_thread("thread-1", |thread| {
        thread["activities"]
            .as_array()
            .is_some_and(|activities| activities.iter().any(|a| a["kind"] == "approval.requested"))
    })
    .await;
    ok(dispatch(
        &*h.engine,
        json!({
            "type": "thread.approval.respond", "commandId": "cmd-approve", "threadId": "thread-1", "requestId": "request-1",
            "decision": "accept", "createdAt": NOW,
        }),
    )
    .await);
    wait_for(|| async { h.calls().iter().any(|call| matches!(call, Call::RespondToRequest(..))) }).await;

    ok(dispatch(
        &*h.engine,
        json!({
            "type": "thread.turn.interrupt", "commandId": "cmd-interrupt", "threadId": "thread-1", "turnId": "turn-thread-1", "createdAt": NOW,
        }),
    )
    .await);
    wait_for(|| async { h.calls().iter().any(|call| matches!(call, Call::Interrupt(..))) }).await;
    let interrupt = h.calls().into_iter().find(|call| matches!(call, Call::Interrupt(..))).unwrap();
    let Call::Interrupt(thread, turn) = interrupt else { unreachable!() };
    assert_eq!(thread.as_str(), "thread-1");
    // Like TS, the reactor interrupts by thread only; the adapter resolves the active turn.
    assert_eq!(turn, None);
    h.reactor.stop();
}
