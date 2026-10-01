//! zc-provider-claude: the Claude driver of zenith code (WP-13 of `docs/zenith-code-rust-plan.md`),
//! without the Node Agent SDK. It spawns the user's `claude` binary exactly as
//! `@anthropic-ai/claude-agent-sdk` 0.3.276 would and speaks its stream-json + control protocol
//! directly.
//!
//! | Module | Ported from |
//! |---|---|
//! | [`adapter`] | `provider/Layers/ClaudeAdapter.ts` (sessions, turns, steering, approvals, user input, interrupt, rollback) |
//! | [`mapping`] | `ClaudeAdapter.ts` handlers: native SDK messages → canonical `ProviderRuntimeEvent`s |
//! | [`protocol`] | the SDK's `ProcessTransport` + `Query` (spawn, NDJSON, control requests both ways) |
//! | [`options`] | the SDK's option → argv/env/`initialize` translation (`k0`, `ProcessTransport.initialize`) |
//! | [`query`] | the `createQuery` seam (`ClaudeQueryRuntime`, `canUseTool`, `onUserDialog`) |
//! | [`provider`] | `provider/Layers/ClaudeProvider.ts` (status probe: version, account, commands, usage) |
//! | [`driver`] | `provider/Drivers/ClaudeDriver.ts` (one instance: adapter + probe + skills + resets) |
//! | [`provider_driver`] | `ClaudeDriver.create`: the [`zc_providers::Driver`] the provider core registers |
//! | [`home`] | `Drivers/ClaudeHome.ts`, `Drivers/ClaudeExecutable.ts` |
//! | [`skills`], [`skill_dispatch`] | `Drivers/ClaudeSkills.ts`, `Drivers/ClaudeSkillDispatch.ts` |
//! | [`catalog`] | `ClaudeModelCatalog.ts`, `ClaudeModelManifest.ts` |
//! | [`history`] | `claudeHistoryWorker.ts` (`getSessionMessages`, `forkSession`) as direct JSONL file operations |
//! | [`usage_limits`], [`reset_credits`] | `Layers/claudeUsageLimits.ts`, `Layers/claudeResetCredits.ts` |
//! | [`cli_args`], [`model_options`], [`semver`] | the `@t3tools/shared` helpers these use |

pub mod adapter;
pub mod catalog;
pub mod cli_args;
pub mod driver;
pub mod history;
pub mod home;
pub mod js;
pub mod mapping;
pub mod model_options;
pub mod options;
pub mod protocol;
pub mod provider;
pub mod provider_driver;
pub mod query;
pub mod reset_credits;
pub mod semver;
pub mod skill_dispatch;
pub mod skills;
pub mod usage_limits;

pub use adapter::{ClaudeAdapter, ClaudeAdapterOptions};
pub use catalog::ClaudeModelCatalog;
pub use driver::{ClaudeDriver, ClaudeInstance};
pub use protocol::{ProcessQuery, ProcessQueryFactory};
pub use provider_driver::ClaudeProviderDriver;

/// The driver kind (persisted; never changes).
pub const DRIVER_KIND: &str = "claudeAgent";

/// The product name the TS build plugin substitutes for "T3 Code" in server strings.
pub const BRAND_NAME: &str = "zenith code";
