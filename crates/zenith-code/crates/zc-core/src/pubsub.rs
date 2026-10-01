//! In-process fan-out, the Rust shape of Effect `PubSub.unbounded` + `Stream.fromSubscription`.
//!
//! The TS server publishes orchestration events, settings changes, provider events, terminal
//! events, … on unbounded PubSubs: **every subscriber gets every message, nothing is dropped**,
//! and a slow subscriber only grows its own queue. `tokio::sync::broadcast` would drop messages
//! (lagging receivers), so this uses one unbounded channel per subscriber instead.
//!
//! Subscribing is eager: a [`Subscription`] receives everything published after
//! [`PubSub::subscribe`] returns, whether or not it has been polled yet. That is the
//! "subscribe before snapshot" rule the WS streams depend on (`ws.ts` attaches live delivery
//! before reading the snapshot, so nothing between the two is lost):
//!
//! - [`subscribe_before_snapshot`]: subscribe, then read the snapshot (duplicates possible,
//!   losses impossible), like `subscribeBeforeSnapshotWithoutMutex`;
//! - [`SnapshotHub`]: keeps the latest value and publishes under one lock, so
//!   [`SnapshotHub::subscribe`] returns a snapshot and a subscription that continue each other
//!   exactly, like `subscribeBeforeSnapshot` with its mutex.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::Stream;
use tokio::sync::mpsc;

/// An unbounded multi-subscriber broadcast.
pub struct PubSub<T> {
    subscribers: Arc<Mutex<Vec<mpsc::UnboundedSender<T>>>>,
}

impl<T> Clone for PubSub<T> {
    fn clone(&self) -> Self {
        Self {
            subscribers: self.subscribers.clone(),
        }
    }
}

impl<T> Default for PubSub<T> {
    fn default() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl<T> std::fmt::Debug for PubSub<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PubSub").field("subscribers", &self.lock().len()).finish()
    }
}

impl<T: Clone + Send + 'static> PubSub<T> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a subscriber. It receives every message published from now on.
    pub fn subscribe(&self) -> Subscription<T> {
        let (sender, receiver) = mpsc::unbounded_channel();
        self.lock().push(sender);
        Subscription { receiver }
    }

    /// Deliver `value` to every live subscriber; returns how many received it. Dropped
    /// subscriptions are pruned.
    pub fn publish(&self, value: T) -> usize {
        let mut subscribers = self.lock();
        subscribers.retain(|sender| !sender.is_closed());
        let count = subscribers.len();
        if let Some((last, rest)) = subscribers.split_last() {
            for sender in rest {
                let _ = sender.send(value.clone());
            }
            let _ = last.send(value);
        }
        count
    }

    /// Publish several values in order, each to every subscriber.
    pub fn publish_all(&self, values: impl IntoIterator<Item = T>) {
        let mut subscribers = self.lock();
        subscribers.retain(|sender| !sender.is_closed());
        for value in values {
            for sender in subscribers.iter() {
                let _ = sender.send(value.clone());
            }
        }
    }

    /// Live subscribers (dropped ones are counted until the next publish prunes them).
    pub fn subscriber_count(&self) -> usize {
        self.lock().iter().filter(|sender| !sender.is_closed()).count()
    }
}

impl<T> PubSub<T> {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<mpsc::UnboundedSender<T>>> {
        self.subscribers.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// End every subscription (their streams finish after draining what is queued).
    pub fn shutdown(&self) {
        self.lock().clear();
    }
}

/// One subscriber's queue. A `Stream`; dropping it unsubscribes.
#[derive(Debug)]
pub struct Subscription<T> {
    receiver: mpsc::UnboundedReceiver<T>,
}

impl<T> Subscription<T> {
    /// The next message, or `None` once the [`PubSub`] is shut down (or dropped) and drained.
    pub async fn recv(&mut self) -> Option<T> {
        self.receiver.recv().await
    }

    /// A queued message without waiting.
    pub fn try_recv(&mut self) -> Option<T> {
        self.receiver.try_recv().ok()
    }

    /// Everything queued right now, without waiting (for batching chunks).
    pub fn drain_ready(&mut self) -> Vec<T> {
        let mut out = Vec::new();
        while let Ok(value) = self.receiver.try_recv() {
            out.push(value);
        }
        out
    }
}

impl<T> Stream for Subscription<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        self.receiver.poll_recv(cx)
    }
}

/// `subscribeBeforeSnapshotWithoutMutex`: subscribe first, then compute the snapshot. Messages
/// published while the snapshot is read are in the subscription (and may also be reflected in
/// the snapshot, so consumers must tolerate duplicates, e.g. by sequence number).
pub async fn subscribe_before_snapshot<T, A, F>(pubsub: &PubSub<T>, snapshot: F) -> (A, Subscription<T>)
where
    T: Clone + Send + 'static,
    F: Future<Output = A>,
{
    let subscription = pubsub.subscribe();
    let latest = snapshot.await;
    (latest, subscription)
}

/// A latest value plus its change feed, updated atomically (`subscribeBeforeSnapshot` with a
/// mutex): a subscriber gets the value as of subscription and then exactly the later changes.
pub struct SnapshotHub<T> {
    state: Arc<Mutex<HubState<T>>>,
}

struct HubState<T> {
    latest: T,
    changes: PubSub<T>,
}

impl<T> Clone for SnapshotHub<T> {
    fn clone(&self) -> Self {
        Self { state: self.state.clone() }
    }
}

impl<T: Clone + Send + 'static> SnapshotHub<T> {
    pub fn new(initial: T) -> Self {
        Self {
            state: Arc::new(Mutex::new(HubState {
                latest: initial,
                changes: PubSub::new(),
            })),
        }
    }

    /// The current value.
    pub fn latest(&self) -> T {
        self.lock().latest.clone()
    }

    /// Replace the value and notify subscribers.
    pub fn publish(&self, value: T) {
        let mut state = self.lock();
        state.latest = value.clone();
        state.changes.publish(value);
    }

    /// Compute the next value from the current one; publishes it when `update` returns `Some`.
    /// Returns the value now current.
    pub fn update(&self, update: impl FnOnce(&T) -> Option<T>) -> T {
        let mut state = self.lock();
        if let Some(next) = update(&state.latest) {
            state.latest = next.clone();
            state.changes.publish(next);
        }
        state.latest.clone()
    }

    /// The current value and a subscription to every later change, atomically.
    pub fn subscribe(&self) -> (T, Subscription<T>) {
        let state = self.lock();
        (state.latest.clone(), state.changes.subscribe())
    }

    /// A subscription to later changes only.
    pub fn subscribe_changes(&self) -> Subscription<T> {
        self.lock().changes.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubState<T>> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn every_subscriber_gets_every_message_in_order() {
        let pubsub = PubSub::new();
        let mut a = pubsub.subscribe();
        let mut b = pubsub.subscribe();
        for i in 0..10_000 {
            pubsub.publish(i);
        }
        let got_a: Vec<i32> = a.drain_ready();
        let got_b: Vec<i32> = (&mut b).take(10_000).collect().await;
        assert_eq!(got_a, (0..10_000).collect::<Vec<_>>());
        assert_eq!(got_b, got_a, "a slow subscriber loses nothing");
    }

    #[tokio::test]
    async fn subscription_is_live_before_it_is_polled() {
        let pubsub = PubSub::new();
        let subscription = pubsub.subscribe();
        pubsub.publish("early");
        let mut subscription = subscription;
        assert_eq!(subscription.recv().await, Some("early"));
    }

    #[tokio::test]
    async fn dropped_subscriptions_are_pruned_and_shutdown_ends_streams() {
        let pubsub = PubSub::new();
        let dropped = pubsub.subscribe();
        let mut kept = pubsub.subscribe();
        drop(dropped);
        assert_eq!(pubsub.publish(1), 1);
        pubsub.shutdown();
        assert_eq!(kept.recv().await, Some(1));
        assert_eq!(kept.recv().await, None);
    }

    #[tokio::test]
    async fn subscribe_before_snapshot_never_loses_concurrent_messages() {
        let pubsub = PubSub::new();
        let publisher = pubsub.clone();
        let (snapshot, mut subscription) = subscribe_before_snapshot(&pubsub, async move {
            // A message published while the snapshot is being read.
            publisher.publish(7);
            "snapshot"
        })
        .await;
        assert_eq!(snapshot, "snapshot");
        assert_eq!(subscription.recv().await, Some(7));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn snapshot_hub_subscribers_continue_exactly_from_their_snapshot() {
        let hub = SnapshotHub::new(0u64);
        let writer = hub.clone();
        let producer = tokio::spawn(async move {
            for i in 1..=5_000u64 {
                writer.publish(i);
                if i % 100 == 0 {
                    tokio::task::yield_now().await;
                }
            }
        });
        let mut checks = Vec::new();
        for _ in 0..20 {
            let (latest, mut changes) = hub.subscribe();
            checks.push(tokio::spawn(async move {
                let mut expected = latest + 1;
                while expected <= 5_000 {
                    let value = changes.recv().await.unwrap();
                    assert_eq!(value, expected, "no gap and no duplicate after the snapshot");
                    expected += 1;
                }
            }));
            tokio::task::yield_now().await;
        }
        producer.await.unwrap();
        for check in checks {
            check.await.unwrap();
        }
        assert_eq!(hub.latest(), 5_000);
        assert_eq!(hub.update(|v| (*v == 5_000).then_some(1)), 1);
        assert_eq!(hub.update(|_| None), 1);
    }
}
