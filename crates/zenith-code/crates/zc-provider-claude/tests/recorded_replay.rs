//! Gate 2: replay recorded provider logs through the mapping and compare with what the
//! TypeScript adapter emitted.
//!
//! The provider logs (`~/.zenith/code/userdata/logs/provider/events.<thread>.log[.N]`) hold, per
//! thread, the native SDK messages the TS adapter received (`NTIVE`) interleaved with the
//! canonical events it emitted (`CANON`). They are personal data, so they are never committed:
//! point `ZC_CLAUDE_RECORDED_LOGS` at a directory holding *copies* and run
//!   ZC_CLAUDE_RECORDED_LOGS=/path/to/copies cargo test -p zc-provider-claude --test recorded_replay -- --nocapture
//! (`ZC_CLAUDE_REPLAY_STRICT=1` turns mismatches into a failure). Without the variable the test
//! is a no-op.
//!
//! What the logs cannot carry is reconstructed, and what the adapter emits from API calls
//! rather than from SDK messages is driven from the CANON stream:
//! - `content_block_delta` frames are never logged: each one is synthesized from the assistant
//!   snapshot that follows it (text, thinking, tool input JSON), just before that snapshot. The
//!   CLI normalizes tool inputs in snapshots (it strips a leading `cd <cwd> &&`, fills defaults
//!   such as `replace_all: false`), so a tool's streamed input is taken from the TS item's
//!   logged `data.input` when the two differ;
//! - records the logger summarized (`{"truncated":true}`, > 64 KiB) are rebuilt where possible:
//!   a summarized tool-result `user` message followed by a summarized `item.completed` answers
//!   the oldest open call whose TS completion was summarized; other summarized messages are
//!   skipped;
//! - `turn.started` without `raw` opens a turn (`sendTurn`), a stream-failure `runtime.error`
//!   and `session.exited` replay the adapter's failure and stop paths, `session.started` starts
//!   a fresh session state;
//! - API-only events (`session.started`, `session.configured`/`session.state.changed` without a
//!   native origin, `request.*`, `user-input.*`, permission-sourced plans) and the logger's
//!   transient types are left out of the comparison on both sides;
//! - our events go through the contract types and the logger's bounding (`summarizeProviderEvent`)
//!   before comparison; event ids, timestamps and every UUID are renumbered by first appearance.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};
use zc_contracts::{ProviderRuntimeEvent, ProviderSessionStartInput};
use zc_provider_claude::mapping::{Clock, EventJson, IdSource, MapperEnv, SessionInit, SessionRecord, SessionState};

const TRANSIENT: &[&str] = &[
    "content.delta",
    "hook.progress",
    "item.updated",
    "task.progress",
    "thread.realtime.audio.delta",
    "tool.progress",
    "turn.proposed.delta",
];

struct ReplayClock(AtomicI64);

impl Clock for ReplayClock {
    fn now_ms(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct CountingIds(AtomicU64);

impl IdSource for CountingIds {
    fn next_id(&self) -> String {
        let n = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        format!("00000000-0000-4000-8000-{n:012}")
    }
}

// ── the logger's bounding (EventNdjsonLogger.ts) ────────────────────────────

const MAX_RECORD_CHARACTERS: i64 = 64 * 1024;
const MAX_RECORD_FIELDS: i64 = 1024;
const MAX_RECORD_DEPTH: usize = 16;
const SUMMARY_FIELDS: &[&str] = &[
    "provider",
    "protocol",
    "kind",
    "providerSessionId",
    "direction",
    "stage",
    "type",
    "subtype",
    "method",
    "id",
    "threadId",
    "turnId",
    "requestId",
    "session_id",
    "status",
    "is_error",
    "api_error_status",
    "terminal_reason",
    "stop_reason",
    "operation",
    "code",
    "willRetry",
    "message",
    "event",
    "payload",
    "params",
    "result",
    "thread",
    "turn",
    "error",
    "turns",
    "items",
    "content",
];

fn utf16_len(text: &str) -> i64 {
    text.encode_utf16().count() as i64
}

fn summarize(event: &Value) -> Value {
    fn walk(value: &Value, depth: usize, fields: &mut i64, chars: &mut i64) -> Option<Value> {
        match value {
            Value::String(text) => {
                let len = utf16_len(text);
                if len > (*chars).min(1024) {
                    return Some(json!({ "omittedCharacters": len }));
                }
                *chars -= len;
                Some(value.clone())
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => Some(value.clone()),
            Value::Array(items) => Some(json!({ "itemCount": items.len() })),
            Value::Object(map) => {
                let mut summary = Map::new();
                summary.insert("truncated".into(), Value::Bool(true));
                if depth >= 6 {
                    return Some(Value::Object(summary));
                }
                for key in SUMMARY_FIELDS {
                    if *fields <= 0 {
                        break;
                    }
                    let Some(nested) = map.get(*key) else { continue };
                    *fields -= 1;
                    if let Some(value) = walk(nested, depth + 1, fields, chars) {
                        summary.insert(key.to_string(), value);
                    }
                }
                Some(Value::Object(summary))
            }
        }
    }
    let (mut fields, mut chars) = (128, 8 * 1024);
    walk(event, 0, &mut fields, &mut chars).unwrap_or(Value::Null)
}

fn bound_for_logging(event: &Value) -> Value {
    fn fits(value: &Value, depth: usize, fields: &mut i64, chars: &mut i64) -> bool {
        match value {
            Value::String(text) => {
                *chars -= utf16_len(text);
                *chars >= 0
            }
            Value::Array(items) => {
                if depth > MAX_RECORD_DEPTH || items.len() as i64 > *fields {
                    return false;
                }
                for (index, item) in items.iter().enumerate() {
                    *fields -= 1;
                    *chars -= index.to_string().len() as i64;
                    if *fields < 0 || *chars < 0 || !fits(item, depth + 1, fields, chars) {
                        return false;
                    }
                }
                true
            }
            Value::Object(map) => {
                if depth > MAX_RECORD_DEPTH {
                    return false;
                }
                for (key, item) in map {
                    *fields -= 1;
                    *chars -= utf16_len(key);
                    if *fields < 0 || *chars < 0 || !fits(item, depth + 1, fields, chars) {
                        return false;
                    }
                }
                true
            }
            _ => true,
        }
    }
    let (mut fields, mut chars) = (MAX_RECORD_FIELDS, MAX_RECORD_CHARACTERS);
    if fits(event, 0, &mut fields, &mut chars) && (event.to_string().len() as i64) <= MAX_RECORD_CHARACTERS {
        return event.clone();
    }
    summarize(event)
}

// ── normalization ───────────────────────────────────────────────────────────

fn uuid_regex() -> regex::Regex {
    regex::Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}").unwrap()
}

struct Normalizer {
    regex: regex::Regex,
    ids: HashMap<String, usize>,
}

impl Normalizer {
    fn new() -> Self {
        Self {
            regex: uuid_regex(),
            ids: HashMap::new(),
        }
    }

    fn apply(&mut self, value: &Value) -> Value {
        match value {
            Value::String(text) => {
                let ids = &mut self.ids;
                Value::String(
                    self.regex
                        .replace_all(text, |captures: &regex::Captures<'_>| {
                            let next = ids.len();
                            format!("<id{}>", ids.entry(captures[0].to_lowercase()).or_insert(next))
                        })
                        .into_owned(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(|item| self.apply(item)).collect()),
            Value::Object(map) => {
                let mut out = Map::new();
                for (key, item) in map {
                    if matches!(key.as_str(), "eventId" | "createdAt" | "providerInstanceId" | "updatedAt") {
                        continue;
                    }
                    out.insert(key.clone(), self.apply(item));
                }
                Value::Object(out)
            }
            _ => value.clone(),
        }
    }
}

// ── the log ─────────────────────────────────────────────────────────────────

enum Record {
    Canon(Value),
    Native { observed_at: String, event: Value },
}

fn read_thread(files: &[std::path::PathBuf]) -> Vec<Record> {
    let mut out = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(file) else { continue };
        for line in text.lines() {
            let Some(rest) = line.strip_prefix('[') else { continue };
            let Some((observed_at, rest)) = rest.split_once("] ") else { continue };
            if let Some(json) = rest.strip_prefix("CANON: ") {
                if let Ok(value) = serde_json::from_str::<Value>(json) {
                    if value["provider"] == "claudeAgent" {
                        out.push(Record::Canon(value));
                    }
                }
            } else if let Some(json) = rest.strip_prefix("NTIVE: ") {
                if let Ok(value) = serde_json::from_str::<Value>(json) {
                    if value["event"]["provider"] == "claudeAgent" {
                        out.push(Record::Native {
                            observed_at: observed_at.to_string(),
                            event: value["event"].clone(),
                        });
                    }
                }
            }
        }
    }
    out
}

fn is_truncated(value: &Value) -> bool {
    value.get("truncated") == Some(&Value::Bool(true))
}

/// Events left out on both sides: API-driven or transient.
fn compared(event: &Value) -> bool {
    let kind = event["type"].as_str().unwrap_or_default();
    let raw_source = event.get("raw").and_then(|raw| raw.get("source")).and_then(Value::as_str);
    if TRANSIENT.contains(&kind) || kind.starts_with("request.") || kind.starts_with("user-input.") || kind == "session.started" {
        return false;
    }
    if matches!(kind, "session.configured" | "session.state.changed") && raw_source.is_none() && !is_truncated(event) {
        return false;
    }
    if kind == "session.configured" && is_truncated(event) {
        // Logged summaries keep no raw: only the native `session.configured` is compared, and
        // the TS log cannot tell which a summary was.
        return false;
    }
    if kind == "turn.proposed.completed" && raw_source == Some("claude.sdk.permission") {
        return false;
    }
    true
}

/// The bundled catalog's context window for an API model id (`slug` or `slug[1m]`), as the
/// adapter derived it from the session's model selection.
fn catalog_context_window(api_model: &str) -> Option<f64> {
    let (slug, options) = match api_model.strip_suffix("[1m]") {
        Some(slug) => (slug, json!([{"id": "contextWindow", "value": "1m"}])),
        None => (api_model, json!([])),
    };
    let selection = serde_json::from_value(json!({"instanceId": "claudeAgent", "model": slug, "options": options})).ok()?;
    zc_provider_claude::ClaudeModelCatalog::bundled().context_window_tokens(Some(&selection))
}

struct Replay {
    env: MapperEnv,
    clock: Arc<ReplayClock>,
    thread_id: String,
    state: Option<SessionState>,
    open_blocks: BTreeMap<i64, (String, bool)>,
    open_tools: Vec<(String, Option<String>)>,
    synthetic: u64,
    /// Whether the session's launch config was unreadable (a summarized `session.configured`),
    /// so the context window comes from the CLI's `init` model instead.
    window_unknown: bool,
    /// Tool inputs as the TS adapter accumulated them from the (unlogged) deltas.
    streamed_inputs: HashMap<String, Value>,
    /// Tool calls the TS adapter started but whose completion the logger summarized.
    summarized_tools: std::collections::HashSet<String>,
    out: Vec<Value>,
    decode_failures: usize,
    stats: Stats,
}

/// What the replay had to reconstruct, reported with the results.
#[derive(Default, Clone, Copy)]
struct Stats {
    native: usize,
    deltas: usize,
    inputs_from_ts: usize,
    rebuilt: usize,
    unreplayable: usize,
}

impl Replay {
    fn new(thread_id: &str) -> Self {
        let clock = Arc::new(ReplayClock(AtomicI64::new(0)));
        let env = MapperEnv {
            ids: Arc::new(CountingIds(AtomicU64::new(0))),
            clock: clock.clone(),
            scoped_limit_names: zc_provider_claude::usage_limits::make_scoped_limit_names(),
            claude_config_dir: None,
            native_sink: None,
        };
        Self {
            env,
            clock,
            thread_id: thread_id.to_string(),
            state: None,
            open_blocks: BTreeMap::new(),
            open_tools: Vec::new(),
            synthetic: 0,
            window_unknown: false,
            streamed_inputs: HashMap::new(),
            summarized_tools: std::collections::HashSet::new(),
            out: Vec::new(),
            decode_failures: 0,
            stats: Stats::default(),
        }
    }

    fn set_clock(&self, iso: &str) {
        if let Some(ms) = zc_core::time::parse_iso_millis(iso) {
            self.clock.0.store(ms, Ordering::SeqCst);
        }
    }

    fn start_session(&mut self, configured: Option<&Value>) {
        let model = configured.and_then(|c| c["payload"]["config"]["model"].as_str()).map(str::to_string);
        let cwd = configured.and_then(|c| c["payload"]["config"]["cwd"].as_str()).map(str::to_string);
        let effort = configured.and_then(|c| c["payload"]["config"]["effort"].as_str()).map(str::to_string);
        let context_window = model.as_deref().and_then(catalog_context_window);
        self.window_unknown = configured.is_none();
        let now = self.env.clock.now_iso();
        let start_input: ProviderSessionStartInput =
            serde_json::from_value(json!({"threadId": self.thread_id, "provider": "claudeAgent", "runtimeMode": "full-access"})).unwrap();
        let record = SessionRecord {
            thread_id: self.thread_id.clone(),
            provider_instance_id: "claudeAgent".into(),
            status: "ready",
            runtime_mode: start_input.runtime_mode,
            cwd,
            model: None,
            resume_cursor: None,
            active_turn_id: None,
            created_at: now.clone(),
            updated_at: now,
            last_error: None,
        };
        self.state = Some(SessionState::new(SessionInit {
            session: record,
            start_input,
            resume_state: None,
            base_permission_mode: None,
            current_api_model_id: model,
            current_effort: effort,
            resume_session_id: None,
            initial_context_window: context_window,
        }));
        self.open_blocks.clear();
        self.open_tools.clear();
    }

    fn state(&mut self) -> &mut SessionState {
        if self.state.is_none() {
            self.start_session(None);
        }
        self.state.as_mut().unwrap()
    }

    fn publish(&mut self, events: Vec<EventJson>) {
        for event in events {
            match serde_json::from_value::<ProviderRuntimeEvent>(event.clone()) {
                Ok(decoded) => {
                    let mut value = serde_json::to_value(&decoded).unwrap();
                    value.as_object_mut().unwrap().insert("providerInstanceId".into(), json!("claudeAgent"));
                    self.out.push(bound_for_logging(&value));
                }
                Err(_) => self.decode_failures += 1,
            }
        }
    }

    fn feed(&mut self, message: &Value) {
        let env = self.env.clone();
        let mut out = Vec::new();
        self.state().handle_sdk_message(&env, message, &mut out);
        self.publish(out);
    }

    fn on_canon(&mut self, event: &Value) {
        let kind = event["type"].as_str().unwrap_or_default();
        let has_raw = event.get("raw").is_some();
        let env = self.env.clone();
        match kind {
            "session.started" => self.state = None,
            "session.configured" if !has_raw && !is_truncated(event) && self.state.is_none() => self.start_session(Some(event)),
            "turn.started" if !has_raw => {
                let turn_id = event["turnId"].as_str().unwrap_or_default().to_string();
                let model = event["payload"]["model"].as_str().map(str::to_string);
                let mut out = Vec::new();
                let state = self.state();
                if model.is_some() {
                    // `sendTurn` keeps the selection's model slug on the session.
                    state.session.model = model.clone();
                }
                state.begin_turn(&env, &turn_id, model.as_deref(), &mut out);
                self.publish(out);
            }
            "runtime.error" if !has_raw && event["payload"]["message"] == "Claude runtime stream failed." => {
                let mut out = Vec::new();
                let state = self.state();
                out.push(state.runtime_error(
                    &env,
                    "Claude runtime stream failed.",
                    Some(json!({ "failureCount": 1, "failureTags": ["ProviderAdapterProcessError"] })),
                ));
                state.complete_turn(&env, "failed", Some("Claude runtime stream failed."), None, &mut out);
                self.publish(out);
            }
            // `stopSessionInternal`: a stop with a running turn shows first as that turn's
            // "Session stopped." completion (server shutdowns end there, without an exit event).
            "turn.completed" if event["payload"]["errorMessage"] == "Session stopped." => self.stop(false, &Value::Null),
            "session.exited" => self.stop(true, &event["payload"]),
            _ => {}
        }
    }

    fn stop(&mut self, emit_exit: bool, exit_payload: &Value) {
        let env = self.env.clone();
        let mut out = Vec::new();
        let state = self.state();
        if !state.stopped {
            state.stopped = true;
            state.settle_live_tasks(&env, &mut out);
            if state.turn_state.is_some() {
                state.complete_turn(&env, "interrupted", Some("Session stopped."), None, &mut out);
            }
        }
        if emit_exit {
            out.push(state.session_event(&env, "session.exited", exit_payload.clone()));
        }
        self.publish(out);
    }

    fn synthetic_uuid(&mut self) -> String {
        self.synthetic += 1;
        format!("ffffffff-0000-4000-8000-{:012}", self.synthetic)
    }

    /// `answered_truncated`: the TS adapter's next events include a summarized `item.completed`,
    /// i.e. this summarized message did complete a tool call.
    fn on_native(&mut self, observed_at: &str, event: &Value, answered_truncated: bool) {
        self.set_clock(event["createdAt"].as_str().unwrap_or(observed_at));
        let mut payload = event["payload"].clone();
        if is_truncated(event) || is_truncated(&payload) {
            match self.rebuild_truncated(&payload).filter(|_| answered_truncated) {
                Some(rebuilt) => {
                    self.stats.rebuilt += 1;
                    payload = rebuilt;
                }
                None => {
                    self.stats.unreplayable += 1;
                    return;
                }
            }
        }
        if self.state.as_ref().is_some_and(|s| s.stopped) {
            return;
        }
        match payload["type"].as_str() {
            Some("stream_event") => {
                let stream = &payload["event"];
                let index = stream["index"].as_i64().unwrap_or(-1);
                match stream["type"].as_str() {
                    Some("content_block_start") => {
                        let kind = stream["content_block"]["type"].as_str().unwrap_or_default().to_string();
                        self.open_blocks.insert(index, (kind, false));
                    }
                    Some("content_block_stop") => {
                        self.open_blocks.remove(&index);
                    }
                    _ => {}
                }
            }
            Some("assistant") => self.synthesize_deltas(&payload),
            Some("system") if payload["subtype"] == "init" && self.window_unknown => {
                if let Some(model) = payload["model"].as_str() {
                    self.window_unknown = false;
                    let window = catalog_context_window(model);
                    let state = self.state();
                    if state.last_known_context_window.is_none() {
                        state.last_known_context_window = window;
                    }
                }
            }
            _ => {}
        }
        self.track_tools(&payload);
        self.stats.native += 1;
        self.feed(&payload);
    }

    /// The deltas the logger dropped, rebuilt from the snapshot of the block they streamed.
    fn synthesize_deltas(&mut self, assistant: &Value) {
        if !assistant["parent_tool_use_id"].is_null() {
            return;
        }
        let Some(content) = assistant["message"]["content"].as_array() else { return };
        for block in content {
            let kind = block["type"].as_str().unwrap_or_default();
            let Some((&index, _)) = self.open_blocks.iter().rev().find(|(_, (open_kind, done))| open_kind == kind && !done) else {
                continue;
            };
            let delta = match kind {
                "text" => json!({"type": "text_delta", "text": block["text"]}),
                "thinking" => json!({"type": "thinking_delta", "thinking": block["thinking"]}),
                "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                    // The CLI normalizes snapshot inputs, so the input the deltas carried is taken
                    // from the TS item when it was logged.
                    let streamed = block["id"]
                        .as_str()
                        .and_then(|id| self.streamed_inputs.get(id))
                        .filter(|input| **input != block["input"])
                        .cloned();
                    self.stats.inputs_from_ts += usize::from(streamed.is_some());
                    let input = streamed.unwrap_or_else(|| block["input"].clone());
                    json!({"type": "input_json_delta", "partial_json": input.to_string()})
                }
                _ => continue,
            };
            if let Some(entry) = self.open_blocks.get_mut(&index) {
                entry.1 = true;
            }
            if delta.get("text").is_some_and(|t| t.as_str().is_none_or(str::is_empty))
                || delta.get("thinking").is_some_and(|t| t.as_str().is_none_or(str::is_empty))
            {
                continue;
            }
            self.stats.deltas += 1;
            let uuid = self.synthetic_uuid();
            let message = json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": index, "delta": delta}, "session_id": assistant["session_id"], "parent_tool_use_id": null, "uuid": uuid});
            self.feed(&message);
        }
    }

    fn track_tools(&mut self, payload: &Value) {
        let parent = payload["parent_tool_use_id"].as_str().map(str::to_string);
        match payload["type"].as_str() {
            Some("assistant") => {
                for block in payload["message"]["content"].as_array().into_iter().flatten() {
                    if let Some(id) = block["id"].as_str().filter(|_| block["type"].as_str().is_some_and(|t| t.ends_with("tool_use"))) {
                        if !self.open_tools.iter().any(|(open, _)| open == id) {
                            self.open_tools.push((id.to_string(), parent.clone()));
                        }
                    }
                }
            }
            Some("user") => {
                for block in payload["message"]["content"].as_array().into_iter().flatten() {
                    if let Some(id) = block["tool_use_id"].as_str() {
                        self.open_tools.retain(|(open, _)| open != id);
                    }
                }
            }
            _ => {}
        }
    }

    /// A summarized tool-result `user` message: one oversized result for the oldest open call.
    fn rebuild_truncated(&mut self, payload: &Value) -> Option<Value> {
        if payload["type"] != "user" {
            return None;
        }
        let count = payload["message"]["content"]["itemCount"].as_u64().unwrap_or(1) as usize;
        // Only calls whose TS completion was itself summarized can be the ones answered here.
        let calls: Vec<(String, Option<String>)> = self
            .open_tools
            .iter()
            .filter(|(id, _)| self.summarized_tools.contains(id))
            .take(count)
            .cloned()
            .collect();
        for (id, _) in &calls {
            self.summarized_tools.remove(id);
        }
        if calls.is_empty() {
            return None;
        }
        let parent = calls[0].1.clone();
        let content: Vec<Value> = calls
            .iter()
            .filter(|(_, p)| *p == parent)
            .map(|(id, _)| json!({"type": "tool_result", "tool_use_id": id, "content": "x".repeat(70_000)}))
            .collect();
        let uuid = self.synthetic_uuid();
        Some(
            json!({"type": "user", "session_id": payload["session_id"], "parent_tool_use_id": parent, "uuid": uuid, "message": {"role": "user", "content": content}}),
        )
    }
}

struct ThreadResult {
    thread: String,
    expected: usize,
    actual: usize,
    matched_prefix: usize,
    decode_failures: usize,
    first_diff: Option<(usize, Value, Value)>,
    stats: Stats,
}

fn replay_thread(thread: &str, files: &[std::path::PathBuf]) -> Option<ThreadResult> {
    let records = read_thread(files);
    if !records.iter().any(|r| matches!(r, Record::Native { .. })) {
        return None;
    }
    let mut replay = Replay::new(thread);
    let mut completed = std::collections::HashSet::new();
    for record in &records {
        if let Record::Canon(event) = record {
            if let Some(id) = event["itemId"].as_str() {
                match event["type"].as_str() {
                    Some("item.started") => {
                        replay.summarized_tools.insert(id.to_string());
                    }
                    Some("item.completed") => {
                        completed.insert(id.to_string());
                    }
                    _ => {}
                }
            }
            let input = &event["payload"]["data"]["input"];
            if event["type"] == "item.completed" && input.is_object() {
                if let Some(id) = event["itemId"].as_str() {
                    replay.streamed_inputs.insert(id.to_string(), input.clone());
                }
            }
        }
    }
    let mut expected = Vec::new();
    replay.summarized_tools.retain(|id| !completed.contains(id));
    for (index, record) in records.iter().enumerate() {
        match record {
            Record::Canon(event) => {
                if let Some(at) = event["createdAt"].as_str() {
                    replay.set_clock(at);
                }
                replay.on_canon(event);
                expected.push(event.clone());
            }
            Record::Native { observed_at, event } => {
                let answered_truncated = records[index + 1..]
                    .iter()
                    // The two log streams are batched separately: look past status frames, up to
                    // the next conversation message.
                    .take_while(|next| match next {
                        Record::Canon(_) => true,
                        Record::Native { event, .. } => {
                            let method = event["method"].as_str().unwrap_or_default();
                            !(method == "claude/assistant" || method.starts_with("claude/result") || method.starts_with("claude/stream_event/message_start"))
                        }
                    })
                    .any(|next| matches!(next, Record::Canon(e) if e["type"] == "item.completed" && (is_truncated(e) || is_truncated(&e["payload"]))));
                replay.on_native(observed_at, event, answered_truncated)
            }
        }
    }
    let mut left = Normalizer::new();
    let mut right = Normalizer::new();
    let expected: Vec<Value> = expected.iter().filter(|e| compared(e)).map(|e| left.apply(e)).collect();
    let actual: Vec<Value> = replay.out.iter().filter(|e| compared(e)).map(|e| right.apply(e)).collect();
    let matched_prefix = expected.iter().zip(actual.iter()).take_while(|(a, b)| a == b).count();
    if matched_prefix < expected.len().max(actual.len()) && std::env::var("ZC_CLAUDE_REPLAY_CONTEXT").is_ok() {
        let window = matched_prefix.saturating_sub(4)..(matched_prefix + 6);
        let line = |e: &Value| format!("{} item={} {}", e["type"], e["itemId"], short(&e["payload"], 160));
        for i in window {
            eprintln!(
                "  #{i}\n    ts:   {}\n    rust: {}",
                expected.get(i).map(line).unwrap_or_default(),
                actual.get(i).map(line).unwrap_or_default()
            );
        }
    }
    let first_diff = (matched_prefix < expected.len().max(actual.len())).then(|| {
        (
            matched_prefix,
            expected.get(matched_prefix).cloned().unwrap_or(Value::Null),
            actual.get(matched_prefix).cloned().unwrap_or(Value::Null),
        )
    });
    Some(ThreadResult {
        thread: thread.to_string(),
        expected: expected.len(),
        actual: actual.len(),
        matched_prefix,
        decode_failures: replay.decode_failures,
        first_diff,
        stats: replay.stats,
    })
}

/// The paths where two events differ, with both values (shortened).
fn diff_paths(path: &str, left: &Value, right: &Value, out: &mut Vec<String>) {
    if out.len() >= 8 || left == right {
        return;
    }
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                diff_paths(
                    &format!("{path}.{key}"),
                    a.get(key).unwrap_or(&Value::Null),
                    b.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (index, (x, y)) in a.iter().zip(b).enumerate() {
                diff_paths(&format!("{path}[{index}]"), x, y, out);
            }
        }
        _ => out.push(format!("{path}: ts={} rust={}", short(left, 200), short(right, 200))),
    }
}

fn short(value: &Value, max: usize) -> String {
    let text = value.to_string();
    if text.chars().count() > max {
        format!("{}…", &text[..text.char_indices().nth(max).map(|(i, _)| i).unwrap_or(text.len())])
    } else {
        text
    }
}

#[test]
fn replays_recorded_claude_threads() {
    let Ok(dir) = std::env::var("ZC_CLAUDE_RECORDED_LOGS") else {
        eprintln!("ZC_CLAUDE_RECORDED_LOGS is not set; skipping the recorded-log replay");
        return;
    };
    let mut groups: BTreeMap<String, Vec<(u32, std::path::PathBuf)>> = BTreeMap::new();
    for entry in std::fs::read_dir(Path::new(&dir)).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix("events.") else { continue };
        let Some((thread, suffix)) = rest.split_once(".log") else { continue };
        let rotation: u32 = suffix.strip_prefix('.').and_then(|n| n.parse().ok()).unwrap_or(0);
        groups.entry(thread.to_string()).or_default().push((rotation, entry.path()));
    }
    // Older rotations (`.log.2`, `.log.1`) come before the live file.
    let groups: BTreeMap<String, Vec<std::path::PathBuf>> = groups
        .into_iter()
        .map(|(thread, mut files)| {
            files.sort_by_key(|(rotation, _)| std::cmp::Reverse(*rotation));
            (thread, files.into_iter().map(|(_, path)| path).collect())
        })
        .collect();
    let only = std::env::var("ZC_CLAUDE_REPLAY_THREAD").ok();
    let mut results = Vec::new();
    for (thread, files) in &groups {
        if only.as_deref().is_some_and(|t| t != thread) {
            continue;
        }
        if let Some(result) = replay_thread(thread, files) {
            results.push(result);
        }
    }
    let (mut matched_threads, mut events, mut matched_events) = (0, 0, 0);
    let mut totals = Stats::default();
    for result in &results {
        totals.native += result.stats.native;
        totals.deltas += result.stats.deltas;
        totals.inputs_from_ts += result.stats.inputs_from_ts;
        totals.rebuilt += result.stats.rebuilt;
        totals.unreplayable += result.stats.unreplayable;
        events += result.expected;
        matched_events += result.matched_prefix;
        let ok = result.first_diff.is_none();
        matched_threads += usize::from(ok);
        eprintln!(
            "{} {}: {}/{} events match (ours {}), decode failures {}",
            if ok { "MATCH" } else { "DIFF " },
            result.thread,
            result.matched_prefix,
            result.expected,
            result.actual,
            result.decode_failures
        );
        if let Some((index, expected, actual)) = &result.first_diff {
            let mut paths = Vec::new();
            diff_paths("", expected, actual, &mut paths);
            eprintln!("  first difference at #{index} ({} vs {}):", expected["type"], actual["type"]);
            for path in paths {
                eprintln!("    {path}");
            }
        }
    }
    eprintln!(
        "recorded replay: {matched_threads}/{} Claude threads identical; {matched_events}/{events} compared events in matching prefixes",
        results.len()
    );
    eprintln!(
        "  {} native messages replayed; reconstructed: {} stream deltas, {} tool inputs taken from the TS item (snapshot rewritten by the CLI), {} summarized tool results rebuilt, {} summarized messages skipped",
        totals.native, totals.deltas, totals.inputs_from_ts, totals.rebuilt, totals.unreplayable
    );
    if std::env::var("ZC_CLAUDE_REPLAY_STRICT").is_ok() {
        assert_eq!(matched_threads, results.len());
    }
}
