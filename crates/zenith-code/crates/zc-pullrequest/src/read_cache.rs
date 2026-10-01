//! `PullRequestReadCache.ts`: the persisted, revisioned read cache behind `summary` and `stack`.
//!
//! Reads are kept for a minute, on disk, so a restarted server answers a page it already read
//! without asking the host again. Each read names the scopes it belongs to (`project:<id>`, a
//! change request's scope); `invalidate(scope)` gives the scope a new revision for a minute, and a
//! held read whose revisions no longer match is read again (once, however many callers ask).
//!
//! The on-disk layout is the TS one (Effect's `KeyValueStore.layerFileSystem` under
//! `PersistedCache` with store id `pr-v2`), so a cache written by either server reads the same in
//! the other:
//! - `<dir>/revisions`: `{"<scope>": {"revision": "<uuid>", "expiresAt": <ms>}, …}`;
//! - `<dir>/pr-v2<sha256 hex of the key>`: `[{"_tag":"Success","value":{"payload": "<json>",
//!   "expiresAt": <ms>, "revision": "<rev>:<rev>…"}}, <expires ms>]`.
//!
//! Failed reads are never kept. A store that cannot be read or written falls back to reading
//! the host; one that cannot record an invalidation disables the cache for good (it could no
//! longer tell a stale read from a fresh one).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt, Shared};
use serde_json::{json, Map, Value};
use tokio::sync::Semaphore;
use zc_sourcecontrol::util::{encode_uri_component, system_clock, SharedClock};

use crate::error::PullRequestError;
use crate::ttl_cache::{TtlCache, FOREVER};
use crate::util::sha256_hex;

const CONCURRENT_READS: u32 = 512;
/// How long a read and a revision are kept.
const ENTRY_TTL_MS: i64 = 60_000;
/// The `PersistedCache` store id: the prefix of every entry's key.
pub const STORE_ID: &str = "pr-v2";
/// The key holding the scope revisions.
pub const REVISIONS_KEY: &str = "revisions";
/// The directory below the provider status cache dir (`ServerConfig.providerStatusCacheDir`).
pub const DIRECTORY_NAME: &str = "pull-requests";

/// `KeyValueStoreError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValueStoreError {
    pub method: String,
    pub key: Option<String>,
    pub message: String,
}

impl KeyValueStoreError {
    pub fn new(method: &str, key: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            method: method.to_owned(),
            key: key.map(str::to_owned),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for KeyValueStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for KeyValueStoreError {}

/// Effect's `KeyValueStore` (string values only).
#[async_trait]
pub trait KeyValueStore: Send + Sync {
    async fn get(&self, key: &str) -> Result<Option<String>, KeyValueStoreError>;
    async fn set(&self, key: &str, value: &str) -> Result<(), KeyValueStoreError>;
    async fn remove(&self, key: &str) -> Result<(), KeyValueStoreError>;
}

/// `KeyValueStore.layerMemory`.
#[derive(Debug, Default)]
pub struct MemoryKeyValueStore {
    entries: Mutex<HashMap<String, String>>,
}

impl MemoryKeyValueStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl KeyValueStore for MemoryKeyValueStore {
    async fn get(&self, key: &str) -> Result<Option<String>, KeyValueStoreError> {
        Ok(self.lock().get(key).cloned())
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), KeyValueStoreError> {
        self.lock().insert(key.to_owned(), value.to_owned());
        Ok(())
    }

    async fn remove(&self, key: &str) -> Result<(), KeyValueStoreError> {
        self.lock().remove(key);
        Ok(())
    }
}

/// `KeyValueStore.layerFileSystem(directory)`: one file per key, named `encodeURIComponent(key)`.
#[derive(Debug, Clone)]
pub struct FileSystemKeyValueStore {
    directory: PathBuf,
}

impl FileSystemKeyValueStore {
    /// Creates `directory` (recursively) when it does not exist.
    pub fn open(directory: impl Into<PathBuf>) -> std::io::Result<Self> {
        let directory = directory.into();
        std::fs::create_dir_all(&directory)?;
        Ok(Self { directory })
    }

    pub fn directory(&self) -> &Path {
        &self.directory
    }

    fn path(&self, method: &str, key: &str) -> Result<PathBuf, KeyValueStoreError> {
        if key.is_empty() || key == "." || key == ".." {
            return Err(KeyValueStoreError::new(method, Some(key), format!("Invalid key {key}")));
        }
        Ok(self.directory.join(encode_uri_component(key)))
    }
}

#[async_trait]
impl KeyValueStore for FileSystemKeyValueStore {
    async fn get(&self, key: &str) -> Result<Option<String>, KeyValueStoreError> {
        let path = self.path("get", key)?;
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(KeyValueStoreError::new("get", Some(key), format!("Unable to get item with key {key}"))),
        }
    }

    async fn set(&self, key: &str, value: &str) -> Result<(), KeyValueStoreError> {
        let path = self.path("set", key)?;
        tokio::fs::write(&path, value)
            .await
            .map_err(|_| KeyValueStoreError::new("set", Some(key), format!("Unable to set item with key {key}")))
    }

    async fn remove(&self, key: &str) -> Result<(), KeyValueStoreError> {
        let path = self.path("remove", key)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(KeyValueStoreError::new("remove", Some(key), format!("Unable to remove item with key {key}"))),
        }
    }
}

/// Why a cached read did not answer: the read itself failed, or the store around it did
/// (`KeyValueStoreError | PersistenceError | SchemaError`), in which case the host is read.
#[derive(Debug, Clone)]
enum Failure {
    Read(PullRequestError),
    Store(String),
}

/// A held read (`{payload, expiresAt, revision?}`).
#[derive(Debug, Clone)]
struct Stored {
    payload: String,
    expires_at: f64,
    revision: Option<String>,
}

/// The in-memory identity of a read: its key's digest and the revision it was asked under.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReadKey {
    digest: String,
    revision: String,
}

impl ReadKey {
    fn primary_key(&self) -> String {
        format!("{STORE_ID}{}", self.digest)
    }

    /// `matchesRevision`: every scope revision this read was asked under is either unset or the
    /// one the held read was stored under.
    fn matches(&self, stored: Option<&str>) -> bool {
        let stored: Vec<&str> = stored.map(|revision| revision.split(':').collect()).unwrap_or_default();
        self.revision
            .split(':')
            .enumerate()
            .all(|(index, value)| value.is_empty() || stored.get(index) == Some(&value))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RevisionEntry {
    revision: String,
    expires_at: f64,
}

/// The scope revisions, in the order the record keeps them.
type Revisions = Arc<Vec<(String, RevisionEntry)>>;

fn number(value: f64) -> Value {
    if value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        json!(value as i64)
    } else {
        json!(value)
    }
}

fn decode_revisions(raw: &str) -> Result<Revisions, String> {
    let value: Value = serde_json::from_str(raw).map_err(|error| error.to_string())?;
    let Value::Object(entries) = value else {
        return Err("Expected an object of revisions".into());
    };
    let mut out = Vec::with_capacity(entries.len());
    for (scope, entry) in entries {
        let revision = entry.get("revision").and_then(Value::as_str).ok_or("Expected a revision")?;
        let expires_at = entry
            .get("expiresAt")
            .and_then(Value::as_f64)
            .filter(|at| at.is_finite())
            .ok_or("Expected a finite expiresAt")?;
        out.push((
            scope,
            RevisionEntry {
                revision: revision.to_owned(),
                expires_at,
            },
        ));
    }
    Ok(Arc::new(out))
}

fn encode_revisions(revisions: &[(String, RevisionEntry)]) -> String {
    let mut map = Map::new();
    for (scope, entry) in revisions {
        map.insert(scope.clone(), json!({ "revision": entry.revision, "expiresAt": number(entry.expires_at) }));
    }
    Value::Object(map).to_string()
}

/// `Persistable.serializeExit` of a success, with its absolute expiry (`[exit, expires]`).
fn encode_entry(stored: &Stored, expires: i64) -> String {
    let mut value = Map::new();
    value.insert("payload".into(), json!(stored.payload));
    value.insert("expiresAt".into(), number(stored.expires_at));
    if let Some(revision) = &stored.revision {
        value.insert("revision".into(), json!(revision));
    }
    json!([{ "_tag": "Success", "value": Value::Object(value) }, expires]).to_string()
}

enum Decoded {
    Missing,
    Expired,
    Exit(Result<Stored, Failure>),
}

/// `BackingPersistence.get` + `Persistable.deserializeExit`.
fn decode_entry(raw: &str, now: i64) -> Result<Decoded, Failure> {
    let parsed: Value = serde_json::from_str(raw).map_err(|error| Failure::Store(format!("Failed to parse value from backing store: {error}")))?;
    let Value::Array(items) = parsed else {
        return Ok(Decoded::Missing);
    };
    let exit = items.first().cloned().unwrap_or(Value::Null);
    if let Some(expires) = items.get(1).and_then(Value::as_f64) {
        if expires <= now as f64 {
            return Ok(Decoded::Expired);
        }
    }
    let schema = |issue: &str| Failure::Store(format!("SchemaError: {issue}"));
    match exit.get("_tag").and_then(Value::as_str) {
        Some("Success") => {
            let value = exit.get("value").ok_or_else(|| schema("missing value"))?;
            let payload = value.get("payload").and_then(Value::as_str).ok_or_else(|| schema("payload"))?;
            let expires_at = value
                .get("expiresAt")
                .and_then(Value::as_f64)
                .filter(|at| at.is_finite())
                .ok_or_else(|| schema("expiresAt"))?;
            let revision = match value.get("revision") {
                None => None,
                Some(Value::String(revision)) => Some(revision.clone()),
                Some(_) => return Err(schema("revision")),
            };
            Ok(Decoded::Exit(Ok(Stored {
                payload: payload.to_owned(),
                expires_at,
                revision,
            })))
        }
        Some("Failure") => {
            let error = exit
                .get("cause")
                .and_then(Value::as_array)
                .and_then(|cause| cause.iter().find(|part| part.get("_tag").and_then(Value::as_str) == Some("Fail")))
                .and_then(|fail| fail.get("error"))
                .and_then(PullRequestError::from_wire)
                .ok_or_else(|| schema("cause"))?;
            Ok(Decoded::Exit(Err(Failure::Read(error))))
        }
        _ => Err(schema("exit")),
    }
}

type Lookup = Shared<BoxFuture<'static, Result<String, PullRequestError>>>;

struct Inner {
    backing: Arc<dyn KeyValueStore>,
    clock: SharedClock,
    enabled: AtomicBool,
    lock: Arc<Semaphore>,
    revisions: TtlCache<(), Revisions, Failure>,
    memory: TtlCache<ReadKey, Stored, Failure>,
    refreshes: TtlCache<ReadKey, Stored, Failure>,
}

impl Inner {
    async fn load_revisions(self: Arc<Self>) -> Result<Revisions, Failure> {
        let raw = self.backing.get(REVISIONS_KEY).await.map_err(|error| Failure::Store(error.message))?;
        decode_revisions(raw.as_deref().unwrap_or("{}")).map_err(Failure::Store)
    }

    async fn current_revisions(self: &Arc<Self>) -> Result<Revisions, Failure> {
        let this = self.clone();
        self.revisions.get((), move || this.load_revisions()).await
    }

    /// `PersistedCache`'s in-memory lookup: the stored exit, or the read (stored when it
    /// succeeded).
    async fn load(self: Arc<Self>, key: ReadKey, lookup: Lookup) -> Result<Stored, Failure> {
        let primary = key.primary_key();
        let raw = self
            .backing
            .get(&primary)
            .await
            .map_err(|error| Failure::Store(format!("Failed to get key {primary} from backing store: {error}")))?;
        if let Some(raw) = raw {
            match decode_entry(&raw, self.clock.now_millis())? {
                Decoded::Exit(exit) => return exit,
                Decoded::Expired => {
                    let _ = self.backing.remove(&primary).await;
                }
                Decoded::Missing => {}
            }
        }
        let result = lookup.await;
        let now = self.clock.now_millis();
        let stored = match result {
            Ok(payload) => Stored {
                payload,
                expires_at: (now + ENTRY_TTL_MS) as f64,
                revision: Some(key.revision.clone()),
            },
            Err(error) => return Err(Failure::Read(error)),
        };
        let ttl = (stored.expires_at - now as f64).max(0.0) as i64;
        if ttl > 0 {
            self.backing
                .set(&primary, &encode_entry(&stored, now + ttl))
                .await
                .map_err(|error| Failure::Store(format!("Failed to set key {primary} in backing store: {error}")))?;
        }
        Ok(stored)
    }

    async fn cached(self: &Arc<Self>, key: ReadKey, lookup: Lookup) -> Result<Stored, Failure> {
        let this = self.clone();
        let memory_key = key.clone();
        self.memory.get(memory_key, move || this.load(key, lookup)).await
    }

    /// The `refreshes` lookup: a held read whose revision moved on is dropped and read again.
    async fn refresh(self: Arc<Self>, key: ReadKey, lookup: Lookup) -> Result<Stored, Failure> {
        let stored = self.cached(key.clone(), lookup.clone()).await?;
        if key.matches(stored.revision.as_deref()) {
            return Ok(stored);
        }
        self.backing
            .remove(&key.primary_key())
            .await
            .map_err(|error| Failure::Store(format!("Failed to remove key from backing store: {error}")))?;
        self.memory.invalidate(&key);
        self.cached(key, lookup).await
    }

    async fn read(self: &Arc<Self>, key: &str, lookup: Lookup, scopes: &[String]) -> Result<String, Failure> {
        let current = self.current_revisions().await?;
        let now = self.clock.now_millis() as f64;
        let revision = scopes
            .iter()
            .map(|scope| match current.iter().find(|(held, _)| held == scope) {
                Some((_, entry)) if entry.expires_at > now => entry.revision.clone(),
                _ => String::new(),
            })
            .collect::<Vec<_>>()
            .join(":");
        let key = ReadKey {
            digest: sha256_hex(key),
            revision,
        };
        let stored = self.cached(key.clone(), lookup.clone()).await?;
        if key.matches(stored.revision.as_deref()) {
            return Ok(stored.payload);
        }
        let this = self.clone();
        let refresh_key = key.clone();
        Ok(self.refreshes.get(refresh_key, move || this.refresh(key, lookup)).await?.payload)
    }

    async fn write_revision(self: &Arc<Self>, scope: &str) -> Result<(), Failure> {
        let now = self.clock.now_millis();
        let current = self.current_revisions().await?;
        let mut next: Vec<(String, RevisionEntry)> = current.iter().filter(|(_, entry)| entry.expires_at > now as f64).cloned().collect();
        let entry = RevisionEntry {
            revision: uuid::Uuid::new_v4().to_string(),
            expires_at: (now + ENTRY_TTL_MS) as f64,
        };
        match next.iter_mut().find(|(held, _)| held == scope) {
            Some((_, held)) => *held = entry,
            None => next.push((scope.to_owned(), entry)),
        }
        self.backing
            .set(REVISIONS_KEY, &encode_revisions(&next))
            .await
            .map_err(|error| Failure::Store(error.message))?;
        self.revisions.set((), Arc::new(next));
        Ok(())
    }
}

/// `PullRequestReadCache`.
#[derive(Clone)]
pub struct PullRequestReadCache {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for PullRequestReadCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PullRequestReadCache")
            .field("enabled", &self.inner.enabled.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl PullRequestReadCache {
    /// `make` over any store, on the given clock.
    pub fn with_store(backing: Arc<dyn KeyValueStore>, clock: SharedClock) -> Self {
        let memory_clock = clock.clone();
        Self {
            inner: Arc::new(Inner {
                backing,
                enabled: AtomicBool::new(true),
                lock: Arc::new(Semaphore::new(CONCURRENT_READS as usize)),
                revisions: TtlCache::new(1, clock.clone(), |_, result| if result.is_ok() { FOREVER } else { 0 }),
                memory: TtlCache::new(
                    CONCURRENT_READS as usize,
                    clock.clone(),
                    move |_, result: &Result<Stored, Failure>| match result {
                        Ok(stored) => (stored.expires_at - memory_clock.now_millis() as f64).max(0.0) as i64,
                        Err(_) => 0,
                    },
                ),
                refreshes: TtlCache::new(CONCURRENT_READS as usize, clock.clone(), |_, _| 0),
                clock,
            }),
        }
    }

    /// The cache in `directory` (one file per key), on the system clock. Falls back to memory,
    /// with a warning, when the directory cannot be created.
    pub fn open(directory: &Path) -> Self {
        Self::open_with_clock(directory, system_clock())
    }

    /// [`Self::open`] on the given clock.
    pub fn open_with_clock(directory: &Path, clock: SharedClock) -> Self {
        match FileSystemKeyValueStore::open(directory) {
            Ok(store) => Self::with_store(Arc::new(store), clock),
            Err(error) => {
                tracing::warn!(directory = %directory.display(), %error, "PR cache directory unavailable; using memory cache");
                Self::memory_with_clock(clock)
            }
        }
    }

    /// The TS layer: `<providerStatusCacheDir>/pull-requests`.
    pub fn open_in_provider_status_cache(provider_status_cache_dir: &Path) -> Self {
        Self::open(&provider_status_cache_dir.join(DIRECTORY_NAME))
    }

    /// An in-memory cache (`KeyValueStore.layerMemory`), on the system clock.
    pub fn memory() -> Self {
        Self::memory_with_clock(system_clock())
    }

    /// [`Self::memory`] on the given clock.
    pub fn memory_with_clock(clock: SharedClock) -> Self {
        Self::with_store(Arc::new(MemoryKeyValueStore::new()), clock)
    }

    /// Whether the cache still answers (it turns itself off after failing to record an
    /// invalidation).
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::SeqCst)
    }

    /// `get(key, lookup, scopes)`: the held read of `key`, or `lookup`'s answer (kept for a
    /// minute). The lookup runs at most once per call, whatever happens around it.
    pub async fn get(&self, key: &str, lookup: BoxFuture<'static, Result<String, PullRequestError>>, scopes: &[String]) -> Result<String, PullRequestError> {
        let read = lookup.shared();
        if !self.is_enabled() {
            return read.await;
        }
        let _permit = self.inner.lock.acquire().await.ok();
        match self.inner.read(key, read.clone(), scopes).await {
            Ok(payload) => Ok(payload),
            Err(Failure::Read(error)) => Err(error),
            Err(Failure::Store(reason)) => {
                tracing::debug!(%reason, "PR cache store unavailable; reading the host");
                read.await
            }
        }
    }

    /// `invalidate(scope)`: reads held under `scope` are read again for the next minute. Waits
    /// for the reads in flight; once started it finishes even if the caller goes away. Never
    /// fails: a store that cannot record it disables the cache.
    pub async fn invalidate(&self, scope: &str) {
        let Ok(permit) = self.inner.lock.clone().acquire_many_owned(CONCURRENT_READS).await else {
            return;
        };
        let inner = self.inner.clone();
        let scope = scope.to_owned();
        let task = tokio::spawn(async move {
            let _permit = permit;
            if inner.write_revision(&scope).await.is_err() {
                inner.enabled.store(false, Ordering::SeqCst);
                tracing::warn!("PR cache disabled after clearing failed");
            }
        });
        let _ = task.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_revisions_slot_by_slot() {
        let key = |revision: &str| ReadKey {
            digest: String::new(),
            revision: revision.into(),
        };
        assert!(key("").matches(None));
        assert!(key(":").matches(Some("a:b")));
        assert!(key("a:").matches(Some("a:b")));
        assert!(!key("a:").matches(Some("c:b")));
        assert!(!key(":b").matches(None));
    }

    #[test]
    fn round_trips_the_effect_layout() {
        let stored = Stored {
            payload: "{\"a\":1}".into(),
            expires_at: 60_000.0,
            revision: Some(":x".into()),
        };
        let raw = encode_entry(&stored, 60_000);
        assert_eq!(
            raw,
            r#"[{"_tag":"Success","value":{"payload":"{\"a\":1}","expiresAt":60000,"revision":":x"}},60000]"#
        );
        match decode_entry(&raw, 0) {
            Ok(Decoded::Exit(Ok(decoded))) => assert_eq!(decoded.payload, stored.payload),
            _ => panic!("decodes"),
        }
        assert!(matches!(decode_entry(&raw, 60_000), Ok(Decoded::Expired)));
        let revisions = decode_revisions(r#"{"pr":{"revision":"r1","expiresAt":5}}"#).unwrap();
        assert_eq!(encode_revisions(&revisions), r#"{"pr":{"revision":"r1","expiresAt":5}}"#);
    }
}
