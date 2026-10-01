//! GitLab through `glab`.

pub mod auth_status;
pub mod cli;
pub mod merge_requests;
pub mod provider;

pub use cli::{GitLabCli, GitLabCliError, GitLabCliErrorKind, GitLabExecuteInput};
pub use provider::GitLabSourceControlProvider;
