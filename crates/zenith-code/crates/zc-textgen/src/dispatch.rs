//! `textGeneration/TextGeneration.ts` `make`: the server's [`TextGeneration`]. Every call is
//! routed by `modelSelection.instanceId` to that provider instance's own text generator; a
//! thread title without supplied linked context first resolves the message's links.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use zc_contracts::ProviderInstanceId;
use zc_ports::text_generation::{
    BranchNameGenerationInput, CommitMessageGenerationInput, CommitMessageGenerationResult, PrContentGenerationInput, PrContentGenerationResult,
    ThreadTitleGenerationInput, ThreadTitleGenerationResult,
};
use zc_ports::{TaggedError, TextGeneration};
use zc_providers::ProviderInstanceRegistry;

use crate::links::{resolve_thread_title_links, ThreadTitleLinkResolver};
use crate::utils::{text_generation_error, Operation};

/// Where the per-instance generators come from (the provider instance registry).
pub trait TextGenerationInstances: Send + Sync {
    /// `None`: no instance has this id. `Some(None)`: the instance has no text generation.
    fn text_generation_for(&self, instance_id: &str) -> Option<Option<Arc<dyn TextGeneration>>>;
}

impl TextGenerationInstances for ProviderInstanceRegistry {
    fn text_generation_for(&self, instance_id: &str) -> Option<Option<Arc<dyn TextGeneration>>> {
        self.get_instance(&ProviderInstanceId::from(instance_id))
            .map(|instance| instance.text_generation.clone())
    }
}

/// The text generation service.
#[derive(Clone)]
pub struct TextGenerationService {
    instances: Arc<dyn TextGenerationInstances>,
    links: Option<Arc<dyn ThreadTitleLinkResolver>>,
}

impl TextGenerationService {
    /// `links` is the source control provider registry; without one, titles get no linked
    /// context (as when no forge reads the message's links).
    pub fn new(instances: Arc<dyn TextGenerationInstances>, links: Option<Arc<dyn ThreadTitleLinkResolver>>) -> Self {
        Self { instances, links }
    }

    /// Over the provider instance registry.
    pub fn from_registry(registry: ProviderInstanceRegistry, links: Option<Arc<dyn ThreadTitleLinkResolver>>) -> Self {
        Self::new(Arc::new(registry), links)
    }

    fn resolve(&self, operation: Operation, model_selection: &Value) -> Result<Arc<dyn TextGeneration>, TaggedError> {
        let instance_id = model_selection.get("instanceId").and_then(Value::as_str).unwrap_or_default();
        match self.instances.text_generation_for(instance_id) {
            None => Err(text_generation_error(
                operation.as_str(),
                format!("No provider instance registered for id '{instance_id}'."),
            )),
            Some(None) => Err(text_generation_error(
                operation.as_str(),
                format!("Provider instance '{instance_id}' has no text generation."),
            )),
            Some(Some(generator)) => Ok(generator),
        }
    }
}

#[async_trait]
impl TextGeneration for TextGenerationService {
    async fn generate_commit_message(&self, input: CommitMessageGenerationInput) -> Result<CommitMessageGenerationResult, TaggedError> {
        self.resolve(Operation::GenerateCommitMessage, &input.model_selection.0)?
            .generate_commit_message(input)
            .await
    }

    async fn generate_pr_content(&self, input: PrContentGenerationInput) -> Result<PrContentGenerationResult, TaggedError> {
        self.resolve(Operation::GeneratePrContent, &input.model_selection.0)?
            .generate_pr_content(input)
            .await
    }

    async fn generate_branch_name(&self, input: BranchNameGenerationInput) -> Result<String, TaggedError> {
        self.resolve(Operation::GenerateBranchName, &input.model_selection.0)?
            .generate_branch_name(input)
            .await
    }

    async fn generate_thread_title(&self, mut input: ThreadTitleGenerationInput) -> Result<ThreadTitleGenerationResult, TaggedError> {
        let generator = self.resolve(Operation::GenerateThreadTitle, &input.model_selection.0)?;
        if input.linked_context.is_none() {
            if let Some(links) = &self.links {
                input.linked_context = resolve_thread_title_links(links.as_ref(), &input.message, &input.cwd).await;
            }
        }
        generator.generate_thread_title(input).await
    }
}
