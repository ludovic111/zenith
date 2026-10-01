//! `orchestration/PullRequestSyncReactor.ts`: keeps every thread ↔ pull request link's host
//! snapshot current. One sweep a minute reads only the active threads that have links, groups
//! visible links by pull request so the host is asked once per PR no matter how many threads
//! share it, and writes back only what changed. Native stacks the host reports are auto-linked
//! to the thread as `source: "stack"`.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use regex::Regex;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use zc_contracts::{
    LitNative, OrchestrationEvent, PullRequestStack, PullRequestSummary, ThreadPullRequestKey, ThreadPullRequestLink, ThreadPullRequestLinkSource,
    ThreadPullRequestSnapshot, ThreadPullRequestStack, ThreadPullRequestStackLayer,
};
use zc_db::pr_keys::{normalize_thread_pull_request_key, parse_change_request_url};
use zc_ports::contracts::{PullRequestInvalidateInput, PullRequestRef};
use zc_ports::orchestration::ThreadPullRequests;
use zc_ports::{OrchestrationDispatch, ProjectionReads, PullRequests, TaggedError};
use zc_projections::pull_requests::{thread_pull_request_key_of, thread_pull_request_keys_equal, KeySource};
use zc_reactors::common::{dispatch_json, pretty, UuidSource};
use zc_reactors::reactor::ExternalReactor;
use zc_reactors::runtime::{DrainableWorker, ReactorClock};

use super::Activation;

/// Closed requests can reopen on the host: they are re-read this often (15 minutes).
pub const SLOW_SYNC_INTERVAL_MS: i64 = 15 * 60 * 1_000;

/// What the pull request sync reactor is built from.
#[derive(Clone)]
pub struct PullRequestSyncDeps {
    pub engine: Arc<dyn OrchestrationDispatch>,
    /// `listThreadsWithPullRequests`.
    pub projections: Arc<dyn ProjectionReads>,
    pub pull_requests: Arc<dyn PullRequests>,
    /// `DateTime.now`.
    pub clock: Arc<dyn ReactorClock>,
    /// `crypto.randomUUIDv4`.
    pub uuids: UuidSource,
    /// The periodic sweep ([`super::SWEEP_INTERVAL`]).
    pub interval: Duration,
}

/// `siblingPullRequestUrl(url, number)` (`shared/changeRequestUrl.ts`): the URL of another
/// pull request of the same repository, on the same host and route.
pub fn sibling_pull_request_url(url: &str, number: i64) -> Option<String> {
    static ROUTE: OnceLock<Regex> = OnceLock::new();
    let reference = parse_change_request_url(url)?;
    if !(1..=9_007_199_254_740_991).contains(&number) {
        return None;
    }
    let mut sibling = url::Url::parse(url).ok()?;
    let rest = sibling.path().get(reference.repository.len() + 1..)?;
    let route = ROUTE
        .get_or_init(|| Regex::new(r"^/(-/merge_requests|pulls?|pull-requests|pullrequest)/\d+(?:/|$)").expect("static regex"))
        .captures(rest)?
        .get(1)?
        .as_str()
        .to_owned();
    sibling.set_path(&format!("/{}/{route}/{number}", reference.repository));
    sibling.set_query(None);
    sibling.set_fragment(None);
    Some(sibling.to_string())
}

fn link_source(link: &ThreadPullRequestLink) -> KeySource<'_> {
    (&link.host, &link.repository, link.number, Some(&link.url))
}

/// `normalizeThreadPullRequestKey(link).host`: the Forgejo HTTP authority recovered from its URL.
fn normalized_host(link: &ThreadPullRequestLink) -> String {
    normalize_thread_pull_request_key(&link.host, &link.repository, link.number, None, Some(&link.url)).host
}

/// `snapshotFieldsOf(summary)`: the snapshot a summary yields, `syncedAt` left empty.
fn snapshot_fields_of(summary: &PullRequestSummary) -> ThreadPullRequestSnapshot {
    ThreadPullRequestSnapshot {
        state: summary.state,
        title: summary.title.clone(),
        head_branch: summary.head_branch.clone(),
        base_branch: summary.base_branch.clone(),
        is_draft: summary.is_draft.unwrap_or(false),
        updated_at: Some(summary.updated_at.clone()),
        synced_at: String::new(),
        closed_at: Some(summary.closed_at.clone().flatten()),
        merged_at: Some(summary.merged_at.clone().flatten()),
        author: summary.author.clone(),
        additions: summary.additions,
        deletions: summary.deletions,
        changed_files: summary.changed_files,
        review_decision: summary.review_decision,
        checks_state: summary.checks_state,
        mergeability: summary.mergeability,
    }
}

/// `snapshotFieldsEqual(left, right)`: `syncedAt` aside, with absent and `null` alike where TS
/// compares through `?? null`.
fn snapshot_fields_equal(left: &ThreadPullRequestSnapshot, right: &ThreadPullRequestSnapshot) -> bool {
    let author = |snapshot: &ThreadPullRequestSnapshot| snapshot.author.clone().flatten();
    let (left_author, right_author) = (author(left), author(right));
    left.state == right.state
        && left.title == right.title
        && left.head_branch == right.head_branch
        && left.base_branch == right.base_branch
        && left.is_draft == right.is_draft
        && left.updated_at == right.updated_at
        && left.closed_at.clone().flatten() == right.closed_at.clone().flatten()
        && left.merged_at.clone().flatten() == right.merged_at.clone().flatten()
        && left_author.as_ref().map(|actor| &actor.login) == right_author.as_ref().map(|actor| &actor.login)
        && left_author.as_ref().and_then(|actor| actor.avatar_url.as_ref()) == right_author.as_ref().and_then(|actor| actor.avatar_url.as_ref())
        && left.additions == right.additions
        && left.deletions == right.deletions
        && left.changed_files == right.changed_files
        && left.review_decision.flatten() == right.review_decision.flatten()
        && left.checks_state.flatten() == right.checks_state.flatten()
        && left.mergeability == right.mergeability
}

/// `stacksEqual(left, right)`.
fn stacks_equal(left: Option<&ThreadPullRequestStack>, right: Option<&ThreadPullRequestStack>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.id == right.id
                && left.number == right.number
                && left.url == right.url
                && left.base == right.base
                && left.layers.len() == right.layers.len()
                && left
                    .layers
                    .iter()
                    .zip(&right.layers)
                    .all(|(layer, other)| layer.number == other.number && layer.head_branch == other.head_branch && layer.state == other.state)
        }
        (None, None) => true,
        _ => false,
    }
}

/// `{kind: "native", ...stack}`, as the link stores it.
fn native_stack(stack: PullRequestStack) -> ThreadPullRequestStack {
    ThreadPullRequestStack {
        kind: LitNative,
        id: stack.id,
        number: stack.number,
        url: stack.url,
        base: stack.base,
        layers: stack
            .layers
            .into_iter()
            .map(|layer| ThreadPullRequestStackLayer {
                number: layer.number,
                head_branch: layer.head_branch,
                state: layer.state,
            })
            .collect(),
    }
}

fn decode<T: serde::de::DeserializeOwned>(what: &str, value: Value) -> Result<T, TaggedError> {
    serde_json::from_value(value).map_err(|error| TaggedError::new("PullRequestDecodeError", format!("cannot decode the pull request {what}: {error}")))
}

fn is_unsettled(thread: &ThreadPullRequests) -> bool {
    thread.settled_override.as_deref() != Some("settled") && thread.settled_at.is_none()
}

fn is_state(link: &ThreadPullRequestLink, state: &str) -> bool {
    link.snapshot.as_ref().is_some_and(|snapshot| snapshot.state.as_str() == state)
}

struct LinkEntry {
    thread: Arc<ThreadPullRequests>,
    link: ThreadPullRequestLink,
}

#[derive(Default)]
struct State {
    last_synced_at: HashMap<String, i64>,
    /// Keys a caller asked to re-read, with the request's generation.
    requested: HashMap<String, u64>,
    request_generation: u64,
    retry_stacks: HashSet<String>,
}

/// One sweep's shared pieces.
struct Sweep {
    now_ms: i64,
    now_iso: String,
    /// Layers auto-linked this sweep, so two links of one thread that share a stack do not
    /// both try to add the same sibling.
    linked_this_sweep: Mutex<HashSet<String>>,
    persistence: tokio::sync::Mutex<()>,
}

struct Core {
    deps: PullRequestSyncDeps,
    state: Mutex<State>,
}

impl Core {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_due(&self, key: &str, entries: &[LinkEntry], now_ms: i64) -> bool {
        let state = self.state();
        if state.requested.contains_key(key) || state.retry_stacks.contains(key) {
            return true;
        }
        if entries.iter().any(|entry| entry.link.snapshot.is_none()) {
            return true;
        }
        if entries.iter().all(|entry| is_state(&entry.link, "merged")) {
            return false;
        }
        if entries.iter().any(|entry| is_state(&entry.link, "open") && is_unsettled(&entry.thread)) {
            return true;
        }
        // Closed requests can reopen on the host, including after the thread settles.
        state.last_synced_at.get(key).is_none_or(|last| now_ms - last >= SLOW_SYNC_INTERVAL_MS)
    }

    fn request(&self, key: String, worker: &DrainableWorker<Option<String>>) {
        {
            let mut state = self.state();
            state.request_generation += 1;
            let generation = state.request_generation;
            state.requested.insert(key.clone(), generation);
        }
        worker.enqueue(Some(key));
    }

    async fn sweep(&self, requested_key: Option<&str>) -> Result<(), TaggedError> {
        let threads = self.deps.projections.list_threads_with_pull_requests().await?;
        let now_ms = self.deps.clock.now_millis();
        let sweep = Sweep {
            now_ms,
            now_iso: zc_core::time::iso_from_millis(now_ms),
            linked_this_sweep: Mutex::new(HashSet::new()),
            persistence: tokio::sync::Mutex::new(()),
        };

        let mut groups: Vec<(String, Vec<LinkEntry>)> = Vec::new();
        let mut index: HashMap<String, usize> = HashMap::new();
        for thread in threads.into_iter().map(Arc::new) {
            for link in thread
                .pull_requests
                .iter()
                .filter(|link| link.source != ThreadPullRequestLinkSource::StackDismissed)
            {
                let key = thread_pull_request_key_of(link_source(link));
                let entry = LinkEntry {
                    thread: thread.clone(),
                    link: link.clone(),
                };
                match index.get(&key) {
                    Some(position) => groups[*position].1.push(entry),
                    None => {
                        index.insert(key.clone(), groups.len());
                        groups.push((key, vec![entry]));
                    }
                }
            }
        }

        {
            let mut state = self.state();
            state.last_synced_at.retain(|key, _| index.contains_key(key));
            state.retry_stacks.retain(|key| index.contains_key(key));
            state.requested.retain(|key, _| index.contains_key(key));
        }

        // As wide as one batched summary read, so the sweep's reads on a host arrive together
        // and GitHub answers them in one request rather than one `gh pr view` apiece.
        futures::stream::iter(groups)
            .for_each_concurrent(25, |(key, entries)| {
                let sweep = &sweep;
                async move {
                    if requested_key.is_some_and(|requested| requested != key) || !self.is_due(&key, &entries, sweep.now_ms) {
                        return;
                    }
                    if let Err(error) = self.sync_group(&key, &entries, sweep).await {
                        tracing::warn!(key = %key, cause = %pretty(&error), "pull request sync skipped");
                    }
                }
            })
            .await;
        Ok(())
    }

    async fn sync_group(&self, key: &str, entries: &[LinkEntry], sweep: &Sweep) -> Result<(), TaggedError> {
        let first = &entries[0];
        let reference = PullRequestRef(json!({
            "projectId": first.thread.project_id,
            "host": normalized_host(&first.link),
            "repository": first.link.repository,
            "number": first.link.number,
        }));
        let generation = self.state().requested.get(key).copied();
        if generation.is_some() {
            self.deps
                .pull_requests
                .invalidate(PullRequestInvalidateInput(json!({"reference": reference.0})), false)
                .await;
        }
        let summary = self.deps.pull_requests.summary(reference.clone(), false).await?;
        let fields = snapshot_fields_of(&decode::<PullRequestSummary>("summary", summary.0)?);
        let needs_stack = generation.is_some()
            || self.state().retry_stacks.contains(key)
            || entries
                .iter()
                .any(|entry| entry.link.snapshot.as_ref().is_none_or(|snapshot| !snapshot_fields_equal(snapshot, &fields)));
        // `Some(stack)` when the host was asked for it this sweep.
        let mut fetched_stack: Option<Option<ThreadPullRequestStack>> = None;
        if needs_stack {
            let stack = match self.deps.pull_requests.stack(reference, false).await {
                Ok(stack) => stack.map(|stack| decode::<PullRequestStack>("stack", stack.0).map(native_stack)).transpose(),
                Err(error) => Err(error),
            };
            match stack {
                Ok(stack) => {
                    self.state().retry_stacks.remove(key);
                    fetched_stack = Some(stack);
                }
                Err(error) => {
                    tracing::warn!(key = %key, cause = %pretty(&error), "pull request stack lookup failed");
                    self.state().retry_stacks.insert(key.to_owned());
                    return Ok(());
                }
            }
        }
        {
            let mut state = self.state();
            // The host answered, so the cadence clock ticks even if a dispatch below is rejected.
            state.last_synced_at.insert(key.to_owned(), sweep.now_ms);
            // A refresh requested while the host read was in flight belongs to the next sweep.
            if state.requested.get(key).copied() == generation {
                state.requested.remove(key);
            }
        }
        for entry in entries {
            let result = {
                let _permit = sweep.persistence.lock().await;
                self.sync_entry(entry, &fields, fetched_stack.as_ref(), sweep).await
            };
            if let Err(error) = result {
                self.state().retry_stacks.insert(key.to_owned());
                tracing::warn!(thread_id = %entry.thread.id, key = %key, cause = %pretty(&error), "pull request sync skipped");
            }
        }
        Ok(())
    }

    async fn sync_entry(
        &self,
        entry: &LinkEntry,
        fields: &ThreadPullRequestSnapshot,
        fetched_stack: Option<&Option<ThreadPullRequestStack>>,
        sweep: &Sweep,
    ) -> Result<(), TaggedError> {
        let LinkEntry { thread, link } = entry;
        let next_stack = match fetched_stack {
            None => link.stack.clone(),
            Some(stack) => stack.clone(),
        };
        let changed =
            link.snapshot.as_ref().is_none_or(|snapshot| !snapshot_fields_equal(snapshot, fields)) || !stacks_equal(link.stack.as_ref(), next_stack.as_ref());
        let host = normalized_host(link);
        // Persist discovered siblings before a terminal snapshot can trigger settlement.
        let layers = fetched_stack.and_then(Option::as_ref).map(|stack| stack.layers.as_slice()).unwrap_or_default();
        for layer in layers {
            let layer_key: KeySource<'_> = (&host, &link.repository, layer.number, None);
            let dedupe_key = format!("{}:{}", thread.id, thread_pull_request_key_of(layer_key));
            if self.linked(sweep, &dedupe_key) {
                continue;
            }
            // Tombstones count as present: a dismissed layer is never re-added.
            if thread
                .pull_requests
                .iter()
                .any(|existing| thread_pull_request_keys_equal(link_source(existing), layer_key))
            {
                continue;
            }
            let Some(url) = sibling_pull_request_url(&link.url, layer.number) else {
                continue;
            };
            dispatch_json(
                &*self.deps.engine,
                json!({
                    "type": "thread.pull-request.link",
                    "commandId": format!("server:pr-stack-link:{}:{}", thread.id, (self.deps.uuids)()),
                    "threadId": thread.id,
                    "host": host,
                    "repository": link.repository,
                    "number": layer.number,
                    "url": url,
                    "source": "stack",
                }),
            )
            .await?;
            sweep
                .linked_this_sweep
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(dedupe_key);
        }
        if changed {
            let snapshot = ThreadPullRequestSnapshot {
                synced_at: sweep.now_iso.clone(),
                ..fields.clone()
            };
            dispatch_json(
                &*self.deps.engine,
                json!({
                    "type": "thread.pull-request-link.sync",
                    "commandId": format!("server:pr-sync:{}:{}", thread.id, (self.deps.uuids)()),
                    "threadId": thread.id,
                    "host": host,
                    "repository": link.repository,
                    "number": link.number,
                    "snapshot": snapshot,
                    "stack": next_stack,
                }),
            )
            .await?;
        }
        Ok(())
    }

    fn linked(&self, sweep: &Sweep, dedupe_key: &str) -> bool {
        sweep
            .linked_this_sweep
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(dedupe_key)
    }
}

/// `PullRequestSyncReactor`.
pub struct PullRequestSyncReactor {
    core: Arc<Core>,
    worker: DrainableWorker<Option<String>>,
    stop: CancellationToken,
}

impl PullRequestSyncReactor {
    pub fn new(deps: PullRequestSyncDeps, stop: CancellationToken) -> Self {
        let core = Arc::new(Core {
            deps,
            state: Mutex::new(State::default()),
        });
        let worker_core = core.clone();
        let worker = DrainableWorker::start(stop.clone(), move |key: Option<String>| {
            let core = worker_core.clone();
            async move {
                if let Err(error) = core.sweep(key.as_deref()).await {
                    tracing::warn!(cause = %pretty(&error), "pull request sync sweep failed");
                }
            }
        });
        Self { core, worker, stop }
    }

    /// `start()`: subscribes to domain events (a new link is synced right away), then sweeps
    /// every minute, the first time right away.
    pub async fn start(&self) {
        self.start_with_activation(None).await;
    }

    /// `start()` under a `ServerActivation`: subscribed now, every sweep only once `activation`
    /// resolves. [`Self::request_sync`] is not parked.
    pub async fn start_with_activation(&self, activation: Option<Activation>) {
        let mut events = self.core.deps.engine.subscribe_domain_events();
        let core = self.core.clone();
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
                    if let OrchestrationEvent::ThreadPullRequestLinked(event) = &event {
                        core.request(thread_pull_request_key_of(link_source(&event.payload.link)), &event_worker);
                    }
                }
            });

            loop {
                worker.enqueue(None);
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = worker.drain() => {}
                }
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep(interval) => {}
                }
            }
        });
    }

    /// `requestSync(key)`: force the next sweep to re-read this pull request, even when its
    /// snapshot is terminal. The RPC handlers call it after `pullRequests.runAction` and
    /// `pullRequests.invalidate`.
    pub async fn request_sync(&self, key: &ThreadPullRequestKey) {
        self.core
            .request(thread_pull_request_key_of((&key.host, &key.repository, key.number, None)), &self.worker);
    }

    /// `drain`.
    pub async fn drain(&self) {
        self.worker.drain().await;
    }

    pub fn stop(&self) {
        self.stop.cancel();
    }
}

#[async_trait]
impl ExternalReactor for PullRequestSyncReactor {
    async fn start(&self) {
        PullRequestSyncReactor::start(self).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_urls_keep_the_host_route() {
        assert_eq!(
            sibling_pull_request_url("https://github.com/owner/repository/pull/42", 43).as_deref(),
            Some("https://github.com/owner/repository/pull/43")
        );
        assert_eq!(
            sibling_pull_request_url("https://gitlab.example.test/group/sub/project/-/merge_requests/5/diffs?tab=1#x", 6).as_deref(),
            Some("https://gitlab.example.test/group/sub/project/-/merge_requests/6")
        );
        assert_eq!(
            sibling_pull_request_url("http://forge.example:3000/Owner/Repository/pulls/7", 8).as_deref(),
            Some("http://forge.example:3000/owner/repository/pulls/8")
        );
        assert_eq!(sibling_pull_request_url("https://github.com/owner/repository/pull/42", 0), None);
        assert_eq!(sibling_pull_request_url("https://example.test/not-a-pull-request", 3), None);
    }
}
