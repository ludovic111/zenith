//! The text generation port (`apps/server/src/textGeneration/TextGeneration.ts`,
//! `TextGenerationPolicy.ts`).
//!
//! Implemented by zc-textgen (one-shot agent CLI calls routed by the model selection's
//! instance); consumed by the provider command reactor (thread titles, worktree branch names)
//! and the git stacked actions (commit messages, PR content).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::contracts::{ChatAttachment, ModelSelection, TextGenerationError};

/// `TextGenerationPolicyKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextGenerationPolicyKind {
    Default,
    ConventionalCommits,
    RepoConventions,
    Custom,
}

/// `TextGenerationPolicy`: how generated commit / PR / branch / title text should be written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextGenerationPolicy {
    pub kind: TextGenerationPolicyKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_request_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch_instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_title_instructions: Option<String>,
    pub infer_repository_conventions: bool,
}

/// `CommitMessageGenerationInput`.
#[derive(Debug, Clone, PartialEq)]
pub struct CommitMessageGenerationInput {
    pub cwd: String,
    pub branch: Option<String>,
    pub staged_summary: String,
    pub staged_patch: String,
    /// Also ask for a semantic branch name.
    pub include_branch: bool,
    pub policy: Option<TextGenerationPolicy>,
    pub model_selection: ModelSelection,
}

/// `CommitMessageGenerationResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitMessageGenerationResult {
    pub subject: String,
    pub body: String,
    /// Only when `include_branch` was set.
    pub branch: Option<String>,
}

/// `PrContentGenerationInput`.
#[derive(Debug, Clone, PartialEq)]
pub struct PrContentGenerationInput {
    pub cwd: String,
    pub base_branch: String,
    pub head_branch: String,
    pub commit_summary: String,
    pub diff_summary: String,
    pub diff_patch: String,
    pub change_request_template: Option<String>,
    pub policy: Option<TextGenerationPolicy>,
    pub model_selection: ModelSelection,
}

/// `PrContentGenerationResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrContentGenerationResult {
    pub title: String,
    pub body: String,
}

/// `BranchNameGenerationInput`.
#[derive(Debug, Clone, PartialEq)]
pub struct BranchNameGenerationInput {
    pub cwd: String,
    pub message: String,
    pub attachments: Vec<ChatAttachment>,
    pub model_selection: ModelSelection,
}

/// `ThreadTitleGenerationInput`.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadTitleGenerationInput {
    pub linked_context: Option<String>,
    pub cwd: String,
    pub message: String,
    /// Present when replacing an existing title from the thread history.
    pub previous_title: Option<String>,
    pub attachments: Vec<ChatAttachment>,
    pub model_selection: ModelSelection,
}

/// `ThreadTitleGenerationResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadTitleGenerationResult {
    pub title: String,
    pub needs_refinement: Option<bool>,
}

/// `TextGeneration`.
#[async_trait]
pub trait TextGeneration: Send + Sync {
    /// `generateCommitMessage(input)`.
    async fn generate_commit_message(&self, input: CommitMessageGenerationInput) -> Result<CommitMessageGenerationResult, TextGenerationError>;

    /// `generatePrContent(input)`.
    async fn generate_pr_content(&self, input: PrContentGenerationInput) -> Result<PrContentGenerationResult, TextGenerationError>;

    /// `generateBranchName(input)`: returns the raw suggestion (`{branch}`); callers sanitize.
    async fn generate_branch_name(&self, input: BranchNameGenerationInput) -> Result<String, TextGenerationError>;

    /// `generateThreadTitle(input)`.
    async fn generate_thread_title(&self, input: ThreadTitleGenerationInput) -> Result<ThreadTitleGenerationResult, TextGenerationError>;
}
