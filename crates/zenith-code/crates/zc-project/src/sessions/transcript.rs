//! The transcript record schema of `AgentSessionScanner.ts` (`TranscriptRecord`), its field
//! selector, its strict decode, and `parseAgentSessionTranscript`: the visible user and
//! assistant text of a Claude or Codex session, ignoring tools, reasoning and malformed records.

use std::collections::HashSet;

use serde_json::{Map, Value};
use zc_contracts::{AgentSessionSource, ProviderInstanceId};

use super::json::PathSegment;

/// `MAX_IMPORTED_MESSAGES`.
pub const MAX_IMPORTED_MESSAGES: usize = 200;
/// `MAX_IMPORT_RECORDS`.
pub const MAX_IMPORT_RECORDS: usize = 100_000;

/// `TranscriptContentBlock`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ContentBlock {
    pub kind: Option<String>,
    pub text: Option<String>,
}

/// `TranscriptMessage.content`: a string or blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

/// `TranscriptMessage`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Message {
    pub role: Option<String>,
    pub content: Option<Content>,
    pub model: Option<String>,
}

/// `TranscriptRecord.payload` (Codex).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Payload {
    pub id: Option<String>,
    pub session_id: Option<String>,
    pub kind: Option<String>,
    pub role: Option<String>,
    pub message: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub content: Option<Vec<ContentBlock>>,
    pub internal_chat_message_metadata_passthrough: Option<Value>,
}

/// `TranscriptRecord`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TranscriptRecord {
    pub kind: Option<String>,
    pub timestamp: Option<String>,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub ai_title: Option<String>,
    pub is_sidechain: Option<bool>,
    pub is_meta: Option<bool>,
    pub is_compact_summary: Option<bool>,
    pub message: Option<Message>,
    pub payload: Option<Payload>,
}

/// `Schema.optional(Schema.String)`: absent, or a string (`null` fails).
fn opt_string(map: &Map<String, Value>, key: &str) -> Result<Option<String>, ()> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(()),
    }
}

fn opt_bool(map: &Map<String, Value>, key: &str) -> Result<Option<bool>, ()> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(()),
    }
}

fn decode_block(value: &Value) -> Result<ContentBlock, ()> {
    let map = value.as_object().ok_or(())?;
    Ok(ContentBlock {
        kind: opt_string(map, "type")?,
        text: opt_string(map, "text")?,
    })
}

fn decode_blocks(value: &Value) -> Result<Vec<ContentBlock>, ()> {
    value.as_array().ok_or(())?.iter().map(decode_block).collect()
}

fn decode_message(value: &Value) -> Result<Message, ()> {
    let map = value.as_object().ok_or(())?;
    let content = match map.get("content") {
        None => None,
        Some(Value::String(text)) => Some(Content::Text(text.clone())),
        Some(other) => Some(Content::Blocks(decode_blocks(other)?)),
    };
    Ok(Message {
        role: opt_string(map, "role")?,
        content,
        model: opt_string(map, "model")?,
    })
}

fn decode_payload(value: &Value) -> Result<Payload, ()> {
    let map = value.as_object().ok_or(())?;
    Ok(Payload {
        id: opt_string(map, "id")?,
        session_id: opt_string(map, "session_id")?,
        kind: opt_string(map, "type")?,
        role: opt_string(map, "role")?,
        message: opt_string(map, "message")?,
        model: opt_string(map, "model")?,
        cwd: opt_string(map, "cwd")?,
        content: map.get("content").map(decode_blocks).transpose()?,
        internal_chat_message_metadata_passthrough: map.get("internal_chat_message_metadata_passthrough").cloned(),
    })
}

/// `decodeTranscriptValue`: the strict decode of one record (`None` when any read field has
/// the wrong type).
pub fn decode_transcript_record(value: &Value) -> Option<TranscriptRecord> {
    let map = value.as_object()?;
    let record = (|| -> Result<TranscriptRecord, ()> {
        Ok(TranscriptRecord {
            kind: opt_string(map, "type")?,
            timestamp: opt_string(map, "timestamp")?,
            cwd: opt_string(map, "cwd")?,
            session_id: opt_string(map, "sessionId")?,
            ai_title: opt_string(map, "aiTitle")?,
            is_sidechain: opt_bool(map, "isSidechain")?,
            is_meta: opt_bool(map, "isMeta")?,
            is_compact_summary: opt_bool(map, "isCompactSummary")?,
            message: map.get("message").map(decode_message).transpose()?,
            payload: map.get("payload").map(decode_payload).transpose()?,
        })
    })();
    record.ok()
}

/// `decodeTranscriptRecord`: one JSON line.
pub fn decode_transcript_line(line: &str) -> Option<TranscriptRecord> {
    let value: Value = serde_json::from_str(line).ok()?;
    decode_transcript_record(&value)
}

fn key(segment: Option<&PathSegment>) -> Option<&str> {
    match segment {
        Some(PathSegment::Key(key)) => Some(key),
        _ => None,
    }
}

/// `includes` over a content block (`Struct{type?, text?}`) from `path[index]`.
fn block_includes(path: &[PathSegment], index: usize) -> bool {
    if index == path.len() {
        return true;
    }
    matches!(key(path.get(index)), Some("type" | "text")) && index + 1 == path.len()
}

/// `includes` over `Array(Block)` from `path[index]`.
fn blocks_includes(path: &[PathSegment], index: usize) -> bool {
    if index == path.len() {
        return true;
    }
    match path.get(index) {
        Some(PathSegment::Index(_)) => block_includes(path, index + 1),
        // A non-numeric key under an array schema reaches the decoder (which rejects it).
        _ => true,
    }
}

/// `createTranscriptJsonSelector(TranscriptRecord)`: whether a JSON path is read by the schema.
pub fn select_transcript_path(path: &[PathSegment]) -> bool {
    if path.is_empty() {
        return true;
    }
    let leaf = path.len() == 1;
    match key(path.first()) {
        Some("type" | "timestamp" | "cwd" | "sessionId" | "aiTitle" | "isSidechain" | "isMeta" | "isCompactSummary") => leaf,
        Some("message") => {
            if leaf {
                return true;
            }
            match key(path.get(1)) {
                Some("role" | "model") => path.len() == 2,
                // `String | Array(Block)`: the string member selects nothing deeper.
                Some("content") => blocks_includes(path, 2),
                _ => false,
            }
        }
        Some("payload") => {
            if leaf {
                return true;
            }
            match key(path.get(1)) {
                Some("id" | "session_id" | "type" | "role" | "message" | "model" | "cwd") => path.len() == 2,
                Some("content") => blocks_includes(path, 2),
                // `Schema.Unknown`: everything below reaches the decoder.
                Some("internal_chat_message_metadata_passthrough") => true,
                _ => false,
            }
        }
        _ => false,
    }
}

/// `AgentSessionThreadMessage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadMessage {
    /// `"user"` or `"assistant"`.
    pub role: String,
    pub text: String,
    pub created_at: String,
}

/// `AgentSessionThread`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentSessionThread {
    pub source: AgentSessionSource,
    pub provider_instance_id: ProviderInstanceId,
    pub provider_session_id: String,
    pub title: String,
    pub model: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub messages: Vec<ThreadMessage>,
}

/// `AgentSessionTranscriptMetadata`.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptMetadata {
    pub source: AgentSessionSource,
    pub provider_instance_id: ProviderInstanceId,
    pub fallback_session_id: String,
    pub last_active_at_ms: i64,
}

fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// `extractText`: a string content trimmed, or the trimmed text blocks joined by newlines.
fn extract_text(content: Option<&Content>) -> String {
    match content {
        None => String::new(),
        Some(Content::Text(text)) => js_trim(text).to_owned(),
        Some(Content::Blocks(blocks)) => blocks
            .iter()
            .filter(|block| matches!(block.kind.as_deref(), Some("text" | "input_text" | "output_text")))
            .map(|block| js_trim(block.text.as_deref().unwrap_or_default()))
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn blocks_content(blocks: &Option<Vec<ContentBlock>>) -> Option<Content> {
    blocks.clone().map(Content::Blocks)
}

/// `normalizeTimestamp`: a parseable timestamp re-encoded as ISO, else the fallback.
fn normalize_timestamp(value: Option<&str>, fallback: &str) -> String {
    match value {
        None => fallback.to_owned(),
        Some(value) => parse_date_time(value).unwrap_or_else(|| fallback.to_owned()),
    }
}

/// `DateTime.make(string)` → `formatIso`: what `new Date(value)` accepts, as an ISO string.
fn parse_date_time(value: &str) -> Option<String> {
    zc_core::time::normalize_iso(value)
}

/// `codexTurnId`.
fn codex_turn_id(metadata: Option<&Value>) -> Option<String> {
    let map = metadata?.as_object()?;
    match map.get("turn_id") {
        Some(Value::String(turn_id)) if !js_trim(turn_id).is_empty() => Some(turn_id.clone()),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Retained {
    message: ThreadMessage,
    codex_response_user: bool,
    /// Identity, for `firstUserMessage === message`.
    id: usize,
}

/// `splitTranscriptRecords`.
fn split_transcript_records(contents: &str, limit: usize) -> Vec<&str> {
    let records = contents.strip_suffix('\n').unwrap_or(contents);
    records.split('\n').take(limit).collect()
}

/// `parseAgentSessionTranscript`.
pub fn parse_agent_session_transcript(metadata: &TranscriptMetadata, contents: &str) -> Option<AgentSessionThread> {
    let lines = split_transcript_records(contents, MAX_IMPORT_RECORDS + 1);
    if lines.len() > MAX_IMPORT_RECORDS {
        return None;
    }
    let records: Vec<TranscriptRecord> = lines.iter().filter_map(|line| decode_transcript_line(line)).collect();
    parse_agent_session_records(metadata, &records)
}

/// `parseAgentSessionRecords`.
pub fn parse_agent_session_records(input: &TranscriptMetadata, records: &[TranscriptRecord]) -> Option<AgentSessionThread> {
    let fallback_timestamp = zc_core::time::iso_from_millis(input.last_active_at_ms);
    let is_codex = input.source == AgentSessionSource::Codex;
    // Claude filenames are session ids; Codex rollout names carry extra text, so only the
    // transcript metadata gives a resumable id.
    let mut provider_session_id = if is_codex { String::new() } else { input.fallback_session_id.clone() };
    let mut title: Option<String> = None;
    let mut model: Option<String> = None;
    let mut has_codex_session_id = false;
    let mut messages: Vec<Retained> = Vec::new();
    let mut first_user: Option<Retained> = None;
    let mut next_id = 0usize;

    // A Codex response item can carry generated setup text beside the real prompt. Response
    // users are suppressed only when the shared turn id and a verbatim event copy prove which
    // prompt the user submitted.
    let mut canonical_indices: HashSet<usize> = HashSet::new();
    if is_codex {
        let mut canonical_texts: HashSet<String> = HashSet::new();
        let mut response_users: Vec<(usize, String, String)> = Vec::new();
        let mut finish_turn = |canonical_texts: &mut HashSet<String>, response_users: &mut Vec<(usize, String, String)>| {
            let turn_ids: HashSet<String> = response_users
                .iter()
                .filter(|(_, _, text)| canonical_texts.contains(text))
                .map(|(_, turn_id, _)| turn_id.clone())
                .collect();
            for (index, turn_id, _) in response_users.iter() {
                if turn_ids.contains(turn_id) {
                    canonical_indices.insert(*index);
                }
            }
            canonical_texts.clear();
            response_users.clear();
        };
        for (index, record) in records.iter().enumerate() {
            let payload = record.payload.as_ref();
            let payload_kind = payload.and_then(|p| p.kind.as_deref());
            let payload_role = payload.and_then(|p| p.role.as_deref());
            if record.kind.as_deref() == Some("response_item") && payload_kind == Some("message") && payload_role == Some("assistant") {
                finish_turn(&mut canonical_texts, &mut response_users);
                continue;
            }
            if record.kind.as_deref() == Some("event_msg") && payload_kind == Some("user_message") {
                let text = js_trim(payload.and_then(|p| p.message.as_deref()).unwrap_or_default());
                if !text.is_empty() {
                    canonical_texts.insert(text.to_owned());
                }
                continue;
            }
            if record.kind.as_deref() == Some("response_item") && payload_kind == Some("message") && payload_role == Some("user") {
                let payload = payload.expect("checked");
                let turn_id = codex_turn_id(payload.internal_chat_message_metadata_passthrough.as_ref());
                let text = extract_text(blocks_content(&payload.content).as_ref());
                if let Some(turn_id) = turn_id {
                    if !text.is_empty() {
                        response_users.push((index, turn_id, text));
                    }
                }
            }
        }
        finish_turn(&mut canonical_texts, &mut response_users);
    }

    let mut retain = |messages: &mut Vec<Retained>, first_user: &mut Option<Retained>, message: ThreadMessage, codex_response_user: bool| {
        let retained = Retained {
            message,
            codex_response_user,
            id: next_id,
        };
        next_id += 1;
        if first_user.is_none() && retained.message.role == "user" {
            *first_user = Some(retained.clone());
        }
        messages.push(retained);
        if messages.len() > MAX_IMPORTED_MESSAGES {
            messages.remove(0);
        }
    };

    let has_matching_codex_event_in_turn = |messages: &[Retained], text: &str| {
        let comparison = js_trim(text);
        for message in messages.iter().rev() {
            if message.message.role == "assistant" {
                return false;
            }
            if message.message.role == "user" && !message.codex_response_user && js_trim(&message.message.text) == comparison {
                return true;
            }
        }
        false
    };

    for (index, record) in records.iter().enumerate() {
        if !is_codex {
            if record.is_sidechain == Some(true) || record.is_meta == Some(true) || record.is_compact_summary == Some(true) {
                continue;
            }
            if let Some(session_id) = record.session_id.as_deref().map(js_trim).filter(|s| !s.is_empty()) {
                provider_session_id = session_id.to_owned();
            }
            if let Some(ai_title) = record.ai_title.as_deref().map(js_trim).filter(|s| !s.is_empty()) {
                title = Some(ai_title.to_owned());
            }
            let message_model = record.message.as_ref().and_then(|m| m.model.as_deref()).map(js_trim);
            // Claude's sentinel for local error responses is not a selectable model.
            if let Some(message_model) = message_model.filter(|m| !m.is_empty() && *m != "<synthetic>") {
                model = Some(message_model.to_owned());
            }
            let role = match record.kind.as_deref() {
                Some(role @ ("user" | "assistant")) => role,
                _ => continue,
            };
            let text = extract_text(record.message.as_ref().and_then(|m| m.content.as_ref()));
            if text.is_empty() {
                continue;
            }
            retain(
                &mut messages,
                &mut first_user,
                ThreadMessage {
                    role: role.to_owned(),
                    text,
                    created_at: normalize_timestamp(record.timestamp.as_deref(), &fallback_timestamp),
                },
                false,
            );
            continue;
        }

        let payload = record.payload.as_ref();
        let kind = record.kind.as_deref();
        let payload_kind = payload.and_then(|p| p.kind.as_deref());
        if kind == Some("session_meta") {
            let session_id = payload
                .and_then(|p| p.id.as_deref().map(js_trim).filter(|s| !s.is_empty()))
                .or_else(|| payload.and_then(|p| p.session_id.as_deref().map(js_trim).filter(|s| !s.is_empty())));
            if let (false, Some(session_id)) = (has_codex_session_id, session_id) {
                provider_session_id = session_id.to_owned();
                has_codex_session_id = true;
            }
            continue;
        }
        if kind == Some("turn_context") {
            if let Some(turn_model) = payload.and_then(|p| p.model.as_deref()).map(js_trim).filter(|m| !m.is_empty()) {
                model = Some(turn_model.to_owned());
                continue;
            }
        }
        if kind == Some("event_msg") && payload_kind == Some("user_message") {
            let text = payload.and_then(|p| p.message.clone()).unwrap_or_default();
            if js_trim(&text).is_empty() {
                continue;
            }
            // Codex can write the same prompt as a response item and an event: drop only the
            // matching response copy so mixed-format logs keep every distinct user message.
            let mut remove_at = None;
            for (position, message) in messages.iter().enumerate().rev() {
                if message.message.role == "assistant" {
                    break;
                }
                if message.codex_response_user && js_trim(&message.message.text) == js_trim(&text) {
                    remove_at = Some(position);
                    break;
                }
            }
            if let Some(position) = remove_at {
                let removed = messages.remove(position);
                if first_user.as_ref().is_some_and(|first| first.id == removed.id) {
                    first_user = None;
                }
            }
            retain(
                &mut messages,
                &mut first_user,
                ThreadMessage {
                    role: "user".into(),
                    text,
                    created_at: normalize_timestamp(record.timestamp.as_deref(), &fallback_timestamp),
                },
                false,
            );
            continue;
        }
        let role = payload.and_then(|p| p.role.as_deref());
        if kind != Some("response_item") || payload_kind != Some("message") || !matches!(role, Some("user" | "assistant")) {
            continue;
        }
        let role = role.expect("checked");
        let extracted = extract_text(blocks_content(&payload.expect("checked").content).as_ref());
        if extracted.is_empty() {
            continue;
        }
        if role == "user" && canonical_indices.contains(&index) {
            continue;
        }
        if role == "user" && has_matching_codex_event_in_turn(&messages, &extracted) {
            continue;
        }
        retain(
            &mut messages,
            &mut first_user,
            ThreadMessage {
                role: role.to_owned(),
                text: extracted,
                created_at: normalize_timestamp(record.timestamp.as_deref(), &fallback_timestamp),
            },
            role == "user",
        );
    }

    let first_user = first_user?;
    if js_trim(&provider_session_id).is_empty() {
        return None;
    }
    let first_retained = messages.iter().any(|message| message.id == first_user.id);
    let visible: Vec<ThreadMessage> = messages.iter().map(|m| m.message.clone()).collect();
    let retained_messages = if first_retained {
        visible
    } else {
        let keep = MAX_IMPORTED_MESSAGES - 1;
        let tail = &visible[visible.len().saturating_sub(keep)..];
        std::iter::once(first_user.message.clone()).chain(tail.iter().cloned()).collect()
    };
    let derived_title: String = {
        let first_line = js_trim(&first_user.message.text).split('\n').next().unwrap_or_default();
        let sliced: String = slice_utf16(first_line, 100);
        js_trim(&sliced).to_owned()
    };
    Some(AgentSessionThread {
        source: input.source,
        provider_instance_id: input.provider_instance_id.clone(),
        provider_session_id,
        title: title.unwrap_or_else(|| if derived_title.is_empty() { "Imported thread".into() } else { derived_title }),
        model,
        created_at: retained_messages.first().map_or_else(|| fallback_timestamp.clone(), |m| m.created_at.clone()),
        updated_at: fallback_timestamp,
        messages: retained_messages,
    })
}

/// `text.slice(0, n)` in UTF-16 units (a split surrogate pair keeps its first half as U+FFFD).
fn slice_utf16(text: &str, units: usize) -> String {
    let encoded: Vec<u16> = text.encode_utf16().take(units).collect();
    String::from_utf16_lossy(&encoded)
}

/// `extractDecodedCwd`.
pub fn extract_decoded_cwd(record: &TranscriptRecord) -> Option<String> {
    let cwd = record
        .cwd
        .as_deref()
        .map(js_trim)
        .filter(|c| !c.is_empty())
        .or_else(|| record.payload.as_ref().and_then(|p| p.cwd.as_deref()).map(js_trim).filter(|c| !c.is_empty()));
    cwd.map(str::to_owned)
}

/// `shouldRetainDecodedRecord`: only the records the history parser reads are kept in memory.
pub fn should_retain_decoded_record(source: AgentSessionSource, record: &TranscriptRecord) -> bool {
    if extract_decoded_cwd(record).is_some() {
        return true;
    }
    let kind = record.kind.as_deref();
    if source == AgentSessionSource::ClaudeAgent {
        return matches!(kind, Some("user" | "assistant"))
            || record.session_id.is_some()
            || record.ai_title.is_some()
            || record.message.as_ref().is_some_and(|m| m.model.is_some());
    }
    let payload = record.payload.as_ref();
    let payload_kind = payload.and_then(|p| p.kind.as_deref());
    matches!(kind, Some("session_meta" | "turn_context"))
        || (kind == Some("event_msg") && payload_kind == Some("user_message"))
        || (kind == Some("response_item") && payload_kind == Some("message") && matches!(payload.and_then(|p| p.role.as_deref()), Some("user" | "assistant")))
}
