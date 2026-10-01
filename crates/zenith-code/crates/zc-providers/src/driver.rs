//! The driver SPI: port of `provider/ProviderDriver.ts`, `Services/ServerProvider.ts`,
//! `ProviderInstanceEnvironment.ts` and the admission half of `ProviderAuthController`.
//!
//! A [`Driver`] is a plain value registered at startup (the Rust `BUILT_IN_DRIVERS` is the list
//! the server wiring hands to [`crate::ProviderInstanceRegistry`]). For every configured
//! instance the registry decodes the opaque `providerInstances[id].config` blob with
//! [`Driver::decode_config`] and calls [`Driver::create`] with a fresh [`InstanceScope`]. The
//! returned [`ProviderInstance`] bundles everything that instance owns: its
//! [`ProviderAdapter`], its status snapshot source, its text generation. Two instances of one
//! driver must share no mutable state; closing the scope (on removal, replacement or shutdown)
//! must release every process, task and file the instance opened.
//!
//! What TS drivers pulled from Effect's `R` channel (loggers, model manifest, settings,
//! background policy, server paths) a Rust driver receives at construction, usually as a
//! [`DriverEnv`].

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use zc_contracts::{ProviderDriverKind, ProviderInstanceEnvironmentVariable, ProviderInstanceId, ProviderUsageLimitsUpdate, ServerProvider};
use zc_ports::adapter::ProviderAdapter;
use zc_ports::{BackgroundPolicy, EventStream, SettingsService, TextGeneration};

use crate::errors::ProviderDriverError;
use crate::logger::ProviderEventLoggers;
use crate::managed::ManagedServerProviderOptions;
use crate::manifest::ModelManifest;
use crate::snapshot::ProviderMaintenanceCapabilities;

/// `BUILT_IN_DRIVERS` order (`builtInDrivers.ts`): the order the server wiring registers the
/// drivers in. The kinds are persisted and never change.
pub const BUILT_IN_DRIVER_KINDS: &[&str] = &["codex", "claudeAgent", "cursor", "grok", "opencode", "antigravity"];

/// The shared environment the built-in drivers are constructed with (`BuiltInDriversEnv`).
#[derive(Clone)]
pub struct DriverEnv {
    /// `native` for the adapters' `NTIVE:` lines.
    pub event_loggers: ProviderEventLoggers,
    pub model_manifest: ModelManifest,
    pub settings: Arc<dyn SettingsService>,
    pub background_policy: Option<Arc<dyn BackgroundPolicy>>,
    /// `ServerConfig.cwd` (where probes run).
    pub server_cwd: PathBuf,
    /// `ServerConfig.stateDir` (`<baseDir>/userdata`).
    pub state_dir: PathBuf,
    pub attachments_dir: PathBuf,
    /// The server's process environment, under each instance's variables.
    pub base_env: HashMap<String, String>,
    /// The thread's `t3-code` MCP session (`readMcpProviderSession`), for the drivers that hand
    /// it to their agent (Claude `--mcp-config`, Codex `-c mcp_servers.t3-code.*`).
    pub mcp_sessions: Option<Arc<dyn crate::hooks::McpSessionReader>>,
}

impl DriverEnv {
    /// The `makeManagedServerProvider` wiring for one instance (health interval from settings,
    /// demand from the background policy).
    pub fn managed_options(&self, scope: InstanceScope) -> ManagedServerProviderOptions {
        let mut options = ManagedServerProviderOptions::new(scope);
        options.server_settings = Some(self.settings.clone());
        options.background_policy = self.background_policy.clone();
        options
    }

    /// `mergeProviderInstanceEnvironment(environment, process.env)`.
    pub fn instance_env(&self, environment: &[ProviderInstanceEnvironmentVariable]) -> HashMap<String, String> {
        merge_provider_instance_environment(environment, &self.base_env)
    }
}

/// `ProviderDriverMetadata`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverMetadata {
    /// Human-readable name of the driver ("Codex").
    pub display_name: String,
    /// False for drivers wrapping a global resource; the registry then rejects a second instance.
    pub supports_multiple_instances: bool,
}

/// `ProviderContinuationIdentity`: sessions can only be continued by an instance with the same
/// key (e.g. two Claude instances sharing one `CLAUDE_CONFIG_DIR`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContinuationIdentity {
    pub driver_kind: ProviderDriverKind,
    pub continuation_key: String,
}

impl ContinuationIdentity {
    /// `defaultProviderContinuationIdentity`: `<driverKind>:instance:<instanceId>`.
    pub fn default_for(driver_kind: &ProviderDriverKind, instance_id: &ProviderInstanceId) -> Self {
        Self {
            driver_kind: driver_kind.clone(),
            continuation_key: format!("{driver_kind}:instance:{instance_id}"),
        }
    }
}

/// The lifetime of one instance (the Effect child `Scope` the registry creates per instance).
///
/// Drivers spawn their background work with [`InstanceScope::spawn`] (aborted on close), watch
/// [`InstanceScope::token`] in long-lived loops, and register teardown with
/// [`InstanceScope::add_finalizer`] (run in reverse order on close, e.g. `adapter.stop_all()`).
/// Closing is idempotent.
#[derive(Clone, Default)]
pub struct InstanceScope {
    inner: Arc<ScopeInner>,
}

#[derive(Default)]
struct ScopeInner {
    token: CancellationToken,
    closed: AtomicBool,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    finalizers: Mutex<Vec<BoxFuture<'static, ()>>>,
}

impl std::fmt::Debug for InstanceScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstanceScope").field("closed", &self.is_closed()).finish()
    }
}

impl InstanceScope {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancelled when the scope closes.
    pub fn token(&self) -> CancellationToken {
        self.inner.token.clone()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Run `task` until it finishes or the scope closes.
    pub fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.is_closed() {
            return;
        }
        let token = self.inner.token.clone();
        let handle = tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {}
                _ = task => {}
            }
        });
        let mut tasks = self.inner.tasks.lock().unwrap();
        tasks.retain(|task| !task.is_finished());
        tasks.push(handle);
    }

    /// Run `finalizer` when the scope closes (immediately-ignored if already closed).
    pub fn add_finalizer<F>(&self, finalizer: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if self.is_closed() {
            return;
        }
        self.inner.finalizers.lock().unwrap().push(Box::pin(finalizer));
    }

    /// `Scope.close`: cancel the token, abort spawned tasks, run finalizers newest first.
    pub async fn close(&self) {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        self.inner.token.cancel();
        for task in self.inner.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        let finalizers: Vec<_> = self.inner.finalizers.lock().unwrap().drain(..).collect();
        for finalizer in finalizers.into_iter().rev() {
            finalizer.await;
        }
    }
}

/// `ServerProviderShape`: one instance's status snapshot (install, auth, version, models).
#[async_trait]
pub trait ServerProviderSource: Send + Sync {
    /// `getSnapshot`: the current snapshot, never probing.
    async fn get_snapshot(&self) -> ServerProvider;
    /// `refresh`: probe now and return the new snapshot.
    async fn refresh(&self) -> ServerProvider;
    /// `streamChanges`: every snapshot published after this call returns (eager, unbounded).
    /// Ends when the instance scope closes.
    fn subscribe_changes(&self) -> EventStream<ServerProvider>;
    /// `resolveMaintenance({fresh})`: who owns the executable and how it updates.
    async fn resolve_maintenance(&self, fresh: bool) -> ProviderMaintenanceCapabilities;
    /// `applyUsageLimits`: fold a runtime rate-limit update into the published snapshot.
    async fn apply_usage_limits(&self, update: ProviderUsageLimitsUpdate, checked_at: String);
}

/// The part of `ProviderAuthController` routing needs: shared credentials and the admission
/// gate around session startup. WP-12b's auth controller implements it (and more).
#[async_trait]
pub trait ProviderAuthAdmission: Send + Sync {
    /// `credentialBinding`: equal `(owner, key)` means these instances share credentials.
    fn credential_binding(&self) -> Option<(String, String)> {
        None
    }
    /// `isChangingCredentials`.
    async fn is_changing_credentials(&self) -> bool {
        false
    }
    /// `withAccess`: hold the returned guard while a session starts; a credential change waits
    /// for (or interrupts) admitted startups. `Err(detail)` is a `ProviderSetupError`.
    async fn begin_access(&self) -> Result<Option<Box<dyn std::any::Any + Send>>, String> {
        Ok(None)
    }
    /// Whether this controller gates startup at all (TS: any of `withAccess`,
    /// `isChangingCredentials`, `credentialBinding` present).
    fn guards_startup(&self) -> bool {
        true
    }
}

/// `snapshotForCwd(cwd)`: workspace-scoped skills and commands.
pub type SnapshotForCwd = Arc<dyn Fn(String) -> BoxFuture<'static, Result<ServerProvider, ProviderDriverError>> + Send + Sync>;
/// `refreshModels()` / `invalidateCaches`.
pub type InstanceHook = Arc<dyn Fn() -> BoxFuture<'static, Result<(), ProviderDriverError>> + Send + Sync>;
/// `consumeResetCredit()`: the wire `ProviderConsumeResetCreditOutcome`.
pub type ConsumeResetCredit = Arc<dyn Fn() -> BoxFuture<'static, Result<Value, ProviderDriverError>> + Send + Sync>;

/// `ProviderInstance`: one materialized, configured instance.
#[derive(Clone)]
pub struct ProviderInstance {
    pub instance_id: ProviderInstanceId,
    pub driver_kind: ProviderDriverKind,
    pub continuation_identity: ContinuationIdentity,
    pub display_name: Option<String>,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub snapshot: Arc<dyn ServerProviderSource>,
    pub snapshot_for_cwd: Option<SnapshotForCwd>,
    pub refresh_models: Option<InstanceHook>,
    /// Invalidate discovery caches before an explicit refresh.
    pub invalidate_caches: Option<InstanceHook>,
    pub consume_reset_credit: Option<ConsumeResetCredit>,
    pub adapter: Arc<dyn ProviderAdapter>,
    /// `None` when the driver has no one-shot text generation (WP-17 fills it in).
    pub text_generation: Option<Arc<dyn TextGeneration>>,
    pub auth: Option<Arc<dyn ProviderAuthAdmission>>,
}

impl std::fmt::Debug for ProviderInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderInstance")
            .field("instance_id", &self.instance_id)
            .field("driver_kind", &self.driver_kind)
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

/// `ProviderDriverCreateInput`.
#[derive(Debug, Clone)]
pub struct DriverCreateInput {
    pub instance_id: ProviderInstanceId,
    pub display_name: Option<String>,
    pub accent_color: Option<String>,
    pub environment: Vec<ProviderInstanceEnvironmentVariable>,
    pub enabled: bool,
    /// The output of [`Driver::decode_config`].
    pub config: Value,
    /// The instance's lifetime; closed by the registry on removal, replacement and shutdown.
    pub scope: InstanceScope,
}

/// `ProviderDriver`: what WP-13 (Claude), WP-14 (Codex), WP-15 (ACP) and WP-16 (OpenCode)
/// implement.
#[async_trait]
pub trait Driver: Send + Sync {
    /// The persisted driver-kind string (`claudeAgent`, `codex`, `cursor`, …). Never changes.
    fn driver_kind(&self) -> ProviderDriverKind;
    fn metadata(&self) -> DriverMetadata;
    /// `configSchema` decode of the opaque `config` blob (with the schema's decoding defaults
    /// applied). An `Err` makes the instance an unavailable snapshot with this detail.
    fn decode_config(&self, raw: &Value) -> Result<Value, String>;
    /// `defaultConfig()`: used when the envelope carries no `config`.
    fn default_config(&self) -> Value;
    /// Materialize one instance. Failures become unavailable snapshots; never panic.
    async fn create(&self, input: DriverCreateInput) -> Result<ProviderInstance, ProviderDriverError>;
}

/// `mergeProviderInstanceEnvironment(environment, baseEnv)`: the instance's variables over the
/// server environment (`CODEX_HOME` / `CLAUDE_CONFIG_DIR` get `~` expanded, since children do
/// not expand them).
pub fn merge_provider_instance_environment(environment: &[ProviderInstanceEnvironmentVariable], base: &HashMap<String, String>) -> HashMap<String, String> {
    let mut next = base.clone();
    for variable in environment {
        let value = if variable.name == "CODEX_HOME" || variable.name == "CLAUDE_CONFIG_DIR" {
            zc_core::expand_home_path(&variable.value).to_string_lossy().into_owned()
        } else {
            variable.value.clone()
        };
        next.insert(variable.name.clone(), value);
    }
    next
}

/// The server process environment as a map (the `baseEnv` default).
pub fn process_environment() -> HashMap<String, String> {
    std::env::vars().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scopes_close_once_and_run_finalizers_in_reverse() {
        let scope = InstanceScope::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        for label in ["first", "second"] {
            let order = order.clone();
            scope.add_finalizer(async move { order.lock().unwrap().push(label) });
        }
        let token = scope.token();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        scope.spawn(async move {
            let _keep = tx;
            futures::future::pending::<()>().await;
        });
        scope.close().await;
        scope.close().await;
        assert!(token.is_cancelled());
        assert!(rx.await.is_err());
        assert_eq!(*order.lock().unwrap(), vec!["second", "first"]);
    }

    #[test]
    fn merges_instance_environment() {
        let base = HashMap::from([("PATH".to_owned(), "/bin".to_owned())]);
        let variables: Vec<ProviderInstanceEnvironmentVariable> =
            serde_json::from_value(serde_json::json!([{"name": "CODEX_HOME", "value": "~/codex-work"}, {"name": "TOKEN", "value": "x", "sensitive": true}]))
                .unwrap();
        let merged = merge_provider_instance_environment(&variables, &base);
        assert_eq!(merged["PATH"], "/bin");
        assert_eq!(merged["TOKEN"], "x");
        assert!(!merged["CODEX_HOME"].starts_with('~'));
        assert_eq!(
            ContinuationIdentity::default_for(&"codex".into(), &"codex_work".into()).continuation_key,
            "codex:instance:codex_work"
        );
    }
}
