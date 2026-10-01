//! The text generators whose runtimes are not in Rust yet.
//!
//! TS runs Cursor, Grok and Antigravity text generation over a one-shot ACP session
//! (`CursorTextGeneration.ts`, `GrokTextGeneration.ts`, `AntigravityTextGeneration.ts`: client
//! info `t3-code-git-text`, the prompt as one `session/prompt`, the agent's message chunks
//! parsed with `extractJsonObject`), and OpenCode over an SDK session on its shared server
//! (`OpenCodeTextGeneration.ts`, `provider/model` selections). Each becomes a
//! [`crate::OneShotBackend`] once WP-15 (ACP runtime) and WP-16 (OpenCode runtime) land; until
//! then every call fails with a `TextGenerationError` that says so, which the reactors log and
//! skip exactly like an unavailable CLI.

use async_trait::async_trait;
use zc_ports::text_generation::{
    BranchNameGenerationInput, CommitMessageGenerationInput, CommitMessageGenerationResult, PrContentGenerationInput, PrContentGenerationResult,
    ThreadTitleGenerationInput, ThreadTitleGenerationResult,
};
use zc_ports::{TaggedError, TextGeneration};

use crate::utils::{text_generation_error, Operation};

/// A driver without a Rust text generation runtime yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedTextGeneration {
    /// The provider's display name (`Cursor`, `Grok`, `Antigravity`, `OpenCode`).
    pub provider: String,
}

impl UnsupportedTextGeneration {
    pub fn new(provider: impl Into<String>) -> Self {
        Self { provider: provider.into() }
    }

    /// The display name the error uses for a driver kind.
    pub fn for_driver_kind(driver_kind: &str) -> Self {
        Self::new(match driver_kind {
            "cursor" => "Cursor",
            "grok" => "Grok",
            "antigravity" => "Antigravity",
            "opencode" => "OpenCode",
            other => other,
        })
    }

    fn error(&self, operation: Operation) -> TaggedError {
        text_generation_error(
            operation.as_str(),
            format!(
                "{} text generation is not supported by this server yet. Choose a Claude or Codex model for text generation.",
                self.provider
            ),
        )
    }
}

#[async_trait]
impl TextGeneration for UnsupportedTextGeneration {
    async fn generate_commit_message(&self, _input: CommitMessageGenerationInput) -> Result<CommitMessageGenerationResult, TaggedError> {
        Err(self.error(Operation::GenerateCommitMessage))
    }

    async fn generate_pr_content(&self, _input: PrContentGenerationInput) -> Result<PrContentGenerationResult, TaggedError> {
        Err(self.error(Operation::GeneratePrContent))
    }

    async fn generate_branch_name(&self, _input: BranchNameGenerationInput) -> Result<String, TaggedError> {
        Err(self.error(Operation::GenerateBranchName))
    }

    async fn generate_thread_title(&self, _input: ThreadTitleGenerationInput) -> Result<ThreadTitleGenerationResult, TaggedError> {
        Err(self.error(Operation::GenerateThreadTitle))
    }
}
