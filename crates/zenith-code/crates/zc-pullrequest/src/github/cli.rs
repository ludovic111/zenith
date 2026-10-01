//! `pullRequest/GitHubPullRequestCli.ts`: every `gh` invocation of the pull request feature.
//!
//! - Everything goes through the shared [`GitHubCli::execute`] (so `gh` concurrency, the
//!   per-host rate-limit pauses and pinned credentials apply); GraphQL reads first reserve their
//!   cost in the shared [`GitHubGraphQlBudget`] (10% held back for interactive reads) and feed the
//!   answer's `rateLimit` back to it.
//! - Variables composed here travel as `-f`/`-F` flags; anything a reader typed (a search, a
//!   body) travels over stdin, because argv is visible in process listings and echoed back in
//!   process-runner failures.
//! - Routing identity: `gh auth token` + `gh api user`, verified once per credential digest
//!   (cached 10 min, single-flight per credential); [`GitHubPullRequestCli::verified_credential`]
//!   pins every later call to the verified token (the TS `withVerifiedCredential`).
//! - Linked-thread summaries asked for within 10 ms of each other, on one host under one
//!   credential, share aliased GraphQL reads of 25 (the TS `RequestResolver`).
//! - Pull request node ids are remembered (128, least recently used).
//!
//! [`GitHubPullRequestCliApi`] is the service interface (the TS `GitHubPullRequestCli` tag), so
//! the provider can be tested against a mock the way the TS tests use `Layer.mock`.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt, TryStreamExt};
use regex::Regex;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::oneshot;
use zc_contracts::{
    PullRequestAction, PullRequestCommentUpdateInputKind, PullRequestDiffFileContentsInputChangeType, PullRequestFileViewed, PullRequestInvolvement,
    PullRequestLabelCandidateList, PullRequestListFilters, PullRequestListFiltersChecks, PullRequestListFiltersDraft, PullRequestListFiltersReview,
    PullRequestListState, PullRequestMergeMethod, PullRequestReviewDecision, PullRequestReviewerCandidateList, PullRequestThreadCommentsResult,
    PullRequestUpdateMethod,
};
use zc_core::vcs_process::VcsProcessOutput;
use zc_sourcecontrol::errors::{error_defect, Cause, CauseError};
use zc_sourcecontrol::github::cli::{current_pinned_github_credential, github_reserve_allowed};
use zc_sourcecontrol::github::{with_pinned_github_credential, GitHubCli, GitHubCliError, GitHubCliErrorKind, GitHubExecuteInput, PinnedGitHubCredential};
use zc_sourcecontrol::graphql_budget::GitHubGraphQlBudget;
use zc_sourcecontrol::rate_limit::{current_credential_scope, with_credential_scope, SourceControlRateLimitPausedError};
use zc_sourcecontrol::util::{encode_uri_component, js_trim, positive_int, trimmed_non_empty, SharedClock};

use crate::contract::resolve_pull_request_author_filter;
use crate::github::json::{self as gh_json};
use crate::github::stack_actions::{run_github_stack_action, GitHubStackActionError, GitHubStackActionInput, StackActionFailure};
use crate::provider::{
    ChangeRequestRef, CommentInput, CredentialScope, DiffFileContentsInput, GetDiffInput, ListChangeRequestStatsInput, ListChangeRequestsAcrossInput,
    ListChangeRequestsInput, ProviderChangeRequestPreview, ProviderChangeRequestSummary, ProviderDiffFileContents, ProviderDiffSlice, ProviderFilesViewed,
    ReplyToThreadInput, ReviewThreadCommentsInput, RoutingIdentity, RunActionInput, SetFilesViewedInput, SetLabelsInput, SetReactionInput,
    SetReviewerRequestInput, SetThreadResolutionInput, SubmitReviewInput, UpdateChangeRequestInput, UpdateCommentInput, VerifiedCredential, VerifiedIdentity,
};

/// A large pull request can produce a multi-megabyte patch; past this it is truncated.
const DIFF_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const DIFF_TIMEOUT_MS: u64 = 60_000;
/// Pierre expansion is for source files, not blobs large enough to stall a review surface.
const DIFF_FILE_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// A search-free fallback may scan older rows for local filters, but never the whole repository.
const PULL_REQUEST_FALLBACK_MAX_ROWS: i64 = 1_000;
/// What the files API serves at most in one response, which is what one slice is made of.
const DIFF_FILES_PAGE_SIZE: i64 = 100;
/// How many hundred-file pages of viewed state one read walks before saying it was cut short.
const FILES_VIEWED_MAX_PAGES: usize = 5;
/// How many pull requests' node ids are remembered at once (least recently used goes first).
pub const NODE_ID_CACHE_CAPACITY: usize = 128;
/// Pages of review threads (a hundred each) to follow before the conversation is truncated.
const REVIEW_THREAD_PAGES: usize = 10;
/// Aliased lookups per request, and requests at once.
const STAT_ALIASES_PER_REQUEST: usize = 25;
const STAT_REQUEST_CONCURRENCY: usize = 4;
/// How long a summary read waits for company before its batch goes out.
const SUMMARY_BATCH_WINDOW: Duration = Duration::from_millis(10);
/// How long a verified routing identity is trusted.
const ROUTING_IDENTITY_TTL_MS: i64 = 10 * 60_000;
const ROUTING_IDENTITY_CAPACITY: usize = 128;
const WORKFLOW_APPROVAL_LIMIT: i64 = 1_000;

/// `parseRepositorySelector`: `owner/repo` split for GraphQL, which takes the two separately.
/// The host travels alongside the identity, never inside it.
pub fn parse_repository_selector(value: &str) -> (String, String) {
    let parts: Vec<&str> = js_trim(value).split('/').filter(|part| !part.is_empty()).collect();
    let name = parts.last().copied().unwrap_or_default().to_owned();
    let owner = if parts.len() >= 2 { parts[parts.len() - 2].to_owned() } else { String::new() };
    (owner, name)
}

/// `diffCursorPage`: the page a diff cursor names, `None` for anything this walk cannot have
/// issued (the cursor goes straight into a request path).
fn diff_cursor_page(cursor: &str) -> Option<i64> {
    static PAGE: OnceLock<Regex> = OnceLock::new();
    PAGE.get_or_init(|| Regex::new(r"^[1-9][0-9]{0,6}$").expect("valid regex"))
        .is_match(cursor)
        .then(|| cursor.parse().ok())
        .flatten()
}

/// `isCommitSha`: hexadecimal, from the shortest abbreviation a host prints up to a whole sha.
fn is_commit_sha(value: &str) -> bool {
    static SHA: OnceLock<Regex> = OnceLock::new();
    SHA.get_or_init(|| Regex::new(r"(?i)^[0-9a-f]{7,64}$").expect("valid regex")).is_match(value)
}

/// `searchPhrase`: the reader's own words as one literal phrase of a search query. The two
/// characters that could end the phrase early are escaped first.
fn search_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('\\', "\\\\").replace('"', "\\\""))
}

/// `REVIEW_QUALIFIERS`: GitHub's own spelling of a review state.
fn review_qualifier(review: PullRequestListFiltersReview) -> &'static str {
    match review {
        PullRequestListFiltersReview::Approved => "approved",
        PullRequestListFiltersReview::ChangesRequested => "changes_requested",
        PullRequestListFiltersReview::ReviewRequired => "required",
        PullRequestListFiltersReview::None => "none",
    }
}

/// `qualifierValue`: a typed value, quoted, with the one character that could end it dropped.
fn qualifier_value(value: &str) -> String {
    format!("\"{}\"", js_trim(&value.replace('"', "")))
}

/// `filterQualifiers`: the extra narrowings as search qualifiers.
fn filter_qualifiers(filters: Option<&PullRequestListFilters>, viewer: &str) -> Vec<String> {
    let Some(filters) = filters else {
        return Vec::new();
    };
    let mut qualifiers = Vec::new();
    // One qualifier per group, its names joined by commas: GitHub's own OR.
    for group in filters.labels.iter().flatten() {
        if !group.is_empty() {
            qualifiers.push(format!(
                "label:{}",
                group.iter().map(|label| qualifier_value(label)).collect::<Vec<_>>().join(",")
            ));
        }
    }
    for label in filters.excluded_labels.iter().flatten() {
        qualifiers.push(format!("-label:{}", qualifier_value(label)));
    }
    if let Some(author) = &filters.author {
        qualifiers.push(format!("author:{}", qualifier_value(&resolve_pull_request_author_filter(author, Some(viewer)))));
    }
    if let Some(draft) = filters.draft {
        qualifiers.push(format!("draft:{}", draft == PullRequestListFiltersDraft::Only));
    }
    if let Some(review) = filters.review {
        qualifiers.push(format!("review:{}", review_qualifier(review)));
    }
    if let Some(checks) = filters.checks {
        qualifiers.push(format!(
            "status:{}",
            if checks == PullRequestListFiltersChecks::Passing {
                "success"
            } else {
                "failure"
            }
        ));
    }
    qualifiers
}

/// `matchesFilters`: the same narrowings over a row that already arrived, for the search-free
/// fallback. `checks` is judged by equality, so a row with no or pending checks fails both
/// values, the way search would not surface it either.
fn matches_filters(item: &gh_json::GitHubPullRequestListItem, filters: Option<&PullRequestListFilters>, viewer: &str) -> bool {
    let Some(filters) = filters else {
        return true;
    };
    let labels: HashSet<String> = item.labels.iter().map(|label| js_trim(&label.name).to_lowercase()).collect();
    let holds = |label: &String| labels.contains(&js_trim(label).to_lowercase());
    let review_matches = match filters.review {
        None => true,
        Some(PullRequestListFiltersReview::None) => item.review_decision.is_none(),
        Some(review) => item.review_decision.map(PullRequestReviewDecision::as_str) == Some(review.as_str()),
    };
    (filters.draft.is_none() || Some(item.is_draft) == filters.draft.map(|draft| draft == PullRequestListFiltersDraft::Only))
        && review_matches
        && filters
            .checks
            .is_none_or(|checks| item.checks_state.map(|state| state.as_str()) == Some(checks.as_str()))
        && filters.labels.as_ref().is_none_or(|groups| groups.iter().all(|group| group.iter().any(holds)))
        && filters.excluded_labels.as_ref().is_none_or(|labels| !labels.iter().any(holds))
        && filters.author.as_ref().is_none_or(|author| {
            item.author.as_ref().map(|actor| actor.login.to_lowercase()) == Some(resolve_pull_request_author_filter(author, Some(viewer)).to_lowercase())
        })
}

/// `involvementArgs`: the tab, the involvement, the reader's words and the cursor as `gh pr
/// list` flags. `sorted` is false on the search-free fallback, which uses no search at all.
fn involvement_args(input: &ListChangeRequestsInput, sorted: bool) -> Vec<String> {
    let query = input.query.as_deref().map(js_trim).unwrap_or_default();
    let mut search_terms: Vec<String> = Vec::new();
    if sorted {
        if input.involvement == PullRequestInvolvement::Reviewing {
            search_terms.push(format!("review-requested:{}", input.viewer));
        }
        // `--state closed` includes merged pull requests, so the Closed tab excludes them here.
        if input.state == PullRequestListState::Closed {
            search_terms.push("is:unmerged".into());
        }
        if !query.is_empty() {
            search_terms.push(search_phrase(query));
        }
        // Inclusive: rows sharing the instant are ordinary, and the caller drops repeats.
        if let Some(cursor) = &input.cursor {
            search_terms.push(format!("updated:<={}", cursor.updated_before));
        }
        search_terms.extend(filter_qualifiers(input.filters.as_ref(), &input.viewer));
        // The order the page reads its rows in, and the only one a continuation can carry on from.
        search_terms.push("sort:updated-desc".into());
    }
    let mut args = Vec::new();
    if input.involvement == PullRequestInvolvement::Authored {
        args.push("--author".to_owned());
        args.push(input.viewer.clone());
    }
    if !search_terms.is_empty() {
        args.push("--search".into());
        args.push(search_terms.join(" "));
    }
    args
}

/// `matchesUnsortedListing`: the search-free fallback is wider than the request, so its rows are
/// narrowed locally. Team review requests survive the reviewing tab: team membership cannot be
/// resolved here, and dropping them would hide legitimate work.
fn matches_unsorted_listing(item: &gh_json::GitHubPullRequestListItem, input: &ListChangeRequestsInput) -> bool {
    let matches_state = input.state == PullRequestListState::All || item.state.as_str() == input.state.as_str();
    let viewer = input.viewer.to_lowercase();
    let matches_involvement = match input.involvement {
        PullRequestInvolvement::All => true,
        PullRequestInvolvement::Authored => item.author.as_ref().map(|actor| actor.login.to_lowercase()) == Some(viewer),
        PullRequestInvolvement::Reviewing => item.has_team_review_request || item.review_request_logins.iter().any(|login| login.to_lowercase() == viewer),
    };
    matches_state && matches_involvement && matches_filters(item, input.filters.as_ref(), &input.viewer)
}

/// `SEARCH_REPOSITORY`: what a repository may hold before it goes into a search as itself.
fn is_search_repository(repository: &str) -> bool {
    static REPOSITORY: OnceLock<Regex> = OnceLock::new();
    REPOSITORY
        .get_or_init(|| Regex::new(r"^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$").expect("valid regex"))
        .is_match(repository)
}

/// `searchQuery`: the same listing as one search across several repositories. Every narrowing
/// `involvement_args` hands `gh pr list` as a flag is a qualifier here. `None` where a repository
/// is not `owner/name`: an unaddressable one refuses the whole read rather than being escaped.
fn search_query(input: &ListChangeRequestsAcrossInput) -> Option<String> {
    if input.repositories.is_empty() {
        return None;
    }
    let repositories: Vec<&str> = input.repositories.iter().map(|repository| js_trim(repository)).collect();
    if !repositories.iter().all(|repository| is_search_repository(repository)) {
        return None;
    }
    let query = input.query.as_deref().map(js_trim).unwrap_or_default();
    let mut terms: Vec<String> = vec!["is:pr".into()];
    match input.state {
        // "all" is every state, which `is:pr` already is.
        PullRequestListState::All => {}
        PullRequestListState::Open => terms.push("is:open".into()),
        PullRequestListState::Closed => {
            terms.push("is:closed".into());
            terms.push("is:unmerged".into());
        }
        PullRequestListState::Merged => terms.push("is:merged".into()),
    }
    match input.involvement {
        PullRequestInvolvement::All => {}
        PullRequestInvolvement::Authored => terms.push(format!("author:{}", input.viewer)),
        PullRequestInvolvement::Reviewing => terms.push(format!("review-requested:{}", input.viewer)),
    }
    if !query.is_empty() {
        terms.push(search_phrase(query));
    }
    if let Some(cursor) = &input.cursor {
        terms.push(format!("updated:<={}", cursor.updated_before));
    }
    terms.extend(filter_qualifiers(input.filters.as_ref(), &input.viewer));
    terms.push("sort:updated-desc".into());
    terms.extend(repositories.iter().map(|repository| format!("repo:{repository}")));
    Some(terms.join(" "))
}

/// `cursorVariable`: gh sends a JSON null only through a typed field, and an empty cursor is
/// refused rather than read as "from the start".
fn cursor_variable(cursor: Option<&str>) -> (&'static str, String) {
    match cursor {
        None => ("-F", "cursor=null".into()),
        Some(cursor) => ("-f", format!("cursor={cursor}")),
    }
}

/// `actionArgs`: the `gh pr` subcommand and flags of a plain action.
fn action_args(action: PullRequestAction, merge_method: Option<PullRequestMergeMethod>, update_method: Option<PullRequestUpdateMethod>) -> Vec<String> {
    let merge = || format!("--{}", merge_method.unwrap_or(PullRequestMergeMethod::Merge).as_str());
    match action {
        PullRequestAction::Merge => vec!["merge".into(), merge()],
        // `--auto` arms the same command and still needs the strategy GitHub stores with it.
        PullRequestAction::EnableAutoMerge => vec!["merge".into(), "--auto".into(), merge()],
        PullRequestAction::DisableAutoMerge => vec!["merge".into(), "--disable-auto".into()],
        // `gh` updates with a merge commit unless asked to rebase, GitHub's own default.
        PullRequestAction::UpdateBranch => {
            let mut args = vec!["update-branch".to_owned()];
            if update_method == Some(PullRequestUpdateMethod::Rebase) {
                args.push("--rebase".into());
            }
            args
        }
        PullRequestAction::Ready => vec!["ready".into()],
        PullRequestAction::Draft => vec!["ready".into(), "--undo".into()],
        PullRequestAction::Close => vec!["close".into()],
        PullRequestAction::Reopen => vec!["reopen".into()],
        // Handled before this is reached (a GraphQL mutation, and run discovery).
        PullRequestAction::Revert | PullRequestAction::ApproveWorkflows => unreachable!("{} is not a plain gh pr action", action.as_str()),
    }
}

/// `repositoryArgs`: `--repo host/owner/repo`, so an Enterprise repository resolves on its own host.
fn repository_args(host: &str, repository: &str) -> [String; 2] {
    ["--repo".into(), format!("{host}/{repository}")]
}

/// `["-f", "owner=…"], ["-f", "name=…"], ["-F", "number=…"]`.
fn pull_request_variables(owner: &str, name: &str, number: i64) -> Vec<(&'static str, String)> {
    vec![
        ("-f", format!("owner={owner}")),
        ("-f", format!("name={name}")),
        ("-F", format!("number={number}")),
    ]
}

/// `String.prototype.trimEnd`.
fn js_trim_end(text: &str) -> &str {
    text.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}
/// `GitHubWorkflowApprovalRefusedError["reason"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowApprovalRefusal {
    HeadListTruncated,
    HeadNotUnique,
    RunListTruncated,
}

impl WorkflowApprovalRefusal {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::HeadListTruncated => "head-list-truncated",
            Self::HeadNotUnique => "head-not-unique",
            Self::RunListTruncated => "run-list-truncated",
        }
    }
}

/// `GitHubDiffFileContentsUnavailableError["reason"]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffFileUnavailableReason {
    Oversized,
    Binary,
}

impl DiffFileUnavailableReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Oversized => "oversized",
            Self::Binary => "binary",
        }
    }
}

/// `GitHubPullRequestCliError`: everything a PR read or write through `gh` can fail with.
#[derive(Debug, Clone)]
pub enum GitHubPullRequestCliError {
    /// `GitHubStackActionError`.
    Stack(GitHubStackActionError),
    /// `GitHubCliError`.
    Cli(GitHubCliError),
    /// `GitHubPullRequestReadError`: names the read that produced unusable output.
    Read { cwd: String, operation: String, cause: Cause },
    /// `GitHubDiffCursorError`: a cursor this walk never handed out.
    DiffCursor { cwd: String },
    /// `GitHubDiffCommitError`: a commit that is not a sha.
    DiffCommit { cwd: String },
    /// `GitHubDiffRevisionsUnavailableError`.
    DiffRevisionsUnavailable { cwd: String, number: i64, commit: Option<String> },
    /// `GitHubDiffFileContentsUnavailableError`.
    DiffFileContentsUnavailable {
        cwd: String,
        path: String,
        reason: DiffFileUnavailableReason,
    },
    /// `GitHubRepositorySelectorError`: a repository GitHub cannot address.
    RepositorySelector { cwd: String, operation: String },
    /// `GitHubSubjectScopeError`: a subject this pull request never handed out.
    SubjectScope { cwd: String, operation: String },
    /// `GitHubWorkflowApprovalRefusedError`.
    WorkflowApprovalRefused {
        cwd: String,
        number: i64,
        reason: WorkflowApprovalRefusal,
        observed_count: i64,
        limit: i64,
    },
    /// `GitHubWorkflowApprovalHeadUnavailableError`.
    WorkflowApprovalHeadUnavailable { cwd: String, number: i64 },
    /// `GitHubWorkflowApprovalHeadChangedError`.
    WorkflowApprovalHeadChanged { cwd: String, number: i64 },
    /// `SourceControlRateLimitPausedError` (the GraphQL budget refused the read).
    RateLimitPaused(SourceControlRateLimitPausedError),
    /// `GitHubViewerLoginUnavailableError`.
    ViewerLoginUnavailable { cwd: String },
    /// `GitHubPullRequestUpdatedAtUnavailableError`.
    UpdatedAtUnavailable { cwd: String, repository: String, number: i64 },
}

impl GitHubPullRequestCliError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Stack(error) => error.tag(),
            Self::Cli(error) => error.tag(),
            Self::Read { .. } => "GitHubPullRequestReadError",
            Self::DiffCursor { .. } => "GitHubDiffCursorError",
            Self::DiffCommit { .. } => "GitHubDiffCommitError",
            Self::DiffRevisionsUnavailable { .. } => "GitHubDiffRevisionsUnavailableError",
            Self::DiffFileContentsUnavailable { .. } => "GitHubDiffFileContentsUnavailableError",
            Self::RepositorySelector { .. } => "GitHubRepositorySelectorError",
            Self::SubjectScope { .. } => "GitHubSubjectScopeError",
            Self::WorkflowApprovalRefused { .. } => "GitHubWorkflowApprovalRefusedError",
            Self::WorkflowApprovalHeadUnavailable { .. } => "GitHubWorkflowApprovalHeadUnavailableError",
            Self::WorkflowApprovalHeadChanged { .. } => "GitHubWorkflowApprovalHeadChangedError",
            Self::RateLimitPaused(_) => "SourceControlRateLimitPausedError",
            Self::ViewerLoginUnavailable { .. } => "GitHubViewerLoginUnavailableError",
            Self::UpdatedAtUnavailable { .. } => "GitHubPullRequestUpdatedAtUnavailableError",
        }
    }

    /// The `detail` getter every member has.
    pub fn detail(&self) -> String {
        match self {
            Self::Stack(error) => error.detail(),
            Self::Cli(error) => error.detail().to_owned(),
            Self::Read { operation, .. } => format!("GitHub CLI returned an unreadable {operation} response."),
            Self::DiffCursor { .. } => "The diff cursor was not one this pull request handed out.".into(),
            Self::DiffCommit { .. } => "The named commit was not a commit sha.".into(),
            Self::DiffRevisionsUnavailable { number, commit, .. } => match commit {
                None => format!("Pull request #{number} reported no usable base and head revisions."),
                Some(commit) => format!("Commit {commit} reported no usable revisions for this file."),
            },
            Self::DiffFileContentsUnavailable { path, reason, .. } => match reason {
                DiffFileUnavailableReason::Oversized => format!("The diff file '{path}' exceeds the 1 MB expansion limit."),
                DiffFileUnavailableReason::Binary => format!("The diff file '{path}' is binary."),
            },
            Self::RepositorySelector { .. } => "A repository was named that GitHub cannot address.".into(),
            Self::SubjectScope { .. } => "The named subject did not belong to the named pull request.".into(),
            Self::WorkflowApprovalRefused {
                number,
                reason,
                observed_count,
                limit,
                ..
            } => match reason {
                WorkflowApprovalRefusal::HeadListTruncated => format!("GitHub returned more than {limit} pull requests for this head branch."),
                WorkflowApprovalRefusal::HeadNotUnique => {
                    format!("The head revision matched {observed_count} pull requests instead of uniquely matching #{number}.")
                }
                WorkflowApprovalRefusal::RunListTruncated => format!("GitHub returned more than {limit} workflow runs awaiting approval."),
            },
            Self::WorkflowApprovalHeadUnavailable { number, .. } => format!("GitHub did not report a complete head revision for #{number}."),
            Self::WorkflowApprovalHeadChanged { number, .. } => {
                format!("The head revision of #{number} changed before its workflows could be approved.")
            }
            Self::RateLimitPaused(error) => error.detail(),
            Self::ViewerLoginUnavailable { .. } => "GitHub CLI returned no login for the authenticated account.".into(),
            Self::UpdatedAtUnavailable { repository, number, .. } => format!("Pull request {repository}#{number} reported no update time."),
        }
    }

    /// The `message` getter.
    pub fn message(&self) -> String {
        match self {
            Self::Stack(error) => error.message(),
            Self::Cli(error) => error.message(),
            Self::Read { operation, .. } | Self::RepositorySelector { operation, .. } | Self::SubjectScope { operation, .. } => {
                format!("GitHub CLI failed in {operation}: {}", self.detail())
            }
            Self::DiffCursor { .. } | Self::DiffCommit { .. } => format!("GitHub CLI failed in getPullRequestDiff: {}", self.detail()),
            Self::DiffRevisionsUnavailable { .. } | Self::DiffFileContentsUnavailable { .. } => {
                format!("GitHub CLI failed in getPullRequestDiffFileContents: {}", self.detail())
            }
            Self::WorkflowApprovalRefused { .. } => format!("GitHub CLI refused listWorkflowRunsRequiringApproval: {}", self.detail()),
            Self::WorkflowApprovalHeadUnavailable { .. } | Self::WorkflowApprovalHeadChanged { .. } => {
                format!("GitHub CLI refused approve-workflows: {}", self.detail())
            }
            Self::RateLimitPaused(error) => error.message(),
            Self::ViewerLoginUnavailable { .. } => format!("GitHub CLI failed in getViewerLogin: {}", self.detail()),
            Self::UpdatedAtUnavailable { .. } => format!("GitHub CLI failed in getPullRequestSummary: {}", self.detail()),
        }
    }

    /// The error's own `cause`, where it keeps one.
    pub fn cause(&self) -> Option<Cause> {
        match self {
            Self::Stack(error) => error.cause.clone(),
            Self::Cli(error) => Some(error.cause.clone()),
            Self::Read { cause, .. } => Some(cause.clone()),
            _ => None,
        }
    }

    fn read(cwd: &str, operation: &str, cause: Cause) -> Self {
        Self::Read {
            cwd: cwd.to_owned(),
            operation: operation.to_owned(),
            cause,
        }
    }
}

impl From<GitHubCliError> for GitHubPullRequestCliError {
    fn from(error: GitHubCliError) -> Self {
        Self::Cli(error)
    }
}

impl From<StackActionFailure> for GitHubPullRequestCliError {
    fn from(failure: StackActionFailure) -> Self {
        match failure {
            StackActionFailure::Stack(error) => Self::Stack(error),
            StackActionFailure::Cli(error) => Self::Cli(error),
        }
    }
}

impl std::fmt::Display for GitHubPullRequestCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GitHubPullRequestCliError {}

impl CauseError for GitHubPullRequestCliError {
    fn defect(&self) -> Value {
        match self {
            Self::Stack(error) => error.defect(),
            Self::Cli(error) => error.defect(),
            Self::RateLimitPaused(error) => error.defect(),
            _ => error_defect(self.tag(), self.message(), self.cause().as_ref().map(Cause::defect)),
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// What a `gh` read of the pull request feature fails with.
pub type CliResult<T> = Result<T, GitHubPullRequestCliError>;

/// `GitHubPullRequestListBatch` (of the CLI): one repository's slice.
#[derive(Debug, Clone, PartialEq)]
pub struct GitHubPullRequestListBatch {
    pub items: Vec<gh_json::GitHubPullRequestListItem>,
    pub truncated: bool,
    /// False for a page GitHub would not search, which came back in `gh`'s own order instead.
    pub continues: bool,
}

/// `GitHubPullRequestSearchBatch` (of the CLI): rows across every repository asked for, newest
/// update first, each naming its own.
#[derive(Debug, Clone, PartialEq)]
pub struct GitHubPullRequestSearchBatch {
    pub items: Vec<gh_json::GitHubPullRequestSearchItem>,
    pub truncated: bool,
}

/// `GitHubPullRequestStat`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHubPullRequestStat {
    pub repository: String,
    pub number: i64,
    pub additions: i64,
    pub deletions: i64,
}

/// `listWorkflowRunsRequiringApproval` input (`isCrossRepository: true` is implied).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowApprovalInput {
    pub change_request: ChangeRequestRef,
    pub head_sha: String,
    pub head_branch: String,
    pub head_repository_owner: String,
}

/// `getPullRequestBaseComparison` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseComparisonInput {
    pub change_request: ChangeRequestRef,
    /// Qualified `owner:branch`, which is the only form a fork's head resolves under.
    pub head_ref: String,
    /// Manual action checks may use the quota held back from automatic reads.
    pub allow_reserve: Option<bool>,
}

/// `getViewerAccess` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewerAccessInput {
    pub change_request: ChangeRequestRef,
    pub allow_reserve: Option<bool>,
}

/// `listActorAvatars` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorAvatarsInput {
    pub cwd: String,
    pub repository: String,
    pub host: String,
    pub ids: Vec<String>,
}

/// `GitHubPullRequestCli`: the service interface the provider consumes. Implemented by
/// [`GitHubPullRequestCli`]; tests may mock it.
#[async_trait]
pub trait GitHubPullRequestCliApi: Send + Sync {
    /// `withVerifiedCredential`: who the host credential is, and a scope pinning every call made
    /// inside it to that credential (and accounting its quota to it).
    async fn verified_credential(&self, cwd: &str, host: &str) -> CliResult<VerifiedCredential>;
    async fn get_routing_identity(&self, cwd: &str, host: &str) -> CliResult<RoutingIdentity>;
    async fn get_viewer_login(&self, cwd: &str, host: &str) -> CliResult<String>;
    async fn list_pull_requests(&self, input: ListChangeRequestsInput) -> CliResult<GitHubPullRequestListBatch>;
    /// The same listing for a whole host in one search; `limit` is the size of the slice across
    /// every repository.
    async fn search_pull_requests(&self, input: ListChangeRequestsAcrossInput) -> CliResult<GitHubPullRequestSearchBatch>;
    /// The line counts the search leaves out, for rows already on the page.
    async fn list_pull_request_stats(&self, input: ListChangeRequestStatsInput) -> CliResult<Vec<GitHubPullRequestStat>>;
    async fn get_pull_request_summary(&self, input: ChangeRequestRef) -> CliResult<ProviderChangeRequestSummary>;
    async fn get_pull_request_detail(&self, input: ChangeRequestRef) -> CliResult<gh_json::GitHubPullRequestCore>;
    async fn get_pull_request_preview(&self, input: ChangeRequestRef) -> CliResult<ProviderChangeRequestPreview>;
    async fn list_workflow_runs_requiring_approval(&self, input: WorkflowApprovalInput) -> CliResult<Vec<gh_json::GitHubWorkflowRunApproval>>;
    /// The host-native stack this pull request is in, or `None` (also for a host that refuses
    /// the stacks preview altogether).
    async fn get_pull_request_stack(&self, input: ChangeRequestRef, include_details: bool) -> CliResult<Option<gh_json::GitHubPullRequestStack>>;
    /// How far the branch trails its base, and whether this viewer may update it.
    async fn get_pull_request_base_comparison(&self, input: BaseComparisonInput) -> CliResult<gh_json::GitHubBaseComparison>;
    async fn get_pull_request_activity(&self, input: ChangeRequestRef) -> CliResult<gh_json::GitHubPullRequestActivity>;
    async fn get_pull_request_diff(&self, input: GetDiffInput) -> CliResult<ProviderDiffSlice>;
    async fn get_pull_request_diff_file_contents(&self, input: DiffFileContentsInput) -> CliResult<ProviderDiffFileContents>;
    /// Which files the signed-in account has cleared, and which were pushed to since.
    async fn get_pull_request_files_viewed(&self, input: ChangeRequestRef) -> CliResult<ProviderFilesViewed>;
    /// Clears files, or puts them back, as one aliased mutation.
    async fn set_pull_request_files_viewed(&self, input: SetFilesViewedInput) -> CliResult<()>;
    async fn list_review_thread_comments(&self, input: ChangeRequestRef) -> CliResult<gh_json::GitHubReviewThreadComments>;
    /// One request for a listing's authors, since no `gh` JSON field reports an avatar.
    async fn list_actor_avatars(&self, input: ActorAvatarsInput) -> CliResult<BTreeMap<String, String>>;
    async fn get_review_thread_comments(&self, input: ReviewThreadCommentsInput) -> CliResult<PullRequestThreadCommentsResult>;
    /// The viewer's standing on its own, for deciding a write without reading the whole detail.
    async fn get_viewer_access(&self, input: ViewerAccessInput) -> CliResult<gh_json::GitHubViewerRepositoryAccess>;
    async fn list_reviewer_candidates(&self, input: ChangeRequestRef) -> CliResult<PullRequestReviewerCandidateList>;
    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> CliResult<()>;
    async fn list_label_candidates(&self, input: ChangeRequestRef) -> CliResult<PullRequestLabelCandidateList>;
    async fn set_labels(&self, input: SetLabelsInput) -> CliResult<()>;
    async fn run_pull_request_action(&self, input: RunActionInput) -> CliResult<()>;
    async fn comment_on_pull_request(&self, input: CommentInput) -> CliResult<()>;
    async fn submit_review(&self, input: SubmitReviewInput) -> CliResult<()>;
    async fn reply_to_review_thread(&self, input: ReplyToThreadInput) -> CliResult<()>;
    async fn set_review_thread_resolution(&self, input: SetThreadResolutionInput) -> CliResult<()>;
    /// Adds a reaction to a remark, or takes it back; `subject_id: None` is the pull request.
    async fn set_reaction(&self, input: SetReactionInput) -> CliResult<()>;
    /// Rewrites the pull request's own words, leaving whichever of the two was not given.
    async fn update_pull_request(&self, input: UpdateChangeRequestInput) -> CliResult<()>;
    /// Rewrites a remark, once it is confirmed to belong to this pull request.
    async fn update_comment(&self, input: UpdateCommentInput) -> CliResult<()>;
}

/// A credential captured and verified by `captureVerifiedCredential`.
#[derive(Clone)]
struct CapturedCredential {
    host: String,
    token: String,
    credential_fingerprint: String,
    account_id: String,
    viewer: String,
}

/// The scope [`GitHubPullRequestCli::verified_credential`] hands out: `CredentialScope` and
/// `PinnedGitHubCredential` around whatever runs inside it.
struct PinnedScope {
    credential: PinnedGitHubCredential,
}

impl CredentialScope for PinnedScope {
    fn run<'a>(&'a self, future: BoxFuture<'a, ()>) -> BoxFuture<'a, ()> {
        let credential = self.credential.clone();
        let scope = credential.credential_fingerprint.clone();
        Box::pin(with_credential_scope(scope, with_pinned_github_credential(credential, future)))
    }
}

/// The task-local context a request ran in, carried into work spawned on its behalf.
#[derive(Clone)]
struct AmbientContext {
    pinned: Option<PinnedGitHubCredential>,
    scope: String,
    allow_reserve: bool,
}

impl AmbientContext {
    fn capture() -> Self {
        Self {
            pinned: current_pinned_github_credential(),
            scope: current_credential_scope(),
            allow_reserve: github_reserve_allowed(),
        }
    }

    async fn run<F: Future>(self, future: F) -> F::Output {
        let scoped = with_credential_scope(self.scope, future);
        match (self.pinned, self.allow_reserve) {
            (Some(pinned), true) => with_pinned_github_credential(pinned, zc_sourcecontrol::github::with_github_reserve(scoped)).await,
            (Some(pinned), false) => with_pinned_github_credential(pinned, scoped).await,
            (None, true) => zc_sourcecontrol::github::with_github_reserve(scoped).await,
            (None, false) => scoped.await,
        }
    }
}

type SummaryReply = oneshot::Sender<CliResult<ProviderChangeRequestSummary>>;

struct SummaryEntry {
    request: ChangeRequestRef,
    reply: SummaryReply,
}

struct PendingSummaries {
    id: u64,
    context: AmbientContext,
    entries: Vec<SummaryEntry>,
}

#[derive(Default)]
struct SummaryBatches {
    next_id: u64,
    pending: HashMap<String, PendingSummaries>,
}

/// `routingIdentities`: verified identities by credential digest, with when they were verified;
/// insertion-ordered like a JS `Map`, the oldest evicted past 128.
#[derive(Default)]
struct RoutingIdentities {
    entries: HashMap<String, (i64, RoutingIdentity)>,
    order: VecDeque<String>,
}

struct IdentityLock {
    gate: Arc<tokio::sync::Mutex<()>>,
    users: usize,
}

struct Inner {
    github: GitHubCli,
    budget: GitHubGraphQlBudget,
    clock: SharedClock,
    /// `routingIdentities`: insertion-ordered, the oldest evicted past 128.
    routing_identities: Mutex<RoutingIdentities>,
    identity_locks: Mutex<HashMap<String, IdentityLock>>,
    /// `nodeIds`: least recently used first.
    node_ids: Mutex<VecDeque<(String, String)>>,
    summaries: Mutex<SummaryBatches>,
}

/// Releases a per-credential identity lock, also when the waiting read is dropped.
struct IdentityLockLease {
    inner: Arc<Inner>,
    key: String,
}

impl Drop for IdentityLockLease {
    fn drop(&mut self) {
        let mut locks = self.inner.identity_locks.lock().expect("identity locks");
        if let Some(lock) = locks.get_mut(&self.key) {
            lock.users -= 1;
            if lock.users == 0 {
                locks.remove(&self.key);
            }
        }
    }
}

/// The `GitHubPullRequestCli` service.
#[derive(Clone)]
pub struct GitHubPullRequestCli {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for GitHubPullRequestCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubPullRequestCli").finish_non_exhaustive()
    }
}

/// One GraphQL read: the document, its variables, and the read it is reported as.
struct GraphQlRead<'a> {
    cwd: &'a str,
    host: &'a str,
    operation: &'a str,
    allow_reserve: bool,
    /// Variables as `-f`/`-F` flags, for values this module composed itself.
    variables: Vec<(&'static str, String)>,
    /// Variables carrying words the reader typed: document and variables travel over stdin.
    private_variables: Option<Vec<(String, String)>>,
    query: &'a str,
}

impl<'a> GraphQlRead<'a> {
    fn new(cwd: &'a str, host: &'a str, operation: &'a str, query: &'a str) -> Self {
        Self {
            cwd,
            host,
            operation,
            allow_reserve: false,
            variables: Vec::new(),
            private_variables: None,
            query,
        }
    }

    fn reserve(mut self, allow_reserve: bool) -> Self {
        self.allow_reserve = allow_reserve;
        self
    }

    fn variables(mut self, variables: Vec<(&'static str, String)>) -> Self {
        self.variables = variables;
        self
    }
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `Schema.decodeUnknownEffect(Schema.fromJsonString(Struct({id: PositiveInt, login: TrimmedNonEmptyString})))`.
fn decode_routing_identity(raw: &str) -> Option<RoutingIdentity> {
    let value: Value = serde_json::from_str(raw).ok()?;
    let id = positive_int(value.get("id")?)?;
    let login = trimmed_non_empty(value.get("login")?)?;
    Some(RoutingIdentity {
        account_id: id.to_string(),
        viewer: login,
    })
}

impl GitHubPullRequestCli {
    /// `GitHubPullRequestCli.make`, over the server's shared `GitHubCli` (whose GraphQL budget
    /// and rate limits it uses). `clock` dates the routing identities it caches.
    pub fn new(github: GitHubCli, clock: SharedClock) -> Self {
        let budget = github.budget().clone();
        Self {
            inner: Arc::new(Inner {
                github,
                budget,
                clock,
                routing_identities: Mutex::default(),
                identity_locks: Mutex::default(),
                node_ids: Mutex::default(),
                summaries: Mutex::default(),
            }),
        }
    }

    /// The `GitHubCli` this service runs `gh` through.
    pub fn github(&self) -> &GitHubCli {
        &self.inner.github
    }

    async fn execute(&self, input: GitHubExecuteInput) -> CliResult<VcsProcessOutput> {
        Ok(self.inner.github.execute(input).await?)
    }

    /// `captureVerifiedCredential`. Only the token's digest is kept; credential lookup output is
    /// never attached to an error.
    async fn capture_verified_credential(&self, cwd: &str, host: &str) -> CliResult<CapturedCredential> {
        let unavailable = || GitHubPullRequestCliError::ViewerLoginUnavailable { cwd: cwd.to_owned() };
        let host = host.to_lowercase();
        let pinned = current_pinned_github_credential();
        if pinned.as_ref().is_some_and(|pinned| pinned.host != host) {
            return Err(unavailable());
        }
        let token = match &pinned {
            Some(pinned) => pinned.token.clone(),
            None => {
                let mut input = GitHubExecuteInput::new(cwd, ["auth", "token", "--hostname", host.as_str()]);
                input.env = Some([("GH_DEBUG".to_owned(), String::new())].into_iter().collect());
                let output = self.inner.github.execute(input).await.map_err(|_| unavailable())?;
                js_trim(&output.stdout).to_owned()
            }
        };
        if token.is_empty() {
            return Err(unavailable());
        }
        let key = format!("{host}:{}", sha256_hex(&token));
        // A cold page may ask several times: wait per credential and check again after the first
        // verification. Dropping a waiter releases the next one without losing its request.
        let gate = {
            let mut locks = self.inner.identity_locks.lock().expect("identity locks");
            let lock = locks.entry(key.clone()).or_insert_with(|| IdentityLock {
                gate: Arc::default(),
                users: 0,
            });
            lock.users += 1;
            lock.gate.clone()
        };
        let _lease = IdentityLockLease {
            inner: self.inner.clone(),
            key: key.clone(),
        };
        let _permit = gate.lock().await;
        let credential = |identity: RoutingIdentity| CapturedCredential {
            host: host.clone(),
            token: token.clone(),
            credential_fingerprint: key.clone(),
            account_id: identity.account_id,
            viewer: identity.viewer,
        };
        let now = self.inner.clock.now_millis();
        let cached = {
            let identities = self.inner.routing_identities.lock().expect("routing identities");
            identities
                .entries
                .get(&key)
                .filter(|(at, _)| now - at < ROUTING_IDENTITY_TTL_MS)
                .map(|(_, identity)| identity.clone())
        };
        if let Some(identity) = cached {
            return Ok(credential(identity));
        }
        // Pin this read so an auth switch cannot poison its cache entry.
        let mut input = GitHubExecuteInput::new(cwd, ["api", "user", "--hostname", host.as_str()]);
        input.env = Some(
            [
                ("GH_HOST", host.as_str()),
                ("GH_TOKEN", token.as_str()),
                ("GITHUB_TOKEN", token.as_str()),
                ("GH_ENTERPRISE_TOKEN", token.as_str()),
                ("GITHUB_ENTERPRISE_TOKEN", token.as_str()),
                ("GH_DEBUG", ""),
            ]
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect(),
        );
        let response = self.inner.github.execute(input).await.map_err(|_| unavailable())?;
        let identity = decode_routing_identity(&response.stdout).ok_or_else(unavailable)?;
        {
            let mut identities = self.inner.routing_identities.lock().expect("routing identities");
            let RoutingIdentities { entries, order } = &mut *identities;
            if entries.len() >= ROUTING_IDENTITY_CAPACITY {
                if let Some(oldest) = order.pop_front() {
                    entries.remove(&oldest);
                }
            }
            if entries.insert(key.clone(), (now, identity.clone())).is_none() {
                order.push_back(key.clone());
            }
        }
        Ok(credential(identity))
    }

    /// `withVerifiedCredential(input, use)`: runs `future` pinned to the verified credential.
    pub async fn with_verified_credential<F, Fut, T>(&self, cwd: &str, host: &str, use_identity: F) -> CliResult<T>
    where
        F: FnOnce(VerifiedIdentity) -> Fut,
        Fut: Future<Output = T>,
    {
        let captured = self.capture_verified_credential(cwd, host).await?;
        let identity = VerifiedIdentity {
            account_id: captured.account_id,
            viewer: captured.viewer,
            credential_fingerprint: captured.credential_fingerprint.clone(),
        };
        let pinned = PinnedGitHubCredential {
            host: captured.host,
            token: captured.token,
            credential_fingerprint: captured.credential_fingerprint.clone(),
        };
        Ok(with_credential_scope(captured.credential_fingerprint, with_pinned_github_credential(pinned, use_identity(identity))).await)
    }

    /// `graphql`: a mutation whose answer is not read back (`gh` exits non-zero on a GraphQL
    /// error). Query and variables travel over stdin as one document.
    async fn graphql(&self, cwd: &str, host: &str, query: &str, variables: &[(String, String)]) -> CliResult<()> {
        let mut input = GitHubExecuteInput::new(cwd, ["api", "graphql", "--hostname", host, "--input", "-"]);
        input.stdin = Some(gh_json::encode_graph_ql_request_json(query, variables));
        self.execute(input).await.map(drop)
    }

    /// `graphqlRead`: a GraphQL read whose answer is decoded, reporting a failure against the
    /// read that made it. The budget reserves its cost first and learns the balance after.
    async fn graphql_read<A>(&self, read: GraphQlRead<'_>, decode: impl FnOnce(&str) -> Result<A, gh_json::DecodeFailure>) -> CliResult<A> {
        let query = self
            .inner
            .budget
            .query(read.host, read.query, read.allow_reserve)
            .map_err(GitHubPullRequestCliError::RateLimitPaused)?;
        let input = match read.private_variables {
            None => {
                let mut args: Vec<String> = vec!["api".into(), "graphql".into(), "--hostname".into(), read.host.to_owned()];
                for (flag, value) in read.variables {
                    args.push(flag.to_owned());
                    args.push(value);
                }
                args.push("-f".into());
                args.push(format!("query={query}"));
                GitHubExecuteInput::new(read.cwd, args)
            }
            Some(variables) => {
                let mut input = GitHubExecuteInput::new(read.cwd, ["api", "graphql", "--hostname", read.host, "--input", "-"]);
                input.stdin = Some(gh_json::encode_graph_ql_request_json(&query, &variables));
                input
            }
        };
        let result = self.execute(input).await?;
        self.inner.budget.observe(read.host, &result.stdout);
        decode(js_trim(&result.stdout)).map_err(|failure| GitHubPullRequestCliError::read(read.cwd, read.operation, failure.cause()))
    }

    /// `pullRequestNodeId`: the pull request's own node id, which a mutation against the pull
    /// request itself is addressed by. Kept for life (least recently used evicted), so ticking
    /// files viewed does not pay a round trip per press; a failed lookup is not remembered.
    async fn pull_request_node_id(&self, input: &ChangeRequestRef, operation: &str) -> CliResult<String> {
        let (owner, name) = parse_repository_selector(&input.repository);
        let key = format!("{} {owner}/{name} {}", input.host, input.number);
        {
            let mut node_ids = self.inner.node_ids.lock().expect("node ids");
            if let Some(position) = node_ids.iter().position(|(held, _)| *held == key) {
                // Put back at the end on every hit, so the open review is not what falls out.
                let entry = node_ids.remove(position).expect("present");
                let node_id = entry.1.clone();
                node_ids.push_back(entry);
                return Ok(node_id);
            }
        }
        let node_id = self
            .graphql_read(
                GraphQlRead::new(&input.cwd, &input.host, operation, gh_json::PULL_REQUEST_NODE_ID_GRAPHQL_QUERY)
                    .reserve(true)
                    .variables(pull_request_variables(&owner, &name, input.number)),
                gh_json::decode_pull_request_node_id_json,
            )
            .await?;
        let mut node_ids = self.inner.node_ids.lock().expect("node ids");
        if node_ids.len() >= NODE_ID_CACHE_CAPACITY {
            node_ids.pop_front();
        }
        node_ids.push_back((key, node_id.clone()));
        Ok(node_id)
    }

    /// `subjectBelongsToPullRequest`: whether a client-given subject belongs to the pull request
    /// the request names (a mutation would otherwise write wherever the id actually belongs).
    async fn subject_belongs_to_pull_request(&self, input: &ChangeRequestRef, subject_id: &str, operation: &str) -> CliResult<bool> {
        let (owner, name) = parse_repository_selector(&input.repository);
        let mut variables = pull_request_variables(&owner, &name, input.number);
        variables.push(("-f", format!("subjectId={subject_id}")));
        self.graphql_read(
            GraphQlRead::new(&input.cwd, &input.host, operation, gh_json::REACTION_SUBJECT_PULL_REQUEST_GRAPHQL_QUERY)
                .reserve(true)
                .variables(variables),
            gh_json::decode_reaction_subject_scope_json,
        )
        .await
    }

    /// `diffFilesPage`: one page of the patch from the files API (GitHub refuses `pr diff` past
    /// 300 files and still serves those hunks here). A named commit reads the commit endpoint.
    async fn diff_files_page(&self, input: &ChangeRequestRef, page: i64, commit: Option<&str>) -> CliResult<ProviderDiffSlice> {
        let (owner, name) = parse_repository_selector(&input.repository);
        let paging = format!("per_page={DIFF_FILES_PAGE_SIZE}&page={page}");
        let mut args: Vec<String> = vec![
            "api".into(),
            "--hostname".into(),
            input.host.clone(),
            match commit {
                None => format!("repos/{owner}/{name}/pulls/{}/files?{paging}", input.number),
                Some(commit) => format!("repos/{owner}/{name}/commits/{commit}?{paging}"),
            },
        ];
        // An empty commit carries no `files` at all, which is a commit with nothing in it.
        if commit.is_some() {
            args.push("--jq".into());
            args.push(".files // []".into());
        }
        let mut request = GitHubExecuteInput::new(&input.cwd, args);
        request.max_output_bytes = Some(DIFF_MAX_OUTPUT_BYTES);
        request.timeout_ms = Some(DIFF_TIMEOUT_MS);
        let result = self.execute(request).await?;
        // Checked before decoding: a byte-truncated response is a JSON prefix.
        if result.stdout_truncated {
            return Err(GitHubPullRequestCliError::read(
                &input.cwd,
                "getPullRequestDiff",
                Cause::message(format!("Page {page} of the changed files was too large to read.")),
            ));
        }
        let decoded = gh_json::decode_pull_request_files_json(js_trim(&result.stdout))
            .map_err(|failure| GitHubPullRequestCliError::read(&input.cwd, "getPullRequestDiff", failure.cause()))?;
        // Counted before decoding, so a page whose files all failed to decode still moves on.
        let more_pages = decoded.raw_count as i64 >= DIFF_FILES_PAGE_SIZE;
        Ok(ProviderDiffSlice {
            patch: decoded.patch,
            truncated: decoded.truncated,
            next_cursor: more_pages.then(|| (page + 1).to_string()),
            omitted_file_stats: (!decoded.omitted_file_stats.is_empty()).then_some(decoded.omitted_file_stats),
        })
    }

    /// `readLegacyDetail`: `gh pr view --json`, which pages check contexts itself.
    async fn read_legacy_detail(&self, input: &ChangeRequestRef, operation: &str) -> CliResult<gh_json::GitHubPullRequestDetail> {
        let mut args = vec!["pr".to_owned(), "view".into(), input.number.to_string()];
        args.extend(repository_args(&input.host, &input.repository));
        args.push("--json".into());
        args.push(gh_json::PULL_REQUEST_DETAIL_JSON_FIELDS.into());
        let result = self.execute(GitHubExecuteInput::new(&input.cwd, args)).await?;
        gh_json::decode_pull_request_detail_json(js_trim(&result.stdout))
            .map_err(|failure| GitHubPullRequestCliError::read(&input.cwd, operation, failure.cause()))
    }

    /// `viewPullRequestSummary`: one `gh pr view` for a summary the batch could not answer.
    async fn view_pull_request_summary(&self, input: &ChangeRequestRef) -> CliResult<ProviderChangeRequestSummary> {
        let detail = self.read_legacy_detail(input, "getPullRequestSummary").await?;
        let item = detail.item;
        Ok(ProviderChangeRequestSummary {
            number: item.number,
            title: item.title,
            url: item.url,
            head_branch: item.head_branch,
            base_branch: item.base_branch,
            state: item.state,
            is_draft: Some(item.is_draft),
            closed_at: Some(detail.closed_at),
            merged_at: Some(detail.merged_at),
            updated_at: item.updated_at,
            author: Some(item.author),
            additions: Some(item.additions),
            deletions: Some(item.deletions),
            changed_files: Some(detail.changed_files),
            review_decision: Some(item.review_decision),
            checks_state: Some(item.checks_state),
            mergeability: Some(item.mergeability),
        })
    }

    /// The summary resolver: entries of one window share aliased reads of 25; whatever the batch
    /// cannot answer (an unaddressable selector, a pull request GitHub returned nothing for, a
    /// batch that failed) is read on its own. A paused budget fails every entry instead: reading
    /// one at a time would only spend what is being saved.
    async fn resolve_summaries(&self, entries: Vec<SummaryEntry>) {
        let first = entries[0].request.clone();
        let batchable: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| gh_json::build_pull_request_summaries_graph_ql_query(&[(entry.request.repository.as_str(), entry.request.number)]).is_some())
            .map(|(index, _)| index)
            .collect();
        let selectors: Vec<(&str, i64)> = batchable
            .iter()
            .map(|&index| (entries[index].request.repository.as_str(), entries[index].request.number))
            .collect();
        let summaries = match gh_json::build_pull_request_summaries_graph_ql_query(&selectors) {
            None => Ok(BTreeMap::new()),
            Some(query) => {
                self.graphql_read(
                    GraphQlRead::new(&first.cwd, &first.host, "getPullRequestSummary", &query),
                    gh_json::decode_pull_request_summaries_json,
                )
                .await
            }
        };
        let summaries = match summaries {
            Ok(summaries) => summaries,
            Err(GitHubPullRequestCliError::RateLimitPaused(paused)) => {
                for entry in entries {
                    let _ = entry.reply.send(Err(GitHubPullRequestCliError::RateLimitPaused(paused.clone())));
                }
                return;
            }
            Err(error) => {
                tracing::debug!(cause = %error, "batched pull request summary read failed");
                BTreeMap::new()
            }
        };
        let mut unanswered = Vec::new();
        for (index, entry) in entries.into_iter().enumerate() {
            let summary = batchable
                .iter()
                .position(|&batched| batched == index)
                .and_then(|position| summaries.get(&position).cloned());
            match summary {
                Some(summary) => {
                    let _ = entry.reply.send(Ok(provider_summary(summary)));
                }
                None => unanswered.push(entry),
            }
        }
        futures::stream::iter(unanswered)
            .for_each_concurrent(STAT_REQUEST_CONCURRENCY, |entry| async move {
                let result = self.view_pull_request_summary(&entry.request).await;
                let _ = entry.reply.send(result);
            })
            .await;
    }

    /// Hands a batch to the resolver in a task of its own, in the context its entries ran in.
    fn spawn_summary_batch(&self, batch: PendingSummaries) {
        let cli = self.clone();
        tokio::spawn(batch.context.clone().run(async move { cli.resolve_summaries(batch.entries).await }));
    }

    /// `getPullRequestSummary`: `Effect.request` against the grouped, delayed resolver.
    async fn request_pull_request_summary(&self, input: ChangeRequestRef) -> CliResult<ProviderChangeRequestSummary> {
        let context = AmbientContext::capture();
        let key = serde_json::json!([
            input.host.to_lowercase(),
            context.pinned.as_ref().map(|pinned| pinned.credential_fingerprint.clone()),
            context.scope
        ])
        .to_string();
        let (reply, answer) = oneshot::channel();
        let entry = SummaryEntry { request: input, reply };
        let full = {
            let mut batches = self.inner.summaries.lock().expect("summary batches");
            let batches = &mut *batches;
            let batch = match batches.pending.get_mut(&key) {
                Some(batch) => batch,
                None => {
                    batches.next_id += 1;
                    let id = batches.next_id;
                    let cli = self.clone();
                    let timer_key = key.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(SUMMARY_BATCH_WINDOW).await;
                        let batch = {
                            let mut batches = cli.inner.summaries.lock().expect("summary batches");
                            match batches.pending.get(&timer_key) {
                                Some(batch) if batch.id == id => batches.pending.remove(&timer_key),
                                _ => None,
                            }
                        };
                        if let Some(batch) = batch {
                            cli.spawn_summary_batch(batch);
                        }
                    });
                    batches.pending.entry(key.clone()).or_insert(PendingSummaries {
                        id,
                        context,
                        entries: Vec::new(),
                    })
                }
            };
            batch.entries.push(entry);
            // A full batch goes now; later reads collect into the next one.
            if batch.entries.len() >= STAT_ALIASES_PER_REQUEST {
                batches.pending.remove(&key)
            } else {
                None
            }
        };
        if let Some(batch) = full {
            self.spawn_summary_batch(batch);
        }
        answer.await.unwrap_or_else(|_| {
            Err(GitHubPullRequestCliError::read(
                "",
                "getPullRequestSummary",
                Cause::message("The batched pull request summary read was interrupted."),
            ))
        })
    }

    /// `listPullRequests`' one read: the search when `continues`, or the search-free fallback,
    /// which grows (doubling up to 1,000 rows) until it fills the filtered page.
    fn list_read<'a>(
        &'a self,
        input: &'a ListChangeRequestsInput,
        continues: bool,
        requested_rows: i64,
    ) -> BoxFuture<'a, CliResult<GitHubPullRequestListBatch>> {
        async move {
            let fallback_max_rows = (input.limit + 1).max(PULL_REQUEST_FALLBACK_MAX_ROWS);
            let mut args = vec!["pr".to_owned(), "list".into()];
            args.extend(repository_args(&input.host, &input.repository));
            args.extend(involvement_args(input, continues));
            args.extend([
                "--state".to_owned(),
                input.state.as_str().into(),
                "--limit".into(),
                // One extra row reveals that the repository has more than the page shows.
                requested_rows.to_string(),
                "--json".into(),
                gh_json::PULL_REQUEST_LIST_JSON_FIELDS.into(),
            ]);
            let result = self.execute(GitHubExecuteInput::new(&input.cwd, args)).await?;
            let raw = js_trim(&result.stdout);
            if raw.is_empty() {
                return Ok(GitHubPullRequestListBatch {
                    items: Vec::new(),
                    truncated: false,
                    continues,
                });
            }
            let decoded = gh_json::decode_pull_request_list_json(raw)
                .map_err(|failure| GitHubPullRequestCliError::read(&input.cwd, "listPullRequests", failure.cause()))?;
            let raw_count = decoded.raw_count as i64;
            let items: Vec<_> = if continues {
                decoded.items
            } else {
                decoded.items.into_iter().filter(|item| matches_unsorted_listing(item, input)).collect()
            };
            let item_count = items.len() as i64;
            if !continues && item_count < input.limit && raw_count >= requested_rows && requested_rows < fallback_max_rows {
                let next_rows = (requested_rows * 2).min(fallback_max_rows);
                if next_rows > requested_rows {
                    return self.list_read(input, false, next_rows).await;
                }
            }
            // One row over the page size is the probe for a next page, counted before decoding:
            // a skipped malformed row must not end paging.
            let truncated = if continues {
                raw_count > input.limit
            } else {
                item_count > input.limit || raw_count >= requested_rows
            };
            Ok(GitHubPullRequestListBatch {
                items: items.into_iter().take(input.limit.max(0) as usize).collect(),
                truncated,
                continues,
            })
        }
        .boxed()
    }
}

/// A batched summary as the provider type.
fn provider_summary(summary: gh_json::GitHubPullRequestSummary) -> ProviderChangeRequestSummary {
    ProviderChangeRequestSummary {
        number: summary.number,
        title: summary.title,
        url: summary.url,
        head_branch: summary.head_branch,
        base_branch: summary.base_branch,
        state: summary.state,
        is_draft: Some(summary.is_draft),
        closed_at: Some(summary.closed_at),
        merged_at: Some(summary.merged_at),
        updated_at: summary.updated_at,
        author: Some(summary.author),
        additions: Some(summary.additions),
        deletions: Some(summary.deletions),
        changed_files: Some(summary.changed_files),
        review_decision: Some(summary.review_decision),
        checks_state: Some(summary.checks_state),
        mergeability: Some(summary.mergeability),
    }
}

/// Splits `items` into chunks of [`STAT_ALIASES_PER_REQUEST`].
fn chunks<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    items.chunks(STAT_ALIASES_PER_REQUEST).map(<[T]>::to_vec).collect()
}

#[async_trait]
impl GitHubPullRequestCliApi for GitHubPullRequestCli {
    async fn verified_credential(&self, cwd: &str, host: &str) -> CliResult<VerifiedCredential> {
        let captured = self.capture_verified_credential(cwd, host).await?;
        Ok(VerifiedCredential {
            identity: VerifiedIdentity {
                account_id: captured.account_id,
                viewer: captured.viewer,
                credential_fingerprint: captured.credential_fingerprint.clone(),
            },
            scope: Arc::new(PinnedScope {
                credential: PinnedGitHubCredential {
                    host: captured.host,
                    token: captured.token,
                    credential_fingerprint: captured.credential_fingerprint,
                },
            }),
        })
    }

    async fn get_routing_identity(&self, cwd: &str, host: &str) -> CliResult<RoutingIdentity> {
        let captured = self.capture_verified_credential(cwd, host).await?;
        Ok(RoutingIdentity {
            account_id: captured.account_id,
            viewer: captured.viewer,
        })
    }

    async fn get_viewer_login(&self, cwd: &str, host: &str) -> CliResult<String> {
        Ok(self.get_routing_identity(cwd, host).await?.viewer)
    }

    async fn list_pull_requests(&self, input: ListChangeRequestsInput) -> CliResult<GitHubPullRequestListBatch> {
        // GitHub does not index every repository for search, and one it will not search answers
        // with no rows rather than an error, so an empty first slice is read again the way `gh`
        // lists without one. Never under a cursor (a repository that answered once has run out)
        // and never under free text (an empty answer to a search is the answer).
        let has_query = input.query.as_deref().is_some_and(|query| !js_trim(query).is_empty());
        let mut batch = self.list_read(&input, true, input.limit + 1).await?;
        if batch.items.is_empty() && input.cursor.is_none() && !has_query {
            batch = self.list_read(&input, false, input.limit + 1).await?;
        }
        // Match the search query's host support, and enrich only rows that survived paging.
        if input.host != "github.com" || batch.items.is_empty() {
            return Ok(batch);
        }
        let enriched: Vec<Vec<gh_json::GitHubPullRequestListItem>> = futures::stream::iter(chunks(&batch.items))
            .map(|chunk| {
                let input = &input;
                async move {
                    let numbers: Vec<i64> = chunk.iter().map(|item| item.number).collect();
                    let Some(query) = gh_json::build_pull_request_stack_memberships_graph_ql_query(&input.repository, &numbers) else {
                        return chunk;
                    };
                    let read = self
                        .graphql_read(
                            GraphQlRead::new(&input.cwd, &input.host, "listPullRequestStackMemberships", &query),
                            gh_json::decode_pull_request_stack_memberships_json,
                        )
                        .await;
                    match read {
                        Ok(memberships) => {
                            let memberships: HashMap<_, _> = memberships.into_iter().collect();
                            chunk
                                .into_iter()
                                .enumerate()
                                .map(|(index, mut item)| {
                                    if let Some(stack) = memberships.get(&index) {
                                        item.stack = Some(stack.clone());
                                    }
                                    item
                                })
                                .collect()
                        }
                        // Optional badges must not take down a listing that already read.
                        Err(_) => {
                            tracing::warn!(operation = "listPullRequestStackMemberships", host = %input.host, rows = chunk.len(), "Pull request stack membership enrichment failed");
                            chunk
                        }
                    }
                }
            })
            .buffered(STAT_REQUEST_CONCURRENCY)
            .collect()
            .await;
        batch.items = enriched.into_iter().flatten().collect();
        Ok(batch)
    }

    async fn search_pull_requests(&self, input: ListChangeRequestsAcrossInput) -> CliResult<GitHubPullRequestSearchBatch> {
        let Some(query) = search_query(&input) else {
            return Err(GitHubPullRequestCliError::RepositorySelector {
                cwd: input.cwd.clone(),
                operation: "searchPullRequests".into(),
            });
        };
        // One extra row reveals that the host has more, up to GitHub's own ceiling on a search
        // page, past which `hasNextPage` says so.
        let rows = (input.limit + 1).min(gh_json::PULL_REQUEST_SEARCH_MAX_ROWS);
        let document = gh_json::pull_request_search_graph_ql_query(rows, input.host == "github.com");
        let mut read = GraphQlRead::new(&input.cwd, &input.host, "searchPullRequests", &document);
        // The reader's own words are in the query, so it travels over stdin rather than argv.
        read.private_variables = Some(vec![("q".to_owned(), query)]);
        let batch = self.graphql_read(read, gh_json::decode_pull_request_search_json).await?;
        Ok(GitHubPullRequestSearchBatch {
            truncated: batch.raw_count as i64 > input.limit || batch.has_next_page,
            items: batch.items.into_iter().take(input.limit.max(0) as usize).collect(),
        })
    }

    async fn list_pull_request_stats(&self, input: ListChangeRequestStatsInput) -> CliResult<Vec<GitHubPullRequestStat>> {
        let results: Vec<Vec<GitHubPullRequestStat>> = futures::stream::iter(chunks(&input.change_requests))
            .map(|chunk| {
                let input = &input;
                async move {
                    let selectors: Vec<(&str, i64)> = chunk.iter().map(|(repository, number)| (repository.as_str(), *number)).collect();
                    let Some(query) = gh_json::build_pull_request_stats_graph_ql_query(&selectors) else {
                        return Err(GitHubPullRequestCliError::RepositorySelector {
                            cwd: input.cwd.clone(),
                            operation: "listPullRequestStats".into(),
                        });
                    };
                    let stats = self
                        .graphql_read(
                            GraphQlRead::new(&input.cwd, &input.host, "listPullRequestStats", &query),
                            gh_json::decode_pull_request_stats_json,
                        )
                        .await?;
                    let stats: HashMap<_, _> = stats.into_iter().collect();
                    Ok(chunk
                        .into_iter()
                        .enumerate()
                        .filter_map(|(index, (repository, number))| {
                            stats.get(&index).map(|stat| GitHubPullRequestStat {
                                repository,
                                number,
                                additions: stat.additions,
                                deletions: stat.deletions,
                            })
                        })
                        .collect())
                }
            })
            .buffered(STAT_REQUEST_CONCURRENCY)
            .try_collect()
            .await?;
        Ok(results.into_iter().flatten().collect())
    }

    async fn get_pull_request_summary(&self, input: ChangeRequestRef) -> CliResult<ProviderChangeRequestSummary> {
        self.request_pull_request_summary(input).await
    }

    async fn get_pull_request_detail(&self, input: ChangeRequestRef) -> CliResult<gh_json::GitHubPullRequestCore> {
        let (owner, name) = parse_repository_selector(&input.repository);
        let mut variables = pull_request_variables(&owner, &name, input.number);
        variables.push(("-f", format!("headRef=refs/pull/{}/head", input.number)));
        let core = self
            .graphql_read(
                GraphQlRead::new(&input.cwd, &input.host, "getPullRequestDetail", gh_json::PULL_REQUEST_CORE_GRAPHQL_QUERY)
                    .reserve(github_reserve_allowed())
                    .variables(variables),
                gh_json::decode_pull_request_core_json,
            )
            .await?;
        if !core.checks_truncated {
            return Ok(core);
        }
        // gh already pages check contexts: keep its complete, deduplicated result for large
        // check suites instead of letting the first 100 checks imply success.
        let detail = self.read_legacy_detail(&input, "getPullRequestDetail").await?;
        if detail.head_sha != core.detail.head_sha {
            return Err(GitHubPullRequestCliError::read(
                &input.cwd,
                "getPullRequestDetail",
                Cause::message("Pull request head changed while reading checks."),
            ));
        }
        let mut core = core;
        core.detail.checks = detail.checks;
        core.detail.item.checks_state = detail.item.checks_state;
        core.checks_truncated = false;
        Ok(core)
    }

    async fn get_pull_request_preview(&self, input: ChangeRequestRef) -> CliResult<ProviderChangeRequestPreview> {
        let (owner, name) = parse_repository_selector(&input.repository);
        let preview = self
            .graphql_read(
                GraphQlRead::new(&input.cwd, &input.host, "getPullRequestPreview", gh_json::PULL_REQUEST_PREVIEW_GRAPHQL_QUERY)
                    .variables(pull_request_variables(&owner, &name, input.number)),
                gh_json::decode_pull_request_preview_json,
            )
            .await?;
        Ok(ProviderChangeRequestPreview {
            number: preview.number,
            title: preview.title,
            url: preview.url,
            author: preview.author,
            state: preview.state,
            is_draft: preview.is_draft,
            created_at: preview.created_at,
        })
    }

    async fn list_workflow_runs_requiring_approval(&self, input: WorkflowApprovalInput) -> CliResult<Vec<gh_json::GitHubWorkflowRunApproval>> {
        let target = &input.change_request;
        let probe_limit = (WORKFLOW_APPROVAL_LIMIT + 1).to_string();
        let refused = |reason: WorkflowApprovalRefusal, observed_count: usize| GitHubPullRequestCliError::WorkflowApprovalRefused {
            cwd: target.cwd.clone(),
            number: target.number,
            reason,
            observed_count: observed_count as i64,
            limit: WORKFLOW_APPROVAL_LIMIT,
        };
        let read_error = |failure: gh_json::DecodeFailure| GitHubPullRequestCliError::read(&target.cwd, "listWorkflowRunsRequiringApproval", failure.cause());
        let heads = async {
            let mut args = vec!["pr".to_owned(), "list".into()];
            args.extend(repository_args(&target.host, &target.repository));
            args.extend([
                "--state".to_owned(),
                "open".into(),
                "--head".into(),
                input.head_branch.clone(),
                "--limit".into(),
                probe_limit.clone(),
                "--json".into(),
                "number,headRefOid,isCrossRepository,headRepositoryOwner".into(),
            ]);
            let result = self.execute(GitHubExecuteInput::new(&target.cwd, args)).await?;
            let decoded = gh_json::decode_pull_request_heads_json(js_trim(&result.stdout)).map_err(read_error)?;
            let owner = input.head_repository_owner.to_lowercase();
            let exact: Vec<_> = decoded
                .iter()
                .filter(|head| {
                    head.head_sha == input.head_sha
                        && head.is_cross_repository == Some(true)
                        && head.head_repository_owner.as_ref().map(|login| login.to_lowercase()) == Some(owner.clone())
                })
                .collect();
            if decoded.len() as i64 > WORKFLOW_APPROVAL_LIMIT {
                return Err(refused(WorkflowApprovalRefusal::HeadListTruncated, decoded.len()));
            }
            if exact.len() != 1 || exact[0].number != target.number {
                return Err(refused(WorkflowApprovalRefusal::HeadNotUnique, exact.len()));
            }
            Ok(())
        };
        let runs = async {
            let mut args = vec!["run".to_owned(), "list".into()];
            args.extend(repository_args(&target.host, &target.repository));
            args.extend([
                "--commit".to_owned(),
                input.head_sha.clone(),
                "--branch".into(),
                input.head_branch.clone(),
                "--event".into(),
                "pull_request".into(),
                "--status".into(),
                "action_required".into(),
                "--limit".into(),
                probe_limit.clone(),
                "--json".into(),
                "databaseId,workflowName,url".into(),
            ]);
            let result = self.execute(GitHubExecuteInput::new(&target.cwd, args)).await?;
            let runs = gh_json::decode_workflow_run_approvals_json(js_trim(&result.stdout)).map_err(read_error)?;
            if runs.len() as i64 > WORKFLOW_APPROVAL_LIMIT {
                return Err(refused(WorkflowApprovalRefusal::RunListTruncated, runs.len()));
            }
            Ok(runs)
        };
        // Both reads go out together (`Effect.all` with concurrency 2); the scope read's failure
        // is the one reported.
        let (heads, runs) = futures::join!(heads, runs);
        heads?;
        runs
    }

    async fn get_pull_request_stack(&self, input: ChangeRequestRef, include_details: bool) -> CliResult<Option<gh_json::GitHubPullRequestStack>> {
        let (owner, name) = parse_repository_selector(&input.repository);
        let read = async {
            let result = self
                .execute(GitHubExecuteInput::new(
                    &input.cwd,
                    [
                        "api".to_owned(),
                        "--hostname".into(),
                        input.host.clone(),
                        format!("repos/{owner}/{name}/stacks?pull_request={}", input.number),
                    ],
                ))
                .await?;
            let stack = gh_json::decode_pull_request_stacks_json(js_trim(&result.stdout))
                .map_err(|failure| GitHubPullRequestCliError::read(&input.cwd, "getPullRequestStack", failure.cause()))?;
            let stack = match stack {
                Some(stack) if include_details => stack,
                other => return Ok(other),
            };
            let result = self
                .execute(GitHubExecuteInput::new(
                    &input.cwd,
                    [
                        "api".to_owned(),
                        "--hostname".into(),
                        input.host.clone(),
                        format!("repos/{owner}/{name}/stacks/{}", stack.number),
                    ],
                ))
                .await?;
            gh_json::decode_pull_request_stacks_json(&format!("[{}]", js_trim(&result.stdout)))
                .map_err(|failure| GitHubPullRequestCliError::read(&input.cwd, "getPullRequestStack", failure.cause()))
        };
        match read.await {
            // Hosts without the stacks preview answer 404. Other failures keep the previously
            // synced stack and let the caller retry.
            Err(GitHubPullRequestCliError::Cli(error)) if error.kind == GitHubCliErrorKind::PullRequestNotFound => Ok(None),
            other => other,
        }
    }

    async fn get_pull_request_base_comparison(&self, input: BaseComparisonInput) -> CliResult<gh_json::GitHubBaseComparison> {
        let target = &input.change_request;
        let (owner, name) = parse_repository_selector(&target.repository);
        let mut variables = pull_request_variables(&owner, &name, target.number);
        variables.push(("-f", format!("headRef={}", input.head_ref)));
        self.graphql_read(
            GraphQlRead::new(
                &target.cwd,
                &target.host,
                "getPullRequestBaseComparison",
                gh_json::BASE_COMPARISON_GRAPHQL_QUERY,
            )
            .reserve(input.allow_reserve == Some(true))
            .variables(variables),
            gh_json::decode_base_comparison_json,
        )
        .await
    }

    async fn get_pull_request_activity(&self, input: ChangeRequestRef) -> CliResult<gh_json::GitHubPullRequestActivity> {
        let mut args = vec!["pr".to_owned(), "view".into(), input.number.to_string()];
        args.extend(repository_args(&input.host, &input.repository));
        args.push("--json".into());
        args.push(gh_json::PULL_REQUEST_ACTIVITY_JSON_FIELDS.into());
        let result = self.execute(GitHubExecuteInput::new(&input.cwd, args)).await?;
        gh_json::decode_pull_request_activity_json(js_trim(&result.stdout))
            .map_err(|failure| GitHubPullRequestCliError::read(&input.cwd, "getPullRequestActivity", failure.cause()))
    }

    async fn get_pull_request_diff(&self, input: GetDiffInput) -> CliResult<ProviderDiffSlice> {
        let target = &input.change_request;
        let commit = input.commit.as_deref();
        if commit.is_some_and(|commit| !is_commit_sha(commit)) {
            return Err(GitHubPullRequestCliError::DiffCommit { cwd: target.cwd.clone() });
        }
        // A cursor only ever comes from the files walk, so a reader carrying one is already past
        // the point where `gh pr diff` had anything to say.
        if let Some(cursor) = &input.cursor {
            return match diff_cursor_page(cursor) {
                None => Err(GitHubPullRequestCliError::DiffCursor { cwd: target.cwd.clone() }),
                Some(page) => self.diff_files_page(target, page, commit).await,
            };
        }
        // `gh pr diff` speaks for the whole pull request and cannot name one commit of it.
        if commit.is_some() {
            return self.diff_files_page(target, 1, commit).await;
        }
        let mut args = vec!["pr".to_owned(), "diff".into(), target.number.to_string()];
        args.extend(repository_args(&target.host, &target.repository));
        args.push("--color".into());
        args.push("never".into());
        let mut request = GitHubExecuteInput::new(&target.cwd, args);
        request.max_output_bytes = Some(DIFF_MAX_OUTPUT_BYTES);
        request.timeout_ms = Some(DIFF_TIMEOUT_MS);
        let whole = match self.execute(request).await {
            // A patch cut at a byte boundary ends mid-file: the files API serves the same change
            // a whole number of files at a time.
            Ok(result) if result.stdout_truncated => self.diff_files_page(target, 1, None).await,
            // One read served the whole patch, so there is no next slice to ask for.
            Ok(result) => Ok(ProviderDiffSlice {
                patch: result.stdout,
                truncated: false,
                next_cursor: None,
                omitted_file_stats: None,
            }),
            Err(error) => Err(error),
        };
        match whole {
            // GitHub answers 406 rather than a diff past 300 changed files, so the patch is read
            // from the files API instead. Only a command that ran and was refused (a missing or
            // signed-out `gh` fails the same way for every request), and a fallback that fails
            // too reports the original refusal.
            Err(GitHubPullRequestCliError::Cli(error)) if error.kind == GitHubCliErrorKind::Command => {
                self.diff_files_page(target, 1, None).await.map_err(|_| GitHubPullRequestCliError::Cli(error))
            }
            other => other,
        }
    }

    async fn get_pull_request_diff_file_contents(&self, input: DiffFileContentsInput) -> CliResult<ProviderDiffFileContents> {
        let target = &input.change_request;
        let commit = input.commit.as_deref();
        if commit.is_some_and(|commit| !is_commit_sha(commit)) {
            return Err(GitHubPullRequestCliError::DiffCommit { cwd: target.cwd.clone() });
        }
        let (owner, name) = parse_repository_selector(&target.repository);
        let (path, jq) = match commit {
            None => (format!("repos/{owner}/{name}/pulls/{}", target.number), "[.base.sha, .head.sha] | @tsv"),
            Some(commit) => (format!("repos/{owner}/{name}/commits/{commit}"), "[.parents[0].sha, .sha] | @tsv"),
        };
        let mut request = GitHubExecuteInput::new(
            &target.cwd,
            ["api".to_owned(), "--hostname".into(), target.host.clone(), path, "--jq".into(), jq.into()],
        );
        request.max_output_bytes = Some(1024);
        request.timeout_ms = Some(DIFF_TIMEOUT_MS);
        let refs = self.execute(request).await?;
        // Keep a leading tab: a root commit has no parent, which jq writes as an empty field.
        let fields: Vec<&str> = js_trim_end(&refs.stdout).split('\t').collect();
        let base_ref = fields[0];
        let head_ref = fields.get(1).copied();
        let root_commit_new_file = commit.is_some() && input.change_type == PullRequestDiffFileContentsInputChangeType::New && base_ref.is_empty();
        let unusable = refs.stdout_truncated
            || head_ref.is_none_or(str::is_empty)
            || fields.len() > 2
            || (!root_commit_new_file && !is_commit_sha(base_ref))
            || !head_ref.is_some_and(is_commit_sha);
        if unusable {
            return Err(GitHubPullRequestCliError::DiffRevisionsUnavailable {
                cwd: target.cwd.clone(),
                number: target.number,
                commit: input.commit.clone(),
            });
        }
        let head_ref = head_ref.unwrap_or_default();
        let (owner, name) = (&owner, &name);
        let read_file = |revision: String, file_path: String| async move {
            let encoded: Vec<String> = file_path.split('/').map(encode_uri_component).collect();
            let mut request = GitHubExecuteInput::new(
                &target.cwd,
                [
                    "api".to_owned(),
                    "--hostname".into(),
                    target.host.clone(),
                    "--header".into(),
                    "Accept: application/vnd.github.raw+json".into(),
                    format!("repos/{owner}/{name}/contents/{}?ref={}", encoded.join("/"), encode_uri_component(&revision)),
                ],
            );
            request.max_output_bytes = Some(DIFF_FILE_MAX_OUTPUT_BYTES);
            request.timeout_ms = Some(DIFF_TIMEOUT_MS);
            let result = self.execute(request).await?;
            if result.stdout_truncated || result.stdout.contains('\0') || result.stdout_invalid_utf8 {
                return Err(GitHubPullRequestCliError::DiffFileContentsUnavailable {
                    cwd: target.cwd.clone(),
                    path: file_path,
                    reason: if result.stdout_truncated {
                        DiffFileUnavailableReason::Oversized
                    } else {
                        DiffFileUnavailableReason::Binary
                    },
                });
            }
            Ok(result.stdout)
        };
        let old = async {
            match input.change_type {
                PullRequestDiffFileContentsInputChangeType::New => Ok(String::new()),
                _ => read_file(base_ref.to_owned(), input.old_path.clone()).await,
            }
        };
        let new = async {
            match input.change_type {
                PullRequestDiffFileContentsInputChangeType::Deleted => Ok(String::new()),
                _ => read_file(head_ref.to_owned(), input.new_path.clone()).await,
            }
        };
        // Both sides are read together (`Effect.all` with concurrency 2).
        let (old_contents, new_contents) = futures::join!(old, new);
        Ok(ProviderDiffFileContents {
            old_contents: old_contents?,
            new_contents: new_contents?,
        })
    }

    async fn get_pull_request_files_viewed(&self, input: ChangeRequestRef) -> CliResult<ProviderFilesViewed> {
        let (owner, name) = parse_repository_selector(&input.repository);
        let mut files: Vec<PullRequestFileViewed> = Vec::new();
        let mut after: Option<String> = None;
        let mut pages_left = FILES_VIEWED_MAX_PAGES;
        loop {
            let mut variables = pull_request_variables(&owner, &name, input.number);
            if let Some(after) = &after {
                variables.push(("-f", format!("after={after}")));
            }
            let page = self
                .graphql_read(
                    GraphQlRead::new(
                        &input.cwd,
                        &input.host,
                        "getPullRequestFilesViewed",
                        gh_json::PULL_REQUEST_FILES_VIEWED_GRAPHQL_QUERY,
                    )
                    .variables(variables),
                    gh_json::decode_pull_request_files_viewed_json,
                )
                .await?;
            files.extend(page.files);
            match page.next_cursor {
                None => return Ok(ProviderFilesViewed { files, truncated: false }),
                Some(_) if pages_left <= 1 => return Ok(ProviderFilesViewed { files, truncated: true }),
                Some(cursor) => {
                    after = Some(cursor);
                    pages_left -= 1;
                }
            }
        }
    }

    async fn set_pull_request_files_viewed(&self, input: SetFilesViewedInput) -> CliResult<()> {
        let files: Vec<(&str, bool)> = input.files.iter().map(|(path, viewed)| (path.as_str(), *viewed)).collect();
        let Some(mutation) = gh_json::build_set_files_viewed_graph_ql_mutation(&files) else {
            return Ok(());
        };
        let target = &input.change_request;
        let pull_request_id = self.pull_request_node_id(target, "setPullRequestFilesViewed").await?;
        let mut variables = vec![("pullRequestId".to_owned(), pull_request_id)];
        variables.extend(mutation.variables);
        self.graphql(&target.cwd, &target.host, &mutation.query, &variables).await
    }

    async fn list_review_thread_comments(&self, input: ChangeRequestRef) -> CliResult<gh_json::GitHubReviewThreadComments> {
        let (owner, name) = parse_repository_selector(&input.repository);
        let mut entries: Vec<gh_json::GitHubReviewThreadEntry> = Vec::new();
        let mut first: Option<gh_json::GitHubReviewThreadPage> = None;
        let mut avatars_by_login: BTreeMap<String, String> = BTreeMap::new();
        let mut bot_logins: BTreeSet<String> = BTreeSet::new();
        let mut cursor: Option<String> = None;
        let mut page = 0;
        loop {
            let mut variables = pull_request_variables(&owner, &name, input.number);
            variables.push(cursor_variable(cursor.as_deref()));
            let read = self
                .graphql_read(
                    GraphQlRead::new(&input.cwd, &input.host, "listReviewThreadComments", gh_json::REVIEW_THREADS_GRAPHQL_QUERY).variables(variables),
                    gh_json::decode_review_threads_json,
                )
                .await?;
            entries.extend(read.threads.iter().cloned());
            bot_logins.extend(read.bot_logins.iter().cloned());
            avatars_by_login.extend(read.avatars_by_login.iter().map(|(login, url)| (login.clone(), url.clone())));
            cursor = read.next_cursor.clone();
            // The roster, the commits and the viewer's standing travel with every page, and the
            // first one already carries all of them.
            if first.is_none() {
                first = Some(read);
            }
            page += 1;
            if cursor.is_none() || page >= REVIEW_THREAD_PAGES {
                break;
            }
        }
        let first = first.expect("one page was read");
        let mut dismissals_by_review_id = first.dismissals_by_review_id.clone();
        // Almost never entered: followed so a review whose event fell past the embedded page
        // still finds its reason.
        let mut dismissal_cursor = first.next_dismissal_cursor.clone();
        let mut dismissal_page = 0;
        while let Some(after) = dismissal_cursor.take().filter(|_| dismissal_page < REVIEW_THREAD_PAGES) {
            let mut variables = pull_request_variables(&owner, &name, input.number);
            variables.push(("-f", format!("cursor={after}")));
            let read = self
                .graphql_read(
                    GraphQlRead::new(&input.cwd, &input.host, "listReviewThreadComments", gh_json::REVIEW_DISMISSALS_GRAPHQL_QUERY).variables(variables),
                    gh_json::decode_review_dismissals_json,
                )
                .await?;
            dismissals_by_review_id.extend(read.dismissals_by_review_id);
            dismissal_cursor = read.next_cursor;
            dismissal_page += 1;
        }
        let truncated = cursor.is_some() || entries.iter().any(|entry| entry.next_comment_cursor.is_some());
        let review_threads: Vec<_> = entries
            .iter()
            .map(|entry| {
                let mut thread = entry.thread.clone();
                thread.comment_count = Some(entry.comment_count);
                if let Some(next) = &entry.next_comment_cursor {
                    thread.next_comments_cursor = Some(next.clone());
                }
                thread
            })
            .collect();
        Ok(gh_json::GitHubReviewThreadComments {
            comments: gh_json::review_thread_conversation(&review_threads),
            dismissals_by_review_id,
            review_threads,
            // GitHub's own count of each thread, even where a bound kept some words on GitHub.
            comment_count: entries.iter().map(|entry| entry.comment_count).sum(),
            truncated,
            reactions: first.reactions,
            reactions_by_id: first.reactions_by_id,
            reviewers: first.reviewers,
            avatars_by_login,
            bot_logins,
            commit_stats: first.commit_stats,
            commits: first.commits,
            viewer: first.viewer,
        })
    }

    async fn list_actor_avatars(&self, input: ActorAvatarsInput) -> CliResult<BTreeMap<String, String>> {
        if input.ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let variables = input.ids.iter().map(|id| ("-f", format!("ids[]={id}"))).collect();
        self.graphql_read(
            GraphQlRead::new(&input.cwd, &input.host, "listActorAvatars", gh_json::ACTOR_AVATARS_GRAPHQL_QUERY).variables(variables),
            gh_json::decode_actor_avatars_json,
        )
        .await
    }

    async fn get_review_thread_comments(&self, input: ReviewThreadCommentsInput) -> CliResult<PullRequestThreadCommentsResult> {
        let target = &input.change_request;
        let (owner, name) = parse_repository_selector(&target.repository);
        let mut variables = pull_request_variables(&owner, &name, target.number);
        variables.push(("-f", format!("threadId={}", input.thread_id)));
        let (flag, value) = cursor_variable(Some(&input.cursor));
        variables.push((flag, value));
        let page = self
            .graphql_read(
                GraphQlRead::new(
                    &target.cwd,
                    &target.host,
                    "getReviewThreadComments",
                    gh_json::REVIEW_THREAD_COMMENTS_GRAPHQL_QUERY,
                )
                .variables(variables),
                gh_json::decode_review_thread_comments_json,
            )
            .await?;
        if !page.belongs_to_pull_request {
            return Err(GitHubPullRequestCliError::SubjectScope {
                cwd: target.cwd.clone(),
                operation: "getReviewThreadComments".into(),
            });
        }
        Ok(PullRequestThreadCommentsResult {
            comments: page.comments,
            next_cursor: page.next_cursor,
        })
    }

    async fn get_viewer_access(&self, input: ViewerAccessInput) -> CliResult<gh_json::GitHubViewerRepositoryAccess> {
        let target = &input.change_request;
        let (owner, name) = parse_repository_selector(&target.repository);
        self.graphql_read(
            GraphQlRead::new(&target.cwd, &target.host, "getViewerAccess", gh_json::VIEWER_PERMISSIONS_GRAPHQL_QUERY)
                .reserve(input.allow_reserve == Some(true))
                .variables(pull_request_variables(&owner, &name, target.number)),
            gh_json::decode_viewer_permissions_json,
        )
        .await
    }

    async fn list_reviewer_candidates(&self, input: ChangeRequestRef) -> CliResult<PullRequestReviewerCandidateList> {
        let (owner, name) = parse_repository_selector(&input.repository);
        self.graphql_read(
            GraphQlRead::new(&input.cwd, &input.host, "listReviewerCandidates", gh_json::REVIEWER_CANDIDATES_GRAPHQL_QUERY)
                .reserve(true)
                .variables(pull_request_variables(&owner, &name, input.number)),
            gh_json::decode_reviewer_candidates_json,
        )
        .await
    }

    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> CliResult<()> {
        let target = &input.change_request;
        let (owner, name) = parse_repository_selector(&target.repository);
        // Posting to a login GitHub was already asked about is what a re-request is. The body
        // travels over stdin, like every other one.
        let mut request = GitHubExecuteInput::new(
            &target.cwd,
            [
                "api".to_owned(),
                "--method".into(),
                if input.requested { "POST" } else { "DELETE" }.into(),
                "--hostname".into(),
                target.host.clone(),
                format!("repos/{owner}/{name}/pulls/{}/requested_reviewers", target.number),
                "--input".into(),
                "-".into(),
            ],
        );
        request.stdin = Some(gh_json::build_reviewer_request_json(&input.reviewers));
        self.execute(request).await.map(drop)
    }

    async fn list_label_candidates(&self, input: ChangeRequestRef) -> CliResult<PullRequestLabelCandidateList> {
        let (owner, name) = parse_repository_selector(&input.repository);
        self.graphql_read(
            GraphQlRead::new(&input.cwd, &input.host, "listLabelCandidates", gh_json::LABEL_CANDIDATES_GRAPHQL_QUERY)
                .reserve(true)
                .variables(pull_request_variables(&owner, &name, input.number)),
            gh_json::decode_label_candidates_json,
        )
        .await
    }

    async fn set_labels(&self, input: SetLabelsInput) -> CliResult<()> {
        let target = &input.change_request;
        let (owner, name) = parse_repository_selector(&target.repository);
        // A pull request is an issue to the labels API. Adding posts a list and keeps what was
        // there; taking off is one delete per label, named in the path encoded.
        let issue = format!("repos/{owner}/{name}/issues/{}/labels", target.number);
        if input.applied {
            let mut request = GitHubExecuteInput::new(
                &target.cwd,
                [
                    "api".to_owned(),
                    "--method".into(),
                    "POST".into(),
                    "--hostname".into(),
                    target.host.clone(),
                    issue,
                    "--input".into(),
                    "-".into(),
                ],
            );
            request.stdin = Some(gh_json::build_label_request_json(&input.labels));
            return self.execute(request).await.map(drop);
        }
        for label in &input.labels {
            self.execute(GitHubExecuteInput::new(
                &target.cwd,
                [
                    "api".to_owned(),
                    "--method".into(),
                    "DELETE".into(),
                    "--hostname".into(),
                    target.host.clone(),
                    format!("{issue}/{}", encode_uri_component(label)),
                ],
            ))
            .await?;
        }
        Ok(())
    }

    async fn run_pull_request_action(&self, input: RunActionInput) -> CliResult<()> {
        let target = &input.change_request;
        if let Some(stack_number) = input.stack_number {
            return run_github_stack_action(
                &self.inner.github,
                GitHubStackActionInput {
                    cwd: target.cwd.clone(),
                    repository: target.repository.clone(),
                    host: target.host.clone(),
                    number: target.number,
                    stack_number,
                    expected_stack_heads: input.expected_stack_heads.clone(),
                    action: input.action,
                    merge_method: input.merge_method,
                },
            )
            .await
            .map_err(GitHubPullRequestCliError::from);
        }
        match input.action {
            PullRequestAction::Revert => {
                let pull_request_id = self.pull_request_node_id(target, "revertPullRequest").await?;
                self.graphql(
                    &target.cwd,
                    &target.host,
                    gh_json::REVERT_PULL_REQUEST_GRAPHQL_MUTATION,
                    &[("pullRequestId".to_owned(), pull_request_id)],
                )
                .await
            }
            PullRequestAction::ApproveWorkflows => self.approve_workflows(target).await,
            action => {
                let mut args = action_args(action, input.merge_method, input.update_method);
                let subcommand = args.remove(0);
                let mut command = vec!["pr".to_owned(), subcommand, target.number.to_string()];
                command.extend(repository_args(&target.host, &target.repository));
                command.extend(args);
                self.execute(GitHubExecuteInput::new(&target.cwd, command)).await.map(drop)
            }
        }
    }

    async fn comment_on_pull_request(&self, input: CommentInput) -> CliResult<()> {
        let target = &input.change_request;
        let mut args = vec!["pr".to_owned(), "comment".into(), target.number.to_string()];
        args.extend(repository_args(&target.host, &target.repository));
        args.push("--body-file".into());
        args.push("-".into());
        // The body travels over stdin: argv is visible in process listings and echoed back in
        // process-runner failures.
        let mut request = GitHubExecuteInput::new(&target.cwd, args);
        request.stdin = Some(input.body.clone());
        self.execute(request).await.map(drop)
    }

    async fn submit_review(&self, input: SubmitReviewInput) -> CliResult<()> {
        let target = &input.change_request;
        let (owner, name) = parse_repository_selector(&target.repository);
        // The whole review is one request, so nothing is visible until the verdict is sent.
        let mut request = GitHubExecuteInput::new(
            &target.cwd,
            [
                "api".to_owned(),
                "--method".into(),
                "POST".into(),
                "--hostname".into(),
                target.host.clone(),
                format!("repos/{owner}/{name}/pulls/{}/reviews", target.number),
                "--input".into(),
                "-".into(),
            ],
        );
        request.stdin = Some(gh_json::build_review_submission_json(input.verdict, &input.body, &input.comments));
        self.execute(request).await.map(drop)
    }

    async fn reply_to_review_thread(&self, input: ReplyToThreadInput) -> CliResult<()> {
        let target = &input.change_request;
        self.graphql(
            &target.cwd,
            &target.host,
            gh_json::REVIEW_THREAD_REPLY_GRAPHQL_MUTATION,
            &[("threadId".to_owned(), input.thread_id.clone()), ("body".to_owned(), input.body.clone())],
        )
        .await
    }

    async fn set_review_thread_resolution(&self, input: SetThreadResolutionInput) -> CliResult<()> {
        let target = &input.change_request;
        let query = if input.resolved {
            gh_json::RESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION
        } else {
            gh_json::UNRESOLVE_REVIEW_THREAD_GRAPHQL_MUTATION
        };
        self.graphql(&target.cwd, &target.host, query, &[("threadId".to_owned(), input.thread_id.clone())])
            .await
    }

    async fn set_reaction(&self, input: SetReactionInput) -> CliResult<()> {
        let target = &input.change_request;
        let subject_id = match &input.subject_id {
            None => self.pull_request_node_id(target, "setReaction").await?,
            Some(subject_id) => {
                if !self.subject_belongs_to_pull_request(target, subject_id, "setReaction").await? {
                    return Err(GitHubPullRequestCliError::SubjectScope {
                        cwd: target.cwd.clone(),
                        operation: "setReaction".into(),
                    });
                }
                subject_id.clone()
            }
        };
        let query = if input.reacted {
            gh_json::ADD_REACTION_GRAPHQL_MUTATION
        } else {
            gh_json::REMOVE_REACTION_GRAPHQL_MUTATION
        };
        self.graphql(
            &target.cwd,
            &target.host,
            query,
            &[
                ("subjectId".to_owned(), subject_id),
                ("content".to_owned(), gh_json::git_hub_reaction_content(input.content).to_owned()),
            ],
        )
        .await
    }

    async fn update_pull_request(&self, input: UpdateChangeRequestInput) -> CliResult<()> {
        let target = &input.change_request;
        let pull_request_id = self.pull_request_node_id(target, "updatePullRequest").await?;
        // A field the caller did not name is left out entirely, so GitHub keeps the words there.
        let mut variables = vec![("pullRequestId".to_owned(), pull_request_id)];
        if let Some(title) = &input.title {
            variables.push(("title".to_owned(), title.clone()));
        }
        if let Some(body) = &input.body {
            variables.push(("body".to_owned(), body.clone()));
        }
        self.graphql(&target.cwd, &target.host, gh_json::UPDATE_PULL_REQUEST_GRAPHQL_MUTATION, &variables)
            .await
    }

    async fn update_comment(&self, input: UpdateCommentInput) -> CliResult<()> {
        let target = &input.change_request;
        if !self.subject_belongs_to_pull_request(target, &input.comment_id, "updateComment").await? {
            return Err(GitHubPullRequestCliError::SubjectScope {
                cwd: target.cwd.clone(),
                operation: "updateComment".into(),
            });
        }
        let query = match input.kind {
            PullRequestCommentUpdateInputKind::IssueComment => gh_json::UPDATE_ISSUE_COMMENT_GRAPHQL_MUTATION,
            PullRequestCommentUpdateInputKind::ReviewComment => gh_json::UPDATE_REVIEW_COMMENT_GRAPHQL_MUTATION,
        };
        self.graphql(
            &target.cwd,
            &target.host,
            query,
            &[("commentId".to_owned(), input.comment_id.clone()), ("body".to_owned(), input.body.clone())],
        )
        .await
    }
}

impl GitHubPullRequestCli {
    /// `approve-workflows`: every fork workflow waiting on a maintainer, approved one by one,
    /// each only after the head is read again and found unchanged.
    async fn approve_workflows(&self, target: &ChangeRequestRef) -> CliResult<()> {
        let (owner, name) = parse_repository_selector(&target.repository);
        let detail = self.get_pull_request_detail(target.clone()).await?.detail;
        if detail.is_cross_repository != Some(true) {
            return Ok(());
        }
        let head_unavailable = || GitHubPullRequestCliError::WorkflowApprovalHeadUnavailable {
            cwd: target.cwd.clone(),
            number: target.number,
        };
        let (Some(expected_head_sha), Some(expected_owner)) = (detail.head_sha.clone(), detail.head_repository_owner.clone()) else {
            return Err(head_unavailable());
        };
        let expected_branch = detail.item.head_branch.clone();
        let runs = self
            .list_workflow_runs_requiring_approval(WorkflowApprovalInput {
                change_request: target.clone(),
                head_sha: expected_head_sha.clone(),
                head_branch: expected_branch.clone(),
                head_repository_owner: expected_owner.clone(),
            })
            .await?;
        for run in runs {
            let current = self.get_pull_request_detail(target.clone()).await?.detail;
            let (Some(current_sha), Some(current_owner)) = (current.head_sha.clone(), current.head_repository_owner.clone()) else {
                return Err(head_unavailable());
            };
            if current.is_cross_repository != Some(true)
                || current_sha != expected_head_sha
                || current.item.head_branch != expected_branch
                || current_owner.to_lowercase() != expected_owner.to_lowercase()
            {
                return Err(GitHubPullRequestCliError::WorkflowApprovalHeadChanged {
                    cwd: target.cwd.clone(),
                    number: target.number,
                });
            }
            let current_runs = self
                .list_workflow_runs_requiring_approval(WorkflowApprovalInput {
                    change_request: target.clone(),
                    head_sha: current_sha,
                    head_branch: current.item.head_branch.clone(),
                    head_repository_owner: current_owner,
                })
                .await?;
            if current_runs.iter().any(|current| current.id == run.id) {
                self.execute(GitHubExecuteInput::new(
                    &target.cwd,
                    [
                        "api".to_owned(),
                        "--method".into(),
                        "POST".into(),
                        "--hostname".into(),
                        target.host.clone(),
                        format!("repos/{owner}/{name}/actions/runs/{}/approve", run.id),
                        "--silent".into(),
                    ],
                ))
                .await?;
            }
        }
        Ok(())
    }
}
