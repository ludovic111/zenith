//! The Bitbucket Cloud pull request provider: REST over zc-sourcecontrol's shared
//! [`BitbucketApi`](zc_sourcecontrol::bitbucket::BitbucketApi).
//!
//! | Module | Ported from (`apps/server/src/pullRequest/`) |
//! |---|---|
//! | [`json`] | `bitbucketPullRequestJson.ts` |
//! | [`api`] | `BitbucketPullRequestApi.ts` |
//! | [`diff_revisions`] | `bitbucketDiffRevisions.ts` |
//! | [`git_patch_path`] | `unquoteGitPatchPath` of `packages/shared/src/gitPatchPath.ts` |
//! | [`provider`] | `BitbucketPullRequestProvider.ts` |

pub mod api;
pub mod diff_revisions;
pub mod git_patch_path;
pub mod json;
pub mod provider;

pub use api::{BitbucketPullRequestApi, BitbucketPullRequestApiError, BitbucketRequester};
pub use provider::BitbucketPullRequestProvider;
