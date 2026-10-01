//! Native Codex events → canonical provider runtime events (`mapToRuntimeEvents` and the
//! per-session bookkeeping of `CodexAdapter.ts`: turn token usage, rate-limit snapshots,
//! usage-limit and managed-sharing errors).
//!
//! Events are built as JSON in the TS shape (same keys, same omissions) and decoded into
//! [`ProviderRuntimeEvent`] by [`to_runtime_event`]; the JSON is what the `CANON:` log lines
//! record, and what the recorded-fixture gate compares.

use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use zc_codex_protocol as protocol;
use zc_contracts::{ProviderEvent, ProviderEventKind, ProviderRequestKind, ProviderRuntimeEvent};

use crate::elicitation::describe_mcp_elicitation;
use crate::managed::{classify_codex_managed_error, ManagedErrorClass};
use crate::usage_limits::{codex_rate_limits_to_update, codex_usage_limit_message, js_number, merge_codex_rate_limits, CodexRateLimitSnapshot};

// ---------------------------------------------------------------------------------------------
// Small helpers

/// `readPayload`: the payload when it matches the protocol schema `T`.
fn read<T: DeserializeOwned>(payload: Option<&Value>) -> Option<T> {
    serde_json::from_value(payload?.clone()).ok()
}

/// JS `.length` (UTF-16 code units).
fn js_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// JS `.slice(0, n)` on UTF-16 code units.
fn js_slice(value: &str, n: usize) -> String {
    let units: Vec<u16> = value.encode_utf16().take(n).collect();
    String::from_utf16_lossy(&units)
}

/// `.trim().replace(/\s+/g, " ")`.
fn collapse_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `trimText`.
fn trim_text(value: Option<&str>) -> Option<String> {
    value.map(str::trim).filter(|value| !value.is_empty()).map(str::to_owned)
}

fn as_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str)
}

fn record(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value.and_then(Value::as_object)
}

fn insert_some(object: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        object.insert(key.to_owned(), value);
    }
}

// ---------------------------------------------------------------------------------------------
// Tool presentation (MCP browser / computer-use calls)

fn normalized_http_url(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?;
    if js_len(value) > 4096 {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    let href = url.as_str();
    (matches!(url.scheme(), "http" | "https") && js_len(href) <= 4096).then(|| href.to_owned())
}

fn normalized_image_url(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?;
    if js_len(value) > 4096 {
        return None;
    }
    let url = url::Url::parse(value).ok()?;
    matches!(url.scheme(), "http" | "https" | "data").then(|| url.as_str().to_owned())
}

fn normalized_app_id(value: Option<&Value>) -> Option<String> {
    let app_id = value?.as_str()?.trim();
    let valid = !app_id.is_empty() && js_len(app_id) <= 512 && app_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    valid.then(|| app_id.to_owned())
}

fn normalized_display_name(value: Option<&Value>) -> Option<String> {
    let name = collapse_whitespace(value?.as_str()?);
    (!name.is_empty() && js_len(&name) <= 160).then_some(name)
}

fn native_app_source_key(app_id: &str) -> String {
    let key = format!("native-app:{}", app_id.to_lowercase());
    if js_len(&key) <= 512 {
        return key;
    }
    let digest = Sha256::digest(key.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!("{}:{digest}", js_slice(&key, 512 - digest.len() - 1))
}

fn browser_display_name(value: Option<&Value>) -> Option<String> {
    let normalized = normalized_display_name(value)?.to_lowercase();
    if normalized.contains("chrome") || normalized == "chromium" {
        return Some("Chrome".into());
    }
    if normalized.contains("edge") {
        return Some("Microsoft Edge".into());
    }
    if normalized.contains("firefox") {
        return Some("Firefox".into());
    }
    if normalized.contains("safari") {
        return Some("Safari".into());
    }
    if normalized.contains("arc") {
        return Some("Arc".into());
    }
    if normalized == "iab" || normalized.contains("in-app") {
        return Some("Browser".into());
    }
    normalized_display_name(value)
}

fn browser_native_app_reference(name: &str) -> Option<Value> {
    match name {
        "Chrome" => Some(json!({"_tag": "display-name", "displayName": "Google Chrome"})),
        "Microsoft Edge" | "Firefox" | "Safari" | "Arc" => Some(json!({"_tag": "display-name", "displayName": name})),
        _ => None,
    }
}

fn app_display_name_from_id(app_id: &str) -> Option<&'static str> {
    match app_id.to_lowercase().as_str() {
        "com.apple.finder" => Some("Finder"),
        "com.apple.safari" => Some("Safari"),
        "com.google.chrome" => Some("Chrome"),
        "com.microsoft.edgemac" => Some("Microsoft Edge"),
        "org.mozilla.firefox" => Some("Firefox"),
        "company.thebrowser.browser" => Some("Arc"),
        _ => None,
    }
}

/// `nativeAppReference`: `{_tag: "app-id", appId}` / `{_tag: "display-name", displayName}`.
fn native_app_reference(value: Option<&Value>) -> Option<Value> {
    let app = record(value)?;
    match app.get("kind").and_then(Value::as_str) {
        Some("appId") => normalized_app_id(app.get("appId")).map(|app_id| json!({"_tag": "app-id", "appId": app_id})),
        Some("displayName") => normalized_display_name(app.get("displayName")).map(|name| json!({"_tag": "display-name", "displayName": name})),
        _ => None,
    }
}

fn themed_logo_icon(records: &[Option<&Map<String, Value>>]) -> Option<Value> {
    for record in records.iter().flatten() {
        let Some(logo_url) = normalized_image_url(record.get("logoUrl")) else {
            continue;
        };
        let dark = record.get("logoUrlDark").filter(|value| !value.is_null()).or_else(|| record.get("logoDarkUrl"));
        let mut icon = Map::new();
        icon.insert("_tag".into(), json!("themed-logo"));
        icon.insert("logoUrl".into(), json!(logo_url));
        insert_some(&mut icon, "logoUrlDark", normalized_image_url(dark).map(Value::from));
        return Some(Value::Object(icon));
    }
    None
}

/// `McpToolPresentation`: `toolSurface`, `toolIcon`, `toolSource` (each optional).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolPresentation {
    pub tool_surface: Option<&'static str>,
    pub tool_icon: Option<Value>,
    pub tool_source: Option<Value>,
}

impl ToolPresentation {
    fn insert_into(&self, payload: &mut Map<String, Value>) {
        insert_some(payload, "toolSurface", self.tool_surface.map(Value::from));
        insert_some(payload, "toolIcon", self.tool_icon.clone());
        insert_some(payload, "toolSource", self.tool_source.clone());
    }
}

fn first_present<'a>(record: Option<&'a Map<String, Value>>, keys: &[&str]) -> Option<&'a Value> {
    let record = record?;
    keys.iter().find_map(|key| record.get(*key).filter(|value| !value.is_null()))
}

/// `mcpToolPresentation`: browser-use and computer-use calls carry their surface in
/// `result._meta["codex/toolSurface"]`.
pub fn mcp_tool_presentation(item: &Value) -> ToolPresentation {
    let result = record(item.get("result"));
    let metadata = record(result.and_then(|result| result.get("_meta")));
    let surface = record(metadata.and_then(|metadata| metadata.get("codex/toolSurface")));
    let source_metadata = record(metadata.and_then(|metadata| metadata.get("source")));
    let app_context = record(item.get("appContext"));
    let source_logo = themed_logo_icon(&[surface, source_metadata, app_context]);
    match surface.and_then(|surface| surface.get("kind")).and_then(Value::as_str) {
        Some("browserUse") => {
            let surface = surface.expect("surface present");
            let screenshot = record(surface.get("screenshot"));
            let browser_use = record(metadata.and_then(|metadata| metadata.get("browser_use")));
            let latest_open_tab = surface
                .get("openTabs")
                .and_then(Value::as_array)
                .and_then(|tabs| {
                    tabs.iter()
                        .rev()
                        .map(Value::as_object)
                        .find(|tab| normalized_http_url(tab.and_then(|tab| tab.get("url"))).is_some())
                })
                .flatten();
            let candidates = [
                (screenshot, screenshot.and_then(|record| record.get("pageUrl"))),
                (browser_use, browser_use.and_then(|record| record.get("url"))),
                (latest_open_tab, latest_open_tab.and_then(|record| record.get("url"))),
            ];
            let selected = candidates
                .iter()
                .find_map(|(record, url)| normalized_http_url(*url).map(|page_url| (*record, page_url)));
            let favicon = selected
                .as_ref()
                .and_then(|(record, _)| normalized_image_url(first_present(*record, &["faviconUrl", "favIconUrl"])));
            let favicon_dark = selected
                .as_ref()
                .and_then(|(record, _)| normalized_image_url(first_present(*record, &["faviconUrlDark", "favIconUrlDark"])));
            let name = browser_display_name(app_context.and_then(|context| context.get("appName")))
                .or_else(|| browser_display_name(surface.get("browserFamily")))
                .or_else(|| browser_display_name(surface.get("backend")))
                .unwrap_or_else(|| "Browser".to_owned());
            let native_icon = browser_native_app_reference(&name);
            let source_icon = source_logo.or_else(|| native_icon.map(|app| json!({"_tag": "native-app", "app": app})));
            let key_part = name.trim().to_lowercase();
            let key_part = if key_part.is_empty() { "browser".to_owned() } else { key_part };
            let tool_icon = selected.map(|(_, page_url)| {
                let mut icon = Map::new();
                icon.insert("_tag".into(), json!("website"));
                icon.insert("pageUrl".into(), json!(page_url));
                insert_some(&mut icon, "faviconUrl", favicon.map(Value::from));
                insert_some(&mut icon, "faviconUrlDark", favicon_dark.map(Value::from));
                Value::Object(icon)
            });
            let mut source = Map::new();
            source.insert("key".into(), json!(format!("browser-use:{key_part}")));
            source.insert("name".into(), json!(name));
            source.insert("kind".into(), json!(if name == "Browser" { "browser" } else { "integration" }));
            insert_some(&mut source, "icon", source_icon);
            ToolPresentation {
                tool_surface: Some("browser"),
                tool_icon,
                tool_source: Some(Value::Object(source)),
            }
        }
        Some("computerUse") => {
            let surface = surface.expect("surface present");
            let app = native_app_reference(surface.get("app"));
            let args = record(item.get("arguments"));
            let argument_app_name = normalized_display_name(args.and_then(|args| args.get("appName")))
                .or_else(|| normalized_display_name(args.and_then(|args| args.get("application"))))
                .or_else(|| normalized_display_name(args.and_then(|args| args.get("app")).filter(|app| app.is_string())));
            let app_tag = app.as_ref().and_then(|app| app["_tag"].as_str());
            let name = normalized_display_name(app_context.and_then(|context| context.get("appName")))
                .or(argument_app_name)
                .or_else(|| (app_tag == Some("display-name")).then(|| app.as_ref().unwrap()["displayName"].as_str().unwrap_or_default().to_owned()))
                .or_else(|| {
                    (app_tag == Some("app-id"))
                        .then(|| app_display_name_from_id(app.as_ref().unwrap()["appId"].as_str().unwrap_or_default()).map(str::to_owned))
                        .flatten()
                })
                .unwrap_or_else(|| "Computer Use".to_owned());
            let source_icon = source_logo.or_else(|| app.clone().map(|app| json!({"_tag": "native-app", "app": app})));
            let source_key = match (&app, app_tag) {
                (Some(app), Some("app-id")) => native_app_source_key(app["appId"].as_str().unwrap_or_default()),
                (Some(app), _) => format!("native-app-name:{}", app["displayName"].as_str().unwrap_or_default().trim().to_lowercase()),
                (None, _) => "computer-use".to_owned(),
            };
            let mut source = Map::new();
            source.insert("key".into(), json!(source_key));
            source.insert("name".into(), json!(name));
            source.insert("kind".into(), json!("computer"));
            insert_some(&mut source, "icon", source_icon);
            ToolPresentation {
                tool_surface: Some("computer"),
                tool_icon: app.map(|app| json!({"_tag": "native-app", "app": app})),
                tool_source: Some(Value::Object(source)),
            }
        }
        _ => ToolPresentation::default(),
    }
}

// ---------------------------------------------------------------------------------------------
// Item types, titles, details

/// `normalizeItemType`: `agentMessage` → `agent message`.
pub fn normalize_item_type(raw: Option<&str>) -> String {
    let Some(value) = trim_text(raw) else { return "item".into() };
    let chars: Vec<char> = value.chars().collect();
    let mut spaced = String::with_capacity(value.len() + 4);
    for (index, char) in chars.iter().enumerate() {
        spaced.push(*char);
        if (char.is_ascii_lowercase() || char.is_ascii_digit()) && chars.get(index + 1).is_some_and(char::is_ascii_uppercase) {
            spaced.push(' ');
        }
    }
    let replaced: String = spaced.chars().map(|c| if matches!(c, '.' | '_' | '/' | '-') { ' ' } else { c }).collect();
    collapse_whitespace(&replaced).to_lowercase()
}

/// `toCanonicalItemType`.
pub fn to_canonical_item_type(raw: Option<&str>) -> &'static str {
    let kind = normalize_item_type(raw);
    let has = |needle: &str| kind.contains(needle);
    if has("user") {
        "user_message"
    } else if has("agent message") || has("assistant") {
        "assistant_message"
    } else if has("reasoning") || has("thought") {
        "reasoning"
    } else if has("plan") || has("todo") {
        "plan"
    } else if has("command") {
        "command_execution"
    } else if has("file change") || has("patch") || has("edit") {
        "file_change"
    } else if has("mcp") {
        "mcp_tool_call"
    } else if has("dynamic tool") {
        "dynamic_tool_call"
    } else if has("collab") {
        "collab_agent_tool_call"
    } else if has("web search") {
        "web_search"
    } else if has("image") {
        "image_view"
    } else if has("review entered") {
        "review_entered"
    } else if has("review exited") {
        "review_exited"
    } else if has("compact") {
        "context_compaction"
    } else if has("error") {
        "error"
    } else {
        "unknown"
    }
}

fn bounded_tool_argument(value: Option<&Value>) -> Option<String> {
    let normalized = collapse_whitespace(value?.as_str()?);
    if normalized.is_empty() {
        return None;
    }
    Some(if js_len(&normalized) <= 48 {
        normalized
    } else {
        format!("{}…", js_slice(&normalized, 47))
    })
}

/// `normalizedMcpToolName`: the last segment of `server__tool`, `a.b`, `a/b`, `a:b`.
fn normalized_mcp_tool_name(value: &str) -> String {
    let mut last = value;
    let mut rest = value;
    loop {
        let next = rest
            .find("__")
            .map(|index| (index, 2))
            .into_iter()
            .chain(rest.find(['.', '/', ':']).map(|index| (index, 1)))
            .min_by_key(|(index, _)| *index);
        match next {
            Some((index, width)) => {
                rest = &rest[index + width..];
                last = rest;
            }
            None => break,
        }
    }
    last.trim().to_owned()
}

/// `normalizeMcpIntentTitle`: single-line, at most 80 code points.
fn normalize_mcp_intent_title(value: Option<&Value>) -> Option<String> {
    let normalized = collapse_whitespace(value?.as_str()?);
    if normalized.is_empty() {
        return None;
    }
    let chars: Vec<char> = normalized.chars().collect();
    Some(if chars.len() <= 80 {
        normalized
    } else {
        format!("{}…", chars[..79].iter().collect::<String>())
    })
}

fn computer_use_tool_title(item: &Value, presentation: &ToolPresentation) -> Option<String> {
    if normalize_item_type(item["server"].as_str()) != "computer use" || item["status"] == "failed" {
        return None;
    }
    let tool = normalize_item_type(Some(&normalized_mcp_tool_name(item["tool"].as_str().unwrap_or_default()))).replace(' ', "_");
    let in_progress = item["status"] == "inProgress";
    let args = record(item.get("arguments"));
    let source_name = presentation
        .tool_source
        .as_ref()
        .filter(|source| source["kind"] == "computer" && source["name"] != "Computer Use")
        .and_then(|source| source["name"].as_str().map(str::to_owned));
    let app_name = source_name
        .or_else(|| normalized_display_name(args.and_then(|args| args.get("appName"))))
        .or_else(|| normalized_display_name(args.and_then(|args| args.get("application"))))
        .or_else(|| normalized_display_name(args.and_then(|args| args.get("app")).filter(|app| app.is_string())));
    let with_app = |label: &str| match &app_name {
        Some(app) => format!("{label} in {app}"),
        None => label.to_owned(),
    };
    let pick = |progress: &str, done: &str| if in_progress { progress.to_owned() } else { done.to_owned() };
    match tool.as_str() {
        "list_apps" => Some(pick("Listing apps", "Listed apps")),
        "click" => Some(with_app(&pick("Clicking", "Clicked"))),
        "drag" => Some(with_app(&pick("Dragging", "Dragged"))),
        "get_app_state" | "get_state" => Some(match &app_name {
            Some(app) => format!("{} {app}", pick("Looking at", "Looked at")),
            None => pick("Looking at the screen", "Looked at the screen"),
        }),
        "perform_accessibility_action" | "perform_secondary_action" => Some(pick("Performing accessibility action", "Performed accessibility action")),
        "press_key" => Some(with_app(&pick("Pressing key", "Pressed key"))),
        "scroll" => {
            let direction = bounded_tool_argument(args.and_then(|args| args.get("direction"))).map(|direction| direction.to_lowercase());
            let label = format!(
                "{}{}",
                pick("Scrolling", "Scrolled"),
                direction.map(|direction| format!(" {direction}")).unwrap_or_default()
            );
            Some(with_app(&label))
        }
        "set_value" => Some(with_app(&pick("Setting value", "Set value"))),
        "type_text" => Some(with_app(&pick("Typing text", "Typed text"))),
        _ => None,
    }
}

/// `itemTitle`.
fn item_title(item_type: &str, item: Option<&Value>, presentation: &ToolPresentation) -> Option<String> {
    if let Some(item) = item.filter(|item| item_type == "mcp_tool_call" && item["type"] == "mcpToolCall") {
        let tool = item["tool"].as_str().unwrap_or_default();
        if normalized_mcp_tool_name(tool) == "js" {
            if let Some(intent) = normalize_mcp_intent_title(record(item.get("arguments")).and_then(|args| args.get("title"))) {
                return Some(intent);
            }
        }
        if let Some(title) = computer_use_tool_title(item, presentation) {
            return Some(title);
        }
        return Some(format!("{} · {tool}", item["server"].as_str().unwrap_or_default()));
    }
    let title = match item_type {
        "assistant_message" => "Assistant message",
        "user_message" => "User message",
        "reasoning" => "Reasoning",
        "plan" => "Plan",
        "command_execution" => "Ran command",
        "file_change" => "File change",
        "mcp_tool_call" => "MCP tool call",
        "dynamic_tool_call" => "Tool call",
        "web_search" => "Web search",
        "image_view" => "Image view",
        "error" => "Error",
        _ => return None,
    };
    Some(title.to_owned())
}

/// `itemDetail`: the first non-blank of query (web search), command, title, summary, text, path,
/// prompt.
fn item_detail(item_type: &str, item: &Value) -> Option<String> {
    let mut candidates: Vec<Option<&Value>> = Vec::new();
    if item_type == "web_search" {
        let action = item.get("action");
        candidates.push(item.get("query"));
        candidates.push(action.and_then(|action| action.get("query")));
        if let Some(queries) = action.and_then(|action| action.get("queries")).and_then(Value::as_array) {
            candidates.extend(queries.iter().map(Some));
        }
        candidates.push(action.and_then(|action| action.get("pattern")));
        candidates.push(action.and_then(|action| action.get("url")));
    }
    for key in ["command", "title", "summary", "text", "path", "prompt"] {
        candidates.push(item.get(key));
    }
    candidates.into_iter().find_map(|candidate| trim_text(as_str(candidate)))
}

fn non_empty_detail(value: Option<&str>) -> Option<String> {
    trim_text(value)
}

const MAX_DESCRIBED_FILE_CHANGES: usize = 20;

/// An approximation of `String.prototype.localeCompare` (ICU root collation) good enough for
/// paths: punctuation and spaces sort before digits, digits before letters, letters compare
/// case-insensitively first, then lowercase before uppercase.
fn locale_compare(left: &str, right: &str) -> std::cmp::Ordering {
    fn primary(char: char) -> (u8, char) {
        if char.is_alphabetic() {
            (3, char.to_lowercase().next().unwrap_or(char))
        } else if char.is_numeric() {
            (2, char)
        } else {
            (1, char)
        }
    }
    let primaries = |value: &str| value.chars().map(primary).collect::<Vec<_>>();
    primaries(left).cmp(&primaries(right)).then_with(|| {
        let tertiary = |value: &str| value.chars().map(|char| char.is_uppercase()).collect::<Vec<_>>();
        tertiary(left).cmp(&tertiary(right))
    })
}

/// `describeFileChanges`: an apply-patch approval's edited paths (sorted, capped).
fn describe_file_changes(file_changes: Option<&Value>) -> Option<String> {
    let changes = file_changes?.as_object()?;
    let mut entries: Vec<(&String, &Value)> = changes.iter().collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort_by(|(left, _), (right, _)| locale_compare(left, right));
    let described: Vec<String> = entries
        .iter()
        .take(MAX_DESCRIBED_FILE_CHANGES)
        .map(|(path, change)| {
            let kind = change["type"].as_str().unwrap_or_default();
            let move_path = (kind == "update")
                .then(|| change["move_path"].as_str())
                .flatten()
                .filter(|path| !path.is_empty());
            match move_path {
                Some(target) => format!("{kind} {path} -> {target}"),
                None => format!("{kind} {path}"),
            }
        })
        .collect();
    let remaining = entries.len() - described.len();
    Some(if remaining > 0 {
        format!("{}\n+{remaining} more", described.join("\n"))
    } else {
        described.join("\n")
    })
}

/// `toRequestTypeFromMethod`.
pub fn request_type_from_method(method: &str) -> &'static str {
    match method {
        "item/commandExecution/requestApproval" => "command_execution_approval",
        "item/fileRead/requestApproval" => "file_read_approval",
        "item/fileChange/requestApproval" => "file_change_approval",
        "mcpServer/elicitation/request" => "mcp_elicitation_approval",
        "item/permissions/requestApproval" => "permission_approval",
        "applyPatchApproval" => "apply_patch_approval",
        "execCommandApproval" => "exec_command_approval",
        "item/tool/requestUserInput" => "tool_user_input",
        "item/tool/call" => "dynamic_tool_call",
        "account/chatgptAuthTokens/refresh" => "auth_tokens_refresh",
        _ => "unknown",
    }
}

/// `toRequestTypeFromKind`.
pub fn request_type_from_kind(kind: Option<ProviderRequestKind>) -> &'static str {
    match kind {
        Some(ProviderRequestKind::Command) => "command_execution_approval",
        Some(ProviderRequestKind::FileRead) => "file_read_approval",
        Some(ProviderRequestKind::FileChange) => "file_change_approval",
        Some(ProviderRequestKind::McpElicitation) => "mcp_elicitation_approval",
        Some(ProviderRequestKind::Permission) => "permission_approval",
        None => "unknown",
    }
}

/// `toUserInputQuestions`: questions with an id, header, prompt and at least one complete option.
fn to_user_input_questions(questions: &Value) -> Option<Value> {
    let parsed: Vec<Value> = questions
        .as_array()?
        .iter()
        .filter_map(|question| {
            let options: Vec<Value> = question["options"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|option| {
                    let label = trim_text(option["label"].as_str())?;
                    let description = trim_text(option["description"].as_str())?;
                    Some(json!({"label": label, "description": description}))
                })
                .collect();
            let id = trim_text(question["id"].as_str())?;
            let header = trim_text(question["header"].as_str())?;
            let prompt = trim_text(question["question"].as_str())?;
            if options.is_empty() {
                return None;
            }
            Some(json!({"id": id, "header": header, "question": prompt, "options": options, "multiSelect": false}))
        })
        .collect();
    (!parsed.is_empty()).then_some(Value::Array(parsed))
}

/// `toCanonicalUserInputAnswers`: a single answer unwrapped, several kept as a list.
fn to_canonical_user_input_answers(answers: &Value) -> Value {
    let mut out = Map::new();
    for (question_id, value) in answers.as_object().into_iter().flatten() {
        let list = value["answers"].as_array().cloned().unwrap_or_default();
        out.insert(question_id.clone(), if list.len() == 1 { list[0].clone() } else { Value::Array(list) });
    }
    Value::Object(out)
}

const FATAL_CODEX_STDERR_SNIPPETS: &[&str] = &["failed to connect to websocket"];

fn is_fatal_stderr(message: &str) -> bool {
    let normalized = message.to_lowercase();
    FATAL_CODEX_STDERR_SNIPPETS.iter().any(|snippet| normalized.contains(snippet))
}

/// `normalizeCodexTokenUsage`: the context-window snapshot (`last` is the newest response).
fn normalize_codex_token_usage(usage: &protocol::ThreadTokenUsage) -> Option<Value> {
    let used = usage.last.total_tokens;
    if used <= 0 {
        return None;
    }
    let total = usage.total.total_tokens;
    let mut snapshot = Map::new();
    snapshot.insert("usedTokens".into(), json!(used));
    if total > used {
        snapshot.insert("totalProcessedTokens".into(), json!(total));
    }
    if let Some(Some(max)) = usage.model_context_window {
        snapshot.insert("maxTokens".into(), json!(max));
    }
    let last = &usage.last;
    snapshot.insert("inputTokens".into(), json!(last.input_tokens));
    snapshot.insert("cachedInputTokens".into(), json!(last.cached_input_tokens));
    snapshot.insert("outputTokens".into(), json!(last.output_tokens));
    snapshot.insert("reasoningOutputTokens".into(), json!(last.reasoning_output_tokens));
    snapshot.insert("lastUsedTokens".into(), json!(used));
    snapshot.insert("lastInputTokens".into(), json!(last.input_tokens));
    snapshot.insert("lastCachedInputTokens".into(), json!(last.cached_input_tokens));
    snapshot.insert("lastOutputTokens".into(), json!(last.output_tokens));
    snapshot.insert("lastReasoningOutputTokens".into(), json!(last.reasoning_output_tokens));
    snapshot.insert("compactsAutomatically".into(), json!(true));
    Some(Value::Object(snapshot))
}

// ---------------------------------------------------------------------------------------------
// Event construction

fn raw_source(event: &ProviderEvent) -> &'static str {
    if event.kind == ProviderEventKind::Request {
        "codex.app-server.request"
    } else {
        "codex.app-server.notification"
    }
}

/// `runtimeEventBase`.
fn base(event: &ProviderEvent, thread_id: &str) -> Map<String, Value> {
    let mut base = Map::new();
    base.insert("eventId".into(), json!(event.id.as_str()));
    base.insert("provider".into(), json!(event.provider.as_str()));
    base.insert("threadId".into(), json!(thread_id));
    base.insert("createdAt".into(), json!(event.created_at));
    let turn_id = event.turn_id.as_ref().map(|turn| turn.as_str()).filter(|turn| !turn.is_empty());
    let item_id = event.item_id.as_ref().map(|item| item.as_str()).filter(|item| !item.is_empty());
    let request_id = event.request_id.as_ref().map(|request| request.as_str()).filter(|request| !request.is_empty());
    insert_some(&mut base, "turnId", turn_id.map(Value::from));
    insert_some(&mut base, "itemId", item_id.map(Value::from));
    insert_some(&mut base, "requestId", request_id.map(Value::from));
    let mut refs = Map::new();
    insert_some(&mut refs, "providerTurnId", turn_id.map(Value::from));
    insert_some(&mut refs, "providerItemId", item_id.map(Value::from));
    insert_some(&mut refs, "providerRequestId", request_id.map(Value::from));
    if !refs.is_empty() {
        base.insert("providerRefs".into(), Value::Object(refs));
    }
    base.insert(
        "raw".into(),
        json!({
            "source": raw_source(event),
            "method": event.method,
            "payload": event.payload.clone().unwrap_or_else(|| json!({})),
        }),
    );
    base
}

fn make(event: &ProviderEvent, thread_id: &str, kind: &str, payload: Map<String, Value>) -> Value {
    let mut out = base(event, thread_id);
    out.insert("type".into(), json!(kind));
    out.insert("payload".into(), Value::Object(payload));
    Value::Object(out)
}

fn obj(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// `mapItemLifecycle`.
fn map_item_lifecycle(event: &ProviderEvent, thread_id: &str, lifecycle: &str) -> Option<Value> {
    let payload = event.payload.as_ref();
    let valid = read::<protocol::ItemStartedNotification>(payload)
        .map(|notification| notification.item)
        .or_else(|| read::<protocol::ItemCompletedNotification>(payload).map(|notification| notification.item));
    // An item of a type the pinned protocol does not list fails the TS schema check.
    if !valid.is_some_and(|item| !matches!(item, protocol::ThreadItem::Unknown(_))) {
        return None;
    }
    let item = &payload?["item"];
    let item_type = to_canonical_item_type(item["type"].as_str());
    if item_type == "unknown" && lifecycle != "item.updated" {
        return None;
    }
    let detail = item_detail(item_type, item);
    let presentation = if item["type"] == "mcpToolCall" {
        mcp_tool_presentation(item)
    } else {
        ToolPresentation::default()
    };
    let title = item_title(item_type, Some(item), &presentation);
    let status = match lifecycle {
        "item.started" => Some("inProgress".to_owned()),
        "item.completed" => Some(match item["status"].as_str() {
            Some(status @ ("failed" | "declined")) => status.to_owned(),
            _ => "completed".to_owned(),
        }),
        _ => None,
    };
    let mut out = Map::new();
    out.insert("itemType".into(), json!(item_type));
    insert_some(&mut out, "status", status.map(Value::from));
    insert_some(&mut out, "title", title.map(Value::from));
    insert_some(&mut out, "detail", detail.map(Value::from));
    presentation.insert_into(&mut out);
    insert_some(&mut out, "data", event.payload.clone());
    Some(make(event, thread_id, lifecycle, out))
}

/// `mapCollabAgentEvent`: synthetic `collabAgent/*` events → `task.*` (child thread id = task
/// id; a completed child turn is idle, not terminal; `timelineBypass` keeps them out of the
/// parent chat).
fn map_collab_agent_event(event: &ProviderEvent, thread_id: &str) -> Vec<Value> {
    let Some(payload) = event.payload.as_ref().and_then(Value::as_object) else {
        return Vec::new();
    };
    let agent_thread_id = payload.get("agentThreadId").and_then(Value::as_str).unwrap_or_default();
    if agent_thread_id.is_empty() {
        return Vec::new();
    }
    let agent_path = payload.get("agentPath").and_then(Value::as_str);
    let path_leaf = agent_path.and_then(|path| path.split('/').rev().find(|segment| !segment.is_empty()));
    let nickname = payload.get("nickname").and_then(Value::as_str);
    let role = payload.get("role").and_then(Value::as_str).or(path_leaf).unwrap_or("general-purpose");
    let known_name = nickname.or(path_leaf);
    let title = known_name.unwrap_or(agent_thread_id);
    let model = payload.get("model").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    let effort = payload.get("effort").and_then(Value::as_str).map(str::trim).unwrap_or_default();
    let linkage = |extra: Vec<(&str, Value)>| -> Map<String, Value> {
        let mut out = Map::new();
        out.insert("taskId".into(), json!(agent_thread_id));
        for (key, value) in extra {
            out.insert(key.into(), value);
        }
        out.insert("role".into(), json!(role));
        insert_some(&mut out, "title", known_name.filter(|name| !name.is_empty()).map(Value::from));
        if !model.is_empty() {
            out.insert("model".into(), json!(model));
        }
        if !effort.is_empty() {
            out.insert("effort".into(), json!(effort));
        }
        insert_some(&mut out, "agentPath", agent_path.filter(|path| !path.is_empty()).map(Value::from));
        out.insert("timelineBypass".into(), json!(true));
        out
    };
    let updated = |status: &str| make(event, thread_id, "task.updated", linkage(vec![("status", json!(status))]));
    let started = |parent: Option<&str>| {
        let mut payload = linkage(vec![("description", json!(title)), ("title", json!(title))]);
        insert_some(&mut payload, "parentAgentId", parent.map(Value::from));
        make(event, thread_id, "task.started", payload)
    };
    match event.method.as_str() {
        "collabAgent/started" => vec![started(payload.get("parentThreadId").and_then(Value::as_str))],
        "collabAgent/metadataUpdated" => vec![make(event, thread_id, "task.updated", linkage(vec![]))],
        "collabAgent/activity" => match payload.get("activityKind").and_then(Value::as_str) {
            Some("interrupted") => vec![updated("interrupted")],
            Some("started") => vec![started(None)],
            // Reading a child's result also emits "interacted" after its turn is idle.
            _ => Vec::new(),
        },
        "collabAgent/turnStarted" => vec![updated("running")],
        "collabAgent/turnCompleted" => {
            let status = match payload.get("turn").and_then(|turn| turn.get("status")).and_then(Value::as_str) {
                Some("failed") => "failed",
                Some("interrupted") => "interrupted",
                _ => "idle",
            };
            vec![updated(status)]
        }
        "collabAgent/statusChanged" => {
            let status = payload.get("status").and_then(Value::as_object);
            match status.and_then(|status| status.get("type")).and_then(Value::as_str) {
                Some("systemError") => vec![updated("failed")],
                Some("active") => {
                    let waiting = status
                        .and_then(|status| status.get("activeFlags"))
                        .and_then(Value::as_array)
                        .is_some_and(|flags| flags.iter().any(|flag| flag == "waitingOnApproval" || flag == "waitingOnUserInput"));
                    vec![updated(if waiting { "waiting" } else { "running" })]
                }
                Some("idle") => vec![updated("idle")],
                _ => Vec::new(),
            }
        }
        "collabAgent/tokenUsage" => {
            let total = payload.get("tokenUsage").and_then(|usage| usage.get("total")).and_then(Value::as_object);
            let count = |key: &str| {
                total
                    .and_then(|total| total.get(key))
                    .and_then(Value::as_f64)
                    .filter(|value| value.is_finite() && *value >= 0.0)
            };
            let Some(total_tokens) = count("totalTokens") else { return Vec::new() };
            let mut usage = Map::new();
            usage.insert("totalTokens".into(), js_number(total_tokens));
            for key in ["inputTokens", "cachedInputTokens", "outputTokens", "reasoningOutputTokens"] {
                insert_some(&mut usage, key, count(key).map(js_number));
            }
            vec![make(
                event,
                thread_id,
                "task.progress",
                linkage(vec![("description", json!(title)), ("typedUsage", Value::Object(usage))]),
            )]
        }
        "collabAgent/item" => {
            let item = payload.get("item").and_then(Value::as_object);
            let Some(raw_type) = item.and_then(|item| item.get("type")).and_then(Value::as_str) else {
                return Vec::new();
            };
            let loose = ["command", "title", "query"]
                .iter()
                .find_map(|key| item.and_then(|item| item.get(*key)).and_then(Value::as_str))
                .map(str::to_owned);
            let summary = loose.unwrap_or_else(|| to_canonical_item_type(Some(raw_type)).replace('_', " "));
            vec![make(
                event,
                thread_id,
                "task.progress",
                linkage(vec![("description", json!(title)), ("summary", json!(summary))]),
            )]
        }
        "collabAgent/closed" => vec![updated("interrupted")],
        _ => Vec::new(),
    }
}

fn content_delta(event: &ProviderEvent, thread_id: &str, kind: &str, delta: Option<String>, extra: Option<(&str, Value)>) -> Vec<Value> {
    let Some(delta) = delta.filter(|delta| !delta.is_empty()) else {
        return Vec::new();
    };
    let mut payload = Map::new();
    payload.insert("streamKind".into(), json!(kind));
    payload.insert("delta".into(), json!(delta));
    if let Some((key, value)) = extra {
        payload.insert(key.into(), value);
    }
    vec![make(event, thread_id, "content.delta", payload)]
}

/// `mapToRuntimeEvents`: one native event → its canonical events (possibly none).
pub fn map_to_runtime_events(event: &ProviderEvent, thread_id: &str) -> Vec<Value> {
    let payload = event.payload.as_ref();
    let method = event.method.as_str();
    if event.kind == ProviderEventKind::Notification && method.starts_with("collabAgent/") {
        return map_collab_agent_event(event, thread_id);
    }
    if event.kind == ProviderEventKind::Error {
        let Some(message) = event.message.as_ref().filter(|message| !message.is_empty()) else {
            return Vec::new();
        };
        let mut out = obj(json!({"message": message, "class": "provider_error"}));
        insert_some(&mut out, "detail", event.payload.clone());
        return vec![make(event, thread_id, "runtime.error", out)];
    }
    if event.kind == ProviderEventKind::Request {
        return map_request(event, thread_id);
    }
    match method {
        "item/requestApproval/decision" if event.request_id.is_some() => {
            let request_type = match event.request_kind {
                Some(kind) => request_type_from_kind(Some(kind)),
                None => request_type_from_method(method),
            };
            let decision = payload
                .and_then(|payload| payload.get("decision"))
                .and_then(Value::as_str)
                .filter(|decision| zc_contracts::ProviderApprovalDecision::ALL.iter().any(|known| known.as_str() == *decision));
            let mut out = obj(json!({"requestType": request_type}));
            insert_some(&mut out, "decision", decision.map(Value::from));
            insert_some(&mut out, "resolution", event.payload.clone());
            vec![make(event, thread_id, "request.resolved", out)]
        }
        "session/connecting" | "session/ready" => {
            let mut out = obj(json!({"state": if method == "session/ready" { "ready" } else { "starting" }}));
            insert_some(&mut out, "reason", event.message.clone().map(Value::from));
            vec![make(event, thread_id, "session.state.changed", out)]
        }
        "session/started" => {
            let mut out = Map::new();
            insert_some(&mut out, "message", event.message.clone().map(Value::from));
            insert_some(&mut out, "resume", event.payload.clone());
            vec![make(event, thread_id, "session.started", out)]
        }
        "session/exited" | "session/closed" => {
            let mut out = Map::new();
            insert_some(&mut out, "reason", event.message.clone().map(Value::from));
            if method == "session/closed" {
                out.insert("exitKind".into(), json!("graceful"));
            }
            vec![make(event, thread_id, "session.exited", out)]
        }
        "thread/started" => match read::<protocol::ThreadStartedNotification>(payload) {
            Some(started) => vec![make(event, thread_id, "thread.started", obj(json!({"providerThreadId": started.thread.id})))],
            None => Vec::new(),
        },
        "thread/status/changed" | "thread/archived" | "thread/unarchived" | "thread/closed" | "thread/compacted" => {
            let state = match method {
                "thread/archived" => "archived",
                "thread/closed" => "closed",
                "thread/compacted" => "compacted",
                "thread/status/changed" => match read::<protocol::ThreadStatusChangedNotification>(payload) {
                    Some(changed) => match changed.status {
                        protocol::ThreadStatus::Idle(_) => "idle",
                        protocol::ThreadStatus::SystemError(_) => "error",
                        _ => "active",
                    },
                    None => "active",
                },
                _ => "active",
            };
            let mut out = obj(json!({"state": state}));
            insert_some(&mut out, "detail", event.payload.clone());
            vec![make(event, thread_id, "thread.state.changed", out)]
        }
        "thread/name/updated" => {
            let updated = read::<protocol::ThreadNameUpdatedNotification>(payload);
            let mut out = Map::new();
            if let Some(updated) = &updated {
                let name = updated.thread_name.clone().flatten();
                insert_some(&mut out, "name", trim_text(name.as_deref()).map(Value::from));
                let mut metadata = obj(json!({"threadId": updated.thread_id}));
                insert_some(&mut metadata, "threadName", name.map(Value::from));
                out.insert("metadata".into(), Value::Object(metadata));
            }
            vec![make(event, thread_id, "thread.metadata.updated", out)]
        }
        "thread/tokenUsage/updated" => {
            match read::<protocol::ThreadTokenUsageUpdatedNotification>(payload).and_then(|updated| normalize_codex_token_usage(&updated.token_usage)) {
                Some(usage) => vec![make(event, thread_id, "thread.token-usage.updated", obj(json!({"usage": usage})))],
                None => Vec::new(),
            }
        }
        "turn/started" => {
            if event.turn_id.as_ref().is_none_or(|turn| turn.as_str().is_empty()) {
                return Vec::new();
            }
            vec![make(event, thread_id, "turn.started", Map::new())]
        }
        "turn/completed" => {
            let Some(completed) = read::<protocol::TurnCompletedNotification>(payload) else {
                return Vec::new();
            };
            let state = match completed.turn.status {
                protocol::TurnStatus::Failed => "failed",
                protocol::TurnStatus::Interrupted => "interrupted",
                ref other if other.as_str() == "cancelled" => "cancelled",
                _ => "completed",
            };
            let error_message = trim_text(completed.turn.error.clone().flatten().map(|error| error.message).as_deref());
            let mut out = obj(json!({"state": state}));
            insert_some(&mut out, "errorMessage", error_message.map(Value::from));
            vec![make(event, thread_id, "turn.completed", out)]
        }
        "turn/aborted" => vec![make(
            event,
            thread_id,
            "turn.aborted",
            obj(json!({"reason": event.message.clone().unwrap_or_else(|| "Turn aborted".into())})),
        )],
        "turn/plan/updated" => {
            let Some(updated) = read::<protocol::TurnPlanUpdatedNotification>(payload) else {
                return Vec::new();
            };
            let mut out = Map::new();
            insert_some(
                &mut out,
                "explanation",
                trim_text(updated.explanation.clone().flatten().as_deref()).map(Value::from),
            );
            let plan: Vec<Value> = updated
                .plan
                .iter()
                .map(|step| {
                    let status = match step.status.as_str() {
                        status @ ("completed" | "inProgress") => status,
                        _ => "pending",
                    };
                    json!({"step": trim_text(Some(&step.step)).unwrap_or_else(|| "step".into()), "status": status})
                })
                .collect();
            out.insert("plan".into(), Value::Array(plan));
            vec![make(event, thread_id, "turn.plan.updated", out)]
        }
        "turn/diff/updated" => match read::<protocol::TurnDiffUpdatedNotification>(payload) {
            Some(updated) => vec![make(event, thread_id, "turn.diff.updated", obj(json!({"unifiedDiff": updated.diff})))],
            None => Vec::new(),
        },
        "item/started" => map_item_lifecycle(event, thread_id, "item.started").into_iter().collect(),
        "item/completed" => map_item_completed(event, thread_id),
        "item/reasoning/summaryPartAdded" | "item/commandExecution/terminalInteraction" => {
            let mut out = obj(json!({"itemType": if method == "item/reasoning/summaryPartAdded" { "reasoning" } else { "command_execution" }}));
            insert_some(&mut out, "data", event.payload.clone());
            vec![make(event, thread_id, "item.updated", out)]
        }
        "item/plan/delta" => {
            let delta = event
                .text_delta
                .clone()
                .or_else(|| read::<protocol::PlanDeltaNotification>(payload).map(|delta| delta.delta));
            match delta.filter(|delta| !delta.is_empty()) {
                Some(delta) => vec![make(event, thread_id, "turn.proposed.delta", obj(json!({"delta": delta})))],
                None => Vec::new(),
            }
        }
        "item/agentMessage/delta" => {
            let delta = event
                .text_delta
                .clone()
                .or_else(|| read::<protocol::AgentMessageDeltaNotification>(payload).map(|delta| delta.delta));
            content_delta(event, thread_id, "assistant_text", delta, None)
        }
        "item/commandExecution/outputDelta" => {
            let delta = event
                .text_delta
                .clone()
                .or_else(|| read::<protocol::CommandExecutionOutputDeltaNotification>(payload).map(|delta| delta.delta));
            content_delta(event, thread_id, "command_output", delta, None)
        }
        "item/fileChange/outputDelta" => {
            let delta = event
                .text_delta
                .clone()
                .or_else(|| read::<protocol::FileChangeOutputDeltaNotification>(payload).map(|delta| delta.delta));
            content_delta(event, thread_id, "file_change_output", delta, None)
        }
        "item/reasoning/summaryTextDelta" => {
            let typed = read::<protocol::ReasoningSummaryTextDeltaNotification>(payload);
            let delta = event.text_delta.clone().or_else(|| typed.as_ref().map(|delta| delta.delta.clone()));
            content_delta(
                event,
                thread_id,
                "reasoning_summary_text",
                delta,
                typed.map(|typed| ("summaryIndex", json!(typed.summary_index))),
            )
        }
        "item/reasoning/textDelta" => {
            let typed = read::<protocol::ReasoningTextDeltaNotification>(payload);
            let delta = event.text_delta.clone().or_else(|| typed.as_ref().map(|delta| delta.delta.clone()));
            content_delta(
                event,
                thread_id,
                "reasoning_text",
                delta,
                typed.map(|typed| ("contentIndex", json!(typed.content_index))),
            )
        }
        "item/mcpToolCall/progress" => match read::<protocol::McpToolCallProgressNotification>(payload) {
            Some(progress) => vec![make(event, thread_id, "tool.progress", obj(json!({"summary": progress.message})))],
            None => Vec::new(),
        },
        "serverRequest/resolved" => {
            if read::<protocol::ServerRequestResolvedNotification>(payload).is_none() {
                return Vec::new();
            }
            let mut out = obj(json!({"requestType": request_type_from_kind(event.request_kind)}));
            insert_some(&mut out, "resolution", event.payload.clone());
            vec![make(event, thread_id, "request.resolved", out)]
        }
        "item/tool/requestUserInput/answered" => {
            if read::<protocol::ToolRequestUserInputResponse>(payload).is_none() {
                return Vec::new();
            }
            let answers = to_canonical_user_input_answers(&payload.expect("validated")["answers"]);
            vec![make(event, thread_id, "user-input.resolved", obj(json!({"answers": answers})))]
        }
        "model/rerouted" => match read::<protocol::ModelReroutedNotification>(payload) {
            Some(rerouted) => vec![make(
                event,
                thread_id,
                "model.rerouted",
                obj(json!({"fromModel": rerouted.from_model, "toModel": rerouted.to_model, "reason": rerouted.reason})),
            )],
            None => Vec::new(),
        },
        "deprecationNotice" => match read::<protocol::DeprecationNoticeNotification>(payload) {
            Some(notice) => {
                let mut out = obj(json!({"summary": notice.summary}));
                insert_some(&mut out, "details", trim_text(notice.details.clone().flatten().as_deref()).map(Value::from));
                vec![make(event, thread_id, "deprecation.notice", out)]
            }
            None => Vec::new(),
        },
        "configWarning" => match read::<protocol::ConfigWarningNotification>(payload) {
            Some(warning) => {
                let mut out = obj(json!({"summary": warning.summary}));
                insert_some(&mut out, "details", trim_text(warning.details.clone().flatten().as_deref()).map(Value::from));
                insert_some(&mut out, "path", trim_text(warning.path.clone().flatten().as_deref()).map(Value::from));
                insert_some(
                    &mut out,
                    "range",
                    payload.and_then(|payload| payload.get("range")).filter(|range| !range.is_null()).cloned(),
                );
                vec![make(event, thread_id, "config.warning", out)]
            }
            None => Vec::new(),
        },
        "account/updated" => {
            if read::<protocol::AccountUpdatedNotification>(payload).is_none() {
                return Vec::new();
            }
            vec![make(
                event,
                thread_id,
                "account.updated",
                obj(json!({"account": event.payload.clone().unwrap_or_else(|| json!({}))})),
            )]
        }
        "account/rateLimits/updated" => {
            let limits = read::<protocol::AccountRateLimitsUpdatedNotification>(payload)
                .and_then(|_| CodexRateLimitSnapshot::from_value(&payload.expect("validated")["rateLimits"]))
                .and_then(|snapshot| codex_rate_limits_to_update(&snapshot));
            match limits {
                Some(limits) => vec![make(event, thread_id, "account.rate-limits.updated", obj(json!({"limits": limits})))],
                None => Vec::new(),
            }
        }
        "mcpServer/oauthLogin/completed" => match read::<protocol::McpServerOauthLoginCompletedNotification>(payload) {
            Some(completed) => {
                let mut out = obj(json!({"success": completed.success, "name": completed.name}));
                insert_some(&mut out, "error", trim_text(completed.error.clone().flatten().as_deref()).map(Value::from));
                vec![make(event, thread_id, "mcp.oauth.completed", out)]
            }
            None => Vec::new(),
        },
        "thread/realtime/started" => {
            if read::<protocol::ThreadRealtimeStartedNotification>(payload).is_none() {
                return Vec::new();
            }
            let mut out = Map::new();
            insert_some(
                &mut out,
                "realtimeSessionId",
                payload.and_then(|payload| payload.get("realtimeSessionId")).filter(|id| !id.is_null()).cloned(),
            );
            vec![make(event, thread_id, "thread.realtime.started", out)]
        }
        "thread/realtime/itemAdded" => {
            if read::<protocol::ThreadRealtimeItemAddedNotification>(payload).is_none() {
                return Vec::new();
            }
            vec![make(
                event,
                thread_id,
                "thread.realtime.item-added",
                obj(json!({"item": payload.expect("validated")["item"]})),
            )]
        }
        "thread/realtime/outputAudio/delta" => {
            if read::<protocol::ThreadRealtimeOutputAudioDeltaNotification>(payload).is_none() {
                return Vec::new();
            }
            vec![make(
                event,
                thread_id,
                "thread.realtime.audio.delta",
                obj(json!({"audio": payload.expect("validated")["audio"]})),
            )]
        }
        "thread/realtime/error" => {
            let message = read::<protocol::ThreadRealtimeErrorNotification>(payload)
                .map(|error| error.message)
                .or_else(|| event.message.clone())
                .unwrap_or_else(|| "Realtime error".into());
            vec![make(event, thread_id, "thread.realtime.error", obj(json!({"message": message})))]
        }
        "thread/realtime/closed" => {
            let reason = read::<protocol::ThreadRealtimeClosedNotification>(payload)
                .and_then(|closed| closed.reason.flatten())
                .or_else(|| event.message.clone());
            let mut out = Map::new();
            insert_some(&mut out, "reason", reason.map(Value::from));
            vec![make(event, thread_id, "thread.realtime.closed", out)]
        }
        "error" => {
            let typed = read::<protocol::ErrorNotification>(payload);
            let message = typed
                .as_ref()
                .map(|error| error.error.message.clone())
                .or_else(|| event.message.clone())
                .unwrap_or_else(|| "Provider runtime error".into());
            let will_retry = typed.as_ref().is_some_and(|error| error.will_retry);
            let mut out = obj(json!({"message": message}));
            if !will_retry {
                out.insert("class".into(), json!("provider_error"));
            }
            insert_some(&mut out, "detail", event.payload.clone());
            vec![make(event, thread_id, if will_retry { "runtime.warning" } else { "runtime.error" }, out)]
        }
        "process/stderr" => {
            let message = event.message.clone().unwrap_or_else(|| "Codex process stderr".into());
            let fatal = is_fatal_stderr(&message);
            let mut out = obj(json!({"message": message}));
            if fatal {
                out.insert("class".into(), json!("provider_error"));
            }
            insert_some(&mut out, "detail", event.payload.clone());
            vec![make(event, thread_id, if fatal { "runtime.error" } else { "runtime.warning" }, out)]
        }
        "windows/worldWritableWarning" => {
            if read::<protocol::WindowsWorldWritableWarningNotification>(payload).is_none() {
                return Vec::new();
            }
            let mut out = obj(json!({"message": event.message.clone().unwrap_or_else(|| "Windows world-writable warning".into())}));
            insert_some(&mut out, "detail", event.payload.clone());
            vec![make(event, thread_id, "runtime.warning", out)]
        }
        "windowsSandbox/setupCompleted" => {
            let Some(completed) = read::<protocol::WindowsSandboxSetupCompletedNotification>(payload) else {
                return Vec::new();
            };
            let success_message = event.message.clone().unwrap_or_else(|| "Windows sandbox setup completed".into());
            let failure_message = event.message.clone().unwrap_or_else(|| "Windows sandbox setup failed".into());
            let failed = !completed.success;
            let mut state = obj(json!({
                "state": if failed { "error" } else { "ready" },
                "reason": if failed { &failure_message } else { &success_message },
            }));
            insert_some(&mut state, "detail", event.payload.clone());
            let mut events = vec![make(event, thread_id, "session.state.changed", state)];
            if failed {
                let mut warning = obj(json!({"message": failure_message}));
                insert_some(&mut warning, "detail", event.payload.clone());
                events.push(make(event, thread_id, "runtime.warning", warning));
            }
            events
        }
        _ => Vec::new(),
    }
}

fn map_item_completed(event: &ProviderEvent, thread_id: &str) -> Vec<Value> {
    let payload = event.payload.as_ref();
    let Some(completed) = read::<protocol::ItemCompletedNotification>(payload) else {
        return Vec::new();
    };
    if matches!(completed.item, protocol::ThreadItem::Unknown(_)) {
        return Vec::new();
    }
    let item = &payload.expect("validated")["item"];
    if let protocol::ThreadItem::AgentMessage(message) = &completed.item {
        let questions = message.questions.clone().flatten().unwrap_or_default();
        if matches!(message.delivery, Some(Some(protocol::AgentMessageDelivery::Async))) && !questions.is_empty() {
            let id = format!("codex-async:{thread_id}:{}", message.id);
            let mut out = base(event, thread_id);
            out.insert("type".into(), json!("user-input.requested"));
            out.insert("requestId".into(), json!(id));
            out.insert("eventId".into(), json!(id));
            out.insert(
                "payload".into(),
                json!({
                    "responseMode": "message",
                    "questions": questions.iter().enumerate().map(|(index, question)| json!({
                        "id": index.to_string(),
                        "header": "Question",
                        "question": question.title,
                        "options": question.options.clone().flatten().unwrap_or_default().iter().map(|label| json!({"label": label, "description": ""})).collect::<Vec<_>>(),
                        "allowCustomAnswer": true,
                        "multiSelect": false,
                    })).collect::<Vec<_>>(),
                }),
            );
            return vec![Value::Object(out)];
        }
    }
    let item_type = to_canonical_item_type(item["type"].as_str());
    if item_type == "plan" {
        return match item_detail(item_type, item) {
            Some(plan) => vec![make(event, thread_id, "turn.proposed.completed", obj(json!({"planMarkdown": plan})))],
            None => Vec::new(),
        };
    }
    let Some(completed_event) = map_item_lifecycle(event, thread_id, "item.completed") else {
        return Vec::new();
    };
    if item_type != "context_compaction" {
        return vec![completed_event];
    }
    let mut compacted = base(event, thread_id);
    compacted.insert("eventId".into(), json!(format!("{}:thread-compacted", event.id.as_str())));
    compacted.insert("type".into(), json!("thread.state.changed"));
    compacted.insert("payload".into(), json!({"state": "compacted"}));
    vec![completed_event, Value::Object(compacted)]
}

fn map_request(event: &ProviderEvent, thread_id: &str) -> Vec<Value> {
    let payload = event.payload.as_ref();
    let method = event.method.as_str();
    if method == "item/tool/requestUserInput" {
        if read::<protocol::ToolRequestUserInputParams>(payload).is_none() {
            return Vec::new();
        }
        return match to_user_input_questions(&payload.expect("validated")["questions"]) {
            Some(questions) => vec![make(event, thread_id, "user-input.requested", obj(json!({"questions": questions})))],
            None => Vec::new(),
        };
    }
    let elicitation = (method == "mcpServer/elicitation/request")
        .then(|| read::<protocol::McpServerElicitationRequestParams>(payload))
        .flatten()
        .filter(|params| !matches!(params, protocol::McpServerElicitationRequestParams::Unknown(_)))
        .map(|_| payload.expect("validated").clone());
    let approval = elicitation.as_ref().map(describe_mcp_elicitation);
    let detail: Option<String> = match method {
        "item/commandExecution/requestApproval" => {
            read::<protocol::CommandExecutionRequestApprovalParams>(payload).and_then(|params| params.command.flatten().or(params.reason.flatten()))
        }
        "item/fileChange/requestApproval" => read::<protocol::FileChangeRequestApprovalParams>(payload).and_then(|params| {
            non_empty_detail(params.reason.clone().flatten().as_deref()).or_else(|| non_empty_detail(params.grant_root.clone().flatten().as_deref()))
        }),
        "mcpServer/elicitation/request" => elicitation.as_ref().and_then(|payload| payload["message"].as_str().map(str::to_owned)),
        "item/permissions/requestApproval" => read::<protocol::PermissionsRequestApprovalParams>(payload).and_then(|_| {
            let payload = payload.expect("validated");
            let paths: Vec<String> = ["read", "write"]
                .iter()
                .flat_map(|key| payload["permissions"]["fileSystem"][*key].as_array().cloned().unwrap_or_default())
                .filter_map(|path| path.as_str().map(str::to_owned))
                .collect();
            non_empty_detail(payload["reason"].as_str()).or_else(|| (!paths.is_empty()).then(|| format!("Access: {}", paths.join(", "))))
        }),
        "applyPatchApproval" => read::<protocol::ApplyPatchApprovalParams>(payload).and_then(|_| {
            let payload = payload.expect("validated");
            non_empty_detail(payload["reason"].as_str())
                .or_else(|| describe_file_changes(payload.get("fileChanges")))
                .or_else(|| non_empty_detail(payload["grantRoot"].as_str()))
        }),
        "execCommandApproval" => {
            read::<protocol::ExecCommandApprovalParams>(payload).map(|params| params.reason.clone().flatten().unwrap_or_else(|| params.command.join(" ")))
        }
        "item/tool/call" => read::<protocol::DynamicToolCallParams>(payload).map(|params| params.tool),
        _ => None,
    };
    let mut out = obj(json!({"requestType": request_type_from_method(method)}));
    insert_some(&mut out, "detail", detail.filter(|detail| !detail.is_empty()).map(Value::from));
    if let Some(approval) = approval {
        out.insert("appName".into(), json!(approval.app_name));
        out.insert("options".into(), approval.options_value());
    }
    insert_some(&mut out, "args", event.payload.clone());
    vec![make(event, thread_id, "request.opened", out)]
}

// ---------------------------------------------------------------------------------------------
// Turn token usage

#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Cumulative {
    input: i64,
    cached: i64,
    cache_creation: Option<i64>,
    output: i64,
    reasoning: i64,
}

#[derive(Debug, Clone, Default)]
struct Accumulator {
    input: i64,
    cached: i64,
    cache_creation: Option<i64>,
    output: i64,
    reasoning: i64,
    observed: bool,
    has_subagents: bool,
}

/// `CodexTurnTokenUsageState`.
#[derive(Debug, Clone, Default)]
pub struct TurnTokenUsageState {
    baseline: Option<Cumulative>,
    active_turn_id: Option<String>,
    by_turn_id: HashMap<String, Accumulator>,
}

impl TurnTokenUsageState {
    fn accumulator(&mut self, turn_id: &str) -> &mut Accumulator {
        self.by_turn_id.entry(turn_id.to_owned()).or_insert_with(|| Accumulator {
            cache_creation: Some(0),
            ..Accumulator::default()
        })
    }

    /// Rollback drops the baseline: the next update counts only `last`.
    pub fn reset(&mut self) {
        self.baseline = None;
        self.active_turn_id = None;
        self.by_turn_id.clear();
    }

    fn turn_started(&mut self, turn_id: &str) {
        if self.active_turn_id.as_deref() != Some(turn_id) {
            self.by_turn_id.clear();
            self.active_turn_id = Some(turn_id.to_owned());
            self.accumulator(turn_id);
        }
    }

    /// `accumulateCodexTurnTokenUsage`: within a turn the growth of `total` is the delta; without
    /// a prior total (or after Codex reset it) `last` is.
    fn accumulate(&mut self, turn_id: &str, usage: &protocol::ThreadTokenUsage) {
        let breakdown = |value: &protocol::TokenUsageBreakdown| Cumulative {
            input: value.input_tokens,
            cached: value.cached_input_tokens,
            cache_creation: value.cache_write_input_tokens,
            output: value.output_tokens,
            reasoning: value.reasoning_output_tokens,
        };
        let current = breakdown(&usage.total);
        if self.active_turn_id.as_deref() != Some(turn_id) {
            self.baseline = Some(current);
            return;
        }
        let last = breakdown(&usage.last);
        let delta = match self.baseline {
            Some(previous)
                if current.input >= previous.input
                    && current.cached >= previous.cached
                    && current.output >= previous.output
                    && current.reasoning >= previous.reasoning =>
            {
                Cumulative {
                    input: current.input - previous.input,
                    cached: current.cached - previous.cached,
                    cache_creation: match (current.cache_creation, previous.cache_creation) {
                        (Some(current), Some(previous)) if current >= previous => Some(current - previous),
                        _ => None,
                    },
                    output: current.output - previous.output,
                    reasoning: current.reasoning - previous.reasoning,
                }
            }
            _ => last,
        };
        self.baseline = Some(current);
        let accumulator = self.accumulator(turn_id);
        if delta.input > 0 || delta.cached > 0 || delta.output > 0 || delta.reasoning > 0 {
            accumulator.observed = true;
        }
        accumulator.input += delta.input;
        accumulator.cached += delta.cached;
        accumulator.output += delta.output;
        accumulator.reasoning += delta.reasoning;
        accumulator.cache_creation = match (delta.cache_creation, accumulator.cache_creation) {
            (None, _) => None,
            (Some(delta), Some(total)) => Some(total + delta),
            (Some(_), None) => None,
        };
    }

    /// `completeCodexTurnTokenUsage`.
    fn complete(&mut self, turn_id: &str, completed: bool) -> Value {
        let usage = self.by_turn_id.remove(turn_id);
        if self.active_turn_id.as_deref() == Some(turn_id) {
            self.active_turn_id = None;
        }
        let Some(usage) = usage else {
            return json!({"usageStatus": "unavailable", "usageScope": "main_agent", "hasSubagents": false});
        };
        if !usage.observed {
            return json!({"usageStatus": "unavailable", "usageScope": "main_agent", "hasSubagents": usage.has_subagents});
        }
        let mut out = obj(json!({
            "usageStatus": if completed { "complete" } else { "partial" },
            "usageScope": "main_agent",
            "inputTokens": usage.input,
            "cachedInputTokens": usage.input.min(usage.cached),
        }));
        insert_some(&mut out, "cacheCreationTokens", usage.cache_creation.map(|value| json!(usage.input.min(value))));
        out.insert("outputTokens".into(), json!(usage.output));
        out.insert("reasoningTokens".into(), json!(usage.output.min(usage.reasoning)));
        out.insert("hasSubagents".into(), json!(usage.has_subagents));
        Value::Object(out)
    }
}

// ---------------------------------------------------------------------------------------------
// The per-session mapper

/// The per-session state of the adapter's event loop.
#[derive(Debug, Default)]
pub struct CodexEventMapper {
    /// Managed (ChatGPT sharing) mode: errors are classified and reworded.
    pub managed: bool,
    pub token_usage: TurnTokenUsageState,
    rate_limits: Option<CodexRateLimitSnapshot>,
}

/// What processing one native event yields.
#[derive(Debug, Default)]
pub struct Mapped {
    pub events: Vec<Value>,
    /// The managed connection was revoked (managed mode).
    pub revoke: bool,
}

impl CodexEventMapper {
    pub fn new(managed: bool) -> Self {
        Self { managed, ..Self::default() }
    }

    /// One native event: bookkeeping, then [`map_to_runtime_events`] with the turn usage and the
    /// usage-limit / managed rewording applied.
    pub fn process(&mut self, event: &ProviderEvent) -> Mapped {
        let payload = event.payload.as_ref();
        let thread_id = event.thread_id.as_str();
        let event_turn = event.turn_id.as_ref().map(|turn| turn.as_str()).filter(|turn| !turn.is_empty());
        if let (true, Some(turn)) = (event.method == "turn/started", event_turn) {
            self.token_usage.turn_started(turn);
        } else if event.method == "thread/tokenUsage/updated" {
            if let Some(updated) = read::<protocol::ThreadTokenUsageUpdatedNotification>(payload) {
                self.token_usage.accumulate(&updated.turn_id, &updated.token_usage);
            }
        } else if let Some(active) = self.token_usage.active_turn_id.clone() {
            let spawn = event.method == "collabAgent/started"
                || (event.method == "collabAgent/activity" && payload.and_then(|payload| payload.get("activityKind")) == Some(&json!("started")));
            if spawn && event_turn == Some(active.as_str()) {
                self.token_usage.accumulator(&active).has_subagents = true;
            }
        }

        if event.method == "account/rateLimits/updated" {
            if let Some(snapshot) = read::<protocol::AccountRateLimitsUpdatedNotification>(payload)
                .and_then(|_| CodexRateLimitSnapshot::from_value(&payload.expect("validated")["rateLimits"]))
            {
                self.rate_limits = merge_codex_rate_limits(self.rate_limits.take(), &snapshot);
            }
        } else if event.method == "error" {
            // The failed turn/completed repeats this sentence and is answered below.
            if read::<protocol::ErrorNotification>(payload).is_some() && payload.expect("validated")["error"]["codexErrorInfo"] == "usageLimitExceeded" {
                return Mapped::default();
            }
        }

        let managed_error: Option<ManagedErrorClass> = if self.managed { payload.and_then(classify_codex_managed_error) } else { None };
        let revoke = managed_error.as_ref().is_some_and(|error| error.revoke);
        let mut usage_limit_error = None;
        let mut usage_limit_message = None;
        if event.method == "turn/completed" {
            let turn_error = read::<protocol::TurnCompletedNotification>(payload)
                .filter(|completed| completed.turn.status == protocol::TurnStatus::Failed)
                .and(payload.map(|payload| payload["turn"]["error"].clone()))
                .filter(|error| error.is_object());
            if let Some(turn_error) = &turn_error {
                if let Some(managed) = &managed_error {
                    usage_limit_message = Some(managed.message.clone());
                    usage_limit_error = Some(make(
                        event,
                        thread_id,
                        "runtime.error",
                        obj(json!({"message": managed.message, "code": managed.code, "class": "provider_error"})),
                    ));
                } else if turn_error["codexErrorInfo"] == "usageLimitExceeded" {
                    let message = codex_usage_limit_message(self.rate_limits.as_ref(), &event.created_at);
                    let mut out = obj(json!({"message": message, "class": "provider_error"}));
                    insert_some(
                        &mut out,
                        "detail",
                        turn_error["message"].as_str().filter(|message| !message.is_empty()).map(Value::from),
                    );
                    usage_limit_message = Some(message);
                    usage_limit_error = Some(make(event, thread_id, "runtime.error", out));
                }
            }
        }

        let mut events: Vec<Value> = map_to_runtime_events(event, thread_id)
            .into_iter()
            .map(|mut runtime_event| {
                let kind = runtime_event["type"].as_str().unwrap_or_default().to_owned();
                if let Some(managed) = managed_error.as_ref().filter(|_| kind == "runtime.error") {
                    let payload = runtime_event["payload"].as_object_mut().expect("payload object");
                    payload.insert("message".into(), json!(managed.message));
                    payload.insert("detail".into(), json!(managed.message));
                    payload.insert("code".into(), json!(managed.code));
                    return runtime_event;
                }
                let turn_id = runtime_event.get("turnId").and_then(Value::as_str).map(str::to_owned);
                if let Some(turn_id) = turn_id {
                    if kind == "turn.completed" {
                        let completed = runtime_event["payload"]["state"] == "completed";
                        let usage = self.token_usage.complete(&turn_id, completed);
                        let payload = runtime_event["payload"].as_object_mut().expect("payload object");
                        if let Some(managed) = &managed_error {
                            payload.insert("errorMessage".into(), json!(managed.message));
                        } else if let Some(message) = &usage_limit_message {
                            payload.insert("errorMessage".into(), json!(message));
                        }
                        payload.insert("tokenUsage".into(), usage);
                    } else if kind == "turn.aborted" {
                        let usage = self.token_usage.complete(&turn_id, false);
                        runtime_event["payload"]["tokenUsage"] = usage;
                    }
                }
                runtime_event
            })
            .collect();
        if let Some(error) = usage_limit_error {
            events.insert(0, error);
        }
        Mapped { events, revoke }
    }
}

/// Decodes a canonical event built in the TS shape.
pub fn to_runtime_event(value: Value) -> Result<ProviderRuntimeEvent, serde_json::Error> {
    serde_json::from_value(value)
}
