//! `orchestration/ActivityPayloadProjection.ts` (plus shared `projectQuestionToolInput`):
//! slims activity payloads for clients (the full payload stays in persistence) and prunes
//! snapshot activities no client reads.
//!
//! Payloads are JSON objects whose key order is what `JSON.stringify` writes, so this works on
//! `serde_json::Map` (insertion ordered): an insert of an existing key keeps its position, as
//! a JS assignment or spread does.

use std::collections::HashSet;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};
use zc_contracts::{OrchestrationEvent, OrchestrationMessageRole, OrchestrationThreadActivity, OrchestrationThreadDetailSnapshot};

use crate::js;

type Record = Map<String, Value>;

fn as_record(value: Option<&Value>) -> Option<&Record> {
    value.and_then(Value::as_object)
}

/// `asTrimmedString`: a non-empty trimmed string.
fn as_trimmed_string(value: Option<&Value>) -> Option<String> {
    let trimmed = js::trim(value?.as_str()?);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `a ?? b` on JSON: `a` unless it is absent or `null`.
fn coalesce<'a>(first: Option<&'a Value>, second: Option<&'a Value>) -> Option<&'a Value> {
    match first {
        Some(value) if !value.is_null() => Some(value),
        _ => second,
    }
}

fn present(value: Option<&Value>) -> Option<&Value> {
    value.filter(|value| !value.is_null())
}

/// JavaScript `String(value)` for JSON values (template literals).
fn js_to_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".to_string(),
        Some(Value::Null) => "null".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(flag)) => flag.to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::Null => String::new(),
                other => js_to_string(Some(other)),
            })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".to_string(),
    }
}

fn push_changed_file(target: &mut Vec<String>, seen: &mut HashSet<String>, value: Option<&Value>) {
    let Some(normalized) = as_trimmed_string(value) else {
        return;
    };
    if seen.insert(normalized.clone()) {
        target.push(normalized);
    }
}

fn collect_changed_files(value: Option<&Value>, target: &mut Vec<String>, seen: &mut HashSet<String>, depth: usize) {
    if depth > 4 || target.len() >= 12 {
        return;
    }
    if let Some(entries) = value.and_then(Value::as_array) {
        for entry in entries {
            collect_changed_files(Some(entry), target, seen, depth + 1);
            if target.len() >= 12 {
                return;
            }
        }
        return;
    }
    let Some(record) = as_record(value) else {
        return;
    };
    for key in ["path", "filePath", "relativePath", "filename", "newPath", "oldPath"] {
        push_changed_file(target, seen, record.get(key));
    }
    for key in ["item", "result", "input", "data", "changes", "files", "edits", "patch", "patches", "operations"] {
        if !record.contains_key(key) {
            continue;
        }
        collect_changed_files(record.get(key), target, seen, depth + 1);
        if target.len() >= 12 {
            return;
        }
    }
}

fn changed_files_value(data: &Record) -> Option<Value> {
    let mut files = Vec::new();
    let data_value = Value::Object(data.clone());
    collect_changed_files(Some(&data_value), &mut files, &mut HashSet::new(), 0);
    (!files.is_empty()).then(|| {
        Value::Array(
            files
                .into_iter()
                .map(|path| {
                    let mut entry = Record::new();
                    entry.insert("path".into(), Value::String(path));
                    Value::Object(entry)
                })
                .collect(),
        )
    })
}

/// `summarizeToolTextOutput`: the first meaningful line (≤ 84 UTF-16 units), or "N lines".
pub fn summarize_tool_text_output(value: &str) -> Option<String> {
    let mut meaningful = 0usize;
    for raw_line in value.split('\n') {
        let collapsed = js::collapse_whitespace(raw_line);
        let line = js::trim(&collapsed);
        if line.is_empty() {
            continue;
        }
        meaningful += 1;
        if line != "```" {
            if js::utf16_len(line) <= 84 {
                return Some(line.to_string());
            }
            let head = js::utf16_slice(line, 0, 83);
            return Some(format!("{}…", js::trim_end(&head)));
        }
    }
    (meaningful > 1).then(|| format!("{} lines", js::to_locale_string(meaningful)))
}

fn project_command_data(data: &Record) -> Option<Record> {
    let item = as_record(data.get("item"))?;
    let mut projected = Record::new();
    if let Some(command) = item.get("command") {
        projected.insert("command".into(), command.clone());
    }
    if let Some(output) = as_trimmed_string(item.get("aggregatedOutput")) {
        if let Some(summary) = summarize_tool_text_output(&output) {
            projected.insert("aggregatedOutput".into(), Value::String(summary));
        }
    }
    if let Some(input) = as_record(item.get("input")) {
        if let Some(command) = input.get("command") {
            let mut entry = Record::new();
            entry.insert("command".into(), command.clone());
            projected.insert("input".into(), Value::Object(entry));
        }
    }
    if let Some(result) = as_record(item.get("result")) {
        let mut projected_result = Record::new();
        if let Some(command) = result.get("command") {
            projected_result.insert("command".into(), command.clone());
        }
        if let Some(content) = as_trimmed_string(result.get("content")) {
            if let Some(summary) = summarize_tool_text_output(&content) {
                projected_result.insert("content".into(), Value::String(summary));
            }
        }
        if !projected_result.is_empty() {
            projected.insert("result".into(), Value::Object(projected_result));
        }
    }
    (!projected.is_empty()).then_some(projected)
}

fn project_command_value(data: &Record) -> Option<Value> {
    if let Some(command) = data.get("command") {
        return Some(command.clone());
    }
    if let Some(command) = as_record(data.get("input")).and_then(|input| input.get("command")) {
        return Some(command.clone());
    }
    as_record(data.get("state"))
        .and_then(|state| as_record(state.get("input")))
        .and_then(|input| input.get("command"))
        .cloned()
}

const WORKSPACE_IMAGE_PREVIEW_EXTENSIONS: &[&str] = &[".avif", ".gif", ".ico", ".jpeg", ".jpg", ".png", ".svg", ".webp"];

/// shared `isWorkspaceImagePreviewPath`.
fn is_workspace_image_preview_path(path: &str) -> bool {
    let without_query = path.split(['?', '#']).next().unwrap_or("").to_lowercase();
    WORKSPACE_IMAGE_PREVIEW_EXTENSIONS.iter().any(|extension| without_query.ends_with(extension))
}

fn project_viewed_image_path(data: &Record) -> Option<String> {
    if let Some(direct) = as_trimmed_string(data.get("imagePath")) {
        if is_workspace_image_preview_path(&direct) {
            return Some(direct);
        }
    }
    let tool_name = as_trimmed_string(data.get("toolName"))?.to_lowercase();
    if tool_name != "read" && tool_name != "read file" {
        return None;
    }
    let input = as_record(data.get("input"));
    let path = as_trimmed_string(input.and_then(|input| input.get("file_path"))).or_else(|| as_trimmed_string(input.and_then(|input| input.get("path"))))?;
    is_workspace_image_preview_path(&path).then_some(path)
}

const MCP_ITEM_KEPT_FIELDS: &[&str] = &["type", "id", "tool", "server", "status", "arguments", "appContext", "error", "durationMs"];

/// `extractMcpResultText`.
fn extract_mcp_result_text(result: Option<&Value>) -> Option<String> {
    let Some(record) = as_record(result) else {
        return result.and_then(Value::as_str).map(str::to_owned);
    };
    match record.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(entries)) => {
            let texts: Vec<&str> = entries
                .iter()
                .filter_map(|entry| entry.as_object()?.get("text")?.as_str())
                .filter(|text| !js::trim(text).is_empty())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

fn summarize_mcp_result(result: Option<&Value>) -> Option<Record> {
    let result = present(result)?;
    let text = extract_mcp_result_text(Some(result))?;
    let summary = summarize_tool_text_output(&text)?;
    let mut record = Record::new();
    record.insert("content".into(), Value::String(summary));
    Some(record)
}

fn preview_tool_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^(?:mcp__)?(?:t3-code|t3_code|t3code)_{1,2}preview_(?:open|navigate|status|snapshot|click|type|press|scroll|resize|set_appearance|evaluate|wait_for|recording_start|recording_stop)$").unwrap()
    })
}

/// `/^\s*\{\s*"content"\s*:\s*\[\s*/.exec(text)`: the length of the match.
fn first_content_block_prefix(text: &str) -> Option<usize> {
    let mut rest = text;
    let skip = |rest: &mut &str| *rest = rest.trim_start_matches(js::is_js_whitespace);
    skip(&mut rest);
    rest = rest.strip_prefix('{')?;
    skip(&mut rest);
    rest = rest.strip_prefix("\"content\"")?;
    skip(&mut rest);
    rest = rest.strip_prefix(':')?;
    skip(&mut rest);
    rest = rest.strip_prefix('[')?;
    skip(&mut rest);
    Some(text.len() - rest.len())
}

fn is_true(record: Option<&Record>, key: &str) -> bool {
    record.and_then(|record| record.get(key)) == Some(&Value::Bool(true))
}

/// `projectPreviewToolMetadata`: reuse the page URL preview tools already returned.
fn project_preview_tool_metadata(data: &Record, status: Option<&Value>) -> Record {
    let empty = Record::new();
    let item = as_record(data.get("item"));
    let name: Option<String> = match item {
        Some(item) => Some(format!("mcp__{}__{}", js_to_string(item.get("server")), js_to_string(item.get("tool")))),
        None => coalesce(data.get("toolName"), data.get("tool")).and_then(Value::as_str).map(str::to_owned),
    };
    let Some(name) = name.filter(|name| preview_tool_pattern().is_match(name)) else {
        return empty;
    };
    let state = as_record(data.get("state"));
    let result = coalesce(
        coalesce(item.and_then(|item| item.get("result")), data.get("result")),
        state.and_then(|state| state.get("output")),
    );
    let record = as_record(result);
    let status = status.and_then(Value::as_str);
    if status == Some("failed")
        || status == Some("declined")
        || state.and_then(|state| state.get("status")).and_then(Value::as_str) == Some("error")
        || present(item.and_then(|item| item.get("error"))).is_some()
        || is_true(record, "isError")
        || is_true(record, "is_error")
    {
        return empty;
    }

    let mut page: Option<Value> = record.map(|record| Value::Object(record.clone()));
    let mut output: Option<Value> = result.cloned();
    for _ in 0..3 {
        let page_record = page.as_ref().and_then(Value::as_object);
        if is_true(page_record, "isError") || is_true(page_record, "is_error") {
            return empty;
        }
        if let Some(structured) = page_record.and_then(|record| record.get("structuredContent")).and_then(Value::as_object) {
            page = Some(Value::Object(structured.clone()));
            break;
        }
        let Some(text) = extract_mcp_result_text(output.as_ref())
            .map(|text| js::utf16_slice(&text, 0, 2 * 1024 * 1024))
            .filter(|text| !text.is_empty())
        else {
            break;
        };
        match serde_json::from_str::<Value>(zc_core::lenient_json::extract_json_object(&text)) {
            Ok(parsed) => page = parsed.is_object().then_some(parsed),
            Err(_) => {
                // A truncated MCP envelope can still contain a complete first text block.
                let Some(prefix) = first_content_block_prefix(&text) else {
                    return empty;
                };
                match serde_json::from_str::<Value>(zc_core::lenient_json::extract_json_object(&text[prefix..])) {
                    Ok(block) => {
                        page = (block.is_object() && block.get("type") == Some(&Value::from("text"))).then(|| {
                            let mut wrapper = Record::new();
                            wrapper.insert("content".into(), Value::Array(vec![block]));
                            Value::Object(wrapper)
                        });
                    }
                    Err(_) => return empty,
                }
            }
        }
        output = page.clone();
    }
    let page_record = page.as_ref().and_then(Value::as_object);
    static PAGE_URL_TOOL: OnceLock<Regex> = OnceLock::new();
    let page_url_tool = PAGE_URL_TOOL.get_or_init(|| Regex::new(r"preview_(?:open|navigate|status|snapshot)$").unwrap());
    let fallback = if page_url_tool.is_match(&name) {
        page_record.and_then(|record| record.get("url"))
    } else {
        None
    };
    let raw_url = as_trimmed_string(coalesce(
        as_record(page_record.and_then(|record| record.get("toolIcon"))).and_then(|icon| icon.get("pageUrl")),
        fallback,
    ));
    let Some(raw_url) = raw_url.filter(|url| js::utf16_len(url) <= 4096) else {
        return empty;
    };
    match url::Url::parse(&raw_url) {
        Ok(url) if url.scheme() == "http" || url.scheme() == "https" => {
            let mut icon = Record::new();
            icon.insert("_tag".into(), Value::from("website"));
            icon.insert("pageUrl".into(), Value::from(url.as_str()));
            let mut out = Record::new();
            out.insert("toolIcon".into(), Value::Object(icon));
            out
        }
        _ => empty,
    }
}

fn project_mcp_tool_call_data(data: &Record) -> Record {
    let mut projected = Record::new();
    let item = as_record(data.get("item"));
    if let Some(item) = item {
        let mut projected_item = Record::new();
        for key in MCP_ITEM_KEPT_FIELDS {
            if let Some(value) = item.get(*key) {
                projected_item.insert((*key).to_string(), value.clone());
            }
        }
        if let Some(result) = summarize_mcp_result(item.get("result")) {
            projected_item.insert("result".into(), Value::Object(result));
        }
        projected.insert("item".into(), Value::Object(projected_item));
    }
    if let Some(value) = data.get("toolName") {
        projected.insert("toolName".into(), value.clone());
    }
    if let Some(value) = data.get("input") {
        projected.insert("input".into(), value.clone());
    }
    if item.is_none() {
        if let Some(result) = summarize_mcp_result(data.get("result")) {
            projected.insert("result".into(), Value::Object(result));
        }
    }
    if let Some(value) = data.get("toolCallId") {
        projected.insert("toolCallId".into(), value.clone());
    }
    if let Some(value) = data.get("kind") {
        projected.insert("kind".into(), value.clone());
    }
    if let Some(files) = changed_files_value(data) {
        projected.insert("files".into(), files);
    }
    projected
}

fn content_record(summary: Option<String>) -> Option<Record> {
    summary.map(|summary| {
        let mut record = Record::new();
        record.insert("content".into(), Value::String(summary));
        record
    })
}

fn project_raw_output(value: Option<&Value>) -> Option<Record> {
    if let Some(direct) = as_trimmed_string(value) {
        return content_record(summarize_tool_text_output(&direct));
    }
    let raw = as_record(value)?;
    if let Some(total) = raw.get("totalFiles").filter(|total| total.is_number()) {
        let mut record = Record::new();
        record.insert("totalFiles".into(), total.clone());
        if raw.get("truncated") == Some(&Value::Bool(true)) {
            record.insert("truncated".into(), Value::Bool(true));
        }
        return Some(record);
    }
    for key in ["content", "stdout", "stderr"] {
        if let Some(text) = as_trimmed_string(raw.get(key)) {
            return content_record(summarize_tool_text_output(&text));
        }
    }
    None
}

fn project_acp_content(value: Option<&Value>) -> Option<Record> {
    let entries = value?.as_array()?;
    let text = entries
        .iter()
        .filter_map(|entry| {
            let entry = entry.as_object()?;
            let content = as_record(entry.get("content"));
            if entry.get("type") == Some(&Value::from("content")) && content.and_then(|content| content.get("type")) == Some(&Value::from("text")) {
                as_trimmed_string(content.and_then(|content| content.get("text")))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    content_record(summarize_tool_text_output(&text))
}

/// shared `projectQuestionToolInput`.
fn project_question_tool_input(data: &Record, title: Option<&Value>) -> Record {
    let empty = Record::new();
    let item = as_record(data.get("item"));
    let tool_name = coalesce(
        coalesce(coalesce(data.get("toolName"), data.get("tool")), item.and_then(|item| item.get("tool"))),
        title,
    );
    let Some(tool_name) = tool_name.and_then(Value::as_str) else {
        return empty;
    };
    static SPLIT: OnceLock<Regex> = OnceLock::new();
    static NAME: OnceLock<Regex> = OnceLock::new();
    let split = SPLIT.get_or_init(|| Regex::new(r"__|[./]").unwrap());
    let name_pattern = NAME.get_or_init(|| Regex::new(r"^(askuserquestion|requestuserinput(?:async)?|askquestion|question)$").unwrap());
    let last = split.split(tool_name).last().unwrap_or("");
    let name: String = last
        .chars()
        .filter(|c| *c != '_' && !js::is_js_whitespace(*c))
        .collect::<String>()
        .to_lowercase();
    if name.is_empty() || !name_pattern.is_match(&name) {
        return empty;
    }
    let state = as_record(data.get("state"));
    let input = as_record(coalesce(
        coalesce(coalesce(data.get("input"), data.get("rawInput")), state.and_then(|state| state.get("input"))),
        item.and_then(|item| item.get("arguments")),
    ));
    let questions = coalesce(
        input.and_then(|input| input.get("questions")),
        as_record(input.and_then(|input| input.get("params"))).and_then(|params| params.get("questions")),
    );
    let Some(questions) = questions.and_then(Value::as_array) else {
        return empty;
    };
    let projected: Vec<Value> = questions
        .iter()
        .map(|value| {
            let question = value.as_object();
            let pick = |key: &str| question.and_then(|question| question.get(key));
            let text = as_trimmed_string(coalesce(
                coalesce(coalesce(pick("question"), pick("question_text")), pick("prompt")),
                pick("title"),
            ));
            let mut entry = Record::new();
            // `{question: undefined}` serializes as `{}`.
            if let Some(text) = text {
                entry.insert("question".into(), Value::String(text));
            }
            Value::Object(entry)
        })
        .collect();
    let mut out = Record::new();
    out.insert("toolName".into(), Value::String(tool_name.to_string()));
    let mut input_out = Record::new();
    input_out.insert("questions".into(), Value::Array(projected));
    out.insert("input".into(), Value::Object(input_out));
    out
}

/// `{...left, ...right}`.
fn spread(mut left: Record, right: &Record) -> Record {
    for (key, value) in right {
        left.insert(key.clone(), value.clone());
    }
    left
}

/// `projectActivityPayload` on an encoded payload: the slimmed payload, or `None` when it is
/// returned unchanged (no object payload or no object `data`).
pub fn project_payload(payload: &Value) -> Option<Value> {
    let payload = payload.as_object()?;
    let data = payload.get("data")?.as_object()?;
    let item_status = as_record(data.get("item")).and_then(|item| item.get("status"));
    let status_payload: Record =
        if payload.get("status") == Some(&Value::from("completed")) && matches!(item_status.and_then(Value::as_str), Some("failed" | "declined")) {
            let mut copy = payload.clone();
            copy.insert("status".into(), item_status.cloned().unwrap_or(Value::Null));
            copy
        } else {
            payload.clone()
        };
    let projected_payload = spread(project_preview_tool_metadata(data, status_payload.get("status")), &status_payload);
    let question_input = project_question_tool_input(data, payload.get("title"));

    if payload.get("itemType") == Some(&Value::from("mcp_tool_call")) {
        let data_out = spread(project_mcp_tool_call_data(data), &question_input);
        let mut out = projected_payload;
        out.insert("data".into(), Value::Object(data_out));
        return Some(Value::Object(out));
    }

    let mut projected_data = question_input;
    if let Some(item) = project_command_data(data) {
        projected_data.insert("item".into(), Value::Object(item));
    }
    if let Some(command) = project_command_value(data) {
        projected_data.insert("command".into(), command);
    }
    if let Some(image_path) = project_viewed_image_path(data) {
        projected_data.insert("imagePath".into(), Value::String(image_path));
    }
    if let Some(files) = changed_files_value(data) {
        projected_data.insert("files".into(), files);
    }
    for key in ["toolCallId", "kind", "toolName"] {
        if let Some(value) = data.get(key) {
            projected_data.insert(key.into(), value.clone());
        }
    }
    let raw_output = project_raw_output(data.get("rawOutput"))
        .or_else(|| project_acp_content(data.get("content")))
        .or_else(|| {
            if payload.get("itemType") == Some(&Value::from("command_execution")) {
                summarize_mcp_result(data.get("result"))
            } else {
                None
            }
        });
    if let Some(raw_output) = raw_output {
        projected_data.insert("rawOutput".into(), Value::Object(raw_output));
    }
    let mut out = projected_payload;
    out.insert("data".into(), Value::Object(projected_data));
    Some(Value::Object(out))
}

/// `projectActivityPayload`.
pub fn project_activity_payload(activity: &OrchestrationThreadActivity) -> OrchestrationThreadActivity {
    match project_payload(&activity.payload) {
        Some(payload) => OrchestrationThreadActivity { payload, ..activity.clone() },
        None => activity.clone(),
    }
}

fn is_resolvable_context_window_activity(activity: &OrchestrationThreadActivity) -> bool {
    if activity.kind.as_str() != "context-window.updated" {
        return false;
    }
    activity
        .payload
        .as_object()
        .and_then(|payload| payload.get("usedTokens"))
        .and_then(Value::as_f64)
        .is_some_and(|used| used.is_finite() && used >= 0.0)
}

fn turn_key(activity: &OrchestrationThreadActivity) -> Option<String> {
    activity.turn_id.as_ref().map(|id| id.as_str().to_string())
}

/// `dropStaleContextWindowActivities`: the last resolvable context-window row per turn.
fn drop_stale_context_window_activities(activities: Vec<OrchestrationThreadActivity>) -> Vec<OrchestrationThreadActivity> {
    let mut latest: Vec<(Option<String>, usize)> = Vec::new();
    for (index, activity) in activities.iter().enumerate() {
        if is_resolvable_context_window_activity(activity) {
            let key = turn_key(activity);
            match latest.iter_mut().find(|(existing, _)| *existing == key) {
                Some(entry) => entry.1 = index,
                None => latest.push((key, index)),
            }
        }
    }
    if latest.is_empty() {
        return activities;
    }
    activities
        .into_iter()
        .enumerate()
        .filter(|(index, activity)| {
            !is_resolvable_context_window_activity(activity) || {
                let key = turn_key(activity);
                latest
                    .iter()
                    .find(|(existing, _)| *existing == key)
                    .is_some_and(|(_, latest_index)| latest_index == index)
            }
        })
        .map(|(_, activity)| activity)
        .collect()
}

/// `toolLifecycleIdentity`.
fn tool_lifecycle_identity(activity: &OrchestrationThreadActivity) -> Option<String> {
    let payload = activity.payload.as_object()?;
    let tool_call_id =
        as_trimmed_string(payload.get("toolCallId")).or_else(|| as_trimmed_string(as_record(payload.get("data")).and_then(|data| data.get("toolCallId"))));
    if let Some(id) = tool_call_id {
        return Some(format!("id:{id}"));
    }
    static COMPLETE: OnceLock<Regex> = OnceLock::new();
    let complete = COMPLETE.get_or_init(|| Regex::new(r"(?i)\s+(?:complete|completed)\s*$").unwrap());
    let item_type = as_trimmed_string(payload.get("itemType")).unwrap_or_default();
    let title = as_trimmed_string(payload.get("title")).unwrap_or_else(|| activity.summary.as_str().to_string());
    let label = js::trim(&complete.replace(&title, "")).to_string();
    let detail = as_trimmed_string(payload.get("detail")).unwrap_or_default();
    if item_type.is_empty() && label.is_empty() && detail.is_empty() {
        return None;
    }
    Some([item_type, label, detail].join("\u{1f}"))
}

/// `dropSupersededToolUpdatedActivities`: `tool.updated` rows a later `tool.completed` of the
/// same call (and turn) supersedes.
fn drop_superseded_tool_updated_activities(activities: Vec<OrchestrationThreadActivity>) -> Vec<OrchestrationThreadActivity> {
    let key_of = |activity: &OrchestrationThreadActivity, identity: &str| format!("{}\u{0}{identity}", turn_key(activity).unwrap_or_default());
    let mut completions: std::collections::HashMap<String, Vec<usize>> = std::collections::HashMap::new();
    for (index, activity) in activities.iter().enumerate() {
        if activity.kind.as_str() != "tool.completed" {
            continue;
        }
        let Some(identity) = tool_lifecycle_identity(activity) else {
            continue;
        };
        completions.entry(key_of(activity, &identity)).or_default().push(index);
    }
    if completions.is_empty() {
        return activities;
    }
    activities
        .into_iter()
        .enumerate()
        .filter(|(index, activity)| {
            if activity.kind.as_str() != "tool.updated" {
                return true;
            }
            let Some(identity) = tool_lifecycle_identity(activity) else {
                return true;
            };
            !completions
                .get(&key_of(activity, &identity))
                .is_some_and(|indices| indices.iter().any(|completion| completion > index))
        })
        .map(|(_, activity)| activity)
        .collect()
}

/// `projectThreadDetailSnapshot`.
pub fn project_thread_detail_snapshot(mut snapshot: OrchestrationThreadDetailSnapshot, reasoning_messages: bool) -> OrchestrationThreadDetailSnapshot {
    if !reasoning_messages {
        for message in &mut snapshot.thread.messages {
            if message.role == OrchestrationMessageRole::Reasoning {
                message.role = OrchestrationMessageRole::System;
            }
        }
    }
    let activities = std::mem::take(&mut snapshot.thread.activities);
    snapshot.thread.activities = drop_superseded_tool_updated_activities(drop_stale_context_window_activities(activities))
        .iter()
        .map(project_activity_payload)
        .collect();
    snapshot
}

/// `projectActivityEvent`.
pub fn project_activity_event(event: &OrchestrationEvent, reasoning_messages: bool) -> OrchestrationEvent {
    match event {
        // Preserve sequence watermarks and message identities for clients whose role
        // decoder predates reasoning.
        OrchestrationEvent::ThreadMessageSent(sent) if !reasoning_messages && sent.payload.role == OrchestrationMessageRole::Reasoning => {
            let mut sent = sent.clone();
            sent.payload.role = OrchestrationMessageRole::System;
            OrchestrationEvent::ThreadMessageSent(sent)
        }
        OrchestrationEvent::ThreadActivityAppended(appended) => {
            let mut appended = appended.clone();
            appended.payload.activity = project_activity_payload(&appended.payload.activity);
            OrchestrationEvent::ThreadActivityAppended(appended)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests;
