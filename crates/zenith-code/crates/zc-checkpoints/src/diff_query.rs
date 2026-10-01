//! `checkpointing/CheckpointDiffQuery.ts`: the patch of one turn, or of a thread up to a turn,
//! computed live from the hidden checkpoint refs.

use std::sync::Arc;

use zc_contracts::{OrchestrationGetFullThreadDiffInput, OrchestrationGetTurnDiffInput, ThreadId, ThreadTurnDiff};
use zc_ports::ProjectionReads;

use crate::errors::{CheckpointDiffOperation, CheckpointEnd, CheckpointServiceError};
use crate::store::{CheckpointStore, DiffCheckpointsInput, DiffFormat};
use crate::utils::checkpoint_ref_for_thread_turn;

/// `CheckpointDiffQuery`.
#[derive(Clone)]
pub struct CheckpointDiffQuery {
    projections: Arc<dyn ProjectionReads>,
    store: Arc<dyn CheckpointStore>,
}

/// `Schema.is(OrchestrationGetTurnDiffResult)`: turn counts are non-negative integers.
fn checked(operation: CheckpointDiffOperation, result: ThreadTurnDiff) -> Result<ThreadTurnDiff, CheckpointServiceError> {
    if result.from_turn_count < 0 || result.to_turn_count < 0 {
        return Err(CheckpointServiceError::DiffResultInvalid {
            operation,
            thread_id: result.thread_id,
        });
    }
    Ok(result)
}

fn result(thread_id: &ThreadId, from_turn_count: i64, to_turn_count: i64, diff: String) -> ThreadTurnDiff {
    ThreadTurnDiff {
        from_turn_count,
        to_turn_count,
        thread_id: thread_id.clone(),
        diff,
    }
}

impl CheckpointDiffQuery {
    pub fn new(projections: Arc<dyn ProjectionReads>, store: Arc<dyn CheckpointStore>) -> Self {
        Self { projections, store }
    }

    /// `getTurnDiff(input)`: the patch between two turn checkpoints. Whitespace changes are
    /// hidden unless `ignoreWhitespace` is false.
    pub async fn get_turn_diff(&self, input: &OrchestrationGetTurnDiffInput) -> Result<ThreadTurnDiff, CheckpointServiceError> {
        let operation = CheckpointDiffOperation::GetTurnDiff;
        let ignore_whitespace = input.ignore_whitespace.unwrap_or(true);
        let thread_id = &input.thread_id;
        if input.from_turn_count == input.to_turn_count {
            return checked(operation, result(thread_id, input.from_turn_count, input.to_turn_count, String::new()));
        }

        let context = self
            .projections
            .get_thread_checkpoint_context(thread_id)
            .await
            .map_err(CheckpointServiceError::Projection)?
            .ok_or_else(|| CheckpointServiceError::ThreadNotFound {
                operation,
                thread_id: thread_id.clone(),
            })?;

        let max_turn_count = context.checkpoints.iter().map(|c| c.checkpoint_turn_count).fold(0, i64::max);
        if input.to_turn_count > max_turn_count {
            return Err(CheckpointServiceError::TurnRangeUnavailable {
                operation,
                thread_id: thread_id.clone(),
                requested_turn_count: input.to_turn_count,
                available_turn_count: max_turn_count,
            });
        }

        let workspace_cwd = context.worktree_path.clone().unwrap_or_else(|| context.workspace_root.clone());
        if workspace_cwd.is_empty() {
            return Err(CheckpointServiceError::WorkspacePathMissing {
                operation,
                thread_id: thread_id.clone(),
            });
        }

        let find = |turn_count: i64| {
            context
                .checkpoints
                .iter()
                .find(|checkpoint| checkpoint.checkpoint_turn_count == turn_count)
                .map(|checkpoint| checkpoint.checkpoint_ref.clone())
        };
        let from_checkpoint_ref = if input.from_turn_count == 0 {
            Some(checkpoint_ref_for_thread_turn(thread_id, 0))
        } else {
            find(input.from_turn_count)
        }
        .ok_or_else(|| CheckpointServiceError::RefUnavailable {
            operation,
            thread_id: thread_id.clone(),
            turn_count: input.from_turn_count,
            checkpoint: CheckpointEnd::From,
        })?;
        let to_checkpoint_ref = find(input.to_turn_count).ok_or_else(|| CheckpointServiceError::RefUnavailable {
            operation,
            thread_id: thread_id.clone(),
            turn_count: input.to_turn_count,
            checkpoint: CheckpointEnd::To,
        })?;

        let diff = self
            .store
            .diff_checkpoints(&DiffCheckpointsInput {
                cwd: workspace_cwd,
                from_checkpoint_ref,
                to_checkpoint_ref,
                fallback_from_to_head: false,
                ignore_whitespace,
                format: DiffFormat::Patch,
            })
            .await?;
        checked(operation, result(thread_id, input.from_turn_count, input.to_turn_count, diff))
    }

    /// `getFullThreadDiff(input)`: turn-diff semantics from turn 0, through the narrow
    /// full-thread context lookup.
    pub async fn get_full_thread_diff(&self, input: &OrchestrationGetFullThreadDiffInput) -> Result<ThreadTurnDiff, CheckpointServiceError> {
        let operation = CheckpointDiffOperation::GetFullThreadDiff;
        let ignore_whitespace = input.ignore_whitespace.unwrap_or(true);
        let thread_id = &input.thread_id;
        if input.to_turn_count == 0 {
            return checked(operation, result(thread_id, 0, 0, String::new()));
        }

        let context = self
            .projections
            .get_full_thread_diff_context(thread_id, input.to_turn_count)
            .await
            .map_err(CheckpointServiceError::Projection)?
            .ok_or_else(|| CheckpointServiceError::ThreadNotFound {
                operation,
                thread_id: thread_id.clone(),
            })?;

        if input.to_turn_count > context.latest_checkpoint_turn_count {
            return Err(CheckpointServiceError::TurnRangeUnavailable {
                operation,
                thread_id: thread_id.clone(),
                requested_turn_count: input.to_turn_count,
                available_turn_count: context.latest_checkpoint_turn_count,
            });
        }

        let workspace_cwd = context.worktree_path.clone().unwrap_or_else(|| context.workspace_root.clone());
        if workspace_cwd.is_empty() {
            return Err(CheckpointServiceError::WorkspacePathMissing {
                operation,
                thread_id: thread_id.clone(),
            });
        }
        let to_checkpoint_ref = context
            .to_checkpoint_ref
            .clone()
            .filter(|reference| !reference.as_str().is_empty())
            .ok_or_else(|| CheckpointServiceError::RefUnavailable {
                operation,
                thread_id: thread_id.clone(),
                turn_count: input.to_turn_count,
                checkpoint: CheckpointEnd::To,
            })?;

        let diff = self
            .store
            .diff_checkpoints(&DiffCheckpointsInput {
                cwd: workspace_cwd,
                from_checkpoint_ref: checkpoint_ref_for_thread_turn(thread_id, 0),
                to_checkpoint_ref,
                fallback_from_to_head: false,
                ignore_whitespace,
                format: DiffFormat::Patch,
            })
            .await?;
        checked(operation, result(thread_id, 0, input.to_turn_count, diff))
    }
}
