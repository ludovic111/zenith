//! zc-providers: zenith code's provider core (WP-12), the Rust port of
//! `code/apps/server/src/provider/` minus the drivers themselves.
//!
//! | Module | Ported from |
//! |---|---|
//! | [`service`] | `Layers/ProviderService.ts` (routing, attachment lines, compaction, rollback, recovery, stop-all, event fan-out) |
//! | [`directory`] | `Layers/ProviderSessionDirectory.ts` over `provider_session_runtime` |
//! | [`reaper`] | `Layers/ProviderSessionReaper.ts` (30 min idle, 5 min sweep) |
//! | [`driver`] | `ProviderDriver.ts`, `builtInDrivers.ts`, `Drivers/instanceIdentity.ts`, `ProviderInstanceEnvironment.ts`, `Services/ServerProvider.ts` |
//! | [`instance_registry`] | `Layers/ProviderInstanceRegistryLive.ts`, `…Hydration.ts`, `Services/ProviderInstanceRegistry*.ts` |
//! | [`adapter_registry`] | `Layers/ProviderAdapterRegistry.ts` |
//! | [`registry`] | `Layers/ProviderRegistry.ts` (status snapshots, refresh, workspace snapshots, change stream) |
//! | [`managed`] | `makeManagedServerProvider.ts` (the snapshot half every driver builds) |
//! | [`snapshot`] | `providerSnapshot.ts`, `unavailableProviderSnapshot.ts`, the version advisory of `providerMaintenance.ts` |
//! | [`status_cache`] | `providerStatusCache.ts` |
//! | [`compatibility`], [`semver`] | `providerCompatibility.ts`, shared `semver.ts` |
//! | [`manifest`] | `ModelManifest.ts`, `ClaudeModelManifest.ts` validation, `model-manifest.json` |
//! | [`usage_limits`] | `providerUsageLimits.ts` |
//! | [`logger`] | `Layers/EventNdjsonLogger.ts`, `ProviderEventLoggers.ts`, shared `RotatingFileSink` |
//! | [`citations`] | shared `assistantCitations.ts` (`expandAssistantCitationsForProvider`) |
//! | [`attachments`] | `attachmentStore.ts` paths, `imageMime.ts`, `userInputAttachments.ts` |
//! | [`ports`] | the `zc_ports::ProviderService` / `ProviderStatusReads` implementations |
//!
//! Drivers (Claude WP-13, Codex WP-14, ACP WP-15, OpenCode WP-16) implement [`driver::Driver`]:
//! `create` returns a [`driver::ProviderInstance`] holding their [`zc_ports::adapter::ProviderAdapter`],
//! their [`driver::ServerProviderSource`] (usually a [`managed::ManagedServerProvider`]) and their
//! text generation.

pub mod adapter_registry;
pub mod attachments;
pub mod citations;
pub mod compatibility;
pub mod directory;
pub mod driver;
pub mod errors;
pub mod events;
pub mod hooks;
pub mod instance_registry;
pub mod js_json;
pub mod logger;
pub mod managed;
pub mod manifest;
pub mod ports;
pub mod reaper;
pub mod registry;
pub mod semver;
pub mod service;
pub mod settings;
pub mod snapshot;
pub mod status_cache;
pub mod usage_limits;

pub use adapter_registry::{AdapterRegistry, InstanceAdapterRegistry};
pub use directory::{ProviderRuntimeBinding, ProviderRuntimeBindingWithMetadata, ProviderSessionDirectory};
pub use driver::{Driver, DriverCreateInput, DriverEnv, DriverMetadata, InstanceScope, ProviderInstance, ServerProviderSource, BUILT_IN_DRIVER_KINDS};
pub use errors::{ProviderDriverError, ProviderServiceError};
pub use instance_registry::ProviderInstanceRegistry;
pub use logger::{EventNdjsonLogStore, EventNdjsonLogger, EventNdjsonStream, ProviderEventLoggers};
pub use manifest::{ModelManifest, ModelManifestData};
pub use reaper::ProviderSessionReaper;
pub use registry::ProviderRegistry;
pub use service::{ProviderServiceImpl, ProviderServiceOptions};
