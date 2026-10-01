//! WP-31 (`zc-telemetry`): resource telemetry, host resources, process and trace diagnostics,
//! the trace file and the opt-in OTLP export.
//!
//! - RPC: `server.getTraceDiagnostics`, `server.getProcessDiagnostics`, `server.getHostResources`,
//!   `server.getProcessResourceHistory`, `server.getResourceTelemetryHistory`,
//!   `server.retryResourceTelemetry`, `server.signalProcess`, `subscribeResourceTelemetry`;
//! - route: `POST /api/observability/v1/traces` (browser OTLP/JSON spans, `orchestration:operate`):
//!   into the trace file, then forwarded when a traces collector is configured (204, or 502
//!   `Trace export failed.`);
//! - `ServerConfig.observability` with the endpoints the OTEL variables resolve to;
//! - the trace file writer (`logs/server.trace.ndjson`, rotated) is installed when built and
//!   flushed at shutdown, with the monitor stopped.
//!
//! Nothing is sent anywhere unless a collector is configured (`T3CODE_OTLP_TRACES_URL`, the
//! `OTEL_EXPORTER_OTLP_*` variables, or `observability.otlpTracesUrl` in settings.json), as in
//! TS. Metrics and logs are never exported by the Rust server (a warning says so when an
//! endpoint is configured for them).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::Router;
use serde_json::{Map, Value};
use zc_auth::EnvironmentAuth;
use zc_core::config::LogLevel;
use zc_http::EnvironmentError;
use zc_rpc::RpcRouterBuilder;
use zc_settings::config::{observability, ConfigError, ConfigOptions, SnapshotContributor};
use zc_telemetry::otel_env::{self, resolve_signal_endpoint, SignalExport, SignalName};
use zc_telemetry::trace::otlp::{parse_otlp_headers, ExportTarget};
use zc_telemetry::{BrowserTracesOutcome, Telemetry, TelemetryConfig};

use crate::app::{AppState, Plugin};

/// The OTLP proxy route of `http.ts`.
pub const OTLP_TRACES_PROXY_PATH: &str = "/api/observability/v1/traces";
const MAX_BROWSER_TRACE_BYTES: usize = 16 * 1024 * 1024;

pub struct TelemetryPlugin {
    telemetry: Arc<Telemetry>,
    auth: Arc<EnvironmentAuth>,
    observability: Value,
}

fn level_rank(level: LogLevel) -> u8 {
    match level {
        LogLevel::All | LogLevel::Trace => 5,
        LogLevel::Debug => 4,
        LogLevel::Info => 3,
        LogLevel::Warn => 2,
        LogLevel::Error | LogLevel::Fatal => 1,
        LogLevel::None => 0,
    }
}

impl TelemetryPlugin {
    pub fn new(state: &Arc<AppState>) -> Self {
        let config = &state.config;
        zc_telemetry::trace::layer::set_levels(level_rank(config.trace_min_level), level_rank(config.log_level));

        let otel = otel_env::load(&|name| std::env::var(name).ok());
        for warning in &otel.warnings {
            tracing::warn!("{warning}");
        }
        let headers = config.otlp_headers.as_deref().and_then(|raw| match parse_otlp_headers(raw) {
            Ok(headers) => Some(headers),
            Err(error) => {
                tracing::warn!(%error, "T3CODE_OTLP_HEADERS is not a list of key=value pairs and was ignored");
                None
            }
        });
        let t3_export = SignalExport {
            protocol: config.otlp_protocol.clone(),
            headers,
            export_interval_ms: config.otlp_export_interval_ms,
        };
        // zc-core folds `T3CODE_OTLP_*_URL` and settings.json together; the variable is read again
        // here so the OTEL endpoints rank between the two, as in TS.
        let resolve = |signal, variable: &str, fallback: Option<&str>| {
            let own = std::env::var(variable).ok();
            resolve_signal_endpoint(&otel, signal, own.as_deref(), &t3_export, &[fallback])
        };
        let traces = resolve(SignalName::Traces, "T3CODE_OTLP_TRACES_URL", config.otlp_traces_url.as_deref());
        for (name, endpoint) in [
            (
                "metrics",
                resolve(SignalName::Metrics, "T3CODE_OTLP_METRICS_URL", config.otlp_metrics_url.as_deref()),
            ),
            ("logs", resolve(SignalName::Logs, "T3CODE_OTLP_LOGS_URL", config.otlp_logs_url.as_deref())),
        ] {
            if let Some(endpoint) = endpoint {
                tracing::warn!(url = %endpoint.url, "an OTLP {name} endpoint is configured, but the Rust server only exports traces");
            }
        }
        if traces.as_ref().is_some_and(|t| t.export.protocol != "http/json") {
            tracing::info!("OTLP traces are exported as http/json");
        }
        let otlp_traces = traces.as_ref().map(|endpoint| ExportTarget {
            url: endpoint.url.clone(),
            headers: endpoint.export.headers.clone().unwrap_or_default(),
            export_interval: Duration::from_millis(endpoint.export.export_interval_ms.max(0) as u64),
        });
        let telemetry = Telemetry::start(TelemetryConfig {
            mode: config.mode.as_str().to_owned(),
            trace_file_path: config.paths.server_trace_path.clone(),
            trace_max_bytes: config.trace_max_bytes,
            trace_max_files: config.trace_max_files,
            trace_batch_window_ms: config.trace_batch_window_ms,
            write_traces: true,
            otlp_traces,
            otlp_resource_attributes: otel.resource_attributes.clone(),
        });
        Self {
            telemetry,
            auth: state.auth.clone(),
            observability: observability(&config.paths.logs_dir.to_string_lossy(), traces.as_ref().map(|t| t.url.as_str()), None, None),
        }
    }
}

/// `ServerConfig.observability` as the Rust server exports.
struct ObservabilityContributor(Value);

#[async_trait]
impl SnapshotContributor for ObservabilityContributor {
    async fn contribute(&self, config: &mut Map<String, Value>, _options: &ConfigOptions) -> Result<(), ConfigError> {
        config.insert("observability".into(), self.0.clone());
        Ok(())
    }
}

#[async_trait]
impl Plugin for TelemetryPlugin {
    fn name(&self) -> &'static str {
        "telemetry"
    }

    fn register_rpc(&self, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        self.telemetry.register_rpc(builder)
    }

    fn routes(&self) -> Router {
        let telemetry = self.telemetry.clone();
        let auth = self.auth.clone();
        Router::new().route(
            OTLP_TRACES_PROXY_PATH,
            post(move |request: axum::extract::Request| {
                let telemetry = telemetry.clone();
                let auth = auth.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    match zc_auth::http::authenticate(&auth, &parts).await {
                        Ok(session) if session.scopes.iter().any(|scope| scope.as_str() == "orchestration:operate") => {}
                        Ok(_) => return EnvironmentError::scope_required("orchestration:operate").into_response(),
                        Err(response) => return response,
                    }
                    let Ok(bytes) = axum::body::to_bytes(body, MAX_BROWSER_TRACE_BYTES).await else {
                        return (StatusCode::BAD_REQUEST, "Bad Request").into_response();
                    };
                    let Ok(json) = serde_json::from_slice::<Value>(&bytes) else {
                        return (StatusCode::BAD_REQUEST, "Bad Request").into_response();
                    };
                    match telemetry.record_browser_traces(&json).await {
                        BrowserTracesOutcome::Accepted => StatusCode::NO_CONTENT.into_response(),
                        BrowserTracesOutcome::ExportFailed => (StatusCode::BAD_GATEWAY, "Trace export failed.").into_response(),
                    }
                }
            }),
        )
    }

    fn config_contributors(&self) -> Vec<Arc<dyn SnapshotContributor>> {
        vec![Arc::new(ObservabilityContributor(self.observability.clone()))]
    }

    async fn shutdown(&self) {
        self.telemetry.shutdown();
    }
}
