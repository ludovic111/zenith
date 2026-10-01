//! OTLP: the browser's spans (`POST /api/observability/v1/traces`, OTLP/JSON) decoded into
//! `otlp-span` trace records (`decodeOtlpTraceRecords`), their forwarding to the configured
//! collector, and the opt-in export of the server's own spans.
//!
//! Nothing leaves the machine unless a collector is configured (`T3CODE_OTLP_TRACES_URL`, an
//! `OTEL_EXPORTER_OTLP_*` endpoint, or `observability.otlpTracesUrl` in settings.json), as in
//! the TS server. Payloads are always OTLP/JSON: the `http/protobuf` protocol is not encoded
//! (the request then still carries JSON, with `content-type: application/json`).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Map, Value};

use super::record::{truncate_trace_attributes, EffectSpanRecord, SpanExit};

/// One decoded browser span (`OtlpTraceRecord`) as its NDJSON object.
fn span_kind(kind: Option<&Value>) -> &'static str {
    match kind.and_then(Value::as_i64) {
        Some(2) => "server",
        Some(3) => "client",
        Some(4) => "producer",
        Some(5) => "consumer",
        _ => "internal",
    }
}

fn decode_value(value: Option<&Value>) -> Value {
    let Some(Value::Object(value)) = value else {
        return Value::Null;
    };
    for key in ["stringValue", "boolValue", "intValue", "doubleValue", "bytesValue"] {
        if let Some(v) = value.get(key) {
            return v.clone();
        }
    }
    if let Some(Value::Object(array)) = value.get("arrayValue") {
        let values = array.get("values").and_then(Value::as_array).cloned().unwrap_or_default();
        return Value::Array(values.iter().map(|v| decode_value(Some(v))).collect());
    }
    if let Some(Value::Object(list)) = value.get("kvlistValue") {
        let values = list.get("values").and_then(Value::as_array).cloned().unwrap_or_default();
        return Value::Object(decode_attributes(&values));
    }
    Value::Null
}

fn decode_attributes(input: &[Value]) -> Map<String, Value> {
    let mut entries = Map::new();
    for attribute in input {
        let Some(key) = attribute.get("key").and_then(Value::as_str) else {
            continue;
        };
        entries.insert(key.to_owned(), decode_value(attribute.get("value")));
    }
    truncate_trace_attributes(entries)
}

fn attributes_of(value: Option<&Value>) -> Map<String, Value> {
    decode_attributes(value.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]))
}

fn parse_nanos(value: Option<&Value>) -> i128 {
    value
        .and_then(|v| match v {
            Value::String(s) => s.trim().parse::<i128>().ok(),
            Value::Number(n) => n.as_i64().map(i128::from),
            _ => None,
        })
        .unwrap_or(0)
}

fn nanos_text(value: Option<&Value>) -> Value {
    match value {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Number(n)) => Value::String(n.to_string()),
        _ => Value::Null,
    }
}

/// `decodeOtlpTraceRecords`. Fails (nothing is recorded) when the payload is not
/// `{resourceSpans: [{scopeSpans: [{scope, spans: [...]}]}]}`.
pub fn decode_otlp_trace_records(payload: &Value) -> Result<Vec<Value>, String> {
    let resource_spans = payload.get("resourceSpans").and_then(Value::as_array).ok_or("resourceSpans is not an array")?;
    let mut records = Vec::new();
    for resource_span in resource_spans {
        let resource_attributes = attributes_of(resource_span.get("resource").and_then(|r| r.get("attributes")));
        let scope_spans = resource_span.get("scopeSpans").and_then(Value::as_array).ok_or("scopeSpans is not an array")?;
        for scope_span in scope_spans {
            let scope = scope_span.get("scope").ok_or("scope is missing")?;
            let spans = scope_span.get("spans").and_then(Value::as_array).ok_or("spans is not an array")?;
            for span in spans {
                let mut scope_record = Map::new();
                if let Some(name) = scope.get("name").and_then(Value::as_str).filter(|n| !n.is_empty()) {
                    scope_record.insert("name".into(), name.into());
                }
                if let Some(version) = scope.get("version").and_then(Value::as_str).filter(|v| !v.is_empty()) {
                    scope_record.insert("version".into(), version.into());
                }
                scope_record.insert("attributes".into(), Value::Object(attributes_of(scope.get("attributes"))));
                let start = parse_nanos(span.get("startTimeUnixNano"));
                let end = parse_nanos(span.get("endTimeUnixNano"));
                let mut record = Map::new();
                record.insert("type".into(), "otlp-span".into());
                record.insert("name".into(), span.get("name").cloned().unwrap_or(Value::Null));
                record.insert("traceId".into(), span.get("traceId").cloned().unwrap_or(Value::Null));
                record.insert("spanId".into(), span.get("spanId").cloned().unwrap_or(Value::Null));
                if let Some(parent) = span.get("parentSpanId").filter(|p| p.as_str().is_some_and(|s| !s.is_empty())) {
                    record.insert("parentSpanId".into(), parent.clone());
                }
                record.insert("sampled".into(), true.into());
                record.insert("kind".into(), span_kind(span.get("kind")).into());
                record.insert("startTimeUnixNano".into(), nanos_text(span.get("startTimeUnixNano")));
                record.insert("endTimeUnixNano".into(), nanos_text(span.get("endTimeUnixNano")));
                record.insert("durationMs".into(), json!((end - start) as f64 / 1_000_000.0));
                record.insert("attributes".into(), Value::Object(attributes_of(span.get("attributes"))));
                record.insert("resourceAttributes".into(), Value::Object(resource_attributes.clone()));
                record.insert("scope".into(), Value::Object(scope_record));
                let events: Vec<Value> = span
                    .get("events")
                    .and_then(Value::as_array)
                    .map(|events| {
                        events
                            .iter()
                            .map(|event| {
                                json!({
                                    "name": event.get("name").cloned().unwrap_or(Value::Null),
                                    "timeUnixNano": nanos_text(event.get("timeUnixNano")),
                                    "attributes": Value::Object(attributes_of(event.get("attributes"))),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                record.insert("events".into(), Value::Array(events));
                let links: Vec<Value> = span
                    .get("links")
                    .and_then(Value::as_array)
                    .map(|links| {
                        links
                            .iter()
                            .map(|link| {
                                json!({
                                    "traceId": link.get("traceId").cloned().unwrap_or(Value::Null),
                                    "spanId": link.get("spanId").cloned().unwrap_or(Value::Null),
                                    "attributes": Value::Object(attributes_of(link.get("attributes"))),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                record.insert("links".into(), Value::Array(links));
                let status = span.get("status");
                let mut status_record = Map::new();
                let code = match status.and_then(|s| s.get("code")) {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Number(n)) => n.to_string(),
                    Some(Value::Bool(b)) => b.to_string(),
                    _ => "undefined".to_owned(),
                };
                status_record.insert("code".into(), code.into());
                if let Some(message) = status.and_then(|s| s.get("message")).and_then(Value::as_str).filter(|m| !m.is_empty()) {
                    status_record.insert("message".into(), message.into());
                }
                record.insert("status".into(), Value::Object(status_record));
                records.push(Value::Object(record));
            }
        }
    }
    Ok(records)
}

/// `OtlpHeadersFromString`: `key=value` pairs joined by commas, percent-encoded values.
pub fn parse_otlp_headers(input: &str) -> Result<BTreeMap<String, String>, String> {
    let mut headers = BTreeMap::new();
    for pair in input.split(',') {
        if pair.trim().is_empty() {
            continue;
        }
        let Some(separator) = pair.find('=') else {
            return Err(format!("Expected key=value but received {:?}.", pair.trim()));
        };
        let key = pair[..separator].trim();
        if key.is_empty() {
            return Err(format!("Expected key=value but received {:?}.", pair.trim()));
        }
        let raw = pair[separator + 1..].trim();
        let value = percent_encoding::percent_decode_str(raw)
            .decode_utf8()
            .map_err(|_| format!("Header {key:?} has a malformed percent-encoded value."))?;
        // `decodeURIComponent` rejects a lone `%` too.
        if raw
            .split('%')
            .skip(1)
            .any(|rest| rest.len() < 2 || !rest[..2].chars().all(|c| c.is_ascii_hexdigit()))
        {
            return Err(format!("Header {key:?} has a malformed percent-encoded value."));
        }
        headers.insert(key.to_owned(), value.into_owned());
    }
    Ok(headers)
}

/// Where one signal exports.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportTarget {
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub export_interval: Duration,
}

/// POSTs an OTLP/JSON payload; `Ok` on a 2xx answer.
pub async fn post_json(client: &reqwest::Client, target: &ExportTarget, body: &Value) -> Result<(), String> {
    let mut request = client.post(&target.url).json(body);
    for (key, value) in &target.headers {
        request = request.header(key, value);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("collector answered {}", response.status()))
    }
}

fn any_value(value: &Value) -> Value {
    match value {
        Value::String(s) => json!({"stringValue": s}),
        Value::Bool(b) => json!({"boolValue": b}),
        Value::Number(n) if n.is_i64() || n.is_u64() => json!({"intValue": n.to_string()}),
        Value::Number(n) => json!({"doubleValue": n.as_f64()}),
        Value::Array(items) => json!({"arrayValue": {"values": items.iter().map(any_value).collect::<Vec<_>>()}}),
        Value::Object(map) => json!({"kvlistValue": {"values": key_values(map)}}),
        Value::Null => json!({}),
    }
}

fn key_values(map: &Map<String, Value>) -> Vec<Value> {
    map.iter().map(|(key, value)| json!({"key": key, "value": any_value(value)})).collect()
}

/// A server span as an OTLP/JSON span.
pub fn otlp_span(record: &EffectSpanRecord) -> Value {
    let kind = match record.kind {
        "server" => 2,
        "client" => 3,
        "producer" => 4,
        "consumer" => 5,
        _ => 1,
    };
    let status = match &record.exit {
        SpanExit::Success => json!({"code": 1}),
        SpanExit::Failure(cause) | SpanExit::Interrupted(cause) => json!({"code": 2, "message": cause}),
    };
    let mut span = json!({
        "traceId": record.trace_id,
        "spanId": record.span_id,
        "name": record.name,
        "kind": kind,
        "startTimeUnixNano": record.start_unix_nano.to_string(),
        "endTimeUnixNano": record.end_unix_nano.to_string(),
        "attributes": key_values(&truncate_trace_attributes(record.attributes.clone())),
        "droppedAttributesCount": 0,
        "events": record.events.iter().map(|event| json!({
            "name": event.name,
            "timeUnixNano": event.time_unix_nano.to_string(),
            "attributes": key_values(&truncate_trace_attributes(event.attributes.clone())),
            "droppedAttributesCount": 0,
        })).collect::<Vec<_>>(),
        "droppedEventsCount": 0,
        "status": status,
        "links": [],
        "droppedLinksCount": 0,
    });
    if let Some(parent) = &record.parent_span_id {
        span["parentSpanId"] = parent.clone().into();
    }
    span
}

/// `otlpResource`: the service identity on every export.
pub fn resource(mode: &str, extra: &BTreeMap<String, String>) -> Value {
    let mut attributes = Map::new();
    for (key, value) in extra {
        attributes.insert(key.clone(), value.clone().into());
    }
    attributes.insert("service.name".into(), "t3code-server".into());
    attributes.insert("service.namespace".into(), "t3code".into());
    attributes.insert("service.runtime".into(), "t3-server".into());
    attributes.insert("service.mode".into(), mode.into());
    json!({"attributes": key_values(&attributes), "droppedAttributesCount": 0})
}

struct ExporterInner {
    target: ExportTarget,
    resource: Value,
    queue: Mutex<Vec<Value>>,
    client: reqwest::Client,
}

/// Batches server spans and POSTs them every export interval (needs a Tokio runtime).
#[derive(Clone)]
pub struct SpanExporter {
    inner: Arc<ExporterInner>,
}

const MAX_QUEUED_SPANS: usize = 4_096;

impl SpanExporter {
    pub fn start(target: ExportTarget, resource: Value) -> Self {
        let exporter = Self {
            inner: Arc::new(ExporterInner {
                target,
                resource,
                queue: Mutex::new(Vec::new()),
                client: reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default(),
            }),
        };
        let weak = Arc::downgrade(&exporter.inner);
        let interval = exporter.inner.target.export_interval.max(Duration::from_millis(100));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let Some(inner) = weak.upgrade() else { return };
                export(&inner).await;
            }
        });
        exporter
    }

    pub fn push(&self, record: &EffectSpanRecord) {
        let mut queue = self.inner.queue.lock().unwrap_or_else(|p| p.into_inner());
        if queue.len() < MAX_QUEUED_SPANS {
            queue.push(otlp_span(record));
        }
    }

    /// Exports what is queued now, in the background.
    pub fn flush_now(&self) {
        let inner = self.inner.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move { export(&inner).await });
        }
    }
}

async fn export(inner: &ExporterInner) {
    let spans = std::mem::take(&mut *inner.queue.lock().unwrap_or_else(|p| p.into_inner()));
    if spans.is_empty() {
        return;
    }
    let body = json!({"resourceSpans": [{
        "resource": inner.resource,
        "scopeSpans": [{"scope": {"name": "zenith-code"}, "spans": spans}],
    }]});
    // Not traced: an export span would feed the next export.
    if let Err(error) = post_json(&inner.client, &inner.target, &body).await {
        eprintln!("zenith-code: failed to export OTLP traces to {}: {error}", inner.target.url);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_oversized_renderer_span_and_event_attributes() {
        let long = "x".repeat(2_000);
        let clamped = format!("{}…[truncated]", "x".repeat(500));
        let records = decode_otlp_trace_records(&json!({
            "resourceSpans": [{
                "resource": {"attributes": [], "droppedAttributesCount": 0},
                "scopeSpans": [{
                    "scope": {"name": "effect"},
                    "spans": [{
                        "traceId": "11111111111111111111111111111111",
                        "spanId": "2222222222222222",
                        "name": "client.span",
                        "kind": 1,
                        "startTimeUnixNano": "1000000",
                        "endTimeUnixNano": "2000000",
                        "attributes": [{"key": "payload", "value": {"stringValue": long}}],
                        "droppedAttributesCount": 0,
                        "events": [{
                            "name": "log",
                            "timeUnixNano": "1500000",
                            "attributes": [{"key": "effect.cause", "value": {"stringValue": long}}],
                            "droppedAttributesCount": 0,
                        }],
                        "droppedEventsCount": 0,
                        "status": {"code": 1},
                        "links": [],
                        "droppedLinksCount": 0,
                    }],
                }],
            }],
        }))
        .unwrap();
        let record = &records[0];
        assert_eq!(record["attributes"]["payload"], clamped);
        assert_eq!(record["events"][0]["attributes"]["effect.cause"], clamped);
        assert_eq!(record["type"], "otlp-span");
        assert_eq!(record["durationMs"], 1.0);
        assert_eq!(record["kind"], "internal");
        assert_eq!(record["scope"], json!({"name": "effect", "attributes": {}}));
        assert_eq!(record["status"], json!({"code": "1"}));
        assert!(record.get("parentSpanId").is_none());
    }

    #[test]
    fn rejects_a_payload_without_resource_spans() {
        assert!(decode_otlp_trace_records(&json!({"spans": []})).is_err());
    }

    #[test]
    fn decodes_nested_values() {
        let records = decode_otlp_trace_records(&json!({"resourceSpans": [{
            "resource": {"attributes": [{"key": "service.name", "value": {"stringValue": "web"}}]},
            "scopeSpans": [{"scope": {"name": "s", "version": "1"}, "spans": [{
                "traceId": "t", "spanId": "s", "parentSpanId": "p", "name": "n", "kind": 3,
                "startTimeUnixNano": "0", "endTimeUnixNano": "5",
                "attributes": [
                    {"key": "list", "value": {"arrayValue": {"values": [{"intValue": "3"}, {"boolValue": true}]}}},
                    {"key": "map", "value": {"kvlistValue": {"values": [{"key": "k", "value": {"doubleValue": 1.5}}]}}},
                    {"key": "none", "value": null}
                ],
                "events": [], "links": [{"traceId": "t2", "spanId": "s2", "attributes": []}],
                "status": {"code": 2, "message": "boom"}
            }]}]
        }]}))
        .unwrap();
        let record = &records[0];
        assert_eq!(record["attributes"]["list"], json!(["3", true]));
        assert_eq!(record["attributes"]["map"], json!({"k": 1.5}));
        assert_eq!(record["attributes"]["none"], Value::Null);
        assert_eq!(record["resourceAttributes"]["service.name"], "web");
        assert_eq!(record["kind"], "client");
        assert_eq!(record["parentSpanId"], "p");
        assert_eq!(record["links"][0]["traceId"], "t2");
        assert_eq!(record["status"], json!({"code": "2", "message": "boom"}));
    }

    #[test]
    fn parses_otlp_headers() {
        let parse = |s: &str| parse_otlp_headers(s).map(|h| h.into_iter().collect::<Vec<_>>());
        assert_eq!(
            parse("authorization=Basic%20abc%3D%3D,x-tenant=t3").unwrap(),
            [("authorization".to_owned(), "Basic abc==".to_owned()), ("x-tenant".to_owned(), "t3".to_owned())]
        );
        assert_eq!(
            parse("authorization=Basic%20abc%3D%3D, x-tenant = t3 ,").unwrap(),
            [("authorization".to_owned(), "Basic abc==".to_owned()), ("x-tenant".to_owned(), "t3".to_owned())]
        );
        assert_eq!(
            parse("authorization=Bearer abc==").unwrap(),
            [("authorization".to_owned(), "Bearer abc==".to_owned())]
        );
        assert_eq!(parse("x-empty=").unwrap(), [("x-empty".to_owned(), String::new())]);
        assert!(parse("authorization").is_err());
        assert!(parse("=value").is_err());
        assert!(parse("authorization=%E0").is_err());
    }

    #[test]
    fn encodes_server_spans_as_otlp_json() {
        let record = EffectSpanRecord {
            name: "ws.rpc.server.getConfig".into(),
            trace_id: "t".into(),
            span_id: "s".into(),
            parent_span_id: Some("p".into()),
            kind: "internal",
            start_unix_nano: 1,
            end_unix_nano: 2,
            attributes: json!({"n": 3, "f": 1.5, "b": true}).as_object().unwrap().clone(),
            events: Vec::new(),
            exit: SpanExit::Failure("boom".into()),
        };
        let span = otlp_span(&record);
        assert_eq!(span["kind"], 1);
        assert_eq!(span["parentSpanId"], "p");
        assert_eq!(span["status"], json!({"code": 2, "message": "boom"}));
        assert!(span["attributes"]
            .as_array()
            .unwrap()
            .contains(&json!({"key": "n", "value": {"intValue": "3"}})));
        assert!(span["attributes"]
            .as_array()
            .unwrap()
            .contains(&json!({"key": "f", "value": {"doubleValue": 1.5}})));
    }
}
