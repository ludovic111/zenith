//! `claudeHistoryWorker.ts` without a worker: the SDK's `getSessionMessages` and `forkSession`
//! as direct operations on `$CLAUDE_CONFIG_DIR/projects/<encoded cwd>/<sessionId>.jsonl`
//! (`sdk.mjs` 0.3.276: `WZ`/`GZ`/`fI`/`LGe`/`UGe`/`mI`, `b9`/`x9`/`OWe`).
//!
//! The rewind in the adapter only reads `type`, `uuid`, `parent_tool_use_id` and `message` of
//! the returned messages and re-checks a fork against the original before trusting it, so the
//! port aims at the same conversation chain the SDK rebuilds (compaction relinks, sibling
//! assistant chunks, queued steering messages), not at every field of its output.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::mapping::is_uuid;

/// `S4`: transcripts larger than this are read from their last compaction boundary.
const PRECOMPACT_SKIP_BYTES: u64 = 5 * 1024 * 1024;
const MAX_ENCODED_DIR: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    #[error("Invalid sessionId: {0}")]
    InvalidSessionId(String),
    #[error("{0}")]
    NotFound(String),
    #[error("Session {0} has no messages to fork")]
    NothingToFork(String),
    #[error("Message {message} not found in session {session}")]
    MessageNotFound { message: String, session: String },
    #[error("{0}")]
    Io(String),
}

/// `Tu`: every non-alphanumeric UTF-16 code unit becomes `-`.
fn sanitize_path(value: &str) -> String {
    value
        .encode_utf16()
        .map(|unit| {
            if unit < 128 && (unit as u8).is_ascii_alphanumeric() {
                unit as u8 as char
            } else {
                '-'
            }
        })
        .collect()
}

/// `vC`: the Java-style 32-bit string hash over UTF-16 code units.
fn string_hash(value: &str) -> i32 {
    value
        .encode_utf16()
        .fold(0i32, |hash, unit| hash.wrapping_shl(5).wrapping_sub(hash).wrapping_add(unit as i32))
}

fn to_base36(mut value: u64) -> String {
    if value == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(std::char::from_digit((value % 36) as u32, 36).unwrap_or('0'));
        value /= 36;
    }
    digits.iter().rev().collect()
}

/// `fm`: the project directory name for a cwd.
pub fn encode_project_dir(cwd: &str) -> String {
    let sanitized = sanitize_path(cwd);
    if sanitized.len() <= MAX_ENCODED_DIR {
        return sanitized;
    }
    format!("{}-{}", &sanitized[..MAX_ENCODED_DIR], to_base36(i64::from(string_hash(cwd)).unsigned_abs()))
}

/// `Ds`: the real path (NFC on macOS), else the path as given.
fn real_dir(dir: &str) -> String {
    std::fs::canonicalize(dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| dir.to_string())
}

fn projects_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("projects")
}

/// `Pi` / `vWe`: the transcript file of a session, looked up under the cwd's project dir, then
/// (long cwds) the hashed-name siblings, then every project dir.
pub fn find_session_file(config_dir: &Path, session_id: &str, dir: Option<&str>) -> Option<PathBuf> {
    let projects = projects_dir(config_dir);
    let file_name = format!("{session_id}.jsonl");
    let usable = |path: &Path| std::fs::metadata(path).map(|m| m.is_file() && m.len() > 0).unwrap_or(false);
    if let Some(dir) = dir {
        let real = real_dir(dir);
        let mut candidates = vec![projects.join(encode_project_dir(&real))];
        if real != dir {
            candidates.push(projects.join(encode_project_dir(dir)));
        }
        let sanitized = sanitize_path(&real);
        if sanitized.len() > MAX_ENCODED_DIR {
            let prefix = format!("{}-", &sanitized[..MAX_ENCODED_DIR]);
            if let Ok(entries) = std::fs::read_dir(&projects) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy().starts_with(&prefix) {
                        candidates.push(entry.path());
                    }
                }
            }
        }
        for candidate in candidates {
            let path = candidate.join(&file_name);
            if usable(&path) {
                return Some(path);
            }
        }
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&projects).ok()?.flatten().map(|entry| entry.path()).collect();
    dirs.sort();
    dirs.into_iter().map(|d| d.join(&file_name)).find(|path| usable(path))
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key) == Some(&Value::Bool(true))
}

fn truthy(value: &Value, key: &str) -> bool {
    crate::js::truthy(value.get(key))
}

/// The transcript text to parse: from the last plain compaction boundary for big files.
fn read_transcript(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let size = text.len() as u64;
    if size <= PRECOMPACT_SKIP_BYTES || std::env::var_os("CLAUDE_CODE_DISABLE_PRECOMPACT_SKIP").is_some() {
        return Some(text);
    }
    let mut boundary_start = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        if line.contains("\"compact_boundary\"") {
            if let Ok(entry) = serde_json::from_str::<Value>(line.trim()) {
                if str_of(&entry, "type") == Some("system") && str_of(&entry, "subtype") == Some("compact_boundary") {
                    let metadata = entry.get("compactMetadata");
                    let preserved = metadata.is_some_and(|m| truthy(m, "preservedSegment") || truthy(m, "preservedMessages"));
                    boundary_start = if preserved { None } else { Some(offset) };
                }
            }
        }
        offset += line.len();
    }
    Some(match boundary_start {
        Some(start) => text[start..].to_string(),
        None => text,
    })
}

/// `DGe`: transcript entries (`user`, `assistant`, `progress`, `system`, `attachment` with a uuid).
fn parse_entries(text: &str) -> Vec<Value> {
    text.split('\n')
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| {
            matches!(str_of(entry, "type"), Some("user" | "assistant" | "progress" | "system" | "attachment"))
                && entry.get("uuid").is_some_and(Value::is_string)
        })
        .collect()
}

fn uuid_of(entry: &Value) -> String {
    str_of(entry, "uuid").unwrap_or_default().to_string()
}

fn parent_of(entry: &Value) -> Option<String> {
    str_of(entry, "parentUuid").filter(|p| !p.is_empty()).map(str::to_string)
}

fn set_parent(entry: &Value, parent: &str) -> Value {
    let mut next = entry.clone();
    if let Some(object) = next.as_object_mut() {
        object.insert("parentUuid".into(), Value::String(parent.to_string()));
    }
    next
}

/// An insertion-ordered uuid → entry map (a JS `Map`).
#[derive(Default)]
struct EntryMap {
    order: Vec<String>,
    entries: HashMap<String, Value>,
}

impl EntryMap {
    fn from(entries: &[Value]) -> Self {
        let mut map = Self::default();
        for entry in entries {
            map.set(uuid_of(entry), entry.clone());
        }
        map
    }

    fn set(&mut self, uuid: String, entry: Value) {
        if !self.entries.contains_key(&uuid) {
            self.order.push(uuid.clone());
        }
        self.entries.insert(uuid, entry);
    }

    fn get(&self, uuid: &str) -> Option<&Value> {
        self.entries.get(uuid)
    }

    fn values(&self) -> Vec<Value> {
        self.order.iter().filter_map(|uuid| self.entries.get(uuid).cloned()).collect()
    }
}

/// `pI`: an assistant entry's API message id.
fn api_message_id(entry: &Value) -> Option<String> {
    (str_of(entry, "type") == Some("assistant"))
        .then(|| entry.get("message").and_then(|m| str_of(m, "id")).map(str::to_string))
        .flatten()
}

/// `BZ`: a user tool-result entry.
fn is_tool_result_entry(entry: &Value) -> bool {
    str_of(entry, "type") == Some("user")
        && parent_of(entry).is_some()
        && entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .is_some_and(|content| content.iter().any(|block| str_of(block, "type") == Some("tool_result")))
}

/// `fI`: relink compaction-preserved segments, pick the latest leaf, walk its chain, then
/// re-attach sibling assistant chunks and their tool results (`LGe`).
fn build_chain(entries: &[Value]) -> Vec<Value> {
    let mut map = EntryMap::from(entries);
    for entry in map.values() {
        if str_of(&entry, "type") != Some("system") || str_of(&entry, "subtype") != Some("compact_boundary") {
            continue;
        }
        let metadata = entry.get("compactMetadata").cloned().unwrap_or(Value::Null);
        if let Some(preserved) = metadata.get("preservedMessages").filter(|p| p.is_object()) {
            let uuids: Vec<String> = preserved
                .get("uuids")
                .and_then(Value::as_array)
                .map(|u| u.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default();
            if uuids.is_empty() || uuids.iter().any(|uuid| map.get(uuid).is_none()) {
                continue;
            }
            let anchor = str_of(preserved, "anchorUuid").unwrap_or_default().to_string();
            let mut previous = anchor.clone();
            for uuid in &uuids {
                let next = set_parent(map.get(uuid).expect("checked"), &previous);
                map.set(uuid.clone(), next);
                previous = uuid.clone();
            }
            let (first, last) = (uuids[0].clone(), uuids[uuids.len() - 1].clone());
            for uuid in map.order.clone() {
                let current = map.get(&uuid).cloned().expect("present");
                if parent_of(&current).as_deref() == Some(anchor.as_str()) && uuid != first {
                    map.set(uuid, set_parent(&current, &last));
                }
            }
        } else if let Some(segment) = metadata.get("preservedSegment").filter(|p| p.is_object()) {
            let head = str_of(segment, "headUuid").unwrap_or_default().to_string();
            let anchor = str_of(segment, "anchorUuid").unwrap_or_default().to_string();
            let tail = str_of(segment, "tailUuid").unwrap_or_default().to_string();
            if let Some(head_entry) = map.get(&head).cloned() {
                map.set(head.clone(), set_parent(&head_entry, &anchor));
            }
            for uuid in map.order.clone() {
                let current = map.get(&uuid).cloned().expect("present");
                if parent_of(&current).as_deref() == Some(anchor.as_str()) && uuid != head {
                    map.set(uuid, set_parent(&current, &tail));
                }
            }
        }
    }
    let mut file_index: HashMap<String, usize> = HashMap::new();
    for (index, entry) in entries.iter().enumerate() {
        file_index.insert(uuid_of(entry), index);
    }
    let parents: HashSet<String> = map.values().iter().filter_map(parent_of).collect();
    let mut leaves = Vec::new();
    for leaf in map.values().into_iter().filter(|entry| !parents.contains(&uuid_of(entry))) {
        let mut current = Some(leaf);
        let mut seen = HashSet::new();
        while let Some(entry) = current {
            if !seen.insert(uuid_of(&entry)) {
                break;
            }
            if matches!(str_of(&entry, "type"), Some("user" | "assistant")) {
                leaves.push(entry);
                break;
            }
            current = parent_of(&entry).and_then(|parent| map.get(&parent).cloned());
        }
    }
    if leaves.is_empty() {
        return Vec::new();
    }
    let latest = |candidates: &[&Value]| -> Option<Value> {
        candidates
            .iter()
            .copied()
            .reduce(|best, next| {
                if file_index.get(&uuid_of(next)).copied().map_or(-1, |i| i as i64) > file_index.get(&uuid_of(best)).copied().map_or(-1, |i| i as i64) {
                    next
                } else {
                    best
                }
            })
            .cloned()
    };
    let main: Vec<&Value> = leaves
        .iter()
        .filter(|l| !flag(l, "isSidechain") && !truthy(l, "teamName") && !flag(l, "isMeta"))
        .collect();
    let all: Vec<&Value> = leaves.iter().collect();
    let Some(leaf) = latest(if main.is_empty() { &all } else { &main }) else {
        return Vec::new();
    };
    let mut chain = Vec::new();
    let mut in_chain = HashSet::new();
    let mut current = map.get(&uuid_of(&leaf)).cloned();
    while let Some(entry) = current {
        if !in_chain.insert(uuid_of(&entry)) {
            break;
        }
        current = parent_of(&entry).and_then(|parent| map.get(&parent).cloned());
        chain.push(entry);
    }
    chain.reverse();
    attach_siblings(&map, chain, &mut in_chain)
}

/// `LGe`.
fn attach_siblings(map: &EntryMap, chain: Vec<Value>, in_chain: &mut HashSet<String>) -> Vec<Value> {
    let assistants: Vec<&Value> = chain.iter().filter(|e| str_of(e, "type") == Some("assistant")).collect();
    if assistants.is_empty() {
        return chain;
    }
    let mut last_by_id: HashMap<String, String> = HashMap::new();
    for assistant in &assistants {
        if let Some(id) = api_message_id(assistant) {
            last_by_id.insert(id, uuid_of(assistant));
        }
    }
    let mut by_id: HashMap<String, Vec<Value>> = HashMap::new();
    let mut results_by_parent: HashMap<String, Vec<Value>> = HashMap::new();
    for entry in map.values() {
        if let Some(id) = api_message_id(&entry) {
            by_id.entry(id).or_default().push(entry);
        } else if is_tool_result_entry(&entry) {
            results_by_parent.entry(parent_of(&entry).unwrap_or_default()).or_default().push(entry);
        }
    }
    let mut done = HashSet::new();
    let mut extras: HashMap<String, Vec<Value>> = HashMap::new();
    for assistant in assistants {
        let Some(id) = api_message_id(assistant) else { continue };
        if !done.insert(id.clone()) {
            continue;
        }
        let group = by_id.get(&id).cloned().unwrap_or_else(|| vec![assistant.clone()]);
        let mut chunks: Vec<Value> = group.iter().filter(|e| !in_chain.contains(&uuid_of(e))).cloned().collect();
        let mut results = Vec::new();
        for chunk in &group {
            for result in results_by_parent.get(&uuid_of(chunk)).into_iter().flatten() {
                if !in_chain.contains(&uuid_of(result)) {
                    results.push(result.clone());
                }
            }
        }
        if chunks.is_empty() && results.is_empty() {
            continue;
        }
        let by_time = |a: &Value, b: &Value| str_of(a, "timestamp").unwrap_or("").cmp(str_of(b, "timestamp").unwrap_or(""));
        chunks.sort_by(by_time);
        results.sort_by(by_time);
        let mut added = chunks;
        added.extend(results);
        for entry in &added {
            in_chain.insert(uuid_of(entry));
        }
        if let Some(anchor) = last_by_id.get(&id) {
            extras.insert(anchor.clone(), added);
        }
    }
    if extras.is_empty() {
        return chain;
    }
    let mut out = Vec::new();
    for entry in chain {
        let uuid = uuid_of(&entry);
        out.push(entry);
        if let Some(added) = extras.remove(&uuid) {
            out.extend(added);
        }
    }
    out
}

/// `dZ`: the local-command role of a user entry's text.
fn local_command_role(entry: &Value) -> Option<&'static str> {
    let content = entry.get("message").and_then(|m| m.get("content"));
    let text = match content {
        Some(Value::Array(blocks)) => blocks.iter().rev().find(|b| str_of(b, "type") == Some("text")).and_then(|b| str_of(b, "text")),
        Some(Value::String(s)) => Some(s.as_str()),
        _ => None,
    }?;
    [
        ("<command-name>", "record"),
        ("<local-command-stdout>", "output"),
        ("<local-command-stderr>", "output"),
        ("<local-command-caveat>", "caveat"),
    ]
    .iter()
    .find(|(prefix, _)| text.starts_with(prefix))
    .map(|(_, role)| *role)
}

const INTERRUPTION_PREFIXES: [&str; 5] = [
    "[Request interrupted by user]",
    "[Request interrupted by user for tool use]",
    "[Tool call did not complete: the turn was ended to deliver the message that follows. Nothing refused it; re-run it if still needed.]",
    "The user doesn't want to take this action right now. STOP what you are doing and wait for the user to tell you how to proceed.",
    "[Tool call skipped: the turn ended to deliver the message that follows before this call ran. Nothing refused it; re-run it if still needed.]",
];

/// `A4`: a synthetic interruption notice.
fn is_interruption_notice(entry: &Value) -> bool {
    if str_of(entry, "type") != Some("user") {
        return false;
    }
    match entry.get("message").and_then(|m| m.get("content")) {
        Some(Value::String(s)) => INTERRUPTION_PREFIXES.iter().any(|p| s.starts_with(p)),
        Some(Value::Array(blocks)) => {
            !blocks.is_empty()
                && blocks.iter().all(|block| {
                    let text = match str_of(block, "type") {
                        Some("text") => str_of(block, "text"),
                        Some("tool_result") if flag(block, "is_error") => str_of(block, "content"),
                        _ => None,
                    };
                    text.is_some_and(|t| INTERRUPTION_PREFIXES.iter().any(|p| t.starts_with(p)))
                })
        }
        _ => false,
    }
}

/// `UGe` with `keepMeta: false`: mark completed local commands and turn answered queued
/// commands (mid-turn steering) into user entries.
fn rewrite_queued_commands(entries: Vec<Value>) -> Vec<Value> {
    let role = |entry: &Value| {
        if entry.get("promptSource").is_none() {
            local_command_role(entry)
        } else {
            None
        }
    };
    let mut completed = HashSet::new();
    for (index, entry) in entries.iter().enumerate() {
        if str_of(entry, "type") != Some("user") || !flag(entry, "isMeta") || role(entry) != Some("caveat") {
            continue;
        }
        let mut recorded = false;
        for (next_index, next) in entries.iter().enumerate().skip(index + 1) {
            if str_of(next, "type") == Some("assistant") {
                break;
            }
            if str_of(next, "type") != Some("user") || flag(next, "isMeta") {
                continue;
            }
            let next_role = role(next);
            if next_role == Some("record") && !recorded {
                recorded = true;
            } else if next_role != Some("output") || !recorded {
                break;
            }
            completed.insert(next_index);
        }
    }
    let mut followed_by_reply = vec![false; entries.len()];
    let mut next_role: Option<&str> = None;
    for index in (0..entries.len()).rev() {
        let entry = &entries[index];
        followed_by_reply[index] = next_role == Some("reply");
        if str_of(entry, "type") == Some("assistant") || is_tool_result_entry(entry) || is_interruption_notice(entry) {
            next_role = Some("reply");
        } else if str_of(entry, "type") == Some("user") && !flag(entry, "isMeta") && !flag(entry, "isCompactSummary") && !completed.contains(&index) {
            next_role = Some("prompt");
        }
    }
    let mut uuids: HashSet<String> = entries.iter().map(uuid_of).collect();
    entries
        .into_iter()
        .enumerate()
        .map(|(index, entry)| {
            if completed.contains(&index) {
                let mut next = entry;
                if let Some(object) = next.as_object_mut() {
                    object.insert("isCompletedLocalCommand".into(), Value::Bool(true));
                }
                return next;
            }
            if !followed_by_reply[index] || str_of(&entry, "type") != Some("attachment") {
                return entry;
            }
            let Some(attachment) = entry.get("attachment").filter(|a| a.is_object()) else {
                return entry;
            };
            if str_of(attachment, "type") != Some("queued_command") || flag(attachment, "isMeta") || truthy(attachment, "isMeta") {
                return entry;
            }
            let Some(prompt) = attachment.get("prompt").filter(|p| p.is_string() || p.is_array()) else {
                return entry;
            };
            if attachment
                .get("forwardedIntent")
                .and_then(|f| f.get("lineage"))
                .and_then(Value::as_str)
                .is_some_and(|l| !l.is_empty())
            {
                return entry;
            }
            let uuid = str_of(attachment, "source_uuid")
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| uuid_of(&entry));
            if uuid != uuid_of(&entry) && uuids.contains(&uuid) {
                return entry;
            }
            uuids.insert(uuid.clone());
            let mut user = Map::new();
            user.insert("type".into(), Value::String("user".into()));
            user.insert("uuid".into(), Value::String(uuid));
            user.insert("parentUuid".into(), entry.get("parentUuid").cloned().unwrap_or(Value::Null));
            user.insert("sessionId".into(), entry.get("sessionId").cloned().unwrap_or(Value::Null));
            if let Some(timestamp) = entry.get("timestamp") {
                user.insert("timestamp".into(), timestamp.clone());
            }
            user.insert("message".into(), json!({ "role": "user", "content": prompt }));
            user.insert("isMeta".into(), Value::Bool(false));
            if let Some(origin) = queued_command_origin(attachment) {
                user.insert("origin".into(), origin);
            }
            user.insert("isQueuedCommand".into(), Value::Bool(true));
            if let Some(sidechain) = entry.get("isSidechain") {
                user.insert("isSidechain".into(), sidechain.clone());
            }
            if let Some(team) = entry.get("teamName") {
                user.insert("teamName".into(), team.clone());
            }
            Value::Object(user)
        })
        .collect()
}

/// `mI`: the SDK's `SessionMessage`.
fn to_session_message(entry: &Value) -> Value {
    let mut message = Map::new();
    message.insert("type".into(), entry.get("type").cloned().unwrap_or(Value::Null));
    message.insert("uuid".into(), entry.get("uuid").cloned().unwrap_or(Value::Null));
    message.insert("session_id".into(), entry.get("sessionId").cloned().unwrap_or(Value::Null));
    if let Some(body) = entry.get("message") {
        message.insert("message".into(), body.clone());
    }
    message.insert("parent_tool_use_id".into(), Value::Null);
    message.insert("parent_agent_id".into(), Value::Null);
    if flag(entry, "interruptedByShutdown") {
        message.insert("interruptedByShutdown".into(), Value::Bool(true));
    }
    if flag(entry, "isCompactSummary") {
        message.insert("isCompactSummary".into(), Value::Bool(true));
    }
    if flag(entry, "isMeta") || flag(entry, "isCompactSummary") || flag(entry, "isVisibleInTranscriptOnly") {
        message.insert("is_meta".into(), Value::Bool(true));
    }
    if flag(entry, "isQueuedCommand") {
        message.insert("isQueuedCommand".into(), Value::Bool(true));
    }
    if flag(entry, "isCompletedLocalCommand") {
        message.insert("isCompletedLocalCommand".into(), Value::Bool(true));
    }
    if let Some(timestamp) = entry.get("timestamp") {
        message.insert("timestamp".into(), timestamp.clone());
    }
    if let Some(origin) = entry.get("origin").filter(|o| !o.is_null()).map(normalize_origin) {
        message.insert("origin".into(), origin);
    }
    Value::Object(message)
}

/// `kk`: a task-notification origin keeps only `kind`, `subkind` and `fireReason`.
fn normalize_origin(origin: &Value) -> Value {
    if str_of(origin, "kind") != Some("task-notification") {
        return origin.clone();
    }
    let mut out = Map::new();
    out.insert("kind".into(), Value::String("task-notification".into()));
    for key in ["subkind", "fireReason"] {
        if let Some(value) = origin.get(key) {
            out.insert(key.into(), value.clone());
        }
    }
    Value::Object(out)
}

/// `Vq(origin, commandMode)`: a queued command's origin, defaulted from its command mode.
fn queued_command_origin(attachment: &Value) -> Option<Value> {
    let origin = attachment.get("origin").filter(|o| o.get("kind").is_some_and(Value::is_string));
    match origin {
        Some(origin) => Some(normalize_origin(origin)),
        None => (str_of(attachment, "commandMode") == Some("task-notification")).then(|| json!({ "kind": "task-notification" })),
    }
}

/// `GZ`: entries → the visible conversation.
fn session_messages_from_entries(entries: &[Value], include_system_messages: bool) -> Vec<Value> {
    rewrite_queued_commands(build_chain(entries))
        .iter()
        .filter(|entry| {
            let kind = str_of(entry, "type");
            (matches!(kind, Some("user" | "assistant")) || (kind == Some("system") && include_system_messages))
                && !flag(entry, "isMeta")
                && !flag(entry, "isSidechain")
                && !truthy(entry, "teamName")
        })
        .map(to_session_message)
        .collect()
}

/// `getSessionMessages(sessionId, {dir, includeSystemMessages})`.
pub fn get_session_messages(config_dir: &Path, session_id: &str, dir: Option<&str>, include_system_messages: bool) -> Vec<Value> {
    if !is_uuid(session_id) {
        return Vec::new();
    }
    let Some(path) = find_session_file(config_dir, session_id, dir) else {
        return Vec::new();
    };
    let Some(text) = read_transcript(&path) else { return Vec::new() };
    session_messages_from_entries(&parse_entries(&text), include_system_messages)
}

/// `EI`: a real conversation message.
fn is_conversation_entry(entry: &Value) -> bool {
    matches!(str_of(entry, "type"), Some("user" | "assistant")) && !flag(entry, "isMeta") && !truthy(entry, "teamName")
}

/// `_9`: a queued command's source uuid.
fn queued_source_uuid(entry: &Value) -> Option<String> {
    if str_of(entry, "type") != Some("attachment") {
        return None;
    }
    let attachment = entry.get("attachment")?;
    (str_of(attachment, "type") == Some("queued_command"))
        .then(|| str_of(attachment, "source_uuid").filter(|s| !s.is_empty()).map(str::to_string))
        .flatten()
}

/// `bI`: the index of a uuid, or of the queued command it came from.
fn index_of_message(entries: &[Value], uuid: &str) -> Option<usize> {
    entries
        .iter()
        .position(|e| str_of(e, "uuid") == Some(uuid))
        .or_else(|| entries.iter().position(|e| queued_source_uuid(e).as_deref() == Some(uuid)))
}

/// `OWe`: drop branches that dangle after the cut point.
fn prune_dangling(prefix: Vec<Value>, chain: &[Value], file_index: &HashMap<String, usize>) -> Vec<Value> {
    let Some(last) = prefix.last() else { return prefix };
    let chain_start = chain.first().and_then(|first| file_index.get(&uuid_of(first)).copied());
    let Some(chain_start) = chain_start else { return prefix };
    if is_conversation_entry(last) {
        return prefix;
    }
    let by_uuid: HashMap<String, &Value> = prefix.iter().map(|e| (uuid_of(e), e)).collect();
    let mut kept: HashSet<String> = chain.iter().map(uuid_of).collect();
    let mut cursor = Some(last);
    while let Some(entry) = cursor {
        if kept.contains(&uuid_of(entry)) {
            break;
        }
        kept.insert(uuid_of(entry));
        cursor = parent_of(entry).and_then(|p| by_uuid.get(&p).copied());
    }
    let anchor = prefix
        .iter()
        .rposition(|e| is_conversation_entry(e) && kept.contains(&uuid_of(e)))
        .map_or(0, |i| i + 1);
    let mut dropped: HashSet<String> = HashSet::new();
    for entry in &prefix[anchor..] {
        if !is_conversation_entry(entry) || kept.contains(&uuid_of(entry)) {
            continue;
        }
        let mut cursor = Some(entry);
        while let Some(current) = cursor {
            let uuid = uuid_of(current);
            if kept.contains(&uuid) || dropped.contains(&uuid) || file_index.get(&uuid).copied().unwrap_or(chain_start) < chain_start {
                break;
            }
            dropped.insert(uuid);
            cursor = parent_of(current).and_then(|p| by_uuid.get(&p).copied());
        }
    }
    if dropped.is_empty() {
        return prefix;
    }
    for entry in &prefix {
        if parent_of(entry).is_some_and(|p| dropped.contains(&p)) {
            dropped.insert(uuid_of(entry));
        }
    }
    prefix.into_iter().filter(|e| !dropped.contains(&uuid_of(e))).collect()
}

struct ForkSource {
    transcript: Vec<Value>,
    content_replacements: Vec<Value>,
    relocated_cwd: Option<String>,
    history_suppressed: bool,
    atis_latch: Option<String>,
}

/// `AWe`/`S9`.
fn read_fork_source(text: &str, session_id: &str) -> ForkSource {
    let mut source = ForkSource {
        transcript: Vec::new(),
        content_replacements: Vec::new(),
        relocated_cwd: None,
        history_suppressed: false,
        atis_latch: None,
    };
    for line in text.split('\n').map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let kind = str_of(&entry, "type");
        let same_session = str_of(&entry, "sessionId") == Some(session_id);
        match kind {
            Some("user" | "assistant" | "attachment" | "system" | "progress") if entry.get("uuid").is_some_and(Value::is_string) => {
                source.transcript.push(entry)
            }
            Some("history-suppression") => source.history_suppressed = true,
            Some("atis-latch") if same_session => {
                if let Some(atis) = str_of(&entry, "atis").filter(|a| a.bytes().all(|b| (0x21..=0x7e).contains(&b))) {
                    source.atis_latch = Some(atis.to_string());
                }
            }
            Some("content-replacement") if same_session => {
                if let Some(replacements) = entry.get("replacements").and_then(Value::as_array) {
                    source.content_replacements.extend(replacements.iter().cloned());
                }
            }
            Some("relocated") if same_session => {
                if let Some(cwd) = str_of(&entry, "relocatedCwd").filter(|c| !c.is_empty()) {
                    source.relocated_cwd = Some(cwd.to_string());
                }
            }
            _ => {}
        }
    }
    source
}

/// `forkSession(sessionId, {dir, upToMessageId, title})`: write a copy of the session (up to
/// a message) under a new id, with every uuid rewritten, and return the new id.
pub fn fork_session(
    config_dir: &Path,
    session_id: &str,
    dir: Option<&str>,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<String, HistoryError> {
    if !is_uuid(session_id) {
        return Err(HistoryError::InvalidSessionId(session_id.to_string()));
    }
    let path = find_session_file(config_dir, session_id, dir).ok_or_else(|| {
        HistoryError::NotFound(match dir {
            Some(dir) => format!("Session {session_id} not found in project directory for {dir}"),
            None => format!("Session {session_id} not found"),
        })
    })?;
    let text = std::fs::read_to_string(&path).map_err(|e| HistoryError::Io(e.to_string()))?;
    let source = read_fork_source(&text, session_id);
    let mut entries: Vec<Value> = source.transcript.iter().filter(|e| !flag(e, "isSidechain")).cloned().collect();
    if entries.is_empty() {
        return Err(HistoryError::NothingToFork(session_id.to_string()));
    }
    if let Some(target) = up_to_message_id {
        let chain = build_chain(&entries);
        let mut first_index: HashMap<String, usize> = HashMap::new();
        for (index, entry) in entries.iter().enumerate() {
            first_index.entry(uuid_of(entry)).or_insert(index);
        }
        let chain_end = chain.iter().filter_map(|e| first_index.get(&uuid_of(e)).copied()).max();
        let mut lookup = chain.clone();
        if let Some(end) = chain_end {
            lookup.extend(entries[end + 1..].iter().cloned());
        } else {
            lookup.extend(entries.iter().cloned());
        }
        let cut = match index_of_message(&lookup, target) {
            Some(found) => first_index.get(&uuid_of(&lookup[found])).copied(),
            None => index_of_message(&entries, target),
        };
        let Some(cut) = cut else {
            return Err(HistoryError::MessageNotFound {
                message: target.to_string(),
                session: session_id.to_string(),
            });
        };
        entries = prune_dangling(entries[..=cut].to_vec(), &chain, &first_index);
    }
    let new_ids: HashMap<String, String> = entries.iter().map(|e| (uuid_of(e), zc_core::uuid_v4())).collect();
    let messages: Vec<&Value> = entries.iter().filter(|e| str_of(e, "type") != Some("progress")).collect();
    if messages.is_empty() {
        return Err(HistoryError::NothingToFork(session_id.to_string()));
    }
    let by_uuid: HashMap<String, &Value> = entries.iter().map(|e| (uuid_of(e), e)).collect();
    let forked_id = zc_core::uuid_v4();
    let now = zc_core::now_iso();
    let mut out: Vec<Value> = Vec::new();
    if source.history_suppressed {
        out.push(json!({ "type": "history-suppression", "sessionId": forked_id, "cause": "fork_inherit", "ts": now }));
    }
    for (index, entry) in messages.iter().enumerate() {
        let mut parent: Option<String> = None;
        let mut cursor = parent_of(entry);
        let mut seen = HashSet::new();
        while let Some(parent_uuid) = cursor {
            let Some(parent_entry) = by_uuid.get(&parent_uuid) else { break };
            if str_of(parent_entry, "type") != Some("progress") {
                parent = new_ids.get(&parent_uuid).cloned();
                break;
            }
            if !seen.insert(parent_uuid.clone()) {
                parent = new_ids.get(&parent_uuid).cloned();
                break;
            }
            cursor = parent_of(parent_entry);
        }
        let mut forked = entry.as_object().cloned().unwrap_or_default();
        if str_of(entry, "type") == Some("system") && str_of(entry, "subtype") == Some("model_refusal_fallback") {
            forked.insert("neutralizedByFork".into(), Value::Bool(true));
        }
        if let Some(attachment) = entry
            .get("attachment")
            .filter(|a| str_of(entry, "type") == Some("attachment") && str_of(a, "type") == Some("deferred_tools_record"))
        {
            if let Some(names) = attachment.get("nameOnlyAnnouncements").and_then(Value::as_array) {
                let mut next = attachment.clone();
                next["nameOnlyAnnouncements"] = Value::Array(
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .filter_map(|n| new_ids.get(n))
                        .map(|n| Value::String(n.clone()))
                        .collect(),
                );
                forked.insert("attachment".into(), next);
            }
        }
        if let Some(mapped) = queued_source_uuid(entry).and_then(|source| new_ids.get(&source).cloned()) {
            let mut attachment = forked.get("attachment").cloned().unwrap_or_else(|| json!({}));
            attachment["source_uuid"] = Value::String(mapped);
            forked.insert("attachment".into(), attachment);
        }
        forked.insert("uuid".into(), Value::String(new_ids[&uuid_of(entry)].clone()));
        forked.insert("parentUuid".into(), parent.map(Value::String).unwrap_or(Value::Null));
        match entry.get("logicalParentUuid") {
            None => {
                forked.remove("logicalParentUuid");
            }
            Some(Value::Null) => {
                forked.insert("logicalParentUuid".into(), Value::Null);
            }
            Some(logical) => {
                let mapped = logical.as_str().and_then(|l| new_ids.get(l)).cloned().map(Value::String).unwrap_or(Value::Null);
                forked.insert("logicalParentUuid".into(), mapped);
            }
        }
        forked.insert("sessionId".into(), Value::String(forked_id.clone()));
        if index == messages.len() - 1 {
            forked.insert("timestamp".into(), Value::String(now.clone()));
        }
        forked.insert("isSidechain".into(), Value::Bool(false));
        for key in ["teamName", "agentName", "sessionKind", "slug", "sourceToolAssistantUUID"] {
            forked.remove(key);
        }
        forked.insert("forkedFrom".into(), json!({ "sessionId": session_id, "messageUuid": uuid_of(entry) }));
        out.push(Value::Object(forked));
    }
    if !source.content_replacements.is_empty() {
        out.push(json!({ "type": "content-replacement", "sessionId": forked_id, "replacements": source.content_replacements, "uuid": zc_core::uuid_v4(), "timestamp": now }));
    }
    if let Some(atis) = source.atis_latch {
        out.push(json!({ "type": "atis-latch", "sessionId": forked_id, "atis": atis }));
    }
    if let Some(cwd) = source.relocated_cwd {
        out.push(json!({ "type": "relocated", "sessionId": forked_id, "relocatedCwd": cwd }));
    }
    let custom_title = title.map(str::trim).filter(|t| !t.is_empty()).map(str::to_string).unwrap_or_else(|| {
        let base = Some(session_title(&text, &path, session_id))
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| "Forked session".into());
        format!("{base} (fork)")
    });
    out.push(json!({ "type": "custom-title", "sessionId": forked_id, "customTitle": custom_title, "uuid": zc_core::uuid_v4(), "timestamp": now }));
    let target = path
        .parent()
        .map(|dir| dir.join(format!("{forked_id}.jsonl")))
        .ok_or_else(|| HistoryError::Io("no project directory".into()))?;
    let mut body = String::new();
    for entry in &out {
        body.push_str(&crate::js::stringify(entry));
        body.push('\n');
    }
    write_private(&target, &body).map_err(|e| HistoryError::Io(e.to_string()))?;
    Ok(forked_id)
}

/// Bytes of a transcript's head and tail the SDK scans for metadata (`jn`).
const METADATA_WINDOW: usize = 64 * 1024;

/// The forked session's base title (`RWe`'s callback): the latest `customTitle` of the tail,
/// else the `custom-title.json` sidecar, else the head's; then `aiTitle` (tail, head); then the
/// first prompt.
fn session_title(text: &str, path: &Path, session_id: &str) -> String {
    let bytes = text.as_bytes();
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(METADATA_WINDOW)]);
    let tail = String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(METADATA_WINDOW)..]);
    let custom = last_string_field(&tail, "customTitle")
        .or_else(|| sidecar_title(path, session_id))
        .or_else(|| last_string_field(&head, "customTitle"));
    [custom, last_string_field(&tail, "aiTitle"), last_string_field(&head, "aiTitle")]
        .into_iter()
        .flatten()
        .find(|t| !t.is_empty())
        .unwrap_or_else(|| first_prompt(&head))
}

/// `fn(text, key)`: the value of the last `"key":"…"` in raw JSONL text.
fn last_string_field(text: &str, key: &str) -> Option<String> {
    let mut best: Option<(usize, String)> = None;
    for pattern in [format!("\"{key}\":\""), format!("\"{key}\": \"")] {
        let mut from = 0;
        while let Some(found) = text[from..].find(&pattern).map(|i| i + from) {
            let start = found + pattern.len();
            let raw = text.as_bytes();
            let mut end = start;
            let mut closed = false;
            while end < raw.len() {
                match raw[end] {
                    b'\\' => end += 2,
                    b'"' => {
                        closed = true;
                        break;
                    }
                    _ => end += 1,
                }
            }
            if closed && best.as_ref().is_none_or(|(at, _)| found > *at) {
                let value = &text[start..end];
                let unescaped = if value.contains('\\') {
                    serde_json::from_str::<String>(&format!("\"{value}\"")).unwrap_or_else(|_| value.to_string())
                } else {
                    value.to_string()
                };
                best = Some((found, unescaped));
            }
            from = (end + 1).min(text.len());
            if !closed {
                break;
            }
        }
    }
    best.map(|(_, value)| value)
}

/// `Mu`: `<project dir>/<session id>/custom-title.json`, normalized like `YZ`.
fn sidecar_title(path: &Path, session_id: &str) -> Option<String> {
    let file = path.parent()?.join(session_id).join("custom-title.json");
    let value: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    let title = value.get("customTitle")?.as_str()?;
    // `dS` folds each run of control/format characters (and line/paragraph separators) into
    // one space.
    let mut folded = String::new();
    let mut in_run = false;
    for c in title.trim().chars() {
        if c.is_control() || is_format_char(c) || c == '\u{2028}' || c == '\u{2029}' {
            if !in_run {
                folded.push(' ');
            }
            in_run = true;
        } else {
            folded.push(c);
            in_run = false;
        }
    }
    let kept: String = folded.chars().filter(|c| !matches!(*c as u32, 0x00..=0x1f | 0x7f..=0x9f)).take(200).collect();
    Some(kept.trim().to_string()).filter(|t| !t.is_empty())
}

/// Unicode `Cf` characters common in titles (zero-width, bidi marks, BOM).
fn is_format_char(c: char) -> bool {
    matches!(c as u32, 0xad | 0x600..=0x605 | 0x61c | 0x6dd | 0x70f | 0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2064 | 0x2066..=0x206f | 0xfeff | 0xfff9..=0xfffb)
}

/// `_S(head)`: the first real user prompt, else the first slash command's name.
fn first_prompt(head: &str) -> String {
    let mut command_fallback = String::new();
    for line in head.split('\n') {
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\": \"user\"") {
            continue;
        }
        if line.contains("\"tool_result\"") || line.contains("\"isMeta\":true") || line.contains("\"isMeta\": true") {
            continue;
        }
        if line.contains("\"isCompactSummary\":true") || line.contains("\"isCompactSummary\": true") {
            continue;
        }
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        if let Some(prompt) = entry_prompt(&entry, &mut command_fallback) {
            return prompt;
        }
    }
    command_fallback
}

/// `Ca(entry, state)`.
fn entry_prompt(entry: &Value, command_fallback: &mut String) -> Option<String> {
    static COMMAND: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static BASH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    static SKIP: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    if str_of(entry, "type") != Some("user") || entry.get("isMeta") == Some(&Value::Bool(true)) || entry.get("isCompactSummary") == Some(&Value::Bool(true)) {
        return None;
    }
    let message = entry.get("message").filter(|m| !m.is_null())?;
    let mut texts = Vec::new();
    match message.get("content") {
        Some(Value::String(text)) => texts.push(text.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks.iter().filter(|b| b.is_object()) {
                if str_of(block, "type") == Some("tool_result") {
                    return None;
                }
                if str_of(block, "type") == Some("text") {
                    if let Some(text) = str_of(block, "text") {
                        texts.push(text.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    let command = COMMAND.get_or_init(|| regex::Regex::new(r"<command-name>(.*?)</command-name>").unwrap());
    let bash = BASH.get_or_init(|| regex::Regex::new(r"(?s)<bash-input>(.*?)</bash-input>").unwrap());
    let skip = SKIP.get_or_init(|| regex::Regex::new(r"^(?:\s*<[a-z][\w-]*[\s>]|\[Request interrupted by user[^\]]*\])").unwrap());
    for text in texts {
        // `LA` also unfolds `<pasted_content>` blocks; prompts carrying them are left as typed.
        let flat = text.replace('\n', " ");
        let flat = flat.trim();
        if flat.is_empty() {
            continue;
        }
        if let Some(found) = command.captures(flat) {
            if command_fallback.is_empty() {
                *command_fallback = found[1].to_string();
            }
            continue;
        }
        if let Some(found) = bash.captures(flat) {
            return Some(format!("! {}", found[1].trim()));
        }
        if skip.is_match(flat) {
            continue;
        }
        if crate::js::utf16_len(flat) > 200 {
            return Some(format!("{}…", crate::js::slice_utf16(flat, 200).trim()));
        }
        return Some(flat.to_string());
    }
    None
}

fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(path)?;
    file.write_all(body.as_bytes())
}

/// The history operations the adapter's rewind needs (injectable for tests).
pub trait HistoryOps: Send + Sync {
    fn get_session_messages(&self, session_id: &str, dir: Option<&str>) -> Result<Vec<Value>, HistoryError>;
    fn fork_session(&self, session_id: &str, dir: Option<&str>, up_to_message_id: &str) -> Result<String, HistoryError>;
}

/// [`HistoryOps`] over a Claude config directory.
#[derive(Debug, Clone)]
pub struct FileHistory {
    pub config_dir: PathBuf,
}

impl HistoryOps for FileHistory {
    fn get_session_messages(&self, session_id: &str, dir: Option<&str>) -> Result<Vec<Value>, HistoryError> {
        Ok(get_session_messages(&self.config_dir, session_id, dir, true))
    }

    fn fork_session(&self, session_id: &str, dir: Option<&str>, up_to_message_id: &str) -> Result<String, HistoryError> {
        fork_session(&self.config_dir, session_id, dir, Some(up_to_message_id), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "550e8400-e29b-41d4-a716-446655440010";

    fn entry(kind: &str, uuid: &str, parent: Option<&str>, content: Value) -> Value {
        json!({ "type": kind, "uuid": uuid, "parentUuid": parent, "sessionId": SESSION, "timestamp": format!("2026-01-01T00:00:0{}.000Z", uuid.len() % 10), "message": { "role": kind, "content": content } })
    }

    fn write_session(dir: &Path, cwd: &str, entries: &[Value]) -> PathBuf {
        let project = dir.join("projects").join(encode_project_dir(cwd));
        std::fs::create_dir_all(&project).unwrap();
        let path = project.join(format!("{SESSION}.jsonl"));
        std::fs::write(&path, entries.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        path
    }

    #[test]
    fn encodes_project_dirs_like_the_cli() {
        assert_eq!(encode_project_dir("/Users/someone/work/app.v2"), "-Users-someone-work-app-v2");
        let long = format!("/{}", "a".repeat(250));
        let encoded = encode_project_dir(&long);
        assert!(encoded.starts_with(&format!("-{}", "a".repeat(199))));
        assert_eq!(encoded.len(), 201 + to_base36(i64::from(string_hash(&long)).unsigned_abs()).len());
    }

    #[test]
    fn rebuilds_the_conversation_chain_and_drops_abandoned_branches() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = "/tmp/history-test-workspace";
        write_session(
            temp.path(),
            cwd,
            &[
                entry("user", "u1", None, json!("first")),
                entry("assistant", "a1", Some("u1"), json!([{"type": "text", "text": "one"}])),
                entry("user", "u2-abandoned", Some("a1"), json!("dead end")),
                entry("user", "u2", Some("a1"), json!("second")),
                entry("assistant", "a2", Some("u2"), json!([{"type": "text", "text": "two"}])),
                json!({"type": "summary", "summary": "x"}),
            ],
        );
        let messages = get_session_messages(temp.path(), SESSION, Some(cwd), true);
        assert_eq!(
            messages.iter().map(|m| m["uuid"].as_str().unwrap()).collect::<Vec<_>>(),
            vec!["u1", "a1", "u2", "a2"]
        );
        assert_eq!(messages[0]["parent_tool_use_id"], Value::Null);
        assert_eq!(messages[0]["message"]["content"], json!("first"));
        assert!(get_session_messages(temp.path(), "not-a-uuid", Some(cwd), true).is_empty());
    }

    #[test]
    fn turns_answered_queued_commands_into_user_messages() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = "/tmp/history-test-queued";
        write_session(
            temp.path(),
            cwd,
            &[
                entry("user", "u1", None, json!("run it")),
                json!({"type": "attachment", "uuid": "q1", "parentUuid": "u1", "sessionId": SESSION, "attachment": {"type": "queued_command", "prompt": "also this", "source_uuid": "steer-1"}}),
                entry("assistant", "a1", Some("q1"), json!([{"type": "text", "text": "ok"}])),
            ],
        );
        let messages = get_session_messages(temp.path(), SESSION, Some(cwd), true);
        assert_eq!(
            messages.iter().map(|m| m["uuid"].as_str().unwrap()).collect::<Vec<_>>(),
            vec!["u1", "steer-1", "a1"]
        );
        assert_eq!(messages[1]["isQueuedCommand"], json!(true));
        assert_eq!(messages[1]["message"]["content"], json!("also this"));
    }

    #[test]
    fn forks_up_to_a_message_with_fresh_ids() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = "/tmp/history-test-fork";
        write_session(
            temp.path(),
            cwd,
            &[
                entry("user", "u1", None, json!("first")),
                entry("assistant", "a1", Some("u1"), json!([{"type": "text", "text": "one"}])),
                entry("user", "u2", Some("a1"), json!("second")),
                entry("assistant", "a2", Some("u2"), json!([{"type": "text", "text": "two"}])),
            ],
        );
        let forked = fork_session(temp.path(), SESSION, Some(cwd), Some("a1"), None).unwrap();
        assert!(is_uuid(&forked));
        let messages = get_session_messages(temp.path(), &forked, Some(cwd), true);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["message"]["content"], json!("first"));
        assert_ne!(messages[0]["uuid"], json!("u1"));
        assert_eq!(messages[1]["message"]["content"], json!([{"type": "text", "text": "one"}]));
        assert!(matches!(
            fork_session(temp.path(), SESSION, Some(cwd), Some("missing"), None),
            Err(HistoryError::MessageNotFound { .. })
        ));
    }
}
