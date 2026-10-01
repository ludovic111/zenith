//! `project/AgentSessionImporter.ts` (`agentSessions.import`): the recent transcript text of a
//! project's external Claude and Codex sessions becomes `import:<instance>:<session>` threads
//! (`thread.create` with `historyImport`, then `thread.history.import`), with the stopped
//! provider binding that lets the user resume the session.
//!
//! Per session, everything is best effort: a session that cannot be imported is logged and
//! counted as skipped. A retry never replaces completed history or an active binding, and a
//! thread the user already used is left alone.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{
    AgentSessionImportInput, AgentSessionImportProjectChangedError, AgentSessionImportProjectNotFoundError, AgentSessionImportResult, AgentSessionImportSource,
    AgentSessionScanError, AgentSessionScanErrorOperation, AgentSessionSource, AgentSessionsImportError, LitAgentSessionImportProjectChangedError,
    LitAgentSessionImportProjectNotFoundError, OrchestrationCommand, OrchestrationProjectShell, OrchestrationThread, ProjectId, ProviderDriverKind,
    ProviderInstanceId, ThreadId,
};
use zc_ports::orchestration::ThreadDetailQuery;
use zc_ports::{OrchestrationDispatch, ProjectionReads};
use zc_providers::directory::{ProviderRuntimeBinding, RuntimeStatus};
use zc_providers::ProviderSessionDirectory;

use super::scanner::{normalize_project_path_for_comparison, AgentSessionScanner, RecentThread};

/// `DEFAULT_RUNTIME_MODE`.
const DEFAULT_RUNTIME_MODE: &str = "full-access";
/// `DEFAULT_PROVIDER_INTERACTION_MODE`.
const DEFAULT_INTERACTION_MODE: &str = "default";
/// `DEFAULT_MODEL`.
const DEFAULT_MODEL: &str = "gpt-6-astra";

/// `DEFAULT_MODEL_BY_PROVIDER` for the two importable drivers.
fn default_model(source: AgentSessionSource) -> &'static str {
    match source {
        AgentSessionSource::Codex => DEFAULT_MODEL,
        AgentSessionSource::ClaudeAgent => "claude-fable-5-1",
    }
}

/// `CLAUDE_SESSION_ID_PATTERN`: a resumable Claude session id is a UUID.
fn is_claude_session_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().enumerate() {
        let ok = match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            14 => (b'1'..=b'8').contains(byte),
            19 => matches!(byte.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_hexdigit(),
        };
        if !ok {
            return false;
        }
    }
    true
}

/// The stored binding of a thread, as far as the import reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportBinding {
    pub provider: String,
    pub provider_instance_id: Option<String>,
    pub status: Option<String>,
}

/// The provider session directory, as far as the import uses it (tests script it).
#[async_trait]
pub trait ImportSessionDirectory: Send + Sync {
    async fn get_binding(&self, thread_id: &ThreadId) -> Result<Option<ImportBinding>, String>;
    /// Insert the stopped binding unless the thread already has one (`onConflict: "ignore"`).
    async fn insert_stopped_binding(
        &self,
        thread_id: &ThreadId,
        provider: &str,
        provider_instance_id: &ProviderInstanceId,
        resume_cursor: Value,
        runtime_payload: Value,
    ) -> Result<(), String>;
    async fn record_imported_transcript(&self, thread_id: &ThreadId, source: &AgentSessionImportSource) -> Result<(), String>;
}

#[async_trait]
impl ImportSessionDirectory for ProviderSessionDirectory {
    async fn get_binding(&self, thread_id: &ThreadId) -> Result<Option<ImportBinding>, String> {
        let binding = ProviderSessionDirectory::get_binding(self, thread_id).await.map_err(|e| format!("{e:?}"))?;
        Ok(binding.map(|binding| ImportBinding {
            provider: binding.provider.to_string(),
            provider_instance_id: binding.provider_instance_id.map(|id| id.to_string()),
            status: binding.status.map(|status| status.as_str().to_owned()),
        }))
    }

    async fn insert_stopped_binding(
        &self,
        thread_id: &ThreadId,
        provider: &str,
        provider_instance_id: &ProviderInstanceId,
        resume_cursor: Value,
        runtime_payload: Value,
    ) -> Result<(), String> {
        let mut binding = ProviderRuntimeBinding::new(thread_id.clone(), ProviderDriverKind::new(provider), provider_instance_id.clone());
        binding.status = Some(RuntimeStatus::Stopped);
        binding.runtime_mode = serde_json::from_value(json!(DEFAULT_RUNTIME_MODE)).ok();
        binding.resume_cursor = Some(resume_cursor);
        binding.runtime_payload = Some(runtime_payload);
        self.upsert(binding, zc_db::repos::provider_session_runtime::OnConflict::Ignore)
            .await
            .map_err(|e| format!("{e:?}"))
    }

    async fn record_imported_transcript(&self, thread_id: &ThreadId, source: &AgentSessionImportSource) -> Result<(), String> {
        let source = serde_json::to_value(source).map_err(|e| e.to_string())?;
        ProviderSessionDirectory::record_imported_transcript(self, thread_id, source)
            .await
            .map_err(|e| format!("{e:?}"))
    }
}

/// `AgentSessionScanner.recentThreads` (tests script it).
#[async_trait]
pub trait RecentThreadsSource: Send + Sync {
    async fn recent_threads(
        &self,
        workspace_root: &str,
        completed_sources: Vec<AgentSessionImportSource>,
    ) -> Result<BoxStream<'static, RecentThread>, AgentSessionScanError>;
}

#[async_trait]
impl RecentThreadsSource for AgentSessionScanner {
    async fn recent_threads(
        &self,
        workspace_root: &str,
        completed_sources: Vec<AgentSessionImportSource>,
    ) -> Result<BoxStream<'static, RecentThread>, AgentSessionScanError> {
        AgentSessionScanner::recent_threads(self, workspace_root, completed_sources).await
    }
}

/// The projection reads the import uses (tests script them).
#[async_trait]
pub trait ImportReads: Send + Sync {
    async fn get_project_shell_by_id(&self, project_id: &ProjectId) -> Result<Option<OrchestrationProjectShell>, String>;
    async fn get_imported_agent_session_sources(&self, project_id: &ProjectId) -> Result<Vec<AgentSessionImportSource>, String>;
    async fn get_thread_detail_by_id(&self, thread_id: &ThreadId) -> Result<Option<OrchestrationThread>, String>;
}

/// [`ImportReads`] over the projection queries.
pub struct ProjectionImportReads(pub Arc<dyn ProjectionReads>);

#[async_trait]
impl ImportReads for ProjectionImportReads {
    async fn get_project_shell_by_id(&self, project_id: &ProjectId) -> Result<Option<OrchestrationProjectShell>, String> {
        self.0.get_project_shell_by_id(project_id).await.map_err(|e| format!("{e:?}"))
    }

    async fn get_imported_agent_session_sources(&self, project_id: &ProjectId) -> Result<Vec<AgentSessionImportSource>, String> {
        let sources = self.0.get_imported_agent_session_sources(project_id).await.map_err(|e| format!("{e:?}"))?;
        Ok(sources.into_iter().map(|entry| entry.source).collect())
    }

    async fn get_thread_detail_by_id(&self, thread_id: &ThreadId) -> Result<Option<OrchestrationThread>, String> {
        self.0
            .get_thread_detail_by_id(thread_id, ThreadDetailQuery::default())
            .await
            .map_err(|e| format!("{e:?}"))
    }
}

/// The engine as the import uses it (tests script it).
#[async_trait]
pub trait ImportEngine: Send + Sync {
    async fn dispatch(&self, command: OrchestrationCommand) -> Result<(), String>;
}

/// [`ImportEngine`] over the orchestration engine (no client origin).
pub struct EngineImport(pub Arc<dyn OrchestrationDispatch>);

#[async_trait]
impl ImportEngine for EngineImport {
    async fn dispatch(&self, command: OrchestrationCommand) -> Result<(), String> {
        self.0.dispatch(command, None).await.map(|_| ()).map_err(|e| format!("{e:?}"))
    }
}

/// What the import needs.
#[derive(Clone)]
pub struct AgentSessionImporter {
    pub scanner: Arc<dyn RecentThreadsSource>,
    pub engine: Arc<dyn ImportEngine>,
    pub reads: Arc<dyn ImportReads>,
    pub directory: Arc<dyn ImportSessionDirectory>,
}

fn has_imported_history(thread: &OrchestrationThread) -> bool {
    thread.messages.iter().any(|message| zc_contracts_is_imported(message.id.as_str()))
}

/// `isImportedAgentSessionMessageId`.
fn zc_contracts_is_imported(message_id: &str) -> bool {
    message_id.starts_with("import:")
}

/// `hasImportBlockingActivity`: anything the user did on the thread (or a non-imported
/// message) blocks writing history into it.
fn has_import_blocking_activity(thread: &OrchestrationThread, imported_history_present: bool) -> bool {
    let encoded = serde_json::to_value(thread).unwrap_or(Value::Null);
    let present = |key: &str| encoded.get(key).is_some_and(|value| !value.is_null());
    let non_empty = |key: &str| encoded.get(key).and_then(Value::as_array).is_some_and(|items| !items.is_empty());
    present("archivedAt")
        || present("deletedAt")
        || present("latestTurn")
        || present("session")
        || thread.messages.iter().any(|message| !zc_contracts_is_imported(message.id.as_str()))
        || non_empty("proposedPlans")
        || non_empty("activities")
        || non_empty("checkpoints")
        || present("snoozedUntil")
        || present("snoozedAt")
        || present("pinnedAt")
        || present("pinOrderKey")
        || present("autoSettleDisabledAt")
        || present("titleRegeneration")
        || present("linkedPullRequest")
        || present("unsettledAt")
        || if imported_history_present {
            encoded.get("settledOverride").and_then(Value::as_str) != Some("settled")
        } else {
            present("settledOverride") || present("settledAt")
        }
}

fn read_projects_error(cause: impl std::fmt::Debug) -> AgentSessionsImportError {
    AgentSessionsImportError::AgentSessionScanError(zc_contracts::AgentSessionScanError {
        tag: zc_contracts::LitAgentSessionScanError,
        operation: AgentSessionScanErrorOperation::ReadProjects,
        cause: json!({ "name": "Error", "message": format!("{cause:?}") }),
    })
}

fn command(value: Value) -> Result<OrchestrationCommand, String> {
    serde_json::from_value(value).map_err(|e| e.to_string())
}

impl AgentSessionImporter {
    /// The importer over the server's services.
    pub fn new(
        scanner: AgentSessionScanner,
        engine: Arc<dyn OrchestrationDispatch>,
        reads: Arc<dyn ProjectionReads>,
        directory: ProviderSessionDirectory,
    ) -> Self {
        Self {
            scanner: Arc::new(scanner),
            engine: Arc::new(EngineImport(engine)),
            reads: Arc::new(ProjectionImportReads(reads)),
            directory: Arc::new(directory),
        }
    }

    /// `importRecentAgentThreads(input)`.
    pub async fn import(&self, input: AgentSessionImportInput) -> Result<AgentSessionImportResult, AgentSessionsImportError> {
        let project = self
            .reads
            .get_project_shell_by_id(&input.project_id)
            .await
            .map_err(read_projects_error)?
            .ok_or_else(|| {
                AgentSessionsImportError::AgentSessionImportProjectNotFoundError(AgentSessionImportProjectNotFoundError {
                    tag: LitAgentSessionImportProjectNotFoundError,
                    project_id: input.project_id.clone(),
                })
            })?;
        let workspace_root = project.workspace_root.to_string();
        if let Some(expected) = input.expected_workspace_root.as_deref() {
            if normalize_project_path_for_comparison(&workspace_root) != normalize_project_path_for_comparison(expected) {
                return Err(AgentSessionsImportError::AgentSessionImportProjectChangedError(
                    AgentSessionImportProjectChangedError {
                        tag: LitAgentSessionImportProjectChangedError,
                        project_id: input.project_id.clone(),
                    },
                ));
            }
        }
        let completed = self
            .reads
            .get_imported_agent_session_sources(&input.project_id)
            .await
            .map_err(read_projects_error)?;
        let mut threads = self
            .scanner
            .recent_threads(&workspace_root, completed)
            .await
            .map_err(AgentSessionsImportError::AgentSessionScanError)?;
        let mut imported_thread_ids: HashSet<String> = HashSet::new();
        let mut imported_count = 0i64;
        let mut skipped_count = 0i64;
        while let Some(outcome) = threads.next().await {
            match outcome {
                RecentThread::Skipped => skipped_count += 1,
                RecentThread::AlreadyImported { source } => {
                    imported_thread_ids.insert(format!("import:{}:{}", source.provider_instance_id, source.provider_session_id));
                    imported_count += 1;
                }
                RecentThread::Duplicate { source } => {
                    let thread_id = format!("import:{}:{}", source.provider_instance_id, source.provider_session_id);
                    if imported_thread_ids.contains(&thread_id) {
                        if let Err(cause) = self.directory.record_imported_transcript(&ThreadId::new(thread_id.clone()), &source).await {
                            skipped_count += 1;
                            tracing::warn!(thread_id, cause, "Could not record an imported transcript copy");
                        }
                    }
                }
                RecentThread::Importable { thread, source } => {
                    let thread_id = format!("import:{}:{}", thread.provider_instance_id, thread.provider_session_id);
                    match self.import_one(&input, &workspace_root, &thread, &source, &thread_id).await {
                        Ok(()) => {
                            imported_thread_ids.insert(thread_id);
                            imported_count += 1;
                        }
                        Err(cause) => {
                            tracing::warn!(
                                provider = thread.source.as_str(),
                                session_id = thread.provider_session_id,
                                cause,
                                "Could not import an agent session"
                            );
                            skipped_count += 1;
                        }
                    }
                }
            }
        }
        Ok(AgentSessionImportResult { imported_count, skipped_count })
    }

    async fn import_one(
        &self,
        input: &AgentSessionImportInput,
        workspace_root: &str,
        thread: &super::transcript::AgentSessionThread,
        source: &AgentSessionImportSource,
        thread_id: &str,
    ) -> Result<(), String> {
        let thread_id = ThreadId::new(thread_id);
        let provider = thread.source.as_str();
        let model = thread.model.clone().unwrap_or_else(|| default_model(thread.source).to_owned());
        let existing = self.reads.get_thread_detail_by_id(&thread_id).await?;
        let binding = self.directory.get_binding(&thread_id).await?;

        if thread.source == AgentSessionSource::ClaudeAgent && !is_claude_session_id(&thread.provider_session_id) {
            return Err(format!("Session '{}' from '{provider}' cannot be resumed.", thread.provider_session_id));
        }
        if let Some(existing) = existing.as_ref().filter(|existing| existing.project_id != input.project_id) {
            return Err(format!(
                "Imported thread '{thread_id}' belongs to project '{}', not '{}'.",
                existing.project_id, input.project_id
            ));
        }
        let imported_history_present = existing.as_ref().is_some_and(has_imported_history);
        if existing.is_some() && imported_history_present && binding.is_some() {
            return self.directory.record_imported_transcript(&thread_id, source).await;
        }
        if existing
            .as_ref()
            .is_some_and(|existing| has_import_blocking_activity(existing, imported_history_present))
        {
            return Err(format!("Imported thread '{thread_id}' changed before its history import completed."));
        }
        if let Some(binding) = binding.as_ref() {
            if binding.provider != provider
                || binding.provider_instance_id.as_deref() != Some(thread.provider_instance_id.as_str())
                || binding.status.as_deref() != Some("stopped")
            {
                return Err(format!("Imported thread '{thread_id}' changed before its history import completed."));
            }
        }
        // The cursor is installed before the thread becomes visible. A concurrent real session
        // can replace it; insert-ignore keeps this import from replacing a newer binding.
        if binding.is_none() {
            let resume_cursor = if thread.source == AgentSessionSource::Codex {
                json!({ "threadId": thread.provider_session_id })
            } else {
                json!({ "threadId": thread_id, "resume": thread.provider_session_id })
            };
            self.directory
                .insert_stopped_binding(
                    &thread_id,
                    provider,
                    &thread.provider_instance_id,
                    resume_cursor,
                    json!({ "cwd": workspace_root }),
                )
                .await?;
        }
        if existing.is_none() {
            let create = command(json!({
                "type": "thread.create",
                "commandId": zc_core::ids::uuid_v4(),
                "threadId": thread_id,
                "projectId": input.project_id,
                "title": thread.title,
                "modelSelection": { "instanceId": thread.provider_instance_id, "model": model },
                "runtimeMode": DEFAULT_RUNTIME_MODE,
                "interactionMode": DEFAULT_INTERACTION_MODE,
                "branch": null,
                "worktreePath": null,
                "createdAt": thread.created_at,
                "historyImport": true,
            }))?;
            self.engine.dispatch(create).await?;
        }
        if !imported_history_present {
            let messages: Vec<Value> = thread
                .messages
                .iter()
                .enumerate()
                .map(|(index, message)| {
                    json!({
                        "messageId": format!("{thread_id}:{index:06}"),
                        "role": message.role,
                        "text": message.text,
                        "createdAt": message.created_at,
                    })
                })
                .collect();
            let history = command(json!({
                "type": "thread.history.import",
                "commandId": zc_core::ids::uuid_v4(),
                "threadId": thread_id,
                "messages": messages,
            }))?;
            self.engine.dispatch(history).await?;
        }
        self.directory.record_imported_transcript(&thread_id, source).await
    }
}
