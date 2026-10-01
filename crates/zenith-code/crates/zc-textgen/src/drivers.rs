//! Giving every provider instance its text generator (what each TS driver's `create` does with
//! `makeClaudeTextGeneration` / `makeCodexTextGeneration` / …).
//!
//! The provider crates cannot depend on this one (it depends on them), so the server wraps its
//! driver list with [`with_text_generation`]: after a driver creates an instance, an instance
//! that has no `text_generation` yet gets the one [`text_generation_for`] builds from the
//! instance's decoded config and environment. A driver that sets its own (managed Codex, whose
//! runtime and admission gate only it holds; see [`codex_managed_text_generation`]) keeps it.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use zc_contracts::{ClaudeSettings, CodexSettings, CodexSettingsSetupMode, ProviderDriverKind};
use zc_ports::text_generation::{
    BranchNameGenerationInput, CommitMessageGenerationInput, CommitMessageGenerationResult, PrContentGenerationInput, PrContentGenerationResult,
    ThreadTitleGenerationInput, ThreadTitleGenerationResult,
};
use zc_ports::{TaggedError, TextGeneration};
use zc_provider_claude::ClaudeModelCatalog;
use zc_provider_codex::home_layout::resolve_codex_home_layout;
use zc_providers::driver::{ProviderAuthAdmission, ServerProviderSource};
use zc_providers::{Driver, DriverCreateInput, DriverEnv, DriverMetadata, ModelManifest, ProviderDriverError, ProviderInstance};

use crate::claude::{ClaudeBackend, ClaudeCatalogSource};
use crate::codex::{CodexBackend, CodexModel, CodexModelsSource, CodexRuntimeResolver};
use crate::one_shot::OneShotTextGeneration;
use crate::unsupported::UnsupportedTextGeneration;
use crate::utils::{text_generation_error, Operation};

/// The Claude catalog of the model manifest (`modelManifest.current.pipe(Effect.map(resolveClaudeModelCatalog))`).
pub fn manifest_claude_catalog(manifest: ModelManifest) -> ClaudeCatalogSource {
    Arc::new(move || {
        let manifest = manifest.clone();
        Box::pin(async move {
            let current = manifest.current().await;
            ClaudeModelCatalog::from_manifest(&serde_json::to_value(&*current).unwrap_or(Value::Null))
        })
    })
}

/// The instance snapshot's models (`snapshot.getSnapshot.pipe(Effect.map((value) => value.models))`).
pub fn snapshot_models(snapshot: Arc<dyn ServerProviderSource>) -> CodexModelsSource {
    Arc::new(move || {
        let snapshot = snapshot.clone();
        Box::pin(async move {
            snapshot
                .get_snapshot()
                .await
                .models
                .into_iter()
                .map(|model| CodexModel {
                    slug: model.slug,
                    is_custom: model.is_custom,
                })
                .collect()
        })
    })
}

/// What [`text_generation_for`] builds a generator from.
pub struct TextGenerationInstanceInput<'a> {
    pub driver_kind: &'a str,
    /// The decoded config (`Driver::decode_config` output).
    pub config: &'a Value,
    pub enabled: bool,
    /// `mergeProviderInstanceEnvironment(environment)`.
    pub environment: BTreeMap<String, String>,
    /// The instance's status snapshot (Codex model dispatch).
    pub snapshot: Option<Arc<dyn ServerProviderSource>>,
}

/// The text generator a TS driver of this kind gives its instances. `None` for a driver kind
/// TS does not know, or a config that does not decode.
pub fn text_generation_for(input: TextGenerationInstanceInput<'_>, env: &DriverEnv) -> Option<Arc<dyn TextGeneration>> {
    match input.driver_kind {
        zc_provider_claude::DRIVER_KIND => {
            let mut settings: ClaudeSettings = serde_json::from_value(input.config.clone()).ok()?;
            settings.enabled = input.enabled;
            settings.binary_path = zc_core::expand_home_path(&settings.binary_path).to_string_lossy().into_owned();
            let backend = ClaudeBackend::new(settings, input.environment, manifest_claude_catalog(env.model_manifest.clone()));
            Some(Arc::new(OneShotTextGeneration::new(backend)))
        }
        zc_provider_codex::DRIVER_KIND => {
            let config: CodexSettings = serde_json::from_value(input.config.clone()).ok()?;
            if config.setup_mode == Some(CodexSettingsSetupMode::Managed) {
                // Only the managed driver holds the runtime resolver and the admission gate.
                return Some(Arc::new(UnsupportedTextGeneration::new("Managed Codex")));
            }
            let layout = resolve_codex_home_layout(&config);
            let mut effective = config;
            effective.enabled = input.enabled;
            effective.binary_path = zc_core::expand_home_path(&effective.binary_path).to_string_lossy().into_owned();
            effective.home_path = layout.effective_home_path.map(|path| path.to_string_lossy().into_owned()).unwrap_or_default();
            let mut backend = CodexBackend::new(effective, input.environment, env.attachments_dir.clone());
            if let Some(snapshot) = input.snapshot {
                backend = backend.with_models(snapshot_models(snapshot));
            }
            Some(Arc::new(OneShotTextGeneration::new(backend)))
        }
        kind @ ("cursor" | "grok" | "antigravity" | "opencode") => Some(Arc::new(UnsupportedTextGeneration::for_driver_kind(kind))),
        _ => None,
    }
}

/// `CodexManagedProvider`'s text generation: `makeCodexTextGeneration(config, undefined,
/// models, resolveRuntime)` behind the auth controller's admission gate. For the managed
/// driver to set on its instances.
pub fn codex_managed_text_generation(
    config: CodexSettings,
    attachments_dir: std::path::PathBuf,
    models: CodexModelsSource,
    resolve_runtime: CodexRuntimeResolver,
    admission: Arc<dyn ProviderAuthAdmission>,
) -> Arc<dyn TextGeneration> {
    let backend = CodexBackend::new(config, zc_provider_codex::adapter::process_environment(), attachments_dir)
        .with_models(models)
        .with_runtime(resolve_runtime);
    Arc::new(AdmissionGuarded {
        inner: Arc::new(OneShotTextGeneration::new(backend)),
        admission,
    })
}

/// `protect(operation, effect)`: run under `withAccess`; a refused admission is a
/// `TextGenerationError` with its detail.
pub struct AdmissionGuarded {
    pub inner: Arc<dyn TextGeneration>,
    pub admission: Arc<dyn ProviderAuthAdmission>,
}

impl AdmissionGuarded {
    async fn admit(&self, operation: Operation) -> Result<Option<Box<dyn std::any::Any + Send>>, TaggedError> {
        self.admission
            .begin_access()
            .await
            .map_err(|detail| text_generation_error(operation.as_str(), detail))
    }
}

#[async_trait]
impl TextGeneration for AdmissionGuarded {
    async fn generate_commit_message(&self, input: CommitMessageGenerationInput) -> Result<CommitMessageGenerationResult, TaggedError> {
        let _access = self.admit(Operation::GenerateCommitMessage).await?;
        self.inner.generate_commit_message(input).await
    }

    async fn generate_pr_content(&self, input: PrContentGenerationInput) -> Result<PrContentGenerationResult, TaggedError> {
        let _access = self.admit(Operation::GeneratePrContent).await?;
        self.inner.generate_pr_content(input).await
    }

    async fn generate_branch_name(&self, input: BranchNameGenerationInput) -> Result<String, TaggedError> {
        let _access = self.admit(Operation::GenerateBranchName).await?;
        self.inner.generate_branch_name(input).await
    }

    async fn generate_thread_title(&self, input: ThreadTitleGenerationInput) -> Result<ThreadTitleGenerationResult, TaggedError> {
        let _access = self.admit(Operation::GenerateThreadTitle).await?;
        self.inner.generate_thread_title(input).await
    }
}

/// A driver whose instances get their text generator from [`text_generation_for`] when the
/// driver did not set one.
pub struct TextGenerationDriver {
    inner: Arc<dyn Driver>,
    env: DriverEnv,
}

impl TextGenerationDriver {
    pub fn new(inner: Arc<dyn Driver>, env: DriverEnv) -> Self {
        Self { inner, env }
    }
}

#[async_trait]
impl Driver for TextGenerationDriver {
    fn driver_kind(&self) -> ProviderDriverKind {
        self.inner.driver_kind()
    }

    fn metadata(&self) -> DriverMetadata {
        self.inner.metadata()
    }

    fn decode_config(&self, raw: &Value) -> Result<Value, String> {
        self.inner.decode_config(raw)
    }

    fn default_config(&self) -> Value {
        self.inner.default_config()
    }

    async fn create(&self, input: DriverCreateInput) -> Result<ProviderInstance, ProviderDriverError> {
        let config = input.config.clone();
        let enabled = input.enabled;
        let environment: BTreeMap<String, String> = self.env.instance_env(&input.environment).into_iter().collect();
        let mut instance = self.inner.create(input).await?;
        if instance.text_generation.is_none() {
            let kind = instance.driver_kind.to_string();
            instance.text_generation = text_generation_for(
                TextGenerationInstanceInput {
                    driver_kind: &kind,
                    config: &config,
                    enabled,
                    environment,
                    snapshot: Some(instance.snapshot.clone()),
                },
                &self.env,
            );
        }
        Ok(instance)
    }
}

/// Wrap the server's drivers (`app::plugins::drivers`) so their instances generate text.
pub fn with_text_generation(drivers: Vec<Arc<dyn Driver>>, env: &DriverEnv) -> Vec<Arc<dyn Driver>> {
    drivers
        .into_iter()
        .map(|driver| Arc::new(TextGenerationDriver::new(driver, env.clone())) as Arc<dyn Driver>)
        .collect()
}
