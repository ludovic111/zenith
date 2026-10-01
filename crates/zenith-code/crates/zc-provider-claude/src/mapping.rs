//! The heart of `provider/Layers/ClaudeAdapter.ts`: one session's state and the mapping of
//! every native SDK message to canonical `ProviderRuntimeEvent`s (text and thinking deltas,
//! tool items, tasks and sub-agents, plan mode, token usage, rate limits, turn lifecycle,
//! resume cursor).
//!
//! Everything here is synchronous and deterministic given the [`MapperEnv`] (ids, clock): the
//! adapter drives it from the live stream and API calls, and the recorded-log replay (gate 2)
//! drives it from `NTIVE` lines. Events are built as contract JSON in the same shape the TS
//! adapter produced, and decoded into `ProviderRuntimeEvent` by the adapter.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use indexmap::{IndexMap, IndexSet};
use serde_json::{json, Map, Value};
use zc_contracts::{ProviderSession, ProviderSessionStartInput, RuntimeMode};

use crate::js;
use crate::usage_limits::{claude_rate_limit_event_to_update, js_number, ScopedLimitNamesRef};

pub const PROVIDER: &str = "claudeAgent";
/// How many racing sub-agent snapshot models are buffered per session.
const PENDING_TASK_MODEL_CAP: usize = 64;
const WORKFLOW_PHASE_CAP: usize = 64;
const WORKFLOW_AGENT_CAP: usize = 100;
/// Beyond this a usage-limit reset time is not credible.
const CLAUDE_USAGE_LIMIT_MAX_WAIT_MS: f64 = 30.0 * 24.0 * 60.0 * 60.0 * 1000.0;

/// Fresh identifiers (`crypto.randomUUIDv4`).
pub trait IdSource: Send + Sync {
    fn next_id(&self) -> String;
}

/// Random v4 UUIDs.
#[derive(Debug, Default, Clone, Copy)]
pub struct RandomIds;

impl IdSource for RandomIds {
    fn next_id(&self) -> String {
        zc_core::uuid_v4()
    }
}

/// The wall clock (`DateTime.now`).
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
    fn now_iso(&self) -> String {
        zc_core::time::try_iso_from_millis(self.now_ms()).unwrap_or_else(zc_core::now_iso)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        zc_core::now_millis()
    }
}

/// Where native SDK messages are logged (`EventNdjsonLogger`, `NTIVE` stream; owned by the
/// provider core). Receives the `{observedAt, event}` record and the thread id.
pub trait NativeEventSink: Send + Sync {
    fn write(&self, record: Value, thread_id: &str);
}

/// The services the mapping uses.
#[derive(Clone)]
pub struct MapperEnv {
    pub ids: Arc<dyn IdSource>,
    pub clock: Arc<dyn Clock>,
    pub scoped_limit_names: ScopedLimitNamesRef,
    /// `claudeEnvironment.CLAUDE_CONFIG_DIR` (for the signed-out message).
    pub claude_config_dir: Option<String>,
    pub native_sink: Option<Arc<dyn NativeEventSink>>,
}

impl MapperEnv {
    fn stamp(&self) -> (String, String) {
        (self.ids.next_id(), self.clock.now_iso())
    }
}

// ── helpers ─────────────────────────────────────────────────────────────────

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `finiteNonNegativeInteger`.
fn finite_non_negative_integer(value: Option<&Value>) -> Option<f64> {
    js::finite(value).filter(|v| *v >= 0.0).map(f64::round)
}

fn fnni(value: f64) -> Option<f64> {
    (value.is_finite() && value >= 0.0).then(|| value.round())
}

/// `finitePositiveInteger`.
fn finite_positive_integer(value: Option<f64>) -> Option<f64> {
    value.filter(|v| v.is_finite() && *v > 0.0).map(f64::round)
}

/// `nonNegativeInt` (floor).
fn non_negative_int(value: Option<&Value>) -> Option<f64> {
    js::finite(value).filter(|v| *v >= 0.0).map(f64::floor)
}

/// `trimmedString` / `readString`.
fn trimmed_string(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn usage_input_tokens(usage: &Map<String, Value>) -> f64 {
    finite_non_negative_integer(usage.get("input_tokens")).unwrap_or(0.0)
        + finite_non_negative_integer(usage.get("cache_creation_input_tokens")).unwrap_or(0.0)
        + finite_non_negative_integer(usage.get("cache_read_input_tokens")).unwrap_or(0.0)
}

fn usage_output_tokens(usage: &Map<String, Value>) -> f64 {
    finite_non_negative_integer(usage.get("output_tokens")).unwrap_or(0.0)
}

/// `lastClaudeUsageIteration`.
fn last_usage_iteration(usage: &Map<String, Value>) -> Option<&Map<String, Value>> {
    usage.get("iterations").and_then(Value::as_array)?.iter().rev().find_map(Value::as_object)
}

/// `claudeTotalProcessedTokens`.
fn total_processed_tokens(value: Option<&Value>) -> Option<f64> {
    let usage = value?.as_object()?;
    if let Some(explicit) = finite_non_negative_integer(usage.get("total_tokens")).filter(|t| *t > 0.0) {
        return Some(explicit);
    }
    let total = usage_input_tokens(usage) + usage_output_tokens(usage);
    (total > 0.0).then_some(total)
}

fn total_processed_tokens_of_map(usage: &Map<String, Value>) -> Option<f64> {
    if let Some(explicit) = finite_non_negative_integer(usage.get("total_tokens")).filter(|t| *t > 0.0) {
        return Some(explicit);
    }
    let total = usage_input_tokens(usage) + usage_output_tokens(usage);
    (total > 0.0).then_some(total)
}

/// `ThreadTokenUsageSnapshot`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenUsage {
    pub used_tokens: f64,
    pub last_used_tokens: Option<f64>,
    pub total_processed_tokens: Option<f64>,
    pub input_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub max_tokens: Option<f64>,
    pub tool_uses: Option<f64>,
    pub duration_ms: Option<f64>,
}

impl TokenUsage {
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("usedTokens".into(), js_number(self.used_tokens));
        let optional = [
            ("lastUsedTokens", self.last_used_tokens),
            ("totalProcessedTokens", self.total_processed_tokens),
            ("inputTokens", self.input_tokens),
            ("outputTokens", self.output_tokens),
            ("maxTokens", self.max_tokens),
            ("toolUses", self.tool_uses),
            ("durationMs", self.duration_ms),
        ];
        for (key, value) in optional {
            if let Some(value) = value {
                map.insert(key.into(), js_number(value));
            }
        }
        Value::Object(map)
    }
}

/// `makeClaudeTokenUsageSnapshot`.
fn make_token_usage(
    active_tokens: f64,
    input: Option<f64>,
    output: Option<f64>,
    context_window: Option<f64>,
    total_processed: Option<f64>,
    last_used: Option<f64>,
) -> Option<TokenUsage> {
    let active = fnni(active_tokens).filter(|a| *a > 0.0)?;
    let max_tokens = finite_positive_integer(context_window);
    let used = max_tokens.map_or(active, |max| active.min(max));
    let last_used = last_used.and_then(fnni).unwrap_or(used);
    let total = total_processed.and_then(fnni);
    Some(TokenUsage {
        used_tokens: used,
        last_used_tokens: Some(last_used),
        total_processed_tokens: total.filter(|t| *t > used),
        input_tokens: input.and_then(fnni).filter(|v| *v > 0.0),
        output_tokens: output.and_then(fnni).filter(|v| *v > 0.0),
        max_tokens,
        tool_uses: None,
        duration_ms: None,
    })
}

/// `normalizeClaudeActiveTokenUsage`.
fn normalize_active_token_usage(value: Option<&Value>, context_window: Option<f64>, total_processed: Option<f64>) -> Option<TokenUsage> {
    let value = value?;
    let empty = Map::new();
    let usage = match value {
        Value::Object(map) => map,
        Value::Array(_) => &empty,
        _ => return None,
    };
    let active_usage = last_usage_iteration(usage).unwrap_or(usage);
    let input = usage_input_tokens(active_usage);
    let output = usage_output_tokens(active_usage);
    let active = total_processed_tokens_of_map(active_usage).unwrap_or(input + output);
    if active <= 0.0 {
        return None;
    }
    make_token_usage(active, Some(input), Some(output), context_window, total_processed, None)
}

/// `compactBoundaryTokenUsageSnapshot`.
fn compact_boundary_token_usage(message: &Value, context_window: Option<f64>, total_processed: Option<f64>) -> Option<TokenUsage> {
    let metadata = message.get("compact_metadata")?.as_object()?;
    let post = finite_non_negative_integer(metadata.get("post_tokens")).filter(|p| *p > 0.0)?;
    let pre = finite_non_negative_integer(metadata.get("pre_tokens"));
    let mut snapshot = make_token_usage(post, None, None, context_window, total_processed, pre)?;
    if pre.is_none() {
        snapshot.last_used_tokens = None;
    }
    Some(snapshot)
}

/// `normalizeTaskUsage`: the typed `RuntimeTaskUsage`.
fn normalize_task_usage(usage: Option<&Value>) -> Option<Value> {
    let usage = usage?.as_object()?;
    let total = non_negative_int(usage.get("total_tokens"))?;
    let mut map = Map::new();
    map.insert("totalTokens".into(), js_number(total));
    for (key, source) in [
        ("inputTokens", "input_tokens"),
        ("cachedInputTokens", "cache_read_input_tokens"),
        ("outputTokens", "output_tokens"),
        ("toolUses", "tool_uses"),
        ("durationMs", "duration_ms"),
    ] {
        if let Some(value) = non_negative_int(usage.get(source)) {
            map.insert(key.into(), js_number(value));
        }
    }
    Some(Value::Object(map))
}

/// `isUuid`.
pub fn is_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        let ok = match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            14 => (b'1'..=b'8').contains(byte),
            19 => matches!(byte.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_hexdigit(),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// `ClaudeResumeState` (`readClaudeResumeState`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClaudeResumeState {
    pub thread_id: Option<String>,
    pub resume: Option<String>,
    pub resume_session_at: Option<String>,
    pub turn_count: Option<usize>,
    pub turn_start_message_ids: Option<Vec<Option<String>>>,
}

/// `readClaudeResumeState(resumeCursor)`.
pub fn read_claude_resume_state(cursor: Option<&Value>) -> Option<ClaudeResumeState> {
    let cursor = cursor?.as_object()?;
    let thread_id = cursor
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && !id.starts_with("claude-thread-"))
        .map(str::to_string);
    let resume = cursor
        .get("resume")
        .and_then(Value::as_str)
        .or_else(|| {
            if cursor.get("resume").is_some_and(Value::is_string) {
                None
            } else {
                cursor.get("sessionId").and_then(Value::as_str)
            }
        })
        .filter(|candidate| !candidate.is_empty() && is_uuid(candidate))
        .map(str::to_string);
    let resume_session_at = cursor
        .get("resumeSessionAt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let turn_count = cursor
        .get("turnCount")
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && n.fract() == 0.0 && *n >= 0.0)
        .map(|n| n as usize);
    let turn_start_message_ids = cursor.get("turnStartMessageIds").and_then(Value::as_array).and_then(|ids| {
        ids.iter()
            .map(|id| match id {
                Value::Null => Some(None),
                Value::String(s) => Some(Some(s.clone())),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
    });
    Some(ClaudeResumeState {
        thread_id,
        resume,
        resume_session_at,
        turn_count,
        turn_start_message_ids,
    })
}

// ── tool classification ─────────────────────────────────────────────────────

const WORKSPACE_IMAGE_PREVIEW_EXTENSIONS: [&str; 8] = [".avif", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".webp"];

/// `isWorkspaceImagePreviewPath`.
fn is_workspace_image_preview_path(path: &str) -> bool {
    let without_query = path.split(['?', '#']).next().unwrap_or("").to_lowercase();
    WORKSPACE_IMAGE_PREVIEW_EXTENSIONS.iter().any(|ext| without_query.ends_with(ext))
}

/// `readToolImagePath`.
fn read_tool_image_path(tool_name: &str, input: &Map<String, Value>) -> Option<String> {
    let normalized = tool_name.trim().to_lowercase();
    if normalized != "read" && normalized != "read file" {
        return None;
    }
    let path = input.get("file_path").filter(|v| !v.is_null()).or_else(|| input.get("path"))?.as_str()?.trim();
    (!path.is_empty() && is_workspace_image_preview_path(path)).then(|| path.to_string())
}

/// `classifyToolItemType`.
pub fn classify_tool_item_type(tool_name: &str, input: &Map<String, Value>) -> &'static str {
    let normalized = tool_name.to_lowercase();
    if read_tool_image_path(tool_name, input).is_some() {
        return "image_view";
    }
    if normalized.contains("agent") || normalized == "task" || normalized.contains("subagent") || normalized.contains("sub-agent") {
        return "collab_agent_tool_call";
    }
    if ["bash", "command", "shell", "terminal"].iter().any(|k| normalized.contains(k)) {
        return "command_execution";
    }
    if ["edit", "write", "file", "patch", "replace", "create", "delete"]
        .iter()
        .any(|k| normalized.contains(k))
    {
        return "file_change";
    }
    if normalized.contains("mcp") {
        return "mcp_tool_call";
    }
    if normalized.contains("websearch") || normalized.contains("web search") {
        return "web_search";
    }
    if normalized.contains("image") {
        return "image_view";
    }
    "dynamic_tool_call"
}

fn is_read_only_tool_name(tool_name: &str) -> bool {
    let normalized = tool_name.to_lowercase();
    normalized == "read" || ["read file", "view", "grep", "glob", "search"].iter().any(|k| normalized.contains(k))
}

/// `classifyRequestType`.
pub fn classify_request_type(tool_name: &str) -> &'static str {
    if is_read_only_tool_name(tool_name) {
        return "file_read_approval";
    }
    match classify_tool_item_type(tool_name, &Map::new()) {
        "command_execution" => "command_execution_approval",
        "file_change" => "file_change_approval",
        _ => "dynamic_tool_call",
    }
}

/// `titleForTool`.
fn title_for_tool(item_type: &str) -> &'static str {
    match item_type {
        "command_execution" => "Command run",
        "file_change" => "File change",
        "mcp_tool_call" => "MCP tool call",
        "collab_agent_tool_call" => "Subagent task",
        "web_search" => "Web search",
        "image_view" => "Image view",
        "dynamic_tool_call" => "Tool call",
        _ => "Item",
    }
}

/// `summarizeToolRequest`.
pub fn summarize_tool_request(tool_name: &str, input: &Map<String, Value>) -> String {
    if let Some(path) = read_tool_image_path(tool_name, input) {
        return path;
    }
    let command = input
        .get("command")
        .filter(|v| !v.is_null())
        .or_else(|| input.get("cmd"))
        .and_then(Value::as_str);
    if let Some(command) = command.map(str::trim).filter(|c| !c.is_empty()) {
        return format!("{tool_name}: {}", js::slice_utf16(command, 400));
    }
    if classify_tool_item_type(tool_name, &Map::new()) == "collab_agent_tool_call" {
        let description = input.get("description").and_then(Value::as_str).map(str::trim).filter(|d| !d.is_empty());
        let prompt = input.get("prompt").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty());
        if let Some(label) = description.map(str::to_string).or_else(|| prompt.map(|p| js::slice_utf16(p, 200))) {
            return label;
        }
    }
    let serialized = js::stringify(&Value::Object(input.clone()));
    if js::utf16_len(&serialized) <= 400 {
        format!("{tool_name}: {serialized}")
    } else {
        format!("{tool_name}: {}...", js::slice_utf16(&serialized, 397))
    }
}

fn is_todo_tool(tool_name: &str) -> bool {
    tool_name.to_lowercase().contains("todowrite")
}

/// `extractPlanStepsFromTodoInput`.
fn plan_steps_from_todo_input(input: &Map<String, Value>) -> Option<Vec<Value>> {
    let todos = input.get("todos")?.as_array().filter(|todos| !todos.is_empty())?;
    Some(
        todos
            .iter()
            .filter(|todo| todo.is_object())
            .map(|todo| {
                let step = todo
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .unwrap_or("Task");
                let status = match str_of(todo, "status") {
                    Some("completed") => "completed",
                    Some("in_progress") => "inProgress",
                    _ => "pending",
                };
                json!({ "step": step, "status": status })
            })
            .collect(),
    )
}

fn is_claude_task_tool(tool_name: &str) -> bool {
    matches!(tool_name, "TaskCreate" | "TaskUpdate" | "TaskList")
}

fn normalize_claude_task_status(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_str) {
        Some("completed") => "completed",
        Some("in_progress") => "inProgress",
        _ => "pending",
    }
}

fn read_string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string).collect())
        .unwrap_or_default()
}

/// `sanitizeSessionUrl`: only http(s).
fn sanitize_session_url(value: Option<&Value>) -> Option<String> {
    let trimmed = value?.as_str()?.trim();
    let lower = trimmed.to_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")).then(|| trimmed.to_string())
}

/// `classifyTaskAgentKind`.
fn classify_task_agent_kind(task_type: Option<&str>, agent_id: Option<&str>) -> &'static str {
    const NON_AGENT: [&str; 6] = ["monitor", "monitor_mcp", "local_bash", "shell", "plan", "dream"];
    let non_agent_type = task_type.is_some_and(|t| NON_AGENT.contains(&t));
    if agent_id.is_some_and(|id| !id.trim().is_empty()) {
        return if task_type.is_none() || non_agent_type { "background" } else { "agent" };
    }
    if non_agent_type {
        "background"
    } else {
        "agent"
    }
}

/// `CLAUDE_TASK_PATCH_STATUS`.
fn task_patch_status(status: &str) -> Option<&'static str> {
    Some(match status {
        "pending" => "pending",
        "running" => "running",
        "completed" => "completed",
        "failed" => "failed",
        "killed" => "cancelled",
        "paused" => "idle",
        _ => return None,
    })
}

// ── result classification ────────────────────────────────────────────────────

fn result_errors_text(result: &Value) -> String {
    result
        .get("errors")
        .and_then(Value::as_array)
        .map(|errors| errors.iter().map(|e| js::template(Some(e))).collect::<Vec<_>>().join(" ").to_lowercase())
        .unwrap_or_default()
}

/// `terminalResultError`.
fn terminal_result_error(reason: Option<&str>, failure_hint: Option<&str>) -> Option<String> {
    Some(
        match reason? {
            "api_error" => return Some(failure_hint.unwrap_or("Claude gave up after repeated API errors.").to_string()),
            "malformed_tool_use_exhausted" => "Claude gave up after repeated malformed tool calls.",
            "budget_exhausted" => "Claude stopped: the turn's token budget was exhausted.",
            "structured_output_retry_exhausted" => "Claude could not produce the requested structured output.",
            "tool_deferred_unavailable" => "Claude could not resume a deferred tool call: the tool is no longer available.",
            "turn_setup_failed" => "Claude could not start the turn.",
            "blocking_limit" => "Claude stopped: a usage limit blocked the request.",
            "rapid_refill_breaker" => "Claude stopped: the context refilled too quickly after compaction.",
            "prompt_too_long" => "Claude stopped: the prompt exceeds the model's context window.",
            "image_error" => "Claude stopped: an image in the conversation could not be processed.",
            "model_error" => "Claude stopped: the model returned an error.",
            _ => return None,
        }
        .to_string(),
    )
}

fn is_interrupted_result(result: &Value) -> bool {
    if matches!(str_of(result, "terminal_reason"), Some("aborted_tools" | "aborted_streaming")) {
        return true;
    }
    let errors = result_errors_text(result);
    if errors.contains("interrupt") {
        return true;
    }
    str_of(result, "subtype") == Some("error_during_execution")
        && result.get("is_error") == Some(&Value::Bool(false))
        && (errors.contains("request was aborted") || errors.contains("interrupted by user") || errors.contains("aborted"))
}

/// `resultOutcome`: the turn status and its error from one result.
pub fn result_outcome(result: &Value, failure_hint: Option<&str>) -> (&'static str, Option<String>) {
    let subtype = str_of(result, "subtype");
    let success_tagged_failure = subtype == Some("success") && result.get("is_error") == Some(&Value::Bool(true));
    let overloaded = subtype == Some("success") && js::finite(result.get("api_error_status")) == Some(529.0);
    let structured = if overloaded {
        Some("Claude API is overloaded (529). Try again shortly.".to_string())
    } else {
        terminal_result_error(str_of(result, "terminal_reason"), failure_hint).or_else(|| {
            if success_tagged_failure {
                failure_hint.map(str::to_string)
            } else {
                None
            }
        })
    };
    let listed = if subtype == Some("success") && !success_tagged_failure {
        None
    } else {
        result.get("errors").and_then(Value::as_array).and_then(|errors| {
            errors
                .iter()
                .filter_map(Value::as_str)
                .find(|e| !e.starts_with("[ede_diagnostic]"))
                .map(str::to_string)
        })
    };
    let error_message = listed.filter(|l| !l.is_empty()).or(structured.clone());
    if structured.is_some() {
        return ("failed", error_message);
    }
    if subtype == Some("success") {
        return ("completed", error_message);
    }
    if is_interrupted_result(result) {
        return ("interrupted", error_message);
    }
    (
        if result_errors_text(result).contains("cancel") {
            "cancelled"
        } else {
            "failed"
        },
        error_message,
    )
}

/// `isClaudeInterruptedMessage`.
pub fn is_interrupted_message(message: &str) -> bool {
    let normalized = message.to_lowercase();
    normalized.contains("all fibers interrupted without error") || normalized.contains("request was aborted") || normalized.contains("interrupted by user")
}

fn usage_limit_label(rate_limit_type: Option<&str>, overage_included_name: Option<&str>) -> Option<String> {
    let kind = rate_limit_type?;
    if kind == "seven_day_overage_included" {
        if let Some(name) = overage_included_name {
            return Some(format!("7-day {name}"));
        }
    }
    Some(
        match kind {
            "five_hour" => "5-hour",
            "seven_day" => "7-day",
            "seven_day_opus" => "7-day Opus",
            "seven_day_sonnet" => "7-day Sonnet",
            "seven_day_overage_included" => "7-day model",
            "overage" => "overage",
            _ => return None,
        }
        .to_string(),
    )
}

fn format_usage_limit_wait(wait_ms: f64) -> String {
    let total_minutes = (wait_ms / 60_000.0).ceil() as i64;
    let hours = total_minutes / 60;
    let minutes = total_minutes % 60;
    if hours == 0 {
        format!("{total_minutes}m")
    } else if minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h {minutes}m")
    }
}

/// `describeClaudeUsageLimit`.
fn describe_usage_limit(info: &Value, now_ms: f64, overage_included_name: Option<&str>) -> String {
    let label = usage_limit_label(str_of(info, "rateLimitType"), overage_included_name);
    let wait = info
        .get("resetsAt")
        .and_then(Value::as_f64)
        .map(|resets| resets * 1000.0 - now_ms)
        .filter(|wait| now_ms.is_finite() && *wait > 0.0 && *wait <= CLAUDE_USAGE_LIMIT_MAX_WAIT_MS)
        .map(format_usage_limit_wait);
    format!(
        "Claude usage limit reached. This turn is paused until the {}limit resets{}.",
        label.map(|l| format!("{l} ")).unwrap_or_default(),
        wait.map(|w| format!(" in {w}")).unwrap_or_default()
    )
}

// ── native method names ──────────────────────────────────────────────────────

/// `sdkNativeMethod`.
pub fn sdk_native_method(message: &Value) -> String {
    let kind = js::template(message.get("type"));
    if let Some(subtype) = str_of(message, "subtype") {
        return format!("claude/{kind}/{subtype}");
    }
    if kind == "stream_event" {
        if let Some(stream_type) = message.get("event").and_then(|e| str_of(e, "type")) {
            if stream_type == "content_block_delta" {
                if let Some(delta_type) = message.get("event").and_then(|e| e.get("delta")).and_then(|d| str_of(d, "type")) {
                    return format!("claude/{kind}/{stream_type}/{delta_type}");
                }
            }
            return format!("claude/{kind}/{stream_type}");
        }
    }
    format!("claude/{kind}")
}

const SDK_MESSAGE_NOISE_KEYS: [&str; 7] = ["type", "subtype", "uuid", "parent_uuid", "session_id", "parent_tool_use_id", "request_id"];

/// `describeUnknownSdkMessage`.
fn describe_unknown_sdk_message(kind: &str, message: &Value) -> String {
    let mut parts = Vec::new();
    if let Some(object) = message.as_object() {
        for (key, value) in object {
            if SDK_MESSAGE_NOISE_KEYS.contains(&key.as_str()) {
                continue;
            }
            match value {
                Value::String(s) if !s.trim().is_empty() => parts.push(format!("{key}: {}", s.trim())),
                Value::Number(_) | Value::Bool(_) => parts.push(format!("{key}: {}", js::template(Some(value)))),
                _ => {}
            }
        }
    }
    if parts.is_empty() {
        return format!("{kind} (no displayable text content)");
    }
    let joined = parts.join(" · ");
    let preview = if js::utf16_len(&joined) > 280 {
        format!("{}…", js::slice_utf16(&joined, 279))
    } else {
        joined
    };
    format!("{kind} — {preview}")
}

/// `sdkNativeItemId`.
fn sdk_native_item_id(message: &Value) -> Option<String> {
    match str_of(message, "type") {
        Some("assistant") => message.get("message").and_then(|m| str_of(m, "id")).map(str::to_string),
        Some("user") => tool_result_blocks(message).first().map(|block| block.tool_use_id.clone()),
        Some("stream_event") => {
            let event = message.get("event")?;
            if str_of(event, "type") == Some("content_block_start") {
                event.get("content_block").and_then(|b| str_of(b, "id")).map(str::to_string)
            } else {
                None
            }
        }
        _ => None,
    }
}

struct ToolResultBlock {
    tool_use_id: String,
    block: Value,
    text: String,
    is_error: bool,
}

/// `extractTextContent`.
fn extract_text_content(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().map(|item| extract_text_content(Some(item))).collect(),
        Some(Value::Object(map)) => match map.get("text") {
            Some(Value::String(text)) => text.clone(),
            _ => extract_text_content(map.get("content")),
        },
        _ => String::new(),
    }
}

/// `toolResultBlocksFromUserMessage`.
fn tool_result_blocks(message: &Value) -> Vec<ToolResultBlock> {
    if str_of(message, "type") != Some("user") {
        return Vec::new();
    }
    let Some(content) = message.get("message").and_then(|m| m.get("content")).and_then(Value::as_array) else {
        return Vec::new();
    };
    content
        .iter()
        .filter(|entry| entry.is_object() && str_of(entry, "type") == Some("tool_result"))
        .filter_map(|block| {
            let tool_use_id = str_of(block, "tool_use_id").filter(|id| !id.is_empty())?.to_string();
            Some(ToolResultBlock {
                tool_use_id,
                block: block.clone(),
                text: extract_text_content(block.get("content")),
                is_error: block.get("is_error") == Some(&Value::Bool(true)),
            })
        })
        .collect()
}

/// `extractAssistantContentBlocks`.
fn assistant_content_blocks(message: &Value, block_type: &str, field: &str) -> Vec<String> {
    if str_of(message, "type") != Some("assistant") {
        return Vec::new();
    }
    message
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .map(|content| {
            content
                .iter()
                .filter(|block| block.is_object() && str_of(block, "type") == Some(block_type))
                .filter_map(|block| block.get(field).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `extractExitPlanModePlan`.
pub fn extract_exit_plan_mode_plan(value: &Value) -> Option<String> {
    value
        .get("plan")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

fn parse_json_record(text: &str) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(map)) => Some(map),
        _ => None,
    }
}

fn input_record(value: Option<&Value>) -> Map<String, Value> {
    match value {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

// ── session state ────────────────────────────────────────────────────────────

/// `AssistantTextBlockState`.
#[derive(Debug, Clone)]
struct AssistantTextBlock {
    item_id: String,
    block_index: i64,
    emitted_text_delta: bool,
    fallback_text: String,
    stream_closed: bool,
    completion_emitted: bool,
}

/// `ClaudeTurnState`.
#[derive(Debug, Clone)]
pub struct TurnState {
    pub turn_id: String,
    pub started_at: String,
    pub synthetic: bool,
    /// block index → position in `blocks`.
    text_blocks: HashMap<i64, usize>,
    blocks: Vec<AssistantTextBlock>,
    captured_proposed_plan_keys: HashSet<String>,
    latest_assistant_usage: Option<Value>,
    compacted_since_latest_assistant_usage: bool,
    has_subagents: bool,
    next_synthetic_block_index: i64,
    authentication_failure_message: Option<String>,
    rejected_rate_limit_types: HashSet<String>,
    latest_assistant_rate_limited: bool,
    emitted_thinking_text: bool,
    thinking_snapshot_ids: HashSet<String>,
}

impl TurnState {
    pub fn new(turn_id: String, started_at: String, synthetic: bool) -> Self {
        Self {
            turn_id,
            started_at,
            synthetic,
            text_blocks: HashMap::new(),
            blocks: Vec::new(),
            captured_proposed_plan_keys: HashSet::new(),
            latest_assistant_usage: None,
            compacted_since_latest_assistant_usage: false,
            has_subagents: false,
            next_synthetic_block_index: -1,
            authentication_failure_message: None,
            rejected_rate_limit_types: HashSet::new(),
            latest_assistant_rate_limited: false,
            emitted_thinking_text: false,
            thinking_snapshot_ids: HashSet::new(),
        }
    }
}

/// `ToolInFlight`.
#[derive(Debug, Clone)]
struct ToolInFlight {
    item_id: String,
    item_type: &'static str,
    tool_name: String,
    title: &'static str,
    detail: Option<String>,
    input: Map<String, Value>,
    partial_input_json: String,
    last_emitted_input_fingerprint: Option<String>,
    agent_id: Option<String>,
    parent_tool_use_id: Option<String>,
}

#[derive(Debug, Clone)]
struct ClaudeTask {
    subject: String,
    status: &'static str,
    blocked_by: IndexSet<String>,
}

/// `ClaudeTaskAgentState`.
#[derive(Debug, Clone, Default)]
struct TaskAgent {
    task_id: String,
    tool_use_id: Option<String>,
    description: Option<String>,
    subagent_type: Option<String>,
    task_type: Option<String>,
    workflow_name: Option<String>,
    run_handles: Option<Value>,
    owning_agent_id: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// The mutable `ProviderSession` fields the adapter keeps.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    pub thread_id: String,
    pub provider_instance_id: String,
    pub status: &'static str,
    pub runtime_mode: RuntimeMode,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub resume_cursor: Option<Value>,
    pub active_turn_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub last_error: Option<String>,
}

impl SessionRecord {
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        map.insert("threadId".into(), Value::String(self.thread_id.clone()));
        map.insert("provider".into(), Value::String(PROVIDER.into()));
        map.insert("providerInstanceId".into(), Value::String(self.provider_instance_id.clone()));
        map.insert("status".into(), Value::String(self.status.into()));
        map.insert("runtimeMode".into(), serde_json::to_value(self.runtime_mode).unwrap_or(Value::Null));
        if let Some(cwd) = &self.cwd {
            map.insert("cwd".into(), Value::String(cwd.clone()));
        }
        if let Some(model) = &self.model {
            map.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(cursor) = &self.resume_cursor {
            map.insert("resumeCursor".into(), cursor.clone());
        }
        if let Some(turn) = &self.active_turn_id {
            map.insert("activeTurnId".into(), Value::String(turn.clone()));
        }
        map.insert("createdAt".into(), Value::String(self.created_at.clone()));
        map.insert("updatedAt".into(), Value::String(self.updated_at.clone()));
        if let Some(error) = &self.last_error {
            map.insert("lastError".into(), Value::String(error.clone()));
        }
        Value::Object(map)
    }

    pub fn to_provider_session(&self) -> ProviderSession {
        serde_json::from_value(self.to_value()).expect("a session record is a valid ProviderSession")
    }
}

/// `ClaudeSessionContext` minus the async plumbing the adapter owns.
pub struct SessionState {
    pub session: SessionRecord,
    pub start_input: ProviderSessionStartInput,
    pub turn_start_message_ids: Vec<Option<String>>,
    pub base_permission_mode: Option<String>,
    pub current_api_model_id: Option<String>,
    pub current_effort: Option<String>,
    pub resume_session_id: Option<String>,
    /// Completed turn ids (`turns`).
    pub turns: Vec<String>,
    in_flight_tools: IndexMap<i64, ToolInFlight>,
    claude_tasks: IndexMap<String, ClaudeTask>,
    task_agents: IndexMap<String, TaskAgent>,
    pending_task_models: IndexMap<String, String>,
    workflow_member_fingerprints: HashMap<String, String>,
    pub live_task_ids: IndexSet<String>,
    pub turn_state: Option<TurnState>,
    pub last_known_context_window: Option<f64>,
    last_known_token_usage: Option<TokenUsage>,
    last_known_total_processed_tokens: Option<f64>,
    pub last_assistant_uuid: Option<String>,
    last_thread_started_id: Option<String>,
    announced_usage_limits: Option<(String, HashSet<String>)>,
    /// Signalled by `complete_turn` while an interrupt waits for Claude to abort the turn.
    pub interrupted_turn_settled: Option<tokio::sync::oneshot::Sender<()>>,
    pub stopped: bool,
}

/// One canonical event, as contract JSON.
pub type EventJson = Value;

struct EventParts<'a> {
    kind: &'a str,
    turn_id: Option<String>,
    item_id: Option<String>,
    request_id: Option<String>,
    payload: Value,
    provider_refs: Value,
    raw: Option<Value>,
}

fn raw(source: &str, method: Option<&str>, payload: Value) -> Value {
    let mut map = Map::new();
    map.insert("source".into(), Value::String(source.into()));
    if let Some(method) = method {
        map.insert("method".into(), Value::String(method.into()));
    }
    map.insert("payload".into(), payload);
    Value::Object(map)
}

fn provider_item_refs(item_id: Option<&str>) -> Value {
    match item_id.filter(|id| !id.is_empty()) {
        Some(id) => json!({ "providerItemId": id }),
        None => json!({}),
    }
}

fn insert_some(map: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        map.insert(key.into(), value);
    }
}

fn insert_str(map: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        map.insert(key.into(), Value::String(value.to_string()));
    }
}

/// What a `SessionState` was created from (`startSession`'s derived values).
pub struct SessionInit {
    pub session: SessionRecord,
    pub start_input: ProviderSessionStartInput,
    pub resume_state: Option<ClaudeResumeState>,
    pub base_permission_mode: Option<String>,
    pub current_api_model_id: Option<String>,
    pub current_effort: Option<String>,
    pub resume_session_id: Option<String>,
    pub initial_context_window: Option<f64>,
}

impl SessionState {
    pub fn new(init: SessionInit) -> Self {
        let resume = init.resume_state.clone().unwrap_or_default();
        let turn_start_message_ids = resume
            .turn_start_message_ids
            .clone()
            .unwrap_or_else(|| vec![None; resume.turn_count.unwrap_or(0)]);
        Self {
            session: init.session,
            start_input: init.start_input,
            turn_start_message_ids,
            base_permission_mode: init.base_permission_mode,
            current_api_model_id: init.current_api_model_id,
            current_effort: init.current_effort,
            resume_session_id: init.resume_session_id,
            turns: Vec::new(),
            in_flight_tools: IndexMap::new(),
            claude_tasks: IndexMap::new(),
            task_agents: IndexMap::new(),
            pending_task_models: IndexMap::new(),
            workflow_member_fingerprints: HashMap::new(),
            live_task_ids: IndexSet::new(),
            turn_state: None,
            last_known_context_window: init.initial_context_window,
            last_known_token_usage: None,
            last_known_total_processed_tokens: None,
            last_assistant_uuid: resume.resume_session_at,
            last_thread_started_id: None,
            announced_usage_limits: None,
            interrupted_turn_settled: None,
            stopped: false,
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.session.thread_id
    }

    fn current_turn_id(&self) -> Option<String> {
        self.turn_state.as_ref().map(|t| t.turn_id.clone())
    }

    fn event(&self, env: &MapperEnv, parts: EventParts<'_>) -> EventJson {
        let (event_id, created_at) = env.stamp();
        self.event_with_stamp(event_id, created_at, parts)
    }

    fn event_with_stamp(&self, event_id: String, created_at: String, parts: EventParts<'_>) -> EventJson {
        let mut map = Map::new();
        map.insert("type".into(), Value::String(parts.kind.into()));
        map.insert("eventId".into(), Value::String(event_id));
        map.insert("provider".into(), Value::String(PROVIDER.into()));
        map.insert("createdAt".into(), Value::String(created_at));
        map.insert("threadId".into(), Value::String(self.session.thread_id.clone()));
        insert_some(&mut map, "turnId", parts.turn_id.map(Value::String));
        insert_some(&mut map, "itemId", parts.item_id.map(Value::String));
        insert_some(&mut map, "requestId", parts.request_id.map(Value::String));
        map.insert("payload".into(), parts.payload);
        map.insert("providerRefs".into(), parts.provider_refs);
        insert_some(&mut map, "raw", parts.raw);
        Value::Object(map)
    }

    /// `updateResumeCursor`.
    pub fn update_resume_cursor(&mut self, env: &MapperEnv) {
        let mut cursor = Map::new();
        cursor.insert("threadId".into(), Value::String(self.session.thread_id.clone()));
        insert_str(&mut cursor, "resume", self.resume_session_id.as_deref());
        insert_str(&mut cursor, "resumeSessionAt", self.last_assistant_uuid.as_deref());
        cursor.insert("turnCount".into(), Value::from(self.turn_start_message_ids.len()));
        cursor.insert(
            "turnStartMessageIds".into(),
            Value::Array(
                self.turn_start_message_ids
                    .iter()
                    .map(|id| id.clone().map(Value::String).unwrap_or(Value::Null))
                    .collect(),
            ),
        );
        self.session.resume_cursor = Some(Value::Object(cursor));
        self.session.updated_at = env.clock.now_iso();
    }

    // ── assistant text blocks ────────────────────────────────────────────────

    /// `ensureAssistantTextBlock`: the open block at `index`, or a new one.
    fn ensure_assistant_text_block(&mut self, env: &MapperEnv, block_index: i64, fallback_text: Option<&str>, stream_closed: bool) -> Option<usize> {
        let turn = self.turn_state.as_mut()?;
        if let Some(&position) = turn.text_blocks.get(&block_index) {
            let block = &mut turn.blocks[position];
            if !block.completion_emitted {
                if block.fallback_text.is_empty() {
                    if let Some(text) = fallback_text.filter(|t| !t.is_empty()) {
                        block.fallback_text = text.to_string();
                    }
                }
                if stream_closed {
                    block.stream_closed = true;
                }
                return Some(position);
            }
        }
        let block = AssistantTextBlock {
            item_id: env.ids.next_id(),
            block_index,
            emitted_text_delta: false,
            fallback_text: fallback_text.unwrap_or_default().to_string(),
            stream_closed,
            completion_emitted: false,
        };
        turn.blocks.push(block);
        let position = turn.blocks.len() - 1;
        turn.text_blocks.insert(block_index, position);
        Some(position)
    }

    /// `completeAssistantTextBlock`.
    fn complete_assistant_text_block(
        &mut self,
        env: &MapperEnv,
        position: usize,
        force: bool,
        raw_method: Option<&str>,
        raw_payload: Option<&Value>,
        out: &mut Vec<EventJson>,
    ) {
        let Some(turn) = self.turn_state.as_ref() else { return };
        let block = turn.blocks[position].clone();
        if block.completion_emitted || (!force && !block.stream_closed) {
            return;
        }
        let turn_id = turn.turn_id.clone();
        let raw_value = (raw_method.is_some() || raw_payload.is_some()).then(|| {
            let mut map = Map::new();
            map.insert("source".into(), Value::String("claude.sdk.message".into()));
            insert_str(&mut map, "method", raw_method);
            map.insert("payload".into(), raw_payload.cloned().unwrap_or(Value::Null));
            Value::Object(map)
        });
        if !block.emitted_text_delta && !block.fallback_text.is_empty() {
            out.push(self.event(
                env,
                EventParts {
                    kind: "content.delta",
                    turn_id: Some(turn_id.clone()),
                    item_id: Some(block.item_id.clone()),
                    request_id: None,
                    payload: json!({ "streamKind": "assistant_text", "delta": block.fallback_text }),
                    provider_refs: json!({}),
                    raw: raw_value.clone(),
                },
            ));
        }
        let turn = self.turn_state.as_mut().expect("turn present");
        turn.blocks[position].completion_emitted = true;
        if turn.text_blocks.get(&block.block_index) == Some(&position) {
            turn.text_blocks.remove(&block.block_index);
        }
        let mut payload = Map::new();
        payload.insert("itemType".into(), Value::String("assistant_message".into()));
        payload.insert("status".into(), Value::String("completed".into()));
        payload.insert("title".into(), Value::String("Assistant message".into()));
        if !block.fallback_text.is_empty() {
            payload.insert("detail".into(), Value::String(block.fallback_text.clone()));
        }
        out.push(self.event(
            env,
            EventParts {
                kind: "item.completed",
                turn_id: Some(turn_id),
                item_id: Some(block.item_id),
                request_id: None,
                payload: Value::Object(payload),
                provider_refs: json!({}),
                raw: raw_value,
            },
        ));
    }

    /// `backfillAssistantTextBlocksFromSnapshot`.
    fn backfill_assistant_text_blocks(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        if self.turn_state.is_none() {
            return;
        }
        let snapshot_blocks = assistant_content_blocks(message, "text", "text");
        if snapshot_blocks.is_empty() {
            return;
        }
        let mut ordered: Vec<usize> = (0..self.turn_state.as_ref().map_or(0, |t| t.blocks.len())).collect();
        for (position, text) in snapshot_blocks.iter().enumerate() {
            let entry = match ordered.get(position) {
                Some(entry) => Some(*entry),
                None => {
                    let created = {
                        let turn = self.turn_state.as_mut().expect("turn present");
                        let index = turn.next_synthetic_block_index;
                        turn.next_synthetic_block_index -= 1;
                        index
                    };
                    let created = self.ensure_assistant_text_block(env, created, Some(text), true);
                    if let Some(created) = created {
                        ordered.push(created);
                    }
                    created
                }
            };
            let Some(entry) = entry else { continue };
            let (closed, done) = {
                let turn = self.turn_state.as_mut().expect("turn present");
                let block = &mut turn.blocks[entry];
                if block.fallback_text.is_empty() {
                    block.fallback_text = text.clone();
                }
                (block.stream_closed, block.completion_emitted)
            };
            if closed && !done {
                self.complete_assistant_text_block(env, entry, false, Some("claude/assistant"), Some(message), out);
            }
        }
    }

    /// `emitReasoningSummaryDelta`.
    fn emit_reasoning_summary_delta(
        &mut self,
        env: &MapperEnv,
        delta: &str,
        content_index: Option<Value>,
        raw_method: &str,
        raw_payload: &Value,
        out: &mut Vec<EventJson>,
    ) {
        let Some(turn) = self.turn_state.as_mut() else { return };
        if delta.is_empty() {
            return;
        }
        turn.emitted_thinking_text = true;
        let turn_id = turn.turn_id.clone();
        let mut payload = Map::new();
        payload.insert("streamKind".into(), Value::String("reasoning_summary_text".into()));
        payload.insert("delta".into(), Value::String(delta.into()));
        insert_some(&mut payload, "contentIndex", content_index);
        out.push(self.event(
            env,
            EventParts {
                kind: "content.delta",
                turn_id: Some(turn_id),
                item_id: None,
                request_id: None,
                payload: Value::Object(payload),
                provider_refs: json!({}),
                raw: Some(raw("claude.sdk.message", Some(raw_method), raw_payload.clone())),
            },
        ));
    }

    /// `backfillThinkingFromSnapshot`.
    fn backfill_thinking(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        let Some(turn) = self.turn_state.as_mut() else { return };
        if str_of(message, "type") != Some("assistant") {
            return;
        }
        let snapshot_id = js::template(message.get("uuid"));
        let already = turn.emitted_thinking_text || turn.thinking_snapshot_ids.contains(&snapshot_id);
        turn.thinking_snapshot_ids.insert(snapshot_id);
        turn.emitted_thinking_text = false;
        if already {
            return;
        }
        for (index, delta) in assistant_content_blocks(message, "thinking", "thinking").iter().enumerate() {
            self.emit_reasoning_summary_delta(env, delta, Some(Value::from(index)), "claude/assistant/thinking", message, out);
        }
        if let Some(turn) = self.turn_state.as_mut() {
            turn.emitted_thinking_text = false;
        }
    }

    /// `ensureThreadId`.
    fn ensure_thread_id(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        let Some(session_id) = js::non_empty_str(message.get("session_id")).map(str::to_string) else {
            return;
        };
        if str_of(message, "type") == Some("system") && matches!(str_of(message, "subtype"), Some("hook_started" | "hook_progress" | "hook_response")) {
            return;
        }
        self.resume_session_id = Some(session_id.clone());
        self.update_resume_cursor(env);
        if self.last_thread_started_id.as_deref() != Some(&session_id) {
            self.last_thread_started_id = Some(session_id.clone());
            out.push(self.event(
                env,
                EventParts {
                    kind: "thread.started",
                    turn_id: None,
                    item_id: None,
                    request_id: None,
                    payload: json!({ "providerThreadId": session_id }),
                    provider_refs: json!({}),
                    raw: Some(raw("claude.sdk.message", Some("claude/thread/started"), json!({ "session_id": session_id }))),
                },
            ));
        }
    }

    /// `emitRuntimeError`.
    pub fn runtime_error(&self, env: &MapperEnv, message: &str, detail: Option<Value>) -> EventJson {
        let mut payload = Map::new();
        payload.insert("message".into(), Value::String(message.into()));
        payload.insert("class".into(), Value::String("provider_error".into()));
        insert_some(&mut payload, "detail", detail);
        self.event(
            env,
            EventParts {
                kind: "runtime.error",
                turn_id: self.current_turn_id(),
                item_id: None,
                request_id: None,
                payload: Value::Object(payload),
                provider_refs: json!({}),
                raw: None,
            },
        )
    }

    /// `emitRuntimeWarning`.
    fn runtime_warning(&self, env: &MapperEnv, message: &str, detail: Option<Value>) -> EventJson {
        let mut payload = Map::new();
        payload.insert("message".into(), Value::String(message.into()));
        insert_some(&mut payload, "detail", detail);
        self.event(
            env,
            EventParts {
                kind: "runtime.warning",
                turn_id: self.current_turn_id(),
                item_id: None,
                request_id: None,
                payload: Value::Object(payload),
                provider_refs: json!({}),
                raw: None,
            },
        )
    }

    /// `emitThreadTokenUsage`.
    fn emit_thread_token_usage(
        &mut self,
        env: &MapperEnv,
        usage: Option<TokenUsage>,
        raw_method: Option<&str>,
        raw_payload: Option<Value>,
        out: &mut Vec<EventJson>,
    ) {
        let Some(usage) = usage else { return };
        self.last_known_total_processed_tokens = usage.total_processed_tokens.or(self.last_known_total_processed_tokens);
        self.last_known_token_usage = Some(usage.clone());
        let raw_value = (raw_method.is_some() || raw_payload.is_some()).then(|| {
            let mut map = Map::new();
            map.insert("source".into(), Value::String("claude.sdk.message".into()));
            insert_str(&mut map, "method", raw_method);
            map.insert("payload".into(), raw_payload.unwrap_or(Value::Null));
            Value::Object(map)
        });
        out.push(self.event(
            env,
            EventParts {
                kind: "thread.token-usage.updated",
                turn_id: self.current_turn_id(),
                item_id: None,
                request_id: None,
                payload: json!({ "usage": usage.to_value() }),
                provider_refs: json!({}),
                raw: raw_value,
            },
        ));
    }

    /// `emitProposedPlanCompleted`.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_proposed_plan_completed(
        &mut self,
        env: &MapperEnv,
        plan_markdown: &str,
        tool_use_id: Option<&str>,
        raw_source: &str,
        raw_method: &str,
        raw_payload: Value,
        out: &mut Vec<EventJson>,
    ) {
        let Some(turn) = self.turn_state.as_mut() else { return };
        let plan = plan_markdown.trim();
        if plan.is_empty() {
            return;
        }
        let key = match tool_use_id.filter(|id| !id.is_empty()) {
            Some(id) => format!("tool:{id}"),
            None => format!("plan:{plan}"),
        };
        if !turn.captured_proposed_plan_keys.insert(key) {
            return;
        }
        let turn_id = turn.turn_id.clone();
        out.push(self.event(
            env,
            EventParts {
                kind: "turn.proposed.completed",
                turn_id: Some(turn_id),
                item_id: None,
                request_id: None,
                payload: json!({ "planMarkdown": plan }),
                provider_refs: provider_item_refs(tool_use_id),
                raw: Some(raw(raw_source, Some(raw_method), raw_payload)),
            },
        ));
    }

    /// `emitClaudeTaskPlanUpdated`.
    fn emit_claude_task_plan_updated(&mut self, env: &MapperEnv, tool_use_id: &str, raw_payload: &Value, out: &mut Vec<EventJson>) {
        let plan: Vec<Value> = self
            .claude_tasks
            .values()
            .map(|task| {
                let blocked: Vec<&str> = task.blocked_by.iter().map(String::as_str).collect();
                let suffix = if blocked.is_empty() {
                    String::new()
                } else {
                    format!(" (blocked by #{})", blocked.join(", #"))
                };
                json!({ "step": format!("{}{suffix}", task.subject), "status": task.status })
            })
            .collect();
        if plan.is_empty() {
            return;
        }
        out.push(self.event(
            env,
            EventParts {
                kind: "turn.plan.updated",
                turn_id: self.current_turn_id(),
                item_id: None,
                request_id: None,
                payload: json!({ "explanation": "Claude Tasks", "plan": plan }),
                provider_refs: provider_item_refs(Some(tool_use_id)),
                raw: Some(raw("claude.sdk.message", Some("claude/user"), raw_payload.clone())),
            },
        ));
    }

    /// `applyClaudeTaskToolResult`: whether the task list changed.
    fn apply_claude_task_tool_result(&mut self, tool: &ToolInFlight, result: Option<&Map<String, Value>>) -> bool {
        if !is_claude_task_tool(&tool.tool_name) {
            return false;
        }
        if tool.tool_name == "TaskList" {
            let Some(result_tasks) = result.and_then(|r| r.get("tasks")).and_then(Value::as_array) else {
                return false;
            };
            self.claude_tasks.clear();
            for entry in result_tasks {
                let Some(task) = entry.as_object() else { continue };
                let (Some(id), Some(subject)) = (trimmed_string(task.get("id")), trimmed_string(task.get("subject"))) else {
                    continue;
                };
                self.claude_tasks.insert(
                    id,
                    ClaudeTask {
                        subject,
                        status: normalize_claude_task_status(task.get("status")),
                        blocked_by: read_string_array(task.get("blockedBy")).into_iter().collect(),
                    },
                );
            }
            return !self.claude_tasks.is_empty();
        }
        if tool.tool_name == "TaskCreate" {
            let result_task = result.and_then(|r| r.get("task")).and_then(Value::as_object);
            let id = trimmed_string(result_task.and_then(|t| t.get("id")));
            let subject = trimmed_string(result_task.and_then(|t| t.get("subject"))).or_else(|| trimmed_string(tool.input.get("subject")));
            let (Some(id), Some(subject)) = (id, subject) else { return false };
            self.claude_tasks.insert(
                id,
                ClaudeTask {
                    subject,
                    status: normalize_claude_task_status(tool.input.get("status")),
                    blocked_by: read_string_array(tool.input.get("blockedBy")).into_iter().collect(),
                },
            );
            return true;
        }
        let Some(task_id) = trimmed_string(tool.input.get("taskId")).or_else(|| trimmed_string(result.and_then(|r| r.get("taskId")))) else {
            return false;
        };
        let Some(task) = self.claude_tasks.get_mut(&task_id) else { return false };
        let mut changed = false;
        if let Some(subject) = trimmed_string(tool.input.get("subject")) {
            if task.subject != subject {
                task.subject = subject;
                changed = true;
            }
        }
        if tool.input.get("status").is_some_and(Value::is_string) {
            let status = normalize_claude_task_status(tool.input.get("status"));
            if task.status != status {
                task.status = status;
                changed = true;
            }
        }
        for dependency in read_string_array(tool.input.get("addBlockedBy")) {
            if task.blocked_by.insert(dependency) {
                changed = true;
            }
        }
        for dependency in read_string_array(tool.input.get("removeBlockedBy")) {
            if task.blocked_by.shift_remove(&dependency) {
                changed = true;
            }
        }
        changed
    }

    /// `agentIdForParentToolUse`.
    fn agent_id_for_parent_tool_use(&self, parent_tool_use_id: Option<&str>) -> Option<String> {
        let parent = parent_tool_use_id?;
        self.task_agents
            .values()
            .find(|agent| agent.tool_use_id.as_deref() == Some(parent))
            .map(|agent| agent.task_id.clone())
    }

    /// `taskLinkageFor`: entries appended to a task payload.
    fn task_linkage(&self, task_id: &str) -> Vec<(&'static str, Value)> {
        let Some(agent) = self.task_agents.get(task_id) else { return Vec::new() };
        let mut entries = Vec::new();
        let mut push = |key: &'static str, value: &Option<String>| {
            if let Some(value) = value.as_ref().filter(|v| !v.is_empty()) {
                entries.push((key, Value::String(value.clone())));
            }
        };
        push("taskType", &agent.task_type);
        push("agentId", &agent.owning_agent_id);
        push("title", &agent.description);
        push("role", &agent.subagent_type);
        push("model", &agent.model);
        push("effort", &agent.effort);
        push("toolUseId", &agent.tool_use_id);
        push("workflowName", &agent.workflow_name);
        if let Some(handles) = &agent.run_handles {
            entries.push(("runHandles", handles.clone()));
        }
        entries
    }

    // ── turn completion ──────────────────────────────────────────────────────

    /// `completeTurn(status, errorMessage, result)`.
    pub fn complete_turn(&mut self, env: &MapperEnv, status: &str, error_message: Option<&str>, result: Option<&Value>, out: &mut Vec<EventJson>) {
        let result_context_window = result.and_then(|r| r.get("modelUsage")).and_then(Value::as_object).map(|usage| {
            usage.values().fold(None::<f64>, |acc, value| {
                let window = value.get("contextWindow").and_then(Value::as_f64).unwrap_or(f64::NAN);
                Some(if acc.unwrap_or(0.0).is_nan() || window.is_nan() {
                    f64::NAN
                } else {
                    acc.unwrap_or(0.0).max(window)
                })
            })
        });
        let result_context_window = result_context_window.flatten();
        if let Some(window) = result_context_window {
            self.last_known_context_window = Some(window);
        }
        let max_tokens = result_context_window.or(self.last_known_context_window);
        let accumulated = total_processed_tokens(result.and_then(|r| r.get("usage")));
        if let Some(accumulated) = accumulated {
            self.last_known_total_processed_tokens = Some(accumulated);
        }
        let usage_record = result.and_then(|r| r.get("usage")).and_then(Value::as_object);
        let has_iteration = usage_record.is_some_and(|u| last_usage_iteration(u).is_some());
        let has_active = usage_record.is_some_and(|u| has_iteration || usage_input_tokens(u) + usage_output_tokens(u) > 0.0);
        let total_only = usage_record.is_some_and(|u| !has_active && total_processed_tokens_of_map(u).is_some());
        let total_for_snapshots = accumulated.or(self.last_known_total_processed_tokens);
        let iteration_snapshot = usage_record.and_then(|u| normalize_active_token_usage(Some(&Value::Object(u.clone())), max_tokens, total_for_snapshots));
        let latest_assistant = normalize_active_token_usage(
            self.turn_state.as_ref().and_then(|t| t.latest_assistant_usage.as_ref()),
            max_tokens,
            total_for_snapshots,
        );
        let last_good = self.last_known_token_usage.clone();
        let with_result_fields = |base: &TokenUsage| {
            let mut next = base.clone();
            if let Some(max) = max_tokens.filter(|m| m.is_finite() && *m > 0.0) {
                next.max_tokens = Some(max);
            }
            if let Some(acc) = accumulated.filter(|a| a.is_finite() && *a > base.used_tokens) {
                next.total_processed_tokens = Some(acc);
            }
            next
        };
        let compacted = self.turn_state.as_ref().is_some_and(|t| t.compacted_since_latest_assistant_usage);
        let middle = if compacted {
            None
        } else if total_only && last_good.is_some() {
            last_good.as_ref().map(with_result_fields)
        } else {
            iteration_snapshot
        };
        let usage_snapshot = latest_assistant.or(middle).or_else(|| last_good.as_ref().map(with_result_fields));
        let raw_payload = result.cloned().unwrap_or_else(|| json!({ "status": status }));

        let Some(turn) = self.turn_state.clone() else {
            self.emit_thread_token_usage(env, usage_snapshot, Some("claude/result"), Some(raw_payload), out);
            tracing::info!(thread_id = self.session.thread_id, status, num_turns = ?result.and_then(|r| r.get("num_turns")), "claude.turn.result-without-active-turn");
            return;
        };

        let tools: Vec<ToolInFlight> = self.in_flight_tools.values().cloned().collect();
        for tool in tools {
            let mut payload = Map::new();
            payload.insert("itemType".into(), Value::String(tool.item_type.into()));
            payload.insert(
                "status".into(),
                Value::String(if status == "completed" { "completed" } else { "failed" }.into()),
            );
            payload.insert("title".into(), Value::String(tool.title.into()));
            insert_str(&mut payload, "detail", tool.detail.as_deref().filter(|d| !d.is_empty()));
            payload.insert("data".into(), json!({ "toolName": tool.tool_name, "input": tool.input }));
            out.push(self.event(
                env,
                EventParts {
                    kind: "item.completed",
                    turn_id: Some(turn.turn_id.clone()),
                    item_id: Some(tool.item_id.clone()),
                    request_id: None,
                    payload: Value::Object(payload),
                    provider_refs: provider_item_refs(Some(&tool.item_id)),
                    raw: Some(raw("claude.sdk.message", Some("claude/result"), raw_payload.clone())),
                },
            ));
        }
        self.in_flight_tools.clear();

        for position in 0..turn.blocks.len() {
            self.complete_assistant_text_block(env, position, true, Some("claude/result"), Some(&raw_payload), out);
        }

        self.turns.push(turn.turn_id.clone());
        self.emit_thread_token_usage(env, usage_snapshot, Some("claude/result"), Some(raw_payload), out);

        let mut payload = Map::new();
        payload.insert("state".into(), Value::String(status.into()));
        if let Some(result) = result {
            if let Some(stop_reason) = result.get("stop_reason") {
                payload.insert("stopReason".into(), stop_reason.clone());
            }
            if js::truthy(result.get("usage")) {
                payload.insert("usage".into(), result["usage"].clone());
            }
            if js::truthy(result.get("modelUsage")) {
                payload.insert("modelUsage".into(), result["modelUsage"].clone());
            }
            if let Some(cost) = result.get("total_cost_usd").filter(|c| c.is_number()) {
                payload.insert("totalCostUsd".into(), cost.clone());
            }
        }
        insert_str(&mut payload, "errorMessage", error_message.filter(|m| !m.is_empty()));
        payload.insert("tokenUsage".into(), normalize_turn_token_usage(result, turn.has_subagents, status));
        out.push(self.event(
            env,
            EventParts {
                kind: "turn.completed",
                turn_id: Some(turn.turn_id.clone()),
                item_id: None,
                request_id: None,
                payload: Value::Object(payload),
                provider_refs: json!({}),
                raw: None,
            },
        ));

        let updated_at = env.clock.now_iso();
        self.turn_state = None;
        if let Some(settled) = self.interrupted_turn_settled.take() {
            let _ = settled.send(());
        }
        self.session.status = "ready";
        self.session.active_turn_id = None;
        self.session.updated_at = updated_at;
        if status == "failed" {
            if let Some(message) = error_message.filter(|m| !m.is_empty()) {
                self.session.last_error = Some(message.to_string());
            }
        }
        self.update_resume_cursor(env);
    }

    // ── SDK message handlers ─────────────────────────────────────────────────

    /// `logNativeSdkMessage`.
    fn log_native(&self, env: &MapperEnv, message: &Value) {
        let Some(sink) = &env.native_sink else { return };
        let observed_at = env.clock.now_iso();
        let id = message
            .get("uuid")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| env.ids.next_id());
        let mut event = Map::new();
        event.insert("id".into(), Value::String(id));
        event.insert("kind".into(), Value::String("notification".into()));
        event.insert("provider".into(), Value::String(PROVIDER.into()));
        event.insert("createdAt".into(), Value::String(observed_at.clone()));
        event.insert("method".into(), Value::String(sdk_native_method(message)));
        insert_str(&mut event, "providerThreadId", message.get("session_id").and_then(Value::as_str));
        insert_some(&mut event, "turnId", self.current_turn_id().map(Value::String));
        insert_some(&mut event, "itemId", sdk_native_item_id(message).map(Value::String));
        event.insert("payload".into(), message.clone());
        sink.write(json!({ "observedAt": observed_at, "event": Value::Object(event) }), &self.session.thread_id);
    }

    /// `handleSdkMessage`: one SDK message → canonical events.
    pub fn handle_sdk_message(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        self.log_native(env, message);
        self.ensure_thread_id(env, message, out);
        let kind = str_of(message, "type");
        if kind == Some("command_lifecycle") {
            return;
        }
        match kind {
            Some("stream_event") => self.handle_stream_event(env, message, out),
            Some("user") => self.handle_user_message(env, message, out),
            Some("assistant") => self.handle_assistant_message(env, message, out),
            Some("result") => self.handle_result_message(env, message, out),
            Some("system") => self.handle_system_message(env, message, out),
            Some("tool_progress" | "tool_use_summary" | "auth_status" | "rate_limit_event") => self.handle_telemetry_message(env, message, out),
            Some("prompt_suggestion" | "conversation_reset") => {}
            _ => {
                let description = describe_unknown_sdk_message(&format!("Claude SDK message '{}'", js::template(message.get("type"))), message);
                out.push(self.runtime_warning(env, &description, Some(message.clone())));
            }
        }
    }

    /// `handleStreamEvent`.
    fn handle_stream_event(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        let Some(event) = message.get("event") else { return };
        let event_type = str_of(event, "type");
        let parent = message.get("parent_tool_use_id").filter(|p| !p.is_null());
        if parent.is_some() {
            let drop_start = event_type == Some("content_block_start")
                && !matches!(
                    event.get("content_block").and_then(|b| str_of(b, "type")),
                    Some("tool_use" | "server_tool_use" | "mcp_tool_use")
                );
            let drop_delta = event_type == Some("content_block_delta")
                && matches!(event.get("delta").and_then(|d| str_of(d, "type")), Some("text_delta" | "thinking_delta"));
            if drop_start || drop_delta {
                return;
            }
        }
        if event_type == Some("message_start") && parent.is_none() {
            if let Some(turn) = self.turn_state.as_mut() {
                turn.emitted_thinking_text = false;
            }
        }
        match event_type {
            Some("message_delta") => {
                if parent.is_some() {
                    return;
                }
                let snapshot = normalize_active_token_usage(event.get("usage"), self.last_known_context_window, self.last_known_total_processed_tokens);
                self.emit_thread_token_usage(env, snapshot, Some("claude/stream_event/message_delta"), Some(message.clone()), out);
            }
            Some("content_block_delta") => self.handle_content_block_delta(env, message, event, out),
            Some("content_block_start") => self.handle_content_block_start(env, message, event, out),
            Some("content_block_stop") => {
                let index = event.get("index").and_then(Value::as_i64).unwrap_or(i64::MIN);
                let position = self.turn_state.as_ref().and_then(|t| t.text_blocks.get(&index).copied());
                if let Some(position) = position {
                    if let Some(turn) = self.turn_state.as_mut() {
                        turn.blocks[position].stream_closed = true;
                    }
                    self.complete_assistant_text_block(env, position, false, Some("claude/stream_event/content_block_stop"), Some(message), out);
                }
            }
            _ => {}
        }
    }

    fn handle_content_block_delta(&mut self, env: &MapperEnv, message: &Value, event: &Value, out: &mut Vec<EventJson>) {
        let delta = event.get("delta").cloned().unwrap_or(Value::Null);
        let delta_type = str_of(&delta, "type");
        if matches!(delta_type, Some("text_delta" | "thinking_delta")) && self.turn_state.is_some() {
            let field = if delta_type == Some("text_delta") { "text" } else { "thinking" };
            let text = delta.get(field).and_then(Value::as_str).unwrap_or("").to_string();
            if text.is_empty() {
                return;
            }
            let index = event.get("index").cloned();
            if delta_type.is_some_and(|t| t.contains("thinking")) {
                self.emit_reasoning_summary_delta(env, &text, index, "claude/stream_event/content_block_delta", message, out);
                return;
            }
            let block_index = event.get("index").and_then(Value::as_i64).unwrap_or(i64::MIN);
            let position = self.ensure_assistant_text_block(env, block_index, None, false);
            let item_id = position.map(|position| {
                let turn = self.turn_state.as_mut().expect("turn present");
                turn.blocks[position].emitted_text_delta = true;
                turn.blocks[position].item_id.clone()
            });
            out.push(self.event(
                env,
                EventParts {
                    kind: "content.delta",
                    turn_id: self.current_turn_id(),
                    item_id,
                    request_id: None,
                    payload: json!({ "streamKind": "assistant_text", "delta": text }),
                    provider_refs: json!({}),
                    raw: Some(raw("claude.sdk.message", Some("claude/stream_event/content_block_delta"), message.clone())),
                },
            ));
            return;
        }
        if delta_type != Some("input_json_delta") {
            return;
        }
        let index = event.get("index").and_then(Value::as_i64).unwrap_or(i64::MIN);
        let Some(tool) = self.in_flight_tools.get(&index).cloned() else { return };
        let Some(partial) = delta.get("partial_json").and_then(Value::as_str) else {
            return;
        };
        let partial_input_json = format!("{}{partial}", tool.partial_input_json);
        let parsed = parse_json_record(&partial_input_json);
        let item_type = parsed.as_ref().map_or(tool.item_type, |input| classify_tool_item_type(&tool.tool_name, input));
        let detail = parsed
            .as_ref()
            .map(|input| summarize_tool_request(&tool.tool_name, input))
            .or(tool.detail.clone());
        let mut next = tool.clone();
        next.item_type = item_type;
        next.title = title_for_tool(item_type);
        next.partial_input_json = partial_input_json;
        if let Some(input) = &parsed {
            next.input = input.clone();
        }
        if detail.is_some() {
            next.detail = detail;
        }
        let fingerprint = parsed
            .as_ref()
            .filter(|input| !input.is_empty())
            .map(|input| Value::Object(input.clone()).to_string());
        if parsed.is_none() || fingerprint.is_none() || tool.last_emitted_input_fingerprint == fingerprint {
            self.in_flight_tools.insert(index, next);
            return;
        }
        next.last_emitted_input_fingerprint = fingerprint;
        self.in_flight_tools.insert(index, next.clone());
        let mut payload = Map::new();
        payload.insert("itemType".into(), Value::String(next.item_type.into()));
        payload.insert("status".into(), Value::String("inProgress".into()));
        payload.insert("title".into(), Value::String(next.title.into()));
        insert_str(&mut payload, "detail", next.detail.as_deref().filter(|d| !d.is_empty()));
        insert_str(&mut payload, "agentId", next.agent_id.as_deref());
        insert_str(&mut payload, "parentToolUseId", next.parent_tool_use_id.as_deref());
        payload.insert("data".into(), json!({ "toolName": next.tool_name, "input": next.input }));
        out.push(self.event(
            env,
            EventParts {
                kind: "item.updated",
                turn_id: self.current_turn_id(),
                item_id: Some(next.item_id.clone()),
                request_id: None,
                payload: Value::Object(payload),
                provider_refs: provider_item_refs(Some(&next.item_id)),
                raw: Some(raw(
                    "claude.sdk.message",
                    Some("claude/stream_event/content_block_delta/input_json_delta"),
                    message.clone(),
                )),
            },
        ));
        if is_todo_tool(&next.tool_name) {
            if let Some(steps) = parsed.as_ref().and_then(plan_steps_from_todo_input).filter(|steps| !steps.is_empty()) {
                out.push(self.event(
                    env,
                    EventParts {
                        kind: "turn.plan.updated",
                        turn_id: self.current_turn_id(),
                        item_id: None,
                        request_id: None,
                        payload: json!({ "plan": steps }),
                        provider_refs: json!({}),
                        raw: None,
                    },
                ));
            }
        }
    }

    fn handle_content_block_start(&mut self, env: &MapperEnv, message: &Value, event: &Value, out: &mut Vec<EventJson>) {
        let index = event.get("index").and_then(Value::as_i64).unwrap_or(i64::MIN);
        let Some(block) = event.get("content_block") else { return };
        match str_of(block, "type") {
            Some("text") => {
                let text = block.get("text").and_then(Value::as_str).unwrap_or("").to_string();
                self.ensure_assistant_text_block(env, index, Some(&text), false);
                return;
            }
            Some("tool_use" | "server_tool_use" | "mcp_tool_use") => {}
            _ => return,
        }
        let tool_name = js::template(block.get("name"));
        let input = input_record(block.get("input"));
        let item_type = classify_tool_item_type(&tool_name, &input);
        let item_id = js::template(block.get("id"));
        let detail = summarize_tool_request(&tool_name, &input);
        let fingerprint = (!input.is_empty()).then(|| Value::Object(input.clone()).to_string());
        let parent = message.get("parent_tool_use_id").and_then(Value::as_str).map(str::to_string);
        let owning_agent = self.agent_id_for_parent_tool_use(parent.as_deref());
        let tool = ToolInFlight {
            item_id: item_id.clone(),
            item_type,
            tool_name: tool_name.clone(),
            title: title_for_tool(item_type),
            detail: Some(detail.clone()),
            input: input.clone(),
            partial_input_json: String::new(),
            last_emitted_input_fingerprint: fingerprint,
            agent_id: owning_agent.clone(),
            parent_tool_use_id: parent.clone().filter(|p| !p.is_empty()),
        };
        self.in_flight_tools.insert(index, tool);
        let mut payload = Map::new();
        payload.insert("itemType".into(), Value::String(item_type.into()));
        payload.insert("status".into(), Value::String("inProgress".into()));
        payload.insert("title".into(), Value::String(title_for_tool(item_type).into()));
        if !detail.is_empty() {
            payload.insert("detail".into(), Value::String(detail));
        }
        insert_str(&mut payload, "agentId", owning_agent.as_deref());
        insert_str(&mut payload, "parentToolUseId", parent.as_deref().filter(|p| !p.is_empty()));
        payload.insert("data".into(), json!({ "toolName": tool_name, "input": input }));
        out.push(self.event(
            env,
            EventParts {
                kind: "item.started",
                turn_id: self.current_turn_id(),
                item_id: Some(item_id.clone()),
                request_id: None,
                payload: Value::Object(payload),
                provider_refs: provider_item_refs(Some(&item_id)),
                raw: Some(raw("claude.sdk.message", Some("claude/stream_event/content_block_start"), message.clone())),
            },
        ));
    }

    /// `handleUserMessage`: tool results complete their tool items.
    fn handle_user_message(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        for result in tool_result_blocks(message) {
            let Some((index, tool)) = self
                .in_flight_tools
                .iter()
                .find(|(_, tool)| tool.item_id == result.tool_use_id)
                .map(|(i, t)| (*i, t.clone()))
            else {
                continue;
            };
            let tool_use_result = message.get("tool_use_result").and_then(Value::as_object).cloned();
            let data = json!({ "toolName": tool.tool_name, "input": tool.input, "result": result.block });
            let base_payload = |status: &str| {
                let mut payload = Map::new();
                payload.insert("itemType".into(), Value::String(tool.item_type.into()));
                payload.insert("status".into(), Value::String(status.into()));
                payload.insert("title".into(), Value::String(tool.title.into()));
                insert_str(&mut payload, "detail", tool.detail.as_deref().filter(|d| !d.is_empty()));
                insert_str(&mut payload, "agentId", tool.agent_id.as_deref());
                insert_str(&mut payload, "parentToolUseId", tool.parent_tool_use_id.as_deref());
                payload.insert("data".into(), data.clone());
                Value::Object(payload)
            };
            let raw_user = raw("claude.sdk.message", Some("claude/user"), message.clone());
            out.push(self.event(
                env,
                EventParts {
                    kind: "item.updated",
                    turn_id: self.current_turn_id(),
                    item_id: Some(tool.item_id.clone()),
                    request_id: None,
                    payload: base_payload(if result.is_error { "failed" } else { "inProgress" }),
                    provider_refs: provider_item_refs(Some(&tool.item_id)),
                    raw: Some(raw_user.clone()),
                },
            ));
            let stream_kind = match tool.item_type {
                "command_execution" => Some("command_output"),
                "file_change" => Some("file_change_output"),
                _ => None,
            };
            if let (Some(stream_kind), false, Some(turn_id)) = (stream_kind, result.text.is_empty(), self.current_turn_id()) {
                out.push(self.event(
                    env,
                    EventParts {
                        kind: "content.delta",
                        turn_id: Some(turn_id),
                        item_id: Some(tool.item_id.clone()),
                        request_id: None,
                        payload: json!({ "streamKind": stream_kind, "delta": result.text }),
                        provider_refs: provider_item_refs(Some(&tool.item_id)),
                        raw: Some(raw_user.clone()),
                    },
                ));
            }
            out.push(self.event(
                env,
                EventParts {
                    kind: "item.completed",
                    turn_id: self.current_turn_id(),
                    item_id: Some(tool.item_id.clone()),
                    request_id: None,
                    payload: base_payload(if result.is_error { "failed" } else { "completed" }),
                    provider_refs: provider_item_refs(Some(&tool.item_id)),
                    raw: Some(raw_user),
                },
            ));

            if !result.is_error && tool.tool_name.to_lowercase() == "workflow" {
                if let Some(workflow_task_id) = tool_use_result.as_ref().and_then(|r| trimmed_string(r.get("taskId"))) {
                    let result_record = tool_use_result.as_ref().expect("checked");
                    let mut handles = Map::new();
                    insert_some(&mut handles, "runId", trimmed_string(result_record.get("runId")).map(Value::String));
                    insert_some(&mut handles, "scriptPath", trimmed_string(result_record.get("scriptPath")).map(Value::String));
                    insert_some(
                        &mut handles,
                        "transcriptDir",
                        trimmed_string(result_record.get("transcriptDir")).map(Value::String),
                    );
                    insert_some(
                        &mut handles,
                        "sessionUrl",
                        sanitize_session_url(result_record.get("sessionUrl")).map(Value::String),
                    );
                    let existing = self.task_agents.get(&workflow_task_id).cloned().unwrap_or_default();
                    self.task_agents.insert(
                        workflow_task_id.clone(),
                        TaskAgent {
                            task_id: workflow_task_id,
                            tool_use_id: existing.tool_use_id.or_else(|| Some(tool.item_id.clone())),
                            task_type: existing.task_type.or_else(|| Some("local_workflow".into())),
                            run_handles: Some(Value::Object(handles)),
                            ..existing
                        },
                    );
                }
            }
            if !result.is_error && self.apply_claude_task_tool_result(&tool, tool_use_result.as_ref()) {
                self.emit_claude_task_plan_updated(env, &tool.item_id, message, out);
            }
            self.in_flight_tools.shift_remove(&index);
        }
    }

    /// Auto-start a synthetic turn (assistant output with no active turn).
    fn start_synthetic_turn(&mut self, env: &MapperEnv, message_uuid: Option<String>, out: &mut Vec<EventJson>) {
        let turn_id = env.ids.next_id();
        let started_at = env.clock.now_iso();
        self.turn_start_message_ids.push(message_uuid);
        self.turn_state = Some(TurnState::new(turn_id.clone(), started_at.clone(), true));
        self.session.status = "running";
        self.session.active_turn_id = Some(turn_id.clone());
        self.session.updated_at = started_at;
        self.update_resume_cursor(env);
        out.push(self.event(
            env,
            EventParts {
                kind: "turn.started",
                turn_id: Some(turn_id.clone()),
                item_id: None,
                request_id: None,
                payload: json!({}),
                provider_refs: json!({ "providerTurnId": turn_id }),
                raw: Some(raw("claude.sdk.message", Some("claude/synthetic-turn-start"), json!({}))),
            },
        ));
    }

    /// `handleAssistantMessage`.
    fn handle_assistant_message(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        let uuid = message.get("uuid").and_then(Value::as_str).map(str::to_string);
        if let Some(parent) = message.get("parent_tool_use_id").filter(|p| !p.is_null()) {
            let parent = js::template(Some(parent));
            let owning_task = self.agent_id_for_parent_tool_use(Some(&parent));
            if let Some(model) = trimmed_string(message.get("message").and_then(|m| m.get("model"))) {
                match owning_task.and_then(|task| self.task_agents.get_mut(&task)) {
                    Some(agent) => agent.model = Some(model),
                    None => {
                        self.pending_task_models.insert(parent, model);
                        if self.pending_task_models.len() > PENDING_TASK_MODEL_CAP {
                            self.pending_task_models.shift_remove_index(0);
                        }
                    }
                }
            }
            self.last_assistant_uuid = uuid;
            self.update_resume_cursor(env);
            return;
        }
        if self.turn_state.is_none() {
            self.start_synthetic_turn(env, uuid.clone(), out);
        }
        if let Some(content) = message.get("message").and_then(|m| m.get("content")).and_then(Value::as_array) {
            for block in content {
                if !block.is_object() || str_of(block, "type") != Some("tool_use") || str_of(block, "name") != Some("ExitPlanMode") {
                    continue;
                }
                let Some(plan) = block.get("input").and_then(extract_exit_plan_mode_plan) else {
                    continue;
                };
                self.emit_proposed_plan_completed(
                    env,
                    &plan,
                    block.get("id").and_then(Value::as_str),
                    "claude.sdk.message",
                    "claude/assistant",
                    message.clone(),
                    out,
                );
            }
        }
        if self.turn_state.is_some() {
            let error = str_of(message, "error");
            let signed_out = (error == Some("authentication_failed")).then(|| {
                let cwd = crate::home::resolve_cwd(self.session.cwd.as_deref());
                crate::home::claude_signed_out_message(env.claude_config_dir.as_deref(), &cwd.to_string_lossy())
            });
            let usage = message.get("message").and_then(|m| m.get("usage")).cloned();
            let has_usage = normalize_active_token_usage(usage.as_ref(), self.last_known_context_window, self.last_known_total_processed_tokens).is_some();
            let turn = self.turn_state.as_mut().expect("checked");
            turn.latest_assistant_rate_limited = error == Some("rate_limit");
            if let Some(message) = signed_out {
                turn.authentication_failure_message = Some(message);
            }
            if has_usage {
                turn.latest_assistant_usage = usage;
                turn.compacted_since_latest_assistant_usage = false;
            }
            self.backfill_thinking(env, message, out);
            self.backfill_assistant_text_blocks(env, message, out);
        }
        self.last_assistant_uuid = uuid;
        self.update_resume_cursor(env);
    }

    /// `handleResultMessage`.
    fn handle_result_message(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        let hint = self.turn_state.as_ref().and_then(|turn| {
            turn.authentication_failure_message.clone().or_else(|| {
                (!turn.rejected_rate_limit_types.is_empty() || turn.latest_assistant_rate_limited)
                    .then(|| "Claude usage limit reached. Send the message again once the limit resets.".to_string())
            })
        });
        let (status, error_message) = result_outcome(message, hint.as_deref());
        if status == "failed" {
            out.push(self.runtime_error(env, error_message.as_deref().unwrap_or("Claude turn failed."), None));
        }
        self.complete_turn(env, status, error_message.as_deref(), Some(message), out);
    }

    /// `handleSystemMessage`.
    fn handle_system_message(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        let (event_id, created_at) = env.stamp();
        let subtype = js::template(message.get("subtype"));
        let base_raw = json!({
            "source": "claude.sdk.message",
            "method": sdk_native_method(message),
            "messageType": format!("{}:{subtype}", js::template(message.get("type"))),
            "payload": message,
        });
        let turn_id = self.current_turn_id();
        let base_event = |state: &SessionState, kind: &str, payload: Value| {
            state.event_with_stamp(
                event_id.clone(),
                created_at.clone(),
                EventParts {
                    kind,
                    turn_id: turn_id.clone(),
                    item_id: None,
                    request_id: None,
                    payload,
                    provider_refs: json!({}),
                    raw: Some(base_raw.clone()),
                },
            )
        };
        let copy = |payload: &mut Map<String, Value>, key: &str, source: &str| {
            if let Some(value) = message.get(source) {
                payload.insert(key.into(), value.clone());
            }
        };
        match subtype.as_str() {
            "vcs_state_changed" | "code_change_published" => {}
            "init" => out.push(base_event(self, "session.configured", json!({ "config": message }))),
            "status" => {
                let status = message.get("status").filter(|s| !s.is_null());
                let state = if str_of(message, "status") == Some("compacting") {
                    "waiting"
                } else {
                    "running"
                };
                let reason = format!("status:{}", status.map(|s| js::template(Some(s))).unwrap_or_else(|| "active".into()));
                out.push(base_event(
                    self,
                    "session.state.changed",
                    json!({ "state": state, "reason": reason, "detail": message }),
                ));
            }
            "compact_boundary" => {
                if let Some(turn) = self.turn_state.as_mut() {
                    turn.latest_assistant_usage = None;
                    turn.compacted_since_latest_assistant_usage = true;
                }
                let compacted = compact_boundary_token_usage(message, self.last_known_context_window, self.last_known_total_processed_tokens);
                self.emit_thread_token_usage(env, compacted.clone(), Some("claude/system/compact_boundary"), Some(message.clone()), out);
                let mut payload = Map::new();
                payload.insert("state".into(), Value::String("compacted".into()));
                insert_some(&mut payload, "beforeTokens", compacted.as_ref().and_then(|c| c.last_used_tokens).map(js_number));
                insert_some(&mut payload, "afterTokens", compacted.as_ref().map(|c| js_number(c.used_tokens)));
                payload.insert("detail".into(), message.clone());
                out.push(base_event(self, "thread.state.changed", Value::Object(payload)));
            }
            "hook_started" => {
                let mut payload = Map::new();
                copy(&mut payload, "hookId", "hook_id");
                copy(&mut payload, "hookName", "hook_name");
                copy(&mut payload, "hookEvent", "hook_event");
                out.push(base_event(self, "hook.started", Value::Object(payload)));
            }
            "hook_progress" => {
                let mut payload = Map::new();
                copy(&mut payload, "hookId", "hook_id");
                copy(&mut payload, "output", "output");
                copy(&mut payload, "stdout", "stdout");
                copy(&mut payload, "stderr", "stderr");
                out.push(base_event(self, "hook.progress", Value::Object(payload)));
            }
            "hook_response" => {
                let mut payload = Map::new();
                copy(&mut payload, "hookId", "hook_id");
                copy(&mut payload, "outcome", "outcome");
                copy(&mut payload, "output", "output");
                copy(&mut payload, "stdout", "stdout");
                copy(&mut payload, "stderr", "stderr");
                if message.get("exit_code").is_some_and(Value::is_number) {
                    copy(&mut payload, "exitCode", "exit_code");
                }
                out.push(base_event(self, "hook.completed", Value::Object(payload)));
            }
            "task_started" => self.handle_task_started(message, base_event, out),
            "task_progress" => {
                let usage = self.task_progress_token_usage(message.get("usage"));
                self.emit_thread_token_usage(env, usage, Some("claude/system/task_progress"), Some(message.clone()), out);
                let task_id = js::template(message.get("task_id"));
                let mut payload = Map::new();
                payload.insert("taskId".into(), Value::String(task_id.clone()));
                copy(&mut payload, "description", "description");
                if js::truthy(message.get("summary")) {
                    copy(&mut payload, "summary", "summary");
                }
                if js::truthy(message.get("usage")) {
                    copy(&mut payload, "usage", "usage");
                }
                insert_some(&mut payload, "typedUsage", normalize_task_usage(message.get("usage")));
                if js::truthy(message.get("last_tool_name")) {
                    copy(&mut payload, "lastToolName", "last_tool_name");
                }
                let progress = parse_workflow_progress(message.get("workflow_progress"));
                if let Some(phases) = progress.as_ref().map(|p| &p.phases).filter(|phases| !phases.is_empty()) {
                    payload.insert(
                        "phases".into(),
                        Value::Array(
                            phases
                                .iter()
                                .map(|(index, title)| json!({ "index": js_number(*index), "title": title }))
                                .collect(),
                        ),
                    );
                }
                for (key, value) in self.task_linkage(&task_id) {
                    payload.insert(key.into(), value);
                }
                if js::truthy(message.get("subagent_type")) {
                    copy(&mut payload, "role", "subagent_type");
                }
                out.push(base_event(self, "task.progress", Value::Object(payload)));
                if let Some(progress) = progress {
                    self.emit_workflow_member_progress(env, &task_id, &progress, &turn_id, &base_raw, out);
                }
            }
            "task_updated" => {
                let task_id = js::template(message.get("task_id"));
                let patch = message.get("patch").cloned().unwrap_or(Value::Null);
                let status = patch.get("status").and_then(Value::as_str).and_then(task_patch_status);
                if matches!(status, Some("completed" | "failed" | "cancelled")) {
                    self.live_task_ids.shift_remove(&task_id);
                }
                let ended_at = js::finite(patch.get("end_time")).and_then(|ms| zc_core::time::try_iso_from_millis(ms.trunc() as i64));
                let mut payload = Map::new();
                payload.insert("taskId".into(), Value::String(task_id.clone()));
                insert_str(&mut payload, "status", status);
                if js::truthy(patch.get("description")) {
                    payload.insert("description".into(), patch["description"].clone());
                }
                if js::truthy(patch.get("error")) {
                    payload.insert("error".into(), patch["error"].clone());
                }
                insert_some(&mut payload, "endedAt", ended_at.map(Value::String));
                if let Some(backgrounded) = patch.get("is_backgrounded") {
                    payload.insert("isBackgrounded".into(), backgrounded.clone());
                }
                for (key, value) in self.task_linkage(&task_id) {
                    payload.insert(key.into(), value);
                }
                out.push(base_event(self, "task.updated", Value::Object(payload)));
            }
            "task_notification" => {
                let task_id = js::template(message.get("task_id"));
                self.live_task_ids.shift_remove(&task_id);
                let usage = self.task_progress_token_usage(message.get("usage"));
                self.emit_thread_token_usage(env, usage, Some("claude/system/task_notification"), Some(message.clone()), out);
                let mut payload = Map::new();
                payload.insert("taskId".into(), Value::String(task_id.clone()));
                copy(&mut payload, "status", "status");
                if js::truthy(message.get("summary")) {
                    copy(&mut payload, "summary", "summary");
                }
                if js::truthy(message.get("usage")) {
                    copy(&mut payload, "usage", "usage");
                }
                insert_some(&mut payload, "typedUsage", normalize_task_usage(message.get("usage")));
                if js::truthy(message.get("output_file")) {
                    copy(&mut payload, "outputFile", "output_file");
                }
                for (key, value) in self.task_linkage(&task_id) {
                    payload.insert(key.into(), value);
                }
                out.push(base_event(self, "task.completed", Value::Object(payload)));
            }
            "files_persisted" => {
                let pick = |entry: &Value, fields: &[(&str, &str)]| {
                    let mut map = Map::new();
                    for (key, source) in fields {
                        insert_some(&mut map, key, entry.get(*source).cloned());
                    }
                    Value::Object(map)
                };
                let files: Vec<Value> = message
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|files| files.iter().map(|f| pick(f, &[("filename", "filename"), ("fileId", "file_id")])).collect())
                    .unwrap_or_default();
                let mut payload = Map::new();
                payload.insert("files".into(), Value::Array(files));
                if let Some(failed) = message.get("failed").and_then(Value::as_array) {
                    payload.insert(
                        "failed".into(),
                        Value::Array(failed.iter().map(|f| pick(f, &[("filename", "filename"), ("error", "error")])).collect()),
                    );
                }
                out.push(base_event(self, "files.persisted", Value::Object(payload)));
            }
            "thinking_tokens" => {}
            "api_retry" => {
                let reason = format!(
                    "api_retry:{}/{}",
                    js::template(message.get("attempt")),
                    js::template(message.get("max_retries"))
                );
                out.push(base_event(self, "session.state.changed", json!({ "state": "running", "reason": reason })));
            }
            "session_state_changed" => {
                let state = match str_of(message, "state") {
                    Some("running") => "running",
                    Some("requires_action") => "waiting",
                    _ => "ready",
                };
                let reason = format!("session_state:{}", js::template(message.get("state")));
                out.push(base_event(self, "session.state.changed", json!({ "state": state, "reason": reason })));
            }
            "notification" => {
                if matches!(str_of(message, "priority"), Some("high" | "immediate")) {
                    out.push(self.runtime_warning(env, &js::template(message.get("text")), Some(message.clone())));
                }
            }
            "model_refusal_fallback" => out.push(self.runtime_warning(env, &js::template(message.get("content")), Some(message.clone()))),
            "local_command_output"
            | "plugin_install"
            | "commands_changed"
            | "memory_recall"
            | "elicitation_complete"
            | "background_tasks_changed"
            | "control_request_progress"
            | "worker_shutting_down" => {}
            "informational" => {
                if str_of(message, "level") == Some("warning") {
                    out.push(self.runtime_warning(env, &js::template(message.get("content")), Some(message.clone())));
                }
            }
            "model_refusal_no_fallback" => {
                let explanation = message
                    .get("api_refusal_explanation")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|e| !e.is_empty());
                let text = explanation.map(str::to_string).unwrap_or_else(|| js::template(message.get("content")));
                out.push(self.runtime_warning(env, &text, Some(message.clone())));
            }
            "permission_denied" => {
                let mut payload = Map::new();
                copy(&mut payload, "toolName", "tool_name");
                if js::truthy(message.get("tool_use_id")) {
                    copy(&mut payload, "toolUseId", "tool_use_id");
                }
                if js::truthy(message.get("decision_reason")) {
                    copy(&mut payload, "reason", "decision_reason");
                }
                if js::truthy(message.get("agent_id")) {
                    copy(&mut payload, "agentId", "agent_id");
                }
                out.push(base_event(self, "tool.denied", Value::Object(payload)));
            }
            "mirror_error" => {
                let text = format!("Claude workspace mirror error: {}", js::template(message.get("error")));
                out.push(self.runtime_error(env, &text, Some(message.clone())));
            }
            _ => {
                let description = describe_unknown_sdk_message(&format!("Claude system message '{subtype}'"), message);
                out.push(self.runtime_warning(env, &description, Some(message.clone())));
            }
        }
    }

    fn handle_task_started(&mut self, message: &Value, base_event: impl Fn(&SessionState, &str, Value) -> EventJson, out: &mut Vec<EventJson>) {
        let tool_use_id = message
            .get("tool_use_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        let launching = tool_use_id
            .as_ref()
            .and_then(|id| self.in_flight_tools.values().find(|tool| &tool.item_id == id).cloned());
        let owning_agent = launching.as_ref().and_then(|tool| tool.agent_id.clone());
        let task_type = message.get("task_type").and_then(Value::as_str).map(str::to_string);
        if self.turn_state.is_some() && classify_task_agent_kind(task_type.as_deref(), owning_agent.as_deref()) == "agent" {
            if let Some(turn) = self.turn_state.as_mut() {
                turn.has_subagents = true;
            }
        }
        let buffered = tool_use_id.as_ref().and_then(|id| self.pending_task_models.shift_remove(id));
        let launch_input = launching.as_ref().map(|tool| &tool.input);
        let model = buffered
            .or_else(|| trimmed_string(launch_input.and_then(|i| i.get("model"))))
            .or_else(|| self.session.model.as_deref().map(str::trim).filter(|m| !m.is_empty()).map(str::to_string));
        let raw_effort = launch_input.and_then(|i| i.get("effort"));
        let effort = trimmed_string(raw_effort)
            .or_else(|| {
                js::finite(raw_effort)
                    .filter(|_| raw_effort.is_some_and(Value::is_number))
                    .map(crate::js::number_to_string)
            })
            .or_else(|| self.current_effort.clone());
        let task_id = js::template(message.get("task_id"));
        let description = message.get("description").and_then(Value::as_str).map(str::to_string);
        let subagent_type = message.get("subagent_type").and_then(Value::as_str).map(str::to_string);
        let workflow_name = message.get("workflow_name").and_then(Value::as_str).map(str::to_string);
        let run_handles = self.task_agents.get(&task_id).and_then(|agent| agent.run_handles.clone());
        self.task_agents.insert(
            task_id.clone(),
            TaskAgent {
                task_id: task_id.clone(),
                tool_use_id: tool_use_id.clone(),
                description: description.clone(),
                subagent_type: subagent_type.clone(),
                task_type: task_type.clone(),
                workflow_name: workflow_name.clone(),
                run_handles,
                owning_agent_id: owning_agent.clone(),
                model: model.clone(),
                effort: effort.clone(),
            },
        );
        self.live_task_ids.insert(task_id.clone());
        let mut payload = Map::new();
        payload.insert("taskId".into(), Value::String(task_id));
        if let Some(value) = message.get("description") {
            payload.insert("description".into(), value.clone());
        }
        insert_str(&mut payload, "taskType", task_type.as_deref().filter(|t| !t.is_empty()));
        insert_str(&mut payload, "agentId", owning_agent.as_deref());
        insert_str(&mut payload, "title", description.as_deref().filter(|d| !d.is_empty()));
        insert_str(&mut payload, "role", subagent_type.as_deref().filter(|s| !s.is_empty()));
        insert_str(&mut payload, "model", model.as_deref());
        insert_str(&mut payload, "effort", effort.as_deref().filter(|e| !e.is_empty()));
        insert_str(&mut payload, "toolUseId", tool_use_id.as_deref());
        insert_str(&mut payload, "workflowName", workflow_name.as_deref().filter(|w| !w.is_empty()));
        out.push(base_event(self, "task.started", Value::Object(payload)));
    }

    /// `normalizeClaudeTaskProgressTokenUsage`.
    fn task_progress_token_usage(&self, value: Option<&Value>) -> Option<TokenUsage> {
        let total = total_processed_tokens(value).filter(|t| *t > 0.0)?;
        let last_used = self.last_known_token_usage.as_ref().map(|u| u.used_tokens);
        let active = last_used.map_or(total, |last| total.max(last));
        if last_used == Some(active) {
            return None;
        }
        let mut snapshot = make_token_usage(
            active,
            None,
            None,
            self.last_known_context_window,
            Some(total.max(self.last_known_total_processed_tokens.unwrap_or(total))),
            None,
        )?;
        let usage = value.and_then(Value::as_object);
        snapshot.tool_uses = usage.and_then(|u| finite_non_negative_integer(u.get("tool_uses")));
        snapshot.duration_ms = usage.and_then(|u| finite_non_negative_integer(u.get("duration_ms")));
        Some(snapshot)
    }

    /// `emitWorkflowMemberProgress`: one task.progress per changed workflow member.
    fn emit_workflow_member_progress(
        &mut self,
        env: &MapperEnv,
        coordinator_id: &str,
        progress: &WorkflowProgress,
        turn_id: &Option<String>,
        base_raw: &Value,
        out: &mut Vec<EventJson>,
    ) {
        for entry in &progress.agents {
            let member_task_id = format!("{coordinator_id}:wf:{}", js::number_to_string(entry.index));
            let status = workflow_agent_status(entry);
            let opt = |v: &Option<String>| v.clone().unwrap_or_default();
            let num = |v: Option<f64>| v.map(js::number_to_string).unwrap_or_default();
            let fingerprint = [
                status.to_string(),
                opt(&entry.label),
                opt(&entry.model),
                opt(&entry.last_tool_name),
                opt(&entry.error),
                num(entry.tokens),
                num(entry.tool_calls),
                num(entry.phase_index),
                opt(&entry.phase_title),
                num(entry.attempt),
            ]
            .join("\u{1f}");
            if self.workflow_member_fingerprints.get(&member_task_id) == Some(&fingerprint) {
                continue;
            }
            self.workflow_member_fingerprints.insert(member_task_id.clone(), fingerprint);
            let mut payload = Map::new();
            payload.insert("taskId".into(), Value::String(member_task_id));
            payload.insert(
                "description".into(),
                Value::String(entry.label.clone().unwrap_or_else(|| format!("agent {}", js::number_to_string(entry.index)))),
            );
            payload.insert("status".into(), Value::String(status.into()));
            insert_str(&mut payload, "error", entry.error.as_deref());
            insert_str(&mut payload, "title", entry.label.as_deref());
            insert_str(&mut payload, "model", entry.model.as_deref());
            insert_str(&mut payload, "lastToolName", entry.last_tool_name.as_deref());
            if let Some(tokens) = entry.tokens {
                let mut typed = Map::new();
                typed.insert("totalTokens".into(), js_number(tokens));
                insert_some(&mut typed, "toolUses", entry.tool_calls.map(js_number));
                payload.insert("typedUsage".into(), Value::Object(typed));
            }
            payload.insert("parentAgentId".into(), Value::String(coordinator_id.to_string()));
            payload.insert("agentIndex".into(), js_number(entry.index));
            insert_some(&mut payload, "phaseIndex", entry.phase_index.map(js_number));
            insert_str(&mut payload, "phaseTitle", entry.phase_title.as_deref());
            insert_some(&mut payload, "attempt", entry.attempt.map(js_number));
            payload.insert("timelineBypass".into(), Value::Bool(true));
            let (event_id, created_at) = env.stamp();
            out.push(self.event_with_stamp(
                event_id,
                created_at,
                EventParts {
                    kind: "task.progress",
                    turn_id: turn_id.clone(),
                    item_id: None,
                    request_id: None,
                    payload: Value::Object(payload),
                    provider_refs: json!({}),
                    raw: Some(base_raw.clone()),
                },
            ));
        }
    }

    /// `handleSdkTelemetryMessage`.
    fn handle_telemetry_message(&mut self, env: &MapperEnv, message: &Value, out: &mut Vec<EventJson>) {
        let (event_id, created_at) = env.stamp();
        let kind = js::template(message.get("type"));
        let base_raw = json!({ "source": "claude.sdk.message", "method": sdk_native_method(message), "messageType": kind, "payload": message });
        let turn_id = self.current_turn_id();
        let event = |state: &SessionState, kind: &str, payload: Value| {
            state.event_with_stamp(
                event_id.clone(),
                created_at.clone(),
                EventParts {
                    kind,
                    turn_id: turn_id.clone(),
                    item_id: None,
                    request_id: None,
                    payload,
                    provider_refs: json!({}),
                    raw: Some(base_raw.clone()),
                },
            )
        };
        let copy = |payload: &mut Map<String, Value>, key: &str, source: &str| {
            if let Some(value) = message.get(source) {
                payload.insert(key.into(), value.clone());
            }
        };
        match kind.as_str() {
            "tool_progress" => {
                let mut payload = Map::new();
                copy(&mut payload, "toolUseId", "tool_use_id");
                copy(&mut payload, "toolName", "tool_name");
                copy(&mut payload, "elapsedSeconds", "elapsed_time_seconds");
                if js::truthy(message.get("task_id")) {
                    copy(&mut payload, "taskId", "task_id");
                }
                if message.get("parent_tool_use_id").is_some_and(|p| !p.is_null()) {
                    copy(&mut payload, "parentToolUseId", "parent_tool_use_id");
                }
                out.push(event(self, "tool.progress", Value::Object(payload)));
            }
            "tool_use_summary" => {
                let mut payload = Map::new();
                copy(&mut payload, "summary", "summary");
                if message
                    .get("preceding_tool_use_ids")
                    .and_then(Value::as_array)
                    .is_some_and(|ids| !ids.is_empty())
                {
                    copy(&mut payload, "precedingToolUseIds", "preceding_tool_use_ids");
                }
                out.push(event(self, "tool.summary", Value::Object(payload)));
            }
            "auth_status" => {
                let mut payload = Map::new();
                copy(&mut payload, "isAuthenticating", "isAuthenticating");
                copy(&mut payload, "output", "output");
                if js::truthy(message.get("error")) {
                    copy(&mut payload, "error", "error");
                }
                out.push(event(self, "auth.status", Value::Object(payload)));
            }
            "rate_limit_event" => {
                let Some(info) = message.get("rate_limit_info").filter(|info| js::truthy(Some(info))) else {
                    return;
                };
                let names = env.scoped_limit_names.read().map(|n| n.clone()).unwrap_or_default();
                if let Some(limits) = claude_rate_limit_event_to_update(info, &names) {
                    out.push(event(self, "account.rate-limits.updated", json!({ "limits": limits })));
                }
                let overage_status = str_of(info, "overageStatus");
                let overage_allowed = matches!(overage_status, Some("allowed" | "allowed_warning"))
                    || info.get("isUsingOverage") == Some(&Value::Bool(true))
                    || info.get("overageInUse") == Some(&Value::Bool(true));
                let status = str_of(info, "status");
                let blocked = status == Some("rejected") && !overage_allowed;
                let limit_type = info
                    .get("rateLimitType")
                    .filter(|t| !t.is_null())
                    .map(|t| js::template(Some(t)))
                    .unwrap_or_else(|| "unknown".into());
                let resets = info
                    .get("resetsAt")
                    .filter(|r| !r.is_null())
                    .map(|r| js::template(Some(r)))
                    .unwrap_or_else(|| "unknown".into());
                let limit_key = format!("{limit_type}:{resets}");
                if let Some(turn) = self.turn_state.as_mut() {
                    if blocked {
                        turn.rejected_rate_limit_types.insert(limit_type.clone());
                    } else if matches!(status, Some("allowed" | "allowed_warning")) || overage_allowed {
                        turn.rejected_rate_limit_types.remove(&limit_type);
                    }
                }
                if blocked {
                    if let Some(turn_id) = self.current_turn_id() {
                        if self.announced_usage_limits.as_ref().map(|(id, _)| id) != Some(&turn_id) {
                            self.announced_usage_limits = Some((turn_id, HashSet::new()));
                        }
                        let announced = &mut self.announced_usage_limits.as_mut().expect("set above").1;
                        if announced.insert(limit_key) {
                            let now = zc_core::time::parse_iso_millis(&created_at).map(|ms| ms as f64).unwrap_or(f64::NAN);
                            let notice = describe_usage_limit(info, now, names.overage_included.as_deref());
                            out.push(self.runtime_warning(env, &notice, Some(info.clone())));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // ── session lifecycle helpers (used by the adapter) ──────────────────────

    /// `sendTurn`'s new-turn branch: open the turn and emit `turn.started`.
    pub fn begin_turn(&mut self, env: &MapperEnv, turn_id: &str, model: Option<&str>, out: &mut Vec<EventJson>) {
        let started_at = env.clock.now_iso();
        self.turn_state = Some(TurnState::new(turn_id.to_string(), started_at, false));
        self.session.status = "running";
        self.session.active_turn_id = Some(turn_id.to_string());
        self.session.updated_at = env.clock.now_iso();
        let payload = match model.filter(|m| !m.is_empty()) {
            Some(model) => json!({ "model": model }),
            None => json!({}),
        };
        out.push(self.event(
            env,
            EventParts {
                kind: "turn.started",
                turn_id: Some(turn_id.to_string()),
                item_id: None,
                request_id: None,
                payload,
                provider_refs: json!({}),
                raw: None,
            },
        ));
    }

    /// The `task.completed {status: "stopped"}` rows `stopSessionInternal` emits for live tasks.
    pub fn settle_live_tasks(&mut self, env: &MapperEnv, out: &mut Vec<EventJson>) {
        let ids: Vec<String> = self.live_task_ids.drain(..).collect();
        for task_id in ids {
            let mut payload = Map::new();
            payload.insert("taskId".into(), Value::String(task_id.clone()));
            payload.insert("status".into(), Value::String("stopped".into()));
            for (key, value) in self.task_linkage(&task_id) {
                payload.insert(key.into(), value);
            }
            out.push(self.event(
                env,
                EventParts {
                    kind: "task.completed",
                    turn_id: self.current_turn_id(),
                    item_id: None,
                    request_id: None,
                    payload: Value::Object(payload),
                    provider_refs: json!({}),
                    raw: None,
                },
            ));
        }
    }

    /// A `request.resolved` event.
    pub fn request_resolved(
        &self,
        env: &MapperEnv,
        request_id: &str,
        request_type: &str,
        decision: &str,
        tool_use_id: Option<&str>,
        raw_value: Option<Value>,
    ) -> EventJson {
        self.event(
            env,
            EventParts {
                kind: "request.resolved",
                turn_id: self.current_turn_id(),
                item_id: None,
                request_id: Some(request_id.to_string()),
                payload: json!({ "requestType": request_type, "decision": decision }),
                provider_refs: provider_item_refs(tool_use_id),
                raw: raw_value,
            },
        )
    }

    /// A `request.opened` event.
    #[allow(clippy::too_many_arguments)]
    pub fn request_opened(
        &self,
        env: &MapperEnv,
        request_id: &str,
        request_type: &str,
        detail: &str,
        tool_name: &str,
        input: &Value,
        tool_use_id: Option<&str>,
    ) -> EventJson {
        let mut args = Map::new();
        args.insert("toolName".into(), Value::String(tool_name.into()));
        args.insert("input".into(), input.clone());
        insert_str(&mut args, "toolUseId", tool_use_id.filter(|id| !id.is_empty()));
        self.event(
            env,
            EventParts {
                kind: "request.opened",
                turn_id: self.current_turn_id(),
                item_id: None,
                request_id: Some(request_id.to_string()),
                payload: json!({ "requestType": request_type, "detail": detail, "args": args }),
                provider_refs: provider_item_refs(tool_use_id),
                raw: Some(raw(
                    "claude.sdk.permission",
                    Some("canUseTool/request"),
                    json!({ "toolName": tool_name, "input": input }),
                )),
            },
        )
    }

    /// A `user-input.requested` event.
    pub fn user_input_requested(&self, env: &MapperEnv, request_id: &str, questions: &Value, tool_input: &Value, tool_use_id: Option<&str>) -> EventJson {
        self.event(
            env,
            EventParts {
                kind: "user-input.requested",
                turn_id: self.current_turn_id(),
                item_id: None,
                request_id: Some(request_id.to_string()),
                payload: json!({ "questions": questions }),
                provider_refs: provider_item_refs(tool_use_id),
                raw: Some(raw(
                    "claude.sdk.permission",
                    Some("canUseTool/AskUserQuestion"),
                    json!({ "toolName": "AskUserQuestion", "input": tool_input }),
                )),
            },
        )
    }

    /// A `user-input.resolved` event.
    pub fn user_input_resolved(&self, env: &MapperEnv, request_id: &str, answers: &Value, tool_use_id: Option<&str>) -> EventJson {
        self.event(
            env,
            EventParts {
                kind: "user-input.resolved",
                turn_id: self.current_turn_id(),
                item_id: None,
                request_id: Some(request_id.to_string()),
                payload: json!({ "answers": answers }),
                provider_refs: provider_item_refs(tool_use_id),
                raw: Some(raw(
                    "claude.sdk.permission",
                    Some("canUseTool/AskUserQuestion/resolved"),
                    json!({ "answers": answers }),
                )),
            },
        )
    }

    /// A session-level event without turn (`session.*`).
    pub fn session_event(&self, env: &MapperEnv, kind: &str, payload: Value) -> EventJson {
        self.event(
            env,
            EventParts {
                kind,
                turn_id: None,
                item_id: None,
                request_id: None,
                payload,
                provider_refs: json!({}),
                raw: None,
            },
        )
    }
}

/// `normalizeClaudeTurnTokenUsage`.
fn normalize_turn_token_usage(result: Option<&Value>, has_subagents: bool, terminal_status: &str) -> Value {
    let unavailable = || json!({ "usageStatus": "unavailable", "usageScope": "main_agent", "hasSubagents": has_subagents });
    let Some(usage) = result.and_then(|r| r.get("usage")).filter(|u| js::truthy(Some(u))) else {
        return unavailable();
    };
    let uncached = finite_non_negative_integer(usage.get("input_tokens"));
    let cached = finite_non_negative_integer(usage.get("cache_read_input_tokens"));
    let creation = finite_non_negative_integer(usage.get("cache_creation_input_tokens"));
    let output = finite_non_negative_integer(usage.get("output_tokens"));
    let thinking = finite_non_negative_integer(usage.get("output_tokens_details").and_then(|d| d.get("thinking_tokens")));
    let cached_contribution = if usage.get("cache_read_input_tokens").is_none_or(Value::is_null) {
        Some(0.0)
    } else {
        cached
    };
    let creation_contribution = if usage.get("cache_creation_input_tokens").is_none_or(Value::is_null) {
        Some(0.0)
    } else {
        creation
    };
    let input = match (uncached, cached_contribution, creation_contribution) {
        (Some(a), Some(b), Some(c)) => Some(a + b + c),
        _ => None,
    };
    let known = uncached.is_some() || cached.is_some() || creation.is_some() || output.is_some();
    let positive = uncached.unwrap_or(0.0) + cached.unwrap_or(0.0) + creation.unwrap_or(0.0) + output.unwrap_or(0.0) > 0.0;
    let subtype = result.and_then(|r| str_of(r, "subtype"));
    if !known || (subtype != Some("success") && !positive) {
        return unavailable();
    }
    let mut map = Map::new();
    map.insert("usageScope".into(), Value::String("main_agent".into()));
    insert_some(&mut map, "cachedInputTokens", cached.map(js_number));
    insert_some(&mut map, "cacheCreationTokens", creation.map(js_number));
    if let (Some(thinking), Some(output)) = (thinking, output) {
        map.insert("reasoningTokens".into(), js_number(output.min(thinking)));
    }
    map.insert("hasSubagents".into(), Value::Bool(has_subagents));
    if terminal_status == "completed" && subtype == Some("success") && input.is_some() && output.is_some() {
        map.insert("usageStatus".into(), Value::String("complete".into()));
        map.insert("inputTokens".into(), js_number(input.unwrap_or(0.0)));
        map.insert("outputTokens".into(), js_number(output.unwrap_or(0.0)));
    } else {
        map.insert("usageStatus".into(), Value::String("partial".into()));
        insert_some(&mut map, "inputTokens", input.map(js_number));
        insert_some(&mut map, "outputTokens", output.map(js_number));
    }
    Value::Object(map)
}

struct WorkflowAgentEntry {
    index: f64,
    state: String,
    label: Option<String>,
    phase_index: Option<f64>,
    phase_title: Option<String>,
    model: Option<String>,
    attempt: Option<f64>,
    last_tool_name: Option<String>,
    started_at: Option<String>,
    error: Option<String>,
    tokens: Option<f64>,
    tool_calls: Option<f64>,
}

struct WorkflowProgress {
    phases: Vec<(f64, String)>,
    agents: Vec<WorkflowAgentEntry>,
}

/// `parseWorkflowProgress`.
fn parse_workflow_progress(value: Option<&Value>) -> Option<WorkflowProgress> {
    let entries = value?.as_array().filter(|entries| !entries.is_empty())?;
    let mut phases: IndexMap<u64, String> = IndexMap::new();
    let mut agents: IndexMap<u64, WorkflowAgentEntry> = IndexMap::new();
    for entry in entries {
        if !entry.is_object() {
            continue;
        }
        let entry_type = trimmed_string(entry.get("type"));
        let index = non_negative_int(entry.get("index"));
        match entry_type.as_deref() {
            Some("workflow_phase") => {
                if let (Some(index), Some(title)) = (index, trimmed_string(entry.get("title"))) {
                    phases.entry(index as u64).or_insert(title);
                }
            }
            Some("workflow_agent") => {
                let state = trimmed_string(entry.get("state"));
                let (Some(index), Some(state)) = (index, state) else { continue };
                if agents.contains_key(&(index as u64)) {
                    continue;
                }
                agents.insert(
                    index as u64,
                    WorkflowAgentEntry {
                        index,
                        state,
                        label: trimmed_string(entry.get("label")),
                        phase_index: non_negative_int(entry.get("phaseIndex")),
                        phase_title: trimmed_string(entry.get("phaseTitle")),
                        model: trimmed_string(entry.get("model")),
                        attempt: non_negative_int(entry.get("attempt")),
                        last_tool_name: trimmed_string(entry.get("lastToolName")),
                        started_at: trimmed_string(entry.get("startedAt")),
                        error: trimmed_string(entry.get("error")),
                        tokens: non_negative_int(entry.get("tokens")),
                        tool_calls: non_negative_int(entry.get("toolCalls")),
                    },
                );
            }
            _ => {}
        }
    }
    if phases.is_empty() && agents.is_empty() {
        return None;
    }
    let mut phases: Vec<(f64, String)> = phases.into_iter().map(|(index, title)| (index as f64, title)).collect();
    phases.sort_by(|a, b| a.0.total_cmp(&b.0));
    phases.truncate(WORKFLOW_PHASE_CAP);
    let mut agents: Vec<WorkflowAgentEntry> = agents.into_values().collect();
    agents.sort_by(|a, b| a.index.total_cmp(&b.index));
    agents.truncate(WORKFLOW_AGENT_CAP);
    Some(WorkflowProgress { phases, agents })
}

/// `workflowAgentStatus`.
fn workflow_agent_status(entry: &WorkflowAgentEntry) -> &'static str {
    match entry.state.as_str() {
        "queued" | "pending" => "pending",
        "done" => "completed",
        "error" => "failed",
        _ => {
            if entry.started_at.is_none() {
                "pending"
            } else {
                "running"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_tools_like_the_ts_adapter() {
        let empty = Map::new();
        assert_eq!(classify_tool_item_type("Bash", &empty), "command_execution");
        assert_eq!(classify_tool_item_type("Task", &empty), "collab_agent_tool_call");
        assert_eq!(classify_tool_item_type("Agent", &empty), "collab_agent_tool_call");
        assert_eq!(classify_tool_item_type("Edit", &empty), "file_change");
        assert_eq!(classify_tool_item_type("mcp__x__y", &empty), "mcp_tool_call");
        assert_eq!(classify_tool_item_type("WebSearch", &empty), "web_search");
        assert_eq!(classify_tool_item_type("Grep", &empty), "dynamic_tool_call");
        let image: Map<String, Value> = serde_json::from_value(json!({"file_path": "/tmp/a.PNG"})).unwrap();
        assert_eq!(classify_tool_item_type("Read", &image), "image_view");
        assert_eq!(classify_request_type("Read"), "file_read_approval");
        assert_eq!(classify_request_type("Bash"), "command_execution_approval");
        assert_eq!(classify_request_type("Write"), "file_change_approval");
        assert_eq!(classify_request_type("Agent"), "dynamic_tool_call");
    }

    #[test]
    fn summarizes_tool_requests() {
        let command: Map<String, Value> = serde_json::from_value(json!({"command": "  ls -la  "})).unwrap();
        assert_eq!(summarize_tool_request("Bash", &command), "Bash: ls -la");
        let agent: Map<String, Value> = serde_json::from_value(json!({"description": "Explore", "prompt": "long"})).unwrap();
        assert_eq!(summarize_tool_request("Task", &agent), "Explore");
        let other: Map<String, Value> = serde_json::from_value(json!({"pattern": "x", "n": 1.0})).unwrap();
        assert_eq!(summarize_tool_request("Grep", &other), r#"Grep: {"pattern":"x","n":1}"#);
        let long: Map<String, Value> = serde_json::from_value(json!({"text": "x".repeat(500)})).unwrap();
        let summary = summarize_tool_request("Grep", &long);
        assert!(summary.ends_with("...") && summary.len() == "Grep: ".len() + 397 + 3);
    }

    #[test]
    fn reads_resume_cursors() {
        let state = read_claude_resume_state(Some(&json!({
            "threadId": "claude-thread-1", "sessionId": "550e8400-e29b-41d4-a716-446655440000",
            "resumeSessionAt": "x", "turnCount": 2, "turnStartMessageIds": [null, "a"]
        })))
        .unwrap();
        assert_eq!(state.thread_id, None);
        assert_eq!(state.resume.as_deref(), Some("550e8400-e29b-41d4-a716-446655440000"));
        assert_eq!(state.turn_count, Some(2));
        assert_eq!(state.turn_start_message_ids, Some(vec![None, Some("a".into())]));
        assert_eq!(read_claude_resume_state(Some(&json!({"resume": "not-a-uuid"}))).unwrap().resume, None);
        assert_eq!(read_claude_resume_state(Some(&json!({"turnCount": 1.5}))).unwrap().turn_count, None);
    }

    #[test]
    fn classifies_results() {
        assert_eq!(result_outcome(&json!({"subtype": "success", "is_error": false}), None), ("completed", None));
        assert_eq!(
            result_outcome(&json!({"subtype": "success", "is_error": false, "api_error_status": 529}), None),
            ("failed", Some("Claude API is overloaded (529). Try again shortly.".into()))
        );
        assert_eq!(
            result_outcome(
                &json!({"subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_tools", "errors": ["[ede_diagnostic] x"]}),
                None
            ),
            ("interrupted", None)
        );
        assert_eq!(
            result_outcome(&json!({"subtype": "error_during_execution", "is_error": true, "errors": ["Cancelled"]}), None),
            ("cancelled", Some("Cancelled".into()))
        );
        assert_eq!(
            result_outcome(&json!({"subtype": "error_max_turns", "is_error": true, "errors": ["Boom"]}), None),
            ("failed", Some("Boom".into()))
        );
        assert_eq!(
            result_outcome(&json!({"subtype": "success", "is_error": true}), Some("hint")),
            ("failed", Some("hint".into()))
        );
    }

    #[test]
    fn describes_usage_limits_with_the_remaining_wait() {
        let info = json!({"rateLimitType": "five_hour", "resetsAt": 7200});
        assert_eq!(
            describe_usage_limit(&info, 0.0, None),
            "Claude usage limit reached. This turn is paused until the 5-hour limit resets in 2h."
        );
        let info = json!({"rateLimitType": "seven_day_overage_included", "resetsAt": 61});
        assert_eq!(
            describe_usage_limit(&info, 0.0, Some("Fable")),
            "Claude usage limit reached. This turn is paused until the 7-day Fable limit resets in 2m."
        );
        assert_eq!(
            describe_usage_limit(&json!({}), 0.0, None),
            "Claude usage limit reached. This turn is paused until the limit resets."
        );
    }
}
