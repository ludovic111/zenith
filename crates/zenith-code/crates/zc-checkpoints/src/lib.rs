//! zc-checkpoints: checkpoints, turn diffs and everything around turn start and cleanup
//! (WP-11 of `docs/zenith-code-rust-plan.md`, §5.5, §5.6, §6.19).
//!
//! | Module | Ported from |
//! |---|---|
//! | [`utils`], [`diffs`], [`errors`] | `checkpointing/Utils.ts`, `Diffs.ts`, `Errors.ts` |
//! | [`store`] | `checkpointing/CheckpointStore.ts` (over zc-vcs's checkpoint primitives) |
//! | [`diff_query`] | `checkpointing/CheckpointDiffQuery.ts` |
//! | [`receipts`] | `orchestration/Services/RuntimeReceiptBus.ts` + its layers |
//! | [`reactor`] | `orchestration/Layers/CheckpointReactor.ts` |
//! | [`worktree_setup`] | `project/WorktreeSetupTracker.ts` |
//! | [`setup_script`] | `project/ProjectSetupScriptRunner.ts` (+ shared `projectScripts`) |
//! | [`bootstrap`] | the `thread.turn.start` bootstrap of `ws.ts` (~1054–1775) |
//! | [`storage_cleanup`] | `storageCleanup.ts` |
//! | [`background`] | `background/BackgroundPolicy.ts`, `background/HostPowerMonitor.ts` |
//! | [`rpc`] | the `ws.ts` handlers of `orchestration.getTurnDiff|getFullThreadDiff`, `subscribeWorktreeSetup`, `worktreeSetup.cancel`, `server.reportClientActivity|reportHostPowerState|getBackgroundPolicy`, `subscribeBackgroundPolicy` |
//! | [`worker`] | `packages/shared/src/DrainableWorker.ts` |
//!
//! `review/**` is already ported in zc-vcs (`zc_vcs::review`, `review.*` RPCs in `zc_vcs::rpc`).

// The error types mirror the wire's tagged errors field for field (like zc-vcs): boxing them
// would only add noise to every `?`.
#![allow(clippy::result_large_err)]

pub mod background;
pub mod bootstrap;
pub mod diff_query;
pub mod diffs;
pub mod errors;
pub mod reactor;
pub mod receipts;
pub mod rpc;
pub mod setup_script;
pub mod storage_cleanup;
pub mod store;
pub mod support;
pub mod utils;
pub mod worker;
pub mod worktree_setup;

pub use background::{BackgroundPolicyService, HostPowerMonitor};
pub use bootstrap::{BootstrapDeps, BootstrapDispatcher, ThreadDeletionDrain};
pub use diff_query::CheckpointDiffQuery;
pub use errors::{CheckpointServiceError, CheckpointStoreError};
pub use reactor::{CheckpointReactor, CheckpointReactorDeps, NoWorkspaceEntries, ReactorTasks, WorkspaceEntriesRefresher};
pub use receipts::{OrchestrationRuntimeReceipt, RuntimeReceiptBus};
pub use setup_script::{ProjectSetupScriptRunner, SetupScriptRunner};
pub use storage_cleanup::StorageCleanup;
pub use store::{CheckpointStore, DiffCheckpointsInput, DiffFormat, VcsCheckpointStore};
pub use utils::checkpoint_ref_for_thread_turn;
pub use worker::DrainableWorker;
pub use worktree_setup::WorktreeSetupTracker;
