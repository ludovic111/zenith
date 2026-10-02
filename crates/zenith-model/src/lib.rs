//! What zenith's clients derive from the server's data, in one place for the window, the CLI
//! and the MCP server (ported from `code/packages/client-runtime` and `code/apps/web`):
//!
//! - [`shell`]: the projects and thread summaries (`orchestration.subscribeShell`), the
//!   sidebar's four sections and their order, each thread's status.
//! - [`thread`]: one thread in full (`orchestration.subscribeThread`), kept up to date with
//!   the server's own projector.
//! - [`worklog`]: the activities a thread shows (tool calls, commands, errors), collapsed per
//!   tool call, and their one-line summaries.
//! - [`requests`]: the approvals and questions a thread waits on.
//! - [`timeline`]: messages, plans and work merged in order, grouped per turn.

pub mod requests;
pub mod shell;
pub mod thread;
pub mod time;
pub mod timeline;
pub mod worklog;

pub use zc_contracts as contracts;
