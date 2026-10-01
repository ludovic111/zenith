//! Managed Codex (ChatGPT token sharing): the runtime overrides of `CodexManagedRuntime.ts`, the
//! error classification of `CodexManagedErrors.ts`, the home of `CodexManagedHome.ts` and the
//! status-check rules of `Drivers/CodexManagedProvider.ts`.
//!
//! The ChatGPT sign-in (`CodexChatGptAuth.ts`) and the managed installation
//! (`CodexInstallation.ts`) are WP-12b: they plug in through [`ManagedExecutableSource`] and
//! [`ChatGptAccessSource`].

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde_json::{json, Value};
use zc_contracts::{CodexSettings, ProviderInstanceId, ProviderSetupError};

use crate::home_layout::{materialize_codex_shadow_home, resolve_codex_home_layout, CodexHomeLayout, CodexHomeMode};
use crate::launch_args::Environment;
use crate::model::from_json;

// ---------------------------------------------------------------------------------------------
// Errors

/// A classified sharing failure: a safe message, whether the connection must be revoked, the code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedErrorClass {
    pub message: String,
    pub revoke: bool,
    pub code: String,
}

const FAILURES: &[(&str, &str)] = &[
    (
        "subscription_sharing_v2_user_not_eligible",
        "ChatGPT sharing is unavailable for this account or workspace. Use another provider or check its sharing policy.",
    ),
    (
        "subscription_sharing_usage_limit_exceeded",
        "Your ChatGPT usage limit was reached. Check ChatGPT Usage settings for your available allowance.",
    ),
    (
        "subscription_sharing_usage_unavailable",
        "ChatGPT usage is temporarily unavailable. Try again shortly.",
    ),
    (
        "subscription_sharing_unsupported_capability",
        "Codex used a feature that ChatGPT sharing does not support. Use another provider for this request.",
    ),
    (
        "subscription_sharing_v2_client_not_enabled",
        "This app is not enabled for this ChatGPT connection. Use your existing CLI or another provider.",
    ),
    ("subscription_sharing_v2_route_not_supported", "ChatGPT does not support this request route."),
    (
        "subscription_sharing_v2_invalid_user",
        "ChatGPT could not validate this connection. Check the selected account and sharing permissions.",
    ),
    (
        "subscription_sharing_v2_user_unavailable",
        "ChatGPT is temporarily unavailable. Try again shortly.",
    ),
    (
        "subscription_sharing_user_not_eligible",
        "ChatGPT sharing is unavailable for this account or workspace. Use another provider or check its sharing policy.",
    ),
    ("subscription_sharing_route_not_supported", "ChatGPT does not support this request route."),
    (
        "subscription_sharing_invalid_user",
        "ChatGPT could not validate this connection. Check the selected account and sharing permissions.",
    ),
    (
        "subscription_sharing_user_unavailable",
        "ChatGPT is temporarily unavailable. Try again shortly.",
    ),
    (
        "chatpass_v2_scope_not_authorized",
        "This ChatGPT grant does not authorize the request. Check the connection's sharing permissions.",
    ),
    (
        "chatpass_v2_invalid_authorization_context",
        "ChatGPT could not authorize this connection. Check the client and sharing permissions.",
    ),
];

/// `classifyCodexManagedError`: looks for a known sharing error code anywhere in the value (a
/// string, or the JSON of anything else). No known code revokes the connection today.
pub fn classify_codex_managed_error(value: &Value) -> Option<ManagedErrorClass> {
    let text = match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    for (code, message) in FAILURES {
        if !text.contains(code) {
            continue;
        }
        let message = if *code == "subscription_sharing_unsupported_capability" && text.contains("tool 'namespace'") {
            "Codex sent a tool namespace that ChatGPT sharing does not support. Use another provider for this request."
        } else if *code == "subscription_sharing_unsupported_capability" && text.contains("additional_tools") {
            "Codex sent an input item that ChatGPT sharing does not support. Use another provider for this request."
        } else {
            message
        };
        return Some(ManagedErrorClass {
            message: message.to_owned(),
            revoke: false,
            code: (*code).to_owned(),
        });
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Runtime

/// The `-c` overrides that point Codex at the token-sharing provider (the token travels in
/// `ACCESS_TOKEN`; native `auth.json` is never written).
pub fn managed_codex_launch_args() -> String {
    [
        r#"model_provider="openai_token_sharing""#,
        r#"model_providers.openai_token_sharing.name="OpenAI Token Sharing""#,
        r#"model_providers.openai_token_sharing.base_url="https://api.openai.com/v1""#,
        r#"model_providers.openai_token_sharing.model_catalog_url="https://api.openai.com/v1/models""#,
        "features.api_key_model_discovery=true",
        r#"model_providers.openai_token_sharing.env_key="ACCESS_TOKEN""#,
        r#"model_providers.openai_token_sharing.wire_api="responses""#,
        "model_providers.openai_token_sharing.requires_openai_auth=false",
        "model_providers.openai_token_sharing.supports_websockets=false",
    ]
    .iter()
    .map(|value| format!("-c '{value}'"))
    .collect::<Vec<_>>()
    .join(" ")
}

/// `resolveManagedCodexHomeLayout`: the default instance uses the shared home directly, other
/// instances an overlay under `<stateDir>/providers/codex/<instanceId>/shadow`.
pub fn resolve_managed_codex_home_layout(state_dir: &Path, instance_id: &ProviderInstanceId, config: &CodexSettings) -> CodexHomeLayout {
    let mut config = config.clone();
    if config.shadow_home_path.trim().is_empty() && instance_id.as_str() != crate::DRIVER_KIND {
        config.shadow_home_path = state_dir
            .join("providers")
            .join("codex")
            .join(instance_id.as_str())
            .join("shadow")
            .to_string_lossy()
            .into_owned();
    }
    resolve_codex_home_layout(&config)
}

/// `CodexEffectiveRuntime`: the settings and environment a managed session runs with, and a
/// revision that changes when the token does (the adapter restarts the app-server then).
#[derive(Debug, Clone, PartialEq)]
pub struct CodexEffectiveRuntime {
    pub config: CodexSettings,
    pub environment: Environment,
    pub revision: String,
}

/// The managed `codex` executable (`CodexInstallation`, WP-12b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedExecutable {
    pub executable_path: String,
    pub version: Option<String>,
}

#[async_trait]
pub trait ManagedExecutableSource: Send + Sync {
    /// Installs if needed and leases the executable for a session.
    async fn acquire(&self) -> Result<ManagedExecutable, String>;
    /// The installed executable, without installing.
    async fn resolve(&self) -> Option<ManagedExecutable>;
}

/// The signed-in ChatGPT account (`CodexChatGptAuth`, WP-12b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGptAccount {
    pub client_id: String,
    pub email: Option<String>,
    pub scopes: Vec<String>,
}

impl ChatGptAccount {
    /// Token sharing needs this scope.
    pub fn can_share_tokens(&self) -> bool {
        self.scopes.iter().any(|scope| scope == "chatgpt.tokens.use.direct")
    }
}

#[async_trait]
pub trait ChatGptAccessSource: Send + Sync {
    /// A fresh access token, or why there is none.
    async fn access_token(&self) -> Result<String, ProviderSetupError>;
    /// The saved account, if any.
    async fn read_account(&self) -> Option<ChatGptAccount>;
    /// Forget the connection (a revoked grant).
    async fn revoke(&self);
}

fn setup_error(instance_id: &ProviderInstanceId, operation: &str, detail: &str) -> ProviderSetupError {
    from_json(json!({"_tag": "ProviderSetupError", "instanceId": instance_id.as_str(), "operation": operation, "detail": detail}))
}

/// `makeCodexManagedRuntime`: resolves the managed runtime per session.
pub struct ManagedCodexRuntime {
    pub instance_id: ProviderInstanceId,
    pub enabled: bool,
    pub environment: Environment,
    pub home_layout: CodexHomeLayout,
    pub home_path: PathBuf,
    pub installation: std::sync::Arc<dyn ManagedExecutableSource>,
    pub auth: std::sync::Arc<dyn ChatGptAccessSource>,
}

impl ManagedCodexRuntime {
    pub fn new(
        instance_id: ProviderInstanceId,
        enabled: bool,
        environment: Environment,
        config: &CodexSettings,
        state_dir: &Path,
        installation: std::sync::Arc<dyn ManagedExecutableSource>,
        auth: std::sync::Arc<dyn ChatGptAccessSource>,
    ) -> Self {
        let home_layout = resolve_managed_codex_home_layout(state_dir, &instance_id, config);
        let home_path = home_layout.effective_home_path.clone().unwrap_or_else(|| home_layout.shared_home_path.clone());
        Self {
            instance_id,
            enabled,
            environment,
            home_layout,
            home_path,
            installation,
            auth,
        }
    }

    /// `resolve`: executable, token, shadow home, then the effective settings and environment.
    pub async fn resolve(&self) -> Result<CodexEffectiveRuntime, ProviderSetupError> {
        let executable = self
            .installation
            .acquire()
            .await
            .map_err(|_| setup_error(&self.instance_id, "install", "Set up managed Codex before starting a session."))?;
        let access_token = self.auth.access_token().await?;
        materialize_codex_shadow_home(&self.home_layout).map_err(|error| setup_error(&self.instance_id, "runtime", &error.to_string()))?;
        std::fs::create_dir_all(&self.home_path).map_err(|_| setup_error(&self.instance_id, "runtime", "Could not prepare the managed Codex runtime."))?;
        Ok(managed_effective_runtime(
            &self.environment,
            self.enabled,
            &executable.executable_path,
            &self.home_path,
            &access_token,
        ))
    }
}

/// The pure part of `resolve`: ambient CLI overrides cannot redirect a T3-owned token.
pub fn managed_effective_runtime(base: &Environment, enabled: bool, executable_path: &str, home_path: &Path, access_token: &str) -> CodexEffectiveRuntime {
    let home = home_path.to_string_lossy().into_owned();
    let mut environment = base.clone();
    environment.insert("ACCESS_TOKEN".into(), access_token.to_owned());
    environment.insert("CODEX_HOME".into(), home.clone());
    for key in ["T3CODE_CODEX_LAUNCH_ARGS", "OPENAI_API_KEY", "OPENAI_BASE_URL"] {
        environment.remove(key);
    }
    CodexEffectiveRuntime {
        config: from_json(json!({
            "enabled": enabled,
            "setupMode": "managed",
            "binaryPath": executable_path,
            "homePath": home,
            "launchArgs": managed_codex_launch_args(),
        })),
        environment,
        revision: access_token.to_owned(),
    }
}

/// `runtimePaths` of a managed snapshot.
pub fn managed_runtime_paths(layout: &CodexHomeLayout, home_path: &Path) -> Value {
    json!({
        "homePath": layout.shared_home_path.to_string_lossy(),
        "shadowHomePath": if layout.mode == CodexHomeMode::AuthOverlay { Value::from(home_path.to_string_lossy().into_owned()) } else { Value::Null },
    })
}

/// The usage limits a managed snapshot shows (ChatGPT tracks them across connected apps).
pub fn managed_usage_limits(checked_at: &str) -> Value {
    json!({
        "checkedAt": checked_at,
        "windows": [],
        "unavailable": {
            "reason": "unsupported",
            "message": "ChatGPT tracks subscription usage across connected apps. Open Usage settings with the account you connected to Codex.",
        },
        "externalUsage": { "label": "ChatGPT usage", "url": "https://chatgpt.com/#settings/Usage" },
    })
}

/// The `auth` a managed snapshot shows for a signed-in, sharing account.
pub fn managed_auth(account: &ChatGptAccount) -> Value {
    let mut auth = json!({
        "subscriptionSharing": true,
        "profileId": account.client_id,
        "status": "authenticated",
        "type": "chatgpt",
        "label": "ChatGPT",
    });
    if let Some(email) = account.email.as_deref().map(str::trim).filter(|email| !email.is_empty()) {
        auth["email"] = json!(email);
    }
    auth
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_streamed_failures_to_safe_messages() {
        let classified =
            classify_codex_managed_error(&json!({"error": {"code": "subscription_sharing_v2_invalid_user", "message": "opaque token dummy-sensitive"}}))
                .unwrap();
        assert_eq!(
            classified,
            ManagedErrorClass {
                message: "ChatGPT could not validate this connection. Check the selected account and sharing permissions.".into(),
                revoke: false,
                code: "subscription_sharing_v2_invalid_user".into(),
            }
        );
        let limit = classify_codex_managed_error(&json!("subscription_sharing_usage_limit_exceeded")).unwrap();
        assert!(limit.message.contains("Usage settings"));
        assert_eq!(limit.code, "subscription_sharing_usage_limit_exceeded");
        assert!(!classify_codex_managed_error(&json!("subscription_sharing_usage_unavailable")).unwrap().revoke);
        assert!(classify_codex_managed_error(&json!("subscription_sharing_unsupported_capability"))
            .unwrap()
            .message
            .contains("feature"));
        assert_eq!(classify_codex_managed_error(&json!({"error": "unknown"})), None);
    }

    #[test]
    fn distinguishes_unsupported_tools_from_input_items() {
        let code = "subscription_sharing_unsupported_capability";
        for (detail, expected) in [
            ("tool 'namespace' is not supported", "tool namespace"),
            ("input item 'additional_tools' is not supported", "input item"),
        ] {
            let response = json!({"error": {"code": code, "message": format!("{detail}; dummy-sensitive")}});
            for value in [response.clone(), Value::String(response.to_string())] {
                let failure = classify_codex_managed_error(&value).unwrap();
                assert!(failure.message.contains(expected));
                assert!(!failure.message.contains("dummy-sensitive"));
                assert!(!failure.revoke);
            }
        }
    }

    #[test]
    fn current_subscriber_and_permission_codes_keep_credentials() {
        for code in [
            "subscription_sharing_invalid_user",
            "subscription_sharing_user_not_eligible",
            "subscription_sharing_route_not_supported",
            "subscription_sharing_user_unavailable",
            "chatpass_v2_scope_not_authorized",
            "chatpass_v2_invalid_authorization_context",
        ] {
            let classified = classify_codex_managed_error(&json!({"error": {"code": code}})).unwrap();
            assert_eq!(classified.code, code);
            assert!(!classified.revoke);
        }
    }

    #[test]
    fn managed_home_defaults_to_the_global_home_and_honors_configured_paths() {
        let state = Path::new("/t3-state");
        let defaults: CodexSettings = from_json(json!({}));
        let primary = resolve_managed_codex_home_layout(state, &ProviderInstanceId::new("codex"), &defaults);
        assert_eq!(primary.shared_home_path, zc_core::paths::home_dir().join(".codex"));
        assert_eq!(primary.mode, CodexHomeMode::Direct);
        let additional = resolve_managed_codex_home_layout(state, &ProviderInstanceId::new("codex-work"), &defaults);
        assert_eq!(additional.shared_home_path, primary.shared_home_path);
        assert_eq!(additional.mode, CodexHomeMode::AuthOverlay);
        assert_eq!(
            additional.effective_home_path.as_deref(),
            Some(Path::new("/t3-state/providers/codex/codex-work/shadow"))
        );
        let configured = resolve_managed_codex_home_layout(
            state,
            &ProviderInstanceId::new("codex-work"),
            &from_json(json!({"homePath": "/custom/shared", "shadowHomePath": "/custom/shadow"})),
        );
        assert_eq!(configured.shared_home_path, Path::new("/custom/shared"));
        assert_eq!(configured.effective_home_path.as_deref(), Some(Path::new("/custom/shadow")));
    }

    #[test]
    fn owned_tokens_never_route_through_ambient_cli_overrides() {
        let ambient: Environment = [
            ("CODEX_HOME", "/user/.codex"),
            ("OPENAI_API_KEY", "dummy-global-key"),
            ("OPENAI_BASE_URL", "https://user-proxy.test"),
            ("T3CODE_CODEX_LAUNCH_ARGS", "--config model_provider=global-proxy"),
            ("PATH", "/usr/bin"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        let effective = managed_effective_runtime(
            &ambient,
            true,
            "/isolated/tools/codex/0.156.1/bin/codex",
            Path::new("/shared/home"),
            "dummy-owned-access",
        );
        assert_eq!(effective.config.binary_path, "/isolated/tools/codex/0.156.1/bin/codex");
        assert_eq!(effective.config.home_path, "/shared/home");
        assert_eq!(effective.environment["ACCESS_TOKEN"], "dummy-owned-access");
        assert_eq!(effective.environment["CODEX_HOME"], "/shared/home");
        for removed in ["OPENAI_API_KEY", "OPENAI_BASE_URL", "T3CODE_CODEX_LAUNCH_ARGS"] {
            assert!(!effective.environment.contains_key(removed));
        }
        let args = crate::launch_args::codex_app_server_args(Some(&effective.config.launch_args));
        for expected in [
            "model_providers.openai_token_sharing.base_url=\"https://api.openai.com/v1\"",
            "model_providers.openai_token_sharing.model_catalog_url=\"https://api.openai.com/v1/models\"",
            "features.api_key_model_discovery=true",
            "model_providers.openai_token_sharing.supports_websockets=false",
            "model_providers.openai_token_sharing.requires_openai_auth=false",
        ] {
            assert!(args.iter().any(|arg| arg == expected), "{expected}");
        }
        assert!(!effective.config.launch_args.contains("dummy-owned-access"));
        assert!(!effective.config.launch_args.contains("model_catalog_json"));
        assert_eq!(ambient["CODEX_HOME"], "/user/.codex");
    }
}
