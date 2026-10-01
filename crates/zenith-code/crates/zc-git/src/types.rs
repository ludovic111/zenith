//! GitManager's internal records (`PullRequestInfo`, `BranchHeadContext`, …) and the wire
//! shapes of `git.ts` it produces: the stacked action input, its result, its progress events,
//! and the pull request resolve/prepare results.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use zc_contracts::{ChangeRequest, ChangeRequestState, DateTimeUtc};
use zc_vcs::contracts::{ChangeRequestState as StatusState, Nullable, VcsStatusChangeRequest};

/// `PullRequestInfo`: a change request as the head matching and the status read it.
#[derive(Debug, Clone, PartialEq)]
pub struct PullRequestInfo {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub base_ref_name: String,
    pub head_ref_name: String,
    pub state: ChangeRequestState,
    pub is_draft: bool,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub updated_at: Option<DateTimeUtc>,
    pub is_cross_repository: Option<bool>,
    pub head_repository_name_with_owner: Option<String>,
    pub head_repository_owner_login: Option<String>,
}

impl PullRequestInfo {
    /// `toPullRequestInfo(summary)`.
    pub fn from_change_request(summary: &ChangeRequest) -> Self {
        Self {
            number: summary.number,
            title: summary.title.clone(),
            url: summary.url.clone(),
            base_ref_name: summary.base_ref_name.clone(),
            head_ref_name: summary.head_ref_name.clone(),
            state: summary.state,
            is_draft: summary.is_draft == Some(true),
            closed_at: summary.closed_at.clone().flatten(),
            merged_at: summary.merged_at.clone().flatten(),
            updated_at: summary.updated_at.0,
            is_cross_repository: summary.is_cross_repository,
            head_repository_name_with_owner: summary.head_repository_name_with_owner.clone().flatten(),
            head_repository_owner_login: summary.head_repository_owner_login.clone().flatten(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.state == ChangeRequestState::Open
    }

    /// `toStatusPr(pr)`.
    pub fn to_status_pr(&self) -> VcsStatusChangeRequest {
        VcsStatusChangeRequest {
            number: self.number.max(0) as u64,
            title: self.title.clone(),
            url: self.url.clone(),
            base_ref: self.base_ref_name.clone(),
            head_ref: self.head_ref_name.clone(),
            state: match self.state {
                ChangeRequestState::Open => StatusState::Open,
                ChangeRequestState::Closed => StatusState::Closed,
                ChangeRequestState::Merged => StatusState::Merged,
            },
            is_draft: self.is_draft.then_some(true),
            updated_at: match self.updated_at {
                Some(updated_at) => Nullable::Value(updated_at.to_iso_string()),
                None => Nullable::Null,
            },
        }
    }
}

/// `BranchHeadContext`: where a branch's change request would come from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BranchHeadContext {
    pub local_branch: String,
    pub head_branch: String,
    pub head_selectors: Vec<String>,
    pub preferred_head_selector: String,
    pub remote_name: Option<String>,
    pub head_remote_url_key: Option<String>,
    pub target_remote_url_key: Option<String>,
    pub head_repository_name_with_owner: Option<String>,
    pub head_repository_owner_login: Option<String>,
    pub is_cross_repository: bool,
}

/// `ResolvedPullRequest` (`GitResolvedPullRequest` on the wire) plus the head remote info.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPullRequest {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub base_branch: String,
    pub head_branch: String,
    pub state: ChangeRequestState,
    pub is_cross_repository: Option<bool>,
    pub head_repository_name_with_owner: Option<String>,
    pub head_repository_owner_login: Option<String>,
}

impl ResolvedPullRequest {
    /// `toResolvedPullRequest` + `toPullRequestHeadRemoteInfo`.
    pub fn from_change_request(pr: &ChangeRequest) -> Self {
        Self {
            number: pr.number,
            title: pr.title.clone(),
            url: pr.url.clone(),
            base_branch: pr.base_ref_name.clone(),
            head_branch: pr.head_ref_name.clone(),
            state: pr.state,
            is_cross_repository: pr.is_cross_repository,
            head_repository_name_with_owner: pr.head_repository_name_with_owner.clone().flatten(),
            head_repository_owner_login: pr.head_repository_owner_login.clone().flatten(),
        }
    }

    /// `GitResolvedPullRequest`.
    pub fn to_wire(&self) -> Value {
        json!({
            "number": self.number,
            "title": self.title,
            "url": self.url,
            "baseBranch": self.base_branch,
            "headBranch": self.head_branch,
            "state": self.state.as_str(),
        })
    }

    /// `resolveHeadRepositoryNameWithOwner(pullRequest)`.
    pub fn head_repository(&self) -> Option<String> {
        crate::helpers::resolve_head_repository_name_with_owner(
            &self.url,
            self.is_cross_repository,
            self.head_repository_name_with_owner.as_deref(),
            self.head_repository_owner_login.as_deref(),
        )
    }
}

/// `GitStackedAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitStackedAction {
    Commit,
    Push,
    CreatePr,
    CommitPush,
    CommitPushPr,
}

impl GitStackedAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Commit => "commit",
            Self::Push => "push",
            Self::CreatePr => "create_pr",
            Self::CommitPush => "commit_push",
            Self::CommitPushPr => "commit_push_pr",
        }
    }

    /// `isCommitAction`.
    pub fn is_commit(self) -> bool {
        matches!(self, Self::Commit | Self::CommitPush | Self::CommitPushPr)
    }
}

/// `GitActionProgressPhase`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GitActionProgressPhase {
    Branch,
    Commit,
    Push,
    Pr,
}

fn trimmed_non_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    let value = String::deserialize(deserializer)?;
    let trimmed = zc_textgen::js::trim(&value);
    if trimmed.is_empty() {
        return Err(serde::de::Error::custom("Expected a non empty string"));
    }
    Ok(trimmed.to_owned())
}

fn optional_commit_message<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<String>, D::Error> {
    let value = trimmed_non_empty(deserializer)?;
    if zc_textgen::js::len16(&value) > 10_000 {
        return Err(serde::de::Error::custom("Expected a string with a length of at most 10000"));
    }
    Ok(Some(value))
}

fn optional_file_paths<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Vec<String>>, D::Error> {
    let values = Vec::<String>::deserialize(deserializer)?;
    if values.is_empty() {
        return Err(serde::de::Error::custom("Expected an array of at least 1 item"));
    }
    values
        .into_iter()
        .map(|value| {
            let trimmed = zc_textgen::js::trim(&value);
            if trimmed.is_empty() {
                Err(serde::de::Error::custom("Expected a non empty string"))
            } else {
                Ok(trimmed.to_owned())
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// `GitRunStackedActionInput` as the server decodes it (trimmed strings, the TS checks).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitRunStackedActionInput {
    #[serde(deserialize_with = "trimmed_non_empty")]
    pub action_id: String,
    #[serde(deserialize_with = "trimmed_non_empty")]
    pub cwd: String,
    pub action: GitStackedAction,
    #[serde(default, deserialize_with = "optional_commit_message", skip_serializing_if = "Option::is_none")]
    pub commit_message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_branch: Option<bool>,
    #[serde(default, deserialize_with = "optional_file_paths", skip_serializing_if = "Option::is_none")]
    pub file_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// `GitPullRequestRefInput`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitPullRequestRefInput {
    #[serde(deserialize_with = "trimmed_non_empty")]
    pub cwd: String,
    #[serde(deserialize_with = "trimmed_non_empty")]
    pub reference: String,
}

/// `mode` of `GitPreparePullRequestThreadInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreparePullRequestThreadMode {
    Local,
    Worktree,
}

/// `GitPreparePullRequestThreadInput`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitPreparePullRequestThreadInput {
    #[serde(deserialize_with = "trimmed_non_empty")]
    pub cwd: String,
    #[serde(deserialize_with = "trimmed_non_empty")]
    pub reference: String,
    pub mode: PreparePullRequestThreadMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

/// `branch` of `GitRunStackedActionResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchStep {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `commit` of `GitRunStackedActionResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitStep {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

/// `push` of `GitRunStackedActionResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushStep {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub set_upstream: Option<bool>,
}

/// `pr` of `GitRunStackedActionResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrStep {
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

impl BranchStep {
    pub fn skipped() -> Self {
        Self {
            status: "skipped_not_requested".into(),
            name: None,
        }
    }
}

impl CommitStep {
    pub fn with_status(status: &str) -> Self {
        Self {
            status: status.into(),
            commit_sha: None,
            subject: None,
        }
    }
}

impl PushStep {
    pub fn skipped() -> Self {
        Self {
            status: "skipped_not_requested".into(),
            branch: None,
            upstream_branch: None,
            set_upstream: None,
        }
    }
}

impl PrStep {
    pub fn skipped() -> Self {
        Self {
            status: "skipped_not_requested".into(),
            url: None,
            number: None,
            base_branch: None,
            head_branch: None,
            title: None,
        }
    }

    pub fn has_pull_request(&self) -> bool {
        self.status == "created" || self.status == "opened_existing"
    }
}

/// `GitRunStackedActionToastCta`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToastCta {
    None,
    OpenPr { label: String, url: String },
    RunAction { label: String, action: ToastRunAction },
}

/// `GitRunStackedActionToastRunAction`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToastRunAction {
    pub kind: GitStackedAction,
}

/// `GitRunStackedActionToast`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Toast {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub cta: ToastCta,
}

/// `GitRunStackedActionResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRunStackedActionResult {
    pub action: GitStackedAction,
    pub branch: BranchStep,
    pub commit: CommitStep,
    pub push: PushStep,
    pub pr: PrStep,
    pub toast: Toast,
}

/// `GitActionProgressEvent` without its `actionId`/`cwd`/`action` context
/// (`GitActionProgressPayload`).
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressPayload {
    ActionStarted {
        phases: Vec<GitActionProgressPhase>,
    },
    PhaseStarted {
        phase: GitActionProgressPhase,
        label: String,
    },
    HookStarted {
        hook_name: String,
    },
    HookOutput {
        hook_name: Option<String>,
        stream: &'static str,
        text: String,
    },
    HookFinished {
        hook_name: String,
        exit_code: Option<i64>,
        duration_ms: Option<u64>,
    },
    ActionFinished {
        result: Box<GitRunStackedActionResult>,
    },
    ActionFailed {
        phase: Option<GitActionProgressPhase>,
        message: String,
    },
}

impl ProgressPayload {
    /// The full `GitActionProgressEvent` wire object.
    pub fn to_event(&self, action_id: &str, cwd: &str, action: GitStackedAction) -> Value {
        let mut event = json!({ "actionId": action_id, "cwd": cwd, "action": action.as_str() });
        let fields = match self {
            Self::ActionStarted { phases } => json!({ "kind": "action_started", "phases": phases }),
            Self::PhaseStarted { phase, label } => json!({ "kind": "phase_started", "phase": phase, "label": label }),
            Self::HookStarted { hook_name } => json!({ "kind": "hook_started", "hookName": hook_name }),
            Self::HookOutput { hook_name, stream, text } => {
                json!({ "kind": "hook_output", "hookName": hook_name, "stream": stream, "text": text })
            }
            Self::HookFinished {
                hook_name,
                exit_code,
                duration_ms,
            } => json!({ "kind": "hook_finished", "hookName": hook_name, "exitCode": exit_code, "durationMs": duration_ms }),
            Self::ActionFinished { result } => json!({ "kind": "action_finished", "result": result }),
            Self::ActionFailed { phase, message } => json!({ "kind": "action_failed", "phase": phase, "message": message }),
        };
        if let (Value::Object(event), Value::Object(fields)) = (&mut event, fields) {
            event.extend(fields);
        }
        event
    }
}
