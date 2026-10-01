//! Server settings: `apps/server/src/serverSettings.ts` and the helpers of
//! `packages/shared/src/serverSettings.ts` / `backgroundActivitySettings.ts`.
//!
//! - [`schema`]: decoding with the TS schema's semantics into canonical encoded JSON.
//! - [`logic`]: the pure transformations (patches, folds, secrets, redaction, sparse form).
//! - [`background`]: background activity profiles.
//! - [`service`]: the service (file, cache, secrets, watcher, change stream).

pub mod background;
pub mod logic;
pub mod schema;
pub mod service;

pub use logic::{redact_server_settings_for_client, resolve_source_control_writer_model_selection, SECRET_REDACTED};
pub use service::{normalize_server_settings, sparse_settings_json, test_settings, SecretBackend, ServerSettingsService, SettingsDatabase};
