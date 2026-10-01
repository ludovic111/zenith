//! zenith code's usage reporting, in Rust (`apps/server/src/usage/**`, plan §6.12, WP-30).
//!
//! - [`service::UsageService`]: `server.getUsageSummary` / `server.refreshUsageRates`. Scans
//!   the provider CLIs' own history — Claude `projects/**.jsonl`, Codex `sessions/`, Grok
//!   `sessions/**/updates.jsonl` ([`reader`], [`records`]), OpenCode and Antigravity SQLite
//!   stores read-only ([`opencode`], [`antigravity`]), Cursor's account dashboard ([`cursor`])
//!   — prices it with LiteLLM's table and the custom prices from settings ([`pricing`]), and
//!   folds it into day/hour × provider × model buckets in the reporting time zone
//!   ([`aggregation`]). Parsed transcripts are cached with resume positions in
//!   `usage-scan-cache.json` ([`scan_cache`]), the rate table in `usage-model-rates.json`;
//!   both files keep the TS format, so the two servers share them.
//! - [`limit_sources::UsageLimitSources`]: CLIProxyAPI hubs' pooled accounts and their quota
//!   ([`cliproxy`]), published as `usageLimitSourcesUpdated` on `subscribeServerConfig`.
//!
//! The server plugs this in through [`rpc::register`], [`limit_sources::UsageLimitSourcesEvents`]
//! and the capabilities [`CAPABILITIES`].

pub mod aggregation;
pub mod antigravity;
pub mod cliproxy;
pub mod collate;
pub mod cursor;
pub mod json;
pub mod limit_sources;
pub mod opencode;
pub mod pricing;
pub mod reader;
pub mod records;
pub mod rpc;
pub mod scan_cache;
pub mod service;
pub mod settings;
pub mod time;

pub use limit_sources::{UsageLimitSources, UsageLimitSourcesEvents};
pub use service::{UsageService, UsageServiceOptions};
pub use settings::UsageSettings;

/// The descriptor capabilities this package implements (`ServerEnvironment.ts`).
pub const CAPABILITIES: &[&str] = &["usageLimitSources", "usagePriceOverrides"];
