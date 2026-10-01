//! `pullRequest/AzureDevOpsPullRequestCli.ts`: the pull request reads and writes of `az repos pr`
//! and `az devops invoke`, over zc-sourcecontrol's shared [`AzureDevOpsCli`].
//!
//! Every command resolves the organization, project and repository from the checkout
//! (`--detect true`), as the rest of the Azure wrapper does: the remote takes three shapes and
//! only `az` reads all of them. REST routes go through `az devops invoke` rather than `az rest`,
//! because it signs in the way the azure-devops extension does; `az rest` mints its own token
//! against the tenant `az` defaults to, and an organisation in any other tenant answers that with
//! a sign-in page.

use std::any::Any;
use std::fmt;

use async_trait::async_trait;
use serde::{Serialize, Serializer};
use serde_json::{json, Map, Value};
use zc_contracts::{PullRequestAction, PullRequestComment, PullRequestInvolvement, PullRequestListState, PullRequestMergeMethod};
use zc_sourcecontrol::azure::{AzureDevOpsCli, AzureDevOpsCliError, AzureDevOpsCliErrorKind, AzureExecuteInput};
use zc_sourcecontrol::errors::{error_defect, Cause, CauseError};
use zc_sourcecontrol::github::cli::SchemaDecodeError;
use zc_sourcecontrol::util::js_trim;

use crate::azure::json::{
    decode_item_content_json, decode_iteration_changes_json, decode_iterations_json, decode_pull_request_json, decode_pull_request_list_json,
    decode_threads_json, decode_viewer_json, AzureDevOpsChangeEntry, AzureDevOpsChangePage, AzureDevOpsItemContent, AzureDevOpsIteration,
    AzureDevOpsPullRequest, AzureDevOpsRepositoryLocation, DecodeFailure,
};
use crate::provider::ProviderListCursor;

/// `AzureDevOpsPullRequestCliError`: the shared CLI's failures plus the four of this module.
#[derive(Debug, Clone)]
pub enum AzureDevOpsPullRequestCliError {
    /// `AzureDevOpsCli.AzureDevOpsCliError` (the process failed, or `az` is missing, signed out
    /// or rate limited).
    Cli(AzureDevOpsCliError),
    /// `AzureDevOpsPullRequestReadError`: names the read that produced unusable output.
    Read { cwd: String, operation: &'static str, cause: Cause },
    /// `AzureDevOpsPullRequestIncompleteError`: a well-formed pull request with no branch or link.
    Incomplete { cwd: String, number: i64 },
    /// `AzureDevOpsReviewerNameError`: a reviewer `az` would read as a flag of its own.
    ReviewerName { cwd: String },
    /// `AzureDevOpsViewerUnavailableError`: az answered, but the account has no name.
    ViewerUnavailable { cwd: String },
    /// An action this host does not declare. TS throws (a defect) while building the arguments;
    /// the service refuses such actions before a provider is reached.
    UnsupportedAction { cwd: String, action: PullRequestAction },
}

impl AzureDevOpsPullRequestCliError {
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Cli(error) => error.tag(),
            Self::Read { .. } => "AzureDevOpsPullRequestReadError",
            Self::Incomplete { .. } => "AzureDevOpsPullRequestIncompleteError",
            Self::ReviewerName { .. } => "AzureDevOpsReviewerNameError",
            Self::ViewerUnavailable { .. } => "AzureDevOpsViewerUnavailableError",
            Self::UnsupportedAction { .. } => "Error",
        }
    }

    /// The TS `detail` getter.
    pub fn detail(&self) -> String {
        match self {
            Self::Cli(error) => error.detail().to_owned(),
            Self::Read { operation, .. } => format!("Azure CLI returned an unreadable {operation} response."),
            Self::Incomplete { .. } => "Azure DevOps returned no branch or link for the pull request.".into(),
            Self::ReviewerName { .. } => "A reviewer is named by an email address or an identity id.".into(),
            Self::ViewerUnavailable { .. } => "Azure CLI returned no account for the current sign-in.".into(),
            Self::UnsupportedAction { action, .. } => format!("Azure DevOps pull request action {} is unsupported", action.as_str()),
        }
    }

    /// The TS `message` getter.
    pub fn message(&self) -> String {
        match self {
            Self::Cli(error) => error.message(),
            Self::Read { operation, .. } => format!("Azure CLI failed in {operation}: {}", self.detail()),
            Self::Incomplete { .. } => format!("Azure CLI failed in getPullRequest: {}", self.detail()),
            Self::ReviewerName { .. } => format!("Azure CLI failed in setPullRequestReviewers: {}", self.detail()),
            Self::ViewerUnavailable { .. } => format!("Azure CLI failed in getViewer: {}", self.detail()),
            Self::UnsupportedAction { .. } => self.detail(),
        }
    }

    /// The kind of the shared CLI's error underneath, if this is one.
    pub fn cli_kind(&self) -> Option<&AzureDevOpsCliErrorKind> {
        match self {
            Self::Cli(error) => Some(&error.kind),
            _ => None,
        }
    }

    fn read(cwd: &str, operation: &'static str, failure: DecodeFailure) -> Self {
        Self::Read {
            cwd: cwd.to_owned(),
            operation,
            cause: Cause::new(SchemaDecodeError(failure)),
        }
    }
}

impl From<AzureDevOpsCliError> for AzureDevOpsPullRequestCliError {
    fn from(error: AzureDevOpsCliError) -> Self {
        Self::Cli(error)
    }
}

impl fmt::Display for AzureDevOpsPullRequestCliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for AzureDevOpsPullRequestCliError {}

impl Serialize for AzureDevOpsPullRequestCliError {
    /// The tagged encoding of each member (`command`, `cwd`, then its own fields and `cause`).
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = Map::new();
        let mut cause = None;
        match self {
            Self::Cli(error) => return error.serialize(serializer),
            Self::Read {
                cwd,
                operation,
                cause: read_cause,
            } => {
                map.insert("cwd".into(), json!(cwd));
                map.insert("operation".into(), json!(operation));
                cause = Some(read_cause.defect());
            }
            Self::Incomplete { cwd, number } => {
                map.insert("cwd".into(), json!(cwd));
                map.insert("number".into(), json!(number));
            }
            Self::ReviewerName { cwd } | Self::ViewerUnavailable { cwd } | Self::UnsupportedAction { cwd, .. } => {
                map.insert("cwd".into(), json!(cwd));
            }
        }
        map.insert("_tag".into(), json!(self.tag()));
        map.insert("command".into(), json!("az"));
        if let Some(cause) = cause {
            map.insert("cause".into(), cause);
        }
        Value::Object(map).serialize(serializer)
    }
}

impl CauseError for AzureDevOpsPullRequestCliError {
    fn defect(&self) -> Value {
        match self {
            Self::Cli(error) => error.defect(),
            Self::Read { cause, .. } => error_defect(self.tag(), self.message(), Some(cause.defect())),
            _ => error_defect(self.tag(), self.message(), None),
        }
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub type CliResult<T> = Result<T, AzureDevOpsPullRequestCliError>;

/// The version every REST call is pinned to, so a new default cannot reshape a response.
const REST_API_VERSION: &str = "7.1";
const PULL_REQUEST_LIST_MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// A full page of change entries (two thousand files with paths, urls and object ids) is past the
/// megabyte a read gets by default, and output cut there arrives as JSON that will not parse.
const CHANGE_ENTRIES_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Four times the megabyte of file the other hosts hand over: the file arrives inside a JSON
/// envelope, escaped if text and base64 if not, both larger than the file.
const ITEM_CONTENT_MAX_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
/// What a review's own history (threads, iterations) is given: neither route pages.
const REVIEW_HISTORY_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
/// Azure's own ceiling for one page of an iteration's changes.
const CHANGE_ENTRIES_PER_PAGE: i64 = 2000;
/// Where following the pages stops, counted in entries Azure was asked to skip (a page can be all
/// folders, so bounding on what was kept could follow a change forever).
const MAX_CHANGE_ENTRIES: i64 = 10_000;

/// What an iteration changed, and whether following its pages reached the end of it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AzureDevOpsIterationChanges {
    pub changes: Vec<AzureDevOpsChangeEntry>,
    pub truncated: bool,
}

/// `listPullRequests` input.
#[derive(Debug, Clone, PartialEq)]
pub struct ListPullRequestsInput {
    pub cwd: String,
    pub repository: String,
    pub state: PullRequestListState,
    pub involvement: PullRequestInvolvement,
    pub viewer: String,
    pub limit: i64,
    /// Azure has no date filter for a listing, so only the delivered count of a cursor is used.
    pub cursor: Option<ProviderListCursor>,
}

/// `listPullRequests` answer.
#[derive(Debug, Clone, PartialEq)]
pub struct AzureDevOpsPullRequestPage {
    pub items: Vec<AzureDevOpsPullRequest>,
    pub truncated: bool,
    /// Raw Azure rows consumed to produce this page, including malformed rows.
    pub cursor_advance: i64,
}

/// The `AzureDevOpsPullRequestCli` service, as a trait so the provider's tests can mock it the
/// way the TS tests mock the layer.
#[async_trait]
pub trait AzureDevOpsPullRequestCliApi: Send + Sync {
    async fn get_viewer(&self, cwd: &str) -> CliResult<String>;
    async fn list_pull_requests(&self, input: ListPullRequestsInput) -> CliResult<AzureDevOpsPullRequestPage>;
    async fn get_pull_request(&self, cwd: &str, number: i64) -> CliResult<AzureDevOpsPullRequest>;
    /// Threads are not reachable through `az repos pr`, so they come from the REST API.
    async fn list_threads(&self, cwd: &str, location: &AzureDevOpsRepositoryLocation, number: i64) -> CliResult<Vec<PullRequestComment>>;
    /// The pushes a pull request has had, oldest first.
    async fn list_iterations(&self, cwd: &str, location: &AzureDevOpsRepositoryLocation, number: i64) -> CliResult<Vec<AzureDevOpsIteration>>;
    /// What one iteration changed, against the merge base: the whole of the pull request.
    async fn list_iteration_changes(
        &self,
        cwd: &str,
        location: &AzureDevOpsRepositoryLocation,
        number: i64,
        iteration_id: i64,
    ) -> CliResult<AzureDevOpsIterationChanges>;
    /// One file's text at one commit.
    async fn read_item_content(&self, cwd: &str, location: &AzureDevOpsRepositoryLocation, path: &str, commit: &str) -> CliResult<AzureDevOpsItemContent>;
    async fn run_pull_request_action(&self, cwd: &str, number: i64, action: PullRequestAction, merge_method: Option<PullRequestMergeMethod>) -> CliResult<()>;
    /// Rewrites the pull request's own words, through the same command that moves it.
    async fn update_pull_request(&self, cwd: &str, number: i64, title: Option<&str>, body: Option<&str>) -> CliResult<()>;
    /// Adds reviewers to a pull request or takes them off it (`az repos pr reviewer`).
    async fn set_pull_request_reviewers(&self, cwd: &str, number: i64, reviewers: &[String], requested: bool) -> CliResult<()>;
}

fn status_args(state: PullRequestListState) -> [&'static str; 2] {
    match state {
        PullRequestListState::Open => ["--status", "active"],
        PullRequestListState::Merged => ["--status", "completed"],
        PullRequestListState::Closed => ["--status", "abandoned"],
        PullRequestListState::All => ["--status", "all"],
    }
}

fn involvement_args(involvement: PullRequestInvolvement, viewer: &str) -> Vec<String> {
    match involvement {
        PullRequestInvolvement::Authored => vec!["--creator".into(), viewer.to_owned()],
        PullRequestInvolvement::Reviewing => vec!["--reviewer".into(), viewer.to_owned()],
        PullRequestInvolvement::All => Vec::new(),
    }
}

/// Azure moves a pull request by setting its state: completing is the merge, abandoning the
/// close, reactivating the reopen. Squashing is a completion option; auto-complete stores it too.
fn action_args(action: PullRequestAction, merge_method: Option<PullRequestMergeMethod>) -> Option<Vec<&'static str>> {
    let squash = |method: Option<PullRequestMergeMethod>| if method == Some(PullRequestMergeMethod::Squash) { "true" } else { "false" };
    Some(match action {
        PullRequestAction::Merge => vec!["--status", "completed", "--squash", squash(merge_method)],
        PullRequestAction::EnableAutoMerge => {
            let mut args = vec!["--auto-complete", "true"];
            if merge_method.is_some() {
                args.extend(["--squash", squash(merge_method)]);
            }
            args
        }
        PullRequestAction::DisableAutoMerge => vec!["--auto-complete", "false"],
        PullRequestAction::Ready => vec!["--draft", "false"],
        PullRequestAction::Draft => vec!["--draft", "true"],
        PullRequestAction::Close => vec!["--status", "abandoned"],
        // Never reached: this host does not declare the action.
        PullRequestAction::UpdateBranch => Vec::new(),
        PullRequestAction::Reopen => vec!["--status", "active"],
        PullRequestAction::Revert | PullRequestAction::ApproveWorkflows => return None,
    })
}

/// A reviewer Azure could be given: anything non-blank that does not start with a dash, since
/// these travel as argv and a value that looks like a flag stops being a value.
fn is_reviewer_name(value: &str) -> bool {
    let name = js_trim(value);
    !name.is_empty() && !name.starts_with('-')
}

/// Azure names its items with a leading slash, which the paths carried here have had taken off;
/// it goes back on the way out.
fn to_item_path(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    }
}

fn repository_route(location: &AzureDevOpsRepositoryLocation) -> Vec<String> {
    vec![format!("project={}", location.project), format!("repositoryId={}", location.repository)]
}

fn pull_request_route(location: &AzureDevOpsRepositoryLocation, number: i64) -> Vec<String> {
    let mut route = repository_route(location);
    route.push(format!("pullRequestId={number}"));
    route
}

fn strings<const N: usize>(args: [&str; N]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

/// One `az devops invoke` call: the operation it reports failures as, and its route.
struct RestRoute {
    operation: &'static str,
    resource: &'static str,
    route_parameters: Vec<String>,
    query_parameters: Option<Vec<String>>,
    max_output_bytes: usize,
}

/// The `AzureDevOpsPullRequestCli` live implementation.
#[derive(Clone)]
pub struct AzureDevOpsPullRequestCli {
    azure: AzureDevOpsCli,
}

impl AzureDevOpsPullRequestCli {
    pub fn new(azure: AzureDevOpsCli) -> Self {
        Self { azure }
    }

    /// `executeJson`: the read flags appended, stdout trimmed.
    async fn execute_json(&self, cwd: &str, mut args: Vec<String>, max_output_bytes: Option<usize>) -> CliResult<String> {
        args.extend(strings(["--only-show-errors", "--output", "json"]));
        let mut input = AzureExecuteInput::new(cwd, args);
        input.max_output_bytes = max_output_bytes;
        let output = self.azure.execute(input).await?;
        Ok(js_trim(&output.stdout).to_owned())
    }

    /// A write: the same flags, the output ignored.
    async fn execute_write(&self, cwd: &str, args: Vec<String>) -> CliResult<()> {
        self.execute_json(cwd, args, None).await.map(drop)
    }

    /// `invoke`: a REST route through `az devops invoke`, addressed by area, resource and route
    /// parameters.
    async fn invoke<T>(&self, cwd: &str, route: RestRoute, decode: fn(&str) -> Result<T, DecodeFailure>) -> CliResult<T> {
        let mut args = strings(["devops", "invoke", "--detect", "true", "--area", "git", "--resource"]);
        args.push(route.resource.to_owned());
        args.extend(strings(["--api-version", REST_API_VERSION, "--route-parameters"]));
        args.extend(route.route_parameters);
        if let Some(query) = route.query_parameters {
            args.push("--query-parameters".into());
            args.extend(query);
        }
        let raw = self.execute_json(cwd, args, Some(route.max_output_bytes)).await?;
        decode(&raw).map_err(|failure| AzureDevOpsPullRequestCliError::read(cwd, route.operation, failure))
    }

    async fn list_iteration_changes_page(
        &self,
        cwd: &str,
        location: &AzureDevOpsRepositoryLocation,
        number: i64,
        iteration_id: i64,
        skip: i64,
    ) -> CliResult<AzureDevOpsChangePage> {
        let mut route = pull_request_route(location, number);
        route.push(format!("iterationId={iteration_id}"));
        let route = RestRoute {
            operation: "listIterationChanges",
            resource: "pullRequestIterationChanges",
            route_parameters: route,
            // Azure pages this route at 1000 entries by default; this is its own maximum.
            query_parameters: Some(vec![format!("$top={CHANGE_ENTRIES_PER_PAGE}"), format!("$skip={skip}")]),
            max_output_bytes: CHANGE_ENTRIES_MAX_OUTPUT_BYTES,
        };
        self.invoke(cwd, route, decode_iteration_changes_json).await
    }
}

#[async_trait]
impl AzureDevOpsPullRequestCliApi for AzureDevOpsPullRequestCli {
    async fn get_viewer(&self, cwd: &str) -> CliResult<String> {
        let raw = self.execute_json(cwd, strings(["account", "show", "--query", "user"]), None).await?;
        // `--query user` narrows the payload to the account, so it is nested back under the key
        // the decoder reads.
        let wrapped = format!("{{\"user\":{}}}", if raw.is_empty() { "null" } else { raw.as_str() });
        match decode_viewer_json(&wrapped) {
            Err(failure) => Err(AzureDevOpsPullRequestCliError::read(cwd, "getViewer", failure)),
            Ok(None) => Err(AzureDevOpsPullRequestCliError::ViewerUnavailable { cwd: cwd.to_owned() }),
            Ok(Some(viewer)) => Ok(viewer),
        }
    }

    /// Azure pages by raw offset: keep reading when malformed rows leave the decoded page short,
    /// and count every raw row consumed so the next cursor skips them all. Azure counts rather
    /// than filters, so a pull request opened between two slices shifts the seam by one.
    async fn list_pull_requests(&self, input: ListPullRequestsInput) -> CliResult<AzureDevOpsPullRequestPage> {
        let mut items: Vec<AzureDevOpsPullRequest> = Vec::new();
        let mut skip = input.cursor.as_ref().map_or(0, |cursor| cursor.delivered);
        let mut cursor_advance = 0i64;
        loop {
            let remaining = input.limit - items.len() as i64;
            let top = remaining + 1;
            let mut args = strings(["repos", "pr", "list", "--detect", "true", "--repository"]);
            args.push(input.repository.clone());
            args.extend(strings(status_args(input.state)));
            args.extend(involvement_args(input.involvement, &input.viewer));
            // A web link per row, which is the only url that needs no assembling.
            args.push("--include-links".into());
            if skip != 0 {
                args.extend(["--skip".to_owned(), skip.to_string()]);
            }
            args.extend(["--top".to_owned(), top.to_string()]);
            let raw = self.execute_json(&input.cwd, args, Some(PULL_REQUEST_LIST_MAX_OUTPUT_BYTES)).await?;
            if raw.is_empty() {
                return Ok(AzureDevOpsPullRequestPage {
                    items,
                    truncated: false,
                    cursor_advance,
                });
            }
            let batch = decode_pull_request_list_json(&raw).map_err(|failure| AzureDevOpsPullRequestCliError::read(&input.cwd, "listPullRequests", failure))?;
            let raw_count = batch.raw_count as i64;
            let last_item_index = usize::try_from(remaining - 1).ok().and_then(|at| batch.raw_indexes.get(at).copied());
            if let Some(last_item_index) = last_item_index {
                let consumed = last_item_index as i64 + 1;
                items.extend(batch.items.into_iter().take(remaining as usize));
                return Ok(AzureDevOpsPullRequestPage {
                    items,
                    // A full raw response may have more rows even when malformed entries used the
                    // probe.
                    truncated: consumed < raw_count || raw_count == top,
                    cursor_advance: cursor_advance + consumed,
                });
            }
            items.extend(batch.items);
            if raw_count < top {
                return Ok(AzureDevOpsPullRequestPage {
                    items,
                    truncated: false,
                    cursor_advance: cursor_advance + raw_count,
                });
            }
            skip += raw_count;
            cursor_advance += raw_count;
        }
    }

    async fn get_pull_request(&self, cwd: &str, number: i64) -> CliResult<AzureDevOpsPullRequest> {
        let mut args = strings(["repos", "pr", "show", "--detect", "true", "--id"]);
        args.push(number.to_string());
        let raw = self.execute_json(cwd, args, None).await?;
        match decode_pull_request_json(&raw) {
            Err(failure) => Err(AzureDevOpsPullRequestCliError::read(cwd, "getPullRequest", failure)),
            // Azure answered with too little to place the pull request: its own outcome.
            Ok(None) => Err(AzureDevOpsPullRequestCliError::Incomplete { cwd: cwd.to_owned(), number }),
            Ok(Some(pull_request)) => Ok(pull_request),
        }
    }

    async fn list_threads(&self, cwd: &str, location: &AzureDevOpsRepositoryLocation, number: i64) -> CliResult<Vec<PullRequestComment>> {
        let route = RestRoute {
            operation: "listThreads",
            resource: "pullRequestThreads",
            route_parameters: pull_request_route(location, number),
            query_parameters: None,
            max_output_bytes: REVIEW_HISTORY_MAX_OUTPUT_BYTES,
        };
        self.invoke(cwd, route, decode_threads_json).await
    }

    async fn list_iterations(&self, cwd: &str, location: &AzureDevOpsRepositoryLocation, number: i64) -> CliResult<Vec<AzureDevOpsIteration>> {
        let route = RestRoute {
            operation: "listIterations",
            resource: "pullRequestIterations",
            route_parameters: pull_request_route(location, number),
            query_parameters: None,
            max_output_bytes: REVIEW_HISTORY_MAX_OUTPUT_BYTES,
        };
        self.invoke(cwd, route, decode_iterations_json).await
    }

    async fn list_iteration_changes(
        &self,
        cwd: &str,
        location: &AzureDevOpsRepositoryLocation,
        number: i64,
        iteration_id: i64,
    ) -> CliResult<AzureDevOpsIterationChanges> {
        let mut changes = Vec::new();
        let mut skip = 0;
        loop {
            let page = self.list_iteration_changes_page(cwd, location, number, iteration_id, skip).await?;
            changes.extend(page.changes);
            // Only a page naming no page after it is the end of the change.
            let Some(next_skip) = page.next_skip else {
                return Ok(AzureDevOpsIterationChanges { changes, truncated: false });
            };
            // A page pointing at where the read already is would be followed forever, and one past
            // the ceiling is a change nobody reads to the end: both stop and say so.
            if next_skip <= skip || next_skip >= MAX_CHANGE_ENTRIES {
                return Ok(AzureDevOpsIterationChanges { changes, truncated: true });
            }
            skip = next_skip;
        }
    }

    async fn read_item_content(&self, cwd: &str, location: &AzureDevOpsRepositoryLocation, path: &str, commit: &str) -> CliResult<AzureDevOpsItemContent> {
        let route = RestRoute {
            operation: "readItemContent",
            resource: "items",
            route_parameters: repository_route(location),
            query_parameters: Some(vec![
                format!("path={}", to_item_path(path)),
                "versionDescriptor.versionType=commit".into(),
                format!("versionDescriptor.version={commit}"),
                "includeContent=true".into(),
                // Azure leaves `contentMetadata` (its word on whether the file is binary) out
                // unless asked.
                "includeContentMetadata=true".into(),
                // Without this Azure answers with the file's own bytes, which `az devops invoke`
                // refuses to parse.
                "$format=json".into(),
            ]),
            max_output_bytes: ITEM_CONTENT_MAX_OUTPUT_BYTES,
        };
        self.invoke(cwd, route, decode_item_content_json).await
    }

    async fn run_pull_request_action(&self, cwd: &str, number: i64, action: PullRequestAction, merge_method: Option<PullRequestMergeMethod>) -> CliResult<()> {
        let Some(action_args) = action_args(action, merge_method) else {
            return Err(AzureDevOpsPullRequestCliError::UnsupportedAction { cwd: cwd.to_owned(), action });
        };
        let mut args = strings(["repos", "pr", "update", "--detect", "true", "--id"]);
        args.push(number.to_string());
        args.extend(action_args.into_iter().map(str::to_owned));
        self.execute_write(cwd, args).await
    }

    async fn update_pull_request(&self, cwd: &str, number: i64, title: Option<&str>, body: Option<&str>) -> CliResult<()> {
        let mut args = strings(["repos", "pr", "update", "--detect", "true", "--id"]);
        args.push(number.to_string());
        // One argument rather than a flag and a value: a description usually opens with a bullet,
        // which az would read as a flag, and `--description` takes several strings.
        if let Some(title) = title {
            args.push(format!("--title={title}"));
        }
        if let Some(body) = body {
            args.push(format!("--description={body}"));
        }
        self.execute_write(cwd, args).await
    }

    async fn set_pull_request_reviewers(&self, cwd: &str, number: i64, reviewers: &[String], requested: bool) -> CliResult<()> {
        if reviewers.iter().any(|reviewer| !is_reviewer_name(reviewer)) {
            return Err(AzureDevOpsPullRequestCliError::ReviewerName { cwd: cwd.to_owned() });
        }
        let mut args = strings(["repos", "pr", "reviewer"]);
        args.push(if requested { "add" } else { "remove" }.to_owned());
        args.extend(strings(["--detect", "true", "--id"]));
        args.push(number.to_string());
        // One `--reviewers` takes them all: a second one would replace the first.
        args.push("--reviewers".into());
        args.extend(reviewers.iter().cloned());
        self.execute_write(cwd, args).await
    }
}
