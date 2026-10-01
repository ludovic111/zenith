//! The pure parts of `ProviderRuntimeIngestion.ts`: the markdown-aware split of buffered
//! assistant text, the runtime event → thread activity mapping, and their helpers.

use serde_json::{json, Map, Value};

use crate::activity_payload::project_activity_payload;
use crate::js::{len16, str_of, trim, truncate_detail, truthy, Obj};
use crate::registries::classify_task_agent_kind;

/// `TOOL_LIFECYCLE_ITEM_TYPES`.
pub const TOOL_LIFECYCLE_ITEM_TYPES: &[&str] = &[
    "command_execution",
    "file_change",
    "mcp_tool_call",
    "dynamic_tool_call",
    "collab_agent_tool_call",
    "web_search",
    "image_view",
];

/// `isToolLifecycleItemType(value)`.
pub fn is_tool_lifecycle_item_type(value: Option<&str>) -> bool {
    value.is_some_and(|value| TOOL_LIFECYCLE_ITEM_TYPES.contains(&value))
}

// ---------------------------------------------------------------------------------------------
// splitBufferedAssistantText

/// `^( *)(`{3,}|~{3,})`: `(indent, marker)`.
fn fence(line: &str) -> Option<(usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let first = rest.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = rest.len() - rest.trim_start_matches(first).len();
    (run >= 3).then(|| (indent, &rest[..run]))
}

/// `^[ \t]*$`.
fn is_blank(line: &str) -> bool {
    line.chars().all(|character| character == ' ' || character == '\t')
}

/// `^[ \t]*(?:[-*+]|\d{1,9}[.)])[ \t]`.
fn is_list_item_start(line: &str) -> bool {
    let rest = line.trim_start_matches([' ', '\t']);
    let bytes = rest.as_bytes();
    let after_marker = match bytes.first() {
        Some(b'-' | b'*' | b'+') => 1,
        Some(byte) if byte.is_ascii_digit() => {
            let digits = bytes.iter().take_while(|byte| byte.is_ascii_digit()).count();
            if digits > 9 || !matches!(bytes.get(digits), Some(b'.' | b')')) {
                return false;
            }
            digits + 1
        }
        _ => return false,
    };
    matches!(bytes.get(after_marker), Some(b' ' | b'\t'))
}

/// `#{1,6}(?:[ \t]|$)` at the start of `text`.
fn atx_heading(text: &str) -> bool {
    let hashes = text.len() - text.trim_start_matches('#').len();
    (1..=6).contains(&hashes) && matches!(text.as_bytes().get(hashes), None | Some(b' ' | b'\t'))
}

/// `^ {0,3}(?:#{1,6}(?:[ \t]|$)|\*\*(?:[^*]|\*(?!\*))+\*\*:?$)`.
fn is_section_title(line: &str) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    if atx_heading(rest) {
        return true;
    }
    let Some(body) = rest.strip_prefix("**") else { return false };
    let inner = body.strip_suffix("**:").or_else(|| body.strip_suffix("**"));
    inner.is_some_and(|inner| !inner.is_empty() && !inner.contains("**") && !inner.ends_with('*'))
}

/// `^#{1,6}(?:[ \t]|$)`.
fn is_top_level_heading(line: &str) -> bool {
    atx_heading(line)
}

/// `splitBufferedAssistantText(text)`: splits at the last blank line, closing fence or list item
/// start that is not inside an open fenced block. `ready` will not change shape as more text
/// arrives; `rest` stays buffered. A section title holds the boundary until content follows.
pub fn split_buffered_assistant_text(text: &str) -> (String, String) {
    let mut open_fence: Option<(String, usize)> = None;
    let mut boundary: Option<usize> = None;
    let mut line_start = 0usize;
    let mut title_awaiting_content = false;
    loop {
        let newline = text[line_start..].find('\n').map(|offset| line_start + offset);
        let raw_line = &text[line_start..newline.unwrap_or(text.len())];
        let line = raw_line.trim_end_matches([' ', '\t', '\r']);
        if open_fence.is_none() && line_start > 0 && !title_awaiting_content && is_list_item_start(line) {
            boundary = Some(line_start);
        }
        let Some(newline) = newline else { break };
        if let Some((indent, marker)) = fence(line) {
            match &open_fence {
                None => {
                    open_fence = Some((marker.to_owned(), indent));
                    title_awaiting_content = false;
                }
                Some((open_marker, open_indent)) => {
                    if marker.as_bytes()[0] == open_marker.as_bytes()[0]
                        && marker.len() >= open_marker.len()
                        && indent <= open_indent + 3
                        && line.len() == indent + marker.len()
                    {
                        open_fence = None;
                        boundary = Some(newline + 1);
                    }
                }
            }
        } else if open_fence.is_none() && is_blank(line) && line_start > 0 {
            if !title_awaiting_content {
                boundary = Some(newline + 1);
            }
        } else if open_fence.is_none() {
            if line_start > 0 && !title_awaiting_content && is_top_level_heading(line) {
                boundary = Some(line_start);
            }
            title_awaiting_content = is_section_title(line);
        }
        line_start = newline + 1;
    }
    match boundary {
        None => (String::new(), text.to_owned()),
        Some(boundary) => (text[..boundary].to_owned(), text[boundary..].to_owned()),
    }
}

/// `hasRenderableAssistantText(text)`.
pub fn has_renderable_text(text: &str) -> bool {
    !trim(text).is_empty()
}

// ---------------------------------------------------------------------------------------------
// formatTokens

/// `Number.prototype.toFixed(digits)` (ties round up, as JS does on exact ties).
fn js_to_fixed(value: f64, digits: usize) -> String {
    let scale = 10f64.powi(digits as i32);
    let scaled = value.abs() * scale;
    let fraction = scaled - scaled.floor();
    if (fraction - 0.5).abs() < 1e-9 {
        let rounded = (scaled.floor() + 1.0) / scale;
        let text = format!("{:.*}", digits, rounded);
        return if value < 0.0 { format!("-{text}") } else { text };
    }
    format!("{:.*}", digits, value)
}

fn format_tokens_trim(value: f64) -> String {
    let abs = value.abs();
    let digits = if abs >= 100.0 {
        0
    } else if abs >= 10.0 {
        1
    } else {
        2
    };
    let fixed = js_to_fixed(value, digits);
    match fixed.find('.') {
        Some(dot) if fixed[dot + 1..].chars().all(|character| character == '0') => fixed[..dot].to_owned(),
        _ => fixed,
    }
}

/// `formatTokens(value)` (`@t3tools/shared/usageFormat`): three significant figures and a unit.
pub fn format_tokens(value: f64) -> String {
    let abs = value.abs();
    if abs >= 1e12 {
        format!("{}T", format_tokens_trim(value / 1e12))
    } else if abs >= 1e9 {
        format!("{}B", format_tokens_trim(value / 1e9))
    } else if abs >= 1e6 {
        format!("{}M", format_tokens_trim(value / 1e6))
    } else if abs >= 1e3 {
        format!("{}K", format_tokens_trim(value / 1e3))
    } else {
        format!("{}", (value + 0.5).floor() as i64)
    }
}

// ---------------------------------------------------------------------------------------------
// runtimeEventToActivities

/// `requestKindFromCanonicalRequestType`.
pub fn request_kind_from_canonical_request_type(request_type: Option<&str>) -> Option<&'static str> {
    match request_type? {
        "command_execution_approval" | "exec_command_approval" => Some("command"),
        "file_read_approval" => Some("file-read"),
        "file_change_approval" | "apply_patch_approval" => Some("file-change"),
        "mcp_elicitation_approval" => Some("mcp-elicitation"),
        "permission_approval" => Some("permission"),
        _ => None,
    }
}

const TASK_LINKAGE_KEYS: &[&str] = &[
    "taskType",
    "agentId",
    "title",
    "role",
    "model",
    "effort",
    "toolUseId",
    "parentAgentId",
    "workflowName",
    "agentIndex",
    "phaseIndex",
    "phaseTitle",
    "phases",
    "attempt",
    "runHandles",
    "outputFile",
    "agentPath",
    "timelineBypass",
    "typedUsage",
    "status",
    "error",
];

/// `taskLinkageActivityFields(payload)`.
pub fn task_linkage_activity_fields(payload: &Value) -> Map<String, Value> {
    let mut fields = Map::new();
    fields.insert(
        "agentKind".into(),
        Value::String(classify_task_agent_kind(str_of(payload, "taskType"), str_of(payload, "agentId")).to_owned()),
    );
    for key in TASK_LINKAGE_KEYS {
        if let Some(value) = payload.get(*key) {
            fields.insert((*key).into(), value.clone());
        }
    }
    fields
}

/// `toTurnId(event.turnId)`.
pub fn event_turn_id(event: &Value) -> Option<String> {
    match event.get("turnId") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(other) => Some(other.to_string()),
    }
}

fn turn_id_value(event: &Value) -> Value {
    event_turn_id(event).map(Value::String).unwrap_or(Value::Null)
}

fn activity(event: &Value, id: Value, tone: &str, kind: &str, summary: String, payload: Value) -> Value {
    let mut built = Obj::new()
        .set("id", id)
        .set("createdAt", event.get("createdAt").cloned().unwrap_or(Value::Null))
        .set("tone", tone)
        .set("kind", kind)
        .set("summary", summary)
        .set("payload", payload)
        .set("turnId", turn_id_value(event));
    if let Some(sequence) = event.get("sessionSequence") {
        built = built.set("sequence", sequence.clone());
    }
    built.build()
}

fn event_id_value(event: &Value) -> Value {
    event.get("eventId").cloned().unwrap_or(Value::Null)
}

fn text<'a>(payload: &'a Value, key: &str) -> &'a str {
    str_of(payload, key).unwrap_or("")
}

/// `runtimeEventToActivities(event, taskTitle?)`.
pub fn runtime_event_to_activities(event: &Value, task_title: Option<&str>) -> Vec<Value> {
    let payload = event.get("payload").cloned().unwrap_or(Value::Null);
    let payload = &payload;
    let id = event_id_value(event);
    let event_type = str_of(event, "type").unwrap_or("");
    match event_type {
        "request.opened" => {
            if str_of(payload, "requestType") == Some("tool_user_input") {
                return Vec::new();
            }
            let request_kind = request_kind_from_canonical_request_type(str_of(payload, "requestType"));
            let summary = match request_kind {
                Some("command") => "Command approval requested",
                Some("file-read") => "File-read approval requested",
                Some("file-change") => "File-change approval requested",
                Some("mcp-elicitation") => "App access approval requested",
                Some("permission") => "App permission approval requested",
                _ => "Approval requested",
            };
            let body = Obj::new()
                .copy_defined(event, "requestId")
                .set_if(request_kind.is_some(), "requestKind", || json!(request_kind))
                .set("requestType", payload.get("requestType").cloned().unwrap_or(Value::Null))
                .copy_truthy(payload, "detail")
                .copy_truthy(payload, "appName")
                .copy_truthy(payload, "options")
                .build();
            vec![activity(event, id, "approval", "approval.requested", summary.into(), body)]
        }
        "request.resolved" => {
            if str_of(payload, "requestType") == Some("tool_user_input") {
                return Vec::new();
            }
            let request_kind = request_kind_from_canonical_request_type(str_of(payload, "requestType"));
            let body = Obj::new()
                .copy_defined(event, "requestId")
                .set_if(request_kind.is_some(), "requestKind", || json!(request_kind))
                .set("requestType", payload.get("requestType").cloned().unwrap_or(Value::Null))
                .copy_truthy(payload, "decision")
                .build();
            vec![activity(event, id, "approval", "approval.resolved", "Approval resolved".into(), body)]
        }
        "runtime.error" => {
            let body = Obj::new()
                .set("message", truncate_detail(text(payload, "message"), 180))
                .copy_truthy(payload, "code")
                .build();
            vec![activity(event, id, "error", "runtime.error", "Runtime error".into(), body)]
        }
        "tool.denied" => {
            let body = Obj::new()
                .set("toolName", payload.get("toolName").cloned().unwrap_or(Value::Null))
                .copy_truthy(payload, "toolUseId")
                .set_if(truthy(payload.get("reason")), "detail", || json!(truncate_detail(text(payload, "reason"), 180)))
                .copy_truthy(payload, "agentId")
                .build();
            vec![activity(
                event,
                id,
                "error",
                "tool.denied",
                format!("Tool denied: {}", text(payload, "toolName")),
                body,
            )]
        }
        "runtime.warning" => {
            let body = Obj::new()
                .set("message", truncate_detail(text(payload, "message"), 180))
                .copy_defined(payload, "detail")
                .build();
            vec![activity(
                event,
                id,
                "info",
                "runtime.warning",
                truncate_detail(text(payload, "message"), 120),
                body,
            )]
        }
        "turn.plan.updated" => {
            let body = Obj::new()
                .set("plan", payload.get("plan").cloned().unwrap_or(Value::Null))
                .copy_defined(payload, "explanation")
                .build();
            vec![activity(event, id, "info", "turn.plan.updated", "Plan updated".into(), body)]
        }
        "user-input.requested" => {
            let body = Obj::new()
                .copy_truthy(event, "requestId")
                .set("questions", payload.get("questions").cloned().unwrap_or(Value::Null))
                .copy_truthy(payload, "responseMode")
                .build();
            vec![activity(event, id, "info", "user-input.requested", "User input requested".into(), body)]
        }
        "user-input.resolved" => {
            let body = Obj::new()
                .copy_truthy(event, "requestId")
                .set("answers", payload.get("answers").cloned().unwrap_or(Value::Null))
                .build();
            vec![activity(event, id, "info", "user-input.resolved", "User input submitted".into(), body)]
        }
        "task.started" => {
            let task_type = str_of(payload, "taskType").filter(|kind| !kind.is_empty());
            let summary = match task_type {
                Some("plan") => "Plan task started".to_owned(),
                Some(kind) => format!("{kind} task started"),
                None => "Task started".to_owned(),
            };
            let body = Obj::new()
                .set("taskId", payload.get("taskId").cloned().unwrap_or(Value::Null))
                .copy_truthy(payload, "taskType")
                .set_if(truthy(payload.get("description")), "detail", || {
                    json!(truncate_detail(text(payload, "description"), 180))
                })
                .spread_map(task_linkage_activity_fields(payload))
                .build();
            vec![activity(event, id, "info", "task.started", summary, body)]
        }
        "task.progress" => {
            let mut identity_linkage = task_linkage_activity_fields(payload);
            identity_linkage.remove("typedUsage");
            identity_linkage.remove("status");
            identity_linkage.remove("error");
            let description = text(payload, "description");
            let has_title = !trim(description).is_empty();
            let title = truncate_detail(description, 120);
            let has_progress_state = payload.get("typedUsage").is_none()
                || payload.get("summary").is_some()
                || payload.get("lastToolName").is_some()
                || payload.get("status").is_some()
                || payload.get("error").is_some();
            let thread_id = text(event, "threadId");
            let task_id = text(payload, "taskId");
            let mut out = Vec::new();
            if has_progress_state {
                let detail_source = str_of(payload, "summary").unwrap_or(description);
                let body = Obj::new()
                    .set("taskId", payload.get("taskId").cloned().unwrap_or(Value::Null))
                    .set_if(has_title, "title", || json!(title))
                    .set("detail", truncate_detail(detail_source, 180))
                    .set_if(truthy(payload.get("summary")), "summary", || {
                        json!(truncate_detail(text(payload, "summary"), 180))
                    })
                    .copy_truthy(payload, "lastToolName")
                    .copy_truthy(payload, "status")
                    .copy_truthy(payload, "error")
                    .copy_defined(payload, "usage")
                    .spread_map(identity_linkage.clone())
                    .build();
                out.push(activity(
                    event,
                    json!(format!("task-progress:{thread_id}:{task_id}")),
                    "info",
                    "task.progress",
                    if has_title { title.clone() } else { "Reasoning update".into() },
                    body,
                ));
            }
            if let Some(typed_usage) = payload.get("typedUsage") {
                let body = Obj::new()
                    .set("taskId", payload.get("taskId").cloned().unwrap_or(Value::Null))
                    .set_if(has_title, "title", || json!(title))
                    .spread_map(identity_linkage)
                    .set("usageSnapshot", true)
                    .set("typedUsage", typed_usage.clone())
                    .build();
                out.push(activity(
                    event,
                    json!(format!("task-usage:{thread_id}:{task_id}")),
                    "info",
                    "task.progress",
                    "Task usage updated".into(),
                    body,
                ));
            }
            out
        }
        "task.updated" => {
            let status = str_of(payload, "status");
            let summary = match status {
                Some("failed") => "Task failed".to_owned(),
                Some(status) if !status.is_empty() => format!("Task {status}"),
                _ => "Task updated".to_owned(),
            };
            let body = Obj::new()
                .set("taskId", payload.get("taskId").cloned().unwrap_or(Value::Null))
                .set_if(truthy(payload.get("description")), "detail", || {
                    json!(truncate_detail(text(payload, "description"), 180))
                })
                .copy_truthy(payload, "endedAt")
                .copy_defined(payload, "isBackgrounded")
                .spread_map(task_linkage_activity_fields(payload))
                .build();
            vec![activity(
                event,
                id,
                if status == Some("failed") { "error" } else { "info" },
                "task.updated",
                summary,
                body,
            )]
        }
        "tool.progress" => {
            let Some(task_id) = payload.get("taskId") else {
                return Vec::new();
            };
            let thread_id = text(event, "threadId");
            let task_text = task_id.as_str().map(str::to_owned).unwrap_or_else(|| task_id.to_string());
            let summary = str_of(payload, "toolName").map(str::to_owned).unwrap_or_else(|| "Tool progress".into());
            let body = Obj::new()
                .set("taskId", task_id.clone())
                .copy_truthy(payload, "toolName")
                .copy_truthy(payload, "toolUseId")
                .copy_defined(payload, "elapsedSeconds")
                .copy_truthy(payload, "parentToolUseId")
                .build();
            vec![activity(
                event,
                json!(format!("tool-progress:{thread_id}:{task_text}")),
                "info",
                "tool.progress",
                summary,
                body,
            )]
        }
        "task.completed" => {
            let status = str_of(payload, "status");
            let summary = match status {
                Some("failed") => "Task failed",
                Some("stopped") => "Task stopped",
                _ => "Task completed",
            };
            let summary_text = truthy(payload.get("summary")).then(|| truncate_detail(text(payload, "summary"), 180));
            let mut body = Obj::new()
                .set("taskId", payload.get("taskId").cloned().unwrap_or(Value::Null))
                .set("status", payload.get("status").cloned().unwrap_or(Value::Null))
                .set_if(task_title.is_some_and(|title| !title.is_empty()), "title", || {
                    json!(truncate_detail(task_title.unwrap_or(""), 120))
                });
            if let Some(summary_text) = summary_text {
                body = body.set("summary", summary_text.clone()).set("detail", summary_text);
            }
            let body = body.copy_defined(payload, "usage").spread_map(task_linkage_activity_fields(payload)).build();
            vec![activity(
                event,
                id,
                if status == Some("failed") { "error" } else { "info" },
                "task.completed",
                summary.into(),
                body,
            )]
        }
        "thread.state.changed" => {
            if str_of(payload, "state") != Some("compacted") {
                return Vec::new();
            }
            let before = payload.get("beforeTokens").filter(|value| !value.is_null());
            let after = payload.get("afterTokens").filter(|value| !value.is_null());
            let summary = match (before.and_then(Value::as_f64), after.and_then(Value::as_f64)) {
                (Some(before), Some(after)) => format!("Compacted context {} → {} tokens", format_tokens(before), format_tokens(after)),
                _ => "Context compacted".to_owned(),
            };
            let body = Obj::new()
                .set("state", payload.get("state").cloned().unwrap_or(Value::Null))
                .set_if(before.is_some(), "beforeTokens", || before.cloned().unwrap_or(Value::Null))
                .set_if(after.is_some(), "afterTokens", || after.cloned().unwrap_or(Value::Null))
                .copy_defined(event, "requestId")
                .copy_defined(payload, "detail")
                .build();
            vec![activity(event, id, "info", "context-compaction", summary, body)]
        }
        "thread.token-usage.updated" => {
            let usage = &payload["usage"];
            if usage["usedTokens"].as_f64().is_none_or(|used| used < 0.0) {
                return Vec::new();
            }
            vec![activity(
                event,
                id,
                "info",
                "context-window.updated",
                "Context window updated".into(),
                usage.clone(),
            )]
        }
        "item.updated" | "item.completed" | "item.started" => {
            if !is_tool_lifecycle_item_type(str_of(payload, "itemType")) {
                return Vec::new();
            }
            let body = Obj::new()
                .set("itemType", payload.get("itemType").cloned().unwrap_or(Value::Null))
                .set_if(event.get("itemId").is_some(), "toolCallId", || {
                    event.get("itemId").cloned().unwrap_or(Value::Null)
                })
                .copy_truthy(payload, "status")
                .copy_truthy(payload, "title")
                .set_if(truthy(payload.get("detail")), "detail", || json!(truncate_detail(text(payload, "detail"), 180)))
                .copy_truthy(payload, "toolSurface")
                .copy_truthy(payload, "toolIcon")
                .copy_truthy(payload, "toolSource")
                .copy_defined(payload, "data")
                .copy_truthy(payload, "agentId")
                .copy_truthy(payload, "parentToolUseId")
                .build();
            let title = str_of(payload, "title").map(str::to_owned);
            match event_type {
                "item.updated" => vec![project_activity_payload(&activity(
                    event,
                    id,
                    "tool",
                    "tool.updated",
                    title.unwrap_or_else(|| "Tool updated".into()),
                    body,
                ))],
                "item.completed" => vec![activity(event, id, "tool", "tool.completed", title.unwrap_or_else(|| "Tool".into()), body)],
                _ => vec![activity(
                    event,
                    id,
                    "tool",
                    "tool.started",
                    format!("{} started", title.unwrap_or_else(|| "Tool".into())),
                    body,
                )],
            }
        }
        _ => Vec::new(),
    }
}

/// `compactedTokenCountsFromActivities`: the last two context-window readings after the latest
/// compaction, when the second is lower.
pub fn compacted_token_counts(activities: &[zc_db::repos::thread_activities::ProjectionThreadActivity]) -> Option<(Value, Value)> {
    let last_compaction_index = activities.iter().rposition(|activity| activity.kind == "context-compaction");
    let last_compaction = last_compaction_index.map(|index| &activities[index]);
    let since = match last_compaction_index {
        Some(index) => &activities[index + 1..],
        None => activities,
    };
    let used: Vec<&Value> = since
        .iter()
        .filter(|activity| activity.kind == "context-window.updated")
        .filter(|activity| match last_compaction {
            None => true,
            Some(compaction) => match (activity.sequence, compaction.sequence) {
                (Some(sequence), Some(compaction_sequence)) => sequence > compaction_sequence,
                _ => activity.created_at > compaction.created_at,
            },
        })
        .filter_map(|activity| activity.payload.get("usedTokens").filter(|used| used.as_f64().is_some_and(|used| used >= 0.0)))
        .collect();
    let (before, after) = match used.as_slice() {
        [.., before, after] => (*before, *after),
        _ => return None,
    };
    (after.as_f64()? < before.as_f64()?).then(|| (before.clone(), after.clone()))
}

/// `findTaskTitleInActivities(activities, taskId)`.
pub fn find_task_title_in_activities(activities: &[zc_db::repos::thread_activities::ProjectionThreadActivity], task_id: &str) -> Option<String> {
    activities.iter().rev().find_map(|activity| {
        if activity.kind != "task.started" && activity.kind != "task.progress" {
            return None;
        }
        if activity.payload.get("taskId").and_then(Value::as_str) != Some(task_id) {
            return None;
        }
        let title = match activity.payload.get("title") {
            Some(Value::String(title)) => Some(title.clone()),
            _ if activity.kind == "task.started" => str_of(&activity.payload, "detail").map(str::to_owned),
            _ => None,
        };
        title.filter(|title| !trim(title).is_empty())
    })
}

/// `normalizeProposedPlanMarkdown`.
pub fn normalize_proposed_plan_markdown(markdown: Option<&str>) -> Option<String> {
    let trimmed = trim(markdown?);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The buffer cap of [`super::MAX_BUFFERED_ASSISTANT_CHARS`] counts UTF-16 units.
pub fn buffered_len(text: &str) -> usize {
    len16(text)
}

#[cfg(test)]
mod tests {
    //! The `splitBufferedAssistantText` cases of `ProviderRuntimeIngestion.test.ts`.
    use super::*;

    #[track_caller]
    fn expect(text: &str, ready: &str, rest: &str) {
        assert_eq!(split_buffered_assistant_text(text), (ready.to_owned(), rest.to_owned()), "split of {text:?}");
    }

    #[test]
    fn keeps_a_partial_trailing_line_buffered() {
        expect("one\n\ntwo", "one\n\n", "two");
        expect("one\ntwo", "", "one\ntwo");
    }

    #[test]
    fn does_not_split_inside_an_open_fence_and_delivers_the_block_at_its_closing_fence() {
        let open = "intro\n\n```\ncode\n\nmore\n";
        expect(open, "intro\n\n", "```\ncode\n\nmore\n");
        expect(&format!("{open}```\nafter"), &format!("{open}```\n"), "after");
    }

    #[test]
    fn does_not_treat_a_fence_with_an_info_string_as_a_closing_fence() {
        let text = "```\n```javascript\nstill code\n\nmore\n";
        expect(text, "", text);
    }

    #[test]
    fn treats_a_fence_indented_four_or_more_spaces_as_code_not_a_closing_fence() {
        let text = "```\n    ```\n\nstill code\n";
        expect(text, "", text);
        expect("```\n   ```\nafter", "```\n   ```\n", "after");
    }

    #[test]
    fn keeps_a_fence_nested_under_a_list_item_open_across_its_blank_lines() {
        expect(
            "- step\n\n    ```ts\n    a\n\n    b\n    ```\n\nafter\n",
            "- step\n\n    ```ts\n    a\n\n    b\n    ```\n\n",
            "after\n",
        );
    }

    #[test]
    fn does_not_treat_a_no_break_space_line_as_blank() {
        expect("para\n\u{a0}\ncont\n\nnext", "para\n\u{a0}\ncont\n\n", "next");
    }

    #[test]
    fn treats_crlf_blank_lines_as_boundaries() {
        expect("one\r\n\r\ntwo", "one\r\n\r\n", "two");
    }

    #[test]
    fn only_closes_a_fence_with_the_same_marker_of_equal_or_greater_length() {
        expect("````\n```\nstill code\n\n````\n\nout\n", "````\n```\nstill code\n\n````\n\n", "out\n");
        expect("~~~\n```\n\nx\n", "", "~~~\n```\n\nx\n");
    }

    #[test]
    fn delivers_tight_list_items_one_at_a_time() {
        expect("## Steps\n\n- one\n- two\n- thr", "## Steps\n\n- one\n- two\n", "- thr");
        expect("1. one\n2. two\n   more\n3. t", "1. one\n2. two\n   more\n", "3. t");
    }

    #[test]
    fn keeps_a_partial_list_marker_and_list_like_code_buffered() {
        expect("intro\n-", "", "intro\n-");
        expect("intro\n1.", "", "intro\n1.");
        expect("intro\n- ", "", "intro\n- ");
        expect("- one\n", "", "- one\n");
        expect("```\n- one\n- two\n", "", "```\n- one\n- two\n");
    }

    #[test]
    fn holds_a_heading_until_the_block_under_it_is_done() {
        expect("intro\n\n## Setup\n\nInstall it", "intro\n\n", "## Setup\n\nInstall it");
        expect(
            "intro\n\n# Plan\n\n## Setup\n\nInstall it.\n\nNext",
            "intro\n\n# Plan\n\n## Setup\n\nInstall it.\n\n",
            "Next",
        );
    }

    #[test]
    fn delivers_the_paragraph_above_a_heading_with_no_blank_line_between_them() {
        expect("para\n## Setup\n\nInstall", "para\n", "## Setup\n\nInstall");
        expect("para\n**Setup**\n\nInstall", "", "para\n**Setup**\n\nInstall");
    }

    #[test]
    fn holds_a_line_of_only_bold_text_like_a_heading() {
        expect("**Risk by area:**\n\n| a |\n|---|\n", "", "**Risk by area:**\n\n| a |\n|---|\n");
        expect("**Use *npm* now**\n\nInstall it", "", "**Use *npm* now**\n\nInstall it");
        expect("**Note:** read this.\n\nNext", "**Note:** read this.\n\n", "Next");
    }

    #[test]
    fn delivers_a_held_heading_with_its_first_list_item_or_its_whole_code_block() {
        expect("## Steps\n\n- one\n- tw", "## Steps\n\n- one\n", "- tw");
        expect("## Code\n\n```ts\na\n\nb\n", "", "## Code\n\n```ts\na\n\nb\n");
        expect("## Code\n\n```ts\na\n```\nafter", "## Code\n\n```ts\na\n```\n", "after");
    }

    #[test]
    fn formats_tokens_like_usage_format() {
        assert_eq!(format_tokens(19_900_000_000.0), "19.9B");
        assert_eq!(format_tokens(76_700_000.0), "76.7M");
        assert_eq!(format_tokens(804_000.0), "804K");
        assert_eq!(format_tokens(1_000.0), "1K");
        assert_eq!(format_tokens(1_500.0), "1.50K");
        assert_eq!(format_tokens(950.0), "950");
    }
}
