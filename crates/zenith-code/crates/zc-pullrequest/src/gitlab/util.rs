//! Helpers the GitLab (and Forgejo) pull request code shares: `@t3tools/shared/gitPatchPath`
//! (`quoteGitPatchPath`, `unquoteGitPatchPath`), `Number(string)`, an insertion-ordered map with the TS `Map`'s
//! semantics, and readers that decode a JSON value the way the TS `Schema.Struct` field
//! declarations do (`Schema.optional(X)` rejects `null`, `Schema.optional(Schema.NullOr(X))`
//! does not, `Schema.Int` is a safe integer, a struct is a non-array object).

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};
use zc_sourcecontrol::util::{js_trim, safe_int};

/// `quoteGitPatchPath`: a name as a patch header can carry it, itself where unambiguous and git's
/// C-quoted form where it holds a quote, a backslash or a control character. Pass the header
/// side's `a/` or `b/` in with the name: git quotes the whole token.
pub fn quote_git_patch_path(path: &str) -> String {
    let mut body = String::with_capacity(path.len());
    let mut quoting = false;
    for character in path.chars() {
        let escape = match character {
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\u{7}' => Some("\\a"),
            '\u{8}' => Some("\\b"),
            '\t' => Some("\\t"),
            '\n' => Some("\\n"),
            '\u{b}' => Some("\\v"),
            '\u{c}' => Some("\\f"),
            '\r' => Some("\\r"),
            _ => None,
        };
        if let Some(escape) = escape {
            body.push_str(escape);
            quoting = true;
            continue;
        }
        let code = character as u32;
        // A control character is one byte in UTF-8, so its code point is the byte git writes.
        if code < 0x20 || code == 0x7f {
            body.push_str(&format!("\\{code:03o}"));
            quoting = true;
            continue;
        }
        body.push(character);
    }
    if quoting {
        format!("\"{body}\"")
    } else {
        path.to_owned()
    }
}

/// The escapes inside a quoted form undone (`unescapeBody`): named escapes and three-digit octal
/// bytes, rejoined and decoded as UTF-8; an escape git would never write reads as C reads it.
fn unescape_git_patch_body(body: &str) -> String {
    if !body.contains('\\') {
        return body.to_owned();
    }
    let mut bytes: Vec<u8> = Vec::with_capacity(body.len());
    let chars: Vec<char> = body.chars().collect();
    let mut at = 0;
    while at < chars.len() {
        let character = chars[at];
        if character != '\\' {
            let mut buffer = [0; 4];
            bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
            at += 1;
            continue;
        }
        let Some(&escaped) = chars.get(at + 1) else {
            bytes.push(b'\\');
            break;
        };
        let named = match escaped {
            '"' => Some(0x22),
            '\\' => Some(0x5c),
            'a' => Some(0x07),
            'b' => Some(0x08),
            'f' => Some(0x0c),
            'n' => Some(0x0a),
            'r' => Some(0x0d),
            't' => Some(0x09),
            'v' => Some(0x0b),
            _ => None,
        };
        if let Some(byte) = named {
            bytes.push(byte);
            at += 2;
            continue;
        }
        let octal: String = chars.iter().skip(at + 1).take(3).collect();
        if octal.len() == 3 && octal.chars().all(|c| ('0'..='7').contains(&c)) {
            // A `Uint8Array` keeps the low byte of `\777`.
            bytes.push((u32::from_str_radix(&octal, 8).unwrap_or(0) & 0xff) as u8);
            at += 4;
            continue;
        }
        let mut buffer = [0; 4];
        bytes.extend_from_slice(escaped.encode_utf8(&mut buffer).as_bytes());
        at += 2;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `unquoteGitPatchPath`: one header's name token as the name it stands for, whether or not the
/// quotes are still around it.
pub fn unquote_git_patch_path(token: &str) -> String {
    if token.len() >= 2 && token.starts_with('"') && token.ends_with('"') {
        return unescape_git_patch_body(&token[1..token.len() - 1]);
    }
    unescape_git_patch_body(token)
}

/// `text.replace(/\n?$/, "\n")`: the text ending on exactly the newline it had, or one added.
pub fn ensure_trailing_newline(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

/// `Number(text)` for a string: JS white space trimmed, empty is 0, `0x`/`0o`/`0b` literals,
/// `Infinity`, and decimal literals; anything else is `NaN`.
pub fn js_number_from_string(text: &str) -> f64 {
    static DECIMAL: OnceLock<Regex> = OnceLock::new();
    let trimmed = js_trim(text);
    if trimmed.is_empty() {
        return 0.0;
    }
    let radix = match trimmed.get(..2) {
        Some("0x" | "0X") => Some(16),
        Some("0o" | "0O") => Some(8),
        Some("0b" | "0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        let digits = &trimmed[2..];
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return f64::NAN;
        }
        return digits
            .chars()
            .fold(0.0, |value, c| value * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0)));
    }
    match trimmed {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let decimal = DECIMAL.get_or_init(|| Regex::new(r"^[+-]?(?:[0-9]+\.?[0-9]*|\.[0-9]+)(?:[eE][+-]?[0-9]+)?$").expect("valid regex"));
    if decimal.is_match(trimmed) {
        trimmed.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// `Number.isSafeInteger`.
pub fn is_safe_integer(value: f64) -> bool {
    value.is_finite() && value.fract() == 0.0 && value.abs() <= 9_007_199_254_740_991.0
}

/// `Number.parseInt(text, 10)`: the leading decimal integer after JS white space, `NaN` without one.
pub fn js_parse_int(text: &str) -> f64 {
    let trimmed = js_trim(text);
    let (sign, digits) = match trimmed.as_bytes().first() {
        Some(b'-') => (-1.0, &trimmed[1..]),
        Some(b'+') => (1.0, &trimmed[1..]),
        _ => (1.0, trimmed),
    };
    let leading: &str = &digits[..digits.bytes().take_while(u8::is_ascii_digit).count()];
    if leading.is_empty() {
        return f64::NAN;
    }
    sign * leading.parse::<f64>().unwrap_or(f64::NAN)
}

/// An insertion-ordered string map with the TS `Map`'s semantics: setting an existing key keeps
/// its place and replaces its value.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OrderedMap<V> {
    entries: Vec<(String, V)>,
    index: HashMap<String, usize>,
}

impl<V> OrderedMap<V> {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            index: HashMap::new(),
        }
    }

    pub fn set(&mut self, key: String, value: V) {
        match self.index.get(&key) {
            Some(&at) => self.entries[at].1 = value,
            None => {
                self.index.insert(key.clone(), self.entries.len());
                self.entries.push((key, value));
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&V> {
        self.index.get(key).map(|&at| &self.entries[at].1)
    }

    pub fn has(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &V)> {
        self.entries.iter().map(|(key, value)| (key, value))
    }

    pub fn into_entries(self) -> Vec<(String, V)> {
        self.entries
    }
}

/// A value that does not match the declared schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mismatch;

pub type Decoded<T> = Result<T, Mismatch>;

/// `Schema.Struct`: a non-array object.
pub fn object(value: &Value) -> Decoded<&Map<String, Value>> {
    value.as_object().ok_or(Mismatch)
}

/// The field of an optional key: absent is `None`; `null` is `None` when `nullable`, else a mismatch.
fn optional<'a>(object: &'a Map<String, Value>, key: &str, nullable: bool) -> Decoded<Option<&'a Value>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Null) if nullable => Ok(None),
        Some(Value::Null) => Err(Mismatch),
        Some(value) => Ok(Some(value)),
    }
}

/// A required key whose value may be `null` (`Schema.NullOr(X)`).
fn required_nullable<'a>(object: &'a Map<String, Value>, key: &str) -> Decoded<Option<&'a Value>> {
    match object.get(key) {
        None => Err(Mismatch),
        Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(value)),
    }
}

/// `key: Schema.String`.
pub fn string(object: &Map<String, Value>, key: &str) -> Decoded<String> {
    object.get(key).and_then(Value::as_str).map(str::to_owned).ok_or(Mismatch)
}

/// `key: Schema.Int`.
pub fn int(object: &Map<String, Value>, key: &str) -> Decoded<i64> {
    object.get(key).and_then(safe_int).ok_or(Mismatch)
}

/// `key: Schema.Boolean`.
pub fn boolean(object: &Map<String, Value>, key: &str) -> Decoded<bool> {
    object.get(key).and_then(Value::as_bool).ok_or(Mismatch)
}

/// `key: Schema.optional(Schema.String)` (`nullable`: `optional(NullOr(String))`).
pub fn opt_string(object: &Map<String, Value>, key: &str, nullable: bool) -> Decoded<Option<String>> {
    optional(object, key, nullable)?
        .map(|value| value.as_str().map(str::to_owned).ok_or(Mismatch))
        .transpose()
}

/// `key: Schema.NullOr(Schema.String)` (required key).
pub fn nullable_string(object: &Map<String, Value>, key: &str) -> Decoded<Option<String>> {
    required_nullable(object, key)?
        .map(|value| value.as_str().map(str::to_owned).ok_or(Mismatch))
        .transpose()
}

/// `key: Schema.optional(Schema.Boolean)` (`nullable`: with `NullOr`).
pub fn opt_bool(object: &Map<String, Value>, key: &str, nullable: bool) -> Decoded<Option<bool>> {
    optional(object, key, nullable)?.map(|value| value.as_bool().ok_or(Mismatch)).transpose()
}

/// `key: Schema.optional(Schema.Int)` (`nullable`: with `NullOr`).
pub fn opt_int(object: &Map<String, Value>, key: &str, nullable: bool) -> Decoded<Option<i64>> {
    optional(object, key, nullable)?.map(|value| safe_int(value).ok_or(Mismatch)).transpose()
}

/// `key: Schema.optional(Schema.Number)` (`nullable`: with `NullOr`).
pub fn opt_number(object: &Map<String, Value>, key: &str, nullable: bool) -> Decoded<Option<f64>> {
    optional(object, key, nullable)?.map(|value| value.as_f64().ok_or(Mismatch)).transpose()
}

/// `key: Schema.optional(Schema.Struct(…))` (`nullable`: with `NullOr`), decoded by `decode`.
pub fn opt_struct<T>(object: &Map<String, Value>, key: &str, nullable: bool, decode: impl FnOnce(&Map<String, Value>) -> Decoded<T>) -> Decoded<Option<T>> {
    optional(object, key, nullable)?.map(|value| decode(self::object(value)?)).transpose()
}

/// `key: Schema.NullOr(Schema.Struct(…))` (required key).
pub fn nullable_struct<T>(object: &Map<String, Value>, key: &str, decode: impl FnOnce(&Map<String, Value>) -> Decoded<T>) -> Decoded<Option<T>> {
    required_nullable(object, key)?.map(|value| decode(self::object(value)?)).transpose()
}

/// `key: Schema.Struct(…)` (required key).
pub fn required_struct<T>(object: &Map<String, Value>, key: &str, decode: impl FnOnce(&Map<String, Value>) -> Decoded<T>) -> Decoded<T> {
    decode(self::object(object.get(key).ok_or(Mismatch)?)?)
}

/// `key: Schema.optional(Schema.Array(item))` (`nullable`: with `NullOr`): every item must decode.
pub fn opt_array<T>(object: &Map<String, Value>, key: &str, nullable: bool, item: impl FnMut(&Value) -> Decoded<T>) -> Decoded<Option<Vec<T>>> {
    optional(object, key, nullable)?.map(|value| array(value, item)).transpose()
}

/// `Schema.Array(item)`: every item must decode.
pub fn array<T>(value: &Value, item: impl FnMut(&Value) -> Decoded<T>) -> Decoded<Vec<T>> {
    value.as_array().ok_or(Mismatch)?.iter().map(item).collect()
}

/// `Schema.NullOr(Schema.Struct(…))` as an array item.
pub fn nullable_item<T>(value: &Value, decode: impl FnOnce(&Map<String, Value>) -> Decoded<T>) -> Decoded<Option<T>> {
    match value {
        Value::Null => Ok(None),
        value => decode(object(value)?).map(Some),
    }
}

/// `Schema.String` as an array item.
pub fn string_item(value: &Value) -> Decoded<String> {
    value.as_str().map(str::to_owned).ok_or(Mismatch)
}

/// `decodeJsonResult(Schema.Array(Schema.Unknown))`: the raw rows of a JSON array.
pub fn parse_list(raw: &str) -> Result<Vec<Value>, String> {
    match serde_json::from_str::<Value>(raw).map_err(|error| error.to_string())? {
        Value::Array(items) => Ok(items),
        _ => Err("Expected an array".into()),
    }
}

/// `decodeJsonResult(schema)` for a struct schema: the parsed object handed to `decode`.
pub fn parse_struct<T>(raw: &str, decode: impl FnOnce(&Map<String, Value>) -> Decoded<T>) -> Result<T, String> {
    let value = serde_json::from_str::<Value>(raw).map_err(|error| error.to_string())?;
    let object = value.as_object().ok_or_else(|| "Expected an object".to_owned())?;
    decode(object).map_err(|_| "The response does not match the expected schema".to_owned())
}

/// `trimmed(value)`: the JS-trimmed text, `None` when absent or blank.
pub fn trimmed(value: Option<&str>) -> Option<String> {
    let text = js_trim(value.unwrap_or_default());
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_like_git() {
        assert_eq!(quote_git_patch_path("a/src/app.ts"), "a/src/app.ts");
        assert_eq!(quote_git_patch_path(r"a/src\notes.ts"), r#""a/src\\notes.ts""#);
        assert_eq!(quote_git_patch_path("b/tab\there"), r#""b/tab\there""#);
        assert_eq!(quote_git_patch_path("b/x\u{1}y\u{7f}"), r#""b/x\001y\177""#);
        assert_eq!(quote_git_patch_path("b/é ü"), "b/é ü");
    }

    #[test]
    fn numbers_like_javascript() {
        assert_eq!(js_number_from_string(" 9 "), 9.0);
        assert_eq!(js_number_from_string(""), 0.0);
        assert_eq!(js_number_from_string("0x10"), 16.0);
        assert_eq!(js_number_from_string("1e3"), 1000.0);
        assert_eq!(js_number_from_string("9.0"), 9.0);
        assert!(js_number_from_string("octocat").is_nan());
        assert!(js_number_from_string("inf").is_nan());
        assert!(js_number_from_string("-0x10").is_nan());
        assert_eq!(js_number_from_string("Infinity"), f64::INFINITY);
        assert_eq!(js_parse_int("1000+"), 1000.0);
        assert_eq!(js_parse_int("-5"), -5.0);
        assert!(js_parse_int("abc").is_nan());
        assert_eq!(js_parse_int("3.7"), 3.0);
    }

    #[test]
    fn ordered_map_keeps_the_first_place() {
        let mut map = OrderedMap::new();
        map.set("b".into(), 1);
        map.set("a".into(), 2);
        map.set("b".into(), 3);
        assert_eq!(map.into_entries(), vec![("b".to_owned(), 3), ("a".to_owned(), 2)]);
    }
}
