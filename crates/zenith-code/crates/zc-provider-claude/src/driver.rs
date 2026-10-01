//! `provider/Drivers/ClaudeDriver.ts`: one Claude provider instance, built from its
//! `ClaudeSettings` and environment. The provider core (WP-12) owns the generic driver trait,
//! the snapshot manager (`makeManagedServerProvider`), maintenance/update advisories and the
//! reset-credit coordinator; this module hands it the Claude-specific pieces:
//!
//! - [`ClaudeInstance::adapter`]: the [`ClaudeAdapter`];
//! - [`ClaudeInstance::check_status`]: the status probe (`checkClaudeProviderStatus` with the
//!   5-minute capabilities cache keyed by binary + config dir + cwd);
//! - [`ClaudeInstance::pending_snapshot`], [`ClaudeInstance::skills_for_cwd`];
//! - [`ClaudeInstance::consume_reset_credit`] (`consumeResetCredit` minus the coordinator);
//! - the continuation key (`claude:home:<config dir>`) and the update resolver inputs.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use zc_contracts::{ClaudeSettings, ProviderInstanceId};

use crate::adapter::{CatalogSource, ClaudeAdapter, ClaudeAdapterOptions};
use crate::home::{capabilities_cache_key, continuation_group_key, resolve_claude_home_path, Env};
use crate::provider::{
    check_claude_provider_status, make_pending_claude_provider, probe_claude_capabilities, run_version_probe, ClaudeCapabilitiesProbe, StatusProbeDeps,
};
use crate::reset_credits::{
    claude_account_config_path, consume_claude_reset_credit, read_claude_reset_credits, ClaimOutcome, ClaudeResetCreditError, ReqwestResetCreditsHttp,
    ResetCreditsHttp,
};
use crate::skills::{discover_claude_skills, host_platform, ClaudeSkill};
use crate::usage_limits::{make_scoped_limit_names, ScopedLimitNamesRef};

/// How long a capabilities probe result is reused.
pub const CAPABILITIES_PROBE_TTL: Duration = Duration::from_secs(5 * 60);

/// The npm package and native-update shape the maintenance resolver uses for Claude.
pub const NPM_PACKAGE_NAME: &str = "@anthropic-ai/claude-code";
pub const NATIVE_UPDATE_ARGS: [&str; 1] = ["update"];

/// `isClaudeNativeCommandPath`: a native install updates itself with `claude update`.
pub fn is_claude_native_command_path(command_path: &str) -> bool {
    let normalized = command_path.replace('\\', "/").to_lowercase();
    normalized.ends_with("/.local/bin/claude") || normalized.ends_with("/.local/bin/claude.exe") || normalized.contains("/.local/share/claude/")
}

/// `ClaudeDriver`: metadata and defaults.
pub struct ClaudeDriver;

impl ClaudeDriver {
    pub const DRIVER_KIND: &'static str = crate::DRIVER_KIND;
    pub const DISPLAY_NAME: &'static str = "Claude";
    pub const SUPPORTS_MULTIPLE_INSTANCES: bool = true;

    /// `defaultConfig()`: `ClaudeSettings` decoded from `{}`.
    pub fn default_config() -> ClaudeSettings {
        serde_json::from_value(serde_json::json!({})).expect("ClaudeSettings has defaults for every field")
    }
}

/// What an instance is created from (`ProviderDriver.create` input).
pub struct ClaudeInstanceInput {
    pub instance_id: ProviderInstanceId,
    pub enabled: bool,
    pub config: ClaudeSettings,
    /// `mergeProviderInstanceEnvironment(environment)`: process env + instance overrides.
    pub environment: Env,
    /// `ServerConfig.cwd` (where status probes run).
    pub server_cwd: Option<String>,
    pub attachments_dir: PathBuf,
    pub catalog: CatalogSource,
}

struct CachedProbe {
    key: String,
    at: Instant,
    value: Option<ClaudeCapabilitiesProbe>,
}

/// One Claude instance.
pub struct ClaudeInstance {
    pub instance_id: ProviderInstanceId,
    pub settings: ClaudeSettings,
    pub environment: Env,
    pub server_cwd: Option<String>,
    pub continuation_key: String,
    pub config_dir: PathBuf,
    pub account_config_path: PathBuf,
    pub scoped_limit_names: ScopedLimitNamesRef,
    pub catalog: CatalogSource,
    adapter: ClaudeAdapter,
    probe_cache: Mutex<Option<CachedProbe>>,
    http: Arc<dyn ResetCreditsHttp>,
}

impl ClaudeInstance {
    /// `ClaudeDriver.create(...)` with default adapter wiring; use [`Self::with_adapter_options`]
    /// to inject MCP sessions, a native event logger, history ops or a query factory.
    pub fn new(input: ClaudeInstanceInput) -> Self {
        Self::with_adapter_options(input, |options| options)
    }

    pub fn with_adapter_options(input: ClaudeInstanceInput, customize: impl FnOnce(ClaudeAdapterOptions) -> ClaudeAdapterOptions) -> Self {
        let mut settings = input.config;
        settings.enabled = input.enabled;
        settings.binary_path = zc_core::paths::expand_home_path(&settings.binary_path).to_string_lossy().into_owned();
        let continuation_key = continuation_group_key(&settings.home_path, Some(&input.environment));
        let config_dir = resolve_claude_home_path(&settings.home_path, Some(&input.environment));
        let explicit_dir = !settings.home_path.trim().is_empty() || input.environment.get("CLAUDE_CONFIG_DIR").is_some_and(|v| !v.trim().is_empty());
        let account_config_path = claude_account_config_path(explicit_dir.then_some(config_dir.as_path()));
        let scoped_limit_names = make_scoped_limit_names();
        let mut options = ClaudeAdapterOptions::new(settings.clone(), input.instance_id.clone(), input.environment.clone(), input.attachments_dir);
        options.catalog = input.catalog.clone();
        options.scoped_limit_names = scoped_limit_names.clone();
        let adapter = ClaudeAdapter::new(customize(options));
        Self {
            instance_id: input.instance_id,
            settings,
            environment: input.environment,
            server_cwd: input.server_cwd,
            continuation_key,
            config_dir,
            account_config_path,
            scoped_limit_names,
            catalog: input.catalog,
            adapter,
            probe_cache: Mutex::new(None),
            http: Arc::new(ReqwestResetCreditsHttp::default()),
        }
    }

    pub fn adapter(&self) -> &ClaudeAdapter {
        &self.adapter
    }

    /// Drop the cached capabilities probe (`invalidateCaches`).
    pub fn invalidate_caches(&self) {
        *self.probe_cache.lock().unwrap_or_else(|p| p.into_inner()) = None;
    }

    async fn cached_capabilities(&self) -> Option<ClaudeCapabilitiesProbe> {
        let key = capabilities_cache_key(
            &self.settings.binary_path,
            &self.settings.home_path,
            self.server_cwd.as_deref(),
            Some(&self.environment),
        );
        if let Some(cached) = self.probe_cache.lock().unwrap_or_else(|p| p.into_inner()).as_ref() {
            if cached.key == key && cached.at.elapsed() < CAPABILITIES_PROBE_TTL {
                return cached.value.clone();
            }
        }
        let value = probe_claude_capabilities(&self.settings, &self.environment, self.server_cwd.as_deref()).await;
        *self.probe_cache.lock().unwrap_or_else(|p| p.into_inner()) = Some(CachedProbe {
            key,
            at: Instant::now(),
            value: value.clone(),
        });
        value
    }

    /// The pending snapshot draft (before the first probe).
    pub fn pending_snapshot(&self) -> Value {
        make_pending_claude_provider(&self.settings, &(self.catalog)(), &zc_core::now_iso())
    }

    /// The status probe: a `ServerProviderDraft` JSON.
    pub async fn check_status(&self) -> Value {
        let catalog = (self.catalog)();
        let config_dir = self.config_dir.clone();
        let http = self.http.clone();
        check_claude_provider_status(
            &self.settings,
            StatusProbeDeps {
                environment: &self.environment,
                cwd: self.server_cwd.as_deref(),
                catalog: &catalog,
                scoped_limit_names: Some(&self.scoped_limit_names),
                checked_at: zc_core::now_iso(),
                version: Box::pin(run_version_probe(&self.settings, &self.environment)),
                capabilities: Some(Box::pin(self.cached_capabilities())),
                reset_credits: Some(Box::new(move |version| {
                    Box::pin(async move { read_claude_reset_credits(http.as_ref(), &config_dir, &version, host_platform(), zc_core::now_millis()).await })
                })),
            },
        )
        .await
    }

    /// `snapshotForCwd`: the skills a workspace sees (the caller merges them into the snapshot).
    pub fn skills_for_cwd(&self, cwd: &str) -> Option<Vec<ClaudeSkill>> {
        self.settings
            .enabled
            .then(|| discover_claude_skills(&self.settings.home_path, Some(cwd), &self.environment))
    }

    /// Redeem a banked reset (`ClaudeResetCredits.consumeClaudeResetCredit`). The caller holds
    /// the coordinator (one request id per grant until Claude answers) and re-probes afterwards.
    pub async fn consume_reset_credit(&self, version: &str, grant_id: &str, request_id: &str) -> Result<ClaimOutcome, ClaudeResetCreditError> {
        consume_claude_reset_credit(
            self.http.as_ref(),
            &self.config_dir,
            &self.account_config_path,
            version,
            grant_id,
            request_id,
            host_platform(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_native_installs() {
        assert!(is_claude_native_command_path("/Users/someone/.local/bin/claude"));
        assert!(is_claude_native_command_path("/home/x/.local/share/claude/versions/2.1.0"));
        assert!(!is_claude_native_command_path("/opt/homebrew/bin/claude"));
    }

    #[tokio::test]
    async fn builds_an_instance_keyed_by_its_config_dir() {
        let mut config = ClaudeDriver::default_config();
        config.home_path = "/tmp/claude-instance-home".into();
        let instance = ClaudeInstance::new(ClaudeInstanceInput {
            instance_id: ProviderInstanceId::from("claude-work"),
            enabled: true,
            config,
            environment: Env::new(),
            server_cwd: None,
            attachments_dir: PathBuf::from("/tmp/attachments"),
            catalog: Arc::new(crate::catalog::ClaudeModelCatalog::bundled),
        });
        assert_eq!(instance.continuation_key, "claude:home:/tmp/claude-instance-home");
        assert_eq!(instance.account_config_path, PathBuf::from("/tmp/claude-instance-home/.claude.json"));
        assert_eq!(
            instance.adapter().claude_environment().get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some("/tmp/claude-instance-home")
        );
        assert_eq!(
            instance.pending_snapshot()["message"],
            serde_json::json!("Claude provider status has not been checked in this session yet.")
        );
    }
}
