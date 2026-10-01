//! Invalidation: the epochs cache keys carry, `invalidate`, `refreshAfterTurn`, and the
//! invalidate-then-notify wrapper every mutation goes through.
//!
//! A key carries its scope's epoch, so bumping the epoch strands every entry made under the old
//! one: no enumerating a cache whose keys (cursors, commits) nothing holds a list of. The counter
//! is shared and monotonic, so a scope re-entering the map after eviction can never mint a key an
//! old entry still has.

use std::future::Future;

use zc_contracts::{ProjectId, PullRequestDiffStat, PullRequestInvalidateInput, PullRequestRef};

use super::refs::{ref_scope, CredRef, RefKey};
use super::{PullRequestService, REF_EPOCH_CAPACITY};
use crate::error::PullRequestError;
use crate::util::OrderedMap;

/// The epochs and the counts held between pages (`recentStats`).
#[derive(Debug, Default)]
pub(crate) struct Epochs {
    pub counter: u64,
    /// Bumped by every refresh that touches listings (whole-workspace invalidation, mutations,
    /// turns).
    pub listings: u64,
    pub ref_epochs: OrderedMap<String, u64>,
    pub project_epochs: OrderedMap<String, u64>,
    /// The epoch of the newest project evicted from `project_epochs`, which every project
    /// without its own entry now counts from.
    pub project_floor: u64,
    /// A press forgets the reader's ticks and nothing else.
    pub files_viewed: OrderedMap<String, u64>,
    /// Bumped by a whole-workspace refresh, the one drop no single reference's epoch covers.
    pub every_file_revision: u64,
    pub recent_stats: OrderedMap<(u64, RefKey), (i64, PullRequestDiffStat)>,
}

impl Epochs {
    pub(crate) fn next(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    /// `refEpoch(ref)`.
    pub(crate) fn ref_epoch(&self, reference: &PullRequestRef) -> u64 {
        let project = self.project_epochs.get(reference.project_id.as_str()).copied().unwrap_or(self.project_floor);
        project.max(self.ref_epochs.get(&ref_scope(reference)).copied().unwrap_or(0))
    }

    fn bump(counter: &mut u64, epochs: &mut OrderedMap<String, u64>, reference: &PullRequestRef) {
        let scope = ref_scope(reference);
        if !epochs.contains_key(&scope) && epochs.len() >= REF_EPOCH_CAPACITY {
            epochs.pop_first();
        }
        *counter += 1;
        epochs.insert(scope, *counter);
    }

    pub(crate) fn bump_ref(&mut self, reference: &PullRequestRef) {
        Self::bump(&mut self.counter, &mut self.ref_epochs, reference);
    }

    pub(crate) fn files_viewed_epoch(&self, reference: &PullRequestRef) -> u64 {
        self.files_viewed.get(&ref_scope(reference)).copied().unwrap_or(0)
    }

    pub(crate) fn bump_files_viewed(&mut self, reference: &PullRequestRef) {
        Self::bump(&mut self.counter, &mut self.files_viewed, reference);
    }
}

impl PullRequestService {
    pub(crate) fn with_epochs<T>(&self, f: impl FnOnce(&mut Epochs) -> T) -> T {
        let mut epochs = self.inner.epochs.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        f(&mut epochs)
    }

    /// `refEpoch(ref)`.
    pub(crate) fn ref_epoch(&self, reference: &PullRequestRef) -> u64 {
        self.with_epochs(|epochs| epochs.ref_epoch(reference))
    }

    /// `refCacheKey(ref)`.
    pub(crate) fn ref_key(&self, reference: &CredRef) -> RefKey {
        RefKey::new(self.ref_epoch(&reference.reference), reference)
    }

    pub(crate) fn file_revisions_epoch(&self) -> u64 {
        self.with_epochs(|epochs| epochs.every_file_revision)
    }

    /// Sets the refresh revision every subscribed reader re-reads on.
    pub(crate) fn notify_readers(&self, revision: u64) {
        self.inner.refreshes.publish(revision);
    }

    pub(crate) async fn invalidate_impl(&self, input: PullRequestInvalidateInput, notify_readers: bool) {
        let reference = input.reference;
        if input.files_viewed_only == Some(true) {
            match reference {
                None => self.inner.files_viewed_cache.invalidate_all(),
                Some(reference) => {
                    if let Ok(canonical) = self.canonical_ref(&reference).await {
                        self.with_epochs(|epochs| epochs.bump_files_viewed(&canonical));
                    }
                }
            }
            return;
        }
        match reference {
            Some(reference) => {
                if let Ok(canonical) = self.canonical_ref(&reference).await {
                    self.inner.read_cache.invalidate(&ref_scope(&canonical)).await;
                    self.with_epochs(|epochs| epochs.bump_ref(&canonical));
                }
            }
            None => {
                self.with_epochs(|epochs| {
                    epochs.listings = epochs.next();
                    epochs.every_file_revision = epochs.next();
                });
                self.inner.viewers_by_host.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clear();
                self.inner.viewer_flights.invalidate_all();
            }
        }
        if notify_readers {
            let revision = self.with_epochs(Epochs::next);
            self.notify_readers(revision);
        }
    }

    pub(crate) async fn refresh_after_turn_impl(&self, project_id: &ProjectId) {
        let listings = self.with_epochs(|epochs| {
            epochs.listings = epochs.next();
            epochs.project_epochs.remove(project_id.as_str());
            if epochs.project_epochs.len() >= REF_EPOCH_CAPACITY {
                if let Some((_, oldest)) = epochs.project_epochs.pop_first() {
                    epochs.project_floor = oldest;
                }
            }
            let listings = epochs.listings;
            epochs.project_epochs.insert(project_id.as_str().to_owned(), listings);
            listings
        });
        self.inner.read_cache.invalidate(&format!("project:{}", project_id.as_str())).await;
        self.notify_readers(listings);
    }

    /// `invalidatedByMutation(method)(input)`: the reference's persisted reads are dropped before
    /// and after the write, its epoch and the listings' are bumped once it succeeded, and every
    /// reader is told to re-read.
    pub(crate) async fn invalidated_by_mutation<F>(&self, input: &PullRequestRef, mutation: F) -> Result<(), PullRequestError>
    where
        F: Future<Output = Result<(), PullRequestError>>,
    {
        let canonical = self.canonical_ref(input).await?;
        let scope = ref_scope(&canonical);
        self.inner.read_cache.invalidate(&scope).await;
        let guard = InvalidateOnDrop::new(self, scope.clone());
        let result = mutation.await;
        guard.disarm();
        self.inner.read_cache.invalidate(&scope).await;
        result?;
        let listings = self.with_epochs(|epochs| {
            epochs.bump_ref(&canonical);
            epochs.listings = epochs.next();
            epochs.listings
        });
        self.notify_readers(listings);
        Ok(())
    }
}

/// `Effect.ensuring(readCache.invalidate(scope))` for a caller that goes away mid-write: the
/// invalidation still runs, on its own task.
pub(crate) struct InvalidateOnDrop {
    service: Option<PullRequestService>,
    scope: String,
}

impl InvalidateOnDrop {
    pub(crate) fn new(service: &PullRequestService, scope: String) -> Self {
        Self {
            service: Some(service.clone()),
            scope,
        }
    }

    pub(crate) fn disarm(mut self) {
        self.service = None;
    }
}

impl Drop for InvalidateOnDrop {
    fn drop(&mut self) {
        if let Some(service) = self.service.take() {
            let scope = std::mem::take(&mut self.scope);
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move { service.inner.read_cache.invalidate(&scope).await });
            }
        }
    }
}
