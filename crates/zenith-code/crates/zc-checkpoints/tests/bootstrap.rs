//! The bootstrap cases of `server.test.ts` (`thread.turn.start` with `bootstrap`), against a
//! recording engine, a scripted git workflow and a scripted setup-script runner, plus the
//! worktree setup card and its cancel.

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::*;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_checkpoints::bootstrap::{BootstrapDeps, BootstrapDispatcher, ThreadDeletionDrain};
use zc_checkpoints::setup_script::{
    ProjectSetupScriptCompletion, ProjectSetupScriptRunnerError, SetupScriptErrorContext, SetupScriptInput, SetupScriptOperation, SetupScriptResult,
    SetupScriptRunner, SetupScriptStarted,
};
use zc_checkpoints::WorktreeSetupTracker;
use zc_contracts::{
    OrchestrationClientOrigin, OrchestrationCommand, OrchestrationDispatchCommandError, OrchestrationEvent, ThreadId, ThreadTurnStartCommand,
    WorktreeSetupPhase, WorktreeSetupSnapshot, WorktreeSetupStageId, WorktreeSetupStageStatus,
};
use zc_core::defect::Defect;
use zc_ports::contracts::*;
use zc_ports::git::{CreateWorktreeOptions, GitBranchPullRequest, GitRemoteStatusOptions, GitRunStackedActionOptions, RemoteTrackingCommit};
use zc_ports::orchestration::{ThreadReplayRange, ThreadReplayStats};
use zc_ports::{DispatchResult, EventStream, GitWorkflow, OrchestrationDispatch, TaggedError};

/// `orchestrationEngine.dispatch` recording every command, `{sequence: count}`.
#[derive(Default)]
struct RecordingEngine {
    commands: Mutex<Vec<Value>>,
    trace: Arc<Mutex<Vec<String>>>,
    /// Fails these command types (the command is still recorded).
    fail_types: Mutex<Vec<String>>,
    /// Fails the nth (1-based) `setup-script.*` activity append (not recorded).
    fail_setup_activity: Mutex<Option<usize>>,
    setup_activity_attempts: Mutex<usize>,
}

impl RecordingEngine {
    fn types(&self) -> Vec<String> {
        self.commands.lock().unwrap().iter().map(|c| c["type"].as_str().unwrap().to_owned()).collect()
    }
    fn commands(&self) -> Vec<Value> {
        self.commands.lock().unwrap().clone()
    }
}

#[async_trait]
impl OrchestrationDispatch for RecordingEngine {
    async fn dispatch(&self, command: OrchestrationCommand, _origin: Option<OrchestrationClientOrigin>) -> Result<DispatchResult, TaggedError> {
        let value = serde_json::to_value(&command).unwrap();
        let kind = value["type"].as_str().unwrap().to_owned();
        if kind == "thread.activity.append" && value["activity"]["kind"].as_str().unwrap().starts_with("setup-script.") {
            let mut attempts = self.setup_activity_attempts.lock().unwrap();
            *attempts += 1;
            if *self.fail_setup_activity.lock().unwrap() == Some(*attempts) {
                return Err(TaggedError::new("PersistenceSqlError", "failed to append setup-script.started activity"));
            }
        }
        let mut commands = self.commands.lock().unwrap();
        commands.push(value);
        self.trace.lock().unwrap().push(kind.clone());
        if self.fail_types.lock().unwrap().contains(&kind) {
            return Err(TaggedError::new("PersistenceSqlError", "thread cleanup exploded"));
        }
        Ok(DispatchResult {
            sequence: commands.len() as i64,
        })
    }
    fn subscribe_domain_events(&self) -> EventStream<OrchestrationEvent> {
        futures::stream::empty().boxed()
    }
    async fn latest_sequence(&self) -> i64 {
        0
    }
    fn read_events(&self, _: i64, _: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        futures::stream::empty().boxed()
    }
    fn read_thread_events(&self, _: ThreadReplayRange, _: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        futures::stream::empty().boxed()
    }
    async fn get_thread_replay_stats(&self, _: ThreadReplayRange, _: u32) -> Result<ThreadReplayStats, TaggedError> {
        unused()
    }
}

/// The git workflow as the bootstrap sees it.
struct ScriptedGit {
    is_repository: bool,
    has_commit: bool,
    /// `Err(message)` makes `createWorktree` fail.
    create: Result<(), String>,
    created: Mutex<Vec<Value>>,
    removed: Mutex<Vec<Value>>,
    /// Report checkout progress (the TS golden's mocked driver reports none).
    progress: std::sync::atomic::AtomicBool,
}

impl ScriptedGit {
    fn new(is_repository: bool, has_commit: bool, create: Result<(), String>) -> Arc<Self> {
        Arc::new(Self {
            is_repository,
            has_commit,
            create,
            created: Mutex::new(Vec::new()),
            removed: Mutex::new(Vec::new()),
            progress: std::sync::atomic::AtomicBool::new(true),
        })
    }
}

#[async_trait]
impl GitWorkflow for ScriptedGit {
    async fn is_repository(&self, _: &str) -> Result<bool, TaggedError> {
        Ok(self.is_repository)
    }
    async fn has_commit(&self, _: &str, _: &str) -> Result<bool, TaggedError> {
        Ok(self.has_commit)
    }
    async fn status(&self, _: VcsStatusInput) -> Result<VcsStatusResult, TaggedError> {
        unused()
    }
    async fn local_status(&self, _: VcsStatusInput) -> Result<VcsStatusLocalResult, TaggedError> {
        unused()
    }
    async fn remote_status(&self, _: VcsStatusInput, _: GitRemoteStatusOptions) -> Result<Option<VcsStatusRemoteResult>, TaggedError> {
        unused()
    }
    async fn branch_pull_request(&self, _: &str, _: &str, _: bool) -> Result<Option<GitBranchPullRequest>, TaggedError> {
        unused()
    }
    async fn invalidate_local_status(&self, _: &str) {}
    async fn invalidate_remote_status(&self, _: &str) {}
    async fn invalidate_status(&self, _: &str) {}
    async fn pull_current_branch(&self, _: &str) -> Result<VcsPullResult, TaggedError> {
        unused()
    }
    async fn run_stacked_action(&self, _: GitRunStackedActionInput, _: GitRunStackedActionOptions) -> Result<GitRunStackedActionResult, TaggedError> {
        unused()
    }
    async fn resolve_pull_request(&self, _: GitPullRequestRefInput) -> Result<GitResolvePullRequestResult, TaggedError> {
        unused()
    }
    async fn prepare_pull_request_thread(&self, _: GitPreparePullRequestThreadInput) -> Result<GitPreparePullRequestThreadResult, TaggedError> {
        unused()
    }
    async fn list_refs(&self, _: VcsListRefsInput) -> Result<VcsListRefsResult, TaggedError> {
        unused()
    }
    async fn create_worktree(&self, input: VcsCreateWorktreeInput, options: CreateWorktreeOptions) -> Result<VcsCreateWorktreeResult, TaggedError> {
        self.created.lock().unwrap().push(input.0.clone());
        if let Err(message) = &self.create {
            return Err(TaggedError::new("GitCommandError", message.clone()));
        }
        if !self.progress.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(VcsCreateWorktreeResult(
                json!({"worktree": {"refName": "t3code/bootstrap-refName", "path": "/tmp/bootstrap-worktree"}}),
            ));
        }
        if let Some(claimed) = &options.progress.on_worktree_claimed {
            claimed("/tmp/bootstrap-worktree");
        }
        if let Some(progress) = &options.progress.on_checkout_progress {
            progress(zc_ports::git::CheckoutProgress {
                percent: 50.0,
                completed: 1200,
                total: 2400,
            });
        }
        Ok(VcsCreateWorktreeResult(
            json!({"worktree": {"refName": "t3code/bootstrap-refName", "path": "/tmp/bootstrap-worktree"}}),
        ))
    }
    async fn fetch_remote(&self, _: &str, _: &str, _: Option<&str>) -> Result<(), TaggedError> {
        Ok(())
    }
    async fn remote_exists(&self, _: &str, _: &str) -> Result<bool, TaggedError> {
        Ok(false)
    }
    async fn remote_branch_exists(&self, _: &str, _: &str, _: &str) -> Result<bool, TaggedError> {
        Ok(false)
    }
    async fn resolve_remote_tracking_commit(&self, _: &str, _: &str, _: &str) -> Result<RemoteTrackingCommit, TaggedError> {
        unused()
    }
    async fn remove_worktree(&self, input: VcsRemoveWorktreeInput) -> Result<(), TaggedError> {
        self.removed.lock().unwrap().push(input.0);
        Ok(())
    }
    async fn prune_worktrees(&self, _: &str) -> Result<(), TaggedError> {
        Ok(())
    }
    async fn create_ref(&self, _: VcsCreateRefInput) -> Result<VcsCreateRefResult, TaggedError> {
        unused()
    }
    async fn switch_ref(&self, _: VcsSwitchRefInput) -> Result<VcsSwitchRefResult, TaggedError> {
        unused()
    }
    async fn rename_branch(&self, _: &str, _: &str, _: &str) -> Result<String, TaggedError> {
        unused()
    }
}

/// `runForThread` scripted: fails, starts a script (optionally waiting on an exit gate), or
/// reports no script.
enum Setup {
    None,
    Fails,
    Started { r#async: bool, exit: Option<Arc<tokio::sync::Semaphore>> },
}

struct ScriptedSetup(Setup);

#[async_trait]
impl SetupScriptRunner for ScriptedSetup {
    async fn run_for_thread(&self, input: SetupScriptInput) -> Result<SetupScriptResult, ProjectSetupScriptRunnerError> {
        match &self.0 {
            Setup::None => Ok(SetupScriptResult::NoScript),
            Setup::Fails => Err(ProjectSetupScriptRunnerError::Operation {
                context: SetupScriptErrorContext {
                    thread_id: input.thread_id,
                    project_id: None,
                    project_cwd: None,
                    worktree_path: input.worktree_path,
                },
                operation: SetupScriptOperation::OpenTerminal,
                cause: Defect(json!({"message": "pty unavailable"})),
            }),
            Setup::Started { r#async, exit } => {
                let completion = exit.clone().map(|gate| {
                    Box::pin(async move {
                        let _ = gate.acquire().await;
                        ProjectSetupScriptCompletion {
                            exit_code: Some(0),
                            duration_ms: 1,
                        }
                    }) as zc_checkpoints::setup_script::SetupCompletion
                });
                Ok(SetupScriptResult::Started(SetupScriptStarted {
                    script_id: "setup".into(),
                    script_name: "Setup".into(),
                    script_command: "npm install".into(),
                    terminal_id: "setup-setup".into(),
                    cwd: "/tmp/bootstrap-worktree".into(),
                    r#async: *r#async,
                    completion,
                }))
            }
        }
    }
}

struct TracingDrain(Arc<Mutex<Vec<String>>>);

#[async_trait]
impl ThreadDeletionDrain for TracingDrain {
    async fn drain_through(&self, sequence: i64) {
        self.0.lock().unwrap().push(format!("drain:{sequence}"));
    }
}

struct Fixture {
    engine: Arc<RecordingEngine>,
    git: Arc<ScriptedGit>,
    terminals: Arc<FakeTerminals>,
    tracker: WorktreeSetupTracker,
    dispatcher: BootstrapDispatcher,
}

async fn fixture(git: Arc<ScriptedGit>, setup: Setup) -> Fixture {
    let engine = Arc::new(RecordingEngine::default());
    let projections = EngineProjections {
        engine: common::engine().await,
    };
    let terminals = Arc::new(FakeTerminals::default());
    let tracker = WorktreeSetupTracker::new();
    let dispatcher = BootstrapDispatcher::new(
        BootstrapDeps {
            engine: engine.clone(),
            projections: Arc::new(projections),
            git: git.clone(),
            vcs_status: FakeVcsStatus::new(None),
            settings: MemorySettings::new(json!({})),
            terminals: terminals.clone(),
            setup_scripts: Arc::new(ScriptedSetup(setup)),
            tracker: tracker.clone(),
            thread_deletion: Arc::new(TracingDrain(engine.trace.clone())),
        },
        None,
    );
    Fixture {
        engine,
        git,
        terminals,
        tracker,
        dispatcher,
    }
}

fn turn_start(thread_id: &str, branch: Option<&str>, prepare: bool, run_setup_script: bool) -> ThreadTurnStartCommand {
    let mut bootstrap = json!({
        "createThread": {"projectId": "project-1", "title": "Bootstrap Thread", "modelSelection": model_selection(), "runtimeMode": "full-access",
                         "interactionMode": "default", "branch": branch, "worktreePath": null, "createdAt": NOW},
        "runSetupScript": run_setup_script,
    });
    if prepare {
        bootstrap["prepareWorktree"] = json!({"projectCwd": "/tmp/project", "baseBranch": "main", "branch": "t3code/bootstrap-refName"});
    }
    decode(json!({
        "type": "thread.turn.start", "commandId": format!("cmd-{thread_id}"), "threadId": thread_id,
        "message": {"messageId": format!("msg-{thread_id}"), "role": "user", "text": "hello", "attachments": []},
        "modelSelection": model_selection(), "runtimeMode": "full-access", "interactionMode": "default", "bootstrap": bootstrap, "createdAt": NOW,
    }))
}

#[tokio::test(flavor = "multi_thread")]
async fn falls_back_to_the_project_checkout_when_worktree_mode_targets_a_non_repository() {
    for (is_repository, has_commit, branch) in [(false, true, None), (true, false, Some("main"))] {
        let f = fixture(ScriptedGit::new(is_repository, has_commit, Ok(())), Setup::None).await;
        let response = f
            .dispatcher
            .dispatch_bootstrap_turn_start(turn_start("thread-bootstrap-non-repo", branch, true, true))
            .await
            .unwrap();
        assert_eq!(response.sequence, 4);
        assert!(f.git.created.lock().unwrap().is_empty());
        assert_eq!(
            f.engine.types(),
            vec![
                "thread.create",
                "thread.message.user.append",
                "thread.activity.append",
                "thread.turn.start",
                "thread.activity.append"
            ]
        );
        let commands = f.engine.commands();
        assert!(commands[3].get("bootstrap").is_none());
        // The card says the thread runs in the project checkout.
        let card = &commands[4]["activity"]["payload"];
        assert_eq!(card["phase"], json!("done"));
        for stage in card["stages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| ["fetch", "checkout", "submodules"].contains(&s["id"].as_str().unwrap()))
        {
            assert_eq!(stage["status"], json!("skipped"));
            assert_eq!(stage["detail"], json!("using project checkout"));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn records_setup_script_failures_without_aborting_bootstrap_turn_start() {
    let f = fixture(ScriptedGit::new(true, true, Ok(())), Setup::Fails).await;
    let response = f
        .dispatcher
        .dispatch_bootstrap_turn_start(turn_start("thread-bootstrap-setup-failure", Some("main"), true, true))
        .await
        .unwrap();
    assert_eq!(response.sequence, 7);
    assert_eq!(
        f.engine.types(),
        vec![
            "thread.create",
            "thread.message.user.append",
            "thread.activity.append",
            "thread.session.set",
            "thread.meta.update",
            "thread.activity.append",
            "thread.turn.start",
            "thread.activity.append",
        ]
    );
    let commands = f.engine.commands();
    let failure = commands
        .iter()
        .find(|c| c["type"] == "thread.activity.append" && c["activity"]["kind"] != "worktree-setup")
        .unwrap();
    assert_eq!(failure["activity"]["kind"], json!("setup-script.failed"));
    assert_eq!(
        failure["activity"]["payload"],
        json!({"detail": "pty unavailable", "worktreePath": "/tmp/bootstrap-worktree"})
    );
    assert!(!f.engine.types().contains(&"thread.delete".to_owned()));
    // Session preparing, meta update to the new worktree, checkout progress on the card.
    assert_eq!(commands[3]["session"]["status"], json!("starting"));
    assert_eq!(commands[3]["session"]["providerInstanceId"], json!("codex"));
    assert_eq!(commands[4]["branch"], json!("t3code/bootstrap-refName"));
    assert_eq!(commands[4]["worktreePath"], json!("/tmp/bootstrap-worktree"));
    let done = &commands[7]["activity"];
    assert_eq!(done["id"], json!("worktree-setup:thread-bootstrap-setup-failure"));
    assert_eq!(done["tone"], json!("error"));
    let checkout = done["payload"]["stages"].as_array().unwrap().iter().find(|s| s["id"] == "checkout").unwrap();
    assert_eq!(checkout["status"], json!("done"));
    assert_eq!(checkout["percent"], json!(100));
    assert_eq!(checkout["detail"], json!("2,400 files"));
    assert_eq!(f.git.created.lock().unwrap()[0]["newRefName"], json!("t3code/bootstrap-refName"));
}

#[tokio::test(flavor = "multi_thread")]
async fn does_not_misattribute_setup_activity_dispatch_failures_as_setup_launch_failures() {
    let f = fixture(ScriptedGit::new(true, true, Ok(())), Setup::Started { r#async: true, exit: None }).await;
    *f.engine.fail_setup_activity.lock().unwrap() = Some(2);
    let response = f
        .dispatcher
        .dispatch_bootstrap_turn_start(turn_start("thread-bootstrap-setup-activity-failure", Some("main"), true, true))
        .await
        .unwrap();
    assert_eq!(response.sequence, 7);
    let kinds: Vec<Value> = f
        .engine
        .commands()
        .into_iter()
        .filter(|c| c["type"] == "thread.activity.append")
        .map(|c| c["activity"]["kind"].clone())
        .collect();
    assert_eq!(kinds, vec![json!("worktree-setup"), json!("setup-script.requested"), json!("worktree-setup")]);
    assert!(!f.engine.types().contains(&"thread.delete".to_owned()));
}

async fn snapshot_where(tracker: &WorktreeSetupTracker, thread_id: &str, predicate: impl Fn(&WorktreeSetupSnapshot) -> bool) -> WorktreeSetupSnapshot {
    let mut stream = tracker.stream(&ThreadId::new(thread_id));
    loop {
        let next = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
            .await
            .expect("a snapshot in time");
        if let Some(Some(snapshot)) = next {
            if predicate(&snapshot) {
                return snapshot;
            }
        }
    }
}

fn stage(snapshot: &WorktreeSetupSnapshot, id: WorktreeSetupStageId) -> WorktreeSetupStageStatus {
    snapshot.stages.iter().find(|s| s.id == id).unwrap().status
}

#[tokio::test(flavor = "multi_thread")]
async fn async_setup_scripts_let_the_turn_start_before_the_script_exits() {
    let exit = Arc::new(tokio::sync::Semaphore::new(0));
    let f = fixture(
        ScriptedGit::new(true, true, Ok(())),
        Setup::Started {
            r#async: true,
            exit: Some(exit.clone()),
        },
    )
    .await;
    let response = f
        .dispatcher
        .dispatch_bootstrap_turn_start(turn_start("thread-bootstrap-async-setup", Some("main"), true, true))
        .await;
    assert!(response.is_ok());
    let started = snapshot_where(&f.tracker, "thread-bootstrap-async-setup", |s| {
        stage(s, WorktreeSetupStageId::Agent) == WorktreeSetupStageStatus::Done
    })
    .await;
    assert!(f.engine.types().contains(&"thread.turn.start".to_owned()));
    assert_eq!(started.phase, WorktreeSetupPhase::Running);
    assert_eq!(stage(&started, WorktreeSetupStageId::SetupScript), WorktreeSetupStageStatus::Running);
    assert_eq!(started.setup_script.as_ref().unwrap().terminal_id, "setup-setup");
    exit.add_permits(1);
    let settled = snapshot_where(&f.tracker, "thread-bootstrap-async-setup", |s| s.phase != WorktreeSetupPhase::Running).await;
    assert_eq!(settled.phase, WorktreeSetupPhase::Done);
    assert_eq!(stage(&settled, WorktreeSetupStageId::SetupScript), WorktreeSetupStageStatus::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_setup_scripts_hold_the_turn_and_survive_the_caller_going_away() {
    let exit = Arc::new(tokio::sync::Semaphore::new(0));
    let f = fixture(
        ScriptedGit::new(true, true, Ok(())),
        Setup::Started {
            r#async: false,
            exit: Some(exit.clone()),
        },
    )
    .await;
    let dispatcher = f.dispatcher.clone();
    let call = tokio::spawn(async move {
        dispatcher
            .dispatch_bootstrap_turn_start(turn_start("thread-bootstrap-sync-setup", Some("main"), true, true))
            .await
    });
    let running = snapshot_where(&f.tracker, "thread-bootstrap-sync-setup", |s| {
        stage(s, WorktreeSetupStageId::SetupScript) == WorktreeSetupStageStatus::Running
    })
    .await;
    assert_eq!(stage(&running, WorktreeSetupStageId::Agent), WorktreeSetupStageStatus::Pending);
    assert!(!f.engine.types().contains(&"thread.turn.start".to_owned()));
    // The caller goes away (a reload): the bootstrap belongs to the server and finishes.
    call.abort();
    exit.add_permits(1);
    snapshot_where(&f.tracker, "thread-bootstrap-sync-setup", |s| {
        stage(s, WorktreeSetupStageId::Agent) == WorktreeSetupStageStatus::Done
    })
    .await;
    assert!(f.engine.types().contains(&"thread.turn.start".to_owned()));
    let settled = snapshot_where(&f.tracker, "thread-bootstrap-sync-setup", |s| s.phase != WorktreeSetupPhase::Running).await;
    assert_eq!(settled.phase, WorktreeSetupPhase::Done);
    assert_eq!(stage(&settled, WorktreeSetupStageId::SetupScript), WorktreeSetupStageStatus::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn cancelling_worktree_setup_publishes_its_outcome_and_cleans_up_the_thread() {
    let exit = Arc::new(tokio::sync::Semaphore::new(0));
    let f = fixture(
        ScriptedGit::new(true, true, Ok(())),
        Setup::Started {
            r#async: false,
            exit: Some(exit.clone()),
        },
    )
    .await;
    let dispatcher = f.dispatcher.clone();
    let call = tokio::spawn(async move {
        dispatcher
            .dispatch_bootstrap_turn_start(turn_start("thread-bootstrap-cancel", Some("main"), true, true))
            .await
    });
    snapshot_where(&f.tracker, "thread-bootstrap-cancel", |s| {
        stage(s, WorktreeSetupStageId::SetupScript) == WorktreeSetupStageStatus::Running
    })
    .await;
    assert!(f.tracker.cancel(&ThreadId::new("thread-bootstrap-cancel")).await);
    // Rolled back before cancel returned.
    assert!(f.engine.types().contains(&"thread.delete".to_owned()));
    let outcome = f
        .engine
        .commands()
        .into_iter()
        .rev()
        .find(|c| c["type"] == "thread.activity.append" && c["activity"]["kind"] == "worktree-setup")
        .unwrap();
    assert_eq!(outcome["activity"]["payload"]["phase"], json!("cancelled"));
    let error: OrchestrationDispatchCommandError = call.await.unwrap().unwrap_err();
    assert_eq!(error.message, "Worktree setup cancelled.");
    assert_eq!(serde_json::to_value(error.bootstrap_thread_disposition).unwrap(), json!("deleted"));
    assert!(!f.engine.types().contains(&"thread.turn.start".to_owned()));
    // The setup terminal is closed and the created worktree removed.
    assert_eq!(
        f.terminals.closes.lock().unwrap()[0],
        json!({"threadId": "thread-bootstrap-cancel", "terminalId": "setup-setup", "deleteHistory": true})
    );
    assert_eq!(
        f.git.removed.lock().unwrap()[0],
        json!({"cwd": "/tmp/project", "path": "/tmp/bootstrap-worktree", "force": true})
    );
    assert!(!f.tracker.cancel(&ThreadId::new("thread-bootstrap-cancel")).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn cleans_up_created_bootstrap_threads_when_worktree_creation_fails() {
    let f = fixture(ScriptedGit::new(true, true, Err("worktree exploded".into())), Setup::None).await;
    let error = f
        .dispatcher
        .dispatch_bootstrap_turn_start(turn_start("thread-bootstrap-defect", Some("main"), true, false))
        .await
        .unwrap_err();
    assert!(error.message.contains("worktree exploded"));
    assert_eq!(serde_json::to_value(error.bootstrap_thread_disposition).unwrap(), json!("deleted"));
    assert_eq!(
        f.engine.types(),
        vec![
            "thread.create",
            "thread.message.user.append",
            "thread.activity.append",
            "thread.session.set",
            "thread.activity.append",
            "thread.delete"
        ]
    );
    let failed = &f.engine.commands()[4]["activity"];
    assert_eq!(failed["payload"]["phase"], json!("failed"));
    assert_eq!(failed["summary"], json!("Worktree setup failed"));
}

#[tokio::test(flavor = "multi_thread")]
async fn does_not_report_a_deleted_bootstrap_thread_when_cleanup_fails() {
    let f = fixture(ScriptedGit::new(true, true, Err("worktree exploded".into())), Setup::None).await;
    f.engine.fail_types.lock().unwrap().push("thread.delete".into());
    let error = f
        .dispatcher
        .dispatch_bootstrap_turn_start(turn_start("thread-bootstrap-cleanup-defect", Some("main"), true, false))
        .await
        .unwrap_err();
    assert!(error.message.contains("worktree exploded"));
    assert!(error.bootstrap_thread_disposition.is_none());
    assert_eq!(
        f.engine.types(),
        vec![
            "thread.create",
            "thread.message.user.append",
            "thread.activity.append",
            "thread.session.set",
            "thread.activity.append",
            "thread.delete",
            "thread.session.set",
        ]
    );
    let failed = &f.engine.commands()[6]["session"];
    assert_eq!(failed["status"], json!("error"));
    assert!(failed["lastError"].as_str().unwrap().contains("worktree exploded"));
}

#[tokio::test(flavor = "multi_thread")]
async fn drains_deletion_cleanup_through_the_re_created_thread_event() {
    let f = fixture(ScriptedGit::new(true, true, Ok(())), Setup::None).await;
    f.dispatcher
        .dispatch_bootstrap_turn_start(turn_start("thread-retry-after-delete", None, false, false))
        .await
        .unwrap();
    assert_eq!(
        *f.engine.trace.lock().unwrap(),
        vec!["thread.create", "drain:1", "thread.message.user.append", "thread.turn.start"]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn require_worktree_fails_without_creating_the_thread() {
    let f = fixture(ScriptedGit::new(false, true, Ok(())), Setup::None).await;
    let mut command = turn_start("thread-require-worktree", None, true, false);
    command.bootstrap.as_mut().unwrap().prepare_worktree.as_mut().unwrap().require_worktree = Some(true);
    let error = f.dispatcher.dispatch_bootstrap_turn_start(command).await.unwrap_err();
    assert_eq!(error.message, "A separate worktree requires a Git repository and a base branch with a commit.");
    assert_eq!(serde_json::to_value(error.bootstrap_thread_disposition).unwrap(), json!("not-created"));
    // Only the (best-effort) failed-setup record: the thread was never created.
    assert_eq!(f.engine.types(), vec!["thread.activity.append"]);
}

#[path = "bootstrap/golden.rs"]
mod golden;
