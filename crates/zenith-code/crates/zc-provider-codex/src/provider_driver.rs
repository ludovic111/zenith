//! The provider core's driver SPI for Codex: `CodexDriver` (`provider/Drivers/CodexDriver.ts`) and
//! `makeManagedCodexProvider` (`Drivers/CodexManagedProvider.ts`) wired onto
//! [`zc_providers::Driver`].
//!
//! [`CodexProviderDriver::create`] decodes the settings, builds the [`CodexInstance`] (home layout,
//! shadow home, environment merge, continuation identity, adapter), then the managed snapshot
//! ([`ManagedServerProvider`] over [`CodexSnapshotProbe`]): the pending snapshot first, the status
//! probe on refresh (run in the server cwd), the model manifest, the version advisory and the
//! instance identity stamped on every draft. Closing the instance scope stops every adapter
//! session and the snapshot's background work.
//!
//! The adapter only writes `NTIVE:` lines (each native event, under its thread id, like
//! `writeNativeEvent`); the `CANON:` lines are the provider service's, which subscribes to every
//! instance's adapter.
//!
//! Simplifications against TS:
//! - Maintenance ([`resolve_codex_maintenance`]): the standalone (`codex update`), Vite+, bun,
//!   pnpm and npm-global layouts are recognized from the executable's real path like
//!   `resolvePackageManagedProviderMaintenance`; a Homebrew keg stays manual-only (TS asks `brew`
//!   for its prefix and latest version), and so does a Windows npm shim.
//! - Text generation (`codex exec`) is WP-17: `text_generation` is `None`.
//! - Managed mode (`setupMode: "managed"`) needs WP-12b's installation and ChatGPT sign-in. With
//!   [`CodexProviderDriver::with_managed_seams`] they are used; without them the instance reports
//!   "Set up Codex to get started." and sessions are refused, and `auth` (the sign-in controller,
//!   whose state changes re-probe in TS) stays `None`. The ChatGPT model catalog filter
//!   (`chatGptModels`) is WP-12b as well.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};
use zc_contracts::{
    CodexSettings, CodexSettingsSetupMode, ProviderDriverKind, ProviderEvent, ProviderInstanceId, ProviderSetupError, ServerProvider, ServerProviderModel,
    ServerProviderUsageLimitsUnavailableReason,
};
use zc_ports::adapter::ProviderAdapter;
use zc_providers::driver::{ConsumeResetCredit, ContinuationIdentity, DriverCreateInput, DriverMetadata, ProviderInstance, SnapshotForCwd};
use zc_providers::managed::{EnrichmentPublisher, ManagedProviderProbe, ManagedServerProvider};
use zc_providers::manifest::{apply_model_manifest, HttpManifestFetcher, ManifestFetcher, ModelManifestData};
use zc_providers::snapshot::{
    build_unavailable_provider_snapshot, create_provider_version_advisory, with_instance_identity, InstanceIdentity, ProviderMaintenanceCapabilities,
    ProviderMaintenanceCommandAction, ServerProviderDraft,
};
use zc_providers::{Driver, DriverEnv, EventNdjsonLogger, ProviderDriverError};

use crate::adapter::{AttachmentsDir, CodexAdapterOptions, McpSessionLookup, NativeEventSink, RuntimeFactory};
use crate::driver::{create_codex_instance, create_managed_codex_instance, CodexDriverCreateInput, CodexDriverServices, CodexInstance, CodexMaintenance};
use crate::launch_args::Environment;
use crate::managed::{ChatGptAccessSource, ChatGptAccount, ManagedExecutable, ManagedExecutableSource};
use crate::provider_status::{draft_into_server_provider, ProbeFn};

/// `MAINTENANCE_CAPABILITIES_CACHE_TTL`.
const MAINTENANCE_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
/// `LATEST_VERSION_CACHE_TTL_MS`.
const LATEST_VERSION_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
/// `LATEST_VERSION_TIMEOUT_MS`.
const LATEST_VERSION_TIMEOUT: Duration = Duration::from_secs(4);
/// The redemption itself is bounded (20 s) by [`CodexInstance::consume_reset_credit`].
const RESET_CREDIT_FAILED: &str = "Codex could not redeem the reset credit.";
const RESET_CREDIT_UNCONFIRMED: &str = "The reset was applied, but Codex could not confirm the new limits. Refresh to check.";

fn driver_kind() -> ProviderDriverKind {
    ProviderDriverKind::new(crate::DRIVER_KIND)
}

fn driver_error(instance_id: &ProviderInstanceId, detail: impl Into<String>) -> ProviderDriverError {
    ProviderDriverError::new(crate::DRIVER_KIND, instance_id.as_str(), detail)
}

/// WP-12b's managed-mode seams: the managed `codex` installation and the ChatGPT sign-in.
#[derive(Clone)]
pub struct CodexManagedSeams {
    /// `<stateDir>` is taken from [`DriverEnv::state_dir`]; this is the installation.
    pub installation: Arc<dyn ManagedExecutableSource>,
    pub auth: Arc<dyn ChatGptAccessSource>,
}

/// `CodexDriver`: the `codex` entry of `BUILT_IN_DRIVERS`.
pub struct CodexProviderDriver {
    env: DriverEnv,
    mcp_sessions: Option<Arc<dyn McpSessionLookup>>,
    lsuite_mcp: bool,
    managed_seams: Option<CodexManagedSeams>,
    status_probe: Option<ProbeFn>,
    runtime_factory: Option<RuntimeFactory>,
    latest_version_fetcher: Arc<dyn ManifestFetcher>,
}

impl CodexProviderDriver {
    pub fn new(env: DriverEnv) -> Self {
        Self {
            env,
            mcp_sessions: None,
            lsuite_mcp: false,
            managed_seams: None,
            status_probe: None,
            runtime_factory: None,
            latest_version_fetcher: Arc::new(HttpManifestFetcher::default()),
        }
    }

    /// The per-thread `t3-code` MCP sessions (WP-27a) sessions are started with.
    pub fn with_mcp_sessions(mut self, lookup: Option<Arc<dyn McpSessionLookup>>) -> Self {
        self.mcp_sessions = lookup;
        self
    }

    /// zenith: hand sessions the other lsuite apps' MCP servers (off by default).
    pub fn with_lsuite_mcp(mut self, on: bool) -> Self {
        self.lsuite_mcp = on;
        self
    }

    /// WP-12b's managed installation and ChatGPT sign-in for `setupMode: "managed"` instances.
    pub fn with_managed_seams(mut self, seams: Option<CodexManagedSeams>) -> Self {
        self.managed_seams = seams;
        self
    }

    /// Replaces the `codex app-server` status probe (tests, embedding).
    pub fn with_status_probe(mut self, probe: Option<ProbeFn>) -> Self {
        self.status_probe = probe;
        self
    }

    /// Replaces how session runtimes are built (tests: fake runtimes instead of `codex`).
    pub fn with_runtime_factory(mut self, factory: Option<RuntimeFactory>) -> Self {
        self.runtime_factory = factory;
        self
    }

    /// Replaces the npm registry client of the version advisory (tests).
    pub fn with_latest_version_fetcher(mut self, fetcher: Arc<dyn ManifestFetcher>) -> Self {
        self.latest_version_fetcher = fetcher;
        self
    }
}

fn decode_codex_settings(raw: &Value) -> Result<CodexSettings, String> {
    if !raw.is_object() {
        return Err(format!("Expected an object, got {raw}"));
    }
    serde_json::from_value(raw.clone()).map_err(|error| error.to_string())
}

#[async_trait]
impl Driver for CodexProviderDriver {
    fn driver_kind(&self) -> ProviderDriverKind {
        driver_kind()
    }

    fn metadata(&self) -> DriverMetadata {
        DriverMetadata {
            display_name: "Codex".into(),
            supports_multiple_instances: true,
        }
    }

    fn decode_config(&self, raw: &Value) -> Result<Value, String> {
        let settings = decode_codex_settings(raw)?;
        serde_json::to_value(settings).map_err(|error| error.to_string())
    }

    fn default_config(&self) -> Value {
        self.decode_config(&json!({})).expect("the Codex settings defaults decode")
    }

    async fn create(&self, input: DriverCreateInput) -> Result<ProviderInstance, ProviderDriverError> {
        let instance_id = input.instance_id.clone();
        let mut config = decode_codex_settings(&input.config).map_err(|detail| driver_error(&instance_id, format!("Invalid Codex settings: {detail}")))?;
        let managed = config.setup_mode == Some(CodexSettingsSetupMode::Managed);
        if managed {
            config.enabled = input.enabled;
        }

        // The adapter reads display names from the published snapshot, which is built after the
        // instance: the source resolves it lazily (and weakly, so no cycle keeps it alive).
        let snapshot_cell: Arc<OnceLock<Weak<ManagedServerProvider<CodexSnapshotProbe>>>> = Arc::new(OnceLock::new());
        let models_cell = snapshot_cell.clone();
        let models: crate::session_runtime::ModelsSource = Arc::new(move || {
            let models: Vec<ServerProviderModel> = models_cell
                .get()
                .and_then(Weak::upgrade)
                .map(|snapshot| snapshot.current().models)
                .unwrap_or_default();
            Box::pin(async move { models })
        });
        let adapter_options = CodexAdapterOptions {
            models: Some(models),
            make_runtime: self.runtime_factory.clone(),
            native_event_sink: self
                .env
                .event_loggers
                .native
                .clone()
                .map(|logger| Arc::new(NativeEventLog(logger)) as Arc<dyn NativeEventSink>),
            mcp_sessions: self.mcp_sessions.clone(),
            lsuite_mcp: self.lsuite_mcp,
            attachments: Some(Arc::new(AttachmentsDir(self.env.attachments_dir.clone()))),
            default_cwd: Some(self.env.server_cwd.to_string_lossy().into_owned()),
            ..CodexAdapterOptions::default()
        };
        let services = CodexDriverServices {
            base_environment: Some(self.env.base_env.iter().map(|(key, value)| (key.clone(), value.clone())).collect()),
            adapter: adapter_options,
            probe: self.status_probe.clone(),
        };
        let codex_input = CodexDriverCreateInput {
            instance_id: instance_id.clone(),
            display_name: input.display_name.clone(),
            accent_color: input.accent_color.clone(),
            environment: input.environment.clone(),
            enabled: input.enabled,
            config,
        };
        let instance = if managed {
            let seams = self.managed_seams.clone().unwrap_or_else(|| {
                let unavailable = Arc::new(ManagedSetupUnavailable(instance_id.clone()));
                CodexManagedSeams {
                    installation: unavailable.clone(),
                    auth: unavailable,
                }
            });
            create_managed_codex_instance(codex_input, services, &self.env.state_dir, seams.installation, seams.auth)
        } else {
            create_codex_instance(codex_input, services).map_err(|error| driver_error(&instance_id, error.detail))?
        };
        let instance = Arc::new(instance);

        let identity = InstanceIdentity {
            instance_id: instance_id.clone(),
            driver_kind: driver_kind(),
            display_name: input.display_name.clone(),
            accent_color: input.accent_color.clone(),
            continuation_group_key: instance.continuation_identity.continuation_key.clone(),
        };
        let maintenance = if managed {
            MaintenanceSource::Fixed(ProviderMaintenanceCapabilities::manual_only(driver_kind(), None))
        } else {
            MaintenanceSource::Resolved(CachedMaintenance::new(
                instance.maintenance.clone(),
                instance.effective_config.binary_path.clone(),
                instance.environment.clone(),
            ))
        };
        let probe = Arc::new(CodexSnapshotProbe {
            instance: instance.clone(),
            identity,
            managed,
            env: self.env.clone(),
            maintenance,
            latest_version_fetcher: self.latest_version_fetcher.clone(),
        });
        let settings_changes = {
            let provider = instance.effective_config.clone();
            self.env
                .settings
                .subscribe_changes()
                .map(move |settings| CodexSnapshotSettings {
                    provider: provider.clone(),
                    enable_provider_update_checks: settings.enable_provider_update_checks,
                })
                .boxed()
        };
        let snapshot = ManagedServerProvider::start(probe, settings_changes, self.env.managed_options(input.scope.clone()))
            .await
            .map_err(|detail| {
                driver_error(
                    &instance_id,
                    if managed {
                        "Could not prepare managed Codex.".to_owned()
                    } else {
                        format!("Failed to build Codex snapshot: {detail}")
                    },
                )
            })?;
        let snapshot = Arc::new(snapshot);
        let _ = snapshot_cell.set(Arc::downgrade(&snapshot));

        // Closing the instance stops every session (their app-server processes with them).
        {
            let adapter = instance.adapter.clone();
            input.scope.add_finalizer(async move {
                if let Err(error) = adapter.stop_all().await {
                    tracing::warn!(%error, "stopping Codex sessions on instance close failed");
                }
            });
        }

        let snapshot_for_cwd: SnapshotForCwd = {
            let instance = instance.clone();
            let snapshot = snapshot.clone();
            Arc::new(move |cwd: String| {
                let instance = instance.clone();
                let snapshot = snapshot.clone();
                Box::pin(async move {
                    if !instance.enabled {
                        return Ok(snapshot.current());
                    }
                    match instance.skills_for_cwd(&cwd).await {
                        Ok(skills) => {
                            let mut scoped = snapshot.current();
                            scoped.skills = skills;
                            Ok(scoped)
                        }
                        // Managed mode falls back to the machine snapshot (`Effect.catch`).
                        Err(_) if managed => Ok(snapshot.current()),
                        Err(error) => Err(driver_error(&instance.instance_id, error.detail)),
                    }
                })
            })
        };

        let consume_reset_credit: Option<ConsumeResetCredit> = (!managed).then(|| {
            let instance = instance.clone();
            let snapshot = snapshot.clone();
            let cwd = self.env.server_cwd.to_string_lossy().into_owned();
            let consume: ConsumeResetCredit = Arc::new(move || {
                let instance = instance.clone();
                let snapshot = snapshot.clone();
                let cwd = cwd.clone();
                Box::pin(async move { consume_reset_credit(&instance, &snapshot, &cwd).await })
            });
            consume
        });

        Ok(ProviderInstance {
            instance_id: instance_id.clone(),
            driver_kind: driver_kind(),
            continuation_identity: ContinuationIdentity {
                driver_kind: instance.continuation_identity.driver_kind.clone(),
                continuation_key: instance.continuation_identity.continuation_key.clone(),
            },
            display_name: input.display_name,
            accent_color: input.accent_color,
            enabled: input.enabled,
            snapshot: snapshot.clone(),
            snapshot_for_cwd: Some(snapshot_for_cwd),
            refresh_models: None,
            invalidate_caches: None,
            consume_reset_credit,
            adapter: Arc::new(instance.adapter.clone()),
            text_generation: None,
            auth: None,
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Snapshot probe

/// `ProviderSnapshotSettings<CodexSettings>`.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexSnapshotSettings {
    pub provider: CodexSettings,
    pub enable_provider_update_checks: bool,
}

/// The Codex half of `makeManagedServerProvider`.
pub struct CodexSnapshotProbe {
    instance: Arc<CodexInstance>,
    identity: InstanceIdentity,
    managed: bool,
    env: DriverEnv,
    maintenance: MaintenanceSource,
    latest_version_fetcher: Arc<dyn ManifestFetcher>,
}

impl CodexSnapshotProbe {
    /// `draft → applyModelManifest → withInstanceIdentity` (managed mode skips the manifest).
    fn finalize(&self, draft: Value, manifest: Option<&ModelManifestData>) -> ServerProvider {
        match draft_into_server_provider(draft, self.identity.instance_id.as_str()) {
            Ok(mut snapshot) => {
                if let Some(manifest) = manifest {
                    snapshot.models = apply_model_manifest(&snapshot.models, manifest, crate::DRIVER_KIND);
                }
                with_instance_identity(&self.identity, ServerProviderDraft(snapshot))
            }
            Err(error) => {
                tracing::error!(%error, instance_id = %self.identity.instance_id, "a Codex snapshot draft does not decode");
                build_unavailable_provider_snapshot(
                    &self.identity.driver_kind,
                    &self.identity.instance_id,
                    self.identity.display_name.as_deref(),
                    self.identity.accent_color.as_deref(),
                    &format!("Codex reported a status this build cannot read: {error}"),
                    None,
                )
            }
        }
    }

    async fn latest_version(&self, capabilities: &ProviderMaintenanceCapabilities) -> Option<String> {
        if let Some(latest) = &capabilities.latest_version {
            return latest.clone();
        }
        let package_name = capabilities.package_name.as_deref()?;
        resolve_npm_latest_version(self.latest_version_fetcher.as_ref(), package_name).await
    }
}

#[async_trait]
impl ManagedProviderProbe for CodexSnapshotProbe {
    type Settings = CodexSnapshotSettings;

    async fn get_settings(&self) -> Result<CodexSnapshotSettings, String> {
        let settings = self
            .env
            .settings
            .get_settings()
            .await
            .map_err(|error| serde_json::to_string(&error).unwrap_or_else(|_| "settings unavailable".into()))?;
        Ok(CodexSnapshotSettings {
            provider: self.instance.effective_config.clone(),
            enable_provider_update_checks: settings.enable_provider_update_checks,
        })
    }

    fn have_settings_changed(&self, previous: &CodexSnapshotSettings, next: &CodexSnapshotSettings) -> bool {
        // Managed mode: `haveSettingsChanged: () => false`.
        !self.managed && previous != next
    }

    async fn initial_snapshot(&self, _settings: &CodexSnapshotSettings) -> ServerProvider {
        let draft = self.instance.pending_snapshot(&zc_core::now_iso());
        if self.managed {
            return self.finalize(draft, None);
        }
        let manifest = self.env.model_manifest.current().await;
        self.finalize(draft, Some(&manifest))
    }

    async fn check_provider(&self) -> Result<ServerProvider, String> {
        let cwd = self.env.server_cwd.to_string_lossy().into_owned();
        let checked_at = zc_core::now_iso();
        if self.managed {
            let draft = self.instance.check_provider(&cwd, &checked_at).await;
            return Ok(self.finalize(draft, None));
        }
        // The TTL-gated manifest refresh runs in the background; the probe classifies with the
        // manifest in memory, so a slow fetch never delays it.
        self.env.model_manifest.refresh_in_background();
        let (draft, manifest) = tokio::join!(self.instance.check_provider(&cwd, &checked_at), self.env.model_manifest.current());
        Ok(self.finalize(draft, Some(&manifest)))
    }

    fn has_enrichment(&self) -> bool {
        !self.managed
    }

    /// `enrichProviderSnapshotWithVersionAdvisory`.
    async fn enrich_snapshot(&self, settings: CodexSnapshotSettings, snapshot: ServerProvider, publisher: EnrichmentPublisher) {
        let capabilities = self.resolve_maintenance(false).await;
        let should_resolve_latest =
            settings.enable_provider_update_checks && snapshot.enabled && snapshot.installed && snapshot.version.as_deref().is_some_and(|v| !v.is_empty());
        let mut enriched = snapshot;
        let advisory = if should_resolve_latest {
            let latest = self.latest_version(&capabilities).await;
            create_provider_version_advisory(
                &enriched.driver,
                enriched.version.as_deref(),
                latest.as_deref(),
                Some(&zc_core::now_iso()),
                &capabilities,
            )
        } else {
            create_provider_version_advisory(&enriched.driver, enriched.version.as_deref(), None, Some(&enriched.checked_at), &capabilities)
        };
        enriched.version_advisory = Some(advisory);
        publisher.publish(enriched);
    }

    async fn resolve_maintenance(&self, fresh: bool) -> ProviderMaintenanceCapabilities {
        match &self.maintenance {
            MaintenanceSource::Fixed(capabilities) => capabilities.clone(),
            MaintenanceSource::Resolved(cached) => cached.get(fresh),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Reset credits

/// `consumeResetCredit`: redeem under the account lock, then re-probe so the snapshot shows the
/// new windows. A `reset` whose refresh did not land is reported as unconfirmed.
async fn consume_reset_credit(instance: &CodexInstance, snapshot: &ManagedServerProvider<CodexSnapshotProbe>, cwd: &str) -> Result<Value, ProviderDriverError> {
    let outcome = redeem_reset_credit(instance.account_key(), |idempotency_key| async move {
        instance.consume_reset_credit(&idempotency_key, cwd).await
    })
    .await
    .map_err(|_| driver_error(&instance.instance_id, RESET_CREDIT_FAILED))?;
    let before = snapshot.current().usage_limits.map(|limits| limits.checked_at);
    let refreshed = snapshot.refresh_snapshot().await;
    let after = refreshed.usage_limits.as_ref().map(|limits| limits.checked_at.clone());
    let probe_failed = refreshed
        .usage_limits
        .as_ref()
        .and_then(|limits| limits.unavailable.as_ref())
        .is_some_and(|unavailable| unavailable.reason == ServerProviderUsageLimitsUnavailableReason::ProbeFailed);
    if outcome == "reset" && (after.is_none() || after == before || probe_failed) {
        return Err(driver_error(&instance.instance_id, RESET_CREDIT_UNCONFIRMED));
    }
    Ok(Value::String(outcome))
}

/// An account's lock around its pending idempotency key.
type AccountRedemption = Arc<tokio::sync::Mutex<Option<String>>>;

/// One pending idempotency key per account, behind the account's lock
/// (`ResetCreditCoordinator`): overlapping redemptions from any instance sharing the account
/// queue, and a retry after a failure (a timeout included) re-sends the same attempt.
fn reset_credit_accounts() -> &'static Mutex<HashMap<PathBuf, AccountRedemption>> {
    static ACCOUNTS: OnceLock<Mutex<HashMap<PathBuf, AccountRedemption>>> = OnceLock::new();
    ACCOUNTS.get_or_init(Default::default)
}

/// `ResetCreditCoordinator.redeem(accountKey, consume)`.
pub async fn redeem_reset_credit<F, Fut, E>(account_key: PathBuf, consume: F) -> Result<String, E>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, E>>,
{
    let state = reset_credit_accounts().lock().unwrap().entry(account_key).or_default().clone();
    let mut pending = state.lock().await;
    let idempotency_key = pending.get_or_insert_with(zc_core::uuid_v4).clone();
    let outcome = consume(idempotency_key).await?;
    *pending = None;
    Ok(outcome)
}

// ---------------------------------------------------------------------------------------------
// Native event log

/// `writeNativeEvent`: each native event as an `NTIVE:` record of its thread.
struct NativeEventLog(EventNdjsonLogger);

impl NativeEventSink for NativeEventLog {
    fn write(&self, event: &ProviderEvent) {
        self.0.write_serializable(event, Some(event.thread_id.as_str()));
    }
}

// ---------------------------------------------------------------------------------------------
// Managed mode without WP-12b

/// The managed seams when WP-12b is not wired: nothing installed, nobody signed in.
struct ManagedSetupUnavailable(ProviderInstanceId);

#[async_trait]
impl ManagedExecutableSource for ManagedSetupUnavailable {
    async fn acquire(&self) -> Result<ManagedExecutable, String> {
        Err("Managed Codex setup is not available in this build.".into())
    }
    async fn resolve(&self) -> Option<ManagedExecutable> {
        None
    }
}

#[async_trait]
impl ChatGptAccessSource for ManagedSetupUnavailable {
    async fn access_token(&self) -> Result<String, ProviderSetupError> {
        Err(crate::model::from_json(json!({
            "_tag": "ProviderSetupError",
            "instanceId": self.0.as_str(),
            "operation": "auth",
            "detail": "Sign in with ChatGPT to use Codex.",
        })))
    }
    async fn read_account(&self) -> Option<ChatGptAccount> {
        None
    }
    async fn revoke(&self) {}
}

// ---------------------------------------------------------------------------------------------
// Maintenance

enum MaintenanceSource {
    /// Managed mode: `{ packageName: null, update: null }`.
    Fixed(ProviderMaintenanceCapabilities),
    Resolved(CachedMaintenance),
}

/// `makeCachedProviderMaintenanceResolution`: one resolution per hour; `fresh` re-derives.
struct CachedMaintenance {
    maintenance: CodexMaintenance,
    binary_path: String,
    environment: Environment,
    cache: Mutex<Option<(Instant, ProviderMaintenanceCapabilities)>>,
}

impl CachedMaintenance {
    fn new(maintenance: CodexMaintenance, binary_path: String, environment: Environment) -> Self {
        Self {
            maintenance,
            binary_path,
            environment,
            cache: Mutex::new(None),
        }
    }

    fn get(&self, fresh: bool) -> ProviderMaintenanceCapabilities {
        if !fresh {
            if let Some((at, capabilities)) = self.cache.lock().unwrap().as_ref() {
                if at.elapsed() < MAINTENANCE_CACHE_TTL {
                    return capabilities.clone();
                }
            }
        }
        let capabilities = resolve_codex_maintenance(&self.maintenance, &self.binary_path, &self.environment);
        *self.cache.lock().unwrap() = Some((Instant::now(), capabilities.clone()));
        capabilities
    }
}

/// `normalizeCommandPath`.
fn normalize_command_path(path: &str) -> String {
    path.replace('\\', "/").to_lowercase()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `resolveCommandPath`: an explicit path must exist; a bare name is looked up on `PATH`.
pub fn resolve_command_path(binary_path: &str, environment: &Environment) -> Option<PathBuf> {
    let binary_path = binary_path.trim();
    if binary_path.is_empty() {
        return None;
    }
    if binary_path.contains('/') || binary_path.contains('\\') || binary_path.starts_with('~') {
        let path = zc_core::paths::resolve_path(&zc_core::expand_home_path(binary_path));
        return is_executable_file(&path).then_some(path);
    }
    let search = environment
        .get("PATH")
        .or_else(|| environment.get("Path"))
        .or_else(|| environment.get("path"))?;
    std::env::split_paths(search)
        .map(|directory| directory.join(binary_path))
        .find(|candidate| is_executable_file(candidate))
}

/// `quoteShellWord` (POSIX): the copyable command must paste into a shell as-is.
fn quote_shell_word(word: &str) -> String {
    let safe = !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "_./:@=-".contains(c));
    if safe {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// `makeProviderMaintenanceCapabilities`.
fn package_managed_capabilities(
    package_name: &str,
    executable: &str,
    args: Vec<String>,
    lock_key: String,
    env: Option<Vec<(String, String)>>,
) -> ProviderMaintenanceCapabilities {
    let command = std::iter::once(executable)
        .chain(args.iter().map(String::as_str))
        .map(quote_shell_word)
        .collect::<Vec<_>>()
        .join(" ");
    ProviderMaintenanceCapabilities {
        provider: driver_kind(),
        package_name: Some(package_name.to_owned()),
        update: Some(ProviderMaintenanceCommandAction {
            command,
            executable: executable.to_owned(),
            args,
            lock_key,
            env,
        }),
        latest_version: None,
    }
}

/// `npmGlobalPrefixFromCommandPath`.
fn npm_global_prefix_from_command_path(real_command_path: &str, package_name: &str) -> Option<String> {
    let slash_path = real_command_path.replace('\\', "/");
    let normalized = slash_path.to_lowercase();
    let segment = format!("/lib/node_modules/{}/", package_name.to_lowercase());
    let index = normalized.rfind(&segment)?;
    let before = &normalized[..index];
    if before.contains("/node_modules/") {
        return None;
    }
    // Mise's npm backend uses a global-looking layout inside a tool version.
    let mut parts = before.rsplit('/');
    let _version = parts.next();
    let tool = parts.next();
    let installs = parts.next();
    let mise = parts.next();
    if mise == Some("mise") && installs == Some("installs") && tool.is_some_and(|tool| tool != "node") {
        return None;
    }
    Some(if index == 0 { "/".to_owned() } else { slash_path[..index].to_owned() })
}

/// `resolveProviderMaintenanceCapabilitiesEffect` + `resolvePackageManagedProviderMaintenance` for
/// Codex: who owns the executable, and the one-click update command when that is proven.
pub fn resolve_codex_maintenance(maintenance: &CodexMaintenance, binary_path: &str, environment: &Environment) -> ProviderMaintenanceCapabilities {
    let package_name = maintenance.npm_package_name;
    let manual = ProviderMaintenanceCapabilities::manual_only(driver_kind(), Some(package_name.to_owned()));
    let Some(resolved) = resolve_command_path(binary_path, environment) else {
        return manual;
    };
    let Ok(real) = std::fs::canonicalize(&resolved) else {
        return manual;
    };
    let resolved = resolved.to_string_lossy().into_owned();
    let real = real.to_string_lossy().into_owned();
    let paths = [resolved.as_str(), real.as_str()];
    if paths.iter().any(|path| maintenance.is_native_command_path(path)) {
        let env = maintenance.native_update_env.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        return package_managed_capabilities(
            package_name,
            &resolved,
            maintenance.native_update_args.clone(),
            format!("{}-native", crate::DRIVER_KIND),
            Some(env),
        );
    }
    let any = |needles: &[&str]| {
        paths
            .iter()
            .any(|path| needles.iter().any(|needle| normalize_command_path(path).contains(needle)))
    };
    let latest = format!("{package_name}@latest");
    if any(&["/.vite-plus/bin/"]) {
        return package_managed_capabilities(
            package_name,
            "vp",
            vec!["i".into(), "-g".into(), package_name.into()],
            "vite-plus-global".into(),
            None,
        );
    }
    if any(&["/.bun/bin/"]) {
        return package_managed_capabilities(package_name, "bun", vec!["i".into(), "-g".into(), latest], "bun-global".into(), None);
    }
    if any(&[
        "/.local/share/pnpm/",
        "/library/pnpm/",
        "/local/share/pnpm/",
        "/appdata/local/pnpm/",
        "/pnpm/global/",
    ]) {
        return package_managed_capabilities(package_name, "pnpm", vec!["add".into(), "-g".into(), latest], "pnpm-global".into(), None);
    }
    if let Some(prefix) = npm_global_prefix_from_command_path(&real, package_name) {
        let lock_key = format!("npm-global:{}", normalize_command_path(&prefix));
        return package_managed_capabilities(
            package_name,
            "npm",
            vec![
                "install".into(),
                "-g".into(),
                "--prefix".into(),
                prefix,
                format!("--allow-scripts={package_name}"),
                latest,
            ],
            lock_key,
            None,
        );
    }
    // Homebrew kegs need `brew --prefix` / `brew info` to be proven: manual-only here.
    manual
}

/// `resolveLatestProviderVersion` for npm packages: `GET registry.npmjs.org/<pkg>/latest`
/// (4 s), cached an hour per package, `None` on any failure.
async fn resolve_npm_latest_version(fetcher: &dyn ManifestFetcher, package_name: &str) -> Option<String> {
    type LatestVersions = Mutex<HashMap<String, (Instant, Option<String>)>>;
    static CACHE: OnceLock<LatestVersions> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((at, version)) = cache.lock().unwrap().get(package_name) {
        if at.elapsed() < LATEST_VERSION_CACHE_TTL {
            return version.clone();
        }
    }
    let encoded = package_name.replace('@', "%40").replace('/', "%2F");
    let url = format!("https://registry.npmjs.org/{encoded}/latest");
    let version = match tokio::time::timeout(LATEST_VERSION_TIMEOUT, fetcher.fetch(&url)).await {
        Ok(Ok(body)) => body
            .get("version")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .map(str::to_owned),
        _ => None,
    };
    cache.lock().unwrap().insert(package_name.to_owned(), (Instant::now(), version.clone()));
    version
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_words_are_quoted_only_when_needed() {
        assert_eq!(quote_shell_word("/usr/local/bin/codex"), "/usr/local/bin/codex");
        assert_eq!(quote_shell_word("/Users/some one/codex"), "'/Users/some one/codex'");
        assert_eq!(quote_shell_word("it's"), "'it'\\''s'");
    }

    #[test]
    fn npm_prefixes_come_from_the_package_path() {
        assert_eq!(
            npm_global_prefix_from_command_path("/usr/local/lib/node_modules/@openai/codex/bin/codex.js", "@openai/codex").as_deref(),
            Some("/usr/local")
        );
        assert_eq!(
            npm_global_prefix_from_command_path("/proj/node_modules/x/lib/node_modules/@openai/codex/bin/codex.js", "@openai/codex"),
            None
        );
        assert_eq!(
            npm_global_prefix_from_command_path(
                "/home/sam/.local/share/mise/installs/npm-openai-codex/1.0.0/lib/node_modules/@openai/codex/bin/codex.js",
                "@openai/codex"
            ),
            None
        );
        assert_eq!(
            npm_global_prefix_from_command_path(
                "/home/sam/.local/share/mise/installs/node/22.0.0/lib/node_modules/@openai/codex/bin/codex.js",
                "@openai/codex"
            )
            .as_deref(),
            Some("/home/sam/.local/share/mise/installs/node/22.0.0")
        );
        assert_eq!(npm_global_prefix_from_command_path("/usr/local/bin/codex", "@openai/codex"), None);
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_follows_the_executable_layout() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let place = |relative: &str| {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        };
        let maintenance = crate::driver::codex_maintenance(Path::new("/shared/codex-home"));
        let empty = Environment::new();

        let standalone = place("home/.codex/packages/standalone/bin/codex");
        let capabilities = resolve_codex_maintenance(&maintenance, &standalone, &empty);
        let update = capabilities.update.expect("standalone updates natively");
        assert_eq!(update.args, vec!["update"]);
        assert_eq!(update.lock_key, "codex-native");
        assert_eq!(update.env, Some(vec![("CODEX_HOME".to_owned(), "/shared/codex-home".to_owned())]));

        let npm = place("prefix/lib/node_modules/@openai/codex/bin/codex.js");
        let capabilities = resolve_codex_maintenance(&maintenance, &npm, &empty);
        let update = capabilities.update.expect("npm global updates through npm");
        assert_eq!(update.executable, "npm");
        assert!(update.lock_key.starts_with("npm-global:"));
        assert_eq!(update.args.last().map(String::as_str), Some("@openai/codex@latest"));

        let plain = place("bin/codex");
        let on_path: Environment = [("PATH".to_owned(), root.path().join("bin").to_string_lossy().into_owned())]
            .into_iter()
            .collect();
        let capabilities = resolve_codex_maintenance(&maintenance, "codex", &on_path);
        assert_eq!(
            capabilities,
            ProviderMaintenanceCapabilities::manual_only(driver_kind(), Some("@openai/codex".into()))
        );
        assert_eq!(
            resolve_command_path("codex", &on_path).map(|path| path.to_string_lossy().into_owned()),
            Some(plain)
        );
        assert_eq!(resolve_command_path("codex", &empty), None);
        assert_eq!(resolve_command_path("/nowhere/codex", &empty), None);
    }

    #[tokio::test]
    async fn redemptions_keep_their_key_until_an_outcome() {
        let account = PathBuf::from("/made-up/codex-account-for-redemption");
        let first: Result<String, ()> = redeem_reset_credit(account.clone(), |key| async move {
            assert!(!key.is_empty());
            Err(())
        })
        .await;
        assert!(first.is_err());
        let keys = Arc::new(Mutex::new(Vec::new()));
        for _ in 0..2 {
            let keys = keys.clone();
            let outcome: Result<String, ()> = redeem_reset_credit(account.clone(), move |key| async move {
                keys.lock().unwrap().push(key);
                Ok("reset".to_owned())
            })
            .await;
            assert_eq!(outcome.unwrap(), "reset");
        }
        let keys = keys.lock().unwrap().clone();
        // The retry after the failure re-sent the same attempt; the next redemption is new.
        assert_ne!(keys[0], keys[1]);
    }

    #[tokio::test]
    async fn npm_latest_versions_are_read_and_cached() {
        struct Registry(Mutex<Vec<String>>);
        #[async_trait]
        impl ManifestFetcher for Registry {
            async fn fetch(&self, url: &str) -> Result<Value, String> {
                self.0.lock().unwrap().push(url.to_owned());
                Ok(json!({"version": "9.8.7"}))
            }
        }
        let registry = Registry(Mutex::new(Vec::new()));
        let package = "@made-up/codex-cache-test";
        assert_eq!(resolve_npm_latest_version(&registry, package).await.as_deref(), Some("9.8.7"));
        assert_eq!(resolve_npm_latest_version(&registry, package).await.as_deref(), Some("9.8.7"));
        assert_eq!(
            *registry.0.lock().unwrap(),
            vec!["https://registry.npmjs.org/%40made-up%2Fcodex-cache-test/latest".to_owned()]
        );
    }
}
