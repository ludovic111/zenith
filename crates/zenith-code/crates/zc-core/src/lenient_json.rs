//! Lenient JSON: the port of `packages/shared/src/schemaJson.ts` (`fromLenientJson`,
//! `extractJsonObject`).
//!
//! `settings.json`, `t3.json` and `.t3code/vcs.json` are read with `fromLenientJson`, which strips
//! `//` and `/* */` comments and trailing commas with three regex passes, then runs a strict
//! `JSON.parse`. The passes are reproduced verbatim (same regexes, same order, same
//! string-literal-preserving alternation) so odd inputs strip exactly like the TS server.
//! Encoding back is plain strict JSON (`JSON.stringify`), which is just `serde_json`.

use std::sync::OnceLock;

use regex::{Captures, Regex};
use serde::de::DeserializeOwned;
use serde_json::Value;

/// JS `\s` (WhiteSpace + LineTerminator), spelled out so it matches exactly what V8 matches.
const JS_WHITESPACE: &str = r"[\t\n\x0B\x0C\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}]";
/// JS `.` without the `s` flag: anything but a line terminator.
const JS_DOT: &str = r"[^\n\r\x{2028}\x{2029}]";
/// `("(?:[^"\\]|\\.)*")` with JS `.` semantics.
fn string_literal() -> String {
    format!(r#"("(?:[^"\\]|\\{JS_DOT})*")"#)
}

fn line_comment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"{}|//[^\n]*", string_literal())).expect("valid regex"))
}

fn block_comment_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"{}|/\*(?s:.)*?\*/", string_literal())).expect("valid regex"))
}

fn trailing_comma_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(&format!(r"{}|,({JS_WHITESPACE}*[\}}\]])", string_literal())).expect("valid regex"))
}

/// Why a lenient JSON document did not parse. The message never contains the input (the TS
/// diagnostics deliberately drop values because settings files hold secrets).
#[derive(Debug, thiserror::Error)]
pub enum LenientJsonError {
    /// Not JSON even after stripping comments and trailing commas.
    #[error("Invalid value")]
    Syntax(#[source] serde_json::Error),
    /// The JSON parsed but does not have the expected shape.
    #[error("Invalid type")]
    Decode(#[source] serde_json::Error),
}

/// Strip JSONC comments and trailing commas, exactly like the TS `parseLenientJsonGetter`.
pub fn strip_lenient_json(input: &str) -> String {
    let keep_strings = |caps: &Captures<'_>| -> String {
        if caps.get(1).is_some() {
            caps[0].to_owned()
        } else {
            String::new()
        }
    };
    let stripped = line_comment_regex().replace_all(input, keep_strings);
    let stripped = block_comment_regex().replace_all(&stripped, keep_strings);
    trailing_comma_regex()
        .replace_all(&stripped, |caps: &Captures<'_>| -> String {
            if caps.get(1).is_some() {
                caps[0].to_owned()
            } else {
                caps.get(2).map(|m| m.as_str().to_owned()).unwrap_or_default()
            }
        })
        .into_owned()
}

/// `Schema.decode(fromLenientJson(Schema.Unknown))`: lenient text to a JSON value.
pub fn parse_lenient_json(input: &str) -> Result<Value, LenientJsonError> {
    serde_json::from_str(&strip_lenient_json(input)).map_err(LenientJsonError::Syntax)
}

/// `fromLenientJson(schema)`: lenient text decoded into `T`.
pub fn from_lenient_json<T: DeserializeOwned>(input: &str) -> Result<T, LenientJsonError> {
    let value = parse_lenient_json(input)?;
    serde_json::from_value(value).map_err(LenientJsonError::Decode)
}

/// `extractJsonObject`: the first balanced `{…}` in `raw` (tolerating prose and code fences
/// around it), or the trimmed input when there is no `{`. An unbalanced object returns the tail
/// from the first `{`.
pub fn extract_json_object(raw: &str) -> &str {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return trimmed;
    }
    let Some(start) = trimmed.find('{') else {
        return trimmed;
    };
    let mut depth: i64 = 0;
    let mut in_string = false;
    let mut escaping = false;
    for (offset, ch) in trimmed[start..].char_indices() {
        if in_string {
            if escaping {
                escaping = false;
            } else if ch == '\\' {
                escaping = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &trimmed[start..start + offset + 1];
                }
            }
            _ => {}
        }
    }
    &trimmed[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `[input, stripped]` pairs produced by running the TS regex passes on Node 26.
    const NODE_FIXTURES: &[(&str, &str)] = &[
        (
            "{\n  // c\n  \"enabled\": true,\n  \"values\": [1, 2,],\n}",
            "{\n  \n  \"enabled\": true,\n  \"values\": [1, 2]\n}",
        ),
        ("{\"note\":\"a,]\"}", "{\"note\":\"a,]\"}"),
        (
            "{\"url\":\"http://x//y\", /* block */ \"a\": 1 /* multi\nline */,}",
            "{\"url\":\"http://x//y\",  \"a\": 1 }",
        ),
        ("{\"a\":\"\\\"//not\"} // tail", "{\"a\":\"\\\"//not\"} "),
        ("{\"a\": 1, /* \"quoted */ \"b\": 2}", "{\"a\": 1,  \"b\": 2}"),
        ("[1,2,\n]", "[1,2\n]"),
        ("{\"s\":\"x\\\\\"} // c", "{\"s\":\"x\\\\\"} "),
    ];

    #[test]
    fn strips_exactly_like_the_ts_regex_passes() {
        for (input, expected) in NODE_FIXTURES {
            assert_eq!(strip_lenient_json(input), *expected, "input {input:?}");
        }
    }

    #[test]
    fn decodes_comments_and_trailing_commas() {
        let value = parse_lenient_json("{\n  // Comments are valid in settings files.\n  \"enabled\": true,\n  \"values\": [1, 2,],\n}").unwrap();
        assert_eq!(value, json!({"enabled": true, "values": [1, 2]}));
    }

    #[test]
    fn rejects_malformed_json_without_echoing_input() {
        let error = parse_lenient_json("{ \"token\": \"credential=secret-value\",, }").unwrap_err();
        assert_eq!(error.to_string(), "Invalid value");
    }

    #[test]
    fn preserves_commas_inside_strings() {
        assert_eq!(parse_lenient_json("{\"note\":\"a,]\"}").unwrap(), json!({"note": "a,]"}));
        assert_eq!(parse_lenient_json("{\"list\":[\"x,}\"]}").unwrap(), json!({"list": ["x,}"]}));
        assert_eq!(parse_lenient_json("{\"values\":[1, 2,],}").unwrap(), json!({"values": [1, 2]}));
    }

    #[test]
    fn decodes_into_typed_values() {
        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct Settings {
            enabled: bool,
        }
        let settings: Settings = from_lenient_json("{ \"enabled\": false, // x\n }").unwrap();
        assert_eq!(settings, Settings { enabled: false });
        assert!(from_lenient_json::<Settings>("{\"enabled\": 1}").is_err());
    }

    #[test]
    fn extracts_a_balanced_object_from_prose() {
        let raw = "Sure, here is the JSON:\n```json\n{\n  \"subject\": \"Update README\",\n  \"body\": \"}\"\n}\n```\nDone.";
        assert_eq!(extract_json_object(raw), "{\n  \"subject\": \"Update README\",\n  \"body\": \"}\"\n}");
        assert_eq!(extract_json_object("  no braces "), "no braces");
        assert_eq!(extract_json_object("x {\"a\": {"), "{\"a\": {");
    }
}
