//! `textGeneration/CodexTextGeneration.ts`: one `codex exec` call per operation, with the
//! prompt on stdin, the output schema and the last message in temp files.
//!
//! ```text
//! <binaryPath|codex> exec [exec-safe launch args] --ephemeral --skip-git-repo-check -s read-only
//!   --model <model> --config model_reasoning_effort="<effort|low>" [--config service_tier="<tier>"]
//!   --output-schema <schema file> --output-last-message <output file> [--image <path>]… -
//! env: the instance environment (+ CODEX_HOME for an instance home); cwd: the project
//! ```
//!
//! A managed (ChatGPT sharing) instance resolves its executable, environment and settings per
//! call ([`CodexRuntimeResolver`]) and never forwards a service tier.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use serde_json::Value;
use zc_contracts::{ChatAttachment, CodexSettings, ModelSelection, ProviderSetupError};
use zc_ports::TaggedError;
use zc_provider_codex::launch_args::{codex_exec_launch_args, resolve_codex_launch_args, Environment};
use zc_provider_codex::managed::CodexEffectiveRuntime;
use zc_provider_codex::model::{codex_model_family, codex_service_tier_option_value, model_selection_string_option};

use crate::one_shot::{OneShotBackend, OneShotRequest, OneShotTextGeneration};
use crate::process::{failed_command_detail, run_cli, CliError, CliInvocation};
use crate::utils::{normalize_cli_error, text_generation_error};

/// `CODEX_TIMEOUT_MS`.
pub const CODEX_TIMEOUT: Duration = Duration::from_secs(180);
/// `DEFAULT_TEXT_GENERATION_REASONING_EFFORT`.
pub const DEFAULT_TEXT_GENERATION_REASONING_EFFORT: &str = "low";

/// One model of the instance snapshot, as model dispatch needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexModel {
    pub slug: String,
    pub is_custom: bool,
}

/// `getModels`: the instance snapshot's models.
pub type CodexModelsSource = Arc<dyn Fn() -> BoxFuture<'static, Vec<CodexModel>> + Send + Sync>;
/// `resolveRuntime`: the managed runtime (executable, token environment, settings) per call.
pub type CodexRuntimeResolver = Arc<dyn Fn() -> BoxFuture<'static, Result<CodexEffectiveRuntime, ProviderSetupError>> + Send + Sync>;

/// No models (the `getModels` default).
pub fn no_models() -> CodexModelsSource {
    Arc::new(|| Box::pin(async { Vec::new() }))
}

/// The Codex [`OneShotBackend`] (`makeCodexTextGeneration(config, environment, getModels,
/// resolveRuntime)`).
pub struct CodexBackend {
    config: CodexSettings,
    environment: Environment,
    models: CodexModelsSource,
    resolve_runtime: Option<CodexRuntimeResolver>,
    attachments_dir: PathBuf,
    timeout: Duration,
}

/// Codex text generation.
pub type CodexTextGeneration = OneShotTextGeneration<CodexBackend>;

impl CodexBackend {
    /// `config` is the driver's effective config (binary path expanded, `homePath` the effective
    /// `CODEX_HOME` or empty); `environment` the instance's merged environment.
    pub fn new(config: CodexSettings, environment: Environment, attachments_dir: PathBuf) -> Self {
        Self {
            config,
            environment,
            models: no_models(),
            resolve_runtime: None,
            attachments_dir,
            timeout: CODEX_TIMEOUT,
        }
    }

    pub fn with_models(mut self, models: CodexModelsSource) -> Self {
        self.models = models;
        self
    }

    pub fn with_runtime(mut self, resolve_runtime: CodexRuntimeResolver) -> Self {
        self.resolve_runtime = Some(resolve_runtime);
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// `materializeImageAttachments`: the persisted files of image attachments; missing or
    /// unresolvable ones are skipped.
    pub fn image_paths(&self, attachments: &[Value]) -> Vec<String> {
        attachments
            .iter()
            .filter(|attachment| attachment.get("type").and_then(Value::as_str) == Some("image"))
            .filter_map(|attachment| serde_json::from_value::<ChatAttachment>(attachment.clone()).ok())
            .filter_map(|attachment| zc_providers::attachments::resolve_attachment_path(&self.attachments_dir, &attachment))
            .filter(|path| path.is_absolute() && std::fs::metadata(path).is_ok_and(|meta| meta.is_file()))
            .map(|path| path.to_string_lossy().into_owned())
            .collect()
    }

    /// The command, argv and environment of one call (everything `runCodexCommand` resolves).
    pub async fn invocation(
        &self,
        operation: &str,
        model_selection: &Value,
        schema_path: &Path,
        output_path: &Path,
        image_paths: &[String],
    ) -> Result<(String, Vec<String>, Environment), TaggedError> {
        let resolved = match &self.resolve_runtime {
            Some(resolve) => Some(resolve().await.map_err(|cause| text_generation_error(operation, cause.detail))?),
            None => None,
        };
        let (config, environment) = match &resolved {
            Some(runtime) => (&runtime.config, &runtime.environment),
            None => (&self.config, &self.environment),
        };
        let selection: Option<ModelSelection> = serde_json::from_value(model_selection.clone()).ok();
        let requested = model_selection.get("model").and_then(Value::as_str).unwrap_or_default().to_owned();
        let models = (self.models)().await;
        let model = models
            .iter()
            .find(|candidate| candidate.slug == requested)
            .or_else(|| {
                models
                    .iter()
                    .find(|candidate| !candidate.is_custom && codex_model_family(&candidate.slug) == requested)
            })
            .map_or(requested.clone(), |candidate| candidate.slug.clone());
        let launch_args = resolve_codex_launch_args(Some(&config.launch_args), environment);
        let reasoning_effort =
            model_selection_string_option(selection.as_ref(), "reasoningEffort").unwrap_or_else(|| DEFAULT_TEXT_GENERATION_REASONING_EFFORT.to_owned());
        let service_tier = if resolved.is_some() {
            None
        } else {
            codex_service_tier_option_value(selection.as_ref())
        };
        let mut args = vec!["exec".to_owned()];
        args.extend(codex_exec_launch_args(Some(&launch_args)));
        args.extend(["--ephemeral", "--skip-git-repo-check", "-s", "read-only", "--model"].map(str::to_owned));
        args.push(model);
        args.push("--config".into());
        args.push(format!("model_reasoning_effort=\"{reasoning_effort}\""));
        if let Some(tier) = service_tier {
            args.push("--config".into());
            args.push(format!("service_tier=\"{tier}\""));
        }
        args.push("--output-schema".into());
        args.push(schema_path.to_string_lossy().into_owned());
        args.push("--output-last-message".into());
        args.push(output_path.to_string_lossy().into_owned());
        for path in image_paths {
            args.push("--image".into());
            args.push(path.clone());
        }
        args.push("-".into());
        let mut env = environment.clone();
        if !config.home_path.is_empty() {
            env.insert("CODEX_HOME".into(), zc_core::expand_home_path(&config.home_path).to_string_lossy().into_owned());
        }
        let command = if config.binary_path.is_empty() {
            "codex".to_owned()
        } else {
            config.binary_path.clone()
        };
        Ok((command, args, env))
    }
}

/// `fileSystem.makeTempFileScoped({prefix})` as Effect's Node file system lays it out: a fresh
/// `<tmp>/<prefix>XXXXXX/` directory holding a file named by 6 random bytes in hex; the whole
/// directory goes away when this is dropped.
struct ScopedTempFile {
    _directory: tempfile::TempDir,
    path: PathBuf,
}

impl std::ops::Deref for ScopedTempFile {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.path
    }
}

fn temp_file(operation: &str, prefix: &str, content: &str) -> Result<ScopedTempFile, TaggedError> {
    let failed = || text_generation_error(operation, "Failed to write temp file");
    let directory = tempfile::Builder::new()
        .prefix(&format!("t3code-{prefix}-{}-", std::process::id()))
        .tempdir()
        .map_err(|_| failed())?;
    let random: String = zc_core::uuid_v4().chars().filter(char::is_ascii_hexdigit).take(12).collect();
    let path = directory.path().join(random);
    std::fs::write(&path, content).map_err(|_| failed())?;
    Ok(ScopedTempFile { _directory: directory, path })
}

#[async_trait]
impl OneShotBackend for CodexBackend {
    fn output_label(&self) -> &str {
        "Codex"
    }

    async fn run(&self, request: OneShotRequest) -> Result<Option<Value>, TaggedError> {
        let operation = request.operation.as_str();
        let image_paths = self.image_paths(&request.attachments);
        let schema_path = temp_file(operation, "codex-schema", &request.output_schema.json_schema_string())?;
        let output_path = temp_file(operation, "codex-output", "")?;
        let run = async {
            let (command, args, env) = self
                .invocation(operation, &request.model_selection, &schema_path, &output_path, &image_paths)
                .await?;
            let invocation = CliInvocation {
                command,
                args,
                env,
                cwd: PathBuf::from(&request.cwd),
                stdin: request.prompt.clone(),
            };
            match run_cli(&invocation).await {
                Err(CliError::Spawn(error)) => Err(normalize_cli_error("codex", operation, &error, "Failed to spawn Codex CLI process")),
                Err(CliError::Read(error)) => Err(normalize_cli_error("codex", operation, &error, "Failed to collect process output")),
                Ok(output) => match output.code {
                    Some(0) => Ok(()),
                    None => Err(text_generation_error(operation, "Failed to read Codex CLI exit code")),
                    Some(_) => Err(text_generation_error(operation, failed_command_detail("Codex", &output))),
                },
            }
        };
        match tokio::time::timeout(self.timeout, run).await {
            Err(_) => return Err(text_generation_error(operation, "Codex CLI request timed out.")),
            Ok(result) => result?,
        }
        let content = std::fs::read(&*output_path).map_err(|_| text_generation_error(operation, "Failed to read Codex output file."))?;
        Ok(serde_json::from_str(&String::from_utf8_lossy(&content)).ok())
    }
}

impl CodexTextGeneration {
    /// `makeCodexTextGeneration(config, environment, getModels, resolveRuntime?)`.
    pub fn codex(backend: CodexBackend) -> Self {
        OneShotTextGeneration::new(backend)
    }
}
