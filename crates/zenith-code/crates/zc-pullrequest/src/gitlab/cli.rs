//! `pullRequest/GitLabPullRequestCli.ts`: every `glab` call of the pull request feature, through
//! zc-sourcecontrol's shared [`GitLabCli`]. Almost everything is `glab api <path>`: REST paths
//! with URL-encoded query strings, JSON bodies on stdin (`--input - --header Content-Type:
//! application/json`, never argv), and GraphQL for awards and blob ids; the merge request
//! actions glab has commands for go through `glab mr <subcommand>`.

use futures::{StreamExt, TryStreamExt};
use serde_json::{json, Map, Value};
use zc_contracts::{
    PullRequestAction, PullRequestComment, PullRequestCommit, PullRequestDiffFileContentsInputChangeType, PullRequestInvolvement, PullRequestListState,
    PullRequestMergeCapabilities, PullRequestMergeMethod, PullRequestReaction, PullRequestReactionContent, PullRequestReviewCommentDraft,
    PullRequestReviewPosition, PullRequestReviewThread, PullRequestReviewVerdict, PullRequestReviewerCandidateList,
};
use zc_core::vcs_process::VcsProcessOutput;
use zc_sourcecontrol::errors::{error_defect, Cause, CauseError};
use zc_sourcecontrol::github::cli::SchemaDecodeError;
use zc_sourcecontrol::gitlab::{GitLabCli, GitLabCliError, GitLabExecuteInput};
use zc_sourcecontrol::util::{encode_uri_component, js_trim};

use super::json::{
    decode_award_emoji_json, decode_commit_diff_refs_json, decode_commits_json, decode_diff_refs_json, decode_discussions_json,
    decode_merge_request_detail_json, decode_merge_request_diffs_json, decode_merge_request_list_json, decode_notes_json, decode_own_award_id_json,
    decode_project_merge_capabilities_json, decode_project_users_json, decode_repository_blobs_json, decode_viewer_json, gitlab_award_name, GitLabDiffRefs,
    GitLabMergeRequestDetail, GitLabMergeRequestListItem, GitLabProjectUsers, AWARD_EMOJI_GRAPHQL_QUERY, REPOSITORY_BLOBS_GRAPHQL_QUERY,
};
use super::util::{is_safe_integer, js_number_from_string, OrderedMap};
use crate::provider::ProviderListCursor;

/// GitLab's own ceiling on `per_page`, so a larger page has to be walked.
const MAX_PAGE_SIZE: i64 = 100;
/// Commit history is read one page deep.
const COMMIT_PAGE_SIZE: i64 = 100;
/// Pages of the conversation to follow before it is reported as truncated (a thousand notes).
const CONVERSATION_PAGES: i64 = 10;
const DIFF_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const DIFF_TIMEOUT_MS: u64 = 60_000;
const DIFF_FILE_MAX_OUTPUT_BYTES: usize = 1024 * 1024;
/// GitLab charges the blobs query by how many paths it is handed, and hands back one page.
const BLOB_PATHS_PER_REQUEST: usize = 100;

/// Why a diff file's contents cannot be expanded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileContentsUnavailableReason {
    Oversized,
    Binary,
}

impl FileContentsUnavailableReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Oversized => "oversized",
            Self::Binary => "binary",
        }
    }
}

/// `GitLabPullRequestCliError`: the shared CLI's errors plus this module's own.
#[derive(Debug, Clone)]
pub enum GitLabPullRequestCliError {
    /// `GitLabCli.GitLabCliError`.
    Cli(GitLabCliError),
    /// `GitLabMergeRequestReadError`: a read produced unusable output, named after the read.
    Read { cwd: String, operation: String, cause: Cause },
    /// `GitLabDiffCursorError`: a diff cursor this walk never handed out.
    DiffCursor { cwd: String },
    /// `GitLabDiffCommitError`: a commit that is not a sha.
    DiffCommit { cwd: String },
    /// `GitLabDiffCommitParentUnavailableError`.
    DiffCommitParentUnavailable { cwd: String, commit: String },
    /// `GitLabDiffFileContentsUnavailableError`.
    DiffFileContentsUnavailable {
        cwd: String,
        path: String,
        reason: FileContentsUnavailableReason,
    },
    /// `GitLabDiffRefsUnavailableError`: GitLab answered, the merge request has no revisions to
    /// place a comment against.
    DiffRefsUnavailable { cwd: String, number: i64 },
    /// `GitLabViewerUnavailableError`: the signed-in account has no username.
    ViewerUnavailable { cwd: String },
    /// An action GitLab does not declare (`revert`, `approve-workflows`). The TS throws a plain
    /// `Error` (a defect) here; the service never asks.
    UnsupportedAction { cwd: String, action: PullRequestAction },
}

impl From<GitLabCliError> for GitLabPullRequestCliError {
    fn from(error: GitLabCliError) -> Self {
        Self::Cli(error)
    }
}

impl GitLabPullRequestCliError {
    fn read(cwd: &str, operation: &str, cause: Cause) -> Self {
        Self::Read {
            cwd: cwd.to_owned(),
            operation: operation.to_owned(),
            cause,
        }
    }

    fn decode(cwd: &str, operation: &str, failure: String) -> Self {
        Self::read(cwd, operation, Cause::new(SchemaDecodeError(failure)))
    }

    /// The `_tag`.
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Cli(error) => error.tag(),
            Self::Read { .. } => "GitLabMergeRequestReadError",
            Self::DiffCursor { .. } => "GitLabDiffCursorError",
            Self::DiffCommit { .. } => "GitLabDiffCommitError",
            Self::DiffCommitParentUnavailable { .. } => "GitLabDiffCommitParentUnavailableError",
            Self::DiffFileContentsUnavailable { .. } => "GitLabDiffFileContentsUnavailableError",
            Self::DiffRefsUnavailable { .. } => "GitLabDiffRefsUnavailableError",
            Self::ViewerUnavailable { .. } => "GitLabViewerUnavailableError",
            Self::UnsupportedAction { .. } => "Error",
        }
    }

    /// The `detail` getter.
    pub fn detail(&self) -> String {
        match self {
            Self::Cli(error) => error.detail(),
            Self::Read { operation, .. } => format!("GitLab CLI returned an unreadable {operation} response."),
            Self::DiffCursor { .. } => "The diff cursor was not one this merge request handed out.".into(),
            Self::DiffCommit { .. } => "The named commit was not a commit sha.".into(),
            Self::DiffCommitParentUnavailable { commit, .. } => format!("Commit {commit} reported no parent revision."),
            Self::DiffFileContentsUnavailable { path, reason, .. } => match reason {
                FileContentsUnavailableReason::Oversized => format!("The diff file '{path}' exceeds the 1 MB expansion limit."),
                FileContentsUnavailableReason::Binary => format!("The diff file '{path}' is binary."),
            },
            Self::DiffRefsUnavailable { .. } => "The merge request reported no diff revisions.".into(),
            Self::ViewerUnavailable { .. } => "GitLab CLI returned no username for the authenticated account.".into(),
            Self::UnsupportedAction { action, .. } => format!("GitLab merge request action {} is unsupported", action.as_str()),
        }
    }

    /// The `message` getter.
    pub fn message(&self) -> String {
        match self {
            Self::Cli(error) => error.message(),
            Self::Read { operation, .. } => format!("GitLab CLI failed in {operation}: {}", self.detail()),
            Self::DiffCursor { .. } | Self::DiffCommit { .. } => format!("GitLab CLI failed in getMergeRequestDiff: {}", self.detail()),
            Self::DiffCommitParentUnavailable { .. } | Self::DiffFileContentsUnavailable { .. } => {
                format!("GitLab CLI failed in getMergeRequestDiffFileContents: {}", self.detail())
            }
            Self::DiffRefsUnavailable { .. } => format!("GitLab CLI failed in getDiffRefs: {}", self.detail()),
            Self::ViewerUnavailable { .. } => format!("GitLab CLI failed in getViewerUsername: {}", self.detail()),
            Self::UnsupportedAction { .. } => self.detail(),
        }
    }
}

impl std::fmt::Display for GitLabPullRequestCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for GitLabPullRequestCliError {}

impl CauseError for GitLabPullRequestCliError {
    fn defect(&self) -> Value {
        match self {
            Self::Cli(error) => error.defect(),
            Self::Read { cause, .. } => error_defect(self.tag(), self.message(), Some(cause.defect())),
            _ => error_defect(self.tag(), self.message(), None),
        }
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

type CliResult<T> = Result<T, GitLabPullRequestCliError>;

/// `GitLabMergeRequestListBatch` of the CLI.
#[derive(Debug, Clone, PartialEq)]
pub struct GitLabMergeRequestListBatch {
    pub items: Vec<GitLabMergeRequestListItem>,
    pub truncated: bool,
    /// Raw GitLab rows consumed to produce this page, including malformed rows.
    pub cursor_advance: i64,
}

/// `GitLabMergeRequestDiffSlice`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitLabMergeRequestDiffSlice {
    pub patch: String,
    /// Files in this slice had their hunks withheld, as opposed to there being more slices.
    pub truncated: bool,
    /// Where the next slice starts, or `None` once the patch is whole.
    pub next_cursor: Option<String>,
}

/// `listMergeRequests` input.
#[derive(Debug, Clone)]
pub struct ListMergeRequestsInput<'a> {
    pub cwd: &'a str,
    pub repository: &'a str,
    pub state: PullRequestListState,
    pub involvement: PullRequestInvolvement,
    pub viewer: &'a str,
    pub limit: i64,
    /// Free text for GitLab's own `search`, which matches title and description.
    pub query: Option<&'a str>,
    /// Where to carry on from in GitLab's stable update-ordered row set.
    pub cursor: Option<&'a ProviderListCursor>,
}

/// A merge request of a project, as most calls address it.
#[derive(Debug, Clone, Copy)]
pub struct MergeRequestTarget<'a> {
    pub cwd: &'a str,
    pub repository: &'a str,
    pub number: i64,
}

/// `getMergeRequestDiffFileContents` input.
#[derive(Debug, Clone)]
pub struct DiffFileContentsInput<'a> {
    pub target: MergeRequestTarget<'a>,
    pub commit: Option<&'a str>,
    pub change_type: PullRequestDiffFileContentsInputChangeType,
    pub old_path: &'a str,
    pub new_path: &'a str,
}

/// The awards on the merge request and on every note of it, keyed by the note's REST id.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GitLabReactions {
    pub reactions: Vec<PullRequestReaction>,
    pub reactions_by_note_id: OrderedMap<Vec<PullRequestReaction>>,
}

/// One `glab api` call.
#[derive(Debug, Clone, Default)]
struct ApiCall {
    path: String,
    method: Option<&'static str>,
    stdin: Option<String>,
    max_output_bytes: Option<usize>,
    timeout_ms: Option<u64>,
}

impl ApiCall {
    fn get(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }

    fn send(path: impl Into<String>, method: &'static str, body: Option<Value>) -> Self {
        Self {
            path: path.into(),
            method: Some(method),
            stdin: body.map(|body| body.to_string()),
            ..Self::default()
        }
    }
}

/// The REST API addresses a project by its URL-encoded full path.
fn project_path(repository: &str) -> String {
    encode_uri_component(js_trim(repository))
}

fn merge_request_path(repository: &str, number: i64) -> String {
    format!("projects/{}/merge_requests/{number}", project_path(repository))
}

/// `key=encodeURIComponent(value)` joined with `&`.
fn query(params: &[(&str, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{key}={}", encode_uri_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

/// GitLab's `closed` already excludes merged merge requests, and `all` spans every state.
fn state_param(state: PullRequestListState) -> &'static str {
    match state {
        PullRequestListState::Open => "opened",
        other => other.as_str(),
    }
}

/// The page a diff cursor names, or `None` for anything this walk cannot have issued (the cursor
/// goes straight into a query; the length bound keeps it out of exponential notation).
fn diff_cursor_page(cursor: &str) -> Option<i64> {
    let bytes = cursor.as_bytes();
    let valid = (1..=7).contains(&bytes.len()) && (b'1'..=b'9').contains(&bytes[0]) && bytes.iter().all(u8::is_ascii_digit);
    valid.then(|| cursor.parse().ok()).flatten()
}

/// A commit sha goes straight into a request path, so it is checked: hexadecimal only, 7 to 64.
fn is_commit_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn search_params(search: Option<&str>) -> Vec<(&'static str, String)> {
    let trimmed = js_trim(search.unwrap_or_default());
    if trimmed.is_empty() {
        Vec::new()
    } else {
        vec![("search", trimmed.to_owned())]
    }
}

fn involvement_params(involvement: PullRequestInvolvement, viewer: &str) -> Vec<(&'static str, String)> {
    match involvement {
        PullRequestInvolvement::Authored => vec![("author_username", viewer.to_owned())],
        PullRequestInvolvement::Reviewing => vec![("reviewer_username", viewer.to_owned())],
        PullRequestInvolvement::All => Vec::new(),
    }
}

fn merge_flags(merge_method: Option<PullRequestMergeMethod>) -> Vec<&'static str> {
    match merge_method {
        Some(PullRequestMergeMethod::Squash) => vec!["--squash"],
        Some(PullRequestMergeMethod::Rebase) => vec!["--rebase"],
        _ => Vec::new(),
    }
}

/// The `glab mr` subcommand and flags of an action; `None` for the ones it has no command for.
fn action_args(action: PullRequestAction, merge_method: Option<PullRequestMergeMethod>) -> Option<Vec<&'static str>> {
    let mut args = match action {
        // glab turns on auto-merge whenever a pipeline is running. The button means merge now.
        PullRequestAction::Merge => vec!["merge", "--auto-merge=false", "--yes"],
        // Here the wait is the whole point, so glab is told to arm the merge.
        PullRequestAction::EnableAutoMerge => vec!["merge", "--auto-merge=true", "--yes"],
        PullRequestAction::Ready => return Some(vec!["update", "--ready"]),
        PullRequestAction::Draft => return Some(vec!["update", "--draft"]),
        PullRequestAction::Close => return Some(vec!["close"]),
        // A rebase: GitLab has no merge-the-target-in equivalent of GitHub's update button.
        PullRequestAction::UpdateBranch => return Some(vec!["rebase"]),
        PullRequestAction::Reopen => return Some(vec!["reopen"]),
        // Taking the arming back has no `glab mr` command (it goes to the API); the others are
        // not declared by this host.
        PullRequestAction::DisableAutoMerge | PullRequestAction::Revert | PullRequestAction::ApproveWorkflows => return None,
    };
    args.extend(merge_flags(merge_method));
    Some(args)
}

/// The position lines of a review comment, as GitLab names them.
fn review_position_lines(position: &PullRequestReviewPosition, into: &mut Map<String, Value>) {
    match position {
        PullRequestReviewPosition::Added(added) => {
            into.insert("new_line".into(), json!(added.new_line));
        }
        PullRequestReviewPosition::Deleted(deleted) => {
            into.insert("old_line".into(), json!(deleted.old_line));
        }
        PullRequestReviewPosition::Context(context) => {
            into.insert("old_line".into(), json!(context.old_line));
            into.insert("new_line".into(), json!(context.new_line));
        }
    }
}

/// The `GitLabPullRequestCli` service.
#[derive(Clone)]
pub struct GitLabPullRequestCli {
    gitlab: GitLabCli,
}

impl GitLabPullRequestCli {
    pub fn new(gitlab: GitLabCli) -> Self {
        Self { gitlab }
    }

    async fn api(&self, cwd: &str, call: ApiCall) -> CliResult<VcsProcessOutput> {
        let mut args = vec!["api".to_owned(), call.path];
        if let Some(method) = call.method {
            args.extend(["--method".to_owned(), method.to_owned()]);
        }
        // A raw body from stdin: argv is visible in process listings and echoed in failures.
        // `glab api --input` sends no Content-Type, and GitLab answers that with HTTP 415.
        if call.stdin.is_some() {
            args.extend(["--input", "-", "--header", "Content-Type: application/json"].map(String::from));
        }
        let mut input = GitLabExecuteInput::new(cwd, args);
        input.stdin = call.stdin;
        input.max_output_bytes = call.max_output_bytes;
        input.timeout_ms = call.timeout_ms;
        Ok(self.gitlab.execute(input).await?)
    }

    /// `getViewerUsername`.
    pub async fn get_viewer_username(&self, cwd: &str) -> CliResult<String> {
        let output = self.api(cwd, ApiCall::get("user")).await?;
        match decode_viewer_json(js_trim(&output.stdout)) {
            Err(failure) => Err(GitLabPullRequestCliError::decode(cwd, "getViewerUsername", failure)),
            Ok(None) => Err(GitLabPullRequestCliError::ViewerUnavailable { cwd: cwd.to_owned() }),
            Ok(Some(username)) => Ok(username),
        }
    }

    /// `listMergeRequests`. `per_page` stops at 100, so a larger page is walked; the walk stops on
    /// a short page, once the extra row that reveals a next page has been read, or after the raw
    /// span the caller's page needs (which ends it when every row fails to decode).
    ///
    /// A continuation uses GitLab's offset pagination: its timestamp filter is inclusive with no
    /// tie-breaker, so `delivered` is the stable offset already handed over.
    pub async fn list_merge_requests(&self, input: ListMergeRequestsInput<'_>) -> CliResult<GitLabMergeRequestListBatch> {
        let delivered = input.cursor.map_or(0, |cursor| cursor.delivered);
        let per_page = (input.limit + 1).min(MAX_PAGE_SIZE);
        let first_page = delivered.div_euclid(per_page) + 1;
        let last_page = (delivered + input.limit).div_euclid(per_page) + 1;
        let mut page = first_page;
        let mut collected: Vec<GitLabMergeRequestListItem> = Vec::new();
        let mut cursor_advance = 0;
        loop {
            let skip_on_first_page = if page == first_page { (delivered % per_page) as usize } else { 0 };
            let mut params = vec![("state", state_param(input.state).to_owned())];
            params.extend(involvement_params(input.involvement, input.viewer));
            // The REST API's own `search` (what `mr list --search` passes on), URL-encoded like
            // every other value, so no text in it can become a parameter of its own.
            params.extend(search_params(input.query));
            params.extend([
                ("order_by", "updated_at".to_owned()),
                ("sort", "desc".to_owned()),
                ("per_page", per_page.to_string()),
                ("page", page.to_string()),
            ]);
            let path = format!("projects/{}/merge_requests?{}", project_path(input.repository), query(&params));
            let output = self.api(input.cwd, ApiCall::get(path)).await?;
            let raw = js_trim(&output.stdout);
            if raw.is_empty() {
                return Ok(GitLabMergeRequestListBatch {
                    items: collected,
                    truncated: false,
                    cursor_advance,
                });
            }
            let decoded = decode_merge_request_list_json(raw).map_err(|failure| GitLabPullRequestCliError::decode(input.cwd, "listMergeRequests", failure))?;
            let mut page_items = Vec::new();
            let mut page_raw_indexes = Vec::new();
            for (item, raw_index) in decoded.items.into_iter().zip(decoded.raw_indexes) {
                if raw_index < skip_on_first_page {
                    continue;
                }
                page_items.push(item);
                page_raw_indexes.push(raw_index);
            }
            let remaining = input.limit - collected.len() as i64;
            let last_item_raw_index = if remaining >= 1 {
                page_raw_indexes.get((remaining - 1) as usize).copied()
            } else {
                None
            };
            if let Some(last_item_raw_index) = last_item_raw_index {
                let consumed = (last_item_raw_index + 1 - skip_on_first_page) as i64;
                collected.extend(page_items.into_iter().take(remaining as usize));
                return Ok(GitLabMergeRequestListBatch {
                    items: collected,
                    truncated: last_item_raw_index + 1 < decoded.raw_count || decoded.raw_count as i64 == per_page,
                    cursor_advance: cursor_advance + consumed,
                });
            }
            collected.extend(page_items);
            let consumed = (decoded.raw_count as i64 - skip_on_first_page as i64).max(0);
            // Counted before decoding, so a skipped malformed row cannot end paging early.
            if (decoded.raw_count as i64) < per_page {
                return Ok(GitLabMergeRequestListBatch {
                    items: collected,
                    truncated: false,
                    cursor_advance: cursor_advance + consumed,
                });
            }
            if page >= last_page {
                return Ok(GitLabMergeRequestListBatch {
                    items: collected,
                    truncated: true,
                    cursor_advance: cursor_advance + consumed,
                });
            }
            page += 1;
            cursor_advance += consumed;
        }
    }

    /// `getMergeRequestDetail`: asks for the divergence GitLab withholds by default, on the same read.
    pub async fn get_merge_request_detail(&self, target: MergeRequestTarget<'_>) -> CliResult<GitLabMergeRequestDetail> {
        let path = format!(
            "{}?{}",
            merge_request_path(target.repository, target.number),
            query(&[("include_diverged_commits_count", "true".into())])
        );
        let output = self.api(target.cwd, ApiCall::get(path)).await?;
        decode_merge_request_detail_json(js_trim(&output.stdout))
            .map_err(|failure| GitLabPullRequestCliError::decode(target.cwd, "getMergeRequestDetail", failure))
    }

    /// `listNotes`: the conversation a page at a time, until a short page (by raw count: a full
    /// page of GitLab's own activity notes still means more) or the page bound.
    pub async fn list_notes(&self, target: MergeRequestTarget<'_>) -> CliResult<(Vec<PullRequestComment>, bool)> {
        let mut comments = Vec::new();
        let mut page = 1;
        loop {
            let path = format!(
                "{}/notes?{}",
                merge_request_path(target.repository, target.number),
                query(&[
                    ("per_page", MAX_PAGE_SIZE.to_string()),
                    ("page", page.to_string()),
                    ("order_by", "created_at".into()),
                    ("sort", "asc".into()),
                ])
            );
            let output = self.api(target.cwd, ApiCall::get(path)).await?;
            let decoded = decode_notes_json(js_trim(&output.stdout)).map_err(|failure| GitLabPullRequestCliError::decode(target.cwd, "listNotes", failure))?;
            comments.extend(decoded.comments);
            if (decoded.raw_count as i64) < MAX_PAGE_SIZE {
                return Ok((comments, false));
            }
            if page >= CONVERSATION_PAGES {
                return Ok((comments, true));
            }
            page += 1;
        }
    }

    /// `listDiscussions`: the positioned discussions, walked the same way (the raw count again:
    /// a full page of plain notes is not the end of the positioned ones).
    pub async fn list_discussions(&self, target: MergeRequestTarget<'_>) -> CliResult<(Vec<PullRequestReviewThread>, bool)> {
        let mut threads = Vec::new();
        let mut page = 1;
        loop {
            let path = format!(
                "{}/discussions?{}",
                merge_request_path(target.repository, target.number),
                query(&[("per_page", MAX_PAGE_SIZE.to_string()), ("page", page.to_string())])
            );
            let output = self.api(target.cwd, ApiCall::get(path)).await?;
            let decoded = decode_discussions_json(js_trim(&output.stdout))
                .map_err(|failure| GitLabPullRequestCliError::decode(target.cwd, "listDiscussions", failure))?;
            threads.extend(decoded.threads);
            if (decoded.raw_count as i64) < MAX_PAGE_SIZE {
                return Ok((threads, false));
            }
            if page >= CONVERSATION_PAGES {
                return Ok((threads, true));
            }
            page += 1;
        }
    }

    /// `listCommits`: one page, with stats.
    pub async fn list_commits(&self, target: MergeRequestTarget<'_>) -> CliResult<Vec<PullRequestCommit>> {
        let path = format!(
            "{}/commits?{}",
            merge_request_path(target.repository, target.number),
            query(&[("per_page", COMMIT_PAGE_SIZE.to_string()), ("with_stats", "true".into())])
        );
        let output = self.api(target.cwd, ApiCall::get(path)).await?;
        decode_commits_json(js_trim(&output.stdout)).map_err(|failure| GitLabPullRequestCliError::decode(target.cwd, "listCommits", failure))
    }

    /// One page of a merge request's files (or of one commit's), as a patch that stands on its
    /// own. GitLab pages `/diffs` by offset with no cursor of its own, so the page number is it.
    async fn diff_page(&self, target: MergeRequestTarget<'_>, page: i64, commit: Option<&str>) -> CliResult<GitLabMergeRequestDiffSlice> {
        let scope = match commit {
            None => format!("merge_requests/{}/diffs", target.number),
            Some(commit) => format!("repository/commits/{commit}/diff"),
        };
        let path = format!(
            "projects/{}/{scope}?{}",
            project_path(target.repository),
            query(&[("per_page", MAX_PAGE_SIZE.to_string()), ("page", page.to_string())])
        );
        let output = self
            .api(
                target.cwd,
                ApiCall {
                    max_output_bytes: Some(DIFF_MAX_OUTPUT_BYTES),
                    timeout_ms: Some(DIFF_TIMEOUT_MS),
                    ..ApiCall::get(path)
                },
            )
            .await?;
        // A byte-truncated response is a JSON prefix: answering with no cursor would call the
        // diff whole while dropping this page and every one after it.
        if output.stdout_truncated {
            return Err(GitLabPullRequestCliError::read(
                target.cwd,
                "getMergeRequestDiff",
                Cause::message(format!("Page {page} of the merge request diff was too large to read.")),
            ));
        }
        let decoded = decode_merge_request_diffs_json(js_trim(&output.stdout))
            .map_err(|failure| GitLabPullRequestCliError::decode(target.cwd, "getMergeRequestDiff", failure))?;
        // Counted before decoding, so a page whose files all failed to decode still moves on.
        let more_pages = decoded.raw_count as i64 >= MAX_PAGE_SIZE;
        Ok(GitLabMergeRequestDiffSlice {
            // Ends on a newline, so a header-only file does not run into the next slice.
            patch: if decoded.patch.is_empty() {
                decoded.patch
            } else {
                super::util::ensure_trailing_newline(&decoded.patch)
            },
            truncated: decoded.truncated,
            next_cursor: more_pages.then(|| (page + 1).to_string()),
        })
    }

    /// `getMergeRequestDiff`: absent cursor is the first slice; a named commit is read from its
    /// own diff, which pages the same way.
    pub async fn get_merge_request_diff(
        &self,
        target: MergeRequestTarget<'_>,
        cursor: Option<&str>,
        commit: Option<&str>,
    ) -> CliResult<GitLabMergeRequestDiffSlice> {
        if commit.is_some_and(|commit| !is_commit_sha(commit)) {
            return Err(GitLabPullRequestCliError::DiffCommit { cwd: target.cwd.to_owned() });
        }
        let page = match cursor {
            None => 1,
            Some(cursor) => diff_cursor_page(cursor).ok_or_else(|| GitLabPullRequestCliError::DiffCursor { cwd: target.cwd.to_owned() })?,
        };
        self.diff_page(target, page, commit).await
    }

    /// The revisions a positioned comment is written against (`getDiffRefs`).
    async fn get_diff_refs(&self, target: MergeRequestTarget<'_>) -> CliResult<GitLabDiffRefs> {
        let output = self.api(target.cwd, ApiCall::get(merge_request_path(target.repository, target.number))).await?;
        match decode_diff_refs_json(js_trim(&output.stdout)) {
            Err(failure) => Err(GitLabPullRequestCliError::decode(target.cwd, "getDiffRefs", failure)),
            // A well-formed answer that cannot carry a positioned comment.
            Ok(None) => Err(GitLabPullRequestCliError::DiffRefsUnavailable {
                cwd: target.cwd.to_owned(),
                number: target.number,
            }),
            Ok(Some(refs)) => Ok(refs),
        }
    }

    async fn get_commit_diff_refs(&self, cwd: &str, repository: &str, commit: &str, allow_root: bool) -> CliResult<GitLabDiffRefs> {
        let path = format!("projects/{}/repository/commits/{commit}", project_path(repository));
        let output = self.api(cwd, ApiCall::get(path)).await?;
        match decode_commit_diff_refs_json(js_trim(&output.stdout)) {
            Err(failure) => Err(GitLabPullRequestCliError::decode(cwd, "getMergeRequestDiffFileContents", failure)),
            Ok(None) if allow_root => Ok(GitLabDiffRefs {
                base_sha: String::new(),
                head_sha: commit.to_owned(),
                start_sha: String::new(),
            }),
            Ok(None) => Err(GitLabPullRequestCliError::DiffCommitParentUnavailable {
                cwd: cwd.to_owned(),
                commit: commit.to_owned(),
            }),
            Ok(Some(refs)) => Ok(refs),
        }
    }

    async fn read_file(&self, target: MergeRequestTarget<'_>, revision: &str, file_path: &str) -> CliResult<String> {
        let path = format!(
            "projects/{}/repository/files/{}/raw?ref={}",
            project_path(target.repository),
            encode_uri_component(file_path),
            encode_uri_component(revision)
        );
        let output = self
            .api(
                target.cwd,
                ApiCall {
                    max_output_bytes: Some(DIFF_FILE_MAX_OUTPUT_BYTES),
                    timeout_ms: Some(DIFF_TIMEOUT_MS),
                    ..ApiCall::get(path)
                },
            )
            .await?;
        if output.stdout_truncated || output.stdout.contains('\0') || output.stdout_invalid_utf8 {
            return Err(GitLabPullRequestCliError::DiffFileContentsUnavailable {
                cwd: target.cwd.to_owned(),
                path: file_path.to_owned(),
                reason: if output.stdout_truncated {
                    FileContentsUnavailableReason::Oversized
                } else {
                    FileContentsUnavailableReason::Binary
                },
            });
        }
        Ok(output.stdout)
    }

    /// `getMergeRequestDiffFileContents`: both sides of one file, read at the merge request's (or
    /// the commit's) revisions.
    pub async fn get_merge_request_diff_file_contents(&self, input: DiffFileContentsInput<'_>) -> CliResult<(String, String)> {
        let target = input.target;
        if input.commit.is_some_and(|commit| !is_commit_sha(commit)) {
            return Err(GitLabPullRequestCliError::DiffCommit { cwd: target.cwd.to_owned() });
        }
        let refs = match input.commit {
            None => self.get_diff_refs(target).await?,
            Some(commit) => {
                self.get_commit_diff_refs(
                    target.cwd,
                    target.repository,
                    commit,
                    input.change_type == PullRequestDiffFileContentsInputChangeType::New,
                )
                .await?
            }
        };
        let old = async {
            if input.change_type == PullRequestDiffFileContentsInputChangeType::New {
                Ok(String::new())
            } else {
                self.read_file(target, &refs.base_sha, input.old_path).await
            }
        };
        let new = async {
            if input.change_type == PullRequestDiffFileContentsInputChangeType::Deleted {
                Ok(String::new())
            } else {
                self.read_file(target, &refs.head_sha, input.new_path).await
            }
        };
        futures::try_join!(old, new)
    }

    /// `getProjectMergeCapabilities`.
    pub async fn get_project_merge_capabilities(&self, cwd: &str, repository: &str) -> CliResult<PullRequestMergeCapabilities> {
        let output = self
            .api(cwd, ApiCall::get(format!("projects/{}?license=false", project_path(repository))))
            .await?;
        decode_project_merge_capabilities_json(js_trim(&output.stdout))
            .map_err(|failure| GitLabPullRequestCliError::decode(cwd, "getProjectMergeCapabilities", failure))
    }

    async fn blobs_at(&self, cwd: &str, repository: &str, reference: &str, paths: &[String]) -> CliResult<Option<OrderedMap<String>>> {
        let body = json!({
            "query": REPOSITORY_BLOBS_GRAPHQL_QUERY,
            "variables": {"fullPath": repository, "ref": reference, "paths": paths},
        });
        let output = self.api(cwd, ApiCall::send("graphql", "POST", Some(body))).await?;
        decode_repository_blobs_json(js_trim(&output.stdout)).map_err(|failure| GitLabPullRequestCliError::decode(cwd, "getFileRevisions", failure))
    }

    /// `getFileRevisions`: what the merge request's head (from its own diff refs, the version the
    /// reader sees) has of each path, as blob ids. Within an answered batch a missing path is the
    /// empty revision (the merge request removed it); a batch GitLab did not answer is left out.
    pub async fn get_file_revisions(&self, target: MergeRequestTarget<'_>, paths: &[String]) -> CliResult<OrderedMap<String>> {
        if paths.is_empty() {
            return Ok(OrderedMap::new());
        }
        let refs = self.get_diff_refs(target).await?;
        let head_sha = refs.head_sha.as_str();
        // Two batches in flight at a time, answered in order (the futures are built up front so
        // the stream holds no borrowing closure).
        let reads: Vec<_> = paths
            .chunks(BLOB_PATHS_PER_REQUEST)
            .map(|batch| self.blobs_at(target.cwd, target.repository, head_sha, batch))
            .collect();
        let answers: Vec<Option<OrderedMap<String>>> = futures::stream::iter(reads).buffered(2).try_collect().await?;
        let pages = paths.chunks(BLOB_PATHS_PER_REQUEST).zip(answers);
        let mut revisions = OrderedMap::new();
        for (batch, page) in pages {
            let Some(page) = page else { continue };
            for (path, oid) in page.into_entries() {
                revisions.set(path, oid);
            }
            for path in batch {
                if !revisions.has(path) {
                    revisions.set(path.clone(), String::new());
                }
            }
        }
        Ok(revisions)
    }

    async fn project_users(&self, cwd: &str, repository: &str) -> CliResult<GitLabProjectUsers> {
        let path = format!(
            "projects/{}/users?{}",
            project_path(repository),
            query(&[("per_page", MAX_PAGE_SIZE.to_string())])
        );
        let output = self.api(cwd, ApiCall::get(path)).await?;
        decode_project_users_json(js_trim(&output.stdout)).map_err(|failure| GitLabPullRequestCliError::decode(cwd, "listReviewerCandidates", failure))
    }

    /// `listReviewerCandidates`: who has access to the project, and who the merge request already
    /// asks. The author is dropped: GitLab refuses to make them a reviewer.
    pub async fn list_reviewer_candidates(&self, target: MergeRequestTarget<'_>) -> CliResult<PullRequestReviewerCandidateList> {
        let (merge_request, users) = futures::try_join!(self.get_merge_request_detail(target), self.project_users(target.cwd, target.repository))?;
        let author = merge_request.item.author.as_ref().map(|author| author.login.clone());
        let requested = &merge_request.item.review_request_logins;
        Ok(PullRequestReviewerCandidateList {
            candidates: users
                .candidates
                .into_iter()
                .filter(|candidate| author.as_deref() != Some(candidate.login.as_str()))
                .map(|mut candidate| {
                    candidate.is_requested = requested.contains(&candidate.login);
                    candidate
                })
                .collect(),
            truncated: users.raw_count as i64 >= MAX_PAGE_SIZE,
        })
    }

    /// `setReviewerRequest`: `reviewer_ids` replaces the whole set, so the set already there is
    /// read and the change applied to it (asking again writes the same set back, which is how
    /// GitLab re-requests a review). An id GitLab could not have handed out is ignored.
    pub async fn set_reviewer_request(&self, target: MergeRequestTarget<'_>, reviewer_ids: &[String], requested: bool) -> CliResult<()> {
        let merge_request = self.get_merge_request_detail(target).await?;
        let mut ids: Vec<i64> = Vec::new();
        for id in merge_request.reviewer_ids {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        for reviewer in reviewer_ids {
            let id = js_number_from_string(reviewer);
            if !is_safe_integer(id) || id <= 0.0 {
                continue;
            }
            let id = id as i64;
            if requested {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            } else {
                ids.retain(|existing| *existing != id);
            }
        }
        self.api(
            target.cwd,
            ApiCall::send(merge_request_path(target.repository, target.number), "PUT", Some(json!({"reviewer_ids": ids}))),
        )
        .await
        .map(drop)
    }

    /// `runMergeRequestAction`. Disarming auto-merge has no `glab mr` flag, so it is asked of the
    /// API; everything else is `glab mr <subcommand> <number> --repo <repository> …`.
    pub async fn run_merge_request_action(
        &self,
        target: MergeRequestTarget<'_>,
        action: PullRequestAction,
        merge_method: Option<PullRequestMergeMethod>,
    ) -> CliResult<()> {
        if action == PullRequestAction::DisableAutoMerge {
            let path = format!("{}/cancel_merge_when_pipeline_succeeds", merge_request_path(target.repository, target.number));
            return self.api(target.cwd, ApiCall::send(path, "POST", None)).await.map(drop);
        }
        let Some(args) = action_args(action, merge_method) else {
            return Err(GitLabPullRequestCliError::UnsupportedAction {
                cwd: target.cwd.to_owned(),
                action,
            });
        };
        let (subcommand, flags) = args.split_first().expect("every action has a subcommand");
        let mut argv = vec![
            "mr".to_owned(),
            (*subcommand).to_owned(),
            target.number.to_string(),
            "--repo".into(),
            target.repository.to_owned(),
        ];
        argv.extend(flags.iter().map(|flag| (*flag).to_owned()));
        self.gitlab
            .execute(GitLabExecuteInput::new(target.cwd, argv))
            .await
            .map(drop)
            .map_err(Into::into)
    }

    /// `updateMergeRequest`: only the fields asked to change (GitLab clears a field sent empty).
    /// GitLab calls a merge request's body its description.
    pub async fn update_merge_request(&self, target: MergeRequestTarget<'_>, title: Option<&str>, description: Option<&str>) -> CliResult<()> {
        let mut body = Map::new();
        if let Some(title) = title {
            body.insert("title".into(), json!(title));
        }
        if let Some(description) = description {
            body.insert("description".into(), json!(description));
        }
        self.api(
            target.cwd,
            ApiCall::send(merge_request_path(target.repository, target.number), "PUT", Some(Value::Object(body))),
        )
        .await
        .map(drop)
    }

    /// `commentOnMergeRequest`: a JSON body rather than `--raw-field`, which glab coerces when it
    /// reads as `true` or a number.
    pub async fn comment_on_merge_request(&self, target: MergeRequestTarget<'_>, body: &str) -> CliResult<()> {
        let path = format!("{}/notes", merge_request_path(target.repository, target.number));
        self.api(target.cwd, ApiCall::send(path, "POST", Some(json!({"body": body})))).await.map(drop)
    }

    /// `updateNote`.
    pub async fn update_note(&self, target: MergeRequestTarget<'_>, note_id: &str, body: &str) -> CliResult<()> {
        let path = format!(
            "{}/notes/{}",
            merge_request_path(target.repository, target.number),
            encode_uri_component(note_id)
        );
        self.api(target.cwd, ApiCall::send(path, "PUT", Some(json!({"body": body})))).await.map(drop)
    }

    /// `submitReview`: GitLab has no pending review, so a review is replayed as its requests: the
    /// line comments, then the summary, then the verdict (last, so a half-sent review is never an
    /// approval).
    pub async fn submit_review(
        &self,
        target: MergeRequestTarget<'_>,
        verdict: PullRequestReviewVerdict,
        body: &str,
        comments: &[PullRequestReviewCommentDraft],
    ) -> CliResult<()> {
        let merge_request = merge_request_path(target.repository, target.number);
        if !comments.is_empty() {
            let refs = self.get_diff_refs(target).await?;
            for comment in comments {
                let mut position = Map::new();
                position.insert("base_sha".into(), json!(refs.base_sha));
                position.insert("head_sha".into(), json!(refs.head_sha));
                position.insert("start_sha".into(), json!(refs.start_sha));
                position.insert("position_type".into(), json!("text"));
                // Both paths, since GitLab resolves a position against both sides; they differ
                // only for a renamed file.
                position.insert("old_path".into(), json!(comment.old_path.as_deref().unwrap_or(&comment.path)));
                position.insert("new_path".into(), json!(comment.path));
                review_position_lines(&comment.position, &mut position);
                let request = json!({"body": comment.body, "position": Value::Object(position)});
                self.api(target.cwd, ApiCall::send(format!("{merge_request}/discussions"), "POST", Some(request)))
                    .await?;
            }
        }
        if !js_trim(body).is_empty() {
            self.api(target.cwd, ApiCall::send(format!("{merge_request}/notes"), "POST", Some(json!({"body": body}))))
                .await?;
        }
        if verdict == PullRequestReviewVerdict::Approve {
            self.api(target.cwd, ApiCall::send(format!("{merge_request}/approve"), "POST", None)).await?;
        }
        Ok(())
    }

    /// `replyToDiscussion`.
    pub async fn reply_to_discussion(&self, target: MergeRequestTarget<'_>, discussion_id: &str, body: &str) -> CliResult<()> {
        let path = format!(
            "{}/discussions/{}/notes",
            merge_request_path(target.repository, target.number),
            encode_uri_component(discussion_id)
        );
        self.api(target.cwd, ApiCall::send(path, "POST", Some(json!({"body": body})))).await.map(drop)
    }

    /// `setDiscussionResolution`.
    pub async fn set_discussion_resolution(&self, target: MergeRequestTarget<'_>, discussion_id: &str, resolved: bool) -> CliResult<()> {
        let path = format!(
            "{}/discussions/{}",
            merge_request_path(target.repository, target.number),
            encode_uri_component(discussion_id)
        );
        self.api(target.cwd, ApiCall::send(path, "PUT", Some(json!({"resolved": resolved}))))
            .await
            .map(drop)
    }

    /// `listReactions`: the awards on the merge request and its notes, a page of notes at a time,
    /// bounded like the conversation itself.
    pub async fn list_reactions(&self, target: MergeRequestTarget<'_>) -> CliResult<GitLabReactions> {
        let mut collected: Option<GitLabReactions> = None;
        let mut cursor: Option<String> = None;
        let mut page = 1;
        loop {
            let body = json!({
                "query": AWARD_EMOJI_GRAPHQL_QUERY,
                "variables": {"fullPath": target.repository, "iid": target.number.to_string(), "cursor": cursor},
            });
            let output = self.api(target.cwd, ApiCall::send("graphql", "POST", Some(body))).await?;
            let decoded =
                decode_award_emoji_json(js_trim(&output.stdout)).map_err(|failure| GitLabPullRequestCliError::decode(target.cwd, "listReactions", failure))?;
            let reactions = collected.get_or_insert_with(|| GitLabReactions {
                reactions: decoded.reactions.clone(),
                reactions_by_note_id: OrderedMap::new(),
            });
            for (id, note_reactions) in decoded.reactions_by_note_id.into_entries() {
                reactions.reactions_by_note_id.set(id, note_reactions);
            }
            match decoded.next_cursor {
                Some(next) if page < CONVERSATION_PAGES => {
                    cursor = Some(next);
                    page += 1;
                }
                _ => return Ok(collected.unwrap_or_default()),
            }
        }
    }

    fn award_subject_path(repository: &str, number: i64, note_id: Option<&str>) -> String {
        let merge_request = merge_request_path(repository, number);
        match note_id {
            None => format!("{merge_request}/award_emoji"),
            Some(note_id) => format!("{merge_request}/notes/{}/award_emoji", encode_uri_component(note_id)),
        }
    }

    /// `setReaction`: awards an emoji (on a note, or the merge request itself), or takes the
    /// reader's own award back by its id, looked up first. Nothing to delete is success.
    pub async fn set_reaction(
        &self,
        target: MergeRequestTarget<'_>,
        note_id: Option<&str>,
        content: PullRequestReactionContent,
        reacted: bool,
    ) -> CliResult<()> {
        let subject = Self::award_subject_path(target.repository, target.number, note_id);
        if reacted {
            let path = format!("{subject}?{}", query(&[("name", gitlab_award_name(content).to_owned())]));
            return self.api(target.cwd, ApiCall::send(path, "POST", None)).await.map(drop);
        }
        let viewer = self.get_viewer_username(target.cwd).await?;
        let listed = self.api(target.cwd, ApiCall::get(subject.clone())).await?;
        let own = decode_own_award_id_json(js_trim(&listed.stdout), content, &viewer)
            .map_err(|failure| GitLabPullRequestCliError::decode(target.cwd, "setReaction", failure))?;
        let Some(own) = own else { return Ok(()) };
        self.api(target.cwd, ApiCall::send(format!("{subject}/{own}"), "DELETE", None)).await.map(drop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_cursors_it_could_have_issued() {
        assert_eq!(diff_cursor_page("2"), Some(2));
        assert_eq!(diff_cursor_page("9999999"), Some(9_999_999));
        assert_eq!(diff_cursor_page("10000000"), None);
        assert_eq!(diff_cursor_page("0"), None);
        assert_eq!(diff_cursor_page("1&per_page=1"), None);
        assert_eq!(diff_cursor_page(""), None);
    }

    #[test]
    fn checks_commit_shas() {
        assert!(is_commit_sha("a1b2c3d"));
        assert!(is_commit_sha("A1B2C3D4E5F6A7B8C9D0E1F2A3B4C5D6E7F8A9B0"));
        assert!(!is_commit_sha("a1b2c3"));
        assert!(!is_commit_sha("../../merge_requests/8/diffs"));
    }
}
