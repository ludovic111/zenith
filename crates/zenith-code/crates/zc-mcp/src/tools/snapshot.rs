//! `preview_snapshot`, registered by hand in `McpHttpServer.ts` so the PNG goes out as an image
//! block: the page metadata is bounded near 20 KB (Claude Code moves a bigger result to a file,
//! losing the locators) and what was cut is listed, `save` writes the PNG under the browser
//! artifacts directory, and failures carry only their tag plus the server-built preview message.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Map, Value};
use zc_ports::TaggedError;

use crate::params::{decode_arguments, js_length, optional, Spec};
use crate::scope::McpInvocationScope;
use crate::tools::preview::PreviewTools;
use crate::tools::{encode_with_schema, success_schema, text_content, McpServices};

/// `MAX_SNAPSHOT_TEXT_BYTES`.
pub const MAX_SNAPSHOT_TEXT_BYTES: usize = 20_000;
const MAX_SNAPSHOT_VISIBLE_TEXT_CHARS: usize = 8_000;
const MAX_SNAPSHOT_ELEMENT_NAME_CHARS: usize = 200;
const MAX_SNAPSHOT_LOG_ENTRIES: usize = 40;
const MAX_SNAPSHOT_LOG_TEXT_CHARS: usize = 500;
const MAX_SNAPSHOT_IDENTIFIER_CHARS: usize = 2_048;
const MAX_SCREENSHOT_SITE_SLUG_LENGTH: usize = 40;

/// `cutText`: the first `max` UTF-16 units and an ellipsis, when longer. Cuts on a character
/// boundary (JS may split a surrogate pair there).
pub fn cut_text(text: &str, max: usize) -> String {
    if js_length(text) <= max {
        return text.to_owned();
    }
    let mut units = 0;
    let mut out = String::new();
    for c in text.chars() {
        units += c.len_utf16();
        if units > max {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

fn json_text(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn cut_entry_strings(entry: &Value) -> Value {
    match entry {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .map(|(key, value)| {
                    let value = match value {
                        Value::String(text) => Value::String(cut_text(text, MAX_SNAPSHOT_LOG_TEXT_CHARS)),
                        other => other.clone(),
                    };
                    (key.clone(), value)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn has_long_string(entry: &Value, max: usize) -> bool {
    entry
        .as_object()
        .is_some_and(|object| object.values().any(|value| value.as_str().is_some_and(|text| js_length(text) > max)))
}

const SHED_ORDER: [&str; 4] = ["actionTimeline", "networkEntries", "consoleEntries", "interactiveElements"];

/// The bounded snapshot: its value, its JSON text and what was left out.
pub struct BoundedSnapshot {
    pub value: Value,
    pub text: String,
    pub omitted: Vec<String>,
}

/// `boundSnapshotMetadata`: drops the accessibility tree, shortens page text, element names,
/// identifiers and log strings, keeps the newest log entries, then halves one thing per round
/// (logs first, then page text, then the locators) until the JSON fits.
pub fn bound_snapshot_metadata(metadata: &Map<String, Value>) -> BoundedSnapshot {
    let mut omitted: Vec<String> = Vec::new();
    let mut without_tree = metadata.clone();
    if without_tree.shift_remove("accessibilityTree").is_some() {
        omitted.push("accessibilityTree (use interactiveElements locators or preview_evaluate)".into());
    }
    let string_of = |key: &str| metadata.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let list_of = |key: &str| metadata.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
    let url = string_of("url");
    let title = string_of("title");
    let visible_text = string_of("visibleText");
    if js_length(&url) > MAX_SNAPSHOT_IDENTIFIER_CHARS || js_length(&title) > MAX_SNAPSHOT_IDENTIFIER_CHARS {
        omitted.push(format!("url or title after {MAX_SNAPSHOT_IDENTIFIER_CHARS} characters"));
    }
    let elements = list_of("interactiveElements");
    if elements.iter().any(|element| {
        element
            .get("name")
            .and_then(Value::as_str)
            .is_some_and(|name| js_length(name) > MAX_SNAPSHOT_ELEMENT_NAME_CHARS)
    }) {
        omitted.push(format!("element names longer than {MAX_SNAPSHOT_ELEMENT_NAME_CHARS} characters"));
    }
    let mut tail = |entries: Vec<Value>, label: &str| -> Vec<Value> {
        if entries.len() > MAX_SNAPSHOT_LOG_ENTRIES {
            omitted.push(format!("{} older {label}", entries.len() - MAX_SNAPSHOT_LOG_ENTRIES));
        }
        let kept: Vec<Value> = entries[entries.len().saturating_sub(MAX_SNAPSHOT_LOG_ENTRIES)..].to_vec();
        if kept.iter().any(|entry| has_long_string(entry, MAX_SNAPSHOT_LOG_TEXT_CHARS)) {
            omitted.push(format!("{label} text after {MAX_SNAPSHOT_LOG_TEXT_CHARS} characters"));
        }
        kept.iter().map(cut_entry_strings).collect()
    };
    let elements: Vec<Value> = elements
        .iter()
        .map(|element| {
            let mut element = element.clone();
            if let Some(name) = element.get("name").and_then(Value::as_str) {
                let cut = cut_text(name, MAX_SNAPSHOT_ELEMENT_NAME_CHARS);
                element["name"] = Value::String(cut);
            }
            element
        })
        .collect();
    let console = tail(list_of("consoleEntries"), "console entries");
    let network = tail(list_of("networkEntries"), "network entries");
    let timeline = tail(list_of("actionTimeline"), "action timeline entries");

    let mut bounded = without_tree;
    bounded.insert("url".into(), json!(cut_text(&url, MAX_SNAPSHOT_IDENTIFIER_CHARS)));
    bounded.insert("title".into(), json!(cut_text(&title, MAX_SNAPSHOT_IDENTIFIER_CHARS)));
    let original_lengths: Map<String, Value> = [
        ("interactiveElements", elements.len()),
        ("consoleEntries", console.len()),
        ("networkEntries", network.len()),
        ("actionTimeline", timeline.len()),
    ]
    .into_iter()
    .map(|(key, length)| (key.to_owned(), json!(length)))
    .collect();
    let mut lists: Vec<(&str, Vec<Value>)> = vec![
        ("interactiveElements", elements),
        ("consoleEntries", console),
        ("networkEntries", network),
        ("actionTimeline", timeline),
    ];
    let mut dropped: [(&str, usize); 4] = [("actionTimeline", 0), ("networkEntries", 0), ("consoleEntries", 0), ("interactiveElements", 0)];
    let mut visible_chars = js_length(&visible_text).min(MAX_SNAPSHOT_VISIBLE_TEXT_CHARS);
    let compose = |bounded: &Map<String, Value>, lists: &[(&str, Vec<Value>)], visible_chars: usize| -> Value {
        let mut value = bounded.clone();
        value.insert("visibleText".into(), json!(cut_text(&visible_text, visible_chars)));
        for (key, list) in lists {
            value.insert((*key).to_owned(), Value::Array(list.clone()));
        }
        Value::Object(value)
    };
    let list_len = |lists: &[(&str, Vec<Value>)], key: &str| lists.iter().find(|(name, _)| *name == key).map(|(_, list)| list.len()).unwrap_or(0);
    let mut text = json_text(&compose(&bounded, &lists, visible_chars));
    while text.len() > MAX_SNAPSHOT_TEXT_BYTES {
        // Elements carry the locators, so they go last; logs shed newest-last.
        let key = SHED_ORDER
            .iter()
            .copied()
            .find(|candidate| *candidate != "interactiveElements" && list_len(&lists, candidate) > 0)
            .or(if visible_chars > 0 {
                Some("visibleText")
            } else if list_len(&lists, "interactiveElements") > 0 {
                Some("interactiveElements")
            } else {
                None
            });
        let Some(key) = key else { break };
        if key == "visibleText" {
            visible_chars /= 2;
        } else {
            let list = &mut lists.iter_mut().find(|(name, _)| *name == key).expect("known list").1;
            let keep = list.len() / 2;
            dropped.iter_mut().find(|(name, _)| *name == key).expect("known list").1 += list.len() - keep;
            *list = if keep == 0 {
                Vec::new()
            } else if key == "interactiveElements" {
                list[..keep].to_vec()
            } else {
                list[list.len() - keep..].to_vec()
            };
        }
        text = json_text(&compose(&bounded, &lists, visible_chars));
    }
    if visible_chars < js_length(&visible_text) {
        omitted.push(format!("visibleText after {visible_chars} characters (use preview_evaluate for more)"));
    }
    for (key, count) in dropped {
        if count > 0 {
            omitted.push(format!("{count} of {} {key}", original_lengths[key]));
        }
    }
    let value = compose(&bounded, &lists, visible_chars);
    BoundedSnapshot { value, text, omitted }
}

/// The hostname as a filename-safe slug, like the desktop's own screenshot names.
pub fn screenshot_site_slug(raw_url: &str) -> String {
    let Ok(url) = url::Url::parse(raw_url) else {
        return "site".into();
    };
    let host = url.host_str().unwrap_or("").to_lowercase();
    let mut slug = String::new();
    let mut in_run = false;
    for c in host.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
            in_run = false;
        } else if !in_run {
            slug.push('-');
            in_run = true;
        }
    }
    let slug: String = slug.trim_matches('-').chars().take(MAX_SCREENSHOT_SITE_SLUG_LENGTH).collect();
    let slug = slug.trim_end_matches('-').to_owned();
    if slug.is_empty() {
        "site".into()
    } else {
        slug
    }
}

fn base36(mut value: u64) -> String {
    if value == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while value > 0 {
        out.push(digits[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// `saveScreenshot`: writes the PNG under the browser artifacts directory.
pub fn save_screenshot(artifacts_dir: &Path, page_url: &str, png: &[u8]) -> Result<PathBuf, TaggedError> {
    let millis = zc_core::time::now_millis().max(0) as u64;
    // Two saves in the same millisecond must not overwrite each other.
    let file_name = format!(
        "browser-screenshot-{}-{}-{}.png",
        screenshot_site_slug(page_url),
        base36(millis),
        &zc_core::ids::uuid_v4()[..8]
    );
    let path = artifacts_dir.join(file_name);
    std::fs::create_dir_all(artifacts_dir)
        .and_then(|()| std::fs::write(&path, png))
        .map_err(|error| {
            TaggedError::new(
                "PreviewScreenshotSaveError",
                format!("Could not save preview screenshot to {}.", path.display()),
            )
            .with("screenshotPath", path.to_string_lossy().into_owned())
            .with("cause", error.to_string())
        })?;
    Ok(path)
}

/// Whether a tag belongs to the `PreviewAutomationError` union, whose messages are built on
/// the server and tell the agent what to do next.
fn is_preview_automation_error(tag: &str) -> bool {
    matches!(
        tag,
        "PreviewAutomationRecordingTransferError"
            | "PreviewAutomationRecordingDesktopUpdateRequiredError"
            | "PreviewAutomationRecordingTooLargeError"
            | "PreviewAutomationRecordingDeadlineExpiredError"
            | "PreviewAutomationUnavailableError"
            | "PreviewAutomationNoAvailableHostError"
            | "PreviewAutomationUnsupportedClientError"
            | "PreviewAutomationTabNotFoundError"
            | "PreviewAutomationTimeoutError"
            | "PreviewAutomationControlInterruptedError"
            | "PreviewAutomationExecutionError"
            | "PreviewAutomationInvalidSelectorError"
            | "PreviewAutomationTargetNotEditableError"
            | "PreviewAutomationResultTooLargeError"
            | "PreviewAutomationClientDisconnectedError"
            | "PreviewAutomationRequestQueueClosedError"
            | "PreviewAutomationRemoteUnavailableError"
            | "PreviewAutomationMalformedResponseError"
    )
}

/// `previewSnapshotFailure`.
pub fn snapshot_failure(error: &TaggedError) -> Value {
    let message = is_preview_automation_error(&error.tag).then(|| error.message.clone());
    tracing::warn!(operation = "snapshot", error_tag = %error.tag, failure_count = 1, "preview snapshot failed");
    let mut details = Map::new();
    details.insert("_tag".into(), json!(error.tag));
    details.insert("operation".into(), json!("snapshot"));
    details.insert("failureCount".into(), json!(1));
    if let Some(message) = &message {
        details.insert("message".into(), json!(message));
    }
    let text = format!("Preview snapshot failed: {}", message.unwrap_or_else(|| format!("{}.", error.tag)));
    json!({
        "content": [text_content(text)],
        "structuredContent": {"error": Value::Object(details)},
        "isError": true,
    })
}

fn ai_error() -> TaggedError {
    TaggedError::new("AiError", "AiError")
}

/// `preview_snapshot`.
pub async fn call(services: &McpServices, arguments: Option<&Value>, scope: &McpInvocationScope) -> Value {
    match run(services, arguments, scope).await {
        Ok(result) => result,
        Err(error) => snapshot_failure(&error),
    }
}

async fn run(services: &McpServices, arguments: Option<&Value>, scope: &McpInvocationScope) -> Result<Value, TaggedError> {
    let fields = vec![
        optional("tabId", Spec::Trimmed { max: Some(128) }),
        optional("includeImage", Spec::Boolean),
        optional("save", Spec::Boolean),
    ];
    // Parameters that do not decode fail as `AiError` (the hand registration runs the
    // toolkit's own decoding inside the tool).
    let mut input = decode_arguments(&fields, arguments, false).map_err(|_| ai_error())?;
    let include_image = input.shift_remove("includeImage") != Some(Value::Bool(false));
    let save = input.shift_remove("save") == Some(Value::Bool(true));
    // Output selection and saving are MCP-only; the browser still produces a complete snapshot.
    let result = PreviewTools { services }.invoke_targeted(scope, "snapshot", input, None).await?;
    let schema = success_schema("preview_snapshot").ok_or_else(ai_error)?;
    let encoded = encode_with_schema(&result, schema).ok_or_else(ai_error)?;
    let mut page = encoded.as_object().cloned().ok_or_else(ai_error)?;
    let screenshot = page.shift_remove("screenshot").unwrap_or(Value::Null);
    let url = page.get("url").and_then(Value::as_str).unwrap_or("").to_owned();
    let png = base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(screenshot.get("data").and_then(Value::as_str).unwrap_or("").trim_end_matches('='))
        .unwrap_or_default();
    let screenshot_path = if save {
        Some(save_screenshot(&services.browser_artifacts_dir, &url, &png)?)
    } else {
        None
    };
    let mime_type = screenshot.get("mimeType").cloned().unwrap_or(json!("image/png"));
    if let (Some(path), false) = (&screenshot_path, include_image) {
        // The agent only wants a file to show the user. The url keeps the site icon on the row.
        let saved = json!({"url": cut_text(&url, MAX_SNAPSHOT_IDENTIFIER_CHARS), "screenshotPath": path.to_string_lossy()});
        return Ok(json!({
            "content": [text_content(json_text(&saved))],
            "structuredContent": saved,
            "isError": false,
        }));
    }
    let mut metadata = page;
    metadata.insert(
        "screenshot".into(),
        json!({"mimeType": mime_type, "width": screenshot.get("width").cloned().unwrap_or(Value::Null), "height": screenshot.get("height").cloned().unwrap_or(Value::Null)}),
    );
    if let Some(path) = &screenshot_path {
        metadata.insert("screenshotPath".into(), json!(path.to_string_lossy()));
    }
    let bounded = bound_snapshot_metadata(&metadata);
    let mut structured = bounded.value.clone();
    if !bounded.omitted.is_empty() {
        structured["omitted"] = json!(bounded.omitted);
    }
    // Keep the page identity readable even if a provider truncates the snapshot.
    let mut content = vec![
        text_content(json_text(&json!({"url": cut_text(&url, MAX_SNAPSHOT_IDENTIFIER_CHARS)}))),
        text_content(bounded.text.clone()),
    ];
    if !bounded.omitted.is_empty() {
        content.push(text_content(format!("Snapshot text was bounded. Omitted: {}.", bounded.omitted.join("; "))));
    }
    if include_image {
        content.push(json!({"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(&png), "mimeType": mime_type}));
    }
    Ok(json!({"content": content, "structuredContent": structured, "isError": false}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn element(name: &str, index: usize) -> Value {
        json!({"tag": "div", "role": "presentation", "name": name, "selector": format!("div:nth-of-type({index})"), "x": 0, "y": 0, "width": 10, "height": 10})
    }

    fn base() -> Value {
        json!({
            "url": "http://example.test/", "title": "Example", "loading": false, "visibleText": "Example",
            "interactiveElements": [], "accessibilityTree": {}, "consoleEntries": [], "networkEntries": [],
            "actionTimeline": [], "screenshot": {"mimeType": "image/png", "width": 10, "height": 5},
        })
    }

    // McpHttpServer.test.ts: "keeps the snapshot text under the agent's output ceiling"
    #[test]
    fn keeps_the_snapshot_text_under_the_agents_output_ceiling() {
        let page_text = "/Users/someone/Code/project\nClaude, Codex · 79 threads\n".repeat(600);
        let mut value = base();
        value["visibleText"] = json!(page_text);
        value["interactiveElements"] = json!([element(&page_text, 1), element(&page_text, 2), element(&page_text, 3), element("Continue", 4)]);
        value["accessibilityTree"] = json!({"nodes": (0..2000).map(|i| json!({"nodeId": i.to_string()})).collect::<Vec<_>>()});
        value["consoleEntries"] = json!((0..100)
            .map(|i| json!({"level": "log", "text": format!("entry {i}"), "timestamp": "t"}))
            .collect::<Vec<_>>());
        let bounded = bound_snapshot_metadata(&metadata(value));
        assert!(bounded.text.len() <= MAX_SNAPSHOT_TEXT_BYTES);
        let parsed: Value = serde_json::from_str(&bounded.text).unwrap();
        assert!(parsed.get("accessibilityTree").is_none());
        assert!(js_length(parsed["visibleText"].as_str().unwrap()) <= 8_001);
        assert_eq!(parsed["interactiveElements"].as_array().unwrap().len(), 4);
        assert!(js_length(parsed["interactiveElements"][0]["name"].as_str().unwrap()) <= 201);
        assert_eq!(parsed["interactiveElements"][3]["name"], "Continue");
        assert_eq!(parsed["consoleEntries"].as_array().unwrap().len(), 40);
        assert_eq!(parsed["consoleEntries"][0]["text"], "entry 60");
        assert!(bounded.omitted.iter().any(|note| note.contains("accessibilityTree")));
        assert!(bounded.omitted.contains(&"60 older console entries".to_owned()));
        assert_eq!(bounded.value, parsed);
    }

    // "bounds the snapshot text even when nothing but logs and the title are large"
    #[test]
    fn bounds_the_snapshot_text_even_when_nothing_but_logs_and_the_title_are_large() {
        let mut value = base();
        value["title"] = json!("t".repeat(70_000));
        value["consoleEntries"] = json!([{"level": "log", "text": "x".repeat(70_000), "timestamp": "t"}]);
        let bounded = bound_snapshot_metadata(&metadata(value));
        assert!(bounded.text.len() <= MAX_SNAPSHOT_TEXT_BYTES);
        let parsed: Value = serde_json::from_str(&bounded.text).unwrap();
        assert_eq!(js_length(parsed["title"].as_str().unwrap()), 2_049);
        assert_eq!(js_length(parsed["consoleEntries"][0]["text"].as_str().unwrap()), 501);
        let notes = bounded.omitted.join("; ");
        assert!(notes.contains("url or title after 2048 characters"));
        assert!(notes.contains("console entries text after 500 characters"));
    }

    // "bounds page text made of wide characters before dropping locators"
    #[test]
    fn bounds_page_text_made_of_wide_characters_before_dropping_locators() {
        let mut value = base();
        value["visibleText"] = json!("界".repeat(9_000));
        value["interactiveElements"] = json!((0..20).map(|i| element(&format!("Button {i}"), i)).collect::<Vec<_>>());
        let bounded = bound_snapshot_metadata(&metadata(value));
        assert!(bounded.text.len() <= MAX_SNAPSHOT_TEXT_BYTES);
        let parsed: Value = serde_json::from_str(&bounded.text).unwrap();
        let text = parsed["visibleText"].as_str().unwrap();
        assert!(text.ends_with('…') && text.trim_end_matches('…').chars().all(|c| c == '界'));
        assert_eq!(parsed["interactiveElements"].as_array().unwrap().len(), 20);
        assert!(bounded.omitted.join("; ").contains("visibleText after 4000 characters"));
    }

    // "sheds log entries before locators when every list is full"
    #[test]
    fn sheds_log_entries_before_locators_when_every_list_is_full() {
        let long = "x".repeat(2_000);
        let mut value = base();
        value["interactiveElements"] = json!((0..20).map(|i| element(&format!("Button {i}"), i)).collect::<Vec<_>>());
        value["consoleEntries"] = json!((0..200)
            .map(|_| json!({"level": long, "text": long, "timestamp": long, "source": long}))
            .collect::<Vec<_>>());
        value["networkEntries"] = json!((0..200)
            .map(|_| json!({"url": long, "method": long, "status": 200, "failed": false, "errorText": long, "timestamp": long}))
            .collect::<Vec<_>>());
        value["actionTimeline"] = json!((0..200)
            .map(|_| json!({"id": long, "action": long, "status": "succeeded", "startedAt": long, "completedAt": long, "error": long}))
            .collect::<Vec<_>>());
        let bounded = bound_snapshot_metadata(&metadata(value));
        assert!(bounded.text.len() <= MAX_SNAPSHOT_TEXT_BYTES);
        let parsed: Value = serde_json::from_str(&bounded.text).unwrap();
        assert_eq!(parsed["interactiveElements"].as_array().unwrap().len(), 20);
        let logs = ["consoleEntries", "networkEntries", "actionTimeline"]
            .iter()
            .map(|key| parsed[*key].as_array().unwrap().len())
            .sum::<usize>();
        assert!(logs < 120);
        let notes = bounded.omitted.join("; ");
        assert!(notes.contains("40 of 40 actionTimeline"));
        assert!(!notes.contains("interactiveElements") || !regex::Regex::new(r"\d+ of \d+ interactiveElements").unwrap().is_match(&notes));
    }

    #[test]
    fn slugs_hostnames_like_the_desktop() {
        assert_eq!(screenshot_site_slug("http://example.test/"), "example-test");
        assert_eq!(screenshot_site_slug("not a url"), "site");
        assert_eq!(screenshot_site_slug("http://[::1]:3000/"), "1");
        assert_eq!(base36(35), "z");
    }
}
