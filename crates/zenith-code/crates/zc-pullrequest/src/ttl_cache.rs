//! Effect `Cache.makeWith(lookup, {capacity, timeToLive: (exit, key) => …})` on the injectable
//! clock, with the parts of its API the pull request code uses (`get`, `getSuccess`, `set`,
//! `invalidate`, `invalidateAll`):
//!
//! - concurrent `get`s of one key share one lookup; a lookup every caller has abandoned is
//!   dropped (Effect interrupts it) and the next `get` starts a new one;
//! - the time to live is chosen per outcome and key when the lookup completes; zero (or less)
//!   keeps nothing, [`FOREVER`] never expires; an entry has expired once `now >= expiresAt`;
//! - past the capacity the least recently used entry (pending ones included) is dropped.
//!
//! zc-sourcecontrol's `ClockedCache` lacks `getSuccess`, `set`, `invalidateAll` and a key-aware
//! time to live, hence this one.

use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use futures::future::{BoxFuture, FutureExt, Shared, WeakShared};
use zc_sourcecontrol::util::SharedClock;

use crate::util::OrderedMap;

/// A time to live that never runs out (`Duration.infinity`).
pub(crate) const FOREVER: i64 = i64::MAX;

type Flight<V, E> = Shared<BoxFuture<'static, Result<V, E>>>;
type TtlFn<K, V, E> = Arc<dyn Fn(&K, &Result<V, E>) -> i64 + Send + Sync>;

enum Slot<V, E> {
    Pending {
        id: u64,
        flight: WeakShared<BoxFuture<'static, Result<V, E>>>,
    },
    Ready {
        value: Result<V, E>,
        expires_at: i64,
    },
}

struct State<K, V, E> {
    entries: OrderedMap<K, Slot<V, E>>,
    flights: u64,
}

pub(crate) struct TtlCache<K, V, E> {
    state: Arc<Mutex<State<K, V, E>>>,
    capacity: usize,
    ttl: TtlFn<K, V, E>,
    clock: SharedClock,
}

impl<K, V, E> Clone for TtlCache<K, V, E> {
    fn clone(&self) -> Self {
        Self {
            state: self.state.clone(),
            capacity: self.capacity,
            ttl: self.ttl.clone(),
            clock: self.clock.clone(),
        }
    }
}

impl<K, V, E> TtlCache<K, V, E>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    pub(crate) fn new(capacity: usize, clock: SharedClock, ttl: impl Fn(&K, &Result<V, E>) -> i64 + Send + Sync + 'static) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                entries: OrderedMap::new(),
                flights: 0,
            })),
            capacity: capacity.max(1),
            ttl: Arc::new(ttl),
            clock,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<K, V, E>> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `Cache.get(cache, key)` with `lookup` as the loader for this key.
    pub(crate) async fn get<F, Fut>(&self, key: K, lookup: F) -> Result<V, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V, E>> + Send + 'static,
    {
        let (id, flight) = {
            let now = self.clock.now_millis();
            let mut state = self.lock();
            let mut joined: Option<(u64, Flight<V, E>)> = None;
            let mut stale = false;
            match state.entries.get(&key) {
                Some(Slot::Ready { value, expires_at }) if now < *expires_at => {
                    let value = value.clone();
                    state.entries.touch(&key);
                    return value;
                }
                Some(Slot::Ready { .. }) => stale = true,
                Some(Slot::Pending { id, flight }) => match flight.upgrade() {
                    Some(flight) => joined = Some((*id, flight)),
                    None => stale = true,
                },
                None => {}
            }
            if stale {
                state.entries.remove(&key);
            }
            match joined {
                Some(joined) => {
                    state.entries.touch(&key);
                    joined
                }
                None => {
                    state.flights += 1;
                    let id = state.flights;
                    let flight: Flight<V, E> = lookup().boxed().shared();
                    if let Some(weak) = flight.downgrade() {
                        state.entries.insert_last(key.clone(), Slot::Pending { id, flight: weak });
                        while state.entries.len() > self.capacity {
                            state.entries.pop_first();
                        }
                    }
                    (id, flight)
                }
            }
        };
        let result = flight.await;
        self.settle(&key, id, &result);
        result
    }

    /// Files a finished lookup under its key, if the key still waits on that lookup.
    fn settle(&self, key: &K, id: u64, result: &Result<V, E>) {
        let ttl = (self.ttl)(key, result);
        let now = self.clock.now_millis();
        let mut state = self.lock();
        let current = matches!(state.entries.get(key), Some(Slot::Pending { id: pending, .. }) if *pending == id);
        if !current {
            return;
        }
        if ttl <= 0 {
            state.entries.remove(key);
        } else if let Some(slot) = state.entries.get_mut(key) {
            *slot = Slot::Ready {
                value: result.clone(),
                expires_at: now.saturating_add(ttl),
            };
        }
    }

    /// `Cache.getSuccess`: a held, unexpired success, without starting or waiting on a lookup.
    pub(crate) fn get_success(&self, key: &K) -> Option<V> {
        let now = self.clock.now_millis();
        let mut state = self.lock();
        let (value, expired) = match state.entries.get(key)? {
            Slot::Ready { value, expires_at } if now < *expires_at => (value.as_ref().ok().cloned(), false),
            Slot::Ready { .. } => (None, true),
            Slot::Pending { .. } => (None, false),
        };
        if expired {
            state.entries.remove(key);
        } else {
            state.entries.touch(key);
        }
        value
    }

    /// `Cache.set`: files `value` as if a lookup had answered it.
    pub(crate) fn set(&self, key: K, value: V) {
        let result = Ok(value);
        let ttl = (self.ttl)(&key, &result);
        let now = self.clock.now_millis();
        let mut state = self.lock();
        if ttl <= 0 {
            state.entries.remove(&key);
            return;
        }
        state.entries.insert_last(
            key,
            Slot::Ready {
                value: result,
                expires_at: now.saturating_add(ttl),
            },
        );
        while state.entries.len() > self.capacity {
            state.entries.pop_first();
        }
    }

    /// `Cache.invalidate`.
    pub(crate) fn invalidate(&self, key: &K) {
        self.lock().entries.remove(key);
    }

    /// `Cache.invalidateAll`.
    pub(crate) fn invalidate_all(&self) {
        self.lock().entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use zc_sourcecontrol::util::ManualClock;

    use super::*;

    #[tokio::test]
    async fn shares_lookups_and_expires_by_outcome() {
        let clock = ManualClock::new(0);
        let cache: TtlCache<&'static str, usize, ()> = TtlCache::new(2, clock.clone(), |_, result| if result.is_ok() { 1_000 } else { 0 });
        let calls = Arc::new(AtomicUsize::new(0));
        let lookup = || {
            let calls = calls.clone();
            move || async move {
                tokio::task::yield_now().await;
                Ok(calls.fetch_add(1, Ordering::SeqCst) + 1)
            }
        };
        let (first, second) = tokio::join!(cache.get("a", lookup()), cache.get("a", lookup()));
        assert_eq!((first, second), (Ok(1), Ok(1)));
        assert_eq!(cache.get_success(&"a"), Some(1));
        clock.advance(1_000);
        assert_eq!(cache.get_success(&"a"), None);
        assert_eq!(cache.get("a", lookup()).await, Ok(2));
        cache.invalidate_all();
        assert_eq!(cache.get("a", lookup()).await, Ok(3));
    }

    #[tokio::test]
    async fn starts_over_after_every_caller_abandoned_a_lookup() {
        let clock = ManualClock::new(0);
        let cache: TtlCache<u8, u8, ()> = TtlCache::new(4, clock, |_, _| 1_000);
        let abandoned = cache.get(1, futures::future::pending::<Result<u8, ()>>);
        assert!(futures::poll!(Box::pin(abandoned)).is_pending());
        assert_eq!(cache.get(1, || async { Ok(7) }).await, Ok(7));
    }
}
