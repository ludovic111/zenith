//! The tagged errors of this crate, as the wire carries them (`ServerSettingsError`,
//! `KeybindingsConfigParseError`), plus the messages the TS classes compute.

use std::path::Path;

use serde_json::Value;
use zc_contracts::{KeybindingsConfigError, LitKeybindingsConfigParseError, LitServerSettingsError, ServerSettingsError, ServerSettingsOperation};
use zc_core::Defect;

/// A `ServerSettingsError` for `operation` at `settings_path`, caused by `cause`.
pub fn settings_error(settings_path: &str, operation: ServerSettingsOperation, cause: Defect) -> ServerSettingsError {
    ServerSettingsError {
        tag: LitServerSettingsError,
        settings_path: settings_path.to_owned(),
        operation,
        provider_instance_id: None,
        environment_variable: None,
        cause: cause.0,
    }
}

/// [`settings_error`] for a path.
pub fn settings_error_at(settings_path: &Path, operation: ServerSettingsOperation, cause: Defect) -> ServerSettingsError {
    settings_error(&settings_path.to_string_lossy(), operation, cause)
}

/// Attach the provider instance and variable a secret operation was about.
pub fn with_secret_context(mut error: ServerSettingsError, provider_instance_id: Option<&str>, environment_variable: Option<&str>) -> ServerSettingsError {
    error.provider_instance_id = provider_instance_id.map(str::to_owned);
    error.environment_variable = environment_variable.map(str::to_owned);
    error
}

/// `ServerSettingsError.message`.
pub fn settings_error_message(error: &ServerSettingsError) -> String {
    let provider = error
        .provider_instance_id
        .as_deref()
        .map(|id| format!(" for provider {id}"))
        .unwrap_or_default();
    let variable = error
        .environment_variable
        .as_deref()
        .map(|name| format!(" and environment variable {name}"))
        .unwrap_or_default();
    format!(
        "Server settings {} failed{provider}{variable} at {}.",
        error.operation.as_str(),
        error.settings_path
    )
}

/// A `KeybindingsConfigParseError`.
pub fn keybindings_error(config_path: &Path, detail: &str, cause: Option<Defect>) -> KeybindingsConfigError {
    KeybindingsConfigError {
        tag: LitKeybindingsConfigParseError,
        config_path: config_path.to_string_lossy().into_owned(),
        detail: detail.to_owned(),
        cause: cause.map(|cause| cause.0),
    }
}

/// `KeybindingsConfigError.message`.
pub fn keybindings_error_message(error: &KeybindingsConfigError) -> String {
    format!("Unable to parse keybindings config at {}: {}", error.config_path, error.detail)
}

/// An `Error`-shaped defect from a message.
pub fn defect(message: impl Into<String>) -> Defect {
    Defect::error("Error", message)
}

/// The cause of an error as text (for logs).
pub fn cause_text(cause: &Value) -> String {
    Defect(cause.clone()).message()
}
