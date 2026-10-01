//! The startup work of `serverRuntimeStartup.ts` that is not plumbing: the reconcilers run
//! before the command gate opens, the automatic pull of clean projects, and the optional
//! project bootstrap from the cwd. The sequence itself is [`super::App::startup`].

use std::collections::HashSet;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::{json, Map, Value};
use zc_contracts::{OrchestrationCommand, ProjectId, ThreadId, WorktreeSetupSnapshot};
use zc_orchestration::OrchestrationEngine;
use zc_ports::ProjectionReads;
use zc_providers::directory::{ProviderRuntimeBinding, RuntimeStatus};
use zc_providers::{ProviderServiceImpl, ProviderSessionDirectory};
use zc_settings::ServerSettingsService;
use zc_vcs::GitVcsDriver;

const ORPHANED_PROVIDER_SESSION_ERROR: &str = "Provider session did not survive a server restart. Send a new message to continue.";
const CONTINUATION_FAILED_ERROR: &str = "Could not continue this thread after the server restart. Send a new message to continue.";
const SERVER_UPDATE_CONTINUATION_KEY: &str = "continueAfterServerUpdate";
const CONTINUATION_PREPARED_KEY: &str = "continueAfterServerUpdatePrepared";
const SERVER_UPDATE_CONTINUATION_PROMPT: &str = "Continue where you left off.";
/// `WORKTREE_SETUP_ACTIVITY_KIND`.
pub const WORKTREE_SETUP_ACTIVITY_KIND: &str = "worktree-setup";

/// Dispatches a server-side command given as wire JSON (with a fresh command id).
async fn dispatch_json(engine: &OrchestrationEngine, mut command: Value) -> anyhow::Result<()> {
    command["commandId"] = json!(zc_core::uuid_v4());
    let command: OrchestrationCommand = serde_json::from_value(command)?;
    engine.dispatch(command, None).await?;
    Ok(())
}

/// Retries a dispatch once (`Effect.retry({times: 1})`).
async fn dispatch_with_retry(engine: &OrchestrationEngine, command: Value) -> anyhow::Result<()> {
    match dispatch_json(engine, command.clone()).await {
        Ok(()) => Ok(()),
        Err(_) => dispatch_json(engine, command).await,
    }
}

fn payload_object(binding: &ProviderRuntimeBinding) -> Map<String, Value> {
    match &binding.runtime_payload {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

fn continuation_marker_present(binding: &ProviderRuntimeBinding) -> bool {
    matches!(&binding.runtime_payload, Some(Value::Object(map)) if map.contains_key(SERVER_UPDATE_CONTINUATION_KEY))
}

/// `readServerUpdateContinuationTurnId`.
fn continuation_turn_id(binding: &ProviderRuntimeBinding) -> Option<String> {
    match &binding.runtime_payload {
        Some(Value::Object(map)) => map
            .get(SERVER_UPDATE_CONTINUATION_KEY)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned),
        _ => None,
    }
}

fn payload_active_turn_id(binding: &ProviderRuntimeBinding) -> Option<Value> {
    payload_object(binding).get("activeTurnId").cloned()
}

fn is_nullish(value: Option<&Value>) -> bool {
    matches!(value, None | Some(Value::Null))
}

/// `reconcileProviderSessions`: sessions the projection says are starting or running but no
/// live adapter knows about did not survive the restart. They are settled as errors, or, when
/// the thread was marked for continuation (server update) or the project opted in to
/// `continueThreadsAfterServerUpdate`, resumed with a continuation turn. Failures only log.
pub async fn reconcile_provider_sessions(
    engine: &OrchestrationEngine,
    reads: &Arc<dyn ProjectionReads>,
    directory: &ProviderSessionDirectory,
    providers: &ProviderServiceImpl,
    settings: &ServerSettingsService,
) {
    let restart_settings = match settings.get_settings_value().await {
        Ok(settings) => Some(settings),
        Err(error) => {
            tracing::warn!(?error, "could not read restart continuation preference");
            None
        }
    };
    let continue_after_restart_for = |project_id: &ProjectId| {
        restart_settings
            .as_ref()
            .is_some_and(|settings| zc_providers::settings::project_scoped_bool(settings, Some(project_id.as_str()), "continueThreadsAfterServerUpdate", false))
    };
    let live: HashSet<String> = providers
        .list_sessions()
        .await
        .into_iter()
        .map(|session| session.thread_id.to_string())
        .collect();
    let model = match reads.get_command_read_model().await {
        Ok(model) => model,
        Err(error) => {
            tracing::warn!(%error, "provider session startup reconciliation failed");
            return;
        }
    };
    let prepared: HashSet<String> = match directory.list_bindings(false).await {
        Ok(bindings) => bindings
            .into_iter()
            .map(|entry| entry.binding)
            .filter(|binding| {
                let payload = payload_object(binding);
                continuation_turn_id(binding).is_some()
                    && payload.get("activeTurnId") == Some(&Value::Null)
                    && payload.get(CONTINUATION_PREPARED_KEY) == Some(&json!(true))
            })
            .map(|binding| binding.thread_id.to_string())
            .collect(),
        Err(error) => {
            tracing::warn!(?error, "failed to read prepared provider continuations");
            HashSet::new()
        }
    };
    let orphaned = model.threads.iter().filter(|thread| {
        let Some(session) = &thread.session else { return false };
        let status = session.status.as_str();
        (status == "starting" || status == "running" || session.active_turn_id.is_some() || (status == "ready" && prepared.contains(thread.id.as_str())))
            && !live.contains(thread.id.as_str())
    });

    for thread in orphaned {
        let Some(session) = &thread.session else { continue };
        let status = session.status.as_str();
        let binding = match directory.get_binding(&thread.id).await {
            Ok(binding) => binding,
            Err(error) => {
                tracing::warn!(thread_id = %thread.id, ?error, "failed to read orphaned provider session directory binding");
                None
            }
        };
        let marker_present = binding.as_ref().is_some_and(continuation_marker_present);
        let continuation_turn = binding.as_ref().and_then(continuation_turn_id);
        let active_turn = session.active_turn_id.as_ref().map(|id| id.to_string());
        let continuation_marked = match (&continuation_turn, &binding) {
            (Some(turn), Some(binding)) => {
                (active_turn.is_none() || active_turn.as_deref() == Some(turn.as_str()))
                    && (active_turn.is_some()
                        || is_nullish(payload_active_turn_id(binding).as_ref())
                        || payload_active_turn_id(binding).as_ref().and_then(Value::as_str) == Some(turn.as_str()))
            }
            _ => false,
        };
        let prepared_while_ready = status == "ready"
            && active_turn.is_none()
            && continuation_marked
            && binding.as_ref().is_some_and(|binding| {
                let payload = payload_object(binding);
                payload.get("activeTurnId") == Some(&Value::Null) && payload.get(CONTINUATION_PREPARED_KEY) == Some(&json!(true))
            });
        let interrupted_by_restart = continue_after_restart_for(&thread.project_id)
            && status == "running"
            && active_turn.is_some()
            && binding
                .as_ref()
                .is_some_and(|binding| binding.status == Some(RuntimeStatus::Running) && binding.has_resume_cursor());
        let session_json = serde_json::to_value(session).unwrap_or_default();

        let settle_as_error = |last_error: &'static str| {
            let binding = binding.clone();
            let session_json = session_json.clone();
            async move {
                if let Some(binding) = binding {
                    let mut payload = payload_object(&binding);
                    payload.insert("activeTurnId".into(), Value::Null);
                    if marker_present || interrupted_by_restart {
                        payload.insert(SERVER_UPDATE_CONTINUATION_KEY.into(), Value::Null);
                        payload.insert(CONTINUATION_PREPARED_KEY.into(), Value::Null);
                    }
                    let next = ProviderRuntimeBinding {
                        status: Some(RuntimeStatus::Stopped),
                        runtime_payload: Some(Value::Object(payload)),
                        ..binding
                    };
                    if let Err(error) = directory.upsert(next, Default::default()).await {
                        tracing::warn!(thread_id = %thread.id, ?error, "failed to reconcile orphaned provider session directory binding");
                    }
                }
                let at = zc_core::now_iso();
                let mut session = session_json;
                session["status"] = json!("error");
                session["activeTurnId"] = Value::Null;
                session["lastError"] = json!(last_error);
                session["updatedAt"] = json!(at);
                let command = json!({ "type": "thread.session.set", "threadId": thread.id, "session": session, "createdAt": at });
                if let Err(error) = dispatch_with_retry(engine, command).await {
                    tracing::warn!(thread_id = %thread.id, %error, "failed to settle orphaned provider session projection");
                }
            }
        };

        let resumable = binding.as_ref().is_some_and(|binding| binding.has_resume_cursor())
            && (continuation_marked || interrupted_by_restart)
            && (status == "running" || status == "starting" || prepared_while_ready)
            && thread.archived_at.is_none()
            && thread.deleted_at.is_none();
        let Some(binding) = binding.clone().filter(|_| resumable) else {
            settle_as_error(ORPHANED_PROVIDER_SESSION_ERROR).await;
            continue;
        };

        // Keep recovery durable in case this process also exits before sending (one retry).
        let turn = active_turn.clone().or(continuation_turn.clone());
        let mut prepared = prepare_continuation(engine, directory, &binding, &session_json, &thread.id, turn.clone()).await;
        if prepared.is_err() {
            prepared = prepare_continuation(engine, directory, &binding, &session_json, &thread.id, turn).await;
        }
        if let Err(error) = prepared {
            tracing::warn!(thread_id = %thread.id, %error, "failed to prepare provider session continuation");
            settle_as_error(ORPHANED_PROVIDER_SESSION_ERROR).await;
            continue;
        }

        // Continue in the background, like `forkParked`.
        let providers = providers.clone();
        let directory = directory.clone();
        let engine = engine.clone();
        let thread_id = thread.id.clone();
        let interaction_mode = thread.interaction_mode;
        let instance_id = binding.provider_instance_id.clone();
        tokio::spawn(async move {
            let outcome = async {
                let instance_id = instance_id.ok_or_else(|| anyhow::anyhow!("Could not continue thread '{thread_id}': the provider instance is missing."))?;
                let capabilities = providers.get_capabilities(&instance_id).map_err(|e| anyhow::anyhow!("{e:?}"))?;
                let mut input = json!({ "threadId": thread_id, "interactionMode": interaction_mode });
                if capabilities.promptless_turn_continuation {
                    input["continuation"] = json!(true);
                } else {
                    input["input"] = json!(SERVER_UPDATE_CONTINUATION_PROMPT);
                }
                providers
                    .send_turn(serde_json::from_value(input)?)
                    .await
                    .map_err(|e| anyhow::anyhow!("{e:?}"))?;
                anyhow::Ok(())
            }
            .await;
            match outcome {
                Ok(()) => {
                    if let Ok(Some(binding)) = directory.get_binding(&thread_id).await {
                        let mut payload = payload_object(&binding);
                        payload.insert(SERVER_UPDATE_CONTINUATION_KEY.into(), Value::Null);
                        payload.insert(CONTINUATION_PREPARED_KEY.into(), Value::Null);
                        let next = ProviderRuntimeBinding {
                            runtime_payload: Some(Value::Object(payload)),
                            ..binding
                        };
                        if let Err(error) = directory.upsert(next, Default::default()).await {
                            tracing::warn!(thread_id = %thread_id, ?error, "failed to clear completed provider session continuation");
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(thread_id = %thread_id, %error, "failed to continue provider session after server restart");
                    settle_continuation_failure(&engine, &directory, &thread_id).await;
                }
            }
        });
    }
}

/// Marks a binding as resuming (`starting`, marker kept, prepared) and the session as starting.
async fn prepare_continuation(
    engine: &OrchestrationEngine,
    directory: &ProviderSessionDirectory,
    binding: &ProviderRuntimeBinding,
    session_json: &Value,
    thread_id: &ThreadId,
    turn: Option<String>,
) -> anyhow::Result<()> {
    let mut payload = payload_object(binding);
    payload.insert(SERVER_UPDATE_CONTINUATION_KEY.into(), json!(turn));
    payload.insert(CONTINUATION_PREPARED_KEY.into(), json!(true));
    payload.insert("activeTurnId".into(), Value::Null);
    let next = ProviderRuntimeBinding {
        status: Some(RuntimeStatus::Starting),
        runtime_payload: Some(Value::Object(payload)),
        ..binding.clone()
    };
    directory.upsert(next, Default::default()).await.map_err(|e| anyhow::anyhow!("{e:?}"))?;
    let at = zc_core::now_iso();
    let mut session = session_json.clone();
    session["status"] = json!("starting");
    session["activeTurnId"] = Value::Null;
    session["lastError"] = Value::Null;
    session["updatedAt"] = json!(at);
    dispatch_json(
        engine,
        json!({ "type": "thread.session.set", "threadId": thread_id, "session": session, "createdAt": at }),
    )
    .await
}

/// The `settleAsError` of a continuation that failed in the background.
async fn settle_continuation_failure(engine: &OrchestrationEngine, directory: &ProviderSessionDirectory, thread_id: &ThreadId) {
    if let Ok(Some(binding)) = directory.get_binding(thread_id).await {
        let mut payload = payload_object(&binding);
        payload.insert("activeTurnId".into(), Value::Null);
        payload.insert(SERVER_UPDATE_CONTINUATION_KEY.into(), Value::Null);
        payload.insert(CONTINUATION_PREPARED_KEY.into(), Value::Null);
        let next = ProviderRuntimeBinding {
            status: Some(RuntimeStatus::Stopped),
            runtime_payload: Some(Value::Object(payload)),
            ..binding
        };
        let _ = directory.upsert(next, Default::default()).await;
    }
    let Ok(model) = engine.command_read_model().await else { return };
    let Some(session) = model
        .threads
        .iter()
        .find(|thread| &thread.id == thread_id)
        .and_then(|thread| thread.session.as_ref())
    else {
        return;
    };
    let at = zc_core::now_iso();
    let mut session = serde_json::to_value(session).unwrap_or_default();
    session["status"] = json!("error");
    session["activeTurnId"] = Value::Null;
    session["lastError"] = json!(CONTINUATION_FAILED_ERROR);
    session["updatedAt"] = json!(at);
    let command = json!({ "type": "thread.session.set", "threadId": thread_id, "session": session, "createdAt": at });
    if let Err(error) = dispatch_with_retry(engine, command).await {
        tracing::warn!(thread_id = %thread_id, %error, "failed to settle orphaned provider session projection");
    }
}

/// `reconcileWorktreeSetups`: a worktree setup recorded as running when the server stopped has
/// nobody left to finish it. Before the turn started it fails (the user sends again); after the
/// handoff only the setup script was running, so its stage fails and the setup settles done.
pub async fn reconcile_worktree_setups(engine: &OrchestrationEngine, reads: &Arc<dyn ProjectionReads>) {
    let recorded = match reads.list_activities_by_kind(WORKTREE_SETUP_ACTIVITY_KIND).await {
        Ok(recorded) => recorded,
        Err(error) => {
            tracing::warn!(%error, "worktree setup startup reconciliation failed");
            return;
        }
    };
    let interrupted_at = zc_core::now_iso();
    for activity in recorded {
        let Ok(snapshot) = serde_json::from_value::<WorktreeSetupSnapshot>(activity.payload.clone()) else {
            continue;
        };
        let mut snapshot = serde_json::to_value(&snapshot).unwrap_or_default();
        if snapshot["phase"] != "running" {
            continue;
        }
        let Some(thread_id) = snapshot["threadId"].as_str().map(str::to_owned) else {
            continue;
        };
        if activity.id.as_str() != format!("worktree-setup:{thread_id}") {
            continue;
        }
        let stages = snapshot["stages"].as_array().cloned().unwrap_or_default();
        let turn_started = stages.iter().any(|stage| stage["id"] == "agent" && stage["status"] == "done");
        let stages: Vec<Value> = stages
            .into_iter()
            .map(|mut stage| {
                if stage["status"] == "running" || stage["status"] == "pending" {
                    stage["status"] = json!("failed");
                    stage["endedAt"] = json!(interrupted_at);
                    stage["detail"] = json!("interrupted by a server restart");
                }
                stage
            })
            .collect();
        snapshot["phase"] = json!(if turn_started { "done" } else { "failed" });
        snapshot["endedAt"] = json!(interrupted_at);
        snapshot["error"] = if turn_started {
            Value::Null
        } else {
            json!("The server restarted before the worktree setup finished. Send the message again.")
        };
        snapshot["stages"] = Value::Array(stages);
        snapshot["sequence"] = json!(snapshot["sequence"].as_i64().unwrap_or(0) + 1);
        let started_at = snapshot["startedAt"].clone();
        let command = json!({
            "type": "thread.activity.append",
            "threadId": thread_id,
            "activity": {
                "id": format!("worktree-setup:{thread_id}"),
                "tone": "error",
                "kind": WORKTREE_SETUP_ACTIVITY_KIND,
                "summary": if turn_started { "Setup script interrupted by a server restart" } else { "Worktree setup interrupted by a server restart" },
                "payload": snapshot,
                "turnId": null,
                "createdAt": started_at,
            },
            "createdAt": interrupted_at,
        });
        if let Err(error) = dispatch_json(engine, command).await {
            tracing::warn!(thread_id, %error, "failed to settle interrupted worktree setup");
        }
    }
}

/// `autoPullProjects`: pulls each project whose `defaultAutoPull` is on and whose checkout is a
/// clean default branch behind its upstream, four at a time. Failures only log.
pub async fn auto_pull_projects(reads: &Arc<dyn ProjectionReads>, settings: &ServerSettingsService, git: &GitVcsDriver) {
    let projects = match reads.get_shell_snapshot(false).await {
        Ok(snapshot) => snapshot.projects,
        Err(error) => {
            tracing::warn!(%error, "Failed to load projects for automatic pull");
            return;
        }
    };
    let settings = match settings.get_settings_value().await {
        Ok(settings) => settings,
        Err(error) => {
            tracing::warn!(?error, "Failed to load projects for automatic pull");
            return;
        }
    };
    let mut roots: Vec<String> = Vec::new();
    for project in &projects {
        let root = project.workspace_root.to_string();
        if zc_vcs::broadcaster::resolve_default_auto_pull(&settings, project.id.as_str()) && !roots.contains(&root) {
            roots.push(root);
        }
    }
    futures::stream::iter(roots)
        .for_each_concurrent(4, |cwd| async move {
            let status = match git.status_details(&cwd).await {
                Ok(status) => status,
                Err(error) => {
                    tracing::warn!(cwd, %error, "Automatic project pull failed");
                    return;
                }
            };
            let skip = if !status.is_repo {
                Some("not-a-repository")
            } else if !status.is_default_branch {
                Some("not-on-default-branch")
            } else if !status.has_upstream {
                Some("no-upstream")
            } else if status.has_working_tree_changes {
                Some("working-tree-changes")
            } else if status.ahead_count > 0 {
                Some("local-commits")
            } else {
                None
            };
            if let Some(reason) = skip {
                tracing::debug!(cwd, reason, "Skipped automatic project pull");
                return;
            }
            if status.behind_count == 0 {
                return;
            }
            match git.pull_current_branch(&cwd).await {
                Ok(result) => tracing::debug!(cwd, ?result, "Automatic project pull completed"),
                Err(error) => tracing::warn!(cwd, %error, "Automatic project pull failed"),
            }
        })
        .await;
}

/// `resolveAutoBootstrapWelcomeTargets` (`--auto-bootstrap-project-from-cwd`): the project of
/// the cwd (created if missing) and its first thread (created if none). Returns the welcome
/// fields `bootstrapProjectId`, `bootstrapThreadId`, `bootstrapProjectCreated`,
/// `bootstrapThreadCreated`.
pub async fn auto_bootstrap_from_cwd(
    engine: &OrchestrationEngine,
    reads: &Arc<dyn ProjectionReads>,
    settings: &ServerSettingsService,
    cwd: &str,
) -> Map<String, Value> {
    let mut out = Map::new();
    let settings = settings.get_settings_value().await.unwrap_or_else(|_| json!({}));
    let default_model = settings
        .get("defaultModelSelection")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| json!({ "instanceId": "codex", "model": "gpt-6-astra" }));
    let existing = match reads.get_active_project_by_workspace_root(cwd).await {
        Ok(existing) => existing,
        Err(error) => {
            tracing::warn!(%error, "startup auto-bootstrap failed");
            return out;
        }
    };
    let (project_id, created, model) = match existing {
        Some(project) => {
            let project_model = settings["projectSettingsOverrides"][project.id.as_str()]["defaultModelSelection"].clone();
            let model = if project_model.is_null() { default_model.clone() } else { project_model };
            (project.id.to_string(), false, model)
        }
        None => {
            let project_id = zc_core::uuid_v4();
            let title = std::path::Path::new(cwd)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "project".into());
            let command = json!({ "type": "project.create", "projectId": project_id, "title": title, "workspaceRoot": cwd, "createdAt": zc_core::now_iso() });
            if let Err(error) = dispatch_json(engine, command).await {
                tracing::warn!(%error, "startup auto-bootstrap failed");
                return out;
            }
            (project_id, true, default_model.clone())
        }
    };
    out.insert("bootstrapProjectId".into(), json!(project_id));
    let existing_thread = reads.get_first_active_thread_id_by_project_id(&ProjectId::new(project_id.clone())).await;
    let (thread_id, thread_created) = match existing_thread {
        Ok(Some(thread_id)) => (Some(thread_id.to_string()), false),
        Ok(None) => {
            let thread_id = zc_core::uuid_v4();
            let runtime_mode = settings["projectSettingsOverrides"][project_id.as_str()]["defaultRuntimeMode"]
                .as_str()
                .or_else(|| settings["defaultRuntimeMode"].as_str())
                .unwrap_or("full-access")
                .to_owned();
            let command = json!({
                "type": "thread.create",
                "threadId": thread_id,
                "projectId": project_id,
                "title": "New thread",
                "modelSelection": model,
                "interactionMode": "default",
                "runtimeMode": runtime_mode,
                "branch": null,
                "worktreePath": null,
                "createdAt": zc_core::now_iso(),
            });
            match dispatch_json(engine, command).await {
                Ok(()) => (Some(thread_id), true),
                Err(error) => {
                    tracing::warn!(bootstrap_project_id = project_id, %error, "startup thread auto-bootstrap failed");
                    (None, false)
                }
            }
        }
        Err(error) => {
            tracing::warn!(bootstrap_project_id = project_id, %error, "startup thread auto-bootstrap failed");
            (None, false)
        }
    };
    if let Some(thread_id) = &thread_id {
        out.insert("bootstrapThreadId".into(), json!(thread_id));
    }
    out.insert("bootstrapProjectCreated".into(), json!(created));
    if thread_id.is_some() {
        out.insert("bootstrapThreadCreated".into(), json!(thread_created));
    }
    out
}
