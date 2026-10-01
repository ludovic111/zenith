//! zc-settings: zenith code's server settings, keybindings and environment themes (WP-07 of
//! `docs/zenith-code-rust-plan.md`), and the RPCs that serve them.
//!
//! | Module | Ported from |
//! |---|---|
//! | [`settings`] | `apps/server/src/serverSettings.ts`, shared `serverSettings.ts`, `backgroundActivitySettings.ts`, `Struct.ts` |
//! | [`keybindings`] | `apps/server/src/keybindings.ts`, shared `keybindings.ts` (defaults, shortcut and `when` parsers) |
//! | [`themes`] | `apps/server/src/environmentTheme.ts` |
//! | [`config`] | the settings/keybindings/themes parts of `ws.ts` `loadServerConfig` and `subscribeServerConfig` |
//! | [`rpc`] | `server.getSettings`, `server.updateSettings`, `server.upsertKeybinding`, `server.removeKeybinding`, `server.getConfig`, `subscribeServerConfig` |
//! | [`js`] | JS semantics the files depend on (`JSON.stringify`, `Equal.equals`, `trim`) |
//! | [`watch`] | `fs.watch(dir)` + `Stream.debounce(100 ms)` |
//!
//! The files these services own (`settings.json`, `keybindings.json`, `themes/*.json`, the
//! settings secrets in `secrets/`) are shared with the TS server and the dashboard, so they are
//! read and written byte for byte like TS does: see `docs/zenith-code/settings.md`.

// The errors are the wire's tagged errors field for field (handlers serialize them as they are),
// and at most one is produced per request, so boxing them buys nothing.
#![allow(clippy::result_large_err)]

pub mod config;
pub mod errors;
pub mod js;
pub mod keybindings;
pub mod rpc;
pub mod settings;
pub mod themes;
pub mod watch;

pub use config::{ConfigEventSource, ServerConfigParts, ServerConfigService, SnapshotContributor};
pub use keybindings::{KeybindingsConfigState, KeybindingsService};
pub use settings::{SecretBackend, ServerSettingsService, SettingsDatabase};
pub use themes::EnvironmentThemeService;
