//! The `Raw*Schema`s of `gitHubPullRequestJson.ts`: what each `gh` answer must look like, field
//! by field, with the TS strictness (see [`super::schema`]). Enum-ish fields stay plain strings and
//! are normalized later, so a `gh` release that adds a conclusion or a state fails nothing.
//!
//! Where the TS reads `null` and absent alike (`trimmed(raw?.x)`), the field is a flattened
//! `Option`; where it tells them apart, the outer `Option` is presence.

use serde_json::{Map, Value};

use super::schema::{array, boolean, int, null_or, number, object, opt, opt_n, record, req, string, unknown, Decoded};

type Obj = Map<String, Value>;

/// `RawActorSchema`. `login` is optional because a team or mannequin reviewer answers with an
/// empty object; `avatarUrl` only comes from GraphQL.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawActor {
    pub typename: Option<String>,
    pub is_bot: Option<bool>,
    pub login: Option<String>,
    pub id: Option<String>,
    pub name: Option<String>,
    pub avatar_url: Option<String>,
}

fn actor_fields(map: &Obj) -> Decoded<RawActor> {
    Ok(RawActor {
        typename: opt(map, "__typename", string)?,
        is_bot: opt(map, "is_bot", boolean)?,
        login: opt(map, "login", string)?,
        id: opt_n(map, "id", string)?,
        name: opt_n(map, "name", string)?,
        avatar_url: opt_n(map, "avatarUrl", string)?,
    })
}

pub(crate) fn raw_actor(value: &Value) -> Decoded<RawActor> {
    actor_fields(object(value)?)
}

/// `RawRequestedReviewerSchema`: an actor, or a team (`slug` where a user has a login).
#[derive(Debug, Clone, Default)]
pub(crate) struct RawRequestedReviewer {
    pub actor: RawActor,
    pub slug: Option<String>,
}

pub(crate) fn raw_requested_reviewer(value: &Value) -> Decoded<RawRequestedReviewer> {
    let map = object(value)?;
    Ok(RawRequestedReviewer {
        actor: actor_fields(map)?,
        slug: opt_n(map, "slug", string)?,
    })
}

/// `RawLabelSchema`.
#[derive(Debug, Clone)]
pub(crate) struct RawLabel {
    pub name: String,
    pub color: Option<String>,
}

pub(crate) fn raw_label(value: &Value) -> Decoded<RawLabel> {
    let map = object(value)?;
    Ok(RawLabel {
        name: req(map, "name", string)?,
        color: opt_n(map, "color", string)?,
    })
}

/// `RawReviewRequestSchema`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawReviewRequest {
    pub login: Option<String>,
    pub slug: Option<String>,
    pub name: Option<String>,
}

pub(crate) fn raw_review_request(value: &Value) -> Decoded<RawReviewRequest> {
    let map = object(value)?;
    Ok(RawReviewRequest {
        login: opt_n(map, "login", string)?,
        slug: opt_n(map, "slug", string)?,
        name: opt_n(map, "name", string)?,
    })
}

/// `RawLatestReviewSchema`: one reviewer's most recent review.
#[derive(Debug, Clone)]
pub(crate) struct RawLatestReview {
    pub author: Option<RawActor>,
    pub state: Option<String>,
}

pub(crate) fn raw_latest_review(value: &Value) -> Decoded<RawLatestReview> {
    let map = object(value)?;
    Ok(RawLatestReview {
        author: opt_n(map, "author", raw_actor)?,
        state: opt_n(map, "state", string)?,
    })
}

/// `RawCheckSchema`: a check run or a commit status of `statusCheckRollup`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawCheck {
    pub name: Option<String>,
    pub context: Option<String>,
    pub status: Option<String>,
    pub conclusion: Option<String>,
    pub state: Option<String>,
    pub description: Option<String>,
    pub details_url: Option<String>,
    pub target_url: Option<String>,
    pub workflow_name: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
}

pub(crate) fn check_fields(map: &Obj) -> Decoded<RawCheck> {
    opt(map, "__typename", string)?;
    Ok(RawCheck {
        name: opt_n(map, "name", string)?,
        context: opt_n(map, "context", string)?,
        status: opt_n(map, "status", string)?,
        conclusion: opt_n(map, "conclusion", string)?,
        state: opt_n(map, "state", string)?,
        description: opt_n(map, "description", string)?,
        details_url: opt_n(map, "detailsUrl", string)?,
        target_url: opt_n(map, "targetUrl", string)?,
        workflow_name: opt_n(map, "workflowName", string)?,
        started_at: opt_n(map, "startedAt", string)?,
        completed_at: opt_n(map, "completedAt", string)?,
    })
}

pub(crate) fn raw_check(value: &Value) -> Decoded<RawCheck> {
    check_fields(object(value)?)
}

/// `RawListItemSchema`: one row of `gh pr list --json`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawListItem {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<RawActor>,
    pub head_ref_name: String,
    pub base_ref_name: String,
    /// Raw, since the core compares it to `"OPEN"` as is.
    pub state: Option<String>,
    pub is_draft: Option<bool>,
    pub mergeable: Option<String>,
    pub review_decision: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub merged_at: Option<String>,
    pub review_requests: Option<Vec<RawReviewRequest>>,
    pub latest_reviews: Option<Vec<RawLatestReview>>,
    pub labels: Option<Vec<RawLabel>>,
    pub status_check_rollup: Option<Vec<RawCheck>>,
}

/// How a read spells `reviewRequests` or `labels`: flat for `gh`, as connections for the core read
/// (which overrides them in place, so they are still checked in the list's order).
pub(crate) type FieldDecoder<'a, T> = &'a dyn Fn(&Obj) -> Decoded<Option<Vec<T>>>;

/// The list fields, with `reviewRequests` and `labels` read as the caller spells them.
pub(crate) fn list_item_with(map: &Obj, review_requests: FieldDecoder<'_, RawReviewRequest>, labels: FieldDecoder<'_, RawLabel>) -> Decoded<RawListItem> {
    Ok(RawListItem {
        number: req(map, "number", int)?,
        title: req(map, "title", string)?,
        url: req(map, "url", string)?,
        author: opt_n(map, "author", raw_actor)?,
        head_ref_name: req(map, "headRefName", string)?,
        base_ref_name: req(map, "baseRefName", string)?,
        state: opt_n(map, "state", string)?,
        is_draft: opt(map, "isDraft", boolean)?,
        mergeable: opt_n(map, "mergeable", string)?,
        review_decision: opt_n(map, "reviewDecision", string)?,
        additions: opt(map, "additions", int)?,
        deletions: opt(map, "deletions", int)?,
        created_at: req(map, "createdAt", string)?,
        updated_at: req(map, "updatedAt", string)?,
        merged_at: opt_n(map, "mergedAt", string)?,
        review_requests: review_requests(map)?,
        latest_reviews: opt_n(map, "latestReviews", |value| array(value, raw_latest_review))?,
        labels: labels(map)?,
        status_check_rollup: opt_n(map, "statusCheckRollup", |value| array(value, raw_check))?,
    })
}

fn list_item_fields(map: &Obj) -> Decoded<RawListItem> {
    list_item_with(map, &|map| opt(map, "reviewRequests", |value| array(value, raw_review_request)), &|map| {
        opt(map, "labels", |value| array(value, raw_label))
    })
}

pub(crate) fn raw_list_item(value: &Value) -> Decoded<RawListItem> {
    list_item_fields(object(value)?)
}

/// `RawDetailSchema`: the list row plus what `gh pr view --json` adds.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawDetail {
    pub item: RawListItem,
    pub is_cross_repository: Option<bool>,
    pub head_repository_owner: Option<String>,
    pub head_ref_oid: Option<String>,
    pub body: Option<String>,
    pub changed_files: Option<i64>,
    pub closed_at: Option<String>,
    /// Absent / `null` / `{ mergeMethod }`, all three meaning something different.
    pub auto_merge_request: Option<Option<Option<String>>>,
}

fn login_object(value: &Value) -> Decoded<String> {
    req(object(value)?, "login", string)
}

pub(crate) fn detail_extra_fields(map: &Obj, item: RawListItem) -> Decoded<RawDetail> {
    Ok(RawDetail {
        item,
        is_cross_repository: opt(map, "isCrossRepository", boolean)?,
        head_repository_owner: opt_n(map, "headRepositoryOwner", login_object)?,
        head_ref_oid: opt_n(map, "headRefOid", string)?,
        body: opt(map, "body", string)?,
        changed_files: opt(map, "changedFiles", int)?,
        closed_at: opt_n(map, "closedAt", string)?,
        auto_merge_request: opt(map, "autoMergeRequest", |value| {
            null_or(value, |request| opt_n(object(request)?, "mergeMethod", string))
        })?,
    })
}

pub(crate) fn raw_detail(value: &Value) -> Decoded<RawDetail> {
    let map = object(value)?;
    detail_extra_fields(map, list_item_fields(map)?)
}

/// `RawStackMembershipSchema`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawStackMembership {
    /// `(number, size, baseRefName)`.
    pub stack: Option<(i64, i64, String)>,
    pub stack_entry_position: Option<i64>,
}

fn stack_membership_fields(map: &Obj) -> Decoded<RawStackMembership> {
    Ok(RawStackMembership {
        stack: opt_n(map, "stack", |value| {
            let stack = object(value)?;
            Ok((req(stack, "number", int)?, req(stack, "size", int)?, req(stack, "baseRefName", string)?))
        })?,
        stack_entry_position: opt_n(map, "stackEntry", |value| req(object(value)?, "position", int))?,
    })
}

pub(crate) fn raw_stack_membership(value: &Value) -> Decoded<RawStackMembership> {
    stack_membership_fields(object(value)?)
}

/// `RawSearchItemSchema`: a search row, one connection deeper than the listing's.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawSearchItem {
    pub membership: RawStackMembership,
    pub number: i64,
    pub title: String,
    pub url: String,
    pub author: Option<RawActor>,
    pub head_ref_name: String,
    pub base_ref_name: String,
    pub state: Option<String>,
    pub is_draft: Option<bool>,
    pub mergeable: Option<String>,
    pub review_decision: Option<String>,
    /// `latestReviews.nodes`, `null` nodes kept as `None`.
    pub latest_reviews: Option<Vec<Option<RawLatestReview>>>,
    /// `None` only for a summary, where it is optional.
    pub created_at: Option<String>,
    pub updated_at: String,
    pub merged_at: Option<String>,
    pub repository: Option<String>,
    /// `reviewRequests.nodes[].requestedReviewer`.
    pub review_requests: Option<Vec<Option<Option<RawActor>>>>,
    pub labels: Option<Vec<Option<RawLabel>>>,
    /// `commits.nodes[].commit.statusCheckRollup.state`.
    pub rollup_states: Option<Vec<Option<Option<String>>>>,
}

fn nodes_of<T>(value: &Value, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<Option<Vec<T>>> {
    opt_n(object(value)?, "nodes", |nodes| array(nodes, &decode))
}

fn search_item_fields(map: &Obj, created_at_required: bool) -> Decoded<RawSearchItem> {
    Ok(RawSearchItem {
        membership: stack_membership_fields(map)?,
        number: req(map, "number", int)?,
        title: req(map, "title", string)?,
        url: req(map, "url", string)?,
        author: opt_n(map, "author", raw_actor)?,
        head_ref_name: req(map, "headRefName", string)?,
        base_ref_name: req(map, "baseRefName", string)?,
        state: opt_n(map, "state", string)?,
        is_draft: opt(map, "isDraft", boolean)?,
        mergeable: opt_n(map, "mergeable", string)?,
        review_decision: opt_n(map, "reviewDecision", string)?,
        latest_reviews: opt_n(map, "latestReviews", |value| {
            req(object(value)?, "nodes", |nodes| array(nodes, |node| null_or(node, raw_latest_review)))
        })?,
        created_at: if created_at_required {
            Some(req(map, "createdAt", string)?)
        } else {
            opt(map, "createdAt", string)?
        },
        updated_at: req(map, "updatedAt", string)?,
        merged_at: opt_n(map, "mergedAt", string)?,
        repository: opt_n(map, "repository", |value| req(object(value)?, "nameWithOwner", string))?,
        review_requests: opt_n(map, "reviewRequests", |value| {
            nodes_of(value, |node| null_or(node, |request| opt_n(object(request)?, "requestedReviewer", raw_actor)))
        })?
        .flatten(),
        labels: opt_n(map, "labels", |value| nodes_of(value, |node| null_or(node, raw_label)))?.flatten(),
        rollup_states: opt_n(map, "commits", |value| {
            nodes_of(value, |node| {
                null_or(node, |node| {
                    Ok(opt_n(object(node)?, "commit", |commit| {
                        opt_n(object(commit)?, "statusCheckRollup", |rollup| req(object(rollup)?, "state", string))
                    })?
                    .flatten())
                })
            })
        })?
        .flatten(),
    })
}

pub(crate) fn raw_search_item(value: &Value) -> Decoded<RawSearchItem> {
    search_item_fields(object(value)?, true)
}

/// `RawSearchSchema`.
pub(crate) struct RawSearch {
    pub has_next_page: Option<bool>,
    pub nodes: Option<Vec<Value>>,
}

pub(crate) fn raw_search(value: &Value) -> Decoded<RawSearch> {
    at_path(value, &["data", "search"], &|search| {
        let search = object(search)?;
        Ok(RawSearch {
            has_next_page: opt_n(search, "pageInfo", |page| req(object(page)?, "hasNextPage", boolean))?,
            nodes: opt_n(search, "nodes", |nodes| array(nodes, unknown))?,
        })
    })
}

/// `RawSummarySchema`: the search row with the counts a summary also reads.
pub(crate) struct RawSummary {
    pub item: RawSearchItem,
    pub changed_files: Option<i64>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub closed_at: Option<String>,
}

pub(crate) fn raw_summary(value: &Value) -> Decoded<RawSummary> {
    let map = object(value)?;
    Ok(RawSummary {
        item: search_item_fields(map, false)?,
        changed_files: opt_n(map, "changedFiles", int)?,
        additions: opt_n(map, "additions", int)?,
        deletions: opt_n(map, "deletions", int)?,
        closed_at: opt_n(map, "closedAt", string)?,
    })
}

/// The `{ data: Record<string, …> | null }` of an aliased batch read, with each value decoded.
pub(crate) fn aliased_data<T>(value: &Value, optional: bool, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<Vec<(String, T)>> {
    let map = object(value)?;
    let entries = if optional {
        opt_n(map, "data", |data| record(data, &decode))?
    } else {
        Some(req(map, "data", |data| record(data, &decode))?)
    };
    Ok(entries.unwrap_or_default())
}

/// `RawReactionGroupsSchema`, one group.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawReactionGroup {
    pub content: Option<String>,
    pub viewer_has_reacted: Option<bool>,
    pub total_count: Option<i64>,
    pub logins: Vec<Option<String>>,
}

pub(crate) fn raw_reaction_groups(map: &Obj) -> Decoded<Option<Vec<RawReactionGroup>>> {
    opt_n(map, "reactionGroups", |groups| {
        array(groups, |group| {
            let group = object(group)?;
            let content = opt_n(group, "content", string)?;
            let viewer_has_reacted = opt(group, "viewerHasReacted", boolean)?;
            let reactors = opt_n(group, "reactors", |reactors| {
                let reactors = object(reactors)?;
                let total_count = opt(reactors, "totalCount", int)?;
                let logins = opt_n(reactors, "nodes", |nodes| {
                    array(nodes, |node| Ok(null_or(node, |node| opt_n(object(node)?, "login", string))?.flatten()))
                })?;
                Ok((total_count, logins.unwrap_or_default()))
            })?;
            let (total_count, logins) = reactors.unwrap_or_default();
            Ok(RawReactionGroup {
                content,
                viewer_has_reacted,
                total_count,
                logins,
            })
        })
    })
}

/// `RawCommentSchema`.
#[derive(Debug, Clone)]
pub(crate) struct RawComment {
    pub id: String,
    pub author: Option<RawActor>,
    pub body: Option<String>,
    pub created_at: String,
    pub url: Option<String>,
    pub reaction_groups: Option<Vec<RawReactionGroup>>,
}

pub(crate) fn raw_comment(value: &Value) -> Decoded<RawComment> {
    let map = object(value)?;
    Ok(RawComment {
        id: req(map, "id", string)?,
        author: opt_n(map, "author", raw_actor)?,
        body: opt(map, "body", string)?,
        created_at: req(map, "createdAt", string)?,
        url: opt_n(map, "url", string)?,
        reaction_groups: raw_reaction_groups(map)?,
    })
}

/// `RawReviewSchema`.
#[derive(Debug, Clone)]
pub(crate) struct RawReview {
    pub id: String,
    pub author: Option<RawActor>,
    pub body: Option<String>,
    pub state: Option<String>,
    pub submitted_at: Option<String>,
    pub url: Option<String>,
}

pub(crate) fn raw_review(value: &Value) -> Decoded<RawReview> {
    let map = object(value)?;
    Ok(RawReview {
        id: req(map, "id", string)?,
        author: opt_n(map, "author", raw_actor)?,
        body: opt(map, "body", string)?,
        state: opt_n(map, "state", string)?,
        submitted_at: opt_n(map, "submittedAt", string)?,
        url: opt_n(map, "url", string)?,
    })
}

/// `RawCommitSchema`'s author: a signature, linked to an account or not.
#[derive(Debug, Clone)]
pub(crate) struct RawCommitAuthor {
    pub email: Option<String>,
    pub login: Option<String>,
    pub name: Option<String>,
}

/// `RawCommitSchema`.
#[derive(Debug, Clone)]
pub(crate) struct RawCommit {
    pub oid: String,
    pub message_headline: Option<String>,
    pub committed_date: String,
    pub authors: Option<Vec<RawCommitAuthor>>,
}

pub(crate) fn raw_commit(value: &Value) -> Decoded<RawCommit> {
    let map = object(value)?;
    Ok(RawCommit {
        oid: req(map, "oid", string)?,
        message_headline: opt(map, "messageHeadline", string)?,
        committed_date: req(map, "committedDate", string)?,
        authors: opt(map, "authors", |authors| {
            array(authors, |author| {
                let author = object(author)?;
                let email = opt_n(author, "email", string)?;
                opt_n(author, "id", string)?;
                Ok(RawCommitAuthor {
                    email,
                    login: opt_n(author, "login", string)?,
                    name: opt_n(author, "name", string)?,
                })
            })
        })?,
    })
}

/// `RawWorkflowRunApprovalSchema`.
pub(crate) struct RawWorkflowRunApproval {
    pub database_id: i64,
    pub workflow_name: Option<String>,
    pub url: Option<String>,
}

pub(crate) fn raw_workflow_run_approval(value: &Value) -> Decoded<RawWorkflowRunApproval> {
    let map = object(value)?;
    Ok(RawWorkflowRunApproval {
        database_id: req(map, "databaseId", int)?,
        workflow_name: opt_n(map, "workflowName", string)?,
        url: opt_n(map, "url", string)?,
    })
}

/// `RawPullRequestHeadSchema`.
pub(crate) struct RawPullRequestHead {
    pub number: i64,
    pub head_ref_oid: String,
    pub is_cross_repository: Option<bool>,
    pub head_repository_owner: Option<String>,
}

pub(crate) fn raw_pull_request_head(value: &Value) -> Decoded<RawPullRequestHead> {
    let map = object(value)?;
    Ok(RawPullRequestHead {
        number: req(map, "number", int)?,
        head_ref_oid: req(map, "headRefOid", string)?,
        is_cross_repository: opt(map, "isCrossRepository", boolean)?,
        head_repository_owner: opt_n(map, "headRepositoryOwner", login_object)?,
    })
}

/// `RawActivitySchema`.
pub(crate) struct RawActivity {
    pub author: Option<RawActor>,
    pub comments: Option<Vec<RawComment>>,
    pub reviews: Option<Vec<RawReview>>,
    pub commits: Option<Vec<RawCommit>>,
}

pub(crate) fn raw_activity(value: &Value) -> Decoded<RawActivity> {
    let map = object(value)?;
    Ok(RawActivity {
        author: opt_n(map, "author", raw_actor)?,
        comments: opt(map, "comments", |comments| array(comments, raw_comment))?,
        reviews: opt(map, "reviews", |reviews| array(reviews, raw_review))?,
        commits: opt(map, "commits", |commits| array(commits, raw_commit))?,
    })
}

/// `RawPageInfoSchema`.
#[derive(Debug, Clone, Default)]
pub(crate) struct RawPageInfo {
    pub has_next_page: Option<bool>,
    pub end_cursor: Option<String>,
}

pub(crate) fn raw_page_info(value: &Value) -> Decoded<RawPageInfo> {
    let map = object(value)?;
    Ok(RawPageInfo {
        has_next_page: opt(map, "hasNextPage", boolean)?,
        end_cursor: opt_n(map, "endCursor", string)?,
    })
}

/// `RawViewerFieldsSchema`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct RawViewerFields {
    pub viewer_can_update: Option<bool>,
    pub viewer_did_author: Option<bool>,
}

pub(crate) fn viewer_fields(map: &Obj) -> Decoded<RawViewerFields> {
    Ok(RawViewerFields {
        viewer_can_update: opt(map, "viewerCanUpdate", boolean)?,
        viewer_did_author: opt(map, "viewerDidAuthor", boolean)?,
    })
}

/// `RawThreadCommentsSchema`.
pub(crate) struct RawThreadComments {
    pub total_count: Option<i64>,
    pub page_info: Option<RawPageInfo>,
    pub nodes: Vec<RawComment>,
}

pub(crate) fn raw_thread_comments(value: &Value) -> Decoded<RawThreadComments> {
    let map = object(value)?;
    Ok(RawThreadComments {
        total_count: opt(map, "totalCount", int)?,
        page_info: opt(map, "pageInfo", raw_page_info)?,
        nodes: req(map, "nodes", |nodes| array(nodes, raw_comment))?,
    })
}

/// `RawRepositoryAccessSchema`: the three merge settings are required (guessing `true` would
/// offer a method the repository forbids); the permission is not.
pub(crate) struct RawRepositoryAccess {
    pub merge_commit_allowed: bool,
    pub squash_merge_allowed: bool,
    pub rebase_merge_allowed: bool,
    pub viewer_permission: Option<String>,
}

pub(crate) fn repository_access_fields(map: &Obj) -> Decoded<RawRepositoryAccess> {
    Ok(RawRepositoryAccess {
        merge_commit_allowed: req(map, "mergeCommitAllowed", boolean)?,
        squash_merge_allowed: req(map, "squashMergeAllowed", boolean)?,
        rebase_merge_allowed: req(map, "rebaseMergeAllowed", boolean)?,
        viewer_permission: opt_n(map, "viewerPermission", string)?,
    })
}

/// `RawPullRequestFileSchema`: one entry of the REST files API.
pub(crate) struct RawPullRequestFile {
    pub filename: String,
    pub status: Option<String>,
    pub previous_filename: Option<String>,
    pub patch: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
}

pub(crate) fn raw_pull_request_file(value: &Value) -> Decoded<RawPullRequestFile> {
    let map = object(value)?;
    Ok(RawPullRequestFile {
        filename: req(map, "filename", string)?,
        status: opt_n(map, "status", string)?,
        previous_filename: opt_n(map, "previous_filename", string)?,
        patch: opt_n(map, "patch", string)?,
        additions: opt_n(map, "additions", int)?,
        deletions: opt_n(map, "deletions", int)?,
    })
}

/// One `reviewThreads` node of `RawReviewThreadsSchema`.
pub(crate) struct RawReviewThread {
    pub id: Option<String>,
    pub is_resolved: Option<bool>,
    pub is_outdated: Option<bool>,
    pub path: Option<String>,
    /// Null once the thread's line has left the diff, which `isOutdated` reports.
    pub line: Option<i64>,
    pub diff_side: Option<String>,
    pub comments: RawThreadComments,
}

/// A conversation comment or review as the thread read names it: for its reactions and author.
pub(crate) struct RawReactableNode {
    pub id: Option<String>,
    pub author: Option<RawActor>,
    pub reaction_groups: Option<Vec<RawReactionGroup>>,
}

/// A `ReviewDismissedEvent`: why a review was dismissed, by the review's node id.
pub(crate) struct RawDismissal {
    pub message: Option<String>,
    pub review_id: Option<String>,
}

/// A commit author off the GraphQL commits connection (`user.login`, not a flat `login`).
pub(crate) struct RawGraphqlCommitAuthor {
    pub name: Option<String>,
    pub avatar_url: Option<String>,
    pub user_login: Option<String>,
}

/// A commit off the GraphQL commits connection.
pub(crate) struct RawGraphqlCommit {
    pub oid: String,
    pub message_headline: Option<String>,
    pub committed_date: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub parents_total_count: Option<i64>,
    pub authors: Option<Vec<RawGraphqlCommitAuthor>>,
}

/// `data.repository.pullRequest` of `RawReviewThreadsSchema`.
pub(crate) struct RawReviewThreadsPullRequest {
    pub threads_page_info: Option<RawPageInfo>,
    pub threads: Vec<RawReviewThread>,
    pub viewer: RawViewerFields,
    pub author: Option<RawActor>,
    pub reaction_groups: Option<Vec<RawReactionGroup>>,
    pub comments: Option<Vec<RawReactableNode>>,
    pub reviews: Option<Vec<RawReactableNode>>,
    /// `reviewRequests.nodes[].requestedReviewer` (null for a team).
    pub review_requests: Option<Vec<Option<RawActor>>>,
    pub latest_reviews: Option<Vec<RawLatestReview>>,
    pub dismissals_page_info: Option<RawPageInfo>,
    pub dismissals: Option<Vec<RawDismissal>>,
    pub commits: Option<Vec<RawGraphqlCommit>>,
}

/// `RawReviewThreadsSchema`.
pub(crate) struct RawReviewThreads {
    pub viewer: Option<String>,
    pub pull_request: RawReviewThreadsPullRequest,
}

fn raw_reactable_node(value: &Value) -> Decoded<RawReactableNode> {
    let map = object(value)?;
    Ok(RawReactableNode {
        id: opt_n(map, "id", string)?,
        author: opt_n(map, "author", raw_actor)?,
        reaction_groups: raw_reaction_groups(map)?,
    })
}

/// `{ pageInfo?, nodes: [{ dismissalMessage?, review? }] }`, the timeline's dismissal events.
pub(crate) fn raw_dismissals(value: &Value) -> Decoded<(Option<RawPageInfo>, Vec<RawDismissal>)> {
    let map = object(value)?;
    let page_info = opt(map, "pageInfo", raw_page_info)?;
    let nodes = req(map, "nodes", |nodes| {
        array(nodes, |node| {
            let node = object(node)?;
            Ok(RawDismissal {
                message: opt_n(node, "dismissalMessage", string)?,
                review_id: opt_n(node, "review", |review| opt_n(object(review)?, "id", string))?.flatten(),
            })
        })
    })?;
    Ok((page_info, nodes))
}

fn raw_review_thread(value: &Value) -> Decoded<RawReviewThread> {
    let map = object(value)?;
    Ok(RawReviewThread {
        id: opt_n(map, "id", string)?,
        is_resolved: opt(map, "isResolved", boolean)?,
        is_outdated: opt(map, "isOutdated", boolean)?,
        path: opt_n(map, "path", string)?,
        line: opt_n(map, "line", int)?,
        diff_side: opt_n(map, "diffSide", string)?,
        comments: req(map, "comments", raw_thread_comments)?,
    })
}

fn raw_graphql_commit(value: &Value) -> Decoded<RawGraphqlCommit> {
    req(object(value)?, "commit", graphql_commit_fields)
}

fn graphql_commit_fields(value: &Value) -> Decoded<RawGraphqlCommit> {
    let map = object(value)?;
    Ok(RawGraphqlCommit {
        oid: req(map, "oid", string)?,
        message_headline: opt_n(map, "messageHeadline", string)?,
        committed_date: opt_n(map, "committedDate", string)?,
        additions: opt(map, "additions", int)?,
        deletions: opt(map, "deletions", int)?,
        parents_total_count: opt_n(map, "parents", |parents| opt(object(parents)?, "totalCount", int))?.flatten(),
        authors: opt_n(map, "authors", |authors| {
            req(object(authors)?, "nodes", |nodes| {
                array(nodes, |author| {
                    let author = object(author)?;
                    Ok(RawGraphqlCommitAuthor {
                        name: opt_n(author, "name", string)?,
                        avatar_url: opt_n(author, "avatarUrl", string)?,
                        user_login: opt_n(author, "user", |user| opt_n(object(user)?, "login", string))?.flatten(),
                    })
                })
            })
        })?,
    })
}

fn connection_nodes<T>(map: &Obj, key: &str, decode: impl Fn(&Value) -> Decoded<T>) -> Decoded<Option<Vec<T>>> {
    opt_n(map, key, |connection| req(object(connection)?, "nodes", |nodes| array(nodes, &decode)))
}

pub(crate) fn raw_review_threads(value: &Value) -> Decoded<RawReviewThreads> {
    req(object(value)?, "data", |data| {
        let data = object(data)?;
        let viewer = viewer_login(data)?;
        let pull_request = req(data, "repository", |repository| {
            req(object(repository)?, "pullRequest", |pull_request| {
                let map = object(pull_request)?;
                let (threads_page_info, threads) = req(map, "reviewThreads", |threads| {
                    let threads = object(threads)?;
                    opt(threads, "totalCount", int)?;
                    Ok((
                        opt(threads, "pageInfo", raw_page_info)?,
                        req(threads, "nodes", |nodes| array(nodes, raw_review_thread))?,
                    ))
                })?;
                // In the schema's order, so the first failure is the one the TS reports.
                let viewer = viewer_fields(map)?;
                let author = opt_n(map, "author", raw_actor)?;
                let reaction_groups = raw_reaction_groups(map)?;
                let comments = connection_nodes(map, "comments", raw_reactable_node)?;
                let reviews = connection_nodes(map, "reviews", raw_reactable_node)?;
                let review_requests = connection_nodes(map, "reviewRequests", |node| opt_n(object(node)?, "requestedReviewer", raw_actor))?;
                let latest_reviews = connection_nodes(map, "latestReviews", raw_latest_review)?;
                let (dismissals_page_info, dismissals) = match opt_n(map, "reviewDismissals", raw_dismissals)? {
                    None => (None, None),
                    Some((page_info, nodes)) => (page_info, Some(nodes)),
                };
                Ok(RawReviewThreadsPullRequest {
                    threads_page_info,
                    threads,
                    viewer,
                    author,
                    reaction_groups,
                    comments,
                    reviews,
                    review_requests,
                    latest_reviews,
                    dismissals_page_info,
                    dismissals,
                    commits: connection_nodes(map, "commits", raw_graphql_commit)?,
                })
            })
        })?;
        Ok(RawReviewThreads { viewer, pull_request })
    })
}

/// `{ id: string }`, the shape every node-id lookup answers with.
pub(crate) fn id_object(value: &Value) -> Decoded<String> {
    req(object(value)?, "id", string)
}

/// The value at `keys` (`data.repository.pullRequest`…), every step a required struct key.
pub(crate) fn at_path<T>(value: &Value, keys: &[&str], decode: &dyn Fn(&Value) -> Decoded<T>) -> Decoded<T> {
    match keys.split_first() {
        None => decode(value),
        Some((key, rest)) => req(object(value)?, key, |inner| at_path(inner, rest, decode)),
    }
}

/// `viewer: { login?: string | null } | null`, optional, as the thread reads answer it.
pub(crate) fn viewer_login(map: &Obj) -> Decoded<Option<String>> {
    Ok(opt_n(map, "viewer", |viewer| opt_n(object(viewer)?, "login", string))?.flatten())
}

/// A required `Schema.Number` (`behindBy` of the base comparison).
pub(crate) fn behind_by(value: &Value) -> Decoded<f64> {
    req(object(value)?, "behindBy", number)
}
