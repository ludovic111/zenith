//! zc-terminal: zenith code's terminals in Rust (WP-26 of `docs/zenith-code-rust-plan.md`,
//! port of `code/apps/server/src/terminal/**`).
//!
//! | Module | Ported from |
//! |---|---|
//! | [`manager`] | `Manager.ts` (`makeWithOptions`): sessions, lifecycle, events, polling |
//! | [`pty`] | `PtyAdapter.ts`, `NodePtyAdapter.ts` (on `portable-pty`) |
//! | [`decoder`] | Node's `StringDecoder("utf8")`, as node-pty uses it |
//! | [`sanitizer`] | `sanitizeTerminalHistoryChunk` |
//! | [`history`] | `BoundedTerminalHistory` |
//! | [`persist`] | history files, legacy migration, the debounced persist worker |
//! | [`shell`] | shell candidates, spawn env, labels |
//! | [`subprocess`] | process table snapshot, `ps` fallback, busy detection, backoff |
//! | [`lease`] | re-export of `zc_core::lease` (`workspace/workspaceLease.ts`, shared with zc-workspace) |
//! | [`contracts`] | `packages/contracts/src/terminal.ts` (until `zc-contracts` lands) |
//! | [`port`] | the `zc_ports::TerminalManager` implementation |
//! | [`rpc`] | the nine terminal RPC methods of `ws.ts`, with `OutputProtocol.ts`'s windowed acks |
//! | [`testing`] | the fake PTY of `Manager.test.ts` |
//!
//! Wiring, in the server crate:
//!
//! ```ignore
//! let options = TerminalManagerOptions::new(paths.terminal_logs_dir, Arc::new(PortablePtyAdapter));
//! let terminals = Arc::new(TerminalManager::new(options).await?);
//! let router = zc_terminal::rpc::register(RpcRouter::builder(), terminals.clone());
//! // on shutdown: terminals.shutdown().await
//! ```

// `TerminalError` mirrors the wire union field for field (callers match on it and serialize
// it), and a call produces at most one, so boxing it buys nothing.
#![allow(clippy::result_large_err)]

pub mod contracts;
pub mod decoder;
pub mod history;
/// The workspace lease moved to `zc_core::lease` so zc-workspace and worktree removal share
/// one process-wide table; re-exported here for existing callers.
pub mod lease {
    pub use zc_core::lease::*;
}
pub mod manager;
pub mod persist;
pub mod port;
pub mod pty;
pub mod rpc;
pub mod sanitizer;
pub mod shell;
pub mod subprocess;
pub mod testing;

pub use contracts::{TerminalError, DEFAULT_TERMINAL_ID};
pub use manager::{FnProviderEnvironment, ListenerStream, ProviderEnvironmentResolver, TerminalManager, TerminalManagerOptions, TerminalProcessRegistry};
pub use pty::{PortablePtyAdapter, PtyAdapter, PtyProcess};
