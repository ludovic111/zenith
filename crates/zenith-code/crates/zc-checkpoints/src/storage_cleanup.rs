//! `storageCleanup.ts`: worktree, rotated log and browser artifact retention.
//!
//! An hourly sweep (and one per relevant settings change or `thread.deleted`) removes:
//! - thread worktrees under `<baseDir>/worktrees` that the project's `worktreeCleanup` rules
//!   allow (old, merged, unchanged, deleted thread), but only clean, idle, unshared linked
//!   worktrees whose ignored files are dependency installs, re-checking everything right
//!   before removal (under the workspace lease);
//! - browser artifacts and rotated logs (`*.log.N`, `*.ndjson.N`) older than their retention.
//!
//! Thread branch and worktree path are preserved: the provider command reactor recreates the
//! checkout when the thread resumes.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use zc_contracts::{OrchestrationEvent, OrchestrationLatestTurnState, OrchestrationSessionStatus, OrchestrationThreadShell, ProjectId, ThreadId};
use zc_ports::orchestration::DeletedWorktreeThread;
use zc_ports::{GitWorkflow, OrchestrationDispatch, ProjectionReads, ProviderService, SettingsService, TerminalManager};
use zc_terminal::contracts::{TerminalMetadataStreamEvent, TerminalSessionStatus, TerminalSummary};
use zc_vcs::contracts::VcsRemoveWorktreeInput;
use zc_vcs::git_exec::ExecuteGitInput;

use crate::bootstrap::ThreadDeletionDrain;
use crate::reactor::ReactorTasks;
use crate::support::ProviderSessionExt;
use crate::worker::DrainableWorker;

const DAY_MS: f64 = 86_400_000.0;
/// The periodic sweep.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);
const IGNORED_FILES_MAX_BYTES: usize = 64 * 1024;
/// `QUEUED_TURN_START_GRACE_MS` (`ThreadSettlementPolicy.ts`).
const QUEUED_TURN_START_GRACE_MS: f64 = 2.0 * 60.0 * 1_000.0;

/// `WorktreeCleanupRules`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorktreeCleanupRules {
    pub worktree_after_days: Option<f64>,
    pub worktree_on_merge: bool,
    pub worktree_on_delete: bool,
    pub worktree_unchanged: bool,
}

impl WorktreeCleanupRules {
    fn off() -> Self {
        Self {
            worktree_after_days: None,
            worktree_on_merge: false,
            worktree_on_delete: false,
            worktree_unchanged: false,
        }
    }

    fn from_json(value: &Value) -> Self {
        Self {
            worktree_after_days: value.get("worktreeAfterDays").and_then(Value::as_f64),
            worktree_on_merge: value.get("worktreeOnMerge").and_then(Value::as_bool).unwrap_or(false),
            worktree_on_delete: value.get("worktreeOnDelete").and_then(Value::as_bool).unwrap_or(false),
            worktree_unchanged: value.get("worktreeUnchanged").and_then(Value::as_bool).unwrap_or(false),
        }
    }

    /// `worktreeCleanupEnabled`.
    pub fn enabled(&self) -> bool {
        self.worktree_after_days.is_some() || self.worktree_on_merge || self.worktree_on_delete || self.worktree_unchanged
    }
}

/// `resolveWorktreeCleanup(settings, projectId)` (`packages/shared/src/projectSettings.ts`):
/// worktree rules are project-scoped; artifact and log retention stays environment-wide.
pub fn resolve_worktree_cleanup(settings: &Value, project_id: Option<&str>) -> WorktreeCleanupRules {
    let project_policy = project_id.and_then(|id| {
        settings
            .get("projectSettingsOverrides")
            .and_then(|overrides| overrides.get(id))
            .and_then(|entry| entry.get("worktreeCleanup"))
    });
    let policy = project_policy.or_else(|| settings.get("worktreeCleanup")).filter(|p| !p.is_null());
    match policy.and_then(|p| p.get("mode")).and_then(Value::as_str) {
        Some("custom") => WorktreeCleanupRules::from_json(policy.and_then(|p| p.get("rules")).unwrap_or(&Value::Null)),
        Some("off") => WorktreeCleanupRules::off(),
        _ => WorktreeCleanupRules::from_json(settings.get("storageCleanup").unwrap_or(&Value::Null)),
    }
}

fn project_override_ids(settings: &Value) -> Vec<String> {
    settings
        .get("projectSettingsOverrides")
        .and_then(Value::as_object)
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

/// `anyWorktreePolicy(settings, predicate)`.
fn any_worktree_policy(settings: &Value, predicate: impl Fn(&WorktreeCleanupRules) -> bool) -> bool {
    predicate(&resolve_worktree_cleanup(settings, None))
        || project_override_ids(settings)
            .iter()
            .any(|id| predicate(&resolve_worktree_cleanup(settings, Some(id))))
}

/// `sameProjectWorktreePolicies`.
fn same_project_worktree_policies(left: &Value, right: &Value) -> bool {
    let ids: HashSet<String> = project_override_ids(left).into_iter().chain(project_override_ids(right)).collect();
    ids.iter().all(|id| {
        let policy = |settings: &Value| {
            settings
                .pointer(&format!("/projectSettingsOverrides/{}/worktreeCleanup", escape_pointer(id)))
                .cloned()
        };
        policy(left) == policy(right)
    })
}

fn escape_pointer(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

fn js_date(value: &str) -> f64 {
    zc_core::time::parse_iso_millis(value).map_or(f64::NAN, |ms| ms as f64)
}

/// `threadHasQueuedTurnStart` (`ThreadSettlementPolicy.ts`).
fn thread_has_queued_turn_start(thread: &OrchestrationThreadShell, now_ms: f64) -> bool {
    let Some(latest_user_message_at) = thread.latest_user_message_at.as_deref() else {
        return false;
    };
    if thread.session.as_ref().map(|s| s.status) == Some(OrchestrationSessionStatus::Error) {
        return false;
    }
    let message_at = js_date(latest_user_message_at);
    let age = now_ms - message_at;
    if age.is_nan() || age.abs() > QUEUED_TURN_START_GRACE_MS {
        return false;
    }
    let Some(turn) = &thread.latest_turn else { return true };
    [Some(turn.requested_at.as_str()), turn.started_at.as_deref(), turn.completed_at.as_deref()]
        .into_iter()
        .all(|value| value.is_none_or(|value| js_date(value) < message_at))
}

/// `storageCleanupThreadIdle`: live sessions keep their cwd even between turns.
fn thread_idle(thread: &OrchestrationThreadShell, now_ms: f64) -> bool {
    thread.branch.is_some()
        && thread.worktree_path.is_some()
        && thread.session.as_ref().is_none_or(|s| s.status == OrchestrationSessionStatus::Stopped)
        && thread.latest_turn.as_ref().is_none_or(|t| t.state != OrchestrationLatestTurnState::Running)
        && thread.background_liveness.as_ref().is_none_or(Option::is_none)
        && !thread.has_pending_approvals
        && !thread.has_pending_user_input
        && !thread_has_queued_turn_start(thread, now_ms)
}

/// `storageCleanupActivityAt`: PR metadata refreshes must not reset the inactivity clock.
fn activity_at(thread: &OrchestrationThreadShell) -> f64 {
    let turn = thread.latest_turn.as_ref();
    [
        Some(thread.created_at.as_str()),
        thread.latest_user_message_at.as_deref(),
        turn.map(|t| t.requested_at.as_str()),
        turn.and_then(|t| t.started_at.as_deref()),
        turn.and_then(|t| t.completed_at.as_deref()),
    ]
    .into_iter()
    .flatten()
    .map(js_date)
    .fold(
        f64::NEG_INFINITY,
        |max, value| if max.is_nan() || value.is_nan() { f64::NAN } else { max.max(value) },
    )
}

/// `path.resolve(p)` for an absolute path: lexical normalization, no symlinks.
fn resolve(path: &str) -> PathBuf {
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// `inside(root, target)`: strictly below `root`.
fn inside(root: &Path, target: &Path) -> bool {
    target != root && target.starts_with(root)
}

fn ignored_files_block_removal(stdout: &str, truncated: bool) -> bool {
    static NODE_MODULES: OnceLock<Regex> = OnceLock::new();
    let node_modules = NODE_MODULES.get_or_init(|| Regex::new(r"(^|/)node_modules/$").expect("valid regex"));
    // Ignored files can hold secrets or datasets; dependency installs are reproducible.
    truncated || stdout.split('\0').any(|entry| !entry.is_empty() && !node_modules.is_match(entry))
}

/// The local status `statusDetailsLocal` reports, as cleanup reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalStatus {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub has_working_tree_changes: bool,
}

/// The `GitVcsDriver` calls cleanup makes (a trait so tests can script them).
#[async_trait]
pub trait CleanupGit: Send + Sync {
    async fn status_details_local(&self, cwd: &str) -> Result<LocalStatus, String>;
    /// `resolveCommit({cwd, revision})`: the commit sha.
    async fn resolve_commit(&self, cwd: &str, revision: &str) -> Result<String, String>;
    /// `ls-files --others --ignored --exclude-standard --directory -z` capped at 64 KiB:
    /// `(stdout, truncated)`.
    async fn ignored_files(&self, cwd: &str) -> Result<(String, bool), String>;
    /// `merge-base --is-ancestor <ancestor> <descendant>` exited 0.
    async fn is_ancestor(&self, cwd: &str, ancestor: &str, descendant: &str) -> Result<bool, String>;
    async fn resolve_primary_remote_name(&self, cwd: &str) -> Result<String, String>;
    async fn resolve_default_branch_name(&self, cwd: &str, remote: &str) -> Result<Option<String>, String>;
    async fn fetch_remote_tracking_branch(&self, cwd: &str, remote: &str, branch: &str) -> Result<(), String>;
    async fn remove_worktree(&self, cwd: &str, path: &str, force: bool) -> Result<(), String>;
}

#[async_trait]
impl CleanupGit for zc_vcs::GitVcsDriver {
    async fn status_details_local(&self, cwd: &str) -> Result<LocalStatus, String> {
        let status = zc_vcs::GitVcsDriver::status_details_local(self, cwd).await.map_err(|e| e.to_string())?;
        Ok(LocalStatus {
            is_repo: status.is_repo,
            branch: status.branch,
            has_working_tree_changes: status.has_working_tree_changes,
        })
    }

    async fn resolve_commit(&self, cwd: &str, revision: &str) -> Result<String, String> {
        zc_vcs::GitVcsDriver::resolve_commit(self, cwd, revision).await.map_err(|e| e.to_string())
    }

    async fn ignored_files(&self, cwd: &str) -> Result<(String, bool), String> {
        let mut input = ExecuteGitInput::new(
            "StorageCleanup.ignoredFiles",
            cwd,
            ["ls-files", "--others", "--ignored", "--exclude-standard", "--directory", "-z"],
        );
        input.max_output_bytes = Some(IGNORED_FILES_MAX_BYTES);
        let result = self.execute(input).await.map_err(|e| e.to_string())?;
        Ok((result.stdout, result.stdout_truncated))
    }

    async fn is_ancestor(&self, cwd: &str, ancestor: &str, descendant: &str) -> Result<bool, String> {
        let mut input = ExecuteGitInput::new("StorageCleanup.integratedBranch", cwd, ["merge-base", "--is-ancestor", ancestor, descendant]);
        input.allow_non_zero_exit = true;
        Ok(self.execute(input).await.map_err(|e| e.to_string())?.exit_code == 0)
    }

    async fn resolve_primary_remote_name(&self, cwd: &str) -> Result<String, String> {
        zc_vcs::GitVcsDriver::resolve_primary_remote_name(self, cwd).await.map_err(|e| e.to_string())
    }

    async fn resolve_default_branch_name(&self, cwd: &str, remote: &str) -> Result<Option<String>, String> {
        zc_vcs::GitVcsDriver::resolve_default_branch_name(self, cwd, remote)
            .await
            .map_err(|e| e.to_string())
    }

    async fn fetch_remote_tracking_branch(&self, cwd: &str, remote: &str, branch: &str) -> Result<(), String> {
        zc_vcs::GitVcsDriver::fetch_remote_tracking_branch(self, cwd, remote, branch)
            .await
            .map_err(|e| e.to_string())
    }

    async fn remove_worktree(&self, cwd: &str, path: &str, force: bool) -> Result<(), String> {
        let input = VcsRemoveWorktreeInput {
            cwd: cwd.to_owned(),
            path: path.to_owned(),
            force: Some(force),
        };
        zc_vcs::GitVcsDriver::remove_worktree(self, &input).await.map_err(|e| e.to_string())
    }
}

/// The directories cleanup owns (`ServerConfig`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageCleanupPaths {
    pub worktrees_dir: PathBuf,
    pub browser_artifacts_dir: PathBuf,
    pub logs_dir: PathBuf,
}

/// What cleanup is built from.
#[derive(Clone)]
pub struct StorageCleanupDeps {
    pub paths: StorageCleanupPaths,
    pub settings: Arc<dyn SettingsService>,
    pub projections: Arc<dyn ProjectionReads>,
    pub engine: Arc<dyn OrchestrationDispatch>,
    pub thread_deletion: Arc<dyn ThreadDeletionDrain>,
    pub providers: Arc<dyn ProviderService>,
    pub git: Arc<dyn CleanupGit>,
    /// `GitManager.branchPullRequest` / `invalidateStatus`.
    pub git_manager: Arc<dyn GitWorkflow>,
    pub terminals: Arc<dyn TerminalManager>,
    /// `Clock.currentTimeMillis`.
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Called with each file before its age is checked (the TS tests' `FileSystem.stat`
    /// seam). `None` in production.
    pub file_check_hook: Option<FileCheckHook>,
}

/// See [`StorageCleanupDeps::file_check_hook`].
pub type FileCheckHook = Arc<dyn Fn(&Path) + Send + Sync>;

/// One cleanup candidate: a live thread shell or a deleted thread's tombstone.
enum Candidate {
    Live(Box<OrchestrationThreadShell>),
    Deleted(DeletedWorktreeThread),
}

impl Candidate {
    fn id(&self) -> &ThreadId {
        match self {
            Self::Live(thread) => &thread.id,
            Self::Deleted(thread) => &thread.id,
        }
    }
    fn project_id(&self) -> &ProjectId {
        match self {
            Self::Live(thread) => &thread.project_id,
            Self::Deleted(thread) => &thread.project_id,
        }
    }
    fn branch(&self) -> Option<&str> {
        match self {
            Self::Live(thread) => thread.branch.as_deref(),
            Self::Deleted(thread) => Some(&thread.branch),
        }
    }
    fn worktree_path(&self) -> &str {
        match self {
            Self::Live(thread) => thread.worktree_path.as_deref().unwrap_or_default(),
            Self::Deleted(thread) => &thread.worktree_path,
        }
    }
}

struct Snapshot {
    project_roots: Vec<(ProjectId, String)>,
    threads: Vec<OrchestrationThreadShell>,
}

struct Inner {
    deps: StorageCleanupDeps,
    live_terminals: Mutex<HashMap<String, HashMap<String, TerminalSummary>>>,
    last_settings: Mutex<Value>,
}

/// `StorageCleanup`.
#[derive(Clone)]
pub struct StorageCleanup {
    inner: Arc<Inner>,
    worker: DrainableWorker<()>,
}

type Outcome<T> = Result<T, String>;

impl StorageCleanup {
    pub fn new(deps: StorageCleanupDeps) -> Self {
        let inner = Arc::new(Inner {
            deps,
            live_terminals: Mutex::new(HashMap::new()),
            last_settings: Mutex::new(Value::Null),
        });
        let sweeper = inner.clone();
        let worker = DrainableWorker::new(move |()| {
            let inner = sweeper.clone();
            async move { inner.sweep().await }
        });
        Self { inner, worker }
    }

    /// `start()`: track live terminals, sweep now and hourly, and again on a relevant settings
    /// change or a `thread.deleted` while some project removes worktrees on delete.
    pub async fn start(&self) -> ReactorTasks {
        let mut terminal_metadata = self.inner.deps.terminals.subscribe_metadata();
        let mut settings_changes = self.inner.deps.settings.subscribe_changes();
        let mut events = self.inner.deps.engine.subscribe_domain_events();
        let initial = match self.inner.deps.settings.get_settings().await {
            Ok(settings) => serde_json::to_value(settings).unwrap_or(Value::Null),
            Err(_) => Value::Null,
        };
        *self.inner.last_settings.lock().unwrap() = initial;

        // The terminal manager emits its snapshot first: take what is ready now, so the first
        // sweep already knows the live terminals (the TS listener runs synchronously).
        while let Some(Some(event)) = futures::FutureExt::now_or_never(terminal_metadata.next()) {
            if let Ok(event) = serde_json::from_value::<TerminalMetadataStreamEvent>(event.0) {
                self.inner.note_terminal_event(event);
            }
        }
        let inner = self.inner.clone();
        let terminals_task = tokio::spawn(async move {
            while let Some(event) = terminal_metadata.next().await {
                let Ok(event) = serde_json::from_value::<TerminalMetadataStreamEvent>(event.0) else {
                    continue;
                };
                inner.note_terminal_event(event);
            }
        });
        let worker = self.worker.clone();
        let periodic_task = tokio::spawn(async move {
            loop {
                worker.enqueue(());
                worker.drain().await;
                tokio::time::sleep(SWEEP_INTERVAL).await;
            }
        });
        let inner = self.inner.clone();
        let worker = self.worker.clone();
        let settings_task = tokio::spawn(async move {
            while let Some(settings) = settings_changes.next().await {
                let settings = serde_json::to_value(settings).unwrap_or(Value::Null);
                let mut last = inner.last_settings.lock().unwrap();
                if settings.get("storageCleanup") == last.get("storageCleanup")
                    && settings.get("worktreeCleanup") == last.get("worktreeCleanup")
                    && same_project_worktree_policies(&settings, &last)
                {
                    continue;
                }
                *last = settings;
                drop(last);
                worker.enqueue(());
            }
        });
        let inner = self.inner.clone();
        let worker = self.worker.clone();
        let events_task = tokio::spawn(async move {
            while let Some(event) = events.next().await {
                if matches!(event, OrchestrationEvent::ThreadDeleted(_))
                    && any_worktree_policy(&inner.last_settings.lock().unwrap(), |rules| rules.worktree_on_delete)
                {
                    worker.enqueue(());
                }
            }
        });
        ReactorTasks::new(vec![terminals_task, periodic_task, settings_task, events_task])
    }

    /// `drain`.
    pub async fn drain(&self) {
        self.worker.drain().await;
    }

    /// One sweep now (what the worker runs).
    pub async fn sweep(&self) {
        self.inner.sweep().await;
    }
}

impl Inner {
    fn note_terminal_event(&self, event: TerminalMetadataStreamEvent) {
        let mut live = self.live_terminals.lock().unwrap();
        match event {
            TerminalMetadataStreamEvent::Snapshot { terminals } => {
                live.clear();
                for terminal in terminals {
                    live.entry(terminal.thread_id.clone())
                        .or_default()
                        .insert(terminal.terminal_id.clone(), terminal);
                }
            }
            TerminalMetadataStreamEvent::Upsert { terminal } => {
                live.entry(terminal.thread_id.clone())
                    .or_default()
                    .insert(terminal.terminal_id.clone(), terminal);
            }
            TerminalMetadataStreamEvent::Remove { thread_id, terminal_id } => {
                if let Some(terminals) = live.get_mut(&thread_id) {
                    terminals.remove(&terminal_id);
                    if terminals.is_empty() {
                        live.remove(&thread_id);
                    }
                }
            }
        }
    }

    /// `hasTerminal(worktreePath)`: a running terminal works in or below the checkout.
    fn has_terminal(&self, worktree_path: &Path) -> bool {
        self.live_terminals.lock().unwrap().values().flat_map(HashMap::values).any(|terminal| {
            if !matches!(terminal.status, TerminalSessionStatus::Starting | TerminalSessionStatus::Running) {
                return false;
            }
            let cwd = resolve(&terminal.cwd);
            terminal.worktree_path.as_deref().is_some_and(|w| resolve(w) == worktree_path) || cwd == worktree_path || inside(worktree_path, &cwd)
        })
    }

    async fn settings(&self) -> Outcome<Value> {
        let settings = self
            .deps
            .settings
            .get_settings()
            .await
            .map_err(|e| zc_settings::errors::settings_error_message(&e))?;
        serde_json::to_value(settings).map_err(|e| e.to_string())
    }

    async fn read_threads(&self) -> Outcome<Snapshot> {
        let active = self.deps.projections.get_shell_snapshot(false).await.map_err(|e| e.to_string())?;
        let archived = self.deps.projections.get_archived_shell_snapshot().await.map_err(|e| e.to_string())?;
        Ok(Snapshot {
            project_roots: active.projects.iter().map(|p| (p.id.clone(), p.workspace_root.clone())).collect(),
            threads: active.threads.into_iter().chain(archived.threads).collect(),
        })
    }

    /// `containsProjectRoot`: a local thread of another project can live inside a worktree.
    fn contains_project_root(worktree_path: &Path, roots: &[String]) -> bool {
        roots.iter().any(|root| {
            let project_path = resolve(root);
            if project_path == worktree_path || inside(worktree_path, &project_path) {
                return true;
            }
            let real = std::fs::canonicalize(&project_path).unwrap_or(project_path);
            real == worktree_path || inside(worktree_path, &real)
        })
    }

    async fn sweep(&self) {
        let settings = match self.settings().await {
            Ok(settings) => settings,
            Err(error) => {
                tracing::warn!(error, "storage cleanup failed");
                return;
            }
        };
        let now = (self.deps.clock)();
        if let Err(error) = self.clean_worktrees(&settings, now).await {
            tracing::warn!(error, "worktree cleanup failed");
        }
        let retention = settings.get("storageCleanup").cloned().unwrap_or(Value::Null);
        let days = |key: &str| retention.get(key).and_then(Value::as_f64);
        if let Err(error) = self
            .clean_files(&self.deps.paths.browser_artifacts_dir, days("browserArtifactsAfterDays"), now, false)
            .await
        {
            tracing::warn!(error, "browser artifact cleanup failed");
        }
        if let Err(error) = self.clean_files(&self.deps.paths.logs_dir, days("logsAfterDays"), now, true).await {
            tracing::warn!(error, "rotated log cleanup failed");
        }
    }

    async fn clean_worktrees(&self, settings: &Value, now: i64) -> Outcome<()> {
        if !any_worktree_policy(settings, WorktreeCleanupRules::enabled) || !self.deps.paths.worktrees_dir.exists() {
            return Ok(());
        }
        let has_delete_rule = any_worktree_policy(settings, |rules| rules.worktree_on_delete);
        let deleted: Vec<DeletedWorktreeThread> = if has_delete_rule {
            self.deps
                .projections
                .get_deleted_worktree_threads()
                .await
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|thread| resolve_worktree_cleanup(settings, Some(thread.project_id.as_str())).worktree_on_delete)
                .collect()
        } else {
            Vec::new()
        };
        if !deleted.is_empty() {
            // Tombstones are read before this fence: each must finish stopping its resources.
            let sequence = self.deps.projections.get_snapshot_sequence().await.map_err(|e| e.to_string())?;
            self.deps.thread_deletion.drain_through(sequence).await;
        }
        let snapshot = self.read_threads().await?;
        let root = std::fs::canonicalize(&self.deps.paths.worktrees_dir).map_err(|e| e.to_string())?;

        // Worktrees owned by exactly one thread, in first-seen order, then tombstones whose
        // path no live thread uses.
        let mut groups: Vec<(PathBuf, Vec<&OrchestrationThreadShell>)> = Vec::new();
        for thread in snapshot.threads.iter().filter(|t| t.worktree_path.is_some()) {
            let key = resolve(thread.worktree_path.as_deref().unwrap_or_default());
            match groups.iter_mut().find(|(path, _)| *path == key) {
                Some((_, members)) => members.push(thread),
                None => groups.push((key, vec![thread])),
            }
        }
        let mut candidates: Vec<Candidate> = groups
            .iter()
            .filter(|(_, members)| members.len() == 1)
            .map(|(_, members)| Candidate::Live(Box::new(members[0].clone())))
            .collect();
        for thread in deleted {
            let key = resolve(&thread.worktree_path);
            if !groups.iter().any(|(path, _)| *path == key) {
                candidates.push(Candidate::Deleted(thread));
            }
        }

        let mut refreshed_default_refs: HashMap<String, HashSet<String>> = HashMap::new();
        for candidate in candidates {
            let rules = resolve_worktree_cleanup(settings, Some(candidate.project_id().as_str()));
            if !rules.enabled() {
                continue;
            }
            let worktree_path = resolve(candidate.worktree_path());
            let project_root = match &candidate {
                Candidate::Deleted(thread) => Some(thread.workspace_root.clone()),
                Candidate::Live(thread) => snapshot
                    .project_roots
                    .iter()
                    .find(|(id, _)| *id == thread.project_id)
                    .map(|(_, root)| root.clone()),
            };
            let Some(project_root) = project_root else { continue };
            if let Candidate::Live(thread) = &candidate {
                if !thread_idle(thread, now as f64) {
                    continue;
                }
            }
            if self.has_terminal(&worktree_path) {
                continue;
            }
            let lease_path = worktree_path.clone();
            let outcome = zc_terminal::lease::with_workspace_lease(&lease_path, async {
                self.clean_one(&candidate, &rules, &worktree_path, &project_root, &root, now, &mut refreshed_default_refs)
                    .await
            })
            .await;
            if let Err(error) = outcome {
                tracing::debug!(thread_id = %candidate.id(), error, "storage cleanup skipped worktree");
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn clean_one(
        &self,
        candidate: &Candidate,
        rules: &WorktreeCleanupRules,
        worktree_path: &Path,
        project_root: &str,
        root: &Path,
        now: i64,
        refreshed_default_refs: &mut HashMap<String, HashSet<String>>,
    ) -> Outcome<()> {
        let worktree = worktree_path.to_string_lossy().into_owned();
        if !inside(root, worktree_path) || !worktree_path.exists() {
            return Ok(());
        }
        if std::fs::canonicalize(worktree_path).map_err(|e| e.to_string())? != worktree_path {
            return Ok(());
        }
        let snapshot = self.read_threads().await?;
        let mut roots = vec![project_root.to_owned()];
        roots.extend(snapshot.project_roots.iter().map(|(_, r)| r.clone()));
        if Self::contains_project_root(worktree_path, &roots) {
            return Ok(());
        }
        // A linked worktree has a `.git` file. Never remove a main checkout.
        if !std::fs::metadata(worktree_path.join(".git")).map_err(|e| e.to_string())?.is_file() {
            return Ok(());
        }
        let status = self.deps.git.status_details_local(&worktree).await?;
        if !status.is_repo || status.branch.as_deref() != candidate.branch() || status.has_working_tree_changes {
            return Ok(());
        }
        let head = self.deps.git.resolve_commit(&worktree, "HEAD").await?;
        let (ignored, truncated) = self.deps.git.ignored_files(&worktree).await?;
        if ignored_files_block_removal(&ignored, truncated) {
            return Ok(());
        }
        let deleted = matches!(candidate, Candidate::Deleted(_));
        let old = match (candidate, rules.worktree_after_days) {
            (Candidate::Live(thread), Some(days)) => activity_at(thread) < now as f64 - days * DAY_MS,
            _ => false,
        };
        let mut eligible = deleted || old;
        if !eligible && (rules.worktree_unchanged || rules.worktree_on_merge) {
            let repository_cwd = resolve(project_root).to_string_lossy().into_owned();
            let remote = self.deps.git.resolve_primary_remote_name(&repository_cwd).await?;
            let Some(branch) = self.deps.git.resolve_default_branch_name(&repository_cwd, &remote).await? else {
                return Ok(());
            };
            let default_ref = format!("refs/remotes/{remote}/{branch}");
            let refreshed = refreshed_default_refs.entry(repository_cwd.clone()).or_default();
            if !refreshed.contains(&default_ref) {
                self.deps.git.fetch_remote_tracking_branch(&repository_cwd, &remote, &branch).await?;
                refreshed.insert(default_ref.clone());
            }
            let base = self.deps.git.resolve_commit(&worktree, &default_ref).await?;
            if !self.deps.git.is_ancestor(&worktree, &head, &base).await? {
                return Ok(());
            }
            eligible = rules.worktree_unchanged;
            if !eligible && rules.worktree_on_merge {
                if let Some(branch) = candidate.branch() {
                    let pull_request = self
                        .deps
                        .git_manager
                        .branch_pull_request(&worktree, branch, true)
                        .await
                        .map_err(|e| e.to_string())?;
                    eligible = pull_request.is_some_and(|pr| pr.pull_request.0.get("state").and_then(Value::as_str) == Some("merged"));
                }
            }
        }
        if !eligible {
            return Ok(());
        }

        // Re-read after the git and host calls: a queued turn, a resumed session or a new
        // thread sharing this path cancels the removal.
        let latest = self.read_threads().await?;
        let mut roots = vec![project_root.to_owned()];
        roots.extend(latest.project_roots.iter().map(|(_, r)| r.clone()));
        if Self::contains_project_root(worktree_path, &roots) {
            return Ok(());
        }
        let sharing: Vec<&OrchestrationThreadShell> = latest
            .threads
            .iter()
            .filter(|t| t.worktree_path.as_deref().is_some_and(|w| resolve(w) == worktree_path))
            .collect();
        if self.has_terminal(worktree_path) {
            return Ok(());
        }
        match candidate {
            Candidate::Deleted(thread) => {
                if !sharing.is_empty() || !resolve_worktree_cleanup(&self.settings().await?, Some(thread.project_id.as_str())).worktree_on_delete {
                    return Ok(());
                }
                // A failed session stop is logged by the deletion reactor; its drain alone does
                // not prove a provider released this checkout.
                let busy = self.deps.providers.list_sessions().await.into_iter().any(|session| {
                    session.status() != Some("closed")
                        && (session.thread_id() == Some(thread.id.as_str())
                            || session.cwd().is_some_and(|cwd| {
                                let cwd = resolve(cwd);
                                cwd == worktree_path || inside(worktree_path, &cwd)
                            }))
                });
                if busy {
                    return Ok(());
                }
            }
            Candidate::Live(thread) => {
                let [only] = sharing.as_slice() else { return Ok(()) };
                if only.id != thread.id || !thread_idle(only, now as f64) || activity_at(only) != activity_at(thread) {
                    return Ok(());
                }
            }
        }
        let final_status = self.deps.git.status_details_local(&worktree).await?;
        if !final_status.is_repo || final_status.branch.as_deref() != candidate.branch() || final_status.has_working_tree_changes {
            return Ok(());
        }
        if self.deps.git.resolve_commit(&worktree, "HEAD").await? != head {
            return Ok(());
        }
        let (ignored, truncated) = self.deps.git.ignored_files(&worktree).await?;
        if ignored_files_block_removal(&ignored, truncated) {
            return Ok(());
        }
        let current = resolve_worktree_cleanup(&self.settings().await?, Some(candidate.project_id().as_str()));
        if current != *rules {
            return Ok(());
        }
        self.deps.git.remove_worktree(project_root, &worktree, false).await?;
        self.deps.git_manager.invalidate_status(project_root).await;
        // Branch and path stay: the provider command reactor recreates the checkout on resume.
        tracing::info!(thread_id = %candidate.id(), "storage cleanup removed worktree");
        Ok(())
    }

    async fn clean_files(&self, root: &Path, days: Option<f64>, now: i64, rotated_logs: bool) -> Outcome<()> {
        let Some(days) = days else { return Ok(()) };
        if !root.exists() {
            return Ok(());
        }
        let real_root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
        if real_root != resolve(&root.to_string_lossy()) {
            return Ok(());
        }
        static ROTATED: OnceLock<Regex> = OnceLock::new();
        let rotated = ROTATED.get_or_init(|| Regex::new(r"\.(?:log|ndjson)\.\d+$").expect("valid regex"));
        let mut directories = vec![real_root.clone()];
        while let Some(directory) = directories.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&directory).map_err(|e| e.to_string())?.flatten().collect();
            entries.sort_by_key(|entry| entry.file_name());
            // A retention change mid-sweep stops this directory (TS returns from its `visit`).
            'entries: for entry in entries {
                let target = directory.join(entry.file_name());
                let real = std::fs::canonicalize(&target).map_err(|e| e.to_string())?;
                if real != target || !inside(&real_root, &target) {
                    continue;
                }
                if let Some(hook) = &self.deps.file_check_hook {
                    hook(&target);
                }
                let metadata = std::fs::metadata(&target).map_err(|e| e.to_string())?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if metadata.is_dir() && rotated_logs {
                    directories.push(target);
                } else if metadata.is_file() && (!rotated_logs || rotated.is_match(&name)) {
                    let modified = metadata
                        .modified()
                        .ok()
                        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|age| age.as_millis() as f64);
                    if modified.is_some_and(|modified| modified < now as f64 - days * DAY_MS) {
                        let current = self.settings().await?;
                        let key = if rotated_logs { "logsAfterDays" } else { "browserArtifactsAfterDays" };
                        if current.pointer(&format!("/storageCleanup/{key}")).and_then(Value::as_f64) != Some(days) {
                            break 'entries;
                        }
                        std::fs::remove_file(&target).map_err(|e| e.to_string())?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// A settings JSON with only what cleanup reads, for tests and examples.
pub fn storage_cleanup_settings(storage_cleanup: Value) -> Value {
    json!({"storageCleanup": storage_cleanup, "projectSettingsOverrides": {}})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worktree_rules_resolve_project_over_environment() {
        let settings = json!({
            "storageCleanup": {"worktreeAfterDays": 8, "worktreeOnMerge": false, "worktreeOnDelete": true, "worktreeUnchanged": false},
            "projectSettingsOverrides": {
                "off": {"worktreeCleanup": {"mode": "off"}},
                "custom": {"worktreeCleanup": {"mode": "custom", "rules": {"worktreeAfterDays": null, "worktreeOnMerge": true, "worktreeOnDelete": false, "worktreeUnchanged": false}}},
            },
        });
        assert_eq!(resolve_worktree_cleanup(&settings, None).worktree_after_days, Some(8.0));
        assert!(!resolve_worktree_cleanup(&settings, Some("off")).enabled());
        let custom = resolve_worktree_cleanup(&settings, Some("custom"));
        assert!(custom.worktree_on_merge && custom.worktree_after_days.is_none());
        assert!(resolve_worktree_cleanup(&settings, Some("other")).worktree_on_delete);
        assert!(any_worktree_policy(&settings, |r| r.worktree_on_merge));
        assert!(same_project_worktree_policies(&settings, &settings));
        assert!(!same_project_worktree_policies(&settings, &json!({"projectSettingsOverrides": {}})));
    }

    #[test]
    fn only_dependency_installs_may_be_ignored() {
        assert!(!ignored_files_block_removal("", false));
        assert!(!ignored_files_block_removal("node_modules/\0pkg/node_modules/\0", false));
        assert!(ignored_files_block_removal(".env\0", false));
        assert!(ignored_files_block_removal(".cache/\0", false));
        assert!(ignored_files_block_removal("", true));
    }

    #[test]
    fn paths_resolve_lexically() {
        assert_eq!(resolve("/a/b/../c/./d/"), PathBuf::from("/a/c/d"));
        assert!(inside(Path::new("/a"), Path::new("/a/b")));
        assert!(!inside(Path::new("/a"), Path::new("/a")));
        assert!(!inside(Path::new("/a"), Path::new("/ab")));
    }
}
