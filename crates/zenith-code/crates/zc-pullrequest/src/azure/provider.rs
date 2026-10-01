//! `pullRequest/AzureDevOpsPullRequestProvider.ts`: Azure DevOps change requests through `az`.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::{try_join, try_join_all};
use tokio::sync::Semaphore;
use zc_contracts::{
    PullRequestAction, PullRequestCapabilities, PullRequestEditCapabilities, PullRequestMergeCapabilities, PullRequestMergeMethod,
    PullRequestReviewCapabilities, PullRequestReviewerCandidateList, PullRequestReviewerCapabilities, PullRequestState, PullRequestViewedFilesStore,
    PullRequestViewerPermissions, SourceControlProviderKind,
};
use zc_sourcecontrol::azure::{AzureDevOpsCli, AzureDevOpsCliError, AzureDevOpsCliErrorKind};
use zc_sourcecontrol::errors::Cause;

use crate::azure::cli::{
    AzureDevOpsIterationChanges, AzureDevOpsPullRequestCli, AzureDevOpsPullRequestCliApi, AzureDevOpsPullRequestCliError, ListPullRequestsInput,
};
use crate::azure::diff::{
    azure_devops_file_patch, azure_devops_unreadable_file_patch, byte_length, format_azure_devops_diff_cursor, parse_azure_devops_diff_cursor,
    AzureDevOpsDiffCursor, AzureDevOpsFileTexts, MAX_DIFF_SLICE_BYTES, MAX_DIFF_SLICE_EDITS, MAX_DIFF_SLICE_FILES, MAX_FILE_DIFF_EDITS,
};
use crate::azure::json::{AzureDevOpsChangeKind, AzureDevOpsItemContent, AzureDevOpsIteration, AzureDevOpsPullRequest, AzureDevOpsRepositoryLocation};
use crate::provider::*;

/// How many of a slice's files are read at once. Every file is two `az` invocations, each paying
/// a Python interpreter's start-up; four files is eight processes at once.
const DIFF_FILE_CONCURRENCY: usize = 4;

/// How many `az` processes the diff reads have out at once across every reader of this provider
/// (built once with the registry), so a second reader waits behind the first.
pub const MAX_DIFF_SPAWNS: usize = 2 * DIFF_FILE_CONCURRENCY;

/// How many pull requests' repository locations one provider remembers at once.
pub const LOCATION_CACHE_CAPACITY: usize = 128;

const KIND: SourceControlProviderKind = SourceControlProviderKind::AzureDevops;

/// `CAPABILITIES`.
fn capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        // Azure serves no patch of its own, so the one the Code tab reads is built here.
        diff: true,
        // Posting a remark is not something this can claim without having run it.
        comment: false,
        actions: vec![
            PullRequestAction::Merge,
            PullRequestAction::Ready,
            PullRequestAction::Draft,
            PullRequestAction::Close,
            PullRequestAction::Reopen,
            PullRequestAction::EnableAutoMerge,
            PullRequestAction::DisableAutoMerge,
        ],
        // Azure squashes as a completion option; it has no rebase strategy of its own.
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash],
        update_methods: None,
        // `az repos pr list` filters by status, creator, reviewer and branch, and by no text.
        search: false,
        reactions: Some(false),
        // Azure keeps a viewed record only behind an undocumented, iteration-keyed endpoint, so
        // the marks are kept in this environment.
        viewed_files: Some(PullRequestViewedFilesStore::Environment),
        review: PullRequestReviewCapabilities {
            inline_comment: false,
            reply: false,
            resolve: false,
            verdicts: Vec::new(),
        },
        // `az repos pr reviewer add|remove` name identities; nothing in `az repos` lists them.
        reviewers: PullRequestReviewerCapabilities {
            request: true,
            list_candidates: false,
        },
        // A new title and description travel on `az repos pr update`; remarks cannot be written.
        edit: Some(PullRequestEditCapabilities {
            change_request: true,
            comment: false,
        }),
        stacks: None,
        stack_actions: None,
        labels: None,
    }
}

/// Everything this host offers, granted to whoever is signed in: Azure states no permission
/// anywhere a pull request read reaches, so a viewer who may not act is told so by Azure when
/// they try rather than having the control hidden.
fn viewer_permissions(capabilities: &PullRequestCapabilities) -> PullRequestViewerPermissions {
    PullRequestViewerPermissions {
        stack_rebase: None,
        actions: capabilities.actions.clone(),
        comment: capabilities.comment,
        resolve: capabilities.review.resolve,
        verdicts: capabilities.review.verdicts.clone(),
        request_reviewers: capabilities.reviewers.request,
        update_methods: None,
        labels: None,
    }
}

/// `azureDevOpsProviderFailure`: the CLI tags that mean the tool itself is unusable, rather than
/// one request failing.
pub fn azure_devops_provider_failure(error: &AzureDevOpsPullRequestCliError) -> ProviderFailureReason {
    match error.cli_kind() {
        Some(AzureDevOpsCliErrorKind::Unavailable { .. }) => ProviderFailureReason::MissingTool,
        Some(AzureDevOpsCliErrorKind::Authentication { .. }) => ProviderFailureReason::Unauthenticated,
        Some(AzureDevOpsCliErrorKind::RateLimit { .. }) => ProviderFailureReason::RateLimited,
        _ => ProviderFailureReason::Failed,
    }
}

/// `fail(operation)`.
fn fail(operation: &str) -> impl Fn(AzureDevOpsPullRequestCliError) -> PullRequestProviderError + '_ {
    move |error| PullRequestProviderError::new(KIND, operation, azure_devops_provider_failure(&error), error.detail()).with_cause(Cause::new(error))
}

/// Refuses what the capabilities already say this host cannot do.
fn unsupported<T>(operation: &str) -> ProviderResult<T> {
    Err(PullRequestProviderError::failed(
        KIND,
        operation,
        "Azure DevOps reviews cannot be written from here yet.",
    ))
}

/// The closing time where the state says it closed that way.
fn terminal_at(pull_request: &AzureDevOpsPullRequest, state: PullRequestState) -> Option<String> {
    if pull_request.state == state {
        pull_request.closed_at.clone()
    } else {
        None
    }
}

fn to_change_request(pull_request: &AzureDevOpsPullRequest) -> ProviderChangeRequest {
    ProviderChangeRequest {
        stack: None,
        number: pull_request.number,
        title: pull_request.title.clone(),
        url: pull_request.url.clone(),
        author: pull_request.author.clone(),
        head_branch: pull_request.head_branch.clone(),
        head_repository_name_with_owner: None,
        base_branch: pull_request.base_branch.clone(),
        state: pull_request.state,
        is_draft: pull_request.is_draft,
        mergeability: pull_request.mergeability,
        // Azure counts a pull request's files but never its lines.
        additions: 0,
        deletions: 0,
        created_at: pull_request.created_at.clone(),
        closed_at: Some(terminal_at(pull_request, PullRequestState::Closed)),
        merged_at: Some(terminal_at(pull_request, PullRequestState::Merged)),
        updated_at: pull_request.updated_at.clone(),
        review_request_logins: pull_request.review_request_logins.clone(),
        // Azure keeps labels on work items rather than on the pull request.
        labels: Vec::new(),
        review_decision: None,
        checks_state: None,
    }
}

/// Where a pull request's repository lives and which pushes it has had.
struct DiffScope {
    location: AzureDevOpsRepositoryLocation,
    iterations: Vec<AzureDevOpsIteration>,
}

/// A least recently used map, oldest first (the TS `Map` re-inserted on every hit).
#[derive(Default)]
struct LocationCache {
    entries: Vec<(String, AzureDevOpsRepositoryLocation)>,
}

impl LocationCache {
    fn get(&mut self, key: &str) -> Option<AzureDevOpsRepositoryLocation> {
        let at = self.entries.iter().position(|(held, _)| held == key)?;
        let entry = self.entries.remove(at);
        let location = entry.1.clone();
        self.entries.push(entry);
        Some(location)
    }

    fn insert(&mut self, key: String, location: AzureDevOpsRepositoryLocation) {
        if self.entries.len() >= LOCATION_CACHE_CAPACITY && !self.entries.is_empty() {
            self.entries.remove(0);
        }
        match self.entries.iter_mut().find(|(held, _)| *held == key) {
            Some(entry) => entry.1 = location,
            None => self.entries.push((key, location)),
        }
    }
}

/// The Azure DevOps [`PullRequestProviderApi`].
pub struct AzureDevOpsPullRequestProvider {
    cli: Arc<dyn AzureDevOpsPullRequestCliApi>,
    capabilities: PullRequestCapabilities,
    permissions: PullRequestViewerPermissions,
    /// Made once with the provider, so this is the whole build's allowance of diff reads.
    diff_spawns: Semaphore,
    /// A pull request cannot move between repositories, so where it lives is remembered.
    locations: Mutex<LocationCache>,
}

impl AzureDevOpsPullRequestProvider {
    /// The provider over the shared `az` wrapper of `zc_sourcecontrol::SourceControl::azure`.
    pub fn new(azure: AzureDevOpsCli) -> Self {
        Self::with_cli(Arc::new(AzureDevOpsPullRequestCli::new(azure)))
    }

    /// The provider over any implementation of the pull request CLI (tests mock it).
    pub fn with_cli(cli: Arc<dyn AzureDevOpsPullRequestCliApi>) -> Self {
        let capabilities = capabilities();
        Self {
            cli,
            permissions: viewer_permissions(&capabilities),
            capabilities,
            diff_spawns: Semaphore::new(MAX_DIFF_SPAWNS),
            locations: Mutex::default(),
        }
    }

    async fn read_item_content(
        &self,
        cwd: &str,
        location: &AzureDevOpsRepositoryLocation,
        path: &str,
        commit: &str,
    ) -> Result<AzureDevOpsItemContent, AzureDevOpsPullRequestCliError> {
        let _permit = self.diff_spawns.acquire().await.expect("the diff semaphore is never closed");
        self.cli.read_item_content(cwd, location, path, commit).await
    }

    /// `locationOf`: least recently used, so a listing walking cold pull requests cannot evict the
    /// one being read.
    async fn location_of(&self, cwd: &str, number: i64) -> Result<Option<AzureDevOpsRepositoryLocation>, AzureDevOpsPullRequestCliError> {
        let key = format!("{cwd} {number}");
        if let Some(held) = self.locations.lock().expect("location cache").get(&key) {
            return Ok(Some(held));
        }
        let pull_request = self.cli.get_pull_request(cwd, number).await?;
        let Some(location) = pull_request.location else { return Ok(None) };
        self.locations.lock().expect("location cache").insert(key, location.clone());
        Ok(Some(location))
    }

    /// `diffScope`: the iterations are read afresh every time, since the newest one is what a
    /// push adds.
    async fn diff_scope(&self, cwd: &str, number: i64) -> Result<Option<DiffScope>, AzureDevOpsPullRequestCliError> {
        let Some(location) = self.location_of(cwd, number).await? else {
            return Ok(None);
        };
        let iterations = self.cli.list_iterations(cwd, &location, number).await?;
        Ok(Some(DiffScope { location, iterations }))
    }

    /// `readTexts`: both sides of one changed file at once, asking only for the sides the change
    /// has (Azure answers for a file absent at a commit with a failure).
    async fn read_texts(
        &self,
        cwd: &str,
        location: &AzureDevOpsRepositoryLocation,
        iteration: &AzureDevOpsIteration,
        change_kind: AzureDevOpsChangeKind,
        path: &str,
        old_path: &str,
    ) -> Result<AzureDevOpsFileTexts, AzureDevOpsPullRequestCliError> {
        let old_side = async {
            if change_kind == AzureDevOpsChangeKind::New {
                Ok(AzureDevOpsItemContent::default())
            } else {
                self.read_item_content(cwd, location, old_path, &iteration.merge_base_commit).await
            }
        };
        let new_side = async {
            if change_kind == AzureDevOpsChangeKind::Deleted {
                Ok(AzureDevOpsItemContent::default())
            } else {
                self.read_item_content(cwd, location, path, &iteration.head_commit).await
            }
        };
        let (old_item, new_item) = try_join(old_side, new_side).await?;
        Ok(AzureDevOpsFileTexts {
            // Azure hands a file it calls binary over in an encoding of its own, so its word on
            // that is taken.
            binary: old_item.is_binary || new_item.is_binary,
            old_contents: old_item.contents,
            new_contents: new_item.contents,
        })
    }

    /// `listLatestChanges`: the newest iteration's changes are the whole of the change.
    async fn list_latest_changes(
        &self,
        cwd: &str,
        location: &AzureDevOpsRepositoryLocation,
        number: i64,
        iterations: &[AzureDevOpsIteration],
    ) -> Result<AzureDevOpsIterationChanges, AzureDevOpsPullRequestCliError> {
        match iterations.last() {
            None => Ok(AzureDevOpsIterationChanges::default()),
            Some(latest) => self.cli.list_iteration_changes(cwd, location, number, latest.id).await,
        }
    }

    async fn changed_files(&self, cwd: &str, location: &AzureDevOpsRepositoryLocation, number: i64) -> Result<i64, AzureDevOpsPullRequestCliError> {
        let iterations = self.cli.list_iterations(cwd, location, number).await?;
        let listed = self.list_latest_changes(cwd, location, number, &iterations).await?;
        Ok(listed.changes.len() as i64)
    }

    async fn read_diff(&self, input: &GetDiffInput) -> Result<ProviderDiffSlice, AzureDevOpsPullRequestCliError> {
        let change_request = &input.change_request;
        let cwd = change_request.cwd.as_str();
        let Some(scope) = self.diff_scope(cwd, change_request.number).await? else {
            return Ok(ProviderDiffSlice::default());
        };
        let cursor = parse_azure_devops_diff_cursor(input.cursor.as_deref());
        // Reading on stays with the push the first slice was taken against.
        let iteration = match cursor {
            None => scope.iterations.last(),
            Some(cursor) => scope.iterations.iter().find(|candidate| candidate.id == cursor.iteration_id),
        };
        let Some(iteration) = iteration else {
            return Ok(ProviderDiffSlice::default());
        };
        let listed = self
            .cli
            .list_iteration_changes(cwd, &scope.location, change_request.number, iteration.id)
            .await?;
        let changes = listed.changes;
        let location = &scope.location;

        let mut sections: Vec<String> = Vec::new();
        let mut truncated = listed.truncated;
        let mut bytes = 0usize;
        let mut edits = 0usize;
        let mut index = cursor.map_or(0, |cursor| cursor.file_index.max(0) as usize);
        let mut full = false;
        // How many files to read at once: what is left of each budget over what a file has spent
        // of it on average so far, and at least one so a file heavier than the budget still moves
        // the cursor.
        let batch_width = |sections: usize, bytes: usize, edits: usize| -> usize {
            if sections == 0 {
                return DIFF_FILE_CONCURRENCY;
            }
            let admits = |left: f64, spent: f64| (left / (spent / sections as f64).max(1.0)).ceil();
            let width = (DIFF_FILE_CONCURRENCY as f64)
                .min((MAX_DIFF_SLICE_FILES - sections) as f64)
                .min(admits(MAX_DIFF_SLICE_BYTES as f64 - bytes as f64, bytes as f64))
                .min(admits(MAX_DIFF_SLICE_EDITS as f64 - MAX_FILE_DIFF_EDITS as f64 - edits as f64, edits as f64));
            width.max(1.0) as usize
        };
        while !full && index < changes.len() {
            let width = batch_width(sections.len(), bytes, edits);
            let batch = &changes[index..(index + width).min(changes.len())];
            let read = try_join_all(batch.iter().map(|change| async move {
                match self
                    .read_texts(cwd, location, iteration, change.change_kind, &change.path, &change.old_path)
                    .await
                {
                    Ok(texts) => Ok((change, Some(texts))),
                    // One file's problem: a pair Azure refuses leaves that file listed without its
                    // hunks. A signed-out CLI, a rate limit or no `az` is the read failing.
                    Err(
                        AzureDevOpsPullRequestCliError::Read { .. }
                        | AzureDevOpsPullRequestCliError::Cli(AzureDevOpsCliError {
                            kind: AzureDevOpsCliErrorKind::PullRequestNotFound { .. } | AzureDevOpsCliErrorKind::CommandFailed { .. },
                            ..
                        }),
                    ) => Ok((change, None)),
                    Err(error) => Err(error),
                }
            }))
            .await?;
            for (change, texts) in read {
                // The diff is synchronous, so the thread is handed back between files.
                tokio::task::yield_now().await;
                let file = match texts {
                    None => azure_devops_unreadable_file_patch(change),
                    Some(texts) => azure_devops_file_patch(change, &texts),
                };
                bytes += byte_length(&file.section);
                edits += file.edits;
                truncated = truncated || file.truncated;
                sections.push(file.section);
                index += 1;
                // Checked after the file is added, so every slice carries at least one.
                if bytes >= MAX_DIFF_SLICE_BYTES
                    || edits + MAX_FILE_DIFF_EDITS > MAX_DIFF_SLICE_EDITS
                    || sections.len() >= MAX_DIFF_SLICE_FILES
                    || file.abandoned
                {
                    full = true;
                    break;
                }
            }
        }

        Ok(ProviderDiffSlice {
            patch: sections.concat(),
            truncated,
            next_cursor: (index < changes.len()).then(|| {
                format_azure_devops_diff_cursor(AzureDevOpsDiffCursor {
                    iteration_id: iteration.id,
                    file_index: index as i64,
                })
            }),
            omitted_file_stats: None,
        })
    }

    async fn read_file_revisions(&self, input: &FileRevisionsInput) -> Result<ProviderFileRevisions, AzureDevOpsPullRequestCliError> {
        let mut revisions: Vec<(String, String)> = Vec::new();
        if input.paths.is_empty() {
            return Ok(ProviderFileRevisions { revisions, complete: None });
        }
        let change_request = &input.change_request;
        let cwd = change_request.cwd.as_str();
        let Some(scope) = self.diff_scope(cwd, change_request.number).await? else {
            return Ok(ProviderFileRevisions { revisions, complete: None });
        };
        let listed = self.list_latest_changes(cwd, &scope.location, change_request.number, &scope.iterations).await?;
        for change in &listed.changes {
            let Some(object_id) = &change.object_id else { continue };
            if !input.paths.contains(&change.path) {
                continue;
            }
            match revisions.iter_mut().find(|(path, _)| *path == change.path) {
                Some(entry) => entry.1 = object_id.clone(),
                None => revisions.push((change.path.clone(), object_id.clone())),
            }
        }
        // A path the change does not carry is at the empty revision, where a deleted file sits,
        // unless the change was too long to follow: then it is left out rather than cleared.
        if !listed.truncated {
            for path in &input.paths {
                if !revisions.iter().any(|(held, _)| held == path) {
                    revisions.push((path.clone(), String::new()));
                }
            }
        }
        Ok(ProviderFileRevisions { revisions, complete: None })
    }
}

#[async_trait]
impl PullRequestProviderApi for AzureDevOpsPullRequestProvider {
    fn kind(&self) -> SourceControlProviderKind {
        KIND
    }

    fn capabilities(&self) -> &PullRequestCapabilities {
        &self.capabilities
    }

    fn optional_methods(&self) -> OptionalMethods {
        OptionalMethods {
            get_change_request_summary: true,
            get_diff_file_contents: true,
            get_file_revisions: true,
            update_change_request: true,
            ..OptionalMethods::default()
        }
    }

    async fn get_viewer(&self, input: ProviderHostRef) -> ProviderResult<String> {
        self.cli.get_viewer(&input.cwd).await.map_err(fail("getViewer"))
    }

    /// `input.query` is dropped: `az repos pr list` has nothing that matches text, so the page
    /// comes back unnarrowed and the caller filters it.
    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> ProviderResult<ProviderChangeRequestPage> {
        let batch = self
            .cli
            .list_pull_requests(ListPullRequestsInput {
                cwd: input.cwd,
                repository: input.repository,
                state: input.state,
                involvement: input.involvement,
                viewer: input.viewer,
                limit: input.limit,
                cursor: input.cursor,
            })
            .await
            .map_err(fail("listChangeRequests"))?;
        Ok(ProviderChangeRequestPage {
            items: batch.items.iter().map(to_change_request).collect(),
            truncated: batch.truncated,
            cursor_advance: Some(batch.cursor_advance),
            // Azure answers in one order whether or not it is being carried on from.
            continues: true,
        })
    }

    async fn get_change_request(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestDetail> {
        let pull_request = self.cli.get_pull_request(&input.cwd, input.number).await.map_err(fail("getChangeRequest"))?;
        // The file count is two reads past the pull request and the only thing riding on them,
        // so a failure leaves it unknown rather than losing the whole detail.
        let changed_files = match &pull_request.location {
            None => 0,
            Some(location) => self.changed_files(&input.cwd, location, input.number).await.unwrap_or(0),
        };
        Ok(ProviderChangeRequestDetail {
            change_request: to_change_request(&pull_request),
            body: pull_request.body.clone(),
            changed_files,
            merged_at: terminal_at(&pull_request, PullRequestState::Merged),
            closed_at: terminal_at(&pull_request, PullRequestState::Closed),
            reviewers: pull_request.reviewers.clone(),
            checks: Vec::new(),
            merge_capabilities: PullRequestMergeCapabilities {
                merge: true,
                squash: true,
                rebase: false,
            },
            viewer_permissions: self.permissions.clone(),
            base_comparison: None,
            behind_by: None,
            auto_merge_enabled: Some(pull_request.auto_merge_enabled),
            auto_merge_method: pull_request.auto_merge_method,
            workflow_approvals_required: None,
        })
    }

    /// The polled path a linked thread's row stays live on: one `az` read.
    async fn get_change_request_summary(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestSummary> {
        let pull_request = self
            .cli
            .get_pull_request(&input.cwd, input.number)
            .await
            .map_err(fail("getChangeRequestSummary"))?;
        Ok(ProviderChangeRequestSummary {
            number: pull_request.number,
            title: pull_request.title.clone(),
            url: pull_request.url.clone(),
            head_branch: pull_request.head_branch.clone(),
            base_branch: pull_request.base_branch.clone(),
            state: pull_request.state,
            is_draft: Some(pull_request.is_draft),
            closed_at: Some(terminal_at(&pull_request, PullRequestState::Closed)),
            merged_at: Some(terminal_at(&pull_request, PullRequestState::Merged)),
            updated_at: pull_request.updated_at.clone(),
            author: Some(pull_request.author.clone()),
            additions: None,
            deletions: None,
            changed_files: None,
            review_decision: None,
            checks_state: None,
            mergeability: Some(pull_request.mergeability),
        })
    }

    async fn get_change_request_activity(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestActivity> {
        let pull_request = self
            .cli
            .get_pull_request(&input.cwd, input.number)
            .await
            .map_err(fail("getChangeRequestActivity"))?;
        let (comments, truncated) = match &pull_request.location {
            None => (Vec::new(), true),
            Some(location) => match self.cli.list_threads(&input.cwd, location, input.number).await {
                Ok(comments) => (comments, false),
                Err(_) => (Vec::new(), true),
            },
        };
        Ok(ProviderChangeRequestActivity {
            author: None,
            reviewers: None,
            comment_count: comments.len() as i64,
            comments,
            comments_truncated: truncated,
            review_threads: Vec::new(),
            commits: Vec::new(),
            reactions: None,
        })
    }

    /// No request at all: Azure has nothing to say about the viewer that a read can reach.
    async fn get_viewer_permissions(&self, _input: ViewerPermissionsInput) -> ProviderResult<PullRequestViewerPermissions> {
        Ok(self.permissions.clone())
    }

    /// `input.commit` is dropped: Azure states no commit list on a pull request, so the Code tab
    /// always asks for the whole change.
    async fn get_diff(&self, input: GetDiffInput) -> ProviderResult<ProviderDiffSlice> {
        self.read_diff(&input).await.map_err(fail("getDiff"))
    }

    /// The patch is built from whole files, so expanding around a hunk is the same two reads,
    /// against the latest iteration.
    async fn get_diff_file_contents(&self, input: DiffFileContentsInput) -> ProviderResult<ProviderDiffFileContents> {
        let change_request = &input.change_request;
        let read = async {
            let scope = self.diff_scope(&change_request.cwd, change_request.number).await?;
            let Some((scope, iteration)) = scope.as_ref().and_then(|scope| Some((scope, scope.iterations.last()?))) else {
                return Ok(ProviderDiffFileContents::default());
            };
            let texts = self
                .read_texts(
                    &change_request.cwd,
                    &scope.location,
                    iteration,
                    input.change_type,
                    &input.new_path,
                    &input.old_path,
                )
                .await?;
            Ok(ProviderDiffFileContents {
                old_contents: texts.old_contents,
                new_contents: texts.new_contents,
            })
        };
        read.await.map_err(fail("getDiffFileContents"))
    }

    /// What the head has of each marked file: the blob Azure names on the latest iteration's
    /// change.
    async fn get_file_revisions(&self, input: FileRevisionsInput) -> ProviderResult<ProviderFileRevisions> {
        self.read_file_revisions(&input).await.map_err(fail("getFileRevisions"))
    }

    async fn run_action(&self, input: RunActionInput) -> ProviderResult<()> {
        let change_request = &input.change_request;
        self.cli
            .run_pull_request_action(&change_request.cwd, change_request.number, input.action, input.merge_method)
            .await
            .map_err(fail("runAction"))
    }

    async fn update_change_request(&self, input: UpdateChangeRequestInput) -> ProviderResult<()> {
        let change_request = &input.change_request;
        self.cli
            .update_pull_request(&change_request.cwd, change_request.number, input.title.as_deref(), input.body.as_deref())
            .await
            .map_err(fail("updateChangeRequest"))
    }

    /// Never called: `capabilities.comment` is false.
    async fn comment(&self, _input: CommentInput) -> ProviderResult<()> {
        unsupported("comment")
    }

    async fn submit_review(&self, _input: SubmitReviewInput) -> ProviderResult<()> {
        unsupported("submitReview")
    }

    /// Never called: `capabilities.reviewers.listCandidates` is false.
    async fn list_reviewer_candidates(&self, _input: ChangeRequestRef) -> ProviderResult<PullRequestReviewerCandidateList> {
        Err(PullRequestProviderError::failed(
            KIND,
            "listReviewerCandidates",
            "Azure DevOps cannot say who may review a pull request.",
        ))
    }

    /// Azure names an identity by an email address or a guid and has no team to ask, so a
    /// candidate's id is the whole of what it takes.
    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> ProviderResult<()> {
        let change_request = &input.change_request;
        let reviewers: Vec<String> = input.reviewers.iter().map(|reviewer| reviewer.id.clone()).collect();
        self.cli
            .set_pull_request_reviewers(&change_request.cwd, change_request.number, &reviewers, input.requested)
            .await
            .map_err(fail("setReviewerRequest"))
    }

    async fn reply_to_thread(&self, _input: ReplyToThreadInput) -> ProviderResult<()> {
        unsupported("replyToThread")
    }

    async fn set_reaction(&self, _input: SetReactionInput) -> ProviderResult<()> {
        unsupported("setReaction")
    }

    async fn set_thread_resolution(&self, _input: SetThreadResolutionInput) -> ProviderResult<()> {
        unsupported("setThreadResolution")
    }
}
