//! Forgejo and Gitea through `fj` or `tea`.

pub mod cli;
pub mod provider;
pub mod pull_requests;

pub use cli::{ForgejoCli, ForgejoCliError, ForgejoCommand, ForgejoEnvironment, ForgejoErrorReason, ForgejoLogin};
pub use provider::{ForgejoDiscovery, ForgejoSourceControlProvider};
