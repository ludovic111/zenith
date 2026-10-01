//! The Azure DevOps pull request provider: `az repos pr` and `az devops invoke` through
//! zc-sourcecontrol's shared `AzureDevOpsCli`, with the diff synthesized locally.
//!
//! | Module | Ported from (`apps/server/src/pullRequest/`) |
//! |---|---|
//! | [`json`] | `azureDevOpsPullRequestJson.ts` |
//! | [`cli`] | `AzureDevOpsPullRequestCli.ts` |
//! | [`diff`] | `azureDevOpsDiff.ts` (with jsdiff's `structuredPatch`) |
//! | [`provider`] | `AzureDevOpsPullRequestProvider.ts` |
//! | [`util`] | `@t3tools/shared/gitPatchPath`, `localeCompare` |

pub mod cli;
pub mod diff;
pub mod json;
pub mod provider;
pub mod util;

pub use cli::{AzureDevOpsPullRequestCli, AzureDevOpsPullRequestCliApi, AzureDevOpsPullRequestCliError};
pub use provider::AzureDevOpsPullRequestProvider;
