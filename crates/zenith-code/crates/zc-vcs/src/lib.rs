//! zc-vcs: zenith code's version control layer in Rust (WP-18 of `docs/zenith-code-rust-plan.md`).
//!
//! | Module | Ported from |
//! |---|---|
//! | [`driver_core`] | `vcs/GitVcsDriverCore.ts` (the `GitVcsDriver` service: status, refs, worktrees, review diffs, commit/push/pull, remotes) |
//! | [`git_exec`] | `GitVcsDriverCore.ts` `executeRaw`/`collectOutput`/`createTrace2Monitor` |
//! | [`vcs_driver`] | `vcs/VcsDriver.ts`, `vcs/GitVcsDriver.ts` `makeVcsDriverShape` (detection, workspace files, remotes, ignore filtering, init, checkpoints) |
//! | [`registry`] | `vcs/VcsDriverRegistry.ts`, `vcs/VcsProjectConfig.ts`, `vcs/VcsProvisioningService.ts` |
//! | [`status`] | the status slice of `git/GitManager.ts` (local/remote status, 1 s caches, forge detection) behind a [`status::PullRequestStatusSource`] hook |
//! | [`workflow`] | `git/GitWorkflowService.ts`, implementing [`zc_ports::GitWorkflow`] |
//! | [`broadcaster`] | `vcs/VcsStatusBroadcaster.ts`, implementing [`zc_ports::VcsStatusRefresher`] |
//! | [`review`] | `review/ReviewService.ts` |
//! | [`rpc`] | the `ws.ts` handlers of `subscribeVcsStatus`, `vcs.*` and `review.*` |
//! | [`remote_refs`], [`shared_git`], [`parse`], [`project_file`] | `git/remoteRefs.ts`, `shared/git.ts`, `shared/sourceControl.ts`, the driver's parsers, `t3.json` |
//! | [`contracts`], [`errors`] | the wire types of `contracts/git.ts`, `vcs.ts`, `review.ts` until zc-contracts lands |
//!
//! Everything shells out to `git` with the TS arguments, flags and environment (plan §6.1:
//! no git2/gix for behaviour).

// The error types mirror the wire's tagged errors field for field (≈250 bytes); boxing them
// everywhere would only add noise to every `?`.
#![allow(clippy::result_large_err)]

pub mod broadcaster;
pub mod cache;
pub mod collate;
pub mod contracts;
pub mod driver_core;
pub mod errors;
pub mod git_exec;
pub mod parse;
pub mod project_file;
pub mod registry;
pub mod remote_refs;
pub mod review;
pub mod rpc;
pub mod shared_git;
pub mod status;
pub mod vcs_driver;
pub mod workflow;

pub use broadcaster::{AutoPullPolicy, VcsStatusBroadcaster};
pub use driver_core::GitVcsDriver;
pub use errors::{GitCommandError, GitManagerError, GitManagerServiceError, ReviewDiffPreviewError, VcsError};
pub use registry::{VcsDriverRegistry, VcsProjectConfig, VcsProvisioningService};
pub use review::ReviewService;
pub use status::{GitStatusService, PullRequestStatusSource};
pub use vcs_driver::{GitVcsProcessDriver, VcsCheckpointOps, VcsDriver};
pub use workflow::{GitManagerBackend, GitWorkflowService};
