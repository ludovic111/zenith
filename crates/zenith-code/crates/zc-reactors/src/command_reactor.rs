//! `orchestration/Layers/ProviderCommandReactor.ts`: orchestration events → provider calls.
//!
//! - `thread.turn-start-requested`: auth prompt commands (`/login`, …), worktree recreation,
//!   first-turn title and branch-name generation, `/compact` (queueing turns sent meanwhile and
//!   replaying them afterwards), session start or restart (model, instance, runtime mode, cwd
//!   changes), then `sendTurn`.
//! - `thread.turn-interrupt-requested`, `thread.approval-response-requested`,
//!   `thread.user-input-response-requested`, `thread.session-stop-requested`: the matching
//!   provider call, with failures surfaced as `provider.*.failed` activities.
//! - `thread.runtime-mode-set`: restart a live session in the new mode.
//! - `thread.meta-updated` / `thread.session-set`: title regeneration and refinement.
//! - `thread.settled`: close idle terminals and stop the session.
//!
//! Events go through one worker in order; sends, compactions and generations run as tasks of
//! the reactor (stopped with it). Title regeneration has its own worker.

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use zc_contracts::{MessageId, OrchestrationEvent, ProjectId, ThreadId};
use zc_ports::contracts::{
    ChatAttachment, ModelSelection, ProviderInstanceId, ProviderInterruptTurnInput, ProviderRespondToRequestInput, ProviderRespondToUserInputInput,
    ProviderSendTurnInput, ProviderSessionStartInput, ProviderStopSessionInput, VcsCreateWorktreeInput, WorktreeSubmodules,
};
use zc_ports::git::CreateWorktreeOptions;
use zc_ports::provider::SessionModelSwitchMode;
use zc_ports::text_generation::{BranchNameGenerationInput, ThreadTitleGenerationInput};
use zc_ports::{
    GitWorkflow, OrchestrationDispatch, ProviderAuthCommands, ProviderService, ProviderStatusReads, SettingsService, TaggedError, TerminalManager,
    TextGeneration, VcsStatusRefresher,
};

use crate::common::{dispatch_json, pretty, UuidSource};
use crate::composer::{assistant_citations_to_plain_text, project_composer_context_for_provider};
use crate::js::{str_of, trim, Obj};
use crate::reads::ReactorReads;
use crate::runtime::{DrainableWorker, ReactorClock, SharedTtlMap};
use crate::settings::{has_any_project_overrides, resolve_project_settings, settings_json};
use crate::settlement::PathExists;
use crate::titles::{
    build_generated_worktree_branch_name, can_replace_thread_title, format_thread_title_context, is_temporary_worktree_branch, DEFAULT_THREAD_TITLE,
};
use crate::workspace_lease::{resolve_path, with_workspace_lease};

const HANDLED_TURN_START_KEY_MAX: usize = 10_000;
const HANDLED_TURN_START_KEY_TTL: Duration = Duration::from_secs(30 * 60);
const DEFAULT_RUNTIME_MODE: &str = "full-access";

/// `ProviderRegistry.refreshWorkspaceSnapshot({instanceId, cwd})`, forked and forgotten.
#[async_trait]
pub trait WorkspaceSnapshotRefresher: Send + Sync {
    async fn refresh_workspace_snapshot(&self, instance_id: &str, cwd: &str);
}

#[async_trait]
impl WorkspaceSnapshotRefresher for zc_providers::ProviderRegistry {
    async fn refresh_workspace_snapshot(&self, instance_id: &str, cwd: &str) {
        let _ = zc_providers::ProviderRegistry::refresh_workspace_snapshot(self, &zc_contracts::ProviderInstanceId::new(instance_id), cwd).await;
    }
}

/// What the command reactor is built from.
#[derive(Clone)]
pub struct CommandReactorDeps {
    pub engine: Arc<dyn OrchestrationDispatch>,
    pub reads: Arc<dyn ReactorReads>,
    pub providers: Arc<dyn ProviderService>,
    pub provider_status: Arc<dyn ProviderStatusReads>,
    pub provider_auth: Arc<dyn ProviderAuthCommands>,
    pub workspace_snapshots: Option<Arc<dyn WorkspaceSnapshotRefresher>>,
    pub git: Arc<dyn GitWorkflow>,
    pub vcs_status: Arc<dyn VcsStatusRefresher>,
    pub text_generation: Arc<dyn TextGeneration>,
    pub settings: Arc<dyn SettingsService>,
    pub terminals: Arc<dyn TerminalManager>,
    pub clock: Arc<dyn ReactorClock>,
    pub uuids: UuidSource,
    pub path_exists: PathExists,
    /// The first retry delay of first-turn title generation (`Schedule.exponential("2 seconds")`).
    pub title_retry_base: Duration,
}

/// `providerErrorLabel(value)`.
fn provider_error_label(value: Option<&str>) -> String {
    match value.map(trim) {
        Some(value) if !value.is_empty() => value.to_owned(),
        _ => "unknown".into(),
    }
}

/// `providerErrorLabelFromInstanceHint({instanceId, modelSelectionInstanceId, sessionProvider})`.
pub fn provider_error_label_from_instance_hint(instance_id: Option<&str>, model_selection_instance_id: Option<&str>, session_provider: Option<&str>) -> String {
    provider_error_label(instance_id.or(model_selection_instance_id).or(session_provider))
}

fn request_error(provider: &str, method: &str, detail: String) -> TaggedError {
    TaggedError::new(
        "ProviderAdapterRequestError",
        format!("Provider adapter request failed ({provider}) for {method}: {detail}"),
    )
    .with("provider", provider)
    .with("method", method)
    .with("detail", detail)
}

fn defect(message: impl Into<String>) -> TaggedError {
    TaggedError::new("Defect", message)
}

/// `formatFailureDetail(cause)`.
fn format_failure_detail(error: &TaggedError) -> String {
    let field = |key: &str| error.fields.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    match error.tag.as_str() {
        "ProviderAdapterRequestError" | "ProviderAdapterProcessError" => field("detail"),
        "ProviderAdapterValidationError" => field("issue"),
        "ProviderWorkspaceMissingError" => error.message.clone(),
        _ => pretty(error),
    }
}

fn mentions_any(error: &TaggedError, needles: &[&str]) -> bool {
    let text = if error.tag == "ProviderAdapterRequestError" {
        error.fields.get("detail").and_then(Value::as_str).unwrap_or("").to_lowercase()
    } else {
        pretty(error).to_lowercase()
    };
    needles.iter().any(|needle| text.contains(needle))
}

fn is_unknown_pending_approval_request_error(error: &TaggedError) -> bool {
    mentions_any(
        error,
        &[
            "unknown pending approval request",
            "unknown pending permission request",
            "unknown pending codex approval request",
        ],
    )
}

fn is_unknown_pending_user_input_request_error(error: &TaggedError) -> bool {
    mentions_any(
        error,
        &[
            "unknown pending user-input request",
            "unknown pending user input request",
            "unknown pending codex user input request",
        ],
    )
}

fn stale_pending_request_detail(kind: &str, request_id: &str) -> String {
    format!("Stale pending {kind} request: {request_id}. Provider callback state does not survive app restarts or recovered sessions. Restart the turn to continue.")
}

/// `mapProviderSessionStatusToOrchestrationStatus`.
fn map_provider_session_status(status: Option<&str>) -> &'static str {
    match status {
        Some("connecting") => "starting",
        Some("running") => "running",
        Some("error") => "error",
        Some("closed") => "stopped",
        _ => "ready",
    }
}

/// `isProviderDriverKind`: a trimmed slug, letter first, at most 64 characters.
fn is_provider_driver_kind(value: &str) -> bool {
    let mut chars = value.chars();
    value.len() <= 64 && chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `isCompactCommandMessage(message)`.
fn is_compact_command_message(message: &Value) -> bool {
    str_of(message, "role") == Some("user")
        && message.get("attachments").and_then(Value::as_array).is_none_or(Vec::is_empty)
        && trim(str_of(message, "text").unwrap_or("")).to_lowercase() == "/compact"
}

/// `resolveThreadWorkspaceCwd({thread, projects})`.
fn resolve_thread_workspace_cwd(thread: &Value, project: Option<&Value>) -> Option<String> {
    if let Some(worktree) = str_of(thread, "worktreePath").filter(|path| !path.is_empty()) {
        return Some(worktree.to_owned());
    }
    project
        .filter(|project| project.get("id") == thread.get("projectId"))
        .and_then(|project| str_of(project, "workspaceRoot"))
        .map(str::to_owned)
}

fn process_cwd() -> String {
    std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/".into())
}

fn non_null<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|field| !field.is_null())
}

/// A one-shot signal (`Deferred<void>`).
#[derive(Clone)]
struct Signal(Arc<watch::Sender<bool>>);

impl Signal {
    fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    fn succeed(&self) {
        self.0.send_replace(true);
    }

    async fn wait(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|done| *done).await;
    }
}

type Queue = Arc<Mutex<VecDeque<Value>>>;

struct Resumed {
    event: Value,
    queued: Queue,
    sent: Signal,
}

#[derive(Default)]
struct ReactorState {
    thread_model_selections: HashMap<String, Value>,
    compacting: HashSet<String>,
    /// Turn starts received while a thread compacts, replayed in order afterwards.
    turns_after_compaction: HashMap<String, Queue>,
    /// Replay command id → the queued turn start it re-requests.
    resumed_turn_starts: HashMap<String, Resumed>,
    stopping: HashSet<String>,
}

#[derive(Default)]
struct EnsureOptions {
    model_selection: Option<Value>,
    pending_turn_start: bool,
    title_seed: Option<String>,
}

/// The outcome of `regenerateThreadTitle`.
enum Regeneration {
    Superseded,
    Completed(Option<String>),
}

struct Core {
    deps: CommandReactorDeps,
    state: Mutex<ReactorState>,
    handled_turn_start_keys: SharedTtlMap<String, bool>,
    tasks: TaskTracker,
    stop: CancellationToken,
}

impl Core {
    fn state<R>(&self, f: impl FnOnce(&mut ReactorState) -> R) -> R {
        f(&mut self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }

    fn server_command_id(&self, tag: &str) -> String {
        format!("server:{tag}:{}", (self.deps.uuids)())
    }

    async fn dispatch(&self, command: Value) -> Result<(), TaggedError> {
        dispatch_json(&*self.deps.engine, command).await.map(|_| ())
    }

    /// `Effect.forkScoped`: a task stopped with the reactor.
    fn fork(&self, work: impl Future<Output = ()> + Send + 'static) {
        let stop = self.stop.clone();
        self.tasks.spawn(async move {
            tokio::select! {
                _ = stop.cancelled() => {}
                _ = work => {}
            }
        });
    }

    async fn settings_value(&self) -> Result<Value, TaggedError> {
        self.deps
            .settings
            .get_settings()
            .await
            .map(|settings| settings_json(&settings))
            .map_err(|error| TaggedError::new("ServerSettingsError", format!("{error:?}")))
    }

    /// `projectSettingsForThread(threadId)`.
    async fn project_settings_for_thread(&self, thread_id: &str) -> Result<Value, TaggedError> {
        let settings = self.settings_value().await?;
        if !has_any_project_overrides(&settings) {
            return Ok(settings);
        }
        let thread = self.deps.reads.thread_shell(&ThreadId::new(thread_id)).await.unwrap_or(None);
        Ok(resolve_project_settings(
            &settings,
            thread.as_ref().and_then(|thread| str_of(thread, "projectId")),
        ))
    }

    async fn thread_shell(&self, thread_id: &str) -> Result<Option<Value>, TaggedError> {
        self.deps.reads.thread_shell(&ThreadId::new(thread_id)).await
    }

    async fn project(&self, project_id: Option<&str>) -> Result<Option<Value>, TaggedError> {
        match project_id {
            Some(project_id) => self.deps.reads.project_shell(&ProjectId::new(project_id)).await,
            None => Ok(None),
        }
    }

    async fn active_session(&self, thread_id: &str) -> Option<Value> {
        self.deps
            .providers
            .list_sessions()
            .await
            .into_iter()
            .map(|session| session.0)
            .find(|session| str_of(session, "threadId") == Some(thread_id))
    }

    /// `appendProviderFailureActivity`.
    #[allow(clippy::too_many_arguments)]
    async fn append_failure(
        &self,
        thread_id: &str,
        kind: &str,
        summary: &str,
        detail: &str,
        turn_id: Option<&str>,
        created_at: &str,
        request_id: Option<&str>,
    ) -> Result<(), TaggedError> {
        let payload = Obj::new()
            .set("detail", detail)
            .set_if(request_id.is_some_and(|id| !id.is_empty()), "requestId", || json!(request_id))
            .build();
        self.dispatch(json!({
            "type": "thread.activity.append",
            "commandId": self.server_command_id("provider-failure-activity"),
            "threadId": thread_id,
            "activity": {
                "id": (self.deps.uuids)(),
                "tone": "error",
                "kind": kind,
                "summary": summary,
                "payload": payload,
                "turnId": turn_id,
                "createdAt": created_at,
            },
            "createdAt": created_at,
        }))
        .await
    }

    /// `setThreadSession`.
    async fn set_thread_session(&self, thread_id: &str, session: Value, created_at: &str) -> Result<(), TaggedError> {
        self.dispatch(json!({
            "type": "thread.session.set",
            "commandId": self.server_command_id("provider-session-set"),
            "threadId": thread_id,
            "session": session,
            "createdAt": created_at,
        }))
        .await
    }

    /// `cancelTurnsAfterCompaction(threadId, detail)`.
    async fn cancel_turns_after_compaction(&self, thread_id: &str, detail: &str) {
        let queued = self.state(|state| state.turns_after_compaction.remove(thread_id));
        let events: Vec<Value> = queued
            .map(|queue| queue.lock().unwrap_or_else(|p| p.into_inner()).iter().cloned().collect())
            .unwrap_or_default();
        for event in events {
            let created_at = self.deps.clock.now_iso();
            let message_id = str_of(&event["payload"], "messageId");
            if let Err(error) = self
                .append_failure(
                    thread_id,
                    "provider.turn.start.failed",
                    "Queued message was not sent",
                    detail,
                    None,
                    &created_at,
                    message_id,
                )
                .await
            {
                tracing::warn!(cause = %pretty(&error), "failed to report canceled queued message");
            }
        }
    }

    fn queue_is_current(&self, thread_id: &str, queued: &Queue) -> bool {
        self.state(|state| state.turns_after_compaction.get(thread_id).is_some_and(|current| Arc::ptr_eq(current, queued)))
    }

    /// `resumeTurnsAfterCompaction(threadId)`.
    async fn resume_turns_after_compaction(&self, thread_id: &str) -> Result<(), TaggedError> {
        let queued = self.state(|state| state.turns_after_compaction.get(thread_id).cloned()).unwrap_or_default();
        loop {
            let first = queued.lock().unwrap_or_else(|p| p.into_inner()).front().cloned();
            let Some(event) = first else { break };
            if !self.queue_is_current(thread_id, &queued) {
                break;
            }
            let message_id = str_of(&event["payload"], "messageId").unwrap_or("").to_owned();
            let turn_start = self
                .deps
                .reads
                .turn_start_message(&ThreadId::new(thread_id), &MessageId::new(&message_id))
                .await?;
            if !self.queue_is_current(thread_id, &queued) {
                return Ok(());
            }
            // In flight from here on: a cancellation reports it when the replay runs.
            queued.lock().unwrap_or_else(|p| p.into_inner()).pop_front();
            let Some(turn_start) = turn_start else { continue };
            let command_id = self.server_command_id("after-compaction");
            let sent = Signal::new();
            self.state(|state| {
                state.resumed_turn_starts.insert(
                    command_id.clone(),
                    Resumed {
                        event: event.clone(),
                        queued: queued.clone(),
                        sent: sent.clone(),
                    },
                )
            });
            let mut command = event["payload"].as_object().cloned().unwrap_or_default();
            command.remove("messageId");
            command.insert("type".into(), json!("thread.turn.start"));
            command.insert("commandId".into(), json!(command_id));
            command.insert(
                "message".into(),
                json!({
                    "messageId": message_id,
                    "role": "user",
                    "text": turn_start.message["text"],
                    "attachments": turn_start.message.get("attachments").filter(|value| !value.is_null()).cloned().unwrap_or(json!([])),
                }),
            );
            if let Err(error) = self.dispatch(Value::Object(command)).await {
                self.state(|state| state.resumed_turn_starts.remove(&command_id));
                queued.lock().unwrap_or_else(|p| p.into_inner()).push_front(event);
                return Err(error);
            }
            sent.wait().await;
            self.state(|state| state.resumed_turn_starts.remove(&command_id));
        }
        self.state(|state| {
            if state.turns_after_compaction.get(thread_id).is_some_and(|current| Arc::ptr_eq(current, &queued)) {
                state.turns_after_compaction.remove(thread_id);
            }
        });
        Ok(())
    }

    /// `setThreadSessionErrorOnTurnStartFailure`.
    async fn set_session_error_on_turn_start_failure(&self, thread_id: &str, detail: &str, created_at: &str) -> Result<(), TaggedError> {
        let Some(thread) = self.thread_shell(thread_id).await? else {
            return Ok(());
        };
        let session = non_null(&thread, "session");
        let mut base = match session {
            Some(session) => session.clone(),
            None => json!({
                "threadId": thread_id,
                "providerName": null,
                "providerInstanceId": thread["modelSelection"]["instanceId"],
                "runtimeMode": thread["runtimeMode"],
            }),
        };
        let status = if session.and_then(|session| str_of(session, "status")) == Some("stopped") {
            "stopped"
        } else {
            "error"
        };
        base["status"] = json!(status);
        base["activeTurnId"] = Value::Null;
        base["lastError"] = json!(detail);
        base["updatedAt"] = json!(created_at);
        self.set_thread_session(thread_id, base, created_at).await
    }

    /// `restoreCompaction(threadId, fromRunning)`.
    async fn restore_compaction(&self, thread_id: &str, from_running: bool) -> Result<(), TaggedError> {
        if self.state(|state| state.stopping.contains(thread_id)) {
            self.state(|state| state.compacting.remove(thread_id));
            return Ok(());
        }
        let Some(thread) = self.thread_shell(thread_id).await? else { return Ok(()) };
        let Some(session) = non_null(&thread, "session").cloned() else {
            return Ok(());
        };
        let status = str_of(&session, "status").unwrap_or("");
        if status != "starting" && status != "ready" && (!from_running || status != "running") {
            return Ok(());
        }
        let completed_at = self.deps.clock.now_iso();
        if self.state(|state| state.stopping.contains(thread_id)) {
            self.state(|state| state.compacting.remove(thread_id));
            return Ok(());
        }
        let mut next = session;
        next["status"] = json!("ready");
        next["activeTurnId"] = Value::Null;
        next["lastError"] = Value::Null;
        next["updatedAt"] = json!(completed_at);
        self.set_thread_session(thread_id, next, &completed_at).await
    }

    /// `ensureThreadWorktree(thread)`: recreate a vanished worktree from its branch (best
    /// effort).
    async fn ensure_thread_worktree(&self, thread: &Value) -> Result<(), TaggedError> {
        let (Some(worktree_path), Some(branch)) = (
            str_of(thread, "worktreePath").filter(|p| !p.is_empty()),
            str_of(thread, "branch").filter(|b| !b.is_empty()),
        ) else {
            return Ok(());
        };
        if (self.deps.path_exists)(worktree_path) {
            return Ok(());
        }
        let Some(project) = self.project(str_of(thread, "projectId")).await? else {
            return Ok(());
        };
        let cwd = str_of(&project, "workspaceRoot").unwrap_or("").to_owned();
        let thread_id = str_of(thread, "id").unwrap_or("");
        tracing::warn!(thread_id, worktree_path, branch, "provider command reactor recreating missing worktree");
        let submodules = match self.project_settings_for_thread(thread_id).await {
            Ok(settings) => serde_json::from_value::<WorktreeSubmodules>(settings["worktreeSubmodules"].clone()).ok(),
            Err(_) => None,
        };
        let result = async {
            self.deps.git.prune_worktrees(&cwd).await?;
            self.deps
                .git
                .create_worktree(
                    VcsCreateWorktreeInput(json!({"cwd": cwd, "refName": branch, "path": worktree_path})),
                    CreateWorktreeOptions {
                        submodules,
                        ..Default::default()
                    },
                )
                .await?;
            Ok::<_, TaggedError>(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(thread_id, worktree_path, cause = %pretty(&error), "provider command reactor failed to recreate worktree");
        }
        Ok(())
    }

    /// `rejectStartedThreadModelChangeIfRequired`.
    async fn reject_started_thread_model_change(&self, thread_id: &str, current: &Value, requested: Option<&Value>) -> Result<(), TaggedError> {
        let Some(requested) = requested else { return Ok(()) };
        if current["instanceId"] == requested["instanceId"] && current["model"] == requested["model"] {
            return Ok(());
        }
        let providers = self.deps.provider_status.get_providers().await;
        let requires = |instance: &Value| {
            providers
                .iter()
                .find(|snapshot| snapshot.0.get("instanceId") == Some(instance))
                .is_some_and(|snapshot| snapshot.0.get("requiresNewThreadForModelChange") == Some(&json!(true)))
        };
        if !requires(&current["instanceId"]) && !requires(&requested["instanceId"]) {
            return Ok(());
        }
        Err(request_error(
            &provider_error_label_from_instance_hint(requested["instanceId"].as_str(), current["instanceId"].as_str(), None),
            "thread.turn.start",
            format!(
                "Thread '{thread_id}' cannot switch models after the conversation has started. Start a new thread to use '{}'.",
                requested["model"].as_str().unwrap_or("")
            ),
        ))
    }

    fn refresh_workspace_snapshot(&self, instance_id: &str, cwd: Option<&str>) {
        let (Some(refresher), Some(cwd)) = (self.deps.workspace_snapshots.clone(), cwd) else {
            return;
        };
        let (instance_id, cwd) = (instance_id.to_owned(), cwd.to_owned());
        tokio::spawn(async move { refresher.refresh_workspace_snapshot(&instance_id, &cwd).await });
    }

    /// `ensureSessionForThread(threadId, createdAt, options)`: the provider session thread id.
    async fn ensure_session_for_thread(&self, thread_id: &str, created_at: &str, options: EnsureOptions) -> Result<String, TaggedError> {
        let Some(thread) = self.thread_shell(thread_id).await? else {
            return Err(defect(format!("Thread '{thread_id}' was not found in read model.")));
        };
        let desired_runtime_mode = thread["runtimeMode"].clone();
        let requested = options.model_selection.clone();
        let active_session = self.active_session(thread_id).await;
        let thread_session = non_null(&thread, "session").cloned();
        let active_thread_session = thread_session
            .as_ref()
            .filter(|session| str_of(session, "status") != Some("stopped") && active_session.is_some())
            .cloned();
        if let (Some(thread_session), Some(active)) = (&active_thread_session, &active_session) {
            if non_null(thread_session, "providerInstanceId").is_none() || non_null(active, "providerInstanceId").is_none() {
                return Err(request_error(
                    &provider_error_label(str_of(thread_session, "providerName")),
                    "thread.turn.start",
                    format!("Thread '{thread_id}' has an active provider session without a provider instance id."),
                ));
            }
        }
        let model_selection = thread["modelSelection"].clone();
        let current_instance_id = match (&active_thread_session, &active_session) {
            (Some(_), Some(active)) if non_null(active, "providerInstanceId").is_some() => active["providerInstanceId"].clone(),
            _ => model_selection["instanceId"].clone(),
        };
        let current_instance = current_instance_id.as_str().unwrap_or("").to_owned();
        let desired_model_selection = requested.clone().unwrap_or_else(|| model_selection.clone());
        let desired_instance = desired_model_selection["instanceId"].as_str().unwrap_or("").to_owned();
        let session_provider = thread_session.as_ref().and_then(|session| str_of(session, "providerName"));
        let current_info = self
            .deps
            .providers
            .get_instance_info(&ProviderInstanceId::new(&current_instance))
            .await
            .map_err(|_| {
                request_error(
                    &provider_error_label_from_instance_hint(Some(&current_instance), model_selection["instanceId"].as_str(), session_provider),
                    "thread.turn.start",
                    format!("Thread '{thread_id}' references unknown provider instance '{current_instance}'. The instance is not configured in this build."),
                )
            })?;
        let desired_info = self
            .deps
            .providers
            .get_instance_info(&ProviderInstanceId::new(&desired_instance))
            .await
            .map_err(|_| {
                request_error(
                    &provider_error_label_from_instance_hint(Some(&desired_instance), None, None),
                    "thread.turn.start",
                    format!("Requested provider instance '{desired_instance}' is not configured in this build."),
                )
            })?;
        let desired_driver = desired_info.driver_kind.to_string();
        if !is_provider_driver_kind(&desired_driver) {
            return Err(request_error(
                &provider_error_label(Some(&desired_driver)),
                "thread.turn.start",
                format!("Requested provider instance '{desired_instance}' uses unknown provider driver '{desired_driver}'. The driver is not installed in this build."),
            ));
        }
        let preferred_provider = desired_driver.clone();
        let session_status = thread_session.as_ref().and_then(|session| str_of(session, "status"));
        if options.pending_turn_start && session_status != Some("running") {
            let provider_name = active_session
                .as_ref()
                .and_then(|session| non_null(session, "provider").cloned())
                .unwrap_or(json!(preferred_provider));
            let provider_instance_id = active_session
                .as_ref()
                .and_then(|session| non_null(session, "providerInstanceId").cloned())
                .unwrap_or(json!(desired_instance));
            self.set_thread_session(
                thread_id,
                json!({
                    "threadId": thread_id,
                    "status": "starting",
                    "providerName": provider_name,
                    "providerInstanceId": provider_instance_id,
                    "runtimeMode": desired_runtime_mode,
                    "activeTurnId": null,
                    "lastError": null,
                    "updatedAt": created_at,
                }),
                created_at,
            )
            .await?;
        }
        if thread_session.is_some() {
            let current_model_selection = match active_session.as_ref().and_then(|session| non_null(session, "model")) {
                Some(model) => {
                    let mut selection = model_selection.clone();
                    selection["instanceId"] = current_instance_id.clone();
                    selection["model"] = model.clone();
                    selection
                }
                None => model_selection.clone(),
            };
            self.reject_started_thread_model_change(thread_id, &current_model_selection, requested.as_ref())
                .await?;
        }
        if let (Some(_), Some(requested)) = (&thread_session, &requested) {
            if requested["instanceId"].as_str() != Some(current_instance.as_str()) {
                if current_info.driver_kind != desired_info.driver_kind {
                    return Err(request_error(
                        &preferred_provider,
                        "thread.turn.start",
                        format!(
                            "Thread '{thread_id}' is bound to driver '{}' and cannot switch to '{}'.",
                            current_info.driver_kind, desired_info.driver_kind
                        ),
                    ));
                }
                if current_info.continuation_identity.continuation_key != desired_info.continuation_identity.continuation_key {
                    return Err(request_error(
                        &preferred_provider,
                        "thread.turn.start",
                        format!(
                            "Thread '{thread_id}' cannot switch from instance '{current_instance}' to '{desired_instance}' because their provider resume state is incompatible."
                        ),
                    ));
                }
            }
        }
        let project = self.project(str_of(&thread, "projectId")).await?;
        let effective_cwd = resolve_thread_workspace_cwd(&thread, project.as_ref());
        // Prompt seeds and the default title are not user titles: let the provider make one.
        let manual_title = if non_null(&thread, "titleState").and_then(|state| str_of(state, "source")) == Some("manual") {
            trim(str_of(&thread, "title").unwrap_or("")).to_owned()
        } else {
            String::new()
        };
        let prompt_seed = options.title_seed.as_deref().map(trim);
        let session_title = (!manual_title.is_empty() && Some(manual_title.as_str()) != prompt_seed).then(|| str_of(&thread, "title").unwrap_or("").to_owned());

        let start_input = |resume_cursor: Option<Value>| {
            let input = Obj::new()
                .set("threadId", thread_id)
                .set("provider", preferred_provider.as_str())
                .set("providerInstanceId", desired_instance.as_str())
                .set_if(effective_cwd.is_some(), "cwd", || json!(effective_cwd))
                .set_if(session_title.is_some(), "title", || json!(session_title))
                .set("modelSelection", desired_model_selection.clone())
                .set_if(resume_cursor.is_some(), "resumeCursor", || resume_cursor.clone().unwrap_or(Value::Null))
                .set("runtimeMode", desired_runtime_mode.clone())
                .build();
            ProviderSessionStartInput(input)
        };

        let existing_session_thread_id = (thread_session.is_some() && session_status != Some("stopped") && active_session.is_some())
            .then(|| str_of(&thread, "id").unwrap_or(thread_id).to_owned());
        let started = if let Some(existing) = existing_session_thread_id {
            let active = active_session.clone().unwrap_or(Value::Null);
            let runtime_mode_changed = thread["runtimeMode"] != thread_session.as_ref().map(|session| session["runtimeMode"].clone()).unwrap_or(Value::Null);
            let cwd_changed = effective_cwd.as_deref() != str_of(&active, "cwd");
            let switch_mode = self
                .deps
                .providers
                .get_capabilities(&ProviderInstanceId::new(&desired_instance))
                .await?
                .session_model_switch;
            let model_changed = requested
                .as_ref()
                .is_some_and(|requested| non_null(requested, "model") != non_null(&active, "model"));
            let instance_changed = requested
                .as_ref()
                .is_some_and(|requested| non_null(&active, "providerInstanceId") != non_null(requested, "instanceId"));
            let restart_for_model_change = model_changed && switch_mode == SessionModelSwitchMode::Unsupported;
            let previous_selection = self.state(|state| state.thread_model_selections.get(thread_id).cloned());
            let restart_for_selection_change =
                preferred_provider == "claudeAgent" && requested.as_ref().is_some_and(|requested| previous_selection.as_ref() != Some(requested));
            if !runtime_mode_changed && !cwd_changed && !instance_changed && !restart_for_model_change && !restart_for_selection_change {
                self.refresh_workspace_snapshot(&desired_instance, effective_cwd.as_deref());
                return Ok(existing);
            }
            let resume_cursor = if restart_for_model_change {
                None
            } else {
                non_null(&active, "resumeCursor").cloned()
            };
            tracing::info!(
                thread_id,
                runtime_mode_changed,
                cwd_changed,
                model_changed,
                instance_changed,
                restart_for_model_change,
                restart_for_selection_change,
                has_resume_cursor = resume_cursor.is_some(),
                "provider command reactor restarting provider session"
            );
            let session = self.deps.providers.start_session(&ThreadId::new(thread_id), start_input(resume_cursor)).await?;
            self.refresh_workspace_snapshot(&desired_instance, effective_cwd.as_deref());
            session.0
        } else {
            let session = self.deps.providers.start_session(&ThreadId::new(thread_id), start_input(None)).await?;
            self.refresh_workspace_snapshot(&desired_instance, effective_cwd.as_deref());
            session.0
        };
        // bindSessionToThread
        let Some(instance_id) = non_null(&started, "providerInstanceId").cloned() else {
            return Err(request_error(
                &provider_error_label(str_of(&started, "provider")),
                "thread.turn.start",
                format!(
                    "Provider session '{}' started without a provider instance id.",
                    str_of(&started, "threadId").unwrap_or("")
                ),
            ));
        };
        let started_status = str_of(&started, "status");
        let status = if options.pending_turn_start && started_status == Some("ready") {
            "starting"
        } else {
            map_provider_session_status(started_status)
        };
        self.set_thread_session(
            thread_id,
            json!({
                "threadId": thread_id,
                "status": status,
                "providerName": started["provider"],
                "providerInstanceId": instance_id,
                "runtimeMode": desired_runtime_mode,
                "activeTurnId": null,
                "lastError": non_null(&started, "lastError").cloned().unwrap_or(Value::Null),
                "updatedAt": started["updatedAt"],
            }),
            created_at,
        )
        .await?;
        Ok(str_of(&started, "threadId").unwrap_or(thread_id).to_owned())
    }

    /// `buildSendTurnRequestForThread`.
    #[allow(clippy::too_many_arguments)]
    async fn build_send_turn_request(
        &self,
        thread_id: &str,
        message_text: &str,
        attachments: Option<Value>,
        model_selection: Option<Value>,
        interaction_mode: Option<Value>,
        created_at: &str,
        title_seed: Option<String>,
    ) -> Result<Value, TaggedError> {
        let Some(thread) = self.thread_shell(thread_id).await? else {
            return Err(defect(format!("Thread '{thread_id}' was not found in read model.")));
        };
        self.ensure_session_for_thread(
            thread_id,
            created_at,
            EnsureOptions {
                model_selection: model_selection.clone(),
                pending_turn_start: true,
                title_seed,
            },
        )
        .await?;
        if let Some(selection) = &model_selection {
            self.state(|state| state.thread_model_selections.insert(thread_id.to_owned(), selection.clone()));
        }
        let normalized_input = trim(message_text);
        let attachments = attachments.and_then(|value| value.as_array().cloned()).unwrap_or_default();
        let active_session = self.active_session(thread_id).await;
        let switch_mode = match &active_session {
            None => SessionModelSwitchMode::InSession,
            Some(session) => match non_null(session, "providerInstanceId").and_then(Value::as_str) {
                None => {
                    return Err(request_error(
                        &provider_error_label(str_of(session, "provider")),
                        "thread.turn.start",
                        format!(
                            "Active provider session '{}' is missing a provider instance id.",
                            str_of(session, "threadId").unwrap_or("")
                        ),
                    ))
                }
                Some(instance_id) => {
                    self.deps
                        .providers
                        .get_capabilities(&ProviderInstanceId::new(instance_id))
                        .await?
                        .session_model_switch
                }
            },
        };
        let requested = model_selection
            .clone()
            .or_else(|| self.state(|state| state.thread_model_selections.get(thread_id).cloned()))
            .unwrap_or_else(|| thread["modelSelection"].clone());
        let model_for_turn = if switch_mode == SessionModelSwitchMode::Unsupported && model_selection.is_none() {
            match active_session.as_ref().and_then(|session| non_null(session, "model")) {
                Some(model) => {
                    let mut selection = requested;
                    selection["model"] = model.clone();
                    Some(selection)
                }
                None => Some(requested),
            }
        } else {
            model_selection
        };
        Ok(Obj::new()
            .set("threadId", thread_id)
            .set_if(!normalized_input.is_empty(), "input", || json!(normalized_input))
            .set_if(!attachments.is_empty(), "attachments", || json!(attachments))
            .set_if(model_for_turn.is_some(), "modelSelection", || model_for_turn.clone().unwrap_or(Value::Null))
            .set_if(interaction_mode.is_some(), "interactionMode", || {
                interaction_mode.clone().unwrap_or(Value::Null)
            })
            .build())
    }

    /// `maybeGenerateAndRenameWorktreeBranchForFirstTurn`.
    async fn maybe_rename_worktree_branch(
        &self,
        thread_id: &str,
        branch: Option<&str>,
        worktree_path: Option<&str>,
        message_text: &str,
        attachments: &[Value],
    ) {
        let (Some(old_branch), Some(cwd)) = (branch.filter(|b| !b.is_empty()), worktree_path.filter(|p| !p.is_empty())) else {
            return;
        };
        if !is_temporary_worktree_branch(old_branch) {
            return;
        }
        let result = async {
            let settings = self.project_settings_for_thread(thread_id).await?;
            let model_selection = if settings["sourceControlWriterModelSelection"].is_null() {
                settings["textGenerationModelSelection"].clone()
            } else {
                let providers: Vec<Value> = self.deps.provider_status.get_providers().await.into_iter().map(|provider| provider.0).collect();
                zc_settings::settings::resolve_source_control_writer_model_selection(&settings, Some(&providers))
            };
            let generated = self
                .deps
                .text_generation
                .generate_branch_name(BranchNameGenerationInput {
                    cwd: cwd.to_owned(),
                    message: message_text.to_owned(),
                    attachments: attachments.iter().cloned().map(ChatAttachment).collect(),
                    model_selection: ModelSelection(model_selection),
                })
                .await?;
            let target = build_generated_worktree_branch_name(&generated);
            if target == old_branch {
                return Ok::<_, TaggedError>(());
            }
            let renamed = self.deps.git.rename_branch(cwd, old_branch, &target).await?;
            self.dispatch(json!({
                "type": "thread.meta.update",
                "commandId": self.server_command_id("worktree-branch-rename"),
                "threadId": thread_id,
                "branch": renamed,
                "worktreePath": cwd,
            }))
            .await?;
            if let Err(error) = self.deps.vcs_status.refresh_status(cwd).await {
                tracing::debug!(cwd, cause = %pretty(&error), "vcs status refresh after branch rename failed");
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(thread_id, cwd, old_branch, cause = %pretty(&error), "provider command reactor failed to generate or rename worktree branch");
        }
    }

    /// `maybeGenerateThreadTitleForFirstTurn`.
    #[allow(clippy::too_many_arguments)]
    async fn maybe_generate_first_title(
        &self,
        thread_id: &str,
        cwd: &str,
        message_text: &str,
        attachments: &[Value],
        title_seed: Option<&str>,
        expected_title: &str,
        expected_version: Value,
    ) {
        let result = async {
            let settings = self.project_settings_for_thread(thread_id).await?;
            let input = ThreadTitleGenerationInput {
                linked_context: None,
                cwd: cwd.to_owned(),
                message: message_text.to_owned(),
                previous_title: None,
                attachments: attachments.iter().cloned().map(ChatAttachment).collect(),
                model_selection: ModelSelection(settings["textGenerationModelSelection"].clone()),
            };
            // `Effect.retry({times: 2, schedule: Schedule.exponential("2 seconds")})`.
            let mut delay = self.deps.title_retry_base;
            let mut attempt = 0;
            let generated = loop {
                match self.deps.text_generation.generate_thread_title(input.clone()).await {
                    Ok(generated) => break generated,
                    Err(error) if attempt >= 2 => return Err(error),
                    Err(_) => {
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        attempt += 1;
                    }
                }
            };
            let Some(thread) = self.thread_shell(thread_id).await? else {
                return Ok::<_, TaggedError>(());
            };
            if !can_replace_thread_title(str_of(&thread, "title").unwrap_or(""), title_seed) {
                return Ok(());
            }
            let is_default = generated.title == DEFAULT_THREAD_TITLE;
            self.dispatch(json!({
                "type": "thread.title.generate.complete",
                "commandId": self.server_command_id("thread-title-rename"),
                "threadId": thread_id,
                "title": if is_default { expected_title.to_owned() } else { generated.title.clone() },
                "expectedTitle": expected_title,
                "expectedVersion": expected_version,
                "needsRefinement": generated.needs_refinement == Some(true) || is_default,
            }))
            .await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(thread_id, cwd, cause = %pretty(&error), "provider command reactor failed to generate or rename thread title");
        }
    }

    /// `maybeRefineThreadTitle(threadId)`.
    async fn maybe_refine_thread_title(&self, thread_id: &str) -> Result<(), TaggedError> {
        let Some(thread) = self.thread_shell(thread_id).await? else { return Ok(()) };
        let title_state = non_null(&thread, "titleState");
        if title_state.and_then(|state| state.get("needsRefinement")) != Some(&json!(true))
            || title_state.and_then(|state| str_of(state, "source")) != Some("generated")
            || non_null(&thread, "titleRegeneration").is_some()
            || non_null(&thread, "latestTurn").and_then(|turn| str_of(turn, "state")) != Some("completed")
            || non_null(&thread, "session").and_then(|session| str_of(session, "status")) != Some("ready")
        {
            return Ok(());
        }
        let Some(detail) = self.deps.reads.thread_detail(&ThreadId::new(thread_id)).await? else {
            return Ok(());
        };
        let user_messages = detail["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|message| str_of(message, "role") == Some("user"))
            .count();
        if user_messages != 1 {
            return Ok(());
        }
        self.dispatch(json!({
            "type": "thread.title.refine",
            "commandId": self.server_command_id("thread-title-refine"),
            "threadId": thread_id,
            "expectedVersion": title_state.and_then(|state| state.get("version")).cloned().unwrap_or(Value::Null),
        }))
        .await
    }

    /// `regenerateThreadTitle(event, requestId)`.
    async fn regenerate_thread_title(&self, event: &Value, request_id: &str) -> Result<Regeneration, TaggedError> {
        let payload = &event["payload"];
        if payload.get("regenerateTitle") != Some(&json!(true)) {
            return Ok(Regeneration::Superseded);
        }
        let thread_id = str_of(payload, "threadId").unwrap_or("");
        let Some(thread) = self.deps.reads.thread_detail(&ThreadId::new(thread_id)).await? else {
            return Ok(Regeneration::Superseded);
        };
        if non_null(&thread, "titleRegeneration").and_then(|regeneration| str_of(regeneration, "requestId")) != Some(request_id) {
            return Ok(Regeneration::Superseded);
        }
        let messages = thread["messages"].as_array().cloned().unwrap_or_default();
        let context = format_thread_title_context(&messages);
        if context.message.is_empty() {
            return Ok(Regeneration::Completed(None));
        }
        let previous_title = str_of(payload, "previousTitle").or_else(|| str_of(&thread, "title")).unwrap_or("").to_owned();
        if str_of(&thread, "title") != Some(previous_title.as_str()) {
            return Ok(Regeneration::Superseded);
        }
        let project = self.project(str_of(&thread, "projectId")).await?;
        let cwd = resolve_thread_workspace_cwd(&thread, project.as_ref()).unwrap_or_else(process_cwd);
        let settings = resolve_project_settings(&self.settings_value().await?, str_of(&thread, "projectId"));
        let generated = self
            .deps
            .text_generation
            .generate_thread_title(ThreadTitleGenerationInput {
                linked_context: None,
                cwd,
                message: context.message,
                previous_title: Some(previous_title.clone()),
                attachments: context.attachments.into_iter().map(ChatAttachment).collect(),
                model_selection: ModelSelection(settings["textGenerationModelSelection"].clone()),
            })
            .await?;
        if generated.title == DEFAULT_THREAD_TITLE || generated.title == previous_title {
            return Ok(Regeneration::Completed(None));
        }
        let Some(latest) = self.thread_shell(thread_id).await? else {
            return Ok(Regeneration::Superseded);
        };
        if non_null(&latest, "titleRegeneration").and_then(|regeneration| str_of(regeneration, "requestId")) != Some(request_id)
            || str_of(&latest, "title") != Some(previous_title.as_str())
        {
            return Ok(Regeneration::Superseded);
        }
        Ok(Regeneration::Completed(Some(generated.title)))
    }

    /// `dispatchThreadTitleRegenerationCompletion`.
    async fn dispatch_title_regeneration_completion(&self, thread_id: &str, request_id: &str, title: Option<&str>) -> Result<(), TaggedError> {
        let command = Obj::new()
            .set("type", "thread.title.regeneration.complete")
            .set("commandId", self.server_command_id("thread-title-regeneration-complete"))
            .set("threadId", thread_id)
            .set("requestId", request_id)
            .set_if(title.is_some(), "title", || json!(title))
            .build();
        self.dispatch(command).await
    }

    /// `processThreadTitleRegenerationSafely(event)`.
    async fn process_title_regeneration(&self, event: &Value) {
        let payload = &event["payload"];
        if payload.get("regenerateTitle") != Some(&json!(true)) {
            return;
        }
        let thread_id = str_of(payload, "threadId").unwrap_or("").to_owned();
        let request_id = non_null(payload, "titleRegeneration")
            .and_then(|regeneration| str_of(regeneration, "requestId"))
            .or_else(|| str_of(event, "commandId"))
            .map(str::to_owned);
        let Some(request_id) = request_id else { return };
        let result = match self.regenerate_thread_title(event, &request_id).await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(thread_id, cause = %pretty(&error), "provider command reactor failed to regenerate thread title");
                Regeneration::Completed(None)
            }
        };
        let Regeneration::Completed(title) = result else { return };
        if let Err(error) = self.dispatch_title_regeneration_completion(&thread_id, &request_id, title.as_deref()).await {
            tracing::warn!(thread_id, cause = %pretty(&error), "provider command reactor retrying title regeneration completion");
            if let Err(error) = self.dispatch_title_regeneration_completion(&thread_id, &request_id, title.as_deref()).await {
                tracing::warn!(thread_id, cause = %pretty(&error), "provider command reactor failed to complete title regeneration");
            }
        }
    }

    /// `findPendingThreadTitles`: interrupted regenerations and titles waiting for refinement.
    async fn find_pending_thread_titles(&self) -> Result<(Vec<(String, String)>, Vec<String>), TaggedError> {
        let threads = self.deps.reads.command_read_model_threads().await?;
        let interrupted = threads
            .iter()
            .filter_map(|thread| {
                let request_id = non_null(thread, "titleRegeneration").and_then(|regeneration| str_of(regeneration, "requestId"))?;
                Some((str_of(thread, "id")?.to_owned(), request_id.to_owned()))
            })
            .collect();
        let refinement = threads
            .iter()
            .filter(|thread| non_null(thread, "titleState").and_then(|state| state.get("needsRefinement")) == Some(&json!(true)))
            .filter_map(|thread| str_of(thread, "id").map(str::to_owned))
            .collect();
        Ok((interrupted, refinement))
    }

    /// `processTurnStartRequested`.
    async fn process_turn_start_requested(self: &Arc<Self>, received: &Value) -> Result<(), TaggedError> {
        let command_id = str_of(received, "commandId").map(str::to_owned);
        let resumed = command_id.as_ref().and_then(|id| {
            self.state(|state| {
                state
                    .resumed_turn_starts
                    .get(id)
                    .map(|resumed| (resumed.event.clone(), resumed.queued.clone(), resumed.sent.clone()))
            })
        });
        let mut event = received.clone();
        if let Some((resumed_event, _, _)) = &resumed {
            event["payload"] = resumed_event["payload"].clone();
        }
        let key = match &command_id {
            Some(id) => format!("command:{id}"),
            None => format!("event:{}", str_of(&event, "eventId").unwrap_or("")),
        };
        let seen = self.handled_turn_start_keys.with(|keys| {
            let seen = keys.contains(&key);
            keys.set(key.clone(), true);
            seen
        });
        if seen {
            return Ok(());
        }
        let payload = event["payload"].clone();
        let thread_id = str_of(&payload, "threadId").unwrap_or("").to_owned();
        let message_id = str_of(&payload, "messageId").unwrap_or("").to_owned();
        let created_at = str_of(&payload, "createdAt").unwrap_or("").to_owned();
        let Some(thread) = self.thread_shell(&thread_id).await? else { return Ok(()) };
        let turn_start = self
            .deps
            .reads
            .turn_start_message(&ThreadId::new(&thread_id), &MessageId::new(&message_id))
            .await?;
        let Some(turn_start) = turn_start.filter(|start| str_of(&start.message, "role") == Some("user")) else {
            return self
                .append_failure(
                    &thread_id,
                    "provider.turn.start.failed",
                    "Provider turn start failed",
                    &format!("User message '{message_id}' was not found for turn start request."),
                    None,
                    &created_at,
                    Some(&message_id),
                )
                .await;
        };
        let message = turn_start.message.clone();
        let has_other_user_messages = turn_start.has_other_user_messages;
        let append_turn_start_failure = {
            let core = self.clone();
            let (thread_id, created_at, message_id) = (thread_id.clone(), created_at.clone(), message_id.clone());
            move |summary: String, detail: String| {
                let core = core.clone();
                let (thread_id, created_at, message_id) = (thread_id.clone(), created_at.clone(), message_id.clone());
                async move {
                    core.append_failure(
                        &thread_id,
                        "provider.turn.start.failed",
                        &summary,
                        &detail,
                        None,
                        &created_at,
                        Some(&message_id),
                    )
                    .await
                }
            }
        };
        if let Some((_, queued, _)) = &resumed {
            if !self.queue_is_current(&thread_id, queued) {
                return append_turn_start_failure(
                    "Queued message was not sent".into(),
                    "The queued message was canceled before it could resume. Send it again to continue.".into(),
                )
                .await;
            }
        }
        let recover_turn_start_failure = {
            let core = self.clone();
            let (thread_id, created_at) = (thread_id.clone(), created_at.clone());
            let append = append_turn_start_failure.clone();
            move |error: TaggedError| {
                let core = core.clone();
                let (thread_id, created_at) = (thread_id.clone(), created_at.clone());
                let append = append.clone();
                async move {
                    let detail = format_failure_detail(&error);
                    let result = async {
                        core.set_session_error_on_turn_start_failure(&thread_id, &detail, &created_at).await?;
                        append("Provider turn start failed".into(), detail.clone()).await
                    }
                    .await;
                    if let Err(recovery) = result {
                        tracing::warn!(thread_id, cause = %pretty(&recovery), original_cause = %pretty(&error), "provider command reactor failed to recover turn start failure");
                    }
                }
            }
        };

        // Native account commands belong to the thread's existing provider session.
        let auth_handled = async {
            let instance_id = non_null(&thread, "session")
                .and_then(|session| str_of(session, "providerInstanceId"))
                .or_else(|| non_null(&payload, "modelSelection").and_then(|selection| str_of(selection, "instanceId")))
                .or_else(|| str_of(&thread["modelSelection"], "instanceId"))
                .unwrap_or("")
                .to_owned();
            let has_attachments = message.get("attachments").and_then(Value::as_array).is_some_and(|items| !items.is_empty());
            let handled = self
                .deps
                .provider_auth
                .try_handle_prompt_command(&ProviderInstanceId::new(&instance_id), str_of(&message, "text").unwrap_or(""), has_attachments)
                .await?;
            if !handled {
                return Ok::<_, TaggedError>(false);
            }
            let info = self.deps.providers.get_instance_info(&ProviderInstanceId::new(&instance_id)).await?;
            self.set_thread_session(
                &thread_id,
                json!({
                    "threadId": thread_id,
                    "status": "stopped",
                    "providerName": info.driver_kind,
                    "providerInstanceId": instance_id,
                    "runtimeMode": thread["runtimeMode"],
                    "activeTurnId": null,
                    "lastError": null,
                    "updatedAt": created_at,
                }),
                &created_at,
            )
            .await?;
            self.dispatch(json!({
                "type": "thread.activity.append",
                "commandId": self.server_command_id("provider-sign-out"),
                "threadId": thread_id,
                "activity": {
                    "id": (self.deps.uuids)(),
                    "tone": "info",
                    "kind": "provider.auth.signed-out",
                    "summary": "Provider signed out",
                    "payload": {"providerInstanceId": instance_id},
                    "turnId": null,
                    "createdAt": created_at,
                },
                "createdAt": created_at,
            }))
            .await?;
            Ok(true)
        }
        .await;
        match auth_handled {
            Ok(false) => {}
            Ok(true) => return Ok(()),
            Err(error) => {
                recover_turn_start_failure(error).await;
                return Ok(());
            }
        }

        self.ensure_thread_worktree(&thread).await?;

        let is_compact = is_compact_command_message(&message);
        let title_seed = str_of(&payload, "titleSeed").map(str::to_owned);
        let attachments = message.get("attachments").cloned();
        if !has_other_user_messages && !is_compact {
            let project = self.project(str_of(&thread, "projectId")).await?;
            let generation_cwd = resolve_thread_workspace_cwd(&thread, project.as_ref()).unwrap_or_else(process_cwd);
            let message_text = assistant_citations_to_plain_text(str_of(&message, "text").unwrap_or(""));
            let attachment_list = attachments.as_ref().and_then(Value::as_array).cloned().unwrap_or_default();
            {
                let core = self.clone();
                let (thread_id, message_text, attachment_list) = (thread_id.clone(), message_text.clone(), attachment_list.clone());
                let branch = str_of(&thread, "branch").map(str::to_owned);
                let worktree_path = str_of(&thread, "worktreePath").map(str::to_owned);
                self.fork(async move {
                    core.maybe_rename_worktree_branch(&thread_id, branch.as_deref(), worktree_path.as_deref(), &message_text, &attachment_list)
                        .await
                });
            }
            let title_state = non_null(&thread, "titleState");
            if title_state.and_then(|state| str_of(state, "source")) != Some("manual")
                && can_replace_thread_title(str_of(&thread, "title").unwrap_or(""), title_seed.as_deref())
            {
                let core = self.clone();
                let thread_id = thread_id.clone();
                let expected_title = str_of(&thread, "title").unwrap_or("").to_owned();
                let expected_version = title_state.and_then(|state| state.get("version")).cloned().unwrap_or(Value::Null);
                let title_seed = title_seed.clone();
                self.fork(async move {
                    core.maybe_generate_first_title(
                        &thread_id,
                        &generation_cwd,
                        &message_text,
                        &attachment_list,
                        title_seed.as_deref(),
                        &expected_title,
                        expected_version,
                    )
                    .await
                });
            }
        }

        let model_selection = non_null(&payload, "modelSelection").cloned();
        if is_compact {
            if !has_other_user_messages {
                return append_turn_start_failure(
                    "Context compaction failed".into(),
                    "Context compaction requires an existing conversation.".into(),
                )
                .await;
            }
            let latest = self.thread_shell(&thread_id).await?;
            let latest_status = latest
                .as_ref()
                .and_then(|thread| non_null(thread, "session"))
                .and_then(|session| str_of(session, "status").map(str::to_owned));
            let busy = self.state(|state| state.compacting.contains(&thread_id) || state.turns_after_compaction.contains_key(&thread_id));
            if busy || matches!(latest_status.as_deref(), Some("starting" | "running")) {
                return append_turn_start_failure(
                    "Context compaction failed".into(),
                    "Context compaction is unavailable while a provider turn is running.".into(),
                )
                .await;
            }
            self.state(|state| state.compacting.insert(thread_id.clone()));
            let core = self.clone();
            let append = append_turn_start_failure.clone();
            self.fork(async move {
                let ensured = AtomicBool::new(false);
                let result = async {
                    core.ensure_session_for_thread(
                        &thread_id,
                        &created_at,
                        EnsureOptions {
                            model_selection: model_selection.clone(),
                            pending_turn_start: true,
                            title_seed: None,
                        },
                    )
                    .await?;
                    ensured.store(true, Ordering::SeqCst);
                    if let Some(selection) = &model_selection {
                        core.state(|state| state.thread_model_selections.insert(thread_id.clone(), selection.clone()));
                    }
                    core.deps
                        .providers
                        .compact_thread(&ThreadId::new(&thread_id), model_selection.clone().map(ModelSelection), Some(MessageId::new(&message_id)))
                        .await?;
                    core.restore_compaction(&thread_id, true).await?;
                    core.state(|state| state.compacting.remove(&thread_id));
                    core.resume_turns_after_compaction(&thread_id).await
                }
                .await;
                if let Err(error) = result {
                    let detail = format_failure_detail(&error);
                    let recovery = async {
                        if !ensured.load(Ordering::SeqCst) {
                            core.set_session_error_on_turn_start_failure(&thread_id, &detail, &created_at).await?;
                            append("Context compaction failed".into(), detail.clone()).await
                        } else {
                            let appended = append("Context compaction failed".into(), detail.clone()).await;
                            if let Err(restore) = core.restore_compaction(&thread_id, false).await {
                                tracing::warn!(thread_id, cause = %pretty(&restore), "failed to restore provider session after compaction failure");
                            }
                            appended
                        }
                    }
                    .await;
                    if let Err(recovery) = recovery {
                        tracing::warn!(thread_id, cause = %pretty(&recovery), original_cause = %pretty(&error), "provider command reactor failed to recover compaction failure");
                    }
                    core.state(|state| state.compacting.remove(&thread_id));
                    core.cancel_turns_after_compaction(&thread_id, "Context compaction failed. Send this message again to continue.").await;
                }
            });
            return Ok(());
        }
        if resumed.is_none() {
            let queued = self.state(|state| {
                if state.compacting.contains(&thread_id) || state.turns_after_compaction.contains_key(&thread_id) {
                    let queue = state.turns_after_compaction.entry(thread_id.clone()).or_default().clone();
                    queue.lock().unwrap_or_else(|p| p.into_inner()).push_back(event.clone());
                    true
                } else {
                    false
                }
            });
            if queued {
                return Ok(());
            }
        }
        let records = message
            .get("context")
            .and_then(|context| context.get("records"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let message_text = project_composer_context_for_provider(str_of(&message, "text").unwrap_or(""), &records);
        let request = self
            .build_send_turn_request(
                &thread_id,
                &message_text,
                attachments,
                model_selection,
                payload.get("interactionMode").cloned(),
                &created_at,
                if !has_other_user_messages { title_seed } else { None },
            )
            .await;
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                recover_turn_start_failure(error).await;
                return Ok(());
            }
        };
        // The forked send settles `sent` from here on, so drop the entry the hook uses.
        if resumed.is_some() {
            if let Some(id) = &command_id {
                self.state(|state| state.resumed_turn_starts.remove(id));
            }
        }
        let core = self.clone();
        let sent = resumed.map(|(_, _, sent)| sent);
        self.fork(async move {
            if let Err(error) = core.deps.providers.send_turn(ProviderSendTurnInput(request)).await {
                recover_turn_start_failure(error).await;
            }
            if let Some(sent) = sent {
                sent.succeed();
            }
        });
        Ok(())
    }

    /// `processTurnInterruptRequested`.
    async fn process_turn_interrupt_requested(&self, event: &Value) -> Result<(), TaggedError> {
        let payload = &event["payload"];
        let thread_id = str_of(payload, "threadId").unwrap_or("").to_owned();
        let turn_id = str_of(payload, "turnId").map(str::to_owned);
        let created_at = str_of(payload, "createdAt").unwrap_or("").to_owned();
        self.cancel_turns_after_compaction(&thread_id, "Context compaction was interrupted. Send this message again to continue.")
            .await;
        let Some(thread) = self.thread_shell(&thread_id).await? else { return Ok(()) };
        let session = non_null(&thread, "session");
        if session.is_none_or(|session| str_of(session, "status") == Some("stopped")) {
            return self
                .append_failure(
                    &thread_id,
                    "provider.turn.interrupt.failed",
                    "Provider turn interrupt failed",
                    "No active provider session is bound to this thread.",
                    turn_id.as_deref(),
                    &created_at,
                    None,
                )
                .await;
        }
        // Orchestration turn ids are not provider turn ids: interrupt by session.
        let Err(error) = self
            .deps
            .providers
            .interrupt_turn(ProviderInterruptTurnInput(json!({"threadId": thread_id})))
            .await
        else {
            return Ok(());
        };
        let detail = format_failure_detail(&error);
        let unrelated = |session: Option<&Value>| match session {
            None => true,
            Some(session) => {
                let status = str_of(session, "status");
                let active = str_of(session, "activeTurnId");
                status == Some("stopped") || status == Some("ready") || (turn_id.is_some() && active.is_some() && active != turn_id.as_deref())
            }
        };
        let latest = self.thread_shell(&thread_id).await?;
        if unrelated(latest.as_ref().and_then(|thread| non_null(thread, "session"))) {
            return Ok(());
        }
        if let Err(stop_error) = self.deps.providers.stop_session(ProviderStopSessionInput(json!({"threadId": thread_id}))).await {
            tracing::warn!(thread_id, cause = %pretty(&stop_error), original_cause = %pretty(&error), "provider command reactor failed to stop session after interrupt failure");
        }
        let stopped = self.thread_shell(&thread_id).await?;
        let stopped_session = stopped.as_ref().and_then(|thread| non_null(thread, "session")).cloned();
        if unrelated(stopped_session.as_ref()) {
            return Ok(());
        }
        let mut next = stopped_session.unwrap_or(Value::Null);
        next["status"] = json!("stopped");
        next["activeTurnId"] = Value::Null;
        next["lastError"] = json!(detail);
        next["updatedAt"] = json!(created_at);
        self.set_thread_session(&thread_id, next, &created_at).await?;
        self.append_failure(
            &thread_id,
            "provider.turn.interrupt.failed",
            "Provider turn interrupt failed",
            &detail,
            turn_id.as_deref(),
            &created_at,
            None,
        )
        .await
    }

    /// `processApprovalResponseRequested`.
    async fn process_approval_response_requested(&self, event: &Value) -> Result<(), TaggedError> {
        let payload = &event["payload"];
        let thread_id = str_of(payload, "threadId").unwrap_or("");
        let request_id = str_of(payload, "requestId").unwrap_or("");
        let created_at = str_of(payload, "createdAt").unwrap_or("");
        let Some(thread) = self.thread_shell(thread_id).await? else { return Ok(()) };
        if non_null(&thread, "session").is_none_or(|session| str_of(session, "status") == Some("stopped")) {
            return self
                .append_failure(
                    thread_id,
                    "provider.approval.respond.failed",
                    "Provider approval response failed",
                    "No active provider session is bound to this thread.",
                    None,
                    created_at,
                    Some(request_id),
                )
                .await;
        }
        let input = json!({"threadId": thread_id, "requestId": request_id, "decision": payload["decision"]});
        if let Err(error) = self.deps.providers.respond_to_request(ProviderRespondToRequestInput(input)).await {
            let detail = if is_unknown_pending_approval_request_error(&error) {
                stale_pending_request_detail("approval", request_id)
            } else {
                pretty(&error)
            };
            self.append_failure(
                thread_id,
                "provider.approval.respond.failed",
                "Provider approval response failed",
                &detail,
                None,
                created_at,
                Some(request_id),
            )
            .await?;
        }
        Ok(())
    }

    /// `processUserInputResponseRequested`.
    async fn process_user_input_response_requested(&self, event: &Value) -> Result<(), TaggedError> {
        let payload = &event["payload"];
        let thread_id = str_of(payload, "threadId").unwrap_or("");
        let request_id = str_of(payload, "requestId").unwrap_or("");
        let created_at = str_of(payload, "createdAt").unwrap_or("");
        let Some(thread) = self.thread_shell(thread_id).await? else { return Ok(()) };
        if non_null(&thread, "session").is_none_or(|session| str_of(session, "status") == Some("stopped")) {
            return self
                .append_failure(
                    thread_id,
                    "provider.user-input.respond.failed",
                    "Provider user input response failed",
                    "No active provider session is bound to this thread.",
                    None,
                    created_at,
                    Some(request_id),
                )
                .await;
        }
        let input = Obj::new()
            .set("threadId", thread_id)
            .set("requestId", request_id)
            .set("answers", payload.get("answers").cloned().unwrap_or(Value::Null))
            .set_if(crate::js::truthy(payload.get("attachmentsByQuestionId")), "attachmentsByQuestionId", || {
                payload["attachmentsByQuestionId"].clone()
            })
            .build();
        if let Err(error) = self.deps.providers.respond_to_user_input(ProviderRespondToUserInputInput(input)).await {
            let detail = if is_unknown_pending_user_input_request_error(&error) {
                stale_pending_request_detail("user-input", request_id)
            } else {
                pretty(&error)
            };
            self.append_failure(
                thread_id,
                "provider.user-input.respond.failed",
                "Provider user input response failed",
                &detail,
                None,
                created_at,
                Some(request_id),
            )
            .await?;
        }
        Ok(())
    }

    /// `processSessionStopRequested`.
    async fn process_session_stop_requested(&self, event: &Value) -> Result<(), TaggedError> {
        let payload = &event["payload"];
        let thread_id = str_of(payload, "threadId").unwrap_or("");
        let Some(thread) = self.thread_shell(thread_id).await? else { return Ok(()) };
        let thread_id = str_of(&thread, "id").unwrap_or(thread_id).to_owned();
        let now = str_of(payload, "createdAt").unwrap_or("").to_owned();
        let was_compacting = self.state(|state| state.compacting.contains(&thread_id));
        self.state(|state| state.stopping.insert(thread_id.clone()));
        let result = async {
            self.cancel_turns_after_compaction(
                &thread_id,
                "The session was stopped during context compaction. Send this message again to continue.",
            )
            .await;
            let session = non_null(&thread, "session");
            if session.is_some_and(|session| str_of(session, "status") != Some("stopped")) {
                self.deps
                    .providers
                    .stop_session(ProviderStopSessionInput(json!({"threadId": thread_id})))
                    .await?;
            }
            Ok::<_, TaggedError>(())
        }
        .await;
        let outcome = match result {
            Err(error) => {
                let detail = format_failure_detail(&error);
                self.state(|state| state.stopping.remove(&thread_id));
                let compaction_settled = was_compacting && !self.state(|state| state.compacting.contains(&thread_id));
                async {
                    if compaction_settled {
                        self.restore_compaction(&thread_id, false).await?;
                    }
                    self.append_failure(
                        &thread_id,
                        "provider.session.stop.failed",
                        "Provider session stop failed",
                        &detail,
                        None,
                        &now,
                        None,
                    )
                    .await
                }
                .await
            }
            Ok(()) => {
                let session = non_null(&thread, "session");
                let field = |key: &str| session.and_then(|session| non_null(session, key)).cloned();
                let next = Obj::new()
                    .set("threadId", thread_id.as_str())
                    .set("status", "stopped")
                    .set("providerName", field("providerName").unwrap_or(Value::Null))
                    .set_if(field("providerInstanceId").is_some(), "providerInstanceId", || {
                        field("providerInstanceId").unwrap_or(Value::Null)
                    })
                    .set("runtimeMode", field("runtimeMode").unwrap_or(json!(DEFAULT_RUNTIME_MODE)))
                    .set("activeTurnId", Value::Null)
                    .set("lastError", field("lastError").unwrap_or(Value::Null))
                    .set("updatedAt", now.as_str())
                    .build();
                self.set_thread_session(&thread_id, next, &now).await
            }
        };
        self.state(|state| state.stopping.remove(&thread_id));
        outcome
    }

    /// `processDomainEvent(event)`.
    async fn process_domain_event(self: &Arc<Self>, event: &Value) -> Result<(), TaggedError> {
        let payload = &event["payload"];
        let thread_id = str_of(payload, "threadId").unwrap_or("").to_owned();
        match str_of(event, "type").unwrap_or("") {
            "thread.meta-updated" => {
                if payload.get("regenerateTitle") == Some(&json!(true)) {
                    // Enqueued by the caller on the title worker.
                } else if non_null(payload, "titleState").and_then(|state| state.get("needsRefinement")) == Some(&json!(true)) {
                    self.maybe_refine_thread_title(&thread_id).await?;
                }
            }
            "thread.session-set" => {
                if str_of(&payload["session"], "status") == Some("ready") {
                    self.maybe_refine_thread_title(&thread_id).await?;
                }
            }
            "thread.runtime-mode-set" => {
                let Some(thread) = self.thread_shell(&thread_id).await? else { return Ok(()) };
                if non_null(&thread, "session").is_none_or(|session| str_of(session, "status") == Some("stopped")) {
                    return Ok(());
                }
                let cached = self.state(|state| state.thread_model_selections.get(&thread_id).cloned());
                let occurred_at = str_of(event, "occurredAt").unwrap_or("").to_owned();
                let resume = self.ensure_session_for_thread(
                    &thread_id,
                    &occurred_at,
                    EnsureOptions {
                        model_selection: cached,
                        ..Default::default()
                    },
                );
                match str_of(&thread, "worktreePath").filter(|path| !path.is_empty()) {
                    Some(path) => with_workspace_lease(&resolve_path(path), resume).await?,
                    None => resume.await?,
                };
            }
            "thread.turn-start-requested" => {
                let thread = self.thread_shell(&thread_id).await?;
                match thread
                    .as_ref()
                    .and_then(|thread| str_of(thread, "worktreePath"))
                    .filter(|path| !path.is_empty())
                {
                    Some(path) => with_workspace_lease(&resolve_path(path), self.process_turn_start_requested(event)).await?,
                    None => self.process_turn_start_requested(event).await?,
                }
            }
            "thread.turn-interrupt-requested" => self.process_turn_interrupt_requested(event).await?,
            "thread.approval-response-requested" => self.process_approval_response_requested(event).await?,
            "thread.user-input-response-requested" => self.process_user_input_response_requested(event).await?,
            "thread.session-stop-requested" => self.process_session_stop_requested(event).await?,
            "thread.settled" => {
                let thread = self.thread_shell(&thread_id).await?;
                // A thread re-engaged before this ran keeps its shells and session.
                let Some(thread) = thread.filter(|thread| str_of(thread, "settledOverride") == Some("settled")) else {
                    return Ok(());
                };
                // Idle shells close; a terminal running a command stays.
                self.deps.terminals.close_idle(&ThreadId::new(&thread_id), None).await;
                if non_null(&thread, "session").is_none_or(|session| str_of(session, "status") == Some("stopped")) {
                    return Ok(());
                }
                let source = str_of(event, "commandId").or_else(|| str_of(event, "eventId")).unwrap_or("");
                self.dispatch(json!({
                    "type": "thread.session.stop",
                    "commandId": format!("session-stop-for-settle:{source}"),
                    "threadId": thread_id,
                    "createdAt": event["occurredAt"],
                    "onlyIfSettled": true,
                }))
                .await?;
            }
            _ => {}
        }
        Ok(())
    }

    /// `processDomainEventSafely(event)`.
    async fn process_domain_event_safely(self: &Arc<Self>, event: Value) {
        let result = self.process_domain_event(&event).await;
        // A replay that returned before forking its send still holds its entry: settle it.
        if let Some(command_id) = str_of(&event, "commandId") {
            if let Some(sent) = self.state(|state| state.resumed_turn_starts.get(command_id).map(|resumed| resumed.sent.clone())) {
                sent.succeed();
            }
        }
        if let Err(error) = result {
            tracing::warn!(event_type = str_of(&event, "type"), cause = %pretty(&error), "provider command reactor failed to process event");
        }
    }
}

/// Whether the reactor's worker takes the event (`processEvent` of `start`).
fn is_provider_intent(event: &Value) -> bool {
    let payload = &event["payload"];
    match str_of(event, "type").unwrap_or("") {
        "thread.meta-updated" => {
            payload.get("regenerateTitle") == Some(&json!(true))
                || non_null(payload, "titleState").and_then(|state| state.get("needsRefinement")) == Some(&json!(true))
        }
        "thread.session-set" => str_of(&payload["session"], "status") == Some("ready"),
        "thread.runtime-mode-set"
        | "thread.turn-start-requested"
        | "thread.turn-interrupt-requested"
        | "thread.approval-response-requested"
        | "thread.user-input-response-requested"
        | "thread.session-stop-requested"
        | "thread.settled" => true,
        _ => false,
    }
}

/// `ProviderCommandReactor`.
pub struct ProviderCommandReactor {
    core: Arc<Core>,
    worker: DrainableWorker<Value>,
    title_worker: DrainableWorker<Value>,
}

impl ProviderCommandReactor {
    pub fn new(deps: CommandReactorDeps, stop: CancellationToken) -> Self {
        let clock = deps.clock.clone();
        let core = Arc::new(Core {
            deps,
            state: Mutex::new(ReactorState::default()),
            handled_turn_start_keys: SharedTtlMap::with_clock(HANDLED_TURN_START_KEY_MAX, HANDLED_TURN_START_KEY_TTL, clock),
            tasks: TaskTracker::new(),
            stop: stop.clone(),
        });
        let title_core = core.clone();
        let title_worker = DrainableWorker::start(stop.clone(), move |event: Value| {
            let core = title_core.clone();
            async move { core.process_title_regeneration(&event).await }
        });
        let worker_core = core.clone();
        let titles = title_worker.clone();
        let worker = DrainableWorker::start(stop, move |event: Value| {
            let core = worker_core.clone();
            let titles = titles.clone();
            async move {
                if str_of(&event, "type") == Some("thread.meta-updated") && event["payload"].get("regenerateTitle") == Some(&json!(true)) {
                    titles.enqueue(event.clone());
                }
                core.process_domain_event_safely(event).await
            }
        });
        Self { core, worker, title_worker }
    }

    /// `start()`: finds interrupted title work, subscribes to domain events (the subscription
    /// exists when this returns), then clears interrupted regenerations and schedules pending
    /// refinements.
    pub async fn start(&self) {
        self.start_with_activation(None).await;
    }

    /// `start()` under a `ServerActivation`: subscribed now, but events are processed (and
    /// interrupted title work recovered) only once `activation` resolves.
    pub async fn start_with_activation(&self, activation: Option<futures::future::BoxFuture<'static, ()>>) {
        let pending = match self.core.find_pending_thread_titles().await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(cause = %pretty(&error), "provider command reactor failed to find pending thread titles");
                (Vec::new(), Vec::new())
            }
        };
        let mut events = self.core.deps.engine.subscribe_domain_events();
        let activated = Signal::new();
        let worker = self.worker.clone();
        let stop = self.core.stop.clone();
        let gate = activated.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = gate.wait() => {}
            }
            loop {
                let event = tokio::select! {
                    _ = stop.cancelled() => break,
                    event = events.next() => event,
                };
                let Some(event) = event else { break };
                let value = match &event {
                    OrchestrationEvent::ThreadMetaUpdated(_)
                    | OrchestrationEvent::ThreadSessionSet(_)
                    | OrchestrationEvent::ThreadRuntimeModeSet(_)
                    | OrchestrationEvent::ThreadTurnStartRequested(_)
                    | OrchestrationEvent::ThreadTurnInterruptRequested(_)
                    | OrchestrationEvent::ThreadApprovalResponseRequested(_)
                    | OrchestrationEvent::ThreadUserInputResponseRequested(_)
                    | OrchestrationEvent::ThreadSessionStopRequested(_)
                    | OrchestrationEvent::ThreadSettled(_) => serde_json::to_value(&event).unwrap_or(Value::Null),
                    _ => continue,
                };
                if is_provider_intent(&value) {
                    worker.enqueue(value);
                }
            }
        });
        let core = self.core.clone();
        let recover = async move {
            let (interrupted, refinements) = pending;
            for (thread_id, request_id) in interrupted {
                if let Err(error) = core.dispatch_title_regeneration_completion(&thread_id, &request_id, None).await {
                    tracing::warn!(thread_id, cause = %pretty(&error), "provider command reactor failed to clear interrupted title regeneration");
                }
            }
            for thread_id in refinements {
                if let Err(error) = core.maybe_refine_thread_title(&thread_id).await {
                    tracing::warn!(thread_id, cause = %pretty(&error), "provider command reactor failed to recover pending thread titles");
                }
            }
        };
        match activation {
            None => {
                activated.succeed();
                recover.await;
            }
            Some(activation) => {
                let stop = self.core.stop.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = stop.cancelled() => {}
                        _ = activation => {
                            activated.succeed();
                            recover.await;
                        }
                    }
                });
            }
        }
    }

    /// `drain`: the event worker, then the title worker.
    pub async fn drain(&self) {
        self.worker.drain().await;
        self.title_worker.drain().await;
    }

    /// Waits for the forked sends, compactions and generations too (tests).
    pub async fn drain_tasks(&self) {
        self.drain().await;
        self.core.tasks.close();
        self.core.tasks.wait().await;
        self.core.tasks.reopen();
    }

    pub fn stop(&self) {
        self.core.stop.cancel();
    }
}
