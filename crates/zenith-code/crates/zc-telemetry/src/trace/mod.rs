//! The trace file `server.trace.ndjson`: record shapes, the batching rotating writer, the
//! `tracing` layer that feeds it, the browser OTLP spans, and the diagnostics reader.

pub mod diagnostics;
pub mod layer;
pub mod otlp;
pub mod record;
pub mod sink;
