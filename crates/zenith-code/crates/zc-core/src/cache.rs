//! A small TTL cache with capacity and single-flight loading (the Rust stand-in for Effect
//! `Cache.make({ capacity, timeToLive, lookup })` as used across the server: repository
//! identity, git status results, command resolution, favicons, …).
//!
//! - Entries expire `ttl` after they were stored (monotonic clock, so wall-clock changes cannot
//!   keep them alive).
//! - When full, expired entries go first, then the least recently used one.
//! - [`TtlCache::get_or_try_insert_with`] coalesces concurrent loads of the same key: one lookup
//!   runs, every waiter gets its result. Failures are returned to all waiters but not stored.
//!
//! Heavier needs (per-entry TTL by outcome, weighers) should use `moka` directly (plan §8.1).

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt, Shared};
use tokio::time::Instant;

struct Entry<V> {
    value: V,
    expires_at: Instant,
    last_used: u64,
}

type Inflight<V, E> = Shared<BoxFuture<'static, Result<V, E>>>;

struct State<K, V, E> {
    entries: HashMap<K, Entry<V>>,
    inflight: HashMap<K, Inflight<V, E>>,
    tick: u64,
}

/// A bounded TTL cache. `E` is the loader error type (use `()` when unused).
pub struct TtlCache<K, V, E = ()> {
    state: Arc<Mutex<State<K, V, E>>>,
    capacity: usize,
    ttl: Duration,
}

impl<K, V, E> Clone for TtlCache<K, V, E> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            capacity: self.capacity,
            ttl: self.ttl,
        }
    }
}

impl<K, V, E> TtlCache<K, V, E>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    pub fn new(capacity: usize, ttl: Duration) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                entries: HashMap::new(),
                inflight: HashMap::new(),
                tick: 0,
            })),
            capacity: capacity.max(1),
            ttl,
        }
    }

    /// A fresh value for `key`, if any.
    pub fn get(&self, key: &K) -> Option<V> {
        let mut state = self.lock();
        let now = Instant::now();
        state.tick += 1;
        let tick = state.tick;
        match state.entries.get_mut(key) {
            Some(entry) if entry.expires_at > now => {
                entry.last_used = tick;
                Some(entry.value.clone())
            }
            Some(_) => {
                state.entries.remove(key);
                None
            }
            None => None,
        }
    }

    /// Store `value` for `key` with the cache's TTL.
    pub fn insert(&self, key: K, value: V) {
        let ttl = self.ttl;
        self.insert_with_ttl(key, value, ttl);
    }

    /// Store `value` for `key` with an explicit TTL.
    pub fn insert_with_ttl(&self, key: K, value: V, ttl: Duration) {
        let mut state = self.lock();
        let now = Instant::now();
        if !state.entries.contains_key(&key) && state.entries.len() >= self.capacity {
            state.entries.retain(|_, entry| entry.expires_at > now);
            if state.entries.len() >= self.capacity {
                if let Some(oldest) = state.entries.iter().min_by_key(|(_, entry)| entry.last_used).map(|(key, _)| key.clone()) {
                    state.entries.remove(&oldest);
                }
            }
        }
        state.tick += 1;
        let last_used = state.tick;
        state.entries.insert(
            key,
            Entry {
                value,
                expires_at: now + ttl,
                last_used,
            },
        );
    }

    /// Drop one key (an in-flight load for it still completes, but is not stored).
    pub fn invalidate(&self, key: &K) {
        let mut state = self.lock();
        state.entries.remove(key);
        state.inflight.remove(key);
    }

    /// Drop everything.
    pub fn invalidate_all(&self) {
        let mut state = self.lock();
        state.entries.clear();
        state.inflight.clear();
    }

    /// Number of stored (possibly expired) entries.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The cached value, or the result of `load` (shared by every concurrent caller of the same
    /// key). Successful results are stored.
    pub async fn get_or_try_insert_with<F, Fut>(&self, key: K, load: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
    {
        if let Some(value) = self.get(&key) {
            return Ok(value);
        }
        let (shared, leader) = {
            let mut state = self.lock();
            match state.inflight.get(&key) {
                Some(existing) => (existing.clone(), false),
                None => {
                    let shared = load().boxed().shared();
                    state.inflight.insert(key.clone(), shared.clone());
                    (shared, true)
                }
            }
        };
        let result = shared.clone().await;
        if leader {
            let mut state = self.lock();
            // Only clear our own flight (an invalidate may have replaced or removed it).
            let ours = state.inflight.get(&key).is_some_and(|current| current.ptr_eq(&shared));
            if ours {
                state.inflight.remove(&key);
                drop(state);
                if let Ok(value) = &result {
                    self.insert(key, value.clone());
                }
            }
        }
        result
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<K, V, E>> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(start_paused = true)]
    async fn entries_expire_after_the_ttl() {
        let cache: TtlCache<&str, u32> = TtlCache::new(8, Duration::from_secs(30));
        cache.insert("a", 1);
        assert_eq!(cache.get(&"a"), Some(1));
        tokio::time::advance(Duration::from_secs(31)).await;
        assert_eq!(cache.get(&"a"), None);
        assert!(cache.is_empty());
    }

    #[tokio::test]
    async fn evicts_least_recently_used_when_full() {
        let cache: TtlCache<u32, u32> = TtlCache::new(2, Duration::from_secs(60));
        cache.insert(1, 1);
        cache.insert(2, 2);
        assert_eq!(cache.get(&1), Some(1));
        cache.insert(3, 3);
        assert_eq!(cache.get(&2), None);
        assert_eq!(cache.get(&1), Some(1));
        assert_eq!(cache.get(&3), Some(3));
        cache.invalidate(&1);
        assert_eq!(cache.get(&1), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_loads_are_coalesced() {
        let cache: TtlCache<&str, usize, String> = TtlCache::new(8, Duration::from_secs(60));
        let calls = Arc::new(AtomicUsize::new(0));
        let tasks: Vec<_> = (0..32)
            .map(|_| {
                let cache = cache.clone();
                let calls = calls.clone();
                tokio::spawn(async move {
                    cache
                        .get_or_try_insert_with("repo", move || async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            Ok(42)
                        })
                        .await
                })
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap(), Ok(42));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(cache.get(&"repo"), Some(42));
    }

    #[tokio::test]
    async fn failures_are_shared_but_not_stored() {
        let cache: TtlCache<&str, usize, String> = TtlCache::new(8, Duration::from_secs(60));
        let result = cache.get_or_try_insert_with("k", || async { Err::<usize, _>("boom".to_owned()) }).await;
        assert_eq!(result, Err("boom".to_owned()));
        assert_eq!(cache.get(&"k"), None);
        let result = cache.get_or_try_insert_with("k", || async { Ok(1) }).await;
        assert_eq!(result, Ok(1));
    }
}
