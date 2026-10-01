//! The `thread.turn.start` bootstrap of `orchestration.dispatchCommand` (`ws.ts`
//! `dispatchBootstrapTurnStart`, plan §5.6):
//!
//! 1. optionally fetch `origin` and resolve the worktree base (`prepareWorktree`);
//! 2. create the thread and append the user message (`createThread`, `deferredTurn`);
//! 3. project a `starting` session, then `git worktree add` with checkout progress and
//!    submodules, tracked by the [`WorktreeSetupTracker`] card;
//! 4. run the project's setup script in a terminal (`runSetupScript`);
//! 5. dispatch the turn start itself.
//!
//! A failure or a user cancel (`worktreeSetup.cancel`) closes the setup terminal, removes the
//! created worktree, deletes the created thread (or marks its session failed when that fails)
//! and fails with `bootstrapThreadDisposition` so the client can restore its draft.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zc_contracts::{
    LitOrchestrationDispatchCommandError, OrchestrationClientOrigin, OrchestrationCommand, OrchestrationDispatchCommandError,
    OrchestrationDispatchCommandErrorBootstrapThreadDisposition as Disposition, ThreadId, ThreadTurnStartCommand, WorktreeSetupPhase, WorktreeSetupSnapshot,
    WorktreeSetupSnapshotSetupScript, WorktreeSetupStageId as Stage, WorktreeSetupStageStatus as Status,
};
use zc_core::defect::Defect;
use zc_ports::contracts::{TerminalCloseInput, VcsCreateWorktreeInput, VcsRemoveWorktreeInput, WorktreeSubmodules};
use zc_ports::git::{CheckoutProgress, CreateWorktreeOptions, CreateWorktreeProgress, SubmodulesDisabledSource};
use zc_ports::{DispatchResult, GitWorkflow, OrchestrationDispatch, ProjectionReads, SettingsService, TaggedError, TerminalManager, VcsStatusRefresher};

use crate::setup_script::{ObserveCompletion, ProjectSetupScriptRunnerError, SetupScriptInput, SetupScriptResult, SetupScriptRunner};
use crate::support::{decode_command, format_count, now_iso, server_command_id, uuid};
use crate::worktree_setup::{BootstrapHandle, StagePatch, WorktreeSetupTracker};

/// `WORKTREE_SETUP_ACTIVITY_KIND`.
pub const WORKTREE_SETUP_ACTIVITY_KIND: &str = "worktree-setup";
/// Attempts and spacing of the worktree removal after a cancel (the terminal is closed
/// asynchronously, so files may still be held open briefly).
const REMOVE_WORKTREE_RETRIES: usize = 4;
const REMOVE_WORKTREE_RETRY_SPACING: Duration = Duration::from_millis(500);

/// `worktreeSetupActivityId(threadId)`.
pub fn worktree_setup_activity_id(thread_id: &ThreadId) -> String {
    format!("worktree-setup:{thread_id}")
}

/// `ThreadDeletionReactor.drainThrough(sequence)`: wait until the deletion cleanup of every
/// thread deleted at or before `sequence` has run (WP-10).
#[async_trait]
pub trait ThreadDeletionDrain: Send + Sync {
    async fn drain_through(&self, sequence: i64);
}

/// Nothing to drain.
pub struct NoThreadDeletion;

#[async_trait]
impl ThreadDeletionDrain for NoThreadDeletion {
    async fn drain_through(&self, _sequence: i64) {}
}

/// What the bootstrap needs.
#[derive(Clone)]
pub struct BootstrapDeps {
    pub engine: Arc<dyn OrchestrationDispatch>,
    pub projections: Arc<dyn ProjectionReads>,
    pub git: Arc<dyn GitWorkflow>,
    pub vcs_status: Arc<dyn VcsStatusRefresher>,
    pub settings: Arc<dyn SettingsService>,
    pub terminals: Arc<dyn TerminalManager>,
    pub setup_scripts: Arc<dyn SetupScriptRunner>,
    pub tracker: WorktreeSetupTracker,
    pub thread_deletion: Arc<dyn ThreadDeletionDrain>,
}

/// Why the bootstrap program stopped.
#[derive(Debug, Clone)]
enum Failure {
    /// Already an `OrchestrationDispatchCommandError`.
    Command(OrchestrationDispatchCommandError),
    /// Any other error: its message and its encoded form as the cause.
    Other { message: String, cause: Defect },
    /// A user cancel.
    Interrupted,
}

impl From<TaggedError> for Failure {
    fn from(error: TaggedError) -> Self {
        let message = error.to_string();
        Self::Other {
            cause: Defect::error(&error.tag, message.clone()),
            message,
        }
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Other {
            cause: Defect::error("Error", message.clone()),
            message,
        }
    }
}

/// `new OrchestrationDispatchCommandError({message, cause?, bootstrapThreadDisposition?})`.
pub fn dispatch_command_error(message: impl Into<String>, cause: Option<Defect>, disposition: Option<Disposition>) -> OrchestrationDispatchCommandError {
    let message = message.into();
    OrchestrationDispatchCommandError {
        tag: LitOrchestrationDispatchCommandError,
        message: if message.trim().is_empty() {
            "Failed to bootstrap thread turn start.".into()
        } else {
            message
        },
        cause: cause.map(|c| c.0),
        bootstrap_thread_disposition: disposition,
    }
}

/// `toBootstrapDispatchCommandCauseError`.
fn to_dispatch_error(failure: &Failure) -> OrchestrationDispatchCommandError {
    match failure {
        Failure::Command(error) => error.clone(),
        Failure::Other { message, cause } => dispatch_command_error(message.clone(), Some(cause.clone()), None),
        Failure::Interrupted => dispatch_command_error("Failed to bootstrap thread turn start.", None, None),
    }
}

/// What the program learned, readable after a cancel dropped it mid-way.
#[derive(Default)]
struct Progress {
    created_thread: bool,
    target_project_id: Option<String>,
    target_project_cwd: Option<String>,
    target_worktree_path: Option<String>,
    /// The setup script's terminal: cancel closes only this one.
    setup_terminal_id: Option<String>,
    preparing_session_set: bool,
}

type PendingSetupScript = tokio::task::JoinHandle<()>;

/// One connection's bootstrap dispatcher: every command it dispatches carries the client's
/// origin (`dispatchFromClient`).
#[derive(Clone)]
pub struct BootstrapDispatcher {
    deps: BootstrapDeps,
    origin: Option<OrchestrationClientOrigin>,
}

struct Run {
    deps: BootstrapDeps,
    origin: Option<OrchestrationClientOrigin>,
    command: ThreadTurnStartCommand,
    tracked: bool,
    thread_id: ThreadId,
    progress: Arc<Mutex<Progress>>,
}

impl BootstrapDispatcher {
    pub fn new(deps: BootstrapDeps, origin: Option<OrchestrationClientOrigin>) -> Self {
        let origin = origin.filter(|o| o.surface.is_some() || o.app_version.is_some());
        Self { deps, origin }
    }

    /// `dispatchBootstrapTurnStart(command)`. A tracked bootstrap (`prepareWorktree`) runs as
    /// its own task, registered with the tracker so `worktreeSetup.cancel` can interrupt it;
    /// it outlives the caller (a reload must not abandon a half-made worktree).
    pub async fn dispatch_bootstrap_turn_start(&self, command: ThreadTurnStartCommand) -> Result<DispatchResult, OrchestrationDispatchCommandError> {
        let bootstrap = command.bootstrap.clone();
        let prepare = bootstrap.as_ref().and_then(|b| b.prepare_worktree.clone());
        let run = Arc::new(Run {
            deps: self.deps.clone(),
            origin: self.origin.clone(),
            thread_id: command.thread_id.clone(),
            tracked: prepare.is_some(),
            progress: Arc::new(Mutex::new(Progress {
                target_project_id: bootstrap.as_ref().and_then(|b| b.create_thread.as_ref()).map(|c| c.project_id.0.clone()),
                target_project_cwd: prepare.as_ref().map(|p| p.project_cwd.clone()),
                target_worktree_path: bootstrap.as_ref().and_then(|b| b.create_thread.as_ref()).and_then(|c| c.worktree_path.clone()),
                ..Progress::default()
            })),
            command,
        });
        if !run.tracked {
            return run.settled(None).await;
        }
        let (handle, token, finished) = BootstrapHandle::new();
        run.deps.tracker.begin(
            &run.thread_id,
            prepare.as_ref().and_then(|p| p.branch.clone()),
            prepare.as_ref().map(|p| p.base_branch.clone()),
            &[Stage::Fetch, Stage::Checkout, Stage::Submodules, Stage::SetupScript, Stage::Agent],
            Some(handle),
        );
        let task = tokio::spawn(async move {
            let result = run.settled(Some(token)).await;
            finished.finish();
            result
        });
        task.await
            .unwrap_or_else(|error| Err(dispatch_command_error(format!("Bootstrap task failed: {error}"), None, None)))
    }
}

impl Run {
    fn progress(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.progress.lock().unwrap()
    }

    fn track(&self, update: impl FnOnce(&WorktreeSetupTracker)) {
        if self.tracked {
            update(&self.deps.tracker);
        }
    }

    async fn dispatch(&self, command: Value) -> Result<DispatchResult, TaggedError> {
        let command = decode_command(command).map_err(|message| TaggedError::new("OrchestrationDispatchCommandError", message))?;
        self.dispatch_typed(command).await
    }

    async fn dispatch_typed(&self, command: OrchestrationCommand) -> Result<DispatchResult, TaggedError> {
        self.deps.engine.dispatch(command, self.origin.clone()).await
    }

    async fn append_setup_script_activity(
        &self,
        kind: &str,
        summary: &str,
        created_at: &str,
        payload: Value,
        tone: &str,
    ) -> Result<DispatchResult, TaggedError> {
        self.dispatch(json!({
            "type": "thread.activity.append",
            "commandId": server_command_id("setup-script-activity"),
            "threadId": self.thread_id,
            "activity": {
                "id": uuid(),
                "tone": tone,
                "kind": kind,
                "summary": summary,
                "payload": payload,
                "turnId": null,
                "createdAt": created_at,
            },
            "createdAt": created_at,
        }))
        .await
    }

    /// `recordWorktreeSetup(snapshot)`: the durable record, upserted by a fixed activity id.
    /// Best effort: the thread may already be gone.
    async fn record_worktree_setup(&self, snapshot: &WorktreeSetupSnapshot) {
        let failed = snapshot.phase == WorktreeSetupPhase::Failed || snapshot.stages.iter().any(|s| s.status == Status::Failed);
        let summary = match snapshot.phase {
            WorktreeSetupPhase::Running => "Setting up worktree",
            WorktreeSetupPhase::Done => "Worktree ready",
            WorktreeSetupPhase::Cancelled => "Worktree setup cancelled",
            WorktreeSetupPhase::Failed => "Worktree setup failed",
        };
        let result = self
            .dispatch(json!({
                "type": "thread.activity.append",
                "commandId": server_command_id("worktree-setup-activity"),
                "threadId": snapshot.thread_id,
                "activity": {
                    "id": worktree_setup_activity_id(&snapshot.thread_id),
                    "tone": if failed { "error" } else { "info" },
                    "kind": WORKTREE_SETUP_ACTIVITY_KIND,
                    "summary": summary,
                    "payload": snapshot,
                    "turnId": null,
                    "createdAt": snapshot.started_at,
                },
                "createdAt": snapshot.ended_at.as_deref().unwrap_or(&snapshot.started_at),
            }))
            .await;
        if let Err(error) = result {
            tracing::info!(thread_id = %snapshot.thread_id, error = %error, "could not record the worktree setup activity");
        }
    }

    async fn finish_and_record(&self, phase: WorktreeSetupPhase, error: Option<&str>) {
        if !self.tracked {
            return;
        }
        if let Some(snapshot) = self.deps.tracker.finish(&self.thread_id, phase, error) {
            self.record_worktree_setup(&snapshot).await;
        }
    }

    /// `resolveBootstrapWorktreeSubmodules`: project setting over environment setting; `None`
    /// lets the driver read the new checkout's own `t3.json`.
    async fn resolve_worktree_submodules(&self, project_id: Option<String>) -> Option<WorktreeSubmodules> {
        let settings = self.deps.settings.get_settings().await.ok()?;
        let settings = serde_json::to_value(&settings).ok()?;
        let project_id = match project_id {
            Some(project_id) => Some(project_id),
            None => self
                .deps
                .projections
                .get_thread_shell_by_id(&self.thread_id)
                .await
                .ok()
                .flatten()
                .map(|thread| thread.project_id.0),
        };
        let project_value = project_id.as_deref().and_then(|id| {
            settings
                .get("projectSettingsOverrides")
                .and_then(|overrides| overrides.get(id))
                .and_then(|entry| entry.get("worktreeSubmodules"))
        });
        let value = project_value.or_else(|| settings.get("worktreeSubmodules"))?;
        serde_json::from_value(value.clone()).ok()
    }

    async fn settled(self: &Arc<Self>, cancel: Option<CancellationToken>) -> Result<DispatchResult, OrchestrationDispatchCommandError> {
        let phase_one = async {
            match &cancel {
                Some(token) => tokio::select! {
                    biased;
                    _ = token.cancelled() => Err(Failure::Interrupted),
                    result = self.prepare() => result,
                },
                None => self.prepare().await,
            }
        };
        let result = match phase_one.await {
            Ok(pending) => self.start_turn(pending).await,
            Err(failure) => Err(failure),
        };
        match result {
            Ok(started) => Ok(started),
            Err(failure) => Err(self.fail(failure).await),
        }
    }

    /// The cancellable part of `bootstrapProgram`, up to the handoff.
    async fn prepare(&self) -> Result<Option<PendingSetupScript>, Failure> {
        let deps = &self.deps;
        let thread_id = &self.thread_id;
        let command = &self.command;
        let bootstrap = command.bootstrap.clone().unwrap_or(zc_contracts::ThreadTurnStartCommandBootstrap {
            create_thread: None,
            prepare_worktree: None,
            run_setup_script: None,
        });
        let prepare = bootstrap.prepare_worktree.clone();
        let mut should_prepare = match &prepare {
            Some(prepare) => deps.git.is_repository(&prepare.project_cwd).await?,
            None => false,
        };
        let mut base_ref: Option<String> = prepare.as_ref().map(|p| p.base_branch.clone());

        if let (Some(prepare), true) = (&prepare, should_prepare) {
            // "Start from origin" is a stored default: repos without the remote branch fall
            // back to the local base branch.
            let start_from_origin = prepare.start_from_origin == Some(true) && deps.git.remote_exists(&prepare.project_cwd, "origin").await?;
            if start_from_origin {
                self.track(|t| t.stage_status(thread_id, Stage::Fetch, Status::Running, None));
                deps.git.fetch_remote(&prepare.project_cwd, "origin", Some(&prepare.base_branch)).await?;
                if deps.git.remote_branch_exists(&prepare.project_cwd, "origin", &prepare.base_branch).await? {
                    let resolved = deps
                        .git
                        .resolve_remote_tracking_commit(&prepare.project_cwd, &prepare.base_branch, "origin")
                        .await?;
                    let short: String = resolved.commit_sha.chars().take(7).collect();
                    base_ref = Some(resolved.commit_sha.clone());
                    let detail = format!("origin/{} at {short}", prepare.base_branch);
                    self.track(|t| t.stage_status(thread_id, Stage::Fetch, Status::Done, Some(Some(&detail))));
                } else {
                    let detail = format!("origin/{} not found, using local branch", prepare.base_branch);
                    self.track(|t| t.stage_status(thread_id, Stage::Fetch, Status::Warning, Some(Some(&detail))));
                }
            } else {
                self.track(|t| t.stage_status(thread_id, Stage::Fetch, Status::Skipped, None));
            }
            let resolved_base = base_ref.clone().unwrap_or_else(|| prepare.base_branch.clone());
            should_prepare = deps.git.has_commit(&prepare.project_cwd, &resolved_base).await?;
            base_ref = Some(resolved_base.clone());
            self.track(|t| t.update(thread_id, |snapshot| snapshot.base_ref = Some(resolved_base)));
        }

        if let (Some(prepare), false) = (&prepare, should_prepare) {
            if prepare.require_worktree == Some(true) {
                return Err(Failure::Command(dispatch_command_error(
                    "A separate worktree requires a Git repository and a base branch with a commit.",
                    None,
                    None,
                )));
            }
            // Not a git repo, or the base has no commit: the thread runs in the project
            // checkout instead.
            self.track(|t| {
                t.update(thread_id, |snapshot| {
                    for stage in snapshot
                        .stages
                        .iter_mut()
                        .filter(|s| matches!(s.id, Stage::Fetch | Stage::Checkout | Stage::Submodules))
                    {
                        stage.status = Status::Skipped;
                        stage.detail = Some("using project checkout".into());
                    }
                })
            });
        }

        if let Some(create) = &bootstrap.create_thread {
            let created = self
                .dispatch(json!({
                    "type": "thread.create",
                    "commandId": server_command_id("bootstrap-thread-create"),
                    "threadId": thread_id,
                    "projectId": create.project_id,
                    "title": create.title,
                    "modelSelection": create.model_selection,
                    "runtimeMode": create.runtime_mode,
                    "interactionMode": create.interaction_mode,
                    "branch": create.branch,
                    "worktreePath": create.worktree_path,
                    "createdAt": create.created_at,
                }))
                .await?;
            // The create is a fence in the engine queue: drain the deletions of any prior
            // incarnation before setup or the turn can own resources under this id.
            self.progress().created_thread = true;
            deps.thread_deletion.drain_through(created.sequence).await;
            // Persist the message now: the thread is real from here on.
            let mut message = json!({
                "messageId": command.message.message_id,
                "text": command.message.text,
                "attachments": command.message.attachments,
            });
            if let Some(context) = &command.message.context {
                message["context"] = serde_json::to_value(context).unwrap_or(Value::Null);
            }
            self.dispatch(json!({
                "type": "thread.message.user.append",
                "commandId": server_command_id("bootstrap-thread-message"),
                "threadId": thread_id,
                "message": message,
                "createdAt": command.created_at,
            }))
            .await?;
            if self.tracked {
                if let Some(running) = deps.tracker.get(thread_id) {
                    self.record_worktree_setup(&running).await;
                }
            }
        }

        if let (Some(prepare), true, Some(base)) = (&prepare, should_prepare, base_ref.clone()) {
            let created_thread = self.progress().created_thread;
            if let (Some(create), true) = (&bootstrap.create_thread, created_thread) {
                // The checkout and setup can take minutes: list the thread as working now.
                let preparing_at = now_iso();
                self.dispatch(json!({
                    "type": "thread.session.set",
                    "commandId": server_command_id("bootstrap-thread-preparing"),
                    "threadId": thread_id,
                    "session": {
                        "threadId": thread_id,
                        "status": "starting",
                        "providerName": null,
                        "providerInstanceId": create.model_selection.instance_id,
                        "runtimeMode": command.runtime_mode,
                        "activeTurnId": null,
                        "lastError": null,
                        "updatedAt": preparing_at,
                    },
                    "createdAt": preparing_at,
                }))
                .await?;
                self.progress().preparing_session_set = true;
            }
            deps.tracker.stage_status(thread_id, Stage::Checkout, Status::Running, None);
            let target_project_id = self.progress().target_project_id.clone();
            let submodules = self.resolve_worktree_submodules(target_project_id).await;
            let checkout_total: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
            let progress = self.progress_callbacks(checkout_total.clone());
            let created = deps
                .git
                .create_worktree(
                    VcsCreateWorktreeInput(json!({
                        "cwd": prepare.project_cwd,
                        "refName": base,
                        "newRefName": prepare.branch,
                        "baseRefName": prepare.base_branch,
                        "path": null,
                    })),
                    CreateWorktreeOptions { progress, submodules },
                )
                .await?;
            let worktree_path = created.0.pointer("/worktree/path").and_then(Value::as_str).unwrap_or_default().to_owned();
            let worktree_ref = created.0.pointer("/worktree/refName").and_then(Value::as_str).unwrap_or_default().to_owned();
            let ended_at = now_iso();
            let total = *checkout_total.lock().unwrap();
            let path_for_card = worktree_path.clone();
            deps.tracker.update(thread_id, |snapshot| {
                snapshot.worktree_path = Some(path_for_card);
                for stage in snapshot.stages.iter_mut() {
                    if stage.id == Stage::Checkout && stage.status == Status::Running {
                        stage.status = Status::Done;
                        stage.percent = Some(100);
                        stage.ended_at = Some(ended_at.clone());
                        if let Some(total) = total {
                            stage.detail = Some(format!("{} files", format_count(total)));
                        }
                    }
                    if stage.id == Stage::Submodules && stage.status == Status::Pending {
                        stage.status = Status::Skipped;
                        stage.detail = Some("none".into());
                    }
                }
            });
            self.progress().target_worktree_path = Some(worktree_path.clone());
            self.dispatch(json!({
                "type": "thread.meta.update",
                "commandId": server_command_id("bootstrap-thread-meta-update"),
                "threadId": thread_id,
                "branch": worktree_ref,
                "worktreePath": worktree_path,
            }))
            .await?;
            let vcs_status = deps.vcs_status.clone();
            tokio::spawn(async move {
                if let Err(error) = vcs_status.refresh_status(&worktree_path).await {
                    tracing::info!(cwd = worktree_path, error = %error, "git status refresh after the worktree bootstrap failed");
                }
            });
        }

        let pending = self.run_setup_program(bootstrap.run_setup_script == Some(true)).await?;

        self.track(|t| t.stage_status(thread_id, Stage::Agent, Status::Running, None));
        // Past this point a cancel would roll back a thread whose turn has started.
        self.track(|t| t.mark_uncancellable(thread_id));
        Ok(pending)
    }

    /// `createWorktree`'s progress callbacks, driving the card.
    fn progress_callbacks(&self, checkout_total: Arc<Mutex<Option<u64>>>) -> CreateWorktreeProgress {
        let tracker = self.deps.tracker.clone();
        let thread_id = self.thread_id.clone();
        let claimed = self.progress.clone();
        let (t1, id1) = (tracker.clone(), thread_id.clone());
        let (t2, id2, total2) = (tracker.clone(), thread_id.clone(), checkout_total.clone());
        let (t3, id3) = (tracker.clone(), thread_id.clone());
        let (t4, id4) = (tracker.clone(), thread_id.clone());
        let (t5, id5) = (tracker, thread_id);
        CreateWorktreeProgress {
            // Git registered the directory: a cancel during submodules can still remove it.
            on_worktree_claimed: Some(Arc::new(move |path: &str| {
                claimed.lock().unwrap().target_worktree_path = Some(path.to_owned());
            })),
            on_checkout_progress: Some(Arc::new(move |progress: CheckoutProgress| {
                *checkout_total.lock().unwrap() = Some(progress.total);
                t1.stage(
                    &id1,
                    Stage::Checkout,
                    StagePatch {
                        percent: Some(Some(progress.percent.round() as i64)),
                        detail: Some(Some(format!("{} / {} files", format_count(progress.completed), format_count(progress.total)))),
                    },
                );
            })),
            on_submodules_started: Some(Arc::new(move || {
                let total = *total2.lock().unwrap();
                let detail = total.map(|total| format!("{} files", format_count(total)));
                t2.stage_status(&id2, Stage::Checkout, Status::Done, Some(detail.as_deref()));
                t2.stage_status(&id2, Stage::Submodules, Status::Running, None);
            })),
            on_submodules_disabled: Some(Arc::new(move |source: SubmodulesDisabledSource| {
                let source = match source {
                    SubmodulesDisabledSource::Settings => "settings",
                    SubmodulesDisabledSource::T3Json => "t3.json",
                };
                t3.stage_status(&id3, Stage::Submodules, Status::Skipped, Some(Some(&format!("disabled in {source}"))));
            })),
            on_submodule_line: Some(Arc::new(move |line: &str| {
                if let Some(path) = submodule_path(line) {
                    t4.stage(
                        &id4,
                        Stage::Submodules,
                        StagePatch {
                            percent: None,
                            detail: Some(Some(path)),
                        },
                    );
                }
            })),
            on_submodules_finished: Some(Arc::new(move |ok: bool, detail: Option<&str>| {
                if ok {
                    t5.stage_status(&id5, Stage::Submodules, Status::Done, None);
                } else {
                    t5.stage_status(
                        &id5,
                        Stage::Submodules,
                        Status::Warning,
                        Some(Some(detail.unwrap_or("submodule checkout failed"))),
                    );
                }
            })),
        }
    }

    /// `runSetupProgram`: starts the setup script. For a tracked bootstrap it returns the task
    /// that waits for an async script to exit (a blocking script is awaited here).
    async fn run_setup_program(&self, run_setup_script: bool) -> Result<Option<PendingSetupScript>, Failure> {
        let thread_id = &self.thread_id;
        let worktree_path = self.progress().target_worktree_path.clone();
        let Some(worktree_path) = worktree_path.filter(|_| run_setup_script) else {
            self.track(|t| t.stage_status(thread_id, Stage::SetupScript, Status::Skipped, None));
            return Ok(None);
        };
        let requested_at = now_iso();
        self.track(|t| t.stage_status(thread_id, Stage::SetupScript, Status::Running, None));
        let (project_id, project_cwd) = {
            let progress = self.progress();
            (progress.target_project_id.clone(), progress.target_project_cwd.clone())
        };
        let observe = self.tracked.then(|| {
            let tracker = self.deps.tracker.clone();
            let thread_id = thread_id.clone();
            ObserveCompletion {
                on_output_line: Some(Arc::new(move |line: &str| tracker.append_tail(&thread_id, Stage::SetupScript, line))),
            }
        });
        let result = self
            .deps
            .setup_scripts
            .run_for_thread(SetupScriptInput {
                thread_id: thread_id.0.clone(),
                project_id: project_id.filter(|p| !p.is_empty()),
                project_cwd: project_cwd.filter(|p| !p.is_empty()),
                worktree_path: worktree_path.clone(),
                preferred_terminal_id: None,
                observe_completion: observe,
            })
            .await;
        let started = match result {
            Err(error) => {
                self.record_setup_script_launch_failure(&error, &requested_at, &worktree_path).await;
                self.track(|t| t.stage_status(thread_id, Stage::SetupScript, Status::Failed, Some(Some("failed to start"))));
                return Ok(None);
            }
            Ok(SetupScriptResult::NoScript) => {
                self.track(|t| t.stage_status(thread_id, Stage::SetupScript, Status::Skipped, Some(Some("no setup script"))));
                return Ok(None);
            }
            Ok(SetupScriptResult::Started(started)) => started,
        };
        self.progress().setup_terminal_id = Some(started.terminal_id.clone());
        self.record_setup_script_started(&requested_at, &worktree_path, &started.script_id, &started.script_name, &started.terminal_id)
            .await;
        let card_script = WorktreeSetupSnapshotSetupScript {
            name: started.script_name.clone(),
            command: started.script_command.clone(),
            terminal_id: started.terminal_id.clone(),
        };
        self.track(|t| t.update(thread_id, |snapshot| snapshot.setup_script = Some(card_script)));
        let Some(completion) = started.completion.filter(|_| self.tracked) else {
            return Ok(None);
        };
        // Best effort, like the untracked path: a failed install keeps the worktree. Spawned
        // right away so the terminal listener is always consumed.
        let tracker = self.deps.tracker.clone();
        let card_thread = thread_id.clone();
        let wait = tokio::spawn(async move {
            let completion = completion.await;
            match completion.exit_code {
                Some(0) => tracker.stage_status(&card_thread, Stage::SetupScript, Status::Done, None),
                None => tracker.stage_status(
                    &card_thread,
                    Stage::SetupScript,
                    Status::Failed,
                    Some(Some("terminal closed before the script finished")),
                ),
                Some(code) => tracker.stage_status(&card_thread, Stage::SetupScript, Status::Failed, Some(Some(&format!("exit {code}")))),
            }
        });
        if !started.r#async {
            let _ = wait.await;
            return Ok(None);
        }
        Ok(Some(wait))
    }

    async fn record_setup_script_launch_failure(&self, error: &ProjectSetupScriptRunnerError, requested_at: &str, worktree_path: &str) {
        let detail = error.compatibility_detail();
        let _ = self
            .append_setup_script_activity(
                "setup-script.failed",
                "Setup script failed to start",
                requested_at,
                json!({"detail": detail, "worktreePath": worktree_path}),
                "error",
            )
            .await;
        tracing::warn!(thread_id = %self.thread_id, worktree_path, detail, "bootstrap turn start failed to launch setup script");
    }

    async fn record_setup_script_started(&self, requested_at: &str, worktree_path: &str, script_id: &str, script_name: &str, terminal_id: &str) {
        let started_at = now_iso();
        let payload = json!({
            "scriptId": script_id,
            "scriptName": script_name,
            "terminalId": terminal_id,
            "worktreePath": worktree_path,
        });
        let result = async {
            self.append_setup_script_activity("setup-script.requested", "Starting setup script", requested_at, payload.clone(), "info")
                .await?;
            self.append_setup_script_activity("setup-script.started", "Setup script started", &started_at, payload.clone(), "info")
                .await
        }
        .await;
        if let Err(error) = result {
            tracing::warn!(thread_id = %self.thread_id, worktree_path, script_id, terminal_id, detail = %error, "bootstrap turn start launched setup script but failed to record setup activity");
        }
    }

    /// The handoff: dispatch the turn start (no longer cancellable), then settle the card.
    async fn start_turn(self: &Arc<Self>, pending: Option<PendingSetupScript>) -> Result<DispatchResult, Failure> {
        let mut final_command = self.command.clone();
        final_command.bootstrap = None;
        let started = self.dispatch_typed(OrchestrationCommand::ThreadTurnStartCommand(final_command)).await?;
        self.track(|t| t.stage_status(&self.thread_id, Stage::Agent, Status::Done, None));
        // An async setup script outlives the handoff: the card stays running and settles when
        // the script exits.
        match pending {
            Some(pending) => {
                let run = self.clone();
                tokio::spawn(async move {
                    let _ = pending.await;
                    run.finish_and_record(WorktreeSetupPhase::Done, None).await;
                });
            }
            None => self.finish_and_record(WorktreeSetupPhase::Done, None).await,
        }
        Ok(started)
    }

    /// The `catchCause` of `settledBootstrapProgram`: record the outcome, roll back, and
    /// return the error the client gets.
    async fn fail(&self, failure: Failure) -> OrchestrationDispatchCommandError {
        let dispatch_error = to_dispatch_error(&failure);
        if matches!(failure, Failure::Interrupted) {
            // A user cancel: record it, close the setup terminal (so a running script cannot
            // hold files open), remove the worktree, then roll the thread back.
            self.finish_and_record(WorktreeSetupPhase::Cancelled, None).await;
            self.remove_created_worktree().await;
            if self.tracked {
                return self.cleanup_and_fail(dispatch_command_error("Worktree setup cancelled.", None, None)).await;
            }
            return dispatch_error;
        }
        self.finish_and_record(WorktreeSetupPhase::Failed, Some(&dispatch_error.message)).await;
        self.cleanup_and_fail(dispatch_error).await
    }

    async fn remove_created_worktree(&self) {
        let (setup_terminal_id, worktree_path) = {
            let progress = self.progress();
            (progress.setup_terminal_id.clone(), progress.target_worktree_path.clone())
        };
        let prepare = self.command.bootstrap.as_ref().and_then(|b| b.prepare_worktree.clone());
        let (true, Some(worktree_path), Some(prepare)) = (self.tracked, worktree_path, prepare) else {
            return;
        };
        if let Some(terminal_id) = setup_terminal_id {
            let closed = self
                .deps
                .terminals
                .close(TerminalCloseInput(
                    json!({"threadId": self.thread_id, "terminalId": terminal_id, "deleteHistory": true}),
                ))
                .await;
            if let Err(error) = closed {
                tracing::info!(thread_id = %self.thread_id, error = %error, "closing the setup terminal after a cancel failed");
            }
        }
        for attempt in 0..=REMOVE_WORKTREE_RETRIES {
            let removed = self
                .deps
                .git
                .remove_worktree(VcsRemoveWorktreeInput(
                    json!({"cwd": prepare.project_cwd, "path": worktree_path, "force": true}),
                ))
                .await;
            match removed {
                Ok(()) => return,
                Err(error) if attempt == REMOVE_WORKTREE_RETRIES => {
                    tracing::info!(thread_id = %self.thread_id, error = %error, "removing the cancelled worktree failed");
                }
                Err(_) => tokio::time::sleep(REMOVE_WORKTREE_RETRY_SPACING).await,
            }
        }
    }

    /// `cleanupAndFail`.
    async fn cleanup_and_fail(&self, dispatch_error: OrchestrationDispatchCommandError) -> OrchestrationDispatchCommandError {
        let created_thread = self.progress().created_thread;
        let cleanup = if created_thread {
            self.dispatch(json!({
                "type": "thread.delete",
                "commandId": server_command_id("bootstrap-thread-delete"),
                "threadId": self.command.thread_id,
            }))
            .await
            .map(|_| true)
        } else {
            Ok(false)
        };
        match cleanup {
            Err(error) => {
                tracing::warn!(thread_id = %self.thread_id, detail = %error, "bootstrap thread cleanup failed");
                // The thread outlived its setup: its preparing session must not read as
                // working forever.
                if self.progress().preparing_session_set {
                    if let Err(error) = self.mark_preparing_session_failed(&dispatch_error.message).await {
                        tracing::info!(thread_id = %self.thread_id, error = %error, "could not mark the preparing session failed");
                    }
                }
                dispatch_error
            }
            Ok(thread_deleted) => {
                let bootstrap = self.command.bootstrap.as_ref();
                let not_created = bootstrap.is_some_and(|b| {
                    b.create_thread.is_some() && b.prepare_worktree.as_ref().is_some_and(|p| p.require_worktree == Some(true)) && !created_thread
                });
                if thread_deleted || not_created {
                    OrchestrationDispatchCommandError {
                        bootstrap_thread_disposition: Some(if thread_deleted { Disposition::Deleted } else { Disposition::NotCreated }),
                        ..dispatch_error
                    }
                } else {
                    dispatch_error
                }
            }
        }
    }

    async fn mark_preparing_session_failed(&self, detail: &str) -> Result<DispatchResult, TaggedError> {
        let failed_at = now_iso();
        let instance_id = self
            .command
            .bootstrap
            .as_ref()
            .and_then(|b| b.create_thread.as_ref())
            .map(|c| c.model_selection.instance_id.clone())
            .or_else(|| self.command.model_selection.as_ref().map(|m| m.instance_id.clone()));
        let mut session = json!({
            "threadId": self.thread_id,
            "status": "error",
            "providerName": null,
            "runtimeMode": self.command.runtime_mode,
            "activeTurnId": null,
            "lastError": if detail.trim().is_empty() { "Worktree setup failed." } else { detail },
            "updatedAt": failed_at,
        });
        if let Some(instance_id) = instance_id {
            session["providerInstanceId"] = json!(instance_id);
        }
        self.dispatch(json!({
            "type": "thread.session.set",
            "commandId": server_command_id("bootstrap-thread-preparing-failed"),
            "threadId": self.thread_id,
            "session": session,
            "createdAt": failed_at,
        }))
        .await
    }
}

/// `/Submodule path '([^']+)'/`.
fn submodule_path(line: &str) -> Option<String> {
    let start = line.find("Submodule path '")? + "Submodule path '".len();
    let rest = &line[start..];
    let end = rest.find('\'')?;
    (end > 0).then(|| rest[..end].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submodule_lines_name_their_path() {
        assert_eq!(submodule_path("Submodule path 'vendor/lib': checked out 'abc'").as_deref(), Some("vendor/lib"));
        assert_eq!(submodule_path("Cloning into 'x'"), None);
        assert_eq!(worktree_setup_activity_id(&ThreadId::new("t")), "worktree-setup:t");
    }
}
