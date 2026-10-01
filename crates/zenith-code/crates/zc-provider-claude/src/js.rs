//! JavaScript value semantics the adapter's strings depend on: `String(number)`,
//! `JSON.stringify`, UTF-16 `slice`, template-literal interpolation and truthiness.

use serde_json::Value;

/// `String(n)` / `Number.prototype.toString()` for a finite or non-finite double.
pub fn number_to_string(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if value == 0.0 {
        return "0".into();
    }
    // Shortest round-trip digits and the decimal exponent, then JS's layout rules.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(|c| c.is_ascii_digit()).collect();
    let k = digits.len() as i32;
    let n = exponent + 1;
    let sign = if value < 0.0 { "-" } else { "" };
    let body = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat((n - k) as usize))
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat((-n) as usize))
    } else {
        let exp = n - 1;
        let exp_text = if exp >= 0 { format!("+{exp}") } else { exp.to_string() };
        if k == 1 {
            format!("{digits}e{exp_text}")
        } else {
            format!("{}.{}e{exp_text}", &digits[..1], &digits[1..])
        }
    };
    format!("{sign}{body}")
}

fn json_number(number: &serde_json::Number) -> String {
    if let Some(i) = number.as_i64() {
        if i.unsigned_abs() < (1u64 << 53) {
            return i.to_string();
        }
    }
    if let Some(u) = number.as_u64() {
        if u < (1u64 << 53) {
            return u.to_string();
        }
    }
    number.as_f64().map(number_to_string).unwrap_or_else(|| number.to_string())
}

/// `JSON.stringify(value)` (compact).
pub fn stringify(value: &Value) -> String {
    let mut out = String::new();
    write_json(value, &mut out);
    out
}

fn write_json(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            let text = json_number(n);
            out.push_str(if text == "NaN" || text.contains("Infinity") { "null" } else { &text });
        }
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_json(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(key).unwrap_or_default());
                out.push(':');
                write_json(item, out);
            }
            out.push('}');
        }
    }
}

/// `${value}` in a template literal; `None` (an absent property) is `undefined`.
pub fn template(value: Option<&Value>) -> String {
    match value {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => json_number(n),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| if item.is_null() { String::new() } else { template(Some(item)) })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// UTF-16 length (`string.length`).
pub fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

/// `string.slice(0, end)` in UTF-16 code units (a split surrogate pair is dropped).
pub fn slice_utf16(value: &str, end: usize) -> String {
    let mut out = String::new();
    let mut units = 0usize;
    for c in value.chars() {
        let width = c.len_utf16();
        if units + width > end {
            break;
        }
        units += width;
        out.push(c);
    }
    out
}

/// JS truthiness.
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `typeof value === "string" && value.length > 0`, returning it.
pub fn non_empty_str(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// A finite JS number (`typeof v === "number" && Number.isFinite(v)`).
pub fn finite(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).filter(|v| v.is_finite())
}

/// `typeof value === "object" && value !== null && !Array.isArray(value)`.
pub fn as_record(value: Option<&Value>) -> Option<&serde_json::Map<String, Value>> {
    value.and_then(Value::as_object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_numbers_like_javascript() {
        for (value, expected) in [
            (0.0, "0"),
            (1.0, "1"),
            (-12.5, "-12.5"),
            (0.15443679999999999, "0.15443679999999999"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (123456789012345680000.0, "123456789012345680000"),
            (0.000001, "0.000001"),
            (2.5e-8, "2.5e-8"),
        ] {
            assert_eq!(number_to_string(value), expected, "{value}");
        }
    }

    #[test]
    fn stringifies_like_json_stringify() {
        assert_eq!(
            stringify(&json!({"b": 1, "a": [1.5, "x\n", null], "c": 1.0})),
            r#"{"b":1,"a":[1.5,"x\n",null],"c":1}"#
        );
        assert_eq!(template(None), "undefined");
        assert_eq!(template(Some(&json!(3))), "3");
        assert_eq!(slice_utf16("a😀b", 2), "a");
        assert_eq!(utf16_len("a😀b"), 4);
    }
}
