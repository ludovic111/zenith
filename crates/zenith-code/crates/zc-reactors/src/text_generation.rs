//! A [`TextGeneration`] that generates nothing, until WP-17's zc-textgen lands: every call
//! fails with `TextGenerationError`, which the command reactor logs and skips (titles and
//! branch names stay as they are), exactly as when a provider CLI is unavailable.

use async_trait::async_trait;
use zc_ports::text_generation::{
    BranchNameGenerationInput, CommitMessageGenerationInput, CommitMessageGenerationResult, PrContentGenerationInput, PrContentGenerationResult,
    ThreadTitleGenerationInput, ThreadTitleGenerationResult,
};
use zc_ports::{TaggedError, TextGeneration};

/// See the module docs.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoTextGeneration;

fn unavailable(operation: &str) -> TaggedError {
    TaggedError::new(
        "TextGenerationError",
        format!("Text generation failed in {operation}: text generation is not available"),
    )
    .with("operation", operation)
    .with("detail", "text generation is not available")
}

#[async_trait]
impl TextGeneration for NoTextGeneration {
    async fn generate_commit_message(&self, _input: CommitMessageGenerationInput) -> Result<CommitMessageGenerationResult, TaggedError> {
        Err(unavailable("generateCommitMessage"))
    }

    async fn generate_pr_content(&self, _input: PrContentGenerationInput) -> Result<PrContentGenerationResult, TaggedError> {
        Err(unavailable("generatePrContent"))
    }

    async fn generate_branch_name(&self, _input: BranchNameGenerationInput) -> Result<String, TaggedError> {
        Err(unavailable("generateBranchName"))
    }

    async fn generate_thread_title(&self, _input: ThreadTitleGenerationInput) -> Result<ThreadTitleGenerationResult, TaggedError> {
        Err(unavailable("generateThreadTitle"))
    }
}
