//! `PullRequestReadCache.test.ts`: the persisted, revisioned read cache, on disk (the Effect
//! `KeyValueStore.layerFileSystem` layout) and in memory.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt};
use zc_pullrequest::read_cache::{KeyValueStore, KeyValueStoreError, MemoryKeyValueStore};
use zc_pullrequest::{PullRequestError, PullRequestReadCache};
use zc_sourcecontrol::util::{ManualClock, SharedClock};

type Lookup = BoxFuture<'static, Result<String, PullRequestError>>;

fn answer(value: &str) -> Lookup {
    let value = value.to_owned();
    async move { Ok(value) }.boxed()
}

fn die(message: &'static str) -> Lookup {
    async move { panic!("{message}") }.boxed()
}

/// `Effect.sync(() => String(++reads))`, run afresh each time it is handed over.
fn counting(reads: &Arc<AtomicUsize>) -> Lookup {
    let reads = reads.clone();
    async move { Ok((reads.fetch_add(1, Ordering::SeqCst) + 1).to_string()) }.boxed()
}

fn scopes(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn files_in(directory: &Path) -> usize {
    std::fs::read_dir(directory).unwrap().count()
}

/// A `Deferred<void>`.
#[derive(Clone, Default)]
struct Gate(Arc<tokio::sync::Notify>, Arc<AtomicBool>);

impl Gate {
    fn open(&self) {
        self.1.store(true, Ordering::SeqCst);
        self.0.notify_waiters();
    }

    async fn wait(&self) {
        loop {
            let notified = self.0.notified();
            if self.1.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

async fn settle() {
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

fn clock() -> (Arc<ManualClock>, SharedClock) {
    let clock = ManualClock::new(0);
    let shared: SharedClock = clock.clone();
    (clock, shared)
}

#[tokio::test]
async fn reuses_files_after_restart_and_respects_the_original_expiry() {
    let directory = tempfile::tempdir().unwrap();
    let (manual, clock) = clock();
    let reads = Arc::new(AtomicUsize::new(0));
    let first = PullRequestReadCache::open_with_clock(directory.path(), clock.clone());
    let key = "long/repository/key".repeat(100);

    assert_eq!(first.get(&key, counting(&reads), &[]).await.unwrap(), "1");
    manual.advance(59_000);
    let restarted = PullRequestReadCache::open_with_clock(directory.path(), clock);
    assert_eq!(restarted.get(&key, counting(&reads), &[]).await.unwrap(), "1");
    manual.advance(1_000);
    assert_eq!(restarted.get(&key, counting(&reads), &[]).await.unwrap(), "2");
    assert_eq!(reads.load(Ordering::SeqCst), 2);
    assert_eq!(files_in(directory.path()), 1);
}

#[tokio::test]
async fn clears_in_flight_reads_before_a_new_service_can_reuse_them() {
    let directory = tempfile::tempdir().unwrap();
    let (_, clock) = clock();
    let started = Gate::default();
    let release = Gate::default();
    let cache = PullRequestReadCache::open_with_clock(directory.path(), clock.clone());
    let read = tokio::spawn({
        let (cache, started, release) = (cache.clone(), started.clone(), release.clone());
        async move {
            let lookup = async move {
                started.open();
                release.wait().await;
                Ok("old".to_owned())
            }
            .boxed();
            cache.get("summary", lookup, &scopes(&["pr"])).await
        }
    });
    started.wait().await;
    let invalidate = tokio::spawn({
        let cache = cache.clone();
        async move { cache.invalidate("pr").await }
    });
    settle().await;
    release.open();
    read.await.unwrap().unwrap();
    invalidate.await.unwrap();
    let restarted = PullRequestReadCache::open_with_clock(directory.path(), clock);
    assert_eq!(restarted.get("summary", answer("new"), &scopes(&["pr"])).await.unwrap(), "new");
}

#[tokio::test]
async fn invalidates_only_the_changed_scope_across_restarts_and_coalesces_its_next_reads() {
    let directory = tempfile::tempdir().unwrap();
    let (_, clock) = clock();
    let reads = Arc::new(AtomicUsize::new(0));
    let cache = PullRequestReadCache::open_with_clock(directory.path(), clock.clone());
    cache.get("first", counting(&reads), &scopes(&["project", "pr-1"])).await.unwrap();
    cache.get("second", counting(&reads), &scopes(&["project", "pr-2"])).await.unwrap();
    cache.get("third", counting(&reads), &scopes(&["other-project", "pr-3"])).await.unwrap();
    cache.invalidate("pr-1").await;
    let restarted = PullRequestReadCache::open_with_clock(directory.path(), clock.clone());
    let first_scopes = scopes(&["project", "pr-1"]);
    let answers = futures::future::join_all((0..10).map(|_| restarted.get("first", counting(&reads), &first_scopes))).await;
    assert_eq!(answers.into_iter().map(Result::unwrap).collect::<Vec<_>>(), vec!["4".to_owned(); 10]);
    assert_eq!(restarted.get("second", counting(&reads), &scopes(&["project", "pr-2"])).await.unwrap(), "2");
    restarted.invalidate("project").await;
    let again = PullRequestReadCache::open_with_clock(directory.path(), clock);
    assert_eq!(again.get("second", counting(&reads), &scopes(&["project", "pr-2"])).await.unwrap(), "5");
    assert_eq!(again.get("third", counting(&reads), &scopes(&["other-project", "pr-3"])).await.unwrap(), "3");
    assert_eq!(reads.load(Ordering::SeqCst), 5);
    let files = files_in(directory.path());
    for _ in 0..3 {
        again.invalidate("pr-1").await;
        again.get("first", counting(&reads), &first_scopes).await.unwrap();
    }
    assert_eq!(files_in(directory.path()), files);
}

#[tokio::test]
async fn shares_a_pending_refresh_without_blocking_an_unrelated_cached_pr() {
    let (_, clock) = clock();
    let cache = PullRequestReadCache::memory_with_clock(clock);
    cache.get("first", answer("old"), &scopes(&["pr-1"])).await.unwrap();
    cache.get("second", answer("warm"), &scopes(&["pr-2"])).await.unwrap();
    cache.invalidate("pr-1").await;
    let started = Gate::default();
    let release = Gate::default();
    let reads = Arc::new(AtomicUsize::new(0));
    let refresh = || {
        let (started, release, reads) = (started.clone(), release.clone(), reads.clone());
        async move {
            reads.fetch_add(1, Ordering::SeqCst);
            started.open();
            release.wait().await;
            Ok("fresh".to_owned())
        }
        .boxed()
    };
    let lookups: Vec<Lookup> = (0..10).map(|_| refresh()).collect();
    let pending = tokio::spawn({
        let cache = cache.clone();
        async move {
            let first_scopes = scopes(&["pr-1"]);
            futures::future::join_all(lookups.into_iter().map(|lookup| cache.get("first", lookup, &first_scopes))).await
        }
    });
    started.wait().await;
    assert_eq!(cache.get("second", die("cache miss"), &scopes(&["pr-2"])).await.unwrap(), "warm");
    release.open();
    let answers = pending.await.unwrap();
    assert_eq!(answers.into_iter().map(Result::unwrap).collect::<Vec<_>>(), vec!["fresh".to_owned(); 10]);
    assert_eq!(reads.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn compacts_expired_scope_records_without_discarding_fresh_pr_data() {
    let directory = tempfile::tempdir().unwrap();
    let (manual, clock) = clock();
    let cache = PullRequestReadCache::open_with_clock(directory.path(), clock.clone());
    cache.get("summary", answer("old"), &scopes(&["pr"])).await.unwrap();
    cache.invalidate("pr").await;
    for index in 0..100 {
        cache.invalidate(&format!("pr-{index}")).await;
    }
    assert_eq!(files_in(directory.path()), 2);
    let revisions = directory.path().join("revisions");
    let before = std::fs::metadata(&revisions).unwrap().len();
    manual.advance(59_000);
    assert_eq!(cache.get("summary", answer("fresh"), &scopes(&["pr"])).await.unwrap(), "fresh");
    manual.advance(1_000);
    cache.invalidate("other-pr").await;
    assert!(std::fs::metadata(&revisions).unwrap().len() < before);
    let restarted = PullRequestReadCache::open_with_clock(directory.path(), clock);
    assert_eq!(restarted.get("summary", die("cache miss"), &scopes(&["pr"])).await.unwrap(), "fresh");
}

#[tokio::test]
async fn does_not_persist_failed_github_reads() {
    let directory = tempfile::tempdir().unwrap();
    let (_, clock) = clock();
    let cache = PullRequestReadCache::open_with_clock(directory.path(), clock.clone());
    let failure = async { Err(PullRequestError::operation("summary", "unavailable")) }.boxed();
    cache.get("summary", failure, &[]).await.unwrap_err();
    let restarted = PullRequestReadCache::open_with_clock(directory.path(), clock);
    assert_eq!(restarted.get("summary", answer("recovered"), &[]).await.unwrap(), "recovered");
}

/// A store whose first `get` fails.
struct FlakyGet {
    backing: MemoryKeyValueStore,
    fail: AtomicBool,
}

#[async_trait]
impl KeyValueStore for FlakyGet {
    async fn get(&self, key: &str) -> Result<Option<String>, KeyValueStoreError> {
        if self.fail.swap(false, Ordering::SeqCst) {
            return Err(KeyValueStoreError::new("get", None, "unavailable"));
        }
        self.backing.get(key).await
    }
    async fn set(&self, key: &str, value: &str) -> Result<(), KeyValueStoreError> {
        self.backing.set(key, value).await
    }
    async fn remove(&self, key: &str) -> Result<(), KeyValueStoreError> {
        self.backing.remove(key).await
    }
}

#[tokio::test]
async fn resumes_caching_after_a_failed_scope_read() {
    let (_, clock) = clock();
    let reads = Arc::new(AtomicUsize::new(0));
    let cache = PullRequestReadCache::with_store(
        Arc::new(FlakyGet {
            backing: MemoryKeyValueStore::new(),
            fail: AtomicBool::new(true),
        }),
        clock,
    );
    let pr = scopes(&["pr"]);
    assert_eq!(cache.get("summary", counting(&reads), &pr).await.unwrap(), "1");
    assert_eq!(cache.get("summary", counting(&reads), &pr).await.unwrap(), "2");
    assert_eq!(cache.get("summary", counting(&reads), &pr).await.unwrap(), "2");
}

#[tokio::test]
async fn cancels_abandoned_reads_without_blocking_invalidation() {
    let (_, clock) = clock();
    let cache = PullRequestReadCache::memory_with_clock(clock);
    let started = Gate::default();
    let read = tokio::spawn({
        let (cache, started) = (cache.clone(), started.clone());
        async move {
            let lookup = async move {
                started.open();
                futures::future::pending::<()>().await;
                Ok(String::new())
            }
            .boxed();
            cache.get("summary", lookup, &scopes(&["pr"])).await
        }
    });
    started.wait().await;
    read.abort();
    assert!(read.await.unwrap_err().is_cancelled());
    cache.invalidate("pr").await;
    assert_eq!(cache.get("summary", answer("fresh"), &scopes(&["pr"])).await.unwrap(), "fresh");
}

/// A store whose writes of the revisions wait for the test after writing.
struct HeldRevisions {
    backing: MemoryKeyValueStore,
    written: Gate,
    release: Gate,
}

#[async_trait]
impl KeyValueStore for HeldRevisions {
    async fn get(&self, key: &str) -> Result<Option<String>, KeyValueStoreError> {
        self.backing.get(key).await
    }
    async fn set(&self, key: &str, value: &str) -> Result<(), KeyValueStoreError> {
        self.backing.set(key, value).await?;
        if key == "revisions" {
            self.written.open();
            self.release.wait().await;
        }
        Ok(())
    }
    async fn remove(&self, key: &str) -> Result<(), KeyValueStoreError> {
        self.backing.remove(key).await
    }
}

#[tokio::test]
async fn finishes_the_in_memory_revision_update_when_invalidation_is_canceled_after_writing() {
    let (_, clock) = clock();
    let written = Gate::default();
    let release = Gate::default();
    let cache = PullRequestReadCache::with_store(
        Arc::new(HeldRevisions {
            backing: MemoryKeyValueStore::new(),
            written: written.clone(),
            release: release.clone(),
        }),
        clock,
    );
    cache.get("summary", answer("old"), &scopes(&["pr"])).await.unwrap();
    let invalidation = tokio::spawn({
        let cache = cache.clone();
        async move { cache.invalidate("pr").await }
    });
    written.wait().await;
    invalidation.abort();
    release.open();
    let _ = invalidation.await;
    assert_eq!(cache.get("summary", answer("fresh"), &scopes(&["pr"])).await.unwrap(), "fresh");
}
