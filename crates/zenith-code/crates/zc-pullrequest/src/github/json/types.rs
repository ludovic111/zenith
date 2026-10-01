//! The interfaces `gitHubPullRequestJson.ts` exports (and the anonymous shapes its decoders
//! return), as structs. They serialize to the JSON the TS objects stringify to (camelCase keys,
//! absent optionals left out, nulls kept), which is what the golden test compares; a TS
//! `interface B extends A` is a struct holding `A` flattened.
//!
//! TS `Map`s become `BTreeMap`s (looked up, never iterated for order) and the `Set` a
//! `BTreeSet`; lists whose order matters stay `Vec`s.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use zc_contracts::prim::JsNumber;
use zc_contracts::{
    PullRequestActor, PullRequestCheck, PullRequestChecksState, PullRequestComment, PullRequestCommit, PullRequestFileViewed, PullRequestLabel,
    PullRequestMergeCapabilities, PullRequestMergeMethod, PullRequestMergeability, PullRequestOmittedFileStat, PullRequestReaction, PullRequestReviewDecision,
    PullRequestReviewThread, PullRequestStackMembership, PullRequestState, PullRequestThreadComment,
};

/// `GitHubPullRequestListItem`: one row of `gh pr list --json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestListItem {
    /// Set only on a search row that belongs to a host-native stack.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stack: Option<PullRequestStackMembership>,
    /// The author's node id, kept so a batch can resolve the avatar the listing does not carry.
    pub author_id: Option<String>,
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<PullRequestActor>,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub mergeability: PullRequestMergeability,
    /// `None` where GitHub has no verdict to summarise, which includes an unreviewed draft.
    pub review_decision: Option<PullRequestReviewDecision>,
    pub additions: i64,
    pub deletions: i64,
    pub created_at: String,
    pub updated_at: String,
    pub review_request_logins: Vec<String>,
    /// At least one outstanding request targets a team rather than an individual login.
    pub has_team_review_request: bool,
    pub labels: Vec<PullRequestLabel>,
    /// `None` where the head commit reported no checks, which is not the same as passing none.
    pub checks_state: Option<PullRequestChecksState>,
}

/// `GitHubPullRequestDetail`: the listing row plus what `gh pr view --json` adds.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestDetail {
    #[serde(flatten)]
    pub item: GitHubPullRequestListItem,
    /// Present only where GitHub said whether the head belongs to another repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_cross_repository: Option<bool>,
    /// The owner of the head branch's repository; `None` where `gh` did not say.
    pub head_repository_owner: Option<String>,
    /// `headSha?: string | null`; the decoders always set it (`None` = null).
    pub head_sha: Option<String>,
    pub body: String,
    pub changed_files: i64,
    pub merged_at: Option<String>,
    pub closed_at: Option<String>,
    pub checks: Vec<PullRequestCheck>,
    /// Absent where `gh` did not answer for auto-merge at all, which is not the same as off.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_enabled: Option<bool>,
    /// Absent where auto-merge is off or GitHub did not report the stored strategy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_merge_method: Option<PullRequestMergeMethod>,
}

/// `GitHubWorkflowRunApproval`: a workflow run waiting for a maintainer's approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitHubWorkflowRunApproval {
    pub id: i64,
    pub name: String,
    pub url: Option<String>,
}

/// `GitHubPullRequestHead`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestHead {
    pub number: i64,
    pub head_sha: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_cross_repository: Option<bool>,
    pub head_repository_owner: Option<String>,
}

/// `GitHubPullRequestActivity`: the conversation `gh pr view --json author,comments,reviews,commits` gives.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GitHubPullRequestActivity {
    pub author: Option<PullRequestActor>,
    pub comments: Vec<PullRequestComment>,
    pub commits: Vec<PullRequestCommit>,
}

/// `GitHubPullRequestListBatch`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestListBatch {
    pub items: Vec<GitHubPullRequestListItem>,
    /// Rows gh returned, counted before decoding, so a skipped row cannot hide a next page.
    pub raw_count: usize,
}

/// `GitHubPullRequestSearchItem`: a listing row that names the repository it came from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GitHubPullRequestSearchItem {
    #[serde(flatten)]
    pub item: GitHubPullRequestListItem,
    /// `owner/name` as GitHub spells it.
    pub repository: String,
}

/// `GitHubPullRequestSearchBatch`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestSearchBatch {
    pub items: Vec<GitHubPullRequestSearchItem>,
    /// Rows the search returned, counted before decoding.
    pub raw_count: usize,
    /// More rows than this slice asked for, which is truncation for every repository in it.
    pub has_next_page: bool,
}

/// `{ additions, deletions }`: a pull request's or a commit's line counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct GitHubLineStats {
    pub additions: i64,
    pub deletions: i64,
}

/// `GitHubPullRequestSummary`: the fields a linked thread keeps current.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestSummary {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub head_branch: String,
    pub base_branch: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub closed_at: Option<String>,
    pub merged_at: Option<String>,
    pub updated_at: String,
    pub author: Option<PullRequestActor>,
    pub additions: i64,
    pub deletions: i64,
    pub changed_files: i64,
    pub review_decision: Option<PullRequestReviewDecision>,
    pub checks_state: Option<PullRequestChecksState>,
    pub mergeability: PullRequestMergeability,
}

/// `GitHubViewerAccess`: what the signed-in account may do here. `can_write` is about the
/// repository, the rest about this pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubViewerAccess {
    pub can_write: bool,
    /// The viewer's role reaches triage, the least that may label.
    pub can_triage: bool,
    /// GitHub's own `viewerCanUpdate`, true for the author as well as for anyone with write.
    pub can_update: bool,
    pub did_author: bool,
    /// `viewerCanUpdateBranch`, read with the base comparison; absent where it was not read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_update_branch: Option<bool>,
}

/// `GitHubRepositoryAccess`: repository settings returned alongside the viewer permissions.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubRepositoryAccess {
    pub merge_capabilities: PullRequestMergeCapabilities,
    pub can_write: bool,
}

/// `GitHubViewerAccess & GitHubRepositoryAccess`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubViewerRepositoryAccess {
    #[serde(flatten)]
    pub viewer: GitHubViewerAccess,
    pub merge_capabilities: PullRequestMergeCapabilities,
}

impl GitHubViewerRepositoryAccess {
    /// The `GitHubRepositoryAccess` half.
    pub fn repository_access(&self) -> GitHubRepositoryAccess {
        GitHubRepositoryAccess {
            merge_capabilities: self.merge_capabilities.clone(),
            can_write: self.viewer.can_write,
        }
    }
}

/// `GitHubBaseComparison`: how far the branch trails its base.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubBaseComparison {
    /// `None` where the host could not compare, which the page reads as "unknown".
    pub behind_by: Option<JsNumber>,
    pub viewer_can_update: bool,
}

/// `GitHubPullRequestCore`: the detail, the viewer's standing and the base comparison of one read.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestCore {
    #[serde(flatten)]
    pub detail: GitHubPullRequestDetail,
    pub viewer_access: GitHubViewerRepositoryAccess,
    pub comparison: Option<GitHubBaseComparison>,
    pub checks_truncated: bool,
}

/// `Omit<PullRequestPreview, "projectId" | "repository">`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestPreview {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: PullRequestState,
    pub is_draft: bool,
    pub created_at: String,
    pub author: Option<PullRequestActor>,
}

/// `{ canUpdate, didAuthor }`: the viewer's standing on one pull request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestViewerFields {
    pub can_update: bool,
    pub did_author: bool,
}

/// `GitHubReviewThreadComments`: the whole conversation once every page has been walked.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubReviewThreadComments {
    pub comments: Vec<PullRequestComment>,
    /// Dismissal reasons by the dismissed review's node id, read off the timeline.
    pub dismissals_by_review_id: BTreeMap<String, String>,
    /// Whole conversations, kept anchored so the diff can pin them to their line.
    pub review_threads: Vec<PullRequestReviewThread>,
    /// The host's own count of the conversation, which a bounded read can fall short of.
    pub comment_count: i64,
    pub truncated: bool,
    /// The pull request's own reactions, which sit on its description.
    pub reactions: Vec<PullRequestReaction>,
    /// Reactions by node id, for the comments and reviews the `gh` JSON read carries none on.
    pub reactions_by_id: BTreeMap<String, Vec<PullRequestReaction>>,
    /// Everyone on the review: those still asked and those who have already answered.
    pub reviewers: Vec<PullRequestActor>,
    /// Avatars by login, for the actors `gh pr view --json` reports without one.
    pub avatars_by_login: BTreeMap<String, String>,
    pub bot_logins: BTreeSet<String>,
    /// Per-commit line counts carried by the same bounded pull-request query.
    pub commit_stats: BTreeMap<String, GitHubLineStats>,
    /// The newest hundred commits, oldest to newest; empty where the read never happened.
    pub commits: Vec<PullRequestCommit>,
    pub viewer: GitHubPullRequestViewerFields,
}

/// `GitHubReviewThreadEntry`: one thread as this page found it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubReviewThreadEntry {
    pub thread: PullRequestReviewThread,
    /// How many comments GitHub says the thread holds, read or not.
    pub comment_count: i64,
    /// Where the rest of this thread's comments carry on from, or `None` once it is whole.
    pub next_comment_cursor: Option<String>,
}

/// `GitHubReviewThreadPage`: one page of review threads and everything riding with it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubReviewThreadPage {
    pub threads: Vec<GitHubReviewThreadEntry>,
    /// Where the next page of threads starts, or `None` once the host has handed them all over.
    pub next_cursor: Option<String>,
    /// The pull request's own reactions, which sit on its description.
    pub reactions: Vec<PullRequestReaction>,
    /// Reactions by node id; only ids with a reaction are here.
    pub reactions_by_id: BTreeMap<String, Vec<PullRequestReaction>>,
    pub reviewers: Vec<PullRequestActor>,
    pub avatars_by_login: BTreeMap<String, String>,
    pub bot_logins: BTreeSet<String>,
    pub commit_stats: BTreeMap<String, GitHubLineStats>,
    pub commits: Vec<PullRequestCommit>,
    pub viewer: GitHubPullRequestViewerFields,
    /// Dismissal reasons by the dismissed review's node id, which the review never carries.
    pub dismissals_by_review_id: BTreeMap<String, String>,
    /// Where the rest of the dismissal events start, or `None` once this page carried them all.
    pub next_dismissal_cursor: Option<String>,
}

/// What `decodeReviewDismissalsJson` returns: one further page of dismissal events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubReviewDismissalsPage {
    pub dismissals_by_review_id: BTreeMap<String, String>,
    pub next_cursor: Option<String>,
}

/// What `decodeReviewThreadCommentsJson` returns: the rest of one thread's comments.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubReviewThreadCommentsPage {
    /// The thread named hangs off the pull request named, as the host says.
    pub belongs_to_pull_request: bool,
    pub comments: Vec<PullRequestThreadComment>,
    pub next_cursor: Option<String>,
}

/// `GitHubPullRequestFilesPatch`: one page of the files API as a unified patch.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestFilesPatch {
    pub patch: String,
    /// At least one file's hunks were withheld by GitHub, so they are missing from the patch.
    pub truncated: bool,
    /// Files GitHub returned, counted before decoding, so the caller can page.
    pub raw_count: usize,
    /// GitHub's own counts for the files whose hunks it withheld.
    pub omitted_file_stats: Vec<PullRequestOmittedFileStat>,
}

/// `GitHubPullRequestFilesViewedPage`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestFilesViewedPage {
    pub files: Vec<PullRequestFileViewed>,
    /// Where the next page carries on, or `None` once the host has no more to give.
    pub next_cursor: Option<String>,
}

/// `GitHubPullRequestStackLayer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubPullRequestStackLayer {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_draft: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub number: i64,
    pub head_branch: String,
    pub state: PullRequestState,
}

/// `GitHubPullRequestStack`: a host-native stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GitHubPullRequestStack {
    pub id: String,
    pub number: i64,
    pub url: String,
    pub base: String,
    /// Bottom to top, which is the order GitHub lists them in.
    pub layers: Vec<GitHubPullRequestStackLayer>,
}
