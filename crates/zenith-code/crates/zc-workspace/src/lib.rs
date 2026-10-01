//! zc-workspace: workspace search and files (WP-24 of `docs/zenith-code-rust-plan.md`, port of
//! `code/apps/server/src/workspace/**` and `process/externalLauncher.ts`).
//!
//! | Module | Ported from |
//! |---|---|
//! | [`backend`] | the `@ff-labs/fff-node` `FileFinder` calls, on the fff Rust library (`fff-search` v0.9.4) |
//! | [`search_index`] | `WorkspaceSearchIndex.ts` (`make`, `list`, `search`, `searchContents`, `refresh`) |
//! | [`index_map`] | `WorkspaceSearchIndexMap` (`LayerMap`, 15 min idle TTL, paths/content variants) |
//! | [`entries`] | `WorkspaceEntries.ts` (`browse`, `list`, `search`, `searchContents`, `refresh`) |
//! | [`file_system`] | `WorkspaceFileSystem.ts` (`readFile`, `writeFile`) |
//! | [`paths`] | `WorkspacePaths.ts`, `pathExpansion.ts`, Node `path` (POSIX) |
//! | [`lease`] | `workspaceLease.ts` (re-export of `zc_core::lease`) |
//! | [`errors`] | the module's tagged errors and their `Schema.Defect` encoding |
//! | [`rpc`] | `ws.ts`: `projects.searchEntries|searchContents|listEntries|readFile|writeFile`, `filesystem.browse`, `shell.openInEditor` |
//! | [`launcher`] | `process/externalLauncher.ts`, the editor catalogue (`contracts/editor.ts`), shared `editor.ts` and `shell.ts` command lookup |
//!
//! Wiring, in the server crate:
//!
//! ```ignore
//! let indexes = SearchIndexMap::new(Arc::new(FffFactory));
//! let entries = WorkspaceEntries::new(WorkspacePaths::new(), indexes, vcs_process.clone());
//! let file_system = WorkspaceFileSystem::new(WorkspacePaths::new(), entries.clone());
//! let launcher = Arc::new(ExternalLauncher::default());
//! let builder = zc_workspace::rpc::register(builder, WorkspaceRpcServices { entries, file_system, launcher: launcher.clone() });
//! // server.getConfig: availableEditors = launcher.resolve_available_editors().await (under its
//! // discovery timeout), fileManagerRevealKind when "file-manager" is among them.
//! ```

// The tagged errors mirror the TS ones field for field and one is produced per call at most.
#![allow(clippy::result_large_err)]

pub mod backend;
mod collate;
pub mod entries;
pub mod errors;
pub mod file_system;
pub mod index_map;
pub mod launcher;
pub mod paths;
pub mod platform;
pub mod rpc;
pub mod search_index;
pub mod text;

/// `withWorkspaceLease` (`workspace/workspaceLease.ts`), shared with zc-terminal through zc-core.
pub mod lease {
    pub use zc_core::lease::*;
}

pub use backend::{FffFactory, FffFinder, Finder, FinderError, FinderFactory, IndexVariant};
pub use entries::{BrowseRequest, EntrySearch, WorkspaceEntries};
pub use file_system::WorkspaceFileSystem;
pub use index_map::{IndexLease, SearchIndexMap};
pub use launcher::ExternalLauncher;
pub use paths::WorkspacePaths;
pub use rpc::{register, WorkspaceRpcServices};
pub use search_index::{ContentSearch, WorkspaceSearchIndex};
