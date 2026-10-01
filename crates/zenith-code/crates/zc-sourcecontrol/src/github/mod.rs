//! GitHub through `gh`.

pub mod auth_status;
pub mod cli;
pub mod provider;
pub mod pull_requests;

pub use cli::{with_github_reserve, with_pinned_github_credential, GitHubCli, GitHubCliError, GitHubCliErrorKind, GitHubExecuteInput, PinnedGitHubCredential};
pub use provider::GitHubSourceControlProvider;
