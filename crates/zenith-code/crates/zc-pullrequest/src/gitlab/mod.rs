//! The GitLab pull request provider, through `glab` (mostly `glab api`).

pub mod cli;
pub mod json;
pub mod provider;
pub mod util;

pub use cli::{GitLabPullRequestCli, GitLabPullRequestCliError};
pub use provider::GitLabPullRequestProvider;
