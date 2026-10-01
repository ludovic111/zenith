//! `JSON.stringify`, byte for byte.
//!
//! serde_json already escapes strings the way V8 does (`\"`, `\\`, `\b\f\n\r\t`, lowercase
//! `\u00xx` for the other control characters, nothing else). Numbers differ: V8 prints
//! `1e+21` and `1.5e-7` where ryu prints `1e21` and `1.5e-7`, and `-0` as `0`. Log lines and
//! prompt text the TS server wrote are compared byte for byte, so everything that ends up there
//! goes through this writer. Object keys keep their insertion order (`preserve_order`), except
//! that array-index keys (`"0"`, `"12"`) come first in ascending order, as in every V8 object;
//! integers beyond 2^53 print as the double JS would hold.

use serde_json::Value;

/// `JSON.stringify(value)`.
pub fn stringify(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, None, 0);
    out
}

/// `JSON.stringify(value, null, indent)`.
pub fn stringify_pretty(value: &Value, indent: usize) -> String {
    let mut out = String::new();
    let unit = " ".repeat(indent.min(10));
    write_value(&mut out, value, if unit.is_empty() { None } else { Some(&unit) }, 0);
    out
}

fn write_value(out: &mut String, value: &Value, indent: Option<&str>, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            const MAX_SAFE: u64 = 9_007_199_254_740_991;
            if let Some(int) = number.as_i64().filter(|int| int.unsigned_abs() <= MAX_SAFE) {
                out.push_str(&int.to_string());
            } else if let Some(uint) = number.as_u64().filter(|uint| *uint <= MAX_SAFE) {
                out.push_str(&uint.to_string());
            } else {
                out.push_str(&format_js_number(number.as_f64().unwrap_or(f64::NAN)));
            }
        }
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
                newline(out, indent, depth + 1);
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            let mut index_keys: Vec<(u32, &String, &Value)> = map.iter().filter_map(|(key, item)| array_index(key).map(|index| (index, key, item))).collect();
            index_keys.sort_by_key(|(index, _, _)| *index);
            let ordered = index_keys
                .into_iter()
                .map(|(_, key, item)| (key, item))
                .chain(map.iter().filter(|(key, _)| array_index(key).is_none()));
            for (index, (key, item)) in ordered.enumerate() {
                if index > 0 {
                    out.push(',');
                }
                newline(out, indent, depth + 1);
                write_string(out, key);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, item, indent, depth + 1);
            }
            newline(out, indent, depth);
            out.push('}');
        }
    }
}

/// A canonical array index (`0` … `2^32 - 2`, no leading zeros), which V8 orders first.
fn array_index(key: &str) -> Option<u32> {
    if key.is_empty() || key.len() > 10 || !key.chars().all(|c| c.is_ascii_digit()) || (key.len() > 1 && key.starts_with('0')) {
        return None;
    }
    key.parse::<u64>().ok().filter(|value| *value < u32::MAX as u64).map(|value| value as u32)
}

fn newline(out: &mut String, indent: Option<&str>, depth: usize) {
    if let Some(unit) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(unit);
        }
    }
}

/// A JSON string literal, escaped like `JSON.stringify` (the same rules serde_json uses).
pub fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
}

/// `Number.prototype.toString()` (and `JSON.stringify` for finite numbers; non-finite ones
/// stringify as `null`).
pub fn format_js_number(value: f64) -> String {
    if !value.is_finite() {
        return "null".to_owned();
    }
    if value == 0.0 {
        return "0".to_owned();
    }
    let negative = value < 0.0;
    // `{:e}` is the shortest round-trip representation: `d[.ddd]e<exp>`.
    let formatted = format!("{:e}", value.abs());
    let (mantissa, exponent) = formatted.split_once('e').unwrap_or((&formatted, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = digits.len() as i32;
    let n = exponent + 1;
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if k <= n && n <= 21 {
        out.push_str(&digits);
        for _ in 0..(n - k) {
            out.push('0');
        }
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        for _ in 0..(-n) {
            out.push('0');
        }
        out.push_str(&digits);
    } else {
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        let e = n - 1;
        out.push(if e < 0 { '-' } else { '+' });
        out.push_str(&e.abs().to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_print_like_v8() {
        assert_eq!(format_js_number(1e21), "1e+21");
        assert_eq!(format_js_number(1.5e-7), "1.5e-7");
        assert_eq!(format_js_number(123.456), "123.456");
        assert_eq!(format_js_number(0.000001), "0.000001");
        assert_eq!(format_js_number(1e20), "100000000000000000000");
        assert_eq!(format_js_number(-0.0), "0");
        assert_eq!(format_js_number(-2.5), "-2.5");
        assert_eq!(format_js_number(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(format_js_number(f64::NAN), "null");
    }

    #[test]
    fn stringifies_compact_and_pretty() {
        let value = json!({"a": [1, 2.5, {"b": null}], "c": "x\u{1}\"", "d": {}, "e": []});
        assert_eq!(stringify(&value), r#"{"a":[1,2.5,{"b":null}],"c":"x\u0001\"","d":{},"e":[]}"#);
        assert_eq!(
            stringify_pretty(&json!({"a": [1], "b": {"c": true}}), 2),
            "{\n  \"a\": [\n    1\n  ],\n  \"b\": {\n    \"c\": true\n  }\n}"
        );
        let keys: Value = serde_json::from_str(r#"{"b":1,"10":2,"2":3,"02":4,"a":5}"#).unwrap();
        assert_eq!(stringify(&keys), r#"{"2":3,"10":2,"b":1,"02":4,"a":5}"#);
        let big: Value = serde_json::from_str("[12345678901234567890, 9007199254740993]").unwrap();
        assert_eq!(stringify(&big), "[12345678901234567000,9007199254740992]");
    }
}
