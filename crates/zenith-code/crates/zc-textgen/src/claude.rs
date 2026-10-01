//! `textGeneration/ClaudeTextGeneration.ts`: one `claude -p` call per operation, with the
//! prompt on stdin and the output schema enforced by `--json-schema`.
//!
//! ```text
//! <binaryPath|claude> -p --output-format json --json-schema <schema> --model <api model id>
//!   [--effort <effort>] --settings {"disableAllHooks":true,…} --tools "" --disable-slash-commands
//!   --strict-mcp-config --permission-mode dontAsk
//! env: the instance environment (+ CLAUDE_CONFIG_DIR for an instance home); cwd: the project,
//! or a fresh temp directory for thread titles (they need no checkout configuration)
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::{json, Map, Value};
use zc_contracts::{ClaudeSettings, ModelSelection};
use zc_ports::TaggedError;
use zc_provider_claude::catalog::is_ultracode_effort;
use zc_provider_claude::home::{make_claude_environment, Env};
use zc_provider_claude::model_options::{find_descriptor, provider_option_descriptors, selection_string_option, selections_of};
use zc_provider_claude::ClaudeModelCatalog;

use crate::one_shot::{OneShotBackend, OneShotRequest, OneShotTextGeneration};
use crate::process::{failed_command_detail, run_cli, CliError, CliInvocation};
use crate::utils::{normalize_cli_error, text_generation_error, Operation};

/// `CLAUDE_TIMEOUT_MS`.
pub const CLAUDE_TIMEOUT: Duration = Duration::from_secs(180);

/// Where the model catalog comes from (`modelManifest.current` → `resolveClaudeModelCatalog`).
pub type ClaudeCatalogSource = Arc<dyn Fn() -> BoxFuture<'static, ClaudeModelCatalog> + Send + Sync>;

/// A catalog that never changes (tests, or the bundled manifest).
pub fn fixed_catalog(catalog: ClaudeModelCatalog) -> ClaudeCatalogSource {
    Arc::new(move || {
        let catalog = catalog.clone();
        Box::pin(async move { catalog })
    })
}

/// The Claude [`OneShotBackend`] (`makeClaudeTextGeneration(claudeSettings, environment,
/// modelCatalog)`).
pub struct ClaudeBackend {
    settings: ClaudeSettings,
    /// `makeClaudeEnvironment(settings, environment)`.
    environment: Env,
    catalog: ClaudeCatalogSource,
    timeout: Duration,
}

/// Claude text generation.
pub type ClaudeTextGeneration = OneShotTextGeneration<ClaudeBackend>;

impl ClaudeBackend {
    /// `settings` as the driver sees them (binary path already `~`-expanded), `environment` the
    /// instance's merged environment.
    pub fn new(settings: ClaudeSettings, environment: Env, catalog: ClaudeCatalogSource) -> Self {
        let environment = make_claude_environment(&settings.home_path, environment);
        Self {
            settings,
            environment,
            catalog,
            timeout: CLAUDE_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The child environment.
    pub fn environment(&self) -> &Env {
        &self.environment
    }

    /// The argv after the binary for one request (everything `runClaudeJson` resolves from the
    /// catalog and the model selection).
    pub async fn args(&self, operation: Operation, schema_json: &str, model_selection: &Value) -> Result<Vec<String>, TaggedError> {
        let custom_models = serde_json::to_value(&self.settings.custom_models).unwrap_or(Value::Array(Vec::new()));
        let catalog = (self.catalog)().await.scoped(&custom_models);
        let mut selection: ModelSelection = serde_json::from_value(model_selection.clone())
            .map_err(|_| text_generation_error(operation.as_str(), "Text generation received an invalid model selection."))?;
        selection.model = catalog.resolve_slug(&selection.model);
        let model = selection.model.clone();
        let caps = catalog.capabilities(Some(&model));
        let descriptors = provider_option_descriptors(&caps, &selections_of(Some(&selection)));
        let raw_effort = selection_string_option(Some(&selection), "effort");
        let resolved_effort = catalog.resolve_effort(Some(&model), raw_effort.as_deref());
        let cli_effort = catalog.normalize_effort(resolved_effort.as_deref(), Some(&model));
        let ultracode = is_ultracode_effort(resolved_effort.as_deref());
        let boolean = |id: &str| {
            find_descriptor(&descriptors, id)
                .filter(|descriptor| descriptor.get("type").and_then(Value::as_str) == Some("boolean"))
                .and_then(|descriptor| descriptor.get("currentValue"))
                .and_then(Value::as_bool)
        };
        let mut settings = Map::new();
        settings.insert("disableAllHooks".into(), json!(true));
        if let Some(thinking) = boolean("thinking") {
            settings.insert("alwaysThinkingEnabled".into(), json!(thinking));
        }
        if boolean("fastMode") == Some(true) {
            settings.insert("fastMode".into(), json!(true));
        }
        if ultracode {
            settings.insert("ultracode".into(), json!(true));
        }
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--output-format".into(),
            "json".into(),
            "--json-schema".into(),
            schema_json.to_owned(),
            "--model".into(),
            catalog.api_model_id(&selection),
        ];
        if let Some(effort) = cli_effort.filter(|effort| !effort.is_empty()) {
            args.extend(["--effort".into(), effort]);
        }
        args.extend([
            "--settings".into(),
            Value::Object(settings).to_string(),
            // Metadata prompts need no executable capabilities, even when they contain a skill name.
            "--tools".into(),
            String::new(),
            "--disable-slash-commands".into(),
            "--strict-mcp-config".into(),
            "--permission-mode".into(),
            "dontAsk".into(),
        ]);
        Ok(args)
    }

    fn binary(&self) -> String {
        if self.settings.binary_path.is_empty() {
            "claude".to_owned()
        } else {
            self.settings.binary_path.clone()
        }
    }
}

/// Stdout that is not `claude -p --output-format json` output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnexpectedOutputFormat;

/// `decodeClaudeOutput` + the envelope pick: the `structured_output` of the JSON envelope, or
/// of the last `result` message of verbose output.
pub fn parse_claude_output(stdout: &str) -> Result<Option<Value>, UnexpectedOutputFormat> {
    let value: Value = serde_json::from_str(stdout).map_err(|_| UnexpectedOutputFormat)?;
    match value {
        Value::Object(object) => object.get("structured_output").cloned().map(Some).ok_or(UnexpectedOutputFormat),
        Value::Array(messages) => {
            if !messages.iter().all(|message| message.get("type").is_some_and(Value::is_string)) {
                return Err(UnexpectedOutputFormat);
            }
            Ok(messages
                .iter()
                .rev()
                .find(|message| message.get("type").and_then(Value::as_str) == Some("result"))
                .and_then(|message| message.get("structured_output").cloned()))
        }
        _ => Err(UnexpectedOutputFormat),
    }
}

#[async_trait]
impl OneShotBackend for ClaudeBackend {
    fn output_label(&self) -> &str {
        "Claude"
    }

    async fn run(&self, request: OneShotRequest) -> Result<Option<Value>, TaggedError> {
        let operation = request.operation.as_str();
        let args = self
            .args(request.operation, &request.output_schema.json_schema_string(), &request.model_selection)
            .await?;
        // Titles need only the supplied prompt, not configuration from the checkout.
        let title_dir = if request.operation == Operation::GenerateThreadTitle {
            Some(
                tempfile::Builder::new()
                    .prefix("t3code-claude-title-")
                    .tempdir()
                    .map_err(|error| normalize_cli_error("claude", operation, &error, "Failed to create title directory"))?,
            )
        } else {
            None
        };
        let cwd = title_dir.as_ref().map_or_else(|| PathBuf::from(&request.cwd), |dir| dir.path().to_path_buf());
        let invocation = CliInvocation {
            command: self.binary(),
            args,
            env: self.environment.clone(),
            cwd,
            stdin: request.prompt,
        };
        let output = match tokio::time::timeout(self.timeout, run_cli(&invocation)).await {
            Err(_) => return Err(text_generation_error(operation, "Claude CLI request timed out.")),
            Ok(Err(CliError::Spawn(error))) => return Err(normalize_cli_error("claude", operation, &error, "Failed to spawn Claude CLI process")),
            Ok(Err(CliError::Read(error))) => return Err(normalize_cli_error("claude", operation, &error, "Failed to collect process output")),
            Ok(Ok(output)) => output,
        };
        drop(title_dir);
        match output.code {
            Some(0) => {}
            None => return Err(text_generation_error(operation, "Failed to read Claude CLI exit code")),
            Some(_) => return Err(text_generation_error(operation, failed_command_detail("Claude", &output))),
        }
        parse_claude_output(&output.stdout).map_err(|_| text_generation_error(operation, "Claude CLI returned unexpected output format."))
    }
}

impl ClaudeTextGeneration {
    /// `makeClaudeTextGeneration(settings, environment, modelCatalog)`.
    pub fn claude(settings: ClaudeSettings, environment: Env, catalog: ClaudeCatalogSource) -> Self {
        OneShotTextGeneration::new(ClaudeBackend::new(settings, environment, catalog))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_structured_output() {
        assert_eq!(parse_claude_output(r#"{"structured_output":{"title":"a"}}"#), Ok(Some(json!({"title": "a"}))));
        assert_eq!(parse_claude_output(r#"{"structured_output":null}"#), Ok(Some(Value::Null)));
        assert_eq!(parse_claude_output(r#"{"type":"result"}"#), Err(UnexpectedOutputFormat));
        assert_eq!(parse_claude_output(r#"[]"#), Ok(None));
        assert_eq!(parse_claude_output(r#"[{"x":1}]"#), Err(UnexpectedOutputFormat));
        assert_eq!(
            parse_claude_output(r#"[{"type":"result","structured_output":{"title":"a"}},{"type":"result","subtype":"error"}]"#),
            Ok(None)
        );
        assert_eq!(parse_claude_output("not json"), Err(UnexpectedOutputFormat));
    }
}
