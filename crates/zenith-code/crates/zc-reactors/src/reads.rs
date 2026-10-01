//! What the reactors read: a slice of `ProjectionSnapshotQuery` plus the projection
//! repositories the ingestion queries directly (`ProjectionTurnRepository`,
//! `ProjectionThreadMessageRepository`, `ProjectionThreadProposedPlanRepository`,
//! `ProjectionThreadActivityRepository`).
//!
//! Shells, threads and projects cross as their wire JSON, so fakes are `json!` literals and the
//! reactors read fields by name the way the TS code does.
//!
//! Two implementations:
//! - [`ProjectionReactorReads`]: the [`ProjectionReads`] port (WP-09's snapshot queries) plus
//!   the zc-db repositories over the projection tables. This is the production wiring once the
//!   projection pipeline fills those tables.
//! - [`crate::event_log_reads::EventLogReactorReads`]: the same answers folded from the event
//!   store (the engine's projector plus a port of the turn and activity projectors), for an
//!   engine without projection tables (tests, replays, the standalone server until WP-09).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};
use zc_contracts::{MessageId, ProjectId, ThreadId, TurnId};
use zc_db::repos::proposed_plans::{self, ProjectionThreadProposedPlan};
use zc_db::repos::thread_activities::{self, ProjectionThreadActivity};
use zc_db::repos::thread_messages::{self, ProjectionThreadMessage};
use zc_db::repos::turns::{self, PendingTurnStart, ProjectionTurn};
use zc_db::{Db, DbError};
use zc_ports::orchestration::ThreadDetailQuery;
use zc_ports::{ProjectionReads, TaggedError};

pub type ReadResult<T> = Result<T, TaggedError>;

fn unsupported(operation: &str) -> TaggedError {
    TaggedError::new("ReactorReadUnsupported", format!("{operation} is not available from this reader"))
}

/// `getTurnStartMessage` result: the message (wire JSON) and whether the thread has another
/// user message that is not a bare `/compact`.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnStartMessage {
    pub message: Value,
    pub has_other_user_messages: bool,
}

/// The reads of the reactors. Every method has a default that fails, so test fakes implement
/// only what their reactor uses.
#[async_trait]
pub trait ReactorReads: Send + Sync {
    /// `getThreadRuntimeContext(threadId)`: `{id, projectId, title, titleState, session}` of an
    /// active (not deleted, not archived) thread.
    async fn thread_runtime_context(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        let _ = thread_id;
        Err(unsupported("getThreadRuntimeContext"))
    }

    /// `getThreadShellById(threadId)`: an active thread's shell.
    async fn thread_shell(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        let _ = thread_id;
        Err(unsupported("getThreadShellById"))
    }

    /// `getProjectShellById(projectId)`.
    async fn project_shell(&self, project_id: &ProjectId) -> ReadResult<Option<Value>> {
        let _ = project_id;
        Err(unsupported("getProjectShellById"))
    }

    /// `getProjectShells(projectIds?)`.
    async fn project_shells(&self, project_ids: Option<Vec<ProjectId>>) -> ReadResult<Vec<Value>> {
        let _ = project_ids;
        Err(unsupported("getProjectShells"))
    }

    /// `getThreadDetailById(threadId, {activityKinds: []})`: the thread with its messages.
    async fn thread_detail(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        let _ = thread_id;
        Err(unsupported("getThreadDetailById"))
    }

    /// `getTurnStartMessage({threadId, messageId})`.
    async fn turn_start_message(&self, thread_id: &ThreadId, message_id: &MessageId) -> ReadResult<Option<TurnStartMessage>> {
        let _ = (thread_id, message_id);
        Err(unsupported("getTurnStartMessage"))
    }

    /// `getCommandReadModel().threads` (wire JSON).
    async fn command_read_model_threads(&self) -> ReadResult<Vec<Value>> {
        Err(unsupported("getCommandReadModel"))
    }

    /// `getThreadCheckpointContext(threadId)`: `{threadId, projectId, workspaceRoot,
    /// worktreePath, checkpoints}`.
    async fn thread_checkpoint_context(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        let _ = thread_id;
        Err(unsupported("getThreadCheckpointContext"))
    }

    /// `getShellSnapshot({unsettledOnly})`: `{snapshotSequence, projects, threads, updatedAt}`.
    async fn shell_snapshot(&self, unsettled_only: bool) -> ReadResult<Value> {
        let _ = unsettled_only;
        Err(unsupported("getShellSnapshot"))
    }

    /// `getSnapshotSequence()`.
    async fn snapshot_sequence(&self) -> ReadResult<i64> {
        Err(unsupported("getSnapshotSequence"))
    }

    /// `ProjectionTurnRepository.getPendingTurnStartByThreadId`.
    async fn pending_turn_start(&self, thread_id: &ThreadId) -> ReadResult<Option<PendingTurnStart>> {
        let _ = thread_id;
        Err(unsupported("getPendingTurnStartByThreadId"))
    }

    /// `ProjectionTurnRepository.getByTurnId`.
    async fn turn(&self, thread_id: &ThreadId, turn_id: &TurnId) -> ReadResult<Option<ProjectionTurn>> {
        let _ = (thread_id, turn_id);
        Err(unsupported("getByTurnId"))
    }

    /// `ProjectionThreadMessageRepository.getByMessageId`.
    async fn message(&self, message_id: &MessageId) -> ReadResult<Option<ProjectionThreadMessage>> {
        let _ = message_id;
        Err(unsupported("getByMessageId"))
    }

    /// `ProjectionThreadMessageRepository.hasAssistantMessageForTurn`.
    async fn has_assistant_message_for_turn(&self, thread_id: &ThreadId, turn_id: &TurnId, streaming_only: bool) -> ReadResult<bool> {
        let _ = (thread_id, turn_id, streaming_only);
        Err(unsupported("hasAssistantMessageForTurn"))
    }

    /// `ProjectionThreadProposedPlanRepository.getByPlanId`.
    async fn proposed_plan(&self, thread_id: &ThreadId, plan_id: &str) -> ReadResult<Option<ProjectionThreadProposedPlan>> {
        let _ = (thread_id, plan_id);
        Err(unsupported("getByPlanId"))
    }

    /// `ProjectionThreadActivityRepository.listUserInputLifecycleByThreadId`.
    async fn user_input_lifecycle(&self, thread_id: &ThreadId) -> ReadResult<Vec<ProjectionThreadActivity>> {
        let _ = thread_id;
        Err(unsupported("listUserInputLifecycleByThreadId"))
    }

    /// `ProjectionThreadActivityRepository.getLatestTaskActivity`.
    async fn latest_task_activity(&self, thread_id: &ThreadId, task_id: &str) -> ReadResult<Option<ProjectionThreadActivity>> {
        let _ = (thread_id, task_id);
        Err(unsupported("getLatestTaskActivity"))
    }

    /// `ProjectionThreadActivityRepository.listByThreadId({threadId, activityKinds, limit})`.
    async fn activities(&self, thread_id: &ThreadId, kinds: &[&str], limit: Option<i64>) -> ReadResult<Vec<ProjectionThreadActivity>> {
        let _ = (thread_id, kinds, limit);
        Err(unsupported("listByThreadId"))
    }
}

fn tagged(error: DbError) -> TaggedError {
    zc_orchestration::errors::persistence_tagged(&error)
}

fn to_json<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// [`ReactorReads`] over WP-09's [`ProjectionReads`] and the projection tables.
pub struct ProjectionReactorReads {
    projections: Arc<dyn ProjectionReads>,
    db: Db,
}

impl ProjectionReactorReads {
    pub fn new(projections: Arc<dyn ProjectionReads>, db: Db) -> Self {
        Self { projections, db }
    }
}

#[async_trait]
impl ReactorReads for ProjectionReactorReads {
    async fn thread_runtime_context(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        Ok(self.projections.get_thread_runtime_context(thread_id).await?.map(|shell| {
            let shell = to_json(&shell);
            json!({
                "id": shell["id"],
                "projectId": shell["projectId"],
                "title": shell["title"],
                "titleState": shell.get("titleState").cloned().unwrap_or(Value::Null),
                "session": shell.get("session").cloned().unwrap_or(Value::Null),
            })
        }))
    }

    async fn thread_shell(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        Ok(self.projections.get_thread_shell_by_id(thread_id).await?.map(|shell| to_json(&shell)))
    }

    async fn project_shell(&self, project_id: &ProjectId) -> ReadResult<Option<Value>> {
        Ok(self.projections.get_project_shell_by_id(project_id).await?.map(|shell| to_json(&shell)))
    }

    async fn project_shells(&self, project_ids: Option<Vec<ProjectId>>) -> ReadResult<Vec<Value>> {
        Ok(self.projections.get_project_shells(project_ids).await?.iter().map(to_json).collect())
    }

    async fn thread_detail(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        let query = ThreadDetailQuery {
            activity_kinds: Some(Vec::new()),
        };
        Ok(self.projections.get_thread_detail_by_id(thread_id, query).await?.map(|thread| to_json(&thread)))
    }

    async fn turn_start_message(&self, thread_id: &ThreadId, message_id: &MessageId) -> ReadResult<Option<TurnStartMessage>> {
        Ok(self
            .projections
            .get_turn_start_message(thread_id, message_id)
            .await?
            .map(|start| TurnStartMessage {
                message: to_json(&start.message),
                has_other_user_messages: start.has_other_user_messages,
            }))
    }

    async fn command_read_model_threads(&self) -> ReadResult<Vec<Value>> {
        let model = self.projections.get_command_read_model().await?;
        Ok(model.threads.iter().map(to_json).collect())
    }

    async fn thread_checkpoint_context(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        Ok(self.projections.get_thread_checkpoint_context(thread_id).await?.map(|context| {
            json!({
                "threadId": context.thread_id,
                "projectId": context.project_id,
                "workspaceRoot": context.workspace_root,
                "worktreePath": context.worktree_path,
                "checkpoints": to_json(&context.checkpoints),
            })
        }))
    }

    async fn shell_snapshot(&self, unsettled_only: bool) -> ReadResult<Value> {
        Ok(to_json(&self.projections.get_shell_snapshot(unsettled_only).await?))
    }

    async fn snapshot_sequence(&self) -> ReadResult<i64> {
        self.projections.get_snapshot_sequence().await
    }

    async fn pending_turn_start(&self, thread_id: &ThreadId) -> ReadResult<Option<PendingTurnStart>> {
        let thread_id = thread_id.0.clone();
        self.db
            .read(move |conn| turns::get_pending_turn_start_by_thread_id(conn, &thread_id))
            .await
            .map_err(tagged)
    }

    async fn turn(&self, thread_id: &ThreadId, turn_id: &TurnId) -> ReadResult<Option<ProjectionTurn>> {
        let (thread_id, turn_id) = (thread_id.0.clone(), turn_id.0.clone());
        self.db
            .read(move |conn| turns::get_by_turn_id(conn, &thread_id, &turn_id))
            .await
            .map_err(tagged)
    }

    async fn message(&self, message_id: &MessageId) -> ReadResult<Option<ProjectionThreadMessage>> {
        let message_id = message_id.0.clone();
        self.db
            .read(move |conn| thread_messages::get_by_message_id(conn, &message_id))
            .await
            .map_err(tagged)
    }

    async fn has_assistant_message_for_turn(&self, thread_id: &ThreadId, turn_id: &TurnId, streaming_only: bool) -> ReadResult<bool> {
        let (thread_id, turn_id) = (thread_id.0.clone(), turn_id.0.clone());
        self.db
            .read(move |conn| thread_messages::has_assistant_message_for_turn(conn, &thread_id, &turn_id, streaming_only))
            .await
            .map_err(tagged)
    }

    async fn proposed_plan(&self, thread_id: &ThreadId, plan_id: &str) -> ReadResult<Option<ProjectionThreadProposedPlan>> {
        let (thread_id, plan_id) = (thread_id.0.clone(), plan_id.to_owned());
        self.db
            .read(move |conn| proposed_plans::get_by_plan_id(conn, &thread_id, &plan_id))
            .await
            .map_err(tagged)
    }

    async fn user_input_lifecycle(&self, thread_id: &ThreadId) -> ReadResult<Vec<ProjectionThreadActivity>> {
        let thread_id = thread_id.0.clone();
        self.db
            .read(move |conn| thread_activities::list_user_input_lifecycle_by_thread_id(conn, &thread_id))
            .await
            .map_err(tagged)
    }

    async fn latest_task_activity(&self, thread_id: &ThreadId, task_id: &str) -> ReadResult<Option<ProjectionThreadActivity>> {
        let (thread_id, task_id) = (thread_id.0.clone(), task_id.to_owned());
        self.db
            .read(move |conn| thread_activities::get_latest_task_activity(conn, &thread_id, &task_id))
            .await
            .map_err(tagged)
    }

    async fn activities(&self, thread_id: &ThreadId, kinds: &[&str], limit: Option<i64>) -> ReadResult<Vec<ProjectionThreadActivity>> {
        let thread_id = thread_id.0.clone();
        let kinds: Vec<String> = kinds.iter().map(|kind| (*kind).to_owned()).collect();
        self.db
            .read(move |conn| thread_activities::list_by_thread_id(conn, &thread_id, Some(&kinds), limit))
            .await
            .map_err(tagged)
    }
}
