//! Best-effort provider event logging with one shared writer per thread: port of
//! `provider/Layers/EventNdjsonLogger.ts`, `ProviderEventLoggers.ts` and the shared
//! `RotatingFileSink` (`packages/shared/src/logging.ts`).
//!
//! Line format (byte-identical to TS): `[<toISOString>] NTIVE|CANON|ORCH: <JSON.stringify>\n`,
//! one file per thread segment: `<dir>/<prefix><segment>.log` with `<prefix>` the basename of the
//! configured path minus its extension plus a dot (`events.log` → `events.`), rotated at
//! `max_bytes` into `.1` … `.max_files`. Records are batched (1 s window, 512 records or 1 MiB),
//! bounded (64 Ki characters, 1,024 fields, depth 16, else a summary), transient deltas are
//! dropped, and a retention pass (14 days, 512 MiB total) runs at most every 5 minutes.
//!
//! Writes are synchronous file appends behind a mutex, like the Node `appendFileSync` the TS
//! logger uses; the batch timer runs on tokio when there is a runtime.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Map, Value};
use zc_core::defect::js_length;

use crate::attachments::to_safe_thread_attachment_segment;
use crate::js_json;

const MEBIBYTE: u64 = 1024 * 1024;
const DAY_MS: i64 = 24 * 60 * 60 * 1_000;
pub const DEFAULT_MAX_BYTES: u64 = 10 * MEBIBYTE;
pub const DEFAULT_MAX_FILES: u64 = 10;
pub const DEFAULT_BATCH_WINDOW_MS: u64 = 1_000;
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 512 * MEBIBYTE;
pub const DEFAULT_MAX_AGE_MS: i64 = 14 * DAY_MS;
pub const DEFAULT_RETENTION_CHECK_INTERVAL_MS: i64 = 5 * 60 * 1_000;
pub const DEFAULT_MAX_BUFFERED_BYTES: u64 = MEBIBYTE;
pub const DEFAULT_MAX_BUFFERED_RECORDS: u64 = 512;
const MAX_RECORD_CHARACTERS: i64 = 64 * 1024;
const MAX_RECORD_FIELDS: i64 = 1_024;
const MAX_RECORD_DEPTH: usize = 16;
const GLOBAL_THREAD_SEGMENT: &str = "_global";

const TRANSIENT_CANONICAL_EVENT_TYPES: &[&str] = &[
    "content.delta",
    "hook.progress",
    "item.updated",
    "task.progress",
    "thread.realtime.audio.delta",
    "tool.progress",
    "turn.proposed.delta",
];
const TRANSIENT_NATIVE_METHODS: &[&str] = &[
    "item/agentMessage/delta",
    "item/commandExecution/outputDelta",
    "item/fileChange/outputDelta",
    "item/plan/delta",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/textDelta",
    "thread/realtime/outputAudio/delta",
    "thread/realtime/transcript/delta",
    "turn/diff/updated",
];
const TRANSIENT_ACP_UPDATES: &[&str] = &["agent_message_chunk", "agent_thought_chunk"];

/// Which view a record belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventNdjsonStream {
    /// Provider-protocol events as the transport sees them (written by the adapters).
    Native,
    /// Runtime events after the provider service normalized them.
    Canonical,
    Orchestration,
}

impl EventNdjsonStream {
    pub fn label(self) -> &'static str {
        match self {
            Self::Native => "NTIVE",
            Self::Canonical => "CANON",
            Self::Orchestration => "ORCH",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Canonical => "canonical",
            Self::Orchestration => "orchestration",
        }
    }
}

/// `ResourceAttribution.record` for provider log writes.
pub trait LogAttribution: Send + Sync {
    fn record(&self, component: &str, operation: &str, logical_write_bytes: u64, count: u64, duration_ms: i64);
}

/// The clock the logger stamps lines with (epoch milliseconds). Tests pin it.
pub type LogClock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// `EventNdjsonLogStoreOptions`.
#[derive(Clone)]
pub struct EventNdjsonLogStoreOptions {
    pub max_bytes: u64,
    pub max_files: u64,
    pub batch_window_ms: u64,
    pub max_total_bytes: u64,
    pub max_age_ms: i64,
    pub retention_check_interval_ms: i64,
    pub max_buffered_bytes: u64,
    pub max_buffered_records: u64,
    pub attribution: Option<Arc<dyn LogAttribution>>,
    pub clock: LogClock,
}

impl Default for EventNdjsonLogStoreOptions {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_BYTES,
            max_files: DEFAULT_MAX_FILES,
            batch_window_ms: DEFAULT_BATCH_WINDOW_MS,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_age_ms: DEFAULT_MAX_AGE_MS,
            retention_check_interval_ms: DEFAULT_RETENTION_CHECK_INTERVAL_MS,
            max_buffered_bytes: DEFAULT_MAX_BUFFERED_BYTES,
            max_buffered_records: DEFAULT_MAX_BUFFERED_RECORDS,
            attribution: None,
            clock: Arc::new(zc_core::now_millis),
        }
    }
}

impl std::fmt::Debug for EventNdjsonLogStoreOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventNdjsonLogStoreOptions")
            .field("max_bytes", &self.max_bytes)
            .field("max_files", &self.max_files)
            .field("batch_window_ms", &self.batch_window_ms)
            .field("max_total_bytes", &self.max_total_bytes)
            .field("max_age_ms", &self.max_age_ms)
            .finish_non_exhaustive()
    }
}

/// `EventNdjsonLogConfigurationError | EventNdjsonLogDirectoryError`.
#[derive(Debug, thiserror::Error)]
pub enum EventNdjsonLogStoreError {
    #[error("Provider event log option '{option}' must be an integer >= {minimum}; received {value} for '{file_path}'")]
    Configuration {
        file_path: String,
        option: &'static str,
        value: i64,
        minimum: i64,
    },
    #[error("Failed to create provider event log directory '{directory}'")]
    Directory { directory: String, cause: std::io::Error },
}

// ---------------------------------------------------------------------------------------------
// RotatingFileSink
// ---------------------------------------------------------------------------------------------

/// Port of the shared `RotatingFileSink` with `throwOnError: true`.
#[derive(Debug)]
pub struct RotatingFileSink {
    file_path: PathBuf,
    max_bytes: u64,
    max_files: u64,
    current_size: u64,
}

impl RotatingFileSink {
    pub fn new(file_path: impl Into<PathBuf>, max_bytes: u64, max_files: u64) -> std::io::Result<Self> {
        let file_path = file_path.into();
        if max_bytes < 1 || max_files < 1 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "maxBytes and maxFiles must be >= 1"));
        }
        if let Some(parent) = file_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut sink = Self {
            file_path,
            max_bytes,
            max_files,
            current_size: 0,
        };
        sink.prune_overflow_backups()?;
        sink.current_size = sink.read_current_size()?;
        Ok(sink)
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub fn write(&mut self, chunk: &[u8]) -> std::io::Result<()> {
        if chunk.is_empty() {
            return Ok(());
        }
        if self.current_size > 0 && self.current_size + chunk.len() as u64 > self.max_bytes {
            self.rotate()?;
        }
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&self.file_path)?;
        file.write_all(chunk)?;
        self.current_size += chunk.len() as u64;
        Ok(())
    }

    fn with_suffix(&self, index: u64) -> PathBuf {
        let mut name = self.file_path.as_os_str().to_owned();
        name.push(format!(".{index}"));
        PathBuf::from(name)
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        let oldest = self.with_suffix(self.max_files);
        if oldest.exists() {
            fs::remove_file(&oldest)?;
        }
        let mut index = self.max_files - 1;
        while index >= 1 {
            let source = self.with_suffix(index);
            if source.exists() {
                fs::rename(&source, self.with_suffix(index + 1))?;
            }
            index -= 1;
        }
        if self.file_path.exists() {
            fs::rename(&self.file_path, self.with_suffix(1))?;
        }
        self.current_size = 0;
        Ok(())
    }

    fn prune_overflow_backups(&self) -> std::io::Result<()> {
        let Some(dir) = self.file_path.parent() else {
            return Ok(());
        };
        let base_name = self.file_path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(suffix) = name.strip_prefix(&format!("{base_name}.")) else {
                continue;
            };
            let Ok(suffix) = suffix.parse::<u64>() else {
                continue;
            };
            if suffix <= self.max_files {
                continue;
            }
            let _ = fs::remove_file(entry.path());
        }
        Ok(())
    }

    fn read_current_size(&self) -> std::io::Result<u64> {
        match fs::metadata(&self.file_path) {
            Ok(metadata) => Ok(metadata.len()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Record shaping
// ---------------------------------------------------------------------------------------------

fn get<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.as_object().and_then(|object| object.get(key))
}

/// `shouldPersistProviderEvent`: drop token deltas and other transient frames.
pub fn should_persist_provider_event(stream: EventNdjsonStream, event: &Value) -> bool {
    if stream == EventNdjsonStream::Orchestration || !event.is_object() {
        return true;
    }
    if let Some(Value::String(event_type)) = get(event, "type") {
        if TRANSIENT_CANONICAL_EVENT_TYPES.contains(&event_type.as_str()) {
            return false;
        }
    }
    if stream != EventNdjsonStream::Native {
        return true;
    }
    let envelope = match get(event, "event") {
        Some(nested) if nested.is_object() => nested,
        _ => event,
    };
    if get(envelope, "stage").and_then(Value::as_str) == Some("raw") {
        return false;
    }
    let native_event = match get(envelope, "payload") {
        Some(payload) if get(envelope, "stage").and_then(Value::as_str) == Some("decoded") && payload.is_object() => payload,
        _ => envelope,
    };
    let method = get(native_event, "method").and_then(Value::as_str);
    if let Some(method) = method {
        if TRANSIENT_NATIVE_METHODS.contains(&method) || method.starts_with("claude/stream_event/content_block_delta/") {
            return false;
        }
    }
    let native_type = get(native_event, "type").and_then(Value::as_str);
    if native_type == Some("message.part.delta") {
        return false;
    }
    if native_type == Some("stream_event") {
        if let Some(stream_event) = get(native_event, "event") {
            if stream_event.is_object() && get(stream_event, "type").and_then(Value::as_str) == Some("content_block_delta") {
                return false;
            }
        }
    }
    let Some(payload) = get(native_event, "payload").filter(|payload| payload.is_object()) else {
        return true;
    };
    if method == Some("session/update") {
        let Some(update) = get(payload, "update").filter(|update| update.is_object()) else {
            return true;
        };
        return match get(update, "sessionUpdate") {
            Some(Value::String(update_type)) => !TRANSIENT_ACP_UPDATES.contains(&update_type.as_str()),
            _ => true,
        };
    }
    if native_type == Some("message.part.updated") {
        let Some(properties) = get(payload, "properties").filter(|value| value.is_object()) else {
            return true;
        };
        let Some(part) = get(properties, "part").filter(|value| value.is_object()) else {
            return true;
        };
        let part_type = get(part, "type");
        if part_type == Some(&Value::String("text".into())) || part_type == Some(&Value::String("reasoning".into())) {
            return false;
        }
        if part_type == Some(&Value::String("tool".into())) {
            if let Some(state) = get(part, "state").filter(|value| value.is_object()) {
                return get(state, "status") != Some(&Value::String("running".into()));
            }
        }
        return true;
    }
    true
}

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

/// `summarizeProviderEvent`: routing and error fields only, strings capped.
pub fn summarize_provider_event(event: &Value) -> Value {
    struct Budget {
        fields: i64,
        characters: i64,
    }
    fn summarize(value: &Value, depth: usize, budget: &mut Budget) -> Value {
        match value {
            Value::String(text) => {
                let length = js_length(text) as i64;
                if length > 1_024.min(budget.characters) {
                    let mut omitted = Map::new();
                    omitted.insert("omittedCharacters".into(), Value::from(length));
                    return Value::Object(omitted);
                }
                budget.characters -= length;
                value.clone()
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => value.clone(),
            Value::Array(items) => {
                let mut summary = Map::new();
                summary.insert("itemCount".into(), Value::from(items.len()));
                Value::Object(summary)
            }
            Value::Object(object) => {
                let mut summary = Map::new();
                summary.insert("truncated".into(), Value::Bool(true));
                if depth >= 6 {
                    return Value::Object(summary);
                }
                for key in SUMMARY_FIELDS {
                    if budget.fields <= 0 {
                        break;
                    }
                    let Some(nested) = object.get(*key) else {
                        continue;
                    };
                    budget.fields -= 1;
                    let summarized = summarize(nested, depth + 1, budget);
                    summary.insert((*key).to_owned(), summarized);
                }
                Value::Object(summary)
            }
        }
    }
    summarize(
        event,
        0,
        &mut Budget {
            fields: 128,
            characters: 8 * 1024,
        },
    )
}

/// `boundProviderEventForLogging`: the event itself when it fits the record budget, otherwise
/// its summary.
pub fn bound_provider_event_for_logging(event: &Value) -> std::borrow::Cow<'_, Value> {
    struct Budget {
        fields: i64,
        characters: i64,
    }
    fn fits(value: &Value, depth: usize, budget: &mut Budget) -> bool {
        match value {
            Value::String(text) => {
                budget.characters -= js_length(text) as i64;
                budget.characters >= 0
            }
            Value::Null | Value::Bool(_) | Value::Number(_) => true,
            Value::Array(items) => {
                if depth > MAX_RECORD_DEPTH || items.len() as i64 > budget.fields {
                    return false;
                }
                for (index, item) in items.iter().enumerate() {
                    budget.fields -= 1;
                    budget.characters -= index.to_string().len() as i64;
                    if budget.fields < 0 || budget.characters < 0 || !fits(item, depth + 1, budget) {
                        return false;
                    }
                }
                true
            }
            Value::Object(object) => {
                if depth > MAX_RECORD_DEPTH {
                    return false;
                }
                for (key, item) in object {
                    budget.fields -= 1;
                    budget.characters -= js_length(key) as i64;
                    if budget.fields < 0 || budget.characters < 0 || !fits(item, depth + 1, budget) {
                        return false;
                    }
                }
                true
            }
        }
    }
    let mut budget = Budget {
        fields: MAX_RECORD_FIELDS,
        characters: MAX_RECORD_CHARACTERS,
    };
    if fits(event, 0, &mut budget) {
        std::borrow::Cow::Borrowed(event)
    } else {
        std::borrow::Cow::Owned(summarize_provider_event(event))
    }
}

/// The `JSON.stringify` payload of one record (bounded, then summarized when escaping pushed it
/// over the byte budget).
pub fn serialize_record(event: &Value) -> String {
    let payload = js_json::stringify(&bound_provider_event_for_logging(event));
    if payload.len() as i64 > MAX_RECORD_CHARACTERS {
        return js_json::stringify(&summarize_provider_event(event));
    }
    payload
}

fn resolve_thread_segment(raw: Option<&str>) -> String {
    raw.and_then(to_safe_thread_attachment_segment)
        .unwrap_or_else(|| GLOBAL_THREAD_SEGMENT.to_owned())
}

fn provider_log_prefix(file_path: &Path) -> String {
    let base_name = file_path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let stem = match base_name.rfind('.') {
        Some(index) if index > 0 => base_name[..index].to_owned(),
        _ => base_name,
    };
    format!("{stem}.")
}

fn provider_log_path(directory: &Path, prefix: &str, thread_segment: &str) -> PathBuf {
    directory.join(format!("{prefix}{thread_segment}.log"))
}

// ---------------------------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------------------------

fn is_provider_log_file(file_path: &Path, file_name: &str, file_prefix: &str) -> std::io::Result<bool> {
    let log_name = regex_lite_is_log_name(file_name);
    if !log_name {
        return Ok(false);
    }
    if file_name.starts_with(file_prefix) {
        return Ok(true);
    }
    let mut header = [0u8; 256];
    let mut file = fs::File::open(file_path)?;
    let read = file.read(&mut header)?;
    let text = String::from_utf8_lossy(&header[..read]);
    Ok(header_looks_like_provider_log(&text))
}

/// `/\.log(?:\.\d+)?$/`.
fn regex_lite_is_log_name(file_name: &str) -> bool {
    if file_name.ends_with(".log") {
        return true;
    }
    match file_name.rfind('.') {
        Some(index) => {
            let digits = &file_name[index + 1..];
            !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) && file_name[..index].ends_with(".log")
        }
        None => false,
    }
}

/// `/^\[[^\]\r\n]+\] (?:NTIVE|CANON|ORCH): /`.
fn header_looks_like_provider_log(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('[') else {
        return false;
    };
    let Some(close) = rest.find(']') else {
        return false;
    };
    let stamp = &rest[..close];
    if stamp.is_empty() || stamp.contains('\r') || stamp.contains('\n') {
        return false;
    }
    let after = &rest[close + 1..];
    ["NTIVE", "CANON", "ORCH"].iter().any(|label| after.starts_with(&format!(" {label}: ")))
}

struct RetentionFile {
    file_path: PathBuf,
    mtime_ms: f64,
    size: u64,
}

fn mtime_ms(metadata: &fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_secs_f64() * 1000.0)
        .unwrap_or(0.0)
}

fn enforce_retention(
    directory: &Path,
    max_total_bytes: u64,
    max_age_ms: i64,
    active: &HashSet<PathBuf>,
    file_prefix: &str,
    now: i64,
) -> Vec<(PathBuf, String)> {
    let mut failures = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => return vec![(directory.to_path_buf(), error.to_string())],
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let file_path = entry.path();
        let file_name = entry.file_name().to_string_lossy().into_owned();
        match is_provider_log_file(&file_path, &file_name, file_prefix).and_then(|is_log| Ok((is_log, fs::metadata(&file_path)?))) {
            Ok((true, metadata)) => files.push(RetentionFile {
                file_path,
                mtime_ms: mtime_ms(&metadata),
                size: metadata.len(),
            }),
            Ok((false, _)) => {}
            Err(error) => failures.push((file_path, error.to_string())),
        }
    }
    let mut total_bytes: u64 = files.iter().map(|file| file.size).sum();
    let remove = |file: &RetentionFile, total: &mut u64, failures: &mut Vec<(PathBuf, String)>| -> bool {
        if active.contains(&file.file_path) {
            return false;
        }
        match fs::remove_file(&file.file_path) {
            Ok(()) => {
                *total = total.saturating_sub(file.size);
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                *total = total.saturating_sub(file.size);
                true
            }
            Err(error) => {
                failures.push((file.file_path.clone(), error.to_string()));
                false
            }
        }
    };
    let mut retained = Vec::new();
    for file in files {
        if now as f64 - file.mtime_ms <= max_age_ms as f64 || !remove(&file, &mut total_bytes, &mut failures) {
            retained.push(file);
        }
    }
    retained.sort_by(|left, right| left.mtime_ms.total_cmp(&right.mtime_ms).then_with(|| left.file_path.cmp(&right.file_path)));
    for file in &retained {
        if total_bytes <= max_total_bytes {
            break;
        }
        remove(file, &mut total_bytes, &mut failures);
    }
    failures
}

// ---------------------------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------------------------

/// One buffered line.
#[derive(Debug, Clone)]
pub struct PendingRecord {
    pub stream: EventNdjsonStream,
    pub thread_segment: String,
    pub line: String,
    pub bytes: u64,
}

/// `writeBatchedMessages`: join records into writes of at most `max_bytes` (a single larger
/// record is written alone).
pub fn write_batched_messages(
    sink: &mut RotatingFileSink,
    records: &[PendingRecord],
    max_bytes: u64,
    mut on_written: impl FnMut(&[PendingRecord]),
) -> std::io::Result<()> {
    let mut start = 0;
    let mut pending_bytes = 0u64;
    let mut flush = |start: &mut usize, end: usize, pending_bytes: &mut u64, sink: &mut RotatingFileSink| -> std::io::Result<()> {
        if end == *start {
            return Ok(());
        }
        let chunk: String = records[*start..end].iter().map(|record| record.line.as_str()).collect();
        sink.write(chunk.as_bytes())?;
        on_written(&records[*start..end]);
        *start = end;
        *pending_bytes = 0;
        Ok(())
    };
    for (index, record) in records.iter().enumerate() {
        if pending_bytes > 0 && pending_bytes + record.bytes > max_bytes {
            flush(&mut start, index, &mut pending_bytes, sink)?;
        }
        pending_bytes += record.bytes;
        if pending_bytes >= max_bytes {
            flush(&mut start, index + 1, &mut pending_bytes, sink)?;
        }
    }
    flush(&mut start, records.len(), &mut pending_bytes, sink)
}

struct StoreState {
    pending: Vec<PendingRecord>,
    pending_bytes: u64,
    sinks: HashMap<String, RotatingFileSink>,
    flush_scheduled: bool,
    closed: bool,
    last_retention_at: i64,
    timer: Option<tokio::task::JoinHandle<()>>,
}

struct StoreInner {
    file_path: PathBuf,
    directory: PathBuf,
    file_prefix: String,
    options: EventNdjsonLogStoreOptions,
    state: Mutex<StoreState>,
}

/// `EventNdjsonLogStore`: the shared writer behind every stream view.
#[derive(Clone)]
pub struct EventNdjsonLogStore {
    inner: Arc<StoreInner>,
}

impl std::fmt::Debug for EventNdjsonLogStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventNdjsonLogStore").field("file_path", &self.inner.file_path).finish()
    }
}

fn validate(file_path: &Path, option: &'static str, value: i64, minimum: i64) -> Result<(), EventNdjsonLogStoreError> {
    if value >= minimum {
        return Ok(());
    }
    Err(EventNdjsonLogStoreError::Configuration {
        file_path: file_path.to_string_lossy().into_owned(),
        option,
        value,
        minimum,
    })
}

impl EventNdjsonLogStore {
    /// `makeEventNdjsonLogStore(filePath, options)`: validate, create the directory, run the
    /// startup retention pass.
    pub fn open(file_path: impl Into<PathBuf>, options: EventNdjsonLogStoreOptions) -> Result<Self, EventNdjsonLogStoreError> {
        let file_path = file_path.into();
        validate(&file_path, "maxBytes", options.max_bytes as i64, 1)?;
        validate(&file_path, "maxFiles", options.max_files as i64, 1)?;
        validate(&file_path, "batchWindowMs", options.batch_window_ms as i64, 0)?;
        validate(&file_path, "maxTotalBytes", options.max_total_bytes as i64, 1)?;
        validate(&file_path, "maxAgeMs", options.max_age_ms, 1)?;
        validate(&file_path, "retentionCheckIntervalMs", options.retention_check_interval_ms, 1)?;
        validate(&file_path, "maxBufferedBytes", options.max_buffered_bytes as i64, 1)?;
        validate(&file_path, "maxBufferedRecords", options.max_buffered_records as i64, 1)?;
        let directory = file_path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        fs::create_dir_all(&directory).map_err(|cause| EventNdjsonLogStoreError::Directory {
            directory: directory.to_string_lossy().into_owned(),
            cause,
        })?;
        let file_prefix = provider_log_prefix(&file_path);
        let initialized_at = (options.clock)();
        for (path, error) in enforce_retention(
            &directory,
            options.max_total_bytes,
            options.max_age_ms,
            &HashSet::new(),
            &file_prefix,
            initialized_at,
        ) {
            tracing::warn!(scope = "provider-observability", file_path = %path.display(), %error, "provider event log retention failed");
        }
        Ok(Self {
            inner: Arc::new(StoreInner {
                file_path,
                directory,
                file_prefix,
                options,
                state: Mutex::new(StoreState {
                    pending: Vec::new(),
                    pending_bytes: 0,
                    sinks: HashMap::new(),
                    flush_scheduled: false,
                    closed: false,
                    last_retention_at: initialized_at,
                    timer: None,
                }),
            }),
        })
    }

    pub fn file_path(&self) -> &Path {
        &self.inner.file_path
    }

    /// One stream view over this store (views never own the store: closing a view is a no-op).
    pub fn logger(&self, stream: EventNdjsonStream) -> EventNdjsonLogger {
        EventNdjsonLogger {
            inner: self.inner.clone(),
            stream,
        }
    }

    /// Flush what is buffered, then stop accepting records.
    pub fn close(&self) {
        StoreInner::flush(&self.inner, false, true);
        if let Some(timer) = self.inner.state.lock().unwrap().timer.take() {
            timer.abort();
        }
    }

    /// Flush what is buffered now (tests, shutdown hooks).
    pub fn flush(&self) {
        StoreInner::flush(&self.inner, false, false);
    }
}

impl StoreInner {
    fn flush(inner: &Arc<StoreInner>, timer_fired: bool, close: bool) {
        let started_at = (inner.options.clock)();
        let mut failures = Vec::new();
        let mut attributions: Vec<(EventNdjsonStream, u64, u64)> = Vec::new();
        {
            let mut state = inner.state.lock().unwrap();
            if state.closed {
                return;
            }
            let pending = std::mem::take(&mut state.pending);
            state.pending_bytes = 0;
            let mut order: Vec<String> = Vec::new();
            let mut by_segment: HashMap<String, Vec<PendingRecord>> = HashMap::new();
            for record in pending {
                if !by_segment.contains_key(&record.thread_segment) {
                    order.push(record.thread_segment.clone());
                }
                by_segment.entry(record.thread_segment.clone()).or_default().push(record);
            }
            for segment in order {
                let records = by_segment.remove(&segment).unwrap_or_default();
                let file_path = provider_log_path(&inner.directory, &inner.file_prefix, &segment);
                if !state.sinks.contains_key(&segment) {
                    match RotatingFileSink::new(&file_path, inner.options.max_bytes, inner.options.max_files) {
                        Ok(sink) => {
                            state.sinks.insert(segment.clone(), sink);
                        }
                        Err(error) => {
                            failures.push((file_path, error.to_string()));
                            continue;
                        }
                    }
                }
                let sink = state.sinks.get_mut(&segment).expect("sink just inserted");
                let result = write_batched_messages(sink, &records, inner.options.max_bytes, |written| {
                    for record in written {
                        match attributions.iter_mut().find(|(stream, _, _)| *stream == record.stream) {
                            Some(entry) => {
                                entry.1 += 1;
                                entry.2 += record.bytes;
                            }
                            None => attributions.push((record.stream, 1, record.bytes)),
                        }
                    }
                });
                if let Err(error) = result {
                    state.sinks.remove(&segment);
                    failures.push((file_path, error.to_string()));
                }
            }
            let retention_due = started_at - state.last_retention_at >= inner.options.retention_check_interval_ms;
            if retention_due {
                let active: HashSet<PathBuf> = state
                    .sinks
                    .keys()
                    .map(|segment| provider_log_path(&inner.directory, &inner.file_prefix, segment))
                    .collect();
                failures.extend(enforce_retention(
                    &inner.directory,
                    inner.options.max_total_bytes,
                    inner.options.max_age_ms,
                    &active,
                    &inner.file_prefix,
                    started_at,
                ));
                state.last_retention_at = started_at;
            }
            if timer_fired {
                state.flush_scheduled = false;
                state.timer = None;
            }
            if close {
                state.closed = true;
            }
        }
        for (path, error) in failures {
            tracing::warn!(scope = "provider-observability", file_path = %path.display(), %error, "provider event log write or retention failed");
        }
        if let Some(attribution) = &inner.options.attribution {
            if !attributions.is_empty() {
                let duration_ms = ((inner.options.clock)() - started_at).max(0);
                let total_bytes: u64 = attributions.iter().map(|(_, _, bytes)| *bytes).sum();
                for (stream, count, bytes) in attributions {
                    let share = if total_bytes == 0 {
                        0
                    } else {
                        (duration_ms as f64 * (bytes as f64 / total_bytes as f64)).round() as i64
                    };
                    attribution.record("provider-event-log", &format!("{}.append", stream.as_str()), bytes, count, share);
                }
            }
        }
    }

    fn schedule_flush(inner: &Arc<StoreInner>, state: &mut StoreState) -> bool {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return false;
        };
        let weak: Weak<StoreInner> = Arc::downgrade(inner);
        let window = Duration::from_millis(inner.options.batch_window_ms);
        state.timer = Some(handle.spawn(async move {
            tokio::time::sleep(window).await;
            if let Some(inner) = weak.upgrade() {
                StoreInner::flush(&inner, true, false);
            }
        }));
        true
    }
}

/// `EventNdjsonLogger`: one stream view (`native`, `canonical`, `orchestration`).
#[derive(Clone)]
pub struct EventNdjsonLogger {
    inner: Arc<StoreInner>,
    stream: EventNdjsonStream,
}

impl std::fmt::Debug for EventNdjsonLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventNdjsonLogger")
            .field("file_path", &self.inner.file_path)
            .field("stream", &self.stream)
            .finish()
    }
}

impl EventNdjsonLogger {
    pub fn file_path(&self) -> &Path {
        &self.inner.file_path
    }

    pub fn stream(&self) -> EventNdjsonStream {
        self.stream
    }

    /// `write(event, threadId)`. Never fails: observability must not break provider work.
    pub fn write(&self, event: &Value, thread_id: Option<&str>) {
        if !should_persist_provider_event(self.stream, event) {
            return;
        }
        let payload = serialize_record(event);
        let observed_at = zc_core::iso_from_millis((self.inner.options.clock)());
        let line = format!("[{observed_at}] {}: {payload}\n", self.stream.label());
        let bytes = line.len() as u64;
        let flush_now = {
            let mut state = self.inner.state.lock().unwrap();
            if state.closed {
                return;
            }
            state.pending.push(PendingRecord {
                stream: self.stream,
                thread_segment: resolve_thread_segment(thread_id),
                line,
                bytes,
            });
            state.pending_bytes += bytes;
            let flush = self.inner.options.batch_window_ms == 0
                || state.pending.len() as u64 >= self.inner.options.max_buffered_records
                || state.pending_bytes >= self.inner.options.max_buffered_bytes;
            if !flush && !state.flush_scheduled {
                if StoreInner::schedule_flush(&self.inner, &mut state) {
                    state.flush_scheduled = true;
                    false
                } else {
                    // No runtime to run the batch timer on: write through.
                    true
                }
            } else {
                flush
            }
        };
        if flush_now {
            StoreInner::flush(&self.inner, false, false);
        }
    }

    /// [`Self::write`] for anything serializable (runtime events, native frames).
    pub fn write_serializable<T: serde::Serialize>(&self, event: &T, thread_id: Option<&str>) {
        match serde_json::to_value(event) {
            Ok(value) => self.write(&value, thread_id),
            Err(error) => tracing::warn!(scope = "provider-observability", %error, "failed to serialize provider event log record"),
        }
    }
}

/// `ProviderEventLoggers`: the `native` and `canonical` views over one store, or none when the
/// store could not be opened (observability never blocks startup).
#[derive(Clone, Debug, Default)]
pub struct ProviderEventLoggers {
    pub native: Option<EventNdjsonLogger>,
    pub canonical: Option<EventNdjsonLogger>,
    store: Option<EventNdjsonLogStore>,
}

impl ProviderEventLoggers {
    /// `NoOpProviderEventLoggers`.
    pub fn none() -> Self {
        Self::default()
    }

    /// `ProviderEventLoggers.make`: one store at `provider_event_log_path`
    /// (`<state>/logs/provider/events.log`).
    pub fn open(provider_event_log_path: &Path, options: EventNdjsonLogStoreOptions) -> Self {
        match EventNdjsonLogStore::open(provider_event_log_path, options) {
            Ok(store) => Self {
                native: Some(store.logger(EventNdjsonStream::Native)),
                canonical: Some(store.logger(EventNdjsonStream::Canonical)),
                store: Some(store),
            },
            Err(error) => {
                tracing::warn!(scope = "provider-observability", %error, "provider event logs disabled");
                Self::none()
            }
        }
    }

    pub fn store(&self) -> Option<&EventNdjsonLogStore> {
        self.store.as_ref()
    }

    /// Flush and close the shared store (server shutdown).
    pub fn close(&self) {
        if let Some(store) = &self.store {
            store.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn log_names_and_headers() {
        assert!(regex_lite_is_log_name("events.thread-1.log"));
        assert!(regex_lite_is_log_name("events.thread-1.log.3"));
        assert!(!regex_lite_is_log_name("events.thread-1.log.x"));
        assert!(!regex_lite_is_log_name("notes.txt"));
        assert!(header_looks_like_provider_log("[2026-01-01T00:00:00.000Z] CANON: {}"));
        assert!(!header_looks_like_provider_log("[x] OTHER: {}"));
        assert_eq!(provider_log_prefix(Path::new("/a/events.log")), "events.");
        assert_eq!(provider_log_prefix(Path::new("/a/events")), "events.");
    }

    #[test]
    fn transient_events_are_dropped() {
        assert!(!should_persist_provider_event(EventNdjsonStream::Canonical, &json!({"type": "content.delta"})));
        assert!(should_persist_provider_event(EventNdjsonStream::Canonical, &json!({"type": "turn.completed"})));
        assert!(!should_persist_provider_event(EventNdjsonStream::Native, &json!({"event": {"stage": "raw"}})));
        assert!(!should_persist_provider_event(
            EventNdjsonStream::Native,
            &json!({"method": "session/update", "payload": {"update": {"sessionUpdate": "agent_message_chunk"}}})
        ));
        assert!(should_persist_provider_event(
            EventNdjsonStream::Orchestration,
            &json!({"type": "content.delta"})
        ));
    }

    #[test]
    fn oversized_records_are_summarized() {
        let event = json!({"type": "turn.completed", "threadId": "t", "payload": {"text": "x".repeat(70_000)}});
        let payload = serialize_record(&event);
        assert_eq!(
            payload,
            r#"{"truncated":true,"type":"turn.completed","threadId":"t","payload":{"truncated":true}}"#
        );
    }
}
