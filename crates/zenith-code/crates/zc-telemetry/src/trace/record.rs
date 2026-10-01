//! The trace record shapes of `packages/shared/src/observability.ts`, one JSON object per line
//! of `server.trace.ndjson`:
//!
//! - `{"type":"effect-span", name, traceId, spanId, parentSpanId?, sampled, kind,
//!   startTimeUnixNano, endTimeUnixNano, durationMs, attributes, events, links, exit}`, the
//!   server's own spans, `exit` being `{"_tag":"Success"}` or `{"_tag":"Failure"|"Interrupted",
//!   "cause"}`;
//! - `{"type":"otlp-span", …, resourceAttributes, scope, status}`, the browser's spans.

use serde_json::{Map, Value};

const TRACE_ATTRIBUTE_MAX_LENGTH: usize = 500;
const TRACE_ATTRIBUTE_TRUNCATED_LENGTH: usize = 200;
const TRACE_ATTRIBUTE_TRUNCATION_SUFFIX: &str = "…[truncated]";
const ALWAYS_TRUNCATED_TRACE_ATTRIBUTES: &[&str] = &["db.query.text"];

/// The first `max` UTF-16 code units of `value` (JavaScript `slice`), or `None` when it fits.
fn js_prefix(value: &str, max: usize) -> Option<&str> {
    let mut units = 0;
    for (index, ch) in value.char_indices() {
        let width = ch.len_utf16();
        if units + width > max {
            return Some(&value[..index]);
        }
        units += width;
    }
    None
}

fn truncate_nested(value: &Value) -> Option<Value> {
    match value {
        Value::String(text) => js_prefix(text, TRACE_ATTRIBUTE_MAX_LENGTH).map(|prefix| Value::String(format!("{prefix}{TRACE_ATTRIBUTE_TRUNCATION_SUFFIX}"))),
        Value::Array(items) => {
            let truncated: Vec<Option<Value>> = items.iter().map(truncate_nested).collect();
            truncated.iter().any(Option::is_some).then(|| {
                Value::Array(
                    truncated
                        .into_iter()
                        .zip(items)
                        .map(|(t, original)| t.unwrap_or_else(|| original.clone()))
                        .collect(),
                )
            })
        }
        Value::Object(map) => {
            let mut changed: Option<Map<String, Value>> = None;
            for (key, entry) in map {
                if let Some(next) = truncate_nested(entry) {
                    changed.get_or_insert_with(|| map.clone()).insert(key.clone(), next);
                }
            }
            changed.map(Value::Object)
        }
        _ => None,
    }
}

/// `truncateTraceAttributes`: strings over 500 UTF-16 units (at any depth) and `db.query.text`
/// over 200 are clamped with `…[truncated]`.
pub fn truncate_trace_attributes(attributes: Map<String, Value>) -> Map<String, Value> {
    let mut attributes = attributes;
    let keys: Vec<String> = attributes.keys().cloned().collect();
    for key in keys {
        let value = &attributes[&key];
        let next = match value {
            Value::String(text) if ALWAYS_TRUNCATED_TRACE_ATTRIBUTES.contains(&key.as_str()) => {
                js_prefix(text, TRACE_ATTRIBUTE_TRUNCATED_LENGTH).map(|prefix| Value::String(format!("{prefix}{TRACE_ATTRIBUTE_TRUNCATION_SUFFIX}")))
            }
            other => truncate_nested(other),
        };
        if let Some(next) = next {
            attributes.insert(key, next);
        }
    }
    attributes
}

/// An `exit` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpanExit {
    Success,
    Failure(String),
    Interrupted(String),
}

impl SpanExit {
    pub fn to_json(&self) -> Value {
        match self {
            Self::Success => serde_json::json!({"_tag": "Success"}),
            Self::Failure(cause) => serde_json::json!({"_tag": "Failure", "cause": cause}),
            Self::Interrupted(cause) => serde_json::json!({"_tag": "Interrupted", "cause": cause}),
        }
    }
}

/// A span event (`timeUnixNano` as a decimal string).
#[derive(Debug, Clone, PartialEq)]
pub struct TraceEvent {
    pub name: String,
    pub time_unix_nano: u128,
    pub attributes: Map<String, Value>,
}

/// An `effect-span` record.
#[derive(Debug, Clone, PartialEq)]
pub struct EffectSpanRecord {
    pub name: String,
    pub trace_id: String,
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub kind: &'static str,
    pub start_unix_nano: u128,
    pub end_unix_nano: u128,
    pub attributes: Map<String, Value>,
    pub events: Vec<TraceEvent>,
    pub exit: SpanExit,
}

impl EffectSpanRecord {
    pub fn duration_ms(&self) -> f64 {
        self.end_unix_nano.saturating_sub(self.start_unix_nano) as f64 / 1_000_000.0
    }

    /// The NDJSON object, key order as the TS writes it.
    pub fn to_json(&self) -> Value {
        let mut record = Map::new();
        record.insert("type".into(), "effect-span".into());
        record.insert("name".into(), self.name.clone().into());
        record.insert("traceId".into(), self.trace_id.clone().into());
        record.insert("spanId".into(), self.span_id.clone().into());
        if let Some(parent) = &self.parent_span_id {
            record.insert("parentSpanId".into(), parent.clone().into());
        }
        record.insert("sampled".into(), true.into());
        record.insert("kind".into(), self.kind.into());
        record.insert("startTimeUnixNano".into(), self.start_unix_nano.to_string().into());
        record.insert("endTimeUnixNano".into(), self.end_unix_nano.to_string().into());
        record.insert("durationMs".into(), serde_json::json!(self.duration_ms()));
        record.insert("attributes".into(), Value::Object(truncate_trace_attributes(self.attributes.clone())));
        record.insert(
            "events".into(),
            Value::Array(
                self.events
                    .iter()
                    .map(|event| {
                        serde_json::json!({
                            "name": event.name,
                            "timeUnixNano": event.time_unix_nano.to_string(),
                            "attributes": Value::Object(truncate_trace_attributes(event.attributes.clone())),
                        })
                    })
                    .collect(),
            ),
        );
        record.insert("links".into(), Value::Array(Vec::new()));
        record.insert("exit".into(), self.exit.to_json());
        Value::Object(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clamps_oversized_strings_at_any_depth_without_mutating_the_input() {
        let stack = "s".repeat(2_000);
        let attributes = json!({
            "db.query.text": "q".repeat(2_000),
            "short": "ok",
            "error": {"name": "Error", "stack": stack, "nested": ["a".repeat(2_000)]},
        });
        let Value::Object(input) = attributes.clone() else { unreachable!() };
        let truncated = truncate_trace_attributes(input);
        let suffix = TRACE_ATTRIBUTE_TRUNCATION_SUFFIX.chars().count();
        assert_eq!(truncated["db.query.text"].as_str().unwrap().chars().count(), 200 + suffix);
        assert_eq!(truncated["short"], "ok");
        assert_eq!(truncated["error"]["stack"].as_str().unwrap().chars().count(), 500 + suffix);
        assert_eq!(truncated["error"]["nested"][0].as_str().unwrap().chars().count(), 500 + suffix);
        assert_eq!(attributes["error"]["stack"].as_str().unwrap().len(), 2_000);
    }

    #[test]
    fn leaves_values_within_the_limits_untouched() {
        let Value::Object(input) = json!({"short": "ok", "nested": {"fine": "also ok"}}) else {
            unreachable!()
        };
        assert_eq!(truncate_trace_attributes(input.clone()), input);
    }

    #[test]
    fn counts_utf16_units_like_javascript() {
        // 300 emoji are 600 UTF-16 units: clamped after 250 of them.
        let Value::Object(input) = json!({"emoji": "🔥".repeat(300)}) else {
            unreachable!()
        };
        let truncated = truncate_trace_attributes(input);
        assert_eq!(
            truncated["emoji"].as_str().unwrap(),
            format!("{}{TRACE_ATTRIBUTE_TRUNCATION_SUFFIX}", "🔥".repeat(250))
        );
    }

    #[test]
    fn writes_the_effect_span_shape() {
        let record = EffectSpanRecord {
            name: "ws.rpc.server.getConfig".into(),
            trace_id: "a".repeat(32),
            span_id: "b".repeat(16),
            parent_span_id: None,
            kind: "internal",
            start_unix_nano: 1_000_000,
            end_unix_nano: 3_500_000,
            attributes: Map::new(),
            events: vec![TraceEvent {
                name: "slow".into(),
                time_unix_nano: 2_000_000,
                attributes: Map::new(),
            }],
            exit: SpanExit::Failure("boom".into()),
        };
        let json = record.to_json();
        assert_eq!(
            json.as_object().unwrap().keys().collect::<Vec<_>>(),
            [
                "type",
                "name",
                "traceId",
                "spanId",
                "sampled",
                "kind",
                "startTimeUnixNano",
                "endTimeUnixNano",
                "durationMs",
                "attributes",
                "events",
                "links",
                "exit"
            ]
        );
        assert_eq!(json["durationMs"], 2.5);
        assert_eq!(json["endTimeUnixNano"], "3500000");
        assert_eq!(json["exit"], json!({"_tag": "Failure", "cause": "boom"}));
        assert_eq!(json["events"][0]["timeUnixNano"], "2000000");
    }
}
