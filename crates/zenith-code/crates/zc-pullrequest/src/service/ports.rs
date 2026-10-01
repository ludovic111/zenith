//! `impl zc_ports::PullRequests`: the slice of the service the orchestration reactors and
//! `POST /api/pull-requests/diff` use. The port still speaks the placeholder `Value` wrappers
//! of `zc_ports::contracts`, converted here through serde.

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;
use zc_ports::contracts as port;
use zc_ports::EventStream;

use super::PullRequestService;
use crate::error::{Cause, PullRequestError};

/// A placeholder payload as its contract type. The wire decoded it already, so a value that does
/// not fit is a caller bug, reported as a failed operation rather than a panic.
fn decode<T: DeserializeOwned>(operation: &str, value: Value) -> Result<T, PullRequestError> {
    serde_json::from_value(value).map_err(|error| {
        PullRequestError::operation(operation, "The request could not be read.")
            .with_cause(Cause::new(zc_core::defect::Defect::error("SchemaError", error.to_string())))
    })
}

fn encode<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[async_trait]
impl zc_ports::PullRequests for PullRequestService {
    async fn summary(&self, reference: port::PullRequestRef, recover_transient_failure: bool) -> Result<port::PullRequestSummary, port::PullRequestError> {
        let reference = decode("summary", reference.0)?;
        let summary = PullRequestService::summary(self, reference, recover_transient_failure).await?;
        Ok(port::PullRequestSummary(encode(&summary)))
    }

    async fn stack(&self, reference: port::PullRequestRef, include_details: bool) -> Result<Option<port::PullRequestStack>, port::PullRequestError> {
        let reference = decode("stack", reference.0)?;
        let stack = PullRequestService::stack(self, reference, include_details).await?;
        Ok(stack.map(|stack| port::PullRequestStack(encode(&stack))))
    }

    async fn diff(&self, input: port::PullRequestDiffInput) -> Result<port::PullRequestDiffResult, port::PullRequestError> {
        let input = decode("diff", input.0)?;
        let diff = PullRequestService::diff(self, input).await?;
        Ok(port::PullRequestDiffResult(encode(&diff)))
    }

    async fn invalidate(&self, input: port::PullRequestInvalidateInput, notify_readers: bool) {
        if let Ok(input) = decode("invalidate", input.0) {
            PullRequestService::invalidate(self, input, notify_readers).await;
        }
    }

    async fn refresh_after_turn(&self, project_id: &port::ProjectId) {
        PullRequestService::refresh_after_turn(self, project_id).await;
    }

    fn subscribe_merges(&self) -> EventStream<zc_ports::pull_requests::PullRequestMergeEvent> {
        PullRequestService::subscribe_merges(self)
    }

    fn subscribe_refreshes(&self) -> EventStream<u64> {
        PullRequestService::subscribe_refreshes(self)
    }
}
