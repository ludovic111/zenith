//! `project/RepositoryIdentityResolver.ts`: the git identity of a workspace (`rev-parse
//! --show-toplevel` for the root, then `remote -v` for the primary remote), behind two caches.
//!
//! - The root cache maps a `cwd` to its repository root (`null` outside a repository).
//! - The identity cache maps a root to its [`RepositoryIdentity`] (`null` without a remote).
//!
//! Hits live 15 minutes, `null`s one minute (so a folder that gains a repository or a remote
//! shows up quickly); git errors and timeouts count as `null`. `refresh` drops both entries of
//! a `cwd` first (clone, publish, pull request discovery). An optional refiner completes the
//! identity (`server.ts` uses it for Forgejo web URLs, see [`ForgejoIdentityRefiner`]); a
//! failing refinement keeps the plain identity.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::Instant;
use zc_contracts::{LitGitRemote, RepositoryIdentity, RepositoryIdentityLocator};
use zc_core::process::{ProcessRunInput, ProcessRunner, SystemProcessRunner, TimeoutBehavior};

/// `DEFAULT_REPOSITORY_IDENTITY_CACHE_CAPACITY`.
pub const DEFAULT_CACHE_CAPACITY: usize = 512;
/// `DEFAULT_POSITIVE_CACHE_TTL`: background sweeps resolve every project each minute, so hits
/// are kept long enough not to spawn git each time.
pub const DEFAULT_POSITIVE_TTL: Duration = Duration::from_secs(15 * 60);
/// `DEFAULT_NEGATIVE_CACHE_TTL`.
pub const DEFAULT_NEGATIVE_TTL: Duration = Duration::from_secs(60);

/// Completes a resolved identity (`RepositoryIdentityResolverOptions.refine`). An error keeps
/// the identity as it was.
#[async_trait]
pub trait RepositoryIdentityRefiner: Send + Sync {
    async fn refine(&self, identity: RepositoryIdentity) -> Result<RepositoryIdentity, String>;
}

/// `RepositoryIdentityResolverOptions`.
#[derive(Clone)]
pub struct RepositoryIdentityOptions {
    pub cache_capacity: usize,
    pub positive_ttl: Duration,
    pub negative_ttl: Duration,
    pub refine: Option<Arc<dyn RepositoryIdentityRefiner>>,
    /// How git is run (tests script it).
    pub runner: Arc<dyn ProcessRunner>,
}

impl Default for RepositoryIdentityOptions {
    fn default() -> Self {
        Self {
            cache_capacity: DEFAULT_CACHE_CAPACITY,
            positive_ttl: DEFAULT_POSITIVE_TTL,
            negative_ttl: DEFAULT_NEGATIVE_TTL,
            refine: None,
            runner: Arc::new(SystemProcessRunner),
        }
    }
}

struct Entry<V> {
    value: Option<V>,
    expires_at: Instant,
    last_used: u64,
}

/// A capacity-bounded cache whose time to live depends on the value (`Cache.makeWith` with a
/// `timeToLive(exit)`). Expired entries go first when full, then the least recently used.
struct OutcomeCache<V> {
    entries: Mutex<(HashMap<String, Entry<V>>, u64)>,
    capacity: usize,
    positive_ttl: Duration,
    negative_ttl: Duration,
}

impl<V: Clone> OutcomeCache<V> {
    fn new(capacity: usize, positive_ttl: Duration, negative_ttl: Duration) -> Self {
        Self {
            entries: Mutex::new((HashMap::new(), 0)),
            capacity: capacity.max(1),
            positive_ttl,
            negative_ttl,
        }
    }

    fn get(&self, key: &str) -> Option<Option<V>> {
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let (entries, tick) = &mut *guard;
        *tick += 1;
        let now = Instant::now();
        match entries.get_mut(key) {
            Some(entry) if entry.expires_at > now => {
                entry.last_used = *tick;
                Some(entry.value.clone())
            }
            Some(_) => {
                entries.remove(key);
                None
            }
            None => None,
        }
    }

    fn insert(&self, key: String, value: Option<V>) {
        let ttl = if value.is_some() { self.positive_ttl } else { self.negative_ttl };
        let mut guard = self.entries.lock().unwrap_or_else(|p| p.into_inner());
        let (entries, tick) = &mut *guard;
        *tick += 1;
        let now = Instant::now();
        if !entries.contains_key(&key) && entries.len() >= self.capacity {
            entries.retain(|_, entry| entry.expires_at > now);
            if entries.len() >= self.capacity {
                if let Some(oldest) = entries.iter().min_by_key(|(_, entry)| entry.last_used).map(|(key, _)| key.clone()) {
                    entries.remove(&oldest);
                }
            }
        }
        entries.insert(
            key,
            Entry {
                value,
                expires_at: now + ttl,
                last_used: *tick,
            },
        );
    }

    fn invalidate(&self, key: &str) {
        self.entries.lock().unwrap_or_else(|p| p.into_inner()).0.remove(key);
    }
}

struct Inner {
    roots: OutcomeCache<String>,
    identities: OutcomeCache<RepositoryIdentity>,
    refine: Option<Arc<dyn RepositoryIdentityRefiner>>,
    runner: Arc<dyn ProcessRunner>,
}

/// The `RepositoryIdentityResolver` service. Cloning shares the caches.
#[derive(Clone)]
pub struct RepositoryIdentities {
    inner: Arc<Inner>,
}

impl Default for RepositoryIdentities {
    fn default() -> Self {
        Self::new(RepositoryIdentityOptions::default())
    }
}

impl RepositoryIdentities {
    pub fn new(options: RepositoryIdentityOptions) -> Self {
        Self {
            inner: Arc::new(Inner {
                roots: OutcomeCache::new(options.cache_capacity, options.positive_ttl, options.negative_ttl),
                identities: OutcomeCache::new(options.cache_capacity, options.positive_ttl, options.negative_ttl),
                refine: options.refine,
                runner: options.runner,
            }),
        }
    }

    /// `resolve(cwd, {refresh})`.
    pub async fn resolve(&self, cwd: &str, refresh: bool) -> Option<RepositoryIdentity> {
        let inner = &self.inner;
        if refresh {
            inner.roots.invalidate(cwd);
        }
        let root = match inner.roots.get(cwd) {
            Some(root) => root,
            None => {
                let root = self.resolve_root(cwd).await;
                inner.roots.insert(cwd.to_owned(), root.clone());
                root
            }
        }?;
        if refresh {
            inner.identities.invalidate(&root);
        }
        if let Some(identity) = inner.identities.get(&root) {
            return identity;
        }
        let mut identity = self.resolve_identity(&root).await;
        if let (Some(plain), Some(refine)) = (identity.clone(), inner.refine.as_ref()) {
            identity = Some(refine.refine(plain.clone()).await.unwrap_or(plain));
        }
        inner.identities.insert(root, identity.clone());
        identity
    }

    async fn git(&self, args: [&str; 4]) -> Option<String> {
        // git is a real executable on every platform: no shell, so paths with spaces stay whole.
        let mut input = ProcessRunInput::new("git", args);
        input.timeout_behavior = TimeoutBehavior::TimedOutResult;
        match self.inner.runner.run(input).await {
            Ok(output) if output.code == Some(0) => Some(output.stdout),
            _ => None,
        }
    }

    /// `resolveRepositoryIdentityCacheKey`.
    async fn resolve_root(&self, cwd: &str) -> Option<String> {
        let stdout = self.git(["-C", cwd, "rev-parse", "--show-toplevel"]).await?;
        let candidate = stdout.trim();
        (!candidate.is_empty()).then(|| candidate.to_owned())
    }

    /// `resolveRepositoryIdentityFromCacheKey`.
    async fn resolve_identity(&self, root: &str) -> Option<RepositoryIdentity> {
        let stdout = self.git(["-C", root, "remote", "-v"]).await?;
        let (remote_name, remote_url) = pick_primary_remote(&parse_remote_fetch_urls(&stdout))?;
        Some(build_repository_identity(&remote_name, &remote_url, root))
    }
}

#[async_trait]
impl zc_projections::RepositoryIdentityResolver for RepositoryIdentities {
    async fn resolve(&self, cwd: &str) -> Option<RepositoryIdentity> {
        RepositoryIdentities::resolve(self, cwd, false).await
    }

    async fn refresh(&self, cwd: &str) -> Option<RepositoryIdentity> {
        RepositoryIdentities::resolve(self, cwd, true).await
    }
}

/// `parseRemoteFetchUrls`: `name → fetch url`, in first-seen order (a later line for the same
/// remote replaces its url).
pub fn parse_remote_fetch_urls(stdout: &str) -> Vec<(String, String)> {
    static LINE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let line_pattern = LINE.get_or_init(|| regex::Regex::new(r"^(\S+)\s+(\S+)\s+\((fetch|push)\)$").expect("valid regex"));
    let mut remotes: Vec<(String, String)> = Vec::new();
    for line in stdout.split('\n') {
        let trimmed = line.trim();
        let Some(captures) = line_pattern.captures(trimmed) else {
            continue;
        };
        let (name, url, direction) = (&captures[1], &captures[2], &captures[3]);
        if direction != "fetch" {
            continue;
        }
        match remotes.iter_mut().find(|(existing, _)| existing == name) {
            Some(entry) => entry.1 = url.to_owned(),
            None => remotes.push((name.to_owned(), url.to_owned())),
        }
    }
    remotes
}

/// `pickPrimaryRemote`: `upstream`, then `origin`, then the first by name.
pub fn pick_primary_remote(remotes: &[(String, String)]) -> Option<(String, String)> {
    for preferred in ["upstream", "origin"] {
        if let Some((name, url)) = remotes.iter().find(|(name, url)| name == preferred && !url.is_empty()) {
            return Some((name.clone(), url.clone()));
        }
    }
    let mut sorted: Vec<&(String, String)> = remotes.iter().collect();
    sorted.sort_by(|left, right| zc_db::collate::locale_compare(&left.0, &right.0));
    sorted
        .first()
        .filter(|(name, url)| !name.is_empty() && !url.is_empty())
        .map(|(name, url)| (name.clone(), url.clone()))
}

/// `buildRepositoryIdentity`.
pub fn build_repository_identity(remote_name: &str, remote_url: &str, root_path: &str) -> RepositoryIdentity {
    let canonical_key = zc_vcs::shared_git::normalize_git_remote_url(remote_url);
    let provider = zc_vcs::shared_git::detect_source_control_provider_from_remote_url(remote_url)
        .and_then(|info| serde_json::to_value(info.kind).ok())
        .and_then(|kind| kind.as_str().map(str::to_owned));
    let repository_path = canonical_key.split('/').skip(1).collect::<Vec<_>>().join("/");
    let segments: Vec<&str> = repository_path.split('/').filter(|segment| !segment.is_empty()).collect();
    RepositoryIdentity {
        canonical_key: canonical_key.clone(),
        locator: RepositoryIdentityLocator {
            source: LitGitRemote,
            remote_name: remote_name.to_owned(),
            remote_url: remote_url.to_owned(),
        },
        web_url: None,
        root_path: Some(root_path.to_owned()),
        display_name: (!repository_path.is_empty()).then(|| repository_path.clone()),
        provider,
        owner: segments.first().map(|s| (*s).to_owned()),
        name: segments.last().map(|s| (*s).to_owned()),
    }
}

/// The Forgejo refinement of `server.ts` (`RepositoryIdentityResolverLayerLive`): a remote that
/// parses as Forgejo and is not already attributed to another forge is checked against the
/// configured `fj` logins; on a match the identity gets `provider: "forgejo"` and the web URL
/// under the login's base URL.
pub struct ForgejoIdentityRefiner(pub zc_sourcecontrol::SourceControlProviderRegistry);

#[async_trait]
impl RepositoryIdentityRefiner for ForgejoIdentityRefiner {
    async fn refine(&self, identity: RepositoryIdentity) -> Result<RepositoryIdentity, String> {
        use zc_contracts::{SourceControlProviderInfo, SourceControlProviderKind};
        let Some(remote) = zc_sourcecontrol::forgejo::cli::parse_forgejo_remote(&identity.locator.remote_url) else {
            return Ok(identity);
        };
        let Some(root) = identity.root_path.clone().filter(|root| !root.is_empty()) else {
            return Ok(identity);
        };
        if identity
            .provider
            .as_deref()
            .is_some_and(|provider| provider != "unknown" && provider != "forgejo")
        {
            return Ok(identity);
        }
        let context = zc_sourcecontrol::SourceControlProviderContext {
            provider: SourceControlProviderInfo {
                kind: SourceControlProviderKind::Unknown,
                name: "Unknown".into(),
                base_url: String::new(),
            },
            remote_name: identity.locator.remote_name.clone(),
            remote_url: identity.locator.remote_url.clone(),
            requested_host: None,
        };
        let handle = self.0.resolve_handle(&root, Some(context)).await.map_err(|error| format!("{error:?}"))?;
        let Some(context) = handle.context.filter(|context| context.provider.kind == SourceControlProviderKind::Forgejo) else {
            return Ok(identity);
        };
        let base_url = context.provider.base_url.trim_end_matches('/').to_owned();
        let Ok(parsed) = url::Url::parse(&base_url) else {
            return Ok(identity);
        };
        let base_path = parsed.path().trim_matches('/').to_owned();
        let prefix = format!("{base_path}/");
        let path = if !remote.ssh && !base_path.is_empty() && remote.path.starts_with(&prefix) {
            remote.path[prefix.len()..].to_owned()
        } else {
            remote.path.clone()
        };
        Ok(RepositoryIdentity {
            provider: Some("forgejo".into()),
            web_url: Some(format!("{base_url}/{path}")),
            ..identity
        })
    }
}
