//! `orchestration/LiveStreamBudget.ts`: one budget per live subscription (1,000 items or
//! 8 MiB of serialized payload by default) over everything the subscription holds: the live
//! buffer, coalescing batches, and the batch on the wire waiting for the client's `Ack`. Past
//! the budget the subscription fails with `OrchestrationGetSnapshotError` ("Resume from the
//! last received sequence") instead of growing without bound.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures::Stream;
use serde::Serialize;
use zc_contracts::{LitOrchestrationGetSnapshotError, OrchestrationGetSnapshotError};

pub const LIVE_STREAM_MAX_ITEMS: usize = 1_000;
pub const LIVE_STREAM_MAX_SERIALIZED_BYTES: usize = 8 * 1024 * 1024;

pub const BUFFER_FULL_MESSAGE: &str = "The live event buffer is full. Resume from the last received sequence.";

/// `new OrchestrationGetSnapshotError({message})`.
pub fn snapshot_error(message: impl Into<String>) -> OrchestrationGetSnapshotError {
    OrchestrationGetSnapshotError {
        tag: LitOrchestrationGetSnapshotError,
        message: message.into(),
        cause: None,
    }
}

/// A value the budget accounts for, with its serialized size.
#[derive(Debug, Clone)]
pub struct Retained<T> {
    pub value: T,
    pub serialized_bytes: usize,
    id: u64,
}

impl<T> Retained<T> {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The same charge, carrying `f(value)`.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Retained<U> {
        Retained {
            value: f(self.value),
            serialized_bytes: self.serialized_bytes,
            id: self.id,
        }
    }
}

/// `Buffer.byteLength(JSON.stringify(value))`.
pub fn serialized_size<T: Serialize + ?Sized>(value: &T) -> usize {
    serde_json::to_vec(value).map(|bytes| bytes.len()).unwrap_or(0)
}

#[derive(Debug)]
struct State {
    max_items: usize,
    max_bytes: usize,
    retained: HashMap<u64, usize>,
    retained_bytes: usize,
    next_id: u64,
    failure: Option<OrchestrationGetSnapshotError>,
    wakers: Vec<Waker>,
    /// Items a delivery stream handed to the RPC layer and that wait for the client's ACK.
    in_flight: std::collections::HashSet<u64>,
}

/// `makeLiveStreamBudget`. Cloning shares the budget.
#[derive(Debug, Clone)]
pub struct LiveStreamBudget {
    state: Arc<Mutex<State>>,
}

/// Retained items and bytes (`usage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetUsage {
    pub retained_items: usize,
    pub retained_serialized_bytes: usize,
}

impl Default for LiveStreamBudget {
    fn default() -> Self {
        Self::new(LIVE_STREAM_MAX_ITEMS, LIVE_STREAM_MAX_SERIALIZED_BYTES)
    }
}

impl LiveStreamBudget {
    pub fn new(max_items: usize, max_serialized_bytes: usize) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                max_items,
                max_bytes: max_serialized_bytes,
                retained: HashMap::new(),
                retained_bytes: 0,
                next_id: 0,
                failure: None,
                wakers: Vec::new(),
                in_flight: std::collections::HashSet::new(),
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `check`: the failure, once the budget overflowed.
    pub fn check(&self) -> Result<(), OrchestrationGetSnapshotError> {
        match &self.lock().failure {
            Some(failure) => Err(failure.clone()),
            None => Ok(()),
        }
    }

    pub fn failure(&self) -> Option<OrchestrationGetSnapshotError> {
        self.lock().failure.clone()
    }

    pub fn usage(&self) -> BudgetUsage {
        let state = self.lock();
        BudgetUsage {
            retained_items: state.retained.len(),
            retained_serialized_bytes: state.retained_bytes,
        }
    }

    fn overflow(state: &mut State, next_items: usize, next_bytes: usize) -> OrchestrationGetSnapshotError {
        let failure = state.failure.get_or_insert_with(|| snapshot_error(BUFFER_FULL_MESSAGE)).clone();
        tracing::warn!(
            retained_items = state.retained.len(),
            retained_serialized_bytes = state.retained_bytes,
            next_items,
            next_serialized_bytes = next_bytes,
            max_items = state.max_items,
            max_serialized_bytes = state.max_bytes,
            "orchestration live event buffer is full"
        );
        // The subscription is over: close every buffer at once (queued, coalescing and
        // pending items), keeping only the batch on the wire charged until its stream ends.
        let in_flight = state.in_flight.clone();
        state.retained.retain(|id, _| in_flight.contains(id));
        state.retained_bytes = state.retained.values().sum();
        for waker in state.wakers.drain(..) {
            waker.wake();
        }
        failure
    }

    /// `retain(value, payload)`: charges `payload`'s serialized size.
    pub fn retain_sized<T>(&self, value: T, serialized_bytes: usize) -> Result<Retained<T>, OrchestrationGetSnapshotError> {
        let mut state = self.lock();
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        let next_items = state.retained.len() + 1;
        let next_bytes = state.retained_bytes + serialized_bytes;
        if next_items > state.max_items || next_bytes > state.max_bytes {
            return Err(Self::overflow(&mut state, next_items, next_bytes));
        }
        let id = state.next_id;
        state.next_id += 1;
        state.retained.insert(id, serialized_bytes);
        state.retained_bytes = next_bytes;
        Ok(Retained { value, serialized_bytes, id })
    }

    /// `retain(value)`: charges the value's own serialized size.
    pub fn retain<T: Serialize>(&self, value: T) -> Result<Retained<T>, OrchestrationGetSnapshotError> {
        let bytes = serialized_size(&value);
        self.retain_sized(value, bytes)
    }

    /// `replace(previous, values, payload)`: swaps one coalescing batch atomically. Discarded
    /// items release their charge; the new ones are charged by `payload`'s size.
    pub fn replace<T>(&self, previous: &[u64], values: Vec<(T, usize)>) -> Result<Vec<Retained<T>>, OrchestrationGetSnapshotError> {
        let mut state = self.lock();
        if let Some(failure) = &state.failure {
            return Err(failure.clone());
        }
        let mut next_items = state.retained.len() + values.len();
        let mut next_bytes = state.retained_bytes + values.iter().map(|(_, bytes)| *bytes).sum::<usize>();
        for id in previous {
            if let Some(bytes) = state.retained.get(id) {
                next_items -= 1;
                next_bytes -= bytes;
            }
        }
        if next_items > state.max_items || next_bytes > state.max_bytes {
            return Err(Self::overflow(&mut state, next_items, next_bytes));
        }
        for id in previous {
            if let Some(bytes) = state.retained.remove(id) {
                state.retained_bytes -= bytes;
            }
        }
        let mut out = Vec::with_capacity(values.len());
        for (value, bytes) in values {
            let id = state.next_id;
            state.next_id += 1;
            state.retained.insert(id, bytes);
            out.push(Retained {
                value,
                serialized_bytes: bytes,
                id,
            });
        }
        state.retained_bytes = next_bytes;
        Ok(out)
    }

    /// `release(items)`.
    pub fn release(&self, ids: impl IntoIterator<Item = u64>) {
        let mut state = self.lock();
        for id in ids {
            state.in_flight.remove(&id);
            if let Some(bytes) = state.retained.remove(&id) {
                state.retained_bytes -= bytes;
            }
        }
    }

    fn mark_in_flight(&self, id: u64) {
        self.lock().in_flight.insert(id);
    }

    /// Releases everything (the subscription ended).
    pub fn release_all(&self) {
        let mut state = self.lock();
        state.retained.clear();
        state.in_flight.clear();
        state.retained_bytes = 0;
    }

    /// Resolves once the budget has failed (`failed`).
    pub fn failed(&self) -> BudgetFailed {
        BudgetFailed { budget: self.clone() }
    }

    fn register_waker(&self, waker: &Waker) {
        let mut state = self.lock();
        if !state.wakers.iter().any(|existing| existing.will_wake(waker)) {
            state.wakers.push(waker.clone());
        }
    }

    /// `deliver(stream)`: the delivery side. Items stay charged while they are on the wire:
    /// a batch is released when the RPC layer polls again after its `Pending` (it polls again
    /// only once the client acknowledged the batch). A budget failure ends the stream with
    /// the error at once, even while the source is waiting.
    pub fn deliver<T, S>(&self, source: S) -> Delivered<T, S>
    where
        S: Stream<Item = Result<Retained<T>, OrchestrationGetSnapshotError>> + Unpin,
    {
        Delivered {
            _items: std::marker::PhantomData,
            budget: self.clone(),
            source: Some(source),
            in_flight: Vec::new(),
            last_pending: false,
            done: false,
        }
    }
}

/// Future of [`LiveStreamBudget::failed`].
pub struct BudgetFailed {
    budget: LiveStreamBudget,
}

impl std::future::Future for BudgetFailed {
    type Output = OrchestrationGetSnapshotError;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(failure) = self.budget.failure() {
            return Poll::Ready(failure);
        }
        self.budget.register_waker(cx.waker());
        match self.budget.failure() {
            Some(failure) => Poll::Ready(failure),
            None => Poll::Pending,
        }
    }
}

/// Stream of [`LiveStreamBudget::deliver`].
pub struct Delivered<T, S> {
    _items: std::marker::PhantomData<fn() -> T>,
    budget: LiveStreamBudget,
    source: Option<S>,
    in_flight: Vec<u64>,
    last_pending: bool,
    done: bool,
}

impl<T, S> Delivered<T, S> {
    fn fail(&mut self, error: OrchestrationGetSnapshotError) -> Poll<Option<Result<T, OrchestrationGetSnapshotError>>> {
        self.done = true;
        // Close the source (the budget already released what it held); the stream ends here,
        // so the batch on the wire is released too.
        self.source = None;
        let delivered = std::mem::take(&mut self.in_flight);
        self.budget.release(delivered);
        Poll::Ready(Some(Err(error)))
    }
}

impl<T, S> Stream for Delivered<T, S>
where
    S: Stream<Item = Result<Retained<T>, OrchestrationGetSnapshotError>> + Unpin,
    T: Unpin,
{
    type Item = Result<T, OrchestrationGetSnapshotError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        if this.done {
            return Poll::Ready(None);
        }
        if this.last_pending {
            let delivered = std::mem::take(&mut this.in_flight);
            this.budget.release(delivered);
            this.last_pending = false;
        }
        if let Err(error) = this.budget.check() {
            return this.fail(error);
        }
        let Some(source) = this.source.as_mut() else {
            return Poll::Ready(None);
        };
        match Pin::new(source).poll_next(cx) {
            Poll::Ready(Some(Ok(item))) => {
                if let Err(error) = this.budget.check() {
                    return this.fail(error);
                }
                this.in_flight.push(item.id);
                this.budget.mark_in_flight(item.id);
                Poll::Ready(Some(Ok(item.value)))
            }
            Poll::Ready(Some(Err(error))) => this.fail(error),
            Poll::Ready(None) => {
                this.done = true;
                let delivered = std::mem::take(&mut this.in_flight);
                this.budget.release(delivered);
                Poll::Ready(None)
            }
            Poll::Pending => {
                this.last_pending = true;
                this.budget.register_waker(cx.waker());
                if let Err(error) = this.budget.check() {
                    return this.fail(error);
                }
                Poll::Pending
            }
        }
    }
}

impl<T, S> Drop for Delivered<T, S> {
    fn drop(&mut self) {
        let delivered = std::mem::take(&mut self.in_flight);
        self.budget.release(delivered);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use serde_json::json;

    // LiveStreamBudget.test.ts
    #[test]
    fn replace_releases_the_previous_batch_charge() {
        let budget = LiveStreamBudget::new(3, 1_000);
        let a = budget.retain(json!({"a": 1})).unwrap();
        let b = budget.retain(json!({"b": 2})).unwrap();
        assert_eq!(budget.usage().retained_items, 2);
        let merged = budget.replace(&[a.id(), b.id()], vec![(json!({"m": 1}), 7)]).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            budget.usage(),
            BudgetUsage {
                retained_items: 1,
                retained_serialized_bytes: 7
            }
        );
    }

    #[test]
    fn overflow_fails_every_later_call() {
        let budget = LiveStreamBudget::new(2, 1_000);
        budget.retain(1).unwrap();
        budget.retain(2).unwrap();
        let error = budget.retain(3).unwrap_err();
        assert_eq!(error.message, BUFFER_FULL_MESSAGE);
        assert!(budget.check().is_err());
        assert!(budget.retain(4).is_err());
        let bytes = LiveStreamBudget::new(10, 5);
        assert!(bytes.retain("toolong").is_err());
    }

    #[tokio::test]
    async fn failed_resolves_on_overflow() {
        let budget = LiveStreamBudget::new(1, 1_000);
        let failed = budget.failed();
        let trigger = budget.clone();
        tokio::spawn(async move {
            trigger.retain(1).unwrap();
            let _ = trigger.retain(2);
        });
        assert_eq!(failed.await.message, BUFFER_FULL_MESSAGE);
    }

    #[tokio::test]
    async fn delivery_keeps_items_charged_until_the_next_pull() {
        let budget = LiveStreamBudget::new(10, 1_000);
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        let source = tokio_stream_from(receiver);
        let mut delivered = budget.deliver(source);
        sender.send(Ok(budget.retain(1).unwrap())).unwrap();
        sender.send(Ok(budget.retain(2).unwrap())).unwrap();
        assert_eq!(delivered.next().await.unwrap().unwrap(), 1);
        assert_eq!(delivered.next().await.unwrap().unwrap(), 2);
        // Still on the wire.
        assert_eq!(budget.usage().retained_items, 2);
        // The batch ends (Pending), then the next poll after the ACK releases it.
        assert!(futures::poll!(delivered.next()).is_pending());
        sender.send(Ok(budget.retain(3).unwrap())).unwrap();
        assert_eq!(delivered.next().await.unwrap().unwrap(), 3);
        assert_eq!(budget.usage().retained_items, 1);
    }

    #[tokio::test]
    async fn delivery_fails_when_the_budget_overflows() {
        let budget = LiveStreamBudget::new(1, 1_000);
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<Result<Retained<i32>, OrchestrationGetSnapshotError>>();
        let mut delivered = budget.deliver(tokio_stream_from(receiver));
        let trigger = budget.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            trigger.retain(1).unwrap();
            let _ = trigger.retain(2);
        });
        let result = delivered.next().await.unwrap();
        assert_eq!(result.unwrap_err().message, BUFFER_FULL_MESSAGE);
        assert!(delivered.next().await.is_none());
        assert_eq!(budget.usage().retained_items, 0);
        drop(sender);
    }

    // LiveStreamBudget.test.ts: "closes the source without releasing a batch still waiting
    // for an ACK".
    #[tokio::test]
    async fn overflow_closes_the_source_but_keeps_the_unacknowledged_batch() {
        let budget = LiveStreamBudget::new(3, LIVE_STREAM_MAX_SERIALIZED_BYTES);
        let items: Vec<_> = ["first", "second", "third"]
            .into_iter()
            .map(|text| Ok(budget.retain(json!({"text": text})).unwrap()))
            .collect();
        let mut delivered = budget.deliver(futures::stream::iter(items));
        assert_eq!(delivered.next().await.unwrap().unwrap(), json!({"text": "first"}));
        assert_eq!(budget.usage().retained_items, 3);
        assert!(budget.retain(json!({"text": "fourth"})).is_err());
        // The consumer is not resumed: only the batch on the wire stays charged.
        assert_eq!(budget.usage().retained_items, 1);
        drop(delivered);
        assert_eq!(
            budget.usage(),
            BudgetUsage {
                retained_items: 0,
                retained_serialized_bytes: 0
            }
        );
    }

    fn tokio_stream_from<T: Send + 'static>(mut receiver: tokio::sync::mpsc::UnboundedReceiver<T>) -> futures::stream::BoxStream<'static, T> {
        Box::pin(futures::stream::poll_fn(move |cx| receiver.poll_recv(cx)))
    }
}
