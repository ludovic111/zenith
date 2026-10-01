//! `orchestration/ThreadPullRequestReactor.ts`: discovers the pull request of each thread's
//! saved branch without client demand, and keeps `branchPullRequest` (and a terminal manual
//! `linkedPullRequest`) in step with it through `thread.pull-request.sync`.
//!
//! Runs at startup (with a one-time backfill of settled threads), every minute after that, and
//! for one thread after it is created, unarchived, edited, unsettled, or finishes a turn. Saved
//! branch lookups share `GitManager`'s provider cache and retry backoff with status and
//! automatic settlement.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zc_contracts::{
    OrchestrationEvent, OrchestrationProjectShell, OrchestrationThreadShell, OrchestrationThreadShellSettledOverride, ProjectId, PullRequestRef,
    RepositoryIdentity, ThreadId, ThreadLinkedPullRequest,
};
use zc_db::pr_keys::canonical_repository_key;
use zc_ports::git::GitBranchPullRequest;
use zc_ports::{GitWorkflow, OrchestrationDispatch, ProjectionReads, PullRequests, TaggedError};
use zc_reactors::common::{dispatch_json, pretty, UuidSource};
use zc_reactors::reactor::ExternalReactor;
use zc_reactors::runtime::DrainableWorker;
use zc_reactors::settlement::PathExists;

use super::{Activation, RepositoryIdentities};
use crate::sync_key::source_control_repository_selector;

/// Startup lookups per settled thread before discovery gives up on it.
pub const BACKFILL_ATTEMPTS: u32 = 5;

/// What the thread pull request reactor is built from.
#[derive(Clone)]
pub struct ThreadPullRequestDeps {
    pub engine: Arc<dyn OrchestrationDispatch>,
    pub projections: Arc<dyn ProjectionReads>,
    /// `GitManager.branchPullRequest`.
    pub git: Arc<dyn GitWorkflow>,
    pub pull_requests: Arc<dyn PullRequests>,
    pub repository_identities: Arc<dyn RepositoryIdentities>,
    /// `crypto.randomUUIDv4`.
    pub uuids: UuidSource,
    /// `FileSystem.exists`.
    pub path_exists: PathExists,
    /// The periodic pass ([`super::SWEEP_INTERVAL`]).
    pub interval: Duration,
}

#[derive(Debug, Clone)]
struct RefreshRequest {
    thread_id: Option<ThreadId>,
    refresh: bool,
    backfill: bool,
}

impl RefreshRequest {
    fn thread(thread_id: &ThreadId, refresh: bool) -> Self {
        Self {
            thread_id: Some(thread_id.clone()),
            refresh,
            backfill: false,
        }
    }

    fn all(backfill: bool) -> Self {
        Self {
            thread_id: None,
            refresh: false,
            backfill,
        }
    }
}

/// `Pick<OrchestrationShellSnapshot, "snapshotSequence" | "projects" | "threads">`.
#[derive(Debug, Clone, PartialEq)]
pub struct SweepSnapshot {
    pub snapshot_sequence: i64,
    pub projects: Vec<OrchestrationProjectShell>,
    pub threads: Vec<OrchestrationThreadShell>,
}

/// `readSweepSnapshot(snapshots, threadId)`: the shell state for a discovery or settlement
/// sweep. A sweep for one thread reads that thread and the projects it names, not every
/// thread. A sweep over all threads reads only unsettled threads, since both sweeps skip settled
/// ones. Discovery's backfill does its own full read.
pub async fn read_sweep_snapshot(projections: &dyn ProjectionReads, thread_id: Option<&ThreadId>) -> Result<SweepSnapshot, TaggedError> {
    let Some(thread_id) = thread_id else {
        let snapshot = projections.get_shell_snapshot(true).await?;
        return Ok(SweepSnapshot {
            snapshot_sequence: snapshot.snapshot_sequence,
            projects: snapshot.projects,
            threads: snapshot.threads,
        });
    };
    // Read the sequence first. The thread is then at least this new, so a command guarded by
    // the sequence is rejected rather than missing a change.
    let snapshot_sequence = projections.get_snapshot_sequence().await?;
    let Some(thread) = projections.get_thread_shell_by_id(thread_id).await? else {
        return Ok(SweepSnapshot {
            snapshot_sequence,
            projects: Vec::new(),
            threads: Vec::new(),
        });
    };
    // Settlement also checks the project a saved pull request names.
    let reference = thread
        .linked_pull_request
        .clone()
        .flatten()
        .or_else(|| thread.branch_pull_request.clone().flatten());
    let mut project_ids = vec![thread.project_id.clone()];
    if let Some(reference) = reference {
        project_ids.push(reference.project_id);
    }
    let projects = projections.get_project_shells(Some(project_ids)).await?;
    Ok(SweepSnapshot {
        snapshot_sequence,
        projects,
        threads: vec![thread],
    })
}

/// `pullRequestMatchesProject(pullRequest, project)`: the branch's pull request lives in the
/// project's repository.
pub fn pull_request_matches_project(repository_key: Option<&str>, identity: Option<&RepositoryIdentity>) -> bool {
    match (repository_key, identity) {
        (Some(key), Some(identity)) => canonical_repository_key(key) == canonical_repository_key(&identity.canonical_key),
        _ => false,
    }
}

/// `samePullRequest(left, right)`.
fn same_pull_request(left: Option<&ThreadLinkedPullRequest>, right: Option<&ThreadLinkedPullRequest>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.project_id == right.project_id
                && left.repository.to_lowercase() == right.repository.to_lowercase()
                && left.number == right.number
                && left.url == right.url
        }
        (None, None) => true,
        _ => false,
    }
}

fn is_settled(thread: &OrchestrationThreadShell) -> bool {
    thread.settled_override == Some(OrchestrationThreadShellSettledOverride::Settled) || thread.settled_at.is_some()
}

fn reference_of(linked: &ThreadLinkedPullRequest) -> zc_ports::contracts::PullRequestRef {
    let reference = PullRequestRef {
        project_id: linked.project_id.clone(),
        host: None,
        expected_account_id: None,
        allow_stale: None,
        repository: linked.repository.clone(),
        number: linked.number,
    };
    zc_ports::contracts::PullRequestRef(serde_json::to_value(reference).unwrap_or(Value::Null))
}

fn detected_number(detected: &GitBranchPullRequest) -> i64 {
    detected.pull_request.0["number"].as_i64().unwrap_or(0)
}

fn detected_url(detected: &GitBranchPullRequest) -> &str {
    detected.pull_request.0["url"].as_str().unwrap_or("")
}

fn detected_state(detected: &GitBranchPullRequest) -> Option<&str> {
    detected.pull_request.0["state"].as_str()
}

/// What one thread of a lookup group needs dispatched.
struct Plan {
    thread: OrchestrationThreadShell,
    branch_pull_request: Option<ThreadLinkedPullRequest>,
    replacement: Option<ThreadLinkedPullRequest>,
}

struct Core {
    deps: ThreadPullRequestDeps,
    /// Settled threads get one link discovery at startup. Failed lookups retry on the periodic
    /// pass a few times, then stop until the thread changes or the server restarts, so a missing
    /// or logged-out CLI cannot loop forever.
    pending_backfill: Mutex<HashMap<ThreadId, u32>>,
}

impl Core {
    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<ThreadId, u32>> {
        self.pending_backfill.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn finish_backfill<'a>(&self, threads: impl IntoIterator<Item = &'a OrchestrationThreadShell>) {
        let mut pending = self.pending();
        for thread in threads {
            pending.remove(&thread.id);
        }
    }

    fn fail_backfill<'a>(&self, threads: impl IntoIterator<Item = &'a OrchestrationThreadShell>) {
        let mut pending = self.pending();
        for thread in threads {
            let Some(remaining) = pending.get(&thread.id).copied() else {
                continue;
            };
            if remaining <= 1 {
                pending.remove(&thread.id);
            } else {
                pending.insert(thread.id.clone(), remaining - 1);
            }
        }
    }

    async fn synchronize(&self, request: &RefreshRequest) -> Result<(), TaggedError> {
        // Backfill looks up settled threads, so its passes read every thread.
        let full_read = request.thread_id.is_none() && (request.backfill || !self.pending().is_empty());
        let snapshot = if full_read {
            let snapshot = self.deps.projections.get_shell_snapshot(false).await?;
            SweepSnapshot {
                snapshot_sequence: snapshot.snapshot_sequence,
                projects: snapshot.projects,
                threads: snapshot.threads,
            }
        } else {
            read_sweep_snapshot(&*self.deps.projections, request.thread_id.as_ref()).await?
        };
        let projects: HashMap<ProjectId, OrchestrationProjectShell> = snapshot.projects.iter().map(|project| (project.id.clone(), project.clone())).collect();
        if request.backfill {
            let mut pending = self.pending();
            for thread in &snapshot.threads {
                if is_settled(thread) && thread.branch_pull_request.clone().flatten().is_none() {
                    pending.insert(thread.id.clone(), BACKFILL_ATTEMPTS);
                }
            }
        }
        // A single-thread read only shows whether its own thread is gone. A thread with no
        // branch has nothing to look up, and its entry would keep every periodic pass on the
        // full read.
        let branch_thread_ids: HashSet<&ThreadId> = snapshot
            .threads
            .iter()
            .filter(|thread| thread.branch.is_some())
            .map(|thread| &thread.id)
            .collect();
        {
            let mut pending = self.pending();
            let checked: Vec<ThreadId> = match &request.thread_id {
                None => pending.keys().cloned().collect(),
                Some(thread_id) => vec![thread_id.clone()],
            };
            for thread_id in checked {
                if !branch_thread_ids.contains(&thread_id) {
                    pending.remove(&thread_id);
                }
            }
        }
        let threads: Vec<&OrchestrationThreadShell> = {
            let pending = self.pending();
            snapshot
                .threads
                .iter()
                .filter(|thread| {
                    thread.archived_at.is_none()
                        && (!is_settled(thread) || request.thread_id.is_some() || pending.contains_key(&thread.id))
                        && (thread.branch.is_some() || thread.branch_pull_request.clone().flatten().is_some())
                })
                .collect()
        };
        let mut groups: Vec<(String, Vec<OrchestrationThreadShell>)> = Vec::new();
        for thread in threads {
            let key = json!([thread.project_id, thread.worktree_path, thread.branch]).to_string();
            match groups.iter_mut().find(|(existing, _)| *existing == key) {
                Some((_, members)) => members.push(thread.clone()),
                None => groups.push((key, vec![thread.clone()])),
            }
        }

        futures::stream::iter(groups)
            .for_each_concurrent(8, |(_, group)| {
                let projects = &projects;
                let snapshot_sequence = snapshot.snapshot_sequence;
                async move {
                    if let Err(error) = self.synchronize_group(request, projects, snapshot_sequence, &group).await {
                        let thread_ids: Vec<&str> = group.iter().map(|thread| thread.id.as_str()).collect();
                        tracing::warn!(thread_ids = ?thread_ids, cause = %pretty(&error), "thread branch pull request lookup failed");
                        self.fail_backfill(&group);
                    }
                }
            })
            .await;
        Ok(())
    }

    async fn synchronize_group(
        &self,
        request: &RefreshRequest,
        projects: &HashMap<ProjectId, OrchestrationProjectShell>,
        snapshot_sequence: i64,
        group: &[OrchestrationThreadShell],
    ) -> Result<(), TaggedError> {
        let first = &group[0];
        let Some(snapshot_project) = projects.get(&first.project_id) else {
            self.finish_backfill(group);
            return Ok(());
        };
        // A finished turn may have added the remote this PR lives on. A failed refresh
        // resolves to nothing, so keep the snapshot's identity then.
        let mut project = snapshot_project.clone();
        if request.refresh {
            if let Some(identity) = self.deps.repository_identities.resolve(&project.workspace_root, true).await {
                project.repository_identity = Some(Some(identity));
            }
        }
        let identity = project.repository_identity.clone().flatten();
        let repository = source_control_repository_selector(identity.as_ref());
        if first.branch.is_some() && repository.is_none() {
            self.finish_backfill(group);
            return Ok(());
        }
        let worktree_exists = first.worktree_path.as_deref().is_some_and(|path| (self.deps.path_exists)(path));
        let cwd = match &first.worktree_path {
            Some(path) if worktree_exists => path.clone(),
            _ => project.workspace_root.clone(),
        };
        let detected = match &first.branch {
            None => None,
            Some(branch) => self.deps.git.branch_pull_request(&cwd, branch, request.refresh).await?,
        };
        // A worktree can have different remotes, and the project identity can lag a remote
        // edit. Do not attach its PR to the wrong repository.
        if let Some(detected) = &detected {
            if !pull_request_matches_project(detected.repository_key.as_deref(), identity.as_ref()) {
                self.finish_backfill(group);
                return Ok(());
            }
        }
        let detected_reference = match (&detected, &repository) {
            (Some(detected), Some(repository)) => Some(ThreadLinkedPullRequest {
                project_id: project.id.clone(),
                repository: repository.clone(),
                number: detected_number(detected),
                url: detected_url(detected).to_owned(),
            }),
            _ => None,
        };

        let mut updates: Vec<Plan> = Vec::new();
        for thread in group {
            match self.plan(thread, detected.as_ref(), detected_reference.as_ref()).await {
                Ok(Some(plan)) => updates.push(plan),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(thread_id = %thread.id, cause = %pretty(&error), "thread pull request discovery failed");
                    self.fail_backfill([thread]);
                }
            }
        }
        if updates.is_empty() {
            return Ok(());
        }

        if let (Some(detected), Some(branch)) = (&detected, &first.branch) {
            // Summary reads can outlast a remote edit. Recheck the branch and the project's
            // primary remote before saving the group's links.
            let current = self.deps.git.branch_pull_request(&cwd, branch, false).await?;
            let current_identity = self.deps.repository_identities.resolve(&project.workspace_root, true).await;
            let unchanged = current.as_ref().is_some_and(|current| {
                detected_number(current) == detected_number(detected)
                    && detected_url(current) == detected_url(detected)
                    && detected_state(current) == detected_state(detected)
                    && current.repository_key == detected.repository_key
                    && pull_request_matches_project(current.repository_key.as_deref(), current_identity.as_ref())
            });
            if !unchanged {
                self.fail_backfill(updates.iter().map(|update| &update.thread));
                return Ok(());
            }
        }

        for Plan {
            thread,
            branch_pull_request,
            replacement,
        } in updates
        {
            let mut command = json!({
                "type": "thread.pull-request.sync",
                "commandId": format!("server:thread-pull-request:{}:{}", thread.id, (self.deps.uuids)()),
                "threadId": thread.id,
                "projectId": project.id,
                "snapshotSequence": snapshot_sequence,
                "expected": {
                    "workspaceRoot": project.workspace_root,
                    "branch": thread.branch,
                    "worktreePath": thread.worktree_path,
                    "linkedPullRequest": thread.linked_pull_request.clone().flatten(),
                    "branchPullRequest": thread.branch_pull_request.clone().flatten(),
                },
                "branchPullRequest": branch_pull_request,
            });
            if let Some(replacement) = replacement {
                command["linkedPullRequest"] = json!(replacement);
            }
            match dispatch_json(&*self.deps.engine, command).await {
                Ok(_) => {
                    self.pending().remove(&thread.id);
                }
                // The thread changed since the lookup. Its own events requeue it.
                Err(error) if error.is("OrchestrationCommandInvariantError") => self.finish_backfill([&thread]),
                Err(error) => {
                    tracing::warn!(thread_id = %thread.id, cause = %pretty(&error), "thread pull request update failed");
                    self.fail_backfill([&thread]);
                }
            }
        }
        Ok(())
    }

    /// One thread's update, or `None` when its links already match.
    async fn plan(
        &self,
        thread: &OrchestrationThreadShell,
        detected: Option<&GitBranchPullRequest>,
        detected_reference: Option<&ThreadLinkedPullRequest>,
    ) -> Result<Option<Plan>, TaggedError> {
        let saved = thread.branch_pull_request.clone().flatten();
        let linked = thread.linked_pull_request.clone().flatten();
        let mut branch_pull_request = detected_reference.cloned();
        // Shared checkouts often return to the default branch after a merge. Keep that
        // thread's terminal PR across the change.
        if branch_pull_request.is_none() && thread.branch.is_some() && thread.worktree_path.is_none() {
            if let Some(saved) = &saved {
                let previous = self.deps.pull_requests.summary(reference_of(saved), false).await?;
                if matches!(previous.0["state"].as_str(), Some("merged" | "closed")) {
                    branch_pull_request = Some(saved.clone());
                }
            }
        }

        let mut replacement = None;
        if let (Some(linked), Some(detected_reference)) = (&linked, detected_reference) {
            if thread.pull_requests.is_empty()
                && detected.and_then(detected_state) == Some("open")
                && !same_pull_request(Some(linked), Some(detected_reference))
            {
                let summary = self.deps.pull_requests.summary(reference_of(linked), false).await?;
                if matches!(summary.0["state"].as_str(), Some("merged" | "closed")) {
                    replacement = Some(detected_reference.clone());
                }
            }
        }

        if same_pull_request(saved.as_ref(), branch_pull_request.as_ref()) && replacement.is_none() {
            self.pending().remove(&thread.id);
            return Ok(None);
        }
        Ok(Some(Plan {
            thread: thread.clone(),
            branch_pull_request,
            replacement,
        }))
    }
}

/// `ThreadPullRequestReactor`.
pub struct ThreadPullRequestReactor {
    core: Arc<Core>,
    worker: DrainableWorker<RefreshRequest>,
    stop: CancellationToken,
}

impl ThreadPullRequestReactor {
    pub fn new(deps: ThreadPullRequestDeps, stop: CancellationToken) -> Self {
        let core = Arc::new(Core {
            deps,
            pending_backfill: Mutex::new(HashMap::new()),
        });
        let worker_core = core.clone();
        let worker = DrainableWorker::start(stop.clone(), move |request: RefreshRequest| {
            let core = worker_core.clone();
            async move {
                if let Err(error) = core.synchronize(&request).await {
                    tracing::warn!(cause = %pretty(&error), "thread pull request refresh failed");
                }
            }
        });
        Self { core, worker, stop }
    }

    /// `start()`: subscribes to domain events, then runs the startup backfill and the periodic
    /// pass (every minute, the first one a minute after the backfill).
    pub async fn start(&self) {
        self.start_with_activation(None).await;
    }

    /// `start()` under a `ServerActivation`: subscribed now, every lookup only once
    /// `activation` resolves.
    pub async fn start_with_activation(&self, activation: Option<Activation>) {
        let mut events = self.core.deps.engine.subscribe_domain_events();
        let worker = self.worker.clone();
        let stop = self.stop.clone();
        let interval = self.core.deps.interval;
        tokio::spawn(async move {
            if let Some(activation) = activation {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = activation => {}
                }
            }

            let event_worker = worker.clone();
            let event_stop = stop.clone();
            tokio::spawn(async move {
                loop {
                    let event = tokio::select! {
                        _ = event_stop.cancelled() => break,
                        event = events.next() => event,
                    };
                    let Some(event) = event else { break };
                    if let Some(request) = request_for(&event) {
                        event_worker.enqueue(request);
                    }
                }
            });

            // Run without client demand.
            worker.enqueue(RefreshRequest::all(true));
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = worker.drain() => {}
            }
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(interval) => {}
                }
                worker.enqueue(RefreshRequest::all(false));
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = worker.drain() => {}
                }
            }
        });
    }

    /// `drain`.
    pub async fn drain(&self) {
        self.worker.drain().await;
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }
}

/// `processEvent`: the lookup an event asks for.
fn request_for(event: &OrchestrationEvent) -> Option<RefreshRequest> {
    match event {
        OrchestrationEvent::ThreadCreated(e) => Some(RefreshRequest::thread(&e.payload.thread_id, false)),
        OrchestrationEvent::ThreadUnarchived(e) => Some(RefreshRequest::thread(&e.payload.thread_id, false)),
        OrchestrationEvent::ThreadMetaUpdated(e) => {
            let payload = &e.payload;
            (payload.branch_pull_request.is_none() && (payload.branch.is_some() || payload.worktree_path.is_some() || payload.linked_pull_request.is_some()))
                .then(|| RefreshRequest::thread(&payload.thread_id, false))
        }
        OrchestrationEvent::ThreadSessionSet(e) => {
            // Checkpoint completion forces the post-turn read. Session lifecycle events reuse it
            // regardless of which event reaches this worker first.
            let status = e.payload.session.status.as_str();
            (status != "running" && status != "starting").then(|| RefreshRequest::thread(&e.payload.thread_id, false))
        }
        OrchestrationEvent::ThreadTurnDiffCompleted(e) => Some(RefreshRequest::thread(&e.payload.thread_id, true)),
        OrchestrationEvent::ThreadUnsettled(e) => Some(RefreshRequest::thread(&e.payload.thread_id, true)),
        OrchestrationEvent::ProjectMetaUpdated(e) => e.payload.workspace_root.is_some().then(|| RefreshRequest::all(false)),
        _ => None,
    }
}

#[async_trait]
impl ExternalReactor for ThreadPullRequestReactor {
    async fn start(&self) {
        ThreadPullRequestReactor::start(self).await;
    }
}
