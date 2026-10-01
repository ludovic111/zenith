//! zc-textgen: zenith code's text generation (WP-17 of `docs/zenith-code-rust-plan.md`, §6.8):
//! commit messages, change request content, worktree branch names and thread titles, each one
//! a one-shot call of the agent CLI behind the provider instance the model selection names.
//!
//! | Module | Ported from |
//! |---|---|
//! | [`dispatch`] | `textGeneration/TextGeneration.ts` (routing by `modelSelection.instanceId`, link resolution for titles) |
//! | [`prompts`] | `TextGenerationPrompts.ts`, `limitTitleMessage` of `ThreadTitleContext.ts` |
//! | [`schema`] | the prompts' output schemas, `toJsonSchemaObject` and their decoding |
//! | [`utils`] | `TextGenerationUtils.ts`, `sanitizeBranchFragment` / `sanitizeFeatureBranchName` of `@t3tools/shared/git` |
//! | [`policy`] | `TextGenerationPolicy.ts`, `TextGenerationPresets.ts` |
//! | [`links`] | `ThreadTitleLinks.ts` |
//! | [`one_shot`] | the provider-independent half of every `*TextGeneration.ts` |
//! | [`claude`] | `ClaudeTextGeneration.ts` (`claude -p --output-format json --json-schema …`) |
//! | [`codex`] | `CodexTextGeneration.ts` (`codex exec … --output-schema … --output-last-message …`) |
//! | [`unsupported`] | `Cursor`/`Grok`/`AntigravityTextGeneration.ts` (ACP), `OpenCodeTextGeneration.ts`: a clear error until WP-15/16 |
//! | [`drivers`] | the `textGeneration` each TS driver puts on its instances (`ClaudeDriver.ts`, `CodexDriver.ts`, `CodexManagedProvider.ts`) |
//!
//! `formatThreadTitleContext` (the other half of `ThreadTitleContext.ts`) lives with its only
//! caller, the provider command reactor (`zc_reactors::titles`).
//!
//! Server wiring: the drivers go through [`with_text_generation`], and the reactors and the git
//! stacked actions get a [`TextGenerationService`] over the provider instance registry.

pub mod claude;
pub mod codex;
pub mod dispatch;
pub mod drivers;
pub mod js;
pub mod links;
pub mod one_shot;
pub mod policy;
pub mod process;
pub mod prompts;
pub mod schema;
pub mod unsupported;
pub mod utils;

pub use claude::{ClaudeBackend, ClaudeTextGeneration};
pub use codex::{CodexBackend, CodexTextGeneration};
pub use dispatch::{TextGenerationInstances, TextGenerationService};
pub use drivers::{text_generation_for, with_text_generation, TextGenerationDriver};
pub use links::{resolve_thread_title_links, LinkSubject, ThreadTitleLinkResolver};
pub use one_shot::{OneShotBackend, OneShotRequest, OneShotTextGeneration};
pub use unsupported::UnsupportedTextGeneration;
pub use utils::Operation;

/// The product name the server build writes where upstream says "T3 Code"
/// (`code/scripts/lib/zenith-brand.ts`).
pub const BRAND_NAME: &str = "zenith";
