//! The Forgejo (and Gitea) pull request provider, through the REST API with `fj`'s token or
//! `tea api`.

pub mod diff_revisions;
pub mod json;
pub mod provider;

pub use provider::ForgejoPullRequestProvider;
