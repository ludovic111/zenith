//! zc-preview: the in-app browser preview of zenith code (WP-28 of
//! `docs/zenith-code-rust-plan.md`, §6.10), ported from `apps/server/src/preview/**`.
//!
//! - [`manager`]: [`PreviewManager`] (`Manager.ts`): the tabs of each thread, with one
//!   monotonic revision and a per-process epoch, and their events.
//! - [`port_scanner`]: [`PortDiscovery`] (`PortScanner.ts`): local servers found without
//!   `lsof`, HTTP-probed, tagged with the terminal that started them.
//! - [`rpc`] registers `preview.*`, `subscribePreviewEvents`, `subscribeDiscoveredLocalServers`.
//! - [`url`] holds `packages/shared/src/preview.ts` (tab ids, loopback hosts, URL normalization).

pub mod manager;
pub mod port_scanner;
pub mod rpc;
pub mod url;

pub use manager::{PreviewError, PreviewManager};
pub use port_scanner::{DiscoveredLocalServer, PortDiscovery};
