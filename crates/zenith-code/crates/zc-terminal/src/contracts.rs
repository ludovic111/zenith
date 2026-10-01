//! The terminal wire types of `code/packages/contracts/src/terminal.ts`, written by hand until
//! `zc-contracts` (WP-01) generates them.
//!
//! **Swapping in the generated types.** Every type here is named after its contract schema and
//! (de)serializes to the same JSON as `Schema.toCodecJson` (plan §1.5):
//!
//! - inputs decode like the schema decodes: `TrimmedNonEmptyString` fields are trimmed (with
//!   JavaScript's definition of white space) and must not be empty, lengths are counted in
//!   UTF-16 code units, `Schema.Int` accepts any integral JSON number, unknown keys are ignored,
//!   and `Schema.optional` keys accept `null` as absent;
//! - `Schema.optional(Schema.NullOr(…))` (`worktreePath`) keeps three states: absent, `null`,
//!   value (`Option<Option<String>>`);
//! - outputs always write `Schema.NullOr` keys and omit absent `Schema.optional` keys;
//! - errors are tagged with `_tag` and carry only their declared fields; [`TerminalError::message`]
//!   is the TS `message` getter.
//!
//! When `zc-contracts` lands, replace the types with `pub use zc_contracts::…` one by one; the
//! fixtures in `tests/contracts.rs` (checked against the TS schemas) say what must not change.

use std::collections::BTreeMap;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{Map, Value};
use zc_core::defect::js_length;
use zc_core::Defect;

/// `DEFAULT_TERMINAL_ID`: the id of the first shell of a thread.
pub const DEFAULT_TERMINAL_ID: &str = "term-1";

const TERMINAL_ID_MAX: usize = 128;
const COLS_MAX: i64 = 1000;
const ROWS_MAX: i64 = 500;
const ENV_KEY_MAX: usize = 128;
const ENV_VALUE_MAX: usize = 8_192;
const ENV_MAX_PROPERTIES: usize = 128;
const WRITE_DATA_MAX: usize = 65_536;
const PROVIDER_SLUG_MAX: usize = 64;

// ---------------------------------------------------------------------------------------------
// Inputs
// ---------------------------------------------------------------------------------------------

/// `Schema.optional(Schema.NullOr(X))`: absent (`None`), `null` (`Some(None)`) or a value.
pub type NullableField<T> = Option<Option<T>>;

/// `TerminalOpenInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalOpenInput {
    pub thread_id: String,
    pub terminal_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: NullableField<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_instance_id: Option<String>,
}

impl TerminalOpenInput {
    /// The minimal input (no size, no env).
    pub fn new(thread_id: impl Into<String>, terminal_id: impl Into<String>, cwd: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            terminal_id: terminal_id.into(),
            cwd: cwd.into(),
            worktree_path: None,
            cols: None,
            rows: None,
            env: None,
            provider_instance_id: None,
        }
    }
}

/// `TerminalAttachInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalAttachInput {
    pub thread_id: String,
    pub terminal_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: NullableField<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_if_not_running: Option<bool>,
}

impl TerminalAttachInput {
    pub fn new(thread_id: impl Into<String>, terminal_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            terminal_id: terminal_id.into(),
            cwd: None,
            worktree_path: None,
            cols: None,
            rows: None,
            env: None,
            provider_instance_id: None,
            restart_if_not_running: None,
        }
    }
}

impl From<TerminalOpenInput> for TerminalAttachInput {
    fn from(input: TerminalOpenInput) -> Self {
        Self {
            thread_id: input.thread_id,
            terminal_id: input.terminal_id,
            cwd: Some(input.cwd),
            worktree_path: input.worktree_path,
            cols: input.cols,
            rows: input.rows,
            env: input.env,
            provider_instance_id: input.provider_instance_id,
            restart_if_not_running: None,
        }
    }
}

/// `TerminalWriteInput`: `data` is 1 to 65,536 UTF-16 code units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalWriteInput {
    pub thread_id: String,
    pub terminal_id: String,
    pub data: String,
}

/// `TerminalResizeInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalResizeInput {
    pub thread_id: String,
    pub terminal_id: String,
    pub cols: u16,
    pub rows: u16,
}

/// `TerminalClearInput` (= `TerminalSessionInput`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalClearInput {
    pub thread_id: String,
    pub terminal_id: String,
}

/// `TerminalRestartInput`: like open, but the size is required.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalRestartInput {
    pub thread_id: String,
    pub terminal_id: String,
    pub cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_path: NullableField<String>,
    pub cols: u16,
    pub rows: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_instance_id: Option<String>,
}

/// `TerminalCloseInput`: one terminal, or every terminal of the thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalCloseInput {
    pub thread_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_history: Option<bool>,
}

/// The `{}` payload of `subscribeTerminalEvents` / `subscribeTerminalMetadata`
/// (`Schema.Struct({})`: any object, extra keys ignored).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct EmptyInput {}

// ---------------------------------------------------------------------------------------------
// Outputs
// ---------------------------------------------------------------------------------------------

/// `TerminalSessionStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalSessionStatus {
    Starting,
    Running,
    Exited,
    Error,
}

/// `TerminalSessionSnapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSessionSnapshot {
    pub thread_id: String,
    pub terminal_id: String,
    pub cwd: String,
    pub worktree_path: Option<String>,
    pub status: TerminalSessionStatus,
    pub pid: Option<u32>,
    pub history: String,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    /// Server-computed display title (idle shell vs subprocess command), ≤ 128 code units.
    pub label: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
}

/// `TerminalSummary`: a snapshot without the history, plus the subprocess flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSummary {
    pub thread_id: String,
    pub terminal_id: String,
    pub cwd: String,
    pub worktree_path: Option<String>,
    pub status: TerminalSessionStatus,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub exit_signal: Option<i32>,
    pub has_running_subprocess: bool,
    pub label: String,
    pub updated_at: String,
}

/// `TerminalEvent`: `{threadId, terminalId, sequence?, type, …}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEvent {
    pub thread_id: String,
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(flatten)]
    pub kind: TerminalEventKind,
}

/// The `type`-tagged part of a [`TerminalEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum TerminalEventKind {
    Started { snapshot: Box<TerminalSessionSnapshot> },
    Output { data: String },
    Exited { exit_code: Option<i32>, exit_signal: Option<i32> },
    Closed,
    Error { message: String },
    Cleared,
    Restarted { snapshot: Box<TerminalSessionSnapshot> },
    Activity { has_running_subprocess: bool, label: String },
}

impl TerminalEventKind {
    /// The wire `type`.
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Started { .. } => "started",
            Self::Output { .. } => "output",
            Self::Exited { .. } => "exited",
            Self::Closed => "closed",
            Self::Error { .. } => "error",
            Self::Cleared => "cleared",
            Self::Restarted { .. } => "restarted",
            Self::Activity { .. } => "activity",
        }
    }
}

/// `TerminalAttachStreamEvent`: `{type: "snapshot", snapshot}` first, then the terminal's
/// events (a later `started` becomes a `snapshot` too).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TerminalAttachStreamEvent {
    Snapshot(Box<TerminalSessionSnapshot>),
    /// Any [`TerminalEvent`] but `started`.
    Event(TerminalEvent),
}

impl Serialize for TerminalAttachStreamEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Snapshot(snapshot) => {
                #[derive(Serialize)]
                struct Wire<'a> {
                    #[serde(rename = "type")]
                    kind: &'static str,
                    snapshot: &'a TerminalSessionSnapshot,
                }
                Wire { kind: "snapshot", snapshot }.serialize(serializer)
            }
            Self::Event(event) => event.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for TerminalAttachStreamEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.get("type").and_then(Value::as_str) == Some("snapshot") {
            let snapshot = value.get("snapshot").cloned().ok_or_else(|| de::Error::missing_field("snapshot"))?;
            return serde_json::from_value(snapshot)
                .map(|snapshot| Self::Snapshot(Box::new(snapshot)))
                .map_err(de::Error::custom);
        }
        let event: TerminalEvent = serde_json::from_value(value).map_err(de::Error::custom)?;
        if matches!(event.kind, TerminalEventKind::Started { .. }) {
            return Err(de::Error::custom("`started` is not a TerminalAttachStreamEvent"));
        }
        Ok(Self::Event(event))
    }
}

/// `TerminalMetadataStreamEvent`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum TerminalMetadataStreamEvent {
    Snapshot { terminals: Vec<TerminalSummary> },
    Upsert { terminal: TerminalSummary },
    Remove { thread_id: String, terminal_id: String },
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// `TerminalHistoryError.operation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TerminalHistoryOperation {
    Read,
    Truncate,
    Migrate,
}

impl TerminalHistoryOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Truncate => "truncate",
            Self::Migrate => "migrate",
        }
    }
}

/// `TerminalError`: the union every terminal method fails with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "_tag", rename_all_fields = "camelCase")]
pub enum TerminalError {
    TerminalCwdNotFoundError {
        cwd: String,
    },
    TerminalCwdNotDirectoryError {
        cwd: String,
    },
    TerminalCwdStatError {
        cwd: String,
        cause: Defect,
    },
    TerminalHistoryError {
        operation: TerminalHistoryOperation,
        thread_id: String,
        terminal_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<Defect>,
    },
    TerminalSessionLookupError {
        thread_id: String,
        terminal_id: String,
    },
    TerminalProviderInstanceNotFoundError {
        provider_instance_id: String,
    },
    TerminalProviderEnvironmentError {
        provider_instance_id: String,
        cause: Defect,
    },
    TerminalNotRunningError {
        thread_id: String,
        terminal_id: String,
    },
    TerminalWriteError {
        thread_id: String,
        terminal_id: String,
        terminal_pid: u32,
        cause: Defect,
    },
    TerminalResizeError {
        thread_id: String,
        terminal_id: String,
        terminal_pid: u32,
        cols: u16,
        rows: u16,
        cause: Defect,
    },
}

impl TerminalError {
    /// The `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::TerminalCwdNotFoundError { .. } => "TerminalCwdNotFoundError",
            Self::TerminalCwdNotDirectoryError { .. } => "TerminalCwdNotDirectoryError",
            Self::TerminalCwdStatError { .. } => "TerminalCwdStatError",
            Self::TerminalHistoryError { .. } => "TerminalHistoryError",
            Self::TerminalSessionLookupError { .. } => "TerminalSessionLookupError",
            Self::TerminalProviderInstanceNotFoundError { .. } => "TerminalProviderInstanceNotFoundError",
            Self::TerminalProviderEnvironmentError { .. } => "TerminalProviderEnvironmentError",
            Self::TerminalNotRunningError { .. } => "TerminalNotRunningError",
            Self::TerminalWriteError { .. } => "TerminalWriteError",
            Self::TerminalResizeError { .. } => "TerminalResizeError",
        }
    }

    /// The TS `message` getter.
    pub fn message(&self) -> String {
        match self {
            Self::TerminalCwdNotFoundError { cwd } => format!("Terminal cwd does not exist: {cwd}"),
            Self::TerminalCwdNotDirectoryError { cwd } => {
                format!("Terminal cwd is not a directory: {cwd}")
            }
            Self::TerminalCwdStatError { cwd, .. } => {
                format!("Failed to access terminal cwd: {cwd}")
            }
            Self::TerminalHistoryError {
                operation,
                thread_id,
                terminal_id,
                ..
            } => format!(
                "Failed to {} terminal history for thread: {thread_id}, terminal: {terminal_id}",
                operation.as_str()
            ),
            Self::TerminalSessionLookupError { thread_id, terminal_id } => format!("Unknown terminal thread: {thread_id}, terminal: {terminal_id}"),
            Self::TerminalProviderInstanceNotFoundError { provider_instance_id } => format!("Provider instance is not available: {provider_instance_id}"),
            Self::TerminalProviderEnvironmentError { provider_instance_id, .. } => {
                format!("Could not prepare the terminal environment for provider instance: {provider_instance_id}")
            }
            Self::TerminalNotRunningError { thread_id, terminal_id } => format!("Terminal is not running for thread: {thread_id}, terminal: {terminal_id}"),
            Self::TerminalWriteError {
                thread_id,
                terminal_id,
                terminal_pid,
                ..
            } => format!("Failed to write to terminal for thread: {thread_id}, terminal: {terminal_id}, PID: {terminal_pid}"),
            Self::TerminalResizeError {
                thread_id,
                terminal_id,
                terminal_pid,
                cols,
                rows,
                ..
            } => format!("Failed to resize terminal for thread: {thread_id}, terminal: {terminal_id}, PID: {terminal_pid} to {cols}x{rows}"),
        }
    }

    /// The port's error shape (`zc_ports::TaggedError`): the same JSON plus the message.
    pub fn to_tagged(&self) -> zc_ports::TaggedError {
        let mut fields = match serde_json::to_value(self) {
            Ok(Value::Object(map)) => map,
            _ => Map::new(),
        };
        fields.remove("_tag");
        zc_ports::TaggedError {
            tag: self.tag().to_owned(),
            fields,
            message: self.message(),
        }
    }

    /// Back from the port's error shape. `None` when the tag is not a terminal error.
    pub fn from_tagged(error: &zc_ports::TaggedError) -> Option<Self> {
        let mut map = error.fields.clone();
        map.insert("_tag".into(), Value::String(error.tag.clone()));
        serde_json::from_value(Value::Object(map)).ok()
    }
}

impl std::fmt::Display for TerminalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for TerminalError {}

impl From<TerminalError> for zc_ports::TaggedError {
    fn from(error: TerminalError) -> Self {
        error.to_tagged()
    }
}

// ---------------------------------------------------------------------------------------------
// Decoding (the schema checks)
// ---------------------------------------------------------------------------------------------

/// JavaScript's `String.prototype.trim` white space: `WhiteSpace` + `LineTerminator`
/// (unlike Rust's `trim`, it includes U+FEFF and excludes U+0085).
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{FEFF}'
            | '\u{000A}'
            | '\u{000D}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

struct Fields {
    schema: &'static str,
    map: Map<String, Value>,
}

impl Fields {
    fn new<'de, D: Deserializer<'de>>(deserializer: D, schema: &'static str) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Object(map) => Ok(Self { schema, map }),
            other => Err(de::Error::custom(format!("{schema}: expected an object, got {other}"))),
        }
    }

    fn fail<E: de::Error>(&self, key: &str, message: impl std::fmt::Display) -> E {
        E::custom(format!("{}: {message} at [\"{key}\"]", self.schema))
    }

    /// `Schema.optional` keys: absent or `null` are both "not given".
    fn optional(&self, key: &str) -> Option<&Value> {
        self.map.get(key).filter(|value| !value.is_null())
    }

    fn required<E: de::Error>(&self, key: &str) -> Result<&Value, E> {
        self.map.get(key).ok_or_else(|| self.fail(key, "Missing key"))
    }

    fn string<E: de::Error>(&self, key: &str, value: &Value) -> Result<String, E> {
        value
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| self.fail(key, format!("Expected string, got {value}")))
    }

    /// `TrimmedNonEmptyString` (+ an optional max length).
    fn trimmed<E: de::Error>(&self, key: &str, value: &Value, max: Option<usize>) -> Result<String, E> {
        let raw = self.string(key, value)?;
        let trimmed = js_trim(&raw);
        if trimmed.is_empty() {
            return Err(self.fail(key, "Expected a value with a length of at least 1, got \"\""));
        }
        if let Some(max) = max {
            if js_length(trimmed) > max {
                return Err(self.fail(key, format!("Expected a value with a length of at most {max}")));
            }
        }
        Ok(trimmed.to_owned())
    }

    fn req_trimmed<E: de::Error>(&self, key: &str, max: Option<usize>) -> Result<String, E> {
        let value = self.required(key)?;
        self.trimmed(key, value, max)
    }

    fn opt_trimmed<E: de::Error>(&self, key: &str, max: Option<usize>) -> Result<Option<String>, E> {
        match self.optional(key) {
            None => Ok(None),
            Some(value) => self.trimmed(key, value, max).map(Some),
        }
    }

    /// `Schema.optional(Schema.NullOr(TrimmedNonEmptyString))`.
    fn nullable_trimmed<E: de::Error>(&self, key: &str) -> Result<NullableField<String>, E> {
        match self.map.get(key) {
            None => Ok(None),
            Some(Value::Null) => Ok(Some(None)),
            Some(value) => self.trimmed(key, value, None).map(|v| Some(Some(v))),
        }
    }

    /// `Schema.Int` between `min` and `max`.
    fn int<E: de::Error>(&self, key: &str, value: &Value, min: i64, max: i64) -> Result<i64, E> {
        let number = value.as_f64().ok_or_else(|| self.fail(key, format!("Expected number, got {value}")))?;
        if number.fract() != 0.0 || !number.is_finite() {
            return Err(self.fail(key, format!("Expected an integer, got {number}")));
        }
        let int = number as i64;
        if int < min || int > max {
            return Err(self.fail(key, format!("Expected a value between {min} and {max}, got {int}")));
        }
        Ok(int)
    }

    fn req_size<E: de::Error>(&self, key: &str, max: i64) -> Result<u16, E> {
        let value = self.required(key)?;
        self.int(key, value, 1, max).map(|v| v as u16)
    }

    fn opt_size<E: de::Error>(&self, key: &str, max: i64) -> Result<Option<u16>, E> {
        match self.optional(key) {
            None => Ok(None),
            Some(value) => self.int(key, value, 1, max).map(|v| Some(v as u16)),
        }
    }

    fn opt_bool<E: de::Error>(&self, key: &str) -> Result<Option<bool>, E> {
        match self.optional(key) {
            None => Ok(None),
            Some(Value::Bool(flag)) => Ok(Some(*flag)),
            Some(other) => Err(self.fail(key, format!("Expected boolean, got {other}"))),
        }
    }

    /// `TerminalEnvSchema`: ≤ 128 entries, keys `^[A-Za-z_][A-Za-z0-9_]*$` ≤ 128, values ≤ 8,192.
    fn opt_env<E: de::Error>(&self, key: &str) -> Result<Option<BTreeMap<String, String>>, E> {
        let Some(value) = self.optional(key) else {
            return Ok(None);
        };
        let Value::Object(entries) = value else {
            return Err(self.fail(key, format!("Expected object, got {value}")));
        };
        if entries.len() > ENV_MAX_PROPERTIES {
            return Err(self.fail(key, format!("Expected an object with at most {ENV_MAX_PROPERTIES} properties")));
        }
        let mut env = BTreeMap::new();
        for (name, value) in entries {
            if !is_env_key(name) || js_length(name) > ENV_KEY_MAX {
                return Err(self.fail(key, format!("Invalid environment variable name {name:?}")));
            }
            let Some(text) = value.as_str() else {
                return Err(self.fail(key, format!("Expected string at {name:?}, got {value}")));
            };
            if js_length(text) > ENV_VALUE_MAX {
                return Err(self.fail(key, format!("Expected a value with a length of at most {ENV_VALUE_MAX} at {name:?}")));
            }
            env.insert(name.clone(), text.to_owned());
        }
        Ok(Some(env))
    }

    /// `ProviderInstanceId`: a trimmed slug `^[a-zA-Z][a-zA-Z0-9_-]*$`, ≤ 64.
    fn opt_provider_instance_id<E: de::Error>(&self, key: &str) -> Result<Option<String>, E> {
        let Some(id) = self.opt_trimmed(key, Some(PROVIDER_SLUG_MAX))? else {
            return Ok(None);
        };
        let mut chars = id.chars();
        let valid = chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !valid {
            return Err(self.fail(key, format!("Expected a provider slug, got {id:?}")));
        }
        Ok(Some(id))
    }
}

fn is_env_key(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl<'de> Deserialize<'de> for TerminalOpenInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let f = Fields::new(deserializer, "TerminalOpenInput")?;
        Ok(Self {
            thread_id: f.req_trimmed("threadId", None)?,
            terminal_id: f.req_trimmed("terminalId", Some(TERMINAL_ID_MAX))?,
            cwd: f.req_trimmed("cwd", None)?,
            worktree_path: f.nullable_trimmed("worktreePath")?,
            cols: f.opt_size("cols", COLS_MAX)?,
            rows: f.opt_size("rows", ROWS_MAX)?,
            env: f.opt_env("env")?,
            provider_instance_id: f.opt_provider_instance_id("providerInstanceId")?,
        })
    }
}

impl<'de> Deserialize<'de> for TerminalAttachInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let f = Fields::new(deserializer, "TerminalAttachInput")?;
        Ok(Self {
            thread_id: f.req_trimmed("threadId", None)?,
            terminal_id: f.req_trimmed("terminalId", Some(TERMINAL_ID_MAX))?,
            cwd: f.opt_trimmed("cwd", None)?,
            worktree_path: f.nullable_trimmed("worktreePath")?,
            cols: f.opt_size("cols", COLS_MAX)?,
            rows: f.opt_size("rows", ROWS_MAX)?,
            env: f.opt_env("env")?,
            provider_instance_id: f.opt_provider_instance_id("providerInstanceId")?,
            restart_if_not_running: f.opt_bool("restartIfNotRunning")?,
        })
    }
}

impl<'de> Deserialize<'de> for TerminalWriteInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let f = Fields::new(deserializer, "TerminalWriteInput")?;
        let data = f.string("data", f.required("data")?)?;
        let length = js_length(&data);
        if length == 0 {
            return Err(f.fail("data", "Expected a value with a length of at least 1, got \"\""));
        }
        if length > WRITE_DATA_MAX {
            return Err(f.fail("data", format!("Expected a value with a length of at most {WRITE_DATA_MAX}")));
        }
        Ok(Self {
            thread_id: f.req_trimmed("threadId", None)?,
            terminal_id: f.req_trimmed("terminalId", Some(TERMINAL_ID_MAX))?,
            data,
        })
    }
}

impl<'de> Deserialize<'de> for TerminalResizeInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let f = Fields::new(deserializer, "TerminalResizeInput")?;
        Ok(Self {
            thread_id: f.req_trimmed("threadId", None)?,
            terminal_id: f.req_trimmed("terminalId", Some(TERMINAL_ID_MAX))?,
            cols: f.req_size("cols", COLS_MAX)?,
            rows: f.req_size("rows", ROWS_MAX)?,
        })
    }
}

impl<'de> Deserialize<'de> for TerminalClearInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let f = Fields::new(deserializer, "TerminalClearInput")?;
        Ok(Self {
            thread_id: f.req_trimmed("threadId", None)?,
            terminal_id: f.req_trimmed("terminalId", Some(TERMINAL_ID_MAX))?,
        })
    }
}

impl<'de> Deserialize<'de> for TerminalRestartInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let f = Fields::new(deserializer, "TerminalRestartInput")?;
        Ok(Self {
            thread_id: f.req_trimmed("threadId", None)?,
            terminal_id: f.req_trimmed("terminalId", Some(TERMINAL_ID_MAX))?,
            cwd: f.req_trimmed("cwd", None)?,
            worktree_path: f.nullable_trimmed("worktreePath")?,
            cols: f.req_size("cols", COLS_MAX)?,
            rows: f.req_size("rows", ROWS_MAX)?,
            env: f.opt_env("env")?,
            provider_instance_id: f.opt_provider_instance_id("providerInstanceId")?,
        })
    }
}

impl<'de> Deserialize<'de> for TerminalCloseInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let f = Fields::new(deserializer, "TerminalCloseInput")?;
        Ok(Self {
            thread_id: f.req_trimmed("threadId", None)?,
            terminal_id: f.opt_trimmed("terminalId", Some(TERMINAL_ID_MAX))?,
            delete_history: f.opt_bool("deleteHistory")?,
        })
    }
}

impl<'de> Deserialize<'de> for EmptyInput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Fields::new(deserializer, "{}").map(|_| Self {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inputs_trim_and_validate_like_the_schema() {
        let input: TerminalOpenInput = serde_json::from_value(json!({
            "threadId": "  thread-1\u{feff}",
            "terminalId": " term-1 ",
            "cwd": "/tmp",
            "worktreePath": null,
            "cols": 120.0,
            "env": {"A_B": "1"},
            "extra": true
        }))
        .unwrap();
        assert_eq!(input.thread_id, "thread-1");
        assert_eq!(input.terminal_id, "term-1");
        assert_eq!(input.worktree_path, Some(None));
        assert_eq!(input.cols, Some(120));
        assert_eq!(input.rows, None);

        for bad in [
            json!({"threadId": " ", "terminalId": "t", "cwd": "/"}),
            json!({"threadId": "t", "terminalId": "t", "cwd": "/", "cols": 0}),
            json!({"threadId": "t", "terminalId": "t", "cwd": "/", "cols": 1001}),
            json!({"threadId": "t", "terminalId": "t", "cwd": "/", "rows": 501}),
            json!({"threadId": "t", "terminalId": "t", "cwd": "/", "rows": 2.5}),
            json!({"threadId": "t", "terminalId": "x".repeat(129), "cwd": "/"}),
            json!({"threadId": "t", "terminalId": "t", "cwd": "/", "env": {"1A": "x"}}),
            json!({"threadId": "t", "terminalId": "t", "cwd": "/", "providerInstanceId": "1x"}),
            json!({"threadId": "t", "terminalId": "t"}),
        ] {
            assert!(serde_json::from_value::<TerminalOpenInput>(bad.clone()).is_err(), "{bad}");
        }

        let write: Result<TerminalWriteInput, _> = serde_json::from_value(json!({"threadId": "t", "terminalId": "t", "data": "😀".repeat(32_768)}));
        assert!(write.is_ok(), "65,536 code units are allowed");
        let write: Result<TerminalWriteInput, _> =
            serde_json::from_value(json!({"threadId": "t", "terminalId": "t", "data": format!("{}a", "😀".repeat(32_768))}));
        assert!(write.is_err(), "65,537 code units are not");
        let write: Result<TerminalWriteInput, _> = serde_json::from_value(json!({"threadId": "t", "terminalId": "t", "data": " "}));
        assert_eq!(write.unwrap().data, " ", "data is not trimmed");
    }

    #[test]
    fn events_encode_like_the_schema() {
        let snapshot = TerminalSessionSnapshot {
            thread_id: "t".into(),
            terminal_id: "term-1".into(),
            cwd: "/".into(),
            worktree_path: None,
            status: TerminalSessionStatus::Running,
            pid: Some(42),
            history: String::new(),
            exit_code: None,
            exit_signal: None,
            label: "Terminal 1".into(),
            updated_at: "2026-10-01T00:00:00.000Z".into(),
            sequence: Some(1),
        };
        let started = TerminalEvent {
            thread_id: "t".into(),
            terminal_id: "term-1".into(),
            sequence: Some(1),
            kind: TerminalEventKind::Started {
                snapshot: Box::new(snapshot.clone()),
            },
        };
        assert_eq!(
            serde_json::to_string(&started).unwrap(),
            r#"{"threadId":"t","terminalId":"term-1","sequence":1,"type":"started","snapshot":{"threadId":"t","terminalId":"term-1","cwd":"/","worktreePath":null,"status":"running","pid":42,"history":"","exitCode":null,"exitSignal":null,"label":"Terminal 1","updatedAt":"2026-10-01T00:00:00.000Z","sequence":1}}"#
        );
        let exited = TerminalEvent {
            thread_id: "t".into(),
            terminal_id: "term-1".into(),
            sequence: Some(4),
            kind: TerminalEventKind::Exited {
                exit_code: Some(0),
                exit_signal: None,
            },
        };
        assert_eq!(
            serde_json::to_value(&exited).unwrap(),
            json!({"threadId":"t","terminalId":"term-1","sequence":4,"type":"exited","exitCode":0,"exitSignal":null})
        );
        let attach = TerminalAttachStreamEvent::Snapshot(Box::new(snapshot));
        let encoded = serde_json::to_value(&attach).unwrap();
        assert_eq!(encoded["type"], "snapshot");
        assert_eq!(serde_json::from_value::<TerminalAttachStreamEvent>(encoded).unwrap(), attach);
        assert_eq!(
            serde_json::to_value(TerminalMetadataStreamEvent::Remove {
                thread_id: "t".into(),
                terminal_id: "x".into()
            })
            .unwrap(),
            json!({"type":"remove","threadId":"t","terminalId":"x"})
        );
    }

    #[test]
    fn errors_encode_declared_fields_and_message() {
        let error = TerminalError::TerminalWriteError {
            thread_id: "t".into(),
            terminal_id: "term-1".into(),
            terminal_pid: 9000,
            cause: Defect::error("Error", "boom"),
        };
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({"_tag":"TerminalWriteError","threadId":"t","terminalId":"term-1","terminalPid":9000,"cause":{"name":"Error","message":"boom"}})
        );
        assert_eq!(error.message(), "Failed to write to terminal for thread: t, terminal: term-1, PID: 9000");
        let tagged = error.to_tagged();
        assert_eq!(tagged.tag, "TerminalWriteError");
        assert_eq!(TerminalError::from_tagged(&tagged), Some(error));
        let history = TerminalError::TerminalHistoryError {
            operation: TerminalHistoryOperation::Migrate,
            thread_id: "t".into(),
            terminal_id: "x".into(),
            cause: None,
        };
        assert_eq!(
            serde_json::to_value(&history).unwrap(),
            json!({"_tag":"TerminalHistoryError","operation":"migrate","threadId":"t","terminalId":"x"})
        );
    }
}
