//! Port of `provider/providerStatusCache.ts`: one `<cacheDir>/<instanceId>.json` per configured
//! instance (`caches/codex.json` for the default Codex instance, as before the instance split),
//! read at boot so the UI has something to show during the first probe.

use std::path::{Path, PathBuf};

use zc_contracts::{ServerProvider, ServerProviderModel};

/// Built-in drivers in presentation order; unknown and fork drivers sort after them.
const BUILT_IN_DRIVER_ORDER: &[&str] = &["codex", "claudeAgent", "cursor", "grok", "opencode", "antigravity"];

fn driver_rank(driver: &str) -> usize {
    BUILT_IN_DRIVER_ORDER
        .iter()
        .position(|candidate| *candidate == driver)
        .unwrap_or(BUILT_IN_DRIVER_ORDER.len())
}

/// `orderProviderSnapshots`: built-in order, then driver, display name, instance id.
pub fn order_provider_snapshots(mut providers: Vec<ServerProvider>) -> Vec<ServerProvider> {
    providers.sort_by(|left, right| {
        driver_rank(left.driver.as_str())
            .cmp(&driver_rank(right.driver.as_str()))
            .then_with(|| left.driver.as_str().cmp(right.driver.as_str()))
            .then_with(|| left.display_name.as_deref().unwrap_or("").cmp(right.display_name.as_deref().unwrap_or("")))
            .then_with(|| left.instance_id.as_str().cmp(right.instance_id.as_str()))
    });
    providers
}

/// `isCachedProviderCorrelated`: the cache file must name the same instance and driver.
pub fn is_cached_provider_correlated(cached: &ServerProvider, fallback: &ServerProvider) -> bool {
    cached.instance_id == fallback.instance_id && cached.driver == fallback.driver
}

fn merge_provider_models(fallback: &[ServerProviderModel], cached: &[ServerProviderModel]) -> Vec<ServerProviderModel> {
    let mut models = fallback.to_vec();
    models.extend(
        cached
            .iter()
            .filter(|model| !model.is_custom && !fallback.iter().any(|candidate| candidate.slug == model.slug))
            .cloned(),
    );
    models
}

/// `hydrateCachedProvider`: settings-derived fields from the fallback, probe results from the
/// cache.
pub fn hydrate_cached_provider(cached: &ServerProvider, fallback: &ServerProvider) -> ServerProvider {
    if !is_cached_provider_correlated(cached, fallback) || !fallback.enabled || cached.enabled != fallback.enabled {
        return fallback.clone();
    }
    let mut hydrated = fallback.clone();
    hydrated.message = cached.message.clone().filter(|message| !message.is_empty());
    hydrated.models = merge_provider_models(&fallback.models, &cached.models);
    hydrated.installed = cached.installed;
    hydrated.version = cached.version.clone();
    hydrated.status = cached.status;
    hydrated.auth = cached.auth.clone();
    hydrated.checked_at = cached.checked_at.clone();
    hydrated.slash_commands = cached.slash_commands.clone();
    hydrated.skills = cached.skills.clone();
    hydrated
}

/// `resolveProviderStatusCachePath`.
pub fn resolve_provider_status_cache_path(cache_dir: &Path, instance_id: &str) -> PathBuf {
    cache_dir.join(format!("{instance_id}.json"))
}

/// `readProviderStatusCache`: `None` when missing, empty or invalid (logged).
pub async fn read_provider_status_cache(file_path: &Path) -> Option<ServerProvider> {
    let raw = tokio::fs::read_to_string(file_path).await.ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<ServerProvider>(trimmed) {
        Ok(provider) => Some(provider),
        Err(error) => {
            tracing::warn!(path = %file_path.display(), error_kind = ?error.classify(), "failed to parse provider status cache, ignoring");
            None
        }
    }
}

/// The cache file contents: `JSON.stringify(provider minus updateState, null, 2) + "\n"`.
pub fn encode_provider_status_cache(provider: &ServerProvider) -> String {
    let mut cacheable = provider.clone();
    cacheable.update_state = None;
    let value = serde_json::to_value(&cacheable).unwrap_or_default();
    format!("{}\n", crate::js_json::stringify_pretty(&value, 2))
}

/// `writeProviderStatusCache`: atomic write of [`encode_provider_status_cache`].
pub async fn write_provider_status_cache(file_path: &Path, provider: &ServerProvider) -> std::io::Result<()> {
    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    zc_core::write_file_string_atomically(file_path, &encode_provider_status_cache(provider)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) fn provider(value: serde_json::Value) -> ServerProvider {
        let mut base = json!({
            "instanceId": "codex", "driver": "codex", "enabled": true, "installed": true, "version": "1.0.0",
            "status": "ready", "auth": {"status": "authenticated"}, "checkedAt": "2026-01-01T00:00:00.000Z",
            "models": [], "slashCommands": [], "skills": []
        });
        for (key, item) in value.as_object().unwrap() {
            base[key] = item.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    #[tokio::test]
    async fn writes_and_reads_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let path = resolve_provider_status_cache_path(dir.path(), "codex");
        let snapshot = provider(json!({"updateState": {"status": "running", "startedAt": null, "finishedAt": null, "message": null, "output": null}}));
        write_provider_status_cache(&path, &snapshot).await.unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.starts_with("{\n  \"instanceId\": \"codex\","));
        assert!(raw.ends_with("}\n"));
        let read = read_provider_status_cache(&path).await.unwrap();
        assert_eq!(read.update_state, None);
        assert_eq!(read.version.as_deref(), Some("1.0.0"));
        std::fs::write(&path, "{not json").unwrap();
        assert!(read_provider_status_cache(&path).await.is_none());
        assert!(read_provider_status_cache(&dir.path().join("missing.json")).await.is_none());
    }

    #[test]
    fn hydrates_probe_results_over_settings_models() {
        let fallback = provider(json!({
            "installed": false, "version": null, "status": "warning", "auth": {"status": "unknown"},
            "message": "Checking provider status...",
            "models": [{"slug": "builtin", "name": "Builtin", "isCustom": false, "capabilities": null}, {"slug": "custom-now", "name": "Custom", "isCustom": true, "capabilities": null}]
        }));
        let cached = provider(json!({
            "checkedAt": "2026-02-02T00:00:00.000Z",
            "models": [
                {"slug": "discovered", "name": "Discovered", "isCustom": false, "capabilities": null},
                {"slug": "custom-removed", "name": "Removed", "isCustom": true, "capabilities": null}
            ]
        }));
        let hydrated = hydrate_cached_provider(&cached, &fallback);
        let slugs: Vec<&str> = hydrated.models.iter().map(|model| model.slug.as_str()).collect();
        assert_eq!(slugs, vec!["builtin", "custom-now", "discovered"]);
        assert!(hydrated.installed);
        assert_eq!(hydrated.message, None);
        assert_eq!(hydrated.checked_at, "2026-02-02T00:00:00.000Z");

        let disabled = provider(json!({"enabled": false, "status": "disabled"}));
        assert_eq!(hydrate_cached_provider(&cached, &disabled), disabled);
        let other = provider(json!({"instanceId": "codex_work"}));
        assert!(!is_cached_provider_correlated(&cached, &other));
        assert_eq!(hydrate_cached_provider(&cached, &other), other);
    }

    #[test]
    fn orders_built_ins_first() {
        let ordered = order_provider_snapshots(vec![
            provider(json!({"instanceId": "fork", "driver": "forkDriver"})),
            provider(json!({"instanceId": "claudeAgent", "driver": "claudeAgent"})),
            provider(json!({"instanceId": "codex_work", "driver": "codex", "displayName": "Work"})),
            provider(json!({"instanceId": "codex", "driver": "codex"})),
        ]);
        let ids: Vec<&str> = ordered.iter().map(|provider| provider.instance_id.as_str()).collect();
        assert_eq!(ids, vec!["codex", "codex_work", "claudeAgent", "fork"]);
    }
}
