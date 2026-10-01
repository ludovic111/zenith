//! Port of `orchestration/PullRequestSyncReactor.test.ts`. `TestClock` is a clock that follows
//! tokio's paused time; `Queue.take(snapshotReads)` waits on the fake projections' read channel.

mod support_reactors;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use support_reactors::*;
use tokio_util::sync::CancellationToken;
use zc_contracts::{OrchestrationShellSnapshot, ThreadPullRequestKey};
use zc_ports::TaggedError;
use zc_pullrequest::reactors::{PullRequestSyncDeps, PullRequestSyncReactor, SWEEP_INTERVAL};
use zc_reactors::settlement::policy::resolve_auto_settlement_at;

const NOW: &str = "2026-08-28T12:00:00.000Z";
const PROJECT_ID: &str = "sync-project";

fn make_project(id: &str) -> Value {
    json!({
        "id": id, "title": format!("Project {id}"), "workspaceRoot": "/workspace/project", "defaultModelSelection": null,
        "scripts": [], "createdAt": "2026-08-01T00:00:00.000Z", "updatedAt": NOW,
    })
}

fn make_thread(id: &str, overrides: Value) -> Value {
    merged(
        json!({
            "id": id, "projectId": PROJECT_ID, "title": id, "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
            "runtimeMode": "full-access", "interactionMode": "default", "branch": null, "worktreePath": null, "pullRequests": [],
            "latestTurn": null, "createdAt": "2026-08-01T00:00:00.000Z", "updatedAt": "2026-08-20T00:00:00.000Z", "archivedAt": null,
            "settledOverride": null, "settledAt": null, "session": null, "latestUserMessageAt": "2026-08-20T00:00:00.000Z",
            "hasPendingApprovals": false, "hasPendingUserInput": false, "hasActionableProposedPlan": false,
        }),
        overrides,
    )
}

/// `makeLink(number, snapshot, overrides)`: `snapshot` `None` is an unsynced link.
fn make_link(number: i64, snapshot: Option<Value>, overrides: Value) -> Value {
    let snapshot = snapshot.map(|snapshot| {
        merged(
            json!({
                "state": "open", "title": "Pull request", "headBranch": "feature", "baseBranch": "main", "isDraft": false,
                "updatedAt": "2026-08-27T00:00:00.000Z", "syncedAt": "2026-08-27T00:00:00.000Z",
            }),
            snapshot,
        )
    });
    merged(
        json!({
            "host": "github.com", "repository": "owner/repository", "number": number,
            "url": format!("https://github.com/owner/repository/pull/{number}"), "source": "manual",
            "linkedAt": "2026-08-10T00:00:00.000Z", "snapshot": snapshot, "stack": null,
        }),
        overrides,
    )
}

fn make_snapshot(threads: Vec<Value>) -> OrchestrationShellSnapshot {
    decode(json!({"snapshotSequence": 1, "projects": [make_project(PROJECT_ID)], "threads": threads, "updatedAt": NOW}))
}

fn make_summary(input: &Value, overrides: Value) -> Value {
    merged(
        json!({
            "provider": "github", "projectId": input["projectId"], "repository": input["repository"], "number": input["number"],
            "title": "Pull request", "url": format!("https://github.com/{}/pull/{}", input["repository"].as_str().unwrap(), input["number"]),
            "state": "open", "headBranch": "feature", "baseBranch": "main", "updatedAt": "2026-08-27T00:00:00.000Z",
        }),
        overrides,
    )
}

fn key(number: i64) -> ThreadPullRequestKey {
    ThreadPullRequestKey {
        host: "github.com".into(),
        repository: "owner/repository".into(),
        number,
    }
}

/// What the reactor would have persisted, so the next sweep sees its own writes.
fn apply_sync(snapshot: &OrchestrationShellSnapshot, commands: &[Value]) -> OrchestrationShellSnapshot {
    let mut value = encode(snapshot);
    value["snapshotSequence"] = json!(snapshot.snapshot_sequence + 1);
    for thread in value["threads"].as_array_mut().unwrap() {
        let thread_id = thread["id"].clone();
        for link in thread["pullRequests"].as_array_mut().unwrap() {
            let command = commands
                .iter()
                .rev()
                .find(|command| command["type"] == "thread.pull-request-link.sync" && command["threadId"] == thread_id && command["number"] == link["number"]);
            if let Some(command) = command {
                link["snapshot"] = command["snapshot"].clone();
                link["stack"] = command["stack"].clone();
            }
        }
    }
    decode(value)
}

#[derive(Default)]
struct Options {
    on_dispatch: Option<DispatchHook>,
    invalidate: Option<Hook<Value, ()>>,
    snapshot: Option<OrchestrationShellSnapshot>,
    summary: Option<RefHook<Value>>,
    stack: Option<RefHook<Option<Value>>>,
}

struct Harness {
    reactor: PullRequestSyncReactor,
    projections: Arc<Projections>,
    reads: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Option<String>>>,
    engine: Arc<Engine>,
    pull_requests: Arc<PullRequestFake>,
    activation: Latch,
}

fn harness(options: Options) -> Harness {
    let (projections, reads) = Projections::new(options.snapshot.unwrap_or_else(|| make_snapshot(vec![])), None);
    let on_dispatch = options.on_dispatch;
    let engine = Engine::new(Some(hook(move |command: Value| {
        let on_dispatch = on_dispatch.clone();
        async move {
            let kind = command["type"].as_str().unwrap_or("");
            if kind != "thread.pull-request-link.sync" && kind != "thread.pull-request.link" {
                return Err(TaggedError::new("Defect", format!("Unexpected command: {kind}")));
            }
            match on_dispatch {
                Some(on_dispatch) => on_dispatch(command).await,
                None => Ok(()),
            }
        }
    })));
    let pull_requests = Arc::new(PullRequestFake {
        summary_hook: options.summary,
        stack_hook: options.stack,
        invalidate_hook: options.invalidate,
        default_summary: Some(Arc::new(|input: &Value| make_summary(input, json!({})))),
        ..Default::default()
    });
    let reactor = PullRequestSyncReactor::new(
        PullRequestSyncDeps {
            engine: engine.clone(),
            projections: projections.clone(),
            pull_requests: pull_requests.clone(),
            clock: TestClock::at(NOW),
            uuids: counter_uuids(),
            interval: SWEEP_INTERVAL,
        },
        CancellationToken::new(),
    );
    Harness {
        reactor,
        projections,
        reads,
        engine,
        pull_requests,
        activation: Latch::new(),
    }
}

impl Harness {
    async fn start_and_sweep(&self) {
        self.reactor.start_with_activation(Some(self.activation.activation())).await;
        self.activation.open();
        take(&self.reads).await;
        self.reactor.drain().await;
    }

    async fn sweep_again(&self) {
        tokio::time::advance(Duration::from_secs(60)).await;
        take(&self.reads).await;
        self.reactor.drain().await;
    }

    async fn request_and_sweep(&self, number: i64) {
        self.reactor.request_sync(&key(number)).await;
        take(&self.reads).await;
        self.reactor.drain().await;
    }

    fn sync_commands(&self) -> Vec<Value> {
        self.engine.commands_of("thread.pull-request-link.sync")
    }

    fn link_commands(&self) -> Vec<Value> {
        self.engine.commands_of("thread.pull-request.link")
    }

    fn summary_calls(&self) -> Vec<Value> {
        self.pull_requests.summary_calls()
    }

    fn stack_calls(&self) -> Vec<Value> {
        self.pull_requests.stack_calls()
    }

    /// The fakes' `assert.strictEqual(readOptions?.recoverTransientFailure, false)` and
    /// `includeDetails` checks.
    fn assert_read_options(&self) {
        assert!(self.pull_requests.recovery.lock().unwrap().iter().all(|recover| !recover));
        assert!(self.pull_requests.details.lock().unwrap().iter().all(|details| !details));
    }

    fn apply_commands(&self) {
        let commands = self.sync_commands();
        self.projections.update(|snapshot| *snapshot = apply_sync(snapshot, &commands));
    }
}

fn summary_with(overrides: Value) -> Option<RefHook<Value>> {
    Some(hook(move |input: Value| {
        let summary = make_summary(&input, overrides.clone());
        async move { Ok(summary) }
    }))
}

fn merged_summary() -> Option<RefHook<Value>> {
    summary_with(json!({"state": "merged", "mergedAt": NOW}))
}

fn without_command_id(commands: Vec<Value>) -> Vec<Value> {
    commands
        .into_iter()
        .map(|mut command| {
            command.as_object_mut().unwrap().remove("commandId");
            command
        })
        .collect()
}

#[tokio::test(start_paused = true)]
async fn syncs_a_newly_linked_merged_pr_without_waiting_for_the_periodic_sweep() {
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread("one", json!({}))])),
        summary: merged_summary(),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    let link = make_link(42, None, json!({}));
    fixture
        .projections
        .set(make_snapshot(vec![make_thread("one", json!({"pullRequests": [link.clone()]}))]));
    fixture.engine.publish(event(
        "thread.pull-request-linked",
        2,
        "linked",
        "one",
        json!({"threadId": "one", "link": link, "updatedAt": NOW}),
    ));
    take(&fixture.reads).await;
    fixture.reactor.drain().await;
    assert_eq!(fixture.sync_commands()[0]["snapshot"]["state"], "merged");
    assert!(fixture.sync_commands()[0]["commandId"].as_str().unwrap().starts_with("server:pr-sync:one:"));
    fixture.assert_read_options();
}

#[tokio::test(start_paused = true)]
async fn retries_a_failed_stack_read_after_the_summary_becomes_terminal() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let native_stack = json!({
        "id": "stack", "number": 7, "url": "https://github.com/owner/repository/stacks/7", "base": "main",
        "layers": [{"number": 7, "headBranch": "feature", "state": "merged"}],
    });
    let stack = native_stack.clone();
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "one",
            json!({"pullRequests": [make_link(7, None, json!({}))]}),
        )])),
        summary: merged_summary(),
        stack: Some(hook(move |_| {
            let attempt = counter.fetch_add(1, Ordering::SeqCst) + 1;
            let stack = stack.clone();
            async move {
                if attempt == 1 {
                    Err(operation_error("stack", "temporary failure"))
                } else {
                    Ok(Some(stack))
                }
            }
        })),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert!(fixture.sync_commands().is_empty());
    fixture.apply_commands();
    fixture.sweep_again().await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        fixture.sync_commands().last().unwrap()["stack"],
        merged(native_stack, json!({"kind": "native"}))
    );
}

#[tokio::test(start_paused = true)]
async fn retries_a_failed_sibling_link_before_publishing_a_terminal_snapshot() {
    let fail_sibling = Arc::new(AtomicBool::new(true));
    let failing = fail_sibling.clone();
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "one",
            json!({"pullRequests": [make_link(7, Some(json!({"state": "closed"})), json!({}))]}),
        )])),
        summary: merged_summary(),
        stack: Some(hook(|_| async {
            Ok(Some(json!({
                "id": "stack", "number": 7, "url": "https://github.com/owner/repository/stacks/7", "base": "main",
                "layers": [{"number": 7, "headBranch": "feature", "state": "merged"}, {"number": 8, "headBranch": "sibling", "state": "open"}],
            })))
        })),
        on_dispatch: Some(hook(move |command: Value| {
            let fail = command["type"] == "thread.pull-request.link" && failing.load(Ordering::SeqCst);
            async move {
                if fail {
                    Err(TaggedError::new("Defect", "temporary link failure"))
                } else {
                    Ok(())
                }
            }
        })),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert!(fixture.sync_commands().is_empty());
    fail_sibling.store(false, Ordering::SeqCst);
    fixture.sweep_again().await;
    assert_eq!(fixture.sync_commands()[0]["snapshot"]["state"], "merged");
    assert_eq!(fixture.link_commands().last().unwrap()["number"], 8);
}

#[tokio::test(start_paused = true)]
async fn concurrent_stack_reads_persist_a_shared_sibling_once_before_syncing_both_roots() {
    let reads_ready = Latch::new();
    let reads = Arc::new(AtomicUsize::new(0));
    let linked = Arc::new(AtomicBool::new(false));
    let violations = Arc::new(Mutex::new(Vec::<String>::new()));
    let (ready, counter) = (reads_ready.clone(), reads.clone());
    let (linked_flag, recorded) = (linked.clone(), violations.clone());
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "one",
            json!({"pullRequests": [make_link(7, None, json!({})), make_link(8, None, json!({}))]}),
        )])),
        stack: Some(hook(move |_| {
            let (ready, counter) = (ready.clone(), counter.clone());
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) + 1 == 2 {
                    ready.open();
                }
                ready.wait().await;
                Ok(Some(json!({
                    "id": "stack", "number": 7, "url": "https://github.com/owner/repository/stacks/7", "base": "main",
                    "layers": [{"number": 9, "headBranch": "sibling", "state": "open"}],
                })))
            }
        })),
        on_dispatch: Some(hook(move |command: Value| {
            let (linked, recorded) = (linked_flag.clone(), recorded.clone());
            async move {
                if command["type"] == "thread.pull-request.link" {
                    tokio::task::yield_now().await;
                    if linked.swap(true, Ordering::SeqCst) {
                        recorded.lock().unwrap().push("sibling linked twice".into());
                    }
                } else if !linked.load(Ordering::SeqCst) {
                    recorded.lock().unwrap().push(format!("root {} synced before its sibling", command["number"]));
                }
                Ok(())
            }
        })),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert_eq!(fixture.link_commands().len(), 1);
    assert_eq!(fixture.sync_commands().len(), 2);
    assert_eq!(*violations.lock().unwrap(), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn explicit_refresh_reads_a_changed_stack_even_when_its_pr_summary_is_unchanged() {
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "one",
            json!({"pullRequests": [make_link(7, Some(json!({})), json!({}))]}),
        )])),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert_eq!(fixture.stack_calls().len(), 0);
    fixture.request_and_sweep(7).await;
    assert_eq!(fixture.stack_calls().len(), 1);
    // A requested read drops the host cache first.
    assert_eq!(
        *fixture.pull_requests.invalidations.lock().unwrap(),
        vec![json!({"reference": {"projectId": PROJECT_ID, "host": "github.com", "repository": "owner/repository", "number": 7}})]
    );
}

#[tokio::test(start_paused = true)]
async fn snapshots_an_unsynced_link_once_and_writes_it_to_the_thread() {
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "one",
            json!({"pullRequests": [make_link(42, None, json!({}))]}),
        )])),
        summary: summary_with(json!({"title": "Ship it", "isDraft": true})),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert_eq!(
        fixture.summary_calls(),
        vec![json!({"projectId": PROJECT_ID, "host": "github.com", "repository": "owner/repository", "number": 42})]
    );
    assert_eq!(
        without_command_id(fixture.sync_commands()),
        vec![json!({
            "type": "thread.pull-request-link.sync",
            "threadId": "one",
            "host": "github.com",
            "repository": "owner/repository",
            "number": 42,
            "snapshot": {
                "state": "open", "title": "Ship it", "headBranch": "feature", "baseBranch": "main", "isDraft": true,
                "updatedAt": "2026-08-27T00:00:00.000Z", "syncedAt": NOW, "closedAt": null, "mergedAt": null,
            },
            "stack": null,
        })]
    );
    assert_eq!(fixture.stack_calls().len(), 1);
    // Reads only linked threads, never the full shell snapshot of every thread.
    assert_eq!(fixture.projections.shell_snapshot_reads.load(Ordering::SeqCst), 0);
    fixture.assert_read_options();
}

#[tokio::test(start_paused = true)]
async fn asks_the_host_once_for_a_pull_request_shared_by_two_threads() {
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![
            make_thread("one", json!({"pullRequests": [make_link(42, None, json!({}))]})),
            make_thread("two", json!({"pullRequests": [make_link(42, None, json!({"repository": "Owner/Repository"}))]})),
        ])),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert_eq!(fixture.summary_calls().len(), 1);
    let mut commands: Vec<(String, String)> = fixture
        .sync_commands()
        .iter()
        .map(|command| {
            (
                command["threadId"].as_str().unwrap().to_owned(),
                command["repository"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    commands.sort();
    assert_eq!(
        commands,
        vec![("one".into(), "owner/repository".into()), ("two".into(), "Owner/Repository".into())]
    );
}

#[tokio::test(start_paused = true)]
async fn recovers_the_http_port_when_syncing_an_older_forgejo_link() {
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "one",
            json!({"pullRequests": [make_link(42, None, json!({"host": "forge.example", "url": "http://forge.example:3000/owner/repository/pulls/42"}))]}),
        )])),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert_eq!(fixture.summary_calls()[0]["host"], "forge.example:3000");
    assert_eq!(fixture.sync_commands()[0]["host"], "forge.example:3000");
}

#[tokio::test(start_paused = true)]
async fn dispatches_nothing_when_the_host_snapshot_is_unchanged() {
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "one",
            json!({"pullRequests": [make_link(42, None, json!({}))]}),
        )])),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert_eq!(fixture.sync_commands().len(), 1);
    fixture.apply_commands();

    fixture.sweep_again().await;

    // Still open on an active thread, so the host was asked again, but nothing changed.
    assert_eq!(fixture.summary_calls().len(), 2);
    assert_eq!(fixture.stack_calls().len(), 1);
    assert_eq!(fixture.sync_commands().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn refreshes_closed_links_through_the_reactors_project_after_reopening_elsewhere() {
    let stale = Arc::new(AtomicBool::new(true));
    let (invalidated, cached) = (stale.clone(), stale.clone());
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![
            make_thread("first", json!({"pullRequests": [make_link(42, Some(json!({"state": "closed"})), json!({}))]})),
            make_thread(
                "second",
                json!({"projectId": "second-project", "pullRequests": [make_link(42, Some(json!({"state": "closed"})), json!({}))]}),
            ),
        ])),
        invalidate: Some(hook(move |input: Value| {
            if input["reference"]["projectId"] == PROJECT_ID && input["reference"]["host"] == "github.com" {
                invalidated.store(false, Ordering::SeqCst);
            }
            async {}
        })),
        summary: Some(hook(move |input: Value| {
            let state = if cached.load(Ordering::SeqCst) { "closed" } else { "open" };
            let summary = make_summary(&input, json!({"state": state}));
            async move { Ok(summary) }
        })),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert_eq!(fixture.summary_calls().len(), 1);
    fixture.request_and_sweep(42).await;
    fixture.apply_commands();
    let states: Vec<Value> = encode(&fixture.projections.get())["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|thread| thread["pullRequests"][0]["snapshot"]["state"].clone())
        .collect();
    assert_eq!(states, vec![json!("open"), json!("open")]);
}

#[tokio::test(start_paused = true)]
async fn stops_asking_the_host_once_a_pull_request_is_merged() {
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "merged",
            json!({"pullRequests": [make_link(1, Some(json!({"state": "merged"})), json!({}))]}),
        )])),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    fixture.sweep_again().await;

    assert!(fixture.summary_calls().is_empty());
    assert!(fixture.sync_commands().is_empty());

    fixture.request_and_sweep(1).await;

    let numbers: Vec<Value> = fixture.summary_calls().iter().map(|call| call["number"].clone()).collect();
    assert_eq!(numbers, vec![json!(1)]);
}

#[tokio::test(start_paused = true)]
async fn discovers_externally_reopened_pull_requests_after_fifteen_minutes() {
    let state = Arc::new(Mutex::new("closed"));
    let current = state.clone();
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "closed",
            json!({"pullRequests": [make_link(2, Some(json!({"state": "closed"})), json!({}))]}),
        )])),
        summary: Some(hook(move |input: Value| {
            let summary = make_summary(&input, json!({"state": *current.lock().unwrap()}));
            async move { Ok(summary) }
        })),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert_eq!(fixture.summary_calls().len(), 1);
    *state.lock().unwrap() = "open";
    for _ in 0..14 {
        fixture.sweep_again().await;
    }
    assert_eq!(fixture.summary_calls().len(), 1);
    fixture.sweep_again().await;
    assert_eq!(fixture.summary_calls().len(), 2);
    assert_eq!(fixture.sync_commands().last().unwrap()["snapshot"]["state"], "open");
}

#[tokio::test(start_paused = true)]
async fn preserves_a_refresh_requested_while_an_older_host_read_is_in_flight() {
    let (reading, release) = (Latch::new(), Latch::new());
    let calls = Arc::new(AtomicUsize::new(0));
    let (reading_signal, released, counter) = (reading.clone(), release.clone(), calls.clone());
    let fixture = harness(Options {
        summary: Some(hook(move |input: Value| {
            let (reading, release, counter) = (reading_signal.clone(), released.clone(), counter.clone());
            async move {
                let call = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if call == 1 {
                    reading.open();
                    release.wait().await;
                }
                Ok(make_summary(&input, json!({"state": if call == 1 { "closed" } else { "open" }})))
            }
        })),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    fixture.projections.set(make_snapshot(vec![make_thread(
        "closed",
        json!({"pullRequests": [make_link(2, Some(json!({"state": "closed"})), json!({}))]}),
    )]));
    fixture.reactor.request_sync(&key(2)).await;
    reading.wait().await;
    fixture.reactor.request_sync(&key(2)).await;
    release.open();
    fixture.reactor.drain().await;
    assert_eq!(fixture.summary_calls().len(), 2);
    assert_eq!(fixture.sync_commands().last().unwrap()["snapshot"]["state"], "open");
}

#[tokio::test(start_paused = true)]
async fn polls_open_pull_requests_on_settled_threads_every_fifteen_minutes() {
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![make_thread(
            "settled",
            json!({
                "settledOverride": "settled", "settledAt": "2026-08-21T00:00:00.000Z",
                "pullRequests": [make_link(5, Some(json!({"state": "open"})), json!({}))],
            }),
        )])),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    assert_eq!(fixture.summary_calls().len(), 1);

    fixture.sweep_again().await;
    assert_eq!(fixture.summary_calls().len(), 1);

    for _ in 0..13 {
        fixture.sweep_again().await;
    }
    assert_eq!(fixture.summary_calls().len(), 1);

    fixture.sweep_again().await;
    assert_eq!(fixture.summary_calls().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn auto_links_missing_native_stack_layers_and_leaves_dismissed_ones_alone() {
    let stack = json!({
        "id": "stack-1", "number": 42, "url": "https://github.com/owner/repository/stack/1", "base": "main",
        "layers": [
            {"number": 41, "headBranch": "layer-1", "state": "merged"},
            {"number": 42, "headBranch": "layer-2", "state": "merged"},
            {"number": 43, "headBranch": "layer-3", "state": "open"},
        ],
    });
    let thread = make_thread(
        "one",
        json!({"pullRequests": [
            make_link(42, Some(json!({"state": "open"})), json!({})),
            make_link(41, Some(json!({"state": "merged"})), json!({"source": "stack-dismissed"})),
        ]}),
    );
    // The thread as each dispatch leaves it.
    let current = Arc::new(Mutex::new(thread.clone()));
    let settled_early = Arc::new(Mutex::new(Vec::<Value>::new()));
    let (state, recorded) = (current.clone(), settled_early.clone());
    let served = stack.clone();
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![thread])),
        summary: merged_summary(),
        stack: Some(hook(move |_| {
            let stack = served.clone();
            async move { Ok(Some(stack)) }
        })),
        on_dispatch: Some(hook(move |command: Value| {
            let mut thread = state.lock().unwrap();
            if command["type"] == "thread.pull-request.link" {
                thread["pullRequests"]
                    .as_array_mut()
                    .unwrap()
                    .push(make_link(command["number"].as_i64().unwrap(), None, json!({})));
            } else {
                let applied = apply_sync(&make_snapshot(vec![thread.clone()]), std::slice::from_ref(&command));
                *thread = encode(&applied)["threads"][0].clone();
            }
            // Every projected event may wake settlement, including the terminal root update.
            if resolve_auto_settlement_at(&thread, None, NOW, None, true).is_some() {
                recorded.lock().unwrap().push(command["type"].clone());
            }
            async { Ok(()) }
        })),
        ..Default::default()
    });
    fixture.start_and_sweep().await;

    let syncs: Vec<(Value, Value)> = fixture
        .sync_commands()
        .iter()
        .map(|command| (command["number"].clone(), command["stack"].clone()))
        .collect();
    assert_eq!(syncs, vec![(json!(42), merged(stack, json!({"kind": "native"})))]);
    assert_eq!(
        without_command_id(fixture.link_commands()),
        vec![json!({
            "type": "thread.pull-request.link",
            "threadId": "one",
            "host": "github.com",
            "repository": "owner/repository",
            "number": 43,
            "url": "https://github.com/owner/repository/pull/43",
            "source": "stack",
        })]
    );
    assert!(fixture.link_commands()[0]["commandId"]
        .as_str()
        .unwrap()
        .starts_with("server:pr-stack-link:one:"));
    assert_eq!(*settled_early.lock().unwrap(), Vec::<Value>::new());
}

#[tokio::test(start_paused = true)]
async fn keeps_existing_snapshots_and_continues_when_the_host_fails() {
    let fixture = harness(Options {
        snapshot: Some(make_snapshot(vec![
            make_thread("failing", json!({"pullRequests": [make_link(7, Some(json!({"state": "open"})), json!({}))]})),
            make_thread("fine", json!({"pullRequests": [make_link(8, None, json!({}))]})),
        ])),
        summary: Some(hook(|input: Value| async move {
            if input["number"] == 7 {
                Err(operation_error("summary", "host down"))
            } else {
                Ok(make_summary(&input, json!({})))
            }
        })),
        ..Default::default()
    });
    fixture.start_and_sweep().await;
    let numbers: Vec<Value> = fixture.sync_commands().iter().map(|command| command["number"].clone()).collect();
    assert_eq!(numbers, vec![json!(8)]);
}
