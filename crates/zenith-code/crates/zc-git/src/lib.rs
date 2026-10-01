//! zc-git: zenith code's GitManager in Rust (WP-19 of `docs/zenith-code-rust-plan.md`, §6.2).
//!
//! | Module | Ported from (`apps/server/src/git/`) |
//! |---|---|
//! | [`manager`] | `GitManager.ts`: stacked actions (branch → commit → push → PR, progress events, toast), `resolvePullRequest`, `preparePullRequestThread`, status through zc-vcs's `GitStatusService` |
//! | [`pr_lookup`] | `GitManager.ts`: `lookupStatusPr` (the `pr` of VCS status), `branchPullRequest` (thread settlement), head contexts and their caches |
//! | [`settings`] | `GitManager.ts`: `projectSettingsFor`, the writer model selection, `resolveStylePolicy` |
//! | [`link`] | `linkCreatedPullRequest.ts` |
//! | [`rpc`] | the `ws.ts` handlers of `git.runStackedAction`, `git.resolvePullRequest`, `git.preparePullRequestThread` |
//! | [`helpers`], [`types`] | the pure functions and records of `GitManager.ts`, the `git.ts` wire shapes it produces |
//!
//! `GitWorkflowService.ts` and `remoteRefs.ts` live in zc-vcs; [`manager::GitManager`] plugs
//! into it as its `GitManagerBackend`. The forges are reached through zc-sourcecontrol
//! ([`providers::SourceControlProviders`]), text through the `zc_ports::TextGeneration` port.

#![allow(clippy::result_large_err)]

pub mod helpers;
pub mod link;
pub mod manager;
pub mod pr_lookup;
pub mod providers;
pub mod rpc;
pub mod settings;
pub mod types;

pub use manager::{GitManager, GitManagerDeps, PullRequestSetupScripts, UuidSource};
pub use pr_lookup::PullRequestLookup;
pub use providers::{FixedProvider, SourceControlProviders};
pub use rpc::GitRpcServices;
pub use settings::SettingsSources;
