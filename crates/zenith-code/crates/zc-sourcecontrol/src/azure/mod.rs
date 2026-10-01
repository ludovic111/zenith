//! Azure DevOps through `az` (with the `azure-devops` extension).

pub mod cli;
pub mod provider;
pub mod pull_requests;

pub use cli::{AzureDevOpsCli, AzureDevOpsCliError, AzureDevOpsCliErrorKind, AzureExecuteInput};
pub use provider::AzureDevOpsSourceControlProvider;
