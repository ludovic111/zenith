//! Port of `project/AgentSessionImporter.test.ts` (the scripted tests and the integration tests
//! with the real engine, projections and session directory). The provider-reactor tests that
//! resume an imported session belong to the reactors and are not repeated here.

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use common::*;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{
    AgentSessionImportInput, AgentSessionImportSource, AgentSessionScanError, AgentSessionSource, AgentSessionsImportError, OrchestrationCommand,
    OrchestrationProjectShell, OrchestrationThread, ProjectId, ProviderInstanceId, ThreadId,
};
use zc_ports::orchestration::ThreadDetailQuery;
use zc_project::sessions::fs::{RealFileSystem, ScanFile, ScanFileSystem};
use zc_project::sessions::{
    AgentSessionImporter, AgentSessionThread, ImportBinding, ImportEngine, ImportReads, ImportSessionDirectory, RecentThread, RecentThreadsSource,
    ScannerConfig, StaticSettings, ThreadMessage,
};
use zc_project::AgentSessionScanner;

const PROJECT_ID: &str = "project-1";
const WORKSPACE_ROOT: &str = "/tmp/project-from-server";
const CLAUDE_SESSION_ID: &str = "123e4567-e89b-42d3-a456-426614174000";

fn make_thread(source: AgentSessionSource) -> AgentSessionThread {
    AgentSessionThread {
        source,
        provider_instance_id: ProviderInstanceId::new(source.as_str()),
        provider_session_id: if source == AgentSessionSource::Codex {
            "codex-session".into()
        } else {
            CLAUDE_SESSION_ID.into()
        },
        title: format!("Imported {} thread", source.as_str()),
        model: None,
        created_at: "2026-08-24T10:00:00.000Z".into(),
        updated_at: "2026-08-24T10:01:00.000Z".into(),
        messages: vec![
            ThreadMessage {
                role: "user".into(),
                text: "Fix the bug".into(),
                created_at: "2026-08-24T10:00:00.000Z".into(),
            },
            ThreadMessage {
                role: "assistant".into(),
                text: "Fixed".into(),
                created_at: "2026-08-24T10:01:00.000Z".into(),
            },
        ],
    }
}

fn outcome(thread: AgentSessionThread) -> RecentThread {
    let source: AgentSessionImportSource = serde_json::from_value(json!({
        "provider": thread.source,
        "providerInstanceId": thread.provider_instance_id,
        "providerSessionId": thread.provider_session_id,
        "filePath": format!("/tmp/transcripts/{}/{}.jsonl", thread.provider_instance_id, thread.provider_session_id),
        "size": 0, "mtimeMs": 0, "device": 0, "inode": 0, "birthtimeMs": 0,
    }))
    .unwrap();
    RecentThread::Importable { thread, source }
}

fn project(workspace_root: &str) -> OrchestrationProjectShell {
    serde_json::from_value(json!({
        "id": PROJECT_ID, "title": "Project", "workspaceRoot": workspace_root, "defaultModelSelection": null,
        "scripts": [], "createdAt": "2026-08-24T09:00:00.000Z", "updatedAt": "2026-08-24T09:00:00.000Z",
    }))
    .unwrap()
}

/// `makeProjectedThread`.
fn projected_thread(source: AgentSessionSource, project_id: &str, imported: bool, followup: bool) -> OrchestrationThread {
    let thread = make_thread(source);
    let thread_id = format!("import:{}:{}", thread.provider_instance_id, thread.provider_session_id);
    let mut messages = Vec::new();
    if imported {
        messages.push(json!({"id": format!("{thread_id}:000000"), "role": "user", "text": "Fix the bug", "turnId": null, "streaming": false, "createdAt": "2026-08-24T10:00:00.000Z", "updatedAt": "2026-08-24T10:00:00.000Z"}));
        if followup {
            messages.push(json!({"id": "user-followup", "role": "user", "text": "Keep going", "turnId": null, "streaming": false, "createdAt": "2026-08-24T10:02:00.000Z", "updatedAt": "2026-08-24T10:02:00.000Z"}));
        }
    }
    serde_json::from_value(json!({
        "id": thread_id, "projectId": project_id, "title": thread.title,
        "modelSelection": {"instanceId": thread.provider_instance_id, "model": "default"},
        "runtimeMode": "full-access", "interactionMode": "default", "pullRequests": [], "branch": null, "worktreePath": null,
        "latestTurn": null, "createdAt": thread.created_at, "updatedAt": thread.updated_at, "archivedAt": null,
        "settledOverride": null, "settledAt": null, "deletedAt": null, "messages": messages,
        "proposedPlans": [], "activities": [], "checkpoints": [], "session": null,
    }))
    .unwrap_or_else(|e| panic!("projected thread: {e}"))
}

// ---------------------------------------------------------------------------------------------
// Scripted ports

type Outcomes = Arc<dyn Fn(&ScriptedScanner) -> Vec<RecentThread> + Send + Sync>;

struct ScriptedScanner {
    calls: Mutex<Vec<String>>,
    outcomes: Outcomes,
}

#[async_trait]
impl RecentThreadsSource for ScriptedScanner {
    async fn recent_threads(&self, root: &str, _completed: Vec<AgentSessionImportSource>) -> Result<BoxStream<'static, RecentThread>, AgentSessionScanError> {
        self.calls.lock().unwrap().push(root.to_owned());
        Ok(futures::stream::iter((self.outcomes)(self)).boxed())
    }
}

type Dispatch = Arc<dyn Fn(&OrchestrationCommand) -> Result<(), String> + Send + Sync>;

struct ScriptedEngine {
    commands: Mutex<Vec<Value>>,
    dispatch: Dispatch,
}

#[async_trait]
impl ImportEngine for ScriptedEngine {
    async fn dispatch(&self, command: OrchestrationCommand) -> Result<(), String> {
        (self.dispatch)(&command)?;
        self.commands.lock().unwrap().push(serde_json::to_value(&command).unwrap());
        Ok(())
    }
}

type ThreadLookup = Arc<dyn Fn(&ThreadId) -> Option<OrchestrationThread> + Send + Sync>;

struct ScriptedReads {
    project: Option<OrchestrationProjectShell>,
    thread: ThreadLookup,
}

#[async_trait]
impl ImportReads for ScriptedReads {
    async fn get_project_shell_by_id(&self, _: &ProjectId) -> Result<Option<OrchestrationProjectShell>, String> {
        Ok(self.project.clone())
    }
    async fn get_imported_agent_session_sources(&self, _: &ProjectId) -> Result<Vec<AgentSessionImportSource>, String> {
        Ok(Vec::new())
    }
    async fn get_thread_detail_by_id(&self, thread_id: &ThreadId) -> Result<Option<OrchestrationThread>, String> {
        Ok((self.thread)(thread_id))
    }
}

type Insert = Arc<dyn Fn(&ThreadId, &str, &ProviderInstanceId, &Value, &Value) -> Result<(), String> + Send + Sync>;

struct ScriptedDirectory {
    binding: Arc<dyn Fn() -> Option<ImportBinding> + Send + Sync>,
    insert: Insert,
    record: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

#[async_trait]
impl ImportSessionDirectory for ScriptedDirectory {
    async fn get_binding(&self, _: &ThreadId) -> Result<Option<ImportBinding>, String> {
        Ok((self.binding)())
    }
    async fn insert_stopped_binding(
        &self,
        thread_id: &ThreadId,
        provider: &str,
        instance: &ProviderInstanceId,
        cursor: Value,
        payload: Value,
    ) -> Result<(), String> {
        (self.insert)(thread_id, provider, instance, &cursor, &payload)
    }
    async fn record_imported_transcript(&self, _: &ThreadId, _: &AgentSessionImportSource) -> Result<(), String> {
        (self.record)()
    }
}

fn input(expected: Option<&str>) -> AgentSessionImportInput {
    AgentSessionImportInput {
        project_id: ProjectId::new(PROJECT_ID),
        expected_workspace_root: expected.map(str::to_owned),
    }
}

#[tokio::test]
async fn uses_the_project_root_and_stores_provider_specific_resume_cursors() {
    let bindings: Arc<Mutex<Vec<Value>>> = Arc::default();
    let scanner = Arc::new(ScriptedScanner {
        calls: Mutex::default(),
        outcomes: Arc::new(|_| {
            vec![
                outcome(make_thread(AgentSessionSource::Codex)),
                outcome(make_thread(AgentSessionSource::ClaudeAgent)),
            ]
        }),
    });
    let engine = Arc::new(ScriptedEngine {
        commands: Mutex::default(),
        dispatch: Arc::new(|_| Ok(())),
    });
    let recorded = bindings.clone();
    let commands_seen = engine.clone();
    let importer = AgentSessionImporter {
        scanner: scanner.clone(),
        engine: engine.clone(),
        reads: Arc::new(ScriptedReads {
            project: Some(project(WORKSPACE_ROOT)),
            thread: Arc::new(|_| None),
        }),
        directory: Arc::new(ScriptedDirectory {
            binding: Arc::new(|| None),
            insert: Arc::new(move |thread_id, provider, instance, cursor, payload| {
                // The cursor is installed before the thread exists.
                let created = commands_seen
                    .commands
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|c| c["type"] == "thread.create" && c["threadId"] == thread_id.as_str());
                assert!(!created);
                recorded
                    .lock()
                    .unwrap()
                    .push(json!({"provider": provider, "providerInstanceId": instance, "resumeCursor": cursor, "runtimePayload": payload}));
                Ok(())
            }),
            record: Arc::new(|| Ok(())),
        }),
    };
    let result = importer.import(input(Some(&format!("{WORKSPACE_ROOT}/")))).await.unwrap();
    assert_eq!((result.imported_count, result.skipped_count), (2, 0));
    assert_eq!(*scanner.calls.lock().unwrap(), [WORKSPACE_ROOT]);
    let commands = engine.commands.lock().unwrap();
    assert_eq!(
        commands.iter().map(|c| c["type"].as_str().unwrap()).collect::<Vec<_>>(),
        ["thread.create", "thread.history.import", "thread.create", "thread.history.import"]
    );
    assert!(commands.iter().filter(|c| c["type"] == "thread.create").all(|c| c["historyImport"] == true));
    let create = commands.iter().find(|c| c["type"] == "thread.create").unwrap();
    assert_eq!(create["modelSelection"], json!({"instanceId": "codex", "model": "gpt-6-astra"}));
    assert_eq!(create["runtimeMode"], "full-access");
    let ids: Vec<&str> = commands
        .iter()
        .filter(|c| c["type"] == "thread.history.import")
        .flat_map(|c| c["messages"].as_array().unwrap().iter().map(|m| m["messageId"].as_str().unwrap()))
        .collect();
    assert_eq!(
        ids,
        [
            "import:codex:codex-session:000000".to_owned(),
            "import:codex:codex-session:000001".to_owned(),
            format!("import:claudeAgent:{CLAUDE_SESSION_ID}:000000"),
            format!("import:claudeAgent:{CLAUDE_SESSION_ID}:000001"),
        ]
    );
    assert_eq!(
        *bindings.lock().unwrap(),
        [
            json!({"provider": "codex", "providerInstanceId": "codex", "resumeCursor": {"threadId": "codex-session"}, "runtimePayload": {"cwd": WORKSPACE_ROOT}}),
            json!({"provider": "claudeAgent", "providerInstanceId": "claudeAgent", "resumeCursor": {"threadId": format!("import:claudeAgent:{CLAUDE_SESSION_ID}"), "resume": CLAUDE_SESSION_ID}, "runtimePayload": {"cwd": WORKSPACE_ROOT}}),
        ]
    );
}

fn unused_directory() -> Arc<ScriptedDirectory> {
    Arc::new(ScriptedDirectory {
        binding: Arc::new(|| panic!("must not read a binding")),
        insert: Arc::new(|_, _, _, _, _| panic!("must not bind")),
        record: Arc::new(|| panic!("must not record")),
    })
}

fn unused_engine() -> Arc<ScriptedEngine> {
    Arc::new(ScriptedEngine {
        commands: Mutex::default(),
        dispatch: Arc::new(|_| panic!("must not dispatch")),
    })
}

#[tokio::test]
async fn rejects_a_changed_project_root_before_scanning_or_writing() {
    let scanner = Arc::new(ScriptedScanner {
        calls: Mutex::default(),
        outcomes: Arc::new(|_| Vec::new()),
    });
    let importer = AgentSessionImporter {
        scanner: scanner.clone(),
        engine: unused_engine(),
        reads: Arc::new(ScriptedReads {
            project: Some(project("/tmp/project-moved")),
            thread: Arc::new(|_| None),
        }),
        directory: unused_directory(),
    };
    let error = importer.import(input(Some(WORKSPACE_ROOT))).await.unwrap_err();
    assert_eq!(
        serde_json::to_value(&error).unwrap(),
        json!({"_tag": "AgentSessionImportProjectChangedError", "projectId": PROJECT_ID})
    );
    assert!(scanner.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn reports_a_missing_project() {
    let importer = AgentSessionImporter {
        scanner: Arc::new(ScriptedScanner {
            calls: Mutex::default(),
            outcomes: Arc::new(|_| Vec::new()),
        }),
        engine: unused_engine(),
        reads: Arc::new(ScriptedReads {
            project: None,
            thread: Arc::new(|_| None),
        }),
        directory: unused_directory(),
    };
    let error = importer.import(input(None)).await.unwrap_err();
    assert!(matches!(error, AgentSessionsImportError::AgentSessionImportProjectNotFoundError(_)));
}

#[tokio::test]
async fn counts_scanner_skips_without_writing_a_thread_or_binding() {
    let importer = AgentSessionImporter {
        scanner: Arc::new(ScriptedScanner {
            calls: Mutex::default(),
            outcomes: Arc::new(|_| vec![RecentThread::Skipped]),
        }),
        engine: unused_engine(),
        reads: Arc::new(ScriptedReads {
            project: Some(project(WORKSPACE_ROOT)),
            thread: Arc::new(|_| None),
        }),
        directory: unused_directory(),
    };
    let result = importer.import(input(None)).await.unwrap();
    assert_eq!((result.imported_count, result.skipped_count), (0, 1));
}

#[tokio::test]
async fn recovers_after_a_rejected_history_receipt_and_a_failed_binding_write() {
    let thread_created = Arc::new(AtomicBool::new(false));
    let history_imported = Arc::new(AtomicBool::new(false));
    let history_attempts = Arc::new(AtomicUsize::new(0));
    let binding_attempts = Arc::new(AtomicUsize::new(0));
    let rejected: Arc<Mutex<HashSet<String>>> = Arc::default();
    let bindings: Arc<Mutex<Vec<ImportBinding>>> = Arc::default();
    let (created, imported, attempts, rejected_ids) = (thread_created.clone(), history_imported.clone(), history_attempts.clone(), rejected.clone());
    let engine = Arc::new(ScriptedEngine {
        commands: Mutex::default(),
        dispatch: Arc::new(move |command| {
            let value = serde_json::to_value(command).unwrap();
            let command_id = value["commandId"].as_str().unwrap().to_owned();
            if rejected_ids.lock().unwrap().contains(&command_id) {
                return Err("Previously rejected.".into());
            }
            match value["type"].as_str() {
                Some("thread.create") => created.store(true, Ordering::SeqCst),
                Some("thread.history.import") => {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        rejected_ids.lock().unwrap().insert(command_id);
                        return Err("Temporary history import failure.".into());
                    }
                    imported.store(true, Ordering::SeqCst);
                }
                _ => {}
            }
            Ok(())
        }),
    });
    let (b, attempts_b) = (bindings.clone(), binding_attempts.clone());
    let b2 = bindings.clone();
    let directory = Arc::new(ScriptedDirectory {
        binding: Arc::new(move || b2.lock().unwrap().first().cloned()),
        insert: Arc::new(move |_, provider, instance, _, _| {
            if attempts_b.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err("Temporary session storage failure.".into());
            }
            b.lock().unwrap().push(ImportBinding {
                provider: provider.into(),
                provider_instance_id: Some(instance.to_string()),
                status: Some("stopped".into()),
            });
            Ok(())
        }),
        record: Arc::new(|| Ok(())),
    });
    let (created, imported) = (thread_created.clone(), history_imported.clone());
    let importer = AgentSessionImporter {
        scanner: Arc::new(ScriptedScanner {
            calls: Mutex::default(),
            outcomes: Arc::new(|_| vec![outcome(make_thread(AgentSessionSource::Codex))]),
        }),
        engine,
        reads: Arc::new(ScriptedReads {
            project: Some(project(WORKSPACE_ROOT)),
            thread: Arc::new(move |_| {
                created
                    .load(Ordering::SeqCst)
                    .then(|| projected_thread(AgentSessionSource::Codex, PROJECT_ID, imported.load(Ordering::SeqCst), false))
            }),
        }),
        directory,
    };
    let counts = |r: zc_contracts::AgentSessionImportResult| (r.imported_count, r.skipped_count);
    assert_eq!(counts(importer.import(input(None)).await.unwrap()), (0, 1));
    assert_eq!(counts(importer.import(input(None)).await.unwrap()), (0, 1));
    assert_eq!(counts(importer.import(input(None)).await.unwrap()), (1, 0));
    let after = history_attempts.load(Ordering::SeqCst);
    assert_eq!(counts(importer.import(input(None)).await.unwrap()), (1, 0));
    assert_eq!(history_attempts.load(Ordering::SeqCst), after);
    assert_eq!(after, 2);
    assert_eq!(bindings.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn does_not_replace_completed_history_or_an_active_binding_on_retry() {
    let importer = AgentSessionImporter {
        scanner: Arc::new(ScriptedScanner {
            calls: Mutex::default(),
            outcomes: Arc::new(|_| vec![outcome(make_thread(AgentSessionSource::Codex))]),
        }),
        engine: unused_engine(),
        reads: Arc::new(ScriptedReads {
            project: Some(project(WORKSPACE_ROOT)),
            thread: Arc::new(|_| Some(projected_thread(AgentSessionSource::Codex, PROJECT_ID, true, true))),
        }),
        directory: Arc::new(ScriptedDirectory {
            binding: Arc::new(|| {
                Some(ImportBinding {
                    provider: "codex".into(),
                    provider_instance_id: Some("codex".into()),
                    status: Some("running".into()),
                })
            }),
            insert: Arc::new(|_, _, _, _, _| panic!("must not replace an active binding")),
            record: Arc::new(|| Ok(())),
        }),
    };
    let result = importer.import(input(None)).await.unwrap();
    assert_eq!((result.imported_count, result.skipped_count), (1, 0));
}

#[tokio::test]
async fn skips_malformed_claude_ids_and_wrong_project_thread_collisions() {
    let engine = Arc::new(ScriptedEngine {
        commands: Mutex::default(),
        dispatch: Arc::new(|_| Ok(())),
    });
    let importer = AgentSessionImporter {
        scanner: Arc::new(ScriptedScanner {
            calls: Mutex::default(),
            outcomes: Arc::new(|_| {
                let mut malformed = make_thread(AgentSessionSource::ClaudeAgent);
                malformed.provider_session_id = "not-a-uuid".into();
                vec![outcome(malformed), outcome(make_thread(AgentSessionSource::Codex))]
            }),
        }),
        engine: engine.clone(),
        reads: Arc::new(ScriptedReads {
            project: Some(project(WORKSPACE_ROOT)),
            thread: Arc::new(|thread_id| {
                (thread_id.as_str() == "import:codex:codex-session").then(|| projected_thread(AgentSessionSource::Codex, "project-other", false, false))
            }),
        }),
        directory: Arc::new(ScriptedDirectory {
            binding: Arc::new(|| None),
            insert: Arc::new(|_, _, _, _, _| panic!("must not bind malformed or wrong-project sessions")),
            record: Arc::new(|| panic!("unused")),
        }),
    };
    let result = importer.import(input(None)).await.unwrap();
    assert_eq!((result.imported_count, result.skipped_count), (0, 2));
    assert!(engine.commands.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Integration: the real engine, projections and session directory

fn integration_thread(session: &str, title: &str) -> AgentSessionThread {
    AgentSessionThread {
        provider_session_id: session.into(),
        title: title.into(),
        updated_at: "2026-08-24T10:00:00.000Z".into(),
        messages: (0..12)
            .map(|index| ThreadMessage {
                role: if index % 2 == 0 { "user".into() } else { "assistant".into() },
                text: format!("Message {index}"),
                created_at: "2026-08-24T10:00:00.000Z".into(),
            })
            .collect(),
        ..make_thread(AgentSessionSource::Codex)
    }
}

fn integration_importer(stack: &Stack, scanner: Arc<dyn RecentThreadsSource>, directory: Arc<dyn ImportSessionDirectory>) -> AgentSessionImporter {
    AgentSessionImporter {
        scanner,
        engine: Arc::new(zc_project::sessions::EngineImport(Arc::new(stack.engine.clone()))),
        reads: Arc::new(zc_project::sessions::ProjectionImportReads(stack.reads.clone())),
        directory,
    }
}

async fn thread_texts(stack: &Stack, thread_id: &str) -> Option<Vec<String>> {
    let thread = stack
        .reads
        .get_thread_detail_by_id(&ThreadId::new(thread_id), ThreadDetailQuery::default())
        .await
        .unwrap()?;
    Some(thread.messages.iter().map(|m| m.text.to_string()).collect())
}

async fn binding(stack: &Stack, thread_id: &str) -> Option<zc_providers::ProviderRuntimeBinding> {
    stack.directory.get_binding(&ThreadId::new(thread_id)).await.unwrap()
}

fn scripted(thread: AgentSessionThread) -> Arc<ScriptedScanner> {
    Arc::new(ScriptedScanner {
        calls: Mutex::default(),
        outcomes: Arc::new(move |_| vec![outcome(thread.clone())]),
    })
}

#[tokio::test]
async fn imports_once_after_the_real_engine_persists_an_old_rejected_receipt() {
    let stack = stack().await;
    let thread_id = "import:codex:codex-session";
    stack.create_project(PROJECT_ID, WORKSPACE_ROOT).await;
    let rejected = stack
        .engine
        .dispatch(
            command(json!({
                "type": "thread.history.import",
                "commandId": format!("agent-session:history:{thread_id}"),
                "threadId": thread_id,
                "messages": [{"messageId": format!("{thread_id}:000000"), "role": "user", "text": "Fix the bug", "createdAt": "2026-08-24T10:00:00.000Z"}],
            })),
            None,
        )
        .await;
    assert!(rejected.is_err());
    let thread = integration_thread("codex-session", "Imported codex thread");
    let importer = integration_importer(&stack, scripted(thread.clone()), Arc::new(stack.directory.clone()));
    let result = importer.import(input(None)).await.unwrap();
    assert_eq!((result.imported_count, result.skipped_count), (1, 0));
    let expected: Vec<String> = thread.messages.iter().map(|m| m.text.clone()).collect();
    assert_eq!(thread_texts(&stack, thread_id).await.unwrap(), expected);
    let detail = stack
        .reads
        .get_thread_detail_by_id(&ThreadId::new(thread_id), ThreadDetailQuery::default())
        .await
        .unwrap()
        .unwrap();
    let encoded = serde_json::to_value(&detail).unwrap();
    assert_eq!(encoded["settledOverride"], "settled");
    assert_eq!(encoded["updatedAt"], "2026-08-24T10:00:00.000Z");
    let bound = binding(&stack, thread_id).await.unwrap();
    assert_eq!(bound.provider.as_str(), "codex");
    assert_eq!(bound.provider_instance_id.as_ref().map(|id| id.as_str()), Some("codex"));
    assert_eq!(bound.resume_cursor, Some(json!({"threadId": "codex-session"})));
    assert_eq!(bound.runtime_payload.as_ref().and_then(|p| p.get("cwd")).cloned(), Some(json!(WORKSPACE_ROOT)));

    stack
        .dispatch(json!({"type": "thread.revert.complete", "commandId": "revert-imported-thread-to-baseline", "threadId": thread_id, "turnCount": 0, "createdAt": "2026-08-24T10:05:00.000Z"}))
        .await;
    assert_eq!(thread_texts(&stack, thread_id).await.unwrap(), expected);
}

/// A session directory whose binding insert waits for a release (the TS gated repository).
struct GatedDirectory {
    inner: zc_providers::ProviderSessionDirectory,
    at_insert: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl ImportSessionDirectory for GatedDirectory {
    async fn get_binding(&self, thread_id: &ThreadId) -> Result<Option<ImportBinding>, String> {
        ImportSessionDirectory::get_binding(&self.inner, thread_id).await
    }
    async fn insert_stopped_binding(
        &self,
        thread_id: &ThreadId,
        provider: &str,
        instance: &ProviderInstanceId,
        cursor: Value,
        payload: Value,
    ) -> Result<(), String> {
        self.at_insert.notify_one();
        self.release.notified().await;
        self.inner.insert_stopped_binding(thread_id, provider, instance, cursor, payload).await
    }
    async fn record_imported_transcript(&self, thread_id: &ThreadId, source: &AgentSessionImportSource) -> Result<(), String> {
        ImportSessionDirectory::record_imported_transcript(&self.inner, thread_id, source).await
    }
}

async fn upsert_running(stack: &Stack, thread_id: &str, workspace_root: &str) {
    use zc_providers::directory::{ProviderRuntimeBinding, RuntimeStatus};
    let mut running = ProviderRuntimeBinding::new(
        ThreadId::new(thread_id),
        zc_contracts::ProviderDriverKind::new("codex"),
        ProviderInstanceId::new("codex"),
    );
    running.status = Some(RuntimeStatus::Running);
    running.resume_cursor = Some(json!({"threadId": "active-client-session"}));
    running.runtime_payload = Some(json!({"cwd": workspace_root, "activeTurnId": "turn-active"}));
    stack
        .directory
        .upsert(running, zc_db::repos::provider_session_runtime::OnConflict::Update)
        .await
        .unwrap();
}

fn assert_running(bound: &zc_providers::ProviderRuntimeBinding, workspace_root: &str) {
    assert_eq!(bound.status, Some(zc_providers::directory::RuntimeStatus::Running));
    assert_eq!(bound.resume_cursor, Some(json!({"threadId": "active-client-session"})));
    assert_eq!(bound.runtime_payload.as_ref().and_then(|p| p.get("cwd")).cloned(), Some(json!(workspace_root)));
    assert_eq!(
        bound.runtime_payload.as_ref().and_then(|p| p.get("activeTurnId")).cloned(),
        Some(json!("turn-active"))
    );
}

#[tokio::test]
async fn persists_the_resume_cursor_before_publishing_a_new_imported_thread() {
    let stack = stack().await;
    let project_id = "project-import-binding-race";
    let workspace_root = "/tmp/project-import-binding-race";
    let thread_id = "import:codex:codex-binding-race";
    stack.create_project(project_id, workspace_root).await;
    let directory = Arc::new(GatedDirectory {
        inner: stack.directory.clone(),
        at_insert: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let thread = integration_thread("codex-binding-race", "Binding race");
    let importer = integration_importer(&stack, scripted(thread.clone()), directory.clone());
    let task = tokio::spawn(async move {
        importer
            .import(AgentSessionImportInput {
                project_id: ProjectId::new(project_id),
                expected_workspace_root: None,
            })
            .await
    });
    directory.at_insert.notified().await;
    assert!(thread_texts(&stack, thread_id).await.is_none());
    upsert_running(&stack, thread_id, workspace_root).await;
    directory.release.notify_one();
    let result = task.await.unwrap().unwrap();
    assert_eq!((result.imported_count, result.skipped_count), (1, 0));
    assert_eq!(
        thread_texts(&stack, thread_id).await.unwrap(),
        thread.messages.iter().map(|m| m.text.clone()).collect::<Vec<_>>()
    );
    assert_running(&binding(&stack, thread_id).await.unwrap(), workspace_root);
}

#[tokio::test]
async fn does_not_import_history_over_a_turn_started_on_a_partial_thread() {
    let stack = stack().await;
    let project_id = "project-import-turn-race";
    let workspace_root = "/tmp/project-import-turn-race";
    let thread_id = "import:codex:codex-turn-race";
    stack.create_project(project_id, workspace_root).await;
    stack
        .dispatch(json!({
            "type": "thread.create", "commandId": "create-import-turn-race-thread", "threadId": thread_id, "projectId": project_id,
            "title": "Turn race", "modelSelection": {"instanceId": "codex", "model": "default"}, "runtimeMode": "full-access",
            "interactionMode": "default", "branch": null, "worktreePath": null, "createdAt": "2026-08-24T10:00:00.000Z",
        }))
        .await;
    let directory = Arc::new(GatedDirectory {
        inner: stack.directory.clone(),
        at_insert: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let importer = integration_importer(&stack, scripted(integration_thread("codex-turn-race", "Turn race")), directory.clone());
    let task = tokio::spawn(async move {
        importer
            .import(AgentSessionImportInput {
                project_id: ProjectId::new(project_id),
                expected_workspace_root: None,
            })
            .await
    });
    directory.at_insert.notified().await;
    upsert_running(&stack, thread_id, workspace_root).await;
    stack
        .dispatch(json!({
            "type": "thread.turn.start", "commandId": "start-turn-during-import", "threadId": thread_id,
            "message": {"messageId": "message-during-import", "role": "user", "text": "Continue while import waits", "attachments": []},
            "runtimeMode": "full-access", "interactionMode": "default", "createdAt": "2026-08-24T10:02:00.000Z",
        }))
        .await;
    directory.release.notify_one();
    let result = task.await.unwrap().unwrap();
    assert_eq!((result.imported_count, result.skipped_count), (0, 1));
    assert_running(&binding(&stack, thread_id).await.unwrap(), workspace_root);
    assert_eq!(thread_texts(&stack, thread_id).await.unwrap(), ["Continue while import waits"]);
}

/// The real file system, counting opens of the tracked transcripts and refusing to reopen the
/// completed ones (`fullReads` are second opens: the first is project discovery).
#[derive(Clone)]
struct CountingFs {
    tracked: Arc<HashSet<PathBuf>>,
    completed: Arc<HashSet<PathBuf>>,
    opens: Arc<Mutex<std::collections::HashMap<PathBuf, usize>>>,
    full_reads: Arc<Mutex<Vec<PathBuf>>>,
}

impl ScanFileSystem for CountingFs {
    fn read_directory(&self, directory: &Path) -> std::io::Result<Vec<String>> {
        RealFileSystem.read_directory(directory)
    }
    fn stat(&self, path: &Path) -> std::io::Result<zc_project::sessions::fs::FileStat> {
        RealFileSystem.stat(path)
    }
    fn real_path(&self, path: &Path) -> std::io::Result<PathBuf> {
        RealFileSystem.real_path(path)
    }
    fn read_file(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        RealFileSystem.read_file(path)
    }
    fn open(&self, path: &Path) -> std::io::Result<Box<dyn ScanFile>> {
        if self.tracked.contains(path) {
            let count = {
                let mut opens = self.opens.lock().unwrap();
                let count = opens.entry(path.to_path_buf()).or_default();
                *count += 1;
                *count
            };
            if count > 1 {
                self.full_reads.lock().unwrap().push(path.to_path_buf());
                assert!(!self.completed.contains(path), "completed transcript reopened");
            }
        }
        RealFileSystem.open(path)
    }
}

/// Fails the first history import of one thread.
struct FailingOnce {
    inner: Arc<dyn zc_ports::OrchestrationDispatch>,
    thread_id: String,
    fail: AtomicBool,
}

#[async_trait]
impl ImportEngine for FailingOnce {
    async fn dispatch(&self, command: OrchestrationCommand) -> Result<(), String> {
        let value = serde_json::to_value(&command).unwrap();
        if value["type"] == "thread.history.import" && value["threadId"] == self.thread_id.as_str() && self.fail.swap(false, Ordering::SeqCst) {
            return Err("Injected history import failure.".into());
        }
        self.inner.dispatch(command, None).await.map(|_| ()).map_err(|e| format!("{e:?}"))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn retries_a_bounded_import_after_scanner_restart_without_rereading_completed_transcripts() {
    let stack = stack().await;
    let now_ms: i64 = 1_787_572_800_000;
    let (_fixture, fixture) = temp_dir("import-retry-");
    let workspace = fixture.join("workspace");
    let claude = fixture.join("claude");
    let codex = fixture.join("codex");
    let sessions = codex.join("sessions/2026/08/24");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::create_dir_all(&sessions).unwrap();
    let project_id = "project-bounded-import-retry";
    let transcripts: Vec<(String, String, PathBuf)> = (0..101)
        .map(|index| {
            let session = format!("bounded-session-{index:03}");
            (
                session.clone(),
                format!("import:codex:{session}"),
                sessions.join(format!("rollout-{session}.jsonl")),
            )
        })
        .collect();
    for (index, (session, _, path)) in transcripts.iter().enumerate() {
        let contents = [
            json!({"type": "session_meta", "payload": {"id": session, "cwd": workspace}}).to_string(),
            json!({"type": "event_msg", "payload": {"type": "user_message", "message": format!("Prompt {session}")}}).to_string(),
        ]
        .join("\n");
        std::fs::write(path, contents).unwrap();
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_millis((now_ms - index as i64 * 1000) as u64))
            .unwrap();
    }
    let (legacy, failed, remaining) = (&transcripts[0], &transcripts[1], &transcripts[100]);
    stack.create_project(project_id, &workspace.to_string_lossy()).await;
    // A completed import from before transcript sources were recorded.
    {
        use zc_providers::directory::{ProviderRuntimeBinding, RuntimeStatus};
        let mut binding = ProviderRuntimeBinding::new(
            ThreadId::new(&legacy.1),
            zc_contracts::ProviderDriverKind::new("codex"),
            ProviderInstanceId::new("codex"),
        );
        binding.status = Some(RuntimeStatus::Stopped);
        binding.resume_cursor = Some(json!({"threadId": "legacy-current-session"}));
        binding.runtime_payload = Some(json!({"cwd": workspace}));
        stack
            .directory
            .upsert(binding, zc_db::repos::provider_session_runtime::OnConflict::Update)
            .await
            .unwrap();
    }
    stack
        .dispatch(json!({
            "type": "thread.create", "commandId": "create-legacy-bounded-import", "threadId": legacy.1, "projectId": project_id,
            "title": "Legacy import", "modelSelection": {"instanceId": "codex", "model": "default"}, "runtimeMode": "full-access",
            "interactionMode": "default", "branch": null, "worktreePath": null, "createdAt": "2026-08-24T10:00:00.000Z", "historyImport": true,
        }))
        .await;
    stack
        .dispatch(json!({
            "type": "thread.history.import", "commandId": "import-legacy-bounded-history", "threadId": legacy.1,
            "messages": [{"messageId": format!("{}:000000", legacy.1), "role": "user", "text": "Legacy imported history", "createdAt": "2026-08-24T10:00:00.000Z"}],
        }))
        .await;
    let sources = |stack: &Stack| {
        let reads = stack.reads.clone();
        async move { reads.get_imported_agent_session_sources(&ProjectId::new(project_id)).await.unwrap() }
    };
    assert!(sources(&stack).await.is_empty());

    let engine = Arc::new(FailingOnce {
        inner: Arc::new(stack.engine.clone()),
        thread_id: failed.1.clone(),
        fail: AtomicBool::new(true),
    });
    let settings = json!({"providers": {"claudeAgent": {"homePath": claude}, "codex": {"homePath": codex}}, "providerInstances": {}});
    let tracked: Arc<HashSet<PathBuf>> = Arc::new(transcripts.iter().map(|t| t.2.clone()).collect());
    let (_home, home) = temp_dir("import-retry-home-");
    let attempt = |completed: HashSet<PathBuf>| {
        let fs = CountingFs {
            tracked: tracked.clone(),
            completed: Arc::new(completed),
            opens: Arc::default(),
            full_reads: Arc::default(),
        };
        let mut config = ScannerConfig::new(fixture.join("base"), fixture.join("base/worktrees"));
        config.home_dir = home.clone();
        config.environment = Default::default();
        config.now_millis = Arc::new(move || now_ms);
        config.fs = Arc::new(fs.clone());
        // A fresh scanner per attempt (a server restart).
        let scanner = AgentSessionScanner::new(config, Arc::new(StaticSettings(settings.clone())), stack.reads.clone());
        let importer = AgentSessionImporter {
            scanner: Arc::new(scanner),
            engine: engine.clone(),
            reads: Arc::new(zc_project::sessions::ProjectionImportReads(stack.reads.clone())),
            directory: Arc::new(stack.directory.clone()),
        };
        async move {
            let result = importer
                .import(AgentSessionImportInput {
                    project_id: ProjectId::new(project_id),
                    expected_workspace_root: None,
                })
                .await
                .unwrap();
            (result, fs)
        }
    };

    let (first, fs) = attempt(HashSet::new()).await;
    assert_eq!((first.imported_count, first.skipped_count), (99, 2));
    assert!(!engine.fail.load(Ordering::SeqCst));
    assert_eq!(
        *fs.full_reads.lock().unwrap(),
        transcripts[..100].iter().map(|t| t.2.clone()).collect::<Vec<_>>()
    );
    assert_eq!(fs.opens.lock().unwrap().get(&remaining.2), Some(&1));
    let completed = sources(&stack).await;
    assert_eq!(completed.len(), 99);
    assert!(completed
        .iter()
        .any(|entry| entry.thread_id.as_str() == legacy.1 && entry.source.file_path == legacy.2.to_string_lossy()));
    assert_eq!(thread_texts(&stack, &failed.1).await.unwrap(), Vec::<String>::new());
    let failed_binding = binding(&stack, &failed.1).await.unwrap();
    assert_eq!(failed_binding.status, Some(zc_providers::directory::RuntimeStatus::Stopped));
    assert_eq!(failed_binding.resume_cursor, Some(json!({"threadId": failed.0})));
    assert!(thread_texts(&stack, &remaining.1).await.is_none());

    let completed_paths: HashSet<PathBuf> = completed.iter().map(|entry| PathBuf::from(&entry.source.file_path)).collect();
    let (second, fs) = attempt(completed_paths.clone()).await;
    assert_eq!((second.imported_count, second.skipped_count), (101, 0));
    assert_eq!(*fs.full_reads.lock().unwrap(), [failed.2.clone(), remaining.2.clone()]);
    for transcript in &transcripts {
        let expected = if completed_paths.contains(&transcript.2) { 1 } else { 2 };
        assert_eq!(fs.opens.lock().unwrap().get(&transcript.2), Some(&expected));
    }
    assert_eq!(sources(&stack).await.len(), 101);
    assert_eq!(thread_texts(&stack, &legacy.1).await.unwrap(), ["Legacy imported history"]);
    assert_eq!(
        binding(&stack, &legacy.1).await.unwrap().resume_cursor,
        Some(json!({"threadId": "legacy-current-session"}))
    );
    for transcript in [failed, remaining] {
        assert_eq!(thread_texts(&stack, &transcript.1).await.unwrap(), [format!("Prompt {}", transcript.0)]);
    }
}
