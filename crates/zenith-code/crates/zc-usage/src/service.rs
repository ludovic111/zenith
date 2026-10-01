//! `UsageService` (`usage/UsageService.ts`): scans the provider transcripts and databases and
//! returns priced usage buckets (`server.getUsageSummary`), and refreshes the rate table
//! (`server.refreshUsageRates`).
//!
//! Transcript records are cached per file by `(size, mtime)` in `usage-scan-cache.json`; a
//! file that only grew resumes from its cached parse position. Files that need parsing are
//! parsed in parallel (the TS server parses them one after the other); everything else —
//! walk order, dedupe order, bucket order — is the TS order, so the summaries are the same.
//! SQLite readers query the live databases on every scan so WAL writes stay visible.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt, Shared};
use indexmap::IndexMap;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use zc_contracts::{UsageProviderKind, UsageReadError, UsageReadErrorReason, UsageResolution, UsageSummaryInput};

use crate::aggregation::{AggregateOptions, UsageAggregator};
use crate::antigravity::read_antigravity_usage;
use crate::cursor::{read_cursor_account_usage, CredentialSource, CursorHttp, KeychainToken};
use crate::json::{self, J};
use crate::opencode::read_opencode_usage;
use crate::pricing::{create_override_rate_table, parse_rate_table, RateTable};
use crate::reader::{list_transcript_files, read_directory_volume_id, read_transcript_records, ParsePosition, ParseResult, TranscriptFile};
use crate::records::UsageRecord;
use crate::scan_cache::{decode_scan_cache, dedupe_within_file, encode_scan_cache, prune_scan_cache, CachedFile, ScanCache};
use crate::settings::UsageSettings;
use crate::time::{date_parse, iso_from_millis};

pub const LITELLM_RATES_URL: &str = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
/// `USAGE_CONTRACT_VERSION`.
pub const USAGE_CONTRACT_VERSION: u32 = 6;
/// Rates move rarely; a day-old table keeps the page working offline.
const RATES_TTL_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
/// An explicit refresh ignores the TTL, but not a table fetched this recently.
const RATES_REFRESH_FLOOR_MS: f64 = 60.0 * 1000.0;
const RATES_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Files are filtered by mtime before opening; the slack covers a session whose last write
/// lands just before local midnight on the window's first day.
const MTIME_SLACK_MS: f64 = 36.0 * 60.0 * 60.0 * 1000.0;
const MAX_HOURLY_WINDOW_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;
/// The longest window the UI offers, plus slack. Older cache entries are pruned.
const CACHE_RETENTION_DAYS: f64 = 90.0;
const DAY_MS: f64 = 24.0 * 60.0 * 60.0 * 1000.0;

/// The host platform (`HostProcessPlatform`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Darwin,
    Linux,
    Win32,
}

impl Platform {
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Platform::Darwin
        } else if cfg!(windows) {
            Platform::Win32
        } else {
            Platform::Linux
        }
    }
}

/// Fetches the LiteLLM rate document (GET, 2xx only).
#[async_trait]
pub trait RatesFetcher: Send + Sync {
    async fn fetch(&self, timeout: Duration) -> Result<Vec<u8>, String>;
}

/// [`RatesFetcher`] over reqwest.
#[derive(Clone, Default)]
pub struct ReqwestRatesFetcher {
    client: reqwest::Client,
}

#[async_trait]
impl RatesFetcher for ReqwestRatesFetcher {
    async fn fetch(&self, timeout: Duration) -> Result<Vec<u8>, String> {
        let response = self
            .client
            .get(LITELLM_RATES_URL)
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        Ok(response.bytes().await.map_err(|error| error.to_string())?.to_vec())
    }
}

/// What the service is built from.
#[derive(Clone)]
pub struct UsageServiceOptions {
    /// `config.stateDir`: `usage-model-rates.json` and `usage-scan-cache.json` live here.
    pub state_dir: PathBuf,
    pub settings: Arc<dyn UsageSettings>,
    /// The host process environment (`HostProcessEnvironment`).
    pub environment: HashMap<String, String>,
    pub platform: Platform,
    /// `os.homedir()`.
    pub home_dir: PathBuf,
    /// `os.hostname()`.
    pub hostname: String,
    pub rates: Arc<dyn RatesFetcher>,
    pub cursor_http: Arc<dyn CursorHttp>,
    pub keychain: Arc<dyn KeychainToken>,
    /// Wall clock in epoch milliseconds.
    pub now_ms: Arc<dyn Fn() -> f64 + Send + Sync>,
}

impl UsageServiceOptions {
    /// The production wiring: real environment, network, Keychain and clock.
    pub fn system(state_dir: PathBuf, settings: Arc<dyn UsageSettings>, hostname: String) -> Self {
        Self {
            state_dir,
            settings,
            environment: std::env::vars().collect(),
            platform: Platform::current(),
            home_dir: zc_core::paths::home_dir(),
            hostname,
            rates: Arc::new(ReqwestRatesFetcher::default()),
            cursor_http: Arc::new(crate::cursor::ReqwestCursorHttp::default()),
            keychain: crate::cursor::shared_keychain_token(),
            #[allow(clippy::cast_precision_loss)]
            now_ms: Arc::new(|| zc_core::time::now_millis() as f64),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CachedSource {
    dir: String,
    volume_id: String,
}

#[derive(Default)]
struct ScanState {
    loaded: bool,
    file_cache: ScanCache,
    source_cache: IndexMap<String, CachedSource>,
    dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RatesStatus {
    Fresh,
    Cached,
    Unavailable,
}

struct RatesState {
    table: Arc<RateTable>,
    fetched_at_ms: Option<f64>,
    status: RatesStatus,
}

type SharedScan = Shared<BoxFuture<'static, Result<Value, UsageReadError>>>;

struct Inner {
    options: UsageServiceOptions,
    rates_cache_path: PathBuf,
    scan_cache_path: PathBuf,
    rates: Mutex<RatesState>,
    /// One rate fetch at a time; a burst of refreshes waits on the first.
    rates_lock: tokio::sync::Mutex<()>,
    scan: tokio::sync::Mutex<ScanState>,
    inflight: Mutex<HashMap<String, SharedScan>>,
}

/// The service; cheap to clone.
#[derive(Clone)]
pub struct UsageService {
    inner: Arc<Inner>,
}

fn read_error(reason: UsageReadErrorReason, detail: String) -> UsageReadError {
    UsageReadError {
        tag: Default::default(),
        reason,
        detail,
        cause: None,
    }
}

/// One provider directory's walk and parse, before rates are involved.
struct ScannedDir {
    provider: UsageProviderKind,
    dir: String,
    volume_id: String,
    host_id: Option<String>,
    status: Option<&'static str>,
    message: Option<String>,
    action: Option<&'static str>,
    /// Records per file, or `None` when the directory does not exist.
    files: Option<Vec<(String, Vec<UsageRecord>)>>,
}

impl ScannedDir {
    fn new(provider: UsageProviderKind, dir: String, volume_id: String, files: Option<Vec<(String, Vec<UsageRecord>)>>) -> Self {
        Self {
            provider,
            dir,
            volume_id,
            host_id: None,
            status: None,
            message: None,
            action: None,
            files,
        }
    }
}

struct TranscriptDir {
    provider: UsageProviderKind,
    dir: String,
    volume_id: String,
    file_name: Option<&'static str>,
}

fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// `fs.realpath`, or `None` when it fails.
fn real_path(path: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(path).ok()
}

/// `isWithinDirectory`: `path.relative(dir, file)` does not climb out.
fn is_within_directory(file_path: &str, dir: &str) -> bool {
    Path::new(file_path).strip_prefix(dir).is_ok()
}

fn trimmed_env<'a>(environment: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    environment.get(key).map(|value| json::js_trim(value)).filter(|value| !value.is_empty())
}

/// `expandHomePath` with the configured home directory.
fn expand_home(value: &str, home: &Path) -> PathBuf {
    zc_core::paths::expand_home_path_with(value, home)
}

/// `mergeProviderInstanceEnvironment(instance.environment, hostEnvironment)`.
fn merge_environment(instance: &Value, host: &HashMap<String, String>, home: &Path) -> HashMap<String, String> {
    let Some(variables) = instance.get("environment").and_then(Value::as_array).filter(|variables| !variables.is_empty()) else {
        return host.clone();
    };
    let mut next = host.clone();
    for variable in variables {
        let (Some(name), Some(value)) = (variable.get("name").and_then(Value::as_str), variable.get("value").and_then(Value::as_str)) else {
            continue;
        };
        let value = if name == "CODEX_HOME" || name == "CLAUDE_CONFIG_DIR" {
            path_string(&expand_home(value, home))
        } else {
            value.to_owned()
        };
        next.insert(name.to_owned(), value);
    }
    next
}

/// Parses `jobs` across a few threads, results in job order.
fn parse_in_parallel(jobs: Vec<(PathBuf, UsageProviderKind, Option<ParsePosition>)>) -> Vec<Option<ParseResult>> {
    if jobs.is_empty() {
        return Vec::new();
    }
    let workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .clamp(1, 8)
        .min(jobs.len());
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<Option<ParseResult>>>> = jobs.iter().map(|_| Mutex::new(None)).collect();
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                let Some((path, provider, resume)) = jobs.get(index) else {
                    break;
                };
                let parsed = read_transcript_records(path, *provider, resume.as_ref());
                *results[index].lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(parsed);
            });
        }
    });
    results
        .into_iter()
        .map(|slot| slot.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner).flatten())
        .collect()
}

/// `readFileRecords` for one directory's files: cache hits as they are, the rest parsed
/// (resuming grown files), then folded back into the cache in walk order.
fn read_dir_records(state: &mut ScanState, provider: UsageProviderKind, files: &[TranscriptFile]) -> Vec<(String, Vec<UsageRecord>)> {
    let mut jobs = Vec::new();
    let mut job_of_file: Vec<Option<usize>> = Vec::with_capacity(files.len());
    #[allow(clippy::cast_precision_loss)]
    for file in files {
        let key = path_string(&file.path);
        let size = file.size as f64;
        let cached = state.file_cache.get(&key);
        if let Some(cached) = cached {
            // Provider is part of the identity: another parser's hit must not be reused.
            if cached.size == size && cached.mtime_ms == file.mtime_ms && cached.provider == provider {
                job_of_file.push(None);
                continue;
            }
        }
        // Only a strictly grown file may resume.
        let resume = cached
            .filter(|cached| cached.provider == provider && size > cached.size)
            .map(|cached| cached.position.clone());
        job_of_file.push(Some(jobs.len()));
        jobs.push((file.path.clone(), provider, resume));
    }
    let mut parsed = parse_in_parallel(jobs).into_iter();
    let mut out = Vec::with_capacity(files.len());
    #[allow(clippy::cast_precision_loss)]
    for (file, job) in files.iter().zip(job_of_file) {
        let key = path_string(&file.path);
        let records = match job {
            None => state.file_cache.get(&key).map(CachedFile::all_records).unwrap_or_default(),
            Some(_) => {
                let result = parsed.next().flatten();
                let cached = state.file_cache.get(&key);
                match result {
                    // A read failure is not an empty transcript: never cache it.
                    None => cached
                        .filter(|cached| cached.provider == provider)
                        .map(CachedFile::all_records)
                        .unwrap_or_default(),
                    Some(result) => {
                        let base = if result.resumed {
                            cached.map(|cached| cached.records.clone()).unwrap_or_default()
                        } else {
                            Vec::new()
                        };
                        let mut seen = HashSet::new();
                        let mut combined = base;
                        combined.extend(result.records);
                        let records = dedupe_within_file(combined, &mut seen);
                        let tail_records = dedupe_within_file(result.tail_records, &mut seen);
                        let entry = CachedFile {
                            size: file.size as f64,
                            mtime_ms: file.mtime_ms,
                            provider,
                            records,
                            tail_records,
                            position: result.position,
                        };
                        let all = entry.all_records();
                        state.file_cache.insert(key.clone(), entry);
                        state.dirty = true;
                        all
                    }
                }
            }
        };
        out.push((key, records));
    }
    out
}

/// The key that lets concurrent identical requests share one scan.
fn scan_key(input: &UsageSummaryInput, price_overrides: &Value, cursor_keychain_usage_enabled: bool) -> String {
    json!([
        input.time_zone.as_str(),
        input.since_day.as_str(),
        input.until_day.as_str(),
        input.resolution.map_or("day", UsageResolution::as_str),
        input.since_time.as_deref(),
        input.until_time.as_deref(),
        price_overrides,
        cursor_keychain_usage_enabled,
    ])
    .to_string()
}

impl UsageService {
    pub fn new(options: UsageServiceOptions) -> Self {
        let rates_cache_path = options.state_dir.join("usage-model-rates.json");
        let scan_cache_path = options.state_dir.join("usage-scan-cache.json");
        Self {
            inner: Arc::new(Inner {
                options,
                rates_cache_path,
                scan_cache_path,
                rates: Mutex::new(RatesState {
                    table: Arc::new(RateTable::new()),
                    fetched_at_ms: None,
                    status: RatesStatus::Unavailable,
                }),
                rates_lock: tokio::sync::Mutex::new(()),
                scan: tokio::sync::Mutex::new(ScanState::default()),
                inflight: Mutex::new(HashMap::new()),
            }),
        }
    }

    fn now(&self) -> f64 {
        (self.inner.options.now_ms)()
    }

    fn rates_state(&self) -> std::sync::MutexGuard<'_, RatesState> {
        self.inner.rates.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The encoded `UsagePricing`.
    pub fn pricing(&self) -> Value {
        let state = self.rates_state();
        json!({
            "status": match state.status {
                RatesStatus::Fresh => "fresh",
                RatesStatus::Cached => "cached",
                RatesStatus::Unavailable => "unavailable",
            },
            "source": LITELLM_RATES_URL,
            "fetchedAt": state.fetched_at_ms.and_then(iso_from_millis),
            "knownModels": state.table.len(),
        })
    }

    /// `loadRates`: a fresh table, else the on-disk snapshot, else unpriced models.
    async fn load_rates(&self, force: bool) {
        let now = self.now();
        let max_age = if force { RATES_REFRESH_FLOOR_MS } else { RATES_TTL_MS };
        let fetched_at = self.rates_state().fetched_at_ms;
        if fetched_at.is_some_and(|fetched| now - fetched < max_age) {
            return;
        }
        if fetched_at.is_none() {
            let from_disk = tokio::fs::read(&self.inner.rates_cache_path)
                .await
                .ok()
                .and_then(|raw| json::parse(&raw))
                .and_then(|document| {
                    let fetched_at_ms = document.get("fetchedAtMs").and_then(J::as_num)?;
                    let table = parse_rate_table(document.get("document").unwrap_or(&J::Null));
                    Some((fetched_at_ms, table))
                });
            if let Some((fetched_at_ms, table)) = from_disk {
                if !table.is_empty() {
                    let mut state = self.rates_state();
                    state.table = Arc::new(table);
                    state.fetched_at_ms = Some(fetched_at_ms);
                    state.status = RatesStatus::Cached;
                    if now - fetched_at_ms < max_age {
                        return;
                    }
                }
            }
        }
        let fetched = self
            .inner
            .options
            .rates
            .fetch(RATES_FETCH_TIMEOUT)
            .await
            .ok()
            .and_then(|body| json::parse(&body));
        let Some(document) = fetched else {
            // Whatever is served is now past its TTL and must not claim to be fresh.
            let mut state = self.rates_state();
            if !state.table.is_empty() {
                state.status = RatesStatus::Cached;
            }
            return;
        };
        let table = parse_rate_table(&document);
        if table.is_empty() {
            return;
        }
        {
            let mut state = self.rates_state();
            state.table = Arc::new(table);
            state.fetched_at_ms = Some(now);
            state.status = RatesStatus::Fresh;
        }
        let serialized = format!(
            "{{\"fetchedAtMs\":{},\"document\":{}}}",
            zc_providers::js_json::format_js_number(now),
            json::stringify(&document)
        );
        if let Err(error) = zc_core::write_file_string_atomically(&self.inner.rates_cache_path, &serialized).await {
            tracing::debug!(%error, "could not save the usage rate table");
        }
    }

    async fn ensure_rates(&self, force: bool) {
        let _permit = self.inner.rates_lock.lock().await;
        self.load_rates(force).await;
    }

    /// `refreshRates`: refetches ahead of the TTL (not within a minute of the last fetch).
    pub async fn refresh_rates(&self) -> Value {
        self.ensure_rates(true).await;
        self.pricing()
    }

    /// `readSummary`: concurrent identical requests share one scan, which runs detached so
    /// a departing caller never cancels it for the others.
    pub async fn read_summary(&self, input: UsageSummaryInput) -> Result<Value, UsageReadError> {
        let settings = self
            .inner
            .options
            .settings
            .get()
            .await
            .map_err(|_| read_error(UsageReadErrorReason::ScanFailed, "Server settings could not be read.".to_owned()))?;
        let key = scan_key(
            &input,
            settings.get("usagePriceOverrides").unwrap_or(&Value::Null),
            settings.get("cursorKeychainUsageEnabled") == Some(&Value::Bool(true)),
        );
        let shared = {
            let mut inflight = self.inner.inflight.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = inflight.get(&key) {
                existing.clone()
            } else {
                let service = self.clone();
                let task_key = key.clone();
                let task = tokio::spawn(async move {
                    let result = service.scan_summary(&input, &settings).await;
                    service
                        .inner
                        .inflight
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&task_key);
                    result
                });
                let shared = async move {
                    task.await
                        .unwrap_or_else(|error| Err(read_error(UsageReadErrorReason::ScanFailed, format!("The usage scan failed: {error}"))))
                }
                .boxed()
                .shared();
                inflight.insert(key, shared.clone());
                shared
            }
        };
        shared.await
    }

    async fn scan_summary(&self, input: &UsageSummaryInput, settings: &Value) -> Result<Value, UsageReadError> {
        let since_day = input.since_day.as_str();
        let until_day = input.until_day.as_str();
        if since_day > until_day {
            return Err(read_error(
                UsageReadErrorReason::InvalidWindow,
                format!("sinceDay '{since_day}' is after untilDay '{until_day}'"),
            ));
        }
        let mut hourly_window = None;
        if input.resolution == Some(UsageResolution::Hour) {
            let since = input.since_time.as_ref().and_then(|value| date_parse(value.as_str()));
            let until = input.until_time.as_ref().and_then(|value| date_parse(value.as_str()));
            let (Some(since), Some(until)) = (since, until) else {
                return Err(read_error(
                    UsageReadErrorReason::InvalidWindow,
                    "Hourly usage requires valid sinceTime and untilTime instants".to_owned(),
                ));
            };
            let duration = until - since;
            if duration <= 0.0 || duration > MAX_HOURLY_WINDOW_MS {
                return Err(read_error(
                    UsageReadErrorReason::InvalidWindow,
                    "Hourly usage window must be greater than zero and at most 24 hours".to_owned(),
                ));
            }
            hourly_window = Some((since, until));
        }

        let started_at = self.now();
        let mut scan = self.inner.scan.lock().await;
        self.ensure_scan_cache_loaded(&mut scan).await;

        let Some(window_start) = date_parse(&format!("{since_day}T00:00:00Z")) else {
            return Err(read_error(
                UsageReadErrorReason::InvalidWindow,
                format!("sinceDay '{since_day}' is not a valid date"),
            ));
        };
        let window_start_ms = hourly_window.map_or(window_start, |(since, _)| since) - MTIME_SLACK_MS;
        let retention_cutoff_ms = started_at - CACHE_RETENTION_DAYS * DAY_MS;

        // The rate table loads while transcripts stream instead of gating them.
        let ((), scanned) = tokio::join!(
            self.ensure_rates(false),
            self.collect_dirs(&mut scan, window_start_ms, settings, retention_cutoff_ms)
        );

        let mut aggregator = UsageAggregator::new(AggregateOptions {
            time_zone: input.time_zone.as_str().to_owned(),
            since_day: since_day.to_owned(),
            until_day: until_day.to_owned(),
            rates: self.rates_state().table.clone(),
            price_overrides: Some(create_override_rate_table(settings.get("usagePriceOverrides"))),
            hourly_window,
        });
        let host_id = self.inner.options.hostname.clone();
        let mut sources = Vec::new();
        for scanned_dir in scanned {
            let ScannedDir {
                provider,
                dir,
                volume_id,
                host_id: source_host_id,
                status,
                message,
                action,
                files,
            } = scanned_dir;
            let files_missing = files.is_none();
            let mut retained = files.unwrap_or_default();
            let live: HashSet<String> = retained.iter().map(|(path, _)| path.clone()).collect();
            // Cleanup may remove transcripts; the usage already saved still counts.
            for (path, entry) in &scan.file_cache {
                if entry.provider != provider || entry.mtime_ms < retention_cutoff_ms || live.contains(path) || !is_within_directory(path, &dir) {
                    continue;
                }
                retained.push((path.clone(), entry.all_records()));
            }
            let mut scanned_files = 0u64;
            let mut skipped_files = 0u64;
            let mut session_ids: HashSet<Arc<str>> = HashSet::new();
            for (_, records) in &retained {
                if records.is_empty() {
                    skipped_files += 1;
                    continue;
                }
                scanned_files += 1;
                let mut codex_occurrences: HashMap<String, u64> = HashMap::new();
                for record in records {
                    let contributed = if record.provider == UsageProviderKind::Codex && !record.session_id.is_empty() {
                        // Matches moved rollout copies without collapsing repeated equal
                        // events within one rollout.
                        let key = format!(
                            "[\"{}\",{},{},{},{}]",
                            record.provider.as_str(),
                            json::quote(&record.session_id),
                            zc_providers::js_json::format_js_number(record.timestamp_ms),
                            json::quote(&record.model),
                            record.totals.stringify()
                        );
                        let occurrence = codex_occurrences.entry(key.clone()).or_insert(0);
                        *occurrence += 1;
                        let mut keyed = record.clone();
                        keyed.dedupe_key = Some(format!("{key}:{occurrence}"));
                        aggregator.add(&keyed, Some(&dir))
                    } else {
                        aggregator.add(record, Some(&dir))
                    };
                    // Only sessions contributing in-window count.
                    if contributed && !record.session_id.is_empty() {
                        session_ids.insert(record.session_id.clone());
                    }
                }
            }
            let mut source = Map::new();
            source.insert(
                "fingerprint".into(),
                json!({
                    "hostId": source_host_id.unwrap_or_else(|| host_id.clone()),
                    "provider": provider.as_str(),
                    "resolvedHomePath": dir,
                    "volumeId": volume_id,
                }),
            );
            // Clients exclude missing sources; saved records keep a source available.
            let status = if files_missing && scanned_files == 0 {
                "missing"
            } else {
                status.unwrap_or("ok")
            };
            source.insert("status".into(), json!(status));
            source.insert("scannedFiles".into(), json!(scanned_files));
            source.insert("skippedFiles".into(), json!(skipped_files));
            source.insert("malformedRecords".into(), json!(0));
            source.insert("distinctSessions".into(), json!(session_ids.len()));
            let message = message.or_else(|| files_missing.then(|| "No transcript directory on this environment.".to_owned()));
            source.insert("message".into(), message.map_or(Value::Null, Value::String));
            if let Some(action) = action {
                source.insert("action".into(), json!(action));
            }
            sources.push(Value::Object(source));
        }

        if prune_scan_cache(&mut scan.file_cache, retention_cutoff_ms) > 0 {
            scan.dirty = true;
        }
        self.persist_scan_cache(&mut scan).await;
        drop(scan);

        let aggregated = aggregator.finish();
        let finished_at = self.now();
        Ok(json!({
            "contractVersion": USAGE_CONTRACT_VERSION,
            "readAt": iso_from_millis(finished_at).unwrap_or_default(),
            "timeZone": input.time_zone.as_str(),
            "sinceDay": since_day,
            "untilDay": until_day,
            "buckets": aggregated.buckets.iter().map(crate::aggregation::Bucket::to_value).collect::<Vec<_>>(),
            "sources": sources,
            "pricing": self.pricing(),
            "scanDurationMs": json::num(f64::max(0.0, finished_at - started_at).trunc()),
        }))
    }

    /// Loads `usage-scan-cache.json` once per process.
    async fn ensure_scan_cache_loaded(&self, scan: &mut ScanState) {
        if scan.loaded {
            return;
        }
        scan.loaded = true;
        let path = self.inner.scan_cache_path.clone();
        let decoded = tokio::task::spawn_blocking(move || {
            let raw = std::fs::read(path).ok()?;
            // The exact reader: serde_json's default float parsing can be one ulp off, and
            // cached mtimes must compare equal to what stat reports.
            let document = json::parse(&raw)?;
            let cache = decode_scan_cache(&document);
            // All or nothing, like the schema decode of `{sources: Record<string, …>}`.
            let sources = document.get("sources").and_then(J::as_obj).and_then(|sources| {
                sources
                    .entries()
                    .into_iter()
                    .map(|(key, source)| {
                        let source = source.as_obj()?;
                        Some((
                            key.to_owned(),
                            CachedSource {
                                dir: source.get("dir")?.as_str()?.to_owned(),
                                volume_id: source.get("volumeId")?.as_str()?.to_owned(),
                            },
                        ))
                    })
                    .collect::<Option<Vec<_>>>()
            });
            Some((cache, sources))
        })
        .await
        .ok()
        .flatten();
        if let Some((cache, sources)) = decoded {
            for (path, entry) in cache {
                scan.file_cache.insert(path, entry);
            }
            for (key, source) in sources.unwrap_or_default() {
                scan.source_cache.insert(key, source);
            }
        }
    }

    async fn persist_scan_cache(&self, scan: &mut ScanState) {
        if !scan.dirty {
            return;
        }
        let mut document = encode_scan_cache(&scan.file_cache);
        let sources: Map<String, Value> = scan
            .source_cache
            .iter()
            .map(|(key, source)| (key.clone(), json!({"dir": source.dir, "volumeId": source.volume_id})))
            .collect();
        document.insert("sources".into(), Value::Object(sources));
        let serialized = Value::Object(document).to_string();
        // A cache we cannot write is a slower next start, not a failed read; it is retried.
        match zc_core::write_file_string_atomically(&self.inner.scan_cache_path, &serialized).await {
            Ok(()) => scan.dirty = false,
            Err(error) => tracing::debug!(%error, "could not save the usage scan cache"),
        }
    }

    /// `resolveTranscriptDirs`: the Claude, Codex and Grok homes of every configured account
    /// (disabled ones included: they still have history), once each.
    fn resolve_transcript_dirs(&self, scan: &mut ScanState, settings: &Value, retention_cutoff_ms: f64) -> Vec<TranscriptDir> {
        let options = &self.inner.options;
        let home = &options.home_dir;
        let instances_setting = settings.get("providerInstances").and_then(Value::as_object);
        let mut dirs = Vec::new();
        let mut seen = HashSet::new();
        for driver in ["claudeAgent", "codex", "grok"] {
            // Explicit default slots replace the legacy settings, as in the provider registry.
            let mut instances: Vec<Value> = instances_setting
                .map(|instances| {
                    instances
                        .values()
                        .filter(|instance| instance.get("driver").and_then(Value::as_str) == Some(driver))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if !instances_setting.is_some_and(|instances| instances.contains_key(driver)) {
                let config = settings
                    .get("providers")
                    .and_then(|providers| providers.get(driver))
                    .cloned()
                    .unwrap_or(Value::Null);
                instances.push(json!({"config": config}));
            }
            for instance in instances {
                let environment = merge_environment(&instance, &options.environment, home);
                let provider = match driver {
                    "claudeAgent" => UsageProviderKind::Claude,
                    "codex" => UsageProviderKind::Codex,
                    _ => UsageProviderKind::Grok,
                };
                let config = match instance.get("config") {
                    None | Some(Value::Null) => json!({}),
                    Some(config) => config.clone(),
                };
                let provider_home: PathBuf = match provider {
                    UsageProviderKind::Codex => {
                        let Ok(mut codex) = serde_json::from_value::<zc_contracts::CodexSettings>(config) else {
                            continue;
                        };
                        let managed = codex.setup_mode.map(|mode| mode.as_str()) == Some("managed");
                        if let Some(environment_home) = trimmed_env(&environment, "CODEX_HOME") {
                            if !managed && codex.home_path.trim().is_empty() && codex.shadow_home_path.trim().is_empty() {
                                environment_home.clone_into(&mut codex.home_path);
                            }
                        }
                        zc_provider_codex::home_layout::resolve_codex_home_layout(&codex).shared_home_path
                    }
                    UsageProviderKind::Claude => {
                        let Ok(claude) = serde_json::from_value::<zc_contracts::ClaudeSettings>(config) else {
                            continue;
                        };
                        let configured = json::js_trim(&claude.home_path);
                        if !configured.is_empty() {
                            expand_home(configured, home)
                        } else if let Some(environment_home) = trimmed_env(&environment, "CLAUDE_CONFIG_DIR") {
                            PathBuf::from(environment_home)
                        } else {
                            home.join(".claude")
                        }
                    }
                    _ => {
                        let grok_home = trimmed_env(&environment, "GROK_HOME").map_or_else(|| home.join(".grok"), PathBuf::from);
                        expand_home(&path_string(&grok_home), home)
                    }
                };
                let directory = zc_core::paths::resolve_path(&provider_home.join(if provider == UsageProviderKind::Claude { "projects" } else { "sessions" }));
                let directory = path_string(&directory);
                let source_key = format!("{}\0{directory}", provider.as_str());
                let previous = scan.source_cache.get(&source_key).cloned();
                // Canonical paths and fingerprints stay stable after root cleanup.
                let dir = real_path(Path::new(&directory))
                    .map(|path| path_string(&path))
                    .or_else(|| previous.as_ref().map(|previous| previous.dir.clone()))
                    .unwrap_or(directory);
                let current_volume_id = read_directory_volume_id(Path::new(&dir));
                let has_retained_history = scan.file_cache.iter().any(|(path, entry)| {
                    entry.provider == provider
                        && entry.mtime_ms >= retention_cutoff_ms
                        && entry.records.len() + entry.tail_records.len() > 0
                        && is_within_directory(path, &dir)
                });
                // A recreated directory keeps reporting retained history under its old identity.
                let volume_id = match &previous {
                    Some(previous) if previous.dir == dir && (has_retained_history || current_volume_id.is_empty()) => {
                        if previous.volume_id.is_empty() {
                            current_volume_id
                        } else {
                            previous.volume_id.clone()
                        }
                    }
                    _ => current_volume_id,
                };
                if previous.as_ref().is_none_or(|previous| previous.dir != dir || previous.volume_id != volume_id) {
                    scan.source_cache.insert(
                        source_key,
                        CachedSource {
                            dir: dir.clone(),
                            volume_id: volume_id.clone(),
                        },
                    );
                    scan.dirty = true;
                }
                if !seen.insert(format!("{}\0{dir}", provider.as_str())) {
                    continue;
                }
                dirs.push(TranscriptDir {
                    provider,
                    dir,
                    volume_id,
                    file_name: (provider == UsageProviderKind::Grok).then_some("updates.jsonl"),
                });
            }
        }
        dirs
    }

    /// `envRoots`: a comma-separated override, else the defaults; canonical and unique.
    fn env_roots(&self, key: &str, defaults: Vec<PathBuf>) -> Vec<PathBuf> {
        let options = &self.inner.options;
        let roots: Vec<PathBuf> = options
            .environment
            .get(key)
            .map(|value| {
                value
                    .split(',')
                    .map(json::js_trim)
                    .filter(|root| !root.is_empty())
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            })
            .filter(|roots| !roots.is_empty())
            .unwrap_or(defaults);
        let mut canonical: Vec<PathBuf> = Vec::new();
        for root in roots {
            let resolved = zc_core::paths::resolve_path(&expand_home(&path_string(&root), &options.home_dir));
            let real = real_path(&resolved).unwrap_or(resolved);
            if !canonical.contains(&real) {
                canonical.push(real);
            }
        }
        canonical
    }

    async fn collect_dirs(&self, scan: &mut ScanState, window_start_ms: f64, settings: &Value, retention_cutoff_ms: f64) -> Vec<ScannedDir> {
        let options = &self.inner.options;
        let dirs = self.resolve_transcript_dirs(scan, settings, retention_cutoff_ms);

        // Transcripts: walks and parses off the async runtime, the cache moved in and back.
        let state = std::mem::take(scan);
        let (state, mut scanned) = tokio::task::spawn_blocking(move || {
            let mut state = state;
            let mut scanned = Vec::new();
            for TranscriptDir {
                provider,
                dir,
                volume_id,
                file_name,
            } in dirs
            {
                if !Path::new(&dir).exists() {
                    scanned.push(ScannedDir::new(provider, dir, volume_id, None));
                    continue;
                }
                let files = list_transcript_files(Path::new(&dir), window_start_ms, file_name);
                let parsed = read_dir_records(&mut state, provider, &files);
                scanned.push(ScannedDir::new(provider, dir, volume_id, Some(parsed)));
            }
            (state, scanned)
        })
        .await
        .expect("the transcript scan does not panic");
        *scan = state;

        let home = options.home_dir.clone();
        let data_home = trimmed_env(&options.environment, "XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        let opencode_roots = self.env_roots(
            "OPENCODE_DATA_DIR",
            vec![data_home.unwrap_or_else(|| home.join(".local").join("share")).join("opencode")],
        );
        let mut antigravity_roots = self.env_roots(
            "ANTIGRAVITY_DATA_DIR",
            ["antigravity", "antigravity-cli", "antigravity-ide", "antigravity-backup"]
                .iter()
                .map(|name| home.join(".gemini").join(name))
                .chain(std::iter::once(home.join(".config").join("antigravity")))
                .collect(),
        );
        if let Some(instances) = settings.get("providerInstances").and_then(Value::as_object) {
            for (instance_id, instance) in instances {
                if instance.get("driver").and_then(Value::as_str) == Some("antigravity") {
                    let key: String = Sha256::digest(instance_id.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect();
                    antigravity_roots.push(options.state_dir.join("providers").join("antigravity").join(key).join("antigravity-acp"));
                }
            }
        }
        let sqlite = tokio::task::spawn_blocking(move || {
            let mut scanned = Vec::new();
            for dir in opencode_roots {
                let result = read_opencode_usage(&dir, window_start_ms);
                let files = if result.missing && !result.error {
                    None
                } else {
                    Some(result.files.into_iter().map(|file| (path_string(&file.path), file.records)).collect())
                };
                let mut source = ScannedDir::new(UsageProviderKind::Opencode, path_string(&dir), read_directory_volume_id(&dir), files);
                source.status = Some(if result.error { "partial" } else { "ok" });
                if result.error {
                    source.message = Some("Some OpenCode history could not be read.".to_owned());
                }
                scanned.push(source);
            }
            let mut antigravity_dirs: Vec<PathBuf> = Vec::new();
            for root in antigravity_roots {
                let resolved_root = real_path(&root).unwrap_or(root);
                let nested = resolved_root.join("conversations");
                let dir = if nested.exists() { nested } else { resolved_root };
                let dir = real_path(&dir).unwrap_or(dir);
                if !antigravity_dirs.contains(&dir) {
                    antigravity_dirs.push(dir);
                }
            }
            let antigravity = read_antigravity_usage(&antigravity_dirs, window_start_ms);
            for dir in antigravity_dirs {
                let exists = dir.exists();
                let failed = antigravity.errors.iter().any(|error| error == &dir || error.starts_with(&dir));
                let files = if !exists && !failed {
                    None
                } else {
                    Some(
                        antigravity
                            .files
                            .iter()
                            .filter(|file| file.root == dir)
                            .map(|file| (path_string(&file.path), file.records.clone()))
                            .collect(),
                    )
                };
                let mut source = ScannedDir::new(UsageProviderKind::Antigravity, path_string(&dir), read_directory_volume_id(&dir), files);
                source.status = Some(if failed { "partial" } else { "ok" });
                if failed {
                    source.message = Some("Some Antigravity history could not be read.".to_owned());
                }
                scanned.push(source);
            }
            scanned
        })
        .await
        .unwrap_or_default();
        scanned.extend(sqlite);

        // Cursor: the account's dashboard history, never a local fallback.
        let environment = &options.environment;
        let platform = options.platform;
        let user_home = environment
            .get(if platform == Platform::Win32 { "USERPROFILE" } else { "HOME" })
            .filter(|value| !value.is_empty())
            .map_or_else(|| home.clone(), PathBuf::from);
        let config_home = trimmed_env(environment, "XDG_CONFIG_HOME").map(PathBuf::from).filter(|path| path.is_absolute());
        let cursor_home = match platform {
            Platform::Darwin => user_home.join("Library").join("Application Support"),
            Platform::Win32 => environment
                .get("APPDATA")
                .filter(|value| !value.is_empty())
                .map_or_else(|| user_home.join("AppData").join("Roaming"), PathBuf::from),
            Platform::Linux => config_home.unwrap_or_else(|| user_home.join(".config")),
        };
        let cursor_auth_path = match platform {
            Platform::Darwin => user_home.join(".cursor").join("auth.json"),
            Platform::Win32 => cursor_home.join("Cursor").join("auth.json"),
            Platform::Linux => cursor_home.join("cursor").join("auth.json"),
        };
        let credential_store = environment.get("AGENT_CLI_CREDENTIAL_STORE").map(String::as_str);
        let login_unavailable = trimmed_env(environment, "CURSOR_AUTH_TOKEN").is_some()
            || trimmed_env(environment, "CURSOR_API_KEY").is_some()
            || credential_store == Some("memory");
        let keychain_enabled = settings.get("cursorKeychainUsageEnabled") == Some(&Value::Bool(true));
        if platform == Platform::Darwin && credential_store != Some("file") && !login_unavailable && !keychain_enabled {
            let mut source = ScannedDir::new(UsageProviderKind::Cursor, path_string(&cursor_auth_path), String::new(), None);
            source.message = Some("Cursor account usage is off on this environment.".to_owned());
            source.action = Some("enableCursorKeychain");
            scanned.push(source);
            return scanned;
        }
        let until_ms = self.now();
        let account = if login_unavailable {
            crate::cursor::CursorAccountUsage {
                account_key: None,
                records: Vec::new(),
                missing: true,
                error: Some("Cursor account history needs a Cursor CLI login on this server.".to_owned()),
            }
        } else {
            let credential = if platform == Platform::Darwin && credential_store != Some("file") {
                CredentialSource::Keychain(options.keychain.as_ref())
            } else {
                CredentialSource::File(&cursor_auth_path)
            };
            read_cursor_account_usage(credential, window_start_ms, until_ms, options.cursor_http.as_ref()).await
        };
        // No saved login means there is no account source, not a setup error.
        if account.missing && account.error.is_none() {
            return scanned;
        }
        if let (Some(account_key), None, false) = (&account.account_key, &account.error, account.missing) {
            // The same account carries CLI and desktop history from every machine: a stable
            // remote fingerprint keeps connected environments from counting it twice.
            let source_path = format!("cursor-account:{account_key}");
            let mut source = ScannedDir::new(
                UsageProviderKind::Cursor,
                source_path.clone(),
                account_key.clone(),
                Some(vec![(source_path, account.records)]),
            );
            source.host_id = Some("cursor.com".to_owned());
            source.status = Some("ok");
            scanned.push(source);
            return scanned;
        }
        let mut source = ScannedDir::new(
            UsageProviderKind::Cursor,
            path_string(&cursor_auth_path),
            read_directory_volume_id(&cursor_auth_path),
            None,
        );
        source.message = Some(
            account
                .error
                .unwrap_or_else(|| "Cursor account history needs a Cursor CLI login saved on this server.".to_owned()),
        );
        scanned.push(source);
        scanned
    }
}

#[cfg(test)]
mod tests;
