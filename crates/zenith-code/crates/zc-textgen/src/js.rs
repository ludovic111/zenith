//! The JavaScript string semantics the prompts and sanitizers reproduce: `String.prototype.trim`,
//! UTF-16 `length` / `slice`, `split(/\r?\n/)[0]`, `JSON.stringify` of a string.

/// `WhiteSpace` and `LineTerminator` as `trim` and the regex `\s` see them.
pub fn is_js_whitespace(character: char) -> bool {
    matches!(
        character,
        '\u{0009}' | '\u{000a}' | '\u{000b}' | '\u{000c}' | '\u{000d}' | '\u{0020}' | '\u{00a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `value.trim()`.
pub fn trim(value: &str) -> &str {
    value.trim_matches(is_js_whitespace)
}

/// `value.trimEnd()`.
pub fn trim_end(value: &str) -> &str {
    value.trim_end_matches(is_js_whitespace)
}

/// `value.length` (UTF-16 code units).
pub fn len16(value: &str) -> usize {
    value.encode_utf16().count()
}

/// `value.slice(0, units)` in UTF-16 units. A cut through a surrogate pair leaves the pair out
/// (JS would keep a lone surrogate, which a Rust string cannot hold).
pub fn slice_head16(value: &str, units: usize) -> &str {
    let mut count = 0;
    for (index, character) in value.char_indices() {
        let width = character.len_utf16();
        if count + width > units {
            return &value[..index];
        }
        count += width;
    }
    value
}

/// `value.slice(-units)` in UTF-16 units (same surrogate caveat as [`slice_head16`]).
pub fn slice_tail16(value: &str, units: usize) -> &str {
    let total = len16(value);
    if units >= total {
        return value;
    }
    let skip = total - units;
    let mut count = 0;
    for (index, character) in value.char_indices() {
        if count >= skip {
            return &value[index..];
        }
        count += character.len_utf16();
    }
    ""
}

/// `value.split(/\r?\n/g)[0]`.
pub fn first_line(value: &str) -> &str {
    match value.find('\n') {
        Some(index) => value[..index].strip_suffix('\r').unwrap_or(&value[..index]),
        None => value,
    }
}

/// `JSON.stringify(value)` for a string.
pub fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// A JSON value as a JS template literal prints it (`${value}`): strings bare, numbers in JS
/// form, `undefined` for absent.
pub fn template_value(value: Option<&serde_json::Value>) -> String {
    match value {
        None => "undefined".to_owned(),
        Some(serde_json::Value::Null) => "null".to_owned(),
        Some(serde_json::Value::String(text)) => text.clone(),
        Some(serde_json::Value::Number(number)) => match number.as_f64() {
            Some(float) if float.fract() == 0.0 && float.abs() < 1e21 => format!("{}", float as i64),
            _ => number.to_string(),
        },
        Some(serde_json::Value::Bool(flag)) => flag.to_string(),
        Some(serde_json::Value::Array(items)) => items.iter().map(|item| template_value(Some(item))).collect::<Vec<_>>().join(","),
        Some(serde_json::Value::Object(_)) => "[object Object]".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slices_by_utf16_units() {
        assert_eq!(len16("a\u{1F600}b"), 4);
        assert_eq!(slice_head16("a\u{1F600}b", 2), "a");
        assert_eq!(slice_head16("a\u{1F600}b", 3), "a\u{1F600}");
        assert_eq!(slice_tail16("abcdef", 2), "ef");
        assert_eq!(slice_tail16("ab", 5), "ab");
    }

    #[test]
    fn takes_the_first_line() {
        assert_eq!(first_line("one\r\ntwo"), "one");
        assert_eq!(first_line("one\ntwo"), "one");
        assert_eq!(first_line("one\rtwo"), "one\rtwo");
        assert_eq!(trim("\u{feff} x \u{3000}"), "x");
    }

    #[test]
    fn prints_values_like_template_literals() {
        assert_eq!(template_value(Some(&serde_json::json!(12345))), "12345");
        assert_eq!(template_value(Some(&serde_json::json!("image/png"))), "image/png");
        assert_eq!(template_value(None), "undefined");
    }
}
