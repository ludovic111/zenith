//! The orchestration read RPCs of `ws.ts`: `orchestration.subscribeShell`,
//! `orchestration.subscribeThread`, `orchestration.getArchivedShellSnapshot` and
//! `orchestration.searchThreads`.
//!
//! Both subscriptions attach live delivery **before** reading any snapshot or replay (the
//! engine's `subscribe_domain_events` is eager), so nothing published meanwhile is lost;
//! overlapping events are deduplicated by sequence on the client. A resuming client passes
//! `afterSequence`: the server replays persisted events after it when the range is small
//! enough (shell: ≤ 1,000 events; thread: ≤ 1,000 of the thread's own events; both ≤ 8 MiB of
//! payload), else sends a fresh snapshot. `requestCompletionMarker` adds a `synchronized` item
//! once everything buffered before it has been delivered.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures::stream::{self, BoxStream};
use futures::{FutureExt, Stream, StreamExt};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zc_contracts::{
    LitOrchestrationSearchThreadsError, LitProjectRemoved, LitProjectUpserted, LitSnapshot, LitSynchronized, LitThreadRemoved, LitThreadUpserted,
    OrchestrationEvent, OrchestrationGetSnapshotError, OrchestrationSearchThreadsError, OrchestrationSearchThreadsInput, OrchestrationSearchThreadsResult,
    OrchestrationShellSnapshot, OrchestrationShellStreamEvent, OrchestrationShellStreamEventProjectRemoved, OrchestrationShellStreamEventProjectUpserted,
    OrchestrationShellStreamEventThreadRemoved, OrchestrationShellStreamEventThreadUpserted, OrchestrationShellStreamItem,
    OrchestrationShellStreamItemSnapshot, OrchestrationShellStreamItemSynchronized, OrchestrationSubscribeShellInput, OrchestrationSubscribeThreadInput,
    OrchestrationThreadDetailWindow, OrchestrationThreadStreamItem, OrchestrationThreadStreamItemEvent, OrchestrationThreadStreamItemSnapshot, ProjectId,
    ThreadId,
};
use zc_ports::orchestration::ThreadReplayRange;
use zc_ports::{OrchestrationDispatch, ProjectionReads, TaggedError};
use zc_rpc::{RpcError, RpcRouterBuilder};

use crate::activity_payload::{project_activity_event, project_thread_detail_snapshot};
use crate::budget::{serialized_size, snapshot_error, LiveStreamBudget, Retained};
use crate::coalescer::{ThreadLiveEventCoalescer, ThreadLiveInput};

/// When a resuming shell cursor is more than this many events behind, send a snapshot.
pub const SHELL_RESUME_MAX_GAP: i64 = 1_000;
/// Thread replay counts only this thread's rows.
pub const THREAD_RESUME_MAX_EVENTS: u32 = 1_000;
/// Replays past this many payload bytes reset with a snapshot instead.
pub const ORCHESTRATION_REPLAY_PAYLOAD_BUDGET_BYTES: u64 = 8 * 1024 * 1024;
/// Window and size over which shell events are coalesced per aggregate.
pub const SHELL_COALESCE_WINDOW: Duration = Duration::from_millis(50);
pub const SHELL_COALESCE_MAX_CHUNK: usize = 512;
const SHELL_REFETCH_CONCURRENCY: usize = 8;

type SnapshotResult<T> = Result<T, OrchestrationGetSnapshotError>;

/// `new OrchestrationGetSnapshotError({message, cause})`, the cause encoded as a defect.
fn snapshot_error_with_cause(message: impl Into<String>, cause: &TaggedError) -> OrchestrationGetSnapshotError {
    let mut error = snapshot_error(message);
    error.cause = Some(zc_core::defect::Defect::error(&cause.tag, cause.message.clone()).0);
    error
}

/// `isThreadDetailEvent`: the event types a thread subscription delivers.
pub fn is_thread_detail_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "thread.message-sent"
            | "thread.proposed-plan-upserted"
            | "thread.activity-appended"
            | "thread.turn-diff-completed"
            | "thread.reverted"
            | "thread.session-set"
    )
}

/// Asks a forwarder to queue a completion marker behind everything published so far.
type FlushRequest = tokio::sync::oneshot::Sender<SnapshotResult<()>>;

/// `toShellEvent`: shell updates refetch the aggregate, so only these fields are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShellEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub aggregate_kind: String,
    pub aggregate_id: String,
    pub sequence: i64,
}

impl ShellEvent {
    pub fn of(event: &OrchestrationEvent) -> Self {
        let value = serde_json::to_value(event).unwrap_or(Value::Null);
        let field = |key: &str| value.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        Self {
            event_type: field("type"),
            aggregate_kind: field("aggregateKind"),
            aggregate_id: field("aggregateId"),
            sequence: value.get("sequence").and_then(Value::as_i64).unwrap_or(0),
        }
    }
}

/// The envelope fields subscriptions filter on.
fn envelope(event: &OrchestrationEvent) -> (String, String, String) {
    let shell = ShellEvent::of(event);
    (shell.event_type, shell.aggregate_kind, shell.aggregate_id)
}

/// `ShellLiveInput`.
#[derive(Debug, Clone)]
enum ShellLiveInput {
    Event(ShellEvent),
    Synchronized,
}

/// A stream that runs `on_drop` when the subscription goes away (the TS RPC scope closing).
struct Scoped<S> {
    inner: S,
    on_drop: Vec<Box<dyn FnOnce() + Send>>,
}

impl<S: Stream + Unpin> Stream for Scoped<S> {
    type Item = S::Item;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<S::Item>> {
        Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl<S> Drop for Scoped<S> {
    fn drop(&mut self) {
        for action in self.on_drop.drain(..) {
            action();
        }
    }
}

fn abort_on_drop(handle: JoinHandle<()>) -> Box<dyn FnOnce() + Send> {
    Box::new(move || handle.abort())
}

/// `Stream.groupedWithin(max, window)`: a group is emitted when it reaches `max` items or
/// `window` after its first item, whichever comes first; the rest at the end of the source.
pub struct GroupedWithin<S: Stream> {
    source: Option<S>,
    group: Vec<S::Item>,
    max: usize,
    window: Duration,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S: Stream> GroupedWithin<S> {
    pub fn new(source: S, max: usize, window: Duration) -> Self {
        Self {
            source: Some(source),
            group: Vec::new(),
            max,
            window,
            deadline: None,
        }
    }
}

impl<S: Stream + Unpin> Stream for GroupedWithin<S>
where
    S::Item: Unpin,
{
    type Item = Vec<S::Item>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if this.group.len() >= this.max {
                this.deadline = None;
                return Poll::Ready(Some(std::mem::take(&mut this.group)));
            }
            let Some(source) = this.source.as_mut() else {
                if this.group.is_empty() {
                    return Poll::Ready(None);
                }
                return Poll::Ready(Some(std::mem::take(&mut this.group)));
            };
            match Pin::new(source).poll_next(cx) {
                Poll::Ready(Some(item)) => {
                    if this.group.is_empty() {
                        this.deadline = Some(Box::pin(tokio::time::sleep(this.window)));
                    }
                    this.group.push(item);
                }
                Poll::Ready(None) => {
                    this.source = None;
                    this.deadline = None;
                }
                Poll::Pending => {
                    if let Some(deadline) = this.deadline.as_mut() {
                        if std::future::Future::poll(deadline.as_mut(), cx).is_ready() {
                            this.deadline = None;
                            return Poll::Ready(Some(std::mem::take(&mut this.group)));
                        }
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}

/// The orchestration read RPCs, over the engine and the projection queries.
#[derive(Clone)]
pub struct OrchestrationSubscriptions {
    engine: Arc<dyn OrchestrationDispatch>,
    reads: Arc<dyn ProjectionReads>,
}

impl OrchestrationSubscriptions {
    pub fn new(engine: Arc<dyn OrchestrationDispatch>, reads: Arc<dyn ProjectionReads>) -> Self {
        Self { engine, reads }
    }

    /// Registers the four methods on a router (scopes from the router's scope table:
    /// `orchestration:read` for all four).
    pub fn register(self: &Arc<Self>, builder: RpcRouterBuilder) -> RpcRouterBuilder {
        let shell = Arc::clone(self);
        let thread = Arc::clone(self);
        let archived = Arc::clone(self);
        let search = Arc::clone(self);
        builder
            .stream("orchestration.subscribeShell", move |_ctx, payload| {
                let subscriptions = Arc::clone(&shell);
                async move {
                    let input: OrchestrationSubscribeShellInput = serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))?;
                    let stream = subscriptions.subscribe_shell(input).await.map_err(RpcError::fail)?;
                    Ok(stream.map(|item| match item {
                        Ok(item) => serde_json::to_value(item).map_err(|e| RpcError::die(e.to_string())),
                        Err(error) => Err(RpcError::fail(error)),
                    }))
                }
            })
            .stream("orchestration.subscribeThread", move |_ctx, payload| {
                let subscriptions = Arc::clone(&thread);
                async move {
                    let input: OrchestrationSubscribeThreadInput = serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))?;
                    let stream = subscriptions.subscribe_thread(input).await.map_err(RpcError::fail)?;
                    Ok(stream.map(|item| match item {
                        Ok(item) => serde_json::to_value(item).map_err(|e| RpcError::die(e.to_string())),
                        Err(error) => Err(RpcError::fail(error)),
                    }))
                }
            })
            .unary("orchestration.getArchivedShellSnapshot", move |_ctx, _payload| {
                let subscriptions = Arc::clone(&archived);
                async move {
                    let snapshot = subscriptions.get_archived_shell_snapshot().await.map_err(RpcError::fail)?;
                    serde_json::to_value(snapshot).map_err(|e| RpcError::die(e.to_string()))
                }
            })
            .unary("orchestration.searchThreads", move |_ctx, payload| {
                let subscriptions = Arc::clone(&search);
                async move {
                    let input: OrchestrationSearchThreadsInput = serde_json::from_value(payload).map_err(|error| RpcError::die_text(error.to_string()))?;
                    let result = subscriptions.search_threads(input).await.map_err(RpcError::fail)?;
                    serde_json::to_value(result).map_err(|e| RpcError::die(e.to_string()))
                }
            })
    }

    /// `orchestration.getArchivedShellSnapshot`.
    pub async fn get_archived_shell_snapshot(&self) -> SnapshotResult<OrchestrationShellSnapshot> {
        self.reads.get_archived_shell_snapshot().await.map_err(|cause| {
            tracing::error!(%cause, "orchestration archived shell snapshot load failed");
            snapshot_error_with_cause("Failed to load archived orchestration shell snapshot", &cause)
        })
    }

    /// `orchestration.searchThreads`.
    pub async fn search_threads(&self, input: OrchestrationSearchThreadsInput) -> Result<OrchestrationSearchThreadsResult, OrchestrationSearchThreadsError> {
        self.reads.search_threads(input).await.map_err(|cause| OrchestrationSearchThreadsError {
            tag: LitOrchestrationSearchThreadsError,
            message: "Failed to search threads".to_string(),
            cause: Some(zc_core::defect::Defect::error(&cause.tag, cause.message.clone()).0),
        })
    }

    // -----------------------------------------------------------------------------------------
    // Shell
    // -----------------------------------------------------------------------------------------

    /// `toShellStreamEvent`: refetches the aggregate's current shell.
    async fn to_shell_stream_event(&self, event: &ShellEvent) -> Option<OrchestrationShellStreamEvent> {
        match event.event_type.as_str() {
            "project.created" | "project.meta-updated" => self.project_upsert_or_remove(&event.aggregate_id, event.sequence).await,
            "project.deleted" => Some(OrchestrationShellStreamEvent::ProjectRemoved(OrchestrationShellStreamEventProjectRemoved {
                kind: LitProjectRemoved,
                sequence: event.sequence,
                project_id: ProjectId::new(event.aggregate_id.clone()),
            })),
            "thread.deleted" | "thread.archived" => Some(OrchestrationShellStreamEvent::ThreadRemoved(OrchestrationShellStreamEventThreadRemoved {
                kind: LitThreadRemoved,
                sequence: event.sequence,
                thread_id: ThreadId::new(event.aggregate_id.clone()),
            })),
            "thread.unarchived" => self.thread_upsert_or_remove(&event.aggregate_id, event.sequence).await,
            _ if event.aggregate_kind != "thread" => None,
            _ => self.thread_upsert_or_remove(&event.aggregate_id, event.sequence).await,
        }
    }

    /// `retryShellProjectionRead`: one retry; a second failure logs and drops the item (a
    /// failed read must not look like a removal).
    async fn project_upsert_or_remove(&self, project_id: &str, sequence: i64) -> Option<OrchestrationShellStreamEvent> {
        let id = ProjectId::new(project_id);
        let read = match self.reads.get_project_shell_by_id(&id).await {
            Ok(read) => read,
            Err(_) => match self.reads.get_project_shell_by_id(&id).await {
                Ok(read) => read,
                Err(error) => {
                    tracing::warn!(aggregate_kind = "project", aggregate_id = project_id, %error, "orchestration shell projection refetch failed");
                    return None;
                }
            },
        };
        Some(match read {
            None => OrchestrationShellStreamEvent::ProjectRemoved(OrchestrationShellStreamEventProjectRemoved {
                kind: LitProjectRemoved,
                sequence,
                project_id: id,
            }),
            Some(project) => OrchestrationShellStreamEvent::ProjectUpserted(OrchestrationShellStreamEventProjectUpserted {
                kind: LitProjectUpserted,
                sequence,
                project,
            }),
        })
    }

    /// Refetches a thread's shell: an upsert while it is active, a `thread-removed` once the
    /// projection has no active row for it (which keeps coalescing correct).
    async fn thread_upsert_or_remove(&self, thread_id: &str, sequence: i64) -> Option<OrchestrationShellStreamEvent> {
        let id = ThreadId::new(thread_id);
        let read = match self.reads.get_thread_shell_by_id(&id).await {
            Ok(read) => read,
            Err(_) => match self.reads.get_thread_shell_by_id(&id).await {
                Ok(read) => read,
                Err(error) => {
                    tracing::warn!(aggregate_kind = "thread", aggregate_id = thread_id, %error, "orchestration shell projection refetch failed");
                    return None;
                }
            },
        };
        Some(match read {
            None => OrchestrationShellStreamEvent::ThreadRemoved(OrchestrationShellStreamEventThreadRemoved {
                kind: LitThreadRemoved,
                sequence,
                thread_id: id,
            }),
            Some(thread) => OrchestrationShellStreamEvent::ThreadUpserted(OrchestrationShellStreamEventThreadUpserted {
                kind: LitThreadUpserted,
                sequence,
                thread,
            }),
        })
    }

    /// `coalesceShellEvents`: the latest event per aggregate, in sequence order, each refetched
    /// (bounded concurrency, order preserved).
    async fn coalesce_shell_events(&self, events: Vec<ShellEvent>) -> Vec<OrchestrationShellStreamEvent> {
        if events.is_empty() {
            return Vec::new();
        }
        let mut latest: Vec<(String, ShellEvent)> = Vec::new();
        for event in events {
            let key = format!("{}:{}", event.aggregate_kind, event.aggregate_id);
            match latest.iter_mut().find(|(existing, _)| *existing == key) {
                Some(entry) => entry.1 = event,
                None => latest.push((key, event)),
            }
        }
        let mut survivors: Vec<ShellEvent> = latest.into_iter().map(|(_, event)| event).collect();
        survivors.sort_by_key(|event| event.sequence);
        stream::iter(survivors)
            .map(|event| async move { self.to_shell_stream_event(&event).await })
            .buffered(SHELL_REFETCH_CONCURRENCY)
            .filter_map(|item| async move { item })
            .collect()
            .await
    }

    /// `coalesceShellLiveInputs`: splits at markers so one cannot overtake an event still in
    /// the window, and coalesces the event segments.
    async fn coalesce_shell_live_inputs(&self, inputs: Vec<ShellLiveInput>) -> Vec<OrchestrationShellStreamItem> {
        let mut output = Vec::new();
        let mut pending: Vec<ShellEvent> = Vec::new();
        for input in inputs {
            match input {
                ShellLiveInput::Event(event) => pending.push(event),
                ShellLiveInput::Synchronized => {
                    output.extend(
                        self.coalesce_shell_events(std::mem::take(&mut pending))
                            .await
                            .into_iter()
                            .map(OrchestrationShellStreamItem::OrchestrationShellStreamEvent),
                    );
                    output.push(shell_synchronized());
                }
            }
        }
        output.extend(
            self.coalesce_shell_events(pending)
                .await
                .into_iter()
                .map(OrchestrationShellStreamItem::OrchestrationShellStreamEvent),
        );
        output
    }

    async fn coalesce_retained(
        &self,
        budget: &LiveStreamBudget,
        items: Vec<Retained<ShellLiveInput>>,
    ) -> SnapshotResult<Vec<Retained<OrchestrationShellStreamItem>>> {
        let previous: Vec<u64> = items.iter().map(Retained::id).collect();
        let output = self.coalesce_shell_live_inputs(items.into_iter().map(|item| item.value).collect()).await;
        budget.replace(
            &previous,
            output
                .into_iter()
                .map(|item| {
                    let bytes = serialized_size(&item);
                    (item, bytes)
                })
                .collect(),
        )
    }

    /// `canReplayPersistedRange`.
    async fn can_replay_persisted_range(&self, after: i64, head: i64, max_gap: i64) -> SnapshotResult<bool> {
        let gap = head - after;
        if gap < 0 || gap > max_gap {
            return Ok(false);
        }
        let stats = self
            .reads
            .get_event_replay_stats(after, head)
            .await
            .map_err(|cause| snapshot_error_with_cause("Failed to measure orchestration replay range", &cause))?;
        if stats.payload_bytes > ORCHESTRATION_REPLAY_PAYLOAD_BUDGET_BYTES {
            tracing::debug!(
                after,
                head,
                gap,
                event_count = stats.event_count,
                payload_bytes = stats.payload_bytes,
                "orchestration replay replaced by snapshot"
            );
            return Ok(false);
        }
        Ok(true)
    }

    async fn load_shell_snapshot(&self) -> SnapshotResult<OrchestrationShellStreamItem> {
        let snapshot = self.reads.get_shell_snapshot(false).await.map_err(|cause| {
            tracing::error!(%cause, "orchestration shell snapshot load failed");
            snapshot_error_with_cause("Failed to load orchestration shell snapshot", &cause)
        })?;
        Ok(OrchestrationShellStreamItem::Snapshot(OrchestrationShellStreamItemSnapshot {
            kind: LitSnapshot,
            snapshot,
        }))
    }

    /// `orchestration.subscribeShell`.
    pub async fn subscribe_shell(
        &self,
        input: OrchestrationSubscribeShellInput,
    ) -> SnapshotResult<BoxStream<'static, SnapshotResult<OrchestrationShellStreamItem>>> {
        // Attach live delivery into a budgeted buffer BEFORE loading any snapshot or replay.
        let budget = LiveStreamBudget::default();
        let (buffer, mut buffered) = mpsc::unbounded_channel::<Retained<ShellLiveInput>>();
        let (control, mut control_rx) = mpsc::unbounded_channel::<FlushRequest>();
        let mut domain = self.engine.subscribe_domain_events();
        let forward_budget = budget.clone();
        let forward_buffer = buffer.clone();
        let forwarder = tokio::spawn(async move {
            let forward = |event: &OrchestrationEvent| -> bool {
                let shell = ShellEvent::of(event);
                let bytes = serialized_size(&shell);
                match forward_budget.retain_sized(ShellLiveInput::Event(shell), bytes) {
                    Ok(item) => forward_buffer.send(item).is_ok(),
                    Err(_) => false,
                }
            };
            let failed = forward_budget.failed();
            tokio::pin!(failed);
            let mut control_open = true;
            loop {
                tokio::select! {
                    // Stop the consumer even if delivery waits for an ACK.
                    _ = &mut failed => return,
                    _ = forward_buffer.closed() => return,
                    request = control_rx.recv(), if control_open => {
                        let Some(ack) = request else {
                            control_open = false;
                            continue;
                        };
                        // The marker goes behind everything already published.
                        let mut alive = true;
                        while let Some(Some(event)) = domain.next().now_or_never() {
                            if !forward(&event) {
                                alive = false;
                                break;
                            }
                        }
                        let result = if alive {
                            forward_budget
                                .retain(serde_json::json!({"kind": "synchronized"}))
                                .map(|item| {
                                    let _ = forward_buffer.send(item.map(|_| ShellLiveInput::Synchronized));
                                })
                        } else {
                            forward_budget.check()
                        };
                        let _ = ack.send(result);
                        if !alive {
                            return;
                        }
                    }
                    event = domain.next() => {
                        let Some(event) = event else { return };
                        if !forward(&event) {
                            return;
                        }
                    }
                }
            }
        });

        let initial: BoxStream<'static, SnapshotResult<OrchestrationShellStreamItem>> = match input.after_sequence {
            Some(after) => {
                let head = self.engine.latest_sequence().await;
                if !self.can_replay_persisted_range(after, head, SHELL_RESUME_MAX_GAP).await? {
                    let snapshot = self.load_shell_snapshot().await?;
                    stream::iter([Ok(snapshot)]).boxed()
                } else {
                    // Replay only through the head captured above; newer events are in
                    // the live buffer already.
                    let gap = u32::try_from(head - after).unwrap_or(u32::MAX);
                    let events = self.engine.read_events(after, Some(gap));
                    let this = self.clone();
                    GroupedWithin::new(events, SHELL_COALESCE_MAX_CHUNK, SHELL_COALESCE_WINDOW)
                        .then(move |batch| {
                            let this = this.clone();
                            async move {
                                let mut shell_events = Vec::with_capacity(batch.len());
                                for event in batch {
                                    match event {
                                        Ok(event) => shell_events.push(ShellEvent::of(&event)),
                                        Err(cause) => return vec![Err(snapshot_error_with_cause("Failed to replay orchestration shell events", &cause))],
                                    }
                                }
                                this.coalesce_shell_events(shell_events)
                                    .await
                                    .into_iter()
                                    .map(|event| Ok(OrchestrationShellStreamItem::OrchestrationShellStreamEvent(event)))
                                    .collect()
                            }
                        })
                        .flat_map(stream::iter)
                        .boxed()
                }
            }
            None => {
                let snapshot = self.load_shell_snapshot().await?;
                stream::iter([Ok(snapshot)]).boxed()
            }
        };

        // The completion marker goes into the same buffer as live events, so anything
        // buffered while snapshot or replay work was in flight is delivered before it.
        let marker = input.request_completion_marker == Some(true);
        drop(buffer);
        let live = ShellLive {
            subscriptions: self.clone(),
            budget: budget.clone(),
            marker: marker.then_some(control),
        };
        let live_stream = live.into_stream(&mut buffered);
        let delivered = budget.deliver(live_stream);
        let stream = initial.chain(delivered).boxed();
        let close_budget = budget.clone();
        Ok(Scoped {
            inner: stream,
            on_drop: vec![abort_on_drop(forwarder), Box::new(move || close_budget.release_all())],
        }
        .boxed())
    }

    // -----------------------------------------------------------------------------------------
    // Thread
    // -----------------------------------------------------------------------------------------

    /// `orchestration.subscribeThread`.
    pub async fn subscribe_thread(
        &self,
        input: OrchestrationSubscribeThreadInput,
    ) -> SnapshotResult<BoxStream<'static, SnapshotResult<OrchestrationThreadStreamItem>>> {
        let thread_id = input.thread_id.as_str().to_string();
        let reasoning = input.reasoning_messages == Some(true);
        let is_this_thread = {
            let thread_id = thread_id.clone();
            move |event: &OrchestrationEvent| {
                let (event_type, kind, aggregate) = envelope(event);
                kind == "thread" && aggregate == thread_id && is_thread_detail_event(&event_type)
            }
        };

        // Attach live delivery before reading either replay or snapshot state.
        let coalescer = ThreadLiveEventCoalescer::default();
        let mut domain = self.engine.subscribe_domain_events();
        let (control, mut control_rx) = mpsc::unbounded_channel::<FlushRequest>();
        let forward = coalescer.clone();
        let forward_filter = is_this_thread.clone();
        let forwarder = tokio::spawn(async move {
            let to_input = |event: OrchestrationEvent| -> Option<ThreadLiveInput> {
                forward_filter(&event).then(|| ThreadLiveInput::event(project_activity_event(&event, reasoning)))
            };
            let failed = forward.budget().failed();
            tokio::pin!(failed);
            let mut control_open = true;
            loop {
                tokio::select! {
                    _ = &mut failed => return,
                    request = control_rx.recv(), if control_open => {
                        let Some(ack) = request else {
                            control_open = false;
                            continue;
                        };
                        // The marker goes behind everything already published.
                        let mut batch = Vec::new();
                        while let Some(Some(event)) = domain.next().now_or_never() {
                            batch.extend(to_input(event));
                        }
                        batch.push(ThreadLiveInput::Synchronized);
                        let result = forward.offer_all(batch);
                        let failed_now = result.is_err();
                        let _ = ack.send(result);
                        if failed_now {
                            return;
                        }
                    }
                    event = domain.next() => {
                        let Some(event) = event else { return };
                        // `runForEachArray`: one offer per batch the pub/sub has ready.
                        let mut batch: Vec<ThreadLiveInput> = to_input(event).into_iter().collect();
                        while let Some(Some(event)) = domain.next().now_or_never() {
                            batch.extend(to_input(event));
                        }
                        if batch.is_empty() {
                            continue;
                        }
                        if forward.offer_all(batch).is_err() || forward.is_closed() {
                            return;
                        }
                    }
                }
            }
        });
        let scope_coalescer = coalescer.clone();
        let on_drop: Vec<Box<dyn FnOnce() + Send>> = vec![abort_on_drop(forwarder), Box::new(move || scope_coalescer.close())];
        let after_live = move |coalescer: ThreadLiveEventCoalescer, marker: bool| -> BoxStream<'static, SnapshotResult<OrchestrationThreadStreamItem>> {
            let control = control.clone();
            // `Stream.unwrap(offer(synchronized).as(buffered))`: offered (and the delivery
            // stream taken) only when reached.
            stream::once(async move {
                if !marker {
                    return coalescer.stream().boxed();
                }
                let (ack, acked) = tokio::sync::oneshot::channel();
                let offered = match control.send(ack) {
                    Ok(()) => acked.await.unwrap_or_else(|_| coalescer.offer(ThreadLiveInput::Synchronized)),
                    Err(_) => coalescer.offer(ThreadLiveInput::Synchronized),
                };
                match offered {
                    Ok(()) => coalescer.stream().boxed(),
                    Err(error) => stream::iter([Err(error)]).boxed(),
                }
            })
            .flatten()
            .boxed()
        };
        let marker = input.request_completion_marker == Some(true);

        let mut replay_on_missing_snapshot: Option<BoxStream<'static, SnapshotResult<OrchestrationThreadStreamItem>>> = None;
        if let Some(after) = input.after_sequence {
            let head = self.engine.latest_sequence().await;
            let range = ThreadReplayRange {
                thread_id: input.thread_id.clone(),
                from_sequence_exclusive: after,
                to_sequence_inclusive: head,
            };
            let stats = if after > head {
                None
            } else {
                Some(
                    self.engine
                        .get_thread_replay_stats(range.clone(), THREAD_RESUME_MAX_EVENTS)
                        .await
                        .map_err(|cause| snapshot_error_with_cause(format!("Failed to measure thread {thread_id} replay range"), &cause))?,
                )
            };
            if let Some(stats) = stats
                .filter(|stats| stats.event_count <= u64::from(THREAD_RESUME_MAX_EVENTS) && stats.payload_bytes <= ORCHESTRATION_REPLAY_PAYLOAD_BUDGET_BYTES)
            {
                let replay_thread = thread_id.clone();
                let filter = is_this_thread.clone();
                let catch_up = self
                    .engine
                    .read_thread_events(range, Some(THREAD_RESUME_MAX_EVENTS))
                    .filter(move |event| std::future::ready(event.as_ref().map(&filter).unwrap_or(true)))
                    .map(move |event| match event {
                        Ok(event) => Ok(thread_event_item(project_activity_event(&event, reasoning))),
                        Err(cause) => Err(snapshot_error_with_cause(format!("Failed to replay thread {replay_thread} events"), &cause)),
                    });
                let replay = catch_up.chain(after_live(coalescer.clone(), marker)).boxed();
                if !stats.has_create_event {
                    return Ok(Scoped { inner: replay, on_drop }.boxed());
                }
                replay_on_missing_snapshot = Some(replay);
            }
            // A recreated thread needs a fresh snapshot if it still exists. Oversized replays
            // and invalid cursors also use the snapshot path.
        }

        // Windowing the fallback snapshot is opt-in per subscription.
        let window = input.turn_limit.map(|turn_limit| OrchestrationThreadDetailWindow {
            turn_limit: Some(turn_limit),
            before_cursor: None,
        });
        let snapshot = self
            .reads
            .get_thread_detail_snapshot(&input.thread_id, window)
            .await
            .map_err(|cause| snapshot_error_with_cause(format!("Failed to load thread {thread_id}"), &cause))?;
        let Some(snapshot) = snapshot else {
            // The recreated thread can already be deleted: keep the bounded replay.
            if let Some(replay) = replay_on_missing_snapshot {
                return Ok(Scoped { inner: replay, on_drop }.boxed());
            }
            let mut error = snapshot_error(format!("Thread {thread_id} was not found"));
            error.cause = Some(Value::String(thread_id.clone()));
            return Err(error);
        };
        let first = OrchestrationThreadStreamItem::Snapshot(OrchestrationThreadStreamItemSnapshot {
            kind: LitSnapshot,
            snapshot: project_thread_detail_snapshot(snapshot, reasoning),
        });
        let stream = stream::iter([Ok(first)]).chain(after_live(coalescer, marker)).boxed();
        Ok(Scoped { inner: stream, on_drop }.boxed())
    }
}

fn shell_synchronized() -> OrchestrationShellStreamItem {
    OrchestrationShellStreamItem::Synchronized(OrchestrationShellStreamItemSynchronized { kind: LitSynchronized })
}

fn thread_event_item(event: OrchestrationEvent) -> OrchestrationThreadStreamItem {
    OrchestrationThreadStreamItem::Event(OrchestrationThreadStreamItemEvent {
        kind: zc_contracts::LitEvent,
        event,
    })
}

/// The live part of a shell subscription: the optional marker stage (marker into the buffer,
/// then everything buffered, coalesced), then the buffer grouped within 50 ms / 512 items and
/// coalesced per group.
struct ShellLive {
    subscriptions: OrchestrationSubscriptions,
    budget: LiveStreamBudget,
    /// The forwarder's control channel, when a completion marker was requested.
    marker: Option<mpsc::UnboundedSender<FlushRequest>>,
}

impl ShellLive {
    fn into_stream(
        self,
        receiver: &mut mpsc::UnboundedReceiver<Retained<ShellLiveInput>>,
    ) -> BoxStream<'static, SnapshotResult<Retained<OrchestrationShellStreamItem>>> {
        // Move the receiver out (the caller keeps an empty one).
        let (_, empty) = mpsc::unbounded_channel();
        let receiver = std::mem::replace(receiver, empty);
        let ShellLive { subscriptions, budget, marker } = self;
        let shared = Arc::new(tokio::sync::Mutex::new(receiver));
        let marker_stage = {
            let shared = Arc::clone(&shared);
            let subscriptions = subscriptions.clone();
            let budget = budget.clone();
            stream::once(async move {
                let Some(control) = marker else {
                    return stream::iter(Vec::<SnapshotResult<Retained<OrchestrationShellStreamItem>>>::new());
                };
                // `Queue.offer(marker)` then `Queue.takeAll`: the forwarder queues the marker
                // behind every event published so far, then everything buffered is taken.
                let (ack, acked) = tokio::sync::oneshot::channel();
                let queued_marker = match control.send(ack) {
                    Ok(()) => match acked.await {
                        Ok(Ok(())) => true,
                        Ok(Err(error)) => return stream::iter(vec![Err(error)]),
                        Err(_) => false,
                    },
                    Err(_) => false,
                };
                let mut receiver = shared.lock().await;
                let mut drained = Vec::new();
                while let Ok(queued) = receiver.try_recv() {
                    drained.push(queued);
                }
                if !queued_marker {
                    // The forwarder is gone (the pub/sub ended): queue the marker here.
                    match budget.retain(serde_json::json!({"kind": "synchronized"})) {
                        Ok(item) => drained.push(item.map(|_| ShellLiveInput::Synchronized)),
                        Err(error) => return stream::iter(vec![Err(error)]),
                    }
                }
                match subscriptions.coalesce_retained(&budget, drained).await {
                    Ok(items) => stream::iter(items.into_iter().map(Ok).collect::<Vec<_>>()),
                    Err(error) => stream::iter(vec![Err(error)]),
                }
            })
            .flatten()
        };
        let grouped = {
            let receiver_stream = ReceiverStream { shared };
            let subscriptions = subscriptions.clone();
            let budget = budget.clone();
            GroupedWithin::new(receiver_stream, SHELL_COALESCE_MAX_CHUNK, SHELL_COALESCE_WINDOW)
                .then(move |items| {
                    let subscriptions = subscriptions.clone();
                    let budget = budget.clone();
                    async move {
                        match subscriptions.coalesce_retained(&budget, items).await {
                            Ok(items) => items.into_iter().map(Ok).collect::<Vec<_>>(),
                            Err(error) => vec![Err(error)],
                        }
                    }
                })
                .flat_map(stream::iter)
        };
        marker_stage.chain(grouped).boxed()
    }
}

/// The live buffer as a stream (shared with the marker stage).
struct ReceiverStream {
    shared: Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Retained<ShellLiveInput>>>>,
}

impl Stream for ReceiverStream {
    type Item = Retained<ShellLiveInput>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.shared.try_lock() {
            Ok(mut receiver) => receiver.poll_recv(cx),
            Err(_) => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}
