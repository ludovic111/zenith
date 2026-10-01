//! `orchestration/ActivityPayloadProjection.ts` `projectActivityPayload`: the slimmed payload of
//! a tool activity (what clients render), used by the ingestion for streaming `tool.updated`
//! rows. The subscription and snapshot paths (WP-09) apply the same projection.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::js::{len16, slice_head16, trim, trim_end, Obj};

fn as_record(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn as_trimmed_string(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    let trimmed = trim(text);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

fn push_changed_file(target: &mut Vec<String>, seen: &mut HashSet<String>, value: Option<&Value>) {
    let Some(normalized) = as_trimmed_string(value) else { return };
    if seen.contains(&normalized) {
        return;
    }
    seen.insert(normalized.clone());
    target.push(normalized);
}

fn collect_changed_files(value: &Value, target: &mut Vec<String>, seen: &mut HashSet<String>, depth: usize) {
    if depth > 4 || target.len() >= 12 {
        return;
    }
    if let Some(entries) = value.as_array() {
        for entry in entries {
            collect_changed_files(entry, target, seen, depth + 1);
            if target.len() >= 12 {
                return;
            }
        }
        return;
    }
    let Some(record) = value.as_object() else { return };
    for key in ["path", "filePath", "relativePath", "filename", "newPath", "oldPath"] {
        push_changed_file(target, seen, record.get(key));
    }
    for nested_key in ["item", "result", "input", "data", "changes", "files", "edits", "patch", "patches", "operations"] {
        let Some(nested) = record.get(nested_key) else { continue };
        collect_changed_files(nested, target, seen, depth + 1);
        if target.len() >= 12 {
            return;
        }
    }
}

fn changed_files(data: &Value) -> Vec<String> {
    let mut target = Vec::new();
    collect_changed_files(data, &mut target, &mut HashSet::new(), 0);
    target
}

/// `(1234).toLocaleString()` in en-US.
fn locale_count(value: usize) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(character);
    }
    out
}

/// `summarizeToolTextOutput(value)`: the first meaningful line (84 units at most), or the line
/// count when only fences carry text.
pub fn summarize_tool_text_output(value: &str) -> Option<String> {
    let mut meaningful_line_count = 0usize;
    for raw_line in value.split('\n') {
        let collapsed = raw_line
            .split(crate::js::is_js_whitespace)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let line = trim(&collapsed);
        if line.is_empty() {
            continue;
        }
        meaningful_line_count += 1;
        if line != "```" {
            return Some(if len16(line) <= 84 {
                line.to_owned()
            } else {
                format!("{}…", trim_end(slice_head16(line, 83)))
            });
        }
    }
    (meaningful_line_count > 1).then(|| format!("{} lines", locale_count(meaningful_line_count)))
}

fn project_command_data(data: &Map<String, Value>) -> Option<Value> {
    let item = as_record(data.get("item"))?;
    let mut projected = Map::new();
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
            projected.insert("input".into(), json!({"command": command}));
        }
    }
    if let Some(result) = as_record(item.get("result")) {
        let mut projected_result = Map::new();
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
    (!projected.is_empty()).then_some(Value::Object(projected))
}

fn project_command_value(data: &Map<String, Value>) -> Option<Value> {
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

/// `isWorkspaceImagePreviewPath(path)`.
pub fn is_workspace_image_preview_path(path: &str) -> bool {
    let without_query = path.split(['?', '#']).next().unwrap_or("").to_lowercase();
    WORKSPACE_IMAGE_PREVIEW_EXTENSIONS.iter().any(|extension| without_query.ends_with(extension))
}

fn project_viewed_image_path(data: &Map<String, Value>) -> Option<String> {
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

fn extract_mcp_result_text(result: &Value) -> Option<String> {
    let Some(record) = result.as_object() else {
        return result.as_str().map(str::to_owned);
    };
    match record.get("content") {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Array(entries)) => {
            let texts: Vec<&str> = entries
                .iter()
                .filter_map(|entry| entry.get("text").and_then(Value::as_str))
                .filter(|text| !trim(text).is_empty())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

fn summarize_mcp_result(result: Option<&Value>) -> Option<Value> {
    let result = result.filter(|value| !value.is_null())?;
    let text = extract_mcp_result_text(result)?;
    if text.is_empty() {
        return None;
    }
    summarize_tool_text_output(&text).map(|summary| json!({"content": summary}))
}

static PREVIEW_TOOL_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:mcp__)?(?:t3-code|t3_code|t3code)_{1,2}preview_(?:open|navigate|status|snapshot|click|type|press|scroll|resize|set_appearance|evaluate|wait_for|recording_start|recording_stop)$",
    )
    .expect("preview tool name pattern")
});
static PREVIEW_URL_TOOL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"preview_(?:open|navigate|status|snapshot)$").expect("preview url tool pattern"));
static FIRST_CONTENT_BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"^\s*\{\s*"content"\s*:\s*\[\s*"#).expect("first block pattern"));

fn js_string(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) => "null".into(),
        Some(other) => other.to_string(),
    }
}

fn is_true(record: Option<&Map<String, Value>>, key: &str) -> bool {
    record.and_then(|record| record.get(key)) == Some(&Value::Bool(true))
}

/// `projectPreviewToolMetadata`: reuse the page URL preview tools already returned.
fn project_preview_tool_metadata(data: &Map<String, Value>, status: Option<&Value>) -> Map<String, Value> {
    let empty = Map::new();
    let item = as_record(data.get("item"));
    let name = match item {
        Some(item) => Some(format!("mcp__{}__{}", js_string(item.get("server")), js_string(item.get("tool")))),
        None => data
            .get("toolName")
            .filter(|value| !value.is_null())
            .or_else(|| data.get("tool"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    let Some(name) = name.filter(|name| PREVIEW_TOOL_NAME.is_match(name)) else {
        return empty;
    };
    let state = as_record(data.get("state"));
    let result = item
        .and_then(|item| item.get("result"))
        .or_else(|| data.get("result"))
        .or_else(|| state.and_then(|state| state.get("output")))
        .filter(|value| !value.is_null());
    let record = as_record(result);
    let status = status.and_then(Value::as_str);
    if status == Some("failed")
        || status == Some("declined")
        || state.and_then(|state| state.get("status")).and_then(Value::as_str) == Some("error")
        || item.and_then(|item| item.get("error")).is_some_and(|error| !error.is_null())
        || is_true(record, "isError")
        || is_true(record, "is_error")
    {
        return empty;
    }

    let mut page: Option<Map<String, Value>> = record.cloned();
    let mut output: Option<Value> = result.cloned();
    for _ in 0..3 {
        if is_true(page.as_ref(), "isError") || is_true(page.as_ref(), "is_error") {
            return empty;
        }
        if let Some(structured) = page.as_ref().and_then(|page| as_record(page.get("structuredContent"))).cloned() {
            page = Some(structured);
            break;
        }
        let text = output
            .as_ref()
            .and_then(extract_mcp_result_text)
            .map(|text| slice_head16(&text, 2 * 1024 * 1024).to_owned());
        let Some(text) = text.filter(|text| !text.is_empty()) else { break };
        match serde_json::from_str::<Value>(zc_core::lenient_json::extract_json_object(&text)) {
            Ok(parsed) => page = parsed.as_object().cloned(),
            Err(_) => {
                let Some(first_block) = FIRST_CONTENT_BLOCK.find(&text) else {
                    return empty;
                };
                match serde_json::from_str::<Value>(zc_core::lenient_json::extract_json_object(&text[first_block.end()..])) {
                    Ok(block) => {
                        page = block
                            .as_object()
                            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
                            .map(|block| json!({"content": [block]}).as_object().cloned().unwrap_or_default());
                    }
                    Err(_) => return empty,
                }
            }
        }
        output = page.clone().map(Value::Object);
    }
    let page_url = page
        .as_ref()
        .and_then(|page| as_record(page.get("toolIcon")))
        .and_then(|icon| icon.get("pageUrl"))
        .filter(|value| !value.is_null());
    let raw = page_url.or_else(|| {
        if PREVIEW_URL_TOOL.is_match(&name) {
            page.as_ref().and_then(|page| page.get("url"))
        } else {
            None
        }
    });
    let Some(raw_url) = as_trimmed_string(raw) else {
        return empty;
    };
    if len16(&raw_url) > 4096 {
        return empty;
    }
    match url::Url::parse(&raw_url) {
        Ok(url) if url.scheme() == "http" || url.scheme() == "https" => {
            let mut out = Map::new();
            out.insert("toolIcon".into(), json!({"_tag": "website", "pageUrl": url.as_str()}));
            out
        }
        _ => empty,
    }
}

static QUESTION_TOOL_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"__|[./]").expect("question tool split"));
static QUESTION_TOOL_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(askuserquestion|requestuserinput(?:async)?|askquestion|question)$").expect("question tool name"));

/// `projectQuestionToolInput(data, title)` (`@t3tools/shared/toolActivity`).
pub fn project_question_tool_input(data: &Map<String, Value>, title: Option<&Value>) -> Map<String, Value> {
    let empty = Map::new();
    let item = as_record(data.get("item"));
    let tool_name = data
        .get("toolName")
        .filter(|value| !value.is_null())
        .or_else(|| data.get("tool").filter(|value| !value.is_null()))
        .or_else(|| item.and_then(|item| item.get("tool")).filter(|value| !value.is_null()))
        .or(title.filter(|value| !value.is_null()));
    let Some(tool_name) = tool_name.and_then(Value::as_str) else {
        return empty;
    };
    let last = QUESTION_TOOL_SPLIT.split(tool_name).last().unwrap_or("");
    let name: String = last
        .chars()
        .filter(|character| *character != '_' && !crate::js::is_js_whitespace(*character))
        .collect::<String>()
        .to_lowercase();
    if name.is_empty() || !QUESTION_TOOL_NAME.is_match(&name) {
        return empty;
    }
    let non_null = |value: Option<&Value>| value.filter(|value| !value.is_null()).cloned();
    let input = non_null(data.get("input"))
        .or_else(|| non_null(data.get("rawInput")))
        .or_else(|| non_null(as_record(data.get("state")).and_then(|state| state.get("input"))))
        .or_else(|| non_null(item.and_then(|item| item.get("arguments"))));
    let input = input.as_ref().and_then(Value::as_object);
    let questions = input.and_then(|input| input.get("questions").filter(|value| !value.is_null())).or_else(|| {
        input
            .and_then(|input| as_record(input.get("params")))
            .and_then(|params| params.get("questions"))
    });
    let Some(questions) = questions.and_then(Value::as_array) else {
        return empty;
    };
    let projected: Vec<Value> = questions
        .iter()
        .map(|value| {
            let question = value.as_object();
            let pick = |key: &str| question.and_then(|question| question.get(key)).filter(|value| !value.is_null());
            let text = as_trimmed_string(
                pick("question")
                    .or_else(|| pick("question_text"))
                    .or_else(|| pick("prompt"))
                    .or_else(|| pick("title")),
            );
            match text {
                Some(text) => json!({"question": text}),
                None => json!({}),
            }
        })
        .collect();
    let mut out = Map::new();
    out.insert("toolName".into(), Value::String(tool_name.to_owned()));
    out.insert("input".into(), json!({"questions": projected}));
    out
}

fn project_mcp_tool_call_data(data: &Map<String, Value>) -> Map<String, Value> {
    let mut projected = Map::new();
    let item = as_record(data.get("item"));
    if let Some(item) = item {
        let mut projected_item = Map::new();
        for key in MCP_ITEM_KEPT_FIELDS {
            if let Some(value) = item.get(*key) {
                projected_item.insert((*key).into(), value.clone());
            }
        }
        if let Some(result) = summarize_mcp_result(item.get("result")) {
            projected_item.insert("result".into(), result);
        }
        projected.insert("item".into(), Value::Object(projected_item));
    }
    for key in ["toolName", "input"] {
        if let Some(value) = data.get(key) {
            projected.insert(key.into(), value.clone());
        }
    }
    if item.is_none() {
        if let Some(result) = summarize_mcp_result(data.get("result")) {
            projected.insert("result".into(), result);
        }
    }
    for key in ["toolCallId", "kind"] {
        if let Some(value) = data.get(key) {
            projected.insert(key.into(), value.clone());
        }
    }
    let files = changed_files(&Value::Object(data.clone()));
    if !files.is_empty() {
        projected.insert("files".into(), Value::Array(files.into_iter().map(|path| json!({"path": path})).collect()));
    }
    projected
}

fn project_raw_output(value: Option<&Value>) -> Option<Value> {
    if let Some(direct) = as_trimmed_string(value) {
        return summarize_tool_text_output(&direct).map(|summary| json!({"content": summary}));
    }
    let raw = as_record(value)?;
    if let Some(total) = raw.get("totalFiles").filter(|value| value.as_f64().is_some_and(f64::is_finite)) {
        let mut out = Obj::new().set("totalFiles", total.clone());
        if raw.get("truncated") == Some(&Value::Bool(true)) {
            out = out.set("truncated", true);
        }
        return Some(out.build());
    }
    for key in ["content", "stdout", "stderr"] {
        if let Some(text) = as_trimmed_string(raw.get(key)) {
            return summarize_tool_text_output(&text).map(|summary| json!({"content": summary}));
        }
    }
    None
}

fn project_acp_content(value: Option<&Value>) -> Option<Value> {
    let entries = value?.as_array()?;
    let text = entries
        .iter()
        .filter_map(|entry| {
            let entry = entry.as_object()?;
            let content = as_record(entry.get("content"))?;
            if entry.get("type").and_then(Value::as_str) == Some("content") && content.get("type").and_then(Value::as_str) == Some("text") {
                as_trimmed_string(content.get("text"))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    summarize_tool_text_output(&text).map(|summary| json!({"content": summary}))
}

/// `projectActivityPayload(activity)`: the activity with a slimmed `payload.data` (the full one
/// stays in the event store). `activity` is wire JSON.
pub fn project_activity_payload(activity: &Value) -> Value {
    let Some(payload) = activity.get("payload").and_then(Value::as_object) else {
        return activity.clone();
    };
    let Some(data) = as_record(payload.get("data")) else {
        return activity.clone();
    };
    let item_status = as_record(data.get("item")).and_then(|item| item.get("status"));
    let mut status_payload = payload.clone();
    if payload.get("status").and_then(Value::as_str) == Some("completed") && matches!(item_status.and_then(Value::as_str), Some("failed" | "declined")) {
        status_payload.insert("status".into(), item_status.cloned().unwrap_or(Value::Null));
    }
    let mut projected_payload = project_preview_tool_metadata(data, status_payload.get("status"));
    for (key, value) in &status_payload {
        projected_payload.insert(key.clone(), value.clone());
    }
    let question_input = project_question_tool_input(data, payload.get("title"));

    let data_value = if payload.get("itemType").and_then(Value::as_str) == Some("mcp_tool_call") {
        let mut projected = project_mcp_tool_call_data(data);
        for (key, value) in question_input {
            projected.insert(key, value);
        }
        projected
    } else {
        let mut projected = question_input;
        if let Some(item) = project_command_data(data) {
            projected.insert("item".into(), item);
        }
        if let Some(command) = project_command_value(data) {
            projected.insert("command".into(), command);
        }
        if let Some(image_path) = project_viewed_image_path(data) {
            projected.insert("imagePath".into(), Value::String(image_path));
        }
        let files = changed_files(&Value::Object(data.clone()));
        if !files.is_empty() {
            projected.insert("files".into(), Value::Array(files.into_iter().map(|path| json!({"path": path})).collect()));
        }
        for key in ["toolCallId", "kind", "toolName"] {
            if let Some(value) = data.get(key) {
                projected.insert(key.into(), value.clone());
            }
        }
        let raw_output = project_raw_output(data.get("rawOutput"))
            .or_else(|| project_acp_content(data.get("content")))
            .or_else(|| {
                if payload.get("itemType").and_then(Value::as_str) == Some("command_execution") {
                    summarize_mcp_result(data.get("result"))
                } else {
                    None
                }
            });
        if let Some(raw_output) = raw_output {
            projected.insert("rawOutput".into(), raw_output);
        }
        projected
    };
    projected_payload.insert("data".into(), Value::Object(data_value));
    let mut out = activity.as_object().cloned().unwrap_or_default();
    out.insert("payload".into(), Value::Object(projected_payload));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarizes_tool_output() {
        assert_eq!(summarize_tool_text_output("\n  first   line \nsecond"), Some("first line".into()));
        assert_eq!(summarize_tool_text_output("```\n```"), Some("2 lines".into()));
        assert_eq!(summarize_tool_text_output("```"), None);
        let long = "x".repeat(100);
        assert_eq!(summarize_tool_text_output(&long), Some(format!("{}…", "x".repeat(83))));
        assert_eq!(locale_count(1234567), "1,234,567");
    }

    #[test]
    fn slims_command_payloads() {
        let activity = json!({
            "id": "a", "kind": "tool.updated", "tone": "tool", "summary": "Ran", "turnId": null, "createdAt": "t",
            "payload": {
                "itemType": "command_execution", "status": "inProgress",
                "data": {"item": {"command": "ls", "aggregatedOutput": "a\nb\n"}, "toolCallId": "c1", "rawOutput": {"stdout": "out"}}
            }
        });
        let projected = project_activity_payload(&activity);
        assert_eq!(
            projected["payload"]["data"],
            json!({"item": {"command": "ls", "aggregatedOutput": "a"}, "toolCallId": "c1", "rawOutput": {"content": "out"}})
        );
    }

    #[test]
    fn keeps_preview_page_urls() {
        let activity = json!({
            "id": "a", "kind": "tool.updated", "tone": "tool", "summary": "s", "turnId": null, "createdAt": "t",
            "payload": {"itemType": "mcp_tool_call", "status": "completed", "data": {
                "toolName": "mcp__t3-code__preview_open",
                "result": {"content": [{"type": "text", "text": "{\"url\":\"http://localhost:3000/a\"}"}]}
            }}
        });
        let projected = project_activity_payload(&activity);
        assert_eq!(
            projected["payload"]["toolIcon"],
            json!({"_tag": "website", "pageUrl": "http://localhost:3000/a"})
        );
    }

    #[test]
    fn projects_question_tool_input() {
        let data = json!({"toolName": "AskUserQuestion", "input": {"questions": [{"question": " Which? "}, {}]}});
        assert_eq!(
            Value::Object(project_question_tool_input(data.as_object().unwrap(), None)),
            json!({"toolName": "AskUserQuestion", "input": {"questions": [{"question": "Which?"}, {}]}})
        );
    }
}
