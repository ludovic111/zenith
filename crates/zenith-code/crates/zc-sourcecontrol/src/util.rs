//! Small JavaScript-compatible helpers the TS sources lean on: `String.prototype.trim`,
//! `encodeURIComponent`, WHATWG `URL`, UTF-16 slicing and lengths, the `Schema` checks behind
//! `TrimmedNonEmptyString` / `PositiveInt`, and a test clock.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, OnceLock};

use regex::Regex;
use serde_json::Value;

/// `String.prototype.trim`: Unicode white space, line terminators and the BOM.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// `String.prototype.trimStart`.
pub fn js_trim_start(text: &str) -> &str {
    text.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// `String.prototype.length`: UTF-16 code units.
pub fn js_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `text.slice(0, units)` in UTF-16 code units, never splitting a character.
pub fn js_slice_units(text: &str, units: usize) -> &str {
    let mut used = 0;
    for (index, character) in text.char_indices() {
        let width = character.len_utf16();
        if used + width > units {
            return &text[..index];
        }
        used += width;
    }
    text
}

/// `encodeURIComponent`.
pub fn encode_uri_component(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')');
        if keep {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `new URL(value)`, `None` where it throws.
pub fn parse_url(value: &str) -> Option<url::Url> {
    url::Url::parse(value).ok()
}

/// `url.host`: the host with a non-default port.
pub fn url_host(url: &url::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_owned(),
    }
}

/// `url.hostname`.
pub fn url_hostname(url: &url::Url) -> String {
    url.host_str().unwrap_or_default().to_owned()
}

/// `url.origin`, `"null"` for opaque origins (like JavaScript).
pub fn url_origin(url: &url::Url) -> String {
    url.origin().ascii_serialization()
}

/// `url.toString()`. The `url` crate already serializes like the WHATWG `href`.
pub fn url_href(url: &url::Url) -> String {
    url.as_str().to_owned()
}

/// `TrimmedNonEmptyString` decoding: the trimmed value, `None` when empty.
pub fn trimmed_non_empty(value: &Value) -> Option<String> {
    let text = value.as_str()?;
    let trimmed = js_trim(text);
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// `Schema.Int` (a safe integer) refined by `positive`.
pub fn positive_int(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && number > 0.0 && number <= 9_007_199_254_740_991.0).then_some(number as i64)
}

/// `Schema.Int`.
pub fn safe_int(value: &Value) -> Option<i64> {
    let number = value.as_f64()?;
    (number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0).then_some(number as i64)
}

/// `trimOptionalString`: the trimmed text, `None` for absent, null or blank values.
pub fn trim_optional_string(value: Option<&str>) -> Option<String> {
    let trimmed = js_trim(value.unwrap_or_default());
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// A value that does not match the schema a decoder expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaMismatch;

/// Reads an optional field the way `Schema.optional(X)` decodes it: absent is `Ok(None)`, a value
/// of the wrong type is an error. `null` is an error too unless `nullable`.
pub fn optional_field<'a>(object: &'a serde_json::Map<String, Value>, key: &str, nullable: bool) -> Result<Option<&'a Value>, SchemaMismatch> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Null) if nullable => Ok(None),
        Some(Value::Null) => Err(SchemaMismatch),
        Some(value) => Ok(Some(value)),
    }
}

/// `Schema.optional(Schema.String)` / `optional(NullOr(String))`.
pub fn optional_string(object: &serde_json::Map<String, Value>, key: &str, nullable: bool) -> Result<Option<String>, SchemaMismatch> {
    match optional_field(object, key, nullable)? {
        None => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(SchemaMismatch),
    }
}

/// `Schema.optional(Schema.Boolean)`.
pub fn optional_bool(object: &serde_json::Map<String, Value>, key: &str) -> Result<Option<bool>, SchemaMismatch> {
    match optional_field(object, key, false)? {
        None => Ok(None),
        Some(Value::Bool(flag)) => Ok(Some(*flag)),
        Some(_) => Err(SchemaMismatch),
    }
}

/// `Schema.optional(Schema.OptionFromNullOr(Schema.DateTimeUtcFromString))`: absent and `null`
/// are `None`; a string that is not a date fails the decode.
pub fn optional_date(object: &serde_json::Map<String, Value>, key: &str) -> Result<Option<zc_contracts::DateTimeUtc>, SchemaMismatch> {
    match optional_field(object, key, true)? {
        None => Ok(None),
        Some(Value::String(text)) => zc_contracts::DateTimeUtc::parse(text).map(Some).map_err(|_| SchemaMismatch),
        Some(_) => Err(SchemaMismatch),
    }
}

/// `Date.parse` for the formats the forges send: ISO 8601 and HTTP dates (RFC 2822/1123).
pub fn date_parse_millis(value: &str) -> Option<i64> {
    let trimmed = js_trim(value);
    if let Ok(date) = zc_contracts::DateTimeUtc::parse(trimmed) {
        return Some(date.as_millis());
    }
    jiff::fmt::rfc2822::parse(trimmed).ok().map(|zoned| zoned.timestamp().as_millisecond())
}

/// `NodeUtil.stripVTControlCharacters`.
pub fn strip_vt_control_characters(text: &str) -> String {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    let pattern = ANSI.get_or_init(|| {
        Regex::new(concat!(
            r"[\x{001B}\x{009B}][\[\]()#;?]*",
            r"(?:(?:(?:(?:;[-a-zA-Z0-9/#&.:=?%@~_]+)*",
            r"|[a-zA-Z0-9]+(?:;[-a-zA-Z0-9/#&.:=?%@~_]*)*)?",
            r"(?:\x{0007}|\x{001B}\x{005C}|\x{009C}))",
            r"|(?:(?:[0-9]{1,4}(?:;[0-9]{0,4})*)?[0-9A-PR-TZcf-ntqry=><~]))"
        ))
        .expect("valid ANSI pattern")
    });
    pattern.replace_all(text, "").into_owned()
}

/// Split on `/\r?\n/`.
pub fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line))
}

/// The wall clock the rate limiters and quota snapshots read (Effect `Clock`), injectable for
/// tests the way `TestClock` is.
pub trait Clock: Send + Sync {
    fn now_millis(&self) -> i64;
}

/// The system wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_millis(&self) -> i64 {
        zc_core::time::now_millis()
    }
}

/// A clock that only moves when told to (Effect's `TestClock` starts at 0 too).
#[derive(Debug, Default)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(start: i64) -> Arc<Self> {
        Arc::new(Self(AtomicI64::new(start)))
    }

    pub fn set(&self, millis: i64) {
        self.0.store(millis, Ordering::SeqCst);
    }

    pub fn advance(&self, millis: i64) {
        self.0.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_millis(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A shared clock.
pub type SharedClock = Arc<dyn Clock>;

/// The system clock as a [`SharedClock`].
pub fn system_clock() -> SharedClock {
    Arc::new(SystemClock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_like_encode_uri_component() {
        assert_eq!(encode_uri_component("group/sub project"), "group%2Fsub%20project");
        assert_eq!(encode_uri_component("a-b_c.d!~*'()"), "a-b_c.d!~*'()");
        assert_eq!(encode_uri_component("é"), "%C3%A9");
    }

    #[test]
    fn strips_ansi() {
        assert_eq!(strip_vt_control_characters("\u{1b}[1mtea version 0.16.0\u{1b}[0m"), "tea version 0.16.0");
    }

    #[test]
    fn parses_http_and_iso_dates() {
        assert_eq!(date_parse_millis("Thu, 01 Jan 1970 00:02:01 GMT"), Some(121_000));
        assert_eq!(date_parse_millis("1970-01-01T00:02:01Z"), Some(121_000));
        assert_eq!(date_parse_millis("later"), None);
    }

    #[test]
    fn slices_utf16_units() {
        assert_eq!(js_slice_units("ab😀c", 3), "ab");
        assert_eq!(js_slice_units("ab😀c", 4), "ab😀");
    }
}
