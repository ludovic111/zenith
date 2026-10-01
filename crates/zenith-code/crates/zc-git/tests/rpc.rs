//! The `git.*` WS RPC handlers (`ws.ts`) and `linkCreatedPullRequest.test.ts` (the effect
//! tests; the `createdPullRequestKey` cases are unit tests in `src/link.rs`), against a fake
//! orchestration engine and fake projection reads, on real repositories with the fake `gh`.

#![allow(clippy::result_large_err)]

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{
    MessageId, OrchestrationCommand, OrchestrationProject, OrchestrationProjectShell, OrchestrationReadModel, OrchestrationSearchThreadsInput,
    OrchestrationSearchThreadsResult, OrchestrationShellSnapshot, OrchestrationThread, OrchestrationThreadActivity, OrchestrationThreadDetailSnapshot,
    OrchestrationThreadDetailWindow, OrchestrationThreadShell, ProjectId, ThreadId,
};
use zc_git::link::link_created_pull_request;
use zc_git::types::PrStep;
use zc_ports::contracts::*;
use zc_ports::orchestration::{
    DeletedWorktreeThread, FullThreadDiffContext, ImportedAgentSessionSource, ReplayStats, SnapshotCounts, ThreadCheckpointContext, ThreadDetailQuery,
    ThreadPullRequests, ThreadReplayRange, ThreadReplayStats, ThreadRuntimeContext, TurnStartMessage,
};
use zc_ports::{DispatchResult, EventStream, OrchestrationDispatch, ProjectionReads, TaggedError};

const THREAD_ID: &str = "thread-1";
const COMMAND_ID: &str = "server:pr-created-link:test";

fn project() -> OrchestrationProjectShell {
    serde_json::from_value(json!({
        "id": "project-1",
        "title": "Project",
        "workspaceRoot": "/workspace/project",
        "defaultModelSelection": null,
        "scripts": [],
        "repositoryIdentity": {
            "canonicalKey": "github.acme.test/platform/api",
            "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "git@github.acme.test:Platform/API.git"},
            "provider": "github",
            "displayName": "Platform/API",
            "owner": "Platform",
            "name": "API"
        },
        "createdAt": "2026-08-01T00:00:00.000Z",
        "updatedAt": "2026-08-01T00:00:00.000Z"
    }))
    .unwrap()
}

fn thread() -> OrchestrationThreadShell {
    serde_json::from_value(json!({
        "id": THREAD_ID,
        "projectId": "project-1",
        "title": "Thread",
        "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
        "runtimeMode": "full-access",
        "interactionMode": "default",
        "branch": null,
        "worktreePath": null,
        "pullRequests": [],
        "latestTurn": null,
        "createdAt": "2026-08-01T00:00:00.000Z",
        "updatedAt": "2026-08-20T00:00:00.000Z",
        "archivedAt": null,
        "settledOverride": null,
        "settledAt": null,
        "session": null,
        "latestUserMessageAt": "2026-08-20T00:00:00.000Z",
        "hasPendingApprovals": false,
        "hasPendingUserInput": false,
        "hasActionableProposedPlan": false
    }))
    .unwrap()
}

fn pr_result(status: &str, number: Option<i64>, url: Option<&str>) -> PrStep {
    PrStep {
        status: status.into(),
        number,
        url: url.map(str::to_owned),
        ..PrStep::skipped()
    }
}

/// How the fake engine answers `dispatch`.
enum DispatchMode {
    /// Records the command, `{sequence: 1}`.
    Record,
    /// `OrchestrationCommandInvariantError({commandType, detail: "already linked"})`.
    RejectInvariant,
    /// Any other failure (TS: `Effect.die(new Error("engine down"))`).
    Fail(&'static str),
    /// Must never be reached.
    Unreachable,
}

struct FakeEngine {
    mode: DispatchMode,
    commands: Mutex<Vec<Value>>,
}

impl FakeEngine {
    fn new(mode: DispatchMode) -> Self {
        Self {
            mode,
            commands: Mutex::new(Vec::new()),
        }
    }

    fn commands(&self) -> Vec<Value> {
        self.commands.lock().unwrap().clone()
    }
}

#[async_trait]
impl OrchestrationDispatch for FakeEngine {
    async fn dispatch(&self, command: OrchestrationCommand, _origin: Option<OrchestrationClientOrigin>) -> Result<DispatchResult, TaggedError> {
        let value = serde_json::to_value(&command).unwrap();
        match self.mode {
            DispatchMode::Record => {
                self.commands.lock().unwrap().push(value);
                Ok(DispatchResult { sequence: 1 })
            }
            DispatchMode::RejectInvariant => Err(TaggedError::new("OrchestrationCommandInvariantError", "already linked")
                .with("commandType", value["type"].clone())
                .with("detail", "already linked")),
            DispatchMode::Fail(message) => Err(TaggedError::new("Error", message)),
            DispatchMode::Unreachable => panic!("unreachable"),
        }
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
        unimplemented!()
    }
}

/// `ProjectionSnapshotQuery` mock: `getThreadShellById` and `getProjectShellById` only.
struct FakeReads {
    thread: Option<OrchestrationThreadShell>,
}

#[async_trait]
impl ProjectionReads for FakeReads {
    async fn get_user_input_activity(&self, _: &ThreadId, _: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
        unimplemented!()
    }
    async fn list_activities_by_kind(&self, _: &str) -> Result<Vec<OrchestrationThreadActivity>, TaggedError> {
        unimplemented!()
    }
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
        unimplemented!()
    }
    async fn get_snapshot(&self) -> Result<OrchestrationReadModel, TaggedError> {
        unimplemented!()
    }
    async fn get_shell_snapshot(&self, _: bool) -> Result<OrchestrationShellSnapshot, TaggedError> {
        unimplemented!()
    }
    async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot, TaggedError> {
        unimplemented!()
    }
    async fn list_threads_with_pull_requests(&self) -> Result<Vec<ThreadPullRequests>, TaggedError> {
        unimplemented!()
    }
    async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>, TaggedError> {
        unimplemented!()
    }
    async fn search_threads(&self, _: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, TaggedError> {
        unimplemented!()
    }
    async fn get_snapshot_sequence(&self) -> Result<i64, TaggedError> {
        unimplemented!()
    }
    async fn get_counts(&self) -> Result<SnapshotCounts, TaggedError> {
        unimplemented!()
    }
    async fn get_event_replay_stats(&self, _: i64, _: i64) -> Result<ReplayStats, TaggedError> {
        unimplemented!()
    }
    async fn get_active_project_by_workspace_root(&self, _: &str) -> Result<Option<OrchestrationProject>, TaggedError> {
        unimplemented!()
    }
    async fn get_project_shell_by_id(&self, _: &ProjectId) -> Result<Option<OrchestrationProjectShell>, TaggedError> {
        Ok(Some(project()))
    }
    async fn get_project_shells(&self, _: Option<Vec<ProjectId>>) -> Result<Vec<OrchestrationProjectShell>, TaggedError> {
        unimplemented!()
    }
    async fn get_first_active_thread_id_by_project_id(&self, _: &ProjectId) -> Result<Option<ThreadId>, TaggedError> {
        unimplemented!()
    }
    async fn get_imported_agent_session_sources(&self, _: &ProjectId) -> Result<Vec<ImportedAgentSessionSource>, TaggedError> {
        unimplemented!()
    }
    async fn get_thread_checkpoint_context(&self, _: &ThreadId) -> Result<Option<ThreadCheckpointContext>, TaggedError> {
        unimplemented!()
    }
    async fn get_full_thread_diff_context(&self, _: &ThreadId, _: i64) -> Result<Option<FullThreadDiffContext>, TaggedError> {
        unimplemented!()
    }
    async fn get_thread_shell_by_id(&self, _: &ThreadId) -> Result<Option<OrchestrationThreadShell>, TaggedError> {
        Ok(self.thread.clone())
    }
    async fn get_thread_runtime_context(&self, _: &ThreadId) -> Result<Option<ThreadRuntimeContext>, TaggedError> {
        unimplemented!()
    }
    async fn get_turn_start_message(&self, _: &ThreadId, _: &MessageId) -> Result<Option<TurnStartMessage>, TaggedError> {
        unimplemented!()
    }
    async fn get_thread_detail_by_id(&self, _: &ThreadId, _: ThreadDetailQuery) -> Result<Option<OrchestrationThread>, TaggedError> {
        unimplemented!()
    }
    async fn get_thread_detail_snapshot(
        &self,
        _: &ThreadId,
        _: Option<OrchestrationThreadDetailWindow>,
    ) -> Result<Option<OrchestrationThreadDetailSnapshot>, TaggedError> {
        unimplemented!()
    }
}

fn reads() -> FakeReads {
    FakeReads { thread: Some(thread()) }
}

// TS: "links a created pull request to the thread with source created"
#[tokio::test]
async fn links_a_created_pull_request_to_the_thread_with_source_created() {
    let engine = FakeEngine::new(DispatchMode::Record);
    link_created_pull_request(
        &engine,
        &reads(),
        THREAD_ID,
        &pr_result("created", Some(42), Some("https://github.com/t3tools/t3code/pull/42")),
        COMMAND_ID.into(),
    )
    .await;

    assert_eq!(
        engine.commands(),
        vec![json!({
            "type": "thread.pull-request.link",
            "commandId": "server:pr-created-link:test",
            "threadId": THREAD_ID,
            "host": "github.com",
            "repository": "t3tools/t3code",
            "number": 42,
            "url": "https://github.com/t3tools/t3code/pull/42",
            "source": "created",
        })]
    );
}

// TS: "dispatches nothing when the action produced no pull request"
#[tokio::test]
async fn dispatches_nothing_when_the_action_produced_no_pull_request() {
    let engine = FakeEngine::new(DispatchMode::Record);
    let reads = reads();
    link_created_pull_request(&engine, &reads, THREAD_ID, &pr_result("skipped_not_requested", None, None), COMMAND_ID.into()).await;
    link_created_pull_request(
        &engine,
        &reads,
        THREAD_ID,
        &pr_result("created", None, Some("https://github.com/t3tools/t3code/pull/42")),
        COMMAND_ID.into(),
    )
    .await;

    assert_eq!(engine.commands(), Vec::<Value>::new());
}

// TS: "swallows an already-linked rejection and other dispatch failures"
#[tokio::test]
async fn swallows_an_already_linked_rejection_and_other_dispatch_failures() {
    let result = pr_result("opened_existing", Some(7), Some("https://github.com/t3tools/t3code/pull/7"));
    link_created_pull_request(&FakeEngine::new(DispatchMode::RejectInvariant), &reads(), THREAD_ID, &result, COMMAND_ID.into()).await;
    link_created_pull_request(
        &FakeEngine::new(DispatchMode::Fail("engine down")),
        &reads(),
        THREAD_ID,
        &result,
        COMMAND_ID.into(),
    )
    .await;
    // A thread that vanished between the action and the link is not an error either.
    link_created_pull_request(
        &FakeEngine::new(DispatchMode::Unreachable),
        &FakeReads { thread: None },
        THREAD_ID,
        &result,
        COMMAND_ID.into(),
    )
    .await;
}

// ---------------------------------------------------------------------------------------------
// The git.* RPC handlers
// ---------------------------------------------------------------------------------------------

mod handlers {
    use super::*;
    use common::*;
    use zc_core::vcs_process::VcsProcess;
    use zc_git::rpc::{self as git_rpc, GitRpcServices};
    use zc_ports::contracts::{AuthSessionId, BackgroundPolicySnapshot, BackgroundScope, ClientActivityReportInput, HostPowerSnapshot, RpcClientId};
    use zc_ports::{BackgroundPolicy, BackgroundPolicySubscription};
    use zc_rpc::RpcError;
    use zc_vcs::broadcaster::{NoAutoPull, VcsStatusBroadcaster};
    use zc_vcs::registry::{VcsDriverRegistry, VcsProjectConfig};
    use zc_vcs::vcs_driver::GitVcsProcessDriver;
    use zc_vcs::{GitVcsDriver, GitWorkflowService};

    struct AlwaysRun;

    #[async_trait]
    impl BackgroundPolicy for AlwaysRun {
        async fn report_client_activity(&self, _: &AuthSessionId, _: &RpcClientId, _: ClientActivityReportInput) {}
        async fn remove_rpc_client(&self, _: &AuthSessionId, _: &RpcClientId) {}
        async fn report_host_power_state(&self, _: HostPowerSnapshot) {}
        async fn snapshot(&self) -> BackgroundPolicySnapshot {
            BackgroundPolicySnapshot::default()
        }
        async fn subscribe(&self) -> BackgroundPolicySubscription {
            BackgroundPolicySubscription {
                latest: BackgroundPolicySnapshot::default(),
                changes: futures::stream::empty().boxed(),
            }
        }
        async fn has_demand(&self, _: &BackgroundScope) -> bool {
            true
        }
        async fn should_run_scope_work(&self, _: &BackgroundScope) -> bool {
            true
        }
        async fn should_run_opportunistic_work(&self) -> bool {
            true
        }
    }

    /// The services `ws.ts` uses: the workflow over the manager, the broadcaster, the engine
    /// and the projections.
    fn services(h: &Harness, engine: Arc<FakeEngine>) -> GitRpcServices {
        let registry = VcsDriverRegistry::new(VcsProjectConfig::new(), Arc::new(GitVcsProcessDriver::new(VcsProcess::default())));
        let driver = GitVcsDriver::new(h.temp.path.join("workflow-worktrees"));
        let workflow = GitWorkflowService::new(registry, driver, Arc::new(h.manager.clone()));
        let broadcaster = VcsStatusBroadcaster::new(Arc::new(workflow.clone()), Arc::new(AlwaysRun), Arc::new(NoAutoPull));
        GitRpcServices {
            workflow,
            broadcaster,
            engine,
            projections: Arc::new(reads()),
            uuids: Arc::new(|| "uuid-1".to_owned()),
        }
    }

    async fn collect(mut stream: zc_rpc::BoxValueStream) -> (Vec<Value>, Option<RpcError>) {
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => events.push(event),
                Err(error) => return (events, Some(error)),
            }
        }
        (events, None)
    }

    /// A pushed `main`, a feature branch with a dirty change, origin reading as GitHub.
    fn feature_repo() -> (Tmp, Tmp) {
        let repo = repo();
        let remote = bare_remote();
        git(&repo.path, &["remote", "add", "origin", remote.str()]);
        configure_visible_remote(&repo.path, "origin", "https://github.com/pingdotgg/codething-mvp.git", remote.str());
        git(&repo.path, &["push", "-u", "origin", "main"]);
        git(&repo.path, &["checkout", "-b", "feature/rpc-stream"]);
        write(&repo.path, "rpc.txt", "stream\n");
        (repo, remote)
    }

    #[tokio::test]
    async fn run_stacked_action_streams_progress_and_links_the_created_pull_request() {
        let (repo, _remote) = feature_repo();
        let h = manager_with(GhScenario {
            pr_list_sequence: vec![
                "[]".into(),
                json!([{"number": 101, "title": "Add stacked git actions", "url": "https://github.com/pingdotgg/codething-mvp/pull/101", "baseRefName": "main", "headRefName": "feature/rpc-stream"}]).to_string(),
            ],
            ..GhScenario::default()
        });
        let engine = Arc::new(FakeEngine::new(DispatchMode::Record));
        let services = services(&h, engine.clone());
        let stream = git_rpc::run_stacked_action(
            &services,
            json!({"actionId": " client-action ", "cwd": repo.str(), "action": "commit_push_pr", "threadId": THREAD_ID}),
        )
        .await
        .unwrap();
        let (events, error) = collect(stream).await;
        assert!(error.is_none(), "{error:?}");
        let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds.first(), Some(&"action_started"));
        assert_eq!(kinds.last(), Some(&"action_finished"));
        assert!(events.iter().all(|e| e["actionId"] == json!("client-action") && e["cwd"] == json!(repo.str())));
        assert_eq!(events.last().unwrap()["result"]["pr"]["number"], json!(101));
        // The created PR is linked to the thread before the stream ends.
        assert_eq!(
            engine.commands(),
            vec![json!({
                "type": "thread.pull-request.link",
                "commandId": "server:pr-created-link:uuid-1",
                "threadId": THREAD_ID,
                "host": "github.com",
                "repository": "pingdotgg/codething-mvp",
                "number": 101,
                "url": "https://github.com/pingdotgg/codething-mvp/pull/101",
                "source": "created",
            })]
        );
    }

    #[tokio::test]
    async fn run_stacked_action_ends_with_the_typed_failure_after_action_failed() {
        let (repo, _remote) = feature_repo();
        let h = manager_with(GhScenario::default());
        let engine = Arc::new(FakeEngine::new(DispatchMode::Unreachable));
        let services = services(&h, engine.clone());
        let stream = git_rpc::run_stacked_action(
            &services,
            json!({"actionId": "a1", "cwd": repo.str(), "action": "create_pr", "threadId": THREAD_ID}),
        )
        .await
        .unwrap();
        let (events, error) = collect(stream).await;
        assert_eq!(
            events,
            vec![json!({
                "actionId": "a1",
                "cwd": repo.str(),
                "action": "create_pr",
                "kind": "action_failed",
                "phase": null,
                "message": "Git manager failed in runStackedAction: Commit local changes before creating a PR.",
            })]
        );
        assert_eq!(
            error,
            Some(RpcError::Fail(json!({
                "_tag": "GitManagerError",
                "operation": "runStackedAction",
                "cwd": repo.str(),
                "detail": "Commit local changes before creating a PR.",
            })))
        );
        assert!(engine.commands().is_empty());
    }

    #[tokio::test]
    async fn run_stacked_action_rejects_bad_payloads_and_non_repositories() {
        let h = manager_with(GhScenario::default());
        let services = services(&h, Arc::new(FakeEngine::new(DispatchMode::Unreachable)));
        let error = git_rpc::run_stacked_action(&services, json!({"actionId": "a1", "cwd": "  ", "action": "commit"}))
            .await
            .err()
            .unwrap();
        assert!(matches!(error, RpcError::Die(Value::String(_))), "{error:?}");

        let plain = Tmp::new("zc-git-rpc-plain-");
        let stream = git_rpc::run_stacked_action(&services, json!({"actionId": "a1", "cwd": plain.str(), "action": "commit"}))
            .await
            .unwrap();
        let (events, error) = collect(stream).await;
        assert!(events.is_empty());
        let Some(RpcError::Fail(failure)) = error else { panic!("{error:?}") };
        assert_eq!(failure["_tag"], json!("GitManagerError"));
        assert_eq!(failure["operation"], json!("GitWorkflowService.runStackedAction"));
    }

    #[tokio::test]
    async fn resolve_and_prepare_pull_request_answer_wire_json() {
        let repo = repo();
        let remote = bare_remote();
        git(&repo.path, &["remote", "add", "origin", remote.str()]);
        git(&repo.path, &["push", "-u", "origin", "main"]);
        let h = manager_with(GhScenario {
            pull_request: Some(json!({
                "number": 55,
                "title": "Resolve me",
                "url": "https://github.com/pingdotgg/codething-mvp/pull/55",
                "baseRefName": "main",
                "headRefName": "feature/resolve-me",
                "state": "OPEN",
            })),
            ..GhScenario::default()
        });
        let services = services(&h, Arc::new(FakeEngine::new(DispatchMode::Unreachable)));
        let resolved = git_rpc::resolve_pull_request(&services, json!({"cwd": repo.str(), "reference": "#55"}))
            .await
            .unwrap();
        assert_eq!(
            resolved,
            json!({"pullRequest": {"number": 55, "title": "Resolve me", "url": "https://github.com/pingdotgg/codething-mvp/pull/55", "baseBranch": "main", "headBranch": "feature/resolve-me", "state": "open"}})
        );
        let prepared = git_rpc::prepare_pull_request_thread(&services, json!({"cwd": repo.str(), "reference": "55", "mode": "local"}))
            .await
            .unwrap();
        assert_eq!(prepared["branch"], json!("feature/resolve-me"));
        assert_eq!(prepared["worktreePath"], Value::Null);
        assert_eq!(prepared["isOnPullRequestHead"], json!(true));
        assert_eq!(git(&repo.path, &["branch", "--show-current"]), "feature/resolve-me");
        assert!(h.gh_calls().iter().any(|call| call == "pr checkout 55 --force"), "{:?}", h.gh_calls());
    }
}
