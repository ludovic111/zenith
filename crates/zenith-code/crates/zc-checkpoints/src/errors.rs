//! `checkpointing/Errors.ts`.

use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use zc_contracts::ThreadId;
use zc_core::defect::Defect;
use zc_ports::TaggedError;
use zc_vcs::VcsError;

/// `CheckpointStoreError` (= `VcsError`).
pub type CheckpointStoreError = VcsError;

/// `CheckpointDiffOperation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointDiffOperation {
    GetTurnDiff,
    GetFullThreadDiff,
}

impl CheckpointDiffOperation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GetTurnDiff => "CheckpointDiffQuery.getTurnDiff",
            Self::GetFullThreadDiff => "CheckpointDiffQuery.getFullThreadDiff",
        }
    }

    fn diff_noun(self) -> &'static str {
        match self {
            Self::GetTurnDiff => "turn diff",
            Self::GetFullThreadDiff => "full thread diff",
        }
    }
}

/// Which end of a diff range lacks its ref (`CheckpointRefUnavailableError.checkpoint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckpointEnd {
    From,
    To,
}

impl CheckpointEnd {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::From => "from",
            Self::To => "to",
        }
    }
}

/// `CheckpointServiceError`: every failure of the checkpoint services.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckpointServiceError {
    Store(CheckpointStoreError),
    /// `ProjectionRepositoryError` from the snapshot query.
    Projection(TaggedError),
    DiffResultInvalid {
        operation: CheckpointDiffOperation,
        thread_id: ThreadId,
    },
    ThreadNotFound {
        operation: CheckpointDiffOperation,
        thread_id: ThreadId,
    },
    WorkspacePathMissing {
        operation: CheckpointDiffOperation,
        thread_id: ThreadId,
    },
    TurnRangeUnavailable {
        operation: CheckpointDiffOperation,
        thread_id: ThreadId,
        requested_turn_count: i64,
        available_turn_count: i64,
    },
    RefUnavailable {
        operation: CheckpointDiffOperation,
        thread_id: ThreadId,
        turn_count: i64,
        checkpoint: CheckpointEnd,
    },
}

impl CheckpointServiceError {
    /// The `_tag`.
    pub fn tag(&self) -> String {
        match self {
            Self::Store(error) => error.tag().to_owned(),
            Self::Projection(error) => error.tag.clone(),
            Self::DiffResultInvalid { .. } => "CheckpointDiffResultInvalidError".into(),
            Self::ThreadNotFound { .. } => "CheckpointThreadNotFoundError".into(),
            Self::WorkspacePathMissing { .. } => "CheckpointWorkspacePathMissingError".into(),
            Self::TurnRangeUnavailable { .. } => "CheckpointTurnRangeUnavailableError".into(),
            Self::RefUnavailable { .. } => "CheckpointRefUnavailableError".into(),
        }
    }

    /// The error's `message` getter.
    pub fn message(&self) -> String {
        match self {
            Self::Store(error) => error.message(),
            Self::Projection(error) => error.to_string(),
            Self::DiffResultInvalid { operation, .. } => format!(
                "Checkpoint invariant violation in {}: Computed {} result does not satisfy contract schema.",
                operation.as_str(),
                operation.diff_noun()
            ),
            Self::ThreadNotFound { operation, thread_id } => {
                format!("Checkpoint invariant violation in {}: Thread '{thread_id}' not found.", operation.as_str())
            }
            Self::WorkspacePathMissing { operation, thread_id } => format!(
                "Checkpoint invariant violation in {}: Workspace path missing for thread '{thread_id}' when computing {}.",
                operation.as_str(),
                operation.diff_noun()
            ),
            Self::TurnRangeUnavailable {
                thread_id,
                requested_turn_count,
                available_turn_count,
                ..
            } => format!(
                "Checkpoint unavailable for thread {thread_id} turn {requested_turn_count}: Turn diff range exceeds current turn count: requested {requested_turn_count}, current {available_turn_count}."
            ),
            Self::RefUnavailable { thread_id, turn_count, .. } => {
                format!("Checkpoint unavailable for thread {thread_id} turn {turn_count}: Checkpoint ref is unavailable for turn {turn_count}.")
            }
        }
    }

    /// The error as a `Schema.Defect` cause (`{name: _tag, message}`), which is how the
    /// `orchestration.getTurnDiff` errors carry it on the wire.
    pub fn to_defect(&self) -> Defect {
        Defect::error(&self.tag(), self.message())
    }
}

impl std::fmt::Display for CheckpointServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for CheckpointServiceError {}

impl From<VcsError> for CheckpointServiceError {
    fn from(error: VcsError) -> Self {
        Self::Store(error)
    }
}

impl Serialize for CheckpointServiceError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Store(error) => error.serialize(serializer),
            Self::Projection(error) => error.serialize(serializer),
            Self::DiffResultInvalid { operation, thread_id }
            | Self::ThreadNotFound { operation, thread_id }
            | Self::WorkspacePathMissing { operation, thread_id } => {
                let mut map = serializer.serialize_map(Some(3))?;
                map.serialize_entry("_tag", &self.tag())?;
                map.serialize_entry("operation", operation.as_str())?;
                map.serialize_entry("threadId", thread_id)?;
                map.end()
            }
            Self::TurnRangeUnavailable {
                operation,
                thread_id,
                requested_turn_count,
                available_turn_count,
            } => {
                let mut map = serializer.serialize_map(Some(5))?;
                map.serialize_entry("_tag", &self.tag())?;
                map.serialize_entry("operation", operation.as_str())?;
                map.serialize_entry("threadId", thread_id)?;
                map.serialize_entry("requestedTurnCount", requested_turn_count)?;
                map.serialize_entry("availableTurnCount", available_turn_count)?;
                map.end()
            }
            Self::RefUnavailable {
                operation,
                thread_id,
                turn_count,
                checkpoint,
            } => {
                let mut map = serializer.serialize_map(Some(5))?;
                map.serialize_entry("_tag", &self.tag())?;
                map.serialize_entry("operation", operation.as_str())?;
                map.serialize_entry("threadId", thread_id)?;
                map.serialize_entry("turnCount", turn_count)?;
                map.serialize_entry("checkpoint", checkpoint.as_str())?;
                map.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn messages_and_encoding_match_the_ts_errors() {
        let error = CheckpointServiceError::ThreadNotFound {
            operation: CheckpointDiffOperation::GetTurnDiff,
            thread_id: ThreadId::new("thread-missing"),
        };
        assert_eq!(
            error.message(),
            "Checkpoint invariant violation in CheckpointDiffQuery.getTurnDiff: Thread 'thread-missing' not found."
        );
        assert_eq!(
            serde_json::to_value(&error).unwrap(),
            json!({"_tag": "CheckpointThreadNotFoundError", "operation": "CheckpointDiffQuery.getTurnDiff", "threadId": "thread-missing"})
        );
        assert_eq!(
            serde_json::to_value(error.to_defect()).unwrap(),
            json!({"name": "CheckpointThreadNotFoundError", "message": error.message()})
        );
        let range = CheckpointServiceError::TurnRangeUnavailable {
            operation: CheckpointDiffOperation::GetFullThreadDiff,
            thread_id: ThreadId::new("t"),
            requested_turn_count: 4,
            available_turn_count: 2,
        };
        assert_eq!(
            range.message(),
            "Checkpoint unavailable for thread t turn 4: Turn diff range exceeds current turn count: requested 4, current 2."
        );
        let missing = CheckpointServiceError::WorkspacePathMissing {
            operation: CheckpointDiffOperation::GetFullThreadDiff,
            thread_id: ThreadId::new("t"),
        };
        assert_eq!(
            missing.message(),
            "Checkpoint invariant violation in CheckpointDiffQuery.getFullThreadDiff: Workspace path missing for thread 't' when computing full thread diff."
        );
        let reference = CheckpointServiceError::RefUnavailable {
            operation: CheckpointDiffOperation::GetTurnDiff,
            thread_id: ThreadId::new("t"),
            turn_count: 2,
            checkpoint: CheckpointEnd::From,
        };
        assert_eq!(
            serde_json::to_value(&reference).unwrap(),
            json!({"_tag": "CheckpointRefUnavailableError", "operation": "CheckpointDiffQuery.getTurnDiff", "threadId": "t", "turnCount": 2, "checkpoint": "from"})
        );
    }
}
