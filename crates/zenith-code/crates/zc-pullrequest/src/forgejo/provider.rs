//! `pullRequest/ForgejoPullRequestProvider.ts`: Forgejo and Gitea pull requests through the REST
//! API, called with zc-sourcecontrol's shared [`ForgejoCli`] (`fj`'s token or `tea api`).

use std::sync::OnceLock;

use async_trait::async_trait;
use futures::{StreamExt, TryStreamExt};
use regex::Regex;
use serde_json::{json, Map, Value};
use zc_contracts::{
    PullRequestAction, PullRequestBaseComparison, PullRequestCapabilities, PullRequestComment, PullRequestCommentKind,
    PullRequestDiffFileContentsInputChangeType, PullRequestEditCapabilities, PullRequestLabelCandidate, PullRequestLabelCandidateList,
    PullRequestMergeCapabilities, PullRequestMergeMethod, PullRequestReviewCapabilities, PullRequestReviewPosition, PullRequestReviewVerdict,
    PullRequestReviewerCandidate, PullRequestReviewerCandidateList, PullRequestReviewerCapabilities, PullRequestReviewerKind, PullRequestThreadComment,
    PullRequestUpdateMethod, PullRequestViewedFilesStore, PullRequestViewerPermissions, SourceControlProviderKind,
};
use zc_core::vcs_process::VcsProcessOutput;
use zc_sourcecontrol::errors::Cause;
use zc_sourcecontrol::forgejo::cli::{forgejo_repository_path, ForgejoApiInput, ForgejoRepositoryInput};
use zc_sourcecontrol::forgejo::{ForgejoCli, ForgejoCliError, ForgejoErrorReason};
use zc_sourcecontrol::github::cli::SchemaDecodeError;
use zc_sourcecontrol::util::encode_uri_component;

use super::diff_revisions::parse_diff_file_revisions;
use super::json::{
    decode_comment, decode_commit, decode_label, decode_pull_request, decode_reaction, decode_repository, decode_review, decode_review_comment,
    decode_review_fields, decode_status, decode_user, decode_user_item, forgejo_actor, forgejo_change_request, forgejo_checks, forgejo_comment, forgejo_commit,
    forgejo_reaction_name, forgejo_reactions, forgejo_review, forgejo_review_thread, ForgejoPullRequest, ForgejoRepository, ForgejoReview,
};
use crate::error::{ProviderFailureReason, PullRequestProviderError};
use crate::gitlab::util::{object, string, Decoded};
use crate::provider::{
    ChangeRequestRef, CommentInput, DiffFileContentsInput, FileRevisionsInput, GetDiffInput, ListChangeRequestsInput, OptionalMethods,
    ProviderChangeRequestActivity, ProviderChangeRequestDetail, ProviderChangeRequestPage, ProviderChangeRequestSummary, ProviderDiffFileContents,
    ProviderDiffSlice, ProviderFileRevisions, ProviderHostRef, ProviderResult, PullRequestProviderApi, ReplyToThreadInput, RunActionInput, SetLabelsInput,
    SetReactionInput, SetReviewerRequestInput, SetThreadResolutionInput, SubmitReviewInput, UpdateChangeRequestInput, UpdateCommentInput,
    ViewerPermissionsInput,
};

const KIND: SourceControlProviderKind = SourceControlProviderKind::Forgejo;
const ACTIONS: [PullRequestAction; 4] = [
    PullRequestAction::Merge,
    PullRequestAction::Close,
    PullRequestAction::Reopen,
    PullRequestAction::UpdateBranch,
];
const VERDICTS: [PullRequestReviewVerdict; 3] = [
    PullRequestReviewVerdict::Comment,
    PullRequestReviewVerdict::Approve,
    PullRequestReviewVerdict::RequestChanges,
];
/// The rows a paged read stops at.
const PAGE_LIMIT: usize = 500;

/// `CAPABILITIES`. Forgejo's public API has no thread replies, resolution or draft conversion.
pub fn forgejo_capabilities() -> PullRequestCapabilities {
    PullRequestCapabilities {
        diff: true,
        comment: true,
        actions: ACTIONS.to_vec(),
        merge_methods: vec![PullRequestMergeMethod::Merge, PullRequestMergeMethod::Squash, PullRequestMergeMethod::Rebase],
        update_methods: Some(vec![PullRequestUpdateMethod::Merge, PullRequestUpdateMethod::Rebase]),
        search: false,
        reactions: Some(true),
        viewed_files: Some(PullRequestViewedFilesStore::Environment),
        review: PullRequestReviewCapabilities {
            inline_comment: true,
            reply: false,
            resolve: false,
            verdicts: VERDICTS.to_vec(),
        },
        reviewers: PullRequestReviewerCapabilities {
            request: true,
            list_candidates: true,
        },
        edit: Some(PullRequestEditCapabilities {
            change_request: true,
            comment: true,
        }),
        stacks: None,
        stack_actions: None,
        labels: Some(true),
    }
}

/// `permissions`: what the viewer may do, from the repository's `permissions.push`, whether they
/// opened the pull request, and whether the repository is archived.
pub fn forgejo_viewer_permissions(repo: &ForgejoRepository, pr: &ForgejoPullRequest, viewer: &str) -> PullRequestViewerPermissions {
    let can_write = repo.permissions_push.unwrap_or(false);
    let is_author = pr.user.as_ref().is_some_and(|user| user.login == viewer);
    let can_edit = can_write || is_author;
    let active = repo.archived != Some(true);
    PullRequestViewerPermissions {
        stack_rebase: None,
        actions: if active {
            ACTIONS
                .iter()
                .copied()
                .filter(|action| {
                    if matches!(action, PullRequestAction::Merge | PullRequestAction::UpdateBranch) {
                        can_write
                    } else {
                        can_edit
                    }
                })
                .collect()
        } else {
            Vec::new()
        },
        comment: active && (pr.is_locked != Some(true) || can_write),
        resolve: false,
        verdicts: match (active, is_author) {
            (false, _) => Vec::new(),
            (true, true) => vec![PullRequestReviewVerdict::Comment],
            (true, false) => VERDICTS.to_vec(),
        },
        request_reviewers: active && can_edit,
        update_methods: Some(if active && can_write {
            if repo.allow_rebase_update == Some(true) {
                vec![PullRequestUpdateMethod::Merge, PullRequestUpdateMethod::Rebase]
            } else {
                vec![PullRequestUpdateMethod::Merge]
            }
        } else {
            Vec::new()
        }),
        labels: Some(active && can_write),
    }
}

/// `failure(operation, detail, cause?)`.
fn failure(operation: &str, detail: &str) -> PullRequestProviderError {
    PullRequestProviderError::failed(KIND, operation, detail)
}

fn request_failure(path: &str, error: ForgejoCliError) -> PullRequestProviderError {
    let reason = match error.reason {
        Some(ForgejoErrorReason::MissingCli) => ProviderFailureReason::MissingTool,
        Some(ForgejoErrorReason::Authentication) => ProviderFailureReason::Unauthenticated,
        Some(ForgejoErrorReason::RateLimit) => ProviderFailureReason::RateLimited,
        _ => ProviderFailureReason::Failed,
    };
    PullRequestProviderError::new(KIND, path, reason, error.detail.clone()).with_cause(Cause::new(error))
}

/// `repos/<owner>/<repo>`, each segment encoded.
fn repo_path(repository: &str) -> String {
    forgejo_repository_path(repository)
}

fn pull_path(input: &ChangeRequestRef) -> String {
    format!("{}/pulls/{}", repo_path(&input.repository), input.number)
}

fn issue_path(input: &ChangeRequestRef) -> String {
    format!("{}/issues/{}", repo_path(&input.repository), input.number)
}

/// The issue comment a review is also kept as (review ids differ from the issue-comment ids the
/// reactions API takes): `…#issuecomment-<id>`.
fn review_comment_id(review: &ForgejoReview) -> Option<String> {
    static ISSUE_COMMENT: OnceLock<Regex> = OnceLock::new();
    ISSUE_COMMENT
        .get_or_init(|| Regex::new(r"#issuecomment-([1-9][0-9]*)$").expect("valid regex"))
        .captures(review.html_url.as_deref().unwrap_or_default())
        .and_then(|captures| captures.get(1))
        .map(|id| id.as_str().to_owned())
}

/// `Buffer.from(text, "base64")`: white space and characters outside the alphabet skipped,
/// the URL-safe alphabet accepted, decoding stopped at padding, a trailing partial byte dropped.
fn decode_base64_lenient(text: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(text.len() * 3 / 4);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    bytes
}

/// `Schema.NullOr(Schema.Array(item))`: `null` is no rows.
fn decode_rows<T>(value: &Value, item: fn(&Value) -> Decoded<T>) -> Decoded<Vec<T>> {
    match value {
        Value::Null => Ok(Vec::new()),
        value => crate::gitlab::util::array(value, item),
    }
}

fn nullable_pull(value: &Value) -> Decoded<Option<ForgejoPullRequest>> {
    match value {
        Value::Null => Ok(None),
        value => decode_pull_request(object(value)?).map(Some),
    }
}

fn pull_item(value: &Value) -> Decoded<ForgejoPullRequest> {
    decode_pull_request(object(value)?)
}

fn repository_item(value: &Value) -> Decoded<ForgejoRepository> {
    decode_repository(object(value)?)
}

/// A JSON read failed to parse or did not match its schema: the cause the TS keeps.
fn decode_failure(operation: &str, cause: String) -> PullRequestProviderError {
    failure(operation, "Forgejo returned an invalid response.").with_cause(Cause::new(SchemaDecodeError(cause)))
}

fn parse<T>(operation: &str, stdout: &str, decode: impl FnOnce(&Value) -> Decoded<T>) -> ProviderResult<T> {
    let value: Value = serde_json::from_str(stdout).map_err(|error| decode_failure(operation, error.to_string()))?;
    decode(&value).map_err(|_| decode_failure(operation, "The response does not match the expected schema".into()))
}

/// The paging links of a `--include` response (`link:` header), and whether one names a next page.
fn has_next_page(stderr: &str) -> Option<bool> {
    static LINK: OnceLock<Regex> = OnceLock::new();
    static NEXT: OnceLock<Regex> = OnceLock::new();
    let links = LINK
        .get_or_init(|| Regex::new(r"(?im)^link:\s*(.*)$").expect("valid regex"))
        .captures(stderr)?
        .get(1)?
        .as_str()
        .to_owned();
    Some(NEXT.get_or_init(|| Regex::new(r#"(?i)rel="?next"?"#).expect("valid regex")).is_match(&links))
}

/// The Forgejo pull request provider (`ForgejoPullRequestProvider.make`).
#[derive(Clone)]
pub struct ForgejoPullRequestProvider {
    cli: ForgejoCli,
    capabilities: PullRequestCapabilities,
}

/// Where a call goes: the checkout, and the repository and host when the call has them.
fn target(cwd: &str, repository: Option<&str>, host: Option<&str>) -> ForgejoRepositoryInput {
    ForgejoRepositoryInput {
        cwd: cwd.to_owned(),
        context: None,
        repository: repository.map(str::to_owned),
        reference: None,
        host: host.map(str::to_owned),
    }
}

fn ref_target(input: &ChangeRequestRef) -> ForgejoRepositoryInput {
    target(&input.cwd, Some(&input.repository), Some(&input.host))
}

impl ForgejoPullRequestProvider {
    /// Built over the shared `fj`/`tea` client ([`zc_sourcecontrol::SourceControl::forgejo`]).
    pub fn new(forgejo: ForgejoCli) -> Self {
        Self {
            cli: forgejo,
            capabilities: forgejo_capabilities(),
        }
    }

    async fn request(&self, target: &ForgejoRepositoryInput, path: String, method: Option<&str>, body: Option<Value>) -> ProviderResult<VcsProcessOutput> {
        let input = ForgejoApiInput {
            target: target.clone(),
            path,
            method: method.map(str::to_owned),
            body,
        };
        self.cli.api(&input).await.map_err(|error| request_failure(&input.path, error))
    }

    async fn write(&self, target: &ForgejoRepositoryInput, path: String, method: &str, body: Option<Value>) -> ProviderResult<()> {
        self.request(target, path, Some(method), body).await.map(drop)
    }

    async fn read<T>(&self, target: &ForgejoRepositoryInput, path: String, decode: impl FnOnce(&Value) -> Decoded<T>) -> ProviderResult<T> {
        let result = self.request(target, path.clone(), None, None).await?;
        if result.stdout_truncated {
            return Err(failure(&path, "Forgejo response exceeded the output limit."));
        }
        parse(&path, &result.stdout, decode)
    }

    async fn read_array<T>(&self, target: &ForgejoRepositoryInput, path: String, item: fn(&Value) -> Decoded<T>) -> ProviderResult<Vec<T>> {
        self.read(target, path, |value| decode_rows(value, item)).await
    }

    /// One page of fifty, and whether there are more (rows came back and the `link` header, when
    /// there is one, names a next page).
    async fn read_page<T>(&self, target: &ForgejoRepositoryInput, path: &str, item: fn(&Value) -> Decoded<T>, index: usize) -> ProviderResult<(Vec<T>, bool)> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let result = self.request(target, format!("{path}{separator}limit=50&page={index}"), None, None).await?;
        if result.stdout_truncated {
            return Err(failure(path, "Forgejo response exceeded the output limit."));
        }
        let rows = parse(path, &result.stdout, |value| decode_rows(value, item))?;
        let more = !rows.is_empty() && has_next_page(&result.stderr).unwrap_or(true);
        Ok((rows, more))
    }

    /// Pages of fifty (which also works with Forgejo's default maximum) until the host runs out or
    /// `limit` rows are in; `true` when it stopped at the limit.
    async fn page<T>(&self, target: &ForgejoRepositoryInput, path: String, item: fn(&Value) -> Decoded<T>) -> ProviderResult<(Vec<T>, bool)> {
        let mut items = Vec::new();
        let mut index = 1;
        while items.len() < PAGE_LIMIT {
            let (rows, more) = self.read_page(target, &path, item, index).await?;
            items.extend(rows);
            if !more {
                return Ok((items, false));
            }
            index += 1;
        }
        Ok((items, true))
    }

    async fn get_pull(&self, input: &ChangeRequestRef) -> ProviderResult<ForgejoPullRequest> {
        self.read(&ref_target(input), pull_path(input), pull_item).await
    }

    async fn get_repo(&self, input: &ChangeRequestRef) -> ProviderResult<ForgejoRepository> {
        self.read(&ref_target(input), repo_path(&input.repository), repository_item).await
    }

    async fn viewer_of(&self, target: &ForgejoRepositoryInput) -> ProviderResult<String> {
        self.read(target, "user".into(), |value| decode_user(object(value)?))
            .await
            .map(|user| user.login)
    }

    fn unsupported(operation: &str) -> PullRequestProviderError {
        failure(operation, &format!("Forgejo does not expose {operation} through its API."))
    }
}

#[async_trait]
impl PullRequestProviderApi for ForgejoPullRequestProvider {
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
            update_comment: true,
            list_label_candidates: true,
            set_labels: true,
            ..OptionalMethods::default()
        }
    }

    async fn get_viewer(&self, input: ProviderHostRef) -> ProviderResult<String> {
        self.viewer_of(&target(&input.cwd, None, input.host.as_deref())).await
    }

    /// Recently updated first; the service's row offset becomes a page number once the host's
    /// real page size is known (self-hosted servers may cap pages below fifty). There is no
    /// server-side search.
    async fn list_change_requests(&self, input: ListChangeRequestsInput) -> ProviderResult<ProviderChangeRequestPage> {
        let offset = input.cursor.as_ref().map_or(0, |cursor| cursor.delivered).max(0) as usize;
        let state = match input.state {
            zc_contracts::PullRequestListState::Merged => "closed",
            other => other.as_str(),
        };
        let target = target(&input.cwd, Some(&input.repository), Some(&input.host));
        let path = format!("{}/pulls?state={state}&sort=recentupdate", repo_path(&input.repository));
        let first = self.read_page(&target, &path, nullable_pull, 1).await?;
        let page_size = if first.0.is_empty() { 50 } else { first.0.len() };
        let first_index = offset / page_size + 1;
        let limit = input.limit.max(0) as usize;
        let mut items = Vec::new();
        let mut consumed = 0;
        let mut more = true;
        let mut first = Some(first);
        let mut index = first_index;
        while more && consumed < limit {
            let (rows, page_more) = match (index, first.take()) {
                (1, Some(first)) => first,
                _ => self.read_page(&target, &path, nullable_pull, index).await?,
            };
            let start = if index == first_index { offset % page_size } else { 0 };
            let count_before = consumed;
            for row in rows.iter().skip(start) {
                if consumed >= limit {
                    break;
                }
                consumed += 1;
                if let Some(row) = row {
                    items.push(forgejo_change_request(row));
                }
            }
            more = page_more || rows.len() as i64 - start as i64 > (consumed - count_before) as i64;
            index += 1;
        }
        Ok(ProviderChangeRequestPage {
            items,
            truncated: more,
            cursor_advance: Some(consumed as i64),
            continues: true,
        })
    }

    async fn get_change_request_summary(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestSummary> {
        let change_request = forgejo_change_request(&self.get_pull(&input).await?);
        Ok(ProviderChangeRequestSummary {
            number: change_request.number,
            title: change_request.title,
            url: change_request.url,
            head_branch: change_request.head_branch,
            base_branch: change_request.base_branch,
            state: change_request.state,
            is_draft: Some(change_request.is_draft),
            closed_at: change_request.closed_at,
            merged_at: change_request.merged_at,
            updated_at: change_request.updated_at,
            author: Some(change_request.author),
            additions: Some(change_request.additions),
            deletions: Some(change_request.deletions),
            changed_files: None,
            review_decision: None,
            checks_state: None,
            mergeability: Some(change_request.mergeability),
        })
    }

    async fn get_change_request(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestDetail> {
        let target = ref_target(&input);
        let (pr, repo, viewer) = futures::try_join!(self.get_pull(&input), self.get_repo(&input), self.viewer_of(&target))?;
        let statuses_path = format!(
            "{}/statuses/{}?sort=recentupdate",
            repo_path(&input.repository),
            encode_uri_component(&pr.head.sha)
        );
        let (statuses, _) = self.page(&target, statuses_path, decode_status).await?;
        let change_request = forgejo_change_request(&pr);
        let (closed_at, merged_at) = (change_request.closed_at.clone().flatten(), change_request.merged_at.clone().flatten());
        Ok(ProviderChangeRequestDetail {
            change_request,
            body: pr.body.clone().unwrap_or_default(),
            changed_files: pr.changed_files.unwrap_or(0),
            merged_at,
            closed_at,
            reviewers: pr
                .requested_reviewers
                .as_deref()
                .unwrap_or_default()
                .iter()
                .filter_map(|user| forgejo_actor(Some(user)))
                .collect(),
            checks: forgejo_checks(&statuses),
            merge_capabilities: PullRequestMergeCapabilities {
                merge: repo.allow_merge_commits.unwrap_or(true),
                squash: repo.allow_squash_merge.unwrap_or(true),
                rebase: repo.allow_rebase.unwrap_or(true),
            },
            viewer_permissions: forgejo_viewer_permissions(&repo, &pr, &viewer),
            base_comparison: Some(match pr.merge_base.as_deref() {
                None | Some("") => PullRequestBaseComparison::Unknown,
                Some(merge_base) if merge_base == pr.base.sha => PullRequestBaseComparison::UpToDate,
                Some(_) => PullRequestBaseComparison::Behind,
            }),
            behind_by: None,
            auto_merge_enabled: None,
            auto_merge_method: None,
            workflow_approvals_required: None,
        })
    }

    async fn get_viewer_permissions(&self, input: ViewerPermissionsInput) -> ProviderResult<PullRequestViewerPermissions> {
        let input = input.change_request;
        let target = ref_target(&input);
        let (repo, pr, viewer) = futures::try_join!(self.get_repo(&input), self.get_pull(&input), self.viewer_of(&target))?;
        Ok(forgejo_viewer_permissions(&repo, &pr, &viewer))
    }

    /// The conversation: issue comments, reviews and their inline comments in one timeline, each
    /// with its reactions.
    async fn get_change_request_activity(&self, input: ChangeRequestRef) -> ProviderResult<ProviderChangeRequestActivity> {
        let target = ref_target(&input);
        let issue = issue_path(&input);
        let pull = pull_path(&input);
        let (comments, reviews, commits, reactions, viewer) = futures::try_join!(
            // Issue comments ignore page/limit: this endpoint is read once.
            async {
                let items = self.read_array(&target, format!("{issue}/comments"), decode_comment).await?;
                let truncated = items.len() > PAGE_LIMIT;
                Ok::<_, PullRequestProviderError>((items.into_iter().take(PAGE_LIMIT).collect::<Vec<_>>(), truncated))
            },
            self.page(&target, format!("{pull}/reviews"), decode_review),
            self.page(&target, format!("{pull}/commits"), decode_commit),
            self.page(&target, format!("{issue}/reactions"), decode_reaction),
            self.viewer_of(&target),
        )?;
        let (comments, comments_truncated) = comments;
        let (reviews, reviews_truncated) = reviews;
        let review_reads: Vec<_> = reviews
            .iter()
            .filter(|review| review.comments_count > 0 && review.state != "PENDING")
            .map(|review| self.read_array(&target, format!("{pull}/reviews/{}/comments", review.id), decode_review_comment))
            .collect();
        let review_comments: Vec<Vec<_>> = futures::stream::iter(review_reads).buffered(4).try_collect().await?;
        let all_inline: Vec<_> = review_comments.into_iter().flatten().collect();
        let inline: Vec<_> = all_inline.iter().take(PAGE_LIMIT).cloned().collect();

        let mut entries: Vec<(PullRequestComment, Option<String>)> = Vec::new();
        entries.extend(comments.iter().map(|comment| (forgejo_comment(comment), Some(comment.id.to_string()))));
        entries.extend(
            reviews
                .iter()
                .filter(|review| review.state != "PENDING" && review.state != "REQUEST_REVIEW")
                .map(|review| (forgejo_review(review), review_comment_id(review))),
        );
        entries.extend(inline.iter().map(|comment| {
            let mut entry = forgejo_comment(&comment.comment);
            entry.kind = PullRequestCommentKind::ReviewComment;
            entry.path = Some(comment.path.clone());
            (entry, Some(comment.comment.id.to_string()))
        }));
        let repository = repo_path(&input.repository);
        let reaction_reads: Vec<_> = entries
            .into_iter()
            .map(|(mut comment, reaction_id)| {
                let target = &target;
                let viewer = viewer.as_str();
                let path = reaction_id.map(|id| format!("{repository}/issues/comments/{id}/reactions"));
                async move {
                    let rows = match path {
                        None => Vec::new(),
                        Some(path) => self.read_array(target, path, decode_reaction).await?,
                    };
                    comment.reactions = Some(forgejo_reactions(&rows, viewer));
                    Ok::<_, PullRequestProviderError>(comment)
                }
            })
            .collect();
        let enriched: Vec<PullRequestComment> = futures::stream::iter(reaction_reads).buffered(4).try_collect().await?;
        let by_id = |id: &str| enriched.iter().rev().find(|comment| comment.id == id).cloned();
        let review_threads = inline
            .iter()
            .map(|comment| {
                let mut thread = forgejo_review_thread(comment);
                let entry = by_id(&comment.comment.id.to_string()).unwrap_or_else(|| forgejo_comment(&comment.comment));
                thread.comments = vec![PullRequestThreadComment {
                    id: entry.id,
                    author: entry.author,
                    body: entry.body,
                    created_at: entry.created_at,
                    url: entry.url,
                    reactions: entry.reactions,
                }];
                thread
            })
            .collect();
        let mut timeline = enriched.clone();
        // `localeCompare` on the normalized ISO timestamps, which order like plain text.
        timeline.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        Ok(ProviderChangeRequestActivity {
            author: None,
            reviewers: None,
            comment_count: timeline.len() as i64,
            comments: timeline,
            comments_truncated: comments_truncated || reviews_truncated || all_inline.len() > inline.len(),
            review_threads,
            commits: commits.0.iter().map(forgejo_commit).collect(),
            reactions: Some(forgejo_reactions(&reactions.0, &viewer)),
        })
    }

    /// The head's blob ids from the pull request's own patch (`index` lines); a path the patch
    /// does not name is the empty revision. Always complete.
    async fn get_file_revisions(&self, input: FileRevisionsInput) -> ProviderResult<ProviderFileRevisions> {
        let input_ref = &input.change_request;
        let result = self
            .request(&ref_target(input_ref), format!("{}.diff", pull_path(input_ref)), None, None)
            .await?;
        if result.stdout_truncated {
            return Err(failure("getFileRevisions", "Forgejo diff exceeded the output limit."));
        }
        let mut revisions = parse_diff_file_revisions(&result.stdout);
        for path in &input.paths {
            if !revisions.has(path) {
                revisions.set(path.clone(), String::new());
            }
        }
        Ok(ProviderFileRevisions {
            revisions: revisions.into_entries(),
            complete: Some(true),
        })
    }

    /// The whole patch in one slice (the pull request's, or one commit's).
    async fn get_diff(&self, input: GetDiffInput) -> ProviderResult<ProviderDiffSlice> {
        let input_ref = &input.change_request;
        let path = match input.commit.as_deref().filter(|commit| !commit.is_empty()) {
            Some(commit) => format!("{}/git/commits/{}.diff", repo_path(&input_ref.repository), encode_uri_component(commit)),
            None => format!("{}.diff", pull_path(input_ref)),
        };
        let result = self.request(&ref_target(input_ref), path, None, None).await?;
        Ok(ProviderDiffSlice {
            patch: result.stdout,
            truncated: result.stdout_truncated,
            next_cursor: None,
            omitted_file_stats: None,
        })
    }

    /// Both sides of one file through the contents API: the old side at the commit's parent (or
    /// the merge base), the new side at the commit (or the head, read from the head repository).
    async fn get_diff_file_contents(&self, input: DiffFileContentsInput) -> ProviderResult<ProviderDiffFileContents> {
        let input_ref = &input.change_request;
        let pr = self.get_pull(input_ref).await?;
        let commit = match input.commit.as_deref().filter(|commit| !commit.is_empty()) {
            Some(commit) => Some(
                self.read(
                    &ref_target(input_ref),
                    format!("{}/git/commits/{}", repo_path(&input_ref.repository), encode_uri_component(commit)),
                    decode_commit,
                )
                .await?,
            ),
            None => None,
        };
        let old_ref = match &commit {
            Some(commit) => commit.parents.first().cloned(),
            None => Some(pr.merge_base.clone().filter(|base| !base.is_empty()).unwrap_or_else(|| pr.base.sha.clone())),
        };
        let new_ref = input.commit.clone().unwrap_or_else(|| pr.head.sha.clone());
        let content = |repository: String, reference: String, path: String| async move {
            let file_path = path.split('/').map(encode_uri_component).collect::<Vec<_>>().join("/");
            let api_path = format!("{}/contents/{file_path}?ref={}", repo_path(&repository), encode_uri_component(&reference));
            let content = self
                .read(&target(&input_ref.cwd, Some(&repository), Some(&input_ref.host)), api_path, |value| {
                    let file = object(value)?;
                    if string(file, "encoding")? != "base64" {
                        return Err(crate::gitlab::util::Mismatch);
                    }
                    string(file, "content")
                })
                .await?;
            Ok::<_, PullRequestProviderError>(String::from_utf8_lossy(&decode_base64_lenient(&content)).into_owned())
        };
        let old = async {
            match old_ref.filter(|reference| !reference.is_empty()) {
                Some(reference) if input.change_type != PullRequestDiffFileContentsInputChangeType::New => {
                    content(input_ref.repository.clone(), reference, input.old_path.clone()).await
                }
                _ => Ok(String::new()),
            }
        };
        let new = async {
            if input.change_type == PullRequestDiffFileContentsInputChangeType::Deleted {
                return Ok(String::new());
            }
            let repository = pr
                .head
                .repo
                .as_ref()
                .map_or_else(|| input_ref.repository.clone(), |repo| repo.full_name.clone());
            content(repository, new_ref.clone(), input.new_path.clone()).await
        };
        let (old_contents, new_contents) = futures::try_join!(old, new)?;
        Ok(ProviderDiffFileContents { old_contents, new_contents })
    }

    async fn run_action(&self, input: RunActionInput) -> ProviderResult<()> {
        let input_ref = &input.change_request;
        let target = ref_target(input_ref);
        match input.action {
            PullRequestAction::Merge => {
                let method = input.merge_method.map_or("merge", PullRequestMergeMethod::as_str);
                self.write(&target, format!("{}/merge", pull_path(input_ref)), "POST", Some(json!({"Do": method})))
                    .await
            }
            PullRequestAction::Close | PullRequestAction::Reopen => {
                let state = if input.action == PullRequestAction::Close { "closed" } else { "open" };
                self.write(&target, pull_path(input_ref), "PATCH", Some(json!({"state": state}))).await
            }
            PullRequestAction::UpdateBranch => {
                let style = input.update_method.map_or("merge", PullRequestUpdateMethod::as_str);
                self.write(&target, format!("{}/update?style={style}", pull_path(input_ref)), "POST", None)
                    .await
            }
            other => Err(Self::unsupported(other.as_str())),
        }
    }

    async fn update_change_request(&self, input: UpdateChangeRequestInput) -> ProviderResult<()> {
        let mut body = Map::new();
        if let Some(title) = &input.title {
            body.insert("title".into(), json!(title));
        }
        if let Some(text) = &input.body {
            body.insert("body".into(), json!(text));
        }
        let input_ref = &input.change_request;
        self.write(&ref_target(input_ref), pull_path(input_ref), "PATCH", Some(Value::Object(body)))
            .await
    }

    async fn comment(&self, input: CommentInput) -> ProviderResult<()> {
        let input_ref = &input.change_request;
        self.write(
            &ref_target(input_ref),
            format!("{}/comments", issue_path(input_ref)),
            "POST",
            Some(json!({"body": input.body})),
        )
        .await
    }

    async fn update_comment(&self, input: UpdateCommentInput) -> ProviderResult<()> {
        let input_ref = &input.change_request;
        let path = format!(
            "{}/issues/comments/{}",
            repo_path(&input_ref.repository),
            encode_uri_component(&input.comment_id)
        );
        self.write(&ref_target(input_ref), path, "PATCH", Some(json!({"body": input.body}))).await
    }

    /// One review request with its inline comments, against the head the reader saw.
    async fn submit_review(&self, input: SubmitReviewInput) -> ProviderResult<()> {
        let input_ref = &input.change_request;
        let pr = self.get_pull(input_ref).await?;
        let event = match input.verdict {
            PullRequestReviewVerdict::Approve => "APPROVED",
            PullRequestReviewVerdict::RequestChanges => "REQUEST_CHANGES",
            PullRequestReviewVerdict::Comment => "COMMENT",
        };
        let comments: Vec<Value> = input
            .comments
            .iter()
            .map(|comment| {
                let (old, old_line, new_line) = match &comment.position {
                    PullRequestReviewPosition::Deleted(deleted) => (true, deleted.old_line, 0),
                    PullRequestReviewPosition::Added(added) => (false, 0, added.new_line),
                    PullRequestReviewPosition::Context(context) => {
                        let old = context.side == zc_contracts::PullRequestDiffSide::Left;
                        (old, context.old_line, context.new_line)
                    }
                };
                let path = if old {
                    comment.old_path.clone().unwrap_or_else(|| comment.path.clone())
                } else {
                    comment.path.clone()
                };
                json!({
                    "path": path,
                    "body": comment.body,
                    "old_position": if old { old_line } else { 0 },
                    "new_position": if old { 0 } else { new_line },
                })
            })
            .collect();
        let body = json!({"event": event, "body": input.body, "commit_id": pr.head.sha, "comments": comments});
        self.write(&ref_target(input_ref), format!("{}/reviews", pull_path(input_ref)), "POST", Some(body))
            .await
    }

    /// The repository's assignees, less the author.
    async fn list_reviewer_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestReviewerCandidateList> {
        let target = ref_target(&input);
        let (pr, (users, truncated)) = futures::try_join!(
            self.get_pull(&input),
            self.page(&target, format!("{}/assignees", repo_path(&input.repository)), decode_user_item)
        )?;
        let author = pr.user.as_ref().map(|user| user.login.as_str());
        Ok(PullRequestReviewerCandidateList {
            candidates: users
                .iter()
                .filter(|user| Some(user.login.as_str()) != author)
                .filter_map(|user| {
                    let actor = forgejo_actor(Some(user))?;
                    Some(PullRequestReviewerCandidate {
                        is_bot: None,
                        login: actor.login,
                        name: actor.name,
                        avatar_url: actor.avatar_url,
                        id: user.login.clone(),
                        kind: PullRequestReviewerKind::User,
                        is_requested: pr
                            .requested_reviewers
                            .as_deref()
                            .is_some_and(|reviewers| reviewers.iter().any(|reviewer| reviewer.login == user.login)),
                    })
                })
                .collect(),
            truncated,
        })
    }

    async fn set_reviewer_request(&self, input: SetReviewerRequestInput) -> ProviderResult<()> {
        let input_ref = &input.change_request;
        let reviewers: Vec<&str> = input.reviewers.iter().map(|reviewer| reviewer.id.as_str()).collect();
        let method = if input.requested { "POST" } else { "DELETE" };
        self.write(
            &ref_target(input_ref),
            format!("{}/requested_reviewers", pull_path(input_ref)),
            method,
            Some(json!({"reviewers": reviewers})),
        )
        .await
    }

    async fn list_label_candidates(&self, input: ChangeRequestRef) -> ProviderResult<PullRequestLabelCandidateList> {
        let target = ref_target(&input);
        let (pr, (labels, truncated)) = futures::try_join!(
            self.get_pull(&input),
            self.page(&target, format!("{}/labels", repo_path(&input.repository)), decode_label)
        )?;
        Ok(PullRequestLabelCandidateList {
            candidates: labels
                .into_iter()
                .map(|label| PullRequestLabelCandidate {
                    is_applied: pr.labels.as_deref().is_some_and(|applied| applied.iter().any(|applied| applied.id == label.id)),
                    name: label.name,
                    color: label.color,
                    description: label.description,
                })
                .collect(),
            truncated,
        })
    }

    /// Labels are written by id, so the names are looked up first; one that does not exist fails
    /// the whole change.
    async fn set_labels(&self, input: SetLabelsInput) -> ProviderResult<()> {
        let input_ref = &input.change_request;
        let target = ref_target(input_ref);
        let (labels, _) = self.page(&target, format!("{}/labels", repo_path(&input_ref.repository)), decode_label).await?;
        let selected: Vec<_> = labels.iter().filter(|label| input.labels.contains(&label.name)).collect();
        if selected.len() != input.labels.len() {
            return Err(failure("setLabels", "One or more requested labels could not be found."));
        }
        if input.applied {
            let ids: Vec<i64> = selected.iter().map(|label| label.id).collect();
            return self
                .write(&target, format!("{}/labels", issue_path(input_ref)), "POST", Some(json!({"labels": ids})))
                .await;
        }
        for label in selected {
            self.write(&target, format!("{}/labels/{}", issue_path(input_ref), label.id), "DELETE", None)
                .await?;
        }
        Ok(())
    }

    /// A review's reactions live on the issue comment it is also kept as, so `review:<id>` is
    /// looked up first; no subject is the pull request itself.
    async fn set_reaction(&self, input: SetReactionInput) -> ProviderResult<()> {
        static REVIEW: OnceLock<Regex> = OnceLock::new();
        static NUMERIC: OnceLock<Regex> = OnceLock::new();
        let input_ref = &input.change_request;
        let target = ref_target(input_ref);
        let mut comment_id = input.subject_id.clone();
        if let Some(subject) = comment_id.as_deref().filter(|subject| subject.starts_with("review:")) {
            let review_id = REVIEW
                .get_or_init(|| Regex::new(r"^review:([1-9][0-9]*)$").expect("valid regex"))
                .captures(subject)
                .and_then(|captures| captures.get(1))
                .map(|id| id.as_str().to_owned());
            let Some(review_id) = review_id else {
                return Err(failure("setReaction", "Invalid Forgejo review ID."));
            };
            let review = self
                .read(&target, format!("{}/reviews/{review_id}", pull_path(input_ref)), |value| {
                    decode_review_fields(object(value)?)
                })
                .await?;
            comment_id = review_comment_id(&review);
            if comment_id.is_none() {
                return Err(failure("setReaction", "Forgejo did not return a comment ID for this review."));
            }
        }
        let comment_id = comment_id.filter(|id| !id.is_empty());
        if let Some(id) = &comment_id {
            if !NUMERIC.get_or_init(|| Regex::new(r"^[1-9][0-9]*$").expect("valid regex")).is_match(id) {
                return Err(failure("setReaction", "Invalid Forgejo comment ID."));
            }
        }
        let path = match &comment_id {
            Some(id) => format!("{}/issues/comments/{id}/reactions", repo_path(&input_ref.repository)),
            None => format!("{}/reactions", issue_path(input_ref)),
        };
        let method = if input.reacted { "POST" } else { "DELETE" };
        self.write(&target, path, method, Some(json!({"content": forgejo_reaction_name(input.content)})))
            .await
    }

    async fn reply_to_thread(&self, _input: ReplyToThreadInput) -> ProviderResult<()> {
        Err(Self::unsupported("thread replies"))
    }

    async fn set_thread_resolution(&self, _input: SetThreadResolutionInput) -> ProviderResult<()> {
        Err(Self::unsupported("thread resolution"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_base64_the_way_node_does() {
        assert_eq!(decode_base64_lenient("aGVsbG8K"), b"hello\n");
        assert_eq!(decode_base64_lenient("aGVs\nbG8K"), b"hello\n");
        assert_eq!(decode_base64_lenient("aGVsbA=="), b"hell");
        assert_eq!(decode_base64_lenient("aGVsbA"), b"hell");
        assert_eq!(decode_base64_lenient("_-8"), [0xff, 0xef]);
    }

    #[test]
    fn reads_the_next_page_link() {
        assert_eq!(has_next_page("HTTP/1.1 200 OK\nLink: <https://x/?page=2>; rel=\"next\"\n"), Some(true));
        assert_eq!(has_next_page("HTTP/1.1 200 OK\nlink: <https://x/?page=1>; rel=\"prev\"\n"), Some(false));
        assert_eq!(has_next_page("HTTP/1.1 200 OK\n"), None);
    }

    #[test]
    fn finds_the_issue_comment_a_review_is_kept_as() {
        let review = |url: Option<&str>| ForgejoReview {
            id: 3,
            body: String::new(),
            user: None,
            state: "APPROVED".into(),
            submitted_at: String::new(),
            html_url: url.map(Into::into),
            comments_count: 0,
        };
        assert_eq!(
            review_comment_id(&review(Some("https://forge.example.test/a/b/pulls/7#issuecomment-42"))).as_deref(),
            Some("42")
        );
        assert_eq!(
            review_comment_id(&review(Some("https://forge.example.test/a/b/pulls/7#issuecomment-042"))),
            None
        );
        assert_eq!(review_comment_id(&review(None)), None);
    }
}
