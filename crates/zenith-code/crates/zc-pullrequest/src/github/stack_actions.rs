//! `pullRequest/githubStackActions.ts`: merging and rebasing a host-native GitHub stack
//! (`/repos/{owner}/{repo}/stacks`, a public preview) entirely on GitHub. A stack rebase never
//! switches or rewrites the environment's checkout.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use zc_contracts::{PullRequestAction, PullRequestMergeMethod, PullRequestStackHead, PullRequestState};
use zc_core::defect::Defect;
use zc_sourcecontrol::errors::{error_defect, Cause, CauseError};
use zc_sourcecontrol::github::cli::SchemaDecodeError;
use zc_sourcecontrol::github::{GitHubCli, GitHubCliError, GitHubExecuteInput};

use crate::github::json::decode_pull_request_stacks_json;

/// How long an accepted merge is polled before it is reported as still running.
const MERGE_POLL_DEADLINE: Duration = Duration::from_secs(5 * 60);

/// The members of the `GitHubStackActionError` union.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitHubStackActionErrorKind {
    Changed { completed: i64 },
    Unsupported,
    ResponseInvalid,
    MergeRejected,
    MergePending,
    Permission,
    RebaseFailed { completed: i64 },
}

/// `GitHubStackActionError`: every refusal of a stack operation, with the stack it was about.
#[derive(Debug, Clone)]
pub struct GitHubStackActionError {
    pub kind: GitHubStackActionErrorKind,
    pub repository: String,
    pub number: i64,
    pub stack_number: i64,
    pub cause: Option<Cause>,
}

impl GitHubStackActionError {
    pub fn tag(&self) -> &'static str {
        match self.kind {
            GitHubStackActionErrorKind::Changed { .. } => "GitHubStackChangedError",
            GitHubStackActionErrorKind::Unsupported => "GitHubStackUnsupportedError",
            GitHubStackActionErrorKind::ResponseInvalid => "GitHubStackResponseInvalidError",
            GitHubStackActionErrorKind::MergeRejected => "GitHubStackMergeRejectedError",
            GitHubStackActionErrorKind::MergePending => "GitHubStackMergePendingError",
            GitHubStackActionErrorKind::Permission => "GitHubStackPermissionError",
            GitHubStackActionErrorKind::RebaseFailed { .. } => "GitHubStackRebaseFailedError",
        }
    }

    pub fn message(&self) -> String {
        match self.kind {
            GitHubStackActionErrorKind::Changed { completed } if completed > 0 => format!(
                "The stack changed at PR #{} after {completed} layers. Earlier updates remain on GitHub. Refresh it before trying again.",
                self.number
            ),
            GitHubStackActionErrorKind::Changed { .. } => "The stack changed. Refresh it before trying again.".into(),
            GitHubStackActionErrorKind::Unsupported => "This operation is not supported for this stack.".into(),
            GitHubStackActionErrorKind::ResponseInvalid => "GitHub returned an unreadable stack operation response.".into(),
            GitHubStackActionErrorKind::MergeRejected => "GitHub refused the stack merge. Check the stack's branch rules and merge requirements.".into(),
            GitHubStackActionErrorKind::MergePending => {
                "The merge is still running on GitHub. Check its status there before submitting another request.".into()
            }
            GitHubStackActionErrorKind::Permission => {
                "You cannot update every branch in this stack. Check write access and fork maintainer permissions before retrying.".into()
            }
            GitHubStackActionErrorKind::RebaseFailed { completed } => format!(
                "Stack rebase stopped at PR #{} after {completed} layers. Earlier updates remain on GitHub; resolve the failing layer before retrying.",
                self.number
            ),
        }
    }

    /// Every stack error's `detail` is its message.
    pub fn detail(&self) -> String {
        self.message()
    }

    /// `completed`, for the two kinds that count layers.
    pub fn completed(&self) -> Option<i64> {
        match self.kind {
            GitHubStackActionErrorKind::Changed { completed } | GitHubStackActionErrorKind::RebaseFailed { completed } => Some(completed),
            _ => None,
        }
    }
}

impl std::fmt::Display for GitHubStackActionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GitHubStackActionError {}

impl CauseError for GitHubStackActionError {
    fn defect(&self) -> Value {
        error_defect(self.tag(), self.message(), self.cause.as_ref().map(Cause::defect))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// What [`run_github_stack_action`] fails with: a refusal of its own, or `gh` itself.
#[derive(Debug, Clone)]
pub enum StackActionFailure {
    Stack(GitHubStackActionError),
    Cli(GitHubCliError),
}

impl From<GitHubStackActionError> for StackActionFailure {
    fn from(error: GitHubStackActionError) -> Self {
        Self::Stack(error)
    }
}

impl From<GitHubCliError> for StackActionFailure {
    fn from(error: GitHubCliError) -> Self {
        Self::Cli(error)
    }
}

impl StackActionFailure {
    fn into_cause(self) -> Cause {
        match self {
            Self::Stack(error) => Cause::new(error),
            Self::Cli(error) => Cause::new(error),
        }
    }
}

/// `runGitHubStackAction` input.
#[derive(Debug, Clone)]
pub struct GitHubStackActionInput {
    pub cwd: String,
    pub repository: String,
    pub host: String,
    pub number: i64,
    pub stack_number: i64,
    pub expected_stack_heads: Option<Vec<PullRequestStackHead>>,
    pub action: PullRequestAction,
    pub merge_method: Option<PullRequestMergeMethod>,
}

/// `MergeResponse`.
#[derive(Debug, Clone, Deserialize)]
struct MergeResponse {
    status: MergeStatus,
    details: MergeDetails,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum MergeStatus {
    Pending,
    Merged,
    Enqueued,
    Failed,
}

impl MergeStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Merged => "merged",
            Self::Enqueued => "enqueued",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct MergeDetails {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

impl MergeResponse {
    /// The decoded value as `Schema.Defect()` keeps it: only the declared keys.
    fn encoded(&self) -> Value {
        let mut details = serde_json::Map::new();
        if let Some(uuid) = &self.details.uuid {
            details.insert("uuid".into(), json!(uuid));
        }
        if let Some(message) = &self.details.message {
            details.insert("message".into(), json!(message));
        }
        json!({"status": self.status.as_str(), "details": details})
    }
}

/// `Schema.NullOr(X)` as a required key: `null` is `None`, absence fails the decode.
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Debug, Deserialize)]
struct BranchAccess {
    data: BranchAccessData,
}

#[derive(Debug, Deserialize)]
struct BranchAccessData {
    #[serde(deserialize_with = "required_nullable")]
    repository: Option<HashMap<String, Option<BranchAccessPullRequest>>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchAccessPullRequest {
    #[serde(deserialize_with = "required_nullable")]
    head_repository: Option<HeadRepository>,
    maintainer_can_modify: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadRepository {
    #[serde(deserialize_with = "required_nullable")]
    viewer_permission: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RebaseBranch {
    data: RebaseBranchData,
}

#[derive(Debug, Deserialize)]
struct RebaseBranchData {
    #[serde(default)]
    processed: Option<Vec<Option<HeadOid>>>,
    repository: RebaseBranchRepository,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadOid {
    head_ref_oid: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RebaseBranchRepository {
    pull_request: RebaseBranchPullRequest,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RebaseBranchPullRequest {
    id: String,
    head_ref_oid: String,
    base_ref: RebaseBaseRef,
}

#[derive(Debug, Deserialize)]
struct RebaseBaseRef {
    compare: RebaseCompare,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RebaseCompare {
    behind_by: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RebaseResponse {
    data: RebaseResponseData,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RebaseResponseData {
    update_pull_request_branch: RebaseResponseUpdate,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RebaseResponseUpdate {
    pull_request: HeadOid,
}

/// `Schema.decodeEffect(Schema.fromJsonString(schema))`.
fn decode<T: for<'de> Deserialize<'de>>(raw: &str) -> Result<T, Cause> {
    serde_json::from_str(raw).map_err(|error| Cause::new(SchemaDecodeError(error.to_string())))
}

struct Processed {
    id: String,
    number: i64,
    head_sha: String,
}

/// `runGitHubStackAction`: a merge of every open layer up to the selected one in one atomic
/// request, or a rebase of every open layer bottom to top. Both refuse to act on a stack that no
/// longer matches the heads the reader reviewed.
pub async fn run_github_stack_action(github: &GitHubCli, input: GitHubStackActionInput) -> Result<(), StackActionFailure> {
    let failure = |kind: GitHubStackActionErrorKind, number: i64, cause: Option<Cause>| GitHubStackActionError {
        kind,
        repository: input.repository.clone(),
        number,
        stack_number: input.stack_number,
        cause,
    };
    if input.action != PullRequestAction::Merge && input.action != PullRequestAction::UpdateBranch {
        return Err(failure(GitHubStackActionErrorKind::Unsupported, input.number, None).into());
    }
    let endpoint = format!("repos/{}", input.repository);
    let read = github
        .execute(GitHubExecuteInput::new(
            &input.cwd,
            [
                "api".to_owned(),
                "--hostname".into(),
                input.host.clone(),
                format!("{endpoint}/stacks?pull_request={}", input.number),
            ],
        ))
        .await?;
    let stack = decode_pull_request_stacks_json(&read.stdout)
        .map_err(|cause| failure(GitHubStackActionErrorKind::ResponseInvalid, input.number, Some(cause.cause())))?;
    let changed = |number: i64, completed: i64| failure(GitHubStackActionErrorKind::Changed { completed }, number, None);
    let Some(stack) = stack.filter(|stack| stack.number == input.stack_number) else {
        return Err(changed(input.number, 0).into());
    };
    let Some(target_index) = stack.layers.iter().position(|layer| layer.number == input.number) else {
        return Err(changed(input.number, 0).into());
    };
    if input.action == PullRequestAction::UpdateBranch && target_index != stack.layers.len() - 1 {
        return Err(changed(input.number, 0).into());
    }
    let target = &stack.layers[target_index];
    let affected = if input.action == PullRequestAction::Merge {
        &stack.layers[..=target_index]
    } else {
        &stack.layers[..]
    };
    let open: Vec<_> = affected.iter().filter(|layer| layer.state != PullRequestState::Merged).collect();
    if input.action == PullRequestAction::Merge && target.state != PullRequestState::Open {
        return Err(failure(GitHubStackActionErrorKind::Unsupported, input.number, None).into());
    }
    let heads_match = match &input.expected_stack_heads {
        None => false,
        Some(expected) => {
            expected.len() == open.len()
                && expected.iter().map(|layer| layer.number).collect::<HashSet<_>>().len() == open.len()
                && open.iter().all(|layer| match layer.head_sha.as_deref() {
                    Some(sha) if !sha.is_empty() => expected.iter().any(|head| head.number == layer.number && head.head_sha == sha),
                    _ => false,
                })
        }
    };
    if !heads_match {
        return Err(changed(input.number, 0).into());
    }
    if open.is_empty() || open.iter().any(|layer| layer.state != PullRequestState::Open) {
        return Err(failure(GitHubStackActionErrorKind::Unsupported, input.number, None).into());
    }

    if input.action == PullRequestAction::UpdateBranch {
        // `const [owner, name] = repository.split("/")`, interpolated as JS would.
        let mut parts = input.repository.split('/');
        let owner = parts.next().unwrap_or("undefined").to_owned();
        let name = parts.next().unwrap_or("undefined").to_owned();
        let selections: Vec<String> = open
            .iter()
            .map(|layer| {
                format!(
                    "pr{0}:pullRequest(number:{0}){{headRepository{{viewerPermission}} maintainerCanModify}}",
                    layer.number
                )
            })
            .collect();
        let permissions = github
            .execute(GitHubExecuteInput::new(
                &input.cwd,
                [
                    "api".to_owned(),
                    "--hostname".into(),
                    input.host.clone(),
                    "graphql".into(),
                    "-f".into(),
                    format!("owner={owner}"),
                    "-f".into(),
                    format!("name={name}"),
                    "-f".into(),
                    format!(
                        "query=query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{{}}}}}",
                        selections.join(" ")
                    ),
                ],
            ))
            .await?;
        let access: BranchAccess =
            decode(&permissions.stdout).map_err(|cause| failure(GitHubStackActionErrorKind::ResponseInvalid, input.number, Some(cause)))?;
        // viewerCanUpdateBranch is false for an already-current layer, even if rebasing its parent
        // will make it stale. Check branch write access separately before touching any layer.
        let denied = open.iter().any(|layer| {
            let pr = access
                .data
                .repository
                .as_ref()
                .and_then(|repository| repository.get(&format!("pr{}", layer.number)))
                .and_then(Option::as_ref);
            match pr {
                Some(pr) => match &pr.head_repository {
                    None => true,
                    Some(head) => !pr.maintainer_can_modify && !["ADMIN", "MAINTAIN", "WRITE"].contains(&head.viewer_permission.as_deref().unwrap_or("")),
                },
                None => true,
            }
        });
        if denied {
            return Err(failure(GitHubStackActionErrorKind::Permission, input.number, None).into());
        }
        let mut processed: Vec<Processed> = Vec::new();
        for (index, layer) in open.iter().enumerate() {
            let completed = index as i64;
            let head_sha = layer.head_sha.clone().unwrap_or_else(|| "undefined".into());
            let step: Result<(), StackActionFailure> = async {
                let processed_selection = if processed.is_empty() {
                    String::new()
                } else {
                    let ids: Vec<&str> = processed.iter().map(|head| head.id.as_str()).collect();
                    format!(
                        "processed:nodes(ids:{}){{... on PullRequest{{headRefOid}}}}",
                        serde_json::to_string(&ids).unwrap_or_else(|_| "[]".into())
                    )
                };
                let read = github
                    .execute(GitHubExecuteInput::new(
                        &input.cwd,
                        [
                            "api".to_owned(),
                            "--hostname".into(),
                            input.host.clone(),
                            "graphql".into(),
                            "-f".into(),
                            format!("owner={owner}"),
                            "-f".into(),
                            format!("name={name}"),
                            "-F".into(),
                            format!("number={}", layer.number),
                            "-f".into(),
                            format!("sha={head_sha}"),
                            "-f".into(),
                            format!(
                                "query=query($owner:String!,$name:String!,$number:Int!,$sha:String!){{{processed_selection} repository(owner:$owner,name:$name){{pullRequest(number:$number){{id headRefOid baseRef{{compare(headRef:$sha){{behindBy}}}}}}}}}}"
                            ),
                        ],
                    ))
                    .await?;
                let decoded: RebaseBranch = decode(&read.stdout)
                    .map_err(|cause| failure(GitHubStackActionErrorKind::RebaseFailed { completed }, layer.number, Some(cause)))?;
                let observed = decoded.data.processed.as_ref();
                // A push to an earlier layer must not silently become the next layer's new base.
                let moved = processed.iter().enumerate().find(|(position, head)| {
                    observed.and_then(|nodes| nodes.get(*position)).and_then(Option::as_ref).map(|node| node.head_ref_oid.as_str())
                        != Some(head.head_sha.as_str())
                });
                if let Some((_, head)) = moved {
                    return Err(changed(head.number, completed).into());
                }
                let pr = decoded.data.repository.pull_request;
                if Some(pr.head_ref_oid.as_str()) != layer.head_sha.as_deref() {
                    return Err(changed(layer.number, completed).into());
                }
                if pr.base_ref.compare.behind_by == 0 {
                    processed.push(Processed {
                        id: pr.id,
                        number: layer.number,
                        head_sha: pr.head_ref_oid,
                    });
                    return Ok(());
                }
                // Pass the reviewed revision to GitHub, including when a push races this read.
                let updated = github
                    .execute(GitHubExecuteInput::new(
                        &input.cwd,
                        [
                            "api".to_owned(),
                            "--hostname".into(),
                            input.host.clone(),
                            "graphql".into(),
                            "-f".into(),
                            format!("id={}", pr.id),
                            "-f".into(),
                            format!("sha={head_sha}"),
                            "-f".into(),
                            "query=mutation($id:ID!,$sha:GitObjectID!){updatePullRequestBranch(input:{pullRequestId:$id,expectedHeadOid:$sha,updateMethod:REBASE}){pullRequest{headRefOid}}}".into(),
                        ],
                    ))
                    .await?;
                let response: RebaseResponse = decode(&updated.stdout)
                    .map_err(|cause| failure(GitHubStackActionErrorKind::RebaseFailed { completed }, layer.number, Some(cause)))?;
                processed.push(Processed {
                    id: pr.id,
                    number: layer.number,
                    head_sha: response.data.update_pull_request_branch.pull_request.head_ref_oid,
                });
                Ok(())
            }
            .await;
            match step {
                Ok(()) => {}
                Err(StackActionFailure::Stack(error)) if matches!(error.kind, GitHubStackActionErrorKind::Changed { .. }) => {
                    return Err(error.into());
                }
                Err(StackActionFailure::Stack(error)) if matches!(error.kind, GitHubStackActionErrorKind::RebaseFailed { .. }) => {
                    // A decode failure inside the step is already the layer's failure, with the
                    // schema error as its cause.
                    return Err(error.into());
                }
                Err(other) => {
                    return Err(failure(GitHubStackActionErrorKind::RebaseFailed { completed }, layer.number, Some(other.into_cause())).into());
                }
            }
        }
        return Ok(());
    }

    if open.iter().any(|layer| layer.is_draft == Some(true)) {
        return Err(failure(GitHubStackActionErrorKind::Unsupported, input.number, None).into());
    }
    let decode_merge = |raw: &str| -> Result<MergeResponse, StackActionFailure> {
        decode(raw).map_err(|cause| failure(GitHubStackActionErrorKind::ResponseInvalid, input.number, Some(cause)).into())
    };
    let request = github
        .execute(GitHubExecuteInput::new(
            &input.cwd,
            [
                "api".to_owned(),
                "--hostname".into(),
                input.host.clone(),
                "--method".into(),
                "PUT".into(),
                format!("{endpoint}/pulls/{}/merge-async", input.number),
                "-f".into(),
                format!("merge_method={}", input.merge_method.unwrap_or(PullRequestMergeMethod::Merge).as_str()),
                "-f".into(),
                "merge_action=default".into(),
                "-f".into(),
                format!("sha={}", target.head_sha.as_deref().unwrap_or("undefined")),
            ],
        ))
        .await?;
    let mut result = decode_merge(&request.stdout)?;
    let deadline = tokio::time::Instant::now() + MERGE_POLL_DEADLINE;
    let mut attempt: u32 = 0;
    while result.status == MergeStatus::Pending && tokio::time::Instant::now() < deadline {
        let uuid = match result.details.uuid.as_deref() {
            Some(uuid) if !uuid.is_empty() => uuid.to_owned(),
            _ => return Err(failure(GitHubStackActionErrorKind::ResponseInvalid, input.number, None).into()),
        };
        let delay = 1_000_u64.saturating_mul(1_u64 << attempt.min(20)).min(10_000);
        tokio::time::sleep(Duration::from_millis(delay)).await;
        let poll = github
            .execute(GitHubExecuteInput::new(
                &input.cwd,
                [
                    "api".to_owned(),
                    "--hostname".into(),
                    input.host.clone(),
                    format!(
                        "{endpoint}/pulls/{}/merge-async/{}",
                        input.number,
                        zc_sourcecontrol::util::encode_uri_component(&uuid)
                    ),
                ],
            ))
            .await?;
        result = decode_merge(&poll.stdout)?;
        attempt += 1;
    }
    match result.status {
        MergeStatus::Pending => Err(failure(GitHubStackActionErrorKind::MergePending, input.number, None).into()),
        MergeStatus::Failed => Err(failure(
            GitHubStackActionErrorKind::MergeRejected,
            input.number,
            Some(Cause::new(Defect(result.encoded()))),
        )
        .into()),
        MergeStatus::Merged | MergeStatus::Enqueued => Ok(()),
    }
}
