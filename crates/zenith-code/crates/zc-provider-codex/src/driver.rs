//! The Codex driver's per-instance pieces (`provider/Drivers/CodexDriver.ts`,
//! `Drivers/CodexManagedProvider.ts`). The provider core (WP-12) owns the instance registry, the
//! managed snapshot (`makeManagedServerProvider`), the model manifest, maintenance and text
//! generation; this module gives it everything Codex-specific:
//!
//! - [`create_codex_instance`]: home layout + shadow home, effective settings, continuation
//!   identity, the adapter, and the status check / skills probe / reset-credit closures;
//! - [`create_managed_codex_instance`]: the same for `setupMode: "managed"` (ChatGPT sharing),
//!   with the sign-in and installation seams of WP-12b;
//! - [`codex_maintenance`]: what the package-managed updater needs.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use zc_contracts::{CodexSettings, ProviderInstanceEnvironment, ProviderInstanceId, ServerProviderSkill};
use zc_ports::provider::ProviderContinuationIdentity;

use crate::adapter::{process_environment, CodexAdapter, CodexAdapterOptions};
use crate::home_layout::{codex_continuation_identity, materialize_codex_shadow_home, resolve_codex_home_layout, CodexHomeLayout};
use crate::launch_args::{resolve_codex_launch_args, Environment};
use crate::managed::{managed_auth, managed_runtime_paths, managed_usage_limits, ChatGptAccessSource, ManagedCodexRuntime, ManagedExecutableSource};
use crate::provider_status::{check_codex_provider_status, consume_reset_credit, make_pending_codex_provider, probe_codex_skills_for_cwd, ProbeFn, ProbeInput};

/// `ProviderDriverError`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct ProviderDriverError {
    pub driver: String,
    pub instance_id: String,
    pub detail: String,
}

fn driver_error(instance_id: &ProviderInstanceId, detail: impl Into<String>) -> ProviderDriverError {
    ProviderDriverError {
        driver: crate::DRIVER_KIND.to_owned(),
        instance_id: instance_id.as_str().to_owned(),
        detail: detail.into(),
    }
}

/// `mergeProviderInstanceEnvironment`: the instance's variables over the server's environment
/// (`CODEX_HOME` / `CLAUDE_CONFIG_DIR` get `~` expanded: children do not shell-expand).
pub fn merge_provider_instance_environment(environment: &ProviderInstanceEnvironment, base: Environment) -> Environment {
    let mut next = base;
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

/// `ProviderDriverCreateInput<CodexSettings>`.
#[derive(Debug, Clone)]
pub struct CodexDriverCreateInput {
    pub instance_id: ProviderInstanceId,
    pub display_name: Option<String>,
    pub accent_color: Option<String>,
    pub environment: ProviderInstanceEnvironment,
    pub enabled: bool,
    pub config: CodexSettings,
}

/// Codex's maintenance description (`makeCodexMaintenanceResolver`): npm package
/// `@openai/codex`, or `codex update` for the standalone install, run against the shared home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexMaintenance {
    pub npm_package_name: &'static str,
    pub native_update_args: Vec<String>,
    pub native_update_env: Environment,
}

impl CodexMaintenance {
    /// `isCodexStandaloneCommandPath`: the standalone installer lays out
    /// `<CODEX_HOME>/packages/standalone/…`.
    pub fn is_native_command_path(&self, command_path: &str) -> bool {
        command_path.replace('\\', "/").to_lowercase().contains("/packages/standalone/")
    }
}

pub fn codex_maintenance(shared_home_path: &std::path::Path) -> CodexMaintenance {
    CodexMaintenance {
        npm_package_name: "@openai/codex",
        native_update_args: vec!["update".to_owned()],
        native_update_env: [("CODEX_HOME".to_owned(), shared_home_path.to_string_lossy().into_owned())]
            .into_iter()
            .collect(),
    }
}

/// One materialized Codex instance (the Codex parts of `ProviderInstance`).
pub struct CodexInstance {
    pub instance_id: ProviderInstanceId,
    pub display_name: Option<String>,
    pub accent_color: Option<String>,
    pub enabled: bool,
    pub continuation_identity: ProviderContinuationIdentity,
    pub home_layout: CodexHomeLayout,
    /// Settings with the binary path expanded and the effective home.
    pub effective_config: CodexSettings,
    pub environment: Environment,
    pub adapter: CodexAdapter,
    pub maintenance: CodexMaintenance,
    probe: Option<ProbeFn>,
    managed: Option<Arc<ManagedCodexRuntime>>,
}

impl CodexInstance {
    /// The snapshot draft before the first check (`initialSnapshot`).
    pub fn pending_snapshot(&self, checked_at: &str) -> Value {
        let draft = make_pending_codex_provider(&self.effective_config, checked_at);
        match &self.managed {
            Some(managed) => self.managed_draft(draft, managed),
            None => draft,
        }
    }

    fn managed_draft(&self, mut draft: Value, managed: &ManagedCodexRuntime) -> Value {
        draft["models"] = json!([]);
        draft["setup"] = json!({"canAuthenticate": true, "canInstall": true});
        draft["runtimePaths"] = managed_runtime_paths(&managed.home_layout, &managed.home_path);
        draft
    }

    /// `checkProvider` (the manifest / identity stamping stay WP-12's).
    pub async fn check_provider(&self, cwd: &str, checked_at: &str) -> Value {
        match &self.managed {
            None => check_codex_provider_status(&self.effective_config, self.probe.clone(), Some(&self.environment), None, cwd, checked_at).await,
            Some(managed) => self.check_managed(managed, cwd, checked_at).await,
        }
    }

    /// `CodexManagedProvider`'s check: installation, sign-in, sharing scope, then the probe
    /// with the managed runtime (no service tiers, no `/feedback`, ChatGPT usage link).
    async fn check_managed(&self, managed: &ManagedCodexRuntime, cwd: &str, checked_at: &str) -> Value {
        let mut base = self.managed_draft(make_pending_codex_provider(&self.effective_config, checked_at), managed);
        if !self.enabled {
            return base;
        }
        let Some(executable) = managed.installation.resolve().await else {
            base["installed"] = json!(false);
            base["message"] = json!("Set up Codex to get started.");
            base["auth"] = json!({"status": "unauthenticated"});
            return base;
        };
        base["installed"] = json!(true);
        base["version"] = json!(executable.version);
        let saved = managed.auth.read_account().await;
        let email = saved
            .as_ref()
            .and_then(|account| account.email.as_deref().map(str::trim).filter(|email| !email.is_empty()).map(str::to_owned));
        match &saved {
            Some(account) if !account.can_share_tokens() => {
                base["message"] =
                    json!("Signed in with ChatGPT, but token sharing is disabled. Sign in again and enable token sharing, or use another provider.");
                base["auth"] = json!({"status": "unauthenticated", "label": "ChatGPT"});
                if let Some(email) = &email {
                    base["auth"]["email"] = json!(email);
                }
                return base;
            }
            None => {
                base["message"] = json!("Sign in with ChatGPT to use Codex.");
                base["auth"] = json!({"status": "unauthenticated"});
                return base;
            }
            Some(_) => {}
        }
        let account = saved.expect("checked above");
        let auth = managed_auth(&account);
        let checked = async {
            let effective = managed.resolve().await.map_err(|error| error.detail)?;
            let mut draft = check_codex_provider_status(
                &effective.config,
                self.probe.clone(),
                Some(&effective.environment),
                Some(auth.clone()),
                cwd,
                checked_at,
            )
            .await;
            // TS then filters the catalog through `chatGptModels` (CodexChatGptModels.ts, an
            // HTTP call with the token): WP-12b.
            // Service tiers are not offered through sharing; `/feedback` needs native auth.
            if let Some(models) = draft["models"].as_array_mut() {
                for model in models {
                    if let Some(descriptors) = model["capabilities"]["optionDescriptors"].as_array_mut() {
                        descriptors.retain(|descriptor| descriptor["id"] != "serviceTier");
                    }
                }
            }
            if let Some(commands) = draft["slashCommands"].as_array_mut() {
                commands.retain(|command| command["name"] != "feedback");
            }
            draft["auth"] = auth.clone();
            if draft["version"].is_null() {
                draft["version"] = json!(executable.version);
            }
            draft["usageLimits"] = managed_usage_limits(checked_at);
            draft["setup"] = json!({"canAuthenticate": true, "canInstall": true});
            draft["runtimePaths"] = managed_runtime_paths(&managed.home_layout, &managed.home_path);
            Ok::<Value, String>(draft)
        }
        .await;
        match checked {
            Ok(draft) => draft,
            Err(_) => {
                let current = managed.auth.read_account().await;
                let sharing = current.as_ref().is_some_and(|account| account.can_share_tokens());
                let mut auth = json!({"status": if sharing { "authenticated" } else { "unauthenticated" }, "label": "ChatGPT"});
                if sharing {
                    let account = current.as_ref().expect("sharing account");
                    auth["subscriptionSharing"] = json!(true);
                    auth["profileId"] = json!(account.client_id);
                }
                if let Some(email) = current
                    .as_ref()
                    .and_then(|account| account.email.as_deref().map(str::trim).filter(|email| !email.is_empty()))
                {
                    auth["email"] = json!(email);
                }
                base["auth"] = auth;
                if sharing {
                    base["usageLimits"] = managed_usage_limits(checked_at);
                }
                base["message"] = json!("Could not check Codex right now. Retry, or reconnect in provider settings.");
                base
            }
        }
    }

    fn probe_input(&self, cwd: &str) -> ProbeInput {
        ProbeInput {
            binary_path: self.effective_config.binary_path.clone(),
            home_path: Some(self.effective_config.home_path.clone()).filter(|home| !home.is_empty()),
            launch_args: Some(resolve_codex_launch_args(Some(&self.effective_config.launch_args), &self.environment)),
            cwd: cwd.to_owned(),
            environment: Some(self.environment.clone()),
        }
    }

    /// `snapshotForCwd`'s skills probe (20 s).
    pub async fn skills_for_cwd(&self, cwd: &str) -> Result<Vec<ServerProviderSkill>, ProviderDriverError> {
        let input = match &self.managed {
            None => self.probe_input(cwd),
            Some(managed) => {
                let effective = managed.resolve().await.map_err(|error| driver_error(&self.instance_id, error.detail))?;
                ProbeInput {
                    binary_path: effective.config.binary_path.clone(),
                    home_path: Some(effective.config.home_path.clone()).filter(|home| !home.is_empty()),
                    launch_args: Some(effective.config.launch_args.clone()),
                    cwd: cwd.to_owned(),
                    environment: Some(effective.environment),
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(20), probe_codex_skills_for_cwd(&input))
            .await
            .map_err(|_| driver_error(&self.instance_id, format!("Failed to probe Codex skills for '{cwd}'")))?
            .map_err(|_| driver_error(&self.instance_id, format!("Failed to probe Codex skills for '{cwd}'")))
    }

    /// The account a reset credit is redeemed on: the directory holding `auth.json`.
    pub fn account_key(&self) -> PathBuf {
        self.home_layout
            .effective_home_path
            .clone()
            .unwrap_or_else(|| self.home_layout.shared_home_path.clone())
    }

    /// `account/rateLimitResetCredit/consume` (20 s); the outcome (`reset`, …). Serializing on
    /// [`Self::account_key`] and re-probing afterwards are the coordinator's (WP-12b).
    pub async fn consume_reset_credit(&self, idempotency_key: &str, cwd: &str) -> Result<String, ProviderDriverError> {
        tokio::time::timeout(Duration::from_secs(20), consume_reset_credit(&self.probe_input(cwd), idempotency_key))
            .await
            .map_err(|_| driver_error(&self.instance_id, "Codex could not redeem the reset credit."))?
            .map_err(|_| driver_error(&self.instance_id, "Codex could not redeem the reset credit."))
    }
}

/// Extra wiring the provider core passes to every instance.
#[derive(Clone, Default)]
pub struct CodexDriverServices {
    /// Overrides the server environment (tests).
    pub base_environment: Option<Environment>,
    pub adapter: CodexAdapterOptions,
    pub probe: Option<ProbeFn>,
}

/// `CodexDriver.create` for `setupMode: "existing"` (the default).
pub fn create_codex_instance(input: CodexDriverCreateInput, services: CodexDriverServices) -> Result<CodexInstance, ProviderDriverError> {
    let environment = merge_provider_instance_environment(&input.environment, services.base_environment.clone().unwrap_or_else(process_environment));
    let home_layout = resolve_codex_home_layout(&input.config);
    materialize_codex_shadow_home(&home_layout).map_err(|error| driver_error(&input.instance_id, error.to_string()))?;
    let mut effective_config = input.config.clone();
    effective_config.enabled = input.enabled;
    effective_config.binary_path = zc_core::expand_home_path(&input.config.binary_path).to_string_lossy().into_owned();
    effective_config.home_path = home_layout
        .effective_home_path
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut adapter_options = services.adapter.clone();
    adapter_options.instance_id = Some(input.instance_id.clone());
    adapter_options.environment = Some(environment.clone());
    let adapter = CodexAdapter::new(effective_config.clone(), adapter_options);
    Ok(CodexInstance {
        continuation_identity: codex_continuation_identity(&home_layout),
        maintenance: codex_maintenance(&home_layout.shared_home_path),
        instance_id: input.instance_id,
        display_name: input.display_name,
        accent_color: input.accent_color,
        enabled: input.enabled,
        home_layout,
        effective_config,
        environment,
        adapter,
        probe: services.probe,
        managed: None,
    })
}

/// `makeManagedCodexProvider`: sessions resolve the managed runtime (and restart when the token
/// changes), a revoked grant calls `auth.revoke`.
pub fn create_managed_codex_instance(
    input: CodexDriverCreateInput,
    services: CodexDriverServices,
    state_dir: &std::path::Path,
    installation: Arc<dyn ManagedExecutableSource>,
    auth: Arc<dyn ChatGptAccessSource>,
) -> CodexInstance {
    let environment = merge_provider_instance_environment(&input.environment, services.base_environment.clone().unwrap_or_else(process_environment));
    let managed = Arc::new(ManagedCodexRuntime::new(
        input.instance_id.clone(),
        input.enabled,
        environment.clone(),
        &input.config,
        state_dir,
        installation,
        auth.clone(),
    ));
    let resolver = managed.clone();
    let revoker = auth;
    let mut adapter_options = services.adapter.clone();
    adapter_options.instance_id = Some(input.instance_id.clone());
    adapter_options.resolve_runtime = Some(Arc::new(move || {
        let resolver = resolver.clone();
        Box::pin(async move { resolver.resolve().await })
    }));
    adapter_options.on_managed_connection_revoked = Some(Arc::new(move || {
        let revoker = revoker.clone();
        Box::pin(async move { revoker.revoke().await })
    }));
    let mut effective_config = input.config.clone();
    effective_config.custom_models = Vec::new();
    let adapter = CodexAdapter::new(input.config.clone(), adapter_options);
    CodexInstance {
        continuation_identity: codex_continuation_identity(&managed.home_layout),
        maintenance: codex_maintenance(&managed.home_layout.shared_home_path),
        home_layout: managed.home_layout.clone(),
        instance_id: input.instance_id,
        display_name: input.display_name,
        accent_color: input.accent_color,
        enabled: input.enabled,
        effective_config,
        environment,
        adapter,
        probe: services.probe,
        managed: Some(managed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_environment_overrides_and_expands_homes() {
        let base: Environment = [("PATH".to_owned(), "/usr/bin".to_owned())].into_iter().collect();
        let environment: ProviderInstanceEnvironment = crate::model::from_json(json!([
            {"name": "CODEX_HOME", "value": "~/.codex_work"},
            {"name": "OTHER", "value": "~/kept"}
        ]));
        let merged = merge_provider_instance_environment(&environment, base);
        assert_eq!(merged["PATH"], "/usr/bin");
        assert!(!merged["CODEX_HOME"].starts_with('~'));
        assert_eq!(merged["OTHER"], "~/kept");
    }

    #[test]
    fn standalone_installs_update_natively_against_the_shared_home() {
        let maintenance = codex_maintenance(std::path::Path::new("/shared/home"));
        assert!(maintenance.is_native_command_path("/Users/someone/.codex/packages/standalone/bin/codex"));
        assert!(!maintenance.is_native_command_path("/usr/local/bin/codex"));
        assert_eq!(maintenance.native_update_env["CODEX_HOME"], "/shared/home");
    }

    #[tokio::test]
    async fn disabled_instances_never_probe() {
        let home = tempfile::tempdir().unwrap();
        let config: CodexSettings = crate::model::from_json(json!({"homePath": home.path().to_string_lossy()}));
        let probe: ProbeFn = Arc::new(|_, _, _| Box::pin(async { panic!("a disabled instance must not probe") }));
        let instance = create_codex_instance(
            CodexDriverCreateInput {
                instance_id: ProviderInstanceId::new("codex-missing"),
                display_name: Some("Codex test".into()),
                accent_color: None,
                environment: Vec::new(),
                enabled: false,
                config,
            },
            CodexDriverServices {
                base_environment: Some(Environment::new()),
                probe: Some(probe),
                ..CodexDriverServices::default()
            },
        )
        .unwrap();
        let draft = instance.check_provider("/tmp", "2026-01-01T00:00:00.000Z").await;
        assert_eq!(draft["status"], "disabled");
        assert_eq!(draft["message"], format!("Codex is disabled in {} settings.", crate::BRAND_NAME));
        assert_eq!(instance.continuation_identity.continuation_key, format!("codex:home:{}", home.path().display()));
    }
}
