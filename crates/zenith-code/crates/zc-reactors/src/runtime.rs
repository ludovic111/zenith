//! Runtime plumbing shared by the reactors: the clock they read, the drainable worker
//! (`@t3tools/shared/DrainableWorker`), and the bounded TTL maps that stand in for the
//! Effect `Cache`s of the ingestion and the command reactor.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------------------------
// Clock

/// What the reactors read time from (`Clock.currentTimeMillis`, `DateTime.now`).
pub trait ReactorClock: Send + Sync {
    /// Epoch milliseconds.
    fn now_millis(&self) -> i64;

    /// `DateTime.formatIso(DateTime.now)`.
    fn now_iso(&self) -> String {
        zc_core::time::iso_from_millis(self.now_millis())
    }
}

/// The system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl ReactorClock for SystemClock {
    fn now_millis(&self) -> i64 {
        zc_core::time::now_millis()
    }
}

/// The real clock plus an offset a test (or a replay) advances, like the TS ingestion tests'
/// shifted clock. With [`ManualClock::fixed`] it does not follow the real clock at all.
#[derive(Debug, Default)]
pub struct ManualClock {
    offset: AtomicI64,
    follow_real_clock: bool,
}

impl ManualClock {
    /// Real time plus an adjustable offset.
    pub fn shifted() -> Self {
        Self {
            offset: AtomicI64::new(0),
            follow_real_clock: true,
        }
    }

    /// A clock frozen at `millis` until advanced.
    pub fn fixed(millis: i64) -> Self {
        Self {
            offset: AtomicI64::new(millis),
            follow_real_clock: false,
        }
    }

    pub fn advance(&self, millis: i64) {
        self.offset.fetch_add(millis, Ordering::SeqCst);
    }

    /// Moves a fixed clock to `millis` (never backwards).
    pub fn set(&self, millis: i64) {
        self.offset.fetch_max(millis, Ordering::SeqCst);
    }
}

impl ReactorClock for ManualClock {
    fn now_millis(&self) -> i64 {
        let offset = self.offset.load(Ordering::SeqCst);
        if self.follow_real_clock {
            zc_core::time::now_millis() + offset
        } else {
            offset
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Drainable worker

/// `makeDrainableWorker(process)`: one task processes enqueued items in order; [`drain`]
/// resolves once everything enqueued before it has been processed.
///
/// [`drain`]: DrainableWorker::drain
pub struct DrainableWorker<T> {
    queue: mpsc::UnboundedSender<T>,
    outstanding: Arc<watch::Sender<u64>>,
}

impl<T> Clone for DrainableWorker<T> {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
            outstanding: self.outstanding.clone(),
        }
    }
}

impl<T: Send + 'static> DrainableWorker<T> {
    /// Starts the worker task. It stops when `stop` is cancelled (items left are dropped).
    pub fn start<F, Fut>(stop: CancellationToken, process: F) -> Self
    where
        F: Fn(T) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let (queue, mut items) = mpsc::unbounded_channel::<T>();
        let outstanding = Arc::new(watch::channel(0u64).0);
        let counter = outstanding.clone();
        tokio::spawn(async move {
            loop {
                let item = tokio::select! {
                    _ = stop.cancelled() => break,
                    item = items.recv() => item,
                };
                let Some(item) = item else { break };
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = process(item) => {}
                }
                counter.send_modify(|count| *count = count.saturating_sub(1));
            }
            // Nothing will process what is left: release every waiter.
            counter.send_replace(0);
        });
        Self { queue, outstanding }
    }

    /// `enqueue(item)`.
    pub fn enqueue(&self, item: T) {
        self.outstanding.send_modify(|count| *count += 1);
        if self.queue.send(item).is_err() {
            self.outstanding.send_modify(|count| *count = count.saturating_sub(1));
        }
    }

    /// `drain`: waits until the queue is empty and the current item is done.
    pub async fn drain(&self) {
        let mut receiver = self.outstanding.subscribe();
        let _ = receiver.wait_for(|count| *count == 0).await;
    }

    /// Items enqueued and not yet processed.
    pub fn outstanding(&self) -> u64 {
        *self.outstanding.borrow()
    }
}

// ---------------------------------------------------------------------------------------------
// TTL map

struct TtlEntry<V> {
    value: V,
    /// Milliseconds on the map's time source.
    expires_at: i64,
    order: u64,
}

/// Where a [`TtlMap`] reads time: the tokio clock (pausable in tests) or a reactor clock, as
/// the Effect `Cache` reads the `Clock` service (so a replay's virtual clock ages it too).
#[derive(Clone)]
enum TimeSource {
    Tokio(Instant),
    Clock(Arc<dyn ReactorClock>),
}

impl TimeSource {
    fn now_millis(&self) -> i64 {
        match self {
            TimeSource::Tokio(origin) => i64::try_from(Instant::now().duration_since(*origin).as_millis()).unwrap_or(i64::MAX),
            TimeSource::Clock(clock) => clock.now_millis(),
        }
    }
}

/// A bounded map with per-entry time to live (Effect `Cache` used as a map: `set`,
/// `getOption`, `invalidate`, `keys`). When full, the oldest entry goes. Not shared: each
/// owner keeps it behind its own lock.
pub struct TtlMap<K, V> {
    entries: HashMap<K, TtlEntry<V>>,
    capacity: usize,
    ttl: i64,
    tick: u64,
    time: TimeSource,
}

impl<K: Eq + Hash + Clone, V: Clone> TtlMap<K, V> {
    /// Aged by the tokio clock.
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self::with_time(capacity, ttl, TimeSource::Tokio(Instant::now()))
    }

    /// Aged by `clock`, like an Effect `Cache` under the reactor's `Clock`.
    pub fn with_clock(capacity: usize, ttl: Duration, clock: Arc<dyn ReactorClock>) -> Self {
        Self::with_time(capacity, ttl, TimeSource::Clock(clock))
    }

    fn with_time(capacity: usize, ttl: Duration, time: TimeSource) -> Self {
        Self {
            entries: HashMap::new(),
            capacity: capacity.max(1),
            ttl: i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX),
            tick: 0,
            time,
        }
    }

    /// `Cache.getOption`.
    pub fn get(&mut self, key: &K) -> Option<V> {
        let now = self.time.now_millis();
        match self.entries.get(key) {
            Some(entry) if entry.expires_at > now => Some(entry.value.clone()),
            Some(_) => {
                self.entries.remove(key);
                None
            }
            None => None,
        }
    }

    pub fn contains(&mut self, key: &K) -> bool {
        self.get(key).is_some()
    }

    /// `Cache.set`.
    pub fn set(&mut self, key: K, value: V) {
        self.tick += 1;
        let expires_at = self.time.now_millis().saturating_add(self.ttl);
        if !self.entries.contains_key(&key) && self.entries.len() >= self.capacity {
            self.evict();
        }
        let order = self.entries.get(&key).map(|entry| entry.order).unwrap_or(self.tick);
        self.entries.insert(key, TtlEntry { value, expires_at, order });
    }

    /// `Cache.invalidate`.
    pub fn invalidate(&mut self, key: &K) {
        self.entries.remove(key);
    }

    /// `Cache.keys`: live keys, oldest first.
    pub fn keys(&mut self) -> Vec<K> {
        let now = self.time.now_millis();
        self.entries.retain(|_, entry| entry.expires_at > now);
        let mut keys: Vec<(u64, K)> = self.entries.iter().map(|(key, entry)| (entry.order, key.clone())).collect();
        keys.sort_by_key(|(order, _)| *order);
        keys.into_iter().map(|(_, key)| key).collect()
    }

    fn evict(&mut self) {
        let now = self.time.now_millis();
        self.entries.retain(|_, entry| entry.expires_at > now);
        if self.entries.len() < self.capacity {
            return;
        }
        if let Some(oldest) = self.entries.iter().min_by_key(|(_, entry)| entry.order).map(|(key, _)| key.clone()) {
            self.entries.remove(&oldest);
        }
    }
}

/// A shared [`TtlMap`] (the command reactor's `handledTurnStartKeys`).
pub struct SharedTtlMap<K, V>(Mutex<TtlMap<K, V>>);

impl<K: Eq + Hash + Clone, V: Clone> SharedTtlMap<K, V> {
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self(Mutex::new(TtlMap::new(capacity, ttl)))
    }

    pub fn with_clock(capacity: usize, ttl: Duration, clock: Arc<dyn ReactorClock>) -> Self {
        Self(Mutex::new(TtlMap::with_clock(capacity, ttl, clock)))
    }

    pub fn with<R>(&self, f: impl FnOnce(&mut TtlMap<K, V>) -> R) -> R {
        f(&mut self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn worker_drains_in_order() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let processed = seen.clone();
        let worker = DrainableWorker::start(CancellationToken::new(), move |item: u32| {
            let processed = processed.clone();
            async move {
                tokio::task::yield_now().await;
                processed.lock().unwrap().push(item);
            }
        });
        for item in 0..20 {
            worker.enqueue(item);
        }
        worker.drain().await;
        assert_eq!(*seen.lock().unwrap(), (0..20).collect::<Vec<_>>());
        assert_eq!(worker.outstanding(), 0);
    }

    #[tokio::test]
    async fn drain_without_items_returns() {
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let worker = DrainableWorker::start(CancellationToken::new(), move |_: ()| {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });
        worker.drain().await;
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn ttl_map_expires_and_evicts() {
        let mut map = TtlMap::new(2, Duration::from_secs(60));
        map.set("a", 1);
        map.set("b", 2);
        map.set("c", 3);
        assert_eq!(map.get(&"a"), None);
        assert_eq!(map.keys(), vec!["b", "c"]);
        tokio::time::advance(Duration::from_secs(61)).await;
        assert_eq!(map.get(&"b"), None);
        assert!(map.keys().is_empty());
    }

    #[test]
    fn ttl_map_ages_on_its_reactor_clock() {
        let clock = Arc::new(ManualClock::fixed(1_000));
        let mut map = TtlMap::with_clock(10, Duration::from_secs(60), clock.clone());
        map.set("a", 1);
        clock.advance(59_999);
        assert_eq!(map.get(&"a"), Some(1));
        clock.advance(1);
        assert_eq!(map.get(&"a"), None);
    }
}
