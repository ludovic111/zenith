//! `pullRequest/BitbucketPullRequestApi.ts`: pull requests over Bitbucket Cloud's REST API,
//! through zc-sourcecontrol's [`BitbucketApi`] (credentials, trusted origin, redirects, body
//! bound, `Retry-After`). Every request goes through [`BitbucketRequester`], which tests replace
//! the way the TS tests mock `BitbucketApi.request`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::{BoxFuture, FutureExt, Shared};
use serde_json::{json, Value};
use zc_contracts::{
    PullRequestAction, PullRequestCheck, PullRequestComment, PullRequestCommit, PullRequestDiffSide, PullRequestListState, PullRequestMergeMethod,
    PullRequestMergeability, PullRequestReviewCommentDraft, PullRequestReviewPosition, PullRequestReviewThread, PullRequestReviewVerdict,
    PullRequestReviewerCandidate, PullRequestReviewerCandidateList,
};
use zc_sourcecontrol::bitbucket::api::{BitbucketRequest, BitbucketResponseBody};
use zc_sourcecontrol::bitbucket::{BitbucketApi, BitbucketApiError};
use zc_sourcecontrol::errors::{error_defect, Cause, CauseError};
use zc_sourcecontrol::util::{encode_uri_component, js_trim, SharedClock};

use super::diff_revisions::{parse_diff_file_revisions, OrderedRevisions};
use super::json::{
    build_review_threads, decode_comments_json, decode_commits_json, decode_conflicts_json, decode_diffstat_json, decode_pull_request_json,
    decode_pull_request_page_json, decode_repository_permission_json, decode_statuses_json, decode_viewer_json, decode_workspace_members_json,
    BitbucketDiffStat, BitbucketPage, BitbucketPullRequest, DecodeFailure,
};
use crate::provider::ProviderListCursor;

/// The one call the pull request API makes: `BitbucketApi.request`.
#[async_trait]
pub trait BitbucketRequester: Send + Sync {
    async fn request(&self, input: BitbucketRequest) -> Result<BitbucketResponseBody, BitbucketApiError>;
}

#[async_trait]
impl BitbucketRequester for BitbucketApi {
    async fn request(&self, input: BitbucketRequest) -> Result<BitbucketResponseBody, BitbucketApiError> {
        BitbucketApi::request(self, input).await
    }
}

/// `BitbucketPullRequestApiError`: the source control client's errors, plus the four this API
/// adds.
#[derive(Debug, Clone)]
pub enum BitbucketPullRequestApiError {
    Api(BitbucketApiError),
    /// `BitbucketPullRequestReadError`: names the read that produced unusable output, so a failure
    /// reports the call it came from rather than borrowing another operation's message.
    Read {
        operation: String,
        cause: Cause,
    },
    /// `BitbucketViewerUnavailableError`: Bitbucket answered, the account it answered for just
    /// has no handle.
    ViewerUnavailable,
    /// `BitbucketRepositoryUnsupportedError`: a repository that is not `workspace/slug`, the only
    /// form Bitbucket addresses.
    RepositoryUnsupported {
        repository: String,
    },
    /// `BitbucketDiffCommitError`: the reader named a commit that is not a sha.
    DiffCommit,
}

impl From<BitbucketApiError> for BitbucketPullRequestApiError {
    fn from(error: BitbucketApiError) -> Self {
        Self::Api(error)
    }
}

impl BitbucketPullRequestApiError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Api(error) => error.tag(),
            Self::Read { .. } => "BitbucketPullRequestReadError",
            Self::ViewerUnavailable => "BitbucketViewerUnavailableError",
            Self::RepositoryUnsupported { .. } => "BitbucketRepositoryUnsupportedError",
            Self::DiffCommit => "BitbucketDiffCommitError",
        }
    }

    /// The fact the error states, without the operation around it.
    pub fn detail(&self) -> String {
        match self {
            Self::Api(error) => error.detail(),
            Self::Read { operation, .. } => format!("Bitbucket returned an unreadable {operation} response."),
            Self::ViewerUnavailable => "Bitbucket returned no account name for the configured credentials.".into(),
            Self::RepositoryUnsupported { .. } => "A Bitbucket repository is addressed as workspace/repository.".into(),
            Self::DiffCommit => "The named commit was not a commit sha.".into(),
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::Api(error) => error.message(),
            Self::Read { operation, .. } => format!("Bitbucket failed in {operation}: {}", self.detail()),
            Self::ViewerUnavailable => format!("Bitbucket failed in getViewer: {}", self.detail()),
            Self::RepositoryUnsupported { .. } => format!("Bitbucket failed in resolveRepository: {}", self.detail()),
            Self::DiffCommit => format!("Bitbucket failed in getPullRequestDiff: {}", self.detail()),
        }
    }

    /// The source control client's error, when this is one.
    pub fn api(&self) -> Option<&BitbucketApiError> {
        match self {
            Self::Api(error) => Some(error),
            _ => None,
        }
    }

    /// The status of `BitbucketResponseError` / `BitbucketResponseBodyReadError`.
    fn response_status(&self) -> Option<u16> {
        match self.api()? {
            BitbucketApiError::Response { status, .. } | BitbucketApiError::ResponseBodyRead { status, .. } => Some(*status),
            _ => None,
        }
    }

    /// A `429` from Bitbucket, which no optional read may swallow.
    pub fn is_rate_limited(&self) -> bool {
        self.response_status() == Some(429)
    }
}

impl std::fmt::Display for BitbucketPullRequestApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for BitbucketPullRequestApiError {}

impl CauseError for BitbucketPullRequestApiError {
    fn defect(&self) -> Value {
        match self {
            Self::Api(error) => error.defect(),
            Self::Read { cause, .. } => error_defect(self.tag(), self.message(), Some(cause.defect())),
            _ => error_defect(self.tag(), self.message(), None),
        }
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

type ApiResult<T> = Result<T, BitbucketPullRequestApiError>;

/// Bitbucket's own page ceiling. Asking for more does not fail — it answers with an empty page
/// and no error at all.
const MAX_PAGE_SIZE: usize = 50;
/// Pages to walk before a listing is reported as truncated.
const MAX_LIST_PAGES: usize = 10;
/// The page size for pull request conversations, commits, and checks.
const CONVERSATION_PAGE_SIZE: usize = 50;
/// Pages of the conversation to follow before it is reported as truncated (five hundred
/// comments).
const CONVERSATION_PAGES: usize = 10;
/// The same ceiling the gh and glab diff reads use.
const DIFF_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Deliberately far shorter than the window the caller holds versions for: a refresh drops what
/// the caller holds precisely so the next read reaches Bitbucket.
const REVISION_PATCH_TTL_MS: i64 = 5_000;
const REVISION_PATCH_CAPACITY: usize = 16;

/// `BitbucketPullRequestBatch`.
#[derive(Debug, Clone, PartialEq)]
pub struct BitbucketPullRequestBatch {
    pub items: Vec<BitbucketPullRequest>,
    pub truncated: bool,
}

/// `listPullRequests` input.
#[derive(Debug, Clone, PartialEq)]
pub struct ListPullRequestsInput {
    pub repository: String,
    pub state: PullRequestListState,
    pub limit: i64,
    /// Free text, matched against a pull request's title and description.
    pub query: Option<String>,
    /// Where to carry on from, as a predicate on `updated_on` beside any other.
    pub cursor: Option<ProviderListCursor>,
}

/// A pull request's whole patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketDiff {
    pub patch: String,
    pub truncated: bool,
}

/// `getFileRevisions`' answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitbucketFileRevisions {
    pub revisions: Vec<(String, String)>,
    pub complete: bool,
}

/// `listComments`' answer.
#[derive(Debug, Clone, PartialEq)]
pub struct BitbucketConversation {
    pub comments: Vec<PullRequestComment>,
    pub threads: Vec<PullRequestReviewThread>,
    pub truncated: bool,
}

/// One parsed patch of the revision cache.
#[derive(Debug)]
struct RevisionPatch {
    revisions: OrderedRevisions,
    truncated: bool,
}

type RevisionLookup = Shared<BoxFuture<'static, Result<Arc<RevisionPatch>, BitbucketPullRequestApiError>>>;

struct CacheEntry {
    key: (String, i64),
    id: u64,
    lookup: RevisionLookup,
    expires_at: Option<i64>,
}

/// `Cache.makeWith(…, {capacity, timeToLive})`: shared lookups, insertion order refreshed on
/// every hit, the oldest dropped past capacity, a success held for its time to live and a
/// failure not at all.
#[derive(Default)]
struct RevisionCache {
    entries: Vec<CacheEntry>,
    next_id: u64,
}

struct Inner {
    requester: Arc<dyn BitbucketRequester>,
    clock: SharedClock,
    revisions: Mutex<RevisionCache>,
}

/// `BitbucketPullRequestApi`.
#[derive(Clone)]
pub struct BitbucketPullRequestApi {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for BitbucketPullRequestApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitbucketPullRequestApi").finish_non_exhaustive()
    }
}

/// `repositorySegments`: `workspace/slug`; Bitbucket has no deeper nesting to address.
fn repository_segments(repository: &str) -> ApiResult<(String, String)> {
    let segments: Vec<&str> = repository.split('/').map(js_trim).filter(|segment| !segment.is_empty()).collect();
    match segments.as_slice() {
        [workspace, slug] => Ok(((*workspace).to_owned(), (*slug).to_owned())),
        _ => Err(BitbucketPullRequestApiError::RepositoryUnsupported {
            repository: repository.to_owned(),
        }),
    }
}

/// The repository's own path, and the workspace above it (the people who may review are kept on
/// the workspace rather than on the repository).
fn with_repository(repository: &str) -> ApiResult<(String, String)> {
    let (workspace, slug) = repository_segments(repository)?;
    Ok((
        format!("/repositories/{}/{}", encode_uri_component(&workspace), encode_uri_component(&slug)),
        workspace,
    ))
}

/// `isCommitSha`: a commit sha arrives from the reader and goes straight into a request path, so
/// it is checked rather than trusted: hexadecimal only, seven to sixty-four characters.
fn is_commit_sha(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// `stateParams`: Bitbucket unions repeated `state` parameters, so a tab that spans several of
/// its states asks for each. A declined and a superseded pull request both read as closed here.
fn state_params(state: PullRequestListState) -> &'static [&'static str] {
    match state {
        PullRequestListState::Open => &["OPEN"],
        PullRequestListState::Merged => &["MERGED"],
        PullRequestListState::Closed => &["DECLINED", "SUPERSEDED"],
        PullRequestListState::All => &["OPEN", "MERGED", "DECLINED", "SUPERSEDED"],
    }
}

/// `filterLiteral`: text as a string literal of Bitbucket's filter grammar. The backslash is
/// escaped first, or escaping the quote would only produce a literal backslash and a live quote.
fn filter_literal(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// `searchFilter`: Bitbucket has no search term, only a filter expression, so free text becomes
/// a case-insensitive contains on the title and description, bracketed so the `OR` cannot
/// swallow the predicates ANDed beside it.
fn search_filter(query: &str) -> String {
    let literal = filter_literal(query);
    format!("(title ~ \"{literal}\" OR description ~ \"{literal}\")")
}

/// `mergeStrategy`: Bitbucket's names for the three merge methods.
fn merge_strategy(method: Option<PullRequestMergeMethod>) -> &'static str {
    match method {
        Some(PullRequestMergeMethod::Squash) => "squash",
        // The linear history GitHub calls "rebase and merge".
        Some(PullRequestMergeMethod::Rebase) => "rebase_fast_forward",
        _ => "merge_commit",
    }
}

/// `bitbucketReviewPosition`: `{from}` on the removed side, `{to}` on the new one.
fn review_position(position: &PullRequestReviewPosition) -> (&'static str, i64) {
    match position {
        PullRequestReviewPosition::Added(added) => ("to", added.new_line),
        PullRequestReviewPosition::Deleted(deleted) => ("from", deleted.old_line),
        PullRequestReviewPosition::Context(context) => match context.side {
            PullRequestDiffSide::Left => ("from", context.old_line),
            PullRequestDiffSide::Right => ("to", context.new_line),
        },
    }
}

/// `Number(text)` as `JSON.stringify` writes it: `null` for `NaN`.
fn js_number(text: &str) -> Value {
    let trimmed = js_trim(text);
    let number = if trimmed.is_empty() {
        Some(0.0)
    } else if let Some(radix) = trimmed.get(..2).and_then(|prefix| match prefix {
        "0x" | "0X" => Some(16),
        "0o" | "0O" => Some(8),
        "0b" | "0B" => Some(2),
        _ => None,
    }) {
        let digits = &trimmed[2..];
        (!digits.is_empty() && digits.chars().all(|digit| digit.is_digit(radix))).then(|| {
            digits
                .chars()
                .fold(0.0, |value, digit| value * f64::from(radix) + f64::from(digit.to_digit(radix).unwrap_or(0)))
        })
    } else {
        match trimmed {
            "Infinity" | "+Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            _ if trimmed
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'+' | b'-' | b'.' | b'e' | b'E')) =>
            {
                trimmed.parse::<f64>().ok()
            }
            _ => None,
        }
    };
    match number {
        Some(number) if number.is_finite() && number.fract() == 0.0 && number.abs() <= 9_007_199_254_740_991.0 => json!(number as i64),
        Some(number) if number.is_finite() => json!(number),
        _ => Value::Null,
    }
}

/// `items.slice(0, limit)`.
fn js_slice_to<T>(mut items: Vec<T>, limit: i64) -> Vec<T> {
    let length = items.len() as i64;
    let end = if limit < 0 { (length + limit).max(0) } else { limit.min(length) };
    items.truncate(end as usize);
    items
}

impl BitbucketPullRequestApi {
    /// Over zc-sourcecontrol's shared Bitbucket client. `clock` times the revision cache.
    pub fn new(bitbucket: BitbucketApi, clock: SharedClock) -> Self {
        Self::with_requester(Arc::new(bitbucket), clock)
    }

    /// Over any requester (tests).
    pub fn with_requester(requester: Arc<dyn BitbucketRequester>, clock: SharedClock) -> Self {
        Self {
            inner: Arc::new(Inner {
                requester,
                clock,
                revisions: Mutex::default(),
            }),
        }
    }

    async fn request(&self, method: &str, url: String, body: Option<String>, max_bytes: Option<usize>) -> ApiResult<BitbucketResponseBody> {
        let request = BitbucketRequest {
            method: method.to_owned(),
            url,
            body,
            max_bytes,
        };
        Ok(self.inner.requester.request(request).await?)
    }

    async fn get(&self, url: String) -> ApiResult<BitbucketResponseBody> {
        self.request("GET", url, None, None).await
    }

    /// `readPage`: one GET, decoded; a body that does not decode names the read it came from.
    async fn read_page<A>(&self, operation: &str, url: String, decode: impl FnOnce(&str) -> Result<A, DecodeFailure>) -> ApiResult<A> {
        let response = self.get(url).await?;
        decode(&response.body).map_err(|failure| BitbucketPullRequestApiError::Read {
            operation: operation.to_owned(),
            cause: Cause::new(failure),
        })
    }

    /// `itemPages`: walks a Bitbucket cursor to its end and combines every decoded item. Commit
    /// pages are individually oldest-first, so older pages are prepended.
    async fn item_pages<A>(
        &self,
        operation: &str,
        url: String,
        decode: fn(&str) -> Result<BitbucketPage<A>, DecodeFailure>,
        prepend: bool,
    ) -> ApiResult<Vec<A>> {
        let mut items: Vec<A> = Vec::new();
        let mut url = url;
        loop {
            let page = self.read_page(operation, url, decode).await?;
            if prepend {
                let mut older = page.items;
                older.extend(items);
                items = older;
            } else {
                items.extend(page.items);
            }
            match page.next {
                None => return Ok(items),
                Some(next) => url = next,
            }
        }
    }

    /// `getViewer`.
    pub async fn get_viewer(&self) -> ApiResult<String> {
        let response = self.get("/user".into()).await?;
        let decoded = decode_viewer_json(&response.body).map_err(|failure| BitbucketPullRequestApiError::Read {
            operation: "getViewer".into(),
            cause: Cause::new(failure),
        })?;
        decoded.ok_or(BitbucketPullRequestApiError::ViewerUnavailable)
    }

    /// `listPullRequests`. Bitbucket pages with a cursor rather than an offset, so the walk follows
    /// the `next` URL it sends. It stops once the caller's page is filled, when Bitbucket reports
    /// no next page, or at the page cap — and anything but running out of pages means there is
    /// more to be had.
    pub async fn list_pull_requests(&self, input: ListPullRequestsInput) -> ApiResult<BitbucketPullRequestBatch> {
        let (path, _) = with_repository(&input.repository)?;
        let search = input.query.as_deref().map(js_trim).unwrap_or_default();
        // Both narrowings share the one `q` Bitbucket takes, so they are ANDed. The boundary
        // instant is read inclusively — the rows already sent at it come back and the caller
        // drops them, which keeps their neighbours at the same instant from being skipped.
        let mut predicates = Vec::new();
        if !search.is_empty() {
            predicates.push(search_filter(search));
        }
        if let Some(cursor) = &input.cursor {
            predicates.push(format!("updated_on <= {}", cursor.updated_before));
        }
        let states = state_params(input.state)
            .iter()
            .map(|state| format!("state={state}"))
            .collect::<Vec<_>>()
            .join("&");
        let filter = if predicates.is_empty() {
            String::new()
        } else {
            format!("&q={}", encode_uri_component(&predicates.join(" AND ")))
        };
        // Reviewers are not on a listing by default, and `viewerReviewRequested` needs them.
        let mut url = format!("{path}/pullrequests?{states}&pagelen={MAX_PAGE_SIZE}&sort=-updated_on&fields=%2Bvalues.reviewers{filter}");
        let mut page = 1;
        let mut collected: Vec<BitbucketPullRequest> = Vec::new();
        loop {
            let response = self.get(url).await?;
            let decoded = decode_pull_request_page_json(&response.body).map_err(|failure| BitbucketPullRequestApiError::Read {
                operation: "listPullRequests".into(),
                cause: Cause::new(failure),
            })?;
            collected.extend(decoded.items);
            let length = collected.len() as i64;
            match decoded.next {
                Some(next) if length < input.limit && page < MAX_LIST_PAGES => {
                    url = next;
                    page += 1;
                }
                next => {
                    return Ok(BitbucketPullRequestBatch {
                        items: js_slice_to(collected, input.limit),
                        // Bitbucket pages in fifties whatever was asked for, so a walk that
                        // stopped on the count rather than on the last page is holding rows it
                        // is about to drop: those are more results too.
                        truncated: next.is_some() || length > input.limit,
                    });
                }
            }
        }
    }

    /// `getPullRequest`.
    pub async fn get_pull_request(&self, repository: &str, number: i64) -> ApiResult<BitbucketPullRequest> {
        let (path, _) = with_repository(repository)?;
        self.read_page("getPullRequest", format!("{path}/pullrequests/{number}"), decode_pull_request_json)
            .await
    }

    /// `getRepositoryPermission`: true where the credentials can write to the repository, which
    /// is what merging needs. Bitbucket permanently removed this endpoint (CHANGE-2770): every
    /// account now gets HTTP 410, the deprecated-endpoint signal rather than a refusal, so it is
    /// read as a permission that could not be learned, which grants. Any other failure fails.
    pub async fn get_repository_permission(&self, repository: &str) -> ApiResult<bool> {
        let result = async {
            with_repository(repository)?;
            let filter = format!("repository.full_name=\"{}\"", filter_literal(js_trim(repository)));
            self.read_page(
                "getRepositoryPermission",
                format!("/user/permissions/repositories?q={}", encode_uri_component(&filter)),
                decode_repository_permission_json,
            )
            .await
        }
        .await;
        match result {
            Err(BitbucketPullRequestApiError::Api(BitbucketApiError::Response { status: 410, .. })) => Ok(true),
            other => other,
        }
    }

    /// `getPullRequestDiff`: already a unified patch, so it needs no decoding, only a bound. A
    /// commit's own patch sits beside the pull request's at `/diff/{sha}`.
    pub async fn get_pull_request_diff(&self, repository: &str, number: i64, commit: Option<&str>) -> ApiResult<BitbucketDiff> {
        if commit.is_some_and(|commit| !is_commit_sha(commit)) {
            return Err(BitbucketPullRequestApiError::DiffCommit);
        }
        let (path, _) = with_repository(repository)?;
        let url = match commit {
            None => format!("{path}/pullrequests/{number}/diff"),
            Some(commit) => format!("{path}/diff/{commit}"),
        };
        let response = self.request("GET", url, None, Some(DIFF_MAX_BYTES)).await?;
        Ok(BitbucketDiff {
            patch: response.body,
            truncated: response.truncated,
        })
    }

    /// The revision cache's lookup (`revisionPatches`): the parsed answer rather than the patch,
    /// which at this capacity would hold sixteen bodies of up to the byte ceiling each.
    async fn revision_patch(&self, repository: &str, number: i64) -> ApiResult<Arc<RevisionPatch>> {
        let key = (repository.to_owned(), number);
        let (id, lookup) = {
            let mut cache = self.inner.revisions.lock().expect("revision cache lock");
            let now = self.inner.clock.now_millis();
            let found = cache.entries.iter().position(|entry| entry.key == key);
            let live = found.filter(|&at| cache.entries[at].expires_at.is_none_or(|expires_at| now < expires_at));
            match live {
                Some(at) => {
                    // Moved to the end to keep it fresh.
                    let entry = cache.entries.remove(at);
                    let hit = (entry.id, entry.lookup.clone());
                    cache.entries.push(entry);
                    hit
                }
                None => {
                    if let Some(at) = found {
                        cache.entries.remove(at);
                    }
                    let api = self.clone();
                    let (repository, number) = key.clone();
                    let lookup: RevisionLookup = async move {
                        let diff = api.get_pull_request_diff(&repository, number, None).await?;
                        Ok(Arc::new(RevisionPatch {
                            revisions: parse_diff_file_revisions(&diff.patch),
                            truncated: diff.truncated,
                        }))
                    }
                    .boxed()
                    .shared();
                    let id = cache.next_id;
                    cache.next_id += 1;
                    cache.entries.push(CacheEntry {
                        key,
                        id,
                        lookup: lookup.clone(),
                        expires_at: None,
                    });
                    let excess = cache.entries.len().saturating_sub(REVISION_PATCH_CAPACITY);
                    cache.entries.drain(..excess);
                    (id, lookup)
                }
            }
        };
        let result = lookup.await;
        let mut cache = self.inner.revisions.lock().expect("revision cache lock");
        let now = self.inner.clock.now_millis();
        if let Some(entry) = cache.entries.iter_mut().find(|entry| entry.id == id && entry.expires_at.is_none()) {
            // A failure is not held: the tick after it should reach Bitbucket.
            entry.expires_at = Some(now + if result.is_ok() { REVISION_PATCH_TTL_MS } else { 0 });
        }
        result
    }

    /// `getFileRevisions`: what the pull request's head has of each of these paths, as opaque
    /// ids, read off the pull request's own patch. A path the patch does not carry is answered as
    /// the empty revision, and left out when the patch was cut short at the byte ceiling (then
    /// `complete` is false). Answers with every file the patch carries, not only the paths asked
    /// about, since reading one file's version means parsing all of them.
    pub async fn get_file_revisions(&self, repository: &str, number: i64, paths: &[String]) -> ApiResult<BitbucketFileRevisions> {
        if paths.is_empty() {
            return Ok(BitbucketFileRevisions {
                revisions: Vec::new(),
                complete: false,
            });
        }
        let diff = self.revision_patch(repository, number).await?;
        let mut revisions = diff.revisions.clone();
        // A patch cut short says nothing about the files past the cut, so those paths are left
        // out rather than reported as removed.
        if !diff.truncated {
            for path in paths {
                if !revisions.contains(path) {
                    revisions.set(path.clone(), String::new());
                }
            }
        }
        Ok(BitbucketFileRevisions {
            revisions: revisions.into_entries(),
            complete: !diff.truncated,
        })
    }

    /// `getDiffStat`: one aggregate per page, folded while following `next`.
    pub async fn get_diff_stat(&self, repository: &str, number: i64) -> ApiResult<BitbucketDiffStat> {
        let (path, _) = with_repository(repository)?;
        let mut url = format!("{path}/pullrequests/{number}/diffstat?pagelen={MAX_PAGE_SIZE}");
        let mut totals = BitbucketDiffStat::default();
        loop {
            let page = self.read_page("getDiffStat", url, decode_diffstat_json).await?;
            totals.additions += page.stat.additions;
            totals.deletions += page.stat.deletions;
            totals.changed_files += page.stat.changed_files;
            match page.next {
                None => return Ok(totals),
                Some(next) => url = next,
            }
        }
    }

    /// `getMergeability`.
    pub async fn get_mergeability(&self, repository: &str, number: i64) -> ApiResult<PullRequestMergeability> {
        let (path, _) = with_repository(repository)?;
        self.read_page("getMergeability", format!("{path}/pullrequests/{number}/conflicts"), decode_conflicts_json)
            .await
    }

    /// `listComments`: the conversation, following `next` until Bitbucket sends none (or the page
    /// cap). Threads are assembled once at the end, because a reply and the remark it answers can
    /// land either side of a page boundary.
    pub async fn list_comments(&self, repository: &str, number: i64) -> ApiResult<BitbucketConversation> {
        let (path, _) = with_repository(repository)?;
        let mut url = format!("{path}/pullrequests/{number}/comments?pagelen={CONVERSATION_PAGE_SIZE}");
        let mut page_number = 1;
        let mut comments = Vec::new();
        let mut entries = Vec::new();
        loop {
            let page = self.read_page("listComments", url, decode_comments_json).await?;
            comments.extend(page.comments);
            entries.extend(page.entries);
            match page.next {
                Some(next) if page_number < CONVERSATION_PAGES => {
                    url = next;
                    page_number += 1;
                }
                next => {
                    return Ok(BitbucketConversation {
                        comments,
                        threads: build_review_threads(&entries),
                        truncated: next.is_some(),
                    })
                }
            }
        }
    }

    /// `listCommits`: the whole timeline, oldest first.
    pub async fn list_commits(&self, repository: &str, number: i64) -> ApiResult<Vec<PullRequestCommit>> {
        let (path, _) = with_repository(repository)?;
        self.item_pages(
            "listCommits",
            format!("{path}/pullrequests/{number}/commits?pagelen={CONVERSATION_PAGE_SIZE}"),
            decode_commits_json,
            true,
        )
        .await
    }

    /// `listChecks`: build statuses from every page.
    pub async fn list_checks(&self, repository: &str, number: i64) -> ApiResult<Vec<PullRequestCheck>> {
        let (path, _) = with_repository(repository)?;
        self.item_pages(
            "listChecks",
            format!("{path}/pullrequests/{number}/statuses?pagelen={CONVERSATION_PAGE_SIZE}"),
            decode_statuses_json,
            false,
        )
        .await
    }

    /// `listReviewerCandidates`: who this pull request may be sent to, and who it has already
    /// been sent to. Two reads at once: Bitbucket keeps the people on the workspace and the
    /// reviewers on the pull request.
    pub async fn list_reviewer_candidates(&self, repository: &str, number: i64) -> ApiResult<PullRequestReviewerCandidateList> {
        let (path, workspace) = with_repository(repository)?;
        let (pull_request, members) = futures::try_join!(
            self.read_page("getPullRequest", format!("{path}/pullrequests/{number}"), decode_pull_request_json),
            self.read_page(
                "listReviewerCandidates",
                format!("/workspaces/{}/members?pagelen={MAX_PAGE_SIZE}", encode_uri_component(&workspace)),
                decode_workspace_members_json,
            ),
        )?;
        let author = pull_request.author.as_ref().map(|author| author.login.as_str());
        let candidates = members
            .items
            .into_iter()
            // The author is dropped rather than shown unusable: Bitbucket refuses to make the
            // person who opened a pull request its reviewer.
            .filter(|candidate| Some(candidate.login.as_str()) != author)
            .map(|candidate| PullRequestReviewerCandidate {
                is_requested: pull_request.reviewer_ids.contains(&candidate.id),
                ..candidate
            })
            .collect();
        Ok(PullRequestReviewerCandidateList {
            candidates,
            truncated: members.next.is_some(),
        })
    }

    /// `setReviewerRequest`: Bitbucket has no endpoint that adds or removes one reviewer — the
    /// pull request's `reviewers` is written whole — so the set already there is read first and
    /// the change applied to it. Everything else is left out of the body, which leaves it as it
    /// was.
    pub async fn set_reviewer_request(&self, repository: &str, number: i64, reviewer_ids: &[String], requested: bool) -> ApiResult<()> {
        let (path, _) = with_repository(repository)?;
        let pull_request = format!("{path}/pullrequests/{number}");
        let current = self.read_page("getPullRequest", pull_request.clone(), decode_pull_request_json).await?;
        let mut uuids: Vec<String> = Vec::new();
        for uuid in current.reviewer_ids {
            if !uuids.contains(&uuid) {
                uuids.push(uuid);
            }
        }
        for id in reviewer_ids {
            if requested {
                if !uuids.contains(id) {
                    uuids.push(id.clone());
                }
            } else {
                uuids.retain(|uuid| uuid != id);
            }
        }
        let body = json!({"reviewers": uuids.iter().map(|uuid| json!({"uuid": uuid})).collect::<Vec<_>>()});
        self.request("PUT", pull_request, Some(body.to_string()), None).await?;
        Ok(())
    }

    /// `runAction`: only merge and close reach here (the provider declares the others
    /// unsupported).
    pub async fn run_action(&self, repository: &str, number: i64, action: PullRequestAction, merge_method: Option<PullRequestMergeMethod>) -> ApiResult<()> {
        let (path, _) = with_repository(repository)?;
        let pull_request = format!("{path}/pullrequests/{number}");
        if action == PullRequestAction::Merge {
            let body = json!({"merge_strategy": merge_strategy(merge_method)});
            self.request("POST", format!("{pull_request}/merge"), Some(body.to_string()), None).await?;
        } else {
            self.request("POST", format!("{pull_request}/decline"), None, None).await?;
        }
        Ok(())
    }

    /// `updateChangeRequest`: only the words this call rewrites travel in the body. Bitbucket's
    /// PUT is a partial update, and sending `reviewers` back would overwrite a change another
    /// user made in between.
    pub async fn update_change_request(&self, repository: &str, number: i64, title: Option<&str>, body: Option<&str>) -> ApiResult<()> {
        let (path, _) = with_repository(repository)?;
        let mut fields = serde_json::Map::new();
        if let Some(title) = title {
            fields.insert("title".into(), json!(title));
        }
        if let Some(body) = body {
            fields.insert("description".into(), json!(body));
        }
        self.request("PUT", format!("{path}/pullrequests/{number}"), Some(Value::Object(fields).to_string()), None)
            .await?;
        Ok(())
    }

    /// `comment`: a JSON document rather than a form field, so the body stays text.
    pub async fn comment(&self, repository: &str, number: i64, body: &str) -> ApiResult<()> {
        let (path, _) = with_repository(repository)?;
        let document = json!({"content": {"raw": body}});
        self.request("POST", format!("{path}/pullrequests/{number}/comments"), Some(document.to_string()), None)
            .await?;
        Ok(())
    }

    /// `updateComment`: Bitbucket keeps a pull request's remarks and its line comments in the
    /// one collection, so this endpoint rewrites either kind.
    pub async fn update_comment(&self, repository: &str, number: i64, comment_id: &str, body: &str) -> ApiResult<()> {
        let (path, _) = with_repository(repository)?;
        let document = json!({"content": {"raw": body}});
        let url = format!("{path}/pullrequests/{number}/comments/{}", encode_uri_component(comment_id));
        self.request("PUT", url, Some(document.to_string()), None).await?;
        Ok(())
    }

    /// `submitReview`: Bitbucket has no pending review, so a review is replayed as the requests
    /// it is made of: the line comments, then the summary, then the verdict — last, so a review
    /// that fails part-way is never left standing as an approval.
    pub async fn submit_review(
        &self,
        repository: &str,
        number: i64,
        verdict: PullRequestReviewVerdict,
        body: &str,
        comments: &[PullRequestReviewCommentDraft],
    ) -> ApiResult<()> {
        let (path, _) = with_repository(repository)?;
        let pull_request = format!("{path}/pullrequests/{number}");
        for comment in comments {
            let (side, line) = review_position(&comment.position);
            let mut inline = serde_json::Map::new();
            inline.insert("path".into(), json!(comment.path));
            inline.insert(side.into(), json!(line));
            let document = json!({"content": {"raw": comment.body}, "inline": inline});
            self.request("POST", format!("{pull_request}/comments"), Some(document.to_string()), None)
                .await?;
        }
        if !js_trim(body).is_empty() {
            let document = json!({"content": {"raw": body}});
            self.request("POST", format!("{pull_request}/comments"), Some(document.to_string()), None)
                .await?;
        }
        match verdict {
            PullRequestReviewVerdict::Approve => {
                self.request("POST", format!("{pull_request}/approve"), None, None).await?;
            }
            PullRequestReviewVerdict::RequestChanges => {
                self.request("POST", format!("{pull_request}/request-changes"), None, None).await?;
            }
            PullRequestReviewVerdict::Comment => {}
        }
        Ok(())
    }

    /// `replyToComment`: a comment naming the one it answers.
    pub async fn reply_to_comment(&self, repository: &str, number: i64, comment_id: &str, body: &str) -> ApiResult<()> {
        let (path, _) = with_repository(repository)?;
        let document = json!({"content": {"raw": body}, "parent": {"id": js_number(comment_id)}});
        self.request("POST", format!("{path}/pullrequests/{number}/comments"), Some(document.to_string()), None)
            .await?;
        Ok(())
    }

    /// `setCommentResolution`: resolving is a sub-resource that is created and deleted, rather
    /// than a field.
    pub async fn set_comment_resolution(&self, repository: &str, number: i64, comment_id: &str, resolved: bool) -> ApiResult<()> {
        let (path, _) = with_repository(repository)?;
        let url = format!("{path}/pullrequests/{number}/comments/{}/resolve", encode_uri_component(comment_id));
        self.request(if resolved { "POST" } else { "DELETE" }, url, None, None).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_comment_id_the_way_number_does() {
        assert_eq!(js_number("10"), json!(10));
        assert_eq!(js_number(" 12 "), json!(12));
        assert_eq!(js_number(""), json!(0));
        assert_eq!(js_number("0x1A"), json!(26));
        assert_eq!(js_number("1.5"), json!(1.5));
        assert_eq!(js_number("abc"), Value::Null);
        assert_eq!(js_number("inf"), Value::Null);
        assert_eq!(js_number("Infinity"), Value::Null);
    }

    #[test]
    fn checks_a_commit_sha() {
        assert!(is_commit_sha("a1b2c3d"));
        assert!(!is_commit_sha("a1b2c3"));
        assert!(!is_commit_sha("../../acme/other/diff/deadbeef"));
    }

    #[test]
    fn slices_like_javascript() {
        assert_eq!(js_slice_to(vec![1, 2, 3], 2), vec![1, 2]);
        assert_eq!(js_slice_to(vec![1, 2, 3], 5), vec![1, 2, 3]);
        assert_eq!(js_slice_to(vec![1, 2, 3], -1), vec![1, 2]);
        assert_eq!(js_slice_to(vec![1, 2, 3], 0), Vec::<i32>::new());
    }
}
