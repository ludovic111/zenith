//! zc-project: zenith code's `apps/server/src/project` in Rust (WP-25 of
//! `docs/zenith-code-rust-plan.md`, §6.5), except what zc-checkpoints already has (the
//! worktree setup tracker and the setup script runner).
//!
//! | Module | Ported from (`apps/server/src/project/`) |
//! |---|---|
//! | [`identity`] | `RepositoryIdentityResolver.ts` (+ the Forgejo refinement of `server.ts`) |
//! | [`clone`] | `ProjectCloneTracker.ts` (progress parsing is zc-sourcecontrol's `clone_progress`, `gitCloneProgress.ts`) |
//! | [`project_file`] | `T3ProjectFileLoader.ts` |
//! | [`sessions`] | `AgentSessionScanner.ts`, `AgentSessionImporter.ts`, `AgentSessionJson.ts` |
//! | [`rpc`] | the `ws.ts` handlers of `projectClone.*`, `subscribeProjectClones`, `agentSessions.*` |
//!
//! `ProjectFaviconResolver.ts` is not here: the favicon route belongs with the assets (WP-29).

pub mod clone;
pub mod identity;
pub mod project_file;
pub mod rpc;
pub mod sessions;

pub use clone::{
    discard_clone_for_deleted_project, reject_commands_during_clone, CloneRepositories, ClonedProject, ProjectCloneHooks, ProjectCloneStartError,
    ProjectCloneTracker,
};
pub use identity::{ForgejoIdentityRefiner, RepositoryIdentities, RepositoryIdentityOptions, RepositoryIdentityRefiner};
pub use project_file::{parse_t3_project_file, T3ProjectFileLoader};
pub use rpc::{register, CloneHooksFactory, ProjectRpcServices};
pub use sessions::{AgentSessionImporter, AgentSessionScanner, ScannerConfig};
