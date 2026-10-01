//! One Codex session: a `codex app-server` process speaking to one provider thread
//! (`provider/Layers/CodexSessionRuntime.ts`).
//!
//! It turns the app-server's notifications and requests into native [`ProviderEvent`]s (what the
//! adapter maps to canonical runtime events and the `NTIVE:` log lines record), parks approvals
//! and user-input prompts until the orchestration answers, tracks multi-agent (collab v2) child
//! threads and re-emits their traffic as synthetic `collabAgent/*` events, and runs turns,
//! interrupts, compaction, history reads and rollback.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use regex::Regex;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use zc_codex_protocol::{client_requests, AdditionalContextEntry, ServerNotification, ServerRequest};
use zc_contracts::{
    ApprovalRequestId, EventId, ProviderApprovalDecision, ProviderDriverKind, ProviderEvent, ProviderEventKind, ProviderInstanceId, ProviderInteractionMode,
    ProviderItemId, ProviderRequestKind, ProviderSession, ProviderSessionStatus, ProviderTurnStartResult, ProviderUserInputAnswers, RuntimeMode,
    ServerProviderModel, ThreadId, TurnId,
};

use crate::client::{decode_response, request, CodexRequester};
use crate::elicitation::to_mcp_elicitation_response;
use crate::errors::{CodexAppServerError, CodexSessionRuntimeError, ProtocolParseOperation, RequestError, RequestOperation};
use crate::instructions::{build_codex_additional_context, build_codex_developer_instructions, CodexRuntimeInfo, ToolAvailability};
use crate::launch_args::{codex_session_app_server_args, Environment};
use crate::model::{normalize_model_slug, DEFAULT_MODEL};
use crate::peer::{CodexPeer, IncomingHandler};
use crate::process::{spawn, ChildHandle, SpawnSpec, FORCE_KILL_AFTER};
use crate::thread_history::{
    open_codex_thread, read_codex_thread, rollback_codex_thread, runtime_mode_to_thread_config, runtime_mode_to_turn_sandbox_policy, CodexThreadSnapshot,
};

// ---------------------------------------------------------------------------------------------
// Options and the runtime interface

/// Supplies the provider's model list (for display names in the runtime context).
pub type ModelsSource = Arc<dyn Fn() -> BoxFuture<'static, Vec<ServerProviderModel>> + Send + Sync>;

/// `CodexResumeCursor`: `{threadId}`.
pub fn resume_cursor_thread_id(cursor: Option<&Value>) -> Option<String> {
    cursor?.get("threadId")?.as_str().map(str::to_owned)
}

/// `CodexSessionRuntimeOptions`.
#[derive(Clone)]
pub struct CodexSessionRuntimeOptions {
    pub thread_id: ThreadId,
    pub provider_instance_id: Option<ProviderInstanceId>,
    pub binary_path: String,
    pub home_path: Option<String>,
    pub launch_args: Option<String>,
    /// The child's whole environment; `None` inherits the server's.
    pub environment: Option<Environment>,
    pub cwd: String,
    pub runtime_mode: RuntimeMode,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    /// `{threadId}` of the provider thread to resume.
    pub resume_cursor: Option<Value>,
    /// Extra `app-server` arguments (the `t3-code` MCP `-c` flags).
    pub app_server_args: Option<Vec<String>>,
    pub models: Option<ModelsSource>,
    /// What the session's `t3-code` MCP credential grants ("preview", "device").
    pub mcp_capabilities: Option<BTreeSet<String>>,
}

impl std::fmt::Debug for CodexSessionRuntimeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexSessionRuntimeOptions")
            .field("thread_id", &self.thread_id)
            .field("provider_instance_id", &self.provider_instance_id)
            .field("binary_path", &self.binary_path)
            .field("home_path", &self.home_path)
            .field("launch_args", &self.launch_args)
            .field("environment", &self.environment.as_ref().map(|env| env.len()))
            .field("cwd", &self.cwd)
            .field("runtime_mode", &self.runtime_mode)
            .field("model", &self.model)
            .field("service_tier", &self.service_tier)
            .field("resume_cursor", &self.resume_cursor)
            .field("app_server_args", &self.app_server_args)
            .field("models", &self.models.is_some())
            .field("mcp_capabilities", &self.mcp_capabilities)
            .finish()
    }
}

impl CodexSessionRuntimeOptions {
    pub fn new(thread_id: ThreadId, binary_path: impl Into<String>, cwd: impl Into<String>, runtime_mode: RuntimeMode) -> Self {
        Self {
            thread_id,
            provider_instance_id: None,
            binary_path: binary_path.into(),
            home_path: None,
            launch_args: None,
            environment: None,
            cwd: cwd.into(),
            runtime_mode,
            model: None,
            service_tier: None,
            resume_cursor: None,
            app_server_args: None,
            models: None,
            mcp_capabilities: None,
        }
    }

    /// The `codex app-server` process this session starts (argv, cwd, environment).
    pub fn spawn_spec(&self) -> SpawnSpec {
        let mut env = self.environment.clone().unwrap_or_default();
        if let Some(home) = self.home_path.as_deref().filter(|home| !home.is_empty()) {
            // `~` is not shell-expanded in a spawned child's environment.
            env.insert("CODEX_HOME".into(), zc_core::expand_home_path(home).to_string_lossy().into_owned());
        }
        SpawnSpec {
            command: self.binary_path.clone(),
            args: codex_session_app_server_args(self.app_server_args.as_deref(), self.launch_args.as_deref()),
            cwd: self.cwd.clone(),
            env,
            extend_env: self.environment.is_none(),
        }
    }
}

/// `CodexSessionRuntimeSendTurnInput`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SendTurnInput {
    pub input: Option<String>,
    /// Image paths, sent as `localImage` inputs.
    pub attachments: Option<Vec<String>>,
    pub model: Option<String>,
    pub service_tier: Option<String>,
    pub effort: Option<String>,
    pub interaction_mode: Option<ProviderInteractionMode>,
}

/// What a session runtime offers the adapter (`CodexSessionRuntimeShape`); tests fake it.
#[async_trait]
pub trait CodexRuntime: Send + Sync {
    async fn start(&self) -> Result<ProviderSession, CodexSessionRuntimeError>;
    async fn get_session(&self) -> ProviderSession;
    async fn send_turn(&self, input: SendTurnInput) -> Result<ProviderTurnStartResult, CodexSessionRuntimeError>;
    async fn compact_thread(&self) -> Result<(), CodexSessionRuntimeError>;
    async fn interrupt_turn(&self, turn_id: Option<TurnId>) -> Result<(), CodexSessionRuntimeError>;
    async fn read_thread(&self) -> Result<CodexThreadSnapshot, CodexSessionRuntimeError>;
    async fn rollback_thread(&self, num_turns: usize) -> Result<CodexThreadSnapshot, CodexSessionRuntimeError>;
    /// `feedback/upload`; the response's `threadId`.
    async fn upload_feedback(&self, reason: Option<String>) -> Result<String, CodexSessionRuntimeError>;
    async fn respond_to_request(&self, request_id: &ApprovalRequestId, decision: ProviderApprovalDecision) -> Result<(), CodexSessionRuntimeError>;
    async fn respond_to_user_input(&self, request_id: &ApprovalRequestId, answers: ProviderUserInputAnswers) -> Result<(), CodexSessionRuntimeError>;
    /// The native event stream; one consumer (the first caller gets it).
    fn take_events(&self) -> Option<mpsc::UnboundedReceiver<ProviderEvent>>;
    async fn close(&self);
}

// ---------------------------------------------------------------------------------------------
// Pure helpers

/// `hasConfiguredMcpServer`.
pub fn has_configured_mcp_server(app_server_args: Option<&[String]>) -> bool {
    app_server_args.is_some_and(|args| args.iter().any(|arg| arg.contains("mcp_servers.")))
}

/// `configuredMcpToolAvailability`: the toolkits the turn's prompt may describe.
pub fn configured_mcp_tool_availability(app_server_args: Option<&[String]>, capabilities: Option<&BTreeSet<String>>) -> ToolAvailability {
    if !has_configured_mcp_server(app_server_args) {
        return ToolAvailability::default();
    }
    match capabilities {
        // Callers predating the capability set attached the browser toolkit only.
        None => ToolAvailability::browser_only(true),
        Some(capabilities) => ToolAvailability {
            browser: capabilities.contains("preview"),
            device: capabilities.contains("device"),
        },
    }
}

fn is_currency_symbol(char: char) -> bool {
    static REGEX: OnceLock<Regex> = OnceLock::new();
    let regex = REGEX.get_or_init(|| Regex::new(r"^\p{Sc}$").expect("valid regex"));
    let mut buffer = [0u8; 4];
    regex.is_match(char.encode_utf8(&mut buffer))
}

fn is_skill_char(char: char) -> bool {
    char.is_ascii_alphanumeric() || matches!(char, ':' | '_' | '-')
}

fn is_js_space(char: char) -> bool {
    char.is_whitespace() || char == '\u{feff}'
}

/// `SKILL_MENTION_PATTERN` replaced by `$1$$$2`: a currency-symbol skill alias (`€review`) in
/// Codex's canonical `$review` form, leaving amounts (`€20`, `€20k`, `€1e6`) as prose.
pub fn rewrite_skill_mentions(prompt: &str) -> String {
    let chars: Vec<char> = prompt.chars().collect();
    let mut out = String::with_capacity(prompt.len());
    let mut index = 0;
    while index < chars.len() {
        let char = chars[index];
        let at_boundary = index == 0 || is_js_space(chars[index - 1]);
        if at_boundary && is_currency_symbol(char) {
            let rest = &chars[index + 1..];
            let run_len = rest.iter().take_while(|c| is_skill_char(**c)).count();
            let run = &rest[..run_len];
            let ends_at_boundary = rest.get(run_len).is_none_or(|next| is_js_space(*next));
            if run_len > 0 && run[0].is_ascii_alphanumeric() && run.iter().any(char::is_ascii_alphabetic) && ends_at_boundary && !looks_like_amount(rest) {
                out.push('$');
                out.extend(run);
                index += 1 + run_len;
                continue;
            }
        }
        out.push(char);
        index += 1;
    }
    out
}

/// The negative lookahead `[0-9][0-9_]*(?:[kKmMbBtT]|[eE][0-9]+)?(?:\s|$)`.
fn looks_like_amount(rest: &[char]) -> bool {
    if !rest.first().is_some_and(char::is_ascii_digit) {
        return false;
    }
    let digits = rest.iter().take_while(|c| c.is_ascii_digit() || **c == '_').count();
    let after = &rest[digits..];
    let boundary = |slice: &[char]| slice.first().is_none_or(|c| is_js_space(*c));
    if boundary(after) {
        return true;
    }
    match after.first() {
        Some(c) if "kKmMbBtT".contains(*c) && boundary(&after[1..]) => true,
        Some('e' | 'E') => {
            let exponent = after[1..].iter().take_while(|c| c.is_ascii_digit()).count();
            exponent > 0 && boundary(&after[1 + exponent..])
        }
        _ => false,
    }
}

/// `normalizeCodexModelSlug(model)`.
fn normalize_codex_model_slug(model: Option<&str>) -> Option<String> {
    normalize_model_slug(model)
}

/// Inputs of [`build_turn_start_params`].
#[derive(Debug, Clone, Default)]
pub struct TurnStartParamsInput {
    pub thread_id: String,
    pub runtime_mode: Option<RuntimeMode>,
    pub prompt: Option<String>,
    pub attachments: Vec<String>,
    pub model: Option<String>,
    pub model_name: Option<String>,
    pub service_tier: Option<String>,
    pub effort: Option<String>,
    pub interaction_mode: Option<ProviderInteractionMode>,
    /// `None` = the browser toolkit only (callers predating the agent-access gate).
    pub tools: Option<ToolAvailability>,
}

/// `buildTurnStartParams`: the `turn/start` params, with the experimental `collaborationMode`
/// and `additionalContext` when an interaction mode is given.
pub fn build_turn_start_params(input: &TurnStartParamsInput) -> Value {
    let mode = input.runtime_mode.unwrap_or(RuntimeMode::FullAccess);
    let config = runtime_mode_to_thread_config(mode);
    let mut turn_input = Vec::new();
    if let Some(prompt) = input.prompt.as_deref().filter(|prompt| !prompt.is_empty()) {
        turn_input.push(json!({ "type": "text", "text": rewrite_skill_mentions(prompt) }));
    }
    for path in &input.attachments {
        turn_input.push(json!({ "type": "localImage", "path": path }));
    }
    let mut params = Map::new();
    params.insert("threadId".into(), json!(input.thread_id));
    params.insert("input".into(), Value::Array(turn_input));
    params.insert("approvalPolicy".into(), json!(config.approval_policy));
    params.insert("approvalsReviewer".into(), json!(config.approvals_reviewer));
    params.insert("sandboxPolicy".into(), runtime_mode_to_turn_sandbox_policy(mode));
    if let Some(model) = &input.model {
        params.insert("model".into(), json!(model));
    }
    if let Some(tier) = &input.service_tier {
        params.insert("serviceTier".into(), json!(tier));
    }
    if let Some(effort) = &input.effort {
        params.insert("effort".into(), json!(effort));
    }
    if let Some(interaction_mode) = input.interaction_mode {
        let model = normalize_codex_model_slug(input.model.as_deref()).unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        let reasoning_effort = input.effort.clone().unwrap_or_else(|| "medium".to_owned());
        params.insert(
            "collaborationMode".into(),
            json!({
                "mode": interaction_mode.as_str(),
                "settings": {
                    "model": model,
                    "reasoning_effort": reasoning_effort,
                    "developer_instructions": build_codex_developer_instructions(interaction_mode),
                }
            }),
        );
        let context = build_codex_additional_context(
            &CodexRuntimeInfo {
                model,
                model_name: input.model_name.clone(),
                reasoning_effort,
            },
            input.tools.unwrap_or(ToolAvailability::browser_only(true)),
        );
        params.insert("additionalContext".into(), serde_json::to_value(context).expect("context encodes"));
    }
    Value::Object(params)
}

const BENIGN_ERROR_LOG_SNIPPETS: &[&str] = &[
    "state db missing rollout path for thread",
    "state db record_discrepancy: find_thread_path_by_id_str_in_subdir, falling_back",
];

/// `classifyCodexStderrLine`: the stderr lines worth surfacing (non-log lines, and `ERROR` log
/// lines that are not known noise).
pub fn classify_codex_stderr_line(raw: &str) -> Option<String> {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    static LOG: OnceLock<Regex> = OnceLock::new();
    let ansi = ANSI.get_or_init(|| Regex::new("\u{1b}\\[[0-9;]*m").expect("valid regex"));
    let log = LOG.get_or_init(|| Regex::new(r"^\d{4}-\d{2}-\d{2}T\S+\s+(TRACE|DEBUG|INFO|WARN|ERROR)\s+\S+:\s+(.*)$").expect("valid regex"));
    let line = ansi.replace_all(raw, "");
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if let Some(captures) = log.captures(line) {
        if captures.get(1).is_some_and(|level| level.as_str() != "ERROR") {
            return None;
        }
        if BENIGN_ERROR_LOG_SNIPPETS.iter().any(|snippet| line.contains(snippet)) {
            return None;
        }
    }
    Some(line.to_owned())
}

/// How a notification addressed to a registered child thread is handled
/// (`routeCodexChildNotification`): mapped to a synthetic `collabAgent/*` event, passed to the
/// parent path, or dropped as child chatter. Unknown methods go to the parent so new wire
/// methods surface instead of vanishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildNotificationRoute {
    AgentEvent,
    Parent,
    Drop,
}

const CHILD_AGENT_EVENT_METHODS: &[&str] = &[
    "turn/started",
    "turn/completed",
    "thread/status/changed",
    "thread/tokenUsage/updated",
    "thread/settings/updated",
    "model/rerouted",
    "item/started",
    "item/completed",
    "thread/closed",
    "error",
];

const CHILD_CHATTER_METHODS: &[&str] = &[
    "item/agentMessage/delta",
    "item/reasoning/textDelta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "item/plan/delta",
    "turn/plan/updated",
    "turn/diff/updated",
    "thread/name/updated",
    "rawResponseItem/completed",
    "thread/archived",
    "thread/unarchived",
    "thread/compacted",
    "thread/started",
];

pub fn route_codex_child_notification(method: &str) -> ChildNotificationRoute {
    if CHILD_AGENT_EVENT_METHODS.contains(&method) {
        ChildNotificationRoute::AgentEvent
    } else if CHILD_CHATTER_METHODS.contains(&method) {
        ChildNotificationRoute::Drop
    } else {
        ChildNotificationRoute::Parent
    }
}

fn should_suppress_child_conversation_notification(method: &str) -> bool {
    matches!(
        method,
        "thread/started"
            | "thread/status/changed"
            | "thread/archived"
            | "thread/unarchived"
            | "thread/closed"
            | "thread/compacted"
            | "thread/name/updated"
            | "thread/settings/updated"
            | "thread/tokenUsage/updated"
            | "model/rerouted"
            | "turn/started"
            | "turn/completed"
            | "turn/plan/updated"
            | "item/plan/delta"
    )
}

const THREAD_ID_METHODS: &[&str] = &[
    "error",
    "thread/status/changed",
    "thread/archived",
    "thread/unarchived",
    "thread/closed",
    "thread/name/updated",
    "thread/settings/updated",
    "thread/tokenUsage/updated",
    "model/rerouted",
    "turn/started",
    "hook/started",
    "turn/completed",
    "hook/completed",
    "turn/diff/updated",
    "turn/plan/updated",
    "item/started",
    "item/autoApprovalReview/started",
    "item/autoApprovalReview/completed",
    "item/completed",
    "rawResponseItem/completed",
    "item/agentMessage/delta",
    "item/plan/delta",
    "item/commandExecution/outputDelta",
    "item/commandExecution/terminalInteraction",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "serverRequest/resolved",
    "item/mcpToolCall/progress",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
    "thread/compacted",
    "thread/realtime/started",
    "thread/realtime/itemAdded",
    "thread/realtime/transcript/delta",
    "thread/realtime/transcript/done",
    "thread/realtime/outputAudio/delta",
    "thread/realtime/sdp",
    "thread/realtime/error",
    "thread/realtime/closed",
];

fn str_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut current = value;
    for key in path {
        current = current.get(key)?;
    }
    current.as_str()
}

/// `readNotificationThreadId`: the provider thread a notification is about.
pub fn read_notification_thread_id(method: &str, params: &Value) -> Option<String> {
    if method == "thread/started" {
        return str_at(params, &["thread", "id"]).map(str::to_owned);
    }
    if THREAD_ID_METHODS.contains(&method) {
        return str_at(params, &["threadId"]).map(str::to_owned);
    }
    None
}

/// `readRouteFields`: the turn and item a notification belongs to.
pub fn read_route_fields(method: &str, params: &Value) -> (Option<String>, Option<String>) {
    let non_empty = |value: Option<&str>| value.filter(|value| !value.is_empty()).map(str::to_owned);
    match method {
        "turn/started" | "turn/completed" => (non_empty(str_at(params, &["turn", "id"])), None),
        "error" | "turn/diff/updated" | "turn/plan/updated" => (non_empty(str_at(params, &["turnId"])), None),
        "item/started" | "item/completed" => (non_empty(str_at(params, &["turnId"])), non_empty(str_at(params, &["item", "id"]))),
        "item/agentMessage/delta"
        | "item/plan/delta"
        | "item/commandExecution/outputDelta"
        | "item/commandExecution/terminalInteraction"
        | "item/fileChange/outputDelta"
        | "item/fileChange/patchUpdated"
        | "item/reasoning/summaryTextDelta"
        | "item/reasoning/summaryPartAdded"
        | "item/reasoning/textDelta" => (non_empty(str_at(params, &["turnId"])), non_empty(str_at(params, &["itemId"]))),
        _ => (None, None),
    }
}

/// `makeMemoryConsolidationNotificationFilter`: hides Codex's internal memory-consolidation
/// sub-agent without hiding other sub-agents.
#[derive(Debug, Default)]
pub struct MemoryConsolidationFilter {
    thread_ids: BTreeSet<String>,
}

impl MemoryConsolidationFilter {
    pub fn should_suppress(&mut self, method: &str, params: &Value) -> bool {
        if method == "thread/started" {
            let thread = &params["thread"];
            let by_source = thread["threadSource"].as_str() == Some("memory_consolidation")
                || thread["source"].get("subAgent").and_then(Value::as_str) == Some("memory_consolidation");
            if by_source {
                if let Some(id) = thread["id"].as_str() {
                    self.thread_ids.insert(id.to_owned());
                }
                return true;
            }
        }
        let thread_id = if method == "thread/started" {
            params["thread"]["id"].as_str()
        } else {
            params.get("threadId").and_then(Value::as_str)
        };
        let Some(thread_id) = thread_id.filter(|id| !id.is_empty() && self.thread_ids.contains(*id)) else {
            return false;
        };
        if method == "serverRequest/resolved" {
            return false;
        }
        if method == "thread/closed" {
            let thread_id = thread_id.to_owned();
            self.thread_ids.remove(&thread_id);
        }
        true
    }
}

/// `toCodexUserInputAnswers`: each answer as `{answers: [...]}`.
pub fn to_codex_user_input_answers(answers: &ProviderUserInputAnswers) -> Result<Value, CodexSessionRuntimeError> {
    let mut out = Map::new();
    for (question_id, value) in answers {
        let list = match value {
            Value::String(answer) => vec![Value::String(answer.clone())],
            Value::Array(entries) => entries.iter().filter(|entry| entry.is_string()).cloned().collect(),
            Value::Object(object) => match object.get("answers") {
                Some(Value::Array(entries)) if entries.iter().all(Value::is_string) => entries.clone(),
                _ => {
                    return Err(CodexSessionRuntimeError::InvalidUserInputAnswers {
                        question_id: question_id.clone(),
                    })
                }
            },
            _ => {
                return Err(CodexSessionRuntimeError::InvalidUserInputAnswers {
                    question_id: question_id.clone(),
                })
            }
        };
        out.insert(question_id.clone(), json!({ "answers": list }));
    }
    Ok(Value::Object(out))
}

/// `buildCodexInitializeParams`.
pub fn build_codex_initialize_params() -> Value {
    json!({
        "clientInfo": { "name": crate::BRAND_NAME, "title": crate::BRAND_NAME, "version": SERVER_VERSION },
        "capabilities": { "experimentalApi": true }
    })
}

/// The server package version (`apps/server/package.json`), sent as `clientInfo.version`.
pub const SERVER_VERSION: &str = "0.0.43";

// ---------------------------------------------------------------------------------------------
// The live runtime

#[derive(Debug, Clone)]
struct CollabChild {
    agent_thread_id: String,
    nickname: Option<String>,
    role: Option<String>,
    agent_path: Option<String>,
    depth: Option<Value>,
    parent_thread_id: Option<String>,
    spawn_turn_id: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct CollabMetadata {
    model: Option<String>,
    effort: Option<String>,
    lookup_started: bool,
    closed: bool,
}

fn collab_identity(child: &CollabChild, metadata: Option<&CollabMetadata>) -> Map<String, Value> {
    let mut identity = Map::new();
    identity.insert("agentThreadId".into(), json!(child.agent_thread_id));
    let mut put = |key: &str, value: Option<&String>| {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
            identity.insert(key.into(), json!(value));
        }
    };
    put("nickname", child.nickname.as_ref());
    put("role", child.role.as_ref());
    put("agentPath", child.agent_path.as_ref());
    put("model", metadata.and_then(|metadata| metadata.model.as_ref()));
    put("effort", metadata.and_then(|metadata| metadata.effort.as_ref()));
    identity
}

fn non_empty_metadata(value: Option<&Value>) -> Option<String> {
    value?.as_str().map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

struct PendingApproval {
    request_id: ApprovalRequestId,
    request_kind: ProviderRequestKind,
    turn_id: Option<String>,
    item_id: Option<String>,
    decision: Option<oneshot::Sender<ProviderApprovalDecision>>,
}

#[derive(Clone)]
struct ApprovalCorrelation {
    request_id: ApprovalRequestId,
    request_kind: ProviderRequestKind,
    turn_id: Option<String>,
    item_id: Option<String>,
}

struct PendingUserInput {
    request_id: ApprovalRequestId,
    turn_id: Option<String>,
    item_id: Option<String>,
    answers: Option<oneshot::Sender<ProviderUserInputAnswers>>,
}

struct State {
    session: ProviderSession,
    pending_approvals: HashMap<String, PendingApproval>,
    approval_correlations: HashMap<String, ApprovalCorrelation>,
    pending_user_inputs: HashMap<String, PendingUserInput>,
    collab_receiver_turns: HashMap<String, String>,
    collab_children: HashMap<String, CollabChild>,
    collab_metadata: HashMap<String, CollabMetadata>,
    collab_live_turns: BTreeMap<String, String>,
    memory_filter: MemoryConsolidationFilter,
    last_additional_context: Option<BTreeMap<String, AdditionalContextEntry>>,
}

struct Shared {
    options: CodexSessionRuntimeOptions,
    state: Mutex<State>,
    events: Mutex<Option<mpsc::UnboundedSender<ProviderEvent>>>,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<ProviderEvent>>>,
    notifications: mpsc::UnboundedSender<(String, Value)>,
    peer: OnceLock<CodexPeer>,
    closed: AtomicBool,
    tasks: Mutex<Vec<AbortHandle>>,
}

/// The live runtime over a `codex app-server` child.
pub struct CodexSessionRuntime {
    shared: Arc<Shared>,
    child: ChildHandle,
}

fn now_iso() -> String {
    zc_core::now_iso()
}

impl Shared {
    fn peer(&self) -> &CodexPeer {
        self.peer.get().expect("the peer starts with the runtime")
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("codex runtime state lock")
    }

    fn provider_thread_id(&self) -> Option<String> {
        resume_cursor_thread_id(self.lock().session.resume_cursor.as_ref())
    }

    /// `emitEvent`.
    fn emit(&self, mut event: ProviderEvent) {
        event.id = EventId::new(zc_core::uuid_v4());
        event.provider = ProviderDriverKind::new(crate::DRIVER_KIND);
        event.provider_instance_id = self.options.provider_instance_id.clone();
        event.created_at = now_iso();
        if let Some(sender) = self.events.lock().expect("codex events lock").as_ref() {
            let _ = sender.send(event);
        }
    }

    fn event(&self, kind: ProviderEventKind, method: &str) -> ProviderEvent {
        ProviderEvent {
            id: EventId::new(String::new()),
            kind,
            provider: ProviderDriverKind::new(crate::DRIVER_KIND),
            provider_instance_id: None,
            thread_id: self.options.thread_id.clone(),
            created_at: String::new(),
            method: method.to_owned(),
            message: None,
            turn_id: None,
            item_id: None,
            request_id: None,
            request_kind: None,
            text_delta: None,
            payload: None,
        }
    }

    fn emit_session_event(&self, method: &str, message: &str) {
        let mut event = self.event(ProviderEventKind::Session, method);
        event.message = Some(message.to_owned());
        self.emit(event);
    }

    fn emit_collab(&self, method: &str, spawn_turn_id: Option<&String>, payload: Map<String, Value>) {
        let mut event = self.event(ProviderEventKind::Notification, method);
        event.turn_id = spawn_turn_id.filter(|turn| !turn.is_empty()).map(|turn| TurnId::new(turn.clone()));
        event.payload = Some(Value::Object(payload));
        self.emit(event);
    }

    fn update_session(&self, update: impl FnOnce(&mut ProviderSession)) {
        let mut state = self.lock();
        update(&mut state.session);
        state.session.updated_at = now_iso();
    }

    fn settle_pending_approvals(&self, decision: ProviderApprovalDecision) {
        let mut state = self.lock();
        for pending in state.pending_approvals.values_mut() {
            if let Some(sender) = pending.decision.take() {
                let _ = sender.send(decision);
            }
        }
    }

    fn settle_pending_user_inputs(&self) {
        let mut state = self.lock();
        for pending in state.pending_user_inputs.values_mut() {
            if let Some(sender) = pending.answers.take() {
                let _ = sender.send(ProviderUserInputAnswers::new());
            }
        }
    }

    // -- server notifications --------------------------------------------------------------

    /// The per-method handlers registered before the generic forwarders (they run first, on
    /// the reader).
    fn apply_session_handlers(&self, notification: &ServerNotification) {
        let provider_thread_id = self.provider_thread_id();
        let other_thread = |thread_id: &str| provider_thread_id.as_deref().is_some_and(|current| current != thread_id);
        match notification {
            ServerNotification::ThreadStarted(params) => {
                if other_thread(&params.thread.id) {
                    return;
                }
                let thread_id = params.thread.id.clone();
                self.update_session(|session| session.resume_cursor = Some(json!({ "threadId": thread_id })));
            }
            ServerNotification::TurnStarted(params) => {
                if other_thread(&params.thread_id) {
                    return;
                }
                let turn_id = params.turn.id.clone();
                self.update_session(|session| {
                    session.status = ProviderSessionStatus::Running;
                    session.active_turn_id = Some(TurnId::new(turn_id));
                });
            }
            ServerNotification::TurnCompleted(params) => {
                if other_thread(&params.thread_id) {
                    return;
                }
                let failed = params.turn.status == zc_codex_protocol::TurnStatus::Failed;
                let last_error = failed
                    .then(|| params.turn.error.clone().flatten().map(|error| error.message))
                    .flatten()
                    .filter(|message| !message.is_empty());
                self.update_session(|session| {
                    session.status = if failed { ProviderSessionStatus::Error } else { ProviderSessionStatus::Ready };
                    session.active_turn_id = None;
                    if let Some(last_error) = last_error {
                        session.last_error = Some(last_error);
                    }
                });
            }
            ServerNotification::Error(params) => {
                if !params.thread_id.is_empty() && other_thread(&params.thread_id) {
                    return;
                }
                let message = params.error.message.clone();
                let will_retry = params.will_retry;
                self.update_session(|session| {
                    session.status = if will_retry {
                        ProviderSessionStatus::Running
                    } else {
                        ProviderSessionStatus::Error
                    };
                    if !message.is_empty() {
                        session.last_error = Some(message);
                    }
                });
            }
            _ => {}
        }
    }

    async fn handle_raw_notification(self: &Arc<Self>, method: String, params: Value) {
        let (is_memory, route_turn_id, route_item_id, child_parent_turn_id, foreign_thread_id) = {
            let mut state = self.lock();
            let is_memory = state.memory_filter.should_suppress(&method, &params);
            let (turn_id, item_id) = read_route_fields(&method, &params);
            let conversation = read_notification_thread_id(&method, &params);
            let child_parent_turn_id = conversation.as_ref().and_then(|id| state.collab_receiver_turns.get(id).cloned());
            (is_memory, turn_id, item_id, child_parent_turn_id, conversation)
        };
        // `rememberCollabReceiverTurns` (TS mutates the map held by the Ref, so it sticks on
        // every path). The parent turn read above predates it.
        if let Some(parent_turn) = &route_turn_id {
            if matches!(method.as_str(), "item/started" | "item/completed") && params["item"]["type"] == "collabAgentToolCall" {
                let mut state = self.lock();
                for receiver in params["item"]["receiverThreadIds"].as_array().into_iter().flatten() {
                    if let Some(receiver) = receiver.as_str() {
                        state.collab_receiver_turns.insert(receiver.to_owned(), parent_turn.clone());
                    }
                }
            }
        }

        if self.intercept_collab_child_notification(&method, &params) {
            return;
        }

        let root = self.provider_thread_id();
        let foreign = foreign_thread_id.as_ref().is_some_and(|id| root.as_ref().is_some_and(|root| root != id));
        if (child_parent_turn_id.is_some() || foreign) && should_suppress_child_conversation_notification(&method) {
            if let Some(foreign_thread_id) = &foreign_thread_id {
                let mut state = self.lock();
                if method == "turn/started" {
                    if let Some(turn) = str_at(&params, &["turn", "id"]) {
                        state.collab_live_turns.insert(foreign_thread_id.clone(), turn.to_owned());
                    }
                } else if method == "turn/completed" || method == "thread/closed" {
                    state.collab_live_turns.remove(foreign_thread_id);
                }
            }
            return;
        }

        if is_memory {
            return;
        }

        if method == "item/completed" && params["item"]["type"] == "contextCompaction" && root.as_deref().is_some_and(|root| params["threadId"] == root) {
            self.restore_additional_context(params["threadId"].as_str().unwrap_or_default()).await;
        }

        let mut turn_id = child_parent_turn_id.or(route_turn_id);
        let mut item_id = route_item_id;
        let mut request_id = None;
        let mut request_kind = None;
        if method == "serverRequest/resolved" {
            let raw = match &params["requestId"] {
                Value::String(value) => value.clone(),
                other => other.to_string(),
            };
            let correlation = self.lock().approval_correlations.remove(&raw);
            if let Some(correlation) = correlation {
                request_id = Some(correlation.request_id);
                request_kind = Some(correlation.request_kind);
                turn_id = correlation.turn_id.or(turn_id);
                item_id = correlation.item_id.or(item_id);
            }
        }
        let mut event = self.event(ProviderEventKind::Notification, &method);
        event.turn_id = turn_id.map(TurnId::new);
        event.item_id = item_id.map(ProviderItemId::new);
        event.request_id = request_id;
        event.request_kind = request_kind;
        if method == "item/agentMessage/delta" {
            event.text_delta = params["delta"].as_str().map(str::to_owned);
        }
        event.payload = Some(params);
        self.emit(event);
    }

    /// `interceptCollabChildNotification`: registers v2 collab children and re-emits their
    /// notifications as `collabAgent/*` events. True when fully handled.
    fn intercept_collab_child_notification(self: &Arc<Self>, method: &str, params: &Value) -> bool {
        // Registration path 1: a child thread announces itself with a thread_spawn source.
        if method == "thread/started" {
            let thread = &params["thread"];
            let Some(spawn) = thread["source"]
                .get("subAgent")
                .and_then(|sub| sub.get("thread_spawn"))
                .filter(|spawn| spawn.is_object())
            else {
                return false;
            };
            let Some(thread_id) = thread["id"].as_str().map(str::to_owned) else {
                return false;
            };
            let (state_child, metadata) = {
                let mut state = self.lock();
                let root = resume_cursor_thread_id(state.session.resume_cursor.as_ref());
                if root.as_deref() == Some(thread_id.as_str()) {
                    return false;
                }
                let existing = state.collab_children.get(&thread_id).cloned();
                let spawn_turn_id = match &existing {
                    Some(existing) => existing.spawn_turn_id.clone(),
                    None => state.session.active_turn_id.as_ref().map(|turn| turn.as_str().to_owned()),
                };
                let text = |value: &Value| value.as_str().map(str::to_owned);
                let child = CollabChild {
                    agent_thread_id: thread_id.clone(),
                    nickname: text(&spawn["agent_nickname"])
                        .or_else(|| text(&thread["agentNickname"]))
                        .or_else(|| existing.as_ref().and_then(|existing| existing.nickname.clone())),
                    role: text(&spawn["agent_role"])
                        .or_else(|| text(&thread["agentRole"]))
                        .or_else(|| existing.as_ref().and_then(|existing| existing.role.clone())),
                    agent_path: text(&spawn["agent_path"]).or_else(|| existing.as_ref().and_then(|existing| existing.agent_path.clone())),
                    depth: spawn
                        .get("depth")
                        .filter(|depth| depth.is_number())
                        .cloned()
                        .or_else(|| existing.as_ref().and_then(|existing| existing.depth.clone())),
                    parent_thread_id: text(&spawn["parent_thread_id"])
                        .or_else(|| text(&thread["parentThreadId"]))
                        .or_else(|| existing.as_ref().and_then(|existing| existing.parent_thread_id.clone())),
                    spawn_turn_id,
                };
                state.collab_children.insert(thread_id.clone(), child.clone());
                let metadata = state.collab_metadata.get(&thread_id).cloned();
                (child, metadata)
            };
            let mut payload = collab_identity(&state_child, metadata.as_ref());
            if let Some(depth) = &state_child.depth {
                payload.insert("depth".into(), depth.clone());
            }
            if let Some(parent) = state_child.parent_thread_id.as_ref().filter(|parent| !parent.is_empty()) {
                payload.insert("parentThreadId".into(), json!(parent));
            }
            self.emit_collab("collabAgent/started", state_child.spawn_turn_id.as_ref(), payload);
            self.start_collab_child_metadata_lookup(&thread_id);
            return true;
        }

        // Registration path 2: a parent-side subAgentActivity item names the child.
        if matches!(method, "item/started" | "item/completed") && params["item"]["type"] == "subAgentActivity" {
            let item = &params["item"];
            let agent_thread_id = item["agentThreadId"].as_str().unwrap_or_default().to_owned();
            let agent_path = item["agentPath"].as_str().unwrap_or_default().to_owned();
            let kind = item["kind"].clone();
            let (registered, metadata) = {
                let mut state = self.lock();
                let root = resume_cursor_thread_id(state.session.resume_cursor.as_ref());
                if root.as_deref() == Some(agent_thread_id.as_str()) || agent_path == "/root" || agent_path == "/" {
                    return false;
                }
                let activity_spawn_turn_id = state.session.active_turn_id.as_ref().map(|turn| turn.as_str().to_owned());
                let existing = state.collab_children.get(&agent_thread_id).cloned();
                let leaf = agent_path.split('/').rev().find(|segment| !segment.is_empty()).map(str::to_owned);
                let child = CollabChild {
                    agent_thread_id: agent_thread_id.clone(),
                    nickname: existing.as_ref().and_then(|existing| existing.nickname.clone()).or(leaf),
                    role: existing.as_ref().and_then(|existing| existing.role.clone()),
                    agent_path: existing
                        .as_ref()
                        .and_then(|existing| existing.agent_path.clone())
                        .or_else(|| Some(agent_path.clone())),
                    depth: existing.as_ref().and_then(|existing| existing.depth.clone()),
                    parent_thread_id: existing.as_ref().and_then(|existing| existing.parent_thread_id.clone()),
                    spawn_turn_id: match &existing {
                        Some(existing) => existing.spawn_turn_id.clone(),
                        None => activity_spawn_turn_id,
                    },
                };
                state.collab_children.insert(agent_thread_id.clone(), child.clone());
                let metadata = state.collab_metadata.get(&agent_thread_id).cloned();
                (child, metadata)
            };
            let mut payload = collab_identity(&registered, metadata.as_ref());
            payload.insert("activityKind".into(), kind.clone());
            self.emit_collab("collabAgent/activity", registered.spawn_turn_id.as_ref(), payload);
            if kind == "started" {
                self.start_collab_child_metadata_lookup(&agent_thread_id);
            }
            return true;
        }

        // Interception: notifications addressed to a registered child become agent events.
        let Some(conversation) = read_notification_thread_id(method, params) else {
            return false;
        };
        let root = self.provider_thread_id();
        if root.as_deref() == Some(conversation.as_str()) {
            return false;
        }
        if root.is_some() && matches!(method, "thread/settings/updated" | "model/rerouted") {
            let (model, effort) = if method == "thread/settings/updated" {
                (
                    non_empty_metadata(params["threadSettings"].get("model")),
                    non_empty_metadata(params["threadSettings"].get("effort")),
                )
            } else {
                (non_empty_metadata(params.get("toModel")), None)
            };
            let changed = self.update_collab_metadata(&conversation, model, effort, true);
            if changed && self.lock().collab_children.contains_key(&conversation) {
                self.emit_collab_metadata_updated(&conversation);
            }
            return true;
        }
        let (child, metadata) = {
            let state = self.lock();
            let Some(child) = state.collab_children.get(&conversation).cloned() else {
                return false;
            };
            let metadata = state.collab_metadata.get(&child.agent_thread_id).cloned();
            (child, metadata)
        };
        let identity = collab_identity(&child, metadata.as_ref());
        let spawn = child.spawn_turn_id.as_ref();
        match method {
            "turn/started" => {
                {
                    let mut state = self.lock();
                    if let Some(metadata) = state.collab_metadata.get_mut(&child.agent_thread_id) {
                        metadata.closed = false;
                    }
                    if let Some(turn) = str_at(params, &["turn", "id"]) {
                        state.collab_live_turns.insert(child.agent_thread_id.clone(), turn.to_owned());
                    }
                }
                self.emit_collab("collabAgent/turnStarted", spawn, identity);
                true
            }
            "turn/completed" => {
                self.lock().collab_live_turns.remove(&child.agent_thread_id);
                let mut payload = identity;
                payload.insert("turn".into(), params["turn"].clone());
                self.emit_collab("collabAgent/turnCompleted", spawn, payload);
                true
            }
            "thread/status/changed" => {
                let mut payload = identity;
                payload.insert("status".into(), params["status"].clone());
                self.emit_collab("collabAgent/statusChanged", spawn, payload);
                true
            }
            "thread/tokenUsage/updated" => {
                let mut payload = identity;
                payload.insert("tokenUsage".into(), params["tokenUsage"].clone());
                self.emit_collab("collabAgent/tokenUsage", spawn, payload);
                true
            }
            "item/started" | "item/completed" => {
                let mut payload = identity;
                payload.insert("item".into(), params["item"].clone());
                self.emit_collab("collabAgent/item", spawn, payload);
                true
            }
            "thread/closed" => {
                {
                    let mut state = self.lock();
                    state.collab_live_turns.remove(&child.agent_thread_id);
                    state.collab_metadata.entry(child.agent_thread_id.clone()).or_default().closed = true;
                }
                self.emit_collab("collabAgent/closed", spawn, identity);
                true
            }
            "error" => {
                if params["willRetry"] == true {
                    return true;
                }
                self.lock().collab_live_turns.remove(&child.agent_thread_id);
                let mut payload = identity;
                payload.insert("status".into(), json!({ "type": "systemError" }));
                self.emit_collab("collabAgent/statusChanged", spawn, payload);
                true
            }
            other => route_codex_child_notification(other) == ChildNotificationRoute::Drop,
        }
    }

    fn update_collab_metadata(&self, agent_thread_id: &str, model: Option<String>, effort: Option<String>, overwrite_known: bool) -> bool {
        let mut state = self.lock();
        let previous = state.collab_metadata.get(agent_thread_id).cloned().unwrap_or_default();
        let model_next = match model {
            Some(model) if overwrite_known || previous.model.is_none() => Some(model),
            _ => previous.model.clone(),
        };
        let effort_next = match effort {
            Some(effort) if overwrite_known || previous.effort.is_none() => Some(effort),
            _ => previous.effort.clone(),
        };
        if model_next == previous.model && effort_next == previous.effort {
            return false;
        }
        state.collab_metadata.insert(
            agent_thread_id.to_owned(),
            CollabMetadata {
                model: model_next,
                effort: effort_next,
                ..previous
            },
        );
        true
    }

    fn emit_collab_metadata_updated(&self, agent_thread_id: &str) {
        let (child, metadata) = {
            let state = self.lock();
            let Some(child) = state.collab_children.get(agent_thread_id).cloned() else {
                return;
            };
            let metadata = state.collab_metadata.get(agent_thread_id).cloned();
            if metadata.as_ref().is_some_and(|metadata| metadata.closed) {
                return;
            }
            (child, metadata)
        };
        self.emit_collab(
            "collabAgent/metadataUpdated",
            child.spawn_turn_id.as_ref(),
            collab_identity(&child, metadata.as_ref()),
        );
    }

    /// Rejoins an already-loaded child (`thread/resume` without history) once, to learn its
    /// model and effort. Best effort, 5 s.
    fn start_collab_child_metadata_lookup(self: &Arc<Self>, agent_thread_id: &str) {
        {
            let mut state = self.lock();
            let entry = state.collab_metadata.entry(agent_thread_id.to_owned()).or_default();
            if entry.lookup_started || entry.closed {
                return;
            }
            entry.lookup_started = true;
        }
        let shared = self.clone();
        let agent_thread_id = agent_thread_id.to_owned();
        let task = tokio::spawn(async move {
            let lookup = shared
                .peer()
                .request("thread/resume", Some(json!({ "threadId": agent_thread_id, "excludeTurns": true })));
            let Ok(Ok(response)) = tokio::time::timeout(Duration::from_secs(5), lookup).await else {
                return;
            };
            let (Some(thread_id), Some(model)) = (str_at(&response, &["thread", "id"]), response.get("model").and_then(Value::as_str)) else {
                return;
            };
            if !matches!(response.get("reasoningEffort"), None | Some(Value::Null | Value::String(_))) || thread_id != agent_thread_id {
                return;
            }
            {
                let state = shared.lock();
                if !state.collab_children.contains_key(&agent_thread_id) || state.collab_metadata.get(&agent_thread_id).is_some_and(|metadata| metadata.closed)
                {
                    return;
                }
            }
            let model = non_empty_metadata(Some(&json!(model)));
            let effort = non_empty_metadata(response.get("reasoningEffort"));
            if shared.update_collab_metadata(&agent_thread_id, model, effort, false) {
                shared.emit_collab_metadata_updated(&agent_thread_id);
            }
        });
        self.tasks.lock().expect("codex tasks lock").push(task.abort_handle());
    }

    /// Compaction drops the `additionalContext` developer messages and Codex only resends an
    /// entry when its value changes, so put them back (awaited, 10 s).
    async fn restore_additional_context(&self, thread_id: &str) {
        let Some(context) = self.lock().last_additional_context.clone() else { return };
        let items: Vec<Value> = context
            .iter()
            .map(|(key, entry)| {
                json!({
                    "type": "message",
                    "role": "developer",
                    "content": [{ "type": "input_text", "text": format!("<{key}>{}</{key}>", entry.value) }],
                })
            })
            .collect();
        let params = json!({ "threadId": thread_id, "items": items });
        let result = tokio::time::timeout(Duration::from_secs(10), async {
            let params: zc_codex_protocol::ThreadInjectItemsParams = serde_json::from_value(params).map_err(|error| {
                CodexAppServerError::Request(RequestError::invalid_payload(
                    "thread/inject_items",
                    RequestOperation::EncodePayload,
                    &error.to_string(),
                ))
            })?;
            request::<client_requests::ThreadInjectItems>(self.peer(), &params).await
        })
        .await;
        match result {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => tracing::warn!(%error, "Failed to restore Codex additional context after compaction."),
            Err(_) => tracing::warn!("Failed to restore Codex additional context after compaction: timed out."),
        }
    }

    // -- server requests -------------------------------------------------------------------

    async fn handle_request(self: Arc<Self>, method: String, params: Option<Value>) -> Result<Value, RequestError> {
        let request = match ServerRequest::decode(&method, params) {
            None => return Err(RequestError::method_not_found(&method)),
            Some(Err(error)) => return Err(RequestError::invalid_payload(&method, RequestOperation::DecodePayload, &error.to_string())),
            Some(Ok(request)) => request,
        };
        let payload = request.params_value();
        match &request {
            ServerRequest::ItemCommandExecutionRequestApproval(params) => {
                let correlation_key = params.approval_id.clone().flatten().unwrap_or_else(|| params.item_id.clone());
                let decision = self
                    .park_approval(
                        &method,
                        payload,
                        ProviderRequestKind::Command,
                        Some(params.turn_id.clone()),
                        Some(params.item_id.clone()),
                        correlation_key,
                    )
                    .await;
                Ok(json!({ "decision": approval_wire_decision(decision) }))
            }
            ServerRequest::ItemFileChangeRequestApproval(params) => {
                let decision = self
                    .park_approval(
                        &method,
                        payload,
                        ProviderRequestKind::FileChange,
                        Some(params.turn_id.clone()),
                        Some(params.item_id.clone()),
                        params.item_id.clone(),
                    )
                    .await;
                Ok(json!({ "decision": approval_wire_decision(decision) }))
            }
            ServerRequest::McpServerElicitationRequest(_) => {
                if to_mcp_elicitation_response(&payload, ProviderApprovalDecision::Accept)["action"] != "accept" {
                    tracing::warn!(
                        server_name = payload["serverName"].as_str().unwrap_or_default(),
                        mode = payload["mode"].as_str().unwrap_or_default(),
                        "Declined an unsupported MCP elicitation."
                    );
                    return Ok(json!({ "action": "decline" }));
                }
                let request_id = zc_core::uuid_v4();
                let turn_id = payload["turnId"]
                    .as_str()
                    .filter(|turn| !turn.is_empty())
                    .map(str::to_owned)
                    .or_else(|| self.lock().session.active_turn_id.as_ref().map(|turn| turn.as_str().to_owned()));
                let json_rpc_id = if payload["mode"] == "url" {
                    payload["elicitationId"].as_str().unwrap_or_default().to_owned()
                } else {
                    request_id.clone()
                };
                let decision = self
                    .park_approval_with_id(
                        &method,
                        payload.clone(),
                        ProviderRequestKind::McpElicitation,
                        turn_id,
                        None,
                        json_rpc_id,
                        request_id,
                    )
                    .await;
                Ok(to_mcp_elicitation_response(&payload, decision))
            }
            ServerRequest::ItemPermissionsRequestApproval(params) => {
                let permissions = payload["permissions"].clone();
                let decision = self
                    .park_approval(
                        &method,
                        payload,
                        ProviderRequestKind::Permission,
                        Some(params.turn_id.clone()),
                        Some(params.item_id.clone()),
                        params.item_id.clone(),
                    )
                    .await;
                let granted = if matches!(decision, ProviderApprovalDecision::Accept | ProviderApprovalDecision::AcceptForSession) {
                    permissions
                } else {
                    json!({})
                };
                let mut response = json!({ "permissions": granted });
                if decision == ProviderApprovalDecision::AcceptForSession {
                    response["scope"] = json!("session");
                }
                Ok(response)
            }
            ServerRequest::ItemToolRequestUserInput(params) => {
                let request_id = ApprovalRequestId::new(zc_core::uuid_v4());
                let (sender, receiver) = oneshot::channel();
                let turn_id = Some(params.turn_id.clone()).filter(|turn| !turn.is_empty());
                let item_id = Some(params.item_id.clone()).filter(|item| !item.is_empty());
                self.lock().pending_user_inputs.insert(
                    request_id.as_str().to_owned(),
                    PendingUserInput {
                        request_id: request_id.clone(),
                        turn_id: turn_id.clone(),
                        item_id: item_id.clone(),
                        answers: Some(sender),
                    },
                );
                let mut event = self.event(ProviderEventKind::Request, &method);
                event.request_id = Some(request_id.clone());
                event.turn_id = turn_id.map(TurnId::new);
                event.item_id = item_id.map(ProviderItemId::new);
                event.payload = Some(payload);
                self.emit(event);
                let guard = RemoveOnDrop {
                    shared: self.clone(),
                    key: request_id.as_str().to_owned(),
                    user_input: true,
                };
                let answers = receiver.await.unwrap_or_default();
                drop(guard);
                let answers = to_codex_user_input_answers(&answers).map_err(|error| {
                    let mut request_error = RequestError::invalid_params(error.to_string());
                    if let CodexSessionRuntimeError::InvalidUserInputAnswers { question_id } = &error {
                        request_error.data = Some(json!({ "questionId": question_id }));
                    }
                    request_error
                })?;
                Ok(json!({ "answers": answers }))
            }
            _ => Err(RequestError::method_not_found(&method)),
        }
    }

    async fn park_approval(
        self: &Arc<Self>,
        method: &str,
        payload: Value,
        kind: ProviderRequestKind,
        turn_id: Option<String>,
        item_id: Option<String>,
        json_rpc_id: String,
    ) -> ProviderApprovalDecision {
        let request_id = zc_core::uuid_v4();
        self.park_approval_with_id(method, payload, kind, turn_id, item_id, json_rpc_id, request_id)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn park_approval_with_id(
        self: &Arc<Self>,
        method: &str,
        payload: Value,
        kind: ProviderRequestKind,
        turn_id: Option<String>,
        item_id: Option<String>,
        json_rpc_id: String,
        request_id: String,
    ) -> ProviderApprovalDecision {
        let request_id = ApprovalRequestId::new(request_id);
        let turn_id = turn_id.filter(|turn| !turn.is_empty());
        let item_id = item_id.filter(|item| !item.is_empty());
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.lock();
            state.pending_approvals.insert(
                request_id.as_str().to_owned(),
                PendingApproval {
                    request_id: request_id.clone(),
                    request_kind: kind,
                    turn_id: turn_id.clone(),
                    item_id: item_id.clone(),
                    decision: Some(sender),
                },
            );
            state.approval_correlations.insert(
                json_rpc_id,
                ApprovalCorrelation {
                    request_id: request_id.clone(),
                    request_kind: kind,
                    turn_id: turn_id.clone(),
                    item_id: item_id.clone(),
                },
            );
        }
        let mut event = self.event(ProviderEventKind::Request, method);
        event.request_id = Some(request_id.clone());
        event.request_kind = Some(kind);
        event.turn_id = turn_id.map(TurnId::new);
        event.item_id = item_id.map(ProviderItemId::new);
        event.payload = Some(payload);
        self.emit(event);
        let guard = RemoveOnDrop {
            shared: self.clone(),
            key: request_id.as_str().to_owned(),
            user_input: false,
        };
        // A dropped sender (the runtime closed) settles as cancel, like close() does.
        let decision = receiver.await.unwrap_or(ProviderApprovalDecision::Cancel);
        drop(guard);
        decision
    }
}

/// The `Effect.ensuring` cleanup of a parked request.
struct RemoveOnDrop {
    shared: Arc<Shared>,
    key: String,
    user_input: bool,
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            if self.user_input {
                state.pending_user_inputs.remove(&self.key);
            } else {
                state.pending_approvals.remove(&self.key);
            }
        }
    }
}

/// The wire decision: `acceptAlways` is `acceptForSession` for command/file approvals.
fn approval_wire_decision(decision: ProviderApprovalDecision) -> &'static str {
    match decision {
        ProviderApprovalDecision::AcceptAlways => ProviderApprovalDecision::AcceptForSession.as_str(),
        other => other.as_str(),
    }
}

struct Handler {
    shared: Arc<Shared>,
}

impl IncomingHandler for Handler {
    fn on_notification(&self, method: String, params: Option<Value>) {
        // Unknown methods have no handler; params that do not decode are dropped (TS logs and
        // ignores them).
        let Some(Ok(notification)) = ServerNotification::decode(&method, params) else {
            return;
        };
        self.shared.apply_session_handlers(&notification);
        let _ = self.shared.notifications.send((method, notification.params_value()));
    }

    fn on_request(&self, method: String, params: Option<Value>) -> BoxFuture<'static, Result<Value, RequestError>> {
        Box::pin(self.shared.clone().handle_request(method, params))
    }
}

impl CodexSessionRuntime {
    /// `makeCodexSessionRuntime`: spawns `codex app-server` and wires the protocol. Call
    /// [`CodexRuntime::start`] to initialize and open the thread.
    pub fn spawn(options: CodexSessionRuntimeOptions) -> Result<Arc<Self>, CodexAppServerError> {
        let spec = options.spawn_spec();
        let child = spawn(&spec)?;
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let (notifications_tx, mut notifications_rx) = mpsc::unbounded_channel::<(String, Value)>();
        let created_at = now_iso();
        let session = ProviderSession {
            provider: ProviderDriverKind::new(crate::DRIVER_KIND),
            provider_instance_id: options.provider_instance_id.clone(),
            status: ProviderSessionStatus::Connecting,
            runtime_mode: options.runtime_mode,
            cwd: Some(options.cwd.clone()),
            model: options.model.clone(),
            thread_id: options.thread_id.clone(),
            resume_cursor: options.resume_cursor.clone(),
            active_turn_id: None,
            created_at: created_at.clone(),
            updated_at: created_at,
            last_error: None,
        };
        let shared = Arc::new(Shared {
            options,
            state: Mutex::new(State {
                session,
                pending_approvals: HashMap::new(),
                approval_correlations: HashMap::new(),
                pending_user_inputs: HashMap::new(),
                collab_receiver_turns: HashMap::new(),
                collab_children: HashMap::new(),
                collab_metadata: HashMap::new(),
                collab_live_turns: BTreeMap::new(),
                memory_filter: MemoryConsolidationFilter::default(),
                last_additional_context: None,
            }),
            events: Mutex::new(Some(events_tx)),
            events_rx: Mutex::new(Some(events_rx)),
            notifications: notifications_tx,
            peer: OnceLock::new(),
            closed: AtomicBool::new(false),
            tasks: Mutex::new(Vec::new()),
        });
        let termination_handle = child.handle.clone();
        let peer = CodexPeer::start(
            child.stdout,
            child.stdin,
            Arc::new(Handler { shared: shared.clone() }),
            Some(Box::pin(async move { termination_handle.termination_error().await })),
        );
        let _ = shared.peer.set(peer);

        let notification_shared = shared.clone();
        let notifications = tokio::spawn(async move {
            while let Some((method, params)) = notifications_rx.recv().await {
                notification_shared.handle_raw_notification(method, params).await;
            }
        });

        let stderr_shared = shared.clone();
        let stderr = child.stderr;
        let stderr_task = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buffer = Vec::new();
            loop {
                buffer.clear();
                match reader.read_until(b'\n', &mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        // An unterminated last line is dropped, as in TS.
                        if buffer.last() != Some(&b'\n') {
                            break;
                        }
                        let line = String::from_utf8_lossy(&buffer);
                        let line = line.trim_end_matches('\n').trim_end_matches('\r');
                        if let Some(message) = classify_codex_stderr_line(line) {
                            let mut event = stderr_shared.event(ProviderEventKind::Notification, "process/stderr");
                            event.message = Some(message);
                            stderr_shared.emit(event);
                        }
                    }
                }
            }
        });

        let exit_shared = shared.clone();
        let exit_handle = child.handle.clone();
        let exit_task = tokio::spawn(async move {
            // A process killed by a signal has no exit code: TS's exitCode fails, nothing is emitted.
            let Ok(Some(code)) = exit_handle.wait().await else { return };
            if exit_shared.closed.load(Ordering::SeqCst) {
                return;
            }
            exit_shared.update_session(|session| {
                session.status = if code == 0 {
                    ProviderSessionStatus::Closed
                } else {
                    ProviderSessionStatus::Error
                };
                session.active_turn_id = None;
            });
            let message = if code == 0 {
                "Codex App Server exited.".to_owned()
            } else {
                format!("Codex App Server exited with code {code}.")
            };
            exit_shared.emit_session_event("session/exited", &message);
        });
        shared
            .tasks
            .lock()
            .expect("codex tasks lock")
            .extend([notifications.abort_handle(), stderr_task.abort_handle(), exit_task.abort_handle()]);
        Ok(Arc::new(Self { shared, child: child.handle }))
    }

    fn read_provider_thread_id(&self) -> Result<String, CodexSessionRuntimeError> {
        self.shared.provider_thread_id().ok_or_else(|| CodexSessionRuntimeError::ThreadIdMissing {
            thread_id: self.shared.options.thread_id.as_str().to_owned(),
        })
    }

    pub fn peer(&self) -> &CodexPeer {
        self.shared.peer()
    }
}

#[async_trait]
impl CodexRuntime for CodexSessionRuntime {
    async fn start(&self) -> Result<ProviderSession, CodexSessionRuntimeError> {
        let shared = &self.shared;
        shared.emit_session_event("session/connecting", "Starting Codex App Server session.");
        let peer = shared.peer();
        let initialize = peer.request_raw("initialize", Some(build_codex_initialize_params())).await?;
        decode_response::<zc_codex_protocol::InitializeResponse>("initialize", initialize)?;
        peer.notify("initialized", None)?;
        let options = &shared.options;
        let requested_model = normalize_codex_model_slug(options.model.as_deref());
        let resume = resume_cursor_thread_id(options.resume_cursor.as_ref());
        let opened = open_codex_thread(
            peer,
            options.thread_id.as_str(),
            options.runtime_mode,
            &options.cwd,
            requested_model.as_deref(),
            options.service_tier.as_deref(),
            resume.as_deref(),
        )
        .await?;
        let session = {
            let mut state = shared.lock();
            state.session.status = ProviderSessionStatus::Ready;
            state.session.cwd = Some(opened.cwd.clone());
            state.session.model = Some(opened.model.clone());
            state.session.resume_cursor = Some(json!({ "threadId": opened.thread.id }));
            state.session.updated_at = now_iso();
            state.session.clone()
        };
        shared.emit_session_event("session/ready", "Codex App Server session ready.");
        Ok(session)
    }

    async fn get_session(&self) -> ProviderSession {
        self.shared.lock().session.clone()
    }

    async fn send_turn(&self, input: SendTurnInput) -> Result<ProviderTurnStartResult, CodexSessionRuntimeError> {
        let provider_thread_id = self.read_provider_thread_id()?;
        let shared = &self.shared;
        let options = &shared.options;
        if has_configured_mcp_server(options.app_server_args.as_deref()) {
            if let Err(error) = shared.peer().request("config/mcpServer/reload", None).await {
                tracing::warn!(%error, "Failed to refresh Codex MCP tool catalog before turn.");
            }
        }
        let session_model = shared.lock().session.model.clone();
        let normalized_model = normalize_codex_model_slug(input.model.as_deref().or(session_model.as_deref()));
        let models = match &options.models {
            Some(models) => models().await,
            None => Vec::new(),
        };
        let model_name = normalized_model
            .as_ref()
            .and_then(|model| models.iter().find(|candidate| &candidate.slug == model).map(|candidate| candidate.name.clone()));
        let params = build_turn_start_params(&TurnStartParamsInput {
            thread_id: provider_thread_id,
            runtime_mode: Some(options.runtime_mode),
            prompt: input.input.clone().filter(|prompt| !prompt.is_empty()),
            attachments: input.attachments.clone().unwrap_or_default(),
            model: normalized_model.clone(),
            model_name,
            service_tier: input.service_tier.clone(),
            effort: input.effort.clone(),
            interaction_mode: input.interaction_mode,
            tools: Some(configured_mcp_tool_availability(
                options.app_server_args.as_deref(),
                options.mcp_capabilities.as_ref(),
            )),
        });
        shared.lock().last_additional_context = params.get("additionalContext").and_then(|context| serde_json::from_value(context.clone()).ok());
        let raw = shared.peer().request("turn/start", Some(params)).await?;
        let response: zc_codex_protocol::TurnStartResponse = serde_json::from_value(raw).map_err(|error| CodexAppServerError::ProtocolParse {
            operation: ProtocolParseOperation::DecodeResponsePayload,
            method: Some("turn/start".to_owned()),
            request_id: None,
            detail: Some(format!("line {} column {}", error.line(), error.column())),
        })?;
        let turn_id = TurnId::new(response.turn.id);
        let resume = {
            let mut state = shared.lock();
            state.session.status = ProviderSessionStatus::Running;
            // Codex accepts follow-ups while a turn runs; turn/interrupt only takes the active id.
            if state.session.active_turn_id.is_none() {
                state.session.active_turn_id = Some(turn_id.clone());
            }
            if let Some(model) = &normalized_model {
                state.session.model = Some(model.clone());
            }
            state.session.updated_at = now_iso();
            resume_cursor_thread_id(state.session.resume_cursor.as_ref())
        };
        Ok(ProviderTurnStartResult {
            thread_id: options.thread_id.clone(),
            turn_id,
            resume_cursor: resume.map(|thread_id| json!({ "threadId": thread_id })),
        })
    }

    async fn compact_thread(&self) -> Result<(), CodexSessionRuntimeError> {
        let provider_thread_id = self.read_provider_thread_id()?;
        self.shared
            .peer()
            .request("thread/compact/start", Some(json!({ "threadId": provider_thread_id })))
            .await?;
        Ok(())
    }

    async fn interrupt_turn(&self, turn_id: Option<TurnId>) -> Result<(), CodexSessionRuntimeError> {
        let provider_thread_id = self.read_provider_thread_id()?;
        let shared = &self.shared;
        let active_turn_id = shared.lock().session.active_turn_id.clone();
        // Settle parked prompts first: Codex blocks on them, and a pending card would otherwise
        // hold up the interrupt.
        shared.settle_pending_approvals(ProviderApprovalDecision::Cancel);
        shared.settle_pending_user_inputs();
        // Stop every live child turn too, best effort and bounded (3 s each, 10 s overall).
        let live: Vec<(String, String)> = shared
            .lock()
            .collab_live_turns
            .iter()
            .map(|(thread, turn)| (thread.clone(), turn.clone()))
            .collect();
        let peer = shared.peer().clone();
        let children = futures::stream::iter(live.into_iter().map(|(thread_id, turn_id)| {
            let peer = peer.clone();
            async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(3),
                    peer.request("turn/interrupt", Some(json!({ "threadId": thread_id, "turnId": turn_id }))),
                )
                .await;
            }
        }));
        let _ = tokio::time::timeout(Duration::from_secs(10), futures::StreamExt::for_each_concurrent(children, 8, |future| future)).await;
        let Some(effective) = turn_id.or(active_turn_id) else { return Ok(()) };
        peer.request("turn/interrupt", Some(json!({ "threadId": provider_thread_id, "turnId": effective.as_str() })))
            .await?;
        Ok(())
    }

    async fn read_thread(&self) -> Result<CodexThreadSnapshot, CodexSessionRuntimeError> {
        let provider_thread_id = self.read_provider_thread_id()?;
        Ok(read_codex_thread(self.shared.peer(), &provider_thread_id).await?)
    }

    async fn rollback_thread(&self, num_turns: usize) -> Result<CodexThreadSnapshot, CodexSessionRuntimeError> {
        let provider_thread_id = self.read_provider_thread_id()?;
        let snapshot = rollback_codex_thread(self.shared.peer(), &provider_thread_id, num_turns).await?;
        self.shared.update_session(|session| {
            session.status = ProviderSessionStatus::Ready;
            session.active_turn_id = None;
        });
        Ok(snapshot)
    }

    async fn upload_feedback(&self, reason: Option<String>) -> Result<String, CodexSessionRuntimeError> {
        let provider_thread_id = self.read_provider_thread_id()?;
        let mut params = json!({ "classification": "bug", "includeLogs": true, "threadId": provider_thread_id });
        if let Some(reason) = reason.filter(|reason| !reason.is_empty()) {
            params["reason"] = json!(reason);
        }
        let params: zc_codex_protocol::FeedbackUploadParams = serde_json::from_value(params).expect("feedback params");
        let response = request::<client_requests::FeedbackUpload>(self.shared.peer(), &params).await?;
        Ok(response.thread_id)
    }

    async fn respond_to_request(&self, request_id: &ApprovalRequestId, decision: ProviderApprovalDecision) -> Result<(), CodexSessionRuntimeError> {
        let shared = &self.shared;
        let pending = shared.lock().pending_approvals.remove(request_id.as_str());
        let Some(mut pending) = pending else {
            return Err(CodexSessionRuntimeError::PendingApprovalNotFound {
                request_id: request_id.as_str().to_owned(),
            });
        };
        if let Some(sender) = pending.decision.take() {
            let _ = sender.send(decision);
        }
        let mut event = shared.event(ProviderEventKind::Notification, "item/requestApproval/decision");
        event.request_id = Some(pending.request_id.clone());
        event.request_kind = Some(pending.request_kind);
        event.turn_id = pending.turn_id.clone().map(TurnId::new);
        event.item_id = pending.item_id.clone().map(ProviderItemId::new);
        event.payload = Some(json!({
            "requestId": pending.request_id.as_str(),
            "requestKind": pending.request_kind.as_str(),
            "decision": decision.as_str(),
        }));
        shared.emit(event);
        Ok(())
    }

    async fn respond_to_user_input(&self, request_id: &ApprovalRequestId, answers: ProviderUserInputAnswers) -> Result<(), CodexSessionRuntimeError> {
        let shared = &self.shared;
        if !shared.lock().pending_user_inputs.contains_key(request_id.as_str()) {
            return Err(CodexSessionRuntimeError::PendingUserInputNotFound {
                request_id: request_id.as_str().to_owned(),
            });
        }
        let codex_answers = to_codex_user_input_answers(&answers)?;
        let Some(mut pending) = shared.lock().pending_user_inputs.remove(request_id.as_str()) else {
            return Err(CodexSessionRuntimeError::PendingUserInputNotFound {
                request_id: request_id.as_str().to_owned(),
            });
        };
        if let Some(sender) = pending.answers.take() {
            let _ = sender.send(answers);
        }
        let mut event = shared.event(ProviderEventKind::Notification, "item/tool/requestUserInput/answered");
        event.request_id = Some(pending.request_id.clone());
        event.turn_id = pending.turn_id.clone().map(TurnId::new);
        event.item_id = pending.item_id.clone().map(ProviderItemId::new);
        event.payload = Some(json!({ "answers": codex_answers }));
        shared.emit(event);
        Ok(())
    }

    fn take_events(&self) -> Option<mpsc::UnboundedReceiver<ProviderEvent>> {
        self.shared.events_rx.lock().expect("codex events lock").take()
    }

    async fn close(&self) {
        let shared = &self.shared;
        if shared.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        shared.settle_pending_approvals(ProviderApprovalDecision::Cancel);
        shared.settle_pending_user_inputs();
        shared.update_session(|session| {
            session.status = ProviderSessionStatus::Closed;
            session.active_turn_id = None;
        });
        shared.emit_session_event("session/closed", "Session stopped");
        shared.peer().shutdown();
        for task in shared.tasks.lock().expect("codex tasks lock").drain(..) {
            task.abort();
        }
        self.child.kill(FORCE_KILL_AFTER).await;
        shared.events.lock().expect("codex events lock").take();
    }
}

#[cfg(test)]
mod tests;
