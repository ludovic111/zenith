//! The two error layers of `pullRequest/`:
//!
//! - [`PullRequestProviderError`] (`PullRequestProvider.ts`): the one failure shape every
//!   provider reports. Its `reason` is what the service acts on: a missing or unauthenticated
//!   tool disables the provider for the workspace, a rate limit pauses its host, anything else is
//!   specific to the request.
//! - [`PullRequestError`] (`PullRequestService.ts`, contracts `PullRequestUnavailableError |
//!   PullRequestOperationError`): what the service and the RPCs fail with. Serializes exactly
//!   like the contracts, with `cause` as its `Schema.Defect()` encoding.

use std::any::Any;
use std::fmt;

use serde::{Serialize, Serializer};
use serde_json::{json, Map, Value};
use zc_contracts::{
    LitPullRequestOperationError, LitPullRequestUnavailableError, PullRequestOperationError, PullRequestUnavailableError, PullRequestUnavailableReason,
    SourceControlProviderKind,
};
pub use zc_sourcecontrol::errors::{error_defect, Cause, CauseError};

/// `PullRequestProviderError["reason"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProviderFailureReason {
    MissingTool,
    Unauthenticated,
    RateLimited,
    Failed,
}

impl ProviderFailureReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingTool => "missing-tool",
            Self::Unauthenticated => "unauthenticated",
            Self::RateLimited => "rate-limited",
            Self::Failed => "failed",
        }
    }
}

/// `PullRequestProviderError` (`Schema.TaggedError`).
#[derive(Debug, Clone)]
pub struct PullRequestProviderError {
    pub provider: SourceControlProviderKind,
    pub operation: String,
    pub reason: ProviderFailureReason,
    pub detail: String,
    /// Epoch milliseconds, for `rate-limited`.
    pub retry_at: Option<i64>,
    pub cause: Option<Cause>,
}

impl PullRequestProviderError {
    pub fn new(provider: SourceControlProviderKind, operation: impl Into<String>, reason: ProviderFailureReason, detail: impl Into<String>) -> Self {
        Self {
            provider,
            operation: operation.into(),
            reason,
            detail: detail.into(),
            retry_at: None,
            cause: None,
        }
    }

    /// A `failed` error.
    pub fn failed(provider: SourceControlProviderKind, operation: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::new(provider, operation, ProviderFailureReason::Failed, detail)
    }

    /// What a provider answers for an optional method it does not implement (the TS service
    /// never calls those: it checks [`crate::provider::OptionalMethods`] first).
    pub fn unsupported(provider: SourceControlProviderKind, operation: impl Into<String>) -> Self {
        let operation = operation.into();
        let detail = format!("{} does not support {operation}.", provider.as_str());
        Self::failed(provider, operation, detail)
    }

    pub fn with_retry_at(mut self, retry_at: Option<i64>) -> Self {
        self.retry_at = retry_at;
        self
    }

    pub fn with_cause(mut self, cause: Cause) -> Self {
        self.cause = Some(cause);
        self
    }

    /// `isProviderUnusable`: a host that cannot be read at all, as opposed to one request that
    /// failed.
    pub fn is_unusable(&self) -> bool {
        matches!(self.reason, ProviderFailureReason::MissingTool | ProviderFailureReason::Unauthenticated)
    }

    /// The TS `message` getter: `${provider} failed in ${operation}: ${detail}`.
    pub fn message(&self) -> String {
        format!("{} failed in {}: {}", self.provider.as_str(), self.operation, self.detail)
    }

    /// The tagged wire encoding (`Schema.encode(PullRequestProviderError)`).
    pub fn to_wire(&self) -> Value {
        let mut map = Map::new();
        map.insert("_tag".into(), json!("PullRequestProviderError"));
        map.insert("provider".into(), json!(self.provider.as_str()));
        map.insert("operation".into(), json!(self.operation));
        map.insert("reason".into(), json!(self.reason.as_str()));
        map.insert("detail".into(), json!(self.detail));
        if let Some(retry_at) = self.retry_at {
            map.insert("retryAt".into(), json!(retry_at));
        }
        if let Some(cause) = &self.cause {
            map.insert("cause".into(), cause.defect());
        }
        Value::Object(map)
    }
}

impl fmt::Display for PullRequestProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for PullRequestProviderError {}

impl Serialize for PullRequestProviderError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_wire().serialize(serializer)
    }
}

impl CauseError for PullRequestProviderError {
    fn defect(&self) -> Value {
        error_defect("PullRequestProviderError", self.message(), self.cause.as_ref().map(Cause::defect))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `PullRequestUnavailableError | PullRequestOperationError`: the service's failures.
#[derive(Debug, Clone)]
pub enum PullRequestError {
    Unavailable {
        reason: PullRequestUnavailableReason,
        provider: Option<SourceControlProviderKind>,
        cause: Option<Cause>,
    },
    Operation {
        operation: String,
        detail: String,
        cause: Option<Cause>,
    },
}

impl PullRequestError {
    pub fn unavailable(reason: PullRequestUnavailableReason) -> Self {
        Self::Unavailable {
            reason,
            provider: None,
            cause: None,
        }
    }

    pub fn operation(operation: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Operation {
            operation: operation.into(),
            detail: detail.into(),
            cause: None,
        }
    }

    pub fn with_cause(mut self, new_cause: Cause) -> Self {
        match &mut self {
            Self::Unavailable { cause, .. } | Self::Operation { cause, .. } => *cause = Some(new_cause),
        }
        self
    }

    pub fn tag(&self) -> &'static str {
        match self {
            Self::Unavailable { .. } => "PullRequestUnavailableError",
            Self::Operation { .. } => "PullRequestOperationError",
        }
    }

    pub fn cause(&self) -> Option<&Cause> {
        match self {
            Self::Unavailable { cause, .. } | Self::Operation { cause, .. } => cause.as_ref(),
        }
    }

    /// The TS `message` getters of the two contract classes.
    pub fn message(&self) -> String {
        match self {
            Self::Unavailable { reason, provider, .. } => crate::contract::unavailable_message(*reason, *provider),
            Self::Operation { operation, detail, .. } => format!("Pull request operation {operation} failed: {detail}"),
        }
    }

    /// `toUnavailableError`: a missing or unauthenticated tool.
    pub fn from_unusable_provider(error: PullRequestProviderError) -> Self {
        Self::Unavailable {
            reason: if error.reason == ProviderFailureReason::MissingTool {
                PullRequestUnavailableReason::CliMissing
            } else {
                PullRequestUnavailableReason::CliUnauthenticated
            },
            provider: Some(error.provider),
            cause: Some(Cause::new(error)),
        }
    }

    /// `toPullRequestError(operation)`.
    pub fn from_provider(operation: &str, error: PullRequestProviderError) -> Self {
        if error.is_unusable() {
            Self::from_unusable_provider(error)
        } else {
            Self::Operation {
                operation: operation.to_owned(),
                detail: error.detail.clone(),
                cause: Some(Cause::new(error)),
            }
        }
    }

    /// The contract value, with `cause` as its defect encoding.
    pub fn to_contract(&self) -> ContractPullRequestError {
        match self {
            Self::Unavailable { reason, provider, cause } => ContractPullRequestError::Unavailable(PullRequestUnavailableError {
                tag: LitPullRequestUnavailableError,
                reason: *reason,
                provider: *provider,
                cause: cause.as_ref().map(Cause::defect),
            }),
            Self::Operation { operation, detail, cause } => ContractPullRequestError::Operation(PullRequestOperationError {
                tag: LitPullRequestOperationError,
                operation: operation.clone(),
                detail: detail.clone(),
                cause: cause.as_ref().map(Cause::defect),
            }),
        }
    }

    /// The wire JSON.
    pub fn to_wire(&self) -> Value {
        serde_json::to_value(self.to_contract()).unwrap_or(Value::Null)
    }

    /// Back from the wire JSON (the persisted read cache stores failures this way). Causes come
    /// back as opaque defects.
    pub fn from_wire(value: &Value) -> Option<Self> {
        let cause = value.get("cause").cloned().map(|defect| Cause::new(zc_core::defect::Defect(defect)));
        match value.get("_tag")?.as_str()? {
            "PullRequestUnavailableError" => {
                let wire: PullRequestUnavailableError = serde_json::from_value(value.clone()).ok()?;
                Some(Self::Unavailable {
                    reason: wire.reason,
                    provider: wire.provider,
                    cause,
                })
            }
            "PullRequestOperationError" => {
                let wire: PullRequestOperationError = serde_json::from_value(value.clone()).ok()?;
                Some(Self::Operation {
                    operation: wire.operation,
                    detail: wire.detail,
                    cause,
                })
            }
            _ => None,
        }
    }
}

/// The contract union, as serde sees it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ContractPullRequestError {
    Unavailable(PullRequestUnavailableError),
    Operation(PullRequestOperationError),
}

impl fmt::Display for PullRequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for PullRequestError {}

impl Serialize for PullRequestError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.to_contract().serialize(serializer)
    }
}

impl CauseError for PullRequestError {
    fn defect(&self) -> Value {
        error_defect(self.tag(), self.message(), self.cause().map(Cause::defect))
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl From<PullRequestError> for zc_ports::TaggedError {
    fn from(error: PullRequestError) -> Self {
        let wire = error.to_wire();
        let mut tagged = zc_ports::TaggedError::new(error.tag(), error.message());
        if let Value::Object(fields) = wire {
            for (key, value) in fields {
                if key != "_tag" {
                    tagged = tagged.with(key, value);
                }
            }
        }
        tagged
    }
}
