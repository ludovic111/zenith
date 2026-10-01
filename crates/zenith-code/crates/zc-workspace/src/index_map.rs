//! `WorkspaceSearchIndexMap` (an Effect `LayerMap` with `idleTimeToLive: "15 minutes"`): one
//! [`WorkspaceSearchIndex`] per `(variant, workspace root)`, built on first use, shared by
//! concurrent callers (one build in flight per key), and destroyed once nobody has used it for
//! 15 minutes.
//!
//! Semantics kept from Effect's `RcMap`:
//! - [`SearchIndexMap::get`] hands out a lease; the idle clock starts when the last lease of a key
//!   is dropped and is cancelled by a new `get`.
//! - A failed build is not cached: the next `get` tries again.
//! - [`SearchIndexMap::invalidate`] forgets the key at once; the index itself is destroyed when
//!   its last lease is dropped.
//! - [`SearchIndexMap::has`] reports keys present (built or being built).

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use tokio::sync::OnceCell;

use crate::backend::{FinderFactory, IndexVariant};
use crate::errors::SearchIndexError;
use crate::search_index::{WorkspaceSearchIndex, WORKSPACE_INDEX_IDLE_TTL};

type Key = (IndexVariant, String);
type Cell = Arc<OnceCell<Arc<WorkspaceSearchIndex>>>;

struct Entry {
    /// Unique per entry, so stale guards and timers never touch a newer entry of the same key.
    id: u64,
    cell: Cell,
    users: usize,
    /// Bumped on every acquisition, so a pending idle eviction can tell it went stale.
    epoch: u64,
}

struct State {
    entries: HashMap<Key, Entry>,
}

struct Shared {
    state: Mutex<State>,
    factory: Arc<dyn FinderFactory>,
    idle_ttl: Duration,
}

/// The per-process map of search indexes.
#[derive(Clone)]
pub struct SearchIndexMap {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for SearchIndexMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<Key> = self.shared.state.lock().unwrap().entries.keys().cloned().collect();
        f.debug_struct("SearchIndexMap").field("keys", &keys).finish()
    }
}

/// A use of an index; derefs to it. Dropping the last lease of a key starts its idle clock.
pub struct IndexLease {
    index: Arc<WorkspaceSearchIndex>,
    _user: UserGuard,
}

impl Deref for IndexLease {
    type Target = WorkspaceSearchIndex;
    fn deref(&self) -> &WorkspaceSearchIndex {
        &self.index
    }
}

/// Counts one user of an entry; on drop, releases it and schedules the idle eviction.
struct UserGuard {
    shared: Weak<Shared>,
    key: Key,
    id: u64,
}

fn next_entry_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

impl Drop for UserGuard {
    fn drop(&mut self) {
        let Some(shared) = self.shared.upgrade() else { return };
        let epoch = {
            let mut state = shared.state.lock().unwrap();
            let Some(entry) = state.entries.get_mut(&self.key) else { return };
            if entry.id != self.id {
                return;
            }
            entry.users -= 1;
            if entry.users > 0 {
                return;
            }
            if !entry.cell.initialized() {
                // The build failed or was cancelled with nobody else waiting: forget it.
                state.entries.remove(&self.key);
                return;
            }
            entry.epoch
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else { return };
        let weak = self.shared.clone();
        let key = self.key.clone();
        let id = self.id;
        let ttl = shared.idle_ttl;
        runtime.spawn(async move {
            tokio::time::sleep(ttl).await;
            let Some(shared) = weak.upgrade() else { return };
            let evicted = {
                let mut state = shared.state.lock().unwrap();
                match state.entries.get(&key) {
                    Some(entry) if entry.id == id && entry.users == 0 && entry.epoch == epoch => state.entries.remove(&key),
                    _ => None,
                }
            };
            if let Some(entry) = evicted {
                // Destroying an fff instance joins its watcher: keep it off the async workers.
                let _ = tokio::task::spawn_blocking(move || drop(entry)).await;
            }
        });
    }
}

impl SearchIndexMap {
    pub fn new(factory: Arc<dyn FinderFactory>) -> Self {
        Self::with_idle_ttl(factory, WORKSPACE_INDEX_IDLE_TTL)
    }

    pub fn with_idle_ttl(factory: Arc<dyn FinderFactory>, idle_ttl: Duration) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State { entries: HashMap::new() }),
                factory,
                idle_ttl,
            }),
        }
    }

    /// Acquires the index of `(variant, cwd)`, building it if needed.
    pub async fn get(&self, cwd: &str, variant: IndexVariant) -> Result<IndexLease, SearchIndexError> {
        let key: Key = (variant, cwd.to_owned());
        let cell = {
            let mut state = self.shared.state.lock().unwrap();
            let entry = state.entries.entry(key.clone()).or_insert_with(|| Entry {
                id: next_entry_id(),
                cell: Arc::new(OnceCell::new()),
                users: 0,
                epoch: 0,
            });
            entry.users += 1;
            entry.epoch += 1;
            (entry.id, entry.cell.clone())
        };
        let (id, cell) = cell;
        let user = UserGuard {
            shared: Arc::downgrade(&self.shared),
            key: key.clone(),
            id,
        };
        let factory = self.shared.factory.clone();
        let built = cell
            .get_or_try_init(|| async move { WorkspaceSearchIndex::make(factory, cwd, variant).await.map(Arc::new) })
            .await;
        match built {
            Ok(index) => Ok(IndexLease {
                index: index.clone(),
                _user: user,
            }),
            Err(error) => {
                // Not cached: drop the entry unless another caller is still waiting on it (its
                // own `get_or_try_init` retries the build).
                let mut state = self.shared.state.lock().unwrap();
                if let Some(entry) = state.entries.get(&key) {
                    if entry.id == id && !cell.initialized() && entry.users <= 1 {
                        state.entries.remove(&key);
                    }
                }
                drop(state);
                drop(user);
                Err(error)
            }
        }
    }

    /// `RcMap.has`: is the key present (built, being built, or idle)?
    pub fn has(&self, cwd: &str, variant: IndexVariant) -> bool {
        self.shared.state.lock().unwrap().entries.contains_key(&(variant, cwd.to_owned()))
    }

    /// `invalidate(key)`: forgets the key; the index goes away with its last lease.
    pub fn invalidate(&self, cwd: &str, variant: IndexVariant) {
        let removed = self.shared.state.lock().unwrap().entries.remove(&(variant, cwd.to_owned()));
        if let Some(entry) = removed {
            if tokio::runtime::Handle::try_current().is_ok() {
                tokio::task::spawn_blocking(move || drop(entry));
            }
        }
    }

    /// The keys present, for diagnostics and tests.
    pub fn keys(&self) -> Vec<(IndexVariant, String)> {
        let mut keys: Vec<Key> = self.shared.state.lock().unwrap().entries.keys().cloned().collect();
        keys.sort();
        keys
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Finder, FinderError};
    use crate::search_index::tests::FakeFinder;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts creations and destructions.
    struct CountingFactory {
        created: Arc<AtomicUsize>,
        destroyed: Arc<AtomicUsize>,
        fail_next: Mutex<bool>,
    }

    struct Tracked {
        inner: FakeFinder,
        destroyed: Arc<AtomicUsize>,
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.destroyed.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Finder for Tracked {
        fn scan_progress(&self) -> Result<crate::backend::ScanProgress, FinderError> {
            self.inner.scan_progress()
        }
        fn file_search(&self, q: &str, p: usize) -> Result<crate::backend::PathSearchPage, FinderError> {
            self.inner.file_search(q, p)
        }
        fn directory_search(&self, q: &str, p: usize) -> Result<crate::backend::PathSearchPage, FinderError> {
            self.inner.directory_search(q, p)
        }
        fn mixed_search(&self, q: &str, p: usize) -> Result<crate::backend::MixedSearchPage, FinderError> {
            self.inner.mixed_search(q, p)
        }
        fn grep(&self, r: &crate::backend::GrepRequest) -> Result<crate::backend::GrepPage, FinderError> {
            self.inner.grep(r)
        }
        fn scan_files(&self) -> Result<(), FinderError> {
            Ok(())
        }
    }

    impl FinderFactory for CountingFactory {
        fn create(&self, _cwd: &str, _variant: IndexVariant) -> Result<Arc<dyn Finder>, FinderError> {
            if std::mem::take(&mut *self.fail_next.lock().unwrap()) {
                return Err(FinderError::Returned("boom".into()));
            }
            self.created.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new(Tracked {
                inner: FakeFinder::default(),
                destroyed: self.destroyed.clone(),
            }))
        }
    }

    fn counting() -> (Arc<CountingFactory>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let created = Arc::new(AtomicUsize::new(0));
        let destroyed = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(CountingFactory {
                created: created.clone(),
                destroyed: destroyed.clone(),
                fail_next: Mutex::new(false),
            }),
            created,
            destroyed,
        )
    }

    async fn settle() {
        for _ in 0..20 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn reuses_and_evicts_after_fifteen_idle_minutes() {
        let (factory, created, destroyed) = counting();
        let map = SearchIndexMap::new(factory);
        {
            let _a = map.get("/w", IndexVariant::Paths).await.unwrap();
            let _b = map.get("/w", IndexVariant::Paths).await.unwrap();
        }
        assert_eq!(created.load(Ordering::SeqCst), 1);
        // Paths and content are separate resources.
        drop(map.get("/w", IndexVariant::Content).await.unwrap());
        assert_eq!(created.load(Ordering::SeqCst), 2);
        tokio::time::sleep(Duration::from_secs(14 * 60)).await;
        // A use before the deadline restarts the idle clock.
        drop(map.get("/w", IndexVariant::Paths).await.unwrap());
        tokio::time::sleep(Duration::from_secs(60 + 1)).await;
        settle().await;
        assert!(map.has("/w", IndexVariant::Paths));
        assert!(!map.has("/w", IndexVariant::Content));
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
        tokio::time::sleep(WORKSPACE_INDEX_IDLE_TTL).await;
        settle().await;
        assert!(!map.has("/w", IndexVariant::Paths));
        assert_eq!(destroyed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn never_evicts_while_leased() {
        let (factory, _created, destroyed) = counting();
        let map = SearchIndexMap::new(factory);
        let lease = map.get("/w", IndexVariant::Paths).await.unwrap();
        tokio::time::sleep(WORKSPACE_INDEX_IDLE_TTL * 2).await;
        settle().await;
        assert!(map.has("/w", IndexVariant::Paths));
        assert_eq!(destroyed.load(Ordering::SeqCst), 0);
        drop(lease);
    }

    #[tokio::test]
    async fn failed_builds_are_not_cached() {
        let (factory, created, _destroyed) = counting();
        *factory.fail_next.lock().unwrap() = true;
        let map = SearchIndexMap::new(factory);
        assert!(map.get("/w", IndexVariant::Paths).await.is_err());
        assert!(!map.has("/w", IndexVariant::Paths));
        assert!(map.get("/w", IndexVariant::Paths).await.is_ok());
        assert_eq!(created.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn invalidate_rebuilds_on_next_use_and_destroys_after_the_last_lease() {
        let (factory, created, destroyed) = counting();
        let map = SearchIndexMap::new(factory);
        let lease = map.get("/w", IndexVariant::Paths).await.unwrap();
        map.invalidate("/w", IndexVariant::Paths);
        assert!(!map.has("/w", IndexVariant::Paths));
        settle().await;
        assert_eq!(destroyed.load(Ordering::SeqCst), 0, "still leased");
        drop(lease);
        settle().await;
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
        drop(map.get("/w", IndexVariant::Paths).await.unwrap());
        assert_eq!(created.load(Ordering::SeqCst), 2);
    }
}
