//! Effect `Cache.makeWith({capacity, lookup, timeToLive: exit => …})` on the injectable
//! [`Clock`](crate::util::Clock): single-flight lookups, a time-to-live chosen per outcome (zero
//! keeps nothing), least-recently-used eviction past the capacity.
//!
//! zc-vcs's `OutcomeCache` does the same on tokio's monotonic clock; this one follows the wall
//! clock the rate limiters use, so tests move both with one `ManualClock`.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt, Shared};

use crate::util::SharedClock;

type Inflight<V, E> = Shared<BoxFuture<'static, Result<V, E>>>;
type TtlFn<V, E> = Arc<dyn Fn(&Result<V, E>) -> i64 + Send + Sync>;

struct Entry<V, E> {
    value: Result<V, E>,
    expires_at: i64,
    last_used: u64,
}

struct State<K, V, E> {
    entries: HashMap<K, Entry<V, E>>,
    inflight: HashMap<K, (u64, Inflight<V, E>)>,
    tick: u64,
    flight: u64,
}

pub struct ClockedCache<K, V, E> {
    state: Arc<Mutex<State<K, V, E>>>,
    capacity: usize,
    ttl_ms: TtlFn<V, E>,
    clock: SharedClock,
}

impl<K, V, E> Clone for ClockedCache<K, V, E> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            capacity: self.capacity,
            ttl_ms: self.ttl_ms.clone(),
            clock: self.clock.clone(),
        }
    }
}

impl<K, V, E> ClockedCache<K, V, E>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    pub fn new(capacity: usize, clock: SharedClock, ttl_ms: impl Fn(&Result<V, E>) -> i64 + Send + Sync + 'static) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                entries: HashMap::new(),
                inflight: HashMap::new(),
                tick: 0,
                flight: 0,
            })),
            capacity: capacity.max(1),
            ttl_ms: Arc::new(ttl_ms),
            clock,
        }
    }

    /// `Cache.get(cache, key)` with `lookup` as the loader.
    pub async fn get<F, Fut>(&self, key: K, lookup: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
    {
        let (id, shared) = {
            let mut state = self.state.lock().expect("cache lock");
            state.tick += 1;
            let tick = state.tick;
            let now = self.clock.now_millis();
            match state.entries.get_mut(&key) {
                Some(entry) if entry.expires_at > now => {
                    entry.last_used = tick;
                    return entry.value.clone();
                }
                Some(_) => {
                    state.entries.remove(&key);
                }
                None => {}
            }
            if let Some((id, shared)) = state.inflight.get(&key) {
                (*id, shared.clone())
            } else {
                state.flight += 1;
                let id = state.flight;
                let shared = lookup().boxed().shared();
                state.inflight.insert(key.clone(), (id, shared.clone()));
                (id, shared)
            }
        };
        let result = shared.await;
        let mut state = self.state.lock().expect("cache lock");
        if state.inflight.get(&key).is_some_and(|(current, _)| *current == id) {
            state.inflight.remove(&key);
            let ttl = (self.ttl_ms)(&result);
            if ttl > 0 {
                state.tick += 1;
                let tick = state.tick;
                state.entries.insert(
                    key,
                    Entry {
                        value: result.clone(),
                        expires_at: self.clock.now_millis().saturating_add(ttl),
                        last_used: tick,
                    },
                );
                while state.entries.len() > self.capacity {
                    let oldest = state.entries.iter().min_by_key(|(_, entry)| entry.last_used).map(|(key, _)| key.clone());
                    match oldest {
                        Some(oldest) => {
                            state.entries.remove(&oldest);
                        }
                        None => break,
                    }
                }
            }
        }
        result
    }

    /// `Cache.invalidate`.
    pub fn invalidate(&self, key: &K) {
        let mut state = self.state.lock().expect("cache lock");
        state.entries.remove(key);
        state.inflight.remove(key);
    }
}
