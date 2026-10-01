//! JavaScript value semantics the settings files depend on.
//!
//! The TS services handle plain JS objects, so three behaviours leak into the files they write
//! and have to be reproduced exactly for the files to stay byte-identical:
//!
//! - [`stringify_pretty`]: `JSON.stringify(value, null, 2)`. Numbers are printed the way
//!   `Number.prototype.toString` prints them (`1e+21`, `1e-7`, `5` for `5.0`), which is not
//!   what `serde_json` writes.
//! - [`js_equal`]: Effect's structural `Equal.equals` on plain data: key order is ignored and
//!   numbers compare by value (`5` equals `5.0`).
//! - [`js_trim`]: `String.prototype.trim`, whose whitespace set differs from Rust's
//!   (`U+FEFF` is whitespace in JS, `U+0085` is not).
//!
//! Object key order is insertion order (`serde_json` with `preserve_order`), except that JS
//! puts integer-like keys first in ascending order ([`js_key_order`]).

use serde_json::{Map, Value};

/// `JSON.stringify(value, null, 2)`.
pub fn stringify_pretty(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, Some(0));
    out
}

/// `JSON.stringify(value)`.
pub fn stringify(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, None);
    out
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number_to_string(number)),
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, indent.map(|level| level + 1));
                write_value(out, item, indent.map(|level| level + 1));
            }
            newline(out, indent);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, indent.map(|level| level + 1));
                write_string(out, key);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent.map(|level| level + 1));
            }
            newline(out, indent);
            out.push('}');
        }
    }
}

fn newline(out: &mut String, indent: Option<usize>) {
    if let Some(level) = indent {
        out.push('\n');
        for _ in 0..level {
            out.push_str("  ");
        }
    }
}

/// `JSON.stringify` string quoting: `"`, `\`, the short escapes and other control characters as
/// `\u00xx`; everything else (including non-ASCII, `/`, U+2028) verbatim.
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// 2^53: integers beyond it are not exact JS numbers.
const MAX_SAFE: u64 = 1 << 53;

fn number_to_string(number: &serde_json::Number) -> String {
    if let Some(value) = number.as_u64() {
        if value <= MAX_SAFE {
            return value.to_string();
        }
    } else if let Some(value) = number.as_i64() {
        if value.unsigned_abs() <= MAX_SAFE {
            return value.to_string();
        }
    }
    js_number_to_string(number.as_f64().unwrap_or(f64::NAN))
}

/// `Number.prototype.toString()` for a finite number (`JSON.stringify` writes `null` for the
/// others, which a `serde_json::Value` cannot hold anyway).
pub fn js_number_to_string(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    let sign = if value < 0.0 { "-" } else { "" };
    // `{:e}` is the shortest round-trip representation: `d.ddde<exp>`.
    let formatted = format!("{:e}", value.abs());
    let (mantissa, exponent) = formatted.split_once('e').expect("LowerExp always has an exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let exponent: i64 = exponent.parse().expect("integral exponent");
    let k = digits.len() as i64;
    let n = exponent + 1;
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let e = n - 1;
        let exp_sign = if e >= 0 { "+" } else { "-" };
        if k == 1 {
            format!("{digits}e{exp_sign}{}", e.abs())
        } else {
            format!("{}.{}e{exp_sign}{}", &digits[..1], &digits[1..], e.abs())
        }
    };
    format!("{sign}{body}")
}

/// Effect `Equal.equals` on JSON data: structural, key order ignored, numbers by value.
pub fn js_equal(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => x == y,
            _ => a == b,
        },
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(x, y)| js_equal(x, y)),
        (Value::Object(a), Value::Object(b)) => a.len() == b.len() && a.iter().all(|(key, x)| b.get(key).is_some_and(|y| js_equal(x, y))),
        _ => left == right,
    }
}

/// JS `WhiteSpace` and `LineTerminator` code points (what `String.prototype.trim` strips).
pub fn is_js_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\t' | '\n' | '\u{B}' | '\u{C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(text: &str) -> &str {
    text.trim_matches(is_js_whitespace)
}

/// Whether `key` is an array index (`"0"`, `"42"`, below 2^32 - 1, no leading zero): JS
/// objects list those keys first, in ascending numeric order.
fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    if !key.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    key.parse::<u64>().ok().filter(|value| *value < u64::from(u32::MAX)).map(|value| value as u32)
}

/// Reorder a map's keys the way a JS object enumerates them: array-index keys ascending, then
/// the other keys in insertion order.
pub fn js_key_order(map: Map<String, Value>) -> Map<String, Value> {
    if !map.keys().any(|key| array_index(key).is_some()) {
        return map;
    }
    let mut indexed: Vec<(u32, String, Value)> = Vec::new();
    let mut named: Vec<(String, Value)> = Vec::new();
    for (key, value) in map {
        match array_index(&key) {
            Some(index) => indexed.push((index, key, value)),
            None => named.push((key, value)),
        }
    }
    indexed.sort_by_key(|(index, _, _)| *index);
    let mut out = Map::new();
    for (_, key, value) in indexed {
        out.insert(key, value);
    }
    for (key, value) in named {
        out.insert(key, value);
    }
    out
}

/// `Predicate.isObject` (non-null, non-array object).
pub fn is_object(value: &Value) -> bool {
    value.is_object()
}

/// `deepMerge` of `packages/shared/src/Struct.ts`: plain objects merge key by key (patch keys
/// appended in patch order), anything else in the patch replaces.
pub fn deep_merge(current: &Value, patch: &Value) -> Value {
    match (current, patch) {
        (Value::Object(current), Value::Object(patch)) => {
            let mut next = current.clone();
            for (key, value) in patch {
                let merged = match next.get(key) {
                    Some(existing) if existing.is_object() && value.is_object() => deep_merge(existing, value),
                    _ => value.clone(),
                };
                next.insert(key.clone(), merged);
            }
            Value::Object(next)
        }
        _ => patch.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_print_like_javascript() {
        let cases = [
            (5.0, "5"),
            (0.1, "0.1"),
            (1.5, "1.5"),
            (-2.25, "-2.25"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (123456789.125, "123456789.125"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (2.5e25, "2.5e+25"),
            (-0.0, "0"),
            (0.000123, "0.000123"),
        ];
        for (value, expected) in cases {
            assert_eq!(js_number_to_string(value), expected, "{value}");
        }
    }

    #[test]
    fn pretty_matches_json_stringify() {
        let value = json!({"a": [], "b": {}, "c": [1, {"d": "x\"\n\u{1}é"}], "e": 5.0, "f": null});
        assert_eq!(
            stringify_pretty(&value),
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"d\": \"x\\\"\\n\\u0001é\"\n    }\n  ],\n  \"e\": 5,\n  \"f\": null\n}"
        );
        assert_eq!(stringify(&json!({"a": [1, 2]})), "{\"a\":[1,2]}");
    }

    #[test]
    fn equality_ignores_key_order_and_number_representation() {
        assert!(js_equal(&json!({"a": 1, "b": 2.0}), &json!({"b": 2, "a": 1.0})));
        assert!(!js_equal(&json!([1, 2]), &json!([2, 1])));
        assert!(!js_equal(&json!({"a": 1}), &json!({"a": 1, "b": null})));
    }

    #[test]
    fn trim_uses_the_javascript_whitespace_set() {
        assert_eq!(js_trim("\u{FEFF} a \u{3000}"), "a");
        assert_eq!(js_trim("\u{85}a"), "\u{85}a");
    }

    #[test]
    fn integer_keys_enumerate_first() {
        let mut map = Map::new();
        map.insert("b".into(), json!(1));
        map.insert("10".into(), json!(2));
        map.insert("2".into(), json!(3));
        map.insert("01".into(), json!(4));
        let keys: Vec<_> = js_key_order(map).keys().cloned().collect();
        assert_eq!(keys, ["2", "10", "b", "01"]);
    }

    #[test]
    fn deep_merge_merges_objects_and_replaces_arrays() {
        let merged = deep_merge(&json!({"a": {"x": 1, "y": [1]}, "b": 1}), &json!({"a": {"y": [2], "z": 3}, "c": 4}));
        assert_eq!(stringify(&merged), r#"{"a":{"x":1,"y":[2],"z":3},"b":1,"c":4}"#);
    }
}
