//! JavaScript string and JSON semantics the reactors reproduce: `String.prototype.trim`,
//! UTF-16 `length` / `slice`, truthiness of optional JSON fields, object spreads.

use serde_json::{Map, Value};

/// `WhiteSpace` and `LineTerminator` as `String.prototype.trim` sees them.
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

/// `value.slice(0, units)` in UTF-16 units. A cut through a surrogate pair keeps the pair out
/// (JS would keep a lone surrogate, which Rust strings cannot hold).
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

/// `value.slice(-units)` in UTF-16 units (the same surrogate caveat as [`slice_head16`]).
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

/// `truncateDetail(value, limit)`: `value.slice(0, limit - 3) + "..."` past `limit` units.
pub fn truncate_detail(value: &str, limit: usize) -> String {
    if len16(value) > limit {
        format!("{}...", slice_head16(value, limit.saturating_sub(3)))
    } else {
        value.to_owned()
    }
}

/// JS truthiness of a decoded JSON value (`undefined` is `None`).
pub fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|n| n != 0.0 && !n.is_nan()),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

/// `value[key]` as a string.
pub fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// `value[key]`, with JSON `null` read as absent (`undefined`).
pub fn defined<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key).filter(|field| !field.is_null())
}

/// `value[key]` present at all (the JS `!== undefined` on decoded JSON, where `null` counts as
/// present).
pub fn present<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get(key)
}

/// An object literal builder that mirrors `...(cond ? { key: value } : {})` spreads.
#[derive(Debug, Default, Clone)]
pub struct Obj(pub Map<String, Value>);

impl Obj {
    pub fn new() -> Self {
        Self(Map::new())
    }

    pub fn set(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.0.insert(key.to_owned(), value.into());
        self
    }

    pub fn set_if(self, condition: bool, key: &str, value: impl FnOnce() -> Value) -> Self {
        if condition {
            self.set(key, value())
        } else {
            self
        }
    }

    /// `...(source[key] ? { key: source[key] } : {})`.
    pub fn copy_truthy(self, source: &Value, key: &str) -> Self {
        match source.get(key) {
            Some(value) if truthy(Some(value)) => self.set(key, value.clone()),
            _ => self,
        }
    }

    /// `...(source[key] !== undefined ? { key: source[key] } : {})`.
    pub fn copy_defined(self, source: &Value, key: &str) -> Self {
        match source.get(key) {
            Some(value) => self.set(key, value.clone()),
            None => self,
        }
    }

    /// `...spread` of an object value (later keys win, existing order kept for overwritten
    /// keys, as in JS).
    pub fn spread(mut self, source: &Value) -> Self {
        if let Some(object) = source.as_object() {
            for (key, value) in object {
                self.0.insert(key.clone(), value.clone());
            }
        }
        self
    }

    pub fn spread_map(mut self, source: Map<String, Value>) -> Self {
        for (key, value) in source {
            self.0.insert(key, value);
        }
        self
    }

    pub fn build(self) -> Value {
        Value::Object(self.0)
    }
}

/// `Date.parse(value)` for the ISO strings the server writes (`None` = `NaN`).
pub fn parse_date_millis(value: &str) -> Option<i64> {
    zc_core::time::parse_iso_millis(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn utf16_slices_and_truncation() {
        assert_eq!(len16("a😀"), 3);
        assert_eq!(slice_head16("a😀b", 2), "a");
        assert_eq!(slice_head16("a😀b", 3), "a😀");
        assert_eq!(slice_tail16("a😀b", 1), "b");
        assert_eq!(slice_tail16("a😀b", 3), "😀b");
        assert_eq!(truncate_detail("abcdef", 5), "ab...");
        assert_eq!(truncate_detail("abcde", 5), "abcde");
    }

    #[test]
    fn trim_follows_javascript() {
        assert_eq!(trim("\u{feff} x \u{a0}"), "x");
        assert_eq!(trim("\u{85}x"), "\u{85}x");
    }

    #[test]
    fn obj_spreads_like_object_literals() {
        let source = json!({"a": 1, "b": "", "c": null});
        let built = Obj::new()
            .copy_truthy(&source, "a")
            .copy_truthy(&source, "b")
            .copy_defined(&source, "c")
            .build();
        assert_eq!(built, json!({"a": 1, "c": null}));
    }
}
