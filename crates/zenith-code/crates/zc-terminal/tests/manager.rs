//! `terminal/Manager.test.ts`, ported: the manager driven through the fake PTY.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::{FutureExt, Stream, StreamExt};
use zc_core::process::{ProcessRunError, ProcessRunOutput};
use zc_core::shell_env::Platform;
use zc_core::{ProcessRunInput, ProcessRunner};
use zc_terminal::contracts::{
    TerminalAttachInput, TerminalAttachStreamEvent, TerminalClearInput, TerminalCloseInput, TerminalError, TerminalEvent, TerminalEventKind,
    TerminalMetadataStreamEvent, TerminalOpenInput, TerminalResizeInput, TerminalRestartInput, TerminalSessionStatus, TerminalWriteInput, DEFAULT_TERMINAL_ID,
};
use zc_terminal::pty::{PtyExitEvent, PtySignal};
use zc_terminal::subprocess::{ProcessTableEntry, SubprocessCheckError, SubprocessInspectResult};
use zc_terminal::testing::FakePtyAdapter;
use zc_terminal::{FnProviderEnvironment, TerminalManager, TerminalManagerOptions};

struct Fixture {
    _dir: tempfile::TempDir,
    base_dir: PathBuf,
    logs_dir: PathBuf,
    pty: Arc<FakePtyAdapter>,
    manager: TerminalManager,
    subscription: Mutex<zc_terminal::ListenerStream<TerminalEvent>>,
    events: Mutex<Vec<TerminalEvent>>,
}

impl Fixture {
    /// Every event published so far (listeners are called synchronously on publish).
    fn events(&self) -> Vec<TerminalEvent> {
        let mut events = self.events.lock().unwrap();
        events.extend(ready(&mut *self.subscription.lock().unwrap()));
        events.clone()
    }

    fn event_types(&self) -> Vec<&'static str> {
        self.events().iter().map(|event| event.kind.type_name()).collect()
    }

    fn history_path(&self, thread_id: &str, terminal_id: &str) -> PathBuf {
        self.manager.history_store().history_path(thread_id, terminal_id)
    }
}

/// `createManager`: a temp base dir, the fake PTY, kill grace 1 ms, and every event recorded.
async fn create_manager(history_line_limit: usize, configure: impl FnOnce(&mut TerminalManagerOptions)) -> Fixture {
    create_manager_with(Arc::new(FakePtyAdapter::default()), history_line_limit, configure).await
}

async fn create_manager_with(pty: Arc<FakePtyAdapter>, history_line_limit: usize, configure: impl FnOnce(&mut TerminalManagerOptions)) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base_dir = dir.path().canonicalize().unwrap();
    let logs_dir = base_dir.join("userdata/logs/terminals");
    let mut options = TerminalManagerOptions::new(&logs_dir, pty.clone());
    options.history_line_limit = history_line_limit;
    options.process_kill_grace = Duration::from_millis(1);
    // Polling stays out of the way unless a test asks for it.
    options.subprocess_poll_interval = Duration::from_secs(3600);
    options.subprocess_inspector = Some(Arc::new(|_| async { Ok(SubprocessInspectResult::default()) }.boxed()));
    configure(&mut options);
    let manager = TerminalManager::new(options).await.unwrap();
    let subscription = Mutex::new(manager.subscribe());
    Fixture {
        _dir: dir,
        base_dir,
        logs_dir,
        pty,
        manager,
        subscription,
        events: Mutex::new(Vec::new()),
    }
}

fn cwd() -> String {
    std::env::current_dir().unwrap().to_string_lossy().into_owned()
}

fn open_input() -> TerminalOpenInput {
    TerminalOpenInput {
        cols: Some(100),
        rows: Some(24),
        ..TerminalOpenInput::new("thread-1", DEFAULT_TERMINAL_ID, cwd())
    }
}

fn open_for(thread_id: &str, terminal_id: &str) -> TerminalOpenInput {
    TerminalOpenInput {
        thread_id: thread_id.into(),
        terminal_id: terminal_id.into(),
        ..open_input()
    }
}

fn restart_input() -> TerminalRestartInput {
    TerminalRestartInput {
        thread_id: "thread-1".into(),
        terminal_id: DEFAULT_TERMINAL_ID.into(),
        cwd: cwd(),
        worktree_path: None,
        cols: 100,
        rows: 24,
        env: None,
        provider_instance_id: None,
    }
}

fn close_thread(thread_id: &str) -> TerminalCloseInput {
    TerminalCloseInput {
        thread_id: thread_id.into(),
        terminal_id: None,
        delete_history: None,
    }
}

fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

async fn wait_for(mut condition: impl FnMut() -> bool, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for condition");
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
}

const WAIT: Duration = Duration::from_millis(1_200);

fn ready<T, S: Stream<Item = T> + Unpin>(stream: &mut S) -> Vec<T> {
    let mut items = Vec::new();
    while let Some(Some(item)) = stream.next().now_or_never() {
        items.push(item);
    }
    items
}

fn attach_types(events: &[TerminalAttachStreamEvent]) -> Vec<&'static str> {
    events
        .iter()
        .map(|event| match event {
            TerminalAttachStreamEvent::Snapshot(_) => "snapshot",
            TerminalAttachStreamEvent::Event(event) => event.kind.type_name(),
        })
        .collect()
}

fn snapshots(events: &[TerminalAttachStreamEvent]) -> Vec<&zc_terminal::contracts::TerminalSessionSnapshot> {
    events
        .iter()
        .filter_map(|event| match event {
            TerminalAttachStreamEvent::Snapshot(snapshot) => Some(&**snapshot),
            _ => None,
        })
        .collect()
}

fn has_exited(fixture: &Fixture) -> bool {
    fixture.event_types().contains(&"exited")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[tokio::test]
async fn spawns_lazily_and_reuses_running_terminal_per_thread() {
    let f = create_manager(5, |_| {}).await;
    let (first, second) = tokio::join!(f.manager.open(open_input()), f.manager.open(open_input()));
    let third = f.manager.open(open_input()).await.unwrap();
    assert_eq!(first.unwrap().thread_id, "thread-1");
    assert_eq!(second.unwrap().terminal_id, DEFAULT_TERMINAL_ID);
    assert_eq!(third.thread_id, "thread-1");
    assert_eq!(f.pty.spawn_inputs().len(), 1);
}

#[tokio::test]
async fn attaches_to_running_sessions_without_restarting_them() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    let mut input = TerminalAttachInput::new("thread-1", DEFAULT_TERMINAL_ID);
    input.cols = Some(100);
    input.rows = Some(40);
    let mut stream = f.manager.attach_stream(input).await.unwrap();
    let events = ready(&mut stream);
    let snapshot = snapshots(&events)[0];
    assert_eq!(snapshot.thread_id, "thread-1");
    assert_eq!(snapshot.terminal_id, DEFAULT_TERMINAL_ID);
    assert_eq!(f.pty.spawn_inputs().len(), 1);
    assert_eq!(f.pty.process(0).resize_calls(), vec![(100, 40)]);
}

#[tokio::test]
async fn keeps_attach_streams_live_when_a_terminal_id_is_closed_and_reopened() {
    let f = create_manager(5, |_| {}).await;
    let mut stream = f.manager.attach_stream(open_input().into()).await.unwrap();
    f.manager
        .close(TerminalCloseInput {
            terminal_id: Some(DEFAULT_TERMINAL_ID.into()),
            delete_history: Some(true),
            ..close_thread("thread-1")
        })
        .await
        .unwrap();
    f.manager.open(open_input()).await.unwrap();
    let events = ready(&mut stream);
    assert_eq!(attach_types(&events), ["snapshot", "closed", "snapshot"]);
    let statuses: Vec<_> = snapshots(&events).iter().map(|s| s.status).collect();
    assert_eq!(statuses, [TerminalSessionStatus::Running, TerminalSessionStatus::Running]);
    assert_eq!(f.pty.spawn_inputs().len(), 2);
}

#[tokio::test]
async fn attaches_to_exited_sessions_without_restarting_them() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_exit(0, Some(0));
    wait_for(|| has_exited(&f), WAIT).await;

    let mut input = open_input();
    input.env = Some(env(&[("T3CODE_WORKTREE_PATH", "/tmp/should-not-restart")]));
    input.worktree_path = Some(Some("/tmp/should-not-restart".into()));
    let mut stream = f.manager.attach_stream(input.into()).await.unwrap();
    let events = ready(&mut stream);
    let snapshot = snapshots(&events)[0];
    assert_eq!(snapshot.status, TerminalSessionStatus::Exited);
    assert_eq!(snapshot.worktree_path, None);
    assert_eq!(f.pty.spawn_inputs().len(), 1);
}

#[tokio::test]
async fn restarts_inactive_sessions_from_attach_only_when_requested() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_exit(0, Some(0));
    wait_for(|| has_exited(&f), WAIT).await;

    let mut input: TerminalAttachInput = TerminalOpenInput {
        env: Some(env(&[("T3CODE_WORKTREE_PATH", "/tmp/restart-requested")])),
        worktree_path: Some(Some("/tmp/restart-requested".into())),
        ..open_input()
    }
    .into();
    input.restart_if_not_running = Some(true);
    let mut stream = f.manager.attach_stream(input).await.unwrap();
    let events = ready(&mut stream);
    let snapshot = snapshots(&events)[0];
    assert_eq!(snapshot.status, TerminalSessionStatus::Running);
    assert_eq!(snapshot.worktree_path.as_deref(), Some("/tmp/restart-requested"));
    assert_eq!(f.pty.spawn_inputs().len(), 2);
}

#[tokio::test]
async fn reports_cwd_errors() {
    let f = create_manager(5, |_| {}).await;
    let missing = f.base_dir.join("missing-cwd").to_string_lossy().into_owned();
    let error = f
        .manager
        .open(TerminalOpenInput {
            cwd: missing.clone(),
            ..open_input()
        })
        .await
        .unwrap_err();
    assert_eq!(error, TerminalError::TerminalCwdNotFoundError { cwd: missing });
    assert!(!serde_json::to_value(&error).unwrap().as_object().unwrap().contains_key("cause"));

    let file = f.base_dir.join("cwd-file");
    std::fs::write(&file, "not a directory").unwrap();
    let file = file.to_string_lossy().into_owned();
    let error = f
        .manager
        .open(TerminalOpenInput {
            cwd: file.clone(),
            ..open_input()
        })
        .await
        .unwrap_err();
    assert_eq!(error, TerminalError::TerminalCwdNotDirectoryError { cwd: file });
}

#[cfg(unix)]
#[tokio::test]
async fn preserves_non_not_found_cwd_stat_failures() {
    use std::os::unix::fs::PermissionsExt;
    // SAFETY: getuid has no preconditions.
    if unsafe { libc::getuid() } == 0 {
        return; // root ignores the permission bits
    }
    let f = create_manager(5, |_| {}).await;
    let blocked_root = f.base_dir.join("blocked-root");
    let blocked_cwd = blocked_root.join("cwd");
    std::fs::create_dir_all(&blocked_cwd).unwrap();
    std::fs::set_permissions(&blocked_root, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = f
        .manager
        .open(TerminalOpenInput {
            cwd: blocked_cwd.to_string_lossy().into_owned(),
            ..open_input()
        })
        .await;
    std::fs::set_permissions(&blocked_root, std::fs::Permissions::from_mode(0o755)).unwrap();
    match result.unwrap_err() {
        TerminalError::TerminalCwdStatError { cwd, cause } => {
            assert_eq!(cwd, blocked_cwd.to_string_lossy());
            assert!(cause.message().to_lowercase().contains("permission"));
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test]
async fn handles_an_exit_replayed_during_subscription_after_publishing_startup() {
    let fake = Arc::new(FakePtyAdapter::default());
    *fake.exit_on_subscribe.lock().unwrap() = Some(PtyExitEvent {
        exit_code: Some(7),
        signal: None,
    });
    let f = create_manager_with(fake.clone(), 5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    wait_for(|| has_exited(&f), WAIT).await;
    assert_eq!(f.event_types(), ["started", "exited"]);
    assert!(matches!(
        f.events()[1].kind,
        TerminalEventKind::Exited {
            exit_code: Some(7),
            exit_signal: None
        }
    ));
    let mut stream = f.manager.attach_stream(open_input().into()).await.unwrap();
    let events = ready(&mut stream);
    let snapshot = snapshots(&events)[0];
    assert_eq!(snapshot.status, TerminalSessionStatus::Exited);
    assert_eq!(snapshot.exit_code, Some(7));
}

#[tokio::test]
async fn supports_asynchronous_pty_spawns() {
    let fake = Arc::new(FakePtyAdapter::new(true));
    let f = create_manager_with(fake.clone(), 5, |_| {}).await;
    let snapshot = f.manager.open(open_input()).await.unwrap();
    assert_eq!(snapshot.status, TerminalSessionStatus::Running);
    assert_eq!(f.pty.spawn_inputs().len(), 1);
    assert_eq!(f.pty.process_count(), 1);
}

fn write_input(terminal_id: &str, data: &str) -> TerminalWriteInput {
    TerminalWriteInput {
        thread_id: "thread-1".into(),
        terminal_id: terminal_id.into(),
        data: data.into(),
    }
}

fn resize_input(cols: u16, rows: u16) -> TerminalResizeInput {
    TerminalResizeInput {
        thread_id: "thread-1".into(),
        terminal_id: DEFAULT_TERMINAL_ID.into(),
        cols,
        rows,
    }
}

#[tokio::test]
async fn forwards_write_and_resize_to_the_active_pty() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.manager.write(write_input(DEFAULT_TERMINAL_ID, "ls\n")).await.unwrap();
    f.manager.resize(resize_input(120, 30)).await.unwrap();
    let process = f.pty.process(0);
    assert_eq!(process.writes(), ["ls\n"]);
    assert_eq!(process.resize_calls(), [(120, 30)]);
}

#[tokio::test]
async fn preserves_structured_context_and_causes_for_pty_io_failures() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    let process = f.pty.process(0);
    *process.write_failure.lock().unwrap() = Some("PTY input handle is unavailable".into());
    let error = f
        .manager
        .write(write_input(DEFAULT_TERMINAL_ID, "secret input that must not be attached to the error"))
        .await
        .unwrap_err();
    let encoded = serde_json::to_value(&error).unwrap();
    assert_eq!(encoded["_tag"], "TerminalWriteError");
    assert_eq!(encoded["threadId"], "thread-1");
    assert_eq!(encoded["terminalPid"], process.pid_value());
    assert_eq!(encoded["cause"]["message"], "PTY input handle is unavailable");
    assert!(!encoded.to_string().contains("secret input"));

    *process.resize_failure.lock().unwrap() = Some("PTY resize handle is unavailable".into());
    let error = f.manager.resize(resize_input(132, 40)).await.unwrap_err();
    let encoded = serde_json::to_value(&error).unwrap();
    assert_eq!(encoded["_tag"], "TerminalResizeError");
    assert_eq!((encoded["cols"].as_u64(), encoded["rows"].as_u64()), (Some(132), Some(40)));
    assert_eq!(encoded["cause"]["message"], "PTY resize handle is unavailable");

    *process.resize_failure.lock().unwrap() = None;
    f.manager
        .open(TerminalOpenInput {
            cols: Some(132),
            rows: Some(40),
            ..open_input()
        })
        .await
        .unwrap();
    assert_eq!(process.resize_calls(), [(132, 40)]);
}

trait PidValue {
    fn pid_value(&self) -> u32;
}

impl PidValue for zc_terminal::testing::FakePtyProcess {
    fn pid_value(&self) -> u32 {
        zc_terminal::PtyProcess::pid(self)
    }
}

#[tokio::test]
async fn ignores_delayed_resize_requests_after_a_terminal_closes() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.manager
        .close(TerminalCloseInput {
            terminal_id: Some(DEFAULT_TERMINAL_ID.into()),
            delete_history: Some(true),
            ..close_thread("thread-1")
        })
        .await
        .unwrap();
    f.manager.resize(resize_input(120, 30)).await.unwrap();
    assert!(f.pty.process(0).resize_calls().is_empty());
}

#[tokio::test]
async fn resizes_a_running_terminal_on_open_when_a_different_size_is_requested() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    let reopened = f
        .manager
        .open(TerminalOpenInput {
            cols: Some(120),
            rows: Some(30),
            ..open_input()
        })
        .await
        .unwrap();
    assert_eq!(reopened.status, TerminalSessionStatus::Running);
    assert_eq!(f.pty.process(0).resize_calls(), [(120, 30)]);
}

#[tokio::test]
async fn supports_multiple_terminals_per_thread_independently() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_for("thread-1", "default")).await.unwrap();
    f.manager.open(open_for("thread-1", "term-2")).await.unwrap();
    f.manager.write(write_input("default", "pwd\n")).await.unwrap();
    f.manager.write(write_input("term-2", "ls\n")).await.unwrap();
    assert_eq!(f.pty.process(0).writes(), ["pwd\n"]);
    assert_eq!(f.pty.process(1).writes(), ["ls\n"]);
    assert_eq!(f.pty.spawn_inputs().len(), 2);
}

#[tokio::test]
async fn clears_transcript_and_emits_cleared_event() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_data("hello\n");
    let path = f.history_path("thread-1", DEFAULT_TERMINAL_ID);
    wait_for(|| path.exists(), WAIT).await;
    f.manager
        .clear(TerminalClearInput {
            thread_id: "thread-1".into(),
            terminal_id: DEFAULT_TERMINAL_ID.into(),
        })
        .await
        .unwrap();
    wait_for(|| read(&path).is_empty(), WAIT).await;
    assert!(f
        .events()
        .iter()
        .any(|e| matches!(e.kind, TerminalEventKind::Cleared) && e.thread_id == "thread-1" && e.terminal_id == DEFAULT_TERMINAL_ID));
}

#[tokio::test]
async fn restarts_terminal_with_empty_transcript_and_respawns_pty() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_data("before restart\n");
    let path = f.history_path("thread-1", DEFAULT_TERMINAL_ID);
    wait_for(|| path.exists(), WAIT).await;
    let snapshot = f.manager.restart(restart_input()).await.unwrap();
    assert_eq!(snapshot.history, "");
    assert_eq!(snapshot.status, TerminalSessionStatus::Running);
    assert_eq!(f.pty.spawn_inputs().len(), 2);
    wait_for(|| read(&path).is_empty(), WAIT).await;
    assert!(f.event_types().contains(&"restarted"));
}

#[tokio::test]
async fn restarts_a_running_session_when_open_is_called_with_a_different_cwd() {
    let f = create_manager(5, |_| {}).await;
    let original = f.base_dir.join("original");
    let different = f.base_dir.join("different");
    std::fs::create_dir_all(&original).unwrap();
    std::fs::create_dir_all(&different).unwrap();
    f.manager
        .open(TerminalOpenInput {
            cwd: original.to_string_lossy().into(),
            ..open_input()
        })
        .await
        .unwrap();
    f.pty.process(0).emit_data("before reopen\n");
    let path = f.history_path("thread-1", DEFAULT_TERMINAL_ID);
    wait_for(|| path.exists(), WAIT).await;
    let reopened = f
        .manager
        .open(TerminalOpenInput {
            cwd: different.to_string_lossy().into(),
            ..open_input()
        })
        .await
        .unwrap();
    assert_eq!(f.pty.spawn_inputs().len(), 2);
    assert!(f.pty.process(0).killed());
    assert_eq!(reopened.cwd, different.to_string_lossy());
    assert_eq!(reopened.history, "");
    wait_for(|| read(&path).is_empty(), WAIT).await;
}

#[tokio::test]
async fn propagates_explicit_worktree_metadata_through_snapshots_and_lifecycle_events() {
    let f = create_manager(5, |_| {}).await;
    let first = f.base_dir.join("worktrees/feature-a");
    let second = f.base_dir.join("worktrees/feature-b");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    let first = first.to_string_lossy().into_owned();
    let second = second.to_string_lossy().into_owned();
    let started = f
        .manager
        .open(TerminalOpenInput {
            cwd: first.clone(),
            worktree_path: Some(Some(first.clone())),
            ..open_input()
        })
        .await
        .unwrap();
    let restarted = f
        .manager
        .restart(TerminalRestartInput {
            cwd: second.clone(),
            worktree_path: Some(Some(second.clone())),
            ..restart_input()
        })
        .await
        .unwrap();
    assert_eq!(started.worktree_path.as_deref(), Some(first.as_str()));
    assert_eq!(restarted.worktree_path.as_deref(), Some(second.as_str()));
    let events = f.events();
    let started_event = events.iter().find_map(|e| match &e.kind {
        TerminalEventKind::Started { snapshot } => Some(snapshot.worktree_path.clone()),
        _ => None,
    });
    let restarted_event = events.iter().find_map(|e| match &e.kind {
        TerminalEventKind::Restarted { snapshot } => Some(snapshot.worktree_path.clone()),
        _ => None,
    });
    assert_eq!(started_event, Some(Some(first)));
    assert_eq!(restarted_event, Some(Some(second)));
}

#[tokio::test]
async fn preserves_worktree_metadata_when_reopening_an_exited_session() {
    let f = create_manager(5, |_| {}).await;
    let worktree = f.base_dir.join("worktrees/feature-a");
    std::fs::create_dir_all(&worktree).unwrap();
    let worktree = worktree.to_string_lossy().into_owned();
    let input = TerminalOpenInput {
        cwd: worktree.clone(),
        worktree_path: Some(Some(worktree.clone())),
        ..open_input()
    };
    f.manager.open(input.clone()).await.unwrap();
    f.pty.process(0).emit_exit(0, Some(0));
    wait_for(|| has_exited(&f), WAIT).await;
    let reopened = f.manager.open(input).await.unwrap();
    assert_eq!(reopened.worktree_path.as_deref(), Some(worktree.as_str()));
    let last_started = f.events().into_iter().rev().find_map(|e| match e.kind {
        TerminalEventKind::Started { snapshot } => Some(snapshot.worktree_path),
        _ => None,
    });
    assert_eq!(last_started, Some(Some(worktree)));
}

#[tokio::test]
async fn emits_exited_event_and_reopens_with_clean_transcript_after_exit() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_data("old data\n");
    let path = f.history_path("thread-1", DEFAULT_TERMINAL_ID);
    wait_for(|| path.exists(), WAIT).await;
    f.pty.process(0).emit_exit(0, Some(0));
    wait_for(|| has_exited(&f), WAIT).await;
    let reopened = f.manager.open(open_input()).await.unwrap();
    assert_eq!(reopened.history, "");
    assert_eq!(f.pty.spawn_inputs().len(), 2);
    assert_eq!(read(&path), "");
}

#[tokio::test]
async fn ignores_trailing_writes_after_terminal_exit() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_exit(0, Some(0));
    // The exit is handled by the session's drain task (TS: a fiber forked on the spot).
    wait_for(|| has_exited(&f), WAIT).await;
    f.manager.write(write_input(DEFAULT_TERMINAL_ID, "\r")).await.unwrap();
    assert!(f.pty.process(0).writes().is_empty());
}

#[tokio::test]
async fn writing_to_an_unknown_terminal_fails_with_lookup_error() {
    let f = create_manager(5, |_| {}).await;
    let error = f.manager.write(write_input("nope", "x")).await.unwrap_err();
    assert_eq!(error.tag(), "TerminalSessionLookupError");
}

fn inspector_from(state: Arc<Mutex<SubprocessInspectResult>>) -> zc_terminal::subprocess::SubprocessInspector {
    Arc::new(move |_| {
        let result = state.lock().unwrap().clone();
        async move { Ok(result) }.boxed()
    })
}

fn has_activity(f: &Fixture, busy: bool, label: &str) -> bool {
    f.events()
        .iter()
        .any(|e| matches!(&e.kind, TerminalEventKind::Activity { has_running_subprocess, label: l } if *has_running_subprocess == busy && l == label))
}

#[tokio::test]
async fn emits_subprocess_activity_events_when_child_process_state_changes() {
    let state = Arc::new(Mutex::new(SubprocessInspectResult::default()));
    let f = create_manager(5, |o| {
        o.subprocess_inspector = Some(inspector_from(state.clone()));
        o.subprocess_poll_interval = Duration::from_millis(20);
    })
    .await;
    f.manager.open(open_input()).await.unwrap();
    assert!(!f.event_types().contains(&"activity"));
    *state.lock().unwrap() = SubprocessInspectResult {
        has_running_subprocess: true,
        child_command: Some("vim".into()),
        process_ids: vec![100, 101],
    };
    wait_for(|| has_activity(&f, true, "vim"), WAIT).await;
    *state.lock().unwrap() = SubprocessInspectResult::default();
    wait_for(|| has_activity(&f, false, "Terminal 1"), WAIT).await;
}

#[tokio::test]
async fn does_not_poll_subprocesses_until_a_terminal_session_is_running() {
    let checks = Arc::new(AtomicUsize::new(0));
    let f = create_manager(5, |o| {
        let checks = checks.clone();
        o.subprocess_inspector = Some(Arc::new(move |_| {
            checks.fetch_add(1, Ordering::SeqCst);
            async { Ok(SubprocessInspectResult::default()) }.boxed()
        }));
        o.subprocess_poll_interval = Duration::from_millis(20);
    })
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(checks.load(Ordering::SeqCst), 0);
    f.manager.open(open_input()).await.unwrap();
    wait_for(|| checks.load(Ordering::SeqCst) > 0, WAIT).await;
}

/// A `ProcessRunner` answering every run with a canned `ps` output.
struct FakeRunner {
    calls: Mutex<Vec<(String, Vec<String>, Instant)>>,
    respond: Box<dyn Fn() -> (String, i32) + Send + Sync>,
}

#[async_trait]
impl ProcessRunner for FakeRunner {
    async fn run(&self, input: ProcessRunInput) -> Result<ProcessRunOutput, ProcessRunError> {
        self.calls.lock().unwrap().push((input.command.clone(), input.args.clone(), Instant::now()));
        let (stdout, code) = (self.respond)();
        Ok(ProcessRunOutput {
            stdout,
            stderr: String::new(),
            code: Some(code),
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_invalid_utf8: false,
            stderr_invalid_utf8: false,
        })
    }
}

fn fake_runner(respond: impl Fn() -> (String, i32) + Send + Sync + 'static) -> Arc<FakeRunner> {
    Arc::new(FakeRunner {
        calls: Mutex::new(Vec::new()),
        respond: Box::new(respond),
    })
}

#[tokio::test]
async fn derives_subprocess_activity_for_every_terminal_from_one_shared_process_snapshot() {
    // The fake PTY's pids start at 9000.
    let runner = fake_runner(|| ("  100  9000 vim\n  101   100 git\n  200  9001 /usr/bin/python3".into(), 0));
    let f = create_manager(5, |o| {
        o.subprocess_inspector = None;
        o.process_runner = runner.clone();
        o.platform = Platform::Linux;
        o.subprocess_poll_interval = Duration::from_millis(20);
    })
    .await;
    f.manager.open(open_input()).await.unwrap();
    f.manager.open(open_for("thread-2", DEFAULT_TERMINAL_ID)).await.unwrap();
    wait_for(|| has_activity(&f, true, "vim") && has_activity(&f, true, "python3"), WAIT).await;
    wait_for(|| runner.calls.lock().unwrap().len() >= 3, WAIT).await;
    assert!(runner.calls.lock().unwrap().iter().all(|(_, args, _)| args.join(" ") == "-eo pid=,ppid=,comm="));
}

#[tokio::test]
async fn keeps_last_known_subprocess_state_when_the_process_snapshot_fails() {
    let fail = Arc::new(AtomicBool::new(false));
    let failed_calls = Arc::new(AtomicUsize::new(0));
    let runner = {
        let (fail, failed_calls) = (fail.clone(), failed_calls.clone());
        fake_runner(move || {
            if fail.load(Ordering::SeqCst) {
                failed_calls.fetch_add(1, Ordering::SeqCst);
                (String::new(), 1)
            } else {
                ("  100  9000 vim".into(), 0)
            }
        })
    };
    let f = create_manager(5, |o| {
        o.subprocess_inspector = None;
        o.process_runner = runner.clone();
        o.platform = Platform::Linux;
        o.subprocess_poll_interval = Duration::from_millis(20);
    })
    .await;
    f.manager.open(open_input()).await.unwrap();
    wait_for(|| has_activity(&f, true, "vim"), WAIT).await;
    fail.store(true, Ordering::SeqCst);
    wait_for(|| failed_calls.load(Ordering::SeqCst) >= 3, Duration::from_secs(3)).await;
    let activity: Vec<_> = f
        .events()
        .into_iter()
        .filter_map(|e| match e.kind {
            TerminalEventKind::Activity { has_running_subprocess, .. } => Some(has_running_subprocess),
            _ => None,
        })
        .collect();
    assert!(!activity.is_empty());
    assert!(activity.iter().all(|busy| *busy), "a failed snapshot is not authoritative");
}

fn table(entries: &[(u32, u32, &str)]) -> zc_terminal::subprocess::ProcessTableSource {
    let entries: Vec<ProcessTableEntry> = entries
        .iter()
        .map(|(pid, ppid, name)| ProcessTableEntry {
            pid: *pid,
            ppid: *ppid,
            name: name.to_string(),
        })
        .collect();
    Arc::new(move || {
        let entries = entries.clone();
        async move { Ok(entries) }.boxed()
    })
}

#[tokio::test]
async fn uses_process_snapshots_from_the_resource_monitor() {
    let calls = Arc::new(AtomicUsize::new(0));
    let f = create_manager(5, |o| {
        let calls = calls.clone();
        let inner = table(&[(100, 9000, "ping.exe")]);
        o.subprocess_inspector = None;
        o.process_table = Some(Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            inner()
        }));
        o.platform = Platform::Windows;
        o.subprocess_poll_interval = Duration::from_millis(20);
    })
    .await;
    f.manager.open(open_input()).await.unwrap();
    wait_for(|| has_activity(&f, true, "ping"), WAIT).await;
    assert!(calls.load(Ordering::SeqCst) > 0);
}

#[tokio::test]
async fn closes_only_a_threads_idle_shells_ignoring_a_helper_forked_from_the_shell() {
    let f = create_manager(5, |o| {
        o.subprocess_inspector = None;
        o.platform = Platform::Linux;
        o.process_table = Some(table(&[
            (9000, 1, "zsh"),
            (100, 9000, "zsh"), // an async prompt worker: a childless copy of the shell
            (9001, 1, "zsh"),
            (200, 9001, "node"),
            (9002, 1, "zsh"),
            (300, 9002, "zsh"), // a subshell with a child is real work
            (301, 300, "sleep"),
            (9003, 1, "zsh"),
        ]));
    })
    .await;
    f.manager.open(open_for("thread-1", "idle")).await.unwrap();
    f.manager.open(open_for("thread-1", "dev-server")).await.unwrap();
    f.manager.open(open_for("thread-1", "subshell")).await.unwrap();
    f.manager.open(open_for("thread-2", DEFAULT_TERMINAL_ID)).await.unwrap();
    f.manager.close_idle("thread-1", None).await;
    let killed: Vec<bool> = (0..4).map(|i| f.pty.process(i).killed()).collect();
    assert_eq!(killed, [true, false, false, false]);
}

#[tokio::test]
async fn keeps_terminals_that_get_input_or_output_while_close_idle_checks_them() {
    // The inspector types into one terminal and echoes into the other while it checks them.
    let manager_cell: Arc<Mutex<Option<TerminalManager>>> = Arc::new(Mutex::new(None));
    let fake = Arc::new(FakePtyAdapter::default());
    let f = create_manager_with(fake.clone(), 5, |o| {
        let (manager_cell, fake) = (manager_cell.clone(), fake.clone());
        o.subprocess_inspector = Some(Arc::new(move |pid| {
            let (manager_cell, fake) = (manager_cell.clone(), fake.clone());
            async move {
                let manager = manager_cell.lock().unwrap().clone().unwrap();
                if pid == 9000 {
                    manager.write(write_input("typed", "make build\r")).await.unwrap();
                } else {
                    let mut events = manager.subscribe();
                    fake.process(1).emit_data("make build\r\n");
                    while let Some(event) = events.next().await {
                        if matches!(event.kind, TerminalEventKind::Output { .. }) {
                            break;
                        }
                    }
                }
                Ok::<_, SubprocessCheckError>(SubprocessInspectResult::default())
            }
            .boxed()
        }));
    })
    .await;
    *manager_cell.lock().unwrap() = Some(f.manager.clone());
    f.manager.open(open_for("thread-1", "typed")).await.unwrap();
    f.manager.open(open_for("thread-1", "echoed")).await.unwrap();
    f.manager.close_idle("thread-1", None).await;
    assert_eq!([f.pty.process(0).killed(), f.pty.process(1).killed()], [false, false]);
}

#[tokio::test]
async fn backs_off_the_spawned_fallback_when_the_resource_monitor_snapshot_fails() {
    let runner = fake_runner(|| ("  100  9000 vim".into(), 0));
    let f = create_manager(5, |o| {
        o.subprocess_inspector = None;
        o.process_runner = runner.clone();
        o.platform = Platform::Linux;
        o.subprocess_poll_interval = Duration::from_millis(20);
        o.process_table = Some(Arc::new(|| {
            async { Err(SubprocessCheckError::new("resource-monitor", "sidecar unavailable")) }.boxed()
        }));
    })
    .await;
    f.manager.open(open_input()).await.unwrap();
    // The fallback's data is still applied while the sidecar is down.
    wait_for(|| has_activity(&f, true, "vim"), WAIT).await;
    wait_for(|| runner.calls.lock().unwrap().len() >= 4, Duration::from_secs(2)).await;
    let calls = runner.calls.lock().unwrap();
    // Four snapshots at the 20 ms cadence would span ~60 ms; the backoff (40 + 80 + 160 ms)
    // stretches them past 150 ms.
    let span = calls[3].2 - calls[0].2;
    assert!(span > Duration::from_millis(150), "{span:?}");
}

#[tokio::test]
async fn caps_persisted_history_to_the_configured_line_limit() {
    let f = create_manager(3, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_data("line1\nline2\nline3\nline4\n");
    wait_for(|| f.event_types().contains(&"output"), WAIT).await;
    f.manager.close(close_thread("thread-1")).await.unwrap();
    let reopened = f.manager.open(open_input()).await.unwrap();
    let lines: Vec<&str> = reopened.history.split('\n').filter(|l| !l.is_empty()).collect();
    assert_eq!(lines, ["line2", "line3", "line4"]);
}

#[tokio::test]
async fn caps_incrementally_appended_history_without_losing_partial_or_empty_lines() {
    let f = create_manager(3, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    for data in ["line1\n", "\n", "line3", "-continued\nline4"] {
        f.pty.process(0).emit_data(data);
    }
    wait_for(|| f.event_types().iter().filter(|t| **t == "output").count() == 4, WAIT).await;
    f.manager.close(close_thread("thread-1")).await.unwrap();
    let reopened = f.manager.open(open_input()).await.unwrap();
    assert_eq!(reopened.history, "\nline3-continued\nline4");
}

#[tokio::test]
async fn bounds_persisted_and_attached_history_without_truncating_live_output() {
    let f = create_manager(5, |o| o.history_byte_limit = 10).await;
    let mut stream = f.manager.attach_stream(open_input().into()).await.unwrap();
    let writes = ["a".repeat(32), "😀\rEND".to_owned()];
    for text in &writes {
        f.pty.process(0).emit_data(text);
    }
    wait_for(|| f.event_types().iter().filter(|t| **t == "output").count() == 2, WAIT).await;
    f.manager.close(close_thread("thread-1")).await.unwrap();
    assert_eq!(read(&f.history_path("thread-1", DEFAULT_TERMINAL_ID)), "aa😀\rEND");
    let reopened = f.manager.open(open_input()).await.unwrap();
    let events = ready(&mut stream);
    let outputs: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            TerminalAttachStreamEvent::Event(TerminalEvent {
                kind: TerminalEventKind::Output { data },
                ..
            }) => Some(data.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(outputs, writes);
    let last = *snapshots(&events).last().unwrap();
    assert_eq!(last.history, "aa😀\rEND");
    assert_eq!(last.sequence, reopened.sequence);
}

#[tokio::test]
async fn reads_only_a_unicode_safe_tail_from_oversized_history() {
    for legacy in [false, true] {
        let f = create_manager(5, |o| o.history_byte_limit = 15).await;
        let next = f.history_path("thread-1", DEFAULT_TERMINAL_ID);
        let source = if legacy { f.logs_dir.join("thread-1.log") } else { next.clone() };
        std::fs::write(&source, format!("{}😀\u{feff}newest\ré", "old".repeat(32_768))).unwrap();
        let snapshot = f.manager.open(open_input()).await.unwrap();
        assert_eq!(snapshot.history, "\u{feff}newest\ré");
        assert_eq!(read(&next), "\u{feff}newest\ré");
        if legacy {
            assert!(!source.exists());
        }
        f.manager.close(close_thread("thread-1")).await.unwrap();
        assert_eq!(f.manager.open(open_input()).await.unwrap().history, "\u{feff}newest\ré");
    }
}

async fn history_after(chunks: &[&str]) -> String {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    for chunk in chunks {
        f.pty.process(0).emit_data(chunk);
    }
    wait_for(|| f.event_types().iter().filter(|t| **t == "output").count() == chunks.len(), WAIT).await;
    f.manager.close(close_thread("thread-1")).await.unwrap();
    f.manager.open(open_input()).await.unwrap().history
}

#[tokio::test]
async fn strips_replay_unsafe_terminal_query_and_reply_sequences_from_persisted_history() {
    assert_eq!(
        history_after(&[
            "prompt ",
            "\u{1b}[32mok\u{1b}[0m ",
            "\u{1b}]11;rgb:ffff/ffff/ffff\u{7}",
            "\u{1b}[1;1R",
            "done\n"
        ])
        .await,
        "prompt \u{1b}[32mok\u{1b}[0m done\n"
    );
    assert_eq!(
        history_after(&[
            "before ",
            "\u{1b}[?2026$",
            "pafter ",
            "\u{1b}P$q ",
            "m\u{1b}",
            "\\after ",
            "\u{9b}?3",
            "1uafter ",
            "\u{90}+q544e",
            "\u{9c}after\n"
        ])
        .await,
        "before after after after after\n"
    );
    assert_eq!(history_after(&["before ", "\u{1b}(", "Bafter\n"]).await, "before \u{1b}(Bafter\n");
}

#[tokio::test]
async fn deletes_history_files_on_close_with_delete_history() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_data("bye\n");
    let path = f.history_path("thread-1", DEFAULT_TERMINAL_ID);
    wait_for(|| path.exists(), WAIT).await;
    f.manager
        .close(TerminalCloseInput {
            delete_history: Some(true),
            ..close_thread("thread-1")
        })
        .await
        .unwrap();
    assert!(!path.exists());
}

#[tokio::test]
async fn closes_all_terminals_for_a_thread_when_close_omits_terminal_id() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_for("thread-1", "default")).await.unwrap();
    f.manager.open(open_for("thread-1", "sidecar")).await.unwrap();
    f.pty.process(0).emit_data("default\n");
    f.pty.process(1).emit_data("sidecar\n");
    let (default_path, sidecar_path) = (f.history_path("thread-1", "default"), f.history_path("thread-1", "sidecar"));
    wait_for(|| default_path.exists() && sidecar_path.exists(), WAIT).await;
    f.manager
        .close(TerminalCloseInput {
            delete_history: Some(true),
            ..close_thread("thread-1")
        })
        .await
        .unwrap();
    assert!(f.pty.process(0).killed() && f.pty.process(1).killed());
    assert!(!default_path.exists() && !sidecar_path.exists());
    let mut closed: Vec<String> = f
        .events()
        .into_iter()
        .filter(|e| matches!(e.kind, TerminalEventKind::Closed))
        .map(|e| e.terminal_id)
        .collect();
    closed.sort();
    assert_eq!(closed, ["default", "sidecar"]);
}

#[tokio::test]
async fn escalates_terminal_shutdown_to_sigkill_when_the_process_does_not_exit_in_time() {
    let f = create_manager(5, |o| o.process_kill_grace = Duration::from_millis(10)).await;
    f.manager.open(open_input()).await.unwrap();
    f.manager.close(close_thread("thread-1")).await.unwrap();
    let process = f.pty.process(0);
    assert_eq!(process.kill_signals(), [PtySignal::Term]);
    wait_for(|| process.kill_signals().contains(&PtySignal::Kill), WAIT).await;
    assert_eq!(process.kill_signals(), [PtySignal::Term, PtySignal::Kill]);
}

#[tokio::test]
async fn evicts_oldest_inactive_terminal_sessions_when_the_retention_limit_is_exceeded() {
    let f = create_manager(5, |o| o.max_retained_inactive_sessions = 1).await;
    f.manager.open(open_for("thread-1", DEFAULT_TERMINAL_ID)).await.unwrap();
    f.manager.open(open_for("thread-2", DEFAULT_TERMINAL_ID)).await.unwrap();
    f.pty.process(0).emit_data("first-history\n");
    f.pty.process(1).emit_data("second-history\n");
    let first_path = f.history_path("thread-1", DEFAULT_TERMINAL_ID);
    wait_for(|| first_path.exists(), WAIT).await;
    f.pty.process(0).emit_exit(0, Some(0));
    tokio::time::sleep(Duration::from_millis(5)).await;
    f.pty.process(1).emit_exit(0, Some(0));
    wait_for(|| f.event_types().iter().filter(|t| **t == "exited").count() == 2, WAIT).await;
    let reopened_second = f.manager.open(open_for("thread-2", DEFAULT_TERMINAL_ID)).await.unwrap();
    let reopened_first = f.manager.open(open_for("thread-1", DEFAULT_TERMINAL_ID)).await.unwrap();
    assert_eq!(reopened_first.history, "first-history\n", "evicted, so restored from disk");
    assert_eq!(reopened_second.history, "", "kept, so reopened clean");
}

#[tokio::test]
async fn migrates_legacy_transcript_filenames_to_the_terminal_scoped_path_on_open() {
    let f = create_manager(5, |_| {}).await;
    let legacy = f.logs_dir.join("thread-1.log");
    std::fs::write(&legacy, "legacy-line\n").unwrap();
    let snapshot = f.manager.open(open_input()).await.unwrap();
    assert_eq!(snapshot.history, "legacy-line\n");
    assert_eq!(read(&f.history_path("thread-1", DEFAULT_TERMINAL_ID)), "legacy-line\n");
    assert!(!legacy.exists());
}

#[tokio::test]
async fn retries_with_fallback_shells_when_the_preferred_shell_spawn_fails() {
    let fake = Arc::new(FakePtyAdapter::default());
    fake.spawn_failures.lock().unwrap().push_back("posix_spawnp failed.".into());
    let f = create_manager_with(fake.clone(), 5, |o| {
        o.shell_resolver = Some(Arc::new(|| "/definitely/missing-shell -l".into()));
        o.platform = Platform::Linux;
    })
    .await;
    let snapshot = f.manager.open(open_input()).await.unwrap();
    assert_eq!(snapshot.status, TerminalSessionStatus::Running);
    let inputs = f.pty.spawn_inputs();
    assert!(inputs.len() >= 2);
    assert_eq!(inputs[0].shell, "/definitely/missing-shell");
    assert!(inputs[1..].iter().any(|i| i.shell != "/definitely/missing-shell"));
}

#[tokio::test]
async fn a_spawn_failure_puts_the_terminal_in_error_and_publishes_it() {
    let fake = Arc::new(FakePtyAdapter::default());
    fake.spawn_failures.lock().unwrap().push_back("permission denied".into());
    let f = create_manager_with(fake.clone(), 5, |o| {
        o.shell_resolver = Some(Arc::new(|| "/bin/sh".into()));
        o.platform = Platform::Linux;
    })
    .await;
    let snapshot = f.manager.open(open_input()).await.unwrap();
    assert_eq!(snapshot.status, TerminalSessionStatus::Error);
    assert_eq!(snapshot.pid, None);
    let message = f.events().into_iter().find_map(|e| match e.kind {
        TerminalEventKind::Error { message } => Some(message),
        _ => None,
    });
    assert_eq!(message.as_deref(), Some("Failed to spawn PTY process '/bin/sh' with fake."));
    assert_eq!(f.pty.spawn_inputs().len(), 1, "not a missing shell, so no fallback");
}

#[tokio::test]
async fn prefers_powershell_over_comspec_and_falls_back_by_absolute_path_on_windows() {
    let windows_env = env(&[
        ("ComSpec", "C:\\Windows\\System32\\cmd.exe"),
        ("PATH", "C:\\Windows\\System32"),
        ("SystemRoot", "C:\\Windows"),
    ]);
    let f = create_manager(5, |o| {
        o.platform = Platform::Windows;
        o.env = Some(windows_env.clone());
    })
    .await;
    f.manager.open(open_input()).await.unwrap();
    let first = &f.pty.spawn_inputs()[0];
    assert_eq!((first.shell.as_str(), first.args.clone()), ("pwsh.exe", vec!["-NoLogo".to_owned()]));

    let fake = Arc::new(FakePtyAdapter::default());
    fake.spawn_failures
        .lock()
        .unwrap()
        .extend(["spawn custom-shell.exe ENOENT".to_owned(), "spawn pwsh.exe ENOENT".to_owned()]);
    let f = create_manager_with(fake.clone(), 5, |o| {
        o.platform = Platform::Windows;
        o.env = Some(windows_env);
        o.shell_resolver = Some(Arc::new(|| "C:\\missing\\custom-shell.exe".into()));
    })
    .await;
    f.manager.open(open_input()).await.unwrap();
    let inputs = f.pty.spawn_inputs();
    let shells: Vec<&str> = inputs.iter().map(|i| i.shell.as_str()).collect();
    assert_eq!(
        shells,
        [
            "C:\\missing\\custom-shell.exe",
            "pwsh.exe",
            "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
        ]
    );
    assert_eq!(inputs[1].args, ["-NoLogo"]);
    assert_eq!(inputs[2].args, ["-NoLogo"]);
}

#[tokio::test]
async fn advertises_truecolor_without_replacing_explicit_values() {
    for platform in [Platform::Linux, Platform::Darwin, Platform::Windows] {
        for (parent, runtime, expected) in [
            (None, None, "truecolor"),
            (Some(""), None, "truecolor"),
            (Some("24bit"), None, "24bit"),
            (Some("24bit"), Some(""), "truecolor"),
            (Some("24bit"), Some("custom"), "custom"),
        ] {
            let f = create_manager(5, |o| {
                o.platform = platform;
                o.shell_resolver = Some(Arc::new(|| "/bin/sh".into()));
                o.env = Some(parent.map_or_else(BTreeMap::new, |p| env(&[("COLORTERM", p)])));
            })
            .await;
            f.manager
                .open(TerminalOpenInput {
                    env: Some(runtime.map_or_else(BTreeMap::new, |r| env(&[("COLORTERM", r)]))),
                    ..open_input()
                })
                .await
                .unwrap();
            assert_eq!(f.pty.spawn_inputs()[0].env["COLORTERM"], expected);
        }
    }
}

#[tokio::test]
async fn filters_app_runtime_env_and_injects_runtime_overrides() {
    let f = create_manager(5, |o| {
        o.env = Some(env(&[
            ("PORT", "5173"),
            ("T3CODE_PORT", "3773"),
            ("VITE_DEV_SERVER_URL", "http://localhost:5173"),
            ("TEST_TERMINAL_KEEP", "keep-me"),
            ("FORCE_COLOR", "3"),
        ]))
    })
    .await;
    f.manager
        .open(TerminalOpenInput {
            env: Some(env(&[
                ("T3CODE_PROJECT_ROOT", "/repo"),
                ("T3CODE_WORKTREE_PATH", "/repo/worktree-a"),
                ("CUSTOM_FLAG", "1"),
                ("NO_COLOR", "1"),
                ("FORCE_COLOR", "0"),
                ("CODEX_HOME", "~/.codex-work"),
            ])),
            ..open_input()
        })
        .await
        .unwrap();
    let spawned = &f.pty.spawn_inputs()[0].env;
    for key in ["PORT", "T3CODE_PORT", "VITE_DEV_SERVER_URL"] {
        assert!(!spawned.contains_key(key), "{key}");
    }
    assert_eq!(spawned["TEST_TERMINAL_KEEP"], "keep-me");
    assert_eq!(spawned["T3CODE_PROJECT_ROOT"], "/repo");
    assert_eq!(spawned["T3CODE_WORKTREE_PATH"], "/repo/worktree-a");
    assert_eq!(spawned["CUSTOM_FLAG"], "1");
    assert_eq!(spawned["NO_COLOR"], "1");
    assert_eq!(spawned["FORCE_COLOR"], "0");
    assert!(spawned["CODEX_HOME"].ends_with("/.codex-work") && !spawned["CODEX_HOME"].starts_with('~'));
}

#[tokio::test]
async fn resolves_a_provider_instance_environment_before_spawning() {
    let f = create_manager(5, |o| {
        o.env = Some(env(&[("T3CODE_SECRET", "server-only")]));
        o.provider_environment = Some(Arc::new(FnProviderEnvironment(|id: &str, env: Option<&BTreeMap<String, String>>| {
            let mut resolved = env.cloned().unwrap_or_default();
            resolved.insert("PROVIDER_SECRET".into(), if id == "codex_work" { "secret-value" } else { "wrong" }.into());
            resolved.insert("CODEX_HOME".into(), "/accounts/codex-work".into());
            Ok(resolved)
        })));
    })
    .await;
    let snapshot = f
        .manager
        .open(TerminalOpenInput {
            provider_instance_id: Some("codex_work".into()),
            env: Some(env(&[("CLIENT_FLAG", "1")])),
            ..open_input()
        })
        .await
        .unwrap();
    let spawned = &f.pty.spawn_inputs()[0].env;
    assert_eq!(spawned["PROVIDER_SECRET"], "secret-value");
    assert_eq!(spawned["CODEX_HOME"], "/accounts/codex-work");
    assert_eq!(spawned["CLIENT_FLAG"], "1");
    assert!(!spawned.contains_key("T3CODE_SECRET"));
    let encoded = serde_json::to_value(&snapshot).unwrap();
    assert!(encoded.get("env").is_none() && encoded.get("providerInstanceId").is_none());
}

#[tokio::test]
async fn fails_closed_when_a_provider_instance_is_missing() {
    let missing = |id: &str, _: Option<&BTreeMap<String, String>>| {
        Err(TerminalError::TerminalProviderInstanceNotFoundError {
            provider_instance_id: id.into(),
        })
    };
    let f = create_manager(5, |o| o.provider_environment = Some(Arc::new(FnProviderEnvironment(missing)))).await;
    let input = TerminalOpenInput {
        provider_instance_id: Some("deleted_instance".into()),
        ..open_input()
    };
    let expected = TerminalError::TerminalProviderInstanceNotFoundError {
        provider_instance_id: "deleted_instance".into(),
    };
    assert_eq!(f.manager.open(input.clone()).await.unwrap_err(), expected);
    assert_eq!(f.manager.attach_stream(input.into()).await.err(), Some(expected));
    assert!(f.pty.spawn_inputs().is_empty());

    // Without any resolver, a provider terminal is unavailable too.
    let f = create_manager(5, |_| {}).await;
    let error = f
        .manager
        .open(TerminalOpenInput {
            provider_instance_id: Some("codex".into()),
            ..open_input()
        })
        .await
        .unwrap_err();
    assert_eq!(error.tag(), "TerminalProviderInstanceNotFoundError");
}

#[tokio::test]
async fn restarts_a_running_terminal_when_the_resolved_provider_environment_changes() {
    let secret = Arc::new(Mutex::new("first-secret".to_owned()));
    let f = create_manager(5, |o| {
        let secret = secret.clone();
        o.provider_environment = Some(Arc::new(FnProviderEnvironment(move |_: &str, _: Option<&BTreeMap<String, String>>| {
            Ok(env(&[("PROVIDER_SECRET", &secret.lock().unwrap())]))
        })));
    })
    .await;
    let input = TerminalOpenInput {
        provider_instance_id: Some("codex_work".into()),
        ..open_input()
    };
    f.manager.open(input.clone()).await.unwrap();
    *secret.lock().unwrap() = "second-secret".into();
    f.manager.open(input).await.unwrap();
    assert!(f.pty.process(0).killed());
    assert_eq!(f.pty.spawn_inputs().len(), 2);
    assert_eq!(f.pty.spawn_inputs()[1].env["PROVIDER_SECRET"], "second-secret");
}

#[tokio::test]
async fn attaches_to_a_running_provider_terminal_without_resolving_the_provider_again() {
    let available = Arc::new(AtomicBool::new(true));
    let f = create_manager(5, |o| {
        let available = available.clone();
        o.provider_environment = Some(Arc::new(FnProviderEnvironment(move |id: &str, _: Option<&BTreeMap<String, String>>| {
            if available.load(Ordering::SeqCst) {
                Ok(env(&[("PROVIDER_SECRET", "secret-value")]))
            } else {
                Err(TerminalError::TerminalProviderInstanceNotFoundError {
                    provider_instance_id: id.into(),
                })
            }
        })));
    })
    .await;
    let input = TerminalOpenInput {
        provider_instance_id: Some("codex_work".into()),
        ..open_input()
    };
    f.manager.open(input.clone()).await.unwrap();
    available.store(false, Ordering::SeqCst);
    let mut attach: TerminalAttachInput = input.into();
    attach.restart_if_not_running = Some(true);
    let mut stream = f.manager.attach_stream(attach).await.unwrap();
    assert_eq!(attach_types(&ready(&mut stream))[0], "snapshot");
    assert_eq!(f.pty.spawn_inputs().len(), 1);
    assert!(!f.pty.process(0).killed());
}

#[tokio::test]
async fn starts_zsh_with_the_prompt_spacer_disabled() {
    let f = create_manager(5, |o| {
        o.shell_resolver = Some(Arc::new(|| "/bin/zsh".into()));
        o.platform = Platform::Darwin;
    })
    .await;
    f.manager.open(open_input()).await.unwrap();
    assert_eq!(f.pty.spawn_inputs()[0].args, ["-o", "nopromptsp"]);
}

#[tokio::test]
async fn pushes_pty_callbacks_to_direct_event_subscribers() {
    let fake = Arc::new(FakePtyAdapter::new(true));
    let f = create_manager_with(fake.clone(), 5, |_| {}).await;
    let mut subscriber = f.manager.subscribe();
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_data("hello from subscriber\n");
    let deadline = tokio::time::timeout(WAIT, async {
        while let Some(event) = subscriber.next().await {
            if matches!(&event.kind, TerminalEventKind::Output { data } if data == "hello from subscriber\n") {
                return;
            }
        }
    });
    deadline.await.unwrap();
}

#[tokio::test]
async fn subscribes_terminal_metadata_with_an_initial_snapshot_and_live_deltas() {
    let f = create_manager(5, |_| {}).await;
    f.manager.open(open_for("existing-thread", DEFAULT_TERMINAL_ID)).await.unwrap();
    let mut metadata = f.manager.subscribe_metadata();
    match metadata.next().await.unwrap() {
        TerminalMetadataStreamEvent::Snapshot { terminals } => {
            assert_eq!(terminals.len(), 1);
            assert_eq!(terminals[0].thread_id, "existing-thread");
            assert_eq!(terminals[0].terminal_id, DEFAULT_TERMINAL_ID);
            assert_eq!(terminals[0].label, "Terminal 1");
        }
        other => panic!("{other:?}"),
    }
    f.manager.open(open_for("new-thread", DEFAULT_TERMINAL_ID)).await.unwrap();
    f.manager
        .close(TerminalCloseInput {
            terminal_id: Some(DEFAULT_TERMINAL_ID.into()),
            ..close_thread("new-thread")
        })
        .await
        .unwrap();
    let events = ready(&mut metadata);
    assert!(events
        .iter()
        .any(|e| matches!(e, TerminalMetadataStreamEvent::Upsert { terminal } if terminal.thread_id == "new-thread")));
    assert!(events.iter().any(
        |e| matches!(e, TerminalMetadataStreamEvent::Remove { thread_id, terminal_id } if thread_id == "new-thread" && terminal_id == DEFAULT_TERMINAL_ID)
    ));
    // Output and clears are not metadata.
    f.pty.process(0).emit_data("x");
    wait_for(|| f.event_types().contains(&"output"), WAIT).await;
    assert!(ready(&mut metadata).is_empty());
}

#[tokio::test]
async fn streams_attach_snapshots_followed_by_live_events_without_duplicate_start_snapshots() {
    let fake = Arc::new(FakePtyAdapter::new(true));
    let f = create_manager_with(fake.clone(), 5, |_| {}).await;
    let mut stream = f.manager.attach_stream(open_input().into()).await.unwrap();
    let first = ready(&mut stream);
    assert_eq!(attach_types(&first), ["snapshot"], "the `started` event is folded into the snapshot");
    f.pty.process(0).emit_data("hello from attach\n");
    let next = tokio::time::timeout(WAIT, stream.next()).await.unwrap().unwrap();
    assert!(matches!(&next, TerminalAttachStreamEvent::Event(TerminalEvent { kind: TerminalEventKind::Output { data }, .. }) if data == "hello from attach\n"));
}

#[tokio::test]
async fn preserves_queued_pty_output_ordering_through_exit_callbacks() {
    let fake = Arc::new(FakePtyAdapter::new(true));
    let f = create_manager_with(fake.clone(), 5, |_| {}).await;
    f.manager.open(open_input()).await.unwrap();
    let process = f.pty.process(0);
    process.emit_data("first\n");
    process.emit_data("second\n");
    process.emit_exit(0, Some(0));
    wait_for(|| has_exited(&f), WAIT).await;
    let relevant: Vec<(String, Option<u64>)> = f
        .events()
        .into_iter()
        .filter_map(|e| match e.kind {
            TerminalEventKind::Output { data } => Some((data, e.sequence)),
            TerminalEventKind::Exited {
                exit_code: Some(0),
                exit_signal: Some(0),
            } => Some(("exited".into(), e.sequence)),
            _ => None,
        })
        .collect();
    assert_eq!(
        relevant,
        [
            ("first\n".to_owned(), Some(2)),
            ("second\n".to_owned(), Some(3)),
            ("exited".to_owned(), Some(4))
        ]
    );
    let mut stream = f
        .manager
        .attach_stream(TerminalAttachInput::new("thread-1", DEFAULT_TERMINAL_ID))
        .await
        .unwrap();
    assert_eq!(snapshots(&ready(&mut stream))[0].sequence, Some(4));
}

#[tokio::test]
async fn attach_without_cwd_to_an_unknown_terminal_fails_with_lookup_error() {
    let f = create_manager(5, |_| {}).await;
    let error = f.manager.attach_stream(TerminalAttachInput::new("thread-1", "missing")).await.err().unwrap();
    assert_eq!(error.tag(), "TerminalSessionLookupError");
}

#[tokio::test]
async fn shutdown_stops_active_terminals_cleanly() {
    let f = create_manager(5, |o| o.process_kill_grace = Duration::from_millis(10)).await;
    f.manager.open(open_input()).await.unwrap();
    f.pty.process(0).emit_data("kept\n");
    wait_for(|| f.event_types().contains(&"output"), WAIT).await;
    f.manager.shutdown().await;
    assert_eq!(f.pty.process(0).kill_signals(), [PtySignal::Term, PtySignal::Kill]);
    assert_eq!(read(&f.history_path("thread-1", DEFAULT_TERMINAL_ID)), "kept\n", "pending writes are flushed");
}

/// Port-level smoke test: the `zc_ports::TerminalManager` view speaks wire JSON.
#[tokio::test]
async fn the_port_speaks_wire_json() {
    use zc_ports::contracts as wire;
    let f = create_manager(5, |_| {}).await;
    let port: Arc<dyn zc_ports::TerminalManager> = Arc::new(f.manager.clone());
    let snapshot = port
        .open(wire::TerminalOpenInput(serde_json::json!({
            "threadId": " thread-1 ", "terminalId": "term-1", "cwd": cwd(), "cols": 100, "rows": 24
        })))
        .await
        .unwrap();
    assert_eq!(snapshot.0["threadId"], "thread-1");
    assert_eq!(snapshot.0["status"], "running");
    assert_eq!(snapshot.0["label"], "Terminal 1");
    let error = port
        .write(wire::TerminalWriteInput(
            serde_json::json!({"threadId": "nope", "terminalId": "x", "data": "a"}),
        ))
        .await
        .unwrap_err();
    assert_eq!(error.tag, "TerminalSessionLookupError");
    assert_eq!(error.message, "Unknown terminal thread: nope, terminal: x");
    let mut events = port.subscribe();
    port.close_idle(&wire::ThreadId::new("thread-1"), None).await;
    assert!(f.pty.process(0).killed(), "an idle shell is closed");
    let closed = tokio::time::timeout(WAIT, events.next()).await.unwrap().unwrap();
    assert_eq!(closed.0["type"], "closed");
}
