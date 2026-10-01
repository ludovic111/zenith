//! The local file tracer (`makeLocalFileTracer` + `Logger.tracerLogger`) as a
//! `tracing-subscriber` layer: every closed span at or above the trace level becomes an
//! `effect-span` record, and every log event inside it a span event carrying
//! `effect.logLevel` (`INFO`, `WARN`, `ERROR`, …), which `server.getTraceDiagnostics` counts.
//!
//! Span fields become attributes, except these conventions:
//! - `otel.name`: the record's name (`tracing` span names are static);
//! - `otel.kind`: the record's kind (`internal` by default);
//! - `exit.tag` (`Success`, `Failure`, `Interrupted`) and `exit.cause`: the record's `exit`.
//!
//! Records go to the target [`install`]ed once the server's configuration is known; before
//! that (and in CLI commands) spans cost one atomic load.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Level, Metadata, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

use super::record::{EffectSpanRecord, SpanExit, TraceEvent};
use super::sink::TraceSink;

/// Where finished spans go.
pub struct TraceTarget {
    pub sink: TraceSink,
    /// The opt-in OTLP export of server spans (`T3CODE_OTLP_TRACES_URL` or `OTEL_*`).
    pub exporter: Option<super::otlp::SpanExporter>,
}

static TARGET: RwLock<Option<Arc<TraceTarget>>> = RwLock::new(None);
/// 0 = off, 1 = ERROR … 5 = TRACE (spans below are not recorded).
static SPAN_LEVEL: AtomicU8 = AtomicU8::new(3);
/// Log events kept as span events (`ServerConfig.logLevel`).
static EVENT_LEVEL: AtomicU8 = AtomicU8::new(3);

fn level_rank(level: &Level) -> u8 {
    match *level {
        Level::ERROR => 1,
        Level::WARN => 2,
        Level::INFO => 3,
        Level::DEBUG => 4,
        Level::TRACE => 5,
    }
}

/// An Effect log level name (`All`, `Trace`, `Debug`, `Info`, `Warn`, `Error`, `Fatal`, `None`)
/// as a rank for [`set_levels`].
pub fn effect_level_rank(name: &str) -> u8 {
    match name {
        "All" | "Trace" => 5,
        "Debug" => 4,
        "Info" => 3,
        "Warn" => 2,
        "Error" | "Fatal" => 1,
        _ => 0,
    }
}

/// `T3CODE_TRACE_MIN_LEVEL` and the log level, as [`effect_level_rank`]s.
pub fn set_levels(span_rank: u8, event_rank: u8) {
    SPAN_LEVEL.store(span_rank, Ordering::Relaxed);
    EVENT_LEVEL.store(event_rank, Ordering::Relaxed);
}

/// Starts recording into `target` (replacing any previous one, which is closed).
pub fn install(target: TraceTarget) {
    let previous = TARGET.write().unwrap_or_else(|p| p.into_inner()).replace(Arc::new(target));
    if let Some(previous) = previous {
        previous.sink.close();
    }
}

/// Stops recording; the sink is flushed and closed.
pub fn uninstall() {
    let previous = TARGET.write().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(previous) = previous {
        previous.sink.close();
        if let Some(exporter) = &previous.exporter {
            exporter.flush_now();
        }
    }
}

/// The installed target, if any.
pub fn current_target() -> Option<Arc<TraceTarget>> {
    TARGET.read().unwrap_or_else(|p| p.into_inner()).clone()
}

fn installed() -> bool {
    TARGET.read().map(|t| t.is_some()).unwrap_or(false)
}

fn unix_nanos() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}

fn random_hex<const N: usize>() -> String {
    let bytes: [u8; N] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether the layer wants this callsite (used as its per-layer filter).
pub fn enabled(metadata: &Metadata<'_>) -> bool {
    let rank = level_rank(metadata.level());
    if metadata.is_span() {
        rank <= SPAN_LEVEL.load(Ordering::Relaxed)
    } else {
        rank <= EVENT_LEVEL.load(Ordering::Relaxed)
    }
}

struct SpanData {
    name: String,
    kind: String,
    trace_id: String,
    span_id: String,
    parent_span_id: Option<String>,
    start: u128,
    attributes: Map<String, Value>,
    events: Vec<TraceEvent>,
    exit_tag: Option<String>,
    exit_cause: Option<String>,
}

struct FieldVisitor<'a> {
    fields: &'a mut Map<String, Value>,
}

impl Visit for FieldVisitor<'_> {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields.insert(field.name().to_owned(), serde_json::json!(value));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.insert(field.name().to_owned(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.insert(field.name().to_owned(), value.into());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.insert(field.name().to_owned(), value.into());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields.insert(field.name().to_owned(), value.into());
    }
    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.fields.insert(field.name().to_owned(), value.to_string().into());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(field.name().to_owned(), format!("{value:?}").into());
    }
}

impl SpanData {
    fn absorb(&mut self, mut fields: Map<String, Value>) {
        let take_string = |fields: &mut Map<String, Value>, key: &str| {
            fields.remove(key).map(|v| match v {
                Value::String(s) => s,
                other => other.to_string(),
            })
        };
        if let Some(name) = take_string(&mut fields, "otel.name") {
            self.name = name;
        }
        if let Some(kind) = take_string(&mut fields, "otel.kind") {
            self.kind = kind;
        }
        if let Some(tag) = take_string(&mut fields, "exit.tag") {
            self.exit_tag = Some(tag);
        }
        if let Some(cause) = take_string(&mut fields, "exit.cause") {
            self.exit_cause = Some(cause);
        }
        self.attributes.append(&mut fields);
    }

    fn into_record(self, end: u128) -> EffectSpanRecord {
        let cause = self.exit_cause.unwrap_or_else(|| "Failure".to_owned());
        let exit = match self.exit_tag.as_deref() {
            Some("Failure") => SpanExit::Failure(cause),
            Some("Interrupted") => SpanExit::Interrupted(cause),
            _ => SpanExit::Success,
        };
        EffectSpanRecord {
            name: self.name,
            trace_id: self.trace_id,
            span_id: self.span_id,
            parent_span_id: self.parent_span_id,
            kind: match self.kind.as_str() {
                "server" => "server",
                "client" => "client",
                "producer" => "producer",
                "consumer" => "consumer",
                _ => "internal",
            },
            start_unix_nano: self.start,
            end_unix_nano: end,
            attributes: self.attributes,
            events: self.events,
            exit,
        }
    }
}

/// The layer; add it to the registry with the [`enabled`] filter (see [`crate::logging`]).
#[derive(Debug, Default, Clone, Copy)]
pub struct TraceLayer;

impl<S> Layer<S> for TraceLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        if !installed() {
            return;
        }
        let Some(span) = ctx.span(id) else { return };
        let parent = span
            .parent()
            .and_then(|parent| parent.extensions().get::<SpanData>().map(|data| (data.trace_id.clone(), data.span_id.clone())));
        let (trace_id, parent_span_id) = match parent {
            Some((trace_id, span_id)) => (trace_id, Some(span_id)),
            None => (random_hex::<16>(), None),
        };
        let mut fields = Map::new();
        attrs.record(&mut FieldVisitor { fields: &mut fields });
        let mut data = SpanData {
            name: attrs.metadata().name().to_owned(),
            kind: "internal".to_owned(),
            trace_id,
            span_id: random_hex::<8>(),
            parent_span_id,
            start: unix_nanos(),
            attributes: Map::new(),
            events: Vec::new(),
            exit_tag: None,
            exit_cause: None,
        };
        data.absorb(fields);
        span.extensions_mut().insert(data);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut extensions = span.extensions_mut();
        let Some(data) = extensions.get_mut::<SpanData>() else { return };
        let mut fields = Map::new();
        values.record(&mut FieldVisitor { fields: &mut fields });
        data.absorb(fields);
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.event_span(event) else { return };
        let mut extensions = span.extensions_mut();
        let Some(data) = extensions.get_mut::<SpanData>() else { return };
        let mut fields = Map::new();
        event.record(&mut FieldVisitor { fields: &mut fields });
        let name = match fields.remove("message") {
            Some(Value::String(message)) => message,
            Some(other) => other.to_string(),
            None => event.metadata().name().to_owned(),
        };
        fields.insert("effect.logLevel".into(), event.metadata().level().as_str().into());
        data.events.push(TraceEvent {
            name,
            time_unix_nano: unix_nanos(),
            attributes: fields,
        });
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(data) = span.extensions_mut().remove::<SpanData>() else { return };
        let Some(target) = current_target() else { return };
        let record = data.into_record(unix_nanos());
        target.sink.push(&record.to_json());
        if let Some(exporter) = &target.exporter {
            exporter.push(&record);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trace::sink::TraceSinkOptions;
    use std::time::Duration;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn writes_nested_spans_to_disk_and_captures_log_messages_as_span_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.trace.ndjson");
        let sink = TraceSink::open(TraceSinkOptions {
            file_path: path.clone(),
            max_bytes: 1 << 20,
            max_files: 2,
            batch_window: Duration::from_secs(10),
            on_flush: None,
        })
        .unwrap();
        let subscriber = tracing_subscriber::registry().with(TraceLayer.with_filter(tracing_subscriber::filter::filter_fn(enabled)));
        install(TraceTarget { sink, exporter: None });
        tracing::subscriber::with_default(subscriber, || {
            let outer = tracing::info_span!(
                "ws.rpc",
                otel.name = "ws.rpc.server.getConfig",
                rpc.method = "server.getConfig",
                exit.tag = tracing::field::Empty
            );
            let _outer = outer.enter();
            {
                let inner = tracing::info_span!("inner", attempt = 2_u64);
                let _inner = inner.enter();
                tracing::warn!(reason = "slow", "status delayed");
                tracing::debug!("not kept below the log level");
            }
            outer.record("exit.tag", "Failure");
            tracing::trace_span!("too verbose").in_scope(|| {});
        });
        uninstall();
        let records: Vec<Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(records.len(), 2);
        let (inner, outer) = (&records[0], &records[1]);
        assert_eq!(inner["name"], "inner");
        assert_eq!(inner["attributes"]["attempt"], 2);
        assert_eq!(inner["traceId"], outer["traceId"]);
        assert_eq!(inner["parentSpanId"], outer["spanId"]);
        assert_eq!(inner["events"][0]["name"], "status delayed");
        assert_eq!(inner["events"][0]["attributes"]["effect.logLevel"], "WARN");
        assert_eq!(inner["events"][0]["attributes"]["reason"], "slow");
        assert_eq!(inner["events"].as_array().unwrap().len(), 1);
        assert_eq!(inner["exit"]["_tag"], "Success");
        assert_eq!(outer["type"], "effect-span");
        assert_eq!(outer["name"], "ws.rpc.server.getConfig");
        assert_eq!(outer["attributes"]["rpc.method"], "server.getConfig");
        assert!(outer["attributes"].get("otel.name").is_none());
        assert_eq!(outer["exit"], serde_json::json!({"_tag": "Failure", "cause": "Failure"}));
        assert_eq!(outer["traceId"].as_str().unwrap().len(), 32);
        assert_eq!(outer["spanId"].as_str().unwrap().len(), 16);
        assert!(outer.get("parentSpanId").is_none());
    }
}
