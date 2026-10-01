//! Everything this crate serves, built from the server configuration, and its WS RPC methods.
//!
//! | Method | Kind | Scope | Behaviour |
//! |---|---|---|---|
//! | `server.getTraceDiagnostics` | unary | `orchestration:read` | [`crate::trace::diagnostics::read_trace_diagnostics`] |
//! | `server.getProcessDiagnostics` | unary | `orchestration:read` | [`crate::diagnostics::read_process_diagnostics`] |
//! | `server.getHostResources` | unary | `orchestration:read` | [`crate::HostResources::read`] |
//! | `server.getProcessResourceHistory` | unary | `orchestration:read` | [`crate::diagnostics::read_process_resource_history`] |
//! | `server.getResourceTelemetryHistory` | unary | `orchestration:read` | [`crate::ResourceTelemetry::read_history`] |
//! | `server.retryResourceTelemetry` | unary | `orchestration:operate` | [`crate::ResourceTelemetry::retry`] |
//! | `server.signalProcess` | unary | `orchestration:operate` | [`crate::diagnostics::signal_process`] |
//! | `subscribeResourceTelemetry` | stream | `orchestration:read` | the latest snapshot, then its changes |
//!
//! Scopes come from the router's scope table. A payload that does not decode dies with the
//! decode error, like the TS schema decode failure.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use zc_contracts::{DateTimeUtc, ResourceTelemetryHistoryInput, ServerProcessResourceHistoryInput, ServerSignalProcessInput};
use zc_rpc::{RpcError, RpcRouterBuilder};

use crate::attribution::{AttributionRecord, ResourceAttribution};
use crate::desktop::NoDesktopTelemetry;
use crate::diagnostics::{kill_process, read_process_diagnostics, read_process_resource_history, signal_process};
use crate::host_resources::HostResources;
use crate::native::NativeTelemetryClient;
use crate::telemetry::ResourceTelemetry;
use crate::trace::diagnostics::read_trace_diagnostics;
use crate::trace::layer::{self, TraceTarget};
use crate::trace::otlp::{self, ExportTarget, SpanExporter};
use crate::trace::sink::{TraceSink, TraceSinkOptions};

pub const SERVER_GET_TRACE_DIAGNOSTICS: &str = "server.getTraceDiagnostics";
pub const SERVER_GET_PROCESS_DIAGNOSTICS: &str = "server.getProcessDiagnostics";
pub const SERVER_GET_HOST_RESOURCES: &str = "server.getHostResources";
pub const SERVER_GET_PROCESS_RESOURCE_HISTORY: &str = "server.getProcessResourceHistory";
pub const SERVER_GET_RESOURCE_TELEMETRY_HISTORY: &str = "server.getResourceTelemetryHistory";
pub const SERVER_RETRY_RESOURCE_TELEMETRY: &str = "server.retryResourceTelemetry";
pub const SERVER_SIGNAL_PROCESS: &str = "server.signalProcess";
pub const SUBSCRIBE_RESOURCE_TELEMETRY: &str = "subscribeResourceTelemetry";

/// Methods whose calls are not traced (`RPC_METHODS_WITH_TRACING_DISABLED`): reading the trace
/// file must not grow it.
pub const UNTRACED_METHODS: &[&str] = &[
    SERVER_GET_TRACE_DIAGNOSTICS,
    SERVER_GET_PROCESS_DIAGNOSTICS,
    SERVER_GET_PROCESS_RESOURCE_HISTORY,
    SERVER_SIGNAL_PROCESS,
];

/// What [`Telemetry::start`] needs.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// `web` or `desktop` (the desktop telemetry health message names it).
    pub mode: String,
    pub trace_file_path: PathBuf,
    pub trace_max_bytes: i64,
    pub trace_max_files: i64,
    pub trace_batch_window_ms: i64,
    /// Install the trace file writer (off in tests that run several servers in one process).
    pub write_traces: bool,
    /// Where server and browser spans are exported, when a collector is configured.
    pub otlp_traces: Option<ExportTarget>,
    pub otlp_resource_attributes: std::collections::BTreeMap<String, String>,
}

/// Resource telemetry, host resources, diagnostics and the trace file.
pub struct Telemetry {
    pub resources: Arc<ResourceTelemetry>,
    pub native: NativeTelemetryClient,
    pub host: HostResources,
    pub attribution: ResourceAttribution,
    trace_file_path: PathBuf,
    trace_max_files: i64,
    otlp_traces: Option<ExportTarget>,
    http: reqwest::Client,
    installed_trace_target: bool,
}

fn decode<T: DeserializeOwned>(payload: Value) -> Result<T, RpcError> {
    serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))
}

fn encode<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::die(format!("could not encode the result: {e}")))
}

/// What `POST /api/observability/v1/traces` answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserTracesOutcome {
    /// 204.
    Accepted,
    /// 502 `Trace export failed.`: the configured collector refused the forward.
    ExportFailed,
}

impl Telemetry {
    /// Starts the resource monitor and, with `write_traces`, the trace file writer.
    pub fn start(config: TelemetryConfig) -> Arc<Self> {
        let attribution = ResourceAttribution::new();
        let server_pid = std::process::id();
        let native = NativeTelemetryClient::start(server_pid);
        let resources = ResourceTelemetry::start(
            Arc::new(native.clone()),
            Arc::new(NoDesktopTelemetry { mode: config.mode.clone() }),
            attribution.clone(),
            i64::from(server_pid),
        );
        let mut installed = false;
        if config.write_traces {
            let flush_attribution = attribution.clone();
            let sink = TraceSink::open(TraceSinkOptions {
                file_path: config.trace_file_path.clone(),
                max_bytes: config.trace_max_bytes.max(1) as u64,
                max_files: config.trace_max_files.max(1) as u64,
                batch_window: Duration::from_millis(config.trace_batch_window_ms.max(1) as u64),
                on_flush: Some(Box::new(move |stats| {
                    flush_attribution.record(AttributionRecord {
                        component: "server-trace".into(),
                        operation: "append".into(),
                        logical_read_bytes: None,
                        logical_write_bytes: Some(stats.logical_write_bytes as f64),
                        count: Some(stats.count as f64),
                        duration_ms: Some(stats.duration_ms),
                    });
                })),
            });
            match sink {
                Ok(sink) => {
                    let exporter = config
                        .otlp_traces
                        .clone()
                        .map(|target| SpanExporter::start(target, otlp::resource(&config.mode, &config.otlp_resource_attributes)));
                    layer::install(TraceTarget { sink, exporter });
                    installed = true;
                }
                Err(error) => tracing::warn!(%error, path = %config.trace_file_path.display(), "could not open the local trace file"),
            }
        }
        Arc::new(Self {
            resources,
            native,
            host: HostResources::new(),
            attribution,
            trace_file_path: config.trace_file_path,
            trace_max_files: config.trace_max_files,
            otlp_traces: config.otlp_traces,
            http: reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default(),
            installed_trace_target: installed,
        })
    }

    /// Stops the monitor and flushes the trace file.
    pub fn shutdown(&self) {
        self.resources.shutdown();
        self.native.shutdown();
        if self.installed_trace_target {
            layer::uninstall();
        }
    }

    /// `server.getTraceDiagnostics`.
    pub async fn trace_diagnostics(&self) -> zc_contracts::ServerTraceDiagnosticsResult {
        let path = self.trace_file_path.clone();
        let max_files = self.trace_max_files;
        // Spans still buffered belong in the answer.
        if let Some(target) = layer::current_target() {
            target.sink.flush();
        }
        let read_at = DateTimeUtc::now();
        tokio::task::spawn_blocking(move || read_trace_diagnostics(&path, max_files, None, read_at))
            .await
            .unwrap_or_else(|_| read_trace_diagnostics(&self.trace_file_path, 0, None, read_at))
    }

    /// `POST /api/observability/v1/traces` once authenticated: the browser's OTLP/JSON spans
    /// go to the trace file, then to the configured collector, if any.
    pub async fn record_browser_traces(&self, body: &Value) -> BrowserTracesOutcome {
        match otlp::decode_otlp_trace_records(body) {
            Ok(records) => {
                if let Some(target) = layer::current_target() {
                    for record in &records {
                        target.sink.push(record);
                    }
                }
            }
            Err(cause) => tracing::warn!(%cause, "Failed to decode browser OTLP traces"),
        }
        let Some(target) = &self.otlp_traces else {
            return BrowserTracesOutcome::Accepted;
        };
        match otlp::post_json(&self.http, target, body).await {
            Ok(()) => BrowserTracesOutcome::Accepted,
            Err(cause) => {
                tracing::warn!(%cause, otlpTracesUrl = %target.url, "Failed to export browser OTLP traces");
                BrowserTracesOutcome::ExportFailed
            }
        }
    }

    /// Adds every method of the table above.
    pub fn register_rpc(self: &Arc<Self>, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        let t = self.clone();
        let builder = builder.unary(SERVER_GET_TRACE_DIAGNOSTICS, move |_ctx, _payload| {
            let t = t.clone();
            async move { encode(&t.trace_diagnostics().await) }
        });
        let t = self.clone();
        let builder = builder.unary(SERVER_GET_PROCESS_DIAGNOSTICS, move |_ctx, _payload| {
            let t = t.clone();
            async move { encode(&read_process_diagnostics(&t.resources).await) }
        });
        let t = self.clone();
        let builder = builder.unary(SERVER_GET_HOST_RESOURCES, move |_ctx, _payload| {
            let t = t.clone();
            async move { encode(&t.host.read().await) }
        });
        let t = self.clone();
        let builder = builder.unary(SERVER_GET_PROCESS_RESOURCE_HISTORY, move |_ctx, payload| {
            let t = t.clone();
            async move {
                let input: ServerProcessResourceHistoryInput = decode(payload)?;
                encode(&read_process_resource_history(&t.resources, input.window_ms, input.bucket_ms).await)
            }
        });
        let t = self.clone();
        let builder = builder.unary(SERVER_GET_RESOURCE_TELEMETRY_HISTORY, move |_ctx, payload| {
            let t = t.clone();
            async move {
                let input: ResourceTelemetryHistoryInput = decode(payload)?;
                encode(&t.resources.read_history(input.window_ms, input.bucket_ms).await.history)
            }
        });
        let t = self.clone();
        let builder = builder.unary(SERVER_RETRY_RESOURCE_TELEMETRY, move |_ctx, _payload| {
            let t = t.clone();
            async move { encode(&t.resources.retry().await) }
        });
        let t = self.clone();
        let builder = builder.unary(SERVER_SIGNAL_PROCESS, move |_ctx, payload| {
            let t = t.clone();
            async move {
                let input: ServerSignalProcessInput = decode(payload)?;
                encode(&signal_process(&t.resources, &input, &kill_process).await)
            }
        });
        let t = self.clone();
        builder.stream(SUBSCRIBE_RESOURCE_TELEMETRY, move |_ctx, _payload| {
            let t = t.clone();
            async move {
                let (latest, changes) = t.resources.subscribe();
                Ok(futures::stream::once(async move { latest }).chain(changes).map(|snapshot| encode(&snapshot)))
            }
        })
    }
}
