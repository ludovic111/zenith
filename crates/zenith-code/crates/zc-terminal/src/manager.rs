//! The terminal manager (`apps/server/src/terminal/Manager.ts`, `makeWithOptions`).
//!
//! Sessions are keyed by `(threadId, terminalId)`. Each holds a PTY process (through
//! [`PtyAdapter`]), its sanitized scrollback, an event sequence and the subprocess state.
//! The behaviour follows the TS service step by step:
//!
//! - **open** reuses a running session; a different cwd, worktree or runtime env restarts it
//!   (history cleared); an exited session is restarted with a clean history; a new session
//!   restores the persisted history first. Opening waits for the thread's lock and the
//!   checkout's workspace lease.
//! - **attach** subscribes before reading the snapshot, sends the snapshot, then the events
//!   buffered meanwhile (minus those already in the snapshot), then live events. It only
//!   (re)starts a terminal when there is none, or with `restartIfNotRunning`.
//! - **output** goes through a per-session queue drained by one task: sanitize, append to the
//!   history, bump the sequence, queue a debounced write, publish. The exit is queued behind
//!   the output, so `exited` always comes after the last `output`.
//! - **stop** (close, restart, reopen) sends `SIGTERM`, then `SIGKILL` after the grace period
//!   (1 s), in the background.
//! - **subprocess polling** reads one process table per tick for every running terminal,
//!   publishes `activity` events when the busy state or child command changes, and backs off
//!   (×2 per failed snapshot, up to 60 s).
//! - **eviction** keeps at most 128 inactive sessions in memory (oldest first out).
//!
//! Events go to listeners synchronously, in publish order; listeners never block (they push
//! into unbounded channels), and nothing is published while a manager lock is held.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use tokio::sync::{mpsc, Notify};
use tokio_util::sync::CancellationToken;
use zc_core::shell_env::Platform;
use zc_core::{now_iso, Defect, ProcessRunner};

use crate::contracts::{
    TerminalAttachInput, TerminalAttachStreamEvent, TerminalClearInput, TerminalCloseInput, TerminalError, TerminalEvent, TerminalEventKind,
    TerminalMetadataStreamEvent, TerminalOpenInput, TerminalResizeInput, TerminalRestartInput, TerminalSessionSnapshot, TerminalSessionStatus, TerminalSummary,
    TerminalWriteInput,
};
use crate::history::{BoundedTerminalHistory, DEFAULT_HISTORY_BYTE_LIMIT, DEFAULT_HISTORY_LINE_LIMIT};
use crate::lease::with_workspace_lease;
use crate::persist::{HistoryStore, PersistRequest, PersistWorker, DEFAULT_PERSIST_DEBOUNCE};
use crate::pty::{PtyAdapter, PtyExitEvent, PtyProcess, PtySignal, PtySpawnError, PtySpawnInput, Unsubscribe};
use crate::sanitizer::sanitize_terminal_history_chunk;
use crate::shell::{
    create_terminal_spawn_env, default_shell, is_retryable_shell_spawn_error, normalized_runtime_env, process_env, resolve_shell_candidates, terminal_label,
    truncate_terminal_wire_label, ShellCandidate,
};
use crate::subprocess::{
    derive_subprocess_inspect_result, fallback_process_table, resolve_posix_ps_command, snapshot_from_entries, subprocess_snapshot_poll_delay,
    ProcessTableSnapshot, ProcessTableSource, SubprocessCheckError, SubprocessInspectResult, SubprocessInspector, DEFAULT_SUBPROCESS_POLL_INTERVAL,
};

/// `DEFAULT_PROCESS_KILL_GRACE_MS`.
pub const DEFAULT_PROCESS_KILL_GRACE: Duration = Duration::from_millis(1_000);
/// `DEFAULT_MAX_RETAINED_INACTIVE_SESSIONS`.
pub const DEFAULT_MAX_RETAINED_INACTIVE_SESSIONS: usize = 128;
/// `DEFAULT_OPEN_COLS`.
pub const DEFAULT_OPEN_COLS: u16 = 120;
/// `DEFAULT_OPEN_ROWS`.
pub const DEFAULT_OPEN_ROWS: u16 = 30;

/// Port discovery's view of terminals (`PortDiscovery.registerTerminalProcesses` /
/// `unregisterTerminal`).
#[async_trait]
pub trait TerminalProcessRegistry: Send + Sync {
    async fn register_terminal_processes(&self, thread_id: &str, terminal_id: &str, process_ids: &[u32]);
    async fn unregister_terminal(&self, thread_id: &str, terminal_id: &str);
}

/// Resolves the environment of a provider instance's terminal
/// (`resolveProviderInstanceTerminalEnvironment`: the instance's environment variables merged
/// with the client's, plus `CODEX_HOME` / `CLAUDE_CONFIG_DIR`). Implemented where the server
/// settings and provider homes live; without one, terminals that name a provider instance
/// fail with `TerminalProviderInstanceNotFoundError`, as in TS.
#[async_trait]
pub trait ProviderEnvironmentResolver: Send + Sync {
    async fn resolve(&self, provider_instance_id: &str, env: Option<&BTreeMap<String, String>>) -> Result<BTreeMap<String, String>, TerminalError>;
}

/// A resolver from a closure.
pub struct FnProviderEnvironment<F>(pub F);

#[async_trait]
impl<F> ProviderEnvironmentResolver for FnProviderEnvironment<F>
where
    F: Fn(&str, Option<&BTreeMap<String, String>>) -> Result<BTreeMap<String, String>, TerminalError> + Send + Sync,
{
    async fn resolve(&self, provider_instance_id: &str, env: Option<&BTreeMap<String, String>>) -> Result<BTreeMap<String, String>, TerminalError> {
        (self.0)(provider_instance_id, env)
    }
}

/// `TerminalManagerOptions`.
#[derive(Clone)]
pub struct TerminalManagerOptions {
    /// `<stateDir>/logs/terminals`.
    pub logs_dir: PathBuf,
    pub history_line_limit: usize,
    pub history_byte_limit: usize,
    pub pty_adapter: Arc<dyn PtyAdapter>,
    /// The requested shell (default: `$SHELL`, else `bash`).
    pub shell_resolver: Option<Arc<dyn Fn() -> String + Send + Sync>>,
    /// The environment terminals inherit (default: the server's).
    pub env: Option<BTreeMap<String, String>>,
    pub platform: Platform,
    /// Replaces the process table entirely (tests).
    pub subprocess_inspector: Option<SubprocessInspector>,
    /// The resource monitor's process table; `ps` is the fallback.
    pub process_table: Option<ProcessTableSource>,
    pub process_runner: Arc<dyn ProcessRunner>,
    pub subprocess_poll_interval: Duration,
    pub process_kill_grace: Duration,
    pub max_retained_inactive_sessions: usize,
    pub process_registry: Option<Arc<dyn TerminalProcessRegistry>>,
    pub provider_environment: Option<Arc<dyn ProviderEnvironmentResolver>>,
    pub persist_debounce: Duration,
}

impl TerminalManagerOptions {
    /// The TS defaults.
    pub fn new(logs_dir: impl Into<PathBuf>, pty_adapter: Arc<dyn PtyAdapter>) -> Self {
        Self {
            logs_dir: logs_dir.into(),
            history_line_limit: DEFAULT_HISTORY_LINE_LIMIT,
            history_byte_limit: DEFAULT_HISTORY_BYTE_LIMIT,
            pty_adapter,
            shell_resolver: None,
            env: None,
            platform: Platform::current(),
            subprocess_inspector: None,
            process_table: None,
            process_runner: Arc::new(zc_core::process::SystemProcessRunner),
            subprocess_poll_interval: DEFAULT_SUBPROCESS_POLL_INTERVAL,
            process_kill_grace: DEFAULT_PROCESS_KILL_GRACE,
            max_retained_inactive_sessions: DEFAULT_MAX_RETAINED_INACTIVE_SESSIONS,
            process_registry: None,
            provider_environment: None,
            persist_debounce: DEFAULT_PERSIST_DEBOUNCE,
        }
    }
}

type SessionKey = (String, String);
type EventListener = Arc<dyn Fn(&TerminalEvent) + Send + Sync>;

enum PendingProcessEvent {
    Output(String),
    Exit(PtyExitEvent),
}

struct SessionState {
    thread_id: String,
    terminal_id: String,
    cwd: String,
    worktree_path: Option<String>,
    status: TerminalSessionStatus,
    pid: Option<u32>,
    history: BoundedTerminalHistory,
    pending_history_control_sequence: String,
    pending_events: VecDeque<PendingProcessEvent>,
    exit_code: Option<i32>,
    exit_signal: Option<i32>,
    updated_at: String,
    event_sequence: u64,
    /// Counts writes, so `close_idle` can see input that has not echoed yet.
    input_count: u64,
    cols: u16,
    rows: u16,
    process: Option<Arc<dyn PtyProcess>>,
    unsubscribe_data: Option<Unsubscribe>,
    unsubscribe_exit: Option<Unsubscribe>,
    /// Wakes the drain task of the current process.
    drain_notify: Arc<Notify>,
    has_running_subprocess: bool,
    /// Normalized child command name while `has_running_subprocess`.
    child_command_label: Option<String>,
    runtime_env: Option<BTreeMap<String, String>>,
}

struct Session {
    state: Mutex<SessionState>,
}

impl SessionState {
    fn advance_event_sequence(&mut self) -> u64 {
        self.event_sequence += 1;
        self.updated_at = now_iso();
        self.event_sequence
    }

    fn cleanup_process_handles(&mut self) {
        if let Some(unsubscribe) = self.unsubscribe_data.take() {
            unsubscribe.call();
        }
        if let Some(unsubscribe) = self.unsubscribe_exit.take() {
            unsubscribe.call();
        }
    }

    fn clear_pending(&mut self) {
        self.pending_history_control_sequence.clear();
        self.pending_events.clear();
    }

    fn wire_label(&self) -> String {
        if self.has_running_subprocess {
            if let Some(label) = &self.child_command_label {
                let trimmed = label.trim();
                if !trimmed.is_empty() {
                    return truncate_terminal_wire_label(trimmed);
                }
            }
        }
        truncate_terminal_wire_label(&terminal_label(&self.terminal_id))
    }

    fn snapshot(&mut self) -> TerminalSessionSnapshot {
        TerminalSessionSnapshot {
            thread_id: self.thread_id.clone(),
            terminal_id: self.terminal_id.clone(),
            cwd: self.cwd.clone(),
            worktree_path: self.worktree_path.clone(),
            status: self.status,
            pid: self.pid,
            history: self.history.to_value(),
            exit_code: self.exit_code,
            exit_signal: self.exit_signal,
            label: self.wire_label(),
            updated_at: self.updated_at.clone(),
            sequence: Some(self.event_sequence),
        }
    }

    fn summary(&self) -> TerminalSummary {
        TerminalSummary {
            thread_id: self.thread_id.clone(),
            terminal_id: self.terminal_id.clone(),
            cwd: self.cwd.clone(),
            worktree_path: self.worktree_path.clone(),
            status: self.status,
            pid: self.pid,
            exit_code: self.exit_code,
            exit_signal: self.exit_signal,
            has_running_subprocess: self.has_running_subprocess,
            label: self.wire_label(),
            updated_at: self.updated_at.clone(),
        }
    }
}

fn process_key(process: &Arc<dyn PtyProcess>) -> usize {
    Arc::as_ptr(process) as *const () as usize
}

fn same_process(a: &Option<Arc<dyn PtyProcess>>, b: &Arc<dyn PtyProcess>) -> bool {
    a.as_ref().is_some_and(|a| process_key(a) == process_key(b))
}

/// Where the subprocess state comes from on one tick.
enum Inspector {
    Custom(SubprocessInspector),
    Table(Arc<ProcessTableSnapshot>),
}

#[derive(Clone)]
struct StartInput {
    cwd: String,
    worktree_path: Option<String>,
    cols: u16,
    rows: u16,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StartKind {
    Started,
    Restarted,
}

struct Inner {
    history: HistoryStore,
    persist: PersistWorker,
    pty: Arc<dyn PtyAdapter>,
    base_env: BTreeMap<String, String>,
    shell_resolver: Arc<dyn Fn() -> String + Send + Sync>,
    platform: Platform,
    inspector_override: Option<SubprocessInspector>,
    process_table: Option<ProcessTableSource>,
    process_runner: Arc<dyn ProcessRunner>,
    ps_command: String,
    poll_interval: Duration,
    kill_grace: Duration,
    max_retained_inactive: usize,
    registry: Option<Arc<dyn TerminalProcessRegistry>>,
    provider_env: Option<Arc<dyn ProviderEnvironmentResolver>>,
    sessions: Mutex<HashMap<SessionKey, Arc<Session>>>,
    thread_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    listeners: Mutex<Vec<(u64, EventListener)>>,
    next_listener: AtomicU64,
    kill_tasks: Mutex<HashMap<usize, tokio::task::AbortHandle>>,
    shutdown: CancellationToken,
}

/// The terminal manager. Cheap to clone; every clone is the same manager.
#[derive(Clone)]
pub struct TerminalManager {
    inner: Arc<Inner>,
}

/// Removes a listener when dropped.
struct ListenerGuard {
    inner: Weak<Inner>,
    id: u64,
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.listeners.lock().unwrap().retain(|(id, _)| *id != self.id);
        }
    }
}

/// A live subscription: items buffered at subscription time first, then live ones. Dropping
/// it unsubscribes.
pub struct ListenerStream<T> {
    buffered: VecDeque<T>,
    receiver: mpsc::UnboundedReceiver<T>,
    _guard: ListenerGuard,
}

impl<T> Stream for ListenerStream<T> {
    type Item = T;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<T>> {
        if let Some(item) = self.buffered.pop_front() {
            return std::task::Poll::Ready(Some(item));
        }
        self.receiver.poll_recv(cx)
    }
}

impl<T> Unpin for ListenerStream<T> {}

/// `shouldPublishTerminalMetadataEvent`.
fn should_publish_metadata(event: &TerminalEvent) -> bool {
    !matches!(event.kind, TerminalEventKind::Output { .. } | TerminalEventKind::Cleared)
}

/// `terminalEventToAttachEvent`.
fn to_attach_event(event: TerminalEvent) -> TerminalAttachStreamEvent {
    match event.kind {
        TerminalEventKind::Started { snapshot } => TerminalAttachStreamEvent::Snapshot(snapshot),
        _ => TerminalAttachStreamEvent::Event(event),
    }
}

fn attach_event_sequence(event: &TerminalAttachStreamEvent) -> Option<u64> {
    match event {
        TerminalAttachStreamEvent::Snapshot(snapshot) => snapshot.sequence,
        TerminalAttachStreamEvent::Event(event) => event.sequence,
    }
}

/// `isDuplicateAttachSnapshotEvent`, on the converted event.
fn is_duplicate_attach_event(event: &TerminalAttachStreamEvent, initial: &TerminalSessionSnapshot) -> bool {
    match (attach_event_sequence(event), initial.sequence) {
        (Some(sequence), Some(initial_sequence)) => sequence <= initial_sequence,
        _ => match event {
            TerminalAttachStreamEvent::Snapshot(snapshot) => {
                snapshot.thread_id == initial.thread_id && snapshot.terminal_id == initial.terminal_id && snapshot.updated_at <= initial.updated_at
            }
            TerminalAttachStreamEvent::Event(_) => false,
        },
    }
}

/// The launch fields shared by open / attach / restart inputs.
trait LaunchInput {
    fn provider_instance_id(&self) -> Option<&str>;
    fn env(&self) -> Option<&BTreeMap<String, String>>;
    fn set_env(&mut self, env: BTreeMap<String, String>);
}

macro_rules! launch_input {
    ($ty:ty) => {
        impl LaunchInput for $ty {
            fn provider_instance_id(&self) -> Option<&str> {
                self.provider_instance_id.as_deref()
            }
            fn env(&self) -> Option<&BTreeMap<String, String>> {
                self.env.as_ref()
            }
            fn set_env(&mut self, env: BTreeMap<String, String>) {
                self.env = Some(env);
            }
        }
    };
}
launch_input!(TerminalOpenInput);
launch_input!(TerminalRestartInput);

impl TerminalManager {
    /// `makeWithOptions`. Creates the logs directory and starts subprocess polling; must be
    /// called inside a Tokio runtime.
    pub async fn new(options: TerminalManagerOptions) -> std::io::Result<Self> {
        tokio::fs::create_dir_all(&options.logs_dir).await?;
        let base_env = options.env.clone().unwrap_or_else(process_env);
        let platform = options.platform;
        let shell_resolver = options.shell_resolver.clone().unwrap_or_else(|| {
            let env = base_env.clone();
            Arc::new(move || default_shell(platform, &env))
        });
        let ps_command = if platform == Platform::Windows {
            String::new()
        } else {
            resolve_posix_ps_command()
        };
        let inner = Arc::new(Inner {
            history: HistoryStore {
                logs_dir: options.logs_dir.clone(),
                line_limit: options.history_line_limit,
                byte_limit: options.history_byte_limit,
            },
            persist: PersistWorker::new(options.persist_debounce),
            pty: options.pty_adapter,
            base_env,
            shell_resolver,
            platform,
            inspector_override: options.subprocess_inspector,
            process_table: options.process_table,
            process_runner: options.process_runner,
            ps_command,
            poll_interval: options.subprocess_poll_interval,
            kill_grace: options.process_kill_grace,
            max_retained_inactive: options.max_retained_inactive_sessions,
            registry: options.process_registry,
            provider_env: options.provider_environment,
            sessions: Mutex::new(HashMap::new()),
            thread_locks: Mutex::new(HashMap::new()),
            listeners: Mutex::new(Vec::new()),
            next_listener: AtomicU64::new(1),
            kill_tasks: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
        });
        tokio::spawn(poll_loop(Arc::downgrade(&inner), inner.shutdown.clone()));
        Ok(Self { inner })
    }

    /// Where the history files are.
    pub fn history_store(&self) -> &HistoryStore {
        &self.inner.history
    }

    /// `open`.
    pub async fn open(&self, input: TerminalOpenInput) -> Result<TerminalSessionSnapshot, TerminalError> {
        let inner = &self.inner;
        let _lock = inner.thread_lock(&input.thread_id).await;
        let input = inner.resolve_launch_input_environment(input).await?;
        inner.open_locked(input).await
    }

    /// `attachStream`: the snapshot, then the terminal's events.
    pub async fn attach_stream(&self, input: TerminalAttachInput) -> Result<ListenerStream<TerminalAttachStreamEvent>, TerminalError> {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let guard = {
            let thread_id = input.thread_id.clone();
            let terminal_id = input.terminal_id.clone();
            self.inner.add_listener(Arc::new(move |event: &TerminalEvent| {
                if event.thread_id == thread_id && event.terminal_id == terminal_id {
                    let _ = sender.send(to_attach_event(event.clone()));
                }
            }))
        };
        let snapshot = self.inner.open_or_attach_for_stream(input).await?;
        let mut buffered = VecDeque::new();
        while let Ok(event) = receiver.try_recv() {
            if !is_duplicate_attach_event(&event, &snapshot) {
                buffered.push_back(event);
            }
        }
        buffered.push_front(TerminalAttachStreamEvent::Snapshot(Box::new(snapshot)));
        Ok(ListenerStream {
            buffered,
            receiver,
            _guard: guard,
        })
    }

    /// `write`. Writing to an exited terminal is ignored.
    pub async fn write(&self, input: TerminalWriteInput) -> Result<(), TerminalError> {
        let session = self.inner.require_session(&input.thread_id, &input.terminal_id)?;
        let process = {
            let mut state = session.state.lock().unwrap();
            match (&state.process, state.status) {
                (Some(process), TerminalSessionStatus::Running) => {
                    let process = process.clone();
                    state.input_count += 1;
                    process
                }
                (_, TerminalSessionStatus::Exited) => return Ok(()),
                _ => {
                    return Err(TerminalError::TerminalNotRunningError {
                        thread_id: input.thread_id,
                        terminal_id: input.terminal_id,
                    })
                }
            }
        };
        process.write(&input.data).map_err(|error| TerminalError::TerminalWriteError {
            thread_id: input.thread_id,
            terminal_id: input.terminal_id,
            terminal_pid: process.pid(),
            cause: error.0,
        })
    }

    /// `resize`. Unknown or stopped terminals are ignored (resize traffic can outlive them).
    pub async fn resize(&self, input: TerminalResizeInput) -> Result<(), TerminalError> {
        let inner = &self.inner;
        let _lock = inner.thread_lock(&input.thread_id).await;
        let Some(session) = inner.get_session(&input.thread_id, &input.terminal_id) else {
            return Ok(());
        };
        let process = {
            let state = session.state.lock().unwrap();
            match (&state.process, state.status) {
                (Some(process), TerminalSessionStatus::Running) => process.clone(),
                _ => return Ok(()),
            }
        };
        resize_process(&session, &process, input.cols, input.rows)
    }

    /// `clear`: empties the history and publishes `cleared`.
    pub async fn clear(&self, input: TerminalClearInput) -> Result<(), TerminalError> {
        let inner = &self.inner;
        let _lock = inner.thread_lock(&input.thread_id).await;
        let session = inner.require_session(&input.thread_id, &input.terminal_id)?;
        let sequence = {
            let mut state = session.state.lock().unwrap();
            state.history.clear();
            state.clear_pending();
            state.advance_event_sequence()
        };
        inner.persist_history(&input.thread_id, &input.terminal_id, &session).await;
        inner.publish(TerminalEvent {
            thread_id: input.thread_id,
            terminal_id: input.terminal_id,
            sequence: Some(sequence),
            kind: TerminalEventKind::Cleared,
        });
        Ok(())
    }

    /// `restart`: always a fresh history and a new process.
    pub async fn restart(&self, input: TerminalRestartInput) -> Result<TerminalSessionSnapshot, TerminalError> {
        let inner = &self.inner;
        let _lock = inner.thread_lock(&input.thread_id).await;
        let input = inner.resolve_launch_input_environment(input).await?;
        let lease = lease_path(input.worktree_path.as_ref(), &input.cwd);
        with_workspace_lease(&lease, inner.restart_resolved(input)).await
    }

    /// `close`: one terminal, or all of the thread's.
    pub async fn close(&self, input: TerminalCloseInput) -> Result<(), TerminalError> {
        let inner = &self.inner;
        let _lock = inner.thread_lock(&input.thread_id).await;
        let delete_history = input.delete_history == Some(true);
        if let Some(terminal_id) = &input.terminal_id {
            inner.close_session(&input.thread_id, terminal_id, delete_history).await;
            return Ok(());
        }
        for terminal_id in inner.terminal_ids_for_thread(&input.thread_id) {
            inner.close_session(&input.thread_id, &terminal_id, false).await;
        }
        if delete_history {
            inner.history.delete_all_history_for_thread(&input.thread_id).await;
        }
        Ok(())
    }

    /// `closeIdle`: closes the thread's terminals that sit at an idle prompt. Never fails.
    pub async fn close_idle(&self, thread_id: &str, terminal_id: Option<&str>) {
        let inner = &self.inner;
        let _lock = inner.thread_lock(thread_id).await;
        if let Err(error) = inner.close_idle_locked(thread_id, terminal_id).await {
            tracing::warn!(thread_id, %error, "failed to close idle terminals");
        }
    }

    /// `subscribe`: every terminal event from now on.
    pub fn subscribe(&self) -> ListenerStream<TerminalEvent> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let guard = self.inner.add_listener(Arc::new(move |event: &TerminalEvent| {
            let _ = sender.send(event.clone());
        }));
        ListenerStream {
            buffered: VecDeque::new(),
            receiver,
            _guard: guard,
        }
    }

    /// `subscribeMetadata`: a snapshot of every terminal, then upserts and removals.
    pub fn subscribe_metadata(&self) -> ListenerStream<TerminalMetadataStreamEvent> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let weak = Arc::downgrade(&self.inner);
        let guard = self.inner.add_listener(Arc::new(move |event: &TerminalEvent| {
            if !should_publish_metadata(event) {
                return;
            }
            let metadata = if matches!(event.kind, TerminalEventKind::Closed) {
                Some(TerminalMetadataStreamEvent::Remove {
                    thread_id: event.thread_id.clone(),
                    terminal_id: event.terminal_id.clone(),
                })
            } else {
                weak.upgrade()
                    .and_then(|inner| inner.read_terminal_metadata(&event.thread_id, &event.terminal_id))
                    .map(|terminal| TerminalMetadataStreamEvent::Upsert { terminal })
            };
            if let Some(metadata) = metadata {
                let _ = sender.send(metadata);
            }
        }));
        let terminals = self.inner.read_all_terminal_metadata();
        ListenerStream {
            buffered: VecDeque::from([TerminalMetadataStreamEvent::Snapshot { terminals }]),
            receiver,
            _guard: guard,
        }
    }

    /// Stops every terminal (`SIGTERM`, then `SIGKILL` after the grace period), stops polling
    /// and flushes pending history writes. The scope finalizer of the TS layer.
    pub async fn shutdown(&self) {
        let inner = &self.inner;
        inner.shutdown.cancel();
        let sessions: Vec<Arc<Session>> = inner.sessions.lock().unwrap().drain().map(|(_, s)| s).collect();
        let mut kills = Vec::new();
        for session in sessions {
            let (process, thread_id, terminal_id) = {
                let mut state = session.state.lock().unwrap();
                state.cleanup_process_handles();
                (state.process.clone(), state.thread_id.clone(), state.terminal_id.clone())
            };
            if let Some(process) = process {
                inner.clear_kill_task(&process);
                let grace = inner.kill_grace;
                kills.push(async move {
                    run_kill_escalation(process, &thread_id, &terminal_id, grace).await;
                });
            }
        }
        futures::future::join_all(kills).await;
        let tasks: Vec<_> = inner.kill_tasks.lock().unwrap().drain().map(|(_, task)| task).collect();
        for task in tasks {
            task.abort();
        }
        inner.persist.drain_all().await;
    }
}

fn lease_path(worktree_path: Option<&Option<String>>, cwd: &str) -> PathBuf {
    let target = worktree_path.and_then(|w| w.as_deref()).unwrap_or(cwd);
    zc_core::paths::resolve_path(std::path::Path::new(target))
}

fn resize_process(session: &Session, process: &Arc<dyn PtyProcess>, cols: u16, rows: u16) -> Result<(), TerminalError> {
    let (thread_id, terminal_id) = {
        let state = session.state.lock().unwrap();
        (state.thread_id.clone(), state.terminal_id.clone())
    };
    process.resize(cols, rows).map_err(|error| TerminalError::TerminalResizeError {
        thread_id,
        terminal_id,
        terminal_pid: process.pid(),
        cols,
        rows,
        cause: error.0,
    })?;
    let mut state = session.state.lock().unwrap();
    state.cols = cols;
    state.rows = rows;
    state.updated_at = now_iso();
    Ok(())
}

async fn run_kill_escalation(process: Arc<dyn PtyProcess>, thread_id: &str, terminal_id: &str, grace: Duration) {
    if let Err(error) = process.kill(PtySignal::Term) {
        tracing::warn!(thread_id, terminal_id, signal = "SIGTERM", %error, "failed to kill terminal process");
        return;
    }
    tokio::time::sleep(grace).await;
    if let Err(error) = process.kill(PtySignal::Kill) {
        tracing::warn!(thread_id, terminal_id, signal = "SIGKILL", %error, "failed to force-kill terminal process");
    }
}

/// The subprocess poller: one tick per interval while terminals run, backing off when the
/// process table cannot be read.
async fn poll_loop(inner: Weak<Inner>, shutdown: CancellationToken) {
    let mut failures: u32 = 0;
    loop {
        let delay = {
            let Some(inner) = inner.upgrade() else { return };
            if inner.has_running_sessions() {
                let succeeded = inner.poll_subprocess_activity().await;
                failures = if succeeded { 0 } else { (failures + 1).min(30) };
                subprocess_snapshot_poll_delay(inner.poll_interval, failures)
            } else {
                failures = 0;
                inner.poll_interval
            }
        };
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

impl Inner {
    // --- events -------------------------------------------------------------------------

    fn add_listener(self: &Arc<Self>, listener: EventListener) -> ListenerGuard {
        let id = self.next_listener.fetch_add(1, Ordering::Relaxed);
        self.listeners.lock().unwrap().push((id, listener));
        ListenerGuard {
            inner: Arc::downgrade(self),
            id,
        }
    }

    fn publish(&self, event: TerminalEvent) {
        let listeners: Vec<EventListener> = self.listeners.lock().unwrap().iter().map(|(_, listener)| listener.clone()).collect();
        for listener in listeners {
            listener(&event);
        }
    }

    // --- sessions -----------------------------------------------------------------------

    async fn thread_lock(&self, thread_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self.thread_locks.lock().unwrap().entry(thread_id.to_owned()).or_default().clone();
        lock.lock_owned().await
    }

    fn get_session(&self, thread_id: &str, terminal_id: &str) -> Option<Arc<Session>> {
        self.sessions.lock().unwrap().get(&(thread_id.to_owned(), terminal_id.to_owned())).cloned()
    }

    fn require_session(&self, thread_id: &str, terminal_id: &str) -> Result<Arc<Session>, TerminalError> {
        self.get_session(thread_id, terminal_id)
            .ok_or_else(|| TerminalError::TerminalSessionLookupError {
                thread_id: thread_id.to_owned(),
                terminal_id: terminal_id.to_owned(),
            })
    }

    fn terminal_ids_for_thread(&self, thread_id: &str) -> Vec<String> {
        self.sessions
            .lock()
            .unwrap()
            .keys()
            .filter(|(thread, _)| thread == thread_id)
            .map(|(_, terminal)| terminal.clone())
            .collect()
    }

    fn has_running_sessions(&self) -> bool {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        sessions
            .iter()
            .any(|session| session.state.lock().unwrap().status == TerminalSessionStatus::Running)
    }

    fn read_terminal_metadata(&self, thread_id: &str, terminal_id: &str) -> Option<TerminalSummary> {
        self.get_session(thread_id, terminal_id).map(|session| session.state.lock().unwrap().summary())
    }

    fn read_all_terminal_metadata(&self) -> Vec<TerminalSummary> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        let mut terminals: Vec<TerminalSummary> = sessions.iter().map(|session| session.state.lock().unwrap().summary()).collect();
        terminals.sort_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.thread_id.cmp(&right.thread_id))
                .then_with(|| left.terminal_id.cmp(&right.terminal_id))
        });
        terminals
    }

    /// `evictInactiveSessionsIfNeeded`: forgets the oldest stopped sessions beyond the limit.
    fn evict_inactive_sessions_if_needed(&self) {
        let mut sessions = self.sessions.lock().unwrap();
        let mut inactive: Vec<(String, String, String)> = sessions
            .values()
            .filter_map(|session| {
                let state = session.state.lock().unwrap();
                (state.status != TerminalSessionStatus::Running).then(|| (state.updated_at.clone(), state.thread_id.clone(), state.terminal_id.clone()))
            })
            .collect();
        if inactive.len() <= self.max_retained_inactive {
            return;
        }
        inactive.sort();
        let to_evict = inactive.len() - self.max_retained_inactive;
        for (_, thread_id, terminal_id) in inactive.into_iter().take(to_evict) {
            sessions.remove(&(thread_id, terminal_id));
        }
    }

    fn new_session_state(
        &self,
        thread_id: &str,
        terminal_id: &str,
        start: &StartInput,
        history: BoundedTerminalHistory,
        runtime_env: Option<BTreeMap<String, String>>,
    ) -> SessionState {
        let StartInput {
            cwd,
            worktree_path,
            cols,
            rows,
        } = start.clone();
        SessionState {
            thread_id: thread_id.to_owned(),
            terminal_id: terminal_id.to_owned(),
            cwd,
            worktree_path,
            status: TerminalSessionStatus::Starting,
            pid: None,
            history,
            pending_history_control_sequence: String::new(),
            pending_events: VecDeque::new(),
            exit_code: None,
            exit_signal: None,
            updated_at: now_iso(),
            event_sequence: 0,
            input_count: 0,
            cols,
            rows,
            process: None,
            unsubscribe_data: None,
            unsubscribe_exit: None,
            drain_notify: Arc::new(Notify::new()),
            has_running_subprocess: false,
            child_command_label: None,
            runtime_env,
        }
    }

    fn insert_session(&self, state: SessionState) -> Arc<Session> {
        let key = (state.thread_id.clone(), state.terminal_id.clone());
        let session = Arc::new(Session { state: Mutex::new(state) });
        self.sessions.lock().unwrap().insert(key, session.clone());
        session
    }

    // --- persistence --------------------------------------------------------------------

    fn persist_request(&self, thread_id: &str, terminal_id: &str, session: &Arc<Session>, immediate: bool) -> PersistRequest {
        let source = {
            let session = session.clone();
            Arc::new(move || session.state.lock().unwrap().history.to_value()) as Arc<dyn Fn() -> String + Send + Sync>
        };
        PersistRequest {
            path: self.history.history_path(thread_id, terminal_id),
            source,
            immediate,
        }
    }

    fn queue_persist(&self, thread_id: &str, terminal_id: &str, session: &Arc<Session>) {
        let request = self.persist_request(thread_id, terminal_id, session, false);
        self.persist.enqueue(&persist_key(thread_id, terminal_id), request);
    }

    async fn flush_persist(&self, thread_id: &str, terminal_id: &str) {
        self.persist.drain_key(&persist_key(thread_id, terminal_id)).await;
    }

    async fn persist_history(&self, thread_id: &str, terminal_id: &str, session: &Arc<Session>) {
        let request = self.persist_request(thread_id, terminal_id, session, true);
        self.persist.enqueue(&persist_key(thread_id, terminal_id), request);
        self.flush_persist(thread_id, terminal_id).await;
    }

    // --- process lifecycle ----------------------------------------------------------------

    async fn unregister(&self, thread_id: &str, terminal_id: &str) {
        if let Some(registry) = &self.registry {
            registry.unregister_terminal(thread_id, terminal_id).await;
        }
    }

    fn clear_kill_task(&self, process: &Arc<dyn PtyProcess>) {
        if let Some(task) = self.kill_tasks.lock().unwrap().remove(&process_key(process)) {
            task.abort();
        }
    }

    fn start_kill_escalation(self: &Arc<Self>, process: Arc<dyn PtyProcess>, thread_id: String, terminal_id: String) {
        let key = process_key(&process);
        let grace = self.kill_grace;
        let weak = Arc::downgrade(self);
        // Registered before the task can finish, so the task's own removal always finds it.
        let mut tasks = self.kill_tasks.lock().unwrap();
        let task = tokio::spawn(async move {
            run_kill_escalation(process, &thread_id, &terminal_id, grace).await;
            if let Some(inner) = weak.upgrade() {
                inner.kill_tasks.lock().unwrap().remove(&key);
            }
        });
        tasks.insert(key, task.abort_handle());
    }

    /// `stopProcess`: detaches the process and starts its kill escalation.
    async fn stop_process(self: &Arc<Self>, session: &Arc<Session>) {
        let (process, notify, thread_id, terminal_id) = {
            let mut state = session.state.lock().unwrap();
            let Some(process) = state.process.take() else {
                return;
            };
            state.cleanup_process_handles();
            state.pid = None;
            state.has_running_subprocess = false;
            state.child_command_label = None;
            state.status = TerminalSessionStatus::Exited;
            state.clear_pending();
            state.updated_at = now_iso();
            (process, state.drain_notify.clone(), state.thread_id.clone(), state.terminal_id.clone())
        };
        // The drain task sees the process is gone and ends.
        notify.notify_one();
        self.clear_kill_task(&process);
        self.unregister(&thread_id, &terminal_id).await;
        self.start_kill_escalation(process, thread_id, terminal_id);
        self.evict_inactive_sessions_if_needed();
    }

    /// `trySpawn`: each candidate in turn while the failure says the shell is missing.
    async fn try_spawn(
        &self,
        candidates: &[ShellCandidate],
        env: &BTreeMap<String, String>,
        start: &StartInput,
    ) -> Result<(Arc<dyn PtyProcess>, String), PtySpawnError> {
        let mut last_error: Option<PtySpawnError> = None;
        for candidate in candidates {
            let attempt = self
                .pty
                .spawn(PtySpawnInput {
                    shell: candidate.shell.clone(),
                    args: candidate.args.clone(),
                    cwd: PathBuf::from(&start.cwd),
                    cols: start.cols,
                    rows: start.rows,
                    env: env.clone(),
                })
                .await;
            match attempt {
                Ok(process) => return Ok((process, candidate.format())),
                Err(error) if is_retryable_shell_spawn_error(&error) => last_error = Some(error),
                Err(error) => return Err(error),
            }
        }
        let mut causes = Vec::new();
        if let Some(error) = last_error {
            causes = error.messages();
        }
        Err(PtySpawnError {
            adapter: "terminal-manager".into(),
            shell: None,
            attempted_shells: Some(candidates.iter().map(ShellCandidate::format).collect()),
            causes,
        })
    }

    /// `startSession`: spawns (with shell fallbacks), publishes `started` / `restarted`, then
    /// starts draining the process's events. A spawn failure leaves the session in `error`
    /// and publishes an `error` event; it is not an error of the call.
    async fn start_session(self: &Arc<Self>, session: &Arc<Session>, start: StartInput, kind: StartKind) {
        self.stop_process(session).await;
        let runtime_env = {
            let mut state = session.state.lock().unwrap();
            state.status = TerminalSessionStatus::Starting;
            state.cwd = start.cwd.clone();
            state.worktree_path = start.worktree_path.clone();
            state.cols = start.cols;
            state.rows = start.rows;
            state.exit_code = None;
            state.exit_signal = None;
            state.has_running_subprocess = false;
            state.child_command_label = None;
            state.pending_events.clear();
            state.updated_at = now_iso();
            state.runtime_env.clone()
        };

        let candidates = resolve_shell_candidates(&(self.shell_resolver)(), self.platform, &self.base_env);
        let env = create_terminal_spawn_env(&self.base_env, runtime_env.as_ref());
        match self.try_spawn(&candidates, &env, &start).await {
            Ok((process, _shell)) => {
                let pid = process.pid();
                let notify = Arc::new(Notify::new());
                {
                    let mut state = session.state.lock().unwrap();
                    state.process = Some(process.clone());
                    state.pid = Some(pid);
                    state.status = TerminalSessionStatus::Running;
                    state.drain_notify = notify.clone();
                }
                // Subscribed after the session accepts events, so an exit replayed during
                // subscription is queued; and outside the state lock (callbacks take it).
                let weak = Arc::downgrade(session);
                let unsubscribe_data = {
                    let (weak, notify) = (weak.clone(), notify.clone());
                    process.on_data(Arc::new(move |data| {
                        enqueue_process_event(&weak, pid, &notify, PendingProcessEvent::Output(data));
                    }))
                };
                let unsubscribe_exit = {
                    let (weak, notify) = (weak.clone(), notify.clone());
                    process.on_exit(Arc::new(move |event| {
                        enqueue_process_event(&weak, pid, &notify, PendingProcessEvent::Exit(event));
                    }))
                };
                let (snapshot, sequence, thread_id, terminal_id) = {
                    let mut state = session.state.lock().unwrap();
                    if same_process(&state.process, &process) {
                        state.unsubscribe_data = Some(unsubscribe_data);
                        state.unsubscribe_exit = Some(unsubscribe_exit);
                    } else {
                        unsubscribe_data.call();
                        unsubscribe_exit.call();
                    }
                    let sequence = state.advance_event_sequence();
                    (state.snapshot(), sequence, state.thread_id.clone(), state.terminal_id.clone())
                };
                let snapshot = Box::new(snapshot);
                self.publish(TerminalEvent {
                    thread_id,
                    terminal_id,
                    sequence: Some(sequence),
                    kind: match kind {
                        StartKind::Started => TerminalEventKind::Started { snapshot },
                        StartKind::Restarted => TerminalEventKind::Restarted { snapshot },
                    },
                });
                // Startup is published before any event replayed during subscription.
                tokio::spawn(drain_process_events(self.clone(), session.clone(), process, notify));
            }
            Err(error) => {
                let (sequence, thread_id, terminal_id) = {
                    let mut state = session.state.lock().unwrap();
                    state.cleanup_process_handles();
                    state.status = TerminalSessionStatus::Error;
                    state.pid = None;
                    state.process = None;
                    state.has_running_subprocess = false;
                    state.child_command_label = None;
                    state.pending_events.clear();
                    let sequence = state.advance_event_sequence();
                    (sequence, state.thread_id.clone(), state.terminal_id.clone())
                };
                self.unregister(&thread_id, &terminal_id).await;
                self.evict_inactive_sessions_if_needed();
                let message = error.message();
                tracing::error!(thread_id, terminal_id, causes = ?error.causes, "failed to start terminal: {message}");
                self.publish(TerminalEvent {
                    thread_id,
                    terminal_id,
                    sequence: Some(sequence),
                    kind: TerminalEventKind::Error { message },
                });
            }
        }
    }

    /// `closeSession`.
    async fn close_session(self: &Arc<Self>, thread_id: &str, terminal_id: &str, delete_history: bool) {
        let session = self.get_session(thread_id, terminal_id);
        let closed_sequence = session.as_ref().map_or(0, |session| session.state.lock().unwrap().event_sequence + 1);
        if let Some(session) = &session {
            self.stop_process(session).await;
            self.unregister(thread_id, terminal_id).await;
            self.persist_history(thread_id, terminal_id, session).await;
        }
        self.flush_persist(thread_id, terminal_id).await;
        let removed = self.sessions.lock().unwrap().remove(&(thread_id.to_owned(), terminal_id.to_owned())).is_some();
        if removed {
            self.publish(TerminalEvent {
                thread_id: thread_id.to_owned(),
                terminal_id: terminal_id.to_owned(),
                sequence: Some(closed_sequence),
                kind: TerminalEventKind::Closed,
            });
        }
        if delete_history {
            self.history.delete_history(thread_id, terminal_id).await;
        }
    }

    // --- open / attach / restart ------------------------------------------------------------

    /// `resolveLaunchInputEnvironment`.
    async fn resolve_launch_input_environment<I: LaunchInput>(&self, mut input: I) -> Result<I, TerminalError> {
        let Some(provider_instance_id) = input.provider_instance_id().map(str::to_owned) else {
            return Ok(input);
        };
        let Some(resolver) = &self.provider_env else {
            return Err(TerminalError::TerminalProviderInstanceNotFoundError { provider_instance_id });
        };
        let env = resolver.resolve(&provider_instance_id, input.env()).await?;
        input.set_env(env);
        Ok(input)
    }

    /// `assertValidCwd`.
    async fn assert_valid_cwd(&self, cwd: &str) -> Result<(), TerminalError> {
        match tokio::fs::metadata(cwd).await {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => Err(TerminalError::TerminalCwdNotDirectoryError { cwd: cwd.to_owned() }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(TerminalError::TerminalCwdNotFoundError { cwd: cwd.to_owned() }),
            Err(error) => Err(TerminalError::TerminalCwdStatError {
                cwd: cwd.to_owned(),
                cause: Defect::from(&error),
            }),
        }
    }

    async fn open_locked(self: &Arc<Self>, input: TerminalOpenInput) -> Result<TerminalSessionSnapshot, TerminalError> {
        let lease = lease_path(input.worktree_path.as_ref(), &input.cwd);
        with_workspace_lease(&lease, self.open_with_workspace_lease(input)).await
    }

    /// `openWithWorkspaceLease`.
    async fn open_with_workspace_lease(self: &Arc<Self>, input: TerminalOpenInput) -> Result<TerminalSessionSnapshot, TerminalError> {
        let thread_id = input.thread_id.clone();
        let terminal_id = input.terminal_id.clone();
        self.assert_valid_cwd(&input.cwd).await?;

        let Some(session) = self.get_session(&thread_id, &terminal_id) else {
            self.flush_persist(&thread_id, &terminal_id).await;
            let history = self.history.read_history(&thread_id, &terminal_id).await?;
            let cols = input.cols.unwrap_or(DEFAULT_OPEN_COLS);
            let rows = input.rows.unwrap_or(DEFAULT_OPEN_ROWS);
            let worktree_path = input.worktree_path.clone().flatten();
            let start = StartInput {
                cwd: input.cwd.clone(),
                worktree_path,
                cols,
                rows,
            };
            let session = self.insert_session(self.new_session_state(&thread_id, &terminal_id, &start, history, normalized_runtime_env(input.env.as_ref())));
            self.evict_inactive_sessions_if_needed();
            self.start_session(&session, start, StartKind::Started).await;
            let snapshot = session.state.lock().unwrap().snapshot();
            return Ok(snapshot);
        };

        let next_runtime_env = normalized_runtime_env(input.env.as_ref());
        let (launch_context_changed, inactive, next_worktree_path, target_cols, target_rows) = {
            let state = session.state.lock().unwrap();
            let next_worktree_path = match &input.worktree_path {
                Some(path) => path.clone(),
                None => state.worktree_path.clone(),
            };
            let changed = state.cwd != input.cwd || state.runtime_env != next_runtime_env || state.worktree_path != next_worktree_path;
            (
                changed,
                matches!(state.status, TerminalSessionStatus::Exited | TerminalSessionStatus::Error),
                next_worktree_path,
                input.cols.unwrap_or(state.cols),
                input.rows.unwrap_or(state.rows),
            )
        };

        if launch_context_changed {
            self.stop_process(&session).await;
            {
                let mut state = session.state.lock().unwrap();
                state.cwd = input.cwd.clone();
                state.worktree_path = next_worktree_path;
                state.runtime_env = next_runtime_env;
                state.history.clear();
                state.clear_pending();
            }
            self.persist_history(&thread_id, &terminal_id, &session).await;
        } else if inactive {
            {
                let mut state = session.state.lock().unwrap();
                state.runtime_env = next_runtime_env;
                state.worktree_path = next_worktree_path;
                state.history.clear();
                state.clear_pending();
            }
            self.persist_history(&thread_id, &terminal_id, &session).await;
        }

        let (process, worktree_path, size_changed) = {
            let state = session.state.lock().unwrap();
            (
                state.process.clone(),
                state.worktree_path.clone(),
                state.cols != target_cols || state.rows != target_rows,
            )
        };
        let Some(process) = process else {
            self.start_session(
                &session,
                StartInput {
                    cwd: input.cwd.clone(),
                    worktree_path,
                    cols: target_cols,
                    rows: target_rows,
                },
                StartKind::Started,
            )
            .await;
            let snapshot = session.state.lock().unwrap().snapshot();
            return Ok(snapshot);
        };
        if size_changed {
            resize_process(&session, &process, target_cols, target_rows)?;
        }
        let snapshot = session.state.lock().unwrap().snapshot();
        Ok(snapshot)
    }

    /// `openOrAttachForStream`.
    async fn open_or_attach_for_stream(self: &Arc<Self>, input: TerminalAttachInput) -> Result<TerminalSessionSnapshot, TerminalError> {
        let _lock = self.thread_lock(&input.thread_id).await;
        let as_open = |cwd: String| TerminalOpenInput {
            thread_id: input.thread_id.clone(),
            terminal_id: input.terminal_id.clone(),
            cwd,
            worktree_path: input.worktree_path.clone(),
            cols: input.cols,
            rows: input.rows,
            env: input.env.clone(),
            provider_instance_id: input.provider_instance_id.clone(),
        };
        let Some(session) = self.get_session(&input.thread_id, &input.terminal_id) else {
            let Some(cwd) = input.cwd.clone() else {
                return Err(TerminalError::TerminalSessionLookupError {
                    thread_id: input.thread_id.clone(),
                    terminal_id: input.terminal_id.clone(),
                });
            };
            let resolved = self.resolve_launch_input_environment(as_open(cwd)).await?;
            return self.open_locked(resolved).await;
        };

        let (process, running, target_cols, target_rows, size_changed) = {
            let state = session.state.lock().unwrap();
            let target_cols = input.cols.unwrap_or(state.cols);
            let target_rows = input.rows.unwrap_or(state.rows);
            (
                state.process.clone(),
                state.status == TerminalSessionStatus::Running,
                target_cols,
                target_rows,
                state.cols != target_cols || state.rows != target_rows,
            )
        };
        if process.is_none() && input.restart_if_not_running == Some(true) {
            if let Some(cwd) = input.cwd.clone() {
                let resolved = self.resolve_launch_input_environment(as_open(cwd)).await?;
                return self.open_locked(resolved).await;
            }
        }
        if let Some(process) = process.filter(|_| running && size_changed) {
            resize_process(&session, &process, target_cols, target_rows)?;
        }
        let snapshot = session.state.lock().unwrap().snapshot();
        Ok(snapshot)
    }

    /// `restartResolved`.
    async fn restart_resolved(self: &Arc<Self>, input: TerminalRestartInput) -> Result<TerminalSessionSnapshot, TerminalError> {
        let thread_id = input.thread_id.clone();
        let terminal_id = input.terminal_id.clone();
        self.assert_valid_cwd(&input.cwd).await?;
        let worktree_path = input.worktree_path.clone().flatten();
        let session = match self.get_session(&thread_id, &terminal_id) {
            None => {
                let start = StartInput {
                    cwd: input.cwd.clone(),
                    worktree_path: worktree_path.clone(),
                    cols: input.cols,
                    rows: input.rows,
                };
                let session = self.insert_session(self.new_session_state(
                    &thread_id,
                    &terminal_id,
                    &start,
                    BoundedTerminalHistory::new(self.history.line_limit, "", self.history.byte_limit),
                    normalized_runtime_env(input.env.as_ref()),
                ));
                self.evict_inactive_sessions_if_needed();
                session
            }
            Some(session) => {
                self.stop_process(&session).await;
                let mut state = session.state.lock().unwrap();
                state.cwd = input.cwd.clone();
                state.worktree_path = worktree_path.clone();
                state.runtime_env = normalized_runtime_env(input.env.as_ref());
                drop(state);
                session
            }
        };
        {
            let mut state = session.state.lock().unwrap();
            state.history.clear();
            state.clear_pending();
        }
        self.persist_history(&thread_id, &terminal_id, &session).await;
        self.start_session(
            &session,
            StartInput {
                cwd: input.cwd.clone(),
                worktree_path,
                cols: input.cols,
                rows: input.rows,
            },
            StartKind::Restarted,
        )
        .await;
        let snapshot = session.state.lock().unwrap().snapshot();
        Ok(snapshot)
    }

    // --- subprocess activity -------------------------------------------------------------------

    /// One process table (or the injected inspector), and whether it came from the primary
    /// source.
    async fn acquire_inspector(&self) -> Result<(Inspector, bool), SubprocessCheckError> {
        if let Some(inspector) = &self.inspector_override {
            return Ok((Inspector::Custom(inspector.clone()), true));
        }
        if let Some(source) = &self.process_table {
            match source().await {
                Ok(entries) => return Ok((Inspector::Table(Arc::new(snapshot_from_entries(&entries))), true)),
                Err(error) => {
                    tracing::debug!(%error, "resource monitor process table unavailable, using the fallback");
                    let snapshot = fallback_process_table(&*self.process_runner, self.platform, &self.ps_command).await?;
                    return Ok((Inspector::Table(Arc::new(snapshot)), false));
                }
            }
        }
        let snapshot = fallback_process_table(&*self.process_runner, self.platform, &self.ps_command).await?;
        Ok((Inspector::Table(Arc::new(snapshot)), true))
    }

    async fn inspect(&self, inspector: &Inspector, pid: u32) -> Result<SubprocessInspectResult, SubprocessCheckError> {
        match inspector {
            Inspector::Custom(inspector) => inspector(pid).await,
            Inspector::Table(snapshot) => Ok(derive_subprocess_inspect_result(snapshot, pid, self.platform)),
        }
    }

    fn running_sessions(&self, thread_id: Option<&str>) -> Vec<(Arc<Session>, String, String, u32)> {
        let sessions: Vec<Arc<Session>> = self.sessions.lock().unwrap().values().cloned().collect();
        sessions
            .into_iter()
            .filter_map(|session| {
                let state = session.state.lock().unwrap();
                let pid = state.pid?;
                if state.status != TerminalSessionStatus::Running || thread_id.is_some_and(|thread_id| state.thread_id != thread_id) {
                    return None;
                }
                let (thread_id, terminal_id) = (state.thread_id.clone(), state.terminal_id.clone());
                drop(state);
                Some((session, thread_id, terminal_id, pid))
            })
            .collect()
    }

    /// `pollSubprocessActivity`: true when the snapshot came from the primary source.
    async fn poll_subprocess_activity(&self) -> bool {
        let running = self.running_sessions(None);
        if running.is_empty() {
            return true;
        }
        let (inspector, succeeded) = match self.acquire_inspector().await {
            Ok(inspector) => inspector,
            Err(error) => {
                tracing::warn!(%error, "failed to snapshot processes for terminal subprocess polling");
                return false;
            }
        };
        let checks = running.into_iter().map(|(_, thread_id, terminal_id, pid)| {
            let inspector = &inspector;
            async move {
                let next = match self.inspect(inspector, pid).await {
                    Ok(next) => next,
                    Err(error) => {
                        tracing::warn!(thread_id, terminal_id, pid, %error, "failed to check terminal subprocess activity");
                        return;
                    }
                };
                if let Some(registry) = &self.registry {
                    registry.register_terminal_processes(&thread_id, &terminal_id, &next.process_ids).await;
                }
                let next_label = if next.has_running_subprocess { next.child_command.clone() } else { None };
                let event = self.get_session(&thread_id, &terminal_id).and_then(|session| {
                    let mut state = session.state.lock().unwrap();
                    if state.status != TerminalSessionStatus::Running
                        || state.pid != Some(pid)
                        || (state.has_running_subprocess == next.has_running_subprocess && state.child_command_label == next_label)
                    {
                        return None;
                    }
                    state.has_running_subprocess = next.has_running_subprocess;
                    state.child_command_label = next_label;
                    let sequence = state.advance_event_sequence();
                    Some(TerminalEvent {
                        thread_id: state.thread_id.clone(),
                        terminal_id: state.terminal_id.clone(),
                        sequence: Some(sequence),
                        kind: TerminalEventKind::Activity {
                            has_running_subprocess: next.has_running_subprocess,
                            label: state.wire_label(),
                        },
                    })
                });
                if let Some(event) = event {
                    self.publish(event);
                }
            }
        });
        futures::future::join_all(checks).await;
        succeeded
    }

    /// `closeIdle`, under the thread lock. Fails (closing nothing more) when a process check
    /// fails.
    async fn close_idle_locked(self: &Arc<Self>, thread_id: &str, terminal_id: Option<&str>) -> Result<(), SubprocessCheckError> {
        let running: Vec<_> = self
            .running_sessions(Some(thread_id))
            .into_iter()
            .filter(|(_, _, id, _)| terminal_id.is_none_or(|terminal_id| id == terminal_id))
            .collect();
        if running.is_empty() {
            return Ok(());
        }
        // A command typed during the check can miss the table, but its input or echo lands;
        // both counters only grow, so their sum changes when either does.
        let activity_mark = |session: &Session| {
            let state = session.state.lock().unwrap();
            state.event_sequence + state.input_count
        };
        let marks: HashMap<String, u64> = running.iter().map(|(session, _, id, _)| (id.clone(), activity_mark(session))).collect();
        let (inspector, _) = self.acquire_inspector().await?;
        for (session, _, id, pid) in &running {
            let result = self.inspect(&inspector, *pid).await?;
            if result.has_running_subprocess || activity_mark(session) != marks[id] {
                continue;
            }
            self.close_session(thread_id, id, false).await;
        }
        Ok(())
    }
}

fn persist_key(thread_id: &str, terminal_id: &str) -> String {
    format!("{thread_id}\u{0}{terminal_id}")
}

/// `enqueueProcessEvent`: queues the event if the session still runs this process.
fn enqueue_process_event(session: &Weak<Session>, pid: u32, notify: &Notify, event: PendingProcessEvent) {
    let Some(session) = session.upgrade() else { return };
    {
        let mut state = session.state.lock().unwrap();
        if state.process.is_none() || state.status != TerminalSessionStatus::Running || state.pid != Some(pid) {
            return;
        }
        state.pending_events.push_back(event);
    }
    notify.notify_one();
}

enum DrainAction {
    Wait,
    Stop,
    Output {
        thread_id: String,
        terminal_id: String,
        sequence: u64,
        history_changed: bool,
        data: String,
    },
    Exit {
        process: Arc<dyn PtyProcess>,
        thread_id: String,
        terminal_id: String,
        sequence: u64,
        exit_code: Option<i32>,
        exit_signal: Option<i32>,
    },
}

/// `drainProcessEvents`: the session's events of one process, in order, until it exits or is
/// replaced.
async fn drain_process_events(inner: Arc<Inner>, session: Arc<Session>, process: Arc<dyn PtyProcess>, notify: Arc<Notify>) {
    let pid = process.pid();
    loop {
        let action = {
            let mut state = session.state.lock().unwrap();
            if state.pid != Some(pid) || !same_process(&state.process, &process) || state.status != TerminalSessionStatus::Running {
                DrainAction::Stop
            } else {
                match state.pending_events.pop_front() {
                    None => DrainAction::Wait,
                    Some(PendingProcessEvent::Output(data)) => {
                        let sanitized = sanitize_terminal_history_chunk(&state.pending_history_control_sequence, &data);
                        state.pending_history_control_sequence = sanitized.pending_control_sequence;
                        let history_changed = !sanitized.visible_text.is_empty();
                        if history_changed {
                            state.history.append(&sanitized.visible_text);
                        }
                        let sequence = state.advance_event_sequence();
                        DrainAction::Output {
                            thread_id: state.thread_id.clone(),
                            terminal_id: state.terminal_id.clone(),
                            sequence,
                            history_changed,
                            data,
                        }
                    }
                    Some(PendingProcessEvent::Exit(event)) => {
                        let process = state.process.take().unwrap_or_else(|| process.clone());
                        state.cleanup_process_handles();
                        state.pid = None;
                        state.has_running_subprocess = false;
                        state.child_command_label = None;
                        state.status = TerminalSessionStatus::Exited;
                        state.clear_pending();
                        state.exit_code = event.exit_code;
                        state.exit_signal = event.signal;
                        let sequence = state.advance_event_sequence();
                        DrainAction::Exit {
                            process,
                            thread_id: state.thread_id.clone(),
                            terminal_id: state.terminal_id.clone(),
                            sequence,
                            exit_code: state.exit_code,
                            exit_signal: state.exit_signal,
                        }
                    }
                }
            }
        };
        match action {
            DrainAction::Stop => return,
            DrainAction::Wait => {
                tokio::select! {
                    _ = notify.notified() => {}
                    _ = inner.shutdown.cancelled() => return,
                }
            }
            DrainAction::Output {
                thread_id,
                terminal_id,
                sequence,
                history_changed,
                data,
            } => {
                if history_changed {
                    inner.queue_persist(&thread_id, &terminal_id, &session);
                }
                inner.publish(TerminalEvent {
                    thread_id,
                    terminal_id,
                    sequence: Some(sequence),
                    kind: TerminalEventKind::Output { data },
                });
            }
            DrainAction::Exit {
                process,
                thread_id,
                terminal_id,
                sequence,
                exit_code,
                exit_signal,
            } => {
                inner.clear_kill_task(&process);
                inner.unregister(&thread_id, &terminal_id).await;
                inner.publish(TerminalEvent {
                    thread_id,
                    terminal_id,
                    sequence: Some(sequence),
                    kind: TerminalEventKind::Exited { exit_code, exit_signal },
                });
                inner.evict_inactive_sessions_if_needed();
                return;
            }
        }
    }
}
