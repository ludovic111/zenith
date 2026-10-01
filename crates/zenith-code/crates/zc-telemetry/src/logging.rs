//! The process's `tracing` subscriber: pretty logs on stderr (filtered by `RUST_LOG`, else the
//! given default) plus the [`crate::trace`] layer that writes `server.trace.ndjson` once a
//! target is installed.

use tracing_subscriber::filter::{filter_fn, EnvFilter, FilterExt};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::trace::layer::{effect_level_rank, enabled, set_levels, TraceLayer};

/// The target of spans that exist for the trace file only (the RPC spans): the console log
/// does not show them as context.
pub const TRACE_TARGET: &str = "zenith::trace";

/// Installs the global subscriber. Trace levels come from `T3CODE_TRACE_MIN_LEVEL` (spans,
/// default `Info`) and `T3CODE_LOG_LEVEL` (span events, default `Info`).
pub fn init(default_filter: &str) {
    let level = |name: &str| {
        std::env::var(name)
            .ok()
            .map(|value| effect_level_rank(value.trim()))
            .filter(|rank| *rank > 0)
            .unwrap_or(3)
    };
    set_levels(level("T3CODE_TRACE_MIN_LEVEL"), level("T3CODE_LOG_LEVEL"));
    let console_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into());
    let console = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        // No colour codes when stderr is a file (the LaunchAgent's server.log).
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_filter(console_filter.and(filter_fn(|metadata| !(metadata.is_span() && metadata.target() == TRACE_TARGET))));
    let _ = tracing_subscriber::registry()
        .with(console)
        .with(TraceLayer.with_filter(filter_fn(enabled)))
        .try_init();
}
