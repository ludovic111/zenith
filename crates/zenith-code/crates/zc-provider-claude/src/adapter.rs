//! `provider/Layers/ClaudeAdapter.ts` (`makeClaudeAdapter`): Claude sessions behind the generic
//! [`ProviderAdapter`] contract. One `claude` process per thread; the stream task maps its
//! messages through [`crate::mapping`]; API calls (turns, steering, model and plan-mode switches,
//! approvals, user input, interrupt, rewind) drive the same session state under one lock, so
//! events keep the order the TS adapter produced them in.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use futures::stream::BoxStream;
use futures::StreamExt;
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zc_contracts::{
    ApprovalRequestId, ChatAttachment, ClaudeSettings, ModelSelection, ProviderApprovalDecision, ProviderDriverKind, ProviderInstanceId,
    ProviderInteractionMode, ProviderRuntimeEvent, ProviderSendTurnInput, ProviderSession, ProviderSessionStartInput, ProviderTurnStartResult,
    ProviderUserInputAnswers, RuntimeMode, ThreadId, TurnId,
};
use zc_core::PubSub;
use zc_ports::adapter::{
    AdapterCapabilities, AdapterError, AdapterResult, Compaction, ProviderAdapter, SessionModelSwitch, ThreadSnapshot, ThreadTurnSnapshot,
};

use crate::catalog::{is_ultracode_effort, ClaudeModelCatalog};
use crate::cli_args::parse_cli_args;
use crate::history::{FileHistory, HistoryOps};
use crate::home::{make_claude_environment, resolve_claude_executable_path, resolve_claude_home_path, Env};
use crate::mapping::{
    classify_request_type, extract_exit_plan_mode_plan, is_interrupted_message, read_claude_resume_state, summarize_tool_request, Clock, EventJson, IdSource,
    MapperEnv, NativeEventSink, RandomIds, SessionInit, SessionRecord, SessionState, SystemClock, PROVIDER,
};
use crate::model_options::{
    apply_claude_prompt_effort_prefix, provider_option_descriptors, resolve_prompt_injected_effort, selection_bool_option, selection_string_option,
};
use crate::options::{ClaudeQueryOptions, SystemPrompt, ThinkingConfig};
use crate::query::{CanUseToolRequest, ClaudeQueryFactory, ClaudeQueryRuntime, PermissionResult, PromptSender, QueryCallbacks, UserDialogRequest};
use crate::skill_dispatch::plan_claude_skill_dispatch;
use crate::skills::discover_claude_skills;
use crate::usage_limits::{js_number, ScopedLimitNamesRef};

/// How long an interrupt waits for Claude to abort the turn before the process is closed.
pub const CLAUDE_INTERRUPT_GRACE: Duration = Duration::from_secs(3);

const SUPPORTED_IMAGE_MIME_TYPES: [&str; 4] = ["image/gif", "image/jpeg", "image/png", "image/webp"];
const SETTING_SOURCES: [&str; 3] = ["user", "project", "local"];
const RESUME_COMPACTION_NEVER_ANSWER: &str = "Don't ask again";
const PULL_REQUEST_LINKING_INSTRUCTIONS: &str = "<pull_request_linking>\nWhen the t3-code MCP server exposes link_pull_request, you must use it to register every pull request you create or work on for this thread. Call link_pull_request with the full PR URL immediately after creating a PR or starting work on an existing PR. For a stack, call it for every layer, not just the current branch or the top PR. This applies when creating or updating PRs through gh, gh stack, another CLI, or the host API: those operations do not register the PRs with this thread. Linking an already-linked PR is safe. Before finishing PR work, call list_thread_pull_requests and link any PR from your work that is missing. Do not link unrelated PRs mentioned only as background. If a linking call fails, report that failure instead of claiming the PR is linked.\n</pull_request_linking>";

/// `buildRuntimeInstructions({harness})` (branded): the session's appended system prompt.
pub fn build_runtime_instructions(harness: &str) -> String {
    let harness = harness.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "<runtime_info>In case you're asked: you are running in {} through the {harness} harness. No need to mention this otherwise. You can embed images and videos in your response using Markdown with absolute file paths.</runtime_info>\n\n{PULL_REQUEST_LINKING_INSTRUCTIONS}",
        crate::BRAND_NAME
    )
}

/// The MCP credentials the provider service issued for a thread (`McpProviderSessionConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpProviderSession {
    pub endpoint: String,
    pub authorization_header: String,
    /// Variables that put `agent-device` on PATH (`PATH`, `PATH_SEPARATOR`, …).
    pub agent_device_environment: Option<BTreeMap<String, String>>,
}

/// `McpProviderSession.readMcpProviderSession` (owned by the provider core / MCP registry).
pub trait McpSessionLookup: Send + Sync {
    fn read(&self, thread_id: &str) -> Option<McpProviderSession>;
}

/// `withAgentDeviceEnvironment(base, config)`.
pub fn with_agent_device_environment(base: &Env, session: Option<&McpProviderSession>) -> Env {
    let Some(extra) = session.and_then(|s| s.agent_device_environment.as_ref()) else {
        return base.clone();
    };
    let separator = extra.get("PATH_SEPARATOR").cloned().unwrap_or_else(|| ":".into());
    let base_path = base.get("PATH").or_else(|| base.get("Path")).cloned();
    let mut env = base.clone();
    for (key, value) in extra {
        if key != "PATH" && key != "PATH_SEPARATOR" {
            env.insert(key.clone(), value.clone());
        }
    }
    if let Some(shim) = extra.get("PATH").filter(|p| !p.is_empty()) {
        env.insert(
            "PATH".into(),
            base_path
                .filter(|p| !p.is_empty())
                .map(|p| format!("{shim}{separator}{p}"))
                .unwrap_or_else(|| shim.clone()),
        );
    }
    env
}

/// The catalog source (`options.modelCatalog`, the model manifest's current Claude models).
pub type CatalogSource = Arc<dyn Fn() -> ClaudeModelCatalog + Send + Sync>;

/// `ClaudeAdapterLiveOptions` + the per-instance settings.
#[derive(Clone)]
pub struct ClaudeAdapterOptions {
    pub settings: ClaudeSettings,
    pub instance_id: ProviderInstanceId,
    /// The provider instance's environment (`process.env` + instance overrides).
    pub environment: Env,
    pub catalog: CatalogSource,
    pub scoped_limit_names: ScopedLimitNamesRef,
    pub query_factory: Arc<dyn ClaudeQueryFactory>,
    /// The server's attachments directory (`ServerConfig.attachmentsDir`).
    pub attachments_dir: PathBuf,
    pub mcp_sessions: Option<Arc<dyn McpSessionLookup>>,
    /// zenith: also hand sessions the other lsuite apps' MCP servers (`zc_core::lsuite`).
    pub lsuite_mcp: bool,
    /// Defaults to the files under the instance's Claude config dir.
    pub history: Option<Arc<dyn HistoryOps>>,
    pub native_sink: Option<Arc<dyn NativeEventSink>>,
    pub ids: Arc<dyn IdSource>,
    pub clock: Arc<dyn Clock>,
    pub interrupt_grace: Duration,
}

impl ClaudeAdapterOptions {
    /// Defaults for an instance: the bundled catalog, system ids and clock, the real `claude`.
    pub fn new(settings: ClaudeSettings, instance_id: ProviderInstanceId, environment: Env, attachments_dir: PathBuf) -> Self {
        Self {
            settings,
            instance_id,
            environment,
            catalog: Arc::new(ClaudeModelCatalog::bundled),
            scoped_limit_names: crate::usage_limits::make_scoped_limit_names(),
            query_factory: Arc::new(crate::protocol::ProcessQueryFactory),
            attachments_dir,
            mcp_sessions: None,
            lsuite_mcp: false,
            history: None,
            native_sink: None,
            ids: Arc::new(RandomIds),
            clock: Arc::new(SystemClock),
            interrupt_grace: CLAUDE_INTERRUPT_GRACE,
        }
    }
}

struct PendingApproval {
    request_type: &'static str,
    tool_use_id: Option<String>,
    decision: oneshot::Sender<ProviderApprovalDecision>,
    /// The query's abort signal for this request (fires on `control_cancel_request` or close).
    abort: CancellationToken,
    /// Set when `stop_session_internal` already emitted the handler's `request.resolved`.
    settled_by_stop: Arc<AtomicBool>,
}

struct PendingUserInput {
    tool_use_id: Option<String>,
    /// `(answers, aborted)`.
    answers: oneshot::Sender<(Value, bool)>,
    settled_by_stop: Arc<AtomicBool>,
}

struct SessionCore {
    state: SessionState,
    approvals: IndexMap<String, PendingApproval>,
    user_inputs: IndexMap<String, PendingUserInput>,
    prompt: Option<PromptSender>,
    stream_task: Option<JoinHandle<()>>,
}

struct Session {
    core: Mutex<SessionCore>,
    runtime: Arc<dyn ClaudeQueryRuntime>,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Inner {
    options: ClaudeAdapterOptions,
    claude_env: Env,
    executable: String,
    history: Arc<dyn HistoryOps>,
    env: MapperEnv,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    events: PubSub<ProviderRuntimeEvent>,
}

/// The Claude [`ProviderAdapter`].
#[derive(Clone)]
pub struct ClaudeAdapter {
    inner: Arc<Inner>,
}

fn validation(operation: &str, issue: impl Into<String>) -> AdapterError {
    AdapterError::Validation {
        provider: PROVIDER.into(),
        operation: operation.into(),
        issue: issue.into(),
    }
}

fn request_error(method: &str, detail: impl Into<String>) -> AdapterError {
    AdapterError::Request {
        provider: PROVIDER.into(),
        method: method.into(),
        detail: detail.into(),
    }
}

/// `toRequestError(threadId, method, cause)`.
fn to_request_error(thread_id: &str, method: &str, cause: &str) -> AdapterError {
    let normalized = cause.to_lowercase();
    if normalized.contains("unknown session") || normalized.contains("not found") {
        return AdapterError::SessionNotFound {
            provider: PROVIDER.into(),
            thread_id: thread_id.into(),
        };
    }
    if normalized.contains("closed") {
        return AdapterError::SessionClosed {
            provider: PROVIDER.into(),
            thread_id: thread_id.into(),
        };
    }
    request_error(method, format!("{method} failed"))
}

/// `formatClaudeResumeCompactionQuestion`.
pub fn format_resume_compaction_question(age_minutes: u64, estimated_tokens: u64) -> String {
    let age = if age_minutes >= 60 {
        format!("{}h {}m", age_minutes / 60, age_minutes % 60)
    } else {
        format!("{age_minutes}m")
    };
    let digits = estimated_tokens.to_string();
    let mut grouped = String::new();
    for (index, c) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("This session is {age} old and uses {grouped} tokens. Compact it before continuing?")
}

/// `inferImageExtension({mimeType, fileName})`.
fn infer_image_extension(mime_type: &str, file_name: &str) -> String {
    let key = mime_type.to_lowercase();
    let from_mime = match key.as_str() {
        "image/avif" => Some(".avif"),
        "image/bmp" => Some(".bmp"),
        "image/gif" => Some(".gif"),
        "image/heic" => Some(".heic"),
        "image/heif" => Some(".heif"),
        "image/jpeg" | "image/jpg" => Some(".jpg"),
        "image/png" => Some(".png"),
        "image/svg+xml" => Some(".svg"),
        "image/tiff" => Some(".tiff"),
        "image/webp" => Some(".webp"),
        "image/x-icon" | "image/vnd.microsoft.icon" => Some(".ico"),
        _ => None,
    };
    if let Some(ext) = from_mime {
        return ext.to_string();
    }
    const SAFE: [&str; 12] = [
        ".avif", ".bmp", ".gif", ".heic", ".heif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".tiff", ".webp",
    ];
    let trimmed = file_name.trim();
    if let Some(dot) = trimmed.rfind('.') {
        let ext = &trimmed[dot + 1..];
        if (1..=8).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric()) {
            let ext = format!(".{}", ext.to_lowercase());
            if SAFE.contains(&ext.as_str()) {
                return ext;
            }
        }
    }
    ".bin".into()
}

/// `resolveAttachmentPath` for an image attachment.
pub fn resolve_image_attachment_path(attachments_dir: &Path, id: &str, mime_type: &str, name: &str) -> Option<PathBuf> {
    let relative = format!("{id}{}", infer_image_extension(mime_type, name));
    let normalized = zc_core::paths::normalize_lexically(Path::new(relative.trim_start_matches(['/', '\\'])));
    let normalized_text = normalized.to_string_lossy();
    if normalized_text.is_empty() || normalized_text.starts_with("..") || normalized_text.contains('\0') {
        return None;
    }
    let root = zc_core::paths::resolve_path(attachments_dir);
    let file = zc_core::paths::normalize_lexically(&root.join(&normalized));
    file.starts_with(&root).then_some(file).filter(|f| f != &root)
}

fn decision_str(decision: ProviderApprovalDecision) -> &'static str {
    decision.as_str()
}

impl ClaudeAdapter {
    /// `makeClaudeAdapter(settings, options)`.
    pub fn new(options: ClaudeAdapterOptions) -> Self {
        let claude_env = make_claude_environment(&options.settings.home_path, options.environment.clone());
        let executable = resolve_claude_executable_path(&options.settings.binary_path, &claude_env);
        let config_dir = resolve_claude_home_path(&options.settings.home_path, Some(&options.environment));
        let history = options.history.clone().unwrap_or_else(|| Arc::new(FileHistory { config_dir }));
        let env = MapperEnv {
            ids: options.ids.clone(),
            clock: options.clock.clone(),
            scoped_limit_names: options.scoped_limit_names.clone(),
            claude_config_dir: claude_env.get("CLAUDE_CONFIG_DIR").cloned(),
            native_sink: options.native_sink.clone(),
        };
        Self {
            inner: Arc::new(Inner {
                options,
                claude_env,
                executable,
                history,
                env,
                sessions: Mutex::default(),
                events: PubSub::new(),
            }),
        }
    }

    /// The environment sessions are spawned with (before MCP device variables).
    pub fn claude_environment(&self) -> &Env {
        &self.inner.claude_env
    }

    fn catalog(&self) -> ClaudeModelCatalog {
        let custom = serde_json::to_value(&self.inner.options.settings.custom_models).unwrap_or(Value::Null);
        (self.inner.options.catalog)().scoped(&custom)
    }

    /// The session of a thread, without holding the sessions lock afterwards (the session lock
    /// is always taken before the sessions lock, never inside it).
    fn session_arc(&self, thread_id: &str) -> Option<Arc<Session>> {
        lock(&self.inner.sessions).get(thread_id).cloned()
    }

    fn require_session(&self, thread_id: &str) -> AdapterResult<Arc<Session>> {
        let session = lock(&self.inner.sessions)
            .get(thread_id)
            .cloned()
            .ok_or_else(|| AdapterError::SessionNotFound {
                provider: PROVIDER.into(),
                thread_id: thread_id.into(),
            })?;
        let closed = {
            let core = lock(&session.core);
            core.state.stopped || core.state.session.status == "closed"
        };
        if closed {
            return Err(AdapterError::SessionClosed {
                provider: PROVIDER.into(),
                thread_id: thread_id.into(),
            });
        }
        Ok(session)
    }

    fn bound_selection(&self, selection: Option<&ModelSelection>) -> Option<ModelSelection> {
        selection.filter(|s| s.instance_id == self.inner.options.instance_id).cloned()
    }
}

impl Inner {
    fn publish(&self, events: Vec<EventJson>) {
        for event in events {
            match serde_json::from_value::<ProviderRuntimeEvent>(event.clone()) {
                Ok(decoded) => {
                    self.events.publish(decoded);
                }
                Err(error) => tracing::error!(%error, event_type = ?event.get("type"), "dropping a Claude runtime event that does not decode"),
            }
        }
    }

    /// `stopSessionInternal(context, {emitExitEvent})`.
    fn stop_session_internal(&self, session: &Arc<Session>, emit_exit_event: bool) -> AdapterResult<()> {
        let mut core = lock(&session.core);
        if core.state.stopped {
            return Ok(());
        }
        session.runtime.close().map_err(|_| AdapterError::Process {
            provider: PROVIDER.into(),
            thread_id: core.state.thread_id().to_string(),
            detail: "Failed to close Claude runtime query.".into(),
        })?;
        core.state.stopped = true;
        let mut out = Vec::new();
        core.state.settle_live_tasks(&self.env, &mut out);
        // Requests the close already aborted (the SDK's abort listener ran inside `close`) left
        // the pending map before this loop in TS; the rest are cancelled here with a row each.
        // Their waiting handlers then settle too: in TS they ran while Stop awaited the stream
        // fiber, so their rows land before `session.exited`, and are emitted here in that order.
        let mut settled_approvals = Vec::new();
        for (id, pending) in core.approvals.drain(..).collect::<Vec<_>>() {
            if !pending.abort.is_cancelled() {
                out.push(core.state.request_resolved(&self.env, &id, pending.request_type, "cancel", None, None));
            }
            pending.settled_by_stop.store(true, Ordering::SeqCst);
            let _ = pending.decision.send(ProviderApprovalDecision::Cancel);
            settled_approvals.push((id, pending.request_type, pending.tool_use_id));
        }
        let mut settled_inputs = Vec::new();
        for (id, pending) in core.user_inputs.drain(..).collect::<Vec<_>>() {
            pending.settled_by_stop.store(true, Ordering::SeqCst);
            let _ = pending.answers.send((json!({}), true));
            settled_inputs.push((id, pending.tool_use_id));
        }
        if core.state.turn_state.is_some() {
            core.state.complete_turn(&self.env, "interrupted", Some("Session stopped."), None, &mut out);
        }
        core.prompt.take();
        if let Some(task) = core.stream_task.take() {
            task.abort();
        }
        for (id, request_type, tool_use_id) in settled_approvals {
            let raw = json!({ "source": "claude.sdk.permission", "method": "canUseTool/decision", "payload": { "decision": "cancel" } });
            out.push(
                core.state
                    .request_resolved(&self.env, &id, request_type, "cancel", tool_use_id.as_deref(), Some(raw)),
            );
        }
        for (id, tool_use_id) in settled_inputs {
            out.push(core.state.user_input_resolved(&self.env, &id, &json!({}), tool_use_id.as_deref()));
        }
        core.state.session.status = "closed";
        core.state.session.active_turn_id = None;
        core.state.session.updated_at = self.env.clock.now_iso();
        let thread_id = core.state.thread_id().to_string();
        let is_current = lock(&self.sessions).get(&thread_id).is_some_and(|current| Arc::ptr_eq(current, session));
        if emit_exit_event && is_current {
            out.push(
                core.state
                    .session_event(&self.env, "session.exited", json!({ "reason": "Session stopped", "exitKind": "graceful" })),
            );
        }
        self.publish(out);
        drop(core);
        if is_current {
            lock(&self.sessions).remove(&thread_id);
        }
        Ok(())
    }
}

/// The session's `canUseTool` / `onUserDialog` handlers.
struct SessionCallbacks {
    adapter: Weak<Inner>,
    session: Arc<OnceLock<Weak<Session>>>,
    runtime_mode: RuntimeMode,
}

impl SessionCallbacks {
    fn context(&self) -> Option<(Arc<Inner>, Arc<Session>)> {
        Some((self.adapter.upgrade()?, self.session.get()?.upgrade()?))
    }

    /// `handleAskUserQuestion`.
    async fn ask_user_question(
        inner: &Arc<Inner>,
        session: &Arc<Session>,
        tool_input: &Value,
        tool_use_id: Option<&str>,
        cancel: CancellationToken,
    ) -> PermissionResult {
        let request_id = inner.env.ids.next_id();
        let questions: Vec<Value> = tool_input
            .get("questions")
            .and_then(Value::as_array)
            .map(|raw| {
                raw.iter()
                    .enumerate()
                    .map(|(index, q)| {
                        let question = q.get("question").and_then(Value::as_str);
                        let options: Vec<Value> = q
                            .get("options")
                            .and_then(Value::as_array)
                            .map(|options| {
                                options
                                    .iter()
                                    .map(|o| json!({ "label": o.get("label").and_then(Value::as_str).unwrap_or(""), "description": o.get("description").and_then(Value::as_str).unwrap_or("") }))
                                    .collect()
                            })
                            .unwrap_or_default();
                        json!({
                            "id": question.filter(|q| !q.is_empty()).map(str::to_string).unwrap_or_else(|| format!("q-{index}")),
                            "header": q.get("header").and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("Question {}", index + 1)),
                            "question": question.unwrap_or(""),
                            "options": options,
                            "multiSelect": q.get("multiSelect").and_then(Value::as_bool).unwrap_or(false),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (sender, mut receiver) = oneshot::channel::<(Value, bool)>();
        let settled_by_stop = Arc::new(AtomicBool::new(false));
        {
            let mut core = lock(&session.core);
            let event = core
                .state
                .user_input_requested(&inner.env, &request_id, &Value::Array(questions), tool_input, tool_use_id);
            inner.publish(vec![event]);
            core.user_inputs.insert(
                request_id.clone(),
                PendingUserInput {
                    tool_use_id: tool_use_id.map(str::to_string),
                    answers: sender,
                    settled_by_stop: settled_by_stop.clone(),
                },
            );
        }
        let settle_as_aborted = || {
            let mut core = lock(&session.core);
            if let Some(pending) = core.user_inputs.shift_remove(&request_id) {
                let _ = pending.answers.send((json!({}), true));
            }
        };
        if cancel.is_cancelled() {
            settle_as_aborted();
        }
        let (answers, aborted) = tokio::select! {
            result = &mut receiver => result.unwrap_or((json!({}), true)),
            _ = cancel.cancelled() => {
                settle_as_aborted();
                receiver.await.unwrap_or((json!({}), true))
            }
        };
        if !settled_by_stop.load(Ordering::SeqCst) {
            let mut core = lock(&session.core);
            core.user_inputs.shift_remove(&request_id);
            let event = core.state.user_input_resolved(&inner.env, &request_id, &answers, tool_use_id);
            inner.publish(vec![event]);
        }
        if aborted {
            return PermissionResult::Deny {
                message: "User cancelled tool execution.".into(),
            };
        }
        PermissionResult::Allow {
            updated_input: json!({ "questions": tool_input.get("questions").cloned().unwrap_or(Value::Null), "answers": answers }),
            updated_permissions: None,
        }
    }
}

/// `toSessionPermissionUpdates`: an "allow for this session" decision as session-scoped rules.
pub fn to_session_permission_updates(tool_name: &str, suggestions: Option<&[Value]>) -> Vec<Value> {
    let scoped: Vec<Value> = suggestions
        .unwrap_or_default()
        .iter()
        .map(|suggestion| {
            let mut next = suggestion.as_object().cloned().unwrap_or_default();
            next.insert("destination".into(), Value::String("session".into()));
            Value::Object(next)
        })
        .collect();
    if !scoped.is_empty() {
        return scoped;
    }
    vec![json!({ "type": "addRules", "rules": [{ "toolName": tool_name }], "behavior": "allow", "destination": "session" })]
}

#[async_trait]
impl QueryCallbacks for SessionCallbacks {
    async fn can_use_tool(&self, request: CanUseToolRequest, cancel: CancellationToken) -> PermissionResult {
        let Some((inner, session)) = self.context() else {
            return PermissionResult::Deny {
                message: "Claude session context is unavailable.".into(),
            };
        };
        let tool_use_id = request.tool_use_id.as_deref();
        if request.tool_name == "AskUserQuestion" {
            return Self::ask_user_question(&inner, &session, &request.input, tool_use_id, cancel).await;
        }
        if request.tool_name == "ExitPlanMode" {
            if let Some(plan) = extract_exit_plan_mode_plan(&request.input) {
                let mut core = lock(&session.core);
                let mut out = Vec::new();
                core.state.emit_proposed_plan_completed(
                    &inner.env,
                    &plan,
                    tool_use_id,
                    "claude.sdk.permission",
                    "canUseTool/ExitPlanMode",
                    json!({ "toolName": request.tool_name, "input": request.input }),
                    &mut out,
                );
                inner.publish(out);
            }
            return PermissionResult::Deny {
                message: "The client captured your proposed plan. Stop here and wait for the user's feedback or implementation request in a later turn.".into(),
            };
        }
        if self.runtime_mode == RuntimeMode::FullAccess {
            return PermissionResult::Allow {
                updated_input: request.input,
                updated_permissions: None,
            };
        }
        let request_id = inner.env.ids.next_id();
        let request_type = classify_request_type(&request.tool_name);
        let input_record = request.input.as_object().cloned().unwrap_or_default();
        let detail = summarize_tool_request(&request.tool_name, &input_record);
        let (sender, mut receiver) = oneshot::channel();
        let settled_by_stop = Arc::new(AtomicBool::new(false));
        {
            let mut core = lock(&session.core);
            let event = core
                .state
                .request_opened(&inner.env, &request_id, request_type, &detail, &request.tool_name, &request.input, tool_use_id);
            inner.publish(vec![event]);
            core.approvals.insert(
                request_id.clone(),
                PendingApproval {
                    request_type,
                    tool_use_id: tool_use_id.map(str::to_string),
                    decision: sender,
                    abort: cancel.clone(),
                    settled_by_stop: settled_by_stop.clone(),
                },
            );
        }
        let on_abort = || {
            let mut core = lock(&session.core);
            if let Some(pending) = core.approvals.shift_remove(&request_id) {
                let _ = pending.decision.send(ProviderApprovalDecision::Cancel);
            }
        };
        if cancel.is_cancelled() {
            on_abort();
        }
        let decision = tokio::select! {
            decision = &mut receiver => decision.unwrap_or(ProviderApprovalDecision::Cancel),
            _ = cancel.cancelled() => {
                on_abort();
                receiver.await.unwrap_or(ProviderApprovalDecision::Cancel)
            }
        };
        if !settled_by_stop.load(Ordering::SeqCst) {
            let mut core = lock(&session.core);
            core.approvals.shift_remove(&request_id);
            let raw = json!({ "source": "claude.sdk.permission", "method": "canUseTool/decision", "payload": { "decision": decision_str(decision) } });
            let event = core
                .state
                .request_resolved(&inner.env, &request_id, request_type, decision_str(decision), tool_use_id, Some(raw));
            inner.publish(vec![event]);
        }
        match decision {
            ProviderApprovalDecision::Accept => PermissionResult::Allow {
                updated_input: request.input,
                updated_permissions: None,
            },
            ProviderApprovalDecision::AcceptForSession => PermissionResult::Allow {
                updated_permissions: Some(to_session_permission_updates(&request.tool_name, request.suggestions.as_deref())),
                updated_input: request.input,
            },
            ProviderApprovalDecision::Cancel => PermissionResult::Deny {
                message: "User cancelled tool execution.".into(),
            },
            _ => PermissionResult::Deny {
                message: "User declined tool execution.".into(),
            },
        }
    }

    /// `handleResumeDialog`.
    async fn on_user_dialog(&self, request: UserDialogRequest, cancel: CancellationToken) -> Option<Value> {
        if request.dialog_kind != "resume_return" {
            return Some(json!({ "behavior": "cancelled" }));
        }
        let Some((inner, session)) = self.context() else {
            return Some(json!({ "behavior": "cancelled" }));
        };
        let number = |key: &str| {
            request
                .payload
                .get(key)
                .and_then(Value::as_f64)
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|v| v.round() as u64)
                .unwrap_or(0)
        };
        let question = format_resume_compaction_question(number("sessionAgeMinutes"), number("estimatedTokens"));
        let tool_input = json!({ "questions": [{
            "header": "Resume session",
            "question": question,
            "options": [
                { "label": "Compact and continue", "description": "Resume with a summary and use fewer tokens." },
                { "label": "Keep full history", "description": "Resume without changing the conversation." },
                { "label": RESUME_COMPACTION_NEVER_ANSWER, "description": "Keep full history and skip future resume prompts." }
            ],
            "multiSelect": false
        }]});
        let result = Self::ask_user_question(&inner, &session, &tool_input, request.tool_use_id.as_deref(), cancel).await;
        let PermissionResult::Allow { updated_input, .. } = result else {
            return Some(json!({ "behavior": "cancelled" }));
        };
        let selection = updated_input
            .get("answers")
            .and_then(Value::as_object)
            .and_then(|answers| answers.get(&question))
            .cloned();
        let action = match selection.as_ref().and_then(Value::as_str) {
            Some("Compact and continue") => "compact",
            Some(RESUME_COMPACTION_NEVER_ANSWER) => "never",
            _ => "continue",
        };
        Some(json!({ "behavior": "completed", "result": action }))
    }
}

/// Everything `startSession` derives from the input and the catalog.
struct LaunchPlan {
    options: ClaudeQueryOptions,
    model_selection: Option<ModelSelection>,
    api_model_id: Option<String>,
    effective_effort: Option<String>,
    permission_mode: Option<String>,
    fast_mode: bool,
    initial_context_window: Option<f64>,
}

impl ClaudeAdapter {
    fn plan_launch(&self, input: &ProviderSessionStartInput, catalog: &ClaudeModelCatalog, resume: Option<&str>, new_session_id: Option<&str>) -> LaunchPlan {
        let settings = &self.inner.options.settings;
        let mut flags = parse_cli_args(&settings.launch_args).flags;
        let launch_permission_mode = flags.shift_remove("permission-mode");
        let launch_skip_permissions = flags.shift_remove("dangerously-skip-permissions");
        let mut extra_args = flags;
        let model_selection = self.bound_selection(input.model_selection.as_ref()).map(|mut selection| {
            selection.model = catalog.resolve_slug(&selection.model);
            selection
        });
        let model = model_selection.as_ref().map(|s| s.model.as_str());
        let caps = catalog.capabilities(model);
        let descriptors = provider_option_descriptors(&caps, &[]);
        let has_boolean = |id: &str| {
            descriptors
                .iter()
                .any(|d| d.get("type").and_then(Value::as_str) == Some("boolean") && d.get("id").and_then(Value::as_str) == Some(id))
        };
        let api_model_id = model_selection.as_ref().map(|s| catalog.api_model_id(s));
        let initial_context_window = catalog.context_window_tokens(model_selection.as_ref());
        let raw_effort = selection_string_option(model_selection.as_ref(), "effort");
        let effort = catalog.resolve_effort(model, raw_effort.as_deref());
        let fast_mode = selection_bool_option(model_selection.as_ref(), "fastMode") == Some(true) && has_boolean("fastMode");
        let thinking = if has_boolean("thinking") {
            selection_bool_option(model_selection.as_ref(), "thinking")
        } else {
            None
        };
        let thinking_display = extra_args.get("thinking-display").and_then(Value::as_str).map(str::to_string);
        let request_summaries = thinking != Some(false) && thinking_display.as_deref() != Some("omitted");
        let ultracode = is_ultracode_effort(effort.as_deref());
        let effective_effort = catalog.normalize_effort(effort.as_deref(), model);
        let permission_mode = match launch_permission_mode {
            Some(Value::String(mode)) => Some(mode),
            _ => {
                if matches!(&launch_skip_permissions, Some(Value::Null)) || launch_skip_permissions.as_ref().and_then(Value::as_str) == Some("true") {
                    Some("bypassPermissions".to_string())
                } else {
                    match input.runtime_mode {
                        RuntimeMode::AutoAcceptEdits => Some("acceptEdits".into()),
                        RuntimeMode::Auto => Some("auto".into()),
                        RuntimeMode::FullAccess => Some("bypassPermissions".into()),
                        RuntimeMode::ApprovalRequired => None,
                    }
                }
            }
        };
        let mut session_settings = Map::new();
        if let Some(thinking) = thinking {
            session_settings.insert("alwaysThinkingEnabled".into(), Value::Bool(thinking));
        }
        if request_summaries {
            session_settings.insert("showThinkingSummaries".into(), Value::Bool(true));
        }
        if fast_mode {
            session_settings.insert("fastMode".into(), Value::Bool(true));
        }
        if ultracode {
            session_settings.insert("ultracode".into(), Value::Bool(true));
        }
        if !settings.auto_compact_window.is_empty() {
            let trimmed = settings.auto_compact_window.trim();
            let number = if trimmed.is_empty() {
                0.0
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN)
            };
            session_settings.insert("autoCompactWindow".into(), js_number(number));
        }
        if request_summaries && !extra_args.contains_key("thinking-display") {
            extra_args.insert("thinking-display".into(), Value::String("summarized".into()));
        }
        let mcp_session = self
            .inner
            .options
            .mcp_sessions
            .as_ref()
            .and_then(|lookup| lookup.read(input.thread_id.as_str()));
        let mut additional_directories = Vec::new();
        if let Some(cwd) = &input.cwd {
            additional_directories.push(cwd.clone());
        }
        additional_directories.push(self.inner.options.attachments_dir.to_string_lossy().into_owned());
        let mut mcp_servers = mcp_session.as_ref().map(|session| {
            let mut servers = Map::new();
            servers.insert(
                "t3-code".into(),
                json!({ "type": "http", "url": session.endpoint, "headers": { "Authorization": session.authorization_header } }),
            );
            servers
        });
        // zenith: the other lsuite apps' MCP servers (zc_core::lsuite).
        let lsuite = if self.inner.options.lsuite_mcp {
            zc_core::lsuite::mcp_servers()
        } else {
            Vec::new()
        };
        if !lsuite.is_empty() {
            let servers = mcp_servers.get_or_insert_with(Map::new);
            for server in lsuite {
                servers
                    .entry(server.name.clone())
                    .or_insert_with(|| json!({ "type": "stdio", "command": server.command, "args": server.args }));
            }
        }
        let thinking_config = (extra_args.get("thinking-display").and_then(Value::as_str) == Some("summarized")).then(|| ThinkingConfig::Adaptive {
            display: Some("summarized".into()),
        });
        let options = ClaudeQueryOptions {
            cwd: input.cwd.clone(),
            model: api_model_id.clone(),
            path_to_claude_code_executable: self.inner.executable.clone(),
            system_prompt: Some(SystemPrompt::Preset {
                append: Some(build_runtime_instructions("Claude Code")),
            }),
            setting_sources: Some(SETTING_SOURCES.iter().map(|s| s.to_string()).collect()),
            effort: effective_effort.clone(),
            thinking: thinking_config,
            permission_mode: permission_mode.clone(),
            allow_dangerously_skip_permissions: permission_mode.as_deref() == Some("bypassPermissions"),
            settings: (!session_settings.is_empty()).then_some(session_settings),
            resume: resume.map(str::to_string),
            session_id: new_session_id.map(str::to_string),
            include_partial_messages: true,
            can_use_tool: true,
            on_user_dialog: true,
            supported_dialog_kinds: Some(vec!["resume_return".into()]),
            env: with_agent_device_environment(&self.inner.claude_env, mcp_session.as_ref()),
            additional_directories,
            extra_args,
            mcp_servers,
            ..ClaudeQueryOptions::default()
        };
        LaunchPlan {
            options,
            model_selection,
            api_model_id,
            effective_effort,
            permission_mode,
            fast_mode,
            initial_context_window,
        }
    }

    /// `startSession(input)`.
    pub async fn start(&self, input: ProviderSessionStartInput) -> AdapterResult<ProviderSession> {
        let catalog = self.catalog();
        if let Some(provider) = &input.provider {
            if provider.as_str() != PROVIDER {
                return Err(validation("startSession", format!("Expected provider '{PROVIDER}' but received '{provider}'.")));
            }
        }
        let thread_id = input.thread_id.as_str().to_string();
        let existing = lock(&self.inner.sessions).get(&thread_id).cloned();
        if let Some(existing) = existing {
            tracing::warn!(thread_id, "claude.session.replacing: startSession called with existing active session");
            self.inner.stop_session_internal(&existing, false)?;
        }
        let env = &self.inner.env;
        let started_at = env.clock.now_iso();
        let resume_state = read_claude_resume_state(input.resume_cursor.as_ref());
        let existing_resume = resume_state.as_ref().and_then(|s| s.resume.clone());
        let new_session_id = if existing_resume.is_none() { Some(env.ids.next_id()) } else { None };
        let session_id = existing_resume.clone().or(new_session_id.clone());
        let plan = self.plan_launch(&input, &catalog, existing_resume.as_deref(), new_session_id.as_deref());
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        let session_slot: Arc<OnceLock<Weak<Session>>> = Arc::new(OnceLock::new());
        let callbacks = Arc::new(SessionCallbacks {
            adapter: Arc::downgrade(&self.inner),
            session: session_slot.clone(),
            runtime_mode: input.runtime_mode,
        });
        let handle = self
            .inner
            .options
            .query_factory
            .create(plan.options.clone(), prompt_rx, callbacks)
            .map_err(|_| AdapterError::Process {
                provider: PROVIDER.into(),
                thread_id: thread_id.clone(),
                detail: "Failed to start Claude runtime session.".into(),
            })?;

        let mut cursor = Map::new();
        cursor.insert("threadId".into(), Value::String(thread_id.clone()));
        if let Some(id) = &session_id {
            cursor.insert("resume".into(), Value::String(id.clone()));
        }
        if let Some(at) = resume_state.as_ref().and_then(|s| s.resume_session_at.clone()) {
            cursor.insert("resumeSessionAt".into(), Value::String(at));
        }
        cursor.insert("turnCount".into(), Value::from(resume_state.as_ref().and_then(|s| s.turn_count).unwrap_or(0)));
        if let Some(ids) = resume_state.as_ref().and_then(|s| s.turn_start_message_ids.clone()) {
            cursor.insert(
                "turnStartMessageIds".into(),
                Value::Array(ids.into_iter().map(|id| id.map(Value::String).unwrap_or(Value::Null)).collect()),
            );
        }
        let record = SessionRecord {
            thread_id: thread_id.clone(),
            provider_instance_id: self.inner.options.instance_id.as_str().to_string(),
            status: "ready",
            runtime_mode: input.runtime_mode,
            cwd: input.cwd.clone(),
            model: plan.model_selection.as_ref().map(|s| s.model.to_string()),
            resume_cursor: Some(Value::Object(cursor)),
            active_turn_id: None,
            created_at: started_at.clone(),
            updated_at: started_at,
            last_error: None,
        };
        let state = SessionState::new(SessionInit {
            session: record.clone(),
            start_input: input.clone(),
            resume_state,
            base_permission_mode: plan.permission_mode.clone(),
            current_api_model_id: plan.api_model_id.clone(),
            current_effort: plan.effective_effort.clone(),
            resume_session_id: session_id,
            initial_context_window: plan.initial_context_window,
        });
        let session = Arc::new(Session {
            core: Mutex::new(SessionCore {
                state,
                approvals: IndexMap::new(),
                user_inputs: IndexMap::new(),
                prompt: Some(prompt_tx),
                stream_task: None,
            }),
            runtime: handle.runtime.clone(),
        });
        let _ = session_slot.set(Arc::downgrade(&session));
        lock(&self.inner.sessions).insert(thread_id.clone(), session.clone());

        {
            let core = lock(&session.core);
            let mut out = Vec::new();
            let started_payload = match &input.resume_cursor {
                Some(cursor) => json!({ "resume": cursor }),
                None => json!({}),
            };
            out.push(core.state.session_event(env, "session.started", started_payload));
            let mut config = Map::new();
            if let Some(model) = &plan.api_model_id {
                config.insert("model".into(), Value::String(model.clone()));
            }
            if let Some(cwd) = &input.cwd {
                config.insert("cwd".into(), Value::String(cwd.clone()));
            }
            if let Some(effort) = &plan.effective_effort {
                config.insert("effort".into(), Value::String(effort.clone()));
            }
            if let Some(mode) = &plan.permission_mode {
                config.insert("permissionMode".into(), Value::String(mode.clone()));
            }
            if plan.fast_mode {
                config.insert("fastMode".into(), Value::Bool(true));
            }
            out.push(core.state.session_event(env, "session.configured", json!({ "config": config })));
            out.push(core.state.session_event(env, "session.state.changed", json!({ "state": "ready" })));
            self.inner.publish(out);
        }

        let task = tokio::spawn(run_stream(Arc::downgrade(&self.inner), Arc::downgrade(&session), handle.messages));
        lock(&session.core).stream_task = Some(task);
        Ok(record.to_provider_session())
    }

    /// `buildUserMessageEffect`: the `SDKUserMessage` for a turn.
    fn build_user_message(&self, input: &ProviderSendTurnInput, catalog: &ClaudeModelCatalog, cwd: Option<&str>) -> AdapterResult<Value> {
        let bound = self.bound_selection(input.model_selection.as_ref());
        let raw_effort = selection_string_option(bound.as_ref(), "effort");
        let caps = catalog.capabilities(bound.as_ref().map(|s| s.model.as_str()));
        let prompt_effort = resolve_prompt_injected_effort(&caps, raw_effort.as_deref());
        let text = apply_claude_prompt_effort_prefix(input.input.as_deref().unwrap_or("").trim(), prompt_effort.as_deref());
        let skills = discover_claude_skills(&self.inner.options.settings.home_path, cwd, &self.inner.claude_env);
        let skill_names: HashSet<String> = skills
            .into_iter()
            .filter(|s| s.enabled && s.user_invocable != Some(false))
            .map(|s| s.name)
            .collect();
        let dispatch = plan_claude_skill_dispatch(&text, &skill_names);
        let mut content: Vec<Value> = Vec::new();
        if let Some(leading) = dispatch.as_ref().and_then(|d| d.leading_text.clone()) {
            content.push(json!({ "type": "text", "text": leading }));
        }
        for attachment in input.attachments.iter().flatten() {
            let ChatAttachment::ChatImageAttachment(image) = attachment else { continue };
            if !SUPPORTED_IMAGE_MIME_TYPES.contains(&image.mime_type.as_str()) {
                return Err(request_error(
                    "turn/start",
                    format!("Unsupported Claude image attachment type '{}'.", image.mime_type),
                ));
            }
            let path = resolve_image_attachment_path(&self.inner.options.attachments_dir, &image.id, &image.mime_type, &image.name)
                .ok_or_else(|| request_error("turn/start", format!("Invalid attachment id '{}'.", image.id)))?;
            let bytes = std::fs::read(&path).map_err(|_| request_error("turn/start", "Failed to read attachment file."))?;
            content.push(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": image.mime_type, "data": base64::engine::general_purpose::STANDARD.encode(bytes) }
            }));
        }
        match dispatch {
            Some(dispatch) => content.push(json!({ "type": "text", "text": dispatch.command_text })),
            None if !text.is_empty() => content.push(json!({ "type": "text", "text": text })),
            None => {}
        }
        Ok(json!({ "type": "user", "session_id": "", "parent_tool_use_id": null, "message": { "role": "user", "content": content } }))
    }

    /// `sendTurn(input)`: a new turn, or a steer of the running one.
    pub async fn send(&self, input: ProviderSendTurnInput) -> AdapterResult<ProviderTurnStartResult> {
        let thread_id = input.thread_id.as_str().to_string();
        let session = self.require_session(&thread_id)?;
        let catalog = self.catalog();
        let env = &self.inner.env;
        let model_selection = self.bound_selection(input.model_selection.as_ref()).map(|mut selection| {
            selection.model = catalog.resolve_slug(&selection.model);
            selection
        });
        let (steering_turn, base_permission_mode, current_api) = {
            let mut core = lock(&session.core);
            if let Some(selection) = &model_selection {
                core.state.start_input.model_selection = Some(selection.clone());
            }
            let steering = core.state.turn_state.as_ref().filter(|turn| !turn.synthetic).map(|turn| turn.turn_id.clone());
            if core.state.turn_state.is_some() && steering.is_none() {
                let mut out = Vec::new();
                core.state.complete_turn(env, "completed", None, None, &mut out);
                self.inner.publish(out);
            }
            (steering, core.state.base_permission_mode.clone(), core.state.current_api_model_id.clone())
        };
        if let Some(selection) = &model_selection {
            let api_model_id = catalog.api_model_id(selection);
            if current_api.as_deref() != Some(api_model_id.as_str()) {
                session
                    .runtime
                    .set_model(Some(&api_model_id))
                    .await
                    .map_err(|e| to_request_error(&thread_id, "turn/setModel", &e.message))?;
            }
            let mut core = lock(&session.core);
            core.state.current_api_model_id = Some(api_model_id);
            core.state.session.model = Some(selection.model.to_string());
            let turn_effort = catalog.resolve_effort(Some(&selection.model), selection_string_option(Some(selection), "effort").as_deref());
            core.state.current_effort = catalog.normalize_effort(turn_effort.as_deref(), Some(&selection.model));
        }
        match input.interaction_mode {
            Some(ProviderInteractionMode::Plan) => session
                .runtime
                .set_permission_mode("plan")
                .await
                .map_err(|e| to_request_error(&thread_id, "turn/setPermissionMode", &e.message))?,
            Some(ProviderInteractionMode::Default) => session
                .runtime
                .set_permission_mode(base_permission_mode.as_deref().unwrap_or("default"))
                .await
                .map_err(|e| to_request_error(&thread_id, "turn/setPermissionMode", &e.message))?,
            None => {}
        }
        let turn_id = match &steering_turn {
            Some(turn_id) => turn_id.clone(),
            None => env.ids.next_id(),
        };
        let cwd = {
            let mut core = lock(&session.core);
            if steering_turn.is_none() {
                let mut out = Vec::new();
                core.state
                    .begin_turn(env, &turn_id, model_selection.as_ref().map(|s| s.model.as_str()), &mut out);
                self.inner.publish(out);
            }
            core.state.session.cwd.clone()
        };
        let mut message = self.build_user_message(&input, &catalog, cwd.as_deref())?;
        if steering_turn.is_none() {
            message["uuid"] = Value::String(turn_id.clone());
        }
        let mut core = lock(&session.core);
        if steering_turn.is_none() {
            core.state.turn_start_message_ids.push(Some(turn_id.clone()));
        }
        core.state.update_resume_cursor(env);
        let sent = core.prompt.as_ref().map(|prompt| prompt.send(message).is_ok()).unwrap_or(false);
        if !sent {
            return Err(to_request_error(&thread_id, "turn/start", "prompt queue closed"));
        }
        Ok(ProviderTurnStartResult {
            thread_id: input.thread_id.clone(),
            turn_id: TurnId::new(turn_id),
            resume_cursor: core.state.session.resume_cursor.clone(),
        })
    }

    /// `settleInterruptedTurn` + `stopSessionInternal`.
    pub async fn interrupt(&self, thread_id: &str) -> AdapterResult<()> {
        let session = self.require_session(thread_id)?;
        let receiver = {
            let mut core = lock(&session.core);
            if core.state.stopped || core.state.turn_state.is_none() || !session.runtime.supports_interrupt() {
                None
            } else {
                let (sender, receiver) = oneshot::channel();
                core.state.interrupted_turn_settled = Some(sender);
                Some(receiver)
            }
        };
        if let Some(receiver) = receiver {
            let runtime = session.runtime.clone();
            let _ = tokio::time::timeout(self.inner.options.interrupt_grace, async move {
                let _ = runtime.interrupt().await;
                let _ = receiver.await;
            })
            .await;
            lock(&session.core).state.interrupted_turn_settled = None;
        }
        self.inner.stop_session_internal(&session, true)
    }

    /// `rollbackThread(threadId, numTurns)`: rewind through a fork of Claude's own history.
    pub async fn rollback(&self, thread_id: &str, num_turns: u32) -> AdapterResult<ThreadSnapshot> {
        let session = self.require_session(thread_id)?;
        if num_turns < 1 {
            return Err(validation("rollbackThread", "numTurns must be an integer >= 1."));
        }
        let num_turns = num_turns as usize;
        let (boundaries, start_input, runtime_mode, resume_session_id, cwd, turns) = {
            let core = lock(&session.core);
            (
                core.state.turn_start_message_ids.clone(),
                core.state.start_input.clone(),
                core.state.session.runtime_mode,
                core.state.resume_session_id.clone(),
                core.state.session.cwd.clone(),
                core.state.turns.clone(),
            )
        };
        if !boundaries.is_empty() && boundaries.iter().all(Option::is_some) && num_turns >= boundaries.len() {
            self.inner.stop_session_internal(&session, false)?;
            Box::pin(self.start(ProviderSessionStartInput {
                runtime_mode,
                resume_cursor: None,
                ..start_input
            }))
            .await?;
            return self.snapshot(thread_id);
        }
        let Some(session_id) = resume_session_id else {
            return Err(request_error("thread/rollback", "Claude session id is unavailable."));
        };
        let history = self.inner.history.clone();
        let messages = history
            .get_session_messages(&session_id, cwd.as_deref())
            .map_err(|e| to_request_error(thread_id, "thread/rollback", &e.to_string()))?;
        let turn_starts: Vec<usize> = messages.iter().enumerate().filter(|(_, m)| is_human_turn_start(m)).map(|(i, _)| i).collect();
        if messages.is_empty() {
            return Err(request_error("thread/rollback", "Claude session history is unavailable."));
        }
        let mut boundaries = boundaries;
        if boundaries.iter().all(Option::is_none) && boundaries.len() == turn_starts.len() {
            boundaries = turn_starts
                .iter()
                .map(|i| messages[*i].get("uuid").and_then(Value::as_str).map(str::to_string))
                .collect();
        }
        let retained_count = boundaries.len().saturating_sub(num_turns);
        let first_removed_id = boundaries.get(retained_count).cloned().flatten();
        let first_removed = first_removed_id
            .as_deref()
            .and_then(|id| messages.iter().position(|m| m.get("uuid").and_then(Value::as_str) == Some(id)));
        if boundaries.is_empty() || boundaries.iter().any(Option::is_none) || (retained_count > 0 && first_removed.is_none_or(|i| i < 1)) {
            return Err(request_error(
                "thread/rollback",
                "The exact Claude turn boundary is unavailable, possibly after compaction or recovery of older history. Start a new thread instead.",
            ));
        }
        let rollback_at = if retained_count > 0 {
            first_removed
                .and_then(|i| messages.get(i - 1))
                .and_then(|m| m.get("uuid").and_then(Value::as_str))
                .map(str::to_string)
        } else {
            None
        };
        let retained_turns: Vec<String> = turns[..turns.len().saturating_sub(num_turns)].to_vec();
        let fork = match &rollback_at {
            Some(at) => Some(
                history
                    .fork_session(&session_id, cwd.as_deref(), at)
                    .map_err(|e| to_request_error(thread_id, "thread/rollback", &e.to_string()))?,
            ),
            None => None,
        };
        let mut retained_boundaries: Vec<Option<String>> = boundaries[..retained_count].to_vec();
        if let Some(fork_id) = &fork {
            let fork_messages = history
                .get_session_messages(fork_id, cwd.as_deref())
                .map_err(|e| to_request_error(thread_id, "thread/rollback", &e.to_string()))?;
            let remapped = remap_fork_turn_boundaries(&messages, &fork_messages, first_removed.unwrap_or(0), &retained_boundaries)
                .ok_or_else(|| request_error("thread/rollback", "Claude fork history did not preserve the retained turn boundaries."))?;
            retained_boundaries = remapped;
        }
        self.inner.stop_session_internal(&session, false)?;
        let resume_cursor = fork.map(|fork_id| {
            json!({
                "resume": fork_id,
                "turnCount": retained_count,
                "turnStartMessageIds": retained_boundaries.iter().map(|id| id.clone().map(Value::String).unwrap_or(Value::Null)).collect::<Vec<_>>(),
            })
        });
        Box::pin(self.start(ProviderSessionStartInput {
            runtime_mode,
            resume_cursor,
            ..start_input
        }))
        .await?;
        let restarted = self.require_session(thread_id)?;
        lock(&restarted.core).state.turns.extend(retained_turns);
        self.snapshot(thread_id)
    }

    fn snapshot(&self, thread_id: &str) -> AdapterResult<ThreadSnapshot> {
        let session = self.require_session(thread_id)?;
        let core = lock(&session.core);
        Ok(ThreadSnapshot {
            thread_id: ThreadId::new(core.state.thread_id()),
            turns: core
                .state
                .turns
                .iter()
                .map(|id| ThreadTurnSnapshot {
                    id: TurnId::new(id.clone()),
                    items: Vec::new(),
                })
                .collect(),
        })
    }

    /// The pending approval ids of a thread (tests and diagnostics).
    pub fn pending_approval_ids(&self, thread_id: &str) -> Vec<String> {
        self.session_arc(thread_id)
            .map(|s| lock(&s.core).approvals.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The pending user-input ids of a thread.
    pub fn pending_user_input_ids(&self, thread_id: &str) -> Vec<String> {
        self.session_arc(thread_id)
            .map(|s| lock(&s.core).user_inputs.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The session record (status, resume cursor, model, …) of a live thread.
    pub fn session_snapshot(&self, thread_id: &str) -> Option<ProviderSession> {
        self.session_arc(thread_id).map(|s| lock(&s.core).state.session.to_provider_session())
    }
}

/// `isClaudeHumanTurnStart`: a human prompt (not a tool result) starts a turn.
fn is_human_turn_start(message: &Value) -> bool {
    if message.get("type").and_then(Value::as_str) != Some("user") || !message.get("parent_tool_use_id").is_none_or(Value::is_null) {
        return false;
    }
    match message.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(_)) => true,
        Some(Value::Array(parts)) => parts
            .iter()
            .any(|part| part.is_object() && part.get("type").is_some_and(|t| t.as_str() != Some("tool_result"))),
        _ => false,
    }
}

fn is_conversation_message(message: &Value) -> bool {
    matches!(message.get("type").and_then(Value::as_str), Some("user" | "assistant"))
}

/// `remapClaudeForkTurnBoundaries`: forks rewrite every uuid; align the retained conversation
/// from the truncated end and map the turn starts onto the fork's ids.
pub fn remap_fork_turn_boundaries(
    messages: &[Value],
    fork_messages: &[Value],
    first_removed: usize,
    retained: &[Option<String>],
) -> Option<Vec<Option<String>>> {
    let retained_conversation: Vec<&Value> = messages[..first_removed.min(messages.len())]
        .iter()
        .filter(|m| is_conversation_message(m))
        .collect();
    let fork_conversation: Vec<&Value> = fork_messages.iter().filter(|m| is_conversation_message(m)).collect();
    if retained_conversation.is_empty() {
        return retained.iter().all(Option::is_none).then(|| retained.to_vec());
    }
    let offset = fork_conversation.len() as i64 - retained_conversation.len() as i64;
    if offset < 0
        || retained_conversation.iter().enumerate().any(|(index, message)| {
            fork_conversation
                .get(index + offset as usize)
                .is_none_or(|fork| fork.get("type") != message.get("type") || fork.get("message") != message.get("message"))
        })
    {
        return None;
    }
    let conversation_index = |uuid: &str| -> Option<usize> {
        let mut index: i64 = -1;
        for message in messages.iter().filter(|m| is_conversation_message(m)) {
            index += 1;
            if message.get("uuid").and_then(Value::as_str) == Some(uuid) {
                return Some(index as usize);
            }
        }
        None
    };
    let remapped: Vec<Option<String>> = retained
        .iter()
        .map(|original| {
            let original = original.as_deref()?;
            let index = conversation_index(original)?;
            let fork = fork_conversation.get(index + offset as usize)?;
            let source = messages.iter().find(|m| m.get("uuid").and_then(Value::as_str) == Some(original))?;
            (fork.get("type") == source.get("type"))
                .then(|| fork.get("uuid").and_then(Value::as_str).map(str::to_string))
                .flatten()
        })
        .collect();
    (!remapped.iter().any(Option::is_none)).then_some(remapped)
}

/// `runSdkStream` + `handleStreamExit`.
async fn run_stream(inner: Weak<Inner>, session: Weak<Session>, mut messages: crate::query::MessageReceiver) {
    let exit = loop {
        let item = messages.recv().await;
        let (Some(inner), Some(session)) = (inner.upgrade(), session.upgrade()) else {
            return;
        };
        match item {
            Some(Ok(message)) => {
                let mut core = lock(&session.core);
                if core.state.stopped {
                    return;
                }
                let mut out = Vec::new();
                core.state.handle_sdk_message(&inner.env, &message, &mut out);
                inner.publish(out);
            }
            Some(Err(error)) => break Err(error),
            None => break Ok(()),
        }
    };
    let (Some(inner), Some(session)) = (inner.upgrade(), session.upgrade()) else {
        return;
    };
    {
        let mut core = lock(&session.core);
        if core.state.stopped {
            return;
        }
        core.stream_task = None;
        let mut out = Vec::new();
        match exit {
            Err(error) if is_interrupted_message(&error.message) => {
                if core.state.turn_state.is_some() {
                    core.state
                        .complete_turn(&inner.env, "interrupted", Some("Claude runtime interrupted."), None, &mut out);
                }
            }
            Err(error) => {
                tracing::warn!(error = %error.message, "Claude runtime stream failed");
                let message = "Claude runtime stream failed.";
                out.push(core.state.runtime_error(
                    &inner.env,
                    message,
                    Some(json!({ "failureCount": 1, "failureTags": ["ProviderAdapterProcessError"] })),
                ));
                core.state.complete_turn(&inner.env, "failed", Some(message), None, &mut out);
            }
            Ok(()) => {
                if core.state.turn_state.is_some() {
                    core.state
                        .complete_turn(&inner.env, "interrupted", Some("Claude runtime stream ended."), None, &mut out);
                }
            }
        }
        inner.publish(out);
    }
    if let Err(error) = inner.stop_session_internal(&session, true) {
        tracing::error!(%error, "Failed to close Claude runtime stream.");
    }
}

#[async_trait]
impl ProviderAdapter for ClaudeAdapter {
    fn provider(&self) -> ProviderDriverKind {
        ProviderDriverKind::new(PROVIDER)
    }

    fn capabilities(&self) -> AdapterCapabilities {
        AdapterCapabilities {
            session_model_switch: SessionModelSwitch::InSession,
            promptless_turn_continuation: false,
            supports_conversation_rollback: true,
        }
    }

    fn compaction(&self) -> Option<Compaction> {
        Some(Compaction::SlashCommand("/compact"))
    }

    async fn start_session(&self, input: ProviderSessionStartInput) -> AdapterResult<ProviderSession> {
        self.start(input).await
    }

    async fn send_turn(&self, input: ProviderSendTurnInput) -> AdapterResult<ProviderTurnStartResult> {
        self.send(input).await
    }

    async fn interrupt_turn(&self, thread_id: &ThreadId, _turn_id: Option<&TurnId>) -> AdapterResult<()> {
        self.interrupt(thread_id.as_str()).await
    }

    async fn respond_to_request(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, decision: ProviderApprovalDecision) -> AdapterResult<()> {
        let session = self.require_session(thread_id.as_str())?;
        let mut core = lock(&session.core);
        let pending = core
            .approvals
            .shift_remove(request_id.as_str())
            .ok_or_else(|| request_error("item/requestApproval/decision", format!("Unknown pending approval request: {request_id}")))?;
        let _ = pending.decision.send(decision);
        Ok(())
    }

    async fn respond_to_user_input(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId, answers: ProviderUserInputAnswers) -> AdapterResult<()> {
        let session = self.require_session(thread_id.as_str())?;
        let mut core = lock(&session.core);
        let pending = core
            .user_inputs
            .shift_remove(request_id.as_str())
            .ok_or_else(|| request_error("item/tool/respondToUserInput", format!("Unknown pending user-input request: {request_id}")))?;
        let answers = Value::Object(answers.into_iter().collect());
        let _ = pending.answers.send((answers, false));
        Ok(())
    }

    async fn stop_session(&self, thread_id: &ThreadId) -> AdapterResult<()> {
        let session = self.require_session(thread_id.as_str())?;
        self.inner.stop_session_internal(&session, true)
    }

    async fn list_sessions(&self) -> Vec<ProviderSession> {
        let sessions: Vec<Arc<Session>> = lock(&self.inner.sessions).values().cloned().collect();
        sessions.iter().map(|s| lock(&s.core).state.session.to_provider_session()).collect()
    }

    async fn has_session(&self, thread_id: &ThreadId) -> bool {
        self.session_arc(thread_id.as_str()).is_some_and(|s| !lock(&s.core).state.stopped)
    }

    async fn read_thread(&self, thread_id: &ThreadId) -> AdapterResult<ThreadSnapshot> {
        self.snapshot(thread_id.as_str())
    }

    async fn rollback_thread(&self, thread_id: &ThreadId, num_turns: u32) -> AdapterResult<ThreadSnapshot> {
        self.rollback(thread_id.as_str(), num_turns).await
    }

    async fn stop_all(&self) -> AdapterResult<()> {
        let sessions: Vec<Arc<Session>> = lock(&self.inner.sessions).values().cloned().collect();
        let mut first_error = None;
        for session in sessions {
            if let Err(error) = self.inner.stop_session_internal(&session, true) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn subscribe_events(&self) -> BoxStream<'static, ProviderRuntimeEvent> {
        self.inner.events.subscribe().boxed()
    }
}

impl ClaudeAdapter {
    /// Close every session without `session.exited` (the TS layer finalizer).
    pub fn shutdown(&self) {
        let sessions: Vec<Arc<Session>> = lock(&self.inner.sessions).values().cloned().collect();
        for session in sessions {
            if let Err(error) = self.inner.stop_session_internal(&session, false) {
                tracing::error!(%error, "Failed to emit Claude session shutdown event.");
            }
        }
        self.inner.events.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_the_resume_compaction_question() {
        assert_eq!(
            format_resume_compaction_question(42, 1234567),
            "This session is 42m old and uses 1,234,567 tokens. Compact it before continuing?"
        );
        assert_eq!(
            format_resume_compaction_question(125, 999),
            "This session is 2h 5m old and uses 999 tokens. Compact it before continuing?"
        );
    }

    #[test]
    fn scopes_session_permission_updates() {
        assert_eq!(
            to_session_permission_updates("mcp__x", None),
            vec![json!({"type": "addRules", "rules": [{"toolName": "mcp__x"}], "behavior": "allow", "destination": "session"})]
        );
        let suggestion = json!({"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "ls"}], "behavior": "allow", "destination": "localSettings"});
        assert_eq!(to_session_permission_updates("Bash", Some(&[suggestion]))[0]["destination"], json!("session"));
    }

    #[test]
    fn resolves_image_attachment_paths_inside_the_attachments_dir() {
        let dir = Path::new("/tmp/attachments");
        assert_eq!(
            resolve_image_attachment_path(dir, "thread-a-1", "image/png", "x.png"),
            Some(PathBuf::from("/tmp/attachments/thread-a-1.png"))
        );
        assert_eq!(resolve_image_attachment_path(dir, "../escape", "image/png", "x.png"), None);
        assert_eq!(
            resolve_image_attachment_path(dir, "a", "image/x-unknown", "photo.JPEG"),
            Some(PathBuf::from("/tmp/attachments/a.jpeg"))
        );
    }

    #[test]
    fn layers_agent_device_variables_over_the_environment() {
        let base = Env::from([("PATH".to_string(), "/usr/bin".to_string()), ("HOME".to_string(), "/h".to_string())]);
        let session = McpProviderSession {
            endpoint: "http://127.0.0.1:1/mcp".into(),
            authorization_header: "Bearer t".into(),
            agent_device_environment: Some(BTreeMap::from([
                ("PATH".to_string(), "/shim".to_string()),
                ("AGENT_DEVICE_NO_UPDATE_NOTIFIER".to_string(), "1".to_string()),
            ])),
        };
        let env = with_agent_device_environment(&base, Some(&session));
        assert_eq!(env.get("PATH").map(String::as_str), Some("/shim:/usr/bin"));
        assert_eq!(env.get("AGENT_DEVICE_NO_UPDATE_NOTIFIER").map(String::as_str), Some("1"));
        assert_eq!(with_agent_device_environment(&base, None), base);
    }
}
