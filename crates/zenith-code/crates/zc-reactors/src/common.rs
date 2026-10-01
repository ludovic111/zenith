//! Small pieces every reactor shares: dispatching wire-JSON commands, command ids, logging a
//! failed event.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use zc_contracts::OrchestrationCommand;
use zc_ports::{GitWorkflow, OrchestrationDispatch, TaggedError};

/// A command built as wire JSON, decoded into the contract type and dispatched.
pub async fn dispatch_json(engine: &dyn OrchestrationDispatch, command: Value) -> Result<i64, TaggedError> {
    let command: OrchestrationCommand = serde_json::from_value(command.clone())
        .map_err(|error| TaggedError::new("OrchestrationCommandDecodeError", format!("cannot decode {} command: {error}", command["type"])))?;
    Ok(engine.dispatch(command, None).await?.sequence)
}

/// Where the reactors take random ids from (`crypto.randomUUIDv4`).
pub type UuidSource = Arc<dyn Fn() -> String + Send + Sync>;

/// [`zc_core::uuid_v4`].
pub fn system_uuids() -> UuidSource {
    Arc::new(zc_core::uuid_v4)
}

/// `Cause.pretty(cause)` for a failure the reactors only log or surface as text.
pub fn pretty(error: &TaggedError) -> String {
    if error.message.is_empty() {
        error.tag.clone()
    } else {
        format!("{}: {}", error.tag, error.message)
    }
}

/// `CheckpointStore.isGitRepository(cwd)`: whether a workspace is a git repository. WP-11's
/// checkpoint store implements it; [`GitWorkflowRepositoryProbe`] answers through the git
/// workflow port meanwhile.
#[async_trait]
pub trait RepositoryProbe: Send + Sync {
    async fn is_git_repository(&self, cwd: &str) -> Result<bool, TaggedError>;
}

/// [`RepositoryProbe`] over `GitWorkflow::is_repository`.
pub struct GitWorkflowRepositoryProbe(pub Arc<dyn GitWorkflow>);

#[async_trait]
impl RepositoryProbe for GitWorkflowRepositoryProbe {
    async fn is_git_repository(&self, cwd: &str) -> Result<bool, TaggedError> {
        self.0.is_repository(cwd).await
    }
}

/// Answers every probe the same way (tests, replays).
pub struct FixedRepositoryProbe(pub bool);

#[async_trait]
impl RepositoryProbe for FixedRepositoryProbe {
    async fn is_git_repository(&self, _cwd: &str) -> Result<bool, TaggedError> {
        Ok(self.0)
    }
}
