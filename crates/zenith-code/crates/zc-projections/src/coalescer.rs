//! `orchestration/ThreadLiveEventCoalescer.ts`: the live tail of `subscribeThread`. Runs of
//! `tool.updated` activities are held for a short window (50 ms, or until 512 are pending)
//! and only the latest update per stable tool call survives; any other event or a
//! synchronization marker flushes the run at once, so ordering is kept. Everything it holds
//! is charged to a [`LiveStreamBudget`].

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zc_contracts::{
    LitEvent, LitSynchronized, OrchestrationEvent, OrchestrationGetSnapshotError, OrchestrationThreadStreamItem, OrchestrationThreadStreamItemEvent,
    OrchestrationThreadStreamItemSynchronized,
};

use crate::activity_payload::project_activity_event;
use crate::budget::{serialized_size, BudgetUsage, Delivered, LiveStreamBudget, Retained};
use crate::js;

pub const COALESCE_WINDOW: Duration = Duration::from_millis(50);
pub const MAX_PENDING_UPDATES: usize = 512;

/// `ThreadLiveInput`.
#[derive(Debug, Clone)]
pub enum ThreadLiveInput {
    Event(Box<OrchestrationEvent>),
    Synchronized,
}

impl ThreadLiveInput {
    pub fn event(event: OrchestrationEvent) -> Self {
        Self::Event(Box::new(event))
    }
}

fn is_tool_updated(event: &OrchestrationEvent) -> bool {
    matches!(event, OrchestrationEvent::ThreadActivityAppended(appended)
        if appended.payload.activity.kind.as_str() == "tool.updated")
}

fn as_trimmed_string(value: Option<&Value>) -> Option<String> {
    let trimmed = js::trim(value?.as_str()?);
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn stable_tool_call_identity(event: &OrchestrationEvent) -> Option<String> {
    let OrchestrationEvent::ThreadActivityAppended(appended) = event else {
        return None;
    };
    let payload = appended.payload.activity.payload.as_object()?;
    let data = payload.get("data").and_then(Value::as_object);
    as_trimmed_string(payload.get("toolCallId")).or_else(|| as_trimmed_string(data.and_then(|data| data.get("toolCallId"))))
}

fn activity_turn_id(event: &OrchestrationEvent) -> String {
    match event {
        OrchestrationEvent::ThreadActivityAppended(appended) => {
            appended.payload.activity.turn_id.as_ref().map(|id| id.as_str().to_string()).unwrap_or_default()
        }
        _ => String::new(),
    }
}

/// The global sequence of an event.
pub fn event_sequence(event: &OrchestrationEvent) -> i64 {
    serde_json::to_value(event)
        .ok()
        .and_then(|value| value.get("sequence").and_then(Value::as_i64))
        .unwrap_or(0)
}

/// `coalesceLiveToolUpdatedEvents`: keeps only the latest in-flight update for each stable
/// tool-call id (per turn) within a run of updates; anonymous calls pass through. Survivors
/// keep their order.
pub fn coalesce_live_tool_updated_events<T>(events: Vec<T>, event_of: impl Fn(&T) -> &OrchestrationEvent) -> Vec<T> {
    let mut survivors: Vec<T> = Vec::new();
    let mut pending: Vec<T> = Vec::new();
    let flush = |pending: &mut Vec<T>, survivors: &mut Vec<T>| {
        let mut seen = std::collections::HashSet::new();
        let mut latest: Vec<T> = Vec::new();
        while let Some(item) = pending.pop() {
            let event = event_of(&item);
            let key = stable_tool_call_identity(event).map(|identity| format!("{}\u{0}{identity}", activity_turn_id(event)));
            if let Some(key) = key {
                if !seen.insert(key) {
                    continue;
                }
            }
            latest.push(item);
        }
        latest.reverse();
        survivors.extend(latest);
    };
    for item in events {
        if is_tool_updated(event_of(&item)) {
            pending.push(item);
            continue;
        }
        flush(&mut pending, &mut survivors);
        survivors.push(item);
    }
    flush(&mut pending, &mut survivors);
    survivors
}

struct State {
    pending: Vec<Retained<OrchestrationEvent>>,
    window_generation: u64,
    window: Option<JoinHandle<()>>,
    closed: bool,
    output: Option<mpsc::UnboundedSender<OutputItem>>,
    /// Dropped on close (or with the state) to stop the failure watcher.
    closed_signal: Option<tokio::sync::oneshot::Sender<()>>,
}

type OutputItem = Result<Retained<OrchestrationThreadStreamItem>, OrchestrationGetSnapshotError>;

fn event_item(event: OrchestrationEvent) -> OrchestrationThreadStreamItem {
    OrchestrationThreadStreamItem::Event(OrchestrationThreadStreamItemEvent { kind: LitEvent, event })
}

fn synchronized_item() -> OrchestrationThreadStreamItem {
    OrchestrationThreadStreamItem::Synchronized(OrchestrationThreadStreamItemSynchronized { kind: LitSynchronized })
}

/// `makeThreadLiveEventCoalescer`. Must be created inside a tokio runtime (the flush window is
/// a task). Dropping it cancels the window.
#[derive(Clone)]
pub struct ThreadLiveEventCoalescer {
    state: Arc<Mutex<State>>,
    budget: LiveStreamBudget,
    window: Duration,
    receiver: Arc<Mutex<Option<mpsc::UnboundedReceiver<OutputItem>>>>,
}

/// Options of [`ThreadLiveEventCoalescer::with_options`].
#[derive(Debug, Clone, Copy)]
pub struct CoalescerOptions {
    pub coalesce_window: Duration,
    pub max_items: usize,
    pub max_serialized_bytes: usize,
}

impl Default for CoalescerOptions {
    fn default() -> Self {
        Self {
            coalesce_window: COALESCE_WINDOW,
            max_items: crate::budget::LIVE_STREAM_MAX_ITEMS,
            max_serialized_bytes: crate::budget::LIVE_STREAM_MAX_SERIALIZED_BYTES,
        }
    }
}

impl Default for ThreadLiveEventCoalescer {
    fn default() -> Self {
        Self::with_options(CoalescerOptions::default())
    }
}

impl ThreadLiveEventCoalescer {
    pub fn with_options(options: CoalescerOptions) -> Self {
        let budget = LiveStreamBudget::new(options.max_items, options.max_serialized_bytes);
        let (sender, receiver) = mpsc::unbounded_channel();
        let (closed_signal, closed) = tokio::sync::oneshot::channel::<()>();
        let coalescer = Self {
            state: Arc::new(Mutex::new(State {
                pending: Vec::new(),
                window_generation: 0,
                window: None,
                closed: false,
                output: Some(sender),
                closed_signal: Some(closed_signal),
            })),
            budget,
            window: options.coalesce_window,
            receiver: Arc::new(Mutex::new(Some(receiver))),
        };
        // `budget.failed` → `close(error)`, until the coalescer closes.
        let failed = coalescer.budget.failed();
        let weak = Arc::downgrade(&coalescer.state);
        let budget = coalescer.budget.clone();
        tokio::spawn(async move {
            tokio::select! {
                error = failed => {
                    if let Some(state) = weak.upgrade() {
                        close(&state, &budget, Some(error));
                    }
                }
                _ = closed => {}
            }
        });
        coalescer
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        lock(&self.state)
    }

    pub fn budget(&self) -> &LiveStreamBudget {
        &self.budget
    }

    pub fn usage(&self) -> BudgetUsage {
        self.budget.usage()
    }

    /// `offer(input)`.
    pub fn offer(&self, input: ThreadLiveInput) -> Result<(), OrchestrationGetSnapshotError> {
        self.offer_all(vec![input])
    }

    /// `offerAll(inputs)`: one source batch at a time, so a synchronization marker cannot pass
    /// events already pulled from the pub/sub but still being coalesced.
    pub fn offer_all(&self, inputs: Vec<ThreadLiveInput>) -> Result<(), OrchestrationGetSnapshotError> {
        let mut state = self.lock();
        for input in inputs {
            self.budget.check()?;
            let tool_updated = match &input {
                ThreadLiveInput::Event(event) => {
                    // Retain only the client payload, not full persisted tool output.
                    let projected = project_activity_event(event, true);
                    let item = self.budget.retain(projected)?;
                    state.pending.push(item);
                    is_tool_updated(event)
                }
                ThreadLiveInput::Synchronized => false,
            };
            if tool_updated {
                if state.pending.len() == 1 {
                    state.window_generation += 1;
                    let generation = state.window_generation;
                    state.window = Some(self.spawn_window(generation));
                }
                if state.pending.len() >= MAX_PENDING_UPDATES {
                    cancel_window(&mut state);
                    state.window_generation += 1;
                    flush_pending(&mut state, &self.budget)?;
                }
                continue;
            }
            cancel_window(&mut state);
            state.window_generation += 1;
            // A non-update event closes the run immediately.
            flush_pending(&mut state, &self.budget)?;
            if matches!(input, ThreadLiveInput::Synchronized) {
                let marker = self.budget.retain(synchronized_item())?;
                if let Some(output) = &state.output {
                    let _ = output.send(Ok(marker));
                }
            }
        }
        Ok(())
    }

    fn spawn_window(&self, generation: u64) -> JoinHandle<()> {
        let state = Arc::downgrade(&self.state);
        let budget = self.budget.clone();
        let window = self.window;
        tokio::spawn(async move {
            tokio::time::sleep(window).await;
            let Some(state) = state.upgrade() else {
                return;
            };
            let mut state = lock(&state);
            if generation == state.window_generation {
                // A budget failure here closes the coalescer through `failed`.
                let _ = flush_pending(&mut state, &budget);
                state.window = None;
            }
        })
    }

    /// The delivery stream (`stream`). Can be taken once.
    pub fn stream(&self) -> Delivered<OrchestrationThreadStreamItem, OutputStream> {
        let receiver = self.receiver.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
        self.budget.deliver(OutputStream { receiver })
    }

    /// Closes the coalescer (the subscription's scope ended).
    pub fn close(&self) {
        close(&self.state, &self.budget, None);
    }

    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }
}

fn lock(state: &Arc<Mutex<State>>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn cancel_window(state: &mut State) {
    if let Some(window) = state.window.take() {
        window.abort();
    }
}

fn flush_pending(state: &mut State, budget: &LiveStreamBudget) -> Result<(), OrchestrationGetSnapshotError> {
    if state.pending.is_empty() {
        return Ok(());
    }
    let previous: Vec<u64> = state.pending.iter().map(Retained::id).collect();
    let pending = std::mem::take(&mut state.pending);
    let survivors = coalesce_live_tool_updated_events(pending, |item| &item.value);
    let values: Vec<(OrchestrationThreadStreamItem, usize)> = survivors
        .into_iter()
        .map(|item| {
            let bytes = serialized_size(&item.value);
            (event_item(item.value), bytes)
        })
        .collect();
    let items = budget.replace(&previous, values)?;
    if let Some(output) = &state.output {
        for item in items {
            let _ = output.send(Ok(item));
        }
    }
    Ok(())
}

/// `close(error?)`: releases what is pending or queued, fails the output with `error`.
fn close(state: &Arc<Mutex<State>>, budget: &LiveStreamBudget, error: Option<OrchestrationGetSnapshotError>) {
    let mut state = lock(state);
    if state.closed {
        return;
    }
    state.closed = true;
    state.closed_signal = None;
    state.window_generation += 1;
    cancel_window(&mut state);
    budget.release(state.pending.drain(..).map(|item| item.id()).collect::<Vec<_>>());
    if let Some(output) = state.output.take() {
        if let Some(error) = error {
            let _ = output.send(Err(error));
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        if let Some(window) = self.window.take() {
            window.abort();
        }
    }
}

/// The coalescer's output queue as a stream.
pub struct OutputStream {
    receiver: Option<mpsc::UnboundedReceiver<OutputItem>>,
}

impl Stream for OutputStream {
    type Item = OutputItem;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.receiver.as_mut() {
            Some(receiver) => receiver.poll_recv(cx),
            None => Poll::Ready(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    fn tool_activity(sequence: i64, kind: &str, tool_call_id: &str, turn_id: &str) -> OrchestrationEvent {
        serde_json::from_value(json!({
            "sequence": sequence,
            "eventId": format!("event-{sequence}"),
            "aggregateKind": "thread",
            "aggregateId": "thread-coalescer-test",
            "occurredAt": "2026-01-01T00:00:01.000Z",
            "commandId": null,
            "causationEventId": null,
            "correlationId": null,
            "metadata": {},
            "type": "thread.activity-appended",
            "payload": {
                "threadId": "thread-coalescer-test",
                "activity": {
                    "id": format!("activity-{sequence}"),
                    "tone": "tool",
                    "kind": kind,
                    "summary": "Editing app.ts",
                    "payload": {
                        "itemType": "file_change",
                        "title": "Editing app.ts",
                        "data": if tool_call_id.is_empty() { json!({}) } else { json!({"toolCallId": tool_call_id}) },
                    },
                    "turnId": turn_id,
                    "createdAt": "2026-01-01T00:00:01.000Z",
                }
            }
        }))
        .unwrap()
    }

    fn update(sequence: i64) -> OrchestrationEvent {
        tool_activity(sequence, "tool.updated", "call-edit", "turn-coalescer-test")
    }

    fn message(sequence: i64, text: &str) -> OrchestrationEvent {
        serde_json::from_value(json!({
            "sequence": sequence,
            "eventId": format!("event-{sequence}"),
            "aggregateKind": "thread",
            "aggregateId": "thread-coalescer-test",
            "occurredAt": "2026-01-01T00:00:02.000Z",
            "commandId": null,
            "causationEventId": null,
            "correlationId": null,
            "metadata": {},
            "type": "thread.message-sent",
            "payload": {
                "threadId": "thread-coalescer-test",
                "messageId": format!("message-{sequence}"),
                "role": "assistant",
                "text": text,
                "turnId": "turn-coalescer-test",
                "streaming": false,
                "createdAt": "2026-01-01T00:00:02.000Z",
                "updatedAt": "2026-01-01T00:00:02.000Z",
            }
        }))
        .unwrap()
    }

    fn sequences(events: Vec<OrchestrationEvent>) -> Vec<i64> {
        coalesce_live_tool_updated_events(events, |event| event).iter().map(event_sequence).collect()
    }

    fn label(item: &OrchestrationThreadStreamItem) -> String {
        match item {
            OrchestrationThreadStreamItem::Event(event) => event_sequence(&event.event).to_string(),
            OrchestrationThreadStreamItem::Synchronized(_) => "synchronized".into(),
            OrchestrationThreadStreamItem::Snapshot(_) => "snapshot".into(),
        }
    }

    #[test]
    fn coalesces_only_calls_with_a_stable_tool_call_id() {
        let turn = "turn-coalescer-test";
        assert_eq!(
            sequences(vec![
                tool_activity(1, "tool.updated", "call-a", turn),
                tool_activity(2, "tool.updated", "call-b", turn),
                tool_activity(3, "tool.updated", "call-a", turn),
            ]),
            vec![2, 3]
        );
    }

    #[test]
    fn preserves_parallel_same_label_calls_without_a_stable_id() {
        let turn = "turn-coalescer-test";
        assert_eq!(
            sequences(vec![
                tool_activity(1, "tool.updated", "", turn),
                tool_activity(2, "tool.updated", "", turn),
                tool_activity(3, "tool.completed", "", turn),
            ]),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn does_not_coalesce_stable_calls_across_turns() {
        assert_eq!(
            sequences(vec![
                tool_activity(1, "tool.updated", "call-edit", "turn-old"),
                tool_activity(2, "tool.updated", "call-edit", "turn-new"),
            ]),
            vec![1, 2]
        );
    }

    #[test]
    fn flushes_a_stable_update_run_before_a_completion() {
        let turn = "turn-coalescer-test";
        assert_eq!(
            sequences(vec![update(1), update(2), tool_activity(3, "tool.completed", "call-edit", turn), update(4),]),
            vec![2, 3, 4]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn flushes_pending_updates_as_soon_as_an_unrelated_event_arrives() {
        let coalescer = ThreadLiveEventCoalescer::with_options(CoalescerOptions {
            coalesce_window: Duration::from_millis(500),
            ..Default::default()
        });
        let started = tokio::time::Instant::now();
        for sequence in 2..12 {
            coalescer.offer(ThreadLiveInput::event(update(sequence))).unwrap();
        }
        coalescer.offer(ThreadLiveInput::event(message(12, "Still working"))).unwrap();
        let items: Vec<String> = coalescer.stream().take(2).map(|item| label(&item.unwrap())).collect().await;
        assert_eq!(items, vec!["11", "12"]);
        assert_eq!(tokio::time::Instant::now(), started);
    }

    #[tokio::test(start_paused = true)]
    async fn flushes_pending_updates_as_soon_as_a_marker_arrives() {
        let coalescer = ThreadLiveEventCoalescer::with_options(CoalescerOptions {
            coalesce_window: Duration::from_millis(500),
            ..Default::default()
        });
        coalescer.offer(ThreadLiveInput::event(update(2))).unwrap();
        coalescer.offer(ThreadLiveInput::event(update(3))).unwrap();
        coalescer.offer(ThreadLiveInput::Synchronized).unwrap();
        let items: Vec<String> = coalescer.stream().take(2).map(|item| label(&item.unwrap())).collect().await;
        assert_eq!(items, vec!["3", "synchronized"]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_window_flushes_a_lone_update() {
        let coalescer = ThreadLiveEventCoalescer::with_options(CoalescerOptions {
            coalesce_window: Duration::from_millis(50),
            ..Default::default()
        });
        coalescer.offer(ThreadLiveInput::event(update(1))).unwrap();
        let mut stream = coalescer.stream();
        assert!(futures::poll!(stream.next()).is_pending());
        tokio::time::advance(Duration::from_millis(50)).await;
        assert_eq!(label(&stream.next().await.unwrap().unwrap()), "1");
    }

    #[tokio::test]
    async fn fails_and_clears_pending_updates_when_the_payload_fills_the_budget() {
        let first = update(1);
        let first_bytes = serialized_size(&project_activity_event(&first, true));
        let coalescer = ThreadLiveEventCoalescer::with_options(CoalescerOptions {
            coalesce_window: Duration::from_millis(500),
            max_serialized_bytes: first_bytes,
            ..Default::default()
        });
        coalescer.offer(ThreadLiveInput::event(first)).unwrap();
        assert_eq!(
            coalescer.usage(),
            BudgetUsage {
                retained_items: 1,
                retained_serialized_bytes: first_bytes
            }
        );
        assert!(coalescer.offer(ThreadLiveInput::event(update(2))).is_err());
        tokio::task::yield_now().await;
        assert_eq!(coalescer.usage().retained_items, 0);
        assert!(coalescer.offer(ThreadLiveInput::Synchronized).is_err());
        let delivered: Vec<_> = coalescer.stream().collect().await;
        assert!(delivered.iter().any(Result::is_err));
    }

    #[tokio::test]
    async fn keeps_an_unacknowledged_batch_charged_and_clears_later_events_on_overflow() {
        let coalescer = ThreadLiveEventCoalescer::with_options(CoalescerOptions {
            max_items: 3,
            ..Default::default()
        });
        let first = message(1, &"é".repeat(1_024));
        let first_bytes = serialized_size(&first);
        coalescer.offer(ThreadLiveInput::event(first)).unwrap();
        let mut stream = coalescer.stream();
        assert_eq!(label(&stream.next().await.unwrap().unwrap()), "1");
        coalescer.offer(ThreadLiveInput::event(message(2, "x"))).unwrap();
        coalescer.offer(ThreadLiveInput::event(update(3))).unwrap();
        assert_eq!(coalescer.usage().retained_items, 3);
        let completed = tool_activity(4, "tool.completed", "call-edit", "turn-coalescer-test");
        assert!(coalescer.offer(ThreadLiveInput::event(completed)).is_err());
        tokio::task::yield_now().await;
        // Only the unacknowledged batch is still charged.
        assert_eq!(
            coalescer.usage(),
            BudgetUsage {
                retained_items: 1,
                retained_serialized_bytes: first_bytes
            }
        );
        drop(stream);
        assert_eq!(coalescer.usage().retained_items, 0);
    }
}
