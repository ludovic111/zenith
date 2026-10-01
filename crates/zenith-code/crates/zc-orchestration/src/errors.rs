//! `orchestration/Errors.ts`: the errors a dispatch can fail with.
//!
//! Messages are the TS `message` getters verbatim: they are persisted in rejected receipts
//! (`orchestration_command_receipts.error`) and shown to users.

use serde_json::{json, Value};
use zc_contracts::ThreadId;
use zc_db::{Correlation, DbError};
use zc_ports::TaggedError;

/// The message of [`CommandRejection::SettleBlocked`] (`OrchestrationThreadSettleBlockedError`).
pub const SETTLE_BLOCKED_MESSAGE: &str = "This thread still needs attention. Resolve or interrupt it first, then try again.";

/// `OrchestrationCommandRejection`: the domain rejections the decider produces. A rejected
/// command gets a `rejected` receipt.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommandRejection {
    /// `OrchestrationCommandInvariantError`.
    #[error("Orchestration command invariant failed ({command_type}): {detail}")]
    Invariant { command_type: String, detail: String },
    /// `OrchestrationThreadSettleBlockedError`.
    #[error("This thread still needs attention. Resolve or interrupt it first, then try again.")]
    SettleBlocked { thread_id: ThreadId },
}

impl CommandRejection {
    pub fn invariant(command_type: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Invariant {
            command_type: command_type.into(),
            detail: detail.into(),
        }
    }

    /// The error's `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Invariant { .. } => "OrchestrationCommandInvariantError",
            Self::SettleBlocked { .. } => "OrchestrationThreadSettleBlockedError",
        }
    }

    /// The encoded tagged error.
    pub fn to_tagged(&self) -> TaggedError {
        let error = TaggedError::new(self.tag(), self.to_string());
        match self {
            Self::Invariant { command_type, detail } => error.with("commandType", command_type.as_str()).with("detail", detail.as_str()),
            Self::SettleBlocked { thread_id } => error.with("threadId", thread_id.as_str()),
        }
    }
}

/// `OrchestrationDispatchError`.
#[derive(Debug, thiserror::Error)]
pub enum OrchestrationDispatchError {
    #[error(transparent)]
    Rejected(#[from] CommandRejection),
    /// `OrchestrationCommandPreviouslyRejectedError`.
    #[error("Command previously rejected ({command_id}): {detail}")]
    PreviouslyRejected { command_id: String, detail: String },
    /// `OrchestrationCommandIdConflictError`.
    #[error("Command id '{command_id}' already used for {receipt_aggregate_kind} '{receipt_aggregate_id}'; refusing to replay its receipt for {command_aggregate_kind} '{command_aggregate_id}'.")]
    CommandIdConflict {
        command_id: String,
        receipt_aggregate_kind: String,
        receipt_aggregate_id: String,
        command_aggregate_kind: String,
        command_aggregate_id: String,
    },
    /// `OrchestrationProjectorDecodeError`. The Rust projector works on decoded events and
    /// cannot fail, so this is only produced when a payload does not decode.
    #[error("Projector decode failed for {event_type}: {issue}")]
    ProjectorDecode { event_type: String, issue: String },
    /// `PersistenceSqlError | PersistenceDecodeError` (`ProjectionRepositoryError`).
    #[error(transparent)]
    Persistence(#[from] DbError),
}

impl OrchestrationDispatchError {
    pub fn invariant(command_type: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Rejected(CommandRejection::invariant(command_type, detail))
    }

    /// The domain rejection, when this is one (`isOrchestrationCommandRejection`).
    pub fn rejection(&self) -> Option<&CommandRejection> {
        match self {
            Self::Rejected(rejection) => Some(rejection),
            _ => None,
        }
    }

    /// The error's `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Rejected(rejection) => rejection.tag(),
            Self::PreviouslyRejected { .. } => "OrchestrationCommandPreviouslyRejectedError",
            Self::CommandIdConflict { .. } => "OrchestrationCommandIdConflictError",
            Self::ProjectorDecode { .. } => "OrchestrationProjectorDecodeError",
            Self::Persistence(error) => persistence_tag(error),
        }
    }

    /// The encoded tagged error, with the human message alongside.
    pub fn to_tagged(&self) -> TaggedError {
        match self {
            Self::Rejected(rejection) => rejection.to_tagged(),
            Self::PreviouslyRejected { command_id, detail } => TaggedError::new(self.tag(), self.to_string())
                .with("commandId", command_id.as_str())
                .with("detail", detail.as_str()),
            Self::CommandIdConflict {
                command_id,
                receipt_aggregate_kind,
                receipt_aggregate_id,
                command_aggregate_kind,
                command_aggregate_id,
            } => TaggedError::new(self.tag(), self.to_string())
                .with("commandId", command_id.as_str())
                .with("receiptAggregateKind", receipt_aggregate_kind.as_str())
                .with("receiptAggregateId", receipt_aggregate_id.as_str())
                .with("commandAggregateKind", command_aggregate_kind.as_str())
                .with("commandAggregateId", command_aggregate_id.as_str()),
            Self::ProjectorDecode { event_type, issue } => TaggedError::new(self.tag(), self.to_string())
                .with("eventType", event_type.as_str())
                .with("issue", issue.as_str()),
            Self::Persistence(error) => persistence_tagged(error),
        }
    }
}

impl From<OrchestrationDispatchError> for TaggedError {
    fn from(error: OrchestrationDispatchError) -> Self {
        error.to_tagged()
    }
}

fn persistence_tag(error: &DbError) -> &'static str {
    if error.is_decode() {
        "PersistenceDecodeError"
    } else {
        "PersistenceSqlError"
    }
}

fn correlation_value(correlation: &Correlation) -> Value {
    match correlation {
        Correlation::SessionId(id) => json!({ "sessionId": id }),
        Correlation::CurrentSessionId(id) => json!({ "currentSessionId": id }),
        Correlation::PairingLinkId(id) => json!({ "pairingLinkId": id }),
        Correlation::ThreadId(id) => json!({ "threadId": id }),
    }
}

/// `PersistenceSqlError` / `PersistenceDecodeError` as a tagged error (`ProjectionRepositoryError`
/// on the wire). Errors that are not about SQL (a closed connection, a panic) become a
/// `PersistenceSqlError` with the description as `detail`.
pub fn persistence_tagged(error: &DbError) -> TaggedError {
    let message = error.to_string();
    match error {
        DbError::Sql {
            operation,
            detail,
            correlation,
            ..
        } => {
            let mut tagged = TaggedError::new("PersistenceSqlError", message).with("operation", operation.as_str());
            if let Some(detail) = detail {
                tagged = tagged.with("detail", detail.as_str());
            }
            if let Some(correlation) = correlation {
                tagged = tagged.with("correlation", correlation_value(correlation));
            }
            tagged
        }
        DbError::Decode { operation, issue, correlation } => {
            let mut tagged = TaggedError::new("PersistenceDecodeError", message)
                .with("operation", operation.as_str())
                .with("issue", issue.as_str());
            if let Some(correlation) = correlation {
                tagged = tagged.with("correlation", correlation_value(correlation));
            }
            tagged
        }
        other => TaggedError::new("PersistenceSqlError", message.clone())
            .with("operation", "OrchestrationEngine")
            .with("detail", other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_match_the_typescript_getters() {
        let invariant = CommandRejection::invariant("thread.settle", "nope");
        assert_eq!(invariant.to_string(), "Orchestration command invariant failed (thread.settle): nope");
        let blocked = CommandRejection::SettleBlocked { thread_id: ThreadId::new("t") };
        assert_eq!(blocked.to_string(), SETTLE_BLOCKED_MESSAGE);
        assert_eq!(
            serde_json::to_value(blocked.to_tagged()).unwrap(),
            json!({"_tag": "OrchestrationThreadSettleBlockedError", "threadId": "t"})
        );
        let conflict = OrchestrationDispatchError::CommandIdConflict {
            command_id: "c".into(),
            receipt_aggregate_kind: "thread".into(),
            receipt_aggregate_id: "a".into(),
            command_aggregate_kind: "thread".into(),
            command_aggregate_id: "b".into(),
        };
        assert_eq!(
            conflict.to_string(),
            "Command id 'c' already used for thread 'a'; refusing to replay its receipt for thread 'b'."
        );
        let previously = OrchestrationDispatchError::PreviouslyRejected {
            command_id: "c".into(),
            detail: "x".into(),
        };
        assert_eq!(previously.to_string(), "Command previously rejected (c): x");
        let decode = persistence_tagged(&DbError::decode("op", "bad"));
        assert_eq!(decode.tag, "PersistenceDecodeError");
        assert_eq!(decode.fields["issue"], "bad");
    }
}
