//! zenith code's observability, ported from `apps/server/src/{resourceTelemetry,diagnostics,
//! observability}` and `packages/shared/src/{observability,otelEnvironment}.ts` (plan §6.19,
//! WP-31).
//!
//! | Module | TS |
//! |---|---|
//! | [`monitor`] | `code/native/resource-monitor` (`t3-resource-monitor`), linked in: a thread, not a sidecar |
//! | [`native`] | `NativeTelemetryClient.ts`: supervision, health, collection cadence, requests |
//! | [`model`], [`history`] | `Model.ts`, `ResourceTelemetryHistory.ts` |
//! | [`telemetry`] | `ResourceTelemetry.ts` |
//! | [`desktop`] | `DesktopTelemetryReceiver.ts` (always unavailable: zenith has no Electron shell) |
//! | [`attribution`] | `ResourceAttribution.ts` |
//! | [`host_resources`] | `HostResources.ts` |
//! | [`diagnostics`] | `ProcessDiagnostics.ts`, `ProcessResourceMonitor.ts` |
//! | [`trace`] | `makeTraceSink`, `makeLocalFileTracer`, `decodeOtlpTraceRecords`, `TraceDiagnostics.ts` |
//! | [`otel_env`] | `otelEnvironment.ts` |
//! | [`logging`] | the `tracing` subscriber (console + trace file) |
//! | [`service`] | the bundle and its WS RPC handlers |
//!
//! Files: `<base>/userdata/logs/server.trace.ndjson` (or `T3CODE_TRACE_FILE`) and its rotated
//! backups `.1` … `.N`; the Diagnostics page opens `observability.logsDirectoryPath`
//! (`<base>/userdata/logs`) in an editor. Remote export (OTLP) is opt-in, as in TS: see
//! [`trace::otlp`]; the metrics and logs signals are not exported by the Rust server.

pub mod attribution;
pub mod desktop;
pub mod diagnostics;
pub mod history;
pub mod host_resources;
pub mod logging;
pub mod model;
pub mod monitor;
pub mod native;
pub mod otel_env;
pub mod service;
pub mod telemetry;
pub mod trace;

pub use attribution::{AttributionRecord, ResourceAttribution};
pub use host_resources::HostResources;
pub use native::{NativeTelemetry, NativeTelemetryClient};
pub use service::{BrowserTracesOutcome, Telemetry, TelemetryConfig, UNTRACED_METHODS};
pub use telemetry::ResourceTelemetry;
