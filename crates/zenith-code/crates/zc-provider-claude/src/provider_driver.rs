//! The provider-core driver for Claude: `ClaudeDriver.create` of
//! `provider/Drivers/ClaudeDriver.ts` on top of [`zc_providers::Driver`].
//!
//! [`ClaudeProviderDriver`] decodes `ClaudeSettings`, builds one [`ClaudeInstance`] per
//! configured instance (adapter, status probe, skills, reset credits) and wires it into the
//! provider core:
//!
//! - the snapshot is a [`ManagedServerProvider`] over [`ClaudeInstance::pending_snapshot`] and
//!   [`ClaudeInstance::check_status`], stamped with the instance identity (`withInstanceIdentity`)
//!   and enriched with the version advisory (`enrichProviderSnapshotWithVersionAdvisory`);
//! - the snapshot settings are the instance's `ClaudeSettings` plus the server's
//!   `enableProviderUpdateChecks` (`makeProviderSnapshotSettingsSource`);
//! - the adapter writes its `NTIVE:` lines to the shared native event logger; canonical events
//!   are logged by the provider service, never here;
//! - the instance scope's finalizer shuts the adapter down (every session closed without
//!   `session.exited`, the event stream ended), like the TS adapter layer finalizer.
//!
//! The model catalog follows the model manifest: [`ClaudeModelCatalog::from_manifest`] already
//! carries the manifest's default and legacy flags, so (like TS) the Claude snapshot does not go
//! through `applyModelManifest` again. The adapter reads the catalog synchronously, so the
//! catalog is re-resolved from `ModelManifest::current()` whenever the snapshot is built or
//! probed (TS re-reads the manifest on every use).
//!
//! Maintenance (`makePackageManagedProviderMaintenanceResolver` for `@anthropic-ai/claude-code`
//! with the native `claude update`): ported for native installs, Vite+, Bun, pnpm and npm
//! global prefixes proven by the executable's real path. Simplified: a Homebrew keg and a
//! Windows npm shim resolve to manual-only (TS asks `brew --prefix` / `brew info` and looks for
//! the package manifest beside the shim). The latest version comes from the npm registry
//! (`/latest`, 4 s timeout, cached for an hour per package across the process), skipped when
//! `enableProviderUpdateChecks` is off or the CLI is not installed.
//!
//! The reset-credit coordinator (`resetCreditCoordinator.ts`) is process-wide here: one lock and
//! one pending idempotency key per Claude config directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{ClaudeSettings, ProviderDriverKind, ServerProvider, ServerProviderSkill};
use zc_ports::adapter::ProviderAdapter;
use zc_ports::SettingsService;
use zc_providers::driver::{ConsumeResetCredit, ContinuationIdentity, DriverCreateInput, DriverMetadata, InstanceHook, ProviderInstance, SnapshotForCwd};
use zc_providers::managed::{EnrichmentPublisher, ManagedProviderProbe, ManagedServerProvider};
use zc_providers::manifest::{ModelManifest, ModelManifestData};
use zc_providers::snapshot::{
    build_unavailable_provider_snapshot, create_provider_version_advisory, with_instance_identity, InstanceIdentity, ProviderMaintenanceCapabilities,
    ProviderMaintenanceCommandAction, ServerProviderDraft,
};
use zc_providers::{Driver, DriverEnv, EventNdjsonLogger, ProviderDriverError};

use crate::adapter::{CatalogSource, McpSessionLookup};
use crate::catalog::ClaudeModelCatalog;
use crate::driver::{is_claude_native_command_path, ClaudeDriver, ClaudeInstance, ClaudeInstanceInput, NATIVE_UPDATE_ARGS, NPM_PACKAGE_NAME};
use crate::home::Env;
use crate::mapping::NativeEventSink;
use crate::reset_credits::{ClaimOutcome, ClaudeResetCreditError};

/// `MAINTENANCE_CAPABILITIES_CACHE_TTL`.
pub const MAINTENANCE_CAPABILITIES_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
/// `LATEST_VERSION_CACHE_TTL_MS`.
pub const LATEST_VERSION_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
/// `LATEST_VERSION_TIMEOUT_MS`.
pub const LATEST_VERSION_TIMEOUT: Duration = Duration::from_millis(4_000);

/// Resolves the latest published version of an npm package (`resolveLatestProviderVersion`).
pub type LatestVersionLookup = Arc<dyn Fn(String) -> BoxFuture<'static, Option<String>> + Send + Sync>;

/// The Claude [`Driver`] (kind `claudeAgent`).
pub struct ClaudeProviderDriver {
    env: DriverEnv,
    mcp_sessions: Option<Arc<dyn McpSessionLookup>>,
    latest_version: LatestVersionLookup,
}

impl ClaudeProviderDriver {
    pub fn new(env: DriverEnv) -> Self {
        Self {
            env,
            mcp_sessions: None,
            latest_version: Arc::new(|package_name| Box::pin(npm_latest_version_cached(package_name))),
        }
    }

    /// The MCP credentials the provider service issues per thread (none by default).
    pub fn with_mcp_sessions(mut self, lookup: Option<Arc<dyn McpSessionLookup>>) -> Self {
        self.mcp_sessions = lookup;
        self
    }

    /// Replace the npm registry lookup behind version advisories (tests, offline builds).
    pub fn with_latest_version_lookup(mut self, lookup: LatestVersionLookup) -> Self {
        self.latest_version = lookup;
        self
    }
}

#[async_trait]
impl Driver for ClaudeProviderDriver {
    fn driver_kind(&self) -> ProviderDriverKind {
        ProviderDriverKind::from(crate::DRIVER_KIND)
    }

    fn metadata(&self) -> DriverMetadata {
        DriverMetadata {
            display_name: ClaudeDriver::DISPLAY_NAME.into(),
            supports_multiple_instances: ClaudeDriver::SUPPORTS_MULTIPLE_INSTANCES,
        }
    }

    fn decode_config(&self, raw: &Value) -> Result<Value, String> {
        decode_claude_config(raw).map(|settings| serde_json::to_value(settings).unwrap_or_default())
    }

    fn default_config(&self) -> Value {
        serde_json::to_value(ClaudeDriver::default_config()).unwrap_or_default()
    }

    async fn create(&self, input: DriverCreateInput) -> Result<ProviderInstance, ProviderDriverError> {
        let driver_kind = self.driver_kind();
        let instance_id = input.instance_id.clone();
        let error = |detail: String| ProviderDriverError::new(crate::DRIVER_KIND, instance_id.as_str(), detail);
        let config = decode_claude_config(&input.config).map_err(error)?;
        let environment: Env = self.env.instance_env(&input.environment).into_iter().collect();

        let catalog = CatalogCell::default();
        catalog.update(&self.env.model_manifest.current().await);
        let native_sink = self
            .env
            .event_loggers
            .native
            .clone()
            .map(|logger| Arc::new(NativeLoggerSink(logger)) as Arc<dyn NativeEventSink>);
        let mcp_sessions = self.mcp_sessions.clone();
        let instance = Arc::new(ClaudeInstance::with_adapter_options(
            ClaudeInstanceInput {
                instance_id: instance_id.clone(),
                enabled: input.enabled,
                config,
                environment: environment.clone(),
                server_cwd: Some(self.env.server_cwd.to_string_lossy().into_owned()),
                attachments_dir: self.env.attachments_dir.clone(),
                catalog: catalog.source(),
            },
            move |mut options| {
                options.native_sink = native_sink;
                options.mcp_sessions = mcp_sessions;
                options
            },
        ));
        let adapter = instance.adapter().clone();
        {
            let adapter = adapter.clone();
            input.scope.add_finalizer(async move { adapter.shutdown() });
        }

        let identity = InstanceIdentity {
            instance_id: instance_id.clone(),
            driver_kind: driver_kind.clone(),
            display_name: input.display_name.clone(),
            accent_color: input.accent_color.clone(),
            continuation_group_key: instance.continuation_key.clone(),
        };
        let probe = Arc::new(ClaudeSnapshotProbe {
            instance: instance.clone(),
            identity,
            server_settings: self.env.settings.clone(),
            manifest: self.env.model_manifest.clone(),
            catalog,
            maintenance: MaintenanceResolution::new(instance.settings.binary_path.clone(), environment),
            latest_version: self.latest_version.clone(),
        });
        let settings_changes = {
            let provider = instance.settings.clone();
            self.env
                .settings
                .subscribe_changes()
                .map(move |settings| ClaudeSnapshotSettings {
                    provider: provider.clone(),
                    enable_provider_update_checks: settings.enable_provider_update_checks,
                })
                .boxed()
        };
        let snapshot = ManagedServerProvider::start(probe, settings_changes, self.env.managed_options(input.scope.clone()))
            .await
            .map_err(|detail| error(format!("Failed to build Claude snapshot: {detail}")))?;

        let snapshot_for_cwd: SnapshotForCwd = {
            let snapshot = snapshot.clone();
            let instance = instance.clone();
            Arc::new(move |cwd: String| {
                let snapshot = snapshot.clone();
                let instance = instance.clone();
                Box::pin(async move {
                    let mut scoped = snapshot.current();
                    if !instance.settings.enabled {
                        return Ok(scoped);
                    }
                    let discovered = tokio::task::spawn_blocking({
                        let instance = instance.clone();
                        move || instance.skills_for_cwd(&cwd)
                    })
                    .await
                    .map_err(|join| ProviderDriverError::new(crate::DRIVER_KIND, instance.instance_id.as_str(), join.to_string()))?;
                    scoped.skills = discovered.unwrap_or_default().iter().filter_map(to_server_skill).collect();
                    Ok(scoped)
                })
            })
        };
        let invalidate_caches: InstanceHook = {
            let instance = instance.clone();
            Arc::new(move || {
                instance.invalidate_caches();
                Box::pin(async { Ok(()) })
            })
        };
        let consume_reset_credit: ConsumeResetCredit = {
            let snapshot = snapshot.clone();
            let instance = instance.clone();
            Arc::new(move || {
                let snapshot = snapshot.clone();
                let instance = instance.clone();
                Box::pin(async move { consume_reset_credit(&instance, &snapshot).await })
            })
        };

        Ok(ProviderInstance {
            continuation_identity: ContinuationIdentity {
                driver_kind: driver_kind.clone(),
                continuation_key: instance.continuation_key.clone(),
            },
            instance_id: input.instance_id,
            driver_kind,
            display_name: input.display_name,
            accent_color: input.accent_color,
            enabled: input.enabled,
            snapshot: Arc::new(snapshot),
            snapshot_for_cwd: Some(snapshot_for_cwd),
            refresh_models: None,
            invalidate_caches: Some(invalidate_caches),
            consume_reset_credit: Some(consume_reset_credit),
            adapter: Arc::new(adapter) as Arc<dyn ProviderAdapter>,
            text_generation: None,
            auth: None,
        })
    }
}

/// `Schema.decodeUnknown(ClaudeSettings)`: the schema's defaults filled in.
pub fn decode_claude_config(raw: &Value) -> Result<ClaudeSettings, String> {
    if !raw.is_object() {
        return Err(format!("Expected an object, got {raw}"));
    }
    serde_json::from_value(raw.clone()).map_err(|error| error.to_string())
}

fn to_server_skill(skill: &crate::skills::ClaudeSkill) -> Option<ServerProviderSkill> {
    serde_json::to_value(skill).ok().and_then(|value| serde_json::from_value(value).ok())
}

// ---------------------------------------------------------------------------------------------
// Native event log
// ---------------------------------------------------------------------------------------------

/// The adapter's `nativeEventLogger`: `{observedAt, event}` records on the `NTIVE:` stream.
struct NativeLoggerSink(EventNdjsonLogger);

impl NativeEventSink for NativeLoggerSink {
    fn write(&self, record: Value, thread_id: &str) {
        self.0.write(&record, Some(thread_id));
    }
}

// ---------------------------------------------------------------------------------------------
// Model catalog
// ---------------------------------------------------------------------------------------------

/// `modelManifest.current.pipe(Effect.map(resolveClaudeModelCatalog))`, readable synchronously.
#[derive(Clone, Default)]
struct CatalogCell {
    inner: Arc<Mutex<(Option<Arc<ModelManifestData>>, ClaudeModelCatalog)>>,
}

impl CatalogCell {
    fn update(&self, manifest: &Arc<ModelManifestData>) {
        let mut inner = self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if inner.0.as_ref().is_some_and(|current| Arc::ptr_eq(current, manifest)) {
            return;
        }
        let catalog = serde_json::to_value(manifest.as_ref())
            .map(|value| ClaudeModelCatalog::from_manifest(&value))
            .unwrap_or_else(|_| ClaudeModelCatalog::bundled());
        *inner = (Some(manifest.clone()), catalog);
    }

    fn source(&self) -> CatalogSource {
        let inner = self.inner.clone();
        Arc::new(move || inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).1.clone())
    }
}

// ---------------------------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------------------------

/// `ProviderSnapshotSettings<ClaudeSettings>`.
#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeSnapshotSettings {
    pub provider: ClaudeSettings,
    pub enable_provider_update_checks: bool,
}

struct ClaudeSnapshotProbe {
    instance: Arc<ClaudeInstance>,
    identity: InstanceIdentity,
    server_settings: Arc<dyn SettingsService>,
    manifest: ModelManifest,
    catalog: CatalogCell,
    maintenance: MaintenanceResolution,
    latest_version: LatestVersionLookup,
}

impl ClaudeSnapshotProbe {
    /// Decode a Claude draft and `stampIdentity` it.
    fn stamp(&self, draft: Value) -> Result<ServerProvider, String> {
        let mut draft = draft;
        if let Some(object) = draft.as_object_mut() {
            object.insert("instanceId".into(), json!(self.identity.instance_id));
            object.insert("driver".into(), json!(self.identity.driver_kind));
        }
        let provider: ServerProvider = serde_json::from_value(draft).map_err(|error| format!("Malformed Claude snapshot: {error}"))?;
        Ok(with_instance_identity(&self.identity, ServerProviderDraft(provider)))
    }
}

#[async_trait]
impl ManagedProviderProbe for ClaudeSnapshotProbe {
    type Settings = ClaudeSnapshotSettings;

    async fn get_settings(&self) -> Result<ClaudeSnapshotSettings, String> {
        let settings = self.server_settings.get_settings().await.map_err(|error| format!("{error:?}"))?;
        Ok(ClaudeSnapshotSettings {
            provider: self.instance.settings.clone(),
            enable_provider_update_checks: settings.enable_provider_update_checks,
        })
    }

    fn have_settings_changed(&self, previous: &ClaudeSnapshotSettings, next: &ClaudeSnapshotSettings) -> bool {
        previous != next
    }

    async fn initial_snapshot(&self, _settings: &ClaudeSnapshotSettings) -> ServerProvider {
        self.catalog.update(&self.manifest.current().await);
        self.stamp(self.instance.pending_snapshot()).unwrap_or_else(|reason| {
            build_unavailable_provider_snapshot(
                &self.identity.driver_kind,
                &self.identity.instance_id,
                self.identity.display_name.as_deref(),
                self.identity.accent_color.as_deref(),
                &reason,
                None,
            )
        })
    }

    async fn check_provider(&self) -> Result<ServerProvider, String> {
        // Start the TTL-gated manifest refresh without delaying the probe; the next check sees it.
        self.manifest.refresh_in_background();
        self.catalog.update(&self.manifest.current().await);
        self.stamp(self.instance.check_status().await)
    }

    fn has_enrichment(&self) -> bool {
        true
    }

    async fn enrich_snapshot(&self, settings: ClaudeSnapshotSettings, snapshot: ServerProvider, publisher: EnrichmentPublisher) {
        let capabilities = self.maintenance.get(false).await;
        let enriched = enrich_with_version_advisory(snapshot, &capabilities, settings.enable_provider_update_checks, &self.latest_version).await;
        publisher.publish(enriched);
    }

    async fn resolve_maintenance(&self, fresh: bool) -> ProviderMaintenanceCapabilities {
        self.maintenance.get(fresh).await
    }
}

/// `enrichProviderSnapshotWithVersionAdvisory(snapshot, capabilities, {enableProviderUpdateChecks})`.
pub async fn enrich_with_version_advisory(
    snapshot: ServerProvider,
    capabilities: &ProviderMaintenanceCapabilities,
    enable_provider_update_checks: bool,
    latest_version: &LatestVersionLookup,
) -> ServerProvider {
    let mut snapshot = snapshot;
    let version = snapshot.version.clone().filter(|version| !version.is_empty());
    let resolve_latest = enable_provider_update_checks && snapshot.enabled && snapshot.installed && version.is_some();
    let advisory = if resolve_latest {
        let latest = match &capabilities.latest_version {
            Some(known) => known.clone(),
            None => match &capabilities.package_name {
                Some(package_name) => latest_version(package_name.clone()).await,
                None => None,
            },
        };
        create_provider_version_advisory(
            &snapshot.driver,
            snapshot.version.as_deref(),
            latest.as_deref(),
            Some(&zc_core::now_iso()),
            capabilities,
        )
    } else {
        create_provider_version_advisory(&snapshot.driver, snapshot.version.as_deref(), None, Some(&snapshot.checked_at), capabilities)
    };
    snapshot.version_advisory = Some(advisory);
    snapshot
}

// ---------------------------------------------------------------------------------------------
// Reset credits
// ---------------------------------------------------------------------------------------------

type PendingKey = Arc<tokio::sync::Mutex<Option<String>>>;

/// `ResetCreditCoordinator`: one lock and one pending idempotency key per account directory.
fn account_redemption(account_key: &Path) -> PendingKey {
    static ACCOUNTS: OnceLock<Mutex<HashMap<PathBuf, PendingKey>>> = OnceLock::new();
    let mut accounts = ACCOUNTS.get_or_init(Mutex::default).lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    accounts.entry(account_key.to_path_buf()).or_default().clone()
}

/// `resetCreditCoordinator.redeem(accountKey, consume, isSettled)`: the key is kept until Claude
/// answers (an outcome, or a settled failure such as a cooldown) so a retry is the same attempt.
async fn redeem<F, Fut>(account_key: &Path, consume: F) -> Result<ClaimOutcome, ClaudeResetCreditError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<ClaimOutcome, ClaudeResetCreditError>>,
{
    let state = account_redemption(account_key);
    let mut pending = state.lock().await;
    let request_id = pending.get_or_insert_with(|| uuid::Uuid::new_v4().to_string()).clone();
    let outcome = consume(request_id).await;
    match &outcome {
        Ok(_) => *pending = None,
        Err(error) if error.is_settled() => *pending = None,
        Err(_) => {}
    }
    outcome
}

/// `consumeResetCredit()`: redeem the next banked reset, then re-probe.
async fn consume_reset_credit(instance: &ClaudeInstance, snapshot: &ManagedServerProvider<ClaudeSnapshotProbe>) -> Result<Value, ProviderDriverError> {
    let error = |detail: &str| ProviderDriverError::new(crate::DRIVER_KIND, instance.instance_id.as_str(), detail);
    let current = serde_json::to_value(snapshot.current()).unwrap_or_default();
    let grant_id = current
        .pointer("/usageLimits/resetCredits/nextCreditId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let version = current.get("version").and_then(Value::as_str).filter(|version| !version.is_empty());
    let (Some(grant_id), Some(version)) = (grant_id, version) else {
        return Ok(json!(ClaimOutcome::NoCredit.as_str()));
    };
    let outcome = redeem(&instance.config_dir, |request_id| async move {
        instance.consume_reset_credit(version, grant_id, &request_id).await
    })
    .await
    .map_err(|failure| error(&failure.to_string()))?;
    // Re-probe after any answer; only a reset claims the limits changed.
    let before = current.pointer("/usageLimits/checkedAt").cloned();
    instance.invalidate_caches();
    let refreshed = serde_json::to_value(snapshot.refresh_snapshot().await).unwrap_or_default();
    let after = refreshed.pointer("/usageLimits/checkedAt").cloned();
    let probe_failed = refreshed.pointer("/usageLimits/unavailable/reason").and_then(Value::as_str) == Some("probeFailed");
    if outcome == ClaimOutcome::Reset && (after.is_none() || after == before || probe_failed) {
        return Err(error("The reset was applied, but Claude could not confirm the new limits. Refresh to check."));
    }
    Ok(json!(outcome.as_str()))
}

// ---------------------------------------------------------------------------------------------
// Maintenance
// ---------------------------------------------------------------------------------------------

/// `makeCachedProviderMaintenanceResolution`: a cached read for advisories, a `fresh` read for
/// update execution.
struct MaintenanceResolution {
    binary_path: String,
    environment: Env,
    cached: Mutex<Option<(Instant, ProviderMaintenanceCapabilities)>>,
}

impl MaintenanceResolution {
    fn new(binary_path: String, environment: Env) -> Self {
        Self {
            binary_path,
            environment,
            cached: Mutex::new(None),
        }
    }

    async fn get(&self, fresh: bool) -> ProviderMaintenanceCapabilities {
        if !fresh {
            if let Some((at, capabilities)) = self.cached.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref() {
                if at.elapsed() < MAINTENANCE_CAPABILITIES_CACHE_TTL {
                    return capabilities.clone();
                }
            }
        }
        let binary_path = self.binary_path.clone();
        let environment = self.environment.clone();
        let capabilities = tokio::task::spawn_blocking(move || resolve_claude_maintenance(&binary_path, &environment))
            .await
            .unwrap_or_else(|_| manual_claude_maintenance());
        *self.cached.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((Instant::now(), capabilities.clone()));
        capabilities
    }
}

fn manual_claude_maintenance() -> ProviderMaintenanceCapabilities {
    ProviderMaintenanceCapabilities::manual_only(crate::DRIVER_KIND.into(), Some(NPM_PACKAGE_NAME.into()))
}

/// `resolveProviderMaintenanceCapabilitiesEffect(UPDATE, {binaryPath, env})`: locate the
/// executable, follow symlinks, and derive who can update it. Blocking (file system only).
pub fn resolve_claude_maintenance(binary_path: &str, environment: &Env) -> ProviderMaintenanceCapabilities {
    let binary_path = binary_path.trim();
    if binary_path.is_empty() {
        return manual_claude_maintenance();
    }
    let Some(resolved) = resolve_command_path(binary_path, environment) else {
        return manual_claude_maintenance();
    };
    let Ok(real) = std::fs::canonicalize(&resolved) else {
        return manual_claude_maintenance();
    };
    claude_maintenance_for_command_paths(&resolved.to_string_lossy(), &real.to_string_lossy())
}

/// `resolveCommandPath`: an explicit path must exist; a bare name is searched on `PATH`.
fn resolve_command_path(binary_path: &str, environment: &Env) -> Option<PathBuf> {
    if binary_path.contains('/') || binary_path.contains('\\') {
        let path = std::path::absolute(binary_path).ok()?;
        return path.is_file().then_some(path);
    }
    let search = environment.get("PATH").or_else(|| environment.get("Path"))?;
    std::env::split_paths(search)
        .map(|dir| dir.join(binary_path))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        metadata.is_file()
    }
}

/// `resolvePackageManagedProviderMaintenance(UPDATE, {resolvedCommandPath, realCommandPath})`,
/// without the Homebrew and Windows-shim branches (those stay manual-only).
pub fn claude_maintenance_for_command_paths(resolved_command_path: &str, real_command_path: &str) -> ProviderMaintenanceCapabilities {
    let paths = [resolved_command_path, real_command_path];
    let package = NPM_PACKAGE_NAME;
    let action = |executable: &str, args: Vec<String>, lock_key: String| ProviderMaintenanceCapabilities {
        provider: crate::DRIVER_KIND.into(),
        package_name: Some(package.into()),
        update: Some(ProviderMaintenanceCommandAction {
            command: std::iter::once(quote_update_executable(executable))
                .chain(args.iter().map(|arg| quote_shell_word(arg)))
                .collect::<Vec<_>>()
                .join(" "),
            executable: executable.into(),
            args,
            lock_key,
            env: None,
        }),
        latest_version: None,
    };
    if paths.iter().any(|path| is_claude_native_command_path(path)) {
        return action(
            resolved_command_path,
            NATIVE_UPDATE_ARGS.iter().map(|arg| arg.to_string()).collect(),
            format!("{}-native", crate::DRIVER_KIND),
        );
    }
    let normalized: Vec<String> = paths.iter().map(|path| normalize_command_path(path)).collect();
    if normalized.iter().any(|path| path.contains("/.vite-plus/bin/")) {
        return action("vp", vec!["i".into(), "-g".into(), package.into()], "vite-plus-global".into());
    }
    if normalized.iter().any(|path| path.contains("/.bun/bin/")) {
        return action("bun", vec!["i".into(), "-g".into(), format!("{package}@latest")], "bun-global".into());
    }
    if normalized.iter().any(|path| is_pnpm_global_command_path(path)) {
        return action("pnpm", vec!["add".into(), "-g".into(), format!("{package}@latest")], "pnpm-global".into());
    }
    if let Some(prefix) = npm_global_prefix_from_command_path(real_command_path, package) {
        // npm 12 blocks install scripts by default; claude's postinstall finishes the install.
        let lock_key = format!("npm-global:{}", normalize_command_path(&prefix));
        return action(
            "npm",
            vec![
                "install".into(),
                "-g".into(),
                "--prefix".into(),
                prefix,
                format!("--allow-scripts={package}"),
                format!("{package}@latest"),
            ],
            lock_key,
        );
    }
    manual_claude_maintenance()
}

/// `normalizeCommandPath`.
fn normalize_command_path(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

fn is_pnpm_global_command_path(normalized: &str) -> bool {
    [
        "/.local/share/pnpm/",
        "/library/pnpm/",
        "/local/share/pnpm/",
        "/appdata/local/pnpm/",
        "/pnpm/global/",
    ]
    .iter()
    .any(|segment| normalized.contains(segment))
}

/// `npmGlobalPrefixFromCommandPath`: `<prefix>/lib/node_modules/<pkg>/…` outside any project
/// `node_modules` (and outside a non-Node mise tool).
pub fn npm_global_prefix_from_command_path(real_command_path: &str, package_name: &str) -> Option<String> {
    let slash_path = real_command_path.replace('\\', "/");
    let normalized = slash_path.to_lowercase();
    let segment = format!("/lib/node_modules/{}/", package_name.to_lowercase());
    let index = normalized.rfind(&segment)?;
    let before = &normalized[..index];
    if before.contains("/node_modules/") {
        return None;
    }
    static MISE: OnceLock<regex::Regex> = OnceLock::new();
    let mise = MISE.get_or_init(|| regex::Regex::new(r"/mise/installs/([^/]+)/[^/]+$").expect("valid regex"));
    if let Some(tool) = mise.captures(before).and_then(|captures| captures.get(1)) {
        if tool.as_str() != "node" {
            return None;
        }
    }
    Some(if index == 0 { "/".into() } else { slash_path[..index].to_owned() })
}

/// `quoteShellWord` (POSIX).
fn quote_shell_word(word: &str) -> String {
    let safe = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "_./:@=-".contains(c));
    if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// `quoteUpdateExecutable` (POSIX: no `&` prefix).
fn quote_update_executable(executable: &str) -> String {
    quote_shell_word(executable)
}

// ---------------------------------------------------------------------------------------------
// Latest version (npm registry)
// ---------------------------------------------------------------------------------------------

/// `resolveLatestProviderVersion` for a package: the process-wide `ProviderVersionCache`, then
/// `GET https://registry.npmjs.org/<pkg>/latest`.
async fn npm_latest_version_cached(package_name: String) -> Option<String> {
    /// Package name → (expiry, latest version).
    type VersionCache = Mutex<HashMap<String, (Instant, Option<String>)>>;
    static CACHE: OnceLock<VersionCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    if let Some((expires_at, version)) = cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).get(&package_name) {
        if *expires_at > Instant::now() {
            return version.clone();
        }
    }
    let version = fetch_npm_latest_version(&package_name).await;
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(package_name, (Instant::now() + LATEST_VERSION_CACHE_TTL, version.clone()));
    version
}

async fn fetch_npm_latest_version(package_name: &str) -> Option<String> {
    let url = format!("https://registry.npmjs.org/{}/latest", encode_uri_component(package_name));
    let request = reqwest::Client::new()
        .get(url)
        .header("accept", "application/json")
        .timeout(LATEST_VERSION_TIMEOUT);
    let response = tokio::time::timeout(LATEST_VERSION_TIMEOUT, request.send()).await.ok()?.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body = tokio::time::timeout(LATEST_VERSION_TIMEOUT, response.text()).await.ok()?.ok()?;
    let payload: Value = serde_json::from_str(&body).ok()?;
    payload
        .get("version")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|version| !version.is_empty())
        .map(str::to_owned)
}

/// JS `encodeURIComponent`.
fn encode_uri_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_package_managed_updates_from_command_paths() {
        let native = claude_maintenance_for_command_paths("/Users/someone/.local/bin/claude", "/Users/someone/.local/share/claude/versions/2.1.0");
        let update = native.update.expect("native update");
        assert_eq!(update.command, "/Users/someone/.local/bin/claude update");
        assert_eq!(update.lock_key, "claudeAgent-native");

        let npm = claude_maintenance_for_command_paths("/opt/tools/bin/claude", "/opt/tools/lib/node_modules/@anthropic-ai/claude-code/cli.js");
        let update = npm.update.expect("npm update");
        assert_eq!(
            update.command,
            "npm install -g --prefix /opt/tools --allow-scripts=@anthropic-ai/claude-code @anthropic-ai/claude-code@latest"
        );
        assert_eq!(update.lock_key, "npm-global:/opt/tools");

        let bun = claude_maintenance_for_command_paths("/home/x/.bun/bin/claude", "/home/x/.bun/install/global/claude");
        assert_eq!(bun.update.expect("bun update").command, "bun i -g @anthropic-ai/claude-code@latest");

        let project_local = claude_maintenance_for_command_paths("/w/app/claude", "/w/app/node_modules/x/lib/node_modules/@anthropic-ai/claude-code/cli.js");
        assert_eq!(project_local, manual_claude_maintenance());
        assert_eq!(
            npm_global_prefix_from_command_path(
                "/h/mise/installs/python/3.12/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                NPM_PACKAGE_NAME
            ),
            None
        );
        assert_eq!(
            npm_global_prefix_from_command_path(
                "/h/mise/installs/node/22.1.0/lib/node_modules/@anthropic-ai/claude-code/cli.js",
                NPM_PACKAGE_NAME
            )
            .as_deref(),
            Some("/h/mise/installs/node/22.1.0")
        );
    }

    #[test]
    fn quotes_and_encodes_like_the_js_helpers() {
        assert_eq!(quote_shell_word("/a b/claude"), "'/a b/claude'");
        assert_eq!(quote_shell_word("it's"), "'it'\\''s'");
        assert_eq!(encode_uri_component("@anthropic-ai/claude-code"), "%40anthropic-ai%2Fclaude-code");
    }

    #[test]
    fn missing_binaries_resolve_to_manual_maintenance() {
        let environment = Env::from([("PATH".to_string(), "/nonexistent-zc-dir".to_string())]);
        assert_eq!(
            resolve_claude_maintenance("/nonexistent-zc-dir/claude", &environment),
            manual_claude_maintenance()
        );
        assert_eq!(resolve_claude_maintenance("claude-missing-zc", &environment), manual_claude_maintenance());
        assert_eq!(resolve_claude_maintenance("  ", &environment), manual_claude_maintenance());
    }

    #[tokio::test]
    async fn redemptions_keep_the_key_until_claude_answers() {
        let account = PathBuf::from("/tmp/zc-redeem-test-account");
        let mut first_key = String::new();
        let failed = redeem(&account, |key| {
            first_key = key;
            async { Err(ClaudeResetCreditError::RequestFailed) }
        })
        .await;
        assert_eq!(failed, Err(ClaudeResetCreditError::RequestFailed));
        let mut second_key = String::new();
        let outcome = redeem(&account, |key| {
            second_key = key;
            async { Ok(ClaimOutcome::Reset) }
        })
        .await;
        assert_eq!(outcome, Ok(ClaimOutcome::Reset));
        assert_eq!(first_key, second_key);
        let mut third_key = String::new();
        let _ = redeem(&account, |key| {
            third_key = key;
            async { Ok(ClaimOutcome::NothingToReset) }
        })
        .await;
        assert_ne!(third_key, second_key);
    }
}
