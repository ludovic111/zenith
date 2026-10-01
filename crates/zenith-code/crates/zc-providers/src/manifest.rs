//! Port of `provider/ModelManifest.ts` (+ the manifest checks of `ClaudeModelManifest.ts`):
//! remote provider-model metadata with a bundled offline fallback.
//!
//! Preference order: the remote file on `main`, then the last successful on-disk copy
//! (`<state>/model-manifest.json`, `{fetchedAtMs, manifest}`), then the bundle compiled in from
//! `code/apps/server/src/provider/model-manifest.json`. A disk or remote copy whose `updatedAt`
//! is older than the bundle's never outranks it. Fetches are TTL-gated (1 h fresh, 5 min retry
//! after a failure, 10 s timeout) and skipped when `enableProviderUpdateChecks` is off. A failed
//! fetch never fails a provider check.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zc_contracts::{LitNew, ModelCapabilities, ServerProviderModel};

use crate::compatibility::ProviderCompatibilityPolicy;
use crate::semver::{compare_semver_versions, parse_semver};

pub const MODEL_MANIFEST_URL: &str = "https://raw.githubusercontent.com/pingdotgg/t3code/main/apps/server/src/provider/model-manifest.json";
/// How long a fetched manifest stays fresh.
pub const MANIFEST_TTL_MS: i64 = 60 * 60 * 1000;
/// Minimum gap between fetch attempts after a failure.
pub const MANIFEST_RETRY_MS: i64 = 5 * 60 * 1000;
pub const FETCH_TIMEOUT_MS: u64 = 10_000;

/// The bundled `model-manifest.json`, as shipped with the TS server.
pub const BUNDLED_MODEL_MANIFEST_JSON: &str = include_str!("../../../../../code/apps/server/src/provider/model-manifest.json");

/// `ManifestModelStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ManifestModelStatus {
    Current,
    Legacy,
}

/// `ManifestModelProfile`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ManifestModelProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ModelCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<Value>,
}

/// `ManifestProviderModel`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManifestProviderModel {
    pub slug: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub short_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    pub status: ManifestModelStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub badge: Option<LitNew>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<Value>,
}

/// `defaults` of a catalog.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ManifestCatalogDefaults {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat: Option<String>,
}

/// `ManifestProviderCatalog`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ManifestProviderCatalog {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults: Option<ManifestCatalogDefaults>,
    pub profiles: BTreeMap<String, ManifestModelProfile>,
    pub models: Vec<ManifestProviderModel>,
}

/// `ModelManifestData` (version 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelManifestData {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compatibility: Option<Vec<ProviderCompatibilityPolicy>>,
    pub current_models: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<BTreeMap<String, ManifestProviderCatalog>>,
}

impl ModelManifestData {
    /// An empty v1 manifest (tests).
    pub fn empty() -> Self {
        Self {
            version: 1,
            updated_at: None,
            compatibility: None,
            current_models: BTreeMap::new(),
            providers: None,
        }
    }

    /// Decode and run every schema check of `ModelManifestSchema`.
    pub fn decode(value: Value) -> Result<Self, String> {
        let manifest: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// The refinements of `ModelManifestSchema`.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != 1 {
            return Err(format!("Expected version 1, got {}", self.version));
        }
        let non_empty = |value: &str| !value.trim().is_empty();
        for catalog in self.providers.iter().flat_map(|providers| providers.values()) {
            for model in &catalog.models {
                if !non_empty(&model.slug) || !non_empty(&model.name) {
                    return Err("model slug and name must be non-empty".into());
                }
            }
        }
        if let Some(policies) = &self.compatibility {
            if let Some(policy) = policies.iter().find(|policy| !policy.is_valid()) {
                return Err(format!("invalid compatibility policy for '{}'", policy.driver));
            }
        }
        if !has_valid_provider_catalog_references(self) {
            return Err("Expected unique model slugs and existing model and profile references".into());
        }
        if !has_valid_claude_manifest_adapters(self) {
            return Err("Expected valid Claude adapter metadata".into());
        }
        Ok(())
    }

    /// Epoch millis of `updatedAt`, or 0 when absent or unparsable.
    pub fn updated_at_ms(&self) -> i64 {
        self.updated_at.as_deref().and_then(zc_core::time::parse_iso_millis).unwrap_or(0)
    }
}

fn has_valid_provider_catalog_references(manifest: &ModelManifestData) -> bool {
    manifest.providers.iter().flat_map(|providers| providers.values()).all(|catalog| {
        let mut slugs = HashSet::new();
        let models_valid = catalog.models.iter().all(|model| {
            if !slugs.insert(model.slug.as_str()) {
                return false;
            }
            model.profile.as_ref().is_none_or(|profile| catalog.profiles.contains_key(profile))
        });
        models_valid
            && catalog
                .defaults
                .as_ref()
                .and_then(|defaults| defaults.chat.as_ref())
                .is_none_or(|chat| slugs.contains(chat.as_str()))
    })
}

fn is_record_of(value: &Value, check: impl Fn(&Value) -> bool) -> bool {
    value
        .as_object()
        .is_some_and(|object| object.iter().all(|(key, value)| !key.trim().is_empty() && check(value)))
}

/// `ClaudeProfileAdapterSchema` decode.
fn is_valid_claude_profile_adapter(adapter: &Value) -> bool {
    let Some(object) = adapter.as_object() else {
        return false;
    };
    let Some(claude_code) = object.get("claudeCode") else {
        return true;
    };
    let Some(claude_code) = claude_code.as_object() else {
        return false;
    };
    let string = |value: &Value| value.as_str().is_some_and(|text| !text.trim().is_empty());
    claude_code
        .get("effortMap")
        .is_none_or(|map| is_record_of(map, |value| value.is_null() || string(value)))
        && claude_code
            .get("modelSuffixes")
            .is_none_or(|map| is_record_of(map, |inner| is_record_of(inner, string)))
        && claude_code.get("contextWindowTokens").is_none_or(|map| is_record_of(map, Value::is_number))
        && claude_code.get("fixedContextWindowTokens").is_none_or(Value::is_number)
}

/// `ClaudeModelAdapterSchema` decode (`minVersion` < `maxVersionExclusive`, both semver).
fn is_valid_claude_model_adapter(adapter: &Value) -> bool {
    let Some(object) = adapter.as_object() else {
        return false;
    };
    let Some(claude_code) = object.get("claudeCode") else {
        return true;
    };
    let Some(claude_code) = claude_code.as_object() else {
        return false;
    };
    let version = |key: &str| -> Result<Option<String>, ()> {
        match claude_code.get(key) {
            None => Ok(None),
            Some(Value::String(text)) if !text.trim().is_empty() && parse_semver(text).is_some() => Ok(Some(text.trim().to_owned())),
            Some(_) => Err(()),
        }
    };
    let (Ok(min), Ok(max)) = (version("minVersion"), version("maxVersionExclusive")) else {
        return false;
    };
    match (min, max) {
        (Some(min), Some(max)) => compare_semver_versions(&min, &max) == std::cmp::Ordering::Less,
        _ => true,
    }
}

/// `hasValidClaudeManifestAdapters`.
pub fn has_valid_claude_manifest_adapters(manifest: &ModelManifestData) -> bool {
    let Some(catalog) = manifest.providers.as_ref().and_then(|providers| providers.get("claudeAgent")) else {
        return true;
    };
    let empty = Value::Object(Default::default());
    catalog
        .profiles
        .values()
        .all(|profile| is_valid_claude_profile_adapter(profile.adapter.as_ref().unwrap_or(&empty)))
        && catalog
            .models
            .iter()
            .all(|model| is_valid_claude_model_adapter(model.adapter.as_ref().unwrap_or(&empty)))
}

/// The bundled manifest (decoded once; an invalid bundle is a build error in TS too).
pub fn bundled_model_manifest() -> &'static ModelManifestData {
    static BUNDLED: OnceLock<ModelManifestData> = OnceLock::new();
    BUNDLED.get_or_init(|| {
        let value: Value = serde_json::from_str(BUNDLED_MODEL_MANIFEST_JSON).expect("bundled model-manifest.json is JSON");
        ModelManifestData::decode(value).expect("bundled model-manifest.json is a valid manifest")
    })
}

/// `ResolvedManifestModel`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedManifestModel {
    pub model: ServerProviderModel,
    pub adapter: Option<Value>,
    pub profile_adapter: Option<Value>,
}

/// `ResolvedProviderCatalog`.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedProviderCatalog {
    pub models: Vec<ResolvedManifestModel>,
    pub default_chat: Option<String>,
}

/// `resolveProviderCatalog(manifest, driverKind)`: provider-neutral presentation and
/// capabilities, `None` when the catalog is absent or inconsistent.
pub fn resolve_provider_catalog(manifest: &ModelManifestData, driver_kind: &str) -> Option<ResolvedProviderCatalog> {
    let catalog = manifest.providers.as_ref()?.get(driver_kind)?;
    let default_chat = catalog.defaults.as_ref().and_then(|defaults| defaults.chat.clone());
    let mut seen = HashSet::new();
    let mut models = Vec::new();
    for entry in &catalog.models {
        if !seen.insert(entry.slug.clone()) {
            return None;
        }
        let profile = match &entry.profile {
            Some(name) => Some(catalog.profiles.get(name)?),
            None => None,
        };
        models.push(ResolvedManifestModel {
            model: ServerProviderModel {
                slug: entry.slug.clone(),
                name: entry.name.clone(),
                short_name: entry.short_name.clone(),
                sub_provider: entry.sub_provider.clone(),
                aliases: entry.aliases.clone(),
                badge: entry.badge,
                is_custom: false,
                is_default: (default_chat.as_deref() == Some(entry.slug.as_str())).then_some(true),
                is_legacy: (entry.status == ManifestModelStatus::Legacy).then_some(true),
                capabilities: profile.and_then(|profile| profile.capabilities.clone()),
            },
            adapter: entry.adapter.clone(),
            profile_adapter: profile.and_then(|profile| profile.adapter.clone()),
        });
    }
    if let Some(chat) = &default_chat {
        if !seen.contains(chat) {
            return None;
        }
    }
    Some(ResolvedProviderCatalog { models, default_chat })
}

/// shared `codexModelFamily`: `openai.gpt-…` → `gpt-…`.
pub fn codex_model_family(slug: &str) -> &str {
    if slug.starts_with("openai.gpt-") {
        &slug["openai.".len()..]
    } else {
        slug
    }
}

fn is_legacy_model(manifest: &ModelManifestData, driver_kind: &str, slug: &str) -> bool {
    let family = if driver_kind == "codex" { codex_model_family(slug) } else { slug };
    let catalog = manifest
        .providers
        .as_ref()
        .and_then(|providers| providers.get(driver_kind))
        .map(|catalog| &catalog.models);
    let catalog_model = catalog.and_then(|models| {
        models
            .iter()
            .find(|model| model.slug == slug)
            .or_else(|| models.iter().find(|model| model.slug == family))
    });
    if let Some(model) = catalog_model {
        return model.status == ManifestModelStatus::Legacy;
    }
    let Some(current) = manifest.current_models.get(driver_kind) else {
        return false;
    };
    !current.iter().any(|entry| entry == slug) && !current.iter().any(|entry| entry == family)
}

/// `classifyModels`: flag non-current built-in models as legacy, clear stale flags, never touch
/// custom models.
pub fn classify_models(models: &[ServerProviderModel], manifest: &ModelManifestData, driver_kind: &str) -> Vec<ServerProviderModel> {
    models
        .iter()
        .map(|model| {
            if model.is_custom {
                return model.clone();
            }
            let mut next = model.clone();
            if is_legacy_model(manifest, driver_kind, &model.slug) {
                if next.is_legacy.is_none() || next.is_legacy == Some(false) {
                    next.is_legacy = Some(true);
                }
                return next;
            }
            if model.is_legacy != Some(true) {
                return next;
            }
            next.is_legacy = None;
            next
        })
        .collect()
}

/// `manifestDefaultModel`.
pub fn manifest_default_model<'a>(manifest: &'a ModelManifestData, driver_kind: &str) -> Option<&'a str> {
    manifest.providers.as_ref()?.get(driver_kind)?.defaults.as_ref()?.chat.as_deref()
}

/// `applyManifestDefault`: move `isDefault` (and the aliases that pointed at the old default)
/// to the manifest's chat default when the account offers it.
pub fn apply_manifest_default(models: &[ServerProviderModel], manifest: &ModelManifestData, driver_kind: &str) -> Vec<ServerProviderModel> {
    let Some(requested) = manifest_default_model(manifest, driver_kind) else {
        return models.to_vec();
    };
    let slug = models.iter().find(|model| model.slug == requested).map(|model| model.slug.clone()).or_else(|| {
        (driver_kind == "codex")
            .then(|| {
                models
                    .iter()
                    .find(|model| !model.is_custom && codex_model_family(&model.slug) == codex_model_family(requested))
                    .map(|model| model.slug.clone())
            })
            .flatten()
    });
    let Some(slug) = slug else {
        return models.to_vec();
    };
    let Some(previous) = models.iter().find(|model| model.is_default == Some(true) && model.slug != slug) else {
        return models.to_vec();
    };
    let previous_slug = previous.slug.clone();
    let moved_aliases = previous.aliases.clone().unwrap_or_default();
    models
        .iter()
        .map(|model| {
            let mut next = model.clone();
            if model.slug == previous_slug {
                next.is_default = None;
                next.aliases = None;
            } else if model.slug == slug {
                let mut aliases: Vec<String> = Vec::new();
                for alias in model.aliases.iter().flatten().chain(moved_aliases.iter()) {
                    if !aliases.contains(alias) {
                        aliases.push(alias.clone());
                    }
                }
                next.is_default = Some(true);
                if !aliases.is_empty() {
                    next.aliases = Some(aliases);
                }
            }
            next
        })
        .collect()
}

/// `applyModelManifest`: classify, then apply the manifest default.
pub fn apply_model_manifest(models: &[ServerProviderModel], manifest: &ModelManifestData, driver_kind: &str) -> Vec<ServerProviderModel> {
    apply_manifest_default(&classify_models(models, manifest, driver_kind), manifest, driver_kind)
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

/// Fetches the remote manifest JSON (`GET`, status 2xx, parsed body). The default is
/// [`HttpManifestFetcher`]; tests script it.
#[async_trait]
pub trait ManifestFetcher: Send + Sync {
    async fn fetch(&self, url: &str) -> Result<Value, String>;
}

/// `HttpClient.get(url)` + `filterStatusOk` + `response.json`.
pub struct HttpManifestFetcher {
    client: reqwest::Client,
}

impl Default for HttpManifestFetcher {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder().build().unwrap_or_default(),
        }
    }
}

#[async_trait]
impl ManifestFetcher for HttpManifestFetcher {
    async fn fetch(&self, url: &str) -> Result<Value, String> {
        let response = self.client.get(url).send().await.map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status()));
        }
        response.json::<Value>().await.map_err(|error| error.to_string())
    }
}

/// Whether `enableProviderUpdateChecks` allows phoning home (read per refresh).
pub type UpdateChecksEnabled = Arc<dyn Fn() -> futures::future::BoxFuture<'static, bool> + Send + Sync>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestCacheFile {
    fetched_at_ms: i64,
    manifest: ModelManifestData,
}

/// `encodeManifestCache`: the disk cache file contents.
pub fn encode_manifest_cache(fetched_at_ms: i64, manifest: &ModelManifestData) -> String {
    serde_json::to_string(&ManifestCacheFile {
        fetched_at_ms,
        manifest: manifest.clone(),
    })
    .unwrap_or_default()
}

struct ManifestState {
    manifest: Arc<ModelManifestData>,
    fetched_at_ms: Option<i64>,
    last_attempt_ms: Option<i64>,
}

struct ManifestInner {
    cache_path: Option<PathBuf>,
    fetcher: Arc<dyn ManifestFetcher>,
    update_checks_enabled: Option<UpdateChecksEnabled>,
    clock: Arc<dyn Fn() -> i64 + Send + Sync>,
    state: Mutex<ManifestState>,
    disk_loaded: tokio::sync::OnceCell<()>,
    refresh_lock: tokio::sync::Mutex<()>,
}

/// The `ModelManifest` service.
#[derive(Clone)]
pub struct ModelManifest {
    inner: Arc<ManifestInner>,
}

/// How to build a [`ModelManifest`].
pub struct ModelManifestOptions {
    /// `<stateDir>/model-manifest.json`; `None` keeps everything in memory.
    pub cache_path: Option<PathBuf>,
    pub fetcher: Arc<dyn ManifestFetcher>,
    /// `None` means update checks are on.
    pub update_checks_enabled: Option<UpdateChecksEnabled>,
    pub clock: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl ModelManifestOptions {
    pub fn new(cache_path: Option<PathBuf>) -> Self {
        Self {
            cache_path,
            fetcher: Arc::new(HttpManifestFetcher::default()),
            update_checks_enabled: None,
            clock: Arc::new(zc_core::now_millis),
        }
    }

    /// Read `enableProviderUpdateChecks` from the settings port.
    pub fn with_settings(mut self, settings: Arc<dyn zc_ports::SettingsService>) -> Self {
        self.update_checks_enabled = Some(Arc::new(move || {
            let settings = settings.clone();
            Box::pin(async move {
                match settings.get_settings().await {
                    Ok(value) => serde_json::to_value(&value)
                        .unwrap_or_default()
                        .get("enableProviderUpdateChecks")
                        .and_then(Value::as_bool)
                        .unwrap_or(true),
                    Err(_) => true,
                }
            })
        }));
        self
    }
}

impl ModelManifest {
    pub fn new(options: ModelManifestOptions) -> Self {
        Self {
            inner: Arc::new(ManifestInner {
                cache_path: options.cache_path,
                fetcher: options.fetcher,
                update_checks_enabled: options.update_checks_enabled,
                clock: options.clock,
                state: Mutex::new(ManifestState {
                    manifest: Arc::new(bundled_model_manifest().clone()),
                    fetched_at_ms: None,
                    last_attempt_ms: None,
                }),
                disk_loaded: tokio::sync::OnceCell::new(),
                refresh_lock: tokio::sync::Mutex::new(()),
            }),
        }
    }

    /// `layerTest`: the bundle, never fetching.
    pub fn bundled_only() -> Self {
        struct Never;
        #[async_trait]
        impl ManifestFetcher for Never {
            async fn fetch(&self, _url: &str) -> Result<Value, String> {
                Err("fetching is disabled".into())
            }
        }
        let mut options = ModelManifestOptions::new(None);
        options.fetcher = Arc::new(Never);
        options.update_checks_enabled = Some(Arc::new(|| Box::pin(async { false })));
        Self::new(options)
    }

    async fn ensure_disk_cache_loaded(&self) {
        self.inner
            .disk_loaded
            .get_or_init(|| async {
                let Some(path) = &self.inner.cache_path else {
                    return;
                };
                let Ok(raw) = tokio::fs::read_to_string(path).await else {
                    return;
                };
                let Ok(value) = serde_json::from_str::<Value>(&raw) else {
                    return;
                };
                let Some(fetched_at_ms) = value.get("fetchedAtMs").and_then(Value::as_f64) else {
                    return;
                };
                let Some(manifest) = value.get("manifest").cloned().and_then(|manifest| ModelManifestData::decode(manifest).ok()) else {
                    return;
                };
                if bundled_model_manifest().updated_at_ms() > manifest.updated_at_ms() {
                    return;
                }
                let mut state = self.inner.state.lock().unwrap();
                state.manifest = Arc::new(manifest);
                state.fetched_at_ms = Some(fetched_at_ms as i64);
            })
            .await;
    }

    /// `current`: the manifest in memory (disk cache or bundle); never fetches.
    pub async fn current(&self) -> Arc<ModelManifestData> {
        self.ensure_disk_cache_loaded().await;
        self.inner.state.lock().unwrap().manifest.clone()
    }

    /// `refresh`: TTL-gated remote refresh; never fails.
    pub async fn refresh(&self) -> Arc<ModelManifestData> {
        let _guard = self.inner.refresh_lock.lock().await;
        self.refresh_locked(false).await
    }

    /// `forceRefresh`: bypass the freshness and retry timers, keeping last-good data.
    pub async fn force_refresh(&self) -> Arc<ModelManifestData> {
        let _guard = self.inner.refresh_lock.lock().await;
        self.refresh_locked(true).await
    }

    /// `refreshInBackground`: fork a refresh that outlives the caller.
    pub fn refresh_in_background(&self) {
        let this = self.clone();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                this.refresh().await;
            });
        }
    }

    async fn refresh_locked(&self, force: bool) -> Arc<ModelManifestData> {
        self.ensure_disk_cache_loaded().await;
        let now = (self.inner.clock)();
        let is_within = |since: Option<i64>, window: i64| since.is_some_and(|since| now >= since && now - since < window);
        let current = {
            let state = self.inner.state.lock().unwrap();
            if !force && (is_within(state.fetched_at_ms, MANIFEST_TTL_MS) || is_within(state.last_attempt_ms, MANIFEST_RETRY_MS)) {
                return state.manifest.clone();
            }
            state.manifest.clone()
        };
        if let Some(enabled) = &self.inner.update_checks_enabled {
            if !enabled().await {
                return current;
            }
        }
        self.inner.state.lock().unwrap().last_attempt_ms = Some(now);
        let fetched = tokio::time::timeout(Duration::from_millis(FETCH_TIMEOUT_MS), self.inner.fetcher.fetch(MODEL_MANIFEST_URL))
            .await
            .ok()
            .and_then(Result::ok)
            .and_then(|value| ModelManifestData::decode(value).ok());
        let Some(fetched) = fetched else {
            return current;
        };
        if fetched.updated_at_ms() < current.updated_at_ms() {
            return current;
        }
        let fetched = Arc::new(fetched);
        {
            let mut state = self.inner.state.lock().unwrap();
            state.manifest = fetched.clone();
            state.fetched_at_ms = Some(now);
        }
        if let Some(path) = &self.inner.cache_path {
            let _ = tokio::fs::write(path, encode_manifest_cache(now, &fetched)).await;
        }
        fetched
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn model(slug: &str) -> ServerProviderModel {
        ServerProviderModel {
            slug: slug.into(),
            name: "GPT Test".into(),
            short_name: None,
            sub_provider: None,
            aliases: None,
            badge: None,
            is_custom: false,
            is_default: None,
            is_legacy: None,
            capabilities: None,
        }
    }

    #[test]
    fn the_bundled_manifest_decodes() {
        let bundled = bundled_model_manifest();
        assert_eq!(bundled.version, 1);
        assert!(bundled.updated_at_ms() > 0);
        let policies = bundled.compatibility.as_ref().unwrap();
        for driver in ["codex", "claudeAgent", "cursor", "grok", "opencode", "antigravity"] {
            assert!(policies.iter().any(|policy| policy.driver == driver), "{driver} has a compatibility policy");
        }
        assert!(policies.iter().all(ProviderCompatibilityPolicy::is_valid));
        for (driver, catalog) in bundled.providers.as_ref().unwrap() {
            let resolved = resolve_provider_catalog(bundled, driver).unwrap();
            assert_eq!(resolved.models.len(), catalog.models.len());
        }
        // Round trip through the disk cache format.
        let encoded = encode_manifest_cache(1, bundled);
        let decoded: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(&ModelManifestData::decode(decoded["manifest"].clone()).unwrap(), bundled);
    }

    #[test]
    fn classifies_qualified_codex_families() {
        let mut manifest = ModelManifestData::empty();
        manifest.current_models.insert("codex".into(), vec!["gpt-test".into()]);
        let mut legacy_flagged = model("openai.gpt-test");
        legacy_flagged.is_legacy = Some(true);
        let classified = classify_models(&[legacy_flagged, model("openai.gpt-old")], &manifest, "codex");
        assert_eq!(classified[0].is_legacy, None);
        assert_eq!(classified[1].is_legacy, Some(true));
    }

    #[test]
    fn flags_non_current_and_skips_custom() {
        let mut manifest = ModelManifestData::empty();
        manifest.current_models.insert("codex".into(), vec!["current-a".into(), "current-b".into()]);
        let mut stale = model("current-b");
        stale.is_legacy = Some(true);
        let mut custom = model("my-own-model");
        custom.is_custom = true;
        let classified = classify_models(&[model("current-a"), stale, model("old-model"), custom], &manifest, "codex");
        let flags: Vec<bool> = classified.iter().map(|m| m.is_legacy.unwrap_or(false)).collect();
        assert_eq!(flags, vec![false, false, true, false]);
    }

    #[test]
    fn moves_the_default_and_its_aliases() {
        let manifest = ModelManifestData::decode(json!({
            "version": 1,
            "currentModels": {},
            "providers": {"antigravity": {"defaults": {"chat": "gemini-new"}, "profiles": {}, "models": [{"slug": "gemini-new", "name": "New", "status": "current"}]}}
        }))
        .unwrap();
        let mut old = model("gemini-old");
        old.is_default = Some(true);
        old.aliases = Some(vec!["antigravity-default".into()]);
        let models = vec![old.clone(), model("gemini-new")];
        let applied = apply_manifest_default(&models, &manifest, "antigravity");
        assert_eq!(applied[0], model("gemini-old"));
        let mut expected = model("gemini-new");
        expected.is_default = Some(true);
        expected.aliases = Some(vec!["antigravity-default".into()]);
        assert_eq!(applied[1], expected);
        assert_eq!(apply_manifest_default(&models[..1], &manifest, "antigravity"), vec![old]);

        // The manifest default resolves to the qualified live Codex model.
        let codex = ModelManifestData {
            providers: Some(BTreeMap::from([(
                "codex".to_owned(),
                ManifestProviderCatalog {
                    defaults: Some(ManifestCatalogDefaults { chat: Some("gpt-test".into()) }),
                    profiles: BTreeMap::new(),
                    models: Vec::new(),
                },
            )])),
            ..ModelManifestData::empty()
        };
        let mut old_default = model("openai.gpt-old");
        old_default.is_default = Some(true);
        let applied = apply_manifest_default(&[old_default, model("openai.gpt-test")], &codex, "codex");
        assert_eq!(
            applied.iter().find(|m| m.is_default == Some(true)).map(|m| m.slug.as_str()),
            Some("openai.gpt-test")
        );
        // Built directly it applies; decoded, a default naming no catalog model is invalid.
        assert!(codex.validate().is_err());
    }

    #[test]
    fn resolves_presentation_through_profiles_and_rejects_bad_references() {
        let manifest = ModelManifestData::decode(json!({
            "version": 1,
            "currentModels": {},
            "providers": {"synthetic": {
                "defaults": {"chat": "model-next"},
                "profiles": {"standard": {
                    "capabilities": {"optionDescriptors": [{"id": "mode", "label": "Mode", "type": "select", "options": [{"id": "fast", "label": "Fast", "isDefault": true}]}]},
                    "adapter": {"opaque": true}
                }},
                "models": [{"slug": "model-next", "name": "Model Next", "aliases": ["next"], "status": "current", "badge": "new", "profile": "standard"}]
            }}
        }))
        .unwrap();
        let catalog = resolve_provider_catalog(&manifest, "synthetic").unwrap();
        let resolved = &catalog.models[0];
        assert_eq!(
            serde_json::to_value(&resolved.model).unwrap(),
            json!({
                "slug": "model-next", "name": "Model Next", "aliases": ["next"], "badge": "new", "isCustom": false, "isDefault": true,
                "capabilities": {"optionDescriptors": [{"id": "mode", "label": "Mode", "type": "select", "options": [{"id": "fast", "label": "Fast", "isDefault": true}]}]}
            })
        );
        assert_eq!(resolved.adapter, None);
        assert_eq!(resolved.profile_adapter, Some(json!({"opaque": true})));

        for invalid in [
            json!([{"slug": "duplicate", "name": "First", "status": "current"}, {"slug": "duplicate", "name": "Second", "status": "current"}]),
            json!([{"slug": "missing-profile", "name": "Missing", "status": "current", "profile": "missing"}]),
        ] {
            let manifest = ModelManifestData {
                providers: Some(BTreeMap::from([(
                    "synthetic".to_owned(),
                    ManifestProviderCatalog {
                        defaults: None,
                        profiles: BTreeMap::new(),
                        models: serde_json::from_value(invalid).unwrap(),
                    },
                )])),
                ..ModelManifestData::empty()
            };
            assert!(resolve_provider_catalog(&manifest, "synthetic").is_none());
            assert!(manifest.validate().is_err());
        }
    }

    #[test]
    fn rejects_invalid_claude_adapters() {
        let base = json!({
            "version": 1, "updatedAt": "2099-01-01T00:00:00Z", "currentModels": {},
            "providers": {"claudeAgent": {
                "profiles": {"synthetic": {"adapter": {"claudeCode": {"effortMap": {"extreme": "high"}}}}},
                "models": [{"slug": "remote-only-model", "name": "Remote Only Model", "status": "current", "profile": "synthetic"}]
            }}
        });
        assert!(ModelManifestData::decode(base.clone()).is_ok());
        let mut bad_effort = base.clone();
        bad_effort["providers"]["claudeAgent"]["profiles"]["synthetic"]["adapter"]["claudeCode"]["effortMap"]["extreme"] = json!(123);
        assert!(ModelManifestData::decode(bad_effort).is_err());
        for compat in [
            json!({"minVersion": "2.x"}),
            json!({"maxVersionExclusive": "2.x"}),
            json!({"minVersion": "2.2", "maxVersionExclusive": "2.1"}),
        ] {
            let mut bad = base.clone();
            bad["providers"]["claudeAgent"]["models"][0]["adapter"] = json!({"claudeCode": compat});
            assert!(ModelManifestData::decode(bad).is_err());
        }
        assert!(ModelManifestData::decode(json!({"version": 999, "nonsense": true})).is_err());
    }
}
