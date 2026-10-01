//! Port of the "storage cleanup" cases of `orchestration/ThreadSettlementReactor.test.ts`: the
//! protection matrix over a real temp base directory (worktrees, browser artifacts, rotated
//! logs), with scripted projections, git, sessions, terminals and settings.

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use common::*;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_checkpoints::bootstrap::ThreadDeletionDrain;
use zc_checkpoints::storage_cleanup::{CleanupGit, LocalStatus, StorageCleanup, StorageCleanupDeps, StorageCleanupPaths};
use zc_contracts::*;
use zc_core::pubsub::PubSub;
use zc_ports::contracts::{
    GitPreparePullRequestThreadInput, GitPreparePullRequestThreadResult, GitPullRequestRefInput, GitResolvePullRequestResult, GitRunStackedActionInput,
    GitRunStackedActionResult, VcsCreateRefInput, VcsCreateRefResult, VcsCreateWorktreeInput, VcsCreateWorktreeResult, VcsListRefsInput, VcsListRefsResult,
    VcsPullResult, VcsRemoveWorktreeInput, VcsStatusInput, VcsStatusLocalResult, VcsStatusPullRequest, VcsStatusRemoteResult, VcsStatusResult,
    VcsSwitchRefInput, VcsSwitchRefResult,
};
use zc_ports::git::{CreateWorktreeOptions, GitBranchPullRequest, GitRemoteStatusOptions, GitRunStackedActionOptions, RemoteTrackingCommit};
use zc_ports::orchestration::*;
use zc_ports::{DispatchResult, EventStream, GitWorkflow, OrchestrationDispatch, ProjectionReads, TaggedError};

const CLEANUP_NOW: &str = "2026-08-28T12:00:00.000Z";
const PROJECT_ID: &str = "settlement-project";
const LINKED_PROJECT_ID: &str = "linked-settlement-project";

fn make_project(id: &str, workspace_root: &Path) -> OrchestrationProjectShell {
    decode(
        json!({"id": id, "title": format!("Project {id}"), "workspaceRoot": workspace_root, "defaultModelSelection": null, "scripts": [],
                  "createdAt": "2026-08-01T00:00:00.000Z", "updatedAt": CLEANUP_NOW}),
    )
}

fn make_thread(id: &str, overrides: Value) -> OrchestrationThreadShell {
    let mut value = json!({
        "id": id, "projectId": PROJECT_ID, "title": id, "modelSelection": {"instanceId": "codex", "model": "gpt-5"},
        "runtimeMode": "full-access", "interactionMode": "default", "pullRequests": [], "branch": null, "worktreePath": null,
        "latestTurn": null, "createdAt": "2026-08-01T00:00:00.000Z", "updatedAt": "2026-08-20T00:00:00.000Z", "archivedAt": null,
        "settledOverride": null, "settledAt": null, "session": null, "latestUserMessageAt": "2026-08-20T00:00:00.000Z",
        "hasPendingApprovals": false, "hasPendingUserInput": false, "hasActionableProposedPlan": false,
    });
    value.as_object_mut().unwrap().extend(overrides.as_object().unwrap().clone());
    decode(value)
}

fn snapshot(threads: Vec<OrchestrationThreadShell>, projects: Vec<OrchestrationProjectShell>) -> OrchestrationShellSnapshot {
    OrchestrationShellSnapshot {
        snapshot_sequence: 1,
        projects,
        threads,
        updated_at: CLEANUP_NOW.into(),
    }
}

/// Everything the scripted services read, per protection case.
struct Scenario {
    protection: &'static str,
    base: PathBuf,
    worktree: PathBuf,
    second_worktree: PathBuf,
    thread: OrchestrationThreadShell,
    tombstoned: AtomicBool,
    snapshot_reads: AtomicUsize,
    head_reads: AtomicUsize,
    default_ref_fetched: AtomicBool,
    fetches: AtomicUsize,
    removals: Mutex<Vec<String>>,
    settings: Arc<MemorySettings>,
    deletion_released: tokio::sync::Semaphore,
    deletion_started: tokio::sync::Notify,
    deletion_entered: AtomicBool,
    events: PubSub<OrchestrationEvent>,
}

impl Scenario {
    fn p(&self) -> &str {
        self.protection
    }
}

struct Reads(Arc<Scenario>);

#[async_trait]
impl ProjectionReads for Reads {
    async fn get_user_input_activity(&self, _: &ThreadId, _: &ApprovalRequestId) -> Result<Option<OrchestrationThreadActivity>, TaggedError> {
        unused()
    }
    async fn list_activities_by_kind(&self, _: &str) -> Result<Vec<OrchestrationThreadActivity>, TaggedError> {
        unused()
    }
    async fn get_command_read_model(&self) -> Result<OrchestrationReadModel, TaggedError> {
        unused()
    }
    async fn get_snapshot(&self) -> Result<OrchestrationReadModel, TaggedError> {
        unused()
    }
    async fn get_shell_snapshot(&self, _: bool) -> Result<OrchestrationShellSnapshot, TaggedError> {
        let s = &self.0;
        let reads = s.snapshot_reads.fetch_add(1, Ordering::SeqCst) + 1;
        let p = s.p();
        let mut projects = if p.starts_with("deleted-owner") || p == "deleted-project-off" || p == "deleted-project-custom" {
            vec![]
        } else {
            vec![make_project(PROJECT_ID, &s.base)]
        };
        let mut threads = if s.tombstoned.load(Ordering::SeqCst) {
            vec![]
        } else {
            vec![s.thread.clone()]
        };
        if p == "deleted-shared" {
            let mut surviving = s.thread.clone();
            surviving.id = ThreadId::new("surviving-thread");
            threads.push(surviving);
        }
        if p == "deleted-project" {
            projects.push(make_project(LINKED_PROJECT_ID, &s.worktree));
        }
        if p == "project-root" || p == "nested-project" || (p == "new-nested-project" && reads > 1) {
            let root = if p == "project-root" { s.worktree.clone() } else { s.worktree.join("nested") };
            projects.push(make_project(LINKED_PROJECT_ID, &root));
            threads.push(make_thread("local-project-thread", json!({"projectId": LINKED_PROJECT_ID})));
        }
        if p == "unchanged-two-worktrees" {
            let mut second = s.thread.clone();
            second.id = ThreadId::new("second-worktree-thread");
            second.branch = Some("feature-two".into());
            second.worktree_path = Some(s.second_worktree.to_string_lossy().into_owned());
            threads.push(second);
        }
        Ok(snapshot(threads, projects))
    }
    async fn get_archived_shell_snapshot(&self) -> Result<OrchestrationShellSnapshot, TaggedError> {
        let s = &self.0;
        let threads = if s.p() == "shared" {
            let mut archived = s.thread.clone();
            archived.id = ThreadId::new("archived-sharing-thread");
            archived.archived_at = Some(CLEANUP_NOW.into());
            vec![archived]
        } else {
            vec![]
        };
        Ok(snapshot(threads, vec![make_project(PROJECT_ID, &s.base)]))
    }
    async fn list_threads_with_pull_requests(&self) -> Result<Vec<zc_ports::orchestration::ThreadPullRequests>, TaggedError> {
        unused()
    }
    async fn get_deleted_worktree_threads(&self) -> Result<Vec<DeletedWorktreeThread>, TaggedError> {
        let s = &self.0;
        if !s.tombstoned.load(Ordering::SeqCst) {
            return Ok(vec![]);
        }
        let workspace_root = match s.p() {
            "deleted-owner-root" => s.worktree.clone(),
            "deleted-owner-nested" => s.worktree.join("nested"),
            _ => s.base.clone(),
        };
        Ok(vec![DeletedWorktreeThread {
            id: s.thread.id.clone(),
            project_id: s.thread.project_id.clone(),
            branch: "feature".into(),
            worktree_path: s.worktree.to_string_lossy().into_owned(),
            workspace_root: workspace_root.to_string_lossy().into_owned(),
            deleted_at: CLEANUP_NOW.into(),
        }])
    }
    async fn search_threads(&self, _: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, TaggedError> {
        unused()
    }
    async fn get_snapshot_sequence(&self) -> Result<i64, TaggedError> {
        Ok(2)
    }
    async fn get_counts(&self) -> Result<SnapshotCounts, TaggedError> {
        unused()
    }
    async fn get_event_replay_stats(&self, _: i64, _: i64) -> Result<ReplayStats, TaggedError> {
        unused()
    }
    async fn get_active_project_by_workspace_root(&self, _: &str) -> Result<Option<OrchestrationProject>, TaggedError> {
        unused()
    }
    async fn get_project_shell_by_id(&self, _: &ProjectId) -> Result<Option<OrchestrationProjectShell>, TaggedError> {
        unused()
    }
    async fn get_project_shells(&self, _: Option<Vec<ProjectId>>) -> Result<Vec<OrchestrationProjectShell>, TaggedError> {
        unused()
    }
    async fn get_first_active_thread_id_by_project_id(&self, _: &ProjectId) -> Result<Option<ThreadId>, TaggedError> {
        unused()
    }
    async fn get_imported_agent_session_sources(&self, _: &ProjectId) -> Result<Vec<ImportedAgentSessionSource>, TaggedError> {
        unused()
    }
    async fn get_thread_checkpoint_context(&self, _: &ThreadId) -> Result<Option<ThreadCheckpointContext>, TaggedError> {
        unused()
    }
    async fn get_full_thread_diff_context(&self, _: &ThreadId, _: i64) -> Result<Option<FullThreadDiffContext>, TaggedError> {
        unused()
    }
    async fn get_thread_shell_by_id(&self, _: &ThreadId) -> Result<Option<OrchestrationThreadShell>, TaggedError> {
        unused()
    }
    async fn get_thread_runtime_context(&self, _: &ThreadId) -> Result<Option<zc_ports::orchestration::ThreadRuntimeContext>, TaggedError> {
        unused()
    }
    async fn get_turn_start_message(&self, _: &ThreadId, _: &MessageId) -> Result<Option<TurnStartMessage>, TaggedError> {
        unused()
    }
    async fn get_thread_detail_by_id(&self, _: &ThreadId, _: ThreadDetailQuery) -> Result<Option<OrchestrationThread>, TaggedError> {
        unused()
    }
    async fn get_thread_detail_snapshot(
        &self,
        _: &ThreadId,
        _: Option<OrchestrationThreadDetailWindow>,
    ) -> Result<Option<OrchestrationThreadDetailSnapshot>, TaggedError> {
        unused()
    }
}

struct Engine(Arc<Scenario>);

#[async_trait]
impl OrchestrationDispatch for Engine {
    async fn dispatch(&self, _: OrchestrationCommand, _: Option<OrchestrationClientOrigin>) -> Result<DispatchResult, TaggedError> {
        unused()
    }
    fn subscribe_domain_events(&self) -> EventStream<OrchestrationEvent> {
        self.0.events.subscribe().boxed()
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
    async fn get_thread_replay_stats(&self, _: ThreadReplayRange, _: u32) -> Result<zc_ports::orchestration::ThreadReplayStats, TaggedError> {
        unused()
    }
}

struct Deletion(Arc<Scenario>);

#[async_trait]
impl ThreadDeletionDrain for Deletion {
    async fn drain_through(&self, sequence: i64) {
        assert_eq!(sequence, 2);
        self.0.deletion_entered.store(true, Ordering::SeqCst);
        self.0.deletion_started.notify_waiters();
        let _permit = self.0.deletion_released.acquire().await;
    }
}

struct Git(Arc<Scenario>);

impl Git {
    async fn maybe_change_policy(&self) {
        let s = &self.0;
        let p = s.p();
        if (p.starts_with("policy-") || p == "project-policy-disabled") && s.head_reads.load(Ordering::SeqCst) > 1 {
            if p == "project-policy-disabled" {
                s.settings
                    .apply(json!({"projectSettingsOverrides": {PROJECT_ID: {"worktreeCleanup": {"mode": "off"}}}}));
            } else {
                s.settings
                    .apply(json!({"storageCleanup": {"worktreeAfterDays": if p == "policy-disabled" { Value::Null } else { json!(60) }}}));
            }
        }
    }
}

#[async_trait]
impl CleanupGit for Git {
    async fn status_details_local(&self, cwd: &str) -> Result<LocalStatus, String> {
        let s = &self.0;
        Ok(LocalStatus {
            is_repo: true,
            branch: Some(if Path::new(cwd) == s.second_worktree { "feature-two" } else { "feature" }.into()),
            has_working_tree_changes: s.p() == "dirty" || s.p() == "deleted-dirty",
        })
    }
    async fn resolve_commit(&self, _: &str, revision: &str) -> Result<String, String> {
        let s = &self.0;
        if revision != "HEAD" {
            return Ok(if s.default_ref_fetched.load(Ordering::SeqCst) { "b" } else { "d" }.repeat(40));
        }
        let reads = s.head_reads.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(if s.p() == "head-moved" && reads > 1 { "c" } else { "a" }.repeat(40))
    }
    async fn ignored_files(&self, _: &str) -> Result<(String, bool), String> {
        self.maybe_change_policy().await;
        let stdout = match self.0.p() {
            "ignored" | "deleted-ignored" => ".env\0",
            "ignored-directory" => ".cache/\0",
            _ => "",
        };
        Ok((stdout.into(), false))
    }
    async fn is_ancestor(&self, _: &str, _ancestor: &str, descendant: &str) -> Result<bool, String> {
        self.maybe_change_policy().await;
        Ok(!(self.0.p() == "diverged" || descendant != "b".repeat(40)))
    }
    async fn resolve_primary_remote_name(&self, _: &str) -> Result<String, String> {
        Ok("origin".into())
    }
    async fn resolve_default_branch_name(&self, _: &str, _: &str) -> Result<Option<String>, String> {
        Ok(Some("main".into()))
    }
    async fn fetch_remote_tracking_branch(&self, cwd: &str, remote: &str, branch: &str) -> Result<(), String> {
        assert_eq!((cwd, remote, branch), (self.0.base.to_str().unwrap(), "origin", "main"));
        self.0.default_ref_fetched.store(true, Ordering::SeqCst);
        self.0.fetches.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn remove_worktree(&self, _: &str, path: &str, force: bool) -> Result<(), String> {
        assert!(!force);
        self.0.removals.lock().unwrap().push(path.to_owned());
        std::fs::remove_dir_all(path).map_err(|e| e.to_string())
    }
}

struct Manager(Arc<Scenario>);

#[async_trait]
impl GitWorkflow for Manager {
    async fn is_repository(&self, _: &str) -> Result<bool, TaggedError> {
        unused()
    }
    async fn has_commit(&self, _: &str, _: &str) -> Result<bool, TaggedError> {
        unused()
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
    async fn branch_pull_request(&self, _: &str, _: &str, refresh: bool) -> Result<Option<GitBranchPullRequest>, TaggedError> {
        assert!(refresh);
        let state = if self.0.p() == "unmerged" { "open" } else { "merged" };
        Ok(Some(GitBranchPullRequest {
            pull_request: VcsStatusPullRequest(
                json!({"number": 42, "title": "Branch pull request", "url": "https://example.test/owner/repository/pull/42",
                                                      "baseRef": "main", "headRef": "saved-feature", "state": state}),
            ),
            repository_key: Some("example.test/owner/repository".into()),
            updated_at: Some(CLEANUP_NOW.into()),
            closed_at: Some(None),
            merged_at: Some((state == "merged").then(|| CLEANUP_NOW.to_owned())),
        }))
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
    async fn create_worktree(&self, _: VcsCreateWorktreeInput, _: CreateWorktreeOptions) -> Result<VcsCreateWorktreeResult, TaggedError> {
        unused()
    }
    async fn fetch_remote(&self, _: &str, _: &str, _: Option<&str>) -> Result<(), TaggedError> {
        unused()
    }
    async fn remote_exists(&self, _: &str, _: &str) -> Result<bool, TaggedError> {
        unused()
    }
    async fn remote_branch_exists(&self, _: &str, _: &str, _: &str) -> Result<bool, TaggedError> {
        unused()
    }
    async fn resolve_remote_tracking_commit(&self, _: &str, _: &str, _: &str) -> Result<RemoteTrackingCommit, TaggedError> {
        unused()
    }
    async fn remove_worktree(&self, _: VcsRemoveWorktreeInput) -> Result<(), TaggedError> {
        unused()
    }
    async fn prune_worktrees(&self, _: &str) -> Result<(), TaggedError> {
        unused()
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

fn set_mtime(path: &Path, iso: &str) {
    let millis = zc_core::time::parse_iso_millis(iso).unwrap();
    let time = SystemTime::UNIX_EPOCH + Duration::from_millis(millis as u64);
    std::fs::File::options().write(true).open(path).unwrap().set_modified(time).unwrap();
}

const PROTECTIONS: [&str; 37] = [
    "none",
    "dirty",
    "ignored",
    "ignored-directory",
    "shared",
    "project-root",
    "nested-project",
    "new-nested-project",
    "session",
    "terminal-cwd",
    "terminal-worktree",
    "recent",
    "merged",
    "unmerged",
    "unchanged",
    "unchanged-two-worktrees",
    "diverged",
    "head-moved",
    "deleted",
    "deleted-event",
    "deleted-dirty",
    "deleted-ignored",
    "deleted-shared",
    "deleted-project",
    "deleted-owner",
    "deleted-owner-root",
    "deleted-owner-nested",
    "deleted-provider",
    "project-off",
    "project-custom",
    "project-policy-disabled",
    "deleted-project-custom",
    "deleted-project-off",
    "policy-disabled",
    "policy-extended",
    "files-disabled",
    "files-extended",
];

async fn run_case(protection: &'static str) {
    let (_dir, base) = temp_dir();
    let paths = StorageCleanupPaths {
        worktrees_dir: base.join("worktrees"),
        browser_artifacts_dir: base.join("browser-artifacts"),
        logs_dir: base.join("logs"),
    };
    let worktree = paths.worktrees_dir.join("feature");
    std::fs::create_dir_all(&worktree).unwrap();
    std::fs::write(worktree.join(".git"), "gitdir: /test/admin").unwrap();
    let second_worktree = paths.worktrees_dir.join("feature-two");
    if protection == "unchanged-two-worktrees" {
        std::fs::create_dir_all(&second_worktree).unwrap();
        std::fs::write(second_worktree.join(".git"), "gitdir: /test/admin-two").unwrap();
    }
    if protection == "ignored" || protection == "deleted-ignored" {
        std::fs::write(worktree.join(".env"), "secret").unwrap();
    }
    if protection == "ignored-directory" {
        std::fs::create_dir_all(worktree.join(".cache")).unwrap();
        std::fs::write(worktree.join(".cache").join("local-data"), "keep").unwrap();
    }
    std::fs::create_dir_all(&paths.browser_artifacts_dir).unwrap();
    std::fs::create_dir_all(&paths.logs_dir).unwrap();
    let old_image = paths.browser_artifacts_dir.join("old.png");
    let recent_image = paths.browser_artifacts_dir.join("recent.png");
    let old_log = paths.logs_dir.join("server.log.1");
    let active_log = paths.logs_dir.join("server.log");
    for file in [&old_image, &old_log, &active_log] {
        std::fs::write(file, "keep or remove").unwrap();
        set_mtime(file, "2026-08-01T00:00:00.000Z");
    }
    std::fs::write(&recent_image, "recent").unwrap();
    set_mtime(&recent_image, CLEANUP_NOW);

    let mut overrides = json!({"branch": "feature", "worktreePath": worktree,
        "latestUserMessageAt": if protection == "recent" { "2026-08-26T00:00:00.000Z" } else { "2026-08-01T00:00:00.000Z" }});
    if protection == "session" {
        overrides["session"] = json!({"threadId": "storage-thread", "status": "ready", "providerName": "codex", "runtimeMode": "full-access",
                                      "activeTurnId": null, "lastError": null, "updatedAt": CLEANUP_NOW});
    }
    let thread = make_thread("storage-thread", overrides);

    let delete_rule = protection.starts_with("deleted");
    let merge_rule = protection == "merged" || protection == "unmerged";
    let unchanged_rule = ["unchanged", "unchanged-two-worktrees", "diverged", "head-moved"].contains(&protection);
    let project_override = match protection {
        "project-off" | "deleted-project-off" => json!({"worktreeCleanup": {"mode": "off"}}),
        "project-custom" | "deleted-project-custom" => json!({"worktreeCleanup": {"mode": "custom", "rules": {
            "worktreeAfterDays": if protection == "project-custom" { json!(8) } else { Value::Null },
            "worktreeOnDelete": delete_rule, "worktreeOnMerge": false, "worktreeUnchanged": false}}}),
        _ => json!({}),
    };
    let settings = MemorySettings::new(json!({
        "projectSettingsOverrides": {PROJECT_ID: project_override},
        "storageCleanup": {
            "worktreeAfterDays": if delete_rule || merge_rule || unchanged_rule || protection == "project-custom" { Value::Null } else { json!(8) },
            "worktreeOnDelete": delete_rule && protection != "deleted-project-custom",
            "worktreeOnMerge": merge_rule,
            "worktreeUnchanged": unchanged_rule,
            "browserArtifactsAfterDays": 8,
            "logsAfterDays": 8,
        },
    }));

    let scenario = Arc::new(Scenario {
        protection,
        base: base.clone(),
        worktree: worktree.clone(),
        second_worktree: second_worktree.clone(),
        thread,
        tombstoned: AtomicBool::new(delete_rule && protection != "deleted-event"),
        snapshot_reads: AtomicUsize::new(0),
        head_reads: AtomicUsize::new(0),
        default_ref_fetched: AtomicBool::new(false),
        fetches: AtomicUsize::new(0),
        removals: Mutex::new(Vec::new()),
        settings: settings.clone(),
        deletion_released: tokio::sync::Semaphore::new(if protection == "deleted-event" { 0 } else { 1_000 }),
        deletion_started: tokio::sync::Notify::new(),
        deletion_entered: AtomicBool::new(false),
        events: PubSub::new(),
    });

    let providers =
        FakeProviders::new((protection == "deleted-provider").then(|| FakeProviders::session("storage-thread", worktree.to_str().unwrap(), "codex")));
    let terminals = Arc::new(FakeTerminals::default());
    if protection == "terminal-cwd" || protection == "terminal-worktree" {
        let with_sep = format!("{}/", worktree.display());
        terminals.metadata.lock().unwrap().push(json!({
            "threadId": "terminal-thread", "terminalId": "default",
            "cwd": if protection == "terminal-cwd" { with_sep.clone() } else { base.to_string_lossy().into_owned() },
            "worktreePath": if protection == "terminal-worktree" { json!(with_sep) } else { Value::Null },
            "status": "running", "pid": 42, "exitCode": null, "exitSignal": null, "hasRunningSubprocess": false, "label": "shell", "updatedAt": CLEANUP_NOW,
        }));
    }
    let hook_settings = settings.clone();
    let (hook_image, hook_log) = (old_image.clone(), old_log.clone());
    let files_hook: Option<zc_checkpoints::storage_cleanup::FileCheckHook> = protection.starts_with("files-").then(|| {
        Arc::new(move |target: &Path| {
            if target != hook_image && target != hook_log {
                return;
            }
            let key = if target == hook_image { "browserArtifactsAfterDays" } else { "logsAfterDays" };
            let value = if protection == "files-disabled" { Value::Null } else { json!(60) };
            hook_settings.apply(json!({"storageCleanup": {key: value}}));
        }) as zc_checkpoints::storage_cleanup::FileCheckHook
    });
    let now = zc_core::time::parse_iso_millis(CLEANUP_NOW).unwrap();
    let cleanup = StorageCleanup::new(StorageCleanupDeps {
        paths,
        settings,
        projections: Arc::new(Reads(scenario.clone())),
        engine: Arc::new(Engine(scenario.clone())),
        thread_deletion: Arc::new(Deletion(scenario.clone())),
        providers,
        git: Arc::new(Git(scenario.clone())),
        git_manager: Arc::new(Manager(scenario.clone())),
        terminals,
        clock: Arc::new(move || now),
        file_check_hook: files_hook,
    });
    let _tasks = cleanup.start().await;
    settle().await;
    cleanup.drain().await;
    if protection == "deleted-event" {
        assert!(worktree.exists());
        scenario.tombstoned.store(true, Ordering::SeqCst);
        scenario.events.publish(decode(json!({
            "type": "thread.deleted", "sequence": 2, "eventId": "storage-thread-deleted", "aggregateKind": "thread", "aggregateId": "storage-thread",
            "occurredAt": CLEANUP_NOW, "commandId": null, "causationEventId": null, "correlationId": null, "metadata": {},
            "payload": {"threadId": "storage-thread", "deletedAt": CLEANUP_NOW},
        })));
        let entered = scenario.clone();
        wait_for(|| entered.deletion_entered.load(Ordering::SeqCst)).await;
        assert!(worktree.exists());
        scenario.deletion_released.add_permits(1_000);
        settle().await;
        cleanup.drain().await;
    }
    settle().await;
    cleanup.drain().await;

    let removed = [
        "project-custom",
        "deleted-project-custom",
        "none",
        "deleted",
        "deleted-event",
        "deleted-owner",
        "files-disabled",
        "files-extended",
        "merged",
        "unchanged",
        "unchanged-two-worktrees",
    ]
    .contains(&protection);
    assert_eq!(worktree.exists(), !removed, "{protection}: worktree");
    let expected_removals: Vec<String> = if protection == "unchanged-two-worktrees" {
        vec![worktree.to_string_lossy().into_owned(), second_worktree.to_string_lossy().into_owned()]
    } else if removed {
        vec![worktree.to_string_lossy().into_owned()]
    } else {
        vec![]
    };
    assert_eq!(*scenario.removals.lock().unwrap(), expected_removals, "{protection}: removals");
    assert_eq!(
        scenario.fetches.load(Ordering::SeqCst),
        usize::from(merge_rule || unchanged_rule),
        "{protection}: fetches"
    );
    let files = protection.starts_with("files-");
    assert_eq!(old_image.exists(), files, "{protection}: old image");
    assert!(recent_image.exists());
    assert_eq!(old_log.exists(), files, "{protection}: old log");
    assert!(active_log.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn retains_protected_worktrees_and_expires_only_old_artifacts_and_rotated_logs() {
    for protection in PROTECTIONS {
        run_case(protection).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn serializes_users_of_one_workspace_while_other_workspaces_can_start() {
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let entered = Arc::new(AtomicBool::new(false));
    let shared = PathBuf::from("/workspace/shared");
    let hold = gate.clone();
    let first = tokio::spawn(zc_terminal::lease::with_workspace_lease(Box::leak(Box::new(shared.clone())), async move {
        let _ = hold.acquire().await;
    }));
    tokio::time::sleep(Duration::from_millis(20)).await;
    let flag = entered.clone();
    let second = tokio::spawn(zc_terminal::lease::with_workspace_lease(Box::leak(Box::new(shared.clone())), async move {
        flag.store(true, Ordering::SeqCst);
    }));
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!entered.load(Ordering::SeqCst));
    zc_terminal::lease::with_workspace_lease(Path::new("/workspace/other"), async {}).await;
    gate.add_permits(1);
    first.await.unwrap();
    second.await.unwrap();
    assert!(entered.load(Ordering::SeqCst));
}
