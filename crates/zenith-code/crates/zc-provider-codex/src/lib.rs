//! zc-provider-codex: the Codex driver of zenith code (WP-14 of `docs/zenith-code-rust-plan.md`).
//!
//! | Module | Ported from |
//! |---|---|
//! | [`peer`], [`client`], [`errors`] | `packages/effect-codex-app-server` (`protocol.ts`, `client.ts`, `errors.ts`) |
//! | [`session_runtime`], [`thread_history`], [`elicitation`] | `provider/Layers/CodexSessionRuntime.ts` |
//! | [`mapping`], [`adapter`] | `provider/Layers/CodexAdapter.ts` |
//! | [`provider_status`] | `provider/Layers/CodexProvider.ts` |
//! | [`launch_args`] | `provider/Layers/codexLaunchArgs.ts`, shared `tokenizeCliArgs` |
//! | [`instructions`] | `provider/CodexDeveloperInstructions.ts`, `provider/RuntimeInstructions.ts` |
//! | [`usage_limits`] | `provider/Layers/codexUsageLimits.ts` |
//! | [`model`] | `codexModelOptions.ts`, shared `model.ts` (Codex parts) |
//! | [`home_layout`] | `provider/Drivers/CodexHomeLayout.ts` |
//! | [`managed`] | `provider/CodexManagedRuntime.ts`, `CodexManagedErrors.ts`, `CodexManagedHome.ts`, `Drivers/CodexManagedProvider.ts` (check logic) |
//! | [`driver`] | `provider/Drivers/CodexDriver.ts` |
//! | [`provider_driver`] | `CodexDriver.ts` + `CodexManagedProvider.ts` on the provider core's `zc_providers::Driver` SPI |
//!
//! Types of the app-server protocol come from `zc-codex-protocol`. See docs/zenith-code/codex.md.

// The app-server errors mirror the TS tagged errors field for field (callers match on them and
// their messages surface), and at most one is produced per failed call, so boxing buys nothing.
#![allow(clippy::result_large_err)]

pub mod adapter;
pub mod client;
pub mod driver;
pub mod elicitation;
pub mod errors;
pub mod home_layout;
mod instruction_texts;
pub mod instructions;
pub mod launch_args;
pub mod managed;
pub mod mapping;
pub mod model;
pub mod peer;
pub mod process;
pub mod provider_driver;
pub mod provider_status;
pub mod session_runtime;
pub mod thread_history;
pub mod usage_limits;

/// The product name the server build writes where upstream says "T3 Code"
/// (`scripts/lib/zenith-brand.ts`).
pub const BRAND_NAME: &str = "zenith code";

/// The driver kind (`ProviderDriverKind.make("codex")`).
pub const DRIVER_KIND: &str = "codex";

pub use adapter::{CodexAdapter, CodexAdapterOptions};
pub use errors::{CodexAppServerError, CodexSessionRuntimeError, RequestError};
pub use provider_driver::CodexProviderDriver;
pub use session_runtime::{CodexSessionRuntime, CodexSessionRuntimeOptions};
