//! What every provider's text generator shares (the bodies of `generateCommitMessage`,
//! `generatePrContent`, `generateBranchName`, `generateThreadTitle` in
//! `ClaudeTextGeneration.ts`, `CodexTextGeneration.ts`, `CursorTextGeneration.ts`, …): build
//! the prompt, run it through the provider's one-shot backend, decode the structured output and
//! sanitize it.
//!
//! A provider only implements [`OneShotBackend`]: run one prompt, return the raw structured
//! output. That is the seam the ACP (Cursor, Grok, Antigravity) and OpenCode generators plug
//! into once their runtimes exist in Rust (WP-15, WP-16).

use async_trait::async_trait;
use serde_json::Value;
use zc_ports::text_generation::{
    BranchNameGenerationInput, CommitMessageGenerationInput, CommitMessageGenerationResult, PrContentGenerationInput, PrContentGenerationResult,
    ThreadTitleGenerationInput, ThreadTitleGenerationResult,
};
use zc_ports::{TaggedError, TextGeneration};

use crate::prompts::{
    build_branch_name_prompt, build_commit_message_prompt, build_pr_content_prompt, build_thread_title_prompt, BranchNamePromptInput, CommitMessagePromptInput,
    PrContentPromptInput, Prompt, ThreadTitlePromptInput,
};
use crate::schema::{Decoded, OutputSchema};
use crate::utils::{
    sanitize_branch_fragment, sanitize_commit_subject, sanitize_feature_branch_name, sanitize_pr_title, sanitize_thread_title, text_generation_error, Operation,
};

/// One prompt for a backend.
#[derive(Debug, Clone, PartialEq)]
pub struct OneShotRequest {
    pub operation: Operation,
    pub cwd: String,
    pub prompt: String,
    pub output_schema: OutputSchema,
    /// The wire `ModelSelection` (`{instanceId, model, options?}`).
    pub model_selection: Value,
    /// Wire `ChatAttachment`s, for the operations that carry them (branch names and titles).
    pub attachments: Vec<Value>,
}

/// A provider's one-shot runner.
#[async_trait]
pub trait OneShotBackend: Send + Sync {
    /// The subject of `<label> returned invalid structured output.` (`Claude`, `Codex`, …).
    fn output_label(&self) -> &str;

    /// Run one prompt and return its structured output, `None` when nothing decodable came
    /// back. Failures are `TextGenerationError`s for `request.operation`.
    async fn run(&self, request: OneShotRequest) -> Result<Option<Value>, TaggedError>;
}

/// A [`TextGeneration`] over a [`OneShotBackend`].
pub struct OneShotTextGeneration<B> {
    backend: B,
}

impl<B: OneShotBackend> OneShotTextGeneration<B> {
    pub fn new(backend: B) -> Self {
        Self { backend }
    }

    pub fn backend(&self) -> &B {
        &self.backend
    }

    async fn run(&self, operation: Operation, cwd: &str, prompt: Prompt, model_selection: &Value, attachments: Vec<Value>) -> Result<Decoded, TaggedError> {
        let output = self
            .backend
            .run(OneShotRequest {
                operation,
                cwd: cwd.to_owned(),
                prompt: prompt.prompt,
                output_schema: prompt.output_schema,
                model_selection: model_selection.clone(),
                attachments,
            })
            .await?;
        output.as_ref().and_then(|value| prompt.output_schema.decode(value)).ok_or_else(|| {
            text_generation_error(
                operation.as_str(),
                format!("{} returned invalid structured output.", self.backend.output_label()),
            )
        })
    }
}

fn wire_attachments(attachments: &[zc_ports::contracts::ChatAttachment]) -> Vec<Value> {
    attachments.iter().map(|attachment| attachment.0.clone()).collect()
}

fn unexpected(operation: Operation) -> TaggedError {
    text_generation_error(operation.as_str(), "Unexpected structured output shape.")
}

#[async_trait]
impl<B: OneShotBackend> TextGeneration for OneShotTextGeneration<B> {
    async fn generate_commit_message(&self, input: CommitMessageGenerationInput) -> Result<CommitMessageGenerationResult, TaggedError> {
        let prompt = build_commit_message_prompt(&CommitMessagePromptInput {
            branch: input.branch.as_deref(),
            staged_summary: &input.staged_summary,
            staged_patch: &input.staged_patch,
            include_branch: input.include_branch,
            policy: input.policy.as_ref(),
        });
        let operation = Operation::GenerateCommitMessage;
        match self.run(operation, &input.cwd, prompt, &input.model_selection.0, Vec::new()).await? {
            Decoded::CommitMessage { subject, body, branch } => Ok(CommitMessageGenerationResult {
                subject: sanitize_commit_subject(&subject),
                body: crate::js::trim(&body).to_owned(),
                branch: branch.map(|branch| sanitize_feature_branch_name(&branch)),
            }),
            _ => Err(unexpected(operation)),
        }
    }

    async fn generate_pr_content(&self, input: PrContentGenerationInput) -> Result<PrContentGenerationResult, TaggedError> {
        let prompt = build_pr_content_prompt(&PrContentPromptInput {
            base_branch: &input.base_branch,
            head_branch: &input.head_branch,
            commit_summary: &input.commit_summary,
            diff_summary: &input.diff_summary,
            diff_patch: &input.diff_patch,
            change_request_template: input.change_request_template.as_deref(),
            policy: input.policy.as_ref(),
        });
        let operation = Operation::GeneratePrContent;
        match self.run(operation, &input.cwd, prompt, &input.model_selection.0, Vec::new()).await? {
            Decoded::PrContent { title, body } => Ok(PrContentGenerationResult {
                title: sanitize_pr_title(&title),
                body: crate::js::trim(&body).to_owned(),
            }),
            _ => Err(unexpected(operation)),
        }
    }

    async fn generate_branch_name(&self, input: BranchNameGenerationInput) -> Result<String, TaggedError> {
        let attachments = wire_attachments(&input.attachments);
        // The providers build branch-name prompts without the policy (as in TS).
        let prompt = build_branch_name_prompt(&BranchNamePromptInput {
            message: &input.message,
            attachments: &attachments,
            policy: None,
        });
        let operation = Operation::GenerateBranchName;
        match self.run(operation, &input.cwd, prompt, &input.model_selection.0, attachments.clone()).await? {
            Decoded::BranchName { branch } => Ok(sanitize_branch_fragment(&branch)),
            _ => Err(unexpected(operation)),
        }
    }

    async fn generate_thread_title(&self, input: ThreadTitleGenerationInput) -> Result<ThreadTitleGenerationResult, TaggedError> {
        let attachments = wire_attachments(&input.attachments);
        let prompt = build_thread_title_prompt(&ThreadTitlePromptInput {
            linked_context: input.linked_context.as_deref(),
            message: &input.message,
            previous_title: input.previous_title.as_deref(),
            attachments: &attachments,
            policy: None,
        });
        let operation = Operation::GenerateThreadTitle;
        match self.run(operation, &input.cwd, prompt, &input.model_selection.0, attachments.clone()).await? {
            Decoded::ThreadTitle { title, needs_refinement } => Ok(ThreadTitleGenerationResult {
                title: sanitize_thread_title(&title),
                needs_refinement: needs_refinement.then_some(true),
            }),
            _ => Err(unexpected(operation)),
        }
    }
}
