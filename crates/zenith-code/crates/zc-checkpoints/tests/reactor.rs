//! Port of `orchestration/Layers/CheckpointReactor.test.ts` on temp git repositories, the real
//! orchestration engine (in-memory SQLite) and a scripted provider service.
//!
//! The TS tests read the SQL projection; here the projection reads are the engine's command
//! read model (WP-09 owns the SQL side), which carries the same thread, checkpoints and
//! activities.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use serde_json::{json, Value};
use zc_checkpoints::reactor::{CheckpointReactor, CheckpointReactorDeps, NoWorkspaceEntries, ReactorTasks, WorkspaceEntriesRefresher};
use zc_checkpoints::receipts::{OrchestrationRuntimeReceipt, RuntimeReceiptBus};
use zc_checkpoints::{checkpoint_ref_for_thread_turn, CheckpointStore, DiffCheckpointsInput};
use zc_contracts::{CheckpointRef, OrchestrationCheckpointStatus, ThreadId};
use zc_core::pubsub::Subscription;
use zc_core::vcs_process::VcsProcessError;
use zc_orchestration::engine::OrchestrationEngine;
use zc_ports::TaggedError;
use zc_vcs::VcsError;

/// A store whose `hasCheckpointRef` can be made to fail (`checkpointLookupFailure`).
struct FlakyStore {
    inner: Arc<dyn CheckpointStore>,
    fail: Arc<AtomicBool>,
    kind: &'static str,
}

#[async_trait]
impl CheckpointStore for FlakyStore {
    async fn is_git_repository(&self, cwd: &str) -> Result<bool, VcsError> {
        self.inner.is_git_repository(cwd).await
    }
    async fn capture_checkpoint(&self, cwd: &str, r: &CheckpointRef) -> Result<(), VcsError> {
        self.inner.capture_checkpoint(cwd, r).await
    }
    async fn has_checkpoint_ref(&self, cwd: &str, r: &CheckpointRef) -> Result<bool, VcsError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(VcsError::Process(match self.kind {
                "timeout" => VcsProcessError::Timeout {
                    operation: "test.refLookup".into(),
                    command: "git".into(),
                    cwd: cwd.into(),
                    argument_count: None,
                    timeout_ms: 30_000,
                },
                _ => VcsProcessError::Spawn {
                    operation: "test.refLookup".into(),
                    command: "git".into(),
                    cwd: cwd.into(),
                    argument_count: None,
                    cause: zc_core::defect::Defect::error("Error", "transient lookup spawn failure"),
                },
            }));
        }
        self.inner.has_checkpoint_ref(cwd, r).await
    }
    async fn restore_checkpoint(&self, cwd: &str, r: &CheckpointRef, fallback: bool) -> Result<bool, VcsError> {
        self.inner.restore_checkpoint(cwd, r, fallback).await
    }
    async fn diff_checkpoints(&self, input: &DiffCheckpointsInput) -> Result<String, VcsError> {
        self.inner.diff_checkpoints(input).await
    }
    async fn delete_checkpoint_refs(&self, cwd: &str, refs: &[CheckpointRef]) -> Result<(), VcsError> {
        self.inner.delete_checkpoint_refs(cwd, refs).await
    }
}

/// Records workspace entry refreshes; the first one can be held.
#[derive(Default)]
struct RecordingEntries {
    calls: Mutex<Vec<String>>,
    hold_first: Option<Arc<tokio::sync::Semaphore>>,
    entered: tokio::sync::Notify,
}

#[async_trait]
impl WorkspaceEntriesRefresher for RecordingEntries {
    async fn refresh(&self, cwd: &str) -> Result<(), String> {
        let first = {
            let mut calls = self.calls.lock().unwrap();
            calls.push(cwd.to_owned());
            calls.len() == 1
        };
        if first {
            if let Some(gate) = &self.hold_first {
                self.entered.notify_waiters();
                let _ = gate.acquire().await;
            }
        }
        Ok(())
    }
}

/// Where a second thread of the project works, given the first one's checkout.
type SecondThread = Box<dyn FnOnce(&Path) -> PathBuf>;

#[derive(Default)]
struct Options {
    lookup_failure: Option<&'static str>,
    entries: Option<Arc<dyn WorkspaceEntriesRefresher>>,
    no_session: bool,
    no_seed: bool,
    no_git: bool,
    project_workspace_root: Option<PathBuf>,
    /// `Some(None)`: the thread has no worktree path.
    thread_worktree_path: Option<Option<PathBuf>>,
    thread_branch: Option<&'static str>,
    second_thread: Option<SecondThread>,
    local_ref_name: Option<&'static str>,
    provider_session_cwd: Option<PathBuf>,
    provider_name: Option<&'static str>,
}

struct Harness {
    _dir: tempfile::TempDir,
    cwd: PathBuf,
    engine: OrchestrationEngine,
    projections: EngineProjections,
    providers: Arc<FakeProviders>,
    vcs: Arc<FakeVcsStatus>,
    prs: Arc<FakePullRequests>,
    reactor: CheckpointReactor,
    _tasks: ReactorTasks,
    receipts: Subscription<OrchestrationRuntimeReceipt>,
    fail_lookup: Arc<AtomicBool>,
}

impl Harness {
    async fn new(options: Options) -> Self {
        let (dir, root) = temp_dir();
        let cwd = root.join("repo");
        create_git_repository(&cwd);
        if options.no_git {
            std::fs::remove_dir_all(cwd.join(".git")).unwrap();
        }
        let session_cwd = options.provider_session_cwd.clone().unwrap_or_else(|| cwd.clone());
        let provider_name = options.provider_name.unwrap_or("codex");
        let providers = FakeProviders::new((!options.no_session).then(|| FakeProviders::session("thread-1", session_cwd.to_str().unwrap(), provider_name)));
        let engine = engine().await;
        let projections = EngineProjections { engine: engine.clone() };
        let fail_lookup = Arc::new(AtomicBool::new(false));
        let store: Arc<dyn CheckpointStore> = match options.lookup_failure {
            Some(kind) => Arc::new(FlakyStore {
                inner: store(),
                fail: fail_lookup.clone(),
                kind,
            }),
            None => store(),
        };
        let vcs = FakeVcsStatus::new(options.local_ref_name);
        let prs = Arc::new(FakePullRequests::default());
        let receipts_bus = RuntimeReceiptBus::for_test();
        let receipts = receipts_bus.subscribe_for_test().unwrap();
        let reactor = CheckpointReactor::new(CheckpointReactorDeps {
            engine: Arc::new(engine.clone()),
            projections: Arc::new(projections.clone()),
            providers: providers.clone(),
            store: store.clone(),
            receipts: receipts_bus,
            workspace_entries: options.entries.clone().unwrap_or_else(|| Arc::new(NoWorkspaceEntries)),
            vcs_status: vcs.clone(),
            pull_requests: prs.clone(),
        });
        let tasks = reactor.start();

        let workspace_root = options.project_workspace_root.clone().unwrap_or_else(|| cwd.clone());
        dispatch(
            &engine,
            json!({"type": "project.create", "commandId": "cmd-project-create", "projectId": "project-1", "title": "Test Project",
                   "workspaceRoot": workspace_root, "defaultModelSelection": model_selection(), "createdAt": NOW}),
        )
        .await;
        let worktree = match &options.thread_worktree_path {
            Some(path) => path.clone(),
            None => Some(cwd.clone()),
        };
        dispatch(
            &engine,
            json!({"type": "thread.create", "commandId": "cmd-thread-create", "threadId": "thread-1", "projectId": "project-1", "title": "Thread",
                   "modelSelection": model_selection(), "interactionMode": "default", "runtimeMode": "approval-required",
                   "branch": options.thread_branch, "worktreePath": worktree, "createdAt": NOW}),
        )
        .await;
        if let Some(second) = options.second_thread {
            let path = second(&cwd);
            dispatch(
                &engine,
                json!({"type": "thread.create", "commandId": "cmd-thread-create-2", "threadId": "thread-2", "projectId": "project-1", "title": "Thread 2",
                       "modelSelection": model_selection(), "interactionMode": "default", "runtimeMode": "approval-required",
                       "branch": null, "worktreePath": path, "createdAt": NOW}),
            )
            .await;
        }

        if !options.no_seed {
            let thread = ThreadId::new("thread-1");
            let cwd_str = cwd.to_str().unwrap();
            store.capture_checkpoint(cwd_str, &checkpoint_ref_for_thread_turn(&thread, 0)).await.unwrap();
            std::fs::write(cwd.join("README.md"), "v2\n").unwrap();
            store.capture_checkpoint(cwd_str, &checkpoint_ref_for_thread_turn(&thread, 1)).await.unwrap();
            std::fs::write(cwd.join("README.md"), "v3\n").unwrap();
            store.capture_checkpoint(cwd_str, &checkpoint_ref_for_thread_turn(&thread, 2)).await.unwrap();
        }

        Self {
            _dir: dir,
            cwd,
            engine,
            projections,
            providers,
            vcs,
            prs,
            reactor,
            _tasks: tasks,
            receipts,
            fail_lookup,
        }
    }

    async fn drain(&self) {
        settle().await;
        self.reactor.drain().await;
        settle().await;
        self.reactor.drain().await;
    }

    async fn next_receipt(&mut self) -> Value {
        let receipt = tokio::time::timeout(std::time::Duration::from_secs(15), self.receipts.recv())
            .await
            .expect("a receipt in time")
            .expect("the bus is open");
        serde_json::to_value(receipt).unwrap()
    }

    async fn dispatch(&self, value: Value) -> i64 {
        dispatch(&self.engine, value).await
    }

    fn emit(&self, event_type: &str, turn_id: &str, extra: Value) {
        self.emit_for("thread-1", event_type, turn_id, extra);
    }

    fn emit_for(&self, thread_id: &str, event_type: &str, turn_id: &str, extra: Value) {
        let mut event = json!({"type": event_type, "eventId": format!("evt-{event_type}-{turn_id}"), "provider": "codex",
                               "createdAt": NOW, "threadId": thread_id, "turnId": turn_id});
        if let Value::Object(extra) = extra {
            event.as_object_mut().unwrap().extend(extra);
        }
        self.providers.emit(event);
    }

    fn reference(&self, turn: i64) -> String {
        checkpoint_ref_for_thread_turn(&ThreadId::new("thread-1"), turn).0
    }

    fn readme(&self) -> String {
        std::fs::read_to_string(self.cwd.join("README.md")).unwrap()
    }

    async fn session_set(&self, status: &str, active_turn_id: Option<&str>) {
        self.dispatch(
            json!({"type": "thread.session.set", "commandId": format!("cmd-session-{status}-{active_turn_id:?}"), "threadId": "thread-1",
            "session": {"threadId": "thread-1", "status": status, "providerName": "codex", "runtimeMode": "approval-required",
                        "activeTurnId": active_turn_id, "lastError": null, "updatedAt": NOW}, "createdAt": NOW}),
        )
        .await;
    }

    async fn diff_complete(&self, turn: i64, reference: &str, status: &str) {
        self.dispatch(
            json!({"type": "thread.turn.diff.complete", "commandId": format!("cmd-diff-{turn}"), "threadId": "thread-1",
            "turnId": format!("turn-{turn}"), "completedAt": NOW, "checkpointRef": reference, "status": status, "files": [],
            "checkpointTurnCount": turn, "createdAt": NOW}),
        )
        .await;
    }

    async fn activities(&self) -> Vec<Value> {
        let thread = self.projections.thread("thread-1").await;
        thread.activities.iter().map(|a| serde_json::to_value(a).unwrap()).collect()
    }
}

fn completed() -> Value {
    json!({"payload": {"state": "completed"}})
}

fn assert_receipt(receipt: &Value, expected: Value) {
    for (key, value) in expected.as_object().unwrap() {
        assert_eq!(&receipt[key], value, "receipt {receipt} field {key}");
    }
}

// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn preserves_sibling_files_when_reverting_a_shared_workspace() {
    for owner in ["active", "archived", "alias", "nested", "ancestor", "project-root", "conversation"] {
        let second: Box<dyn FnOnce(&Path) -> PathBuf> = match owner {
            "ancestor" => Box::new(|cwd: &Path| cwd.parent().unwrap().to_path_buf()),
            "nested" => Box::new(|cwd: &Path| {
                let nested = cwd.join("nested-owner");
                std::fs::create_dir(&nested).unwrap();
                nested
            }),
            "alias" => Box::new(|cwd: &Path| {
                let alias = cwd.with_file_name("repo-alias");
                std::os::unix::fs::symlink(cwd, &alias).unwrap();
                alias
            }),
            _ => Box::new(|cwd: &Path| cwd.to_path_buf()),
        };
        let harness = Harness::new(Options {
            second_thread: Some(second),
            ..Options::default()
        })
        .await;
        if owner == "archived" {
            harness
                .dispatch(json!({"type": "thread.archive", "commandId": "cmd-archive-owner", "threadId": "thread-2"}))
                .await;
        }
        if owner == "project-root" {
            harness
                .dispatch(json!({"type": "thread.meta.update", "commandId": "cmd-root-owner", "threadId": "thread-2", "worktreePath": null}))
                .await;
        }
        let sibling = if owner == "nested" {
            harness.cwd.join("nested-owner").join("sibling-work.txt")
        } else {
            harness.cwd.join("sibling-work.txt")
        };
        std::fs::write(&sibling, "sibling work\n").unwrap();
        harness
            .dispatch(
                json!({"type": if owner == "conversation" { "thread.conversation.revert" } else { "thread.checkpoint.revert" },
                             "commandId": "cmd-shared-revert", "threadId": "thread-1", "turnCount": 0, "createdAt": "2026-01-01T00:00:02.000Z"}),
            )
            .await;
        harness.drain().await;
        assert_eq!(std::fs::read_to_string(&sibling).unwrap(), "sibling work\n", "{owner}");
        assert_eq!(harness.readme(), "v3\n", "{owner}");
        let failure = harness.activities().await.into_iter().find(|a| a["kind"] == "checkpoint.revert.failed");
        if owner == "conversation" {
            assert!(failure.is_none(), "{owner}");
        } else {
            let failure = failure.unwrap_or_else(|| panic!("{owner}: a revert failure"));
            assert!(failure["payload"]["detail"].as_str().unwrap().contains("isolated worktree"), "{owner}");
            assert!(harness.providers.rollbacks().is_empty(), "{owner}");
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_and_finalizes_a_turn_when_previous_checkpoint_lookup_fails() {
    for kind in ["timeout", "spawn"] {
        let mut harness = Harness::new(Options {
            no_seed: true,
            lookup_failure: Some(kind),
            ..Options::default()
        })
        .await;
        harness.emit("turn.started", "turn-ref-timeout", json!({}));
        assert_receipt(&harness.next_receipt().await, json!({"type": "checkpoint.baseline.captured"}));
        std::fs::write(harness.cwd.join("README.md"), "new snapshot\n").unwrap();
        harness.fail_lookup.store(true, Ordering::SeqCst);
        harness.emit("turn.completed", "turn-ref-timeout", completed());
        harness.drain().await;
        assert_eq!(git_show(&harness.cwd, &harness.reference(1), "README.md"), "new snapshot\n");
        assert_receipt(
            &harness.next_receipt().await,
            json!({"type": "checkpoint.diff.finalized", "turnId": "turn-ref-timeout"}),
        );
        assert_receipt(
            &harness.next_receipt().await,
            json!({"type": "turn.processing.quiesced", "turnId": "turn-ref-timeout"}),
        );
        let thread = harness.projections.thread("thread-1").await;
        let checkpoint = serde_json::to_value(&thread.checkpoints[0]).unwrap();
        assert_eq!(checkpoint["checkpointRef"], json!(harness.reference(1)));
        assert_eq!(checkpoint["status"], json!("ready"));
        assert_eq!(checkpoint["files"], json!([]));
        assert!(!harness.activities().await.iter().any(|a| a["kind"] == "checkpoint.capture.failed"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn finalizes_checkpoints_in_both_workspaces_while_entry_refresh_is_blocked_and_coalesces_later_scans() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let entries = Arc::new(RecordingEntries {
        hold_first: Some(gate.clone()),
        ..RecordingEntries::default()
    });
    let second_cwd = Arc::new(Mutex::new(PathBuf::new()));
    let second_for_harness = second_cwd.clone();
    let mut harness = Harness::new(Options {
        no_seed: true,
        entries: Some(entries.clone()),
        second_thread: Some(Box::new(move |cwd: &Path| {
            let second = cwd.with_file_name("second");
            create_git_repository(&second);
            *second_for_harness.lock().unwrap() = second.clone();
            second
        })),
        ..Options::default()
    })
    .await;
    let second_cwd = second_cwd.lock().unwrap().clone();
    let entered = entries.entered.notified();
    tokio::pin!(entered);
    entered.as_mut().enable();
    for (index, thread_id) in ["thread-1", "thread-2", "thread-1", "thread-1", "thread-1"].iter().enumerate() {
        let turn_id = format!("turn-refresh-{index}");
        harness.emit_for(thread_id, "turn.started", &turn_id, json!({}));
        if index < 2 {
            assert_receipt(
                &harness.next_receipt().await,
                json!({"type": "checkpoint.baseline.captured", "threadId": thread_id}),
            );
        }
        let cwd = if *thread_id == "thread-1" { harness.cwd.clone() } else { second_cwd.clone() };
        std::fs::write(cwd.join("README.md"), format!("snapshot {index}\n")).unwrap();
        harness.emit_for(thread_id, "turn.completed", &turn_id, completed());
        assert_receipt(
            &harness.next_receipt().await,
            json!({"type": "checkpoint.diff.finalized", "threadId": thread_id, "turnId": turn_id}),
        );
        assert_receipt(
            &harness.next_receipt().await,
            json!({"type": "turn.processing.quiesced", "threadId": thread_id, "turnId": turn_id}),
        );
        if index == 0 {
            entered.as_mut().await;
        }
    }
    let cwd = harness.cwd.to_str().unwrap().to_owned();
    assert_eq!(*entries.calls.lock().unwrap(), vec![cwd.clone()]);
    assert_eq!(git_show(&harness.cwd, &harness.reference(4), "README.md"), "snapshot 4\n");
    assert_eq!(
        git_show(&second_cwd, &checkpoint_ref_for_thread_turn(&ThreadId::new("thread-2"), 1).0, "README.md"),
        "snapshot 1\n"
    );
    gate.add_permits(1);
    harness.drain().await;
    assert_eq!(*entries.calls.lock().unwrap(), vec![cwd.clone(), second_cwd.to_str().unwrap().to_owned(), cwd]);
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_baseline_and_large_turn_summaries_before_completion_receipts() {
    let mut harness = Harness::new(Options {
        no_seed: true,
        ..Options::default()
    })
    .await;
    harness.session_set("ready", None).await;
    harness.emit("turn.started", "turn-1", json!({}));
    assert_receipt(
        &harness.next_receipt().await,
        json!({"type": "checkpoint.baseline.captured", "checkpointTurnCount": 0}),
    );
    std::fs::write(harness.cwd.join("README.md"), "v2\n").unwrap();
    let lines = 25_000;
    std::fs::write(harness.cwd.join("large.txt"), format!("{}\n", "payload".repeat(64)).repeat(lines)).unwrap();
    harness.emit("turn.completed", "turn-1", completed());
    assert_receipt(
        &harness.next_receipt().await,
        json!({"type": "checkpoint.diff.finalized", "turnId": "turn-1", "checkpointTurnCount": 1}),
    );
    let thread = harness.projections.thread("thread-1").await;
    assert_eq!(
        serde_json::to_value(&thread.checkpoints[0].files).unwrap(),
        json!([
            {"path": "large.txt", "kind": "modified", "additions": lines, "deletions": 0},
            {"path": "README.md", "kind": "modified", "additions": 1, "deletions": 1},
        ])
    );
    assert_receipt(
        &harness.next_receipt().await,
        json!({"type": "turn.processing.quiesced", "turnId": "turn-1", "checkpointTurnCount": 1}),
    );
    harness.drain().await;
    assert_eq!(git_show(&harness.cwd, &harness.reference(0), "README.md"), "v1\n");
    assert_eq!(git_show(&harness.cwd, &harness.reference(1), "README.md"), "v2\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_and_reverts_checkpoints_from_a_nested_git_workspace() {
    let (_outer, root) = temp_dir();
    let repository = root.join("repository");
    create_git_repository(&repository);
    let workspace = repository.join("apps").join("server");
    std::fs::create_dir_all(&workspace).unwrap();
    let file = workspace.join("index.ts");
    std::fs::write(&file, "export const value = 1;\n").unwrap();
    git(&repository, &["add", "."]);
    git(&repository, &["commit", "-m", "Add nested workspace"]);
    let mut harness = Harness::new(Options {
        no_seed: true,
        project_workspace_root: Some(workspace.clone()),
        thread_worktree_path: Some(Some(workspace.clone())),
        provider_session_cwd: Some(workspace.clone()),
        ..Options::default()
    })
    .await;
    harness.emit("turn.started", "turn-nested", json!({}));
    harness.drain().await;
    let reference = |turn| checkpoint_ref_for_thread_turn(&ThreadId::new("thread-1"), turn).0;
    assert!(git_ref_exists(&repository, &reference(0)));
    assert_receipt(&harness.next_receipt().await, json!({"type": "checkpoint.baseline.captured"}));

    std::fs::write(&file, "export const value = 2;\n").unwrap();
    harness.emit("turn.completed", "turn-nested", completed());
    harness.drain().await;
    let thread = harness.projections.thread("thread-1").await;
    let checkpoint = serde_json::to_value(&thread.checkpoints[0]).unwrap();
    assert_eq!(checkpoint["status"], json!("ready"));
    assert_eq!(
        checkpoint["files"],
        json!([{"path": "apps/server/index.ts", "kind": "modified", "additions": 1, "deletions": 1}])
    );
    assert_receipt(
        &harness.next_receipt().await,
        json!({"type": "checkpoint.diff.finalized", "turnId": "turn-nested"}),
    );
    assert_receipt(&harness.next_receipt().await, json!({"type": "turn.processing.quiesced"}));

    harness
        .dispatch(json!({"type": "thread.checkpoint.revert", "commandId": "cmd-nested-revert", "threadId": "thread-1", "turnCount": 0, "createdAt": NOW}))
        .await;
    harness.drain().await;
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "export const value = 1;\n");
    assert_eq!(harness.providers.rollbacks(), vec![("thread-1".to_owned(), 1)]);
    assert!(!git_ref_exists(&repository, &reference(1)));
    assert!(harness.projections.thread("thread-1").await.checkpoints.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_every_edit_after_a_mid_turn_diff_update() {
    for terminal in ["turn.completed", "turn.aborted"] {
        let mut harness = Harness::new(Options {
            no_seed: true,
            ..Options::default()
        })
        .await;
        harness.session_set("running", Some("turn-1")).await;
        harness.emit("turn.started", "turn-1", json!({}));
        assert_receipt(&harness.next_receipt().await, json!({"type": "checkpoint.baseline.captured"}));
        std::fs::write(harness.cwd.join("early.ts"), "export const early = 1;\n").unwrap();
        harness
            .dispatch(
                json!({"type": "thread.turn.diff.complete", "commandId": "cmd-mid-turn-diff", "threadId": "thread-1", "turnId": "turn-1",
                "completedAt": NOW, "checkpointRef": "provider-diff:mid-turn", "assistantMessageId": "assistant:mid-turn", "status": "missing",
                "files": [], "checkpointTurnCount": 1, "createdAt": NOW}),
            )
            .await;
        harness.drain().await;
        std::fs::write(harness.cwd.join("late.ts"), "export const late = 2;\n").unwrap();
        harness
            .session_set(if terminal == "turn.aborted" { "interrupted" } else { "ready" }, None)
            .await;
        let extra = if terminal == "turn.completed" {
            completed()
        } else {
            json!({"payload": {"reason": "Interrupted by user."}})
        };
        harness.emit(terminal, "turn-1", extra);
        harness.drain().await;
        assert!(git_ref_exists(&harness.cwd, &harness.reference(1)), "{terminal}");
        assert_receipt(
            &harness.next_receipt().await,
            json!({"type": "checkpoint.diff.finalized", "turnId": "turn-1", "checkpointTurnCount": 1}),
        );
        assert_receipt(&harness.next_receipt().await, json!({"type": "turn.processing.quiesced", "turnId": "turn-1"}));
        harness.drain().await;
        let thread = harness.projections.thread("thread-1").await;
        assert_eq!(thread.checkpoints.len(), 1, "{terminal}");
        assert_eq!(thread.checkpoints[0].status, OrchestrationCheckpointStatus::Ready);
        assert_eq!(
            serde_json::to_value(thread.latest_turn.as_ref().unwrap().state).unwrap(),
            json!(if terminal == "turn.aborted" { "interrupted" } else { "completed" })
        );
        assert_eq!(thread.checkpoints[0].assistant_message_id.as_ref().unwrap().as_str(), "assistant:mid-turn");
        let paths: Vec<_> = thread.checkpoints[0].files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths, vec!["early.ts", "late.ts"]);
        assert_eq!(git_show(&harness.cwd, &harness.reference(1), "late.ts"), "export const late = 2;\n");

        harness.emit("turn.started", "turn-2", json!({}));
        harness.emit("turn.completed", "turn-2", completed());
        assert_receipt(
            &harness.next_receipt().await,
            json!({"type": "checkpoint.diff.finalized", "turnId": "turn-2", "checkpointTurnCount": 2}),
        );
        let thread = harness.projections.thread("thread-1").await;
        let follow_up = thread.checkpoints.iter().find(|c| c.turn_id.as_str() == "turn-2").unwrap();
        assert_eq!(follow_up.checkpoint_turn_count, 2);
        assert!(follow_up.files.is_empty());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn does_not_capture_an_aborted_turn_without_a_matching_start_or_active_session() {
    let harness = Harness::new(Options {
        no_seed: true,
        ..Options::default()
    })
    .await;
    harness.emit(
        "turn.aborted",
        "turn-untracked",
        json!({"payload": {"reason": "Interrupted before the turn started."}}),
    );
    harness.drain().await;
    assert!(harness.projections.thread("thread-1").await.checkpoints.is_empty());
    assert!(!git_ref_exists(&harness.cwd, &harness.reference(1)));
}

#[tokio::test(flavor = "multi_thread")]
async fn refreshes_local_git_status_on_turn_completion_using_the_session_cwd() {
    let harness = Harness::new(Options {
        no_seed: true,
        ..Options::default()
    })
    .await;
    harness.emit("turn.completed", "turn-refresh-local-status", completed());
    harness.drain().await;
    assert_eq!(*harness.vcs.local_refreshes.lock().unwrap(), vec![harness.cwd.to_str().unwrap().to_owned()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn re_asks_for_the_pull_request_at_turn_end_when_the_thread_branch_is_checked_out() {
    let harness = Harness::new(Options {
        no_seed: true,
        thread_branch: Some("t3code/feature"),
        local_ref_name: Some("t3code/feature"),
        ..Options::default()
    })
    .await;
    harness.emit("turn.completed", "turn-refresh-pr", completed());
    harness.drain().await;
    assert_eq!(
        *harness.vcs.pull_request_refreshes.lock().unwrap(),
        vec![harness.cwd.to_str().unwrap().to_owned()]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_files_while_the_pull_request_lookup_is_still_pending() {
    let mut harness = Harness::new(Options {
        no_seed: true,
        thread_branch: Some("t3code/feature"),
        local_ref_name: Some("t3code/feature"),
        ..Options::default()
    })
    .await;
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    *harness.vcs.pull_request_gate.lock().unwrap() = Some(gate.clone());
    let vcs = harness.vcs.clone();
    std::fs::write(harness.cwd.join("README.md"), "completed turn\n").unwrap();
    harness.emit("turn.completed", "turn-slow-pr", completed());
    wait_for(|| !vcs.pull_request_refreshes.lock().unwrap().is_empty()).await;
    assert_receipt(
        &harness.next_receipt().await,
        json!({"type": "checkpoint.diff.finalized", "turnId": "turn-slow-pr"}),
    );
    assert_eq!(git_show(&harness.cwd, &harness.reference(1), "README.md"), "completed turn\n");
    gate.add_permits(1);
    harness.drain().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn branch_drift_is_adopted_only_on_a_dedicated_worktree() {
    // Re-asks for the PR after adopting a drifted checkout.
    let harness = Harness::new(Options {
        no_seed: true,
        thread_branch: Some("t3code/original-branch"),
        local_ref_name: Some("t3code/renamed-by-agent"),
        ..Options::default()
    })
    .await;
    harness.emit("turn.completed", "turn-drift-pr", completed());
    harness.drain().await;
    let projections = harness.projections.clone();
    let mut branch = None;
    for _ in 0..200 {
        branch = projections.thread("thread-1").await.branch;
        if branch.as_deref() == Some("t3code/renamed-by-agent") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(branch.as_deref(), Some("t3code/renamed-by-agent"));
    assert_eq!(
        *harness.vcs.pull_request_refreshes.lock().unwrap(),
        vec![harness.cwd.to_str().unwrap().to_owned()]
    );

    // Follows a checkout from a saved placeholder branch.
    let harness = Harness::new(Options {
        no_seed: true,
        thread_branch: Some("t3code/fd9cbe0e"),
        local_ref_name: Some("fix/mobile-tool-detail-expansion"),
        ..Options::default()
    })
    .await;
    harness.emit("turn.completed", "turn-placeholder-drift", completed());
    harness.drain().await;
    let thread = harness.projections.thread("thread-1").await;
    assert_eq!(thread.branch.as_deref(), Some("fix/mobile-tool-detail-expansion"));
    assert_eq!(thread.worktree_path.as_deref(), harness.cwd.to_str());
    assert_eq!(harness.vcs.pull_request_refreshes.lock().unwrap().len(), 1);

    // Not on a worktree shared by another thread.
    for branch in ["t3code/original-branch", "t3code/fd9cbe0e"] {
        let harness = Harness::new(Options {
            no_seed: true,
            thread_branch: Some(branch),
            local_ref_name: Some("t3code/renamed-by-agent"),
            second_thread: Some(Box::new(|cwd: &Path| cwd.to_path_buf())),
            ..Options::default()
        })
        .await;
        harness.emit("turn.completed", "turn-branch-drift-shared", completed());
        harness.drain().await;
        assert_eq!(harness.projections.thread("thread-1").await.branch.as_deref(), Some(branch));
        assert!(harness.vcs.pull_request_refreshes.lock().unwrap().is_empty());
    }

    // Not to a temporary placeholder checkout.
    let harness = Harness::new(Options {
        no_seed: true,
        thread_branch: Some("t3code/original-branch"),
        local_ref_name: Some("t3code/0a1b2c3d"),
        ..Options::default()
    })
    .await;
    harness.emit("turn.completed", "turn-branch-drift-temp", completed());
    harness.drain().await;
    assert_eq!(harness.projections.thread("thread-1").await.branch.as_deref(), Some("t3code/original-branch"));

    // Never on the default branch.
    let harness = Harness::new(Options {
        no_seed: true,
        thread_branch: Some("main"),
        local_ref_name: Some("main"),
        ..Options::default()
    })
    .await;
    harness.emit("turn.completed", "turn-no-pr-refresh", completed());
    harness.drain().await;
    assert!(harness.vcs.pull_request_refreshes.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn ignores_auxiliary_thread_turn_completion_while_primary_turn_is_active() {
    let harness = Harness::new(Options {
        no_seed: true,
        thread_branch: Some("t3code/feature"),
        local_ref_name: Some("t3code/feature"),
        ..Options::default()
    })
    .await;
    harness.session_set("running", Some("turn-main")).await;
    harness.emit("turn.started", "turn-main", json!({}));
    let cwd = harness.cwd.clone();
    let baseline = harness.reference(0);
    wait_for(|| git_ref_exists(&cwd, &baseline)).await;
    std::fs::write(harness.cwd.join("README.md"), "v2\n").unwrap();
    harness.emit("turn.started", "turn-aux", json!({}));
    harness.emit("turn.completed", "turn-aux", completed());
    harness.drain().await;
    assert!(harness.projections.thread("thread-1").await.checkpoints.is_empty());
    assert!(harness.vcs.pull_request_refreshes.lock().unwrap().is_empty());
    assert!(harness.prs.refreshes.lock().unwrap().is_empty());

    harness.emit("turn.completed", "turn-main", completed());
    harness.drain().await;
    let thread = harness.projections.thread("thread-1").await;
    assert_eq!(thread.checkpoints.len(), 1);
    assert_eq!(thread.checkpoints[0].checkpoint_turn_count, 1);
    assert_eq!(
        *harness.vcs.pull_request_refreshes.lock().unwrap(),
        vec![harness.cwd.to_str().unwrap().to_owned()]
    );
    assert_eq!(*harness.prs.refreshes.lock().unwrap(), vec!["project-1".to_owned()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_a_checkpoint_without_a_summary_when_the_baseline_is_missing() {
    let mut harness = Harness::new(Options {
        no_seed: true,
        ..Options::default()
    })
    .await;
    harness.emit("turn.completed", "turn-missing-baseline", completed());
    assert_receipt(
        &harness.next_receipt().await,
        json!({"type": "checkpoint.diff.finalized", "checkpointTurnCount": 1}),
    );
    harness.drain().await;
    let thread = harness.projections.thread("thread-1").await;
    assert_eq!(thread.checkpoints[0].status, OrchestrationCheckpointStatus::Ready);
    assert!(thread.checkpoints[0].files.is_empty());
    assert!(git_ref_exists(&harness.cwd, &harness.reference(1)));
    assert!(!harness.activities().await.iter().any(|a| a["kind"] == "checkpoint.capture.failed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn resumes_checkpointing_after_git_init() {
    for (between_turns, commit) in [(true, false), (true, true), (false, false), (false, true)] {
        let mut harness = Harness::new(Options {
            no_git: true,
            no_seed: true,
            ..Options::default()
        })
        .await;
        harness.emit("turn.started", "turn-1", json!({}));
        harness.drain().await;
        std::fs::write(harness.cwd.join("README.md"), "before git\n").unwrap();
        harness.emit("turn.completed", "turn-1", completed());
        harness.drain().await;
        assert!(harness.projections.thread("thread-1").await.checkpoints.is_empty());

        if !between_turns {
            harness.emit("turn.started", "turn-2", json!({}));
            harness.drain().await;
        }
        git(&harness.cwd, &["init", "--initial-branch=main"]);
        if commit {
            git(&harness.cwd, &["add", "."]);
            git(
                &harness.cwd,
                &["-c", "user.name=Test", "-c", "user.email=test@example.com", "commit", "-m", "Initial"],
            );
        }
        if between_turns {
            harness
                .dispatch(json!({"type": "thread.turn.start", "commandId": "cmd-after-git-init", "threadId": "thread-1",
                    "message": {"messageId": "message-after-git-init", "role": "user", "text": "continue", "attachments": []},
                    "interactionMode": "default", "runtimeMode": "approval-required", "createdAt": NOW}))
                .await;
            assert_receipt(
                &harness.next_receipt().await,
                json!({"type": "checkpoint.baseline.captured", "checkpointTurnCount": 0}),
            );
            harness.emit("turn.started", "turn-2", json!({}));
            harness.drain().await;
        }
        std::fs::write(harness.cwd.join("README.md"), "after git\n").unwrap();
        harness.emit("turn.completed", "turn-2", completed());
        assert_receipt(
            &harness.next_receipt().await,
            json!({"type": "checkpoint.diff.finalized", "checkpointTurnCount": 1}),
        );
        assert_receipt(&harness.next_receipt().await, json!({"type": "turn.processing.quiesced"}));
        harness.drain().await;
        let first = serde_json::to_value(&harness.projections.thread("thread-1").await.checkpoints[0].files).unwrap();
        assert_eq!(
            first,
            if between_turns {
                json!([{"path": "README.md", "kind": "modified", "additions": 1, "deletions": 1}])
            } else {
                json!([])
            },
            "between turns {between_turns}, commit {commit}"
        );
        assert_eq!(git_show(&harness.cwd, &harness.reference(1), "README.md"), "after git\n");
        assert_eq!(git_ref_exists(&harness.cwd, &harness.reference(0)), between_turns);

        harness.emit("turn.started", "turn-3", json!({}));
        harness.drain().await;
        std::fs::write(harness.cwd.join("README.md"), "next turn\n").unwrap();
        harness.emit("turn.completed", "turn-3", completed());
        assert_receipt(
            &harness.next_receipt().await,
            json!({"type": "checkpoint.diff.finalized", "checkpointTurnCount": 2}),
        );
        harness.drain().await;
        let thread = harness.projections.thread("thread-1").await;
        assert_eq!(
            serde_json::to_value(&thread.checkpoints[1].files).unwrap(),
            json!([{"path": "README.md", "kind": "modified", "additions": 1, "deletions": 1}])
        );
        assert!(!harness.activities().await.iter().any(|a| a["kind"] == "checkpoint.capture.failed"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_pre_turn_baseline_from_project_workspace_root_when_thread_worktree_is_unset() {
    let harness = Harness::new(Options {
        no_session: true,
        no_seed: true,
        thread_worktree_path: Some(None),
        ..Options::default()
    })
    .await;
    harness
        .dispatch(
            json!({"type": "thread.turn.start", "commandId": "cmd-turn-start-for-baseline", "threadId": "thread-1",
            "message": {"messageId": "message-user-1", "role": "user", "text": "start turn", "attachments": []},
            "interactionMode": "default", "runtimeMode": "approval-required", "createdAt": NOW}),
        )
        .await;
    let (cwd, baseline) = (harness.cwd.clone(), harness.reference(0));
    wait_for(|| git_ref_exists(&cwd, &baseline)).await;
    assert_eq!(git_show(&harness.cwd, &harness.reference(0), "README.md"), "v1\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn does_not_create_checkpoints_while_importing_historical_user_messages() {
    let harness = Harness::new(Options {
        no_session: true,
        no_seed: true,
        thread_worktree_path: Some(None),
        ..Options::default()
    })
    .await;
    harness
        .dispatch(
            json!({"type": "thread.history.import", "commandId": "cmd-import-history-without-checkpoint", "threadId": "thread-1",
            "messages": [{"messageId": "imported-user-message", "role": "user", "text": "A message from an existing agent session", "createdAt": NOW}]}),
        )
        .await;
    harness.drain().await;
    assert!(!git_ref_exists(&harness.cwd, &harness.reference(0)));
}

#[tokio::test(flavor = "multi_thread")]
async fn captures_turn_completion_from_project_workspace_root_when_provider_session_cwd_is_unavailable() {
    let harness = Harness::new(Options {
        no_session: true,
        no_seed: true,
        thread_worktree_path: Some(None),
        ..Options::default()
    })
    .await;
    harness.session_set("running", Some("turn-missing-cwd")).await;
    std::fs::write(harness.cwd.join("README.md"), "v2\n").unwrap();
    harness.emit("turn.completed", "turn-missing-cwd", completed());
    harness.drain().await;
    assert!(git_ref_exists(&harness.cwd, &harness.reference(1)));
    assert_eq!(git_show(&harness.cwd, &harness.reference(1), "README.md"), "v2\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn ignores_non_v2_checkpoint_captured_runtime_events() {
    let harness = Harness::new(Options::default()).await;
    harness.session_set("ready", None).await;
    harness.emit("checkpoint.captured", "turn-3", json!({"turnCount": 3, "status": "completed"}));
    harness.drain().await;
    assert!(!harness
        .projections
        .thread("thread-1")
        .await
        .checkpoints
        .iter()
        .any(|c| c.checkpoint_turn_count == 3));
}

#[tokio::test(flavor = "multi_thread")]
async fn continues_processing_runtime_events_after_a_single_checkpoint_runtime_failure() {
    let (_other, other_root) = temp_dir();
    let harness = Harness::new(Options {
        no_seed: true,
        provider_session_cwd: Some(other_root.clone()),
        ..Options::default()
    })
    .await;
    harness.session_set("ready", None).await;
    harness.emit("turn.completed", "turn-runtime-failure", completed());
    harness.emit("turn.started", "turn-after-runtime-failure", json!({}));
    let (cwd, baseline) = (harness.cwd.clone(), harness.reference(0));
    wait_for(|| git_ref_exists(&cwd, &baseline)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_unsupported_rewind_before_changing_files_checkpoints_or_history() {
    let harness = Harness::new(Options {
        provider_name: Some("antigravity"),
        ..Options::default()
    })
    .await;
    *harness.providers.rollback_supported.lock().unwrap() = Some(
        TaggedError::new(
            "ProviderValidationError",
            "Provider validation failed in ProviderService.assertConversationRollbackSupported: Provider 'antigravity' does not support conversation rewind.",
        )
        .with("operation", "ProviderService.assertConversationRollbackSupported")
        .with("issue", "Provider 'antigravity' does not support conversation rewind."),
    );
    for turn in [1, 2] {
        harness
            .dispatch(json!({"type": "thread.turn.start", "commandId": format!("cmd-unsupported-rewind-message-{turn}"), "threadId": "thread-1",
                "message": {"messageId": format!("message-unsupported-rewind-{turn}"), "role": "user", "text": format!("Keep message {turn}"), "attachments": []},
                "interactionMode": "default", "runtimeMode": "approval-required", "createdAt": NOW}))
            .await;
        harness
            .dispatch(
                json!({"type": "thread.turn.diff.complete", "commandId": format!("cmd-unsupported-rewind-diff-{turn}"), "threadId": "thread-1",
                "turnId": format!("turn-unsupported-rewind-{turn}"), "completedAt": NOW, "checkpointRef": harness.reference(turn), "status": "ready",
                "files": [], "checkpointTurnCount": turn, "createdAt": NOW}),
            )
            .await;
    }
    harness.drain().await;
    let before = harness.projections.thread("thread-1").await;
    harness
        .dispatch(json!({"type": "thread.checkpoint.revert", "commandId": "cmd-unsupported-rewind", "threadId": "thread-1", "turnCount": 1, "createdAt": NOW}))
        .await;
    harness.drain().await;
    let after = harness.projections.thread("thread-1").await;
    assert_eq!(after.checkpoints, before.checkpoints);
    assert_eq!(after.messages, before.messages);
    assert_eq!(after.latest_turn, before.latest_turn);
    let failure = harness
        .activities()
        .await
        .into_iter()
        .find(|a| a["kind"] == "checkpoint.revert.failed")
        .unwrap();
    assert!(failure["payload"]["detail"].as_str().unwrap().contains("does not support conversation rewind"));
    assert!(harness.providers.rollbacks().is_empty());
    assert_eq!(harness.readme(), "v3\n");
    assert!(git_ref_exists(&harness.cwd, &harness.reference(2)));
}

#[tokio::test(flavor = "multi_thread")]
async fn rewinds_history_with_the_requested_filesystem_behavior() {
    for (command_type, initialize_git) in [
        ("thread.checkpoint.revert", true),
        ("thread.conversation.revert", true),
        ("thread.conversation.revert", false),
    ] {
        let harness = Harness::new(Options {
            no_git: !initialize_git,
            no_seed: !initialize_git,
            ..Options::default()
        })
        .await;
        harness.session_set("ready", None).await;
        for turn in [1, 2] {
            let reference = if initialize_git {
                harness.reference(turn)
            } else {
                format!("provider-diff:thread-1:turn-{turn}")
            };
            harness.diff_complete(turn, &reference, if initialize_git { "ready" } else { "missing" }).await;
        }
        std::fs::write(harness.cwd.join("README.md"), "staged edit\n").unwrap();
        if initialize_git {
            git(&harness.cwd, &["add", "README.md"]);
        }
        std::fs::write(harness.cwd.join("README.md"), "unstaged edit\n").unwrap();
        std::fs::write(harness.cwd.join("scratch.txt"), "untracked edit\n").unwrap();
        let index_before = initialize_git.then(|| git(&harness.cwd, &["ls-files", "--stage"]));
        harness
            .dispatch(json!({"type": command_type, "commandId": "cmd-revert-request", "threadId": "thread-1", "turnCount": 1, "createdAt": NOW}))
            .await;
        harness.drain().await;
        let thread = harness.projections.thread("thread-1").await;
        assert_eq!(
            serde_json::to_value(thread.latest_turn.as_ref().unwrap()).unwrap()["turnId"],
            json!("turn-1"),
            "{command_type}"
        );
        assert_eq!(thread.checkpoints.len(), 1);
        assert_eq!(thread.checkpoints[0].checkpoint_turn_count, 1);
        assert_eq!(harness.providers.rollbacks(), vec![("thread-1".to_owned(), 1)]);
        assert_eq!(
            harness.readme(),
            if command_type == "thread.conversation.revert" {
                "unstaged edit\n"
            } else {
                "v2\n"
            }
        );
        if command_type == "thread.conversation.revert" {
            assert_eq!(std::fs::read_to_string(harness.cwd.join("scratch.txt")).unwrap(), "untracked edit\n");
            if let Some(index_before) = index_before {
                assert_eq!(git(&harness.cwd, &["ls-files", "--stage"]), index_before);
            }
        }
        if initialize_git {
            assert!(!git_ref_exists(&harness.cwd, &harness.reference(2)));
        } else {
            assert!(!harness.cwd.join(".git").exists());
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn executes_provider_revert_for_claude_sessions_and_sequences_consecutive_reverts() {
    let harness = Harness::new(Options {
        provider_name: Some("claudeAgent"),
        ..Options::default()
    })
    .await;
    harness.session_set("ready", None).await;
    harness.diff_complete(1, &harness.reference(1), "ready").await;
    harness.diff_complete(2, &harness.reference(2), "ready").await;
    harness
        .dispatch(json!({"type": "thread.checkpoint.revert", "commandId": "cmd-sequenced-revert-request-1", "threadId": "thread-1", "turnCount": 1, "createdAt": NOW}))
        .await;
    harness
        .dispatch(json!({"type": "thread.checkpoint.revert", "commandId": "cmd-sequenced-revert-request-0", "threadId": "thread-1", "turnCount": 0, "createdAt": NOW}))
        .await;
    harness.drain().await;
    assert_eq!(harness.providers.rollbacks(), vec![("thread-1".to_owned(), 1), ("thread-1".to_owned(), 1)]);
    assert_eq!(harness.readme(), "v1\n");
    let events: Vec<_> = futures::StreamExt::collect::<Vec<_>>(harness.engine.read_events(0, None)).await;
    let reverted = events
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| matches!(e, zc_contracts::OrchestrationEvent::ThreadReverted(_)))
        .count();
    assert_eq!(reverted, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn restores_files_only_in_an_isolated_worktree_without_an_active_session() {
    for use_project_cwd in [false, true] {
        let harness = Harness::new(Options {
            no_session: true,
            thread_worktree_path: use_project_cwd.then_some(None),
            ..Options::default()
        })
        .await;
        harness.diff_complete(1, &harness.reference(1), "ready").await;
        harness
            .dispatch(
                json!({"type": "thread.checkpoint.revert", "commandId": "cmd-revert-no-session", "threadId": "thread-1", "turnCount": 0, "createdAt": NOW}),
            )
            .await;
        harness.drain().await;
        if use_project_cwd {
            assert!(harness.providers.rollbacks().is_empty());
            assert_eq!(harness.readme(), "v3\n");
            let failure = harness
                .activities()
                .await
                .into_iter()
                .find(|a| a["kind"] == "checkpoint.revert.failed")
                .unwrap();
            assert!(failure["payload"]["detail"].as_str().unwrap().contains("isolated worktree"));
        } else {
            assert_eq!(harness.providers.rollbacks(), vec![("thread-1".to_owned(), 1)]);
            assert_eq!(harness.readme(), "v1\n");
        }
    }
}
