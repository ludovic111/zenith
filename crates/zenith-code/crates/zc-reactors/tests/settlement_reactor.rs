//! Port of `ThreadSettlementReactor.test.ts`.
//!
//! Not ported here: "settles the same threads from the unsettled read as from the full read"
//! runs the SQL `ProjectionSnapshotQuery` (WP-09), and the "storage cleanup" cases belong to
//! `storageCleanup.ts` (WP-11). The `withWorkspaceLease` case is a unit test of
//! `zc_reactors::workspace_lease`.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use zc_contracts::{OrchestrationCommand, OrchestrationEvent, ProjectId, ThreadId};
use zc_core::PubSub;
use zc_ports::contracts as ports;
use zc_ports::git::{CreateWorktreeOptions, GitBranchPullRequest, GitRemoteStatusOptions, GitRunStackedActionOptions, RemoteTrackingCommit};
use zc_ports::orchestration::{ThreadReplayRange, ThreadReplayStats};
use zc_ports::pull_requests::PullRequestMergeEvent;
use zc_ports::{DispatchResult, EventStream, GitWorkflow, OrchestrationDispatch, PullRequests, SettingsService, TaggedError};
use zc_reactors::reads::{ReactorReads, ReadResult};
use zc_reactors::settlement::{auto_settlement_settings_key, SettlementDeps, ThreadSettlementReactor};
use zc_reactors::ManualClock;

const NOW: &str = "2026-08-28T12:00:00.000Z";
const PROJECT_ID: &str = "settlement-project";
const LINKED_PROJECT_ID: &str = "linked-settlement-project";

type Hook<A, R> = Arc<dyn Fn(A) -> BoxFuture<'static, R> + Send + Sync>;

fn hook<A, R, F, Fut>(f: F) -> Hook<A, R>
where
    F: Fn(A) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = R> + Send + 'static,
{
    Arc::new(move |a| f(a).boxed())
}

#[derive(Clone)]
struct Latch(Arc<tokio::sync::watch::Sender<bool>>);

impl Latch {
    fn new() -> Self {
        Self(Arc::new(tokio::sync::watch::channel(false).0))
    }
    fn open(&self) {
        self.0.send_replace(true);
    }
    async fn wait(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|open| *open).await;
    }
}

fn project(id: &str, workspace_root: &str) -> Value {
    json!({"id": id, "title": format!("Project {id}"), "workspaceRoot": workspace_root, "defaultModelSelection": null, "scripts": [], "createdAt": "2026-08-01T00:00:00.000Z", "updatedAt": NOW})
}

fn thread(id: &str, overrides: Value) -> Value {
    let mut thread = json!({
        "id": id, "projectId": PROJECT_ID, "title": id, "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
        "runtimeMode": "full-access", "interactionMode": "default", "pullRequests": [], "branch": null, "worktreePath": null,
        "latestTurn": null, "createdAt": "2026-08-01T00:00:00.000Z", "updatedAt": "2026-08-20T00:00:00.000Z", "archivedAt": null,
        "settledOverride": null, "settledAt": null, "session": null, "latestUserMessageAt": "2026-08-20T00:00:00.000Z",
        "hasPendingApprovals": false, "hasPendingUserInput": false, "hasActionableProposedPlan": false,
    });
    for (key, value) in overrides.as_object().unwrap() {
        thread[key] = value.clone();
    }
    thread
}

fn snapshot(threads: Vec<Value>, projects: Vec<Value>) -> Value {
    json!({"snapshotSequence": 1, "projects": projects, "threads": threads, "updatedAt": NOW})
}

fn default_projects() -> Vec<Value> {
    vec![project(PROJECT_ID, "/workspace/project")]
}

fn summary(project_id: &str, repository: &str, number: i64, state: &str) -> Value {
    json!({
        "provider": "github", "projectId": project_id, "repository": repository, "number": number, "title": "Pull request",
        "url": format!("https://example.test/{repository}/pull/{number}"), "state": state, "headBranch": "feature", "baseBranch": "main",
        "updatedAt": NOW, "closedAt": if state == "closed" { Some(NOW) } else { None }, "mergedAt": if state == "merged" { Some(NOW) } else { None },
    })
}

fn branch_pr(state: &str) -> GitBranchPullRequest {
    branch_pr_with_key(state, "example.test/owner/repository")
}

fn branch_pr_with_key(state: &str, key: &str) -> GitBranchPullRequest {
    GitBranchPullRequest {
        pull_request: ports::VcsStatusPullRequest(json!({
            "number": 42, "title": "Branch pull request", "url": "https://example.test/owner/repository/pull/42",
            "baseRef": "main", "headRef": "saved-feature", "state": state,
        })),
        repository_key: Some(key.into()),
        updated_at: Some(NOW.into()),
        closed_at: Some((state == "closed").then(|| NOW.to_owned())),
        merged_at: Some((state == "merged").then(|| NOW.to_owned())),
    }
}

fn linked(project_id: &str, number: i64) -> Value {
    json!({"projectId": project_id, "repository": "owner/repository", "number": number, "url": format!("https://example.test/owner/repository/pull/{number}")})
}

// ---------------------------------------------------------------------------------------------
// Fakes

struct Snapshots {
    snapshot: Mutex<Value>,
    read_count: AtomicUsize,
    reads: mpsc::UnboundedSender<Option<String>>,
    full_read: Option<Hook<(), Value>>,
}

#[async_trait]
impl ReactorReads for Snapshots {
    async fn shell_snapshot(&self, _unsettled_only: bool) -> ReadResult<Value> {
        self.read_count.fetch_add(1, Ordering::SeqCst);
        let _ = self.reads.send(None);
        match &self.full_read {
            Some(read) => Ok(read(()).await),
            None => Ok(self.snapshot.lock().unwrap().clone()),
        }
    }
    async fn snapshot_sequence(&self) -> ReadResult<i64> {
        Ok(self.snapshot.lock().unwrap()["snapshotSequence"].as_i64().unwrap_or(0))
    }
    async fn thread_shell(&self, thread_id: &ThreadId) -> ReadResult<Option<Value>> {
        let found = self.snapshot.lock().unwrap()["threads"]
            .as_array()
            .unwrap()
            .iter()
            .find(|thread| thread["id"] == thread_id.as_str() && thread["archivedAt"].is_null())
            .cloned();
        let _ = self.reads.send(Some(thread_id.0.clone()));
        Ok(found)
    }
    async fn project_shells(&self, project_ids: Option<Vec<ProjectId>>) -> ReadResult<Vec<Value>> {
        Ok(self.snapshot.lock().unwrap()["projects"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|project| project_ids.as_ref().is_none_or(|ids| ids.iter().any(|id| project["id"] == id.as_str())))
            .cloned()
            .collect())
    }
}

struct Settings {
    current: Mutex<Value>,
    reads: mpsc::UnboundedSender<()>,
    changes: PubSub<ports::ServerSettings>,
}

impl Settings {
    fn decode(value: &Value) -> ports::ServerSettings {
        serde_json::from_value(value.clone()).expect("settings decode")
    }

    /// `applyServerSettingsPatch` for the keys these tests touch.
    fn update(&self, patch: Value) {
        let next = {
            let mut current = self.current.lock().unwrap();
            for (key, value) in patch.as_object().unwrap() {
                if key == "projectSettingsOverrides" {
                    for (project, entry) in value.as_object().unwrap() {
                        let overrides = current["projectSettingsOverrides"].as_object_mut().unwrap();
                        if entry.is_null() {
                            overrides.remove(project);
                        } else {
                            overrides.insert(project.clone(), entry.clone());
                        }
                    }
                } else {
                    current[key] = value.clone();
                }
            }
            current.clone()
        };
        self.changes.publish(Self::decode(&next));
    }
}

#[async_trait]
impl SettingsService for Settings {
    async fn get_settings(&self) -> Result<ports::ServerSettings, ports::ServerSettingsError> {
        let _ = self.reads.send(());
        Ok(Self::decode(&self.current.lock().unwrap()))
    }
    async fn update_settings(&self, _patch: ports::ServerSettingsPatch) -> Result<ports::ServerSettings, ports::ServerSettingsError> {
        Ok(Self::decode(&self.current.lock().unwrap()))
    }
    fn subscribe_changes(&self) -> EventStream<ports::ServerSettings> {
        self.changes.subscribe().boxed()
    }
}

type BranchHook = Hook<(String, String, bool), Result<Option<GitBranchPullRequest>, TaggedError>>;

struct Git {
    branch_calls: Mutex<Vec<Value>>,
    branch_hook: Option<BranchHook>,
    invalidated: Mutex<Vec<String>>,
}

fn unsupported<T>() -> Result<T, TaggedError> {
    Err(TaggedError::new("Defect", "unexpected git call"))
}

#[async_trait]
impl GitWorkflow for Git {
    async fn is_repository(&self, _cwd: &str) -> Result<bool, TaggedError> {
        unsupported()
    }
    async fn has_commit(&self, _cwd: &str, _ref_name: &str) -> Result<bool, TaggedError> {
        unsupported()
    }
    async fn status(&self, _input: ports::VcsStatusInput) -> Result<ports::VcsStatusResult, TaggedError> {
        unsupported()
    }
    async fn local_status(&self, _input: ports::VcsStatusInput) -> Result<ports::VcsStatusLocalResult, TaggedError> {
        unsupported()
    }
    async fn remote_status(
        &self,
        _input: ports::VcsStatusInput,
        _options: GitRemoteStatusOptions,
    ) -> Result<Option<ports::VcsStatusRemoteResult>, TaggedError> {
        unsupported()
    }
    async fn branch_pull_request(&self, cwd: &str, branch: &str, refresh: bool) -> Result<Option<GitBranchPullRequest>, TaggedError> {
        self.branch_calls.lock().unwrap().push(json!({"cwd": cwd, "branch": branch}));
        match &self.branch_hook {
            Some(hook) => hook((cwd.to_owned(), branch.to_owned(), refresh)).await,
            None => Ok(None),
        }
    }
    async fn invalidate_local_status(&self, _cwd: &str) {}
    async fn invalidate_remote_status(&self, _cwd: &str) {}
    async fn invalidate_status(&self, cwd: &str) {
        self.invalidated.lock().unwrap().push(cwd.to_owned());
    }
    async fn pull_current_branch(&self, _cwd: &str) -> Result<ports::VcsPullResult, TaggedError> {
        unsupported()
    }
    async fn run_stacked_action(
        &self,
        _input: ports::GitRunStackedActionInput,
        _options: GitRunStackedActionOptions,
    ) -> Result<ports::GitRunStackedActionResult, TaggedError> {
        unsupported()
    }
    async fn resolve_pull_request(&self, _input: ports::GitPullRequestRefInput) -> Result<ports::GitResolvePullRequestResult, TaggedError> {
        unsupported()
    }
    async fn prepare_pull_request_thread(
        &self,
        _input: ports::GitPreparePullRequestThreadInput,
    ) -> Result<ports::GitPreparePullRequestThreadResult, TaggedError> {
        unsupported()
    }
    async fn list_refs(&self, _input: ports::VcsListRefsInput) -> Result<ports::VcsListRefsResult, TaggedError> {
        unsupported()
    }
    async fn create_worktree(
        &self,
        _input: ports::VcsCreateWorktreeInput,
        _options: CreateWorktreeOptions,
    ) -> Result<ports::VcsCreateWorktreeResult, TaggedError> {
        unsupported()
    }
    async fn fetch_remote(&self, _cwd: &str, _remote: &str, _ref_name: Option<&str>) -> Result<(), TaggedError> {
        unsupported()
    }
    async fn remote_exists(&self, _cwd: &str, _remote: &str) -> Result<bool, TaggedError> {
        unsupported()
    }
    async fn remote_branch_exists(&self, _cwd: &str, _remote: &str, _ref_name: &str) -> Result<bool, TaggedError> {
        unsupported()
    }
    async fn resolve_remote_tracking_commit(&self, _cwd: &str, _ref_name: &str, _fallback: &str) -> Result<RemoteTrackingCommit, TaggedError> {
        unsupported()
    }
    async fn remove_worktree(&self, _input: ports::VcsRemoveWorktreeInput) -> Result<(), TaggedError> {
        unsupported()
    }
    async fn prune_worktrees(&self, _cwd: &str) -> Result<(), TaggedError> {
        unsupported()
    }
    async fn create_ref(&self, _input: ports::VcsCreateRefInput) -> Result<ports::VcsCreateRefResult, TaggedError> {
        unsupported()
    }
    async fn switch_ref(&self, _input: ports::VcsSwitchRefInput) -> Result<ports::VcsSwitchRefResult, TaggedError> {
        unsupported()
    }
    async fn rename_branch(&self, _cwd: &str, _old: &str, _new: &str) -> Result<String, TaggedError> {
        unsupported()
    }
}

struct PullRequestFake {
    summary_calls: Mutex<Vec<Value>>,
    recovery: Mutex<Vec<bool>>,
    summary_hook: Option<Hook<Value, Result<Value, TaggedError>>>,
    merges: PubSub<PullRequestMergeEvent>,
}

#[async_trait]
impl PullRequests for PullRequestFake {
    async fn summary(&self, reference: ports::PullRequestRef, recover_transient_failure: bool) -> Result<ports::PullRequestSummary, TaggedError> {
        self.summary_calls.lock().unwrap().push(reference.0.clone());
        self.recovery.lock().unwrap().push(recover_transient_failure);
        let value = match &self.summary_hook {
            Some(hook) => hook(reference.0).await?,
            None => summary(
                reference.0["projectId"].as_str().unwrap_or(""),
                reference.0["repository"].as_str().unwrap_or(""),
                reference.0["number"].as_i64().unwrap_or(0),
                "open",
            ),
        };
        Ok(ports::PullRequestSummary(value))
    }
    async fn stack(&self, _reference: ports::PullRequestRef, _include_details: bool) -> Result<Option<ports::PullRequestStack>, TaggedError> {
        Ok(None)
    }
    async fn diff(&self, _input: ports::PullRequestDiffInput) -> Result<ports::PullRequestDiffResult, TaggedError> {
        Err(TaggedError::new("Defect", "unexpected diff"))
    }
    async fn invalidate(&self, _input: ports::PullRequestInvalidateInput, _notify_readers: bool) {}
    async fn refresh_after_turn(&self, _project_id: &ports::ProjectId) {}
    fn subscribe_merges(&self) -> EventStream<PullRequestMergeEvent> {
        self.merges.subscribe().boxed()
    }
    fn subscribe_refreshes(&self) -> EventStream<u64> {
        futures::stream::empty().boxed()
    }
}

struct Engine {
    commands: Mutex<Vec<Value>>,
    on_dispatch: Option<Hook<Value, Result<(), TaggedError>>>,
    events: PubSub<OrchestrationEvent>,
}

#[async_trait]
impl OrchestrationDispatch for Engine {
    async fn dispatch(&self, command: OrchestrationCommand, _origin: Option<ports::OrchestrationClientOrigin>) -> Result<DispatchResult, TaggedError> {
        let value = serde_json::to_value(&command).unwrap();
        assert_eq!(value["type"], "thread.auto-settle", "Unexpected command");
        self.commands.lock().unwrap().push(value.clone());
        if let Some(on_dispatch) = &self.on_dispatch {
            on_dispatch(value).await?;
        }
        Ok(DispatchResult { sequence: 1 })
    }
    fn subscribe_domain_events(&self) -> EventStream<OrchestrationEvent> {
        self.events.subscribe().boxed()
    }
    async fn latest_sequence(&self) -> i64 {
        0
    }
    fn read_events(&self, _from: i64, _limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        futures::stream::empty().boxed()
    }
    fn read_thread_events(&self, _range: ThreadReplayRange, _limit: Option<u32>) -> EventStream<Result<OrchestrationEvent, TaggedError>> {
        futures::stream::empty().boxed()
    }
    async fn get_thread_replay_stats(&self, _range: ThreadReplayRange, _max: u32) -> Result<ThreadReplayStats, TaggedError> {
        Ok(ThreadReplayStats::default())
    }
}

// ---------------------------------------------------------------------------------------------
// Harness

#[derive(Default)]
struct Options {
    snapshot: Value,
    full_read: Option<Hook<(), Value>>,
    settings: Option<Value>,
    branch: Option<BranchHook>,
    summary: Option<Hook<Value, Result<Value, TaggedError>>>,
    existing_worktrees: Vec<String>,
    on_dispatch: Option<Hook<Value, Result<(), TaggedError>>>,
}

struct Harness {
    reactor: ThreadSettlementReactor,
    snapshots: Arc<Snapshots>,
    snapshot_reads: tokio::sync::Mutex<mpsc::UnboundedReceiver<Option<String>>>,
    settings: Arc<Settings>,
    settings_reads: tokio::sync::Mutex<mpsc::UnboundedReceiver<()>>,
    git: Arc<Git>,
    pull_requests: Arc<PullRequestFake>,
    engine: Arc<Engine>,
    clock: Arc<ManualClock>,
    activation: Latch,
}

fn default_settings() -> Value {
    zc_settings::settings::test_settings(&json!({}))
}

fn settings_with(overrides: Value) -> Value {
    let mut settings = default_settings();
    for (key, value) in overrides.as_object().unwrap() {
        settings[key] = value.clone();
    }
    settings
}

impl Harness {
    fn new(options: Options) -> Self {
        let (snapshot_tx, snapshot_rx) = mpsc::unbounded_channel();
        let (settings_tx, settings_rx) = mpsc::unbounded_channel();
        let snapshots = Arc::new(Snapshots {
            snapshot: Mutex::new(options.snapshot),
            read_count: AtomicUsize::new(0),
            reads: snapshot_tx,
            full_read: options.full_read,
        });
        let settings = Arc::new(Settings {
            current: Mutex::new(options.settings.unwrap_or_else(default_settings)),
            reads: settings_tx,
            changes: PubSub::new(),
        });
        let git = Arc::new(Git {
            branch_calls: Mutex::new(Vec::new()),
            branch_hook: options.branch,
            invalidated: Mutex::new(Vec::new()),
        });
        let pull_requests = Arc::new(PullRequestFake {
            summary_calls: Mutex::new(Vec::new()),
            recovery: Mutex::new(Vec::new()),
            summary_hook: options.summary,
            merges: PubSub::new(),
        });
        let engine = Arc::new(Engine {
            commands: Mutex::new(Vec::new()),
            on_dispatch: options.on_dispatch,
            events: PubSub::new(),
        });
        let clock = Arc::new(ManualClock::fixed(zc_core::time::parse_iso_millis(NOW).unwrap()));
        let existing = options.existing_worktrees;
        let reactor = ThreadSettlementReactor::new(
            SettlementDeps {
                engine: engine.clone(),
                reads: snapshots.clone(),
                settings: settings.clone(),
                git: git.clone(),
                pull_requests: pull_requests.clone(),
                clock: clock.clone(),
                uuids: zc_reactors::common::system_uuids(),
                path_exists: Arc::new(move |path: &str| existing.iter().any(|existing| existing == path)),
                interval: None,
            },
            tokio_util::sync::CancellationToken::new(),
        );
        Self {
            reactor,
            snapshots,
            snapshot_reads: tokio::sync::Mutex::new(snapshot_rx),
            settings,
            settings_reads: tokio::sync::Mutex::new(settings_rx),
            git,
            pull_requests,
            engine,
            clock,
            activation: Latch::new(),
        }
    }

    async fn start(&self) {
        let activation = self.activation.clone();
        self.reactor.start_with_activation(Some(async move { activation.wait().await }.boxed())).await;
    }

    /// `startHarness`: start, activate, wait for the first read, drain.
    async fn start_harness(&self) {
        self.start().await;
        self.activation.open();
        self.take_snapshot_read().await;
        self.reactor.drain().await;
    }

    async fn take_snapshot_read(&self) -> Option<String> {
        self.snapshot_reads.lock().await.recv().await.expect("a snapshot read")
    }

    async fn take_settings_read(&self) {
        self.settings_reads.lock().await.recv().await.expect("a settings read");
    }

    async fn clear_settings_reads(&self) {
        let mut reads = self.settings_reads.lock().await;
        while reads.try_recv().is_ok() {}
    }

    fn commands(&self) -> Vec<Value> {
        self.engine.commands.lock().unwrap().clone()
    }

    fn thread_ids(&self) -> Vec<String> {
        self.commands().iter().map(|command| command["threadId"].as_str().unwrap().to_owned()).collect()
    }

    fn sorted_ids(&self) -> Vec<String> {
        let mut ids = self.thread_ids();
        ids.sort();
        ids
    }

    fn branch_calls(&self) -> Vec<Value> {
        self.git.branch_calls.lock().unwrap().clone()
    }

    fn summary_calls(&self) -> Vec<Value> {
        self.pull_requests.summary_calls.lock().unwrap().clone()
    }

    fn publish_merge(&self) {
        self.pull_requests.merges.publish(PullRequestMergeEvent {
            reference: ports::PullRequestRef(json!({"projectId": PROJECT_ID, "repository": "owner/repository", "number": 42})),
            merged_at: NOW.into(),
        });
    }

    fn publish_event(&self, event: Value) {
        self.engine.events.publish(serde_json::from_value(event).expect("event decode"));
    }
}

fn ids(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

// ---------------------------------------------------------------------------------------------
// Tests

#[test]
fn distinguishes_a_project_that_inherits_the_threshold_from_one_that_disables_it() {
    let inherits = auto_settlement_settings_key(&settings_with(
        json!({"projectSettingsOverrides": {PROJECT_ID: {"sidebarAutoSettleOnMerge": true}}}),
    ));
    let never = auto_settlement_settings_key(&settings_with(
        json!({"projectSettingsOverrides": {PROJECT_ID: {"sidebarAutoSettleOnMerge": true, "sidebarAutoSettleAfterDays": null}}}),
    ));
    assert_ne!(inherits, never);
}

#[test]
fn ignores_project_overrides_that_do_not_touch_settlement() {
    let base = auto_settlement_settings_key(&settings_with(
        json!({"projectSettingsOverrides": {PROJECT_ID: {"sidebarAutoSettleOnMerge": false}}}),
    ));
    let unrelated = auto_settlement_settings_key(&settings_with(json!({"projectSettingsOverrides": {
        LINKED_PROJECT_ID: {"defaultThreadEnvMode": "worktree"},
        PROJECT_ID: {"sidebarAutoSettleOnMerge": false, "defaultAutoPull": true},
    }})));
    assert_eq!(base, unrelated);
}

fn link(number: i64, state: Option<&str>) -> Value {
    json!({
        "host": "example.test", "repository": "owner/repository", "number": number, "url": format!("https://example.test/owner/repository/pull/{number}"),
        "source": "manual", "linkedAt": NOW, "stack": null,
        "snapshot": state.map(|state| json!({
            "state": state, "title": "Review", "headBranch": "feature", "baseBranch": "main", "isDraft": false, "updatedAt": NOW, "syncedAt": NOW,
            "mergedAt": if state == "merged" { Some(NOW) } else { None }, "closedAt": if state == "closed" { Some(NOW) } else { None },
        })),
    })
}

#[tokio::test]
async fn settles_synced_terminal_links_immediately_while_open_unsynced_or_running_threads_wait() {
    let running_session = json!({"threadId": "running", "status": "running", "providerName": "Codex", "runtimeMode": "full-access", "activeTurnId": null, "lastError": null, "updatedAt": NOW});
    let threads = vec![
        thread("merged", json!({"pullRequests": [link(1, Some("open")), link(2, Some("merged"))]})),
        thread("closed", json!({"pullRequests": [link(1, Some("open"))]})),
        thread("open", json!({"pullRequests": [link(1, Some("open")), link(2, Some("open"))]})),
        thread("unsynced", json!({"pullRequests": [link(1, Some("open")), link(2, None)]})),
        thread("running", json!({"pullRequests": [link(1, Some("open"))], "session": running_session})),
    ];
    let h = Harness::new(Options {
        snapshot: snapshot(threads.clone(), default_projects()),
        settings: Some(settings_with(json!({"sidebarAutoSettleOnMerge": true}))),
        branch: Some(hook(|_| async { Err(TaggedError::new("Defect", "linked threads must not query the branch")) })),
        summary: Some(hook(|_| async { Err(TaggedError::new("Defect", "linked threads must use their snapshots")) })),
        ..Default::default()
    });
    h.start_harness().await;
    assert!(h.commands().is_empty());
    let base = |kind: &str, id: &str, payload: Value| {
        json!({"sequence": 2, "eventId": "pull-request-synced", "aggregateKind": "thread", "aggregateId": id, "occurredAt": NOW, "commandId": null,
            "causationEventId": null, "correlationId": null, "metadata": {}, "type": kind, "payload": payload})
    };
    for thread in &threads {
        let id = thread["id"].as_str().unwrap();
        let terminal = link(1, Some(if id == "closed" { "closed" } else { "merged" }));
        {
            let mut snapshot = h.snapshots.snapshot.lock().unwrap();
            for current in snapshot["threads"].as_array_mut().unwrap() {
                if current["id"] == id {
                    let mut links = current["pullRequests"].as_array().unwrap().clone();
                    links[0] = terminal.clone();
                    current["pullRequests"] = json!(links);
                }
            }
        }
        h.publish_event(base(
            "thread.pull-request-synced",
            id,
            json!({"threadId": id, "host": terminal["host"], "repository": terminal["repository"], "number": terminal["number"], "snapshot": terminal["snapshot"], "stack": null, "updatedAt": NOW}),
        ));
        assert_eq!(h.take_snapshot_read().await.as_deref(), Some(id));
        h.reactor.drain().await;
    }
    assert_eq!(h.thread_ids(), ids(&["merged", "closed"]));
    let mut ready = running_session.clone();
    ready["status"] = json!("ready");
    {
        let mut snapshot = h.snapshots.snapshot.lock().unwrap();
        for current in snapshot["threads"].as_array_mut().unwrap() {
            if current["id"] == "running" {
                current["session"] = ready.clone();
            }
        }
    }
    h.publish_event(base("thread.session-set", "running", json!({"threadId": "running", "session": ready})));
    assert_eq!(h.take_snapshot_read().await.as_deref(), Some("running"));
    h.reactor.drain().await;
    assert_eq!(h.thread_ids(), ids(&["merged", "closed", "running"]));
    assert!(h.branch_calls().is_empty());
    assert!(h.summary_calls().is_empty());
}

fn merged_summary() -> Hook<Value, Result<Value, TaggedError>> {
    hook(|input: Value| async move {
        let mut value = summary(
            input["projectId"].as_str().unwrap(),
            input["repository"].as_str().unwrap(),
            input["number"].as_i64().unwrap(),
            "merged",
        );
        value["mergedAt"] = json!("2026-08-27T00:00:00.000Z");
        Ok(value)
    })
}

#[tokio::test]
async fn skips_the_branch_recheck_when_a_terminal_link_would_settle_nothing() {
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![thread(
                "resumed-manual",
                json!({"branch": "main", "linkedPullRequest": linked(PROJECT_ID, 1), "latestUserMessageAt": "2026-08-28T00:00:00.000Z"}),
            )],
            default_projects(),
        ),
        settings: Some(settings_with(json!({"sidebarAutoSettleAfterDays": null, "sidebarAutoSettleOnMerge": true}))),
        branch: Some(hook(|_| async { Ok(Some(branch_pr("open"))) })),
        summary: Some(merged_summary()),
        ..Default::default()
    });
    h.start_harness().await;
    assert!(h.commands().is_empty());
    assert_eq!(h.summary_calls().len(), 1);
    assert!(h.branch_calls().is_empty());
}

#[tokio::test]
async fn uses_saved_prs_without_settling_resumed_threads_or_branches_with_newer_prs() {
    let mut project = project(PROJECT_ID, "/workspace/project");
    project["repositoryIdentity"] = json!({
        "canonicalKey": "example.test/owner/repository", "rootPath": "/workspace/project", "displayName": "owner/repository",
        "locator": {"source": "git-remote", "remoteName": "origin", "remoteUrl": "https://example.test/owner/repository.git"},
    });
    let previous = linked(PROJECT_ID, 1);
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread("retained-terminal", json!({"branch": "main", "branchPullRequest": previous})),
                thread("reused-manual", json!({"branch": "reused", "linkedPullRequest": previous})),
                thread("reused-detected", json!({"branch": "reused", "branchPullRequest": previous})),
                thread("foreign-branch-pr", json!({"branch": "foreign", "linkedPullRequest": previous})),
                thread(
                    "resumed-manual",
                    json!({"branch": "main", "linkedPullRequest": previous, "latestUserMessageAt": "2026-08-28T00:00:00.000Z"}),
                ),
                thread(
                    "resumed-detected",
                    json!({"branch": "main", "branchPullRequest": previous, "latestUserMessageAt": "2026-08-28T00:00:00.000Z"}),
                ),
            ],
            vec![project],
        ),
        settings: Some(settings_with(json!({"sidebarAutoSettleAfterDays": null, "sidebarAutoSettleOnMerge": true}))),
        branch: Some(hook(|(_, branch, refresh): (String, String, bool)| async move {
            Ok(match branch.as_str() {
                "reused" => Some(branch_pr(if refresh { "open" } else { "merged" })),
                "foreign" => Some(branch_pr_with_key("open", "example.test/another/repository")),
                _ => None,
            })
        })),
        summary: Some(merged_summary()),
        ..Default::default()
    });
    h.start_harness().await;
    let settled: HashSet<String> = h.thread_ids().into_iter().collect();
    assert_eq!(settled, ["retained-terminal", "foreign-branch-pr"].iter().map(|id| (*id).to_owned()).collect());
}

#[tokio::test]
async fn skips_pr_work_on_startup_timer_and_merge_sweeps_when_settlement_is_disabled() {
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread("branch-thread", json!({"branch": "feature"})),
                thread("linked-thread", json!({"linkedPullRequest": linked(PROJECT_ID, 42)})),
            ],
            default_projects(),
        ),
        settings: Some(settings_with(json!({"sidebarAutoSettleAfterDays": null, "sidebarAutoSettleOnMerge": false}))),
        ..Default::default()
    });
    h.start().await;
    h.take_settings_read().await;
    h.activation.open();
    h.take_settings_read().await;
    h.reactor.drain().await;
    h.clock.advance(60_000);
    h.reactor.tick();
    h.take_settings_read().await;
    h.reactor.drain().await;
    h.publish_merge();
    h.take_settings_read().await;
    h.reactor.drain().await;
    assert!(h.branch_calls().is_empty());
    assert!(h.summary_calls().is_empty());
    assert!(h.git.invalidated.lock().unwrap().is_empty());
    assert!(h.commands().is_empty());
    assert_eq!(h.snapshots.read_count.load(Ordering::SeqCst), 0);
    h.settings.update(json!({"sidebarAutoSettleAfterDays": 1}));
    h.take_snapshot_read().await;
    h.reactor.drain().await;
    assert_eq!(h.sorted_ids(), ids(&["branch-thread", "linked-thread"]));
}

#[tokio::test]
async fn a_project_override_settles_only_that_projects_inactive_threads() {
    let overridden = "overridden-project";
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread("inherits-thread", json!({})),
                thread("overridden-thread", json!({"projectId": overridden})),
            ],
            vec![project(PROJECT_ID, "/workspace/project"), project(overridden, "/workspace/overridden")],
        ),
        settings: Some(settings_with(json!({
            "sidebarAutoSettleAfterDays": null, "sidebarAutoSettleOnMerge": false,
            "projectSettingsOverrides": {overridden: {"sidebarAutoSettleAfterDays": 1}},
        }))),
        ..Default::default()
    });
    h.start().await;
    h.take_settings_read().await;
    h.activation.open();
    h.take_snapshot_read().await;
    h.reactor.drain().await;
    assert_eq!(h.thread_ids(), ids(&["overridden-thread"]));
    h.settings
        .update(json!({"projectSettingsOverrides": {overridden: null}, "sidebarAutoSettleAfterDays": 1}));
    h.take_snapshot_read().await;
    h.reactor.drain().await;
    assert!(h.thread_ids().contains(&"inherits-thread".to_owned()));
}

#[tokio::test]
async fn starts_without_clients_and_skips_protected_threads_before_pull_request_lookup() {
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread("inactive", json!({"branch": "inactive-feature"})),
                thread(
                    "closed-pr",
                    json!({"linkedPullRequest": linked(LINKED_PROJECT_ID, 42), "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
                thread("pending-approval", json!({"branch": "skip-approval", "hasPendingApprovals": true})),
                thread("snoozed", json!({"branch": "skip-snoozed", "snoozedUntil": "2026-08-29T00:00:00.000Z"})),
            ],
            vec![project(PROJECT_ID, "/workspace/project"), project(LINKED_PROJECT_ID, "/workspace/linked")],
        ),
        branch: Some(hook(|_| async { Ok(None) })),
        summary: Some(hook(|input: Value| async move {
            Ok(summary(
                input["projectId"].as_str().unwrap(),
                input["repository"].as_str().unwrap(),
                input["number"].as_i64().unwrap(),
                "closed",
            ))
        })),
        ..Default::default()
    });
    h.start().await;
    assert_eq!(h.snapshots.read_count.load(Ordering::SeqCst), 0);
    h.activation.open();
    h.take_snapshot_read().await;
    h.reactor.drain().await;
    let mut commands: Vec<Value> = h
        .commands()
        .iter()
        .map(|command| json!({"threadId": command["threadId"], "snapshotSequence": command["snapshotSequence"], "settledAt": command["settledAt"]}))
        .collect();
    commands.sort_by_key(|command| command["threadId"].as_str().unwrap().to_owned());
    assert_eq!(
        commands,
        vec![
            json!({"threadId": "closed-pr", "snapshotSequence": 1, "settledAt": "2026-08-27T00:00:00.000Z"}),
            json!({"threadId": "inactive", "snapshotSequence": 1, "settledAt": "2026-08-20T00:00:00.000Z"}),
        ]
    );
    assert!(h.branch_calls().is_empty());
    assert_eq!(
        h.summary_calls(),
        vec![json!({"projectId": LINKED_PROJECT_ID, "repository": "owner/repository", "number": 42})]
    );
    assert_eq!(*h.pull_requests.recovery.lock().unwrap(), vec![false]);
}

#[tokio::test]
async fn reevaluates_inactivity_and_pull_request_state_once_per_minute() {
    let state = Arc::new(Mutex::new("open"));
    let current = state.clone();
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread("at-boundary", json!({"latestUserMessageAt": "2026-08-25T12:00:00.000Z"})),
                thread("open-pr", json!({"branch": "saved-feature", "latestUserMessageAt": "2026-08-27T00:00:00.000Z"})),
            ],
            default_projects(),
        ),
        branch: Some(hook(move |_| {
            let current = current.clone();
            async move { Ok(Some(branch_pr(*current.lock().unwrap()))) }
        })),
        ..Default::default()
    });
    h.start_harness().await;
    assert!(h.commands().is_empty());
    *state.lock().unwrap() = "merged";
    h.clock.advance(60_000);
    h.reactor.tick();
    h.take_snapshot_read().await;
    h.reactor.drain().await;
    assert_eq!(h.sorted_ids(), ids(&["at-boundary", "open-pr"]));
    assert_eq!(h.branch_calls().len(), 2);
}

#[tokio::test]
async fn reevaluates_immediately_after_a_pull_request_merge() {
    let periodic_started = Latch::new();
    let release_periodic = Latch::new();
    let settled = Latch::new();
    let count = Arc::new(AtomicUsize::new(0));
    let (started, release) = (periodic_started.clone(), release_periodic.clone());
    let signal = settled.clone();
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread(
                    "merged-in-app",
                    json!({"latestUserMessageAt": "2026-08-27T00:00:00.000Z", "linkedPullRequest": linked(PROJECT_ID, 42)}),
                ),
                thread(
                    "slow-periodic-lookup",
                    json!({"branch": "another-feature", "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
            ],
            default_projects(),
        ),
        branch: Some(hook(move |_| {
            let (count, started, release) = (count.clone(), started.clone(), release.clone());
            async move {
                if count.fetch_add(1, Ordering::SeqCst) > 0 {
                    started.open();
                    release.wait().await;
                }
                Ok(Some(branch_pr("open")))
            }
        })),
        on_dispatch: Some(hook(move |_| {
            let signal = signal.clone();
            async move {
                signal.open();
                Ok(())
            }
        })),
        ..Default::default()
    });
    h.start_harness().await;
    h.settings.update(json!({"sidebarAutoSettleAfterDays": 4}));
    periodic_started.wait().await;
    h.publish_merge();
    settled.wait().await;
    assert_eq!(h.thread_ids(), ids(&["merged-in-app"]));
    release_periodic.open();
    h.reactor.drain().await;
}

#[tokio::test]
async fn settles_branch_threads_on_a_pull_request_merge_without_waiting_for_the_next_sweep() {
    let state = Arc::new(Mutex::new("open"));
    let current = state.clone();
    let settled = Latch::new();
    let signal = settled.clone();
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![thread(
                "branch-thread",
                json!({"branch": "saved-feature", "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
            )],
            default_projects(),
        ),
        branch: Some(hook(move |_| {
            let current = current.clone();
            async move { Ok(Some(branch_pr(*current.lock().unwrap()))) }
        })),
        on_dispatch: Some(hook(move |_| {
            let signal = signal.clone();
            async move {
                signal.open();
                Ok(())
            }
        })),
        ..Default::default()
    });
    h.start_harness().await;
    assert!(h.commands().is_empty());
    assert!(h.git.invalidated.lock().unwrap().is_empty());
    *state.lock().unwrap() = "merged";
    h.publish_merge();
    settled.wait().await;
    assert_eq!(h.thread_ids(), ids(&["branch-thread"]));
    assert_eq!(*h.git.invalidated.lock().unwrap(), vec!["/workspace/project".to_owned()]);
    h.reactor.drain().await;
}

#[tokio::test]
async fn a_merge_does_not_settle_threads_linked_to_an_unrelated_pull_request() {
    let settled = Latch::new();
    let lookup_started = Latch::new();
    let release_lookup = Latch::new();
    let count = Arc::new(AtomicUsize::new(0));
    let (started, release) = (lookup_started.clone(), release_lookup.clone());
    let signal = settled.clone();
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread(
                    "merged-in-app",
                    json!({"latestUserMessageAt": "2026-08-27T00:00:00.000Z", "linkedPullRequest": linked(PROJECT_ID, 42)}),
                ),
                thread(
                    "unrelated-linked",
                    json!({"latestUserMessageAt": "2026-08-27T00:00:00.000Z", "linkedPullRequest": linked(PROJECT_ID, 99)}),
                ),
            ],
            default_projects(),
        ),
        summary: Some(hook(move |input: Value| {
            let (count, started, release) = (count.clone(), started.clone(), release.clone());
            async move {
                if count.fetch_add(1, Ordering::SeqCst) + 1 == 3 {
                    started.open();
                    release.wait().await;
                }
                Ok(summary(
                    input["projectId"].as_str().unwrap(),
                    input["repository"].as_str().unwrap(),
                    input["number"].as_i64().unwrap(),
                    "open",
                ))
            }
        })),
        on_dispatch: Some(hook(move |_| {
            let signal = signal.clone();
            async move {
                signal.open();
                Ok(())
            }
        })),
        ..Default::default()
    });
    h.start_harness().await;
    assert!(h.commands().is_empty());
    h.publish_merge();
    lookup_started.wait().await;
    settled.wait().await;
    release_lookup.open();
    h.reactor.drain().await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert_eq!(h.thread_ids(), ids(&["merged-in-app"]));
    let mut numbers: Vec<i64> = h.summary_calls().iter().map(|call| call["number"].as_i64().unwrap()).collect();
    numbers.sort();
    assert_eq!(numbers, vec![42, 99, 99]);
}

#[tokio::test]
async fn uses_fresh_settlement_settings_after_lookup_and_ignores_unrelated_changes() {
    let state = Arc::new(Mutex::new("merged"));
    let first_started = Latch::new();
    let release_first = Latch::new();
    let later_started = Latch::new();
    let release_later = Latch::new();
    let count = Arc::new(AtomicUsize::new(0));
    let (current, counter) = (state.clone(), count.clone());
    let (fs, fr, ls, lr) = (first_started.clone(), release_first.clone(), later_started.clone(), release_later.clone());
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![thread(
                "settings-thread",
                json!({"branch": "saved-feature", "latestUserMessageAt": "2026-08-28T00:00:00.000Z"}),
            )],
            default_projects(),
        ),
        settings: Some(settings_with(json!({"sidebarAutoSettleAfterDays": null, "sidebarAutoSettleOnMerge": true}))),
        branch: Some(hook(move |_| {
            let (current, counter, fs, fr, ls, lr) = (current.clone(), counter.clone(), fs.clone(), fr.clone(), ls.clone(), lr.clone());
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                if n == 1 {
                    fs.open();
                    fr.wait().await;
                } else if n == 2 {
                    ls.open();
                    lr.wait().await;
                }
                Ok(Some(branch_pr(*current.lock().unwrap())))
            }
        })),
        ..Default::default()
    });
    h.start().await;
    h.activation.open();
    h.take_snapshot_read().await;
    first_started.wait().await;
    h.clear_settings_reads().await;
    h.settings.update(json!({"sidebarAutoSettleOnMerge": false}));
    release_first.open();
    // The in-flight decision and the newly queued sweep both read the disabled settings.
    h.take_settings_read().await;
    h.take_settings_read().await;
    h.reactor.drain().await;
    assert!(h.commands().is_empty());
    assert_eq!(h.snapshots.read_count.load(Ordering::SeqCst), 1);

    *state.lock().unwrap() = "closed";
    h.settings.update(json!({"enableAgentBrowserAccess": false}));
    h.settings.update(json!({"sidebarAutoSettleAfterDays": 1}));
    later_started.wait().await;
    release_later.open();
    h.reactor.drain().await;
    assert_eq!(h.snapshots.read_count.load(Ordering::SeqCst), 2);
    assert_eq!(count.load(Ordering::SeqCst), 2);
    assert_eq!(h.thread_ids(), ids(&["settings-thread"]));
}

fn unavailable() -> TaggedError {
    TaggedError::new("PullRequestOperationError", "host unavailable")
        .with("operation", "summary")
        .with("detail", "host unavailable")
}

#[tokio::test]
async fn keeps_an_unknown_pull_request_active_and_continues_with_other_candidates() {
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread(
                    "lookup-failed",
                    json!({"latestUserMessageAt": "2026-08-27T00:00:00.000Z", "linkedPullRequest": linked(LINKED_PROJECT_ID, 9)}),
                ),
                thread("inactive-without-pr", json!({})),
            ],
            vec![project(PROJECT_ID, "/workspace/project"), project(LINKED_PROJECT_ID, "/workspace/linked")],
        ),
        summary: Some(hook(|_| async { Err(unavailable()) })),
        ..Default::default()
    });
    h.start_harness().await;
    assert_eq!(h.thread_ids(), ids(&["inactive-without-pr"]));
    assert_eq!(h.summary_calls().len(), 1);
}

#[tokio::test]
async fn settles_inactive_linked_and_branch_threads_without_reading_an_unavailable_host() {
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread("inactive-linked", json!({"linkedPullRequest": linked(PROJECT_ID, 42)})),
                thread(
                    "inactive-branch",
                    json!({"branch": "saved-feature", "latestUserMessageAt": "2026-08-21T00:00:00.000Z"}),
                ),
            ],
            default_projects(),
        ),
        branch: Some(hook(|_| async { Err(TaggedError::new("Defect", "host unavailable")) })),
        summary: Some(hook(|_| async { Err(unavailable()) })),
        ..Default::default()
    });
    h.start_harness().await;
    let mut commands: Vec<Value> = h
        .commands()
        .iter()
        .map(|command| json!({"threadId": command["threadId"], "snapshotSequence": command["snapshotSequence"], "settledAt": command["settledAt"]}))
        .collect();
    commands.sort_by_key(|command| command["threadId"].as_str().unwrap().to_owned());
    assert_eq!(
        commands,
        vec![
            json!({"threadId": "inactive-branch", "snapshotSequence": 1, "settledAt": "2026-08-21T00:00:00.000Z"}),
            json!({"threadId": "inactive-linked", "snapshotSequence": 1, "settledAt": "2026-08-20T00:00:00.000Z"}),
        ]
    );
    assert!(h.summary_calls().is_empty());
    assert!(h.branch_calls().is_empty());
}

#[tokio::test]
async fn settles_an_inactive_thread_before_its_shared_pull_request_lookup_completes() {
    let started = Latch::new();
    let release = Latch::new();
    let (s1, r1) = (started.clone(), release.clone());
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread(
                    "recent-linked",
                    json!({"linkedPullRequest": linked(PROJECT_ID, 42), "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
                thread("inactive-linked", json!({"linkedPullRequest": linked(PROJECT_ID, 42)})),
            ],
            default_projects(),
        ),
        summary: Some(hook(move |_| {
            let (s1, r1) = (s1.clone(), r1.clone());
            async move {
                s1.open();
                r1.wait().await;
                Err(unavailable())
            }
        })),
        ..Default::default()
    });
    h.start().await;
    h.activation.open();
    started.wait().await;
    assert_eq!(h.thread_ids(), ids(&["inactive-linked"]));
    release.open();
    h.reactor.drain().await;
    assert_eq!(h.commands().len(), 1);
    assert_eq!(h.summary_calls().len(), 1);
}

#[tokio::test]
async fn keeps_threads_active_when_their_pull_request_project_is_unavailable() {
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread(
                    "missing-own-project",
                    json!({"latestUserMessageAt": "2026-08-27T00:00:00.000Z", "linkedPullRequest": linked(LINKED_PROJECT_ID, 10)}),
                ),
                thread(
                    "missing-branch-project",
                    json!({"branch": "saved-feature", "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
            ],
            vec![project(LINKED_PROJECT_ID, "/workspace/linked")],
        ),
        summary: Some(hook(|input: Value| async move {
            Ok(summary(
                input["projectId"].as_str().unwrap(),
                input["repository"].as_str().unwrap(),
                input["number"].as_i64().unwrap(),
                "open",
            ))
        })),
        ..Default::default()
    });
    h.start_harness().await;
    assert!(h.commands().is_empty());
    assert_eq!(
        h.summary_calls(),
        vec![json!({"projectId": LINKED_PROJECT_ID, "repository": "owner/repository", "number": 10})]
    );
    assert!(h.branch_calls().is_empty());
}

#[tokio::test]
async fn deduplicates_saved_branch_and_linked_pull_request_lookups_within_a_sweep() {
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread(
                    "branch-one",
                    json!({"branch": "saved-feature", "worktreePath": "/deleted/worktree-one", "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
                thread(
                    "branch-two",
                    json!({"branch": "saved-feature", "worktreePath": "/deleted/worktree-two", "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
                thread(
                    "linked-one",
                    json!({"linkedPullRequest": linked(LINKED_PROJECT_ID, 77), "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
                thread(
                    "linked-two",
                    json!({"linkedPullRequest": linked(LINKED_PROJECT_ID, 77), "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
            ],
            vec![
                project(PROJECT_ID, "/workspace/project-root"),
                project(LINKED_PROJECT_ID, "/workspace/linked-root"),
            ],
        ),
        branch: Some(hook(|_| async { Ok(Some(branch_pr("closed"))) })),
        summary: Some(hook(|input: Value| async move {
            Ok(summary(
                input["projectId"].as_str().unwrap(),
                input["repository"].as_str().unwrap(),
                input["number"].as_i64().unwrap(),
                "merged",
            ))
        })),
        ..Default::default()
    });
    h.start_harness().await;
    assert_eq!(h.branch_calls(), vec![json!({"cwd": "/workspace/project-root", "branch": "saved-feature"})]);
    assert_eq!(
        h.summary_calls(),
        vec![json!({"projectId": LINKED_PROJECT_ID, "repository": "owner/repository", "number": 77})]
    );
    let settled: HashSet<String> = h.thread_ids().into_iter().collect();
    assert_eq!(
        settled,
        ["branch-one", "branch-two", "linked-one", "linked-two"]
            .iter()
            .map(|id| (*id).to_owned())
            .collect()
    );
}

#[tokio::test]
async fn looks_up_the_branch_pull_request_from_a_threads_live_worktree() {
    let h = Harness::new(Options {
        snapshot: snapshot(
            vec![
                thread(
                    "live-worktree",
                    json!({"branch": "feature/live", "worktreePath": "/workspace/project-root/.worktrees/live", "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
                thread(
                    "deleted-worktree",
                    json!({"branch": "feature/deleted", "worktreePath": "/workspace/project-root/.worktrees/deleted", "latestUserMessageAt": "2026-08-27T00:00:00.000Z"}),
                ),
            ],
            vec![project(PROJECT_ID, "/workspace/project-root")],
        ),
        existing_worktrees: vec!["/workspace/project-root/.worktrees/live".into()],
        ..Default::default()
    });
    h.start_harness().await;
    let calls: HashSet<String> = h.branch_calls().iter().map(Value::to_string).collect();
    let expected: HashSet<String> = [
        json!({"cwd": "/workspace/project-root/.worktrees/live", "branch": "feature/live"}),
        json!({"cwd": "/workspace/project-root", "branch": "feature/deleted"}),
    ]
    .iter()
    .map(Value::to_string)
    .collect();
    assert_eq!(calls, expected);
}

#[tokio::test]
async fn carries_the_snapshot_guard_and_survives_a_stale_dispatch_rejection() {
    let h = Harness::new(Options {
        snapshot: snapshot(vec![thread("stale", json!({})), thread("next-candidate", json!({}))], default_projects()),
        on_dispatch: Some(hook(|command: Value| async move {
            if command["threadId"] == "stale" {
                Err(TaggedError::new(
                    "OrchestrationCommandInvariantError",
                    "thread changed after settlement evaluation",
                ))
            } else {
                Ok(())
            }
        })),
        ..Default::default()
    });
    h.start_harness().await;
    let first = h.commands();
    assert_eq!(first.iter().find(|command| command["threadId"] == "stale").unwrap()["snapshotSequence"], 1);
    assert!(first.iter().any(|command| command["threadId"] == "next-candidate"));
    h.settings.update(json!({"sidebarAutoSettleAfterDays": 4}));
    h.take_snapshot_read().await;
    h.reactor.drain().await;
    assert_eq!(h.commands().len(), 4);
}

#[tokio::test]
async fn sweep_full_read_hook_is_used_for_full_sweeps() {
    // The TS harness's `getShellSnapshot` override: a full sweep reads from it.
    let h = Harness::new(Options {
        snapshot: snapshot(Vec::new(), default_projects()),
        full_read: Some(hook(|_| async { snapshot(vec![thread("from-hook", json!({}))], default_projects()) })),
        ..Default::default()
    });
    h.start_harness().await;
    assert_eq!(h.thread_ids(), ids(&["from-hook"]));
}
