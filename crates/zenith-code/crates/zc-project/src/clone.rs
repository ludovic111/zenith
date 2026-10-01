//! `project/ProjectCloneTracker.ts`: repository clones that back newly added projects, with
//! their progress for every client (`projectClone.start|cancel|retry`, `subscribeProjectClones`).
//!
//! The clone is detached from the request that started it: the project already exists
//! (pointing at the empty destination) when `start` returns, and git runs in its own task.
//! Snapshots are memory only. A done clone is dropped after a 30 s grace window; a failed or
//! cancelled one stays until it is retried, discarded or the server restarts, since the empty
//! project is the durable record the user can act on.
//!
//! The tracker never dispatches itself: the caller's [`ProjectCloneHooks`] create the project
//! (with the client's origin) and refresh identity and git status after the clone.
//! [`reject_commands_during_clone`] and [`discard_clone_for_deleted_project`] are the two
//! dispatch hooks every transport runs.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::StreamExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use zc_contracts::{
    LitOrchestrationDispatchCommandError, OrchestrationDispatchCommandError, ProjectClonePhase, ProjectCloneSnapshot, ProjectCloneStage,
    ProjectCloneStartInput, ProjectCloneStartResult, ProjectId, SourceControlCloneRepositoryInput, SourceControlCloneRepositoryResult,
    SourceControlProviderKind,
};
use zc_sourcecontrol::clone_progress::GitCloneProgressLine;
use zc_sourcecontrol::repository::CloneTimeout;
use zc_sourcecontrol::{SourceControlCloneOptions, SourceControlPreparedClone, SourceControlRepositoryError, SourceControlRepositoryService};

/// `PROJECT_CLONE_DETAIL_MAX_LENGTH`.
pub const DETAIL_MAX_LENGTH: usize = 200;
/// `PROJECT_CLONE_ERROR_MAX_LENGTH`.
pub const ERROR_MAX_LENGTH: usize = 1000;
/// `DONE_RETENTION`: finished snapshots stay visible this long so a late subscriber sees them.
pub const DONE_RETENTION: Duration = Duration::from_secs(30);

/// `clampText`: at most `max` UTF-16 units, ending in an ellipsis when cut.
pub fn clamp_text(text: &str, max: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= max {
        return text.to_owned();
    }
    let mut out = String::from_utf16_lossy(&units[..max - 1]);
    out.push('\u{2026}');
    out
}

/// The part of `SourceControlRepositoryService` the tracker uses (tests script it).
#[async_trait]
pub trait CloneRepositories: Send + Sync {
    async fn prepare_clone(&self, input: &SourceControlCloneRepositoryInput) -> Result<SourceControlPreparedClone, SourceControlRepositoryError>;
    async fn clone_repository(
        &self,
        input: &SourceControlCloneRepositoryInput,
        options: &SourceControlCloneOptions,
    ) -> Result<SourceControlCloneRepositoryResult, SourceControlRepositoryError>;
    async fn discard_clone(&self, destination_path: &str) -> Result<(), SourceControlRepositoryError>;
}

#[async_trait]
impl CloneRepositories for SourceControlRepositoryService {
    async fn prepare_clone(&self, input: &SourceControlCloneRepositoryInput) -> Result<SourceControlPreparedClone, SourceControlRepositoryError> {
        SourceControlRepositoryService::prepare_clone(self, input).await
    }

    async fn clone_repository(
        &self,
        input: &SourceControlCloneRepositoryInput,
        options: &SourceControlCloneOptions,
    ) -> Result<SourceControlCloneRepositoryResult, SourceControlRepositoryError> {
        SourceControlRepositoryService::clone_repository(self, input, options).await
    }

    async fn discard_clone(&self, destination_path: &str) -> Result<(), SourceControlRepositoryError> {
        SourceControlRepositoryService::discard_clone(self, destination_path).await
    }
}

/// The project a clone backs (`createProject` input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClonedProject {
    pub project_id: ProjectId,
    pub title: String,
    pub workspace_root: String,
    pub created_at: String,
}

/// `ProjectCloneHooks`: the orchestration side effects the caller owns.
#[async_trait]
pub trait ProjectCloneHooks: Send + Sync {
    /// Dispatch `project.create` for the (still empty) destination.
    async fn create_project(&self, project: ClonedProject) -> Result<(), OrchestrationDispatchCommandError>;
    /// After a successful clone: refresh the cached repository identity and git status.
    async fn on_cloned(&self, project_id: &ProjectId, workspace_root: &str);
}

/// Why `start` failed (before anything was created, or when creating the project failed).
#[derive(Debug, Clone)]
pub enum ProjectCloneStartError {
    Repository(SourceControlRepositoryError),
    Dispatch(OrchestrationDispatchCommandError),
}

impl ProjectCloneStartError {
    /// The TS `message` of the error.
    pub fn message(&self) -> String {
        match self {
            Self::Repository(error) => error.message(),
            Self::Dispatch(error) => error.message.to_string(),
        }
    }

    /// The encoded failure (`SourceControlRepositoryError | OrchestrationDispatchCommandError`).
    pub fn to_wire(&self) -> serde_json::Value {
        match self {
            Self::Repository(error) => serde_json::to_value(error.to_wire()).unwrap_or_default(),
            Self::Dispatch(error) => serde_json::to_value(error).unwrap_or_default(),
        }
    }
}

/// The running clone task (the TS fiber): cancel it, then wait until it has recorded its
/// outcome.
#[derive(Clone)]
struct CloneTask {
    token: CancellationToken,
    finished: watch::Receiver<bool>,
}

impl CloneTask {
    async fn interrupt(&self) {
        self.token.cancel();
        let mut finished = self.finished.clone();
        let _ = finished.wait_for(|done| *done).await;
    }
}

struct Tracked {
    snapshot: ProjectCloneSnapshot,
    task: Option<CloneTask>,
    hooks: Arc<dyn ProjectCloneHooks>,
    /// What git is given; may carry credentials and never leaves the server.
    clone_url: String,
    destination_path: String,
}

#[derive(Default)]
struct State {
    /// Insertion order, like the TS `Map`.
    clones: Vec<(ProjectId, Tracked)>,
    retention: Vec<(ProjectId, u64, JoinHandle<()>)>,
    retention_ids: u64,
    sequence: i64,
}

impl State {
    fn get(&self, project_id: &ProjectId) -> Option<&Tracked> {
        self.clones.iter().find(|(id, _)| id == project_id).map(|(_, tracked)| tracked)
    }

    fn get_mut(&mut self, project_id: &ProjectId) -> Option<&mut Tracked> {
        self.clones.iter_mut().find(|(id, _)| id == project_id).map(|(_, tracked)| tracked)
    }

    fn list(&self) -> Vec<ProjectCloneSnapshot> {
        self.clones.iter().map(|(_, tracked)| tracked.snapshot.clone()).collect()
    }

    fn remove(&mut self, project_id: &ProjectId) -> bool {
        let before = self.clones.len();
        self.clones.retain(|(id, _)| id != project_id);
        before != self.clones.len()
    }
}

struct Inner {
    repositories: Arc<dyn CloneRepositories>,
    state: Mutex<State>,
    changes: watch::Sender<Vec<ProjectCloneSnapshot>>,
    /// start/cancel/retry/discard mutate the same entry and directory: one at a time keeps a
    /// double-clicked Retry from racing two clones into it.
    action_lock: tokio::sync::Mutex<()>,
    /// Clone tasks outlive the RPC that started them but not the server.
    shutdown: CancellationToken,
}

/// The `ProjectCloneTracker` service. Cloning shares it.
#[derive(Clone)]
pub struct ProjectCloneTracker {
    inner: Arc<Inner>,
}

fn now_iso() -> String {
    zc_core::now_iso()
}

impl ProjectCloneTracker {
    pub fn new(repositories: Arc<dyn CloneRepositories>) -> Self {
        let (changes, _) = watch::channel(Vec::new());
        Self {
            inner: Arc::new(Inner {
                repositories,
                state: Mutex::new(State::default()),
                changes,
                action_lock: tokio::sync::Mutex::new(()),
                shutdown: CancellationToken::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn publish_locked(&self, state: &State) {
        self.inner.changes.send_replace(state.list());
    }

    /// `modify`: update an entry, bump its sequence, publish. `None` when it is gone.
    fn modify(&self, project_id: &ProjectId, mutate: impl FnOnce(&mut Tracked)) -> Option<ProjectCloneSnapshot> {
        let mut state = self.lock();
        state.sequence += 1;
        let sequence = state.sequence;
        let tracked = state.get_mut(project_id)?;
        mutate(tracked);
        tracked.snapshot.sequence = sequence;
        let snapshot = tracked.snapshot.clone();
        self.publish_locked(&state);
        Some(snapshot)
    }

    fn remove(&self, project_id: &ProjectId) {
        let mut state = self.lock();
        state.remove(project_id);
        self.publish_locked(&state);
    }

    fn clear_retention(&self, project_id: &ProjectId) {
        let mut state = self.lock();
        state.retention.retain(|(id, _, handle)| {
            if id == project_id {
                handle.abort();
                false
            } else {
                true
            }
        });
    }

    fn schedule_removal(&self, project_id: &ProjectId) {
        self.clear_retention(project_id);
        let tracker = self.clone();
        let mut state = self.lock();
        state.retention_ids += 1;
        let id = state.retention_ids;
        let target = project_id.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(DONE_RETENTION).await;
            tracker.remove(&target);
            tracker
                .lock()
                .retention
                .retain(|(project, retention_id, _)| !(project == &target && *retention_id == id));
        });
        state.retention.push((project_id.clone(), id, handle));
    }

    fn progress(&self, project_id: &ProjectId, update: GitCloneProgressLine) {
        self.modify(project_id, |tracked| {
            tracked.snapshot.stage = update.stage;
            tracked.snapshot.percent = update.percent.map(i64::from);
            tracked.snapshot.detail = update.detail.as_deref().map(|detail| clamp_text(detail, DETAIL_MAX_LENGTH));
        });
    }

    fn finish(&self, project_id: &ProjectId, phase: ProjectClonePhase, error: Option<String>) {
        let ended_at = now_iso();
        self.modify(project_id, |tracked| {
            tracked.task = None;
            tracked.snapshot.phase = phase;
            tracked.snapshot.ended_at = Some(ended_at);
            if phase == ProjectClonePhase::Done {
                tracked.snapshot.percent = Some(100);
            }
            tracked.snapshot.error = error.as_deref().map(|error| clamp_text(error, ERROR_MAX_LENGTH));
        });
        if phase == ProjectClonePhase::Done {
            self.schedule_removal(project_id);
        }
    }

    /// `launch`: run the clone in its own task. The task records the outcome even when it is
    /// cancelled; once git has finished the clone is marked done before the post-clone hook
    /// runs, so a late cancel cannot tear down a complete checkout.
    fn launch(&self, project_id: &ProjectId) {
        let token = self.inner.shutdown.child_token();
        let (finished_sender, finished) = watch::channel(false);
        let (hooks, input) = {
            let mut state = self.lock();
            let Some(tracked) = state.get_mut(project_id) else {
                return;
            };
            // Registered before the task can run, so its `finish` clears it.
            tracked.task = Some(CloneTask {
                token: token.clone(),
                finished,
            });
            (
                tracked.hooks.clone(),
                SourceControlCloneRepositoryInput {
                    provider: None,
                    repository: None,
                    remote_url: Some(tracked.clone_url.clone()),
                    destination_path: tracked.destination_path.clone(),
                    protocol: None,
                },
            )
        };
        let tracker = self.clone();
        let project_id = project_id.clone();
        tokio::spawn(async move {
            let _finished = DropSignal(finished_sender);
            let progress_tracker = tracker.clone();
            let progress_id = project_id.clone();
            let options = SourceControlCloneOptions {
                on_progress: Some(Arc::new(move |line: GitCloneProgressLine| progress_tracker.progress(&progress_id, line))),
                timeout: CloneTimeout::None,
            };
            let outcome = tokio::select! {
                result = tracker.inner.repositories.clone_repository(&input, &options) => Some(result),
                () = token.cancelled() => None,
            };
            match outcome {
                Some(Ok(_)) => tracker.finish(&project_id, ProjectClonePhase::Done, None),
                Some(Err(error)) => {
                    tracker.finish(&project_id, ProjectClonePhase::Failed, Some(describe_clone_failure(&error)));
                    return;
                }
                None => {
                    tracker.finish(&project_id, ProjectClonePhase::Cancelled, None);
                    return;
                }
            }
            tokio::select! {
                () = hooks.on_cloned(&project_id, &input.destination_path) => {}
                () = token.cancelled() => {}
            }
        });
    }

    /// `start`: resolve the remote, create the project, start the clone in the background.
    /// Fails before creating anything when the destination or repository is unusable.
    pub async fn start(&self, input: ProjectCloneStartInput, hooks: Arc<dyn ProjectCloneHooks>) -> Result<ProjectCloneStartResult, ProjectCloneStartError> {
        let _guard = self.inner.action_lock.lock().await;
        let clone_input = SourceControlCloneRepositoryInput {
            provider: input.provider,
            repository: input.repository.clone(),
            remote_url: input.remote_url.clone(),
            destination_path: input.destination_path.clone(),
            protocol: input.protocol,
        };
        let prepared = self
            .inner
            .repositories
            .prepare_clone(&clone_input)
            .await
            .map_err(ProjectCloneStartError::Repository)?;
        let started_at = now_iso();
        let claimed = {
            let mut state = self.lock();
            // A second start for the same project, or into a destination another clone owns,
            // must not race two gits into one directory.
            let conflict = state
                .clones
                .iter()
                .any(|(id, tracked)| id == &input.project_id || tracked.destination_path == prepared.destination_path);
            if !conflict {
                state.sequence += 1;
                let snapshot = ProjectCloneSnapshot {
                    project_id: input.project_id.clone(),
                    remote_url: prepared.remote_url.clone(),
                    destination_path: prepared.destination_path.clone(),
                    repository: prepared.repository.clone(),
                    phase: ProjectClonePhase::Running,
                    stage: ProjectCloneStage::Connecting,
                    percent: None,
                    detail: None,
                    error: None,
                    started_at,
                    ended_at: None,
                    sequence: state.sequence,
                };
                state.clones.push((
                    input.project_id.clone(),
                    Tracked {
                        snapshot,
                        task: None,
                        hooks: hooks.clone(),
                        clone_url: prepared.clone_url.clone(),
                        destination_path: prepared.destination_path.clone(),
                    },
                ));
            }
            !conflict
        };
        if !claimed {
            return Err(ProjectCloneStartError::Repository(SourceControlRepositoryError::new(
                "cloneRepository",
                input.provider.unwrap_or(SourceControlProviderKind::Unknown),
                "A clone into this destination is already in progress.",
            )));
        }
        // Everything after the claim runs to completion even if the requesting connection
        // drops (a claimed entry with no task could neither be cancelled nor retried); any
        // failure releases the claim.
        let tracker = self.clone();
        let project_id = input.project_id.clone();
        let project = ClonedProject {
            project_id: input.project_id.clone(),
            title: input.title.clone(),
            workspace_root: prepared.destination_path.clone(),
            created_at: input.created_at.clone(),
        };
        let completed = tokio::spawn(async move {
            tracker.clear_retention(&project_id);
            // The entry exists before the project does, so a racing thread.create or
            // project.delete already sees it.
            if let Err(error) = hooks.create_project(project).await {
                tracker.remove(&project_id);
                return Err(ProjectCloneStartError::Dispatch(error));
            }
            {
                let state = tracker.lock();
                tracker.publish_locked(&state);
            }
            tracker.launch(&project_id);
            Ok(())
        })
        .await;
        match completed {
            Ok(result) => result?,
            Err(error) => {
                self.remove(&input.project_id);
                return Err(ProjectCloneStartError::Dispatch(OrchestrationDispatchCommandError {
                    tag: LitOrchestrationDispatchCommandError,
                    message: format!("The project could not be created: {error}"),
                    cause: None,
                    bootstrap_thread_disposition: None,
                }));
            }
        }
        Ok(ProjectCloneStartResult {
            project_id: input.project_id,
            cwd: prepared.destination_path,
            remote_url: prepared.remote_url,
            repository: prepared.repository,
        })
    }

    /// `get(projectId)`.
    pub fn get(&self, project_id: &ProjectId) -> Option<ProjectCloneSnapshot> {
        self.lock().get(project_id).map(|tracked| tracked.snapshot.clone())
    }

    /// Every tracked clone, in start order.
    pub fn list(&self) -> Vec<ProjectCloneSnapshot> {
        self.lock().list()
    }

    /// `cancel(projectId)`: interrupt a running clone and delete the partial checkout. `false`
    /// when nothing is running.
    pub async fn cancel(&self, project_id: &ProjectId) -> bool {
        let _guard = self.inner.action_lock.lock().await;
        let (task, destination) = {
            let state = self.lock();
            match state.get(project_id) {
                Some(tracked) if tracked.snapshot.phase == ProjectClonePhase::Running && tracked.task.is_some() => {
                    (tracked.task.clone().expect("checked"), tracked.destination_path.clone())
                }
                _ => return false,
            }
        };
        // Detached: a client that disconnects mid-cancel must not leave a dead task behind a
        // "running" snapshot.
        let tracker = self.clone();
        let project_id = project_id.clone();
        let _ = tokio::spawn(async move {
            task.interrupt().await;
            match tracker.get(&project_id).map(|snapshot| snapshot.phase) {
                // Git finished in the window before the interrupt landed: keep it.
                Some(ProjectClonePhase::Done) => return,
                Some(ProjectClonePhase::Running) => tracker.finish(&project_id, ProjectClonePhase::Cancelled, None),
                _ => {}
            }
            // The partial checkout goes so a retry starts from an empty destination.
            let _ = tracker.inner.repositories.discard_clone(&destination).await;
        })
        .await;
        true
    }

    /// `retry(projectId)`: restart a failed or cancelled clone into the same destination.
    pub async fn retry(&self, project_id: &ProjectId) -> Result<bool, SourceControlRepositoryError> {
        let _guard = self.inner.action_lock.lock().await;
        let destination = match self.lock().get(project_id) {
            Some(tracked) if matches!(tracked.snapshot.phase, ProjectClonePhase::Failed | ProjectClonePhase::Cancelled) => tracked.destination_path.clone(),
            _ => return Ok(false),
        };
        // A retry into leftover files would fail on the non-empty destination with a less
        // useful message, so a cleanup failure is the error here.
        self.inner.repositories.discard_clone(&destination).await?;
        let started_at = now_iso();
        self.modify(project_id, |tracked| {
            let snapshot = &mut tracked.snapshot;
            snapshot.phase = ProjectClonePhase::Running;
            snapshot.stage = ProjectCloneStage::Connecting;
            snapshot.percent = None;
            snapshot.detail = None;
            snapshot.error = None;
            snapshot.started_at = started_at;
            snapshot.ended_at = None;
        });
        self.launch(project_id);
        Ok(true)
    }

    /// `discard(projectId)`: stop tracking a project's clone (the project is being deleted),
    /// interrupting it and removing an unfinished checkout.
    pub async fn discard(&self, project_id: &ProjectId) {
        let _guard = self.inner.action_lock.lock().await;
        let (task, destination) = match self.lock().get(project_id) {
            Some(tracked) => (tracked.task.clone(), tracked.destination_path.clone()),
            None => return,
        };
        if let Some(task) = task {
            task.interrupt().await;
        }
        // Git may have finished while the interrupt was landing; a complete checkout is the
        // user's.
        if self.get(project_id).map(|snapshot| snapshot.phase) != Some(ProjectClonePhase::Done) {
            let _ = self.inner.repositories.discard_clone(&destination).await;
        }
        self.clear_retention(project_id);
        self.remove(project_id);
    }

    /// `stream`: every tracked clone first, then the full list after each change. A slow
    /// subscriber only ever holds the newest list (lists are whole states).
    pub fn stream(&self) -> BoxStream<'static, Vec<ProjectCloneSnapshot>> {
        let mut receiver = self.inner.changes.subscribe();
        let first = self.list();
        receiver.mark_unchanged();
        let rest = futures::stream::unfold(receiver, |mut receiver| async move {
            receiver.changed().await.ok()?;
            let value = receiver.borrow_and_update().clone();
            Some((value, receiver))
        });
        futures::stream::once(async move { first }).chain(rest).boxed()
    }

    /// Server shutdown: interrupt every running clone (their snapshots become `cancelled`).
    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
    }
}

struct DropSignal(watch::Sender<bool>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// `describeCloneFailure`: git's own explanation when there is one.
fn describe_clone_failure(error: &SourceControlRepositoryError) -> String {
    if error.detail.trim().is_empty() {
        "The repository could not be cloned.".into()
    } else {
        error.detail.clone()
    }
}

fn dispatch_error(message: &str) -> OrchestrationDispatchCommandError {
    OrchestrationDispatchCommandError {
        tag: LitOrchestrationDispatchCommandError,
        message: message.into(),
        cause: None,
        bootstrap_thread_disposition: None,
    }
}

/// `rejectCommandsDuringClone`: a project whose clone has not landed has no files to work in,
/// so `thread.create` (and a `thread.turn.start` bootstrapping a thread) for it is refused.
/// Every dispatch transport runs this before normalizing, so no attachment copies are made
/// for a command about to be refused. `command` is the encoded client command.
pub fn reject_commands_during_clone(tracker: &ProjectCloneTracker, command: &serde_json::Value) -> Result<(), OrchestrationDispatchCommandError> {
    let project_id = match command.get("type").and_then(serde_json::Value::as_str) {
        Some("thread.create") => command.get("projectId").and_then(serde_json::Value::as_str),
        Some("thread.turn.start") => command
            .get("bootstrap")
            .and_then(|bootstrap| bootstrap.get("createThread"))
            .and_then(|create| create.get("projectId"))
            .and_then(serde_json::Value::as_str),
        _ => None,
    };
    let Some(project_id) = project_id else {
        return Ok(());
    };
    match tracker.get(&ProjectId::new(project_id)) {
        None => Ok(()),
        Some(clone) if clone.phase == ProjectClonePhase::Done => Ok(()),
        Some(clone) if clone.phase == ProjectClonePhase::Running => Err(dispatch_error("The repository is still being cloned.")),
        Some(_) => Err(dispatch_error("The repository was not cloned. Retry the clone first.")),
    }
}

/// `discardCloneForDeletedProject`: removing a project mid-clone stops the clone and drops its
/// partial checkout. `command` is the encoded normalized command.
pub async fn discard_clone_for_deleted_project(tracker: &ProjectCloneTracker, command: &serde_json::Value) {
    if command.get("type").and_then(serde_json::Value::as_str) != Some("project.delete") {
        return;
    }
    if let Some(project_id) = command.get("projectId").and_then(serde_json::Value::as_str) {
        tracker.discard(&ProjectId::new(project_id)).await;
    }
}
