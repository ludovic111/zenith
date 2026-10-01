//! Effect `Cache.makeWith({ capacity, lookup, timeToLive: (exit, key) => … })` semantics:
//! single-flight lookups, a time-to-live chosen per outcome (success *or* failure; zero means
//! "do not keep"), LRU eviction past the capacity, and invalidation that also detaches an
//! in-flight lookup (its waiters still get the result, but it is not stored).

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::time::Instant;

type Inflight<V, E> = Shared<BoxFuture<'static, Result<V, E>>>;
type TtlFn<K, V, E> = Arc<dyn Fn(&Result<V, E>, &K) -> Duration + Send + Sync>;

struct Entry<V, E> {
    value: Result<V, E>,
    expires_at: Instant,
    last_used: u64,
}

struct State<K, V, E> {
    entries: HashMap<K, Entry<V, E>>,
    inflight: HashMap<K, (u64, Inflight<V, E>)>,
    tick: u64,
    flight_id: u64,
}

pub struct OutcomeCache<K, V, E> {
    state: Arc<Mutex<State<K, V, E>>>,
    capacity: usize,
    ttl: TtlFn<K, V, E>,
}

impl<K, V, E> Clone for OutcomeCache<K, V, E> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            capacity: self.capacity,
            ttl: self.ttl.clone(),
        }
    }
}

impl<K, V, E> OutcomeCache<K, V, E>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    pub fn new(capacity: usize, ttl: impl Fn(&Result<V, E>, &K) -> Duration + Send + Sync + 'static) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                entries: HashMap::new(),
                inflight: HashMap::new(),
                tick: 0,
                flight_id: 0,
            })),
            capacity,
            ttl: Arc::new(ttl),
        }
    }

    /// The cached outcome, or the shared result of `load`.
    pub async fn get<F, Fut>(&self, key: K, load: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
    {
        let (flight, id) = {
            let mut state = self.lock();
            state.tick += 1;
            let tick = state.tick;
            let now = Instant::now();
            if let Some(entry) = state.entries.get_mut(&key) {
                if entry.expires_at > now {
                    entry.last_used = tick;
                    return entry.value.clone();
                }
                state.entries.remove(&key);
            }
            match state.inflight.get(&key) {
                Some((id, flight)) => (flight.clone(), *id),
                None => {
                    state.flight_id += 1;
                    let id = state.flight_id;
                    let flight = load().boxed().shared();
                    state.inflight.insert(key.clone(), (id, flight.clone()));
                    (flight, id)
                }
            }
        };
        let result = flight.await;
        // Whichever waiter finishes first stores the outcome (the caller that started the
        // lookup may have been cancelled).
        {
            let ttl = (self.ttl)(&result, &key);
            let mut state = self.lock();
            let ours = state.inflight.get(&key).is_some_and(|(current, _)| *current == id);
            if ours {
                state.inflight.remove(&key);
                if !ttl.is_zero() {
                    state.tick += 1;
                    let tick = state.tick;
                    state.entries.insert(
                        key,
                        Entry {
                            value: result.clone(),
                            expires_at: Instant::now() + ttl,
                            last_used: tick,
                        },
                    );
                    Self::evict(&mut state, self.capacity);
                }
            }
        }
        result
    }

    /// Drop the entry (and detach an in-flight lookup) for `key`.
    pub fn invalidate(&self, key: &K) {
        let mut state = self.lock();
        state.entries.remove(key);
        state.inflight.remove(key);
    }

    pub fn invalidate_all(&self) {
        let mut state = self.lock();
        state.entries.clear();
        state.inflight.clear();
    }

    /// `Cache.getOption`: the stored, unexpired outcome of `key` (no lookup is started).
    pub fn peek(&self, key: &K) -> Option<Result<V, E>> {
        let state = self.lock();
        state
            .entries
            .get(key)
            .filter(|entry| entry.expires_at > Instant::now())
            .map(|entry| entry.value.clone())
    }

    pub fn contains(&self, key: &K) -> bool {
        let state = self.lock();
        state.entries.get(key).is_some_and(|entry| entry.expires_at > Instant::now())
    }

    fn evict(state: &mut State<K, V, E>, capacity: usize) {
        while state.entries.len() > capacity {
            let now = Instant::now();
            let victim = state
                .entries
                .iter()
                .min_by_key(|(_, entry)| (entry.expires_at > now, entry.last_used))
                .map(|(key, _)| key.clone());
            match victim {
                Some(key) => {
                    state.entries.remove(&key);
                }
                None => break,
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<K, V, E>> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// A bounded insertion-ordered map with "delete then set" recency, like the JS `Map`s the TS
/// driver trims to a capacity by dropping `keys().next()`.
pub struct BoundedOrderMap<V> {
    order: std::collections::VecDeque<String>,
    values: HashMap<String, V>,
    capacity: usize,
}

impl<V: Clone> BoundedOrderMap<V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            order: Default::default(),
            values: HashMap::new(),
            capacity,
        }
    }

    pub fn get(&self, key: &str) -> Option<V> {
        self.values.get(key).cloned()
    }

    /// `map.delete(key); map.set(key, value)` then drop the oldest past the capacity.
    pub fn set(&mut self, key: &str, value: V) {
        if self.values.remove(key).is_some() {
            self.order.retain(|existing| existing != key);
        }
        self.values.insert(key.to_owned(), value);
        self.order.push_back(key.to_owned());
        if self.values.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.values.remove(&oldest);
            }
        }
    }

    pub fn remove(&mut self, key: &str) {
        if self.values.remove(key).is_some() {
            self.order.retain(|existing| existing != key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn caches_by_outcome_and_coalesces() {
        let cache: OutcomeCache<String, u32, String> = OutcomeCache::new(8, |result, _| if result.is_ok() { Duration::from_secs(10) } else { Duration::ZERO });
        let calls = Arc::new(AtomicUsize::new(0));
        let load = |calls: Arc<AtomicUsize>| async move {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(5)).await;
            Ok::<_, String>(7)
        };
        let (a, b) = tokio::join!(cache.get("k".into(), || load(calls.clone())), cache.get("k".into(), || load(calls.clone())));
        assert_eq!((a, b), (Ok(7), Ok(7)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.get("k".into(), || load(calls.clone())).await, Ok(7));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(Duration::from_secs(11)).await;
        cache.get("k".into(), || load(calls.clone())).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        let failures = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let failures = failures.clone();
            let result = cache
                .get("bad".into(), || async move {
                    failures.fetch_add(1, Ordering::SeqCst);
                    Err::<u32, _>("no".to_owned())
                })
                .await;
            assert!(result.is_err());
        }
        assert_eq!(failures.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn bounded_order_map_drops_the_oldest() {
        let mut map = BoundedOrderMap::new(2);
        map.set("a", 1);
        map.set("b", 2);
        map.set("a", 3);
        map.set("c", 4);
        assert_eq!(map.get("b"), None);
        assert_eq!(map.get("a"), Some(3));
        assert_eq!(map.get("c"), Some(4));
    }
}
