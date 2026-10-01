//! `orchestration/ThreadSettlementReactor.ts`: settles idle threads on their own, every minute,
//! after settings changes that touch settlement, after a pull request this server saw merge,
//! and after a thread's links change or its session leaves running/starting.
//!
//! A thread settles when inactive for `sidebarAutoSettleAfterDays`, or when its pull request
//! closed (or merged, with `sidebarAutoSettleOnMerge`) after the user last worked on it. The
//! inactivity decisions run first, before any source control lookup can fail or wait; linked
//! pull requests use their synced snapshots, unlinked branches ask the git manager (grouped so
//! threads sharing a branch share one lookup).

pub mod policy;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::future::join_all;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zc_contracts::{OrchestrationEvent, ProjectId, ThreadId};
use zc_ports::contracts::PullRequestRef;
use zc_ports::pull_requests::PullRequestMergeEvent;
use zc_ports::{GitWorkflow, OrchestrationDispatch, PullRequests, SettingsService, TaggedError};

use crate::common::{dispatch_json, pretty, UuidSource};
use crate::js::str_of;
use crate::reads::ReactorReads;
use crate::runtime::{DrainableWorker, ReactorClock};
use crate::settings::{resolve_project_settings, settings_json};

use policy::{is_auto_settlement_candidate, resolve_auto_settlement_at, SettlementPullRequest};

/// Whether a path exists (`FileSystem.exists`), injectable for tests.
pub type PathExists = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// What the settlement reactor is built from.
#[derive(Clone)]
pub struct SettlementDeps {
    pub engine: Arc<dyn OrchestrationDispatch>,
    pub reads: Arc<dyn ReactorReads>,
    pub settings: Arc<dyn SettingsService>,
    pub git: Arc<dyn GitWorkflow>,
    pub pull_requests: Arc<dyn PullRequests>,
    pub clock: Arc<dyn ReactorClock>,
    pub uuids: UuidSource,
    pub path_exists: PathExists,
    /// The periodic sweep interval (one minute). `None`: no timer, sweeps run on events and
    /// on [`ThreadSettlementReactor::tick`] (tests drive time that way).
    pub interval: Option<Duration>,
}

/// `Schedule.spaced("1 minute")`.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// The real file system's `exists`.
pub fn fs_path_exists() -> PathExists {
    Arc::new(|path: &str| std::path::Path::new(path).exists())
}

/// `autoSettlementConfigured(settings)`: any environment default or project override can
/// settle a thread.
pub fn auto_settlement_configured(settings: &Value) -> bool {
    if settings["sidebarAutoSettleOnMerge"] == json!(true) || !settings["sidebarAutoSettleAfterDays"].is_null() {
        return true;
    }
    settings["projectSettingsOverrides"].as_object().into_iter().flatten().any(|(_, entry)| {
        entry.get("sidebarAutoSettleOnMerge") == Some(&json!(true)) || entry.get("sidebarAutoSettleAfterDays").is_some_and(|days| !days.is_null())
    })
}

/// `autoSettlementSettingsKey(settings)`: the identity of every settlement input, so unrelated
/// settings edits do not queue a sweep.
pub fn auto_settlement_settings_key(settings: &Value) -> String {
    let mut overrides: Vec<(&String, &Value)> = settings["projectSettingsOverrides"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, entry)| entry.get("sidebarAutoSettleOnMerge").is_some() || entry.get("sidebarAutoSettleAfterDays").is_some())
        .collect();
    overrides.sort_by_key(|(key, _)| *key);
    let entries: Vec<Value> = overrides
        .into_iter()
        .map(|(project_id, entry)| {
            json!([
                project_id,
                entry.get("sidebarAutoSettleOnMerge").cloned().unwrap_or(json!("inherit")),
                entry.get("sidebarAutoSettleAfterDays").cloned().unwrap_or(json!("inherit")),
            ])
        })
        .collect();
    json!([settings["sidebarAutoSettleOnMerge"], settings["sidebarAutoSettleAfterDays"], entries]).to_string()
}

/// `pullRequestMatchesProject(pullRequest, project)`.
pub fn pull_request_matches_project(repository_key: Option<&str>, project: &Value) -> bool {
    let identity = project
        .get("repositoryIdentity")
        .and_then(|identity| identity.get("canonicalKey"))
        .and_then(Value::as_str);
    match (repository_key, identity) {
        (Some(key), Some(identity)) => zc_db::pr_keys::canonical_repository_key(key) == zc_db::pr_keys::canonical_repository_key(identity),
        _ => false,
    }
}

/// `{snapshotSequence, projects, threads}` of a sweep.
struct SweepSnapshot {
    snapshot_sequence: Value,
    projects: Vec<Value>,
    threads: Vec<Value>,
}

/// The pull request a lookup group decided on (`SettlementPullRequest | null | undefined`).
enum Lookup {
    /// `undefined`: skip this group for this sweep.
    Skip,
    Found(Option<SettlementPullRequest>),
}

/// A merge the settlement reacts to (`PullRequestMergeEvent`).
#[derive(Debug, Clone)]
pub struct MergedPullRequest {
    pub project_id: String,
    pub repository: String,
    pub number: i64,
    pub merged_at: String,
}

impl MergedPullRequest {
    pub fn from_event(event: &PullRequestMergeEvent) -> Self {
        let reference = &event.reference.0;
        Self {
            project_id: str_of(reference, "projectId").unwrap_or("").to_owned(),
            repository: str_of(reference, "repository").unwrap_or("").to_owned(),
            number: reference["number"].as_i64().unwrap_or(0),
            merged_at: event.merged_at.clone(),
        }
    }
}

fn linked_reference(thread: &Value) -> Option<&Value> {
    thread
        .get("linkedPullRequest")
        .filter(|link| !link.is_null())
        .or_else(|| thread.get("branchPullRequest").filter(|link| !link.is_null()))
}

struct Core {
    deps: SettlementDeps,
}

impl Core {
    async fn settings(&self) -> Result<Value, TaggedError> {
        self.deps
            .settings
            .get_settings()
            .await
            .map(|settings| settings_json(&settings))
            .map_err(|error| TaggedError::new("ServerSettingsError", format!("{error:?}")))
    }

    /// `readSweepSnapshot(snapshots, threadId)`.
    async fn read_sweep_snapshot(&self, thread_id: Option<&ThreadId>) -> Result<SweepSnapshot, TaggedError> {
        let Some(thread_id) = thread_id else {
            let snapshot = self.deps.reads.shell_snapshot(true).await?;
            return Ok(SweepSnapshot {
                snapshot_sequence: snapshot["snapshotSequence"].clone(),
                projects: snapshot["projects"].as_array().cloned().unwrap_or_default(),
                threads: snapshot["threads"].as_array().cloned().unwrap_or_default(),
            });
        };
        // The sequence first: the thread read after it is at least this new.
        let snapshot_sequence = json!(self.deps.reads.snapshot_sequence().await?);
        let Some(thread) = self.deps.reads.thread_shell(thread_id).await? else {
            return Ok(SweepSnapshot {
                snapshot_sequence,
                projects: Vec::new(),
                threads: Vec::new(),
            });
        };
        let mut project_ids = vec![ProjectId::new(str_of(&thread, "projectId").unwrap_or(""))];
        if let Some(reference) = linked_reference(&thread) {
            project_ids.push(ProjectId::new(str_of(reference, "projectId").unwrap_or("")));
        }
        let projects = self.deps.reads.project_shells(Some(project_ids)).await?;
        Ok(SweepSnapshot {
            snapshot_sequence,
            projects,
            threads: vec![thread],
        })
    }

    fn resolve_settled_at(&self, settings: &Value, thread: &Value, pull_request: Option<&SettlementPullRequest>, now: &str) -> Option<String> {
        let project = resolve_project_settings(settings, str_of(thread, "projectId"));
        resolve_auto_settlement_at(
            thread,
            pull_request,
            now,
            project["sidebarAutoSettleAfterDays"].as_f64(),
            project["sidebarAutoSettleOnMerge"].as_bool().unwrap_or(false),
        )
    }

    /// `settleThread`: the thread back when it still needs a pull request decision.
    async fn settle_thread(&self, snapshot_sequence: &Value, thread: &Value, pull_request: Option<&SettlementPullRequest>) -> Option<Value> {
        let attempt = async {
            let settings = self.settings().await?;
            let now = self.deps.clock.now_iso();
            let Some(settled_at) = self.resolve_settled_at(&settings, thread, pull_request, &now) else {
                return Ok::<_, TaggedError>(Some(thread.clone()));
            };
            let thread_id = str_of(thread, "id").unwrap_or("");
            dispatch_json(
                &*self.deps.engine,
                json!({
                    "type": "thread.auto-settle",
                    "commandId": format!("server:auto-settle:{thread_id}:{}", (self.deps.uuids)()),
                    "threadId": thread_id,
                    "snapshotSequence": snapshot_sequence,
                    "settledAt": settled_at,
                }),
            )
            .await?;
            Ok(None)
        };
        match attempt.await {
            Ok(result) => result,
            Err(error) => {
                tracing::warn!(thread_id = str_of(thread, "id"), cause = %pretty(&error), "automatic thread settlement skipped");
                None
            }
        }
    }

    /// `wouldSettle(group, pullRequest)`.
    async fn would_settle(&self, group: &[Value], pull_request: &SettlementPullRequest) -> Result<bool, TaggedError> {
        let settings = self.settings().await?;
        let now = self.deps.clock.now_iso();
        Ok(group
            .iter()
            .any(|thread| self.resolve_settled_at(&settings, thread, Some(pull_request), &now).is_some()))
    }

    /// `pullRequestFor(group)`.
    async fn pull_request_for(
        &self,
        group: &[Value],
        projects: &HashMap<String, Value>,
        cwds: &HashMap<String, String>,
        merged: Option<&MergedPullRequest>,
    ) -> Result<Lookup, TaggedError> {
        let thread = &group[0];
        let thread_id = str_of(thread, "id").unwrap_or("");
        let branch = str_of(thread, "branch");
        let cwd = cwds.get(thread_id);
        if let Some(reference) = linked_reference(thread) {
            let reference_project = str_of(reference, "projectId").unwrap_or("");
            let repository = str_of(reference, "repository").unwrap_or("");
            let number = reference["number"].as_i64().unwrap_or(0);
            let matches_merge = merged.is_some_and(|merged| {
                merged.project_id == reference_project && merged.repository.to_lowercase() == repository.to_lowercase() && merged.number == number
            });
            if !matches_merge && !projects.contains_key(reference_project) {
                return Err(TaggedError::new("Defect", "linked pull request project not found"));
            }
            let summary = match merged.filter(|_| matches_merge) {
                Some(merged) => SettlementPullRequest::new("merged", None, Some(&merged.merged_at)),
                None => {
                    let summary = self
                        .deps
                        .pull_requests
                        .summary(
                            PullRequestRef(json!({"projectId": reference_project, "repository": repository, "number": number})),
                            false,
                        )
                        .await?;
                    let summary = summary.0;
                    SettlementPullRequest::new(
                        str_of(&summary, "state").unwrap_or(""),
                        str_of(&summary, "closedAt"),
                        str_of(&summary, "mergedAt"),
                    )
                }
            };
            if summary.state != "open" {
                if let (Some(branch), Some(cwd)) = (branch, cwd) {
                    // A reused branch can already have a new open PR; only pay for the uncached
                    // lookup when this sweep would otherwise settle.
                    if !self.would_settle(group, &summary).await? {
                        return Ok(Lookup::Skip);
                    }
                    let current = self.deps.git.branch_pull_request(cwd, branch, true).await?;
                    if let Some(current) = current {
                        let state = str_of(&current.pull_request.0, "state");
                        let project = projects.get(str_of(thread, "projectId").unwrap_or(""));
                        if state == Some("open") && project.is_some_and(|project| pull_request_matches_project(current.repository_key.as_deref(), project)) {
                            return Ok(Lookup::Found(Some(SettlementPullRequest {
                                state: "open".into(),
                                closed_at: current.closed_at.flatten(),
                                merged_at: current.merged_at.flatten(),
                                updated_at: current.updated_at,
                            })));
                        }
                    }
                }
            }
            return Ok(Lookup::Found(Some(summary)));
        }
        let Some(branch) = branch else {
            return Ok(Lookup::Found(None));
        };
        let Some(cwd) = cwd else {
            return Err(TaggedError::new("Defect", "thread project not found"));
        };
        let current = self.deps.git.branch_pull_request(cwd, branch, false).await?;
        Ok(Lookup::Found(current.map(|current| SettlementPullRequest {
            state: str_of(&current.pull_request.0, "state").unwrap_or("").to_owned(),
            closed_at: current.closed_at.flatten(),
            merged_at: current.merged_at.flatten(),
            updated_at: current.updated_at,
        })))
    }

    /// `sweep(mergedPullRequest, threadId?)`.
    async fn sweep(&self, merged: Option<&MergedPullRequest>, thread_id: Option<&ThreadId>) -> Result<(), TaggedError> {
        let settings = self.settings().await?;
        if !auto_settlement_configured(&settings) {
            return Ok(());
        }
        let snapshot = self.read_sweep_snapshot(thread_id).await?;
        let now = self.deps.clock.now_iso();
        let projects: HashMap<String, Value> = snapshot
            .projects
            .iter()
            .map(|project| (str_of(project, "id").unwrap_or("").to_owned(), project.clone()))
            .collect();
        let candidates: Vec<&Value> = snapshot.threads.iter().filter(|thread| is_auto_settlement_candidate(thread, &now)).collect();

        // Inactivity needs no host state: decide it before any lookup.
        let sequence = &snapshot.snapshot_sequence;
        let mut undecided: Vec<Option<Value>> = Vec::new();
        for chunk in candidates.chunks(8) {
            let mut decisions = Vec::new();
            for thread in chunk {
                decisions.push(self.settle_thread(sequence, thread, None));
            }
            undecided.extend(join_all(decisions).await);
        }
        let lookup_candidates: Vec<Value> = undecided
            .into_iter()
            .flatten()
            .filter(|thread| {
                !thread["pullRequests"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|link| str_of(link, "source") != Some("stack-dismissed"))
            })
            .collect();

        // The same cwd as PR discovery, so both paths share the git manager's cache.
        let mut cwds: HashMap<String, String> = HashMap::new();
        for thread in &lookup_candidates {
            let Some(project) = projects.get(str_of(thread, "projectId").unwrap_or("")) else {
                continue;
            };
            if str_of(thread, "branch").is_none() {
                continue;
            }
            let worktree_path = str_of(thread, "worktreePath");
            let worktree_exists = worktree_path.is_some_and(|path| (self.deps.path_exists)(path));
            let cwd = match worktree_path {
                Some(path) if worktree_exists => path.to_owned(),
                _ => str_of(project, "workspaceRoot").unwrap_or("").to_owned(),
            };
            cwds.insert(str_of(thread, "id").unwrap_or("").to_owned(), cwd);
        }
        if merged.is_some() {
            // The merge confirmed a state the branch cache can still call open.
            let mut seen = HashSet::new();
            let unique: Vec<String> = cwds.values().filter(|cwd| seen.insert((*cwd).clone())).cloned().collect();
            for chunk in unique.chunks(8) {
                let mut invalidations = Vec::new();
                for cwd in chunk {
                    invalidations.push(self.deps.git.invalidate_status(cwd));
                }
                join_all(invalidations).await;
            }
        }
        let lookup_key = |thread: &Value| -> String {
            let thread_id = str_of(thread, "id").unwrap_or("");
            if let Some(reference) = linked_reference(thread) {
                return json!([
                    "linked",
                    reference["projectId"],
                    reference["repository"],
                    reference["number"],
                    cwds.get(thread_id),
                    thread["branch"]
                ])
                .to_string();
            }
            match (str_of(thread, "branch"), cwds.get(thread_id)) {
                (None, _) => json!(["none", thread_id]).to_string(),
                (Some(_), None) => json!(["missing-project", thread_id]).to_string(),
                (Some(branch), Some(cwd)) => json!(["branch", cwd, branch]).to_string(),
            }
        };
        let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
        for thread in lookup_candidates {
            let key = lookup_key(&thread);
            match groups.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, members)) => members.push(thread),
                None => groups.push((key, vec![thread])),
            }
        }
        for chunk in groups.chunks(8) {
            let mut lookups = Vec::new();
            for (_, group) in chunk {
                lookups.push(self.settle_group(group, &projects, &cwds, merged, sequence));
            }
            join_all(lookups).await;
        }
        Ok(())
    }

    /// One lookup group: decide its pull request, then settle each thread with it.
    async fn settle_group(
        &self,
        group: &[Value],
        projects: &HashMap<String, Value>,
        cwds: &HashMap<String, String>,
        merged: Option<&MergedPullRequest>,
        sequence: &Value,
    ) {
        let result = async {
            let decision = self.pull_request_for(group, projects, cwds, merged).await?;
            if let Lookup::Found(pull_request) = decision {
                for thread in group {
                    self.settle_thread(sequence, thread, pull_request.as_ref()).await;
                }
            }
            Ok::<_, TaggedError>(())
        }
        .await;
        if let Err(error) = result {
            let ids: Vec<&str> = group.iter().filter_map(|thread| str_of(thread, "id")).collect();
            tracing::warn!(thread_ids = ?ids, cause = %pretty(&error), "automatic thread settlement skipped");
        }
    }

    async fn run_sweep(&self, merged: Option<&MergedPullRequest>, thread_id: Option<&ThreadId>) {
        if let Err(error) = self.sweep(merged, thread_id).await {
            tracing::warn!(cause = %pretty(&error), "automatic thread settlement sweep failed");
        }
    }
}

/// `ThreadSettlementReactor`.
pub struct ThreadSettlementReactor {
    core: Arc<Core>,
    worker: DrainableWorker<Option<ThreadId>>,
    stop: CancellationToken,
}

impl ThreadSettlementReactor {
    pub fn new(deps: SettlementDeps, stop: CancellationToken) -> Self {
        let core = Arc::new(Core { deps });
        let worker_core = core.clone();
        let worker = DrainableWorker::start(stop.clone(), move |thread_id: Option<ThreadId>| {
            let core = worker_core.clone();
            async move { core.run_sweep(None, thread_id.as_ref()).await }
        });
        Self { core, worker, stop }
    }

    /// One sweep now, outside the worker (the merge path, and tests).
    pub async fn sweep_now(&self, merged: Option<&MergedPullRequest>, thread_id: Option<&ThreadId>) {
        self.core.run_sweep(merged, thread_id).await;
    }

    /// `start()`: subscribes to settings changes, merges and domain events, and starts the
    /// periodic sweep (the first one runs right away).
    pub async fn start(&self) {
        self.start_with_activation(None).await;
    }

    /// `start()` under a `ServerActivation`: subscribed (and the initial settings read) now,
    /// every sweep only once `activation` resolves.
    pub async fn start_with_activation(&self, activation: Option<futures::future::BoxFuture<'static, ()>>) {
        let deps = &self.core.deps;
        let mut settings_changes = deps.settings.subscribe_changes();
        let mut merges = deps.pull_requests.subscribe_merges();
        let mut events = deps.engine.subscribe_domain_events();
        let initial = self.core.settings().await.unwrap_or(Value::Null);
        let mut last_key = auto_settlement_settings_key(&initial);

        let worker = self.worker.clone();
        let core = self.core.clone();
        let stop = self.stop.clone();
        let interval = deps.interval;
        tokio::spawn(async move {
            if let Some(activation) = activation {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = activation => {}
                }
            }

            let periodic_worker = worker.clone();
            let periodic_stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    periodic_worker.enqueue(None);
                    tokio::select! {
                        _ = periodic_stop.cancelled() => return,
                        _ = periodic_worker.drain() => {}
                    }
                    let Some(interval) = interval else { return };
                    tokio::select! {
                        _ = periodic_stop.cancelled() => return,
                        _ = tokio::time::sleep(interval) => {}
                    }
                }
            });

            let settings_worker = worker.clone();
            let settings_stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    let settings = tokio::select! {
                        _ = settings_stop.cancelled() => break,
                        settings = settings_changes.next() => settings,
                    };
                    let Some(settings) = settings else { break };
                    let key = auto_settlement_settings_key(&settings_json(&settings));
                    if key != last_key {
                        last_key = key;
                        settings_worker.enqueue(None);
                    }
                }
            });

            let merge_core = core.clone();
            let merge_stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    let merge = tokio::select! {
                        _ = merge_stop.cancelled() => break,
                        merge = merges.next() => merge,
                    };
                    let Some(merge) = merge else { break };
                    merge_core.run_sweep(Some(&MergedPullRequest::from_event(&merge)), None).await;
                }
            });

            loop {
                let event = tokio::select! {
                    _ = stop.cancelled() => break,
                    event = events.next() => event,
                };
                let Some(event) = event else { break };
                match &event {
                    OrchestrationEvent::ThreadPullRequestLinked(e) => worker.enqueue(Some(e.payload.thread_id.clone())),
                    OrchestrationEvent::ThreadPullRequestSynced(e) => worker.enqueue(Some(e.payload.thread_id.clone())),
                    OrchestrationEvent::ThreadPullRequestUnlinked(e) => worker.enqueue(Some(e.payload.thread_id.clone())),
                    OrchestrationEvent::ThreadSessionSet(e) => {
                        let status = e.payload.session.status.as_str();
                        if status != "running" && status != "starting" {
                            worker.enqueue(Some(e.payload.thread_id.clone()));
                        }
                    }
                    _ => {}
                }
            }
        });
    }

    /// One timer tick: queue a full sweep (what the one-minute schedule does).
    pub fn tick(&self) {
        self.worker.enqueue(None);
    }

    /// `drain`.
    pub async fn drain(&self) {
        self.worker.drain().await;
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }
}
