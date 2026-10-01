//! [`zc_ports::ProjectionReads`] for [`ProjectionSnapshotQuery`]: the port the RPC handlers,
//! the HTTP API, MCP, storage cleanup and the reactors read projections through.

use async_trait::async_trait;
use zc_contracts::{
    ApprovalRequestId, CheckpointRef, MessageId, OrchestrationProject, OrchestrationProjectShell, OrchestrationReadModel, OrchestrationSearchThreadsInput,
    OrchestrationSearchThreadsResult, OrchestrationShellSnapshot, OrchestrationThread, OrchestrationThreadActivity, OrchestrationThreadDetailSnapshot,
    OrchestrationThreadDetailWindow, OrchestrationThreadShell, ProjectId, ThreadId,
};
use zc_db::DbError;
use zc_ports::orchestration::{
    DeletedWorktreeThread, FullThreadDiffContext, ImportedAgentSessionSource, ReplayStats, SnapshotCounts, ThreadCheckpointContext, ThreadDetailQuery,
    ThreadPullRequests, ThreadRuntimeContext, TurnStartMessage,
};
use zc_ports::{contracts::PersistenceError, ProjectionReads, TaggedError};

use crate::query::{self, ProjectionSnapshotQuery};

/// A [`DbError`] as the wire-shaped `PersistenceSqlError` / `PersistenceDecodeError`.
pub fn persistence_error(error: DbError) -> PersistenceError {
    let message = error.to_string();
    match error {
        DbError::Decode { operation, issue, .. } => TaggedError::new("PersistenceDecodeError", message)
            .with("operation", operation)
            .with("issue", issue),
        DbError::Sql { operation, detail, .. } => {
            let error = TaggedError::new("PersistenceSqlError", message).with("operation", operation);
            match detail {
                Some(detail) => error.with("detail", detail),
                None => error,
            }
        }
        other => TaggedError::new("PersistenceSqlError", other.to_string()).with("operation", "ProjectionSnapshotQuery"),
    }
}

type Result<T> = std::result::Result<T, PersistenceError>;

#[async_trait]
impl ProjectionReads for ProjectionSnapshotQuery {
    async fn get_user_input_activity(&self, thread_id: &ThreadId, request_id: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>> {
        ProjectionSnapshotQuery::get_user_input_activity(self, thread_id.as_str(), request_id.as_str())
            .await
            .map_err(persistence_error)
    }

    async fn list_activities_by_kind(&self, kind: &str) -> Result<Vec<OrchestrationThreadActivity>> {
        ProjectionSnapshotQuery::list_activities_by_kind(self, kind).await.map_err(persistence_error)
    }

    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel> {
        ProjectionSnapshotQuery::get_command_read_model(self).await.map_err(persistence_error)
    }

    async fn get_snapshot(&self) -> Result<OrchestrationReadModel> {
        ProjectionSnapshotQuery::get_snapshot(self).await.map_err(persistence_error)
    }

    async fn get_shell_snapshot(&self, unsettled_only: bool) -> Result<OrchestrationShellSnapshot> {
        ProjectionSnapshotQuery::get_shell_snapshot(self, unsettled_only)
            .await
            .map_err(persistence_error)
    }

    async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot> {
        ProjectionSnapshotQuery::get_archived_shell_snapshot(self).await.map_err(persistence_error)
    }

    async fn list_threads_with_pull_requests(&self) -> Result<Vec<ThreadPullRequests>> {
        let rows = ProjectionSnapshotQuery::list_threads_with_pull_requests(self)
            .await
            .map_err(persistence_error)?;
        Ok(rows
            .into_iter()
            .map(|row| ThreadPullRequests {
                id: ThreadId::new(row.id),
                project_id: ProjectId::new(row.project_id),
                settled_override: row.settled_override,
                settled_at: row.settled_at,
                pull_requests: row.pull_requests,
            })
            .collect())
    }

    async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>> {
        let rows = ProjectionSnapshotQuery::get_deleted_worktree_threads(self).await.map_err(persistence_error)?;
        Ok(rows
            .into_iter()
            .map(|row| DeletedWorktreeThread {
                id: ThreadId::new(row.id),
                project_id: ProjectId::new(row.project_id),
                branch: row.branch,
                worktree_path: row.worktree_path,
                workspace_root: row.workspace_root,
                deleted_at: row.deleted_at,
            })
            .collect())
    }

    async fn search_threads(&self, input: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult> {
        ProjectionSnapshotQuery::search_threads(self, &input).await.map_err(persistence_error)
    }

    async fn get_snapshot_sequence(&self) -> Result<i64> {
        ProjectionSnapshotQuery::get_snapshot_sequence(self).await.map_err(persistence_error)
    }

    async fn get_counts(&self) -> Result<SnapshotCounts> {
        let counts = ProjectionSnapshotQuery::get_counts(self).await.map_err(persistence_error)?;
        Ok(SnapshotCounts {
            project_count: counts.project_count.max(0) as u64,
            thread_count: counts.thread_count.max(0) as u64,
        })
    }

    async fn get_event_replay_stats(&self, from_sequence_exclusive: i64, to_sequence_inclusive: i64) -> Result<ReplayStats> {
        let stats = ProjectionSnapshotQuery::get_event_replay_stats(self, from_sequence_exclusive, to_sequence_inclusive)
            .await
            .map_err(persistence_error)?;
        Ok(ReplayStats {
            event_count: stats.event_count.max(0) as u64,
            payload_bytes: stats.payload_bytes.max(0) as u64,
        })
    }

    async fn get_active_project_by_workspace_root(&self, workspace_root: &str) -> Result<Option<OrchestrationProject>> {
        ProjectionSnapshotQuery::get_active_project_by_workspace_root(self, workspace_root)
            .await
            .map_err(persistence_error)
    }

    async fn get_project_shell_by_id(&self, project_id: &ProjectId) -> Result<Option<OrchestrationProjectShell>> {
        ProjectionSnapshotQuery::get_project_shell_by_id(self, project_id.as_str())
            .await
            .map_err(persistence_error)
    }

    async fn get_project_shells(&self, project_ids: Option<Vec<ProjectId>>) -> Result<Vec<OrchestrationProjectShell>> {
        ProjectionSnapshotQuery::get_project_shells(self, project_ids.map(|ids| ids.into_iter().map(|id| id.0).collect()))
            .await
            .map_err(persistence_error)
    }

    async fn get_first_active_thread_id_by_project_id(&self, project_id: &ProjectId) -> Result<Option<ThreadId>> {
        ProjectionSnapshotQuery::get_first_active_thread_id_by_project_id(self, project_id.as_str())
            .await
            .map(|id| id.map(ThreadId::new))
            .map_err(persistence_error)
    }

    async fn get_imported_agent_session_sources(&self, project_id: &ProjectId) -> Result<Vec<ImportedAgentSessionSource>> {
        let rows = ProjectionSnapshotQuery::get_imported_agent_session_sources(self, project_id.as_str())
            .await
            .map_err(persistence_error)?;
        Ok(rows
            .into_iter()
            .map(|row| ImportedAgentSessionSource {
                thread_id: ThreadId::new(row.thread_id),
                source: row.source,
            })
            .collect())
    }

    async fn get_thread_checkpoint_context(&self, thread_id: &ThreadId) -> Result<Option<ThreadCheckpointContext>> {
        let context = ProjectionSnapshotQuery::get_thread_checkpoint_context(self, thread_id.as_str())
            .await
            .map_err(persistence_error)?;
        Ok(context.map(|context| ThreadCheckpointContext {
            thread_id: ThreadId::new(context.thread_id),
            project_id: ProjectId::new(context.project_id),
            workspace_root: context.workspace_root,
            worktree_path: context.worktree_path,
            checkpoints: context.checkpoints,
        }))
    }

    async fn get_full_thread_diff_context(&self, thread_id: &ThreadId, to_turn_count: i64) -> Result<Option<FullThreadDiffContext>> {
        let context = ProjectionSnapshotQuery::get_full_thread_diff_context(self, thread_id.as_str(), to_turn_count)
            .await
            .map_err(persistence_error)?;
        Ok(context.map(|context| FullThreadDiffContext {
            thread_id: ThreadId::new(context.thread_id),
            project_id: ProjectId::new(context.project_id),
            workspace_root: context.workspace_root,
            worktree_path: context.worktree_path,
            latest_checkpoint_turn_count: context.latest_checkpoint_turn_count,
            to_checkpoint_ref: context.to_checkpoint_ref.map(CheckpointRef::new),
        }))
    }

    async fn get_thread_shell_by_id(&self, thread_id: &ThreadId) -> Result<Option<OrchestrationThreadShell>> {
        ProjectionSnapshotQuery::get_thread_shell_by_id(self, thread_id.as_str())
            .await
            .map_err(persistence_error)
    }

    async fn get_thread_runtime_context(&self, thread_id: &ThreadId) -> Result<Option<ThreadRuntimeContext>> {
        let context = ProjectionSnapshotQuery::get_thread_runtime_context(self, thread_id.as_str())
            .await
            .map_err(persistence_error)?;
        Ok(context.map(|context| ThreadRuntimeContext {
            id: ThreadId::new(context.id),
            project_id: ProjectId::new(context.project_id),
            title: context.title,
            title_state: context.title_state,
            session: context.session,
        }))
    }

    async fn get_turn_start_message(&self, thread_id: &ThreadId, message_id: &MessageId) -> Result<Option<TurnStartMessage>> {
        let message = ProjectionSnapshotQuery::get_turn_start_message(self, thread_id.as_str(), message_id.as_str())
            .await
            .map_err(persistence_error)?;
        Ok(message.map(|message| TurnStartMessage {
            message: message.message,
            has_other_user_messages: message.has_other_user_messages,
        }))
    }

    async fn get_thread_detail_by_id(&self, thread_id: &ThreadId, query: ThreadDetailQuery) -> Result<Option<OrchestrationThread>> {
        ProjectionSnapshotQuery::get_thread_detail_by_id(
            self,
            thread_id.as_str(),
            Some(query::ThreadDetailQuery {
                activity_kinds: query.activity_kinds,
            }),
        )
        .await
        .map_err(persistence_error)
    }

    async fn get_thread_detail_snapshot(
        &self,
        thread_id: &ThreadId,
        window: Option<OrchestrationThreadDetailWindow>,
    ) -> Result<Option<OrchestrationThreadDetailSnapshot>> {
        ProjectionSnapshotQuery::get_thread_detail_snapshot(self, thread_id.as_str(), window)
            .await
            .map_err(persistence_error)
    }
}
