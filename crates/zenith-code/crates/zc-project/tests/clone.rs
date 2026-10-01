//! Port of `project/ProjectCloneTracker.test.ts` (the progress parser's cases live with
//! zc-sourcecontrol's `clone_progress`, which `gitCloneProgress.ts` became), plus the dispatch
//! guards.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::json;
use zc_contracts::{
    LitOrchestrationDispatchCommandError, OrchestrationDispatchCommandError, ProjectClonePhase, ProjectCloneStage, ProjectCloneStartInput, ProjectId,
    SourceControlCloneRepositoryInput, SourceControlCloneRepositoryResult, SourceControlProviderKind,
};
use zc_project::clone::{discard_clone_for_deleted_project, reject_commands_during_clone, CloneRepositories, ClonedProject, ProjectCloneHooks};
use zc_project::ProjectCloneTracker;
use zc_sourcecontrol::clone_progress::GitCloneProgressLine;
use zc_sourcecontrol::{SourceControlCloneOptions, SourceControlPreparedClone, SourceControlRepositoryError};

const DESTINATION: &str = "/workspace/sample-app";

fn project_id() -> ProjectId {
    ProjectId::new("project-1")
}

fn start_input() -> ProjectCloneStartInput {
    ProjectCloneStartInput {
        project_id: project_id(),
        title: "sample-app".into(),
        created_at: "2026-01-01T00:00:00.000Z".into(),
        provider: None,
        repository: None,
        remote_url: Some("git@github.com:octo-org/sample-app.git".into()),
        destination_path: DESTINATION.into(),
        protocol: None,
    }
}

type CloneFn = Arc<
    dyn Fn(
            SourceControlCloneRepositoryInput,
            SourceControlCloneOptions,
        ) -> futures::future::BoxFuture<'static, Result<SourceControlCloneRepositoryResult, SourceControlRepositoryError>>
        + Send
        + Sync,
>;
type PrepareFn = Arc<dyn Fn(&SourceControlCloneRepositoryInput) -> Result<SourceControlPreparedClone, SourceControlRepositoryError> + Send + Sync>;

struct Repositories {
    prepare: PrepareFn,
    clone: CloneFn,
    discarded: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl CloneRepositories for Repositories {
    async fn prepare_clone(&self, input: &SourceControlCloneRepositoryInput) -> Result<SourceControlPreparedClone, SourceControlRepositoryError> {
        (self.prepare)(input)
    }
    async fn clone_repository(
        &self,
        input: &SourceControlCloneRepositoryInput,
        options: &SourceControlCloneOptions,
    ) -> Result<SourceControlCloneRepositoryResult, SourceControlRepositoryError> {
        (self.clone)(input.clone(), options.clone()).await
    }
    async fn discard_clone(&self, destination: &str) -> Result<(), SourceControlRepositoryError> {
        self.discarded.lock().unwrap().push(destination.to_owned());
        Ok(())
    }
}

fn plain_prepare() -> PrepareFn {
    Arc::new(|input| {
        Ok(SourceControlPreparedClone {
            destination_path: input.destination_path.clone(),
            remote_url: input.remote_url.clone().unwrap_or_default(),
            clone_url: input.remote_url.clone().unwrap_or_default(),
            repository: None,
        })
    })
}

fn succeed() -> CloneFn {
    Arc::new(|input, _| {
        Box::pin(async move {
            Ok(SourceControlCloneRepositoryResult {
                cwd: input.destination_path.clone(),
                remote_url: input.remote_url.clone().unwrap_or_default(),
                repository: None,
            })
        })
    })
}

fn never() -> CloneFn {
    Arc::new(|_, _| Box::pin(futures::future::pending()))
}

#[derive(Default)]
struct Hooks {
    created: Mutex<Vec<(ProjectId, String)>>,
    cloned: Mutex<Vec<ProjectId>>,
    fail_create: Option<String>,
    cloned_gate: Option<Arc<tokio::sync::Notify>>,
}

#[async_trait]
impl ProjectCloneHooks for Hooks {
    async fn create_project(&self, project: ClonedProject) -> Result<(), OrchestrationDispatchCommandError> {
        if let Some(message) = &self.fail_create {
            return Err(OrchestrationDispatchCommandError {
                tag: LitOrchestrationDispatchCommandError,
                message: message.clone(),
                cause: None,
                bootstrap_thread_disposition: None,
            });
        }
        self.created.lock().unwrap().push((project.project_id, project.workspace_root));
        Ok(())
    }
    async fn on_cloned(&self, project_id: &ProjectId, _workspace_root: &str) {
        if let Some(gate) = &self.cloned_gate {
            gate.notified().await;
        }
        self.cloned.lock().unwrap().push(project_id.clone());
    }
}

fn harness(clone: CloneFn) -> (ProjectCloneTracker, Arc<Mutex<Vec<String>>>) {
    harness_with(plain_prepare(), clone)
}

fn harness_with(prepare: PrepareFn, clone: CloneFn) -> (ProjectCloneTracker, Arc<Mutex<Vec<String>>>) {
    let discarded: Arc<Mutex<Vec<String>>> = Arc::default();
    let tracker = ProjectCloneTracker::new(Arc::new(Repositories {
        prepare,
        clone,
        discarded: discarded.clone(),
    }));
    (tracker, discarded)
}

/// Let spawned tasks run (`Effect.yieldNow`).
async fn settle() {
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn creates_the_project_first_and_reports_the_clone_through_the_stream() {
    let release = Arc::new(tokio::sync::Notify::new());
    let gate = release.clone();
    let clone: CloneFn = Arc::new(move |input, options| {
        let gate = gate.clone();
        Box::pin(async move {
            if let Some(progress) = &options.on_progress {
                progress(GitCloneProgressLine {
                    stage: ProjectCloneStage::Receiving,
                    percent: Some(40),
                    detail: Some("1 MiB".into()),
                });
            }
            gate.notified().await;
            Ok(SourceControlCloneRepositoryResult {
                cwd: input.destination_path,
                remote_url: input.remote_url.unwrap_or_default(),
                repository: None,
            })
        })
    });
    let (tracker, _) = harness(clone);
    let mut stream = tracker.stream();
    assert_eq!(stream.next().await.unwrap(), Vec::new());
    let collected = tokio::spawn(async move {
        let mut lists = Vec::new();
        while let Some(list) = stream.next().await {
            let done = list.first().is_some_and(|clone| clone.phase == ProjectClonePhase::Done);
            lists.push(list);
            if done {
                break;
            }
        }
        lists
    });
    let hooks = Arc::new(Hooks::default());
    let result = tracker.start(start_input(), hooks.clone()).await.unwrap();
    assert_eq!(result.cwd, DESTINATION);
    // The project exists before git runs, so the draft can open right away.
    assert_eq!(*hooks.created.lock().unwrap(), [(project_id(), DESTINATION.to_owned())]);
    settle().await;
    let running = tracker.get(&project_id()).unwrap();
    assert_eq!(
        (running.phase, running.stage, running.percent),
        (ProjectClonePhase::Running, ProjectCloneStage::Receiving, Some(40))
    );
    assert_eq!(running.detail.as_deref(), Some("1 MiB"));

    release.notify_one();
    let lists = collected.await.unwrap();
    let last = lists.last().unwrap()[0].clone();
    assert_eq!((last.phase, last.percent), (ProjectClonePhase::Done, Some(100)));
    assert!(last.ended_at.is_some());
    settle().await;
    assert_eq!(*hooks.cloned.lock().unwrap(), [project_id()]);
    // Done clones drop out after the grace window.
    assert!(tracker.get(&project_id()).is_some());
    tokio::time::advance(Duration::from_secs(31)).await;
    settle().await;
    assert!(tracker.get(&project_id()).is_none());
}

#[tokio::test]
async fn keeps_a_failed_clone_with_git_explanation_and_retries_it() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let clone: CloneFn = Arc::new(move |input, _| {
        let attempt = counter.fetch_add(1, Ordering::SeqCst) + 1;
        Box::pin(async move {
            if attempt == 1 {
                return Err(SourceControlRepositoryError::new(
                    "cloneRepository",
                    SourceControlProviderKind::Unknown,
                    "fatal: repository not found",
                ));
            }
            Ok(SourceControlCloneRepositoryResult {
                cwd: input.destination_path,
                remote_url: input.remote_url.unwrap_or_default(),
                repository: None,
            })
        })
    });
    let (tracker, discarded) = harness(clone);
    tracker.start(start_input(), Arc::new(Hooks::default())).await.unwrap();
    settle().await;
    let failed = tracker.get(&project_id()).unwrap();
    assert_eq!(failed.phase, ProjectClonePhase::Failed);
    assert_eq!(failed.error.as_deref(), Some("fatal: repository not found"));
    assert!(tracker.retry(&project_id()).await.unwrap());
    // The partial checkout is cleared so git sees an empty destination.
    assert_eq!(*discarded.lock().unwrap(), [DESTINATION]);
    settle().await;
    assert_eq!(tracker.get(&project_id()).unwrap().phase, ProjectClonePhase::Done);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn cancel_interrupts_the_clone_and_removes_the_partial_checkout() {
    let (tracker, discarded) = harness(never());
    tracker.start(start_input(), Arc::new(Hooks::default())).await.unwrap();
    settle().await;
    assert!(tracker.cancel(&project_id()).await);
    assert_eq!(tracker.get(&project_id()).unwrap().phase, ProjectClonePhase::Cancelled);
    assert_eq!(*discarded.lock().unwrap(), [DESTINATION]);
    // Nothing left to cancel; retry brings it back.
    assert!(!tracker.cancel(&project_id()).await);
    assert!(tracker.retry(&project_id()).await.unwrap());
    assert_eq!(tracker.get(&project_id()).unwrap().phase, ProjectClonePhase::Running);
}

#[tokio::test]
async fn a_cancel_that_lands_after_git_finished_keeps_the_checkout() {
    let gate = Arc::new(tokio::sync::Notify::new());
    let (tracker, discarded) = harness(succeed());
    let hooks = Arc::new(Hooks {
        cloned_gate: Some(gate.clone()),
        ..Hooks::default()
    });
    tracker.start(start_input(), hooks).await.unwrap();
    settle().await;
    assert_eq!(tracker.get(&project_id()).unwrap().phase, ProjectClonePhase::Done);
    assert!(!tracker.cancel(&project_id()).await);
    assert!(discarded.lock().unwrap().is_empty());
    gate.notify_one();
}

#[tokio::test]
async fn hands_git_the_credential_bearing_url_while_snapshots_carry_the_redacted_one() {
    let urls: Arc<Mutex<Vec<String>>> = Arc::default();
    let seen = urls.clone();
    let clone: CloneFn = Arc::new(move |input, _| {
        seen.lock().unwrap().push(input.remote_url.clone().unwrap_or_default());
        Box::pin(async move {
            Ok(SourceControlCloneRepositoryResult {
                cwd: input.destination_path,
                remote_url: String::new(),
                repository: None,
            })
        })
    });
    let prepare: PrepareFn = Arc::new(|input| {
        Ok(SourceControlPreparedClone {
            destination_path: input.destination_path.clone(),
            remote_url: "https://github.com/octo-org/sample-app.git".into(),
            clone_url: "https://user:s3cret@github.com/octo-org/sample-app.git".into(),
            repository: None,
        })
    });
    let (tracker, _) = harness_with(prepare, clone);
    let result = tracker.start(start_input(), Arc::new(Hooks::default())).await.unwrap();
    settle().await;
    assert_eq!(result.remote_url, "https://github.com/octo-org/sample-app.git");
    assert_eq!(tracker.get(&project_id()).unwrap().remote_url, "https://github.com/octo-org/sample-app.git");
    assert_eq!(*urls.lock().unwrap(), ["https://user:s3cret@github.com/octo-org/sample-app.git"]);
}

#[tokio::test]
async fn discard_forgets_a_project_clone_when_the_project_is_deleted() {
    let (tracker, discarded) = harness(never());
    tracker.start(start_input(), Arc::new(Hooks::default())).await.unwrap();
    settle().await;
    tracker.discard(&project_id()).await;
    assert!(tracker.get(&project_id()).is_none());
    assert_eq!(*discarded.lock().unwrap(), [DESTINATION]);
}

#[tokio::test]
async fn releases_the_claim_when_project_creation_fails() {
    let (tracker, _) = harness(succeed());
    let failing = Arc::new(Hooks {
        fail_create: Some("workspace root exists".into()),
        ..Hooks::default()
    });
    let error = tracker.start(start_input(), failing).await.unwrap_err();
    assert!(error.message().contains("workspace root exists"));
    assert!(tracker.get(&project_id()).is_none());
    // The destination is free again for a corrected attempt.
    tracker.start(start_input(), Arc::new(Hooks::default())).await.unwrap();
    settle().await;
    assert_eq!(tracker.get(&project_id()).unwrap().phase, ProjectClonePhase::Done);
}

#[tokio::test]
async fn does_not_create_a_project_when_the_clone_cannot_be_prepared() {
    let prepare: PrepareFn = Arc::new(|_| {
        Err(SourceControlRepositoryError::new(
            "cloneRepository",
            SourceControlProviderKind::Unknown,
            "Destination path already exists and is not empty.",
        ))
    });
    let (tracker, _) = harness_with(prepare, succeed());
    let hooks = Arc::new(Hooks::default());
    let error = tracker.start(start_input(), hooks.clone()).await.unwrap_err();
    assert!(error.message().contains("not empty"));
    assert!(hooks.created.lock().unwrap().is_empty());
    assert!(tracker.get(&project_id()).is_none());
}

#[tokio::test]
async fn refuses_a_second_clone_into_the_same_destination() {
    let (tracker, _) = harness(never());
    tracker.start(start_input(), Arc::new(Hooks::default())).await.unwrap();
    let mut other = start_input();
    other.project_id = ProjectId::new("project-2");
    let error = tracker.start(other, Arc::new(Hooks::default())).await.unwrap_err();
    assert_eq!(
        error.to_wire(),
        json!({"_tag": "SourceControlRepositoryError", "provider": "unknown", "operation": "cloneRepository", "detail": "A clone into this destination is already in progress."})
    );
}

#[tokio::test]
async fn guards_dispatch_while_a_clone_has_not_landed() {
    let (tracker, _) = harness(never());
    let create = json!({"type": "thread.create", "projectId": "project-1"});
    let bootstrap = json!({"type": "thread.turn.start", "bootstrap": {"createThread": {"projectId": "project-1"}}});
    let other = json!({"type": "thread.create", "projectId": "project-2"});
    assert!(reject_commands_during_clone(&tracker, &create).is_ok());
    tracker.start(start_input(), Arc::new(Hooks::default())).await.unwrap();
    settle().await;
    assert_eq!(
        reject_commands_during_clone(&tracker, &create).unwrap_err().message,
        "The repository is still being cloned."
    );
    assert!(reject_commands_during_clone(&tracker, &bootstrap).is_err());
    assert!(reject_commands_during_clone(&tracker, &other).is_ok());
    tracker.cancel(&project_id()).await;
    assert_eq!(
        reject_commands_during_clone(&tracker, &create).unwrap_err().message,
        "The repository was not cloned. Retry the clone first."
    );
    discard_clone_for_deleted_project(&tracker, &json!({"type": "project.delete", "projectId": "project-1"})).await;
    assert!(tracker.get(&project_id()).is_none());
    assert!(reject_commands_during_clone(&tracker, &create).is_ok());
}
